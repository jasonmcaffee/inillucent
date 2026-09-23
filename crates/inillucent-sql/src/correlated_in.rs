//! Lowering a correlated `IN` subquery to `EXISTS`.
//!
//! Invariant: **the lowering answers what SQLite answers, NULLs included.**
//! `x IN (SELECT y FROM s WHERE p)` is not "is there a row where `y = x`": an
//! empty list is false even when `x` is NULL, a NULL `x` over a list with rows
//! is NULL, and a list that holds a NULL turns a non-match into NULL. A
//! lowering that kept only the first of those would answer false where SQLite
//! answers NULL, and `WHERE` treats the two the same - so the difference would
//! never show in a row count and would show in an `IS NULL` or a `CASE`.
//!
//! **Why it is a rewrite at all.** The physical pass computes a correlated
//! block once per outer row and hands the operator one column; an `IN` needs
//! the whole list, so `crates/inillucent-exec/src/correlate.rs` refused one by
//! name rather than answering wrongly. `EXISTS` over the same block is what the
//! engine already runs per row, so writing the `IN` as three `EXISTS` tests
//! reaches an execution path that exists (task-1979, section 8.2, gap 1).
//!
//! The three blocks are: does the list have any row, does it hold a row equal
//! to the operand, and does it hold a NULL. The `CASE` below orders them so
//! that each one is only consulted where its answer decides the result.
//!
//! **What is not lowered.** A block with `GROUP BY`, `HAVING`, `DISTINCT`,
//! `LIMIT`, `OFFSET`, a compound arm, a window or a `VALUES` list keeps the
//! refusal: the equality is pushed into the block's `WHERE`, and a `WHERE` is
//! applied before grouping and before a limit, so pushing it past either would
//! ask a different question.

use inillucent_value::{Affinity, Collation};

use crate::ast::BinaryOp;
use crate::bind::{BoundExpr, BoundSelect, BoundStatement, SubqueryKind};

/// Rewrites every correlated `IN` in a statement into `EXISTS` tests.
///
/// Called after binding and before planning. A block this cannot lower is left
/// exactly as it was, so the refusal that names it still fires.
///
/// @param statement - the bound statement, rewritten in place
pub fn lower(statement: &mut BoundStatement) {
    let mut next = highest_subquery_id(statement).saturating_add(1);
    let mut rewrite = |expr: &mut BoundExpr| lower_one(expr, &mut next);
    match statement {
        BoundStatement::Select(select) => crate::rewrite::rewrite_select(select, &mut rewrite),
        BoundStatement::Insert(insert) => crate::rewrite::rewrite_insert(insert, &mut rewrite),
        BoundStatement::Update(update) => crate::rewrite::rewrite_update(update, &mut rewrite),
        BoundStatement::Delete(delete) => crate::rewrite::rewrite_delete(delete, &mut rewrite),
        BoundStatement::Directive(_) | BoundStatement::Empty => {}
    }
}

/// Replaces one expression when it is a correlated `IN` this can lower.
///
/// @param expr - the expression, replaced in place
/// @param next - the next free subquery number, advanced by three on a rewrite
fn lower_one(expr: &mut BoundExpr, next: &mut usize) {
    let BoundExpr::Subquery {
        kind: SubqueryKind::In,
        negated,
        operand: Some(operand),
        block,
        affinity,
        collation,
        ..
    } = expr
    else {
        return;
    };
    if block.correlations.is_empty() || !liftable(block) {
        return;
    }
    let listed = match block.columns.first() {
        Some(column) => column.expr.clone(),
        None => return,
    };
    let replacement = lowered(
        &Lowering {
            operand: (**operand).clone(),
            listed,
            negated: *negated,
            affinity: affinity.clone(),
            collation: collation.clone(),
        },
        block,
        next,
    );
    *expr = replacement;
}

/// Returns whether a block's `WHERE` decides the same rows the block reports.
///
/// The equality is pushed into the block's `WHERE`, so anything that reads the
/// rows *after* the `WHERE` - grouping, a limit, a compound arm - would be
/// asked a different question by the rewritten block.
///
/// @param block - the subquery's own select
fn liftable(block: &BoundSelect) -> bool {
    block.group_by.is_empty()
        && block.having.is_none()
        && !block.distinct
        && block.limit.is_none()
        && block.offset.is_none()
        && block.compounds.is_empty()
        && block.windows.is_empty()
        && block.aggregates.is_empty()
        && block.values.is_empty()
        && !block.sources.is_empty()
}

/// What one `IN` was written as, which is what the lowering needs.
///
/// A struct rather than six parameters, because
/// `crates/inillucent-compat/tests/policy.rs` refuses an
/// `#[allow(clippy::too_many_arguments)]`: the threshold is set once in
/// `clippy.toml` with the argument for where it is, and an attribute moves the
/// bar for one function and says nothing about why.
struct Lowering {
    /// The left side of the `IN`.
    operand: BoundExpr,
    /// The block's first result column, which is what `IN` compares against.
    listed: BoundExpr,
    /// Whether `NOT IN` was written.
    negated: bool,
    /// The affinity `IN` applies to both sides.
    affinity: Option<Affinity>,
    /// The collation `IN` compares with.
    collation: Collation,
}

/// Builds the `CASE` that answers what `IN` answers.
///
/// ```text
/// CASE WHEN <a row equals the operand>   THEN 1
///      WHEN <the operand is NULL>        THEN CASE WHEN <the list has a row> THEN NULL ELSE 0 END
///      WHEN <the list holds a NULL>      THEN NULL
///      ELSE 0 END
/// ```
///
/// The first arm cannot fire when the operand is NULL, because `y = NULL` is
/// NULL rather than true, so the second arm is reached exactly when the operand
/// is NULL. `NOT IN` is the same shape with the 1 and the 0 exchanged; NULL
/// stays NULL, which is what makes `NOT IN` over a list holding a NULL answer
/// nothing.
///
/// @param about - what the `IN` was written as
/// @param block - the subquery's own select
/// @param next - the next free subquery number, advanced by three
fn lowered(about: &Lowering, block: &BoundSelect, next: &mut usize) -> BoundExpr {
    let matched = exists(
        block,
        Some(BoundExpr::Compare {
            op: BinaryOp::Equal,
            left: Box::new(about.listed.clone()),
            right: Box::new(about.operand.clone()),
            affinity: about.affinity.clone(),
            collation: about.collation.clone(),
        }),
        next,
    );
    let any_row = exists(block, None, next);
    let any_null = exists(
        block,
        Some(BoundExpr::IsNull {
            negated: false,
            operand: Box::new(about.listed.clone()),
        }),
        next,
    );
    let (found, missing) = match about.negated {
        true => (BoundExpr::Integer(0), BoundExpr::Integer(1)),
        false => (BoundExpr::Integer(1), BoundExpr::Integer(0)),
    };
    BoundExpr::Case {
        operand: None,
        branches: vec![
            (matched, found),
            (
                BoundExpr::IsNull {
                    negated: false,
                    operand: Box::new(about.operand.clone()),
                },
                BoundExpr::Case {
                    operand: None,
                    branches: vec![(any_row, BoundExpr::Null)],
                    otherwise: Some(Box::new(missing.clone())),
                    comparisons: Vec::new(),
                },
            ),
            (any_null, BoundExpr::Null),
        ],
        otherwise: Some(Box::new(missing)),
        comparisons: Vec::new(),
    }
}

/// Returns an `EXISTS` over a copy of the block, with one more `WHERE` term.
///
/// @param block - the subquery's own select
/// @param extra - the term to add to its `WHERE`, when there is one
/// @param next - the next free subquery number, advanced by one
fn exists(block: &BoundSelect, extra: Option<BoundExpr>, next: &mut usize) -> BoundExpr {
    let mut copy = block.clone();
    // The block's own result columns are not read by `EXISTS`, and one of them
    // may be the column the equality now tests - so they are replaced by a
    // constant rather than kept.
    copy.columns.truncate(1);
    if let Some(first) = copy.columns.first_mut() {
        first.expr = BoundExpr::Integer(1);
        first.origin = None;
    }
    copy.order_by.clear();
    if let Some(extra) = extra {
        copy.filter = Some(match copy.filter.take() {
            Some(held) => BoundExpr::And(Box::new(held), Box::new(extra)),
            None => extra,
        });
    }
    let id = *next;
    *next = next.saturating_add(1);
    BoundExpr::Subquery {
        id,
        kind: SubqueryKind::Exists,
        negated: false,
        operand: None,
        block: Box::new(copy),
        affinity: None,
        collation: Collation::Binary,
    }
}

/// Returns the largest subquery number a statement uses.
///
/// The rewrite adds blocks, and every block's number has to be one nothing else
/// in the statement holds: the compiler builds a block once per number and
/// would otherwise build one of the new blocks in place of an old one.
///
/// @param statement - the bound statement
fn highest_subquery_id(statement: &mut BoundStatement) -> usize {
    let mut highest = 0usize;
    let mut look = |expr: &mut BoundExpr| {
        if let BoundExpr::Subquery { id, .. } = expr {
            highest = highest.max(*id);
        }
    };
    match statement {
        BoundStatement::Select(select) => crate::rewrite::rewrite_select(select, &mut look),
        BoundStatement::Insert(insert) => crate::rewrite::rewrite_insert(insert, &mut look),
        BoundStatement::Update(update) => crate::rewrite::rewrite_update(update, &mut look),
        BoundStatement::Delete(delete) => crate::rewrite::rewrite_delete(delete, &mut look),
        BoundStatement::Directive(_) | BoundStatement::Empty => {}
    }
    highest
}
