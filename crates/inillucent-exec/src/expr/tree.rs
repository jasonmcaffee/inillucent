//! What a compiled expression is, before it is compiled.
//!
//! Invariant: **`Expr` is a data definition and holds no behaviour.** The
//! planner builds one, `compile` turns it into a closure, and nothing in
//! between evaluates anything - which is what lets a plan be cached and
//! re-run against different parameters.

use inillucent_base::error::Unwind;
use inillucent_base::DbResult;
use inillucent_sql::ast::{BinaryOp, UnaryOp};
use inillucent_sql::function::{JsonFunc, MathFunc, ScalarFunc, TimeFunc};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_value::affinity::Affinity;
use inillucent_value::collation::Collation;
use inillucent_value::value::Value;

use crate::batch::Batch;

use super::*;

/// A compiled expression node.
///
/// Object-safe despite the lifetime parameter on `value`, because a lifetime
/// One expression's answer for one row: a borrow of a page, or a new value.
///
/// A scan hands the executor values that *live in a leaf*, and everything the
/// design is about depends on not copying them. But a scalar function
/// **constructs** its answer - `substr(label, 1, 4)` is bytes that were not in
/// the page and `printf` is bytes that were nowhere - so the return type has to
/// admit both. Making it one or the other would pick a wrong side: forcing
/// every answer to be owned puts an allocation on the scan's hot path, and
/// forcing every answer to be borrowed makes the built-in function set
/// unexpressible.
///
/// The cost of the enum is one discriminant on a value that was going to be
/// matched on anyway. The cost of *not* having it is measured in the shape of
/// the alternative, which is why the borrowed variant carries `Datum<'p>` and
/// not `&'p Datum`: a borrowed answer is still one word plus a tag and copies
/// like one.
#[derive(Clone, Debug)]
pub enum Computed<'p> {
    /// A value borrowed from a pinned page, or a scalar that owns nothing.
    Borrowed(Datum<'p>),
    /// A value this expression built.
    Owned(OwnedDatum),
}
impl<'p> Computed<'p> {
    /// Returns the value, borrowing whichever half holds it.
    ///
    /// The lifetime is the *borrow of self* rather than `'p`, because an owned
    /// answer lives in this object. A caller that needs the value to outlive
    /// the `Computed` keeps the `Computed`, which is what every operator that
    /// builds a batch out of computed columns does.
    pub fn get(&self) -> Datum<'_> {
        match self {
            Computed::Borrowed(value) => *value,
            Computed::Owned(value) => value.borrow(),
        }
    }

    /// Returns the value as an owned one, copying only when it has to.
    pub fn into_owned(self) -> OwnedDatum {
        match self {
            Computed::Borrowed(value) => OwnedDatum::from_datum(&value),
            Computed::Owned(value) => value,
        }
    }

    /// Reports whether the answer is NULL.
    pub fn is_null(&self) -> bool {
        match self {
            Computed::Borrowed(value) => value.is_null(),
            Computed::Owned(OwnedDatum::Null) => true,
            Computed::Owned(_) => false,
        }
    }

    /// Returns the answer as an integer, when it is one.
    pub fn as_int(&self) -> Option<i64> {
        self.get().as_int()
    }
}
impl<'p> From<Datum<'p>> for Computed<'p> {
    fn from(value: Datum<'p>) -> Computed<'p> {
        Computed::Borrowed(value)
    }
}
impl From<OwnedDatum> for Computed<'_> {
    fn from(value: OwnedDatum) -> Computed<'static> {
        Computed::Owned(value)
    }
}
/// parameter on a method is allowed on a trait object where a type parameter is
/// not. That is what lets a node hand back a value borrowing the page.
pub trait Eval: Send + Sync {
    /// Evaluates this expression for one live row of a batch.
    ///
    /// @param batch - the batch being evaluated
    /// @param nth - the position among the batch's live rows
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>>;

    /// Returns the column this reads, when it is a bare column reference.
    ///
    /// The aggregate operators use this to take a whole-column fast path: a
    /// `sum` over a bare column reference reads the mini-column's bytes rather
    /// than calling `value` per row.
    fn column(&self) -> Option<usize> {
        None
    }
}
/// What an application-defined scalar does with one row's arguments.
///
/// A newtype rather than a bare `Arc` for one reason: [`Expr`] derives `Debug`
/// and [`crate::aggregate::AggregateKind`] derives `Eq` as well, and a closure
/// has neither. Two registrations are the same registration when they are the
/// same allocation, which is the only comparison that means anything about a
/// function nobody here wrote.
///
/// The inner type is the one `inillucent-ext` names. It is spelled again here
/// rather than imported because `inillucent-exec` sits *beside* that crate in
/// the layering, not above it, and an `Arc<dyn Fn(..)>` is structural - the
/// pointer the engine hands over is this type whichever crate spells it.
#[derive(Clone)]
pub struct ScalarBody(
    pub std::sync::Arc<dyn Fn(&[Value<'static>]) -> DbResult<Value<'static>> + Send + Sync>,
);
impl core::fmt::Debug for ScalarBody {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        out.write_str("a registered scalar")
    }
}
impl PartialEq for ScalarBody {
    fn eq(&self, other: &ScalarBody) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ScalarBody {}
/// What an application-defined aggregate does with a whole group.
///
/// Every row of the group, in order, rather than a running accumulator - see
/// [`crate::aggregate::AggregateKind::External`] for why.
#[derive(Clone)]
pub struct AggregateBody(pub std::sync::Arc<AggregateFn>);
impl core::fmt::Debug for AggregateBody {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        out.write_str("a registered aggregate")
    }
}
impl PartialEq for AggregateBody {
    fn eq(&self, other: &AggregateBody) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for AggregateBody {}
/// The expression language the physical planner hands to the compiler.
#[derive(Clone, Debug)]
pub enum Expr {
    /// One column of the input batch.
    Column(usize),
    /// `sqlite_offset(X)`: where in the file the row holding X lives.
    ///
    /// The boundaries are one entry per leaf - the lowest key on it and the
    /// offset of its page - built while the statement was prepared. A lookup is
    /// a binary search, so the cost per row is a handful of comparisons rather
    /// than a descent.
    RowOffset {
        /// The leaf boundaries, sorted by key.
        boundaries: std::sync::Arc<Vec<(i64, i64)>>,
        /// The row's key.
        rowid: Box<Expr>,
    },
    /// A constant.
    Literal(OwnedDatum),
    /// One of the statement's bound parameters, read when it is evaluated.
    ///
    /// **The node that lets a compiled chain outlive the values it was built
    /// for.** `translate` used to answer `?2` with an `Expr::Literal` holding
    /// whatever was bound at the time, so a chain was correct only for that one
    /// execution - which is why nothing on the execution path could keep one.
    /// This reads the cell instead, and the cell is refreshed before each run.
    ///
    /// It costs a repeated `Literal` nothing it did not already cost: a text or
    /// blob literal clones per evaluation too, because the node owns the bytes
    /// and the signature promises `'p`. A number does not clone either way.
    Parameter {
        /// The one-based parameter number, as the SQL wrote it.
        index: u32,
        /// The values the statement is running against.
        bound: crate::physical::Bindings,
    },
    /// A call to a scalar an application registered.
    ///
    /// The body rather than the name, resolved when the chain was built: a
    /// bound tree that carried a closure would depend on who was holding it,
    /// and a compiled chain that looked the name up per row would answer a
    /// registration made after it was compiled. Registering or removing a
    /// function throws the compiled statements away, which is what makes
    /// resolving once correct.
    External {
        /// What it does.
        body: ScalarBody,
        /// The arguments, in written order.
        arguments: Vec<Expr>,
    },
    /// `RAISE(ABORT|FAIL|ROLLBACK, 'message')`, which never returns a value.
    ///
    /// It sits in a result column because that is where the grammar puts it -
    /// `SELECT RAISE(ABORT, 'FOREIGN KEY constraint failed') WHERE NOT EXISTS
    /// (...)` is the whole body of every foreign-key check trigger the binder
    /// synthesises - and evaluating it is the error. So the node is a value in
    /// the type system and a failure in practice, which is exactly what the
    /// virtual machine's `HaltError` is.
    ///
    /// `RAISE(IGNORE)` is not here: it does not fail, it abandons the row, and
    /// the firing point rather than the expression is what can do that. It
    /// arrives as [`RAISE_IGNORE`] and is caught there.
    Raise {
        /// The extended result code, which says which constraint asked.
        code: i32,
        /// The message the caller sees.
        message: Vec<u8>,
        /// How much of what has been written the action undoes.
        ///
        /// The whole of the difference between `RAISE(ABORT)`, `RAISE(FAIL)`
        /// and `RAISE(ROLLBACK)`: they report the same code and the same
        /// message and differ only here. It used to be dropped, so
        /// all three behaved as `RAISE(ABORT)` - which itself did not abort.
        unwind: Unwind,
    },
    /// Addition, subtraction, multiplication.
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    /// A comparison with no affinity conversion and BINARY collation.
    Compare(CompareOp, Box<Expr>, Box<Expr>),
    /// A comparison that applies an affinity, a collation, or both.
    ///
    /// SQLite converts both operands to a common affinity before comparing -
    /// `WHERE id = ?1` against an `INTEGER PRIMARY KEY` gives the parameter
    /// integer affinity, so binding `'42'` finds row 42 - and compares text
    /// under the column's collation. An executor that ignored either would be
    /// *quietly wrong* rather than incomplete, which is why this is a distinct
    /// variant rather than a flag on the one above: the plain comparison stays
    /// a two-branch fast path, and anything with a conversion in it goes
    /// through `inillucent-value`'s own rules rather than a second copy of them.
    CompareWith {
        /// Which comparison.
        op: CompareOp,
        /// The affinity applied to both sides first, if any.
        affinity: Option<Affinity>,
        /// The collation text is compared under.
        collation: Collation,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
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
    ///
    /// It has a node of its own rather than going through [`Expr::Call`]
    /// because it reads the leaf's bytes in place where the general path copies
    /// them into a `Value` first, and `range.lookaside` calls it once per row
    /// over a hundred thousand rows.
    Length(Box<Expr>),
    /// A call to one of the dialect's scalar functions.
    Call {
        /// Which function.
        func: ScalarFunc,
        /// The arguments.
        arguments: Vec<Expr>,
        /// The collation the function's comparisons use.
        collation: Collation,
        /// What the connection's counters said when the statement began.
        ///
        /// Read once here for the same reason [`Expr::Time`]'s `now` is: every
        /// `changes()` in one statement is the same number, because SQLite
        /// moves the counters when a statement *finishes*. Reading it per row
        /// in the node would be a different answer wearing the same name - and
        /// leaving it out is what made all four of them answer `0` for ever.
        context: crate::scalar::Context,
    },
    /// A call to one of the math functions.
    Math {
        /// Which function.
        func: MathFunc,
        /// The arguments.
        arguments: Vec<Expr>,
    },
    /// A call to one of the date and time functions.
    Time {
        /// Which function.
        func: TimeFunc,
        /// The arguments.
        arguments: Vec<Expr>,
        /// The julian day the statement calls "now", fixed for the statement.
        now: f64,
    },
    /// An arithmetic, bitwise or concatenation operator of any kind.
    General {
        /// Which operator.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
        /// The largest value this connection admits, in bytes.
        ///
        /// See `crate::scalar::GeneralArith`: `||` is the one operator that
        /// builds a value out of two others, so it is the one that can produce
        /// something past `Limit::Length` with no function being called.
        length_limit: i64,
    },
    /// A call to one of the JSON built-ins, or `->` / `->>`.
    ///
    /// Its own variant rather than a `Call`, for the reason `JsonFunc` is its
    /// own enum in the binder: a JSON function's answer carries a *subtype* -
    /// whether the value is JSON - and the plain function path has nowhere to
    /// put one. The subtype decides whether `json_extract(json_object(...))`
    /// re-parses its argument as text or reads it as a document.
    Json {
        /// Which function.
        func: JsonFunc,
        /// The arguments.
        arguments: Vec<Expr>,
    },
    /// A unary operator.
    Unary {
        /// Which operator.
        op: UnaryOp,
        /// The operand.
        operand: Box<Expr>,
    },
    /// `CAST(x AS type)`.
    Cast {
        /// The operand.
        operand: Box<Expr>,
        /// The affinity the declared type maps to.
        affinity: Affinity,
    },
    /// A comparison's affinity conversion, applied on its own.
    ///
    /// **Not a `CAST`, and the difference is the whole reason it exists.** A
    /// `CAST('abc' AS INTEGER)` is `0`; applying integer *affinity* to `'abc'`
    /// leaves it as `'abc'`, because affinity converts only what converts
    /// losslessly. `CompareWith` does this to both operands before comparing,
    /// so anything that has to reproduce that comparison's *equality* by another
    /// route - hashing it, for one - has to do the same conversion first, and
    /// doing it with a `CAST` would put `'abc'` and `0` in one bucket.
    ///
    /// Its one caller is the automatic index; see `crate::autoindex`.
    Affinity {
        /// The operand.
        operand: Box<Expr>,
        /// The affinity the comparison applies.
        affinity: Affinity,
    },
    /// `IS` / `IS NOT`.
    Is {
        /// Whether `NOT` was written.
        negated: bool,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
        /// The affinity applied before comparing.
        affinity: Option<Affinity>,
        /// The collation the comparison uses.
        collation: Collation,
    },
    /// `BETWEEN`.
    Between {
        /// Whether `NOT` was written.
        negated: bool,
        /// The value being tested.
        operand: Box<Expr>,
        /// The lower bound.
        low: Box<Expr>,
        /// The upper bound.
        high: Box<Expr>,
        /// The affinity applied to the comparisons.
        affinity: Option<Affinity>,
        /// The collation the comparisons use.
        collation: Collation,
    },
    /// `IN` over a value list.
    InList {
        /// Whether `NOT` was written.
        negated: bool,
        /// The value being tested.
        operand: Box<Expr>,
        /// The list.
        list: Vec<Expr>,
        /// The affinity applied before comparing.
        affinity: Option<Affinity>,
        /// The collation the comparison uses.
        collation: Collation,
    },
    /// `CASE`, in both its forms.
    Case {
        /// The base operand, when the form has one.
        operand: Option<Box<Expr>>,
        /// The `WHEN`/`THEN` pairs.
        branches: Vec<(Expr, Expr)>,
        /// The `ELSE` arm.
        otherwise: Option<Box<Expr>>,
        /// The collation comparisons in the base form use.
        collation: Collation,
    },
    /// `LIKE` or `GLOB`.
    Pattern {
        /// Whether `NOT` was written.
        negated: bool,
        /// Which operator.
        kind: crate::scalar::PatternKind,
        /// The value being matched.
        operand: Box<Expr>,
        /// The pattern.
        pattern: Box<Expr>,
        /// The `ESCAPE` argument.
        escape: Option<Box<Expr>>,
        /// Whether `LIKE` compares ASCII letters exactly.
        ///
        /// `PRAGMA case_sensitive_like`, read from the catalog at translation.
        case_sensitive: bool,
    },
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
        Expr::RowOffset { boundaries, rowid } => Box::new(RowOffsetOf {
            boundaries: std::sync::Arc::clone(boundaries),
            rowid: compile(rowid, types)?,
        }),
        Expr::Literal(value) => Box::new(Literal {
            value: value.clone(),
        }),
        Expr::Parameter { index, bound } => Box::new(ParamRef {
            at: index.saturating_sub(1) as usize,
            bound: std::sync::Arc::clone(bound),
        }),
        Expr::External { body, arguments } => Box::new(ExternalCall {
            body: body.clone(),
            arguments: compile_all(arguments, types)?,
        }),
        Expr::Raise {
            code,
            message,
            unwind,
        } => Box::new(Raise {
            code: *code,
            message: String::from_utf8_lossy(message).into_owned(),
            unwind: *unwind,
        }),
        Expr::CompareWith {
            op,
            affinity,
            collation,
            left,
            right,
        } => Box::new(AffinityCompare {
            op: *op,
            affinity: *affinity,
            collation: *collation,
            left: compile(left, types)?,
            right: compile(right, types)?,
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
        Expr::Call {
            func,
            arguments,
            collation,
            context,
        } => Box::new(crate::scalar::ScalarCall {
            func: *func,
            arguments: compile_all(arguments, types)?,
            collation: *collation,
            context: *context,
            stream: std::sync::atomic::AtomicU64::new(context.seed),
        }),
        Expr::Json { func, arguments } => {
            Box::new(crate::scalar::compile_json(*func, arguments, types)?)
        }
        Expr::Math { func, arguments } => Box::new(crate::scalar::MathCall {
            func: *func,
            arguments: compile_all(arguments, types)?,
        }),
        Expr::Time {
            func,
            arguments,
            now,
        } => time_call(*func, arguments, *now, types)?,
        Expr::General {
            op,
            left,
            right,
            length_limit,
        } => general_arith(*op, left, right, *length_limit, types)?,
        Expr::Unary { op, operand } => Box::new(crate::scalar::Unary {
            op: *op,
            operand: compile(operand, types)?,
        }),
        Expr::Affinity { operand, affinity } => Box::new(ApplyAffinity {
            operand: compile(operand, types)?,
            affinity: *affinity,
        }),
        Expr::Cast { operand, affinity } => Box::new(crate::scalar::Cast {
            operand: compile(operand, types)?,
            affinity: *affinity,
        }),
        Expr::Is {
            negated,
            left,
            right,
            affinity,
            collation,
        } => Box::new(crate::scalar::IsTest {
            negated: *negated,
            left: compile(left, types)?,
            right: compile(right, types)?,
            affinity: *affinity,
            collation: *collation,
        }),
        Expr::Between {
            negated,
            operand,
            low,
            high,
            affinity,
            collation,
        } => Box::new(crate::scalar::Between {
            negated: *negated,
            operand: compile(operand, types)?,
            low: compile(low, types)?,
            high: compile(high, types)?,
            affinity: *affinity,
            collation: *collation,
        }),
        Expr::InList {
            negated,
            operand,
            list,
            affinity,
            collation,
        } => Box::new(crate::scalar::InList {
            negated: *negated,
            operand: compile(operand, types)?,
            list: compile_all(list, types)?,
            affinity: *affinity,
            collation: *collation,
        }),
        Expr::Case {
            operand,
            branches,
            otherwise,
            collation,
        } => {
            let mut compiled = Vec::with_capacity(branches.len());
            for (when, then) in branches {
                compiled.push((compile(when, types)?, compile(then, types)?));
            }
            Box::new(crate::scalar::Case {
                operand: match operand {
                    Some(operand) => Some(compile(operand, types)?),
                    None => None,
                },
                branches: compiled,
                otherwise: match otherwise {
                    Some(otherwise) => Some(compile(otherwise, types)?),
                    None => None,
                },
                collation: *collation,
            })
        }
        Expr::Pattern {
            negated,
            kind,
            operand,
            pattern,
            escape,
            case_sensitive,
        } => Box::new(crate::scalar::Pattern {
            negated: *negated,
            kind: *kind,
            operand: compile(operand, types)?,
            pattern: compile(pattern, types)?,
            escape: match escape {
                Some(escape) => Some(compile(escape, types)?),
                None => None,
            },
            case_sensitive: *case_sensitive,
        }),
    })
}
/// Compiles a list of expressions.
///
/// @param exprs - the expressions to compile
/// @param types - the static type of each input column
/// Compiles a call to one of the date and time built-ins.
///
/// Its own function for the reason `general_arith` below is: `compile` has a
/// recorded length in `crates/inillucent-compat/tests/policy.rs`, and an arm
/// that is three fields wide is one of the cheapest to lift out of it.
///
/// @param func - which function
/// @param arguments - the compiled arguments
/// @param now - the julian day the statement calls "now"
/// @param types - the static types of the columns in scope
fn time_call(
    func: TimeFunc,
    arguments: &[Expr],
    now: f64,
    types: &[StaticType],
) -> DbResult<Box<dyn Eval>> {
    Ok(Box::new(crate::scalar::TimeCall {
        func,
        arguments: compile_all(arguments, types)?,
        now,
    }))
}

/// Compiles an arithmetic, bitwise or concatenation operator.
///
/// Its own function only because `compile` has a recorded length in
/// `crates/inillucent-compat/tests/policy.rs` and this arm is the one that
/// grew a field (task-1980). What the field is for is in
/// `crate::scalar::GeneralArith`.
///
/// @param op - which operator
/// @param left - the left operand
/// @param right - the right operand
/// @param length_limit - the largest value this connection admits, in bytes
/// @param types - the static types of the columns in scope
fn general_arith(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    length_limit: i64,
    types: &[StaticType],
) -> DbResult<Box<dyn Eval>> {
    Ok(Box::new(crate::scalar::GeneralArith {
        length_limit,
        op,
        left: compile(left, types)?,
        right: compile(right, types)?,
    }))
}

fn compile_all(exprs: &[Expr], types: &[StaticType]) -> DbResult<Vec<Box<dyn Eval>>> {
    exprs.iter().map(|expr| compile(expr, types)).collect()
}
/// Returns what the compiler can prove about an expression's type.
///
/// @param expr - the expression to inspect
/// @param types - the static type of each input column
pub fn static_type(expr: &Expr, types: &[StaticType]) -> StaticType {
    match expr {
        Expr::Column(index) => column_type(*index, types),
        // Nothing is proved about a value that is never produced, nor about
        // one somebody else's code returns.
        // A parameter's type is a property of the value bound to it, which is
        // not known while the chain is being built and may differ on the next
        // execution. `Unknown` costs a generic node; a claim would cost a wrong
        // answer the first time a caller bound a string to a slot that had held
        // a number.
        Expr::Raise { .. } | Expr::External { .. } | Expr::Parameter { .. } => StaticType::Unknown,
        Expr::Literal(OwnedDatum::Int(_)) => StaticType::Int,
        // A file offset is an integer or it is NULL, which is the same thing a
        // rowid column proves and is what makes it comparable without a cast.
        Expr::RowOffset { .. } => StaticType::Unknown,
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
        | Expr::CompareWith { .. }
        | Expr::And(..)
        | Expr::Or(..)
        | Expr::Not(..)
        | Expr::IsNull(..)
        | Expr::IsNotNull(..)
        | Expr::Length(..)
        | Expr::Is { .. }
        | Expr::Between { .. }
        | Expr::InList { .. }
        | Expr::Pattern { .. } => StaticType::Int,
        // A function's result type is a question about the function and its
        // arguments, and claiming an answer here would be claiming one the
        // compiler cannot check. `Unknown` costs a generic node; a wrong claim
        // costs a wrong answer.
        Expr::Call { .. }
        | Expr::Json { .. }
        | Expr::Math { .. }
        | Expr::Time { .. }
        | Expr::General { .. }
        | Expr::Unary { .. }
        | Expr::Cast { .. }
        | Expr::Affinity { .. }
        | Expr::Case { .. } => StaticType::Unknown,
    }
}
/// Returns one input column's static type.
///
/// @param index - which column
/// @param types - the static type of each input column
fn column_type(index: usize, types: &[StaticType]) -> StaticType {
    types.get(index).copied().unwrap_or(StaticType::Unknown)
}
