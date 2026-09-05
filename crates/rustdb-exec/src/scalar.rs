//! The built-in functions and the remaining SQL operators, as executor nodes.
//!
//! Invariant: the *semantics* of every function here are `rustdb-scalar`'s, not
//! this module's. `substr`, `strftime`, `LIKE`, `printf`, `CAST`, the bitwise
//! operators and three-valued `IS` all live in one place in this workspace, and
//! the TDD's "port the built-in function set from `rustdb-vm`" was done by
//! moving that code down a layer rather than by copying it. What is in this
//! file is the *bridge*: borrowed page values in, a `Value` call, an answer out.
//! A second implementation of `substr` would be two implementations of `substr`
//! that agree today.
//!
//! ## What the bridge costs, and why it is paid here and not in the scan
//!
//! `rustdb-scalar` takes `Value<'static>`, so a text argument is copied out of
//! the page before the call. That is a real allocation per text argument per
//! row, and it is the reason `length()` is *not* routed through here: it has a
//! specialised node in [`crate::expr`] that reads the leaf's bytes in place,
//! because `range.lookaside` calls it a hundred thousand times.
//!
//! Everything else is on the general path, where a query that calls `upper()`
//! per row was always going to pay for the string it builds. The alternative -
//! a second, borrowing implementation of the whole function set - trades an
//! allocation for a divergence, and the divergence is the expensive one.
//!
//! ## What is not here
//!
//! JSON functions, which need `rustdb-ext`'s binary form and are the TDD's
//! Phase 4; window functions, which are operator state rather than expressions;
//! and subqueries, which are pipelines. Each is refused by name in
//! [`crate::physical`] rather than approximated.

use rustdb_base::error::misuse;
use rustdb_base::DbResult;
use rustdb_scalar::{builtin, datetime, eval, mathfn, pattern};
use rustdb_sql::ast::{BinaryOp, UnaryOp};
use rustdb_sql::function::{MathFunc, ScalarFunc, TimeFunc};
use rustdb_tree::datum::{Datum, OwnedDatum};
use rustdb_value::affinity::Affinity;
use rustdb_value::collation::Collation;
use rustdb_value::encoding::TextEncoding;
use rustdb_value::value::Value;
use rustdb_value::{cast, compare};

use crate::batch::Batch;
use crate::expr::{Computed, Eval};

/// The database encoding, which is UTF-8 and only UTF-8.
///
/// The TDD fixes it in the leaf layout: "Text is stored as UTF-8 bytes with no
/// terminator; the database encoding is UTF-8 only." So this is a constant
/// rather than a parameter nobody could vary, and it is named rather than
/// written out at eleven call sites.
const ENCODING: TextEncoding = TextEncoding::Utf8;

/// Returns a borrowed page value as an owned `Value`.
///
/// Text and blobs are copied, which is the cost the module documentation
/// explains. Everything else is free.
///
/// @param datum - the value read out of a batch
pub fn to_value(datum: Datum<'_>) -> Value<'static> {
    match datum {
        Datum::Null => Value::Null,
        Datum::Int(number) => Value::Integer(number),
        Datum::Real(number) => Value::Real(number),
        Datum::Text(bytes) => Value::owned_text(bytes).unwrap_or(Value::Null),
        Datum::Blob(bytes) => Value::owned_blob(bytes).unwrap_or(Value::Null),
    }
}

/// Returns a `Value` as an owned datum.
///
/// @param value - the value a function produced
pub fn from_value(value: Value<'_>) -> OwnedDatum {
    match value {
        Value::Null => OwnedDatum::Null,
        Value::Integer(number) => OwnedDatum::Int(number),
        Value::Real(number) => OwnedDatum::Real(number),
        Value::Text(text) => OwnedDatum::Text(text.utf8_bytes().into_owned()),
        Value::Blob(blob) => OwnedDatum::Blob(blob.raw().to_vec()),
    }
}

/// Evaluates a list of argument expressions into owned values.
///
/// @param arguments - the compiled argument expressions
/// @param batch - the batch being evaluated
/// @param nth - the row's position among the live rows
fn arguments_of(
    arguments: &[Box<dyn Eval>],
    batch: &Batch<'_>,
    nth: usize,
) -> DbResult<Vec<Value<'static>>> {
    let mut out = Vec::with_capacity(arguments.len());
    for argument in arguments {
        out.push(to_value(argument.value(batch, nth)?.get()));
    }
    Ok(out)
}

/// A call to one of the dialect's scalar functions.
pub struct ScalarCall {
    /// Which function.
    pub func: ScalarFunc,
    /// The compiled arguments.
    pub arguments: Vec<Box<dyn Eval>>,
    /// The collation the function's comparisons use.
    pub collation: Collation,
}

impl Eval for ScalarCall {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let values = arguments_of(&self.arguments, batch, nth)?;
        let answer = builtin::call(self.func, &values, self.collation, ENCODING);
        Ok(Computed::Owned(from_value(answer)))
    }
}

/// A call to one of the math functions.
pub struct MathCall {
    /// Which function.
    pub func: MathFunc,
    /// The compiled arguments.
    pub arguments: Vec<Box<dyn Eval>>,
}

impl Eval for MathCall {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let values = arguments_of(&self.arguments, batch, nth)?;
        Ok(Computed::Owned(from_value(mathfn::call(
            self.func, &values,
        ))))
    }
}

/// A call to one of the date and time functions.
pub struct TimeCall {
    /// Which function.
    pub func: TimeFunc,
    /// The compiled arguments.
    pub arguments: Vec<Box<dyn Eval>>,
    /// The julian day the statement calls "now".
    ///
    /// Fixed for the whole statement rather than read per row, which is what
    /// SQLite does: every `now` in one statement is the same instant, or a
    /// query could see two.
    pub now: f64,
}

impl Eval for TimeCall {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let values = arguments_of(&self.arguments, batch, nth)?;
        Ok(Computed::Owned(from_value(datetime::call(
            self.func, &values, self.now, ENCODING,
        ))))
    }
}

/// An arithmetic, bitwise or concatenation operator over anything.
///
/// The specialised integer arithmetic in [`crate::expr`] covers `+`, `-` and
/// `*`; this covers those too when the operands are not integral, and covers
/// `/`, `%`, `||` and the bitwise operators, which the specialisation never
/// claimed.
pub struct GeneralArith {
    /// Which operator.
    pub op: BinaryOp,
    /// The left operand.
    pub left: Box<dyn Eval>,
    /// The right operand.
    pub right: Box<dyn Eval>,
}

impl Eval for GeneralArith {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        let answer = eval::arithmetic(
            self.op,
            &to_value(left.get()),
            &to_value(right.get()),
            ENCODING,
        );
        Ok(Computed::Owned(from_value(answer)))
    }
}

/// A unary operator: `-`, `+` or `~`.
pub struct Unary {
    /// Which operator.
    pub op: UnaryOp,
    /// The operand.
    pub operand: Box<dyn Eval>,
}

impl Eval for Unary {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let operand = self.operand.value(batch, nth)?;
        let value = to_value(operand.get());
        let answer = match self.op {
            UnaryOp::Negate => eval::negate(&value),
            // Unary plus is the identity in SQLite - it does not even apply a
            // numeric affinity - so the operand is handed back unchanged.
            UnaryOp::Identity => return Ok(operand),
            UnaryOp::BitNot => eval::bit_not(&value),
            UnaryOp::Not => eval::logical_not(&value),
        };
        Ok(Computed::Owned(from_value(answer)))
    }
}

/// `CAST(x AS type)`.
pub struct Cast {
    /// The operand.
    pub operand: Box<dyn Eval>,
    /// The affinity the declared type maps to.
    pub affinity: Affinity,
}

impl Eval for Cast {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let operand = self.operand.value(batch, nth)?;
        let value = to_value(operand.get());
        let answer = cast::cast_value(value, self.affinity, ENCODING)
            .map_err(|_| misuse("a cast could not be evaluated"))?;
        Ok(Computed::Owned(from_value(answer)))
    }
}

/// `IS` and `IS NOT`, which are never NULL.
pub struct IsTest {
    /// Whether `NOT` was written.
    pub negated: bool,
    /// The left operand.
    pub left: Box<dyn Eval>,
    /// The right operand.
    pub right: Box<dyn Eval>,
    /// The affinity applied before comparing.
    pub affinity: Option<Affinity>,
    /// The collation the comparison uses.
    pub collation: Collation,
}

impl Eval for IsTest {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        let answer = eval::is_comparison(
            self.negated,
            &to_value(left.get()),
            &to_value(right.get()),
            self.affinity,
            self.collation,
            ENCODING,
        );
        Ok(Computed::Owned(from_value(answer)))
    }
}

/// `BETWEEN`, kept as one node so its operand is evaluated once.
pub struct Between {
    /// Whether `NOT` was written.
    pub negated: bool,
    /// The value being tested.
    pub operand: Box<dyn Eval>,
    /// The lower bound.
    pub low: Box<dyn Eval>,
    /// The upper bound.
    pub high: Box<dyn Eval>,
    /// The affinity applied to the comparisons.
    pub affinity: Option<Affinity>,
    /// The collation the comparisons use.
    pub collation: Collation,
}

impl Eval for Between {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let operand = self.operand.value(batch, nth)?;
        let low = self.low.value(batch, nth)?;
        let high = self.high.value(batch, nth)?;
        let operand = to_value(operand.get());
        let above = eval::comparison(
            BinaryOp::GreaterEqual,
            &operand,
            &to_value(low.get()),
            self.affinity,
            self.collation,
            ENCODING,
        );
        let below = eval::comparison(
            BinaryOp::LessEqual,
            &operand,
            &to_value(high.get()),
            self.affinity,
            self.collation,
            ENCODING,
        );
        let inside = eval::logical_and(&above, &below);
        let answer = if self.negated {
            eval::logical_not(&inside)
        } else {
            inside
        };
        Ok(Computed::Owned(from_value(answer)))
    }
}

/// `IN` over a value list.
///
/// The NULL rule is the one people get wrong and the one SQLite documents:
/// `x IN (list)` is false only when `x` matches nothing *and* nothing in the
/// list is NULL; a NULL in the list turns a non-match into NULL rather than
/// into false. `NOT IN` inherits it by negation, which is why the negation is
/// applied to a three-valued answer rather than to a boolean.
pub struct InList {
    /// Whether `NOT` was written.
    pub negated: bool,
    /// The value being tested.
    pub operand: Box<dyn Eval>,
    /// The list.
    pub list: Vec<Box<dyn Eval>>,
    /// The affinity applied before comparing.
    pub affinity: Option<Affinity>,
    /// The collation the comparison uses.
    pub collation: Collation,
}

impl Eval for InList {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let operand = self.operand.value(batch, nth)?;
        let operand = to_value(operand.get());
        if operand.is_null() {
            // NULL IN (anything) is NULL, and NULL IN () is false. The empty
            // list is the exception SQLite makes and it is worth the branch.
            if self.list.is_empty() {
                return Ok(Computed::Owned(OwnedDatum::Int(i64::from(self.negated))));
            }
            return Ok(Computed::Owned(OwnedDatum::Null));
        }
        let mut saw_null = false;
        for candidate in &self.list {
            let candidate = candidate.value(batch, nth)?;
            let candidate = to_value(candidate.get());
            if candidate.is_null() {
                saw_null = true;
                continue;
            }
            let equal = eval::comparison(
                BinaryOp::Equal,
                &operand,
                &candidate,
                self.affinity,
                self.collation,
                ENCODING,
            );
            if eval::truth(&equal) == compare::Truth::True {
                return Ok(Computed::Owned(OwnedDatum::Int(i64::from(!self.negated))));
            }
        }
        if saw_null {
            return Ok(Computed::Owned(OwnedDatum::Null));
        }
        Ok(Computed::Owned(OwnedDatum::Int(i64::from(self.negated))))
    }
}

/// `CASE`, in both its forms.
pub struct Case {
    /// The base operand, when the form has one.
    pub operand: Option<Box<dyn Eval>>,
    /// The `WHEN`/`THEN` pairs.
    pub branches: Vec<(Box<dyn Eval>, Box<dyn Eval>)>,
    /// The `ELSE` arm.
    pub otherwise: Option<Box<dyn Eval>>,
    /// The collation comparisons in the base form use.
    pub collation: Collation,
}

impl Eval for Case {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let base = match &self.operand {
            Some(operand) => Some(to_value(operand.value(batch, nth)?.get())),
            None => None,
        };
        for (when, then) in &self.branches {
            let candidate = when.value(batch, nth)?;
            let matched = match &base {
                // `CASE x WHEN y` compares; `CASE WHEN p` tests a predicate.
                Some(base) => {
                    let equal = eval::comparison(
                        BinaryOp::Equal,
                        base,
                        &to_value(candidate.get()),
                        None,
                        self.collation,
                        ENCODING,
                    );
                    eval::truth(&equal) == compare::Truth::True
                }
                None => eval::truth(&to_value(candidate.get())) == compare::Truth::True,
            };
            if matched {
                return then.value(batch, nth);
            }
        }
        match &self.otherwise {
            Some(otherwise) => otherwise.value(batch, nth),
            None => Ok(Computed::Borrowed(Datum::Null)),
        }
    }
}

/// Which pattern operator a [`Pattern`] node applies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternKind {
    /// `LIKE`, case-insensitive for ASCII.
    Like,
    /// `GLOB`, case-sensitive with shell wildcards.
    Glob,
}

/// `LIKE` and `GLOB`.
pub struct Pattern {
    /// Whether `NOT` was written.
    pub negated: bool,
    /// Which operator.
    pub kind: PatternKind,
    /// The value being matched.
    pub operand: Box<dyn Eval>,
    /// The pattern.
    pub pattern: Box<dyn Eval>,
    /// The `ESCAPE` argument, for `LIKE`.
    pub escape: Option<Box<dyn Eval>>,
}

impl Eval for Pattern {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let operand = self.operand.value(batch, nth)?;
        let pattern = self.pattern.value(batch, nth)?;
        if operand.is_null() || pattern.is_null() {
            return Ok(Computed::Borrowed(Datum::Null));
        }
        let escape = match &self.escape {
            Some(expression) => {
                let value = expression.value(batch, nth)?;
                if value.is_null() {
                    return Ok(Computed::Borrowed(Datum::Null));
                }
                eval::text_bytes(&to_value(value.get()), ENCODING)
                    .first()
                    .copied()
            }
            None => None,
        };
        let subject = eval::text_bytes(&to_value(operand.get()), ENCODING);
        let pattern_bytes = eval::text_bytes(&to_value(pattern.get()), ENCODING);
        let matched = match self.kind {
            PatternKind::Like => pattern::like(&pattern_bytes, &subject, escape),
            PatternKind::Glob => pattern::glob(&pattern_bytes, &subject),
        };
        Ok(Computed::Borrowed(Datum::Int(i64::from(
            matched != self.negated,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::Vector;
    use crate::expr::{compile, Expr, StaticType};

    /// Returns a one-row batch over the given values.
    fn one_row<'p>(values: &[Datum<'p>]) -> Batch<'p> {
        Batch::new(
            1,
            values.iter().map(|value| Vector::Const(*value)).collect(),
        )
    }

    /// Evaluates a node over a one-row batch.
    fn eval_one<'p>(node: &dyn Eval, values: &[Datum<'p>]) -> OwnedDatum {
        let batch = one_row(values);
        node.value(&batch, 0).unwrap().into_owned()
    }

    /// Returns a compiled column reference.
    fn column(index: usize, count: usize) -> Box<dyn Eval> {
        compile(&Expr::Column(index), &vec![StaticType::Unknown; count]).unwrap()
    }

    /// `substr` answers what SQLite answers, through the shared implementation.
    #[test]
    fn a_scalar_call_reaches_the_shared_implementation() {
        let node = ScalarCall {
            func: ScalarFunc::Substr,
            arguments: vec![column(0, 3), column(1, 3), column(2, 3)],
            collation: Collation::Binary,
        };
        let answer = eval_one(
            &node,
            &[Datum::Text(b"abcdefgh"), Datum::Int(3), Datum::Int(4)],
        );
        assert_eq!(answer, OwnedDatum::Text(b"cdef".to_vec()));
        // SQLite counts from one and accepts a negative start.
        let answer = eval_one(
            &node,
            &[Datum::Text(b"abcdefgh"), Datum::Int(-3), Datum::Int(2)],
        );
        assert_eq!(answer, OwnedDatum::Text(b"fg".to_vec()));
    }

    /// `upper` builds a value that was not in the page, which is the case the
    /// borrowed-only evaluator could not express.
    #[test]
    fn a_function_can_return_a_value_the_page_never_held() {
        let node = ScalarCall {
            func: ScalarFunc::Upper,
            arguments: vec![column(0, 1)],
            collation: Collation::Binary,
        };
        assert_eq!(
            eval_one(&node, &[Datum::Text(b"mixed Case")]),
            OwnedDatum::Text(b"MIXED CASE".to_vec())
        );
    }

    /// Concatenation and the bitwise operators go through the general node.
    #[test]
    fn the_general_operators_answer() {
        let concat = GeneralArith {
            op: BinaryOp::Concat,
            left: column(0, 2),
            right: column(1, 2),
        };
        assert_eq!(
            eval_one(&concat, &[Datum::Text(b"ab"), Datum::Int(7)]),
            OwnedDatum::Text(b"ab7".to_vec())
        );
        let and = GeneralArith {
            op: BinaryOp::BitAnd,
            left: column(0, 2),
            right: column(1, 2),
        };
        assert_eq!(
            eval_one(&and, &[Datum::Int(0b1100), Datum::Int(0b1010)]),
            OwnedDatum::Int(0b1000)
        );
        // NULL poisons an arithmetic operator, as it must.
        assert_eq!(
            eval_one(&concat, &[Datum::Null, Datum::Int(7)]),
            OwnedDatum::Null
        );
    }

    /// The unary operators, including the one that does nothing.
    #[test]
    fn the_unary_operators_answer() {
        for (op, input, wanted) in [
            (UnaryOp::Negate, Datum::Int(5), OwnedDatum::Int(-5)),
            (
                UnaryOp::Identity,
                Datum::Text(b"x"),
                OwnedDatum::Text(b"x".to_vec()),
            ),
            (UnaryOp::BitNot, Datum::Int(0), OwnedDatum::Int(-1)),
            (UnaryOp::Not, Datum::Int(0), OwnedDatum::Int(1)),
            (UnaryOp::Not, Datum::Null, OwnedDatum::Null),
        ] {
            let node = Unary {
                op,
                operand: column(0, 1),
            };
            assert_eq!(eval_one(&node, &[input]), wanted, "{op:?}");
        }
    }

    /// `CAST` converts through the shared rules.
    #[test]
    fn a_cast_converts() {
        let node = Cast {
            operand: column(0, 1),
            affinity: Affinity::Integer,
        };
        assert_eq!(
            eval_one(&node, &[Datum::Text(b"42abc")]),
            OwnedDatum::Int(42)
        );
        let node = Cast {
            operand: column(0, 1),
            affinity: Affinity::Text,
        };
        assert_eq!(
            eval_one(&node, &[Datum::Int(42)]),
            OwnedDatum::Text(b"42".to_vec())
        );
    }

    /// `IS` is never NULL, where `=` is.
    #[test]
    fn is_is_never_null() {
        let node = IsTest {
            negated: false,
            left: column(0, 2),
            right: column(1, 2),
            affinity: None,
            collation: Collation::Binary,
        };
        assert_eq!(
            eval_one(&node, &[Datum::Null, Datum::Null]),
            OwnedDatum::Int(1)
        );
        assert_eq!(
            eval_one(&node, &[Datum::Null, Datum::Int(1)]),
            OwnedDatum::Int(0)
        );
        let node = IsTest {
            negated: true,
            left: column(0, 2),
            right: column(1, 2),
            affinity: None,
            collation: Collation::Binary,
        };
        assert_eq!(
            eval_one(&node, &[Datum::Null, Datum::Null]),
            OwnedDatum::Int(0)
        );
    }

    /// `BETWEEN` is inclusive at both ends and NULL-poisoned.
    #[test]
    fn between_is_inclusive() {
        let node = Between {
            negated: false,
            operand: column(0, 3),
            low: column(1, 3),
            high: column(2, 3),
            affinity: None,
            collation: Collation::Binary,
        };
        for (value, wanted) in [(4i64, 0i64), (5, 1), (7, 1), (10, 1), (11, 0)] {
            assert_eq!(
                eval_one(&node, &[Datum::Int(value), Datum::Int(5), Datum::Int(10)]),
                OwnedDatum::Int(wanted),
                "{value}"
            );
        }
        assert_eq!(
            eval_one(&node, &[Datum::Null, Datum::Int(5), Datum::Int(10)]),
            OwnedDatum::Null
        );
    }

    /// `IN` follows SQLite's NULL rule, including the empty list.
    #[test]
    fn in_follows_the_null_rule() {
        let with_null = InList {
            negated: false,
            operand: column(0, 3),
            list: vec![column(1, 3), column(2, 3)],
            affinity: None,
            collation: Collation::Binary,
        };
        // A match wins even with a NULL in the list.
        assert_eq!(
            eval_one(&with_null, &[Datum::Int(1), Datum::Int(1), Datum::Null]),
            OwnedDatum::Int(1)
        );
        // No match plus a NULL in the list is NULL, not false.
        assert_eq!(
            eval_one(&with_null, &[Datum::Int(2), Datum::Int(1), Datum::Null]),
            OwnedDatum::Null
        );
        // No match and no NULL is false.
        assert_eq!(
            eval_one(&with_null, &[Datum::Int(2), Datum::Int(1), Datum::Int(3)]),
            OwnedDatum::Int(0)
        );
        // A NULL operand is NULL...
        assert_eq!(
            eval_one(&with_null, &[Datum::Null, Datum::Int(1), Datum::Int(3)]),
            OwnedDatum::Null
        );
        // ...except against the empty list, which is false.
        let empty = InList {
            negated: false,
            operand: column(0, 1),
            list: Vec::new(),
            affinity: None,
            collation: Collation::Binary,
        };
        assert_eq!(eval_one(&empty, &[Datum::Null]), OwnedDatum::Int(0));
        let empty_not = InList {
            negated: true,
            operand: column(0, 1),
            list: Vec::new(),
            affinity: None,
            collation: Collation::Binary,
        };
        assert_eq!(eval_one(&empty_not, &[Datum::Null]), OwnedDatum::Int(1));
    }

    /// `CASE` in both forms, including the missing `ELSE`.
    #[test]
    fn case_takes_the_first_matching_branch() {
        let searched = Case {
            operand: None,
            branches: vec![(column(0, 4), column(1, 4)), (column(2, 4), column(3, 4))],
            otherwise: None,
            collation: Collation::Binary,
        };
        assert_eq!(
            eval_one(
                &searched,
                &[Datum::Int(0), Datum::Int(10), Datum::Int(1), Datum::Int(20)]
            ),
            OwnedDatum::Int(20)
        );
        assert_eq!(
            eval_one(
                &searched,
                &[Datum::Int(0), Datum::Int(10), Datum::Int(0), Datum::Int(20)]
            ),
            OwnedDatum::Null,
            "no branch and no ELSE is NULL"
        );
        let simple = Case {
            operand: Some(column(0, 3)),
            branches: vec![(column(1, 3), column(2, 3))],
            otherwise: None,
            collation: Collation::Binary,
        };
        assert_eq!(
            eval_one(&simple, &[Datum::Int(7), Datum::Int(7), Datum::Int(99)]),
            OwnedDatum::Int(99)
        );
        assert_eq!(
            eval_one(&simple, &[Datum::Int(7), Datum::Int(8), Datum::Int(99)]),
            OwnedDatum::Null
        );
    }

    /// `LIKE` and `GLOB` reach the shared matcher, with the negation and the
    /// escape applied here.
    #[test]
    fn the_pattern_operators_match() {
        let like = Pattern {
            negated: false,
            kind: PatternKind::Like,
            operand: column(0, 2),
            pattern: column(1, 2),
            escape: None,
        };
        assert_eq!(
            eval_one(&like, &[Datum::Text(b"Hello"), Datum::Text(b"h%o")]),
            OwnedDatum::Int(1),
            "LIKE is ASCII case-insensitive"
        );
        let glob = Pattern {
            negated: false,
            kind: PatternKind::Glob,
            operand: column(0, 2),
            pattern: column(1, 2),
            escape: None,
        };
        assert_eq!(
            eval_one(&glob, &[Datum::Text(b"Hello"), Datum::Text(b"h*o")]),
            OwnedDatum::Int(0),
            "GLOB is case-sensitive"
        );
        let not_like = Pattern {
            negated: true,
            kind: PatternKind::Like,
            operand: column(0, 2),
            pattern: column(1, 2),
            escape: None,
        };
        assert_eq!(
            eval_one(&not_like, &[Datum::Text(b"Hello"), Datum::Text(b"h%o")]),
            OwnedDatum::Int(0)
        );
        assert_eq!(
            eval_one(&like, &[Datum::Null, Datum::Text(b"x")]),
            OwnedDatum::Null
        );
    }

    /// The value bridge round-trips every class.
    #[test]
    fn the_value_bridge_round_trips() {
        let cases = [
            Datum::Null,
            Datum::Int(-7),
            Datum::Real(1.5),
            Datum::Text(b"text"),
            Datum::Blob(b"\x00\xFFbytes"),
        ];
        for datum in cases {
            let round = from_value(to_value(datum));
            assert_eq!(round, OwnedDatum::from_datum(&datum), "{datum:?}");
        }
    }

    /// A math function reaches the shared implementation.
    #[test]
    fn a_math_call_answers() {
        let node = MathCall {
            func: MathFunc::Sqrt,
            arguments: vec![column(0, 1)],
        };
        assert_eq!(eval_one(&node, &[Datum::Int(16)]), OwnedDatum::Real(4.0));
        assert_eq!(eval_one(&node, &[Datum::Null]), OwnedDatum::Null);
    }

    /// A time function reaches the shared implementation, at a fixed instant.
    #[test]
    fn a_time_call_answers_at_a_fixed_instant() {
        let node = TimeCall {
            func: TimeFunc::Date,
            arguments: vec![column(0, 1)],
            now: 2_451_545.0,
        };
        assert_eq!(
            eval_one(&node, &[Datum::Text(b"2024-03-04 05:06:07")]),
            OwnedDatum::Text(b"2024-03-04".to_vec())
        );
    }
}
