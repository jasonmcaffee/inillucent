//! The binder: names to columns, and the bound relational tree.
//!
//! Invariant: the binder is a pure function of one SQL text and one immutable
//! catalog snapshot. It resolves every name, expands every star, decides every
//! affinity and collation, and extracts every aggregate, and it does all of
//! that before a single page is read. A bound statement therefore says exactly
//! what it will touch, which is what lets the authorizer run here rather than
//! part-way through execution.
//!
//! Resolution order is SQLite's: FROM terms left to right, then result aliases
//! where SQLite permits them, with a column always preferred over an alias of
//! the same name. `rowid`, `_rowid_` and `oid` resolve only on a rowid table
//! and only when no real column shadows them.

mod cte;
mod refusal;
// The refusals live in `bind/refusal.rs` and are named here so every call
// site reads as it did. See that file for why they moved.
pub(crate) use refusal::{
    ambiguous_column, compound_order_unmatched, no_query_solution, no_such_collation,
    no_such_column, no_such_column_quoted, no_such_function, no_such_index, no_such_table,
    order_out_of_range, schema_refused, unsupported, wrong_arguments,
};
mod aggregate;
mod collation;
mod having;
mod literal;
mod rowvalue;
mod scratch;

use collation::{apply_collation, explicit_argument_collation};
pub use collation::{comparison_rules, result_collation};
use literal::integer_literal;

pub use cte::CteBinding;
use cte::RecursiveTarget;
pub use scratch::BinderScratch;

use inillucent_value::{Affinity, Collation};

use crate::ast::{
    self, Ast, BinaryOp, CompoundOp, Expr, ExprId, FromSource, InRhs, JoinConstraint, JoinKind,
    Literal, NullOrder, PatternOp, SelectBody, SelectId, SortOrder, UnaryOp,
};
use crate::ast::{FrameBound, FrameExclude, FrameUnit};
use crate::catalog_view::{CatalogView, ColumnInfo, TableInfo, TableKind};
use crate::diagnostic::{ParseError, ParseErrorKind};
use crate::function::{self, AggregateFunc, JsonFunc, MathFunc, ScalarFunc, TimeFunc, WindowFunc};
use crate::lexer::{QuoteForm, Span};

/// What an authorizer decided about one action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authorization {
    /// The action is allowed.
    Allow,
    /// The action is refused and the statement fails.
    Deny,
    /// The action is allowed but the column reads as NULL.
    Ignore,
}

/// One action an authorizer is asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthAction<'a> {
    /// Reading a column of a table.
    Read {
        /// The database name.
        database: &'a [u8],
        /// The table name.
        table: &'a [u8],
        /// The column name.
        column: &'a [u8],
    },
    /// Running a SELECT at all.
    Select,
    /// Calling a function.
    Function {
        /// The function name.
        name: &'a [u8],
    },
}

/// The callback the binder consults before it binds an action.
pub trait Authorizer {
    /// Returns what to do about one action.
    fn authorize(&self, action: AuthAction<'_>) -> Authorization;

    /// Reports whether this authorizer allows every action unconditionally.
    ///
    /// A plan cache may only reuse a compiled program when re-running the
    /// authorizer could not have changed the outcome, and the only authorizer
    /// that is true of is one that allows everything. Defaulting to `false`
    /// means an application's authorizer opts out by doing nothing, which is
    /// the safe direction: a new authorizer that forgot to answer this question
    /// gets its callbacks, it does not get silently skipped.
    fn allows_everything(&self) -> bool {
        false
    }
}

/// An authorizer that allows everything, which is the default.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

/// Where a result column came from: database, table, and column name.
///
/// Absent for an expression, which has no single column behind it - which is
/// exactly what `sqlite3_column_database_name` and its two siblings report.
pub type ColumnOrigin = (Vec<u8>, Vec<u8>, Vec<u8>);

impl Authorizer for AllowAll {
    /// Reports that nothing this authorizer is asked can be refused.
    fn allows_everything(&self) -> bool {
        true
    }

    /// Allows every action.
    fn authorize(&self, _action: AuthAction<'_>) -> Authorization {
        Authorization::Allow
    }
}

/// What a nested query used as a value does with its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubqueryKind {
    /// `EXISTS (...)`: true when the block produced a row.
    Exists,
    /// `(SELECT ...)` in a value position: the first row's first column, or
    /// NULL when it produced nothing.
    Scalar,
    /// The right side of an `IN`.
    In,
}

/// A bound expression, with every name resolved and every rule decided.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundExpr {
    /// A NULL literal.
    Null,
    /// An integer literal.
    Integer(i64),
    /// A real literal.
    Real(f64),
    /// A text literal.
    Text(Vec<u8>),
    /// A blob literal.
    Blob(Vec<u8>),
    /// A bound parameter.
    Parameter(u32),
    /// `RAISE(...)` inside a trigger body.
    ///
    /// It is an expression in the grammar and it never produces a value: every
    /// action either stops the statement or abandons the row. It is bound as one
    /// anyway because that is where it is written - `SELECT RAISE(ABORT, 'no')
    /// WHERE new.x < 0` puts it in a result column, guarded by a WHERE - and a
    /// statement form would not reach that position.
    Raise {
        /// Which action.
        action: crate::ast::RaiseAction,
        /// The message, when the action takes one.
        message: Option<Vec<u8>>,
        /// Whether the abort is a foreign key's rather than a trigger's.
        ///
        /// The two are the same expression and report different codes, and
        /// nothing in the SQL says which: the foreign-key bodies the binder
        /// synthesises set it, and `RAISE` as anybody writes it does not.
        foreign_key: bool,
    },
    /// A column of a FROM term.
    Column {
        /// Which FROM term, by position.
        source: usize,
        /// Which column of it, by declared position.
        column: u16,
        /// Which slot of the row's record holds it.
        ///
        /// Not the same number as the declared position once the table has a
        /// `VIRTUAL` generated column: that column takes no slot, so every
        /// column after it sits one place earlier in the record. Carrying both
        /// is what keeps an index key - which names declared positions - and a
        /// record read - which names slots - from being confused for each
        /// other.
        slot: u16,
        /// The column's affinity.
        affinity: Affinity,
        /// The column's declared collation.
        collation: Collation,
    },
    /// The rowid of a FROM term.
    Rowid {
        /// Which FROM term.
        source: usize,
    },
    /// A call to a function an application registered.
    ///
    /// It carries the name and nothing else: the binder resolved that such a
    /// function exists and takes this many arguments, and the machine looks up
    /// what it does when it runs. A closure in a bound tree would make the tree
    /// depend on who was holding it.
    External {
        /// The folded name.
        name: Vec<u8>,
        /// The arguments, already bound.
        arguments: Vec<BoundExpr>,
    },
    /// One of a module's auxiliary functions, written `f(table, ...)`.
    ///
    /// It reads the module's cursor rather than a column, which is why it
    /// names a FROM term instead of taking the table as an argument: `bm25`
    /// wants to know which phrase matched where in the row the cursor is on,
    /// and no column carries that.
    VirtualFunction {
        /// Which FROM term - the virtual table the call is about.
        source: usize,
        /// The function's folded name, for the module to recognise.
        name: Vec<u8>,
        /// The arguments after the table.
        arguments: Vec<BoundExpr>,
    },
    /// A unary operator.
    Unary {
        /// Which operator.
        op: UnaryOp,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// An arithmetic, bitwise or concatenation operator.
    Arithmetic {
        /// Which operator.
        op: BinaryOp,
        /// The left operand.
        left: Box<BoundExpr>,
        /// The right operand.
        right: Box<BoundExpr>,
    },
    /// A comparison, with the affinity and collation it applies.
    Compare {
        /// Which comparison.
        op: BinaryOp,
        /// The left operand.
        left: Box<BoundExpr>,
        /// The right operand.
        right: Box<BoundExpr>,
        /// The affinity applied to both sides before comparing.
        affinity: Option<Affinity>,
        /// The collation the comparison uses.
        collation: Collation,
    },
    /// `AND`, with three-valued semantics.
    And(Box<BoundExpr>, Box<BoundExpr>),
    /// `OR`, with three-valued semantics.
    Or(Box<BoundExpr>, Box<BoundExpr>),
    /// `NOT`.
    Not(Box<BoundExpr>),
    /// `IS NULL` or `NOT NULL`.
    IsNull {
        /// Whether the test is for not-null.
        negated: bool,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// `IS` / `IS NOT`, which never yields NULL.
    Is {
        /// Whether `NOT` was written.
        negated: bool,
        /// The left operand.
        left: Box<BoundExpr>,
        /// The right operand.
        right: Box<BoundExpr>,
        /// The affinity applied before comparing.
        affinity: Option<Affinity>,
        /// The collation the comparison uses.
        collation: Collation,
    },
    /// `BETWEEN`, kept as one node so its operand is evaluated once.
    ///
    /// **Each bound has its own affinity and collation (task-2088).** SQLite
    /// codes `x BETWEEN lo AND hi` as `x >= lo AND x <= hi`, and each of those
    /// comparisons takes its rules from its own two operands. One pair of rules
    /// taken from `x` and `lo` ignored `hi` entirely: measured against 3.53.4,
    /// `s BETWEEN 'a' AND 'B' COLLATE NOCASE` returned no rows where SQLite
    /// returns `a` and `b`, and `'5' BETWEEN 1 AND CAST('9' AS INTEGER)`
    /// answered 0 where SQLite applies the upper bound's INTEGER affinity and
    /// answers 1.
    Between {
        /// Whether `NOT` was written.
        negated: bool,
        /// The value being tested.
        operand: Box<BoundExpr>,
        /// The lower bound.
        low: Box<BoundExpr>,
        /// The upper bound.
        high: Box<BoundExpr>,
        /// The affinity `operand >= low` applies.
        low_affinity: Option<Affinity>,
        /// The collation `operand >= low` uses.
        low_collation: Collation,
        /// The affinity `operand <= high` applies.
        high_affinity: Option<Affinity>,
        /// The collation `operand <= high` uses.
        high_collation: Collation,
    },
    /// `IN` over a value list.
    InList {
        /// Whether `NOT` was written.
        negated: bool,
        /// The value being tested.
        operand: Box<BoundExpr>,
        /// The list.
        list: Vec<BoundExpr>,
        /// The affinity applied before comparing.
        affinity: Option<Affinity>,
        /// The collation the comparison uses.
        collation: Collation,
    },
    /// `CASE`.
    Case {
        /// The base operand, when the form has one.
        operand: Option<Box<BoundExpr>>,
        /// The `WHEN`/`THEN` pairs.
        branches: Vec<(BoundExpr, BoundExpr)>,
        /// The `ELSE` arm.
        otherwise: Option<Box<BoundExpr>>,
        /// The affinity and collation each `WHEN` comparison uses in the base
        /// form, one per branch, and empty in the searched form.
        ///
        /// SQLite codes `CASE x WHEN y` as `x = y` for each branch, so each
        /// comparison takes its rules from `x` and its own `y` through
        /// [`comparison_rules`]. One collation taken from `x` for every branch
        /// made `CASE 'a' WHEN 'A' COLLATE NOCASE` answer 0 where 3.53.4
        /// answers 1, and no affinity made `CASE id WHEN '1'` answer 0 on an
        /// INTEGER column where 3.53.4 answers 1 (task-2094).
        comparisons: Vec<(Option<Affinity>, Collation)>,
    },
    /// `CAST`.
    Cast {
        /// The operand.
        operand: Box<BoundExpr>,
        /// The affinity the declared type maps to.
        affinity: Affinity,
    },
    /// `LIKE`, `GLOB`, `REGEXP` or `MATCH`.
    Pattern {
        /// Whether `NOT` was written.
        negated: bool,
        /// Which operator.
        op: PatternOp,
        /// The value being matched.
        operand: Box<BoundExpr>,
        /// The pattern.
        pattern: Box<BoundExpr>,
        /// The `ESCAPE` argument.
        escape: Option<Box<BoundExpr>>,
    },
    /// A date or time function call.
    Time {
        /// Which function.
        func: TimeFunc,
        /// The arguments.
        arguments: Vec<BoundExpr>,
    },
    /// A math function call.
    ///
    /// It is its own variant rather than a `Function` with a different tag
    /// because a math function has no collation to carry: none of them
    /// compares anything.
    Math {
        /// Which function.
        func: MathFunc,
        /// The arguments.
        arguments: Vec<BoundExpr>,
    },
    /// A JSON function call.
    ///
    /// Its own variant for the reason `JsonFunc` is its own enum: every one of
    /// these can fail, and every one of them reads the JSON mark its arguments
    /// carry. A `Function` node promises neither.
    Json {
        /// Which function.
        func: JsonFunc,
        /// The arguments.
        arguments: Vec<BoundExpr>,
    },
    /// A scalar function call.
    Function {
        /// Which function.
        func: ScalarFunc,
        /// The arguments.
        arguments: Vec<BoundExpr>,
        /// The collation the function's comparisons use.
        collation: Collation,
    },
    /// A reference to a window value computed for this row.
    WindowRef {
        /// Which window call, by position in the block's list.
        slot: usize,
        /// The explicit collation the call's arguments carry, if one does.
        ///
        /// The arguments live in the block's window list, out of reach of
        /// [`BoundExpr::explicit_collation`], so the binder copies the answer
        /// here (task-2094). The `PARTITION BY` and the `ORDER BY` of the
        /// window do not count: 3.53.4 answers `max(s) OVER (PARTITION BY s
        /// COLLATE NOCASE) = 'C'` with 0.
        collation: Option<Collation>,
    },
    /// A reference to an aggregate accumulator computed for this row group.
    Aggregate {
        /// Which accumulator, by position.
        slot: usize,
        /// The explicit collation the call's arguments carry, if one does.
        ///
        /// SQLite marks the aggregate call `EP_Collate` from its arguments, so
        /// `max(s COLLATE NOCASE) = 'C'` compares with NOCASE. The arguments
        /// live in the binder's aggregate list, out of reach of
        /// [`BoundExpr::explicit_collation`], so the binder copies the answer
        /// here (task-2094). An argument's `ORDER BY` and a `FILTER` do not
        /// count: 3.53.4 answers `group_concat(s ORDER BY s COLLATE NOCASE) =
        /// 'A,A,B,B,C,C'` with 0.
        collation: Option<Collation>,
    },
    /// A column of the current sorter row, used after an ORDER BY sort.
    SorterColumn {
        /// Which column of the sorted record.
        column: u16,
    },
    /// A nested query used as a value: `EXISTS`, a scalar, or the right side
    /// of an `IN`.
    ///
    /// The three are one variant because they differ only in what they do with
    /// the block's rows, and the machinery underneath - a store, filled once or
    /// once per outer row depending on correlation - is identical. Splitting
    /// them would mean three copies of the correlation rule, which is the part
    /// that is easy to get wrong.
    Subquery {
        /// The statement-wide number of this subquery, so the compiler can
        /// build it once even when the expression is compiled twice.
        id: usize,
        /// What the rows are used for.
        kind: SubqueryKind,
        /// Whether `NOT` was written.
        negated: bool,
        /// The left side of an `IN`.
        operand: Option<Box<BoundExpr>>,
        /// The block.
        block: Box<BoundSelect>,
        /// The affinity an `IN` applies to both sides before comparing.
        affinity: Option<Affinity>,
        /// The collation an `IN` compares with.
        collation: Collation,
    },
    /// An explicit `COLLATE` on an expression that is not a column.
    ///
    /// The node exists so the collation survives to the comparison that uses
    /// it. Attaching it only to columns loses `x = 'BLUE' COLLATE BINARY`,
    /// where the operand carrying the collation is a literal - and losing it
    /// means the column's own collation wins and the comparison quietly
    /// answers a different question.
    Collate {
        /// The operand, which evaluates unchanged.
        operand: Box<BoundExpr>,
        /// The collation the operand forces on a comparison.
        collation: Collation,
    },
}

impl BoundExpr {
    /// Returns the affinity this expression has as an operand.
    ///
    /// SQLite's rule: a column has its own affinity, a cast has the cast's, a
    /// parenthesised expression has its operand's, and everything else has
    /// none. "None" is a real answer here, not a missing one.
    pub fn affinity(&self) -> Option<Affinity> {
        match self {
            BoundExpr::Column { affinity, .. } => Some(*affinity),
            BoundExpr::Cast { affinity, .. } => Some(*affinity),
            BoundExpr::Rowid { .. } => Some(Affinity::Integer),
            BoundExpr::Collate { operand, .. } => operand.affinity(),
            _ => None,
        }
    }

    /// Returns whether the expression reads any column or aggregate.
    pub fn is_constant(&self) -> bool {
        match self {
            BoundExpr::Null
            | BoundExpr::Integer(_)
            | BoundExpr::Real(_)
            | BoundExpr::Text(_)
            | BoundExpr::Blob(_)
            | BoundExpr::Parameter(_) => true,
            // RAISE never produces a value, so it is not constant: folding it
            // away would delete the abort it exists to perform.
            BoundExpr::Raise { .. }
            | BoundExpr::Column { .. }
            | BoundExpr::Rowid { .. }
            | BoundExpr::External { .. }
            | BoundExpr::VirtualFunction { .. }
            | BoundExpr::Aggregate { .. }
            | BoundExpr::WindowRef { .. }
            | BoundExpr::SorterColumn { .. } => false,
            BoundExpr::Unary { operand, .. } => operand.is_constant(),
            BoundExpr::Collate { operand, .. } => operand.is_constant(),
            BoundExpr::Json { arguments, .. } => arguments.iter().all(BoundExpr::is_constant),
            BoundExpr::Not(operand) => operand.is_constant(),
            BoundExpr::IsNull { operand, .. } => operand.is_constant(),
            BoundExpr::Cast { operand, .. } => operand.is_constant(),
            BoundExpr::Arithmetic { left, right, .. }
            | BoundExpr::Compare { left, right, .. }
            | BoundExpr::Is { left, right, .. } => left.is_constant() && right.is_constant(),
            BoundExpr::And(left, right) | BoundExpr::Or(left, right) => {
                left.is_constant() && right.is_constant()
            }
            BoundExpr::Between {
                operand, low, high, ..
            } => operand.is_constant() && low.is_constant() && high.is_constant(),
            BoundExpr::InList { operand, list, .. } => {
                operand.is_constant() && list.iter().all(BoundExpr::is_constant)
            }
            BoundExpr::Case {
                operand,
                branches,
                otherwise,
                ..
            } => {
                operand.as_ref().is_none_or(|e| e.is_constant())
                    && branches
                        .iter()
                        .all(|(when, then)| when.is_constant() && then.is_constant())
                    && otherwise.as_ref().is_none_or(|e| e.is_constant())
            }
            BoundExpr::Pattern {
                operand,
                pattern,
                escape,
                ..
            } => {
                operand.is_constant()
                    && pattern.is_constant()
                    && escape.as_ref().is_none_or(|e| e.is_constant())
            }
            BoundExpr::Function { arguments, .. }
            | BoundExpr::Math { arguments, .. }
            | BoundExpr::Time { arguments, .. } => arguments.iter().all(BoundExpr::is_constant),
            // A subquery is never constant. It may read no column of the query
            // that encloses it, but it reads the database, and hoisting it out
            // of a loop is the compiler's decision to make from its correlation
            // list rather than one this predicate can make.
            BoundExpr::Subquery { .. } => false,
        }
    }

    /// Returns which declared column positions the expression reads.
    ///
    /// The declared position rather than the record slot, because the callers
    /// that ask - a generated column's dependency order, and the index-key
    /// matcher - both think in declared positions.
    pub fn columns_used(&self, into: &mut Vec<u16>) {
        if let BoundExpr::Column { column, .. } = self {
            if !into.contains(column) {
                into.push(*column);
            }
        }
        for child in self.children() {
            child.columns_used(into);
        }
    }

    /// Returns every sub-expression one expression holds, in no order.
    ///
    /// The match is exhaustive on purpose: there is no `_` arm, so a variant
    /// added later is a compilation error here rather than a silently unvisited
    /// subtree. That matters because the covering-index decision is built on
    /// this walk, and a missed subtree there would be a column read from an
    /// index that does not hold it.
    ///
    /// A subquery's *block* is deliberately not a child. It is a query of its
    /// own with its own FROM terms, and the only thing about it that concerns
    /// an enclosing term is which of that term's columns it correlates to -
    /// which the block records separately and which the caller reads.
    pub fn children(&self) -> Vec<&BoundExpr> {
        match self {
            BoundExpr::Null
            | BoundExpr::Integer(_)
            | BoundExpr::Real(_)
            | BoundExpr::Text(_)
            | BoundExpr::Blob(_)
            | BoundExpr::Parameter(_)
            | BoundExpr::Raise { .. }
            | BoundExpr::Column { .. }
            | BoundExpr::Rowid { .. }
            | BoundExpr::WindowRef { .. }
            | BoundExpr::Aggregate { .. }
            | BoundExpr::SorterColumn { .. } => Vec::new(),
            BoundExpr::Unary { operand, .. }
            | BoundExpr::Not(operand)
            | BoundExpr::IsNull { operand, .. }
            | BoundExpr::Collate { operand, .. }
            | BoundExpr::Cast { operand, .. } => vec![operand],
            BoundExpr::Arithmetic { left, right, .. }
            | BoundExpr::Compare { left, right, .. }
            | BoundExpr::Is { left, right, .. }
            | BoundExpr::And(left, right)
            | BoundExpr::Or(left, right) => vec![left, right],
            BoundExpr::Between {
                operand, low, high, ..
            } => vec![operand, low, high],
            BoundExpr::InList { operand, list, .. } => {
                let mut found: Vec<&BoundExpr> = vec![operand];
                found.extend(list.iter());
                found
            }
            BoundExpr::Case {
                operand,
                branches,
                otherwise,
                ..
            } => {
                let mut found: Vec<&BoundExpr> = Vec::new();
                if let Some(operand) = operand {
                    found.push(operand);
                }
                for (when, then) in branches {
                    found.push(when);
                    found.push(then);
                }
                if let Some(otherwise) = otherwise {
                    found.push(otherwise);
                }
                found
            }
            BoundExpr::Pattern {
                operand,
                pattern,
                escape,
                ..
            } => {
                let mut found: Vec<&BoundExpr> = vec![operand, pattern];
                if let Some(escape) = escape {
                    found.push(escape);
                }
                found
            }
            BoundExpr::External { arguments, .. }
            | BoundExpr::VirtualFunction { arguments, .. }
            | BoundExpr::Function { arguments, .. }
            | BoundExpr::Math { arguments, .. }
            | BoundExpr::Json { arguments, .. }
            | BoundExpr::Time { arguments, .. } => arguments.iter().collect(),
            BoundExpr::Subquery { operand, .. } => operand.iter().map(|held| &**held).collect(),
        }
    }

    /// Returns every sub-expression one expression holds, mutably.
    ///
    /// The mirror of [`BoundExpr::children`], and exhaustive for the same
    /// reason: a variant added later is a compilation error here rather than a
    /// subtree some rewrite silently skips. `crate::rewrite` is the only caller
    /// and the trigger firing point is why it exists - a body's `OLD` and `NEW`
    /// reads are replaced by the values the row actually holds, and one missed
    /// subtree there is a trigger that reads a NULL where a value was.
    ///
    /// A subquery's *block* is not a child here either, for the reason it is
    /// not one there: it is a query of its own. `crate::rewrite` descends into
    /// it separately, because a correlated block is exactly where a foreign
    /// key's `NOT EXISTS (SELECT 1 FROM parent WHERE p.k = NEW.c)` keeps its
    /// `NEW`.
    pub fn children_mut(&mut self) -> Vec<&mut BoundExpr> {
        match self {
            BoundExpr::Null
            | BoundExpr::Integer(_)
            | BoundExpr::Real(_)
            | BoundExpr::Text(_)
            | BoundExpr::Blob(_)
            | BoundExpr::Parameter(_)
            | BoundExpr::Raise { .. }
            | BoundExpr::Column { .. }
            | BoundExpr::Rowid { .. }
            | BoundExpr::WindowRef { .. }
            | BoundExpr::Aggregate { .. }
            | BoundExpr::SorterColumn { .. } => Vec::new(),
            BoundExpr::Unary { operand, .. }
            | BoundExpr::Not(operand)
            | BoundExpr::IsNull { operand, .. }
            | BoundExpr::Collate { operand, .. }
            | BoundExpr::Cast { operand, .. } => vec![operand],
            BoundExpr::Arithmetic { left, right, .. }
            | BoundExpr::Compare { left, right, .. }
            | BoundExpr::Is { left, right, .. }
            | BoundExpr::And(left, right)
            | BoundExpr::Or(left, right) => vec![left, right],
            BoundExpr::Between {
                operand, low, high, ..
            } => vec![operand, low, high],
            BoundExpr::InList { operand, list, .. } => {
                let mut found: Vec<&mut BoundExpr> = vec![operand];
                found.extend(list.iter_mut());
                found
            }
            BoundExpr::Case {
                operand,
                branches,
                otherwise,
                ..
            } => {
                let mut found: Vec<&mut BoundExpr> = Vec::new();
                if let Some(operand) = operand {
                    found.push(operand);
                }
                for (when, then) in branches {
                    found.push(when);
                    found.push(then);
                }
                if let Some(otherwise) = otherwise {
                    found.push(otherwise);
                }
                found
            }
            BoundExpr::Pattern {
                operand,
                pattern,
                escape,
                ..
            } => {
                let mut found: Vec<&mut BoundExpr> = vec![operand, pattern];
                if let Some(escape) = escape {
                    found.push(escape);
                }
                found
            }
            BoundExpr::External { arguments, .. }
            | BoundExpr::VirtualFunction { arguments, .. }
            | BoundExpr::Function { arguments, .. }
            | BoundExpr::Math { arguments, .. }
            | BoundExpr::Json { arguments, .. }
            | BoundExpr::Time { arguments, .. } => arguments.iter_mut().collect(),
            BoundExpr::Subquery { operand, .. } => {
                operand.iter_mut().map(|held| &mut **held).collect()
            }
        }
    }

    /// Returns the block a subquery expression holds, when it is one.
    ///
    /// Separate from [`BoundExpr::children_mut`] because a block is not a
    /// sub-expression: it is a query, with its own FROM terms and its own
    /// scope. A rewrite that treats it as one would run over the wrong tree.
    pub fn block_mut(&mut self) -> Option<&mut BoundSelect> {
        match self {
            BoundExpr::Subquery { block, .. } => Some(block),
            _ => None,
        }
    }

    /// Records which of one FROM term's columns this expression reads.
    ///
    /// A correlated subquery makes the answer unknowable from here - the block
    /// is a query of its own and could read any column of the term it
    /// correlates to - so it is recorded as opaque rather than guessed at.
    /// @param source - the FROM term to look for
    /// @param into - what has been found so far
    pub fn columns_read(&self, source: usize, into: &mut ColumnUse) {
        match self {
            BoundExpr::Column {
                source: held, slot, ..
            } if *held == source => into.add(*slot),
            BoundExpr::Rowid { source: held } if *held == source => into.rowid = true,
            BoundExpr::Subquery { block, .. } if block.correlations.contains(&source) => {
                into.opaque = true;
            }
            BoundExpr::VirtualFunction {
                source: held,
                name,
                arguments,
            } if *held == source => into.add_function(name, arguments),
            _ => {}
        }
        for child in self.children() {
            child.columns_read(source, into);
        }
    }
}

/// Which of one FROM term's columns a query reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColumnUse {
    /// The record slots read, ascending and without duplicates.
    pub columns: Vec<u16>,
    /// Whether the term's rowid is read.
    pub rowid: bool,
    /// Whether something was met whose column reads cannot be enumerated.
    ///
    /// An opaque use is never coverable. It is set rather than ignored because
    /// the whole value of this answer is that it is complete: a covering path
    /// that turned out not to cover a column would read it from an index that
    /// does not hold it.
    pub opaque: bool,
    /// The module's auxiliary functions this term is asked for, in the order
    /// they were met, as a folded name and the arguments after the table.
    ///
    /// `score(t)` and `bm25(t)` read the *cursor* rather than a column, so they
    /// are neither a column read nor an opaque one: the module can answer them
    /// per row, and a materialised virtual scan carries the answers beside the
    /// columns. Recorded here because this is already the answer to "what does
    /// this term have to produce", and a second list would be a second thing
    /// that can disagree with it.
    /// **The arguments, not their count.** `highlight(t, 0, '[', ']')` and
    /// `bm25(t, 10.0, 1.0)` are answered by the module from the cursor, and the
    /// module cannot answer either without the values - which used to be
    /// dropped here and replaced with an empty list at the call, so every
    /// auxiliary function saw no arguments at all. Two calls of one name with
    /// different arguments are also two different answers, so the arguments are
    /// part of what identifies a slot rather than a detail hanging off one.
    pub functions: Vec<(Vec<u8>, Vec<BoundExpr>)>,
}

impl ColumnUse {
    /// Records that one slot is read.
    pub fn add(&mut self, slot: u16) {
        if let Err(position) = self.columns.binary_search(&slot) {
            self.columns.insert(position, slot);
        }
    }

    /// Records that one of the module's auxiliary functions is read.
    ///
    /// @param name - the function's folded name
    /// @param arguments - the arguments after the table
    pub fn add_function(&mut self, name: &[u8], arguments: &[BoundExpr]) {
        let held = (name.to_vec(), arguments.to_vec());
        if !self.functions.contains(&held) {
            self.functions.push(held);
        }
    }

    /// Folds another use into this one.
    pub fn merge(&mut self, other: &ColumnUse) {
        for slot in &other.columns {
            self.add(*slot);
        }
        self.rowid |= other.rowid;
        self.opaque |= other.opaque;
        for (name, arguments) in &other.functions {
            self.add_function(name, arguments);
        }
    }
}

impl BoundExpr {
    /// Returns which FROM terms the expression reads.
    pub fn sources_used(&self, into: &mut Vec<usize>) {
        match self {
            BoundExpr::Column { source, .. } | BoundExpr::Rowid { source }
                if !into.contains(source) =>
            {
                into.push(*source);
            }
            BoundExpr::Unary { operand, .. }
            | BoundExpr::Not(operand)
            | BoundExpr::IsNull { operand, .. }
            | BoundExpr::Collate { operand, .. }
            | BoundExpr::Cast { operand, .. } => operand.sources_used(into),
            BoundExpr::Arithmetic { left, right, .. }
            | BoundExpr::Compare { left, right, .. }
            | BoundExpr::Is { left, right, .. }
            | BoundExpr::And(left, right)
            | BoundExpr::Or(left, right) => {
                left.sources_used(into);
                right.sources_used(into);
            }
            BoundExpr::Between {
                operand, low, high, ..
            } => {
                operand.sources_used(into);
                low.sources_used(into);
                high.sources_used(into);
            }
            BoundExpr::InList { operand, list, .. } => {
                operand.sources_used(into);
                for item in list {
                    item.sources_used(into);
                }
            }
            BoundExpr::Case {
                operand,
                branches,
                otherwise,
                ..
            } => {
                if let Some(operand) = operand {
                    operand.sources_used(into);
                }
                for (when, then) in branches {
                    when.sources_used(into);
                    then.sources_used(into);
                }
                if let Some(otherwise) = otherwise {
                    otherwise.sources_used(into);
                }
            }
            BoundExpr::Pattern {
                operand,
                pattern,
                escape,
                ..
            } => {
                operand.sources_used(into);
                pattern.sources_used(into);
                if let Some(escape) = escape {
                    escape.sources_used(into);
                }
            }
            BoundExpr::Function { arguments, .. }
            | BoundExpr::Math { arguments, .. }
            | BoundExpr::Time { arguments, .. } => {
                for argument in arguments {
                    argument.sources_used(into);
                }
            }
            BoundExpr::Subquery { operand, block, .. } => {
                if let Some(operand) = operand {
                    operand.sources_used(into);
                }
                // The block's correlations are terms of the *enclosing* query,
                // so they decide which loop level the subquery can first be
                // evaluated at. Leaving them out put a correlated `EXISTS`
                // before the loop whose row it reads.
                for source in &block.correlations {
                    if !into.contains(source) {
                        into.push(*source);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Where one FROM term's rows come from.
///
/// A subquery, a view and a CTE are all the same thing to everything below the
/// binder: a block of SQL whose rows are materialised into an ephemeral table
/// and then scanned like any other. Keeping them one variant is what stops the
/// planner and the compiler growing three nearly-identical paths.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceRows {
    /// A real table's B-tree.
    Table,
    /// A nested query, materialised before the loop that scans it.
    Subquery(Box<BoundSelect>),
    /// A recursive CTE, filled by running its seed and then its step arms
    /// until the step arms stop producing rows that are new.
    Recursive(Box<RecursiveBody>),
    /// A reference to the recursive CTE being filled, which stands for exactly
    /// the one row the fill loop is currently on.
    ///
    /// It shares the enclosing CTE's store, so it is not a source that produces
    /// rows of its own: it is a window onto the row the queue is at.
    RecursiveSelf {
        /// The statement-wide number of the CTE term whose store it reads.
        cte: usize,
    },
}

/// A recursive CTE's arms, split by whether they refer to the CTE.
///
/// SQLite's rule is that the arms which do not reference the CTE are its seed
/// and run once, and the arms which do are its step and run against each row
/// the seed and earlier steps produced. Splitting them at bind time rather than
/// at compile time is what lets the compiler emit one queue walk rather than
/// re-deciding per arm what each one is.
#[derive(Clone, Debug, PartialEq)]
pub struct RecursiveBody {
    /// The arms that do not reference the CTE, with the operator before each.
    pub seeds: Vec<(CompoundOp, BoundSelect)>,
    /// The arms that do.
    pub steps: Vec<(CompoundOp, BoundSelect)>,
}

/// One FROM term, bound to a table.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundSource {
    /// The statement-wide number every bound expression refers to it by.
    ///
    /// A block's own position in its FROM clause is not enough: a correlated
    /// subquery reads a column of a term belonging to an enclosing block, and
    /// the two numbering schemes would collide. One number per FROM term in
    /// the whole statement means a column reference is unambiguous wherever it
    /// is evaluated, and the compiler can map it to the cursor that is already
    /// open.
    pub id: usize,
    /// Where the rows come from.
    pub rows: SourceRows,
    /// The table, view or virtual table.
    /// The table this source reads, shared with the catalog rather than copied.
    ///
    /// **It used to be a `TableInfo` by value.** Every table reference
    /// in every statement therefore deep-cloned the catalog's entry - two name
    /// vectors, a `ColumnInfo` per column each with its own heap fields, the
    /// full `CREATE` text, and an `IndexInfo` per index with its own column
    /// vector - which measured at 2,938 ns of `prepare.point`'s 6,093 ns
    /// compile, 48% of it. Every read of it still goes through `Deref`, so
    /// nothing above this line had to change.
    pub table: std::rc::Rc<TableInfo>,
    /// The name the query refers to it by.
    pub alias: Vec<u8>,
    /// The join that attaches it to the term before it.
    pub join: JoinKind,
    /// The join constraint, already desugared from NATURAL and USING.
    pub constraint: Option<BoundExpr>,
    /// Columns suppressed from `*` by a NATURAL or USING join.
    pub suppressed: Vec<u16>,
    /// The expressions this table's partial and expression indexes are built
    /// from, bound against **this term alone**.
    ///
    /// **The planner cannot bind, and the binder is the only thing that can.**
    /// An index's predicate and its expression keys are schema *text*; deciding
    /// whether a query's `WHERE` implies the predicate, or whether a `WHERE`
    /// names the key an index computes, is a comparison between bound
    /// expressions. So they are bound here and carried, in a list that is empty
    /// for every table with neither - which is every table the gate measures,
    /// and the reason this costs a compile nothing.
    ///
    /// They are bound against a scope holding only this term, never against the
    /// statement's whole FROM clause: a predicate reading `b` must mean *this*
    /// table's `b` even when another term in the query has one too. An index
    /// whose expressions do not bind is simply left out, which leaves the
    /// planner unable to choose it - the conservative answer, and the one that
    /// was in force while these forms were refused outright.
    pub index_exprs: Vec<crate::dml::BoundIndexExprs>,
    /// `INDEXED BY name` or `NOT INDEXED`, as the FROM term wrote it.
    ///
    /// **The planner could not see this until task-2066 section 4.4.14.** The
    /// parser built it, `check_index_hint` checked that an `INDEXED BY` named a
    /// real index, and then nothing carried it any further - so both hints were
    /// accepted and ignored. Measured against the pinned 3.53.4 shell on a
    /// 2,000 row table with an index on each of two columns:
    /// `SELECT count(*) FROM h NOT INDEXED WHERE a = 3 AND b = 100` planned as
    /// `SCAN h` there and as `SEARCH h USING INDEX h_b (b=?)` here.
    ///
    /// Both are honoured now. `INDEXED BY` was the second half, in task-2078:
    /// the same statement with `INDEXED BY h_a` planned as
    /// `SEARCH h USING INDEX h_a (a=?)` there and as `h_b` here, and it is
    /// held as the index's folded name rather than as the parser's name id
    /// because the planner has no syntax tree to look the id up in.
    pub index_hint: IndexChoice,
}

/// Which indexes the planner may use for one FROM term.
///
/// SQLite's two clauses are opposite restrictions and the planner reads them
/// in one place, `choose_path`. `NOT INDEXED` takes every index away and leaves
/// the rowid. `INDEXED BY` takes everything *else* away, the rowid and the
/// table scan included: the pinned 3.53.4 shell plans
/// `SELECT * FROM h INDEXED BY h_a WHERE id = 5` as `SCAN h USING INDEX h_a`,
/// a walk of the whole index, with a rowid seek sitting unused beside it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum IndexChoice {
    /// Nothing was written, so every path is a candidate.
    #[default]
    Any,
    /// `NOT INDEXED`: no index, and the rowid is still allowed.
    NotIndexed,
    /// `INDEXED BY name`: that index and nothing else, by its folded name.
    Only(Vec<u8>),
}

/// Refuses a block, or one of its compound arms, that forces an index which
/// cannot answer it.
///
/// Here rather than in the planner because this is the last point with a
/// `Result` to put the refusal in, and every block reaches it: a nested query,
/// a view body and a CTE body are all bound through `bind_select`. The block's
/// sources and its `ORDER BY` and `LIMIT` are attached by now, which the
/// nearest neighbour probe needs.
/// The refusal points at nothing, because SQLite's does not: the pinned 3.53.4
/// shell prints `no query solution` with no caret under the statement.
/// @param bound - the block, with its sources attached
fn refuse_unanswerable_hints(bound: &BoundSelect) -> Result<(), ParseError> {
    let arms = core::iter::once(bound).chain(bound.compounds.iter().map(|(_, arm)| arm));
    for arm in arms {
        if crate::plan::unanswerable_index_hint(arm).is_some() {
            return Err(no_query_solution(Span::default()));
        }
    }
    Ok(())
}

/// One aggregate the statement computes.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundAggregate {
    /// Which aggregate.
    pub func: AggregateFunc,
    /// The name, when the aggregate is one an application registered.
    pub external: Option<Vec<u8>>,
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// The arguments, or empty for `count(*)`.
    pub arguments: Vec<BoundExpr>,
    /// Whether the call was `count(*)`.
    pub star: bool,
    /// The collation the aggregate compares with.
    pub collation: Collation,
    /// The `FILTER (WHERE ...)` clause, when one was written.
    ///
    /// A row the filter does not keep is not folded in at all - it does not
    /// count, it does not sum and it does not appear in a `group_concat`.
    pub filter: Option<BoundExpr>,
    /// The `ORDER BY` written inside the argument list.
    ///
    /// Empty for nearly every call. It matters to the aggregates whose answer
    /// depends on the order the rows arrive in - `group_concat` and the JSON
    /// group aggregates - and SQLite accepts it on any of them.
    pub order_by: Vec<BoundOrderTerm>,
}

/// One result column, after star expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundResultColumn {
    /// The expression.
    pub expr: BoundExpr,
    /// The name the column reports.
    pub name: Vec<u8>,
    /// The table the column came from, when it came from one.
    pub origin: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    /// The declared type the column reports, when it has one.
    pub declared_type: Vec<u8>,
}

/// One `ORDER BY` term, bound.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundOrderTerm {
    /// The expression to sort by.
    pub expr: BoundExpr,
    /// The direction.
    pub order: SortOrder,
    /// Where NULLs sort.
    pub nulls: NullOrder,
    /// The collation the sort compares with.
    pub collation: Collation,
}

/// What a window call computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowCall {
    /// An aggregate, over the frame.
    Aggregate(AggregateFunc),
    /// One of the eleven functions that only exist in a window.
    Plain(WindowFunc),
}

/// One end of a window frame, bound.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundFrameBound {
    /// `UNBOUNDED PRECEDING`.
    UnboundedPreceding,
    /// `expr PRECEDING`.
    Preceding(BoundExpr),
    /// `CURRENT ROW`.
    CurrentRow,
    /// `expr FOLLOWING`.
    Following(BoundExpr),
    /// `UNBOUNDED FOLLOWING`.
    UnboundedFollowing,
}

/// One window function call, with the window it is computed over.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundWindow {
    /// What it computes.
    pub call: WindowCall,
    /// Whether `DISTINCT` was written, which only an aggregate may carry.
    pub distinct: bool,
    /// The collation its comparisons use.
    pub collation: Collation,
    /// The arguments.
    pub arguments: Vec<BoundExpr>,
    /// Whether the call was `count(*)`.
    pub star: bool,
    /// The `FILTER (WHERE ...)` predicate.
    pub filter: Option<BoundExpr>,
    /// `PARTITION BY`.
    pub partition_by: Vec<BoundExpr>,
    /// `ORDER BY`, which also decides the peer groups.
    pub order_by: Vec<BoundOrderTerm>,
    /// The frame unit.
    pub unit: FrameUnit,
    /// The frame start.
    pub start: BoundFrameBound,
    /// The frame end.
    pub end: BoundFrameBound,
    /// The `EXCLUDE` clause.
    pub exclude: FrameExclude,
}

/// A bound SELECT.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundSelect {
    /// The FROM terms, in written order.
    pub sources: Vec<BoundSource>,
    /// The `WHERE` clause.
    pub filter: Option<BoundExpr>,
    /// The `GROUP BY` terms.
    pub group_by: Vec<BoundExpr>,
    /// The `HAVING` clause.
    pub having: Option<BoundExpr>,
    /// The result columns, after star expansion.
    pub columns: Vec<BoundResultColumn>,
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// The `ORDER BY` terms.
    pub order_by: Vec<BoundOrderTerm>,
    /// The `LIMIT` expression.
    pub limit: Option<BoundExpr>,
    /// The `OFFSET` expression.
    pub offset: Option<BoundExpr>,
    /// The aggregates the statement computes.
    pub aggregates: Vec<BoundAggregate>,
    /// The rows of a `VALUES` arm, when the statement is one.
    pub values: Vec<Vec<BoundExpr>>,
    /// The later arms of a compound, each with the operator that joined it.
    ///
    /// When this is not empty, the `order_by`, `limit` and `offset` on *this*
    /// block belong to the compound as a whole rather than to the first arm -
    /// which is exactly SQLite's rule, since an arm of a compound may not
    /// carry its own. `distinct` stays the first arm's own.
    pub compounds: Vec<(CompoundOp, BoundSelect)>,
    /// The window calls the block computes, in the order they were bound.
    pub windows: Vec<BoundWindow>,
    /// The FROM terms belonging to an enclosing block that this one reads.
    ///
    /// A block with an empty list is uncorrelated and can be evaluated once; a
    /// block with a non-empty one has to be re-evaluated for each row of the
    /// outermost term it names. The compiler needs no more than that, because
    /// the outer cursors are still open and positioned when the child runs.
    pub correlations: Vec<usize>,
}

impl BoundSelect {
    /// Returns which of one FROM term's columns this block reads.
    ///
    /// Every expression the block holds is visited, because the question this
    /// answers is whether an index carries everything the query needs from a
    /// table - and a single missed expression would be a column read from an
    /// index that does not hold it. The walk is therefore written to be
    /// obviously complete rather than briefly: every field of the block that
    /// can hold an expression is named here, and `BoundExpr::children` is
    /// exhaustive so a new expression variant is a compilation error rather
    /// than an unvisited subtree.
    ///
    /// Anything it cannot enumerate marks the answer opaque, and an opaque
    /// answer is never coverable. A nested block that correlates to this term
    /// is the case that matters: it is a query of its own and could read any
    /// column of the term it correlates to.
    /// @param source - the statement-wide number of the FROM term
    pub fn columns_read(&self, source: usize) -> ColumnUse {
        let mut used = ColumnUse::default();
        self.gather_columns(source, &mut used);
        used
    }

    /// Adds this block's reads of one FROM term, and its compounds' reads.
    fn gather_columns(&self, source: usize, into: &mut ColumnUse) {
        for term in &self.sources {
            if let Some(constraint) = &term.constraint {
                constraint.columns_read(source, into);
            }
            match &term.rows {
                SourceRows::Table | SourceRows::RecursiveSelf { .. } => {}
                SourceRows::Subquery(block) => {
                    if block.correlations.contains(&source) {
                        into.opaque = true;
                    }
                }
                SourceRows::Recursive(body) => {
                    for (_, arm) in body.seeds.iter().chain(body.steps.iter()) {
                        if arm.correlations.contains(&source) {
                            into.opaque = true;
                        }
                    }
                }
            }
        }
        for expr in self.filter.iter().chain(self.having.iter()) {
            expr.columns_read(source, into);
        }
        for expr in self
            .group_by
            .iter()
            .chain(self.limit.iter())
            .chain(self.offset.iter())
        {
            expr.columns_read(source, into);
        }
        for column in &self.columns {
            column.expr.columns_read(source, into);
        }
        for term in &self.order_by {
            term.expr.columns_read(source, into);
        }
        for aggregate in &self.aggregates {
            for argument in &aggregate.arguments {
                argument.columns_read(source, into);
            }
            // The call's own `FILTER` and `ORDER BY` read the row too. Missing
            // them here would let a covering index be chosen that does not hold
            // a column the filter tests, which reads as a wrong answer rather
            // than as a refusal.
            if let Some(filter) = &aggregate.filter {
                filter.columns_read(source, into);
            }
            for term in &aggregate.order_by {
                term.expr.columns_read(source, into);
            }
        }
        for window in &self.windows {
            for argument in &window.arguments {
                argument.columns_read(source, into);
            }
            if let Some(filter) = &window.filter {
                filter.columns_read(source, into);
            }
            for expr in &window.partition_by {
                expr.columns_read(source, into);
            }
            for term in &window.order_by {
                term.expr.columns_read(source, into);
            }
            // A frame bound is an expression when it is `n PRECEDING`, and a
            // window over a covering index would read it like anything else.
            for bound in [&window.start, &window.end] {
                if let BoundFrameBound::Preceding(expr) | BoundFrameBound::Following(expr) = bound {
                    expr.columns_read(source, into);
                }
            }
        }
        for row in &self.values {
            for expr in row {
                expr.columns_read(source, into);
            }
        }
        for (_, arm) in &self.compounds {
            arm.gather_columns(source, into);
        }
    }

    /// Returns whether the statement aggregates its input into one group or
    /// into groups.
    pub fn is_aggregate(&self) -> bool {
        !self.aggregates.is_empty() || !self.group_by.is_empty()
    }
}

/// Every database and cookie a bound statement depends on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dependencies {
    /// The `(database index, schema cookie)` pairs the statement was bound
    /// against.
    pub schemas: Vec<(usize, u32)>,
    /// The catalog generation the statement was bound against.
    pub generation: u64,
}

/// A bound statement.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundStatement {
    /// A SELECT or VALUES.
    Select(Box<BoundSelect>),
    /// An INSERT or REPLACE.
    Insert(Box<crate::dml::BoundInsert>),
    /// An UPDATE.
    Update(Box<crate::dml::BoundUpdate>),
    /// A DELETE.
    Delete(Box<crate::dml::BoundDelete>),
    /// A statement the session executes itself rather than compiling.
    Directive(Box<crate::directive::Directive>),
    /// A statement that compiles to no program.
    Empty,
}

/// The binder's working state for one statement.
pub struct Binder<'a> {
    pub(crate) catalog: &'a dyn CatalogView,
    pub(crate) ast: &'a Ast,
    /// The statement text the parse came from.
    ///
    /// It is here for one reason: a result column with no alias that is
    /// not a bare column reference is named after the text it was written
    /// as, and the arena holds spans rather than the bytes they cut.
    pub(crate) source: &'a [u8],
    pub(crate) authorizer: &'a dyn Authorizer,
    /// The functions an application registered on this connection.
    ///
    /// Names and arities only - what they do is the machine's business - so a
    /// bound statement stays a pure function of the SQL, the catalog
    /// generation, and this list.
    pub(crate) externals: &'a [function::ExternalFunction],
    /// The collations an application defined on this connection.
    pub(crate) collations: &'a [(String, Collation)],
    /// Whether the expression being bound was written in the schema.
    ///
    /// **The whole of `direct_only` and `innocuous` enforcement (task-1972).**
    /// A `DEFAULT`, a `CHECK`, a generated column's expression, an index
    /// expression, a partial-index predicate, a view's body and a trigger's
    /// body are all strings in a file somebody else may have written, and a
    /// binder with no notion of where it was reading could not tell one from
    /// the statement an application submitted. `Registry::authorize_function`
    /// existed and had no caller for exactly that reason.
    ///
    /// It only ever moves from `Statement` to `Schema`: once inside a schema
    /// expression, everything the binder reaches through it - a view over a
    /// view, a generated column a `CHECK` reads, a subquery in a trigger body -
    /// is schema too, and each of those sites saves and restores this rather
    /// than clearing it.
    pub(crate) call_site: function::CallSite,
    /// Whether the connection trusts the schema it read, which
    /// `PRAGMA trusted_schema` decides.
    ///
    /// It is read with the call site above and nowhere else: a trusted schema
    /// may name a function that is merely not innocuous, and may still not name
    /// a direct-only one.
    pub(crate) trusted_schema: bool,
    pub(crate) sources: Vec<BoundSource>,
    /// One entry per query block currently being bound, innermost last, each
    /// holding the ids of the FROM terms that block owns.
    ///
    /// Resolution walks it from the back, so an inner name shadows an outer one
    /// and a name that only an outer block can satisfy makes the inner block
    /// correlated - which is exactly the information the compiler needs to
    /// decide whether the child runs once or once per outer row.
    pub(crate) scopes: Vec<Vec<usize>>,
    aggregates: Vec<BoundAggregate>,
    result_aliases: Vec<(Vec<u8>, BoundExpr)>,
    /// Whether anything bound after this block's result columns can name one of
    /// them by its alias.
    ///
    /// **Recording an alias costs an allocation per result column, and almost
    /// no statement reads one (task-2026).** `result_aliases` is consulted in
    /// exactly one place - `bind_column_reference`, after a real column has
    /// failed to match - and the only clauses that reach it are `GROUP BY`,
    /// `HAVING` and the statement's `ORDER BY`, `LIMIT` and `OFFSET`, all of
    /// which are bound after the result columns and inside the same block. A
    /// `SELECT` with none of them fills the list and never reads it, which on
    /// `SELECT 1` was a lowercased copy of the name `1`, and on a wider select
    /// is that plus a clone of every result expression.
    ///
    /// It is per block and restored by [`BlockFrame`] for the reason the alias
    /// list itself is: a subquery's tail clauses are its own, and an outer
    /// `ORDER BY` cannot name an inner block's alias.
    ///
    /// It starts `true`, so a binder reached by a path that does not set it
    /// records aliases exactly as it did before.
    tail_may_name_an_alias: bool,
    dependencies: Dependencies,
    inside_aggregate: bool,
    allow_aggregates: bool,
    /// The CTEs visible to the block being bound, innermost `WITH` last.
    pub(crate) ctes: Vec<Vec<CteBinding>>,
    /// The recursive CTEs whose own definition is being bound right now.
    ///
    /// A reference to a name on this stack is the recursion itself, and binding
    /// its definition again would not terminate - which is exactly what it did
    /// before this existed: the depth guard tripped a hundred frames down, in a
    /// function large enough that a hundred frames overflowed the stack.
    recursing: Vec<RecursiveTarget>,
    /// The CTEs being bound as ordinary subqueries right now, innermost last.
    ///
    /// **The guard against a cycle no recursion can carry (task-1913).** A CTE
    /// that names itself somewhere the recursion cannot read it - in a
    /// `WHERE (SELECT ... FROM c)`, or in a body with no compound arm to
    /// separate a seed from a step - used to bind its own definition again, and
    /// again, until the process ran out of stack and died. `inillucent` exited
    /// 127 with `has overflowed its stack` on three one-line queries, which in
    /// a library linked into an application is that application's crash.
    /// SQLite answers `circular reference: c`, and so does this now.
    ///
    /// Held as the definition's own `SelectId` rather than its name, because an
    /// inner `WITH` may bind the same name to a different query and that one is
    /// not a cycle - `WITH c AS (WITH c AS (SELECT 7) SELECT * FROM c)` is an
    /// ordinary query SQLite answers.
    binding_ctes: Vec<ast::SelectId>,
    /// The enclosing FROM terms the block being bound has read.
    correlations: Vec<usize>,
    /// How deep the binder is inside nested query blocks.
    depth: u32,
    /// How many nested queries used as values have been bound so far.
    subqueries: usize,
    /// How deep the binder is inside a generated column's own expression.
    generating: u32,
    /// The window calls bound in the block being bound.
    windows: Vec<BoundWindow>,
    /// The windows the block's `WINDOW` clause named.
    named_windows: Vec<(Vec<u8>, ast::WindowId)>,
    /// The table `excluded` names while an upsert's `DO UPDATE` is bound.
    pub(crate) excluded: Option<crate::catalog_view::TableInfo>,
    /// The row `OLD` and `NEW` name while a trigger body is bound.
    pub(crate) row_aliases: Option<RowAliases>,
    /// The FROM term a write to a view runs against, when the target is one.
    ///
    /// A view has no rows of its own, so an `UPDATE` or `DELETE` on one is
    /// pushed as an ordinary subquery term and the statement's `WHERE` and
    /// `SET` bind against that. Remembering its number is what lets the block
    /// that produces `OLD` be built out of the very same term, with no
    /// re-pointing of anything already bound.
    pub(crate) view_target: Option<usize>,
    /// Whether foreign keys are enforced, which `PRAGMA foreign_keys` decides.
    pub(crate) foreign_keys: bool,
    /// Whether every key's checks wait for the commit, which
    /// `PRAGMA defer_foreign_keys` decides for the transaction.
    pub(crate) defer_foreign_keys: bool,
    /// The synthesised triggers whose bodies are being bound.
    ///
    /// A key that can lead back to its own table would inline its body once per
    /// level the data happens to be deep, which is not knowable when the
    /// statement is compiled. Re-entry stops here instead, and the connection
    /// repeats the action after the statement until nothing changes.
    pub(crate) firing_foreign_keys: Vec<Vec<u8>>,
    /// How many foreign-key action bodies are currently being inlined.
    pub(crate) foreign_key_depth: usize,
    /// How many more foreign-key action bodies may be inlined at all.
    ///
    /// A foreign key's action is inlined rather than called, so a cascade that
    /// can reach the same table again - a tree with `ON DELETE CASCADE` on its
    /// parent column is the everyday case - needs the body once per level it
    /// can reach. An acyclic set of keys never touches this: each level is a
    /// different table and the inlining stops on its own. A cycle spends the
    /// budget, and running out is reported rather than silently leaving the
    /// rows the cascade did not reach.
    pub(crate) foreign_key_budget: usize,
    /// Equalities a table-valued function's arguments implied, waiting to be
    /// ANDed into the block's `WHERE`.
    ///
    /// They cannot be added when the term is bound, because the filter has not
    /// been bound yet and the arguments have to be inside it rather than beside
    /// it: `json_each(x) WHERE key > 1` is one conjunction, not two filters.
    pub(crate) pending_constraints: Vec<BoundExpr>,
    /// The folded names of the triggers whose bodies are being bound, outermost
    /// first.
    ///
    /// SQLite's default is `recursive_triggers = off`, which skips a trigger
    /// that is already on the stack rather than firing it again. Skipping is
    /// also what makes inlining terminate, so the two agree: this list is both
    /// the parity rule and the recursion guard.
    pub(crate) firing: Vec<Vec<u8>>,
    /// How deep `firing` may get, from the connection's `Limit::TriggerDepth`.
    ///
    /// The limit is settable - `.limit trigger_depth 10` and the driver's limit
    /// setter both reach it - so it is a field rather than the constant it used
    /// to be, and the refusal names the number that was in force.
    pub(crate) trigger_depth: usize,
}

/// How deeply query blocks may nest.
///
/// SQLite's own limit is expression depth rather than a separate select depth,
/// but a subquery per level costs a scope, a frame and a compiled subprogram,
/// so the recursion is bounded here where the recursion happens.
pub const MAX_SELECT_DEPTH: u32 = 64;

/// How many arms a compound SELECT may have, which is `SQLITE_MAX_COMPOUND_SELECT`.
pub const MAX_COMPOUND_SELECT: usize = 500;

/// How deep one generated column may reach through others.
///
/// A cycle is refused when the table is created, so this is a second line of
/// defence for a schema that arrived from somewhere else: a file whose
/// `CREATE TABLE` describes a cycle would otherwise recurse until the stack ran
/// out, and a corrupt file must not be able to do that.
pub const MAX_GENERATED_DEPTH: u32 = 32;

/// The source number a column of an upsert's `excluded` row carries.
///
/// It is not a FROM term: `excluded` is the row the INSERT was about to write,
/// which lives in registers rather than under a cursor. Giving it a number no
/// real source can have means the compiler must substitute it - and a compiler
/// that forgot to would try to open a cursor two billion and be refused by the
/// verifier, rather than reading the wrong row.
pub const EXCLUDED_SOURCE: usize = usize::MAX;

/// The source number a column of a trigger's `OLD` row carries.
///
/// Like [`EXCLUDED_SOURCE`], it is not a FROM term: `OLD` and `NEW` are the row
/// the write is about, which the compiler already holds in registers by the
/// time a trigger fires. Numbering them where no real source can reach means a
/// compiler that forgot to substitute one is caught by the verifier rather than
/// quietly reading whatever cursor happened to be open.
pub const OLD_SOURCE: usize = usize::MAX - 1;

/// The source number a column of a trigger's `NEW` row carries.
pub const NEW_SOURCE: usize = usize::MAX - 2;

/// The row a trigger body's `OLD` and `NEW` name.
///
/// Which of the two are in scope is decided by the event: an INSERT has no
/// previous row and a DELETE has no next one, and SQLite refuses the name that
/// does not apply rather than reading NULLs out of it.
#[derive(Clone, Debug)]
pub(crate) struct RowAliases {
    /// The table the trigger is attached to, whose columns the names carry.
    pub(crate) table: crate::catalog_view::TableInfo,
    /// Whether `OLD` is in scope.
    pub(crate) old: bool,
    /// Whether `NEW` is in scope.
    pub(crate) new: bool,
}

impl<'a> Binder<'a> {
    /// Points the binder at the text its parse came from.
    ///
    /// A binder with no source names an unaliased expression column with
    /// the empty string, which is what a nested parse of schema text
    /// wants: those columns are never returned to anybody.
    pub fn with_source(mut self, source: &'a [u8]) -> Binder<'a> {
        self.source = source;
        self
    }

    /// Names the functions an application registered on this connection.
    pub fn with_functions(mut self, functions: &'a [function::ExternalFunction]) -> Binder<'a> {
        self.externals = functions;
        self
    }

    /// Names the collations an application defined on this connection.
    pub fn with_collations(mut self, collations: &'a [(String, Collation)]) -> Binder<'a> {
        self.collations = collations;
        self
    }

    /// Says whether the connection trusts the schema it read.
    ///
    /// `PRAGMA trusted_schema` is the lever, and it is read at bind time, so a
    /// connection that changes it throws its compiled statements away - a plan
    /// bound under one answer is that answer.
    ///
    /// @param trusted - whether a schema may name a function that is not
    ///   innocuous
    pub fn with_trusted_schema(mut self, trusted: bool) -> Binder<'a> {
        self.trusted_schema = trusted;
        self
    }

    /// Binds as though every expression had been written in the schema.
    ///
    /// For a caller that already knows what it is holding is schema text and
    /// has no enclosing statement to inherit the site from: the query
    /// `CREATE INDEX` builds to fill an index on an expression, and the view
    /// body `PRAGMA table_info` binds to find out a view's columns.
    ///
    /// **The index build is why this exists (task-1972).** An index on an
    /// expression is filled by running a `SELECT` the engine writes out of that
    /// expression, and a `SELECT` is a statement - so the build was the one
    /// place a schema expression reached the machine with a statement's
    /// permissions, and `CREATE INDEX i ON t (embed(body))` loaded a 275 MB
    /// model once per row before any later write of the table was refused for
    /// naming it.
    pub fn in_schema(mut self) -> Binder<'a> {
        self.call_site = function::CallSite::Schema;
        self
    }

    /// Returns a binder over one catalog snapshot and one parse.
    pub fn new(
        catalog: &'a dyn CatalogView,
        ast: &'a Ast,
        authorizer: &'a dyn Authorizer,
    ) -> Binder<'a> {
        Binder {
            catalog,
            ast,
            source: &[],
            authorizer,
            externals: &[],
            collations: &[],
            call_site: function::CallSite::Statement,
            // SQLite's default, and `Policy::default()`'s. A connection that
            // wants the stricter stance says so; a binder built with no
            // connection behind it - a test over a hand-built catalog - gets
            // the same answer the engine's default gives.
            trusted_schema: true,
            sources: Vec::new(),
            scopes: Vec::new(),
            aggregates: Vec::new(),
            result_aliases: Vec::new(),
            tail_may_name_an_alias: true,
            dependencies: Dependencies {
                schemas: Vec::new(),
                generation: catalog.generation(),
            },
            inside_aggregate: false,
            allow_aggregates: false,
            ctes: Vec::new(),
            recursing: Vec::new(),
            binding_ctes: Vec::new(),
            correlations: Vec::new(),
            depth: 0,
            subqueries: 0,
            generating: 0,
            windows: Vec::new(),
            named_windows: Vec::new(),
            excluded: None,
            row_aliases: None,
            view_target: None,
            firing: Vec::new(),
            trigger_depth: crate::dml::MAX_TRIGGER_DEPTH,
            pending_constraints: Vec::new(),
            foreign_keys: false,
            defer_foreign_keys: false,
            firing_foreign_keys: Vec::new(),
            foreign_key_depth: 0,
            foreign_key_budget: crate::dml::MAX_FOREIGN_KEY_STATEMENTS,
        }
    }

    /// Names the limits this connection is configured with.
    ///
    /// Only `Limit::TriggerDepth` is read here; the parser reads the rest for
    /// itself. A limit below one would refuse the first trigger of any chain,
    /// which is not what a limit of zero means anywhere else, so it is floored
    /// at one the way `limits.toml`'s own `minimum` says.
    ///
    /// @param limits - the connection's limits
    pub fn with_limits(mut self, limits: &inillucent_base::limits::Limits) -> Binder<'a> {
        let configured = limits.get(inillucent_base::limits::Limit::TriggerDepth);
        self.trigger_depth = configured.max(1) as usize;
        self
    }

    /// Turns foreign-key enforcement on, and says whether it is deferred.
    ///
    /// Off is the default, and it is SQLite's: a constraint that has never been
    /// enforced on an existing database would refuse writes the application has
    /// always made, so the application asks for it.
    pub fn with_foreign_keys(mut self, enforced: bool, deferred: bool) -> Binder<'a> {
        self.foreign_keys = enforced;
        self.defer_foreign_keys = deferred;
        self
    }

    /// Returns what the bound statement depends on.
    pub fn dependencies(&self) -> &Dependencies {
        &self.dependencies
    }

    /// Binds a statement, or reports why it cannot be bound.
    pub fn bind_statement(
        &mut self,
        statement: &ast::Statement,
    ) -> Result<BoundStatement, ParseError> {
        match statement {
            ast::Statement::Empty => Ok(BoundStatement::Empty),
            ast::Statement::Select(select) => {
                let bound = self.bind_select(*select)?;
                Ok(BoundStatement::Select(Box::new(bound)))
            }
            ast::Statement::Insert(insert) => {
                let bound = self.bind_insert(insert)?;
                Ok(BoundStatement::Insert(Box::new(bound)))
            }
            ast::Statement::Update(update) => {
                let bound = self.bind_update(update)?;
                Ok(BoundStatement::Update(Box::new(bound)))
            }
            ast::Statement::Delete(delete) => {
                let bound = self.bind_delete(delete)?;
                Ok(BoundStatement::Delete(Box::new(bound)))
            }
            // `EXPLAIN` is handled a level up, where the inner statement's
            // program is available to render. Reaching it here means a nested
            // one, which SQLite refuses too.
            ast::Statement::Explain { .. } => Err(unsupported("nested EXPLAIN", Span::default())),
            other => {
                let directive = self.bind_directive(other)?;
                Ok(BoundStatement::Directive(Box::new(directive)))
            }
        }
    }

    /// Binds a SELECT, including its `WITH` prefix and every compound arm.
    ///
    /// The block's scope is pushed here rather than in the arm binder because
    /// `ORDER BY` belongs to the statement and resolves in the first arm's
    /// scope: pushing and popping around the arm alone made every qualified
    /// name in an `ORDER BY` report "no such table".
    pub fn bind_select(&mut self, id: SelectId) -> Result<BoundSelect, ParseError> {
        let Some(select) = self.ast.select(id) else {
            return Err(unsupported("missing select", Span::default()));
        };
        if self.authorizer.authorize(AuthAction::Select) == Authorization::Deny {
            return Err(denied("not authorized", select.span));
        }
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_SELECT_DEPTH {
            self.depth = self.depth.saturating_sub(1);
            return Err(ParseError::new(
                ParseErrorKind::Unsupported("too many levels of nested SELECT"),
                select.span,
            ));
        }
        let result = self.bind_select_body(id);
        self.depth = self.depth.saturating_sub(1);
        result
    }

    /// Binds one SELECT's `WITH`, arms and tail clauses.
    fn bind_select_body(&mut self, id: SelectId) -> Result<BoundSelect, ParseError> {
        let Some(select) = self.ast.select(id) else {
            return Err(unsupported("missing select", Span::default()));
        };
        let pushed = self.push_ctes(&select.with)?;
        let bound = self.bind_arms(select);
        if pushed {
            self.ctes.pop();
        }
        bound
    }

    /// Binds the first arm, every compound arm, and the tail clauses.
    fn bind_arms(&mut self, select: &'a ast::Select) -> Result<BoundSelect, ParseError> {
        if select.compounds.len() > MAX_COMPOUND_SELECT {
            return Err(ParseError::new(
                ParseErrorKind::Unsupported("too many terms in compound SELECT"),
                select.span,
            ));
        }
        let frame = self.enter_block();
        // Decided here because this is the only place that holds both the block
        // and the tail clauses bound into it. A compound arm opens its own
        // frame inside `finish_select` and inherits this, which is right: the
        // statement's `ORDER BY` is resolved against the compound's columns
        // rather than through any one arm's aliases, so an arm that inherits a
        // `true` records aliases it will not read, and never the other way.
        self.tail_may_name_an_alias =
            !select.order_by.is_empty() || select.limit.is_some() || select.offset.is_some();
        let bound = self.bind_arm(select.first);
        let mut bound = match bound {
            Ok(bound) => bound,
            Err(reason) => {
                self.leave_block(frame);
                return Err(reason);
            }
        };
        let outcome = self.finish_select(select, &mut bound);
        let ids = self.leave_block(frame);
        outcome?;
        bound.sources = ids
            .iter()
            .filter_map(|id| self.sources.get(*id).cloned())
            .collect();
        refuse_unanswerable_hints(&bound)?;
        Ok(bound)
    }

    /// Binds the compound arms and the tail clauses onto a first arm.
    ///
    /// **An arm goes through [`Binder::bind_isolated_arm`] (task-2042).** A
    /// bare `enter_block` / `bind_arm` / `leave_block` threw the arm's
    /// aggregates and windows away, because `leave_block` restores the
    /// enclosing block's lists, so every arm but the head reached the planner
    /// claiming to compute nothing: refused, or - with a `GROUP BY` on that
    /// arm - one blank row per group. `compound.arm.aggregate` and
    /// `compound.arm.grouped` in `tests/semantics.rs` name both shapes.
    ///
    /// @param select - the statement as written
    /// @param bound - the head arm the arms and clauses are added to
    fn finish_select(
        &mut self,
        select: &'a ast::Select,
        bound: &mut BoundSelect,
    ) -> Result<(), ParseError> {
        for (op, arm) in &select.compounds {
            let armed = self.bind_isolated_arm(*arm)?;
            if armed.columns.len() != bound.columns.len() {
                return Err(ParseError::new(
                    ParseErrorKind::Unsupported(
                        "SELECTs to the left and right of a compound operator do not have the same number of result columns",
                    ),
                    select.span,
                ));
            }
            bound.compounds.push((*op, armed));
        }
        // **The result columns are read where they are, not copied first
        // (task-2026).** `bound` is a parameter rather than a field, so a
        // shared borrow of its columns and the mutable borrow of the binder are
        // two different objects and the compiler accepts both at once. The
        // clone that used to stand here was a `Vec<BoundResultColumn>` plus one
        // allocation for every name, origin and declared type in it - six of
        // the 109 allocations `SELECT a FROM t WHERE id = ?1` made, and two of
        // `SELECT 1`'s 21 - spent to hand `bind_order_by` a copy of something
        // it only reads, on every statement including the ones with no
        // `ORDER BY` at all.
        let order_by = match bound.compounds.is_empty() {
            true => self.bind_order_by(&select.order_by, &bound.columns)?,
            false => self.bind_compound_order_by(&select.order_by, &bound.columns)?,
        };
        bound.order_by = order_by;
        bound.limit = match select.limit {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        bound.offset = match select.offset {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        bound.aggregates = self.aggregates.clone();
        bound.windows = self.windows.clone();
        bound.correlations = self.correlations.clone();
        Ok(())
    }

    /// Binds one arm of a compound: a `SELECT` core or a `VALUES` list.
    fn bind_arm(&mut self, id: ast::SelectCoreId) -> Result<BoundSelect, ParseError> {
        let Some(core) = self.ast.core(id) else {
            return Err(unsupported("missing select core", Span::default()));
        };
        match &core.body {
            SelectBody::Values(rows) => self.bind_values(rows, core.span),
            SelectBody::Select { .. } => self.bind_select_core(id),
        }
    }

    /// Binds a compound's `ORDER BY`, which may only name a result column.
    ///
    /// SQLite resolves a compound's `ORDER BY` against the output of the
    /// compound rather than against any arm's FROM clause, because the arms do
    /// not share one. A term that is neither an ordinal nor the name of a
    /// result column is an error there and is an error here.
    fn bind_compound_order_by(
        &mut self,
        terms: &[ast::OrderTerm],
        columns: &[BoundResultColumn],
    ) -> Result<Vec<BoundOrderTerm>, ParseError> {
        let mut bound = Vec::with_capacity(terms.len());
        for term in terms {
            let span = self.ast.expr_span(term.expr);
            let (target, named) = self.order_term_collation(term.expr, span)?;
            let index = match self.as_ordinal(target) {
                Some(ordinal) => match ordinal.checked_sub(1) {
                    Some(index) if index < columns.len() => index,
                    _ => return Err(order_out_of_range(ordinal, span)),
                },
                None => {
                    let Some(Expr::Column {
                        database: None,
                        table: None,
                        column,
                    }) = self.ast.expr(target)
                    else {
                        return Err(compound_order_unmatched(span));
                    };
                    let folded = self.ast.folded(*column).to_vec();
                    let Some(index) = columns
                        .iter()
                        .position(|candidate| candidate.name.eq_ignore_ascii_case(&folded))
                    else {
                        return Err(compound_order_unmatched(span));
                    };
                    index
                }
            };
            let Some(column) = columns.get(index) else {
                return Err(order_out_of_range(index.saturating_add(1), span));
            };
            // With no `COLLATE` on the term, the result column's own collation
            // governs, read the same way the compound's duplicate removal reads
            // it - an explicit `COLLATE` on the result column beats the implicit
            // one - so the sort and the duplicate removal cannot disagree about
            // a column.
            let collation = named.unwrap_or_else(|| result_collation(&column.expr));
            let nulls = term.nulls.unwrap_or(match term.order {
                SortOrder::Ascending => NullOrder::First,
                SortOrder::Descending => NullOrder::Last,
            });
            bound.push(BoundOrderTerm {
                expr: BoundExpr::SorterColumn {
                    column: index as u16,
                },
                order: term.order,
                nulls,
                collation,
            });
        }
        Ok(bound)
    }

    /// Splits a compound `ORDER BY` term into the term itself and the
    /// collation an explicit `COLLATE` named on it.
    ///
    /// **`UNION ... ORDER BY a COLLATE NOCASE` was a parse error (task-1979,
    /// F15).** A compound's `ORDER BY` may only name a result column, and the
    /// match was made against the term exactly as written, so `a COLLATE
    /// NOCASE` was an `Expr::Collate` rather than an `Expr::Column` and the
    /// term matched nothing. SQLite reads through the `COLLATE`, matches the
    /// name underneath it, and sorts that column with the collation the term
    /// named rather than the one the column carries.
    ///
    /// @param expr - the term as written
    /// @param span - where to point a `no such collation` diagnostic
    fn order_term_collation(
        &self,
        expr: ExprId,
        span: Span,
    ) -> Result<(ExprId, Option<Collation>), ParseError> {
        let Some(Expr::Collate { operand, collation }) = self.ast.expr(expr) else {
            return Ok((expr, None));
        };
        let name = self.ast.text(*collation);
        let Some(named) = self.collation_named(name) else {
            return Err(no_such_collation(name, span));
        };
        Ok((*operand, Some(named)))
    }
    /// Returns the FROM-term ids the innermost block owns.
    pub(crate) fn scope(&self) -> &[usize] {
        self.scopes.last().map_or(&[], |scope| scope.as_slice())
    }

    /// Returns the statement-wide id of the innermost block's nth FROM term.
    fn scope_id(&self, position: usize) -> Option<usize> {
        self.scope().get(position).copied()
    }

    /// Records that the block being bound reads a FROM term it does not own.
    fn note_correlation(&mut self, id: usize) {
        if self.scope().contains(&id) || self.correlations.contains(&id) {
            return;
        }
        self.correlations.push(id);
    }

    /// Binds a `VALUES` arm, which has no FROM and no names to resolve.
    fn bind_values(&mut self, rows: &[Vec<ExprId>], span: Span) -> Result<BoundSelect, ParseError> {
        let mut bound_rows = Vec::with_capacity(rows.len());
        let mut width = 0usize;
        for row in rows {
            let mut values = Vec::with_capacity(row.len());
            for expr in row {
                values.push(self.bind_expr(*expr)?);
            }
            if bound_rows.is_empty() {
                width = values.len();
            } else if values.len() != width {
                return Err(ParseError::new(
                    ParseErrorKind::Unsupported("all VALUES rows must have the same width"),
                    span,
                ));
            }
            bound_rows.push(values);
        }
        let columns = (0..width)
            .map(|index| BoundResultColumn {
                expr: BoundExpr::SorterColumn {
                    column: index as u16,
                },
                name: format!("column{}", index.saturating_add(1)).into_bytes(),
                origin: None,
                declared_type: Vec::new(),
            })
            .collect();
        Ok(BoundSelect {
            sources: Vec::new(),
            filter: None,
            group_by: Vec::new(),
            having: None,
            columns,
            distinct: false,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            aggregates: Vec::new(),
            values: bound_rows,
            compounds: Vec::new(),
            windows: Vec::new(),
            correlations: Vec::new(),
        })
    }

    /// Binds a `SELECT` arm: FROM, WHERE, GROUP BY, HAVING, and the results.
    fn bind_select_core(&mut self, id: ast::SelectCoreId) -> Result<BoundSelect, ParseError> {
        let Some(core) = self.ast.core(id) else {
            return Err(unsupported("missing select core", Span::default()));
        };
        let SelectBody::Select {
            distinct,
            columns,
            from,
            filter,
            group_by,
            having,
            windows,
            ..
        } = &core.body
        else {
            return Err(unsupported("expected a select core", core.span));
        };
        self.declare_windows(windows)?;
        for term in from {
            self.bind_from_term(*term)?;
        }
        self.desugar_join_constraints(from)?;
        let pending = core::mem::take(&mut self.pending_constraints);
        let mut bound_filter = match filter {
            Some(expr) => Some(self.bind_expr(*expr)?),
            None => None,
        };
        for constraint in pending {
            bound_filter = Some(match bound_filter.take() {
                Some(existing) => BoundExpr::And(Box::new(existing), Box::new(constraint)),
                None => constraint,
            });
        }
        self.allow_aggregates = true;
        let bound_columns = self.bind_result_columns(columns)?;
        // Read before the `HAVING` is bound, because by then `self.aggregates`
        // holds the ones the `HAVING` itself introduced. `bind::having` says
        // why that distinction is the whole rule.
        let aggregates_in_columns = self.aggregates.len();
        // See `tail_may_name_an_alias`. This core's own `GROUP BY` and `HAVING`
        // are read here rather than from the flag because they belong to the
        // core and the flag belongs to the statement around it.
        if self.tail_may_name_an_alias || !group_by.is_empty() || having.is_some() {
            for column in &bound_columns {
                if !column.name.is_empty() {
                    self.result_aliases
                        .push((column.name.to_ascii_lowercase(), column.expr.clone()));
                }
            }
        }
        let mut bound_group = Vec::with_capacity(group_by.len());
        for expr in group_by {
            bound_group.push(self.bind_group_term(*expr, &bound_columns)?);
        }
        let bound_having = match having {
            Some(expr) => Some(self.bind_expr(*expr)?),
            None => None,
        };
        having::refuse_when_nothing_aggregates(
            bound_having.is_some(),
            bound_group.len(),
            aggregates_in_columns,
        )?;
        // The sources stay in the binder's scope: `ORDER BY` and `LIMIT` belong
        // to the whole statement and are bound after this returns, and
        // `ORDER BY b.id` needs the same scope the result columns had.
        Ok(BoundSelect {
            sources: Vec::new(),
            filter: bound_filter,
            group_by: bound_group,
            having: bound_having,
            columns: bound_columns,
            distinct: *distinct,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            aggregates: Vec::new(),
            values: Vec::new(),
            compounds: Vec::new(),
            windows: Vec::new(),
            correlations: Vec::new(),
        })
    }

    /// Refuses an `INDEXED BY` that names no index of the table just bound.
    ///
    /// **It was read and thrown away (task-1979, F7).** The hint reached the
    /// AST and nothing below the parser looked at it, so
    /// `SELECT * FROM t INDEXED BY nosuch WHERE a = 1` answered rows where
    /// SQLite refuses the statement with `no such index: nosuch`. A caller who
    /// wrote the hint to make a plan use a particular index, and misspelled it,
    /// got a plan that did something else and no way to tell.
    ///
    /// `NOT INDEXED` names nothing and is a planner instruction rather than a
    /// reference, so it passes through here untouched.
    ///
    /// @param hint - the hint as written
    /// @param span - where to point the diagnostic
    fn check_index_hint(&mut self, hint: ast::IndexHint, span: Span) -> Result<(), ParseError> {
        let ast::IndexHint::IndexedBy(name) = hint else {
            return Ok(());
        };
        let folded = self.ast.folded(name).to_vec();
        let Some(source) = self.sources.last() else {
            return Ok(());
        };
        if source
            .table
            .indexes
            .iter()
            .any(|index| index.folded == folded)
        {
            return Ok(());
        }
        Err(no_such_index(self.ast.text(name), span))
    }

    /// Turns a hint as the parser wrote it into the form the planner reads.
    ///
    /// @param hint - the hint as written
    pub(crate) fn index_choice(&self, hint: ast::IndexHint) -> IndexChoice {
        match hint {
            ast::IndexHint::None => IndexChoice::Any,
            ast::IndexHint::NotIndexed => IndexChoice::NotIndexed,
            ast::IndexHint::IndexedBy(name) => IndexChoice::Only(self.ast.folded(name).to_vec()),
        }
    }

    /// Binds one FROM term, registering it as a source of the current block.
    ///
    /// A table, a CTE reference, a view and a parenthesised subquery all end up
    /// as one entry in the block's scope. The last three carry the block they
    /// stand for, and everything below the binder treats them alike.
    pub(crate) fn bind_from_term(&mut self, id: ast::FromTermId) -> Result<(), ParseError> {
        let Some(term) = self.ast.from_term(id) else {
            return Err(unsupported("missing FROM term", Span::default()));
        };
        let join = term.join;
        let span = term.span;
        match &term.source {
            FromSource::Table {
                database,
                name,
                arguments,
                indexed_by,
                ..
            } => {
                let arguments = arguments.clone();
                let indexed_by = *indexed_by;
                self.bind_table_term(*database, *name, term.alias, join, span)?;
                self.check_index_hint(indexed_by, span)?;
                // The hint belongs to the term that was just pushed, and this
                // is the only place that knows both.
                let choice = self.index_choice(indexed_by);
                if let Some(source) = self.sources.last_mut() {
                    source.index_hint = choice;
                }
                if let Some(arguments) = arguments {
                    self.bind_table_arguments(&arguments, span)?;
                }
                Ok(())
            }
            FromSource::Subquery(select) => {
                let alias = term.alias.map(|alias| self.ast.text(alias).to_vec());
                self.bind_subquery_term(*select, alias, Vec::new(), join, span)
            }
            FromSource::Join(terms) => {
                // A parenthesised join is a term to whatever contains it, and
                // SQLite flattens it into the enclosing FROM list. The first
                // inner term inherits the join that attached the parentheses;
                // the rest keep their own.
                let inner = terms.clone();
                for (position, nested) in inner.iter().enumerate() {
                    let before = self.scope().len();
                    self.bind_from_term(*nested)?;
                    if position == 0 {
                        if let Some(id) = self.scope_id(before) {
                            if let Some(source) = self.sources.get_mut(id) {
                                source.join = join;
                            }
                        }
                    }
                }
                self.desugar_join_constraints(&inner)?;
                Ok(())
            }
        }
    }

    /// Binds a named FROM term: a CTE, a view, or a real table.
    fn bind_table_term(
        &mut self,
        database: Option<ast::NameId>,
        name: ast::NameId,
        alias: Option<ast::NameId>,
        join: JoinKind,
        span: Span,
    ) -> Result<(), ParseError> {
        let folded = self.ast.folded(name).to_vec();
        let written = self.ast.text(name).to_vec();
        if database.is_none() {
            // A reference to the CTE whose own definition is being bound is
            // the recursion. It reads the row the fill loop is on rather than
            // being another materialisation of the same query.
            if let Some(position) = self
                .recursing
                .iter()
                .rposition(|target| target.folded == folded)
            {
                return self.push_recursive_self(position, alias, join);
            }
            if let Some(cte) = self.find_cte(&folded) {
                let alias = match alias {
                    Some(alias) => self.ast.text(alias).to_vec(),
                    None => cte.name.clone(),
                };
                // A definition already being bound cannot be bound again: that
                // is a cycle, and following it does not end.
                if self.binding_ctes.contains(&cte.select) {
                    return Err(ParseError::new(
                        ParseErrorKind::Unsupported("circular reference in a CTE"),
                        span,
                    ));
                }
                self.binding_ctes.push(cte.select);
                // **`RECURSIVE` is a keyword SQLite does not require.** A CTE
                // whose FROM names itself *is* the recursion, written or not,
                // and reading the keyword as the only evidence sent this
                // binder round the same definition until the stack ran out.
                let outcome = if cte.recursive || self.select_names_itself(cte.select, &folded) {
                    self.bind_recursive_cte(&cte, alias, join, span)
                } else {
                    self.bind_subquery_term(
                        cte.select,
                        Some(alias),
                        cte.columns.clone(),
                        join,
                        span,
                    )
                };
                self.binding_ctes.pop();
                return outcome;
            }
        }
        let database_name = database.map(|id| self.ast.folded(id).to_vec());
        let Some(table) = self.catalog.find_table(database_name.as_deref(), &folded) else {
            return Err(no_such_table(&written, span));
        };
        if table.kind == TableKind::Virtual && table.columns.is_empty() {
            // A virtual table with no declared columns is one whose module this
            // build does not have. The schema still loaded - every other table
            // in the file works - and naming this one is what fails.
            return Err(unsupported("that virtual table's module", span));
        }
        if table.kind == TableKind::View {
            let view_alias = match alias {
                Some(alias) => self.ast.text(alias).to_vec(),
                None => table.name.clone(),
            };
            let database_index = table.database;
            let Some(body) = table.view.as_ref() else {
                return Err(ParseError::new(
                    ParseErrorKind::Unsupported("the view's definition could not be parsed"),
                    span,
                ));
            };
            self.record_dependency(database_index);
            // The view's own arena outlives the binder because it belongs to
            // the catalog snapshot the binder holds, which is what lets the
            // body be bound in place rather than re-parsed here.
            let columns = body.columns.clone();
            let saved = self.ast;
            // A view's body is a string in the schema, so everything it names
            // is named from a schema - including anything a further view or a
            // generated column it reads goes on to name. The site is saved and
            // restored rather than set, because a view inside a view is still
            // inside the outer one.
            let saved_site = self.call_site;
            self.ast = &body.ast;
            self.call_site = function::CallSite::Schema;
            let bound = self.bind_select(body.select);
            self.call_site = saved_site;
            self.ast = saved;
            let bound = bound?;
            return self.push_subquery_source(bound, view_alias, columns, join, span);
        }
        self.record_dependency(table.database);
        let alias = match alias {
            Some(alias) => self.ast.text(alias).to_vec(),
            None => table.name.clone(),
        };
        // The shared pointer, taken here rather than above: a view binds its
        // body out of the catalog's own arena, and only the borrow keeps that
        // alive. The second lookup is a folded-name comparison over the
        // catalog's tables and costs a fraction of the clone it replaces.
        let Some(table) = self.catalog.shared_table(database_name.as_deref(), &folded) else {
            return Err(no_such_table(&written, span));
        };
        let id = self.sources.len();
        self.sources.push(BoundSource {
            index_hint: crate::bind::IndexChoice::Any,
            id,
            rows: SourceRows::Table,
            table,
            alias,
            join,
            constraint: None,
            suppressed: Vec::new(),
            index_exprs: Vec::new(),
        });
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(id);
        }
        self.attach_index_exprs(id);
        Ok(())
    }

    /// Binds a term's partial-index predicates and expression keys onto it.
    ///
    /// **Scoped to the one term, and tolerant of a schema it cannot bind.** The
    /// expressions are bound in a nested binder holding only this source, so a
    /// predicate reading `b` means *this* table's `b` and not another term's;
    /// and an index whose expressions do not bind is left out rather than
    /// failing the statement, which leaves the planner unable to choose it.
    /// That is the same answer the planner gave while these forms were refused
    /// outright, so a schema this cannot read is slower and never wrong.
    ///
    /// It returns immediately for a table with neither kind of index, which is
    /// every table in the performance gate.
    ///
    /// @param id - the FROM term's statement-wide number
    fn attach_index_exprs(&mut self, id: usize) {
        let Some(source) = self.sources.get(id) else {
            return;
        };
        let table = std::rc::Rc::clone(&source.table);
        let wanted: Vec<usize> = table
            .indexes
            .iter()
            .enumerate()
            .filter(|(_, index)| {
                index.partial_sql.is_some()
                    || index.columns.iter().any(|key| key.expr_sql.is_some())
            })
            .map(|(position, _)| position)
            .collect();
        if wanted.is_empty() {
            return;
        }
        let alone = source.clone();
        let mut bound = Vec::with_capacity(wanted.len());
        for position in wanted {
            let Some(index) = table.indexes.get(position) else {
                continue;
            };
            let predicate = match index.partial_sql.as_ref() {
                Some(sql) => match self.bind_alone(&alone, sql) {
                    Some(expr) => Some(expr),
                    None => continue,
                },
                None => None,
            };
            let mut keys = Vec::with_capacity(index.columns.len());
            let mut readable = true;
            for key in &index.columns {
                match key.expr_sql.as_ref() {
                    Some(sql) => match self.bind_alone(&alone, sql) {
                        Some(expr) => keys.push(Some(expr)),
                        None => {
                            readable = false;
                            break;
                        }
                    },
                    None => keys.push(None),
                }
            }
            if !readable {
                continue;
            }
            bound.push(crate::dml::BoundIndexExprs {
                position,
                predicate,
                keys,
            });
        }
        if let Some(source) = self.sources.get_mut(id) {
            source.index_exprs = bound;
        }
    }

    /// Binds one piece of schema text against a single FROM term.
    ///
    /// `None` when it does not parse or does not bind, which the caller reads
    /// as "this index cannot be reasoned about" rather than as an error.
    ///
    /// @param alone - the only term the expression may name
    /// @param sql - the expression as it was written in the schema
    fn bind_alone(&self, alone: &BoundSource, sql: &[u8]) -> Option<BoundExpr> {
        let limits = inillucent_base::limits::Limits::default();
        let (ast, expr) = crate::parser::parse_expression(sql, &limits).ok()?;
        let mut nested = Binder::new(self.catalog, &ast, self.authorizer);
        nested.trigger_depth = self.trigger_depth;
        // **The nested binder inherits what the connection registered, and
        // reads as a schema (task-1972).** It used to inherit neither, so an
        // index expression naming a registered function did not resolve at all
        // here and the planner silently left the index out; and had it
        // resolved, it would have resolved with a statement's permissions.
        nested.externals = self.externals;
        nested.collations = self.collations;
        nested.trusted_schema = self.trusted_schema;
        nested.call_site = function::CallSite::Schema;
        // **The term sits at its own id, not at zero (task-2078).** A column is
        // resolved by looking its term up in `sources` by statement-wide id,
        // and this list used to hold the one term at position zero. For the
        // first FROM term those agree. For every later one the lookup found
        // nothing, the expression did not bind, and the index was left out
        // without a word: `CREATE INDEX h_part ON h(c) WHERE c > 3` served
        // `FROM h, s WHERE h.c > 3` and not `FROM s, h WHERE h.c > 3`. The
        // positions below the term's are filled with copies of it, and the
        // scope names only the term's own id, so nothing can resolve to them.
        nested.sources = vec![alone.clone(); alone.id.saturating_add(1)];
        nested.scopes = vec![vec![alone.id]];
        nested.bind_expr(expr).ok()
    }

    /// Binds one compound arm in a scope of its own.
    fn bind_isolated_arm(&mut self, arm: ast::SelectCoreId) -> Result<BoundSelect, ParseError> {
        let frame = self.enter_block();
        let mut bound = self.bind_arm(arm);
        // The arm owns whatever aggregates and correlations it accumulated, and
        // they have to be read off the binder before the frame is restored.
        if let Ok(bound) = bound.as_mut() {
            bound.aggregates = self.aggregates.clone();
            bound.windows = self.windows.clone();
            bound.correlations = self.correlations.clone();
        }
        let ids = self.leave_block(frame);
        let mut bound = bound?;
        bound.sources = ids
            .iter()
            .filter_map(|id| self.sources.get(*id).cloned())
            .collect();
        Ok(bound)
    }

    /// Returns the next statement-wide number for a nested query used as a
    /// value.
    fn next_subquery_id(&mut self) -> usize {
        let id = self.subqueries;
        self.subqueries = self.subqueries.saturating_add(1);
        id
    }

    /// Binds a nested query that is used as a value rather than as a source.
    ///
    /// It gets a scope of its own, so its own FROM terms shadow the enclosing
    /// query's, and a name it can only resolve outward is recorded as a
    /// correlation - which is what tells the compiler to rebuild it per row.
    fn bind_value_subquery(
        &mut self,
        select: SelectId,
        span: Span,
    ) -> Result<BoundSelect, ParseError> {
        let _ = span;
        self.bind_select(select)
    }

    /// Binds `x IN (SELECT ...)`.
    fn bind_in_subquery(
        &mut self,
        operand: BoundExpr,
        select: SelectId,
        negated: bool,
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        let block = self.bind_value_subquery(select, span)?;
        if block.columns.len() != 1 {
            return Err(ParseError::new(
                ParseErrorKind::Unsupported("sub-select returns more than one column"),
                span,
            ));
        }
        let Some(column) = block.columns.first() else {
            return Err(unsupported("a subquery with no result column", span));
        };
        let (affinity, collation) = comparison_rules(&operand, &column.expr);
        Ok(BoundExpr::Subquery {
            id: self.next_subquery_id(),
            kind: SubqueryKind::In,
            negated,
            operand: Some(Box::new(operand)),
            block: Box::new(block),
            affinity,
            collation,
        })
    }

    /// Binds a subquery FROM term and registers it as a source.
    fn bind_subquery_term(
        &mut self,
        select: SelectId,
        alias: Option<Vec<u8>>,
        columns: Vec<Vec<u8>>,
        join: JoinKind,
        span: Span,
    ) -> Result<(), ParseError> {
        let bound = self.bind_select(select)?;
        let alias = alias.unwrap_or_else(|| b"subquery".to_vec());
        self.push_subquery_source(bound, alias, columns, join, span)
    }

    /// Registers a bound block as one FROM term of the current block.
    fn push_subquery_source(
        &mut self,
        bound: BoundSelect,
        alias: Vec<u8>,
        columns: Vec<Vec<u8>>,
        join: JoinKind,
        span: Span,
    ) -> Result<(), ParseError> {
        if !columns.is_empty() && columns.len() != bound.columns.len() {
            return Err(ParseError::new(
                ParseErrorKind::Unsupported("the named column list does not match the query"),
                span,
            ));
        }
        let table = subquery_table(&alias, &columns, &bound);
        let id = self.sources.len();
        self.sources.push(BoundSource {
            index_hint: crate::bind::IndexChoice::Any,
            id,
            rows: SourceRows::Subquery(Box::new(bound)),
            table: std::rc::Rc::new(table),
            alias,
            join,
            constraint: None,
            suppressed: Vec::new(),
            index_exprs: Vec::new(),
        });
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(id);
        }
        Ok(())
    }

    /// Records the named windows a `WINDOW` clause declares.
    fn declare_windows(
        &mut self,
        windows: &[(ast::NameId, ast::WindowId)],
    ) -> Result<(), ParseError> {
        for (name, window) in windows {
            self.named_windows
                .push((self.ast.folded(*name).to_vec(), *window));
        }
        Ok(())
    }

    /// Binds a call carrying an `OVER` clause.
    ///
    /// The window is resolved first, because a call over a named window that
    /// does not exist is an error about the name rather than about the
    /// function - and because `OVER w` and `OVER (w ORDER BY x)` both have to
    /// end up as one fully-resolved specification before the frame defaults can
    /// be applied.
    fn bind_window_call(
        &mut self,
        name: ast::NameId,
        distinct: bool,
        arguments: Option<Vec<ExprId>>,
        filter: Option<ExprId>,
        over: ast::WindowId,
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        let folded = self.ast.folded(name).to_vec();
        let spec = self.resolve_window(over, span)?;
        let star = arguments.is_none();
        let mut bound_arguments = Vec::new();
        for argument in arguments.unwrap_or_default() {
            bound_arguments.push(self.bind_expr(argument)?);
        }
        let call = match function::lookup_window(&folded) {
            Some(func) => {
                let (least, most) = func.arity();
                if bound_arguments.len() < least || bound_arguments.len() > most {
                    return Err(wrong_arguments(&folded, span));
                }
                if distinct {
                    return Err(unsupported("DISTINCT in a window function", span));
                }
                WindowCall::Plain(func)
            }
            None => match window_aggregate(&folded, bound_arguments.len()) {
                Some(func) => WindowCall::Aggregate(func),
                None => return Err(no_such_function(&folded, span)),
            },
        };
        let bound_filter = match filter {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let collation = bound_arguments
            .first()
            .and_then(BoundExpr::collation)
            .unwrap_or(Collation::Binary);

        let mut partition_by = Vec::new();
        for expr in &spec.partition_by {
            partition_by.push(self.bind_expr(*expr)?);
        }
        let order_by = self.bind_order_by(&spec.order_by, &[])?;
        // SQLite's defaults, and they are not the same clause: with an
        // `ORDER BY` the frame ends at the current row's peer group, and
        // without one it covers the whole partition. Using one default for both
        // makes every ordered `sum() OVER ()` a running total or none of them.
        let unit = spec.unit.unwrap_or(FrameUnit::Range);
        let (start, end) = match (spec.start, spec.end) {
            (None, None) => (
                BoundFrameBound::UnboundedPreceding,
                if order_by.is_empty() {
                    BoundFrameBound::UnboundedFollowing
                } else {
                    BoundFrameBound::CurrentRow
                },
            ),
            (Some(start), None) => (
                self.bind_frame_bound(start, span)?,
                BoundFrameBound::CurrentRow,
            ),
            (Some(start), Some(end)) => (
                self.bind_frame_bound(start, span)?,
                self.bind_frame_bound(end, span)?,
            ),
            (None, Some(end)) => (
                BoundFrameBound::UnboundedPreceding,
                self.bind_frame_bound(end, span)?,
            ),
        };
        if matches!(start, BoundFrameBound::UnboundedFollowing)
            || matches!(end, BoundFrameBound::UnboundedPreceding)
        {
            return Err(ParseError::new(
                ParseErrorKind::Unsupported("unsupported frame specification"),
                span,
            ));
        }
        if unit != FrameUnit::Rows
            && matches!(
                (&start, &end),
                (BoundFrameBound::Preceding(_), _)
                    | (BoundFrameBound::Following(_), _)
                    | (_, BoundFrameBound::Preceding(_))
                    | (_, BoundFrameBound::Following(_))
            )
            && order_by.len() != 1
        {
            return Err(ParseError::new(
                ParseErrorKind::Unsupported(
                    "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY expression",
                ),
                span,
            ));
        }
        let slot = self.windows.len();
        let explicit = explicit_argument_collation(&bound_arguments);
        self.windows.push(BoundWindow {
            call,
            distinct,
            collation,
            arguments: bound_arguments,
            star,
            filter: bound_filter,
            partition_by,
            order_by,
            unit,
            start,
            end,
            exclude: spec.exclude,
        });
        Ok(BoundExpr::WindowRef {
            slot,
            collation: explicit,
        })
    }

    /// Resolves an `OVER` clause into one fully-written window specification.
    fn resolve_window(&self, id: ast::WindowId, span: Span) -> Result<ast::Window, ParseError> {
        let Some(window) = self.ast.window(id) else {
            return Err(unsupported("missing window", span));
        };
        let mut spec = window.clone();
        let mut guard = 0usize;
        while let Some(base) = spec.base {
            guard = guard.saturating_add(1);
            if guard > MAX_COMPOUND_SELECT {
                return Err(unsupported("a window that inherits from itself", span));
            }
            let folded = self.ast.folded(base).to_vec();
            let Some((_, id)) = self.named_windows.iter().find(|(name, _)| *name == folded) else {
                return Err(no_such_window(&folded, span));
            };
            let Some(parent) = self.ast.window(*id) else {
                return Err(unsupported("missing window", span));
            };
            // The inheriting window may add an `ORDER BY` and a frame; it may
            // not replace the base's `PARTITION BY`, which is SQLite's rule and
            // the reason the merge is one-directional.
            let parent = parent.clone();
            spec.base = parent.base;
            spec.partition_by = parent.partition_by.clone();
            if spec.order_by.is_empty() {
                spec.order_by = parent.order_by.clone();
            }
            if spec.unit.is_none() {
                spec.unit = parent.unit;
                spec.start = parent.start;
                spec.end = parent.end;
                spec.exclude = parent.exclude;
            }
        }
        Ok(spec)
    }

    /// Binds one end of a frame.
    fn bind_frame_bound(
        &mut self,
        bound: FrameBound,
        span: Span,
    ) -> Result<BoundFrameBound, ParseError> {
        let bound = match bound {
            FrameBound::UnboundedPreceding => BoundFrameBound::UnboundedPreceding,
            FrameBound::CurrentRow => BoundFrameBound::CurrentRow,
            FrameBound::UnboundedFollowing => BoundFrameBound::UnboundedFollowing,
            FrameBound::Preceding(expr) => {
                BoundFrameBound::Preceding(self.bind_frame_offset(expr, span)?)
            }
            FrameBound::Following(expr) => {
                BoundFrameBound::Following(self.bind_frame_offset(expr, span)?)
            }
        };
        Ok(bound)
    }

    /// Binds a frame offset, which may not read a column.
    fn bind_frame_offset(&mut self, expr: ExprId, span: Span) -> Result<BoundExpr, ParseError> {
        let bound = self.bind_expr(expr)?;
        if !bound.is_constant() {
            return Err(ParseError::new(
                ParseErrorKind::Unsupported("a frame offset must be a constant"),
                span,
            ));
        }
        Ok(bound)
    }

    /// Turns `ON`, `USING` and `NATURAL` into ordinary predicates.
    ///
    /// The output-column rules survive the rewrite: a `USING` or `NATURAL`
    /// column is suppressed from the right-hand term's contribution to `*`,
    /// which is the only visible difference between a `USING` join and the
    /// equality predicate it means.
    ///
    /// The terms are addressed by their position in *this block's* FROM list,
    /// which the scope turns into the statement-wide source id. A parenthesised
    /// join has already flattened itself into the same list by the time this
    /// runs, so a position is always a real term.
    fn desugar_join_constraints(&mut self, terms: &[ast::FromTermId]) -> Result<(), ParseError> {
        let base = self
            .scope()
            .len()
            .saturating_sub(terms.iter().map(|_| 1usize).sum::<usize>());
        for (offset, id) in terms.iter().enumerate() {
            let Some(term) = self.ast.from_term(*id) else {
                continue;
            };
            if matches!(term.source, FromSource::Join(_)) {
                // Its own constraints were desugared when it was flattened.
                continue;
            }
            let position = base.saturating_add(offset);
            let constraint = term.constraint.clone();
            let natural = term.natural;
            let span = term.span;
            if natural {
                let names = self.natural_columns(position);
                let predicate = self.equality_over(position, &names)?;
                self.set_constraint(position, predicate);
                continue;
            }
            match constraint {
                JoinConstraint::None => {}
                JoinConstraint::On(expr) => {
                    let bound = self.bind_expr(expr)?;
                    self.set_constraint(position, Some(bound));
                }
                JoinConstraint::Using(names) => {
                    let folded: Vec<Vec<u8>> = names
                        .iter()
                        .map(|name| self.ast.folded(*name).to_vec())
                        .collect();
                    for name in &folded {
                        if self.find_column_in(position, name).is_none() {
                            return Err(no_such_column(name, span));
                        }
                    }
                    let predicate = self.equality_over(position, &folded)?;
                    if predicate.is_none() {
                        return Err(unsupported("empty USING list", span));
                    }
                    self.set_constraint(position, predicate);
                }
            }
        }
        Ok(())
    }

    /// Stores a join constraint on a source of the current block.
    fn set_constraint(&mut self, position: usize, constraint: Option<BoundExpr>) {
        let Some(id) = self.scope_id(position) else {
            return;
        };
        if let Some(source) = self.sources.get_mut(id) {
            source.constraint = constraint;
        }
    }

    /// Returns the column names a NATURAL join equates: every name the right
    /// term shares with any term to its left in the same block.
    fn natural_columns(&self, position: usize) -> Vec<Vec<u8>> {
        let Some(right) = self.source_at(position) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for column in &right.table.columns {
            if column.hidden {
                continue;
            }
            let shared = (0..position).any(|earlier| {
                self.source_at(earlier)
                    .is_some_and(|left| left.table.column_position(&column.folded).is_some())
            });
            if shared {
                names.push(column.folded.clone());
            }
        }
        names
    }

    /// Returns one source of the current block by its position in the block.
    fn source_at(&self, position: usize) -> Option<&BoundSource> {
        let id = self.scope_id(position)?;
        self.sources.get(id)
    }

    /// Builds `left.name = right.name AND ...` for a USING or NATURAL join,
    /// and suppresses the right-hand columns from star expansion.
    fn equality_over(
        &mut self,
        position: usize,
        names: &[Vec<u8>],
    ) -> Result<Option<BoundExpr>, ParseError> {
        let mut predicate: Option<BoundExpr> = None;
        for name in names {
            let Some((left_source, left_column)) = self.find_column_left_of(position, name) else {
                continue;
            };
            let Some((right_source, right_column)) = self.find_column_in(position, name) else {
                continue;
            };
            if let Some(id) = self.scope_id(position) {
                if let Some(source) = self.sources.get_mut(id) {
                    source.suppressed.push(right_column);
                }
            }
            let left = self.column_expr(left_source, left_column)?;
            let right = self.column_expr(right_source, right_column)?;
            let (affinity, collation) = comparison_rules(&left, &right);
            let equality = BoundExpr::Compare {
                op: BinaryOp::Equal,
                left: Box::new(left),
                right: Box::new(right),
                affinity,
                collation,
            };
            predicate = Some(match predicate {
                Some(existing) => BoundExpr::And(Box::new(existing), Box::new(equality)),
                None => equality,
            });
        }
        Ok(predicate)
    }

    /// Finds a column by folded name in one source, returning its source id.
    fn find_column_in(&self, position: usize, folded: &[u8]) -> Option<(usize, u16)> {
        let id = self.scope_id(position)?;
        let source = self.sources.get(id)?;
        source.table.column_position(folded).map(|c| (id, c))
    }

    /// Finds a column by folded name in the sources before one.
    fn find_column_left_of(&self, position: usize, folded: &[u8]) -> Option<(usize, u16)> {
        for index in (0..position).rev() {
            if let Some(found) = self.find_column_in(index, folded) {
                return Some(found);
            }
        }
        None
    }

    /// Records that the statement depends on a database's schema cookie.
    fn record_dependency(&mut self, database: usize) {
        if self
            .dependencies
            .schemas
            .iter()
            .any(|(index, _)| *index == database)
        {
            return;
        }
        let cookie = self.catalog.schema_cookie(database);
        self.dependencies.schemas.push((database, cookie));
    }

    /// Binds the result columns, expanding `*` and `table.*`.
    fn bind_result_columns(
        &mut self,
        columns: &[ast::ResultColumn],
    ) -> Result<Vec<BoundResultColumn>, ParseError> {
        // One column of the AST is usually one bound column, so this is the
        // right answer rather than a guess; `*` expands to more and the vector
        // grows from here, which is still fewer growths than starting empty.
        // `Vec::new` grew to four for a one-column select, which is 704 bytes
        // asked for to hold 176 (task-2026).
        let mut bound = Vec::with_capacity(columns.len());
        for column in columns {
            match self.ast.expr(column.expr) {
                Some(Expr::Star { table }) => {
                    let qualifier = table.map(|id| self.ast.folded(id).to_vec());
                    self.expand_star(qualifier.as_deref(), column.span, &mut bound)?;
                }
                _ => {
                    let expr = self.bind_expr(column.expr)?;
                    let name = match column.alias {
                        Some(alias) => self.ast.text(alias).to_vec(),
                        None => self.default_column_name(column.expr, &expr),
                    };
                    let (origin, declared_type) = self.column_origin(&expr);
                    bound.push(BoundResultColumn {
                        expr,
                        name,
                        origin,
                        declared_type,
                    });
                }
            }
        }
        if bound.is_empty() {
            return Err(unsupported(
                "a SELECT must have result columns",
                Span::default(),
            ));
        }
        Ok(bound)
    }

    /// Turns a table-valued function's arguments into hidden-column equalities.
    ///
    /// The nth argument constrains the nth *hidden* column, which is the rule
    /// that makes `generate_series(1,5)` mean `start = 1 AND stop = 5`. More
    /// arguments than hidden columns is an error at bind time, because there is
    /// nothing for the extra one to constrain.
    fn bind_table_arguments(&mut self, arguments: &[ExprId], span: Span) -> Result<(), ParseError> {
        let Some(id) = self.scope().last().copied() else {
            return Err(unsupported("a table-valued function with no term", span));
        };
        let Some(source) = self.sources.get(id) else {
            return Err(unsupported("a table-valued function with no term", span));
        };
        if source.table.kind != TableKind::Virtual {
            return Err(unsupported(
                "arguments on a table that is not virtual",
                span,
            ));
        }
        let hidden: Vec<(u16, Affinity, Collation)> = source
            .table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.hidden)
            .map(|(index, column)| {
                (
                    index as u16,
                    column.affinity,
                    self.collation_named(&column.collation)
                        .unwrap_or(Collation::Binary),
                )
            })
            .collect();
        if arguments.len() > hidden.len() {
            return Err(wrong_arguments(&source.table.name.clone(), span));
        }
        for (position, argument) in arguments.iter().enumerate() {
            let Some((column, affinity, collation)) = hidden.get(position).copied() else {
                break;
            };
            let value = self.bind_expr(*argument)?;
            self.pending_constraints.push(BoundExpr::Compare {
                op: BinaryOp::Equal,
                left: Box::new(BoundExpr::Column {
                    source: id,
                    column,
                    slot: column,
                    affinity,
                    collation,
                }),
                right: Box::new(value),
                affinity: None,
                collation,
            });
        }
        Ok(())
    }

    /// Returns the collation a name selects.
    ///
    /// A connection's own definitions come first, so an application that
    /// defines `NOCASE` gets its own rather than the built-in - which is what
    /// SQLite does, and is the only way `sqlite3_create_collation` can be used
    /// to change how an existing schema compares.
    fn collation_named(&self, name: &[u8]) -> Option<Collation> {
        // **The name is compared where it is (task-2026).** `create_collation`
        // stores the name uppercased, so an uppercase-insensitive comparison
        // against a stored name answers exactly what building an uppercase copy
        // of `name` and comparing bytes answered. Building the copy cost an
        // allocation per column reference, whether or not the connection had
        // registered any collation at all - two of the 109 allocations
        // `SELECT a FROM t WHERE id = ?1` made.
        if let Some((_, collation)) = self
            .collations
            .iter()
            .find(|(candidate, _)| candidate.as_bytes().eq_ignore_ascii_case(name))
        {
            return Some(*collation);
        }
        Collation::from_name(core::str::from_utf8(name).unwrap_or(""))
    }

    /// Binds `f(table, ...)` as a module's auxiliary function, if that is what
    /// it is.
    ///
    /// The tell is the first argument: a bare reference to a virtual table's
    /// own hidden column, which is a thing no ordinary function is ever handed
    /// on purpose. `bm25(docs)` takes this path; an unknown name is refused by
    /// the module rather than here, because the module is what knows its own
    /// functions.
    fn bind_auxiliary_call(
        &mut self,
        name: &[u8],
        arguments: &[ExprId],
        span: Span,
    ) -> Result<Option<BoundExpr>, ParseError> {
        let Some(first) = arguments.first() else {
            return Ok(None);
        };
        let Some(&Expr::Column {
            database: None,
            table: None,
            column,
        }) = self.ast.expr(*first)
        else {
            return Ok(None);
        };
        let Ok(BoundExpr::Column { source, column, .. }) =
            self.bind_column_reference(None, None, column, span)
        else {
            return Ok(None);
        };
        let Some(entry) = self.sources.get(source) else {
            return Ok(None);
        };
        if entry.table.kind != TableKind::Virtual {
            return Ok(None);
        }
        // The self column is the hidden one named after the table, and only
        // that one: `rank` is a column, not a handle.
        let self_column = entry
            .table
            .column(column)
            .is_some_and(|info| info.folded == entry.table.folded);
        if !self_column {
            return Ok(None);
        }
        let mut rest = Vec::with_capacity(arguments.len() - 1);
        for argument in arguments.iter().skip(1) {
            rest.push(self.bind_expr(*argument)?);
        }
        Ok(Some(BoundExpr::VirtualFunction {
            source,
            name: name.to_ascii_lowercase(),
            arguments: rest,
        }))
    }

    /// Returns whether an expression is a column of a virtual table.
    fn is_virtual_column(&self, expr: &BoundExpr) -> bool {
        let BoundExpr::Column { source, .. } = expr else {
            return false;
        };
        self.sources
            .get(*source)
            .is_some_and(|source| source.table.kind == TableKind::Virtual)
    }

    /// Expands `*` or `table.*` into one bound column per visible column.
    ///
    /// Only the block's own FROM terms are expanded. An enclosing block's terms
    /// are visible to a *name*, which is what makes a subquery correlated, but
    /// they are not part of this block's `*`.
    fn expand_star(
        &mut self,
        qualifier: Option<&[u8]>,
        span: Span,
        into: &mut Vec<BoundResultColumn>,
    ) -> Result<(), ParseError> {
        let scope: Vec<usize> = self.scope().to_vec();
        if scope.is_empty() {
            return Err(ParseError::new(
                ParseErrorKind::Unexpected {
                    found: "*".to_string(),
                    expected: vec!["a FROM clause"],
                },
                span,
            ));
        }
        let mut matched = false;
        for id in scope {
            let Some(source) = self.sources.get(id) else {
                continue;
            };
            if let Some(qualifier) = qualifier {
                if !source.alias.eq_ignore_ascii_case(qualifier) {
                    continue;
                }
            }
            matched = true;
            let columns = source.table.columns.clone();
            let suppressed = source.suppressed.clone();
            let database = self.catalog.database_name(source.table.database).to_vec();
            let table_name = source.table.name.clone();
            let synthetic = source.table.kind == TableKind::Subquery;
            for (index, column) in columns.iter().enumerate() {
                let position_u16 = index as u16;
                if column.hidden || suppressed.contains(&position_u16) {
                    continue;
                }
                if self.authorizer.authorize(AuthAction::Read {
                    database: &database,
                    table: &table_name,
                    column: &column.name,
                }) == Authorization::Deny
                {
                    return Err(denied("not authorized", span));
                }
                let expr = self.column_expr(id, position_u16)?;
                into.push(BoundResultColumn {
                    expr,
                    name: column.name.clone(),
                    // A subquery's column has no table of origin: it came from
                    // an expression, and reporting the synthetic name as one
                    // would make `sqlite3_column_table_name` invent a table.
                    origin: (!synthetic)
                        .then(|| (database.clone(), table_name.clone(), column.name.clone())),
                    declared_type: column.declared_type.clone(),
                });
            }
        }
        if !matched {
            return Err(no_such_table(qualifier.unwrap_or(b"*"), span));
        }
        Ok(())
    }

    /// Returns the name an unaliased result column reports.
    ///
    /// A bare column reference is named after its declared name rather than
    /// the query's text - `rowid`/`oid`/`_rowid_` resolve to the column they
    /// alias and take its name too. Everything else keeps the source text.
    fn default_column_name(&self, id: ExprId, bound: &BoundExpr) -> Vec<u8> {
        let name = match bound {
            BoundExpr::Column { source, column, .. } => self
                .sources
                .get(*source)
                .and_then(|held| held.table.column(*column)),
            BoundExpr::Rowid { source } => self
                .sources
                .get(*source)
                .and_then(|held| held.table.column(held.table.rowid_alias?)),
            _ => None,
        };
        if let Some(name) = name {
            return name.name.clone();
        }
        // **The three spellings of the rowid are one column name (task-1979,
        // F22).** `SELECT rowid, oid, _rowid_ FROM t` answers three columns
        // called `rowid` in SQLite, whichever way each was written. On a table
        // with no INTEGER PRIMARY KEY there is no declared column to take the
        // name from, and the fallback below took the text as typed, so the
        // last two came back called `oid` and `_rowid_` - names no caller
        // could match against the one SQLite reports.
        if matches!(bound, BoundExpr::Rowid { .. }) {
            return b"rowid".to_vec();
        }
        if let Some(Expr::Column { column, .. }) = self.ast.expr(id) {
            return self.ast.text(*column).to_vec();
        }
        // Everything else is named after the text it was written as,
        // exactly as written - `SELECT 1 +  2` has a column called
        // `1 +  2`, spaces and all, because SQLite cuts the span rather
        // than re-rendering the expression.
        let span = self.ast.expr_span(id);
        span.slice(self.source).to_vec()
    }

    /// Returns the origin triple and declared type of a bound column.
    fn column_origin(&self, expr: &BoundExpr) -> (Option<ColumnOrigin>, Vec<u8>) {
        // A rowid alias is a column, and `SELECT a FROM t` where `a` is the
        // INTEGER PRIMARY KEY binds to the rowid rather than to a record slot.
        // It still has an origin and a declared type, and reporting neither
        // made `sqlite3_column_decltype` empty for the commonest column there
        // is - and `PRAGMA table_info` on a view over one report no type.
        let expr = match expr {
            BoundExpr::Rowid { source } => {
                let alias = self
                    .sources
                    .get(*source)
                    .and_then(|source| source.table.rowid_alias);
                match alias {
                    Some(column) => &BoundExpr::Column {
                        source: *source,
                        column,
                        slot: column,
                        affinity: Affinity::Integer,
                        collation: Collation::Binary,
                    },
                    None => return (None, Vec::new()),
                }
            }
            other => other,
        };
        let BoundExpr::Column { source, column, .. } = expr else {
            return (None, Vec::new());
        };
        let Some(source) = self.sources.get(*source) else {
            return (None, Vec::new());
        };
        let Some(info) = source.table.column(*column) else {
            return (None, Vec::new());
        };
        (
            Some((
                self.catalog.database_name(source.table.database).to_vec(),
                source.table.name.clone(),
                info.name.clone(),
            )),
            info.declared_type.clone(),
        )
    }

    /// Binds one `GROUP BY` term, which may be an ordinal or a result alias.
    fn bind_group_term(
        &mut self,
        id: ExprId,
        columns: &[BoundResultColumn],
    ) -> Result<BoundExpr, ParseError> {
        if let Some(index) = self.as_ordinal(id) {
            let Some(column) = columns.get(index.saturating_sub(1)) else {
                return Err(unsupported(
                    "GROUP BY term is out of range",
                    self.ast.expr_span(id),
                ));
            };
            return Ok(column.expr.clone());
        }
        self.bind_expr(id)
    }

    /// Returns the one-based ordinal an expression is, if it is an integer.
    fn as_ordinal(&self, id: ExprId) -> Option<usize> {
        let Some(Expr::Literal(Literal::Integer(text))) = self.ast.expr(id) else {
            return None;
        };
        let mut value: usize = 0;
        for byte in text {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value
                .saturating_mul(10)
                .saturating_add(usize::from(byte.saturating_sub(b'0')));
        }
        Some(value)
    }

    /// Binds an `ORDER BY` list, resolving ordinals and result aliases.
    fn bind_order_by(
        &mut self,
        terms: &[ast::OrderTerm],
        columns: &[BoundResultColumn],
    ) -> Result<Vec<BoundOrderTerm>, ParseError> {
        let mut bound = Vec::with_capacity(terms.len());
        for term in terms {
            // A bare integer is an ordinal into the result columns; anything
            // else, including `1 + 0`, is an expression. SQLite draws the line
            // at a literal, and so does this.
            let expr = match self.as_ordinal(term.expr) {
                Some(ordinal) => {
                    let Some(column) = ordinal.checked_sub(1).and_then(|index| columns.get(index))
                    else {
                        return Err(order_out_of_range(ordinal, self.ast.expr_span(term.expr)));
                    };
                    column.expr.clone()
                }
                None => self.bind_expr(term.expr)?,
            };
            let collation = expr.collation().unwrap_or(Collation::Binary);
            let nulls = term.nulls.unwrap_or(match term.order {
                // SQLite sorts NULLs first ascending and last descending when
                // no explicit null ordering is written.
                SortOrder::Ascending => NullOrder::First,
                SortOrder::Descending => NullOrder::Last,
            });
            bound.push(BoundOrderTerm {
                expr,
                order: term.order,
                nulls,
                collation,
            });
        }
        Ok(bound)
    }

    /// Binds the `ORDER BY` written inside an aggregate's argument list.
    ///
    /// Not [`Binder::bind_order_by`]: that one resolves a bare integer as an
    /// ordinal into the *result columns*, which an aggregate's own `ORDER BY`
    /// has none of. `group_concat(b ORDER BY 1)` sorts by the literal 1 in
    /// SQLite, which is to say by nothing.
    ///
    /// @param terms - the terms as written
    fn bind_aggregate_order(
        &mut self,
        terms: &[ast::OrderTerm],
    ) -> Result<Vec<BoundOrderTerm>, ParseError> {
        let mut bound = Vec::with_capacity(terms.len());
        for term in terms {
            let expr = self.bind_expr(term.expr)?;
            let collation = expr.collation().unwrap_or(Collation::Binary);
            let nulls = term.nulls.unwrap_or(match term.order {
                SortOrder::Ascending => NullOrder::First,
                SortOrder::Descending => NullOrder::Last,
            });
            bound.push(BoundOrderTerm {
                expr,
                order: term.order,
                nulls,
                collation,
            });
        }
        Ok(bound)
    }

    /// Returns a bound column reference, checking the authorizer.
    fn column_expr(&mut self, source: usize, column: u16) -> Result<BoundExpr, ParseError> {
        let Some(bound) = self.sources.get(source) else {
            return Err(unsupported("unknown source", Span::default()));
        };
        let Some(info) = bound.table.column(column) else {
            return Err(unsupported("unknown column", Span::default()));
        };
        let affinity = info.affinity;
        let collation = self
            .collation_named(&info.collation)
            .unwrap_or(Collation::Binary);
        if bound.table.rowid_alias == Some(column) {
            // An INTEGER PRIMARY KEY column *is* the rowid, and reading it
            // through the record would read a NULL placeholder.
            return Ok(BoundExpr::Rowid { source });
        }
        // A `VIRTUAL` generated column is not in the record at all: it is its
        // own expression, so the reference is replaced by the expression here
        // and nothing below the binder ever sees the column.
        if info.generated && !info.stored {
            let Some(sql) = info.generated_sql.clone() else {
                return Err(unsupported(
                    "a generated column with no expression",
                    Span::default(),
                ));
            };
            self.generating = self.generating.saturating_add(1);
            if self.generating > MAX_GENERATED_DEPTH {
                self.generating = self.generating.saturating_sub(1);
                return Err(ParseError::new(
                    ParseErrorKind::Unsupported("a generated column refers to itself"),
                    Span::default(),
                ));
            }
            let bound = self.bind_schema_expr_for(source, &sql);
            self.generating = self.generating.saturating_sub(1);
            return bound;
        }
        let slot = bound
            .table
            .record_slot(column)
            .unwrap_or(usize::from(column)) as u16;
        Ok(BoundExpr::Column {
            source,
            column,
            slot,
            affinity,
            collation,
        })
    }

    /// Binds a schema expression against one FROM term's scope.
    ///
    /// A generated column's expression names other columns of its own table, so
    /// it is bound with exactly that term visible and nothing else - a name it
    /// cannot resolve there is an error rather than something it picks up from
    /// the query that happened to read it.
    fn bind_schema_expr_for(&mut self, source: usize, sql: &[u8]) -> Result<BoundExpr, ParseError> {
        let saved = core::mem::replace(&mut self.scopes, vec![vec![source]]);
        let bound = self.bind_schema_expr(sql);
        self.scopes = saved;
        bound
    }

    /// Binds a result-column list against the current sources.
    ///
    /// `RETURNING` is a result-column list over the row a DML statement wrote,
    /// so it is bound by the same code that binds a `SELECT` list rather than
    /// by a second implementation that would have to be kept in step with it.
    pub fn bind_result_columns_public(
        &mut self,
        columns: &[ast::ResultColumn],
    ) -> Result<Vec<BoundResultColumn>, ParseError> {
        self.bind_result_columns(columns)
    }

    /// Records that the statement depends on a database's schema.
    pub(crate) fn record_write_dependency(&mut self, database: usize) {
        self.record_dependency(database);
    }

    /// Binds a unary operator over one expression.
    ///
    /// **A negated integer literal is one literal, not an operator over one.**
    /// `-9223372036854775808` is the smallest integer there is; `9223372036854775808` on
    /// its own is one past the largest, so binding the operand first turned it into a real
    /// and the negation then produced `-9.2233720368547758e+18`. Every comparison, every
    /// affinity and every write of that value is a different value from the one that was
    /// written. SQLite folds the sign into the literal in its own parser for exactly this
    /// reason.
    ///
    /// @param op - the operator
    /// @param operand - the expression it applies to
    fn bind_unary(&mut self, op: UnaryOp, operand: ExprId) -> Result<BoundExpr, ParseError> {
        if op == UnaryOp::Negate {
            if let Some(Expr::Literal(Literal::Integer(text))) = self.ast.expr(operand) {
                let mut negated = Vec::with_capacity(text.len().saturating_add(1));
                negated.push(b'-');
                negated.extend_from_slice(text);
                return Ok(integer_literal(&negated));
            }
        }
        let operand = Box::new(self.bind_expr(operand)?);
        match op {
            UnaryOp::Not => Ok(BoundExpr::Not(operand)),
            _ => Ok(BoundExpr::Unary { op, operand }),
        }
    }

    /// Binds one expression.
    pub fn bind_expr(&mut self, id: ExprId) -> Result<BoundExpr, ParseError> {
        let span = self.ast.expr_span(id);
        let Some(expr) = self.ast.expr(id) else {
            return Err(unsupported("missing expression", span));
        };
        // **A literal is bound off the arena, before the clone** (task-2006). `Literal`
        // owns its digits, so `SELECT 1` allocated one byte to copy the byte `1` in order
        // to match on it, and a statement full of literals paid that per literal. The
        // clone below is a borrow split rather than a choice - the arms call `&mut self`
        // methods and need the owned names and sub-expression lists their variants hold -
        // but a literal needs neither.
        if let Expr::Literal(literal) = expr {
            return self.bind_literal(literal, span);
        }
        match expr.clone() {
            Expr::Literal(literal) => self.bind_literal(&literal, span),
            Expr::Parameter { index, .. } => Ok(BoundExpr::Parameter(index)),
            Expr::Column {
                database,
                table,
                column,
            } => self.bind_column_reference(database, table, column, span),
            Expr::Star { .. } => Err(ParseError::new(
                ParseErrorKind::Unexpected {
                    found: "*".to_string(),
                    expected: vec!["an expression"],
                },
                span,
            )),
            Expr::Unary { op, operand } => self.bind_unary(op, operand),
            Expr::Binary { op, left, right } => self.bind_binary(op, left, right),
            Expr::Collate { operand, collation } => {
                let name = self.ast.text(collation);
                let Some(collation) = self.collation_named(name) else {
                    return Err(no_such_collation(name, span));
                };
                let bound = self.bind_expr(operand)?;
                Ok(apply_collation(bound, collation))
            }
            Expr::Cast { operand, declared } => {
                let operand = Box::new(self.bind_expr(operand)?);
                let affinity =
                    inillucent_value::affinity::affinity_of_declared_type(self.ast.text(declared));
                Ok(BoundExpr::Cast { operand, affinity })
            }
            Expr::Pattern {
                negated,
                op,
                operand,
                pattern,
                escape,
            } => {
                if op == PatternOp::Regexp {
                    // `X REGEXP Y` is sugar for `regexp(Y, X)` - the pattern
                    // first - and the operator exists only because the function
                    // does. The reference shell registers one, so this engine
                    // registers one too, and the operator binds to it here
                    // rather than refusing.
                    let subject = self.bind_expr(operand)?;
                    let pattern = self.bind_expr(pattern)?;
                    let call = BoundExpr::Function {
                        func: ScalarFunc::Regexp,
                        arguments: vec![pattern, subject],
                        collation: Collation::Binary,
                    };
                    return Ok(if negated {
                        BoundExpr::Not(Box::new(call))
                    } else {
                        call
                    });
                }
                if op == PatternOp::Match {
                    // `x MATCH y` is a call to a function called `match`, which
                    // does not exist - unless `x` is a column of a virtual
                    // table, in which case it is a constraint the module is
                    // offered and the module says what it means. That is the
                    // whole of how `t MATCH 'word'` reaches FTS5.
                    let left = self.bind_expr(operand)?;
                    if !self.is_virtual_column(&left) {
                        return Err(no_such_function(b"match", span));
                    }
                    let pattern = Box::new(self.bind_expr(pattern)?);
                    return Ok(BoundExpr::Pattern {
                        negated,
                        op: PatternOp::Match,
                        operand: Box::new(left),
                        pattern,
                        escape: None,
                    });
                }
                let operand = Box::new(self.bind_expr(operand)?);
                let pattern = Box::new(self.bind_expr(pattern)?);
                let escape = match escape {
                    Some(expr) => Some(Box::new(self.bind_expr(expr)?)),
                    None => None,
                };
                Ok(BoundExpr::Pattern {
                    negated,
                    op,
                    operand,
                    pattern,
                    escape,
                })
            }
            Expr::Between {
                negated,
                operand,
                low,
                high,
            } => {
                let operand = self.bind_expr(operand)?;
                let low = self.bind_expr(low)?;
                let high = self.bind_expr(high)?;
                let (low_affinity, low_collation) = comparison_rules(&operand, &low);
                let (high_affinity, high_collation) = comparison_rules(&operand, &high);
                Ok(BoundExpr::Between {
                    negated,
                    operand: Box::new(operand),
                    low: Box::new(low),
                    high: Box::new(high),
                    low_affinity,
                    low_collation,
                    high_affinity,
                    high_collation,
                })
            }
            Expr::In {
                negated,
                operand,
                rhs,
            } => {
                // **The row-value `IN` form is an OR of equality chains**, which
                // is exactly what SQLite's `IN` over a value list means: `(a, b)
                // IN (VALUES (1,2),(3,4))` is `(a=1 AND b=2) OR (a=3 AND b=4)`,
                // with the same unknown-rather-than-false behaviour when a part
                // is NULL. The rows are written as a `VALUES` clause, which the
                // grammar parses as a select, so the desugaring reads them back
                // out of it rather than adding a second spelling.
                if let Some(parts) = self.row_value_parts(operand) {
                    return self.bind_row_in(&parts, &rhs, negated, span);
                }
                let operand = self.bind_expr(operand)?;
                let rhs = match rhs {
                    InRhs::Select(select) => {
                        return self.bind_in_subquery(operand, select, negated, span)
                    }
                    InRhs::Table { .. } => {
                        return Err(unsupported("IN over a table name", span));
                    }
                    other => other,
                };
                let InRhs::List(items) = rhs else {
                    return Err(unsupported("IN over a subquery or table", span));
                };
                let mut list = Vec::with_capacity(items.len());
                for item in &items {
                    list.push(self.bind_expr(*item)?);
                }
                let (affinity, collation) = match list.first() {
                    Some(first) => comparison_rules(&operand, first),
                    None => (None, Collation::Binary),
                };
                Ok(BoundExpr::InList {
                    negated,
                    operand: Box::new(operand),
                    list,
                    affinity,
                    collation,
                })
            }
            Expr::IsNull { negated, operand } => Ok(BoundExpr::IsNull {
                negated,
                operand: Box::new(self.bind_expr(operand)?),
            }),
            Expr::Is {
                negated,
                distinct_from,
                left,
                right,
            } => {
                let left = self.bind_expr(left)?;
                let right = self.bind_expr(right)?;
                let (affinity, collation) = comparison_rules(&left, &right);
                // **`DISTINCT FROM` inverts the sense, and it was being
                // dropped.** `a IS b` is already NULL-safe equality, so
                // `a IS NOT DISTINCT FROM b` is `a IS b` and
                // `a IS DISTINCT FROM b` is `a IS NOT b`. Binding the keyword
                // away left `1 IS DISTINCT FROM NULL` meaning `1 IS NULL` -
                // 0 where SQLite answers 1, and 0 again for
                // `1 IS NOT DISTINCT FROM 1`, so both spellings answered the
                // opposite of the truth.
                let negated = negated != distinct_from;
                Ok(BoundExpr::Is {
                    negated,
                    left: Box::new(left),
                    right: Box::new(right),
                    affinity,
                    collation,
                })
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                let bound_operand = match operand {
                    Some(expr) => Some(Box::new(self.bind_expr(expr)?)),
                    None => None,
                };
                let mut bound_branches = Vec::with_capacity(branches.len());
                for (when, then) in &branches {
                    bound_branches.push((self.bind_expr(*when)?, self.bind_expr(*then)?));
                }
                let bound_otherwise = match otherwise {
                    Some(expr) => Some(Box::new(self.bind_expr(expr)?)),
                    None => None,
                };
                let comparisons = match &bound_operand {
                    Some(operand) => bound_branches
                        .iter()
                        .map(|(when, _)| comparison_rules(operand, when))
                        .collect(),
                    None => Vec::new(),
                };
                Ok(BoundExpr::Case {
                    operand: bound_operand,
                    branches: bound_branches,
                    otherwise: bound_otherwise,
                    comparisons,
                })
            }
            Expr::Function {
                name,
                distinct,
                arguments,
                order_by,
                filter,
                over,
            } => {
                if let Some(over) = over {
                    return self.bind_window_call(name, distinct, arguments, filter, over, span);
                }
                // **`FILTER` and an in-argument `ORDER BY` belong to the
                // aggregate, not to the window.** Both were refused here, so
                // `count(*) FILTER (WHERE a > 15)` and
                // `group_concat(b ORDER BY a DESC)` - two shapes an ordinary
                // report is written in - could not be asked at all. They are
                // bound onto the call and applied by the accumulator.
                self.bind_call_with(name, distinct, arguments, filter, &order_by, span)
            }
            Expr::Exists { negated, select } => {
                let block = self.bind_value_subquery(select, span)?;
                Ok(BoundExpr::Subquery {
                    id: self.next_subquery_id(),
                    kind: SubqueryKind::Exists,
                    negated,
                    operand: None,
                    block: Box::new(block),
                    affinity: None,
                    collation: Collation::Binary,
                })
            }
            Expr::Subquery(select) => {
                let block = self.bind_value_subquery(select, span)?;
                if block.columns.len() != 1 {
                    return Err(ParseError::new(
                        ParseErrorKind::Unsupported("sub-select returns more than one column"),
                        span,
                    ));
                }
                Ok(BoundExpr::Subquery {
                    id: self.next_subquery_id(),
                    kind: SubqueryKind::Scalar,
                    negated: false,
                    operand: None,
                    block: Box::new(block),
                    affinity: None,
                    collation: Collation::Binary,
                })
            }
            Expr::RowValue(_) => Err(unsupported("row values", span)),
            Expr::Raise { action, message } => {
                // Outside a trigger body there is nothing for it to abandon, so
                // SQLite refuses it there rather than treating it as a no-op.
                if self.row_aliases.is_none() {
                    return Err(unsupported("RAISE outside a trigger", span));
                }
                Ok(BoundExpr::Raise {
                    action,
                    message: message.clone(),
                    foreign_key: false,
                })
            }
        }
    }

    /// Binds a literal, converting its written text into a value.
    fn bind_literal(&self, literal: &Literal, _span: Span) -> Result<BoundExpr, ParseError> {
        match literal {
            Literal::Null => Ok(BoundExpr::Null),
            Literal::Boolean(value) => Ok(BoundExpr::Integer(i64::from(*value))),
            Literal::Integer(text) => Ok(integer_literal(text)),
            Literal::Float(text) => {
                let parsed =
                    inillucent_value::numeric::atof(text, inillucent_value::TextEncoding::Utf8);
                Ok(BoundExpr::Real(parsed.value))
            }
            Literal::String(text) => Ok(BoundExpr::Text(text.clone())),
            Literal::Blob(bytes) => Ok(BoundExpr::Blob(bytes.clone())),
            Literal::CurrentDate | Literal::CurrentTime | Literal::CurrentTimestamp => {
                // The three keywords are the three functions with no argument,
                // and `CURRENT_TIMESTAMP` is `datetime('now')` rather than a
                // fourth thing that formats differently.
                let func = match literal {
                    Literal::CurrentDate => TimeFunc::Date,
                    Literal::CurrentTime => TimeFunc::Time,
                    _ => TimeFunc::DateTime,
                };
                Ok(BoundExpr::Time {
                    func,
                    arguments: Vec::new(),
                })
            }
        }
    }

    /// Resolves `excluded.column` inside an upsert's `DO UPDATE`.
    ///
    /// `excluded` is only in scope there, so a query that uses the name
    /// anywhere else gets the ordinary "no such table" answer rather than a
    /// row that came from nowhere.
    fn bind_excluded_column(&mut self, folded: &[u8], span: Span) -> Result<BoundExpr, ParseError> {
        let Some(table) = self.excluded.clone() else {
            return Err(no_such_table(b"excluded", span));
        };
        if let Some(position) = table.column_position(folded) {
            if table.rowid_alias == Some(position) {
                return Ok(BoundExpr::Rowid {
                    source: EXCLUDED_SOURCE,
                });
            }
            let Some(info) = table.column(position) else {
                return Err(no_such_column(folded, span));
            };
            let collation = self
                .collation_named(&info.collation)
                .unwrap_or(Collation::Binary);
            return Ok(BoundExpr::Column {
                source: EXCLUDED_SOURCE,
                column: position,
                // `excluded` is a row in registers rather than a record, so the
                // compiler substitutes it wholesale and the slot is never read.
                slot: position,
                affinity: info.affinity,
                collation,
            });
        }
        if table.is_rowid_name(folded) {
            return Ok(BoundExpr::Rowid {
                source: EXCLUDED_SOURCE,
            });
        }
        Err(no_such_column(folded, span))
    }

    /// Resolves `old.column` or `new.column` inside a trigger body.
    ///
    /// The event decides which of the two exists: an INSERT has no previous row
    /// and a DELETE has no next one. Naming the missing one is the ordinary
    /// "no such table" error, because that is what it is - outside a trigger
    /// body neither name resolves at all.
    fn bind_row_alias_column(
        &mut self,
        source: usize,
        folded: &[u8],
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        let written: &[u8] = if source == OLD_SOURCE { b"old" } else { b"new" };
        let Some(aliases) = self.row_aliases.clone() else {
            return Err(no_such_table(written, span));
        };
        let available = if source == OLD_SOURCE {
            aliases.old
        } else {
            aliases.new
        };
        if !available {
            return Err(no_such_table(written, span));
        }
        let table = &aliases.table;
        if let Some(position) = table.column_position(folded) {
            if table.rowid_alias == Some(position) {
                return Ok(BoundExpr::Rowid { source });
            }
            let Some(info) = table.column(position) else {
                return Err(no_such_column(folded, span));
            };
            let collation = self
                .collation_named(&info.collation)
                .unwrap_or(Collation::Binary);
            return Ok(BoundExpr::Column {
                source,
                column: position,
                // The row lives in registers rather than in a record, so the
                // compiler substitutes it wholesale and the slot is never read.
                slot: position,
                affinity: info.affinity,
                collation,
            });
        }
        if table.is_rowid_name(folded) {
            return Ok(BoundExpr::Rowid { source });
        }
        Err(no_such_column(folded, span))
    }

    /// Resolves a column reference against the scope stack.
    ///
    /// The innermost block is searched first and a hit there ends the search,
    /// so an inner name shadows an outer one. A hit in an enclosing block is
    /// recorded as a correlation, which is the fact the compiler uses to decide
    /// whether the block runs once or once per outer row.
    fn bind_column_reference(
        &mut self,
        database: Option<ast::NameId>,
        table: Option<ast::NameId>,
        column: ast::NameId,
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        let folded = self.ast.folded(column).to_vec();
        let table_folded = table.map(|id| self.ast.folded(id).to_vec());
        let database_folded = database.map(|id| self.ast.folded(id).to_vec());
        if table_folded.as_deref() == Some(b"excluded".as_slice()) {
            return self.bind_excluded_column(&folded, span);
        }
        // `OLD` and `NEW` shadow a table of the same name only inside a trigger
        // body, which is the one place they mean anything.
        if self.row_aliases.is_some() && database.is_none() {
            match table_folded.as_deref() {
                Some(b"old") => return self.bind_row_alias_column(OLD_SOURCE, &folded, span),
                Some(b"new") => return self.bind_row_alias_column(NEW_SOURCE, &folded, span),
                _ => {}
            }
        }
        let mut resolved: Option<(usize, u16)> = None;
        let mut rowid_of: Option<usize> = None;
        let levels = self.scopes.len();
        for level in (0..levels).rev() {
            let ids: Vec<usize> = self
                .scopes
                .get(level)
                .map_or(Vec::new(), |scope| scope.clone());
            let mut found: Option<(usize, u16)> = None;
            let mut rowid_here: Option<usize> = None;
            for id in ids {
                let Some(source) = self.sources.get(id) else {
                    continue;
                };
                if let Some(qualifier) = table_folded.as_deref() {
                    if !source.alias.eq_ignore_ascii_case(qualifier) {
                        continue;
                    }
                }
                if let Some(qualifier) = database_folded.as_deref() {
                    if !self
                        .catalog
                        .database_name(source.table.database)
                        .eq_ignore_ascii_case(qualifier)
                    {
                        continue;
                    }
                }
                if let Some(index) = source.table.column_position(&folded) {
                    // **A `USING` or `NATURAL` join coalesces the named
                    // column.** The join has one `k`, not two: it comes from
                    // the left term, and the right term's copy is suppressed -
                    // from `*`, which this already did, and from an
                    // *unqualified* reference, which it did not. That is why
                    // `SELECT * FROM a JOIN b USING (k) ORDER BY k` answered
                    // `ambiguous column name: k`, and why four of the five join
                    // spellings failed on one message. A qualified `b.k` still
                    // reaches the right-hand copy, which is what SQLite does.
                    if table_folded.is_none() && source.suppressed.contains(&index) {
                        continue;
                    }
                    if found.is_some() {
                        return Err(ambiguous_column(self.ast.text(column), span));
                    }
                    found = Some((id, index));
                    continue;
                }
                if source.table.is_rowid_name(&folded) && rowid_here.is_none() {
                    rowid_here = Some(id);
                }
            }
            if found.is_some() {
                resolved = found;
                break;
            }
            if let Some(id) = rowid_here {
                rowid_of = Some(id);
                break;
            }
        }
        if let Some((source, index)) = resolved {
            let (database_name, table_name, column_name) = {
                let Some(bound) = self.sources.get(source) else {
                    return Err(unsupported("unknown source", span));
                };
                let Some(info) = bound.table.column(index) else {
                    return Err(unsupported("unknown column", span));
                };
                (
                    self.catalog.database_name(bound.table.database).to_vec(),
                    bound.table.name.clone(),
                    info.name.clone(),
                )
            };
            match self.authorizer.authorize(AuthAction::Read {
                database: &database_name,
                table: &table_name,
                column: &column_name,
            }) {
                Authorization::Allow => {}
                Authorization::Deny => return Err(denied("not authorized", span)),
                Authorization::Ignore => return Ok(BoundExpr::Null),
            }
            self.note_correlation(source);
            return self.column_expr(source, index);
        }
        if let Some(source) = rowid_of {
            self.note_correlation(source);
            return Ok(BoundExpr::Rowid { source });
        }
        // A result alias is visible to GROUP BY, HAVING and ORDER BY, and only
        // after a real column has failed to match, which is SQLite's order.
        if table_folded.is_none() {
            if let Some((_, expr)) = self
                .result_aliases
                .iter()
                .find(|(name, _)| name.as_slice() == folded.as_slice())
            {
                return Ok(expr.clone());
            }
        }
        if self.sources.is_empty() && table_folded.is_none() {
            return Err(no_such_column_quoted(
                self.ast.text(column),
                self.ast
                    .name(column)
                    .map(|name| name.quote)
                    .unwrap_or(QuoteForm::Bare),
                span,
            ));
        }
        match table_folded {
            Some(_)
                if !self.sources.iter().any(|source| {
                    table_folded
                        .as_deref()
                        .is_some_and(|q| source.alias.eq_ignore_ascii_case(q))
                }) =>
            {
                Err(no_such_table(
                    table.map(|id| self.ast.text(id)).unwrap_or(b""),
                    span,
                ))
            }
            _ if table_folded.is_none() => Err(no_such_column_quoted(
                self.ast.text(column),
                self.ast
                    .name(column)
                    .map(|name| name.quote)
                    .unwrap_or(QuoteForm::Bare),
                span,
            )),
            // A qualified reference names both halves, which is what the
            // reference prints: `no such column: t.b`, not `no such column: b`.
            _ => {
                let qualifier = table.map(|id| self.ast.text(id)).unwrap_or(b"");
                Err(no_such_column(
                    &[qualifier, b".", self.ast.text(column)].concat(),
                    span,
                ))
            }
        }
    }

    /// Binds a binary operator, choosing comparison or arithmetic semantics.
    fn bind_binary(
        &mut self,
        op: BinaryOp,
        left: ExprId,
        right: ExprId,
    ) -> Result<BoundExpr, ParseError> {
        // **A row-value comparison is a comparison of its parts.** `(a, b) =
        // (1, 2)` is `a = 1 AND b = 2`, and the ordering operators are
        // lexicographic - `(a, b) < (x, y)` is `a < x OR (a = x AND b < y)`,
        // which is where the NULL behaviour comes from rather than being a rule
        // of its own. It is desugared here rather than carried into the plan
        // because there is nothing about it the executor would do differently:
        // the parts are ordinary comparisons over ordinary expressions.
        if let (Some(lefts), Some(rights)) =
            (self.row_value_parts(left), self.row_value_parts(right))
        {
            return self.bind_row_comparison(op, &lefts, &rights, self.ast.expr_span(left));
        }
        // **A row value against a query**, which is the form an application
        // actually writes: `WHERE (a, b) = (SELECT a, b FROM t WHERE id = 3)`.
        // Only the row-against-a-row spelling was desugared, so this was
        // `unsupported: row values`.
        if let (Some(lefts), Some(select)) = (
            self.row_value_parts(left),
            self.ast.expr(right).and_then(|expr| match expr {
                Expr::Subquery(select) => Some(*select),
                _ => None,
            }),
        ) {
            return self.bind_row_against_query(op, &lefts, select, self.ast.expr_span(left));
        }
        let bound_left = self.bind_expr(left)?;
        let bound_right = self.bind_expr(right)?;
        match op {
            BinaryOp::And => Ok(BoundExpr::And(Box::new(bound_left), Box::new(bound_right))),
            BinaryOp::Or => Ok(BoundExpr::Or(Box::new(bound_left), Box::new(bound_right))),
            BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterEqual => {
                let (affinity, collation) = comparison_rules(&bound_left, &bound_right);
                Ok(BoundExpr::Compare {
                    op,
                    left: Box::new(bound_left),
                    right: Box::new(bound_right),
                    affinity,
                    collation,
                })
            }
            BinaryOp::Regexp => Ok(BoundExpr::Function {
                func: ScalarFunc::Regexp,
                arguments: vec![bound_right, bound_left],
                collation: Collation::Binary,
            }),
            // **pgvector's distance operators are sugar for the functions**,
            // which is exactly what they are in pgvector too: an operator class
            // over a function, so that an index can be asked for the same
            // ordering the expression writes. `<#>` is the odd one, and it is
            // odd in pgvector as well - it answers the *negative* inner product,
            // so that a smaller number is a better match and one index
            // direction serves every operator.
            BinaryOp::L2Distance
            | BinaryOp::CosineDistance
            | BinaryOp::L1Distance
            | BinaryOp::HammingDistance
            | BinaryOp::JaccardDistance => Ok(BoundExpr::Function {
                func: match op {
                    BinaryOp::L2Distance => ScalarFunc::VectorDistanceL2,
                    BinaryOp::CosineDistance => ScalarFunc::VectorDistanceCos,
                    BinaryOp::L1Distance => ScalarFunc::VectorDistanceL1,
                    BinaryOp::HammingDistance => ScalarFunc::VectorDistanceHamming,
                    _ => ScalarFunc::VectorDistanceJaccard,
                },
                arguments: vec![bound_left, bound_right],
                collation: Collation::Binary,
            }),
            BinaryOp::NegativeInnerProduct => Ok(BoundExpr::Unary {
                op: UnaryOp::Negate,
                operand: Box::new(BoundExpr::Function {
                    func: ScalarFunc::VectorDot,
                    arguments: vec![bound_left, bound_right],
                    collation: Collation::Binary,
                }),
            }),
            BinaryOp::Match => Err(no_such_function(b"match", self.ast.expr_span(right))),
            BinaryOp::Extract | BinaryOp::ExtractText => Ok(BoundExpr::Json {
                func: if op == BinaryOp::Extract {
                    JsonFunc::Arrow
                } else {
                    JsonFunc::ArrowShift
                },
                arguments: vec![bound_left, bound_right],
            }),
            _ => {
                // **A vector has no arithmetic, and answering zero is worse
                // than refusing.** `v + v` used to be accepted and answer
                // `0.0`: the blob went through numeric affinity, which reads no
                // leading digits and calls that nothing. pgvector defines `+`
                // element-wise; this engine does not implement it, and a
                // caller who wrote it gets told so rather than getting a
                // column of zeroes.
                // **Element-wise, which is what pgvector defines.** `+`, `-`
                // and `*` over two vectors work component by component, and
                // `*` with a number on one side scales. Anything else over a
                // vector - a division, a modulo, a shift - has no pgvector
                // meaning, and answering `0.0` for it is worse than refusing:
                // the blob would go through numeric affinity, which reads no
                // leading digits and calls that nothing.
                if let Some(func) = match op {
                    BinaryOp::Add => Some(ScalarFunc::VectorAdd),
                    BinaryOp::Subtract => Some(ScalarFunc::VectorSubtract),
                    BinaryOp::Multiply => Some(ScalarFunc::VectorMultiply),
                    _ => None,
                } {
                    if self.reads_a_vector(&bound_left) || self.reads_a_vector(&bound_right) {
                        return Ok(BoundExpr::Function {
                            func,
                            arguments: vec![bound_left, bound_right],
                            collation: Collation::Binary,
                        });
                    }
                }
                if self.reads_a_vector(&bound_left) || self.reads_a_vector(&bound_right) {
                    return Err(unsupported(
                        "arithmetic over a vector column",
                        self.ast.expr_span(left),
                    ));
                }
                Ok(BoundExpr::Arithmetic {
                    op,
                    left: Box::new(bound_left),
                    right: Box::new(bound_right),
                })
            }
        }
    }

    /// Reports whether an expression is a reference to a `VECTOR` column.
    ///
    /// Only a bare reference, and deliberately: `length(v)` and `hex(v)` are
    /// questions about the bytes and answer them, and a general "does this
    /// expression have vector in it anywhere" rule would refuse those too.
    ///
    /// @param expr - the bound expression to look at
    fn reads_a_vector(&self, expr: &BoundExpr) -> bool {
        let BoundExpr::Column { source, column, .. } = expr else {
            return false;
        };
        self.sources
            .iter()
            .find(|held| held.id == *source)
            .and_then(|held| held.table.columns.get(usize::from(*column)))
            .is_some_and(crate::catalog_view::ColumnInfo::is_vector)
    }

    /// Binds a call that may carry a `FILTER` and an in-argument `ORDER BY`.
    ///
    /// Both belong to an *aggregate* call and are dropped for anything else,
    /// which is what the arity and aggregate checks below already establish:
    /// a scalar call cannot reach the arm that reads them.
    ///
    /// @param name - the function name
    /// @param distinct - whether `DISTINCT` was written
    /// @param arguments - the argument list, or `None` for `count(*)`
    /// @param filter - the `FILTER (WHERE ...)` clause, when one was written
    /// @param order_by - the `ORDER BY` inside the argument list
    /// @param span - where the call was written
    fn bind_call_with(
        &mut self,
        name: ast::NameId,
        distinct: bool,
        arguments: Option<Vec<ExprId>>,
        filter: Option<ExprId>,
        order_by: &[ast::OrderTerm],
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        let folded = self.ast.folded(name).to_vec();
        if self
            .authorizer
            .authorize(AuthAction::Function { name: &folded })
            == Authorization::Deny
        {
            return Err(denied("not authorized", span));
        }
        let star = arguments.is_none();
        let list = arguments.unwrap_or_default();
        if !star && !distinct && !list.is_empty() {
            if let Some(bound) = self.bind_auxiliary_call(&folded, &list, span)? {
                return Ok(bound);
            }
        }
        if !star {
            if let Some(bound) = self.bind_external_call(&folded, &list, distinct, span)? {
                return Ok(bound);
            }
        }
        if function::is_aggregate_call(&folded, list.len(), star) {
            let Some(func) =
                function::lookup_aggregate(&folded).or_else(|| function::minmax_aggregate(&folded))
            else {
                return Err(no_such_function(&folded, span));
            };
            if !self.allow_aggregates || self.inside_aggregate {
                return Err(unsupported("misuse of aggregate function", span));
            }
            if star && func != AggregateFunc::Count {
                return Err(wrong_arguments(&folded, span));
            }
            if !function::aggregate_arity_ok(func, if star { 0 } else { list.len() }, star) {
                return Err(wrong_arguments(&folded, span));
            }
            self.inside_aggregate = true;
            let mut bound = Vec::with_capacity(list.len());
            for argument in &list {
                bound.push(self.bind_expr(*argument)?);
            }
            self.inside_aggregate = false;
            // The `FILTER` and the `ORDER BY` read the row the aggregate is
            // folding, so they bind in the same scope the arguments did - and
            // outside `inside_aggregate`, because neither may itself contain
            // an aggregate.
            let bound_filter = match filter {
                Some(expr) => Some(self.bind_expr(expr)?),
                None => None,
            };
            let bound_order = self.bind_aggregate_order(order_by)?;
            let collation = bound
                .first()
                .and_then(BoundExpr::collation)
                .unwrap_or(Collation::Binary);
            // The same reason `v + v` refuses: `sum(v)` and `avg(v)` coerced
            // the blob through numeric affinity and answered `0.0` for a whole
            // column of embeddings. pgvector's `avg(vector)` is an element-wise
            // mean; this engine does not compute one, and says so.
            // **A vector column folds component by component.** `sum(v)` and
            // `avg(v)` over embeddings used to coerce the blob through numeric
            // affinity and answer `0.0` for a whole column; pgvector defines
            // them as element-wise, and this is that - chosen here, where the
            // argument's type is known, rather than at run time where a blob is
            // just a blob.
            let func = match func {
                function::AggregateFunc::Sum | function::AggregateFunc::Total
                    if bound.iter().any(|argument| self.reads_a_vector(argument)) =>
                {
                    function::AggregateFunc::VectorSum
                }
                function::AggregateFunc::Avg
                    if bound.iter().any(|argument| self.reads_a_vector(argument)) =>
                {
                    function::AggregateFunc::VectorAvg
                }
                other => other,
            };
            // **`DISTINCT` takes exactly one argument (task-1913).** SQLite
            // answers `DISTINCT aggregates must have exactly one argument`,
            // and this accepted `group_concat(DISTINCT s, ',')` and answered
            // it - a statement the reference cannot read, which is the same
            // class `refusals_match_the_oracle` exists to stop. There is
            // nothing for the second argument to be distinct *by*: the
            // de-duplication compares the first value alone, so the separator
            // of whichever duplicate arrived first is the one that survives.
            if distinct && bound.len() > 1 {
                return Err(refused(
                    "DISTINCT aggregates must have exactly one argument",
                    span,
                ));
            }
            let candidate = BoundAggregate {
                func,
                external: None,
                distinct,
                arguments: bound,
                star,
                collation,
                filter: bound_filter,
                order_by: bound_order,
            };
            return Ok(self.aggregate_slot(candidate));
        }
        if let Some(func) = function::lookup_time(&folded) {
            if star {
                return Err(wrong_arguments(&folded, span));
            }
            if distinct {
                return Err(unsupported("DISTINCT in a scalar function", span));
            }
            if func == function::TimeFunc::TimeDiff && list.len() != 2 {
                return Err(wrong_arguments(&folded, span));
            }
            if func == function::TimeFunc::StrfTime && list.is_empty() {
                return Err(wrong_arguments(&folded, span));
            }
            let mut bound = Vec::with_capacity(list.len());
            for argument in &list {
                bound.push(self.bind_expr(*argument)?);
            }
            return Ok(BoundExpr::Time {
                func,
                arguments: bound,
            });
        }
        if let Some(func) = function::lookup_math(&folded) {
            if star {
                return Err(wrong_arguments(&folded, span));
            }
            if distinct {
                return Err(unsupported("DISTINCT in a scalar function", span));
            }
            let (least, most) = func.arity();
            if list.len() < least || list.len() > most {
                return Err(wrong_arguments(&folded, span));
            }
            let mut bound = Vec::with_capacity(list.len());
            for argument in &list {
                bound.push(self.bind_expr(*argument)?);
            }
            return Ok(BoundExpr::Math {
                func,
                arguments: bound,
            });
        }
        if let Some(func) = function::lookup_json(&folded) {
            if star {
                return Err(wrong_arguments(&folded, span));
            }
            if distinct {
                return Err(unsupported("DISTINCT in a scalar function", span));
            }
            if !func.arity_ok(list.len()) {
                return Err(wrong_arguments(&folded, span));
            }
            let mut bound = Vec::with_capacity(list.len());
            for argument in &list {
                bound.push(self.bind_expr(*argument)?);
            }
            return Ok(BoundExpr::Json {
                func,
                arguments: bound,
            });
        }
        // **`subtype` is answered where the producing function is known.**
        // A subtype is not a property of a value here - `Value` has no slot
        // for one - it is a property of the *call* that made it, which is
        // exactly what the reference records at run time and what the binder
        // can see. The one call whose answer depends on the data is
        // `json_extract`, which carries the JSON subtype only when what it
        // extracted was itself an array or an object; that one is left to run.
        if folded == b"subtype" && list.len() == 1 {
            let Some(argument) = list.first().copied() else {
                return Err(wrong_arguments(&folded, span));
            };
            let bound = self.bind_expr(argument)?;
            // A JSON group aggregate carries the subtype too, and its function
            // is in the binder's list rather than in the expression - so the
            // slot is resolved here, where the list is.
            if let BoundExpr::Aggregate { slot, .. } = &bound {
                let carries = matches!(
                    self.aggregates.get(*slot).map(|held| held.func),
                    Some(
                        function::AggregateFunc::JsonGroupArray
                            | function::AggregateFunc::JsonGroupObject
                    )
                );
                return Ok(BoundExpr::Integer(if carries { 74 } else { 0 }));
            }
            return Ok(match json_subtype(&bound) {
                Subtyped::Always => BoundExpr::Integer(74),
                Subtyped::Never => BoundExpr::Integer(0),
                Subtyped::WhenShaped => BoundExpr::Function {
                    func: function::ScalarFunc::Subtype,
                    arguments: vec![bound],
                    collation: Collation::Binary,
                },
            });
        }
        let Some(func) = function::lookup_scalar(&folded) else {
            return Err(no_such_function(&folded, span));
        };
        if star {
            return Err(wrong_arguments(&folded, span));
        }
        if distinct {
            return Err(unsupported("DISTINCT in a scalar function", span));
        }
        if !function::scalar_arity_ok(func, list.len()) {
            return Err(wrong_arguments(&folded, span));
        }
        let mut bound = Vec::with_capacity(list.len());
        for argument in &list {
            bound.push(self.bind_expr(*argument)?);
        }
        let collation = bound
            .first()
            .and_then(BoundExpr::collation)
            .unwrap_or(Collation::Binary);
        Ok(BoundExpr::Function {
            func,
            arguments: bound,
            collation,
        })
    }
}

/// Returns a refusal whose text is computed rather than a fixed phrase.
///
/// `Unsupported` carries a `&'static str` because most refusals are one of a
/// closed set of phrases and interning them keeps the error type cheap. A
/// refusal that has to name a column or count something cannot be one of those,
/// so it carries the whole sentence.
///
/// **`Refused`, not `Unexpected`.** It used to be reported as
/// an unexpected-input failure carrying the sentence, on the reasoning that
/// this is the shape SQLite's own messages take - and it is not.
/// `ParseErrorKind::Unexpected` renders as `near "X": syntax error`, so
/// `CREATE TABLE t(a)` on a table that exists answered
/// `near "table t already exists": syntax error` where the reference answers
/// `table t already exists`. Forty-seven refusals in `directive.rs` alone took
/// that shape, and the register audit's own probe is what printed it side by
/// side. `Refused` is the variant whose whole purpose is a sentence the schema
/// wants said in the reference's words, and it renders as one.
pub(crate) fn refused(detail: impl Into<String>, span: Span) -> ParseError {
    ParseError::new(ParseErrorKind::Refused(detail.into()), span)
}

/// Builds the table a nested query's rows are read through.
///
/// The columns are the block's result columns. Their affinity and collation
/// come from the expressions behind them, so a comparison against a subquery
/// column applies the rules it would have applied one level down; a column with
/// no affinity of its own gets none, which is what SQLite does for an
/// expression that is not a bare column or a cast.
/// Returns the columns a nested query's result presents to a reader.
///
/// Public because a write to a view needs them before there is a FROM term to
/// hang them on: the view's catalog entry carries no column list at all.
pub fn subquery_columns(select: &BoundSelect, names: &[Vec<u8>]) -> Vec<ColumnInfo> {
    select
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let name = names
                .get(index)
                .cloned()
                .unwrap_or_else(|| column.name.clone());
            let folded = name.to_ascii_lowercase();
            let collation = column.expr.collation().unwrap_or(Collation::Binary);
            ColumnInfo {
                name,
                folded,
                declared_type: column.declared_type.clone(),
                affinity: column.expr.affinity().unwrap_or(Affinity::Blob),
                collation: collation.name().as_bytes().to_ascii_lowercase(),
                not_null: false,
                not_null_conflict: None,
                primary_key_conflict: None,
                default_sql: None,
                primary_key_position: None,
                hidden: false,
                generated: false,
                stored: false,
                generated_sql: None,
            }
        })
        .collect()
}

/// Returns a block that reads one FROM term and nothing else.
///
/// Everything a `SELECT` can carry is empty here on purpose: this exists to
/// wrap a term the binder has already produced so the compiler can iterate it,
/// not to stand in for a query somebody wrote.
pub fn block_over(
    source: BoundSource,
    filter: Option<BoundExpr>,
    columns: Vec<BoundResultColumn>,
) -> BoundSelect {
    BoundSelect {
        sources: vec![source],
        filter,
        group_by: Vec::new(),
        having: None,
        columns,
        distinct: false,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        aggregates: Vec::new(),
        values: Vec::new(),
        compounds: Vec::new(),
        windows: Vec::new(),
        correlations: Vec::new(),
    }
}

fn subquery_table(alias: &[u8], names: &[Vec<u8>], select: &BoundSelect) -> TableInfo {
    TableInfo::subquery(alias.to_vec(), 0, subquery_columns(select, names))
}

/// Returns the aggregate a name spells inside an `OVER` clause.
///
/// `min` and `max` are the awkward pair: with one argument they are aggregates
/// and with two or more they are scalars, and only the argument count tells
/// them apart. Inside a window the one-argument form is always the aggregate,
/// which is why the ordinary aggregate lookup - which has to leave them out -
/// is not enough here.
fn window_aggregate(folded: &[u8], arguments: usize) -> Option<AggregateFunc> {
    if let Some(func) = function::lookup_aggregate(folded) {
        return Some(func);
    }
    match (folded, arguments) {
        (b"min", 1) => Some(AggregateFunc::Min),
        (b"max", 1) => Some(AggregateFunc::Max),
        _ => None,
    }
}

/// Returns a "no such window" failure.
fn no_such_window(name: &[u8], span: Span) -> ParseError {
    ParseError::new(
        ParseErrorKind::Refused(format!("no such window: {}", String::from_utf8_lossy(name))),
        span,
    )
}

/// Returns an authorizer refusal.
fn denied(what: &'static str, span: Span) -> ParseError {
    ParseError::new(ParseErrorKind::Unsupported(what), span)
}

/// Whether a bound expression carries the JSON subtype.
///
/// SQLite marks a value with the subtype `74` - the letter `J` - when it was
/// produced by a function that returns JSON *text*. The binary spellings do
/// not carry it (a `jsonb_` result is a blob, and a blob read back out of a
/// column has no subtype either), and the functions that answer a number or a
/// type name are not JSON at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Subtyped {
    /// The call always marks its answer.
    Always,
    /// The call never does.
    Never,
    /// It depends on what came out: `json_extract` marks an array or an
    /// object and does not mark the scalar it may equally have found.
    WhenShaped,
}

/// Returns whether an expression's value carries the JSON subtype.
///
/// @param bound - the argument to `subtype`
fn json_subtype(bound: &BoundExpr) -> Subtyped {
    let BoundExpr::Json { func, .. } = bound else {
        return Subtyped::Never;
    };
    use function::JsonFunc;
    match func {
        JsonFunc::Extract | JsonFunc::Arrow => Subtyped::WhenShaped,
        JsonFunc::Jsonb
        | JsonFunc::ArrayB
        | JsonFunc::ExtractB
        | JsonFunc::InsertB
        | JsonFunc::ObjectB
        | JsonFunc::PatchB
        | JsonFunc::RemoveB
        | JsonFunc::ReplaceB
        | JsonFunc::SetB
        | JsonFunc::ArrayInsertB
        | JsonFunc::ArrowShift
        | JsonFunc::ArrayLength
        | JsonFunc::ErrorPosition
        | JsonFunc::Type
        | JsonFunc::Valid
        | JsonFunc::Pretty => Subtyped::Never,
        _ => Subtyped::Always,
    }
}
