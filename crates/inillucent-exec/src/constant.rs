//! Values that are known before a scan starts.
//!
//! Invariant: **nothing here reads a row.** A seek key, a range bound, a
//! `LIMIT` and the probe vector of a vector index are all decided before the
//! first row is fetched, and an expression that reaches a column is refused
//! here rather than evaluated against whatever row happened to be current.
//!
//! It lives beside `physical` rather than inside it because `physical.rs` is
//! one of the modules `crates/inillucent-compat/tests/policy.rs` holds a line
//! ceiling over, and that test names this group as the extraction to make: the
//! four functions below are one idea - *what is constant, and what is it worth*
//! - and the rest of the physical pass is about turning a plan into operators.
//! `physical` re-exports `literal_value`, which a dozen call sites in
//! `inillucent-engine` name by path.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_sql::ast::UnaryOp;
use inillucent_sql::bind::BoundExpr;
use inillucent_tree::datum::OwnedDatum;
use inillucent_value::affinity::Affinity;

use crate::batch::Batch;
use crate::expr::{compile, ArithOp, Expr};
use crate::physical::{translate_scan, Params, Space, TreeCatalog};

/// Returns the value an expression that reads no column folds to.
///
/// For a caller outside a pipeline - a `VALUES` row handed to a virtual table's
/// module, which has no scan behind it and no columns to read.
///
/// @param expr - the bound expression
/// @param params - the values bound to `?1`, `?2`, ...
pub fn literal_value(expr: &BoundExpr, params: &Params) -> DbResult<OwnedDatum> {
    literal_value_in(expr, params, None)
}

/// Returns the value an expression that reads no column folds to, with a
/// catalog to resolve a registered function through.
///
/// The catalog is the whole difference, and it is the difference between a
/// semantic search working and refusing. `ORDER BY vector_distance_cos(v,
/// embed('a question')) LIMIT k` over a column with an `inillucent_hnsw` index
/// is planned as a probe of that index, and the probe vector is this fold. With
/// no catalog the fold cannot look `embed`'s body up and the whole statement is
/// refused as `unsupported` - so **creating the index broke the query the index
/// exists for**, while the same query over an unindexed column went on working
/// because it never reaches here. Every call site that plans a probe has a
/// catalog; it simply was not passed.
///
/// @param expr - the bound expression
/// @param params - the values bound to `?1`, `?2`, ...
/// @param catalog - where a registered function's body is looked up
pub fn literal_value_in(
    expr: &BoundExpr,
    params: &Params,
    catalog: Option<&dyn TreeCatalog>,
) -> DbResult<OwnedDatum> {
    let empty = Space {
        stages: &[],
        layouts: &[],
        types: &[],
        order: &[],
        catalog,
        correlations: &[],
    };
    constant_value(expr, &empty, params, None)
}

/// Returns the value a constant expression folds to.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param affinity - the affinity a comparison would apply
pub(crate) fn constant_value(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    affinity: Option<Affinity>,
) -> DbResult<OwnedDatum> {
    // **A bare `?N` is answered without building an expression for it.** The
    // seek key of `WHERE id = ?1` is the commonest bound there is, and once
    // `translate` stopped folding a parameter it cost an `Expr` node, an `Arc`
    // clone and a lock on the bindings to arrive at a value the parameter set
    // could hand over directly. Measured on the gate: `point.miss` 199 ns to
    // 245, `point.rowid` and `point.index` about five per cent each. Anything
    // more than a bare parameter - `?1 + 200` is the gate's own range bound -
    // still goes the general way below.
    let value = match expr {
        BoundExpr::Parameter(index) => params.get(*index),
        other => {
            let translated = translate_scan(other, space, params)?;
            match fold(&translated) {
                Some(value) => value,
                // A registered function over constant arguments is a constant,
                // and `fold` cannot answer one because the answer is in a body
                // only the catalog holds. `embed('search_query: ' || ?1)` is
                // that case and it is the probe vector of every semantic
                // search, so it is evaluated here - once, through the same
                // evaluator a row would use, rather than a second constant
                // evaluator that would agree with the first until somebody
                // fixed a rounding rule in one of them.
                //
                // **Only this case.** Everything else `fold` declines is
                // declined here too. A missing column reads back as NULL from a
                // batch with no columns rather than failing, so evaluating
                // whatever `fold` could not would turn "this reads a column"
                // from a refusal into a silently wrong seek key.
                None if constant_call(&translated) => evaluated_constant(&translated)?,
                None => {
                    return Err(misuse(
                        "a seek key or range bound reads a column, which it may not",
                    ))
                }
            }
        }
    };
    // A seek key is one side of a comparison and takes the comparison's
    // affinity like any other. `WHERE id = '4'` against an `INTEGER PRIMARY
    // KEY` finds row 4 in SQLite, because the text is converted before the
    // rowid is compared - and a probe that descended for the *text* `'4'`
    // found nothing at all. The predicate path already applied this; the seek
    // path did not, and the two disagreeing is worse than either being wrong.
    let Some(affinity) = affinity else {
        return Ok(value);
    };
    let borrowed = value.borrow();
    let converted = inillucent_value::affinity::apply_affinity(
        crate::scalar::to_value(borrowed),
        affinity,
        inillucent_value::encoding::TextEncoding::Utf8,
    )
    .unwrap_or(inillucent_value::value::Value::Null);
    Ok(crate::scalar::from_value(converted))
}

/// Reports whether an expression is a registered function over constants.
///
/// @param expr - the translated expression
fn constant_call(expr: &Expr) -> bool {
    match expr {
        Expr::External { arguments, .. } => arguments.iter().all(|each| fold(each).is_some()),
        _ => false,
    }
}

/// Returns the value a translated expression evaluates to, reading no row.
///
/// The batch is one row wide and holds no columns. **A column reference would
/// not fail against it - it would read back NULL**, which is why the only
/// expression this is ever handed is one the caller has already checked reads
/// no column at all - [`constant_call`] for a seek key or a range bound,
/// [`translate`](crate::physical) for a deterministic registered function's
/// call over constant arguments (`docs/roadmap.md` item 15).
///
/// @param expr - the translated expression
pub(crate) fn evaluated_constant(expr: &Expr) -> DbResult<OwnedDatum> {
    let evaluator = compile(expr, &[])?;
    let batch = Batch::new(1, Vec::new());
    Ok(evaluator.value(&batch, 0)?.into_owned())
}

/// Folds a constant expression to a value, or returns `None` if it reads a
/// column.
///
/// **A parameter is a constant here and nowhere else.** `translate` stopped
/// folding `?N` into a literal so that a chain could outlive the values it was
/// built for, and this reads the binding instead - which is correct because the
/// only caller is [`constant_value`], and every one of *its* callers is inside
/// `source_for` or `rowid_seek_key`. Those run **per execution**, after the
/// window `Statement::rebindable` measures, so a value read here is never kept:
/// a seek key and a range bound are rebuilt every time the statement runs, which
/// is the whole reason the source is the part a reused chain does rebuild.
///
/// @param expr - the translated expression
fn fold(expr: &Expr) -> Option<OwnedDatum> {
    match expr {
        Expr::Literal(value) => Some(value.clone()),
        Expr::Parameter { index, bound } => Some(
            bound
                .lock()
                .ok()?
                .get(index.saturating_sub(1) as usize)
                .cloned()
                .unwrap_or(OwnedDatum::Null),
        ),
        Expr::Arith(op, left, right) => {
            let left = fold(left)?;
            let right = fold(right)?;
            let (a, b) = (left.borrow(), right.borrow());
            match (a.as_int(), b.as_int()) {
                (Some(a), Some(b)) => Some(OwnedDatum::Int(match op {
                    ArithOp::Add => a.wrapping_add(b),
                    ArithOp::Subtract => a.wrapping_sub(b),
                    ArithOp::Multiply => a.wrapping_mul(b),
                })),
                _ => {
                    let a = a.as_f64()?;
                    let b = b.as_f64()?;
                    Some(OwnedDatum::Real(match op {
                        ArithOp::Add => a + b,
                        ArithOp::Subtract => a - b,
                        ArithOp::Multiply => a * b,
                    }))
                }
            }
        }
        // A unary operator over a constant, which is what a negative literal
        // is: `WHERE id = -3` and `LIMIT -1` both bind as a negation of a
        // literal rather than as a literal, and both were refused as "reads a
        // column" - a message that named the wrong thing entirely.
        //
        // The value comes from `inillucent_scalar::eval`, which is where the
        // executor's own unary operators get theirs. A second implementation
        // here would agree with that one until the first time somebody fixed a
        // rounding rule in one of them.
        Expr::Unary { op, operand } => {
            let value = crate::scalar::to_value(fold(operand)?.borrow());
            let answer = match op {
                UnaryOp::Negate => inillucent_scalar::eval::negate(&value),
                UnaryOp::Identity => value,
                UnaryOp::BitNot => inillucent_scalar::eval::bit_not(&value),
                UnaryOp::Not => inillucent_scalar::eval::logical_not(&value),
            };
            Some(crate::scalar::from_value(answer))
        }
        _ => None,
    }
}
