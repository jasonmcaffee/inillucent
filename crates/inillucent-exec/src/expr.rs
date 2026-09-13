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

use inillucent_base::error::Unwind;
use inillucent_base::DbResult;
use inillucent_sql::ast::{BinaryOp, UnaryOp};
use inillucent_sql::function::{JsonFunc, MathFunc, ScalarFunc, TimeFunc};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_value::affinity::{self, Affinity};
use inillucent_value::collation::Collation;
use inillucent_value::compare::compare_sql;
use inillucent_value::encoding::TextEncoding;
use inillucent_value::value::{BlobValue, TextValue, Value};

use crate::batch::Batch;

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

/// What a registered aggregate is: every row of the group in, one value out.
pub type AggregateFn = dyn Fn(&[Vec<Value<'static>>]) -> DbResult<Value<'static>> + Send + Sync;

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
        } => Box::new(crate::scalar::TimeCall {
            func: *func,
            arguments: compile_all(arguments, types)?,
            now: *now,
        }),
        Expr::General { op, left, right } => Box::new(crate::scalar::GeneralArith {
            op: *op,
            left: compile(left, types)?,
            right: compile(right, types)?,
        }),
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

/// A bare column reference.
struct ColumnRef {
    index: usize,
}

impl Eval for ColumnRef {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        Ok(Computed::Borrowed(batch.value(nth, self.index)?))
    }

    fn column(&self) -> Option<usize> {
        Some(self.index)
    }
}

/// `sqlite_offset(X)`: where in the file the row holding X lives.
struct RowOffsetOf {
    /// One entry per leaf: the lowest key on it and the offset of its page.
    boundaries: std::sync::Arc<Vec<(i64, i64)>>,
    /// The row's key.
    rowid: Box<dyn Eval>,
}

impl Eval for RowOffsetOf {
    /// Returns the offset of the page the row is on, or NULL when there is
    /// none - a row with no integer key is a row this cannot be asked about,
    /// which is what SQLite answers NULL for too.
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let Some(rowid) = self.rowid.value(batch, nth)?.get().as_int() else {
            return Ok(Computed::Owned(OwnedDatum::Null));
        };
        // The last boundary at or below the key: a leaf covers a contiguous run
        // of keys, so that is the leaf the row is on.
        let at = match self
            .boundaries
            .binary_search_by_key(&rowid, |(key, _)| *key)
        {
            Ok(found) => found,
            Err(0) => return Ok(Computed::Owned(OwnedDatum::Null)),
            Err(after) => after.saturating_sub(1),
        };
        match self.boundaries.get(at) {
            Some((_, offset)) => Ok(Computed::Owned(OwnedDatum::Int(*offset))),
            None => Ok(Computed::Owned(OwnedDatum::Null)),
        }
    }
}

/// A constant.
struct Literal {
    value: OwnedDatum,
}

impl Eval for Literal {
    fn value<'p>(&self, _batch: &Batch<'p>, _nth: usize) -> DbResult<Computed<'p>> {
        // Phase 1 could not return a text literal at all: the node owns the
        // bytes and the signature promised `'p`, so a text literal was compared
        // in place and refused anywhere else. `Computed` is what removes the
        // restriction - an owned answer is one of the two things the type
        // admits - and a text literal in a select list now works.
        Ok(match &self.value {
            OwnedDatum::Null => Computed::Borrowed(Datum::Null),
            OwnedDatum::Int(number) => Computed::Borrowed(Datum::Int(*number)),
            OwnedDatum::Real(number) => Computed::Borrowed(Datum::Real(*number)),
            owned => Computed::Owned(owned.clone()),
        })
    }
}

/// One of the statement's bound parameters, read when it is evaluated.
///
/// See [`Expr::Parameter`]. A missing slot reads as NULL, which is what an
/// unbound parameter is.
struct ParamRef {
    /// Where the value sits, zero-based.
    at: usize,
    /// The values the statement is running against.
    bound: crate::physical::Bindings,
}

impl Eval for ParamRef {
    fn value<'p>(&self, _batch: &Batch<'p>, _nth: usize) -> DbResult<Computed<'p>> {
        // The same three arms `Literal` has, and for the same reason: a number
        // is copied out of the cell and owes nothing to it, and a text or a blob
        // cannot be borrowed past the guard so it is cloned.
        let Ok(held) = self.bound.lock() else {
            return Ok(Computed::Borrowed(Datum::Null));
        };
        Ok(match held.get(self.at) {
            None | Some(OwnedDatum::Null) => Computed::Borrowed(Datum::Null),
            Some(OwnedDatum::Int(number)) => Computed::Borrowed(Datum::Int(*number)),
            Some(OwnedDatum::Real(number)) => Computed::Borrowed(Datum::Real(*number)),
            Some(owned) => Computed::Owned(owned.clone()),
        })
    }
}

/// A call to a scalar an application registered.
///
/// The arguments are materialised into owned `Value`s before the call, because
/// the body is somebody else's code and may hold them for as long as it likes -
/// handing it a borrow of a pinned page would be handing it a borrow of a frame
/// the pool may evict.
struct ExternalCall {
    /// What it does.
    body: ScalarBody,
    /// The compiled arguments.
    arguments: Vec<Box<dyn Eval>>,
}

impl Eval for ExternalCall {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let mut values = Vec::with_capacity(self.arguments.len());
        for argument in &self.arguments {
            values.push(crate::scalar::to_value(argument.value(batch, nth)?.get()));
        }
        let answer = (self.body.0)(&values)?;
        Ok(Computed::Owned(crate::scalar::from_value(answer)))
    }
}

/// The message a `RAISE(IGNORE)` reports.
///
/// **It is a control transfer wearing an error's clothes**, and it is never
/// seen by a caller: `RAISE(IGNORE)` in a trigger body means "stop this trigger
/// and abandon the row the write is on", which no expression can do on its own
/// because an expression does not know what a row is. So it travels as a
/// failure with this exact text, and the trigger firing point - the one place
/// that does know - catches it and skips the row.
///
/// A `RAISE(IGNORE)` outside a trigger body is a parse error, so there is no
/// path by which this can escape to an application.
pub const RAISE_IGNORE: &str = "inillucent: RAISE(IGNORE)";

/// `RAISE(...)`: an expression whose evaluation is the failure.
struct Raise {
    /// The extended result code.
    code: i32,
    /// The message.
    message: String,
    /// What the action undoes.
    unwind: Unwind,
}

impl Eval for Raise {
    fn value<'p>(&self, _batch: &Batch<'p>, _nth: usize) -> DbResult<Computed<'p>> {
        Err(
            inillucent_base::error::DbError::new(inillucent_base::error::ExtendedCode(self.code))
                .with_message(self.message.clone())
                // Written out, so nothing overrides it: a trigger body's
                // `RAISE(ROLLBACK)` rolls the transaction back whatever the
                // statement that fired the trigger asked for.
                .with_raised_unwind(self.unwind),
        )
    }
}

/// Integer arithmetic, both operands proved integral.
struct IntArith {
    op: ArithOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for IntArith {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Computed::Borrowed(Datum::Null));
        }
        // The static type is a claim the binder derived from column *affinity*,
        // and a dynamically typed engine can hold a row that violates it - a
        // string or a real in a column declared INTEGER, which the leaf records
        // as an exception. Producing NULL for those rows would make the
        // specialisation faster and wrong, which is the one thing it may not be,
        // so it falls back to the generic arithmetic instead.
        let (Some(a), Some(b)) = (left.as_int(), right.as_int()) else {
            return generic_arith(self.op, &left.get(), &right.get());
        };
        Ok(Computed::Borrowed(integer_arith(self.op, a, b)))
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
            real_or_null(match op {
                ArithOp::Add => a + b,
                ArithOp::Subtract => a - b,
                ArithOp::Multiply => a * b,
            })
        }
    }
}

/// Returns a double, or NULL when the arithmetic had no answer.
///
/// **SQLite has no NaN.** `1e999 - 1e999` is NULL there and was `NaN` here - a
/// value that is not equal to itself, that no comparison orders and that no
/// caller of an arithmetic expression has any way to handle. The same rule
/// already covered division (`0.0/0.0` is NULL); this is the other operators,
/// which reach it only through an infinity.
///
/// @param value - the computed double
fn real_or_null<'p>(value: f64) -> Datum<'p> {
    if value.is_nan() {
        return Datum::Null;
    }
    Datum::Real(value)
}

/// Applies an arithmetic operator to two values of any class.
///
/// @param op - the operator
/// @param left - the left operand
/// @param right - the right operand
pub(crate) fn generic_arith<'p>(
    op: ArithOp,
    left: &Datum<'_>,
    right: &Datum<'_>,
) -> DbResult<Computed<'p>> {
    if left.is_null() || right.is_null() {
        return Ok(Computed::Borrowed(Datum::Null));
    }
    // **A text or blob operand converts to an integer where SQLite makes one.**
    // `x'00' + x'00'` is `0` there and was `0.0` here, and `'abc' + 1` was
    // `1.0` against `1`: the conversion went straight to a double, so every
    // sum with a non-numeric operand in it came out a real. SQLite's rule is
    // the one `CAST(x AS NUMERIC)` follows - integral text becomes an integer,
    // text with a point or an exponent becomes a real, and text that is not a
    // number at all becomes the integer zero - and the class it produces is
    // the class of the answer.
    let held = (numeric_datum(left), numeric_datum(right));
    let (left, right) = (&held.0, &held.1);
    if let (Some(a), Some(b)) = (left.as_int(), right.as_int()) {
        return Ok(Computed::Borrowed(integer_arith(op, a, b)));
    }
    let (a, b) = (numeric(left), numeric(right));
    Ok(Computed::Borrowed(real_or_null(match op {
        ArithOp::Add => a + b,
        ArithOp::Subtract => a - b,
        ArithOp::Multiply => a * b,
    })))
}

/// Returns a value as the number SQLite's arithmetic reads it as.
///
/// An integer or a real stays what it is; a text or blob becomes an integer
/// when its numeric prefix is integral and fits, a real when it is not, and the
/// integer zero when there is no numeric prefix at all - which is what makes
/// `x'00' + x'00'` an integer.
///
/// @param value - the operand
fn numeric_datum<'p>(value: &Datum<'_>) -> Datum<'p> {
    let (Datum::Text(bytes) | Datum::Blob(bytes)) = value else {
        return match value {
            Datum::Int(number) => Datum::Int(*number),
            Datum::Real(number) => Datum::Real(*number),
            _ => Datum::Null,
        };
    };
    if let Some(number) = prefix_integer(bytes) {
        return Datum::Int(number);
    }
    Datum::Real(prefix_number(bytes))
}

/// Returns the numeric prefix as an integer, when it is one.
///
/// `None` when the prefix carries a point or an exponent, or when the digits do
/// not fit an `i64` - both of which are the cases SQLite reads as a real.
///
/// @param bytes - the string to read
fn prefix_integer(bytes: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(bytes).ok()?.trim_start();
    let raw = text.as_bytes();
    let mut end = 0usize;
    let mut seen_digit = false;
    for (index, byte) in raw.iter().enumerate() {
        let accept = match byte {
            b'0'..=b'9' => {
                seen_digit = true;
                true
            }
            b'+' | b'-' => index == 0,
            // A point or an exponent means the value is a real, so there is no
            // integer prefix to read - not a shorter one.
            b'.' | b'e' | b'E' if seen_digit => return None,
            _ => false,
        };
        if !accept {
            break;
        }
        end = index.saturating_add(1);
    }
    if !seen_digit {
        // No number at all, which SQLite reads as the integer zero.
        return Some(0);
    }
    text.get(..end)
        .and_then(|prefix| prefix.parse::<i64>().ok())
}

/// Arithmetic over anything.
struct GenericArith {
    op: ArithOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for GenericArith {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        generic_arith(self.op, &left.get(), &right.get())
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
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let value = batch.value(nth, self.column)?;
        Ok(Computed::Borrowed(match value.as_int() {
            Some(number) => Datum::Int(i64::from(self.op.holds(number.cmp(&self.constant)))),
            None if value.is_null() => Datum::Null,
            // The column's affinity said integer and the row holds something
            // else - an exception row. Fall back to the dialect's comparison
            // rather than guessing, so the specialisation stays a speed change.
            None => Datum::Int(i64::from(
                self.op.holds(value.compare(&Datum::Int(self.constant))),
            )),
        }))
    }
}

/// A comparison of two proved-integral expressions.
struct IntCompare {
    op: CompareOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for IntCompare {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Computed::Borrowed(Datum::Null));
        }
        Ok(Computed::Borrowed(Datum::Int(i64::from(
            self.op.holds(left.get().compare(&right.get())),
        ))))
    }
}

/// A comparison of anything, by the dialect's rules.
struct GenericCompare {
    op: CompareOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for GenericCompare {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Computed::Borrowed(Datum::Null));
        }
        Ok(Computed::Borrowed(Datum::Int(i64::from(
            self.op.holds(left.get().compare(&right.get())),
        ))))
    }
}

/// A comparison that applies an affinity and a collation.
///
/// The conversion and the comparison are `inillucent-value`'s, not a second
/// implementation of them: `apply_affinity` and `compare_sql` are the functions
/// the old engine used and the ones the affinity tests are written against. The
/// only thing here is the bridge from a borrowed page value to a `Value` and
/// back to a truth.
struct AffinityCompare {
    op: CompareOp,
    affinity: Option<Affinity>,
    collation: Collation,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for AffinityCompare {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Computed::Borrowed(Datum::Null));
        }
        let (left, right) = (left.get(), right.get());
        let (left, right) = (as_value(&left), as_value(&right));
        let (left, right) = match self.affinity {
            None => (left, right),
            Some(affinity) => (
                affinity::apply_affinity(left, affinity, TextEncoding::Utf8).unwrap_or(Value::Null),
                affinity::apply_affinity(right, affinity, TextEncoding::Utf8)
                    .unwrap_or(Value::Null),
            ),
        };
        let order = compare_sql(&left, &right, self.collation);
        match order.ordering() {
            Some(order) => Ok(Computed::Borrowed(Datum::Int(i64::from(
                self.op.holds(order),
            )))),
            None => Ok(Computed::Borrowed(Datum::Null)),
        }
    }
}

/// Returns a borrowed page value as a `inillucent-value` value.
///
/// The database encoding is UTF-8 and only UTF-8 - the TDD says so in the leaf
/// layout - so the encoding argument is a constant rather than a parameter
/// nobody could vary.
///
/// @param datum - the value read out of a batch
fn as_value<'p>(datum: &Datum<'p>) -> Value<'p> {
    match datum {
        Datum::Null => Value::Null,
        Datum::Int(number) => Value::Integer(*number),
        Datum::Real(number) => Value::Real(*number),
        Datum::Text(bytes) => Value::Text(TextValue::utf8(bytes)),
        Datum::Blob(bytes) => Value::Blob(BlobValue::borrowed(bytes)),
    }
}

/// `AND` with three-valued logic.
struct Conjunction {
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for Conjunction {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        // FALSE AND anything is FALSE, even when the other side is NULL, so a
        // definite false short-circuits.
        if truth(&left.get()) == Some(false) {
            return Ok(Computed::Borrowed(Datum::Int(0)));
        }
        let right = self.right.value(batch, nth)?;
        Ok(Computed::Borrowed(
            match (truth(&left.get()), truth(&right.get())) {
                (_, Some(false)) => Datum::Int(0),
                (Some(true), Some(true)) => Datum::Int(1),
                _ => Datum::Null,
            },
        ))
    }
}

/// `OR` with three-valued logic.
struct Disjunction {
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for Disjunction {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        if truth(&left.get()) == Some(true) {
            return Ok(Computed::Borrowed(Datum::Int(1)));
        }
        let right = self.right.value(batch, nth)?;
        Ok(Computed::Borrowed(
            match (truth(&left.get()), truth(&right.get())) {
                (_, Some(true)) => Datum::Int(1),
                (Some(false), Some(false)) => Datum::Int(0),
                _ => Datum::Null,
            },
        ))
    }
}

/// `NOT` with three-valued logic.
struct Negation {
    inner: Box<dyn Eval>,
}

impl Eval for Negation {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let inner = self.inner.value(batch, nth)?;
        Ok(Computed::Borrowed(match truth(&inner.get()) {
            Some(true) => Datum::Int(0),
            Some(false) => Datum::Int(1),
            None => Datum::Null,
        }))
    }
}

/// `IS NULL` and `IS NOT NULL`, which are never NULL themselves.
struct NullTest {
    inner: Box<dyn Eval>,
    wanted: bool,
}

impl Eval for NullTest {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let is_null = self.inner.value(batch, nth)?.is_null();
        Ok(Computed::Borrowed(Datum::Int(i64::from(
            is_null == self.wanted,
        ))))
    }
}

/// `length(x)`: characters for text, bytes for a blob, NULL for NULL.
struct Length {
    inner: Box<dyn Eval>,
}

impl Eval for Length {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let inner = self.inner.value(batch, nth)?;
        Ok(Computed::Borrowed(match inner.get() {
            Datum::Null => Datum::Null,
            // SQLite counts characters in text and bytes in a blob. The
            // database encoding is UTF-8, so a character is a non-continuation
            // byte - and the count stops at the first NUL, which is SQLite's
            // documented definition rather than an accident of C strings:
            // `length(char(0))` is 0 and `length(char(65,0,66))` is 1, while
            // `hex()` of the same values shows every byte is still there.
            Datum::Text(bytes) => {
                let counted = match bytes.iter().position(|byte| *byte == 0) {
                    Some(at) => bytes.get(..at).unwrap_or(bytes),
                    None => bytes,
                };
                Datum::Int(counted.iter().filter(|byte| (*byte & 0xC0) != 0x80).count() as i64)
            }
            Datum::Blob(bytes) => Datum::Int(bytes.len() as i64),
            Datum::Int(number) => Datum::Int(number.to_string().len() as i64),
            Datum::Real(number) => Datum::Int(format_real(number).len() as i64),
        }))
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
/// One line, delegating, and it stays that way. What was here before was a
/// hand-rolled approximation - `{:.1}` for whole numbers under 1e15 and Rust's
/// `Display` for everything else - sitting a crate away from
/// [`inillucent_value::numeric::real_to_text`], which is a transcription of
/// SQLite's `%!.17g` down to the double rounding and the round-trip
/// shortening.
///
/// The two agreed on every value anybody had thought to test and disagreed on
/// the ones nobody had. Rust's `Display` renders `1e300` as three hundred and
/// one digits where SQLite renders `1.0e+300`, so `length(score)` came back as
/// 302 instead of 9 and `group_concat` produced a line of zeros; and `{:.1}`
/// renders `-0.0` as `-0.0` where SQLite deliberately drops the sign, because
/// it decides with `r < 0.0` and `-0.0 < 0.0` is false.
///
/// A generated differential sweep found both in its first run. Neither would
/// ever have been found by reading the code, because the code looked right.
///
/// @param number - the value to render
pub fn format_real(number: f64) -> String {
    String::from_utf8_lossy(&inillucent_value::numeric::real_to_text(number)).into_owned()
}

/// Applies a comparison's affinity to one value.
///
/// See [`Expr::Affinity`] for why this is not a cast.
struct ApplyAffinity {
    /// What to convert.
    operand: Box<dyn Eval>,
    /// The conversion.
    affinity: Affinity,
}

impl Eval for ApplyAffinity {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let value = self.operand.value(batch, nth)?;
        let converted = inillucent_value::affinity::apply_affinity(
            crate::scalar::to_value(value.get()),
            self.affinity,
            inillucent_value::TextEncoding::Utf8,
        )
        .unwrap_or(inillucent_value::Value::Null);
        Ok(Computed::Owned(crate::scalar::from_value(converted)))
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
                        a.get().compare(&b.get()),
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
                        a.get().compare(&b.get()),
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
        assert!(matches!(value.get(), Datum::Real(_)), "{value:?}");
        assert_eq!(value.get().as_f64().unwrap(), i64::MAX as f64 + 1.0);
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
