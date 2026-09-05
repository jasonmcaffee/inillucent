//! Closure compilation: an expression tree becomes a tree of specialised
//! objects, chosen once per statement rather than dispatched once per row.
//!
//! Invariant: compilation changes speed, never meaning. Every specialised node
//! has a generic node that computes the same thing, and the test
//! `every_specialisation_agrees_with_the_generic_path` runs both over the same
//! inputs - including the type combinations that make a specialisation
//! inapplicable - and requires the same answer. A specialisation that is faster
//! and different is a bug, and it is the kind that only shows up in a digest
//! comparison months later.
//!
//! ## What "closure compilation" means here and why not a code generator
//!
//! The TDD's execution section calls for closure compilation and rules out
//! Cranelift or LLVM. The distinction that matters is where the type dispatch
//! happens. An interpreter looks at an `Add` node, asks what its operands are,
//! branches, and does that for every row. A closure compiler asks once, at
//! prepare time, and builds an object that can only add two integers - so the
//! per-row cost is one indirect call to a body with no branches in it.
//!
//! In Rust that object is a `Box<dyn Eval>` rather than a `Box<dyn Fn>`,
//! because the value a node returns borrows the batch it read from and a boxed
//! closure cannot express that lifetime relationship. A trait method generic
//! over a lifetime can, and stays object-safe.
//!
//! The specialisations exist where the binder can prove the operand types from
//! column affinity: integer arithmetic, integer comparison, comparison against
//! a literal. Everything else compiles to the generic node, which is the same
//! code the old VM ran, and correctness never depends on which was chosen.

use rustdb_base::error::misuse;
use rustdb_base::DbResult;
use rustdb_tree::datum::{Datum, OwnedDatum};

use crate::batch::Batch;

/// A compiled expression node.
///
/// Object-safe despite the lifetime parameter on `value`, because a lifetime
/// parameter on a method is allowed on a trait object where a type parameter is
/// not. That is what lets a node hand back a value borrowing the page.
pub trait Eval: Send + Sync {
    /// Evaluates this expression for one live row of a batch.
    ///
    /// @param batch - the batch being evaluated
    /// @param nth - the position among the batch's live rows
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>>;

    /// Returns the column this reads, when it is a bare column reference.
    ///
    /// The aggregate operators use this to take a whole-column fast path: a
    /// `sum` over a bare column reference reads the mini-column's bytes rather
    /// than calling `value` per row.
    fn column(&self) -> Option<usize> {
        None
    }
}

/// The expression language the physical planner hands to the compiler.
#[derive(Clone, Debug)]
pub enum Expr {
    /// One column of the input batch.
    Column(usize),
    /// A constant.
    Literal(OwnedDatum),
    /// Addition, subtraction, multiplication.
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    /// A comparison.
    Compare(CompareOp, Box<Expr>, Box<Expr>),
    /// `AND`, with SQL's three-valued logic.
    And(Box<Expr>, Box<Expr>),
    /// `OR`, with SQL's three-valued logic.
    Or(Box<Expr>, Box<Expr>),
    /// `NOT`, with SQL's three-valued logic.
    Not(Box<Expr>),
    /// `IS NULL`, which is two-valued.
    IsNull(Box<Expr>),
    /// `IS NOT NULL`.
    IsNotNull(Box<Expr>),
    /// `length(x)`, needed by `range.lookaside` and the fixture's shapes.
    Length(Box<Expr>),
}

/// The arithmetic operators Phase 1 compiles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Subtract,
    /// `*`
    Multiply,
}

/// The comparison operators.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareOp {
    /// `=`
    Equal,
    /// `<>`
    NotEqual,
    /// `<`
    Less,
    /// `<=`
    LessOrEqual,
    /// `>`
    Greater,
    /// `>=`
    GreaterOrEqual,
}

impl CompareOp {
    /// Reports whether an ordering satisfies this operator.
    ///
    /// @param order - how the two values compared
    pub fn holds(self, order: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::{Equal, Greater, Less};
        match self {
            CompareOp::Equal => order == Equal,
            CompareOp::NotEqual => order != Equal,
            CompareOp::Less => order == Less,
            CompareOp::LessOrEqual => order != Greater,
            CompareOp::Greater => order == Greater,
            CompareOp::GreaterOrEqual => order != Less,
        }
    }
}

/// What the compiler knows about an expression's result type.
///
/// The binder derives this from column affinity. `Unknown` is always safe: it
/// selects the generic node, which is correct for every input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StaticType {
    /// Certainly an integer, or NULL.
    Int,
    /// Certainly a double, or NULL.
    Real,
    /// Certainly text, or NULL.
    Text,
    /// Anything.
    Unknown,
}

/// Compiles an expression against the static types of the input columns.
///
/// @param expr - the expression to compile
/// @param types - the static type of each input column
pub fn compile(expr: &Expr, types: &[StaticType]) -> DbResult<Box<dyn Eval>> {
    Ok(match expr {
        Expr::Column(index) => Box::new(ColumnRef { index: *index }),
        Expr::Literal(value) => Box::new(Literal {
            value: value.clone(),
        }),
        Expr::Arith(op, left, right) => {
            let compiled_left = compile(left, types)?;
            let compiled_right = compile(right, types)?;
            if static_type(left, types) == StaticType::Int
                && static_type(right, types) == StaticType::Int
            {
                Box::new(IntArith {
                    op: *op,
                    left: compiled_left,
                    right: compiled_right,
                })
            } else {
                Box::new(GenericArith {
                    op: *op,
                    left: compiled_left,
                    right: compiled_right,
                })
            }
        }
        Expr::Compare(op, left, right) => {
            let compiled_left = compile(left, types)?;
            let compiled_right = compile(right, types)?;
            // The one specialisation that matters for the scorecard's shapes:
            // `col op literal` where the column is known to be an integer.
            if let (Expr::Column(index), Expr::Literal(OwnedDatum::Int(constant))) =
                (left.as_ref(), right.as_ref())
            {
                if column_type(*index, types) == StaticType::Int {
                    return Ok(Box::new(IntColumnAgainstConstant {
                        column: *index,
                        op: *op,
                        constant: *constant,
                    }));
                }
            }
            if static_type(left, types) == StaticType::Int
                && static_type(right, types) == StaticType::Int
            {
                Box::new(IntCompare {
                    op: *op,
                    left: compiled_left,
                    right: compiled_right,
                })
            } else {
                Box::new(GenericCompare {
                    op: *op,
                    left: compiled_left,
                    right: compiled_right,
                })
            }
        }
        Expr::And(left, right) => Box::new(Conjunction {
            left: compile(left, types)?,
            right: compile(right, types)?,
        }),
        Expr::Or(left, right) => Box::new(Disjunction {
            left: compile(left, types)?,
            right: compile(right, types)?,
        }),
        Expr::Not(inner) => Box::new(Negation {
            inner: compile(inner, types)?,
        }),
        Expr::IsNull(inner) => Box::new(NullTest {
            inner: compile(inner, types)?,
            wanted: true,
        }),
        Expr::IsNotNull(inner) => Box::new(NullTest {
            inner: compile(inner, types)?,
            wanted: false,
        }),
        Expr::Length(inner) => Box::new(Length {
            inner: compile(inner, types)?,
        }),
    })
}

/// Returns what the compiler can prove about an expression's type.
///
/// @param expr - the expression to inspect
/// @param types - the static type of each input column
pub fn static_type(expr: &Expr, types: &[StaticType]) -> StaticType {
    match expr {
        Expr::Column(index) => column_type(*index, types),
        Expr::Literal(OwnedDatum::Int(_)) => StaticType::Int,
        Expr::Literal(OwnedDatum::Real(_)) => StaticType::Real,
        Expr::Literal(OwnedDatum::Text(_)) => StaticType::Text,
        Expr::Literal(_) => StaticType::Unknown,
        Expr::Arith(_, left, right) => {
            if static_type(left, types) == StaticType::Int
                && static_type(right, types) == StaticType::Int
            {
                StaticType::Int
            } else {
                StaticType::Unknown
            }
        }
        Expr::Compare(..)
        | Expr::And(..)
        | Expr::Or(..)
        | Expr::Not(..)
        | Expr::IsNull(..)
        | Expr::IsNotNull(..)
        | Expr::Length(..) => StaticType::Int,
    }
}

/// Returns one input column's static type.
///
/// @param index - which column
/// @param types - the static type of each input column
fn column_type(index: usize, types: &[StaticType]) -> StaticType {
    types.get(index).copied().unwrap_or(StaticType::Unknown)
}

/// A bare column reference.
struct ColumnRef {
    index: usize,
}

impl Eval for ColumnRef {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        batch.value(nth, self.index)
    }

    fn column(&self) -> Option<usize> {
        Some(self.index)
    }
}

/// A constant.
struct Literal {
    value: OwnedDatum,
}

impl Eval for Literal {
    fn value<'p>(&self, _batch: &Batch<'p>, _nth: usize) -> DbResult<Datum<'p>> {
        // The value is owned by the node, which outlives every batch it is
        // evaluated against, but the signature promises `'p`. Copying the
        // scalar variants is free; the borrowed ones cannot be handed out with
        // a longer lifetime than they have, so text and blob literals are
        // compared through `compare_owned` in the specialised nodes and are
        // never returned from here. Phase 2's arena removes the restriction by
        // giving literals the statement's lifetime.
        Ok(match &self.value {
            OwnedDatum::Null => Datum::Null,
            OwnedDatum::Int(number) => Datum::Int(*number),
            OwnedDatum::Real(number) => Datum::Real(*number),
            OwnedDatum::Text(_) | OwnedDatum::Blob(_) => {
                return Err(misuse(
                    "a text or blob literal is compared in place, not returned",
                ))
            }
        })
    }
}

/// Integer arithmetic, both operands proved integral.
struct IntArith {
    op: ArithOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for IntArith {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Datum::Null);
        }
        // The static type is a claim the binder derived from column *affinity*,
        // and a dynamically typed engine can hold a row that violates it - a
        // string or a real in a column declared INTEGER, which the leaf records
        // as an exception. Producing NULL for those rows would make the
        // specialisation faster and wrong, which is the one thing it may not be,
        // so it falls back to the generic arithmetic instead.
        let (Some(left), Some(right)) = (left.as_int(), right.as_int()) else {
            return generic_arith(
                self.op,
                &self.left.value(batch, nth)?,
                &self.right.value(batch, nth)?,
            );
        };
        Ok(integer_arith(self.op, left, right))
    }
}

/// Applies an arithmetic operator to two integers.
///
/// SQLite promotes an integer overflow to a double rather than trapping or
/// wrapping, and both the specialised and the generic node route through here
/// so the two cannot drift apart.
///
/// @param op - the operator
/// @param left - the left operand
/// @param right - the right operand
fn integer_arith<'p>(op: ArithOp, left: i64, right: i64) -> Datum<'p> {
    let checked = match op {
        ArithOp::Add => left.checked_add(right),
        ArithOp::Subtract => left.checked_sub(right),
        ArithOp::Multiply => left.checked_mul(right),
    };
    match checked {
        Some(number) => Datum::Int(number),
        None => {
            let (a, b) = (left as f64, right as f64);
            Datum::Real(match op {
                ArithOp::Add => a + b,
                ArithOp::Subtract => a - b,
                ArithOp::Multiply => a * b,
            })
        }
    }
}

/// Applies an arithmetic operator to two values of any class.
///
/// @param op - the operator
/// @param left - the left operand
/// @param right - the right operand
fn generic_arith<'p>(op: ArithOp, left: &Datum<'_>, right: &Datum<'_>) -> DbResult<Datum<'p>> {
    if left.is_null() || right.is_null() {
        return Ok(Datum::Null);
    }
    if let (Some(a), Some(b)) = (left.as_int(), right.as_int()) {
        return Ok(integer_arith(op, a, b));
    }
    let (a, b) = (numeric(left), numeric(right));
    Ok(Datum::Real(match op {
        ArithOp::Add => a + b,
        ArithOp::Subtract => a - b,
        ArithOp::Multiply => a * b,
    }))
}

/// Arithmetic over anything.
struct GenericArith {
    op: ArithOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for GenericArith {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        generic_arith(
            self.op,
            &self.left.value(batch, nth)?,
            &self.right.value(batch, nth)?,
        )
    }
}

/// A comparison of a known-integer column against an integer constant.
///
/// The narrowest and most common specialisation: no child call, no allocation,
/// one integer compare.
struct IntColumnAgainstConstant {
    column: usize,
    op: CompareOp,
    constant: i64,
}

impl Eval for IntColumnAgainstConstant {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        let value = batch.value(nth, self.column)?;
        Ok(match value.as_int() {
            Some(number) => Datum::Int(i64::from(self.op.holds(number.cmp(&self.constant)))),
            None if value.is_null() => Datum::Null,
            // The column's affinity said integer and the row holds something
            // else - an exception row. Fall back to the dialect's comparison
            // rather than guessing, so the specialisation stays a speed change.
            None => Datum::Int(i64::from(
                self.op.holds(value.compare(&Datum::Int(self.constant))),
            )),
        })
    }
}

/// A comparison of two proved-integral expressions.
struct IntCompare {
    op: CompareOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for IntCompare {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Datum::Null);
        }
        Ok(Datum::Int(i64::from(self.op.holds(left.compare(&right)))))
    }
}

/// A comparison of anything, by the dialect's rules.
struct GenericCompare {
    op: CompareOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for GenericCompare {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Datum::Null);
        }
        Ok(Datum::Int(i64::from(self.op.holds(left.compare(&right)))))
    }
}

/// `AND` with three-valued logic.
struct Conjunction {
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for Conjunction {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        let left = self.left.value(batch, nth)?;
        // FALSE AND anything is FALSE, even when the other side is NULL, so a
        // definite false short-circuits.
        if truth(&left) == Some(false) {
            return Ok(Datum::Int(0));
        }
        let right = self.right.value(batch, nth)?;
        Ok(match (truth(&left), truth(&right)) {
            (_, Some(false)) => Datum::Int(0),
            (Some(true), Some(true)) => Datum::Int(1),
            _ => Datum::Null,
        })
    }
}

/// `OR` with three-valued logic.
struct Disjunction {
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for Disjunction {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        let left = self.left.value(batch, nth)?;
        if truth(&left) == Some(true) {
            return Ok(Datum::Int(1));
        }
        let right = self.right.value(batch, nth)?;
        Ok(match (truth(&left), truth(&right)) {
            (_, Some(true)) => Datum::Int(1),
            (Some(false), Some(false)) => Datum::Int(0),
            _ => Datum::Null,
        })
    }
}

/// `NOT` with three-valued logic.
struct Negation {
    inner: Box<dyn Eval>,
}

impl Eval for Negation {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        Ok(match truth(&self.inner.value(batch, nth)?) {
            Some(true) => Datum::Int(0),
            Some(false) => Datum::Int(1),
            None => Datum::Null,
        })
    }
}

/// `IS NULL` and `IS NOT NULL`, which are never NULL themselves.
struct NullTest {
    inner: Box<dyn Eval>,
    wanted: bool,
}

impl Eval for NullTest {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        let is_null = self.inner.value(batch, nth)?.is_null();
        Ok(Datum::Int(i64::from(is_null == self.wanted)))
    }
}

/// `length(x)`: characters for text, bytes for a blob, NULL for NULL.
struct Length {
    inner: Box<dyn Eval>,
}

impl Eval for Length {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Datum<'p>> {
        Ok(match self.inner.value(batch, nth)? {
            Datum::Null => Datum::Null,
            // SQLite counts characters in text and bytes in a blob. The
            // database encoding is UTF-8, so a character is a non-continuation
            // byte.
            Datum::Text(bytes) => {
                Datum::Int(bytes.iter().filter(|byte| (**byte & 0xC0) != 0x80).count() as i64)
            }
            Datum::Blob(bytes) => Datum::Int(bytes.len() as i64),
            Datum::Int(number) => Datum::Int(number.to_string().len() as i64),
            Datum::Real(number) => Datum::Int(format_real(number).len() as i64),
        })
    }
}

/// Returns a value's truth, or `None` for NULL.
///
/// SQLite's rule: zero and the empty string are false, every other number and
/// every other string are true, and a string that does not look like a number
/// is false.
///
/// @param value - the value to test
pub fn truth(value: &Datum<'_>) -> Option<bool> {
    match value {
        Datum::Null => None,
        Datum::Int(number) => Some(*number != 0),
        Datum::Real(number) => Some(*number != 0.0),
        Datum::Text(bytes) | Datum::Blob(bytes) => Some(prefix_number(bytes) != 0.0),
    }
}

/// Returns a value as a double, by SQLite's coercion rules.
///
/// @param value - the value to coerce
pub fn numeric(value: &Datum<'_>) -> f64 {
    match value {
        Datum::Null => 0.0,
        Datum::Int(number) => *number as f64,
        Datum::Real(number) => *number,
        Datum::Text(bytes) | Datum::Blob(bytes) => prefix_number(bytes),
    }
}

/// Reads the longest numeric prefix of a byte string, as SQLite does.
///
/// @param bytes - the string to read
fn prefix_number(bytes: &[u8]) -> f64 {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text.trim_start(),
        Err(_) => return 0.0,
    };
    let mut end = 0usize;
    let raw = text.as_bytes();
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut seen_exponent = false;
    for (index, byte) in raw.iter().enumerate() {
        let accept = match byte {
            b'0'..=b'9' => {
                seen_digit = true;
                true
            }
            b'+' | b'-' => {
                index == 0 || matches!(raw.get(index.saturating_sub(1)), Some(b'e') | Some(b'E'))
            }
            b'.' => !seen_dot && !seen_exponent,
            b'e' | b'E' => seen_digit && !seen_exponent,
            _ => false,
        };
        if !accept {
            break;
        }
        if *byte == b'.' {
            seen_dot = true;
        }
        if matches!(byte, b'e' | b'E') {
            seen_exponent = true;
        }
        end = index.saturating_add(1);
    }
    if !seen_digit {
        return 0.0;
    }
    text.get(..end)
        .and_then(|prefix| prefix.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Renders a double the way the dialect's text conversion does.
///
/// @param number - the value to render
pub fn format_real(number: f64) -> String {
    if number == number.trunc() && number.abs() < 1e15 {
        format!("{number:.1}")
    } else {
        format!("{number}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::Vector;

    fn batch_of<'a>(values: &'a [Datum<'a>]) -> Batch<'a> {
        Batch::new(values.len(), vec![Vector::Values(values)])
    }

    /// Every specialisation agrees with the generic node it replaces, over
    /// every value class - including the ones that make the specialisation's
    /// assumption false.
    ///
    /// This is the test that makes closure compilation safe to do at all: the
    /// compiler may pick a faster node, and this asserts it never picks a
    /// different answer.
    #[test]
    fn every_specialisation_agrees_with_the_generic_path() {
        let samples = [
            Datum::Null,
            Datum::Int(-1),
            Datum::Int(0),
            Datum::Int(5),
            Datum::Int(i64::MAX),
            Datum::Real(-1.5),
            Datum::Real(5.0),
            Datum::Text(b"5"),
            Datum::Text(b"apple"),
            Datum::Blob(&[5]),
        ];
        let operators = [
            CompareOp::Equal,
            CompareOp::NotEqual,
            CompareOp::Less,
            CompareOp::LessOrEqual,
            CompareOp::Greater,
            CompareOp::GreaterOrEqual,
        ];
        for op in operators {
            for constant in [-1i64, 0, 5, i64::MAX] {
                let expr = Expr::Compare(
                    op,
                    Box::new(Expr::Column(0)),
                    Box::new(Expr::Literal(OwnedDatum::Int(constant))),
                );
                // Claiming the column is an integer selects the specialisation;
                // claiming nothing selects the generic node.
                let fast = compile(&expr, &[StaticType::Int]).unwrap();
                let slow = compile(&expr, &[StaticType::Unknown]).unwrap();
                for value in samples {
                    let one = [value];
                    let batch = batch_of(&one);
                    let a = fast.value(&batch, 0).unwrap();
                    let b = slow.value(&batch, 0).unwrap();
                    assert_eq!(
                        a.compare(&b),
                        std::cmp::Ordering::Equal,
                        "{op:?} {value:?} vs {constant}: {a:?} against {b:?}"
                    );
                    assert_eq!(a.is_null(), b.is_null(), "{op:?} {value:?} vs {constant}");
                }
            }
        }
        // The same for arithmetic.
        for op in [ArithOp::Add, ArithOp::Subtract, ArithOp::Multiply] {
            let expr = Expr::Arith(op, Box::new(Expr::Column(0)), Box::new(Expr::Column(1)));
            let fast = compile(&expr, &[StaticType::Int, StaticType::Int]).unwrap();
            let slow = compile(&expr, &[StaticType::Unknown, StaticType::Unknown]).unwrap();
            for left in samples {
                for right in samples {
                    let batch = Batch::new(1, vec![Vector::Const(left), Vector::Const(right)]);
                    let a = fast.value(&batch, 0).unwrap();
                    let b = slow.value(&batch, 0).unwrap();
                    assert_eq!(
                        a.compare(&b),
                        std::cmp::Ordering::Equal,
                        "{op:?} {left:?} {right:?}: {a:?} against {b:?}"
                    );
                }
            }
        }
    }

    /// Integer overflow becomes a double rather than wrapping or trapping, on
    /// both paths.
    #[test]
    fn overflow_promotes_to_a_double() {
        let expr = Expr::Arith(
            ArithOp::Add,
            Box::new(Expr::Column(0)),
            Box::new(Expr::Column(1)),
        );
        let fast = compile(&expr, &[StaticType::Int, StaticType::Int]).unwrap();
        let batch = Batch::new(
            1,
            vec![
                Vector::Const(Datum::Int(i64::MAX)),
                Vector::Const(Datum::Int(1)),
            ],
        );
        let value = fast.value(&batch, 0).unwrap();
        assert!(matches!(value, Datum::Real(_)), "{value:?}");
        assert_eq!(value.as_f64().unwrap(), i64::MAX as f64 + 1.0);
    }

    /// Three-valued logic follows SQL: `FALSE AND NULL` is FALSE, and
    /// `TRUE OR NULL` is TRUE, but `TRUE AND NULL` is NULL.
    #[test]
    fn three_valued_logic_is_sql_logic() {
        let and = compile(
            &Expr::And(Box::new(Expr::Column(0)), Box::new(Expr::Column(1))),
            &[StaticType::Unknown, StaticType::Unknown],
        )
        .unwrap();
        let or = compile(
            &Expr::Or(Box::new(Expr::Column(0)), Box::new(Expr::Column(1))),
            &[StaticType::Unknown, StaticType::Unknown],
        )
        .unwrap();
        let cases = [
            (Datum::Int(0), Datum::Null, Some(0i64), None),
            (Datum::Null, Datum::Int(0), Some(0), None),
            (Datum::Int(1), Datum::Null, None, Some(1)),
            (Datum::Null, Datum::Int(1), None, Some(1)),
            (Datum::Int(1), Datum::Int(1), Some(1), Some(1)),
            (Datum::Int(0), Datum::Int(0), Some(0), Some(0)),
            (Datum::Null, Datum::Null, None, None),
        ];
        for (left, right, wanted_and, wanted_or) in cases {
            let batch = Batch::new(1, vec![Vector::Const(left), Vector::Const(right)]);
            assert_eq!(
                and.value(&batch, 0).unwrap().as_int(),
                wanted_and,
                "AND {left:?} {right:?}"
            );
            assert_eq!(
                or.value(&batch, 0).unwrap().as_int(),
                wanted_or,
                "OR {left:?} {right:?}"
            );
        }
    }

    /// `IS NULL` is never NULL, whatever its operand is.
    #[test]
    fn null_tests_are_two_valued() {
        let is_null = compile(
            &Expr::IsNull(Box::new(Expr::Column(0))),
            &[StaticType::Unknown],
        )
        .unwrap();
        let is_not = compile(
            &Expr::IsNotNull(Box::new(Expr::Column(0))),
            &[StaticType::Unknown],
        )
        .unwrap();
        for value in [Datum::Null, Datum::Int(0), Datum::Text(b"")] {
            let batch = Batch::new(1, vec![Vector::Const(value)]);
            let a = is_null.value(&batch, 0).unwrap().as_int().unwrap();
            let b = is_not.value(&batch, 0).unwrap().as_int().unwrap();
            assert_eq!(a + b, 1, "{value:?}");
        }
    }

    /// `length` counts characters in text and bytes in a blob, as the dialect
    /// does.
    #[test]
    fn length_counts_characters_not_bytes() {
        let length = compile(
            &Expr::Length(Box::new(Expr::Column(0))),
            &[StaticType::Unknown],
        )
        .unwrap();
        let cases: [(Datum<'_>, Option<i64>); 5] = [
            (Datum::Null, None),
            (Datum::Text(b"abc"), Some(3)),
            // Three characters, seven bytes.
            (Datum::Text("aé漢".as_bytes()), Some(3)),
            (Datum::Blob(&[1, 2, 3, 4]), Some(4)),
            (Datum::Int(-1234), Some(5)),
        ];
        for (value, wanted) in cases {
            let batch = Batch::new(1, vec![Vector::Const(value)]);
            assert_eq!(
                length.value(&batch, 0).unwrap().as_int(),
                wanted,
                "{value:?}"
            );
        }
    }

    /// A text value's numeric prefix is read the way SQLite reads it.
    #[test]
    fn the_numeric_prefix_is_the_dialect_prefix() {
        assert_eq!(prefix_number(b"12abc"), 12.0);
        assert_eq!(prefix_number(b"  -3.5xyz"), -3.5);
        assert_eq!(prefix_number(b"1e3"), 1000.0);
        assert_eq!(prefix_number(b"abc"), 0.0);
        assert_eq!(prefix_number(b""), 0.0);
        assert_eq!(prefix_number(b"."), 0.0);
        assert_eq!(prefix_number(b"1.2.3"), 1.2);
        assert_eq!(prefix_number(b"+7"), 7.0);
    }

    /// A bare column reference reports which column it is, so the aggregate
    /// operators can take their whole-column path.
    #[test]
    fn a_column_reference_names_its_column() {
        let compiled = compile(&Expr::Column(3), &[StaticType::Int; 4]).unwrap();
        assert_eq!(compiled.column(), Some(3));
        let other = compile(
            &Expr::IsNull(Box::new(Expr::Column(3))),
            &[StaticType::Int; 4],
        )
        .unwrap();
        assert_eq!(other.column(), None);
    }
}
