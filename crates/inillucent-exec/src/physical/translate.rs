//! Turning a bound expression into the closure the executor evaluates.
//!
//! Invariant: **a expression is compiled once, into a closure that borrows
//! nothing of the plan.** The executor evaluates the closure per row, so
//! anything that can be decided while compiling - a constant fold, a collation,
//! a static type - is decided here and never again.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
// `literal_value` is named by path from a dozen call sites in
// `inillucent-engine`, so it stays reachable here after the move to `constant`.
// `Compiled`, `Slot` and `try_compile` moved to `crate::compiled` to keep this
// file under its recorded ceiling; re-exported here so every existing
// `physical::Slot` / `physical::Compiled` / `physical::try_compile` reference
// - `inillucent-engine`'s `Cached::Select` among them - did not have to move
// with them.
use inillucent_sql::ast::{BinaryOp, PatternOp, SortOrder, UnaryOp};
use inillucent_sql::bind::{BoundExpr, BoundSelect, SubqueryKind};
use inillucent_sql::function::{AggregateFunc, ScalarFunc};
use inillucent_tree::datum::OwnedDatum;
use inillucent_value::collation::Collation;

use crate::aggregate::{AggregateKind, Percentile};
use crate::expr::{compile, ArithOp, CompareOp, Expr, StaticType};
use crate::ops::AggregateSpec;

/// A catalog that also answers one recursive CTE's queue.
///
/// Everything else is delegated, so the step arm sees exactly the trees, the
/// layouts and the modules the statement sees. Wrapping rather than threading a
/// parameter through every builder is what keeps a recursive query from
/// changing the shape of a signature nothing else uses.
use super::*;

/// Returns what `rtreecheck` answers for the table it names.
///
/// `ok` when the module found nothing wrong, the module's own report when it
/// did, and a refusal when the name is not a table this can be asked about -
/// which is the reference's behaviour too: `rtreecheck` on a table that is not
/// an R-Tree is an error rather than a cheerful `ok`.
///
/// @param arguments - the schema and table, or just the table
/// @param space - the joined column space, which carries the catalog
fn rtree_check(arguments: &[BoundExpr], space: &Space<'_>) -> DbResult<OwnedDatum> {
    let Some(BoundExpr::Text(name)) = arguments.last() else {
        return unsupported("rtreecheck with a table name that is not a literal");
    };
    let Some(catalog) = space.catalog else {
        return unsupported("rtreecheck from here");
    };
    match catalog.module_integrity(name)? {
        ModuleIntegrity::Clean => Ok(OwnedDatum::Text(b"ok".to_vec())),
        ModuleIntegrity::Report(report) => Ok(OwnedDatum::Text(report.into_bytes())),
        ModuleIntegrity::NoSuchModule => Err(inillucent_base::error::misuse(format!(
            "no such rtree table: {}",
            String::from_utf8_lossy(name)
        ))),
    }
}
pub(crate) fn translate_scan(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Expr> {
    translate(expr, space, params, Frame::Scan)
}
/// Turns one bound expression into the closure the executor evaluates.
///
/// **A dispatcher over six named groups (task-1962, A8).** It was 494 lines of
/// one `match` with about thirty arms and no doc comment - the longest function
/// in the workspace, and the one a contributor reading the executor has to read
/// first. The arms were already grouped by what kind of expression they are
/// about; each group is a function now, and this is the ordered list of them.
///
/// The order matters in one place only: `resolve_in_frame` runs first, because
/// a window or post-aggregation pass answers some expressions out of what an
/// earlier pass computed rather than by translating them again. Every group
/// after it is disjoint, so the rest of the order is the one a reader would
/// want.
///
/// @param expr - the bound expression
/// @param space - the joined column space, which carries the catalog
/// @param params - the statement's bound values
/// @param frame - which pass is translating, which decides how a leaf resolves
pub(crate) fn translate(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Expr> {
    if let Some(found) = resolve_in_frame(expr, frame)? {
        return Ok(found);
    }
    if let Some(found) = translate_literal(expr, params)? {
        return Ok(found);
    }
    if let Some(found) = translate_reference(expr, space, params, frame)? {
        return Ok(found);
    }
    if let Some(found) = translate_logical(expr, space, params, frame)? {
        return Ok(found);
    }
    if let Some(found) = translate_comparison(expr, space, params, frame)? {
        return Ok(found);
    }
    if let Some(found) = translate_pattern(expr, space, params, frame)? {
        return Ok(found);
    }
    match translate_call(expr, space, params, frame)? {
        Some(found) => Ok(found),
        // `translate_call` ends in the refusal, so an expression that reached
        // it either translated or produced an error. This arm is here for the
        // type and is not a case.
        None => unsupported(&format!("the expression {}", name_of(expr))),
    }
}

/// Answers an expression an earlier pass already computed, if this is one.
///
/// **The two early returns the frame decides (task-1962, A8).** A window pass
/// and a post-aggregation pass each run over the rows a previous pass produced,
/// so some expressions are answered by naming the column that holds the result
/// rather than by translating them again. Every node below this point recurses
/// with the same frame, which is what makes `translate` one traversal rather
/// than two that have to be kept in step - and keeping them in step is exactly
/// what failed before: the post-aggregation copy handled six node kinds and
/// refused the rest, so `length(group_concat(x))` was "a function call outside
/// an aggregate" and `HAVING` had nowhere to be translated at all.
///
/// @param expr - the bound expression
/// @param frame - which pass is translating
fn resolve_in_frame(expr: &BoundExpr, frame: Frame<'_>) -> DbResult<Option<Expr>> {
    if let Frame::Window { pre, width } = frame {
        if let BoundExpr::WindowRef { slot } = expr {
            return Ok(Some(Expr::Column(width.saturating_add(*slot))));
        }
        // A whole sub-expression the pass already computed, which is how a
        // window's own argument resolves without being recomputed.
        if let Some(position) = pre.iter().position(|held| held == expr) {
            return Ok(Some(Expr::Column(position)));
        }
        if matches!(expr, BoundExpr::Column { .. } | BoundExpr::Rowid { .. }) {
            return unsupported("a column a window pass did not carry");
        }
    }
    if let Frame::Post {
        select,
        group_width,
    } = frame
    {
        if let BoundExpr::Aggregate { slot } = expr {
            return Ok(Some(Expr::Column(group_width.saturating_add(*slot))));
        }
        if let Some(position) = select.group_by.iter().position(|key| key == expr) {
            return Ok(Some(Expr::Column(position)));
        }
        // **A bare column is one SQLite answers, by a rule rather than by
        // luck.** `SELECT id, max(a) FROM t` gives the `id` of the row that
        // produced the maximum; with no single `min` or `max` in the query it
        // gives an arbitrary row's, which SQLite takes as the last. Refusing
        // was the honest thing to do while nothing implemented the rule, and it
        // refused a query every "the row with the highest score" report is
        // written as. Each bare column gets an accumulator of its own, after
        // the aggregates - see `bare_columns` and `aggregate_specs`.
        if matches!(expr, BoundExpr::Column { .. } | BoundExpr::Rowid { .. }) {
            let bare = bare_columns(select);
            let Some(at) = bare.iter().position(|held| held == expr) else {
                return unsupported(&format!(
                    "the expression {} outside an aggregate",
                    name_of(expr)
                ));
            };
            return Ok(Some(Expr::Column(
                group_width
                    .saturating_add(select.aggregates.len())
                    .saturating_add(at),
            )));
        }
    }
    Ok(None)
}

/// Translates a literal, a bound parameter and `RAISE`.
///
/// The expressions that read nothing: no column, no row, no catalog. They are
/// together because that is what a reader looking for one of them is looking
/// for, and because every one of them is a constant of the statement.
///
/// @param expr - the bound expression
/// @param params - the statement's bound values, for a parameter's own type
fn translate_literal(expr: &BoundExpr, params: &Params) -> DbResult<Option<Expr>> {
    let found = match expr {
        BoundExpr::Null => Expr::Literal(OwnedDatum::Null),
        BoundExpr::Integer(number) => Expr::Literal(OwnedDatum::Int(*number)),
        BoundExpr::Real(number) => Expr::Literal(OwnedDatum::Real(*number)),
        BoundExpr::Text(bytes) => Expr::Literal(OwnedDatum::Text(bytes.clone())),
        BoundExpr::Blob(bytes) => Expr::Literal(OwnedDatum::Blob(bytes.clone())),
        // **Read when it is evaluated, not folded in here.** Answering this
        // with `Expr::Literal(params.get(*index))` made a chain correct only for
        // the values it was built against, which is why `Statement::rebindable`
        // had to refuse a re-run and why nothing on the execution path could
        // keep a chain. See `Expr::Parameter`.
        BoundExpr::Parameter(index) => Expr::Parameter {
            index: *index,
            bound: params.bindings(),
        },
        // `RAISE(...)` is a value in the grammar and a failure in practice,
        // which is why it is compiled rather than refused: the whole body of
        // every foreign-key check trigger the binder synthesises is one
        // `SELECT RAISE(ABORT, '...') WHERE <the key is missing>`, and the
        // error is what enforcement *is*. `IGNORE` abandons the row instead of
        // failing, and the firing point is what catches it.
        BoundExpr::Raise {
            action,
            message,
            foreign_key,
        } => Expr::Raise {
            code: match (action, foreign_key) {
                (inillucent_sql::ast::RaiseAction::Ignore, _) => 0,
                (_, true) => inillucent_sql::dml::codes::FOREIGN_KEY,
                (_, false) => inillucent_sql::dml::codes::TRIGGER,
            },
            message: match action {
                inillucent_sql::ast::RaiseAction::Ignore => {
                    crate::expr::RAISE_IGNORE.as_bytes().to_vec()
                }
                _ => message.clone().unwrap_or_default(),
            },
            // The one thing the three failing actions differ in. `IGNORE` never
            // reaches an unwind - the firing point catches it and skips the row
            // - so its value here is never read.
            unwind: match action {
                inillucent_sql::ast::RaiseAction::Rollback => {
                    inillucent_base::error::Unwind::Transaction
                }
                inillucent_sql::ast::RaiseAction::Fail => inillucent_base::error::Unwind::Nothing,
                _ => inillucent_base::error::Unwind::Statement,
            },
        },
        // A call to a scalar an application registered. The body is resolved
        // here, once, and carried by the compiled node - see `user_scalar`.
        _ => return Ok(None),
    };
    Ok(Some(found))
}

/// Translates the expressions that name something the row or the catalog has.
///
/// A column, a rowid, a call to a registered scalar, a virtual table's own
/// function, and the sorter's column. Each resolves against the space the
/// statement was planned over, which is what makes them different from a
/// literal.
///
/// @param expr - the bound expression
/// @param space - the joined column space, which carries the catalog
/// @param params - the statement's bound values
/// @param frame - which pass is translating, for a nested call
fn translate_reference(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Option<Expr>> {
    let found = match expr {
        BoundExpr::External { name, arguments } => {
            let Some(catalog) = space.catalog else {
                return unsupported(&format!(
                    "a call to the registered function {} from here",
                    String::from_utf8_lossy(name)
                ));
            };
            let Some(body) = catalog.user_scalar(name, arguments.len()) else {
                return unsupported(&format!(
                    "a call to the registered function {} from here",
                    String::from_utf8_lossy(name)
                ));
            };
            let translated_arguments = arguments
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?;
            // **A deterministic call over arguments that read no column
            // answers the same value for every row, so it is worth answering
            // once instead of once per row.** `docs/roadmap.md` item 15:
            // `embed('search_query: ' || ?1)` in an `ORDER BY` used to call
            // the embedding model once per row of the scan - 2,661 calls to
            // embed the same sentence, 64 of a 65-second query, because
            // nothing here distinguished it from `embed(body)`, which does
            // read a column and has to run per row. `user_scalar_is_deterministic`
            // is what tells the two apart, and `reads_a_column` - already used
            // for a table-valued function's constant argument - is the same
            // question asked of this call's arguments.
            if catalog.user_scalar_is_deterministic(name, arguments.len())
                && arguments.iter().all(|argument| !reads_a_column(argument))
            {
                // A call whose arguments are every one of them literal folds
                // to the same value regardless of which execution asked, and
                // is safe to keep in a chain forever. A call that reads a
                // bound parameter is a constant only for the execution now
                // building this chain - see `reads_a_parameter`.
                if arguments.iter().any(reads_a_parameter) {
                    params.note_execution_constant();
                }
                let folded = Expr::External {
                    body,
                    arguments: translated_arguments,
                };
                return Ok(Some(Expr::Literal(crate::constant::evaluated_constant(
                    &folded,
                )?)));
            }
            Expr::External {
                body,
                arguments: translated_arguments,
            }
        }
        BoundExpr::Column { source, column, .. } => {
            let index = space.column(*source, *column as usize).ok_or_else(|| {
                misuse(format!(
                    "the tree read for FROM term {source} does not carry column {column}"
                ))
            })?;
            Expr::Column(index)
        }
        BoundExpr::Rowid { source } => Expr::Column(
            space
                .rowid(*source)
                .ok_or_else(|| misuse("the tree read does not carry a rowid"))?,
        ),
        // `score(t)`, `bm25(t)`: the module answered it per row when the rows
        // were materialised, so by the time an expression is translated it is a
        // column like any other. It cannot be evaluated here - the module is
        // the only thing that knows the answer, and it is not reachable from an
        // expression node.
        BoundExpr::VirtualFunction {
            source,
            name,
            arguments,
        } => Expr::Column(
            space
                .virtual_function(*source, name, arguments)
                .ok_or_else(|| {
                    misuse(format!(
                        "the tree read does not carry {}, which the module answers per row",
                        String::from_utf8_lossy(name)
                    ))
                })?,
        ),
        // "Column n of the row at this point", which is what the binder gives a
        // `VALUES` arm's result columns and an `ORDER BY` written as an
        // ordinal. It is already an index rather than a name, so there is
        // nothing to resolve.
        BoundExpr::SorterColumn { column } => Expr::Column(usize::from(*column)),
        _ => return Ok(None),
    };
    Ok(Some(found))
}

/// Translates `NOT`, `IS NULL`, `AND` and `OR`.
///
/// Three-valued logic, and the four nodes that implement it. The thing worth
/// knowing about all four is the same: a NULL operand is not false, and
/// `FALSE AND NULL` is false where `TRUE AND NULL` is NULL.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the statement's bound values
/// @param frame - which pass is translating
fn translate_logical(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Option<Expr>> {
    let found = match expr {
        BoundExpr::Not(operand) => Expr::Not(Box::new(translate(operand, space, params, frame)?)),
        BoundExpr::IsNull { operand, negated } => {
            let inner = Box::new(translate(operand, space, params, frame)?);
            if *negated {
                Expr::IsNotNull(inner)
            } else {
                Expr::IsNull(inner)
            }
        }
        BoundExpr::And(left, right) => Expr::And(
            Box::new(translate(left, space, params, frame)?),
            Box::new(translate(right, space, params, frame)?),
        ),
        BoundExpr::Or(left, right) => Expr::Or(
            Box::new(translate(left, space, params, frame)?),
            Box::new(translate(right, space, params, frame)?),
        ),
        _ => return Ok(None),
    };
    Ok(Some(found))
}

/// Translates arithmetic, comparison, `CAST`, `BETWEEN`, `IN` and `CASE`.
///
/// Everything that compares or converts two values. The collation and the
/// affinity a comparison runs under are decided here, once, rather than per
/// row - which is what makes `ORDER BY team COLLATE NOCASE` a different
/// compiled node rather than a different runtime branch.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the statement's bound values
/// @param frame - which pass is translating
fn translate_comparison(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Option<Expr>> {
    let found = match expr {
        BoundExpr::Arithmetic { op, left, right } => {
            let left = Box::new(translate(left, space, params, frame)?);
            let right = Box::new(translate(right, space, params, frame)?);
            // `+`, `-` and `*` have a specialised integer node; everything else
            // - divide, modulo, concatenation, the bitwise operators - goes
            // through the shared implementation.
            match arith_op(*op) {
                Ok(op) => Expr::Arith(op, left, right),
                Err(_) => Expr::General {
                    op: *op,
                    left,
                    right,
                    // The connection's own `Limit::Length`, carried the same
                    // way a scalar call's is.
                    length_limit: params.context().length_limit,
                },
            }
        }
        BoundExpr::Compare {
            op,
            left,
            right,
            affinity,
            collation,
        } => {
            // Affinity conversion before comparison and a non-BINARY
            // collation both change the answer, so an executor that ignored
            // them would be quietly wrong rather than incomplete. The plain
            // form is kept for the case where there is nothing to apply,
            // because it is the fast path and most comparisons are it.
            let op = compare_op(*op)?;
            let left = Box::new(translate(left, space, params, frame)?);
            let right = Box::new(translate(right, space, params, frame)?);
            if affinity.is_none() && *collation == inillucent_value::collation::Collation::Binary {
                Expr::Compare(op, left, right)
            } else {
                Expr::CompareWith {
                    op,
                    affinity: *affinity,
                    collation: *collation,
                    left,
                    right,
                }
            }
        }
        BoundExpr::Unary { op, operand } => Expr::Unary {
            op: *op,
            operand: Box::new(translate(operand, space, params, frame)?),
        },
        BoundExpr::Cast { operand, affinity } => Expr::Cast {
            operand: Box::new(translate(operand, space, params, frame)?),
            affinity: *affinity,
        },
        BoundExpr::Collate { operand, .. } => translate(operand, space, params, frame)?,
        BoundExpr::Is {
            negated,
            left,
            right,
            affinity,
            collation,
        } => Expr::Is {
            negated: *negated,
            left: Box::new(translate(left, space, params, frame)?),
            right: Box::new(translate(right, space, params, frame)?),
            affinity: *affinity,
            collation: *collation,
        },
        BoundExpr::Between {
            negated,
            operand,
            low,
            high,
            affinity,
            collation,
        } => Expr::Between {
            negated: *negated,
            operand: Box::new(translate(operand, space, params, frame)?),
            low: Box::new(translate(low, space, params, frame)?),
            high: Box::new(translate(high, space, params, frame)?),
            affinity: *affinity,
            collation: *collation,
        },
        BoundExpr::InList {
            negated,
            operand,
            list,
            affinity,
            collation,
        } => Expr::InList {
            negated: *negated,
            operand: Box::new(translate(operand, space, params, frame)?),
            list: list
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?,
            affinity: *affinity,
            collation: *collation,
        },
        BoundExpr::Case {
            operand,
            branches,
            otherwise,
            collation,
        } => {
            let mut translated = Vec::with_capacity(branches.len());
            for (when, then) in branches {
                translated.push((
                    translate(when, space, params, frame)?,
                    translate(then, space, params, frame)?,
                ));
            }
            Expr::Case {
                operand: match operand {
                    Some(operand) => Some(Box::new(translate(operand, space, params, frame)?)),
                    None => None,
                },
                branches: translated,
                otherwise: match otherwise {
                    Some(otherwise) => Some(Box::new(translate(otherwise, space, params, frame)?)),
                    None => None,
                },
                collation: *collation,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(found))
}

/// Translates `LIKE`, `GLOB`, `REGEXP` and `MATCH`.
///
/// One arm, and its own function because the pattern operators are the one
/// place a comparison's shape is decided by a pragma: `case_sensitive_like`
/// changes which matcher is compiled in, and it is read here rather than
/// consulted per row.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the statement's bound values
/// @param frame - which pass is translating
fn translate_pattern(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Option<Expr>> {
    let found = match expr {
        BoundExpr::Pattern {
            negated,
            op,
            operand,
            pattern,
            escape,
        } => {
            let kind = match op {
                PatternOp::Like => crate::scalar::PatternKind::Like,
                PatternOp::Glob => crate::scalar::PatternKind::Glob,
                // `REGEXP` and `MATCH` are not built in: SQLite leaves them to
                // an application-defined function or a module, and a query that
                // uses one without registering it is an error rather than a
                // false.
                other => return unsupported(&format!("the {other:?} operator")),
            };
            Expr::Pattern {
                negated: *negated,
                kind,
                operand: Box::new(translate(operand, space, params, frame)?),
                pattern: Box::new(translate(pattern, space, params, frame)?),
                escape: match escape {
                    Some(escape) => Some(Box::new(translate(escape, space, params, frame)?)),
                    None => None,
                },
                // The connection's `case_sensitive_like`, asked of the catalog
                // here rather than carried on the parameters: it is a property
                // of the connection the expression is being compiled for, and
                // the pragma empties the statement cache when it changes.
                case_sensitive: space
                    .catalog
                    .is_some_and(inillucent_exec_like_case_sensitive),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(found))
}

/// Translates a function call, and refuses what is left.
///
/// The built-in families - JSON, the scalar functions, the maths ones, the date
/// and time ones - and a scalar subquery. It ends with the refusal, so an
/// expression no group above claimed is named in the error rather than falling
/// through to a wrong answer.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the statement's bound values
/// @param frame - which pass is translating
fn translate_call(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Option<Expr>> {
    let found = match expr {
        BoundExpr::Json { func, arguments } => Expr::Json {
            func: *func,
            arguments: arguments
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?,
        },
        BoundExpr::Function {
            func,
            arguments,
            collation,
        } => {
            // `length` keeps its specialised node: it reads the leaf's bytes in
            // place where the general path copies them into a `Value` first,
            // and `range.lookaside` calls it once per row.
            let translated = arguments
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?;
            // **Folded here, where the trees are.** `sqlite_offset` asks where
            // in the file a row lives, which is a question about a tree rather
            // than about a value; the map from rowid to page is built once for
            // the statement, out of the leaf boundaries.
            if *func == ScalarFunc::Offset {
                return row_offset(arguments, space, params, frame).map(Some);
            }
            // **Folded here, where the catalog is.** See `ScalarFunc::RTreeCheck`.
            if *func == ScalarFunc::RTreeCheck {
                return Ok(Some(Expr::Literal(rtree_check(arguments, space)?)));
            }
            if *func == ScalarFunc::Length && translated.len() == 1 {
                match translated.into_iter().next() {
                    Some(only) => Expr::Length(Box::new(only)),
                    None => return unsupported("length with no argument"),
                }
            } else {
                Expr::Call {
                    func: *func,
                    arguments: translated,
                    collation: *collation,
                    // Every `changes()` in one statement is the same number,
                    // for the same reason every `now` is the same instant.
                    context: params.context(),
                }
            }
        }
        BoundExpr::Math { func, arguments } => Expr::Math {
            func: *func,
            arguments: arguments
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?,
        },
        BoundExpr::Time { func, arguments } => Expr::Time {
            func: *func,
            arguments: arguments
                .iter()
                .map(|expr| translate(expr, space, params, frame))
                .collect::<DbResult<Vec<Expr>>>()?,
            // Every `now` in one statement is the same instant, which is
            // SQLite's rule and the reason this is read once here rather than
            // per row in the node. It is therefore true of *this* execution
            // only, so the chain that holds it may not be kept for the next one
            // - which is what `note_execution_constant` records.
            now: {
                params.note_execution_constant();
                inillucent_scalar::datetime::julian_now()
            },
        },
        BoundExpr::Subquery {
            id,
            kind,
            negated,
            operand,
            affinity,
            collation,
            ..
        } => {
            // **A correlated block is a column, not a constant.** It reads the
            // row being tested, so `crate::correlate` computed it beside the
            // row and put the answer here; `EXISTS` and its negation are
            // already applied, because the operator is the only thing that
            // knows whether the block produced anything.
            if let Some(column) = space.correlated(*id) {
                return Ok(Some(match kind {
                    SubqueryKind::Exists | SubqueryKind::Scalar => Expr::Column(column),
                    SubqueryKind::In => {
                        return unsupported("a correlated IN subquery");
                    }
                }));
            }
            // Folded before the chain was built, by `subquery::fold`. A slot
            // that is empty is a correlated subquery whose column this pass was
            // not given, which is a plan the builder should not have produced.
            let Some(value) = params.subquery(*id) else {
                return unsupported("a correlated subquery used as a value");
            };
            match kind {
                SubqueryKind::Exists => {
                    Expr::Literal(OwnedDatum::Int(i64::from(value.exists() != *negated)))
                }
                SubqueryKind::Scalar => Expr::Literal(value.scalar()),
                // An `IN` over a folded block is an `IN` over a list of
                // literals, which already carries SQLite's three-valued NULL
                // rule and the affinity and collation the binder attached.
                SubqueryKind::In => Expr::InList {
                    negated: *negated,
                    operand: Box::new(match operand {
                        Some(held) => translate(held, space, params, frame)?,
                        None => return unsupported("an IN with no left operand"),
                    }),
                    list: value
                        .column
                        .iter()
                        .cloned()
                        .map(Expr::Literal)
                        .collect::<Vec<Expr>>(),
                    affinity: *affinity,
                    collation: *collation,
                },
            }
        }
        other => return unsupported(&format!("the expression {}", name_of(other))),
    };
    Ok(Some(found))
}

/// Translates a bound expression in the space after aggregation.
///
/// A result column of an aggregating query reads either a `GROUP BY` key or an
/// accumulator, and both are columns of the row the aggregate operator emits:
/// the keys first, then the accumulators.
///
/// It is [`translate`] with a different frame rather than a second traversal,
/// and that is the point: the old copy handled six node kinds and refused
/// everything else, so `SELECT length(group_concat(name)) ... GROUP BY team` was
/// "a function call outside an aggregate" - a refusal about the *shape* of a
/// query the engine can perfectly well answer.
///
/// @param expr - the bound expression
/// @param select - the bound statement, for the aggregate list
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param group_width - how many `GROUP BY` keys precede the accumulators
pub(crate) fn translate_post(
    expr: &BoundExpr,
    select: &BoundSelect,
    space: &Space<'_>,
    params: &Params,
    group_width: usize,
) -> DbResult<Expr> {
    // **A query that groups reads the grouped row, aggregate or not
    // (task-1979, F1).** This asked only whether the statement had an
    // aggregate, so `SELECT g FROM t GROUP BY g` translated its result column
    // against the *input* row while the operator underneath emits the grouped
    // one - keys first - and the column index therefore pointed past the end.
    // The answer was one NULL per group: two NULL rows for `a` and `b`, exit 0,
    // on a statement every ORM emits. It was hidden from the suite because
    // `ordering.rs` groups on an indexed key, where the plan takes a different
    // path and the answer is right.
    if select.aggregates.is_empty() && select.group_by.is_empty() {
        return translate_scan(expr, space, params);
    }
    translate(
        expr,
        space,
        params,
        Frame::Post {
            select,
            group_width,
        },
    )
}
/// Returns the static type of each column an aggregate operator emits.
///
/// @param select - the bound statement
/// @param space - the joined column space
/// @param params - the bound parameters
pub(crate) fn aggregate_output_types(
    select: &BoundSelect,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Vec<StaticType>> {
    let mut types = Vec::with_capacity(
        select
            .group_by
            .len()
            .saturating_add(select.aggregates.len()),
    );
    for key in &select.group_by {
        let translated = translate_scan(key, space, params)?;
        types.push(static_type_of(&translated, space.types));
    }
    for call in &select.aggregates {
        // `count` is always an integer; the rest depend on their input and on
        // whether a sum overflowed, so nothing is claimed about them.
        types.push(match call.func {
            AggregateFunc::Count => StaticType::Int,
            AggregateFunc::Total | AggregateFunc::Avg => StaticType::Real,
            _ => StaticType::Unknown,
        });
    }
    Ok(types)
}
/// Returns the static type an expression produces.
///
/// @param expr - the translated expression
/// @param types - the input columns' types
fn static_type_of(expr: &Expr, types: &[StaticType]) -> StaticType {
    match expr {
        Expr::Column(index) => types.get(*index).copied().unwrap_or(StaticType::Unknown),
        Expr::Literal(OwnedDatum::Int(_)) => StaticType::Int,
        Expr::Literal(OwnedDatum::Real(_)) => StaticType::Real,
        Expr::Literal(OwnedDatum::Text(_)) => StaticType::Text,
        _ => StaticType::Unknown,
    }
}
/// Returns the bare columns an aggregating query reads, in a stable order.
///
/// A **bare column** is a column or rowid reference that appears outside every
/// aggregate and is not a `GROUP BY` key. SQLite answers one; standard SQL
/// refuses it. The order here is the order they are met in - result columns,
/// then `HAVING`, then `ORDER BY` - and it has to be the same order twice,
/// because `translate` looks a column up in this list and `aggregate_specs`
/// builds one accumulator per entry.
///
/// Deriving it rather than storing it on the plan is what keeps the two in
/// step: there is one function, and a caller that forgot to call it gets a
/// refusal rather than a wrong column.
///
/// @param select - the bound statement
fn bare_columns(select: &BoundSelect) -> Vec<BoundExpr> {
    let mut found: Vec<BoundExpr> = Vec::new();
    let visit = |expr: &BoundExpr, found: &mut Vec<BoundExpr>| {
        let mut stack = vec![expr.clone()];
        while let Some(node) = stack.pop() {
            // An aggregate's arguments are read *inside* it, so nothing under
            // one is bare.
            if matches!(
                node,
                BoundExpr::Aggregate { .. } | BoundExpr::WindowRef { .. }
            ) {
                continue;
            }
            if matches!(node, BoundExpr::Column { .. } | BoundExpr::Rowid { .. }) {
                if !select.group_by.contains(&node) && !found.contains(&node) {
                    found.push(node);
                }
                continue;
            }
            for child in node.children() {
                stack.push(child.clone());
            }
        }
    };
    for column in &select.columns {
        visit(&column.expr, &mut found);
    }
    if let Some(having) = &select.having {
        visit(having, &mut found);
    }
    for term in &select.order_by {
        visit(&term.expr, &mut found);
    }
    found
}
/// Returns the witness a bare column follows, when the query has exactly one.
///
/// SQLite's rule: with one `min` or one `max` in the query, a bare column comes
/// from the row that produced it. With none, or with more than one, the row is
/// arbitrary and this answers `None` - which the accumulator reads as "keep the
/// last".
///
/// @param select - the bound statement
fn bare_witness(select: &BoundSelect) -> Option<(BoundExpr, std::cmp::Ordering)> {
    let mut extremes = select.aggregates.iter().filter(|call| {
        matches!(call.func, AggregateFunc::Min | AggregateFunc::Max) && !call.arguments.is_empty()
    });
    let only = extremes.next()?;
    if extremes.next().is_some() {
        return None;
    }
    let wanted = if only.func == AggregateFunc::Min {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Greater
    };
    Some((only.arguments.first()?.clone(), wanted))
}
/// Builds the accumulator specifications for an aggregating query.
///
/// @param select - the bound statement
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param types - the scan's column types
pub(crate) fn aggregate_specs(
    select: &BoundSelect,
    space: &Space<'_>,
    params: &Params,
    types: &[StaticType],
) -> DbResult<Vec<AggregateSpec>> {
    let mut specs = Vec::with_capacity(select.aggregates.len());
    for call in &select.aggregates {
        let kind = match call.func {
            AggregateFunc::Count if call.star => AggregateKind::CountStar,
            AggregateFunc::Count => AggregateKind::Count,
            AggregateFunc::Sum => AggregateKind::Sum,
            AggregateFunc::Total => AggregateKind::Total,
            AggregateFunc::Avg => AggregateKind::Average,
            AggregateFunc::Min => AggregateKind::Minimum,
            AggregateFunc::Max => AggregateKind::Maximum,
            AggregateFunc::GroupConcat => match call.arguments.get(1) {
                None => AggregateKind::GroupConcat(",".to_string()),
                Some(BoundExpr::Text(bytes)) => {
                    AggregateKind::GroupConcat(String::from_utf8_lossy(bytes).into_owned())
                }
                // A separator that is not a literal is a value of each row -
                // see `AggregateKind::GroupConcatComputed` (task-1913).
                Some(_) => AggregateKind::GroupConcatComputed,
            },
            AggregateFunc::JsonGroupArray => AggregateKind::JsonGroupArray(false),
            AggregateFunc::JsonbGroupArray => AggregateKind::JsonGroupArray(true),
            AggregateFunc::JsonGroupObject => AggregateKind::JsonGroupObject(false),
            AggregateFunc::JsonbGroupObject => AggregateKind::JsonGroupObject(true),
            AggregateFunc::Median => AggregateKind::Percentile(Percentile::Median),
            AggregateFunc::GeopolyGroupBbox => AggregateKind::GeopolyBox,
            AggregateFunc::VectorSum => AggregateKind::VectorFold(false),
            AggregateFunc::VectorAvg => AggregateKind::VectorFold(true),
            AggregateFunc::Percentile => AggregateKind::Percentile(Percentile::Hundredths),
            AggregateFunc::PercentileCont => AggregateKind::Percentile(Percentile::Continuous),
            AggregateFunc::PercentileDisc => AggregateKind::Percentile(Percentile::Discrete),
            AggregateFunc::External => {
                let name = call.external.clone().unwrap_or_default();
                let Some(body) = space
                    .catalog
                    .and_then(|catalog| catalog.user_aggregate(&name, call.arguments.len()))
                else {
                    return unsupported(&format!(
                        "a call to the registered aggregate {} from here",
                        String::from_utf8_lossy(&name)
                    ));
                };
                AggregateKind::External(body)
            }
        };
        // Only a registered aggregate reads past the first argument; every
        // built-in reduces one value per row.
        let extra = match &kind {
            // The object form's second argument. It rides in `extra` for the
            // same reason a registered aggregate's do: the accumulator is
            // handed the whole row, and the vectorised single-value path stays
            // exactly as it was for everything that reduces one value.
            // The percentile family's second argument is the fraction, and it
            // reaches the accumulator the same way: the whole row is kept, so
            // any row's copy of the constant will do at `finish`.
            AggregateKind::JsonGroupObject(_)
            | AggregateKind::External(_)
            | AggregateKind::GroupConcatComputed
            | AggregateKind::Percentile(_) => call
                .arguments
                .iter()
                .skip(1)
                .map(|expr| {
                    let translated = translate_scan(expr, space, params)?;
                    compile(&translated, types)
                })
                .collect::<DbResult<Vec<_>>>()?,
            _ => Vec::new(),
        };
        let argument = match (kind == AggregateKind::CountStar, call.arguments.first()) {
            (true, _) | (_, None) => None,
            (false, Some(expr)) => {
                let translated = translate_scan(expr, space, params)?;
                Some(compile(&translated, types)?)
            }
        };
        // `count(DISTINCT x)` compares its values under the collation `x`
        // carries, which is the same rule `SELECT DISTINCT x` follows - and
        // over the corpus's `NOCASE` team column the two have to agree.
        let distinct = if call.distinct {
            Some(
                call.arguments
                    .first()
                    .map(expression_collation)
                    .unwrap_or(Collation::Binary),
            )
        } else {
            None
        };
        let filter = match &call.filter {
            Some(expr) => {
                let translated = translate_scan(expr, space, params)?;
                Some(compile(&translated, types)?)
            }
            None => None,
        };
        let mut order_by = Vec::with_capacity(call.order_by.len());
        for term in &call.order_by {
            let translated = translate_scan(&term.expr, space, params)?;
            order_by.push((
                compile(&translated, types)?,
                term.order == SortOrder::Descending,
            ));
        }
        specs.push(AggregateSpec {
            kind,
            argument,
            extra,
            distinct,
            filter,
            order_by,
        });
    }
    // **The bare columns, after the aggregates and in the same order
    // `translate` looks them up in.** Each keeps one row's value; the witness
    // is what says which row, and it is compiled once per bare column rather
    // than shared, so an accumulator never has to see inside another.
    let witness = bare_witness(select);
    for expr in bare_columns(select) {
        let translated = translate_scan(&expr, space, params)?;
        let mut extra = Vec::new();
        let wanted = match &witness {
            Some((seen, wanted)) => {
                let translated = translate_scan(seen, space, params)?;
                extra.push(compile(&translated, types)?);
                Some(*wanted)
            }
            None => None,
        };
        specs.push(AggregateSpec {
            kind: AggregateKind::Bare(wanted),
            argument: Some(compile(&translated, types)?),
            extra,
            distinct: None,
            filter: None,
            order_by: Vec::new(),
        });
    }
    Ok(specs)
}
/// Returns a projection that keeps the first `width` columns.
///
/// @param width - how many columns the statement's result has
/// @param types - the input columns' types
pub(crate) fn trim(
    width: usize,
    types: &[StaticType],
) -> DbResult<Vec<Box<dyn crate::expr::Eval>>> {
    (0..width)
        .map(|index| compile(&Expr::Column(index), types))
        .collect()
}
/// Returns the statement's `LIMIT`, when it is a constant.
///
/// @param select - the bound statement
/// @param params - the bound parameters
pub(crate) fn constant_limit(select: &BoundSelect, params: &Params) -> DbResult<Option<usize>> {
    constant_count(select.limit.as_ref(), params, Negative::NoLimit)
}
/// Returns the statement's `OFFSET`, when it is a constant.
///
/// @param select - the bound statement
/// @param params - the bound parameters
pub(crate) fn constant_offset(select: &BoundSelect, params: &Params) -> DbResult<Option<usize>> {
    constant_count(select.offset.as_ref(), params, Negative::Zero)
}
/// Returns a `LIMIT`/`OFFSET` expression's value.
///
/// @param expr - the expression, when there is one
/// @param params - the bound parameters
/// @param negative - what a negative value means for this clause
pub(crate) fn constant_count(
    expr: Option<&BoundExpr>,
    params: &Params,
    negative: Negative,
) -> DbResult<Option<usize>> {
    let number = match expr {
        None => return Ok(None),
        Some(BoundExpr::Integer(number)) => *number,
        // A negative literal binds as a negation of a literal rather than as a
        // literal, which is why `LIMIT -1` was refused as "not a constant".
        Some(BoundExpr::Unary {
            op: UnaryOp::Negate,
            operand,
        }) => match operand.as_ref() {
            BoundExpr::Integer(number) => number.saturating_neg(),
            _ => return unsupported("a LIMIT or OFFSET that is not a constant"),
        },
        Some(BoundExpr::Parameter(index)) => match params.get(*index) {
            OwnedDatum::Int(number) => number,
            OwnedDatum::Null => return Ok(None),
            _ => return unsupported("a LIMIT bound to a non-integer"),
        },
        Some(_) => return unsupported("a LIMIT or OFFSET that is not a constant"),
    };
    if number < 0 {
        return Ok(match negative {
            Negative::NoLimit => None,
            Negative::Zero => Some(0),
        });
    }
    Ok(Some(number as usize))
}
/// Reports whether two translated expressions are the same expression.
///
/// @param left - one expression
/// @param right - the other
pub(crate) fn same_expr(left: &Expr, right: &Expr) -> bool {
    match (left, right) {
        (Expr::Column(a), Expr::Column(b)) => a == b,
        (Expr::Literal(a), Expr::Literal(b)) => {
            a.borrow().compare(&b.borrow()) == std::cmp::Ordering::Equal
        }
        (Expr::Arith(a, al, ar), Expr::Arith(b, bl, br)) => {
            a == b && same_expr(al, bl) && same_expr(ar, br)
        }
        (Expr::Compare(a, al, ar), Expr::Compare(b, bl, br)) => {
            a == b && same_expr(al, bl) && same_expr(ar, br)
        }
        (
            Expr::CompareWith {
                op: a,
                affinity: aa,
                collation: ac,
                left: al,
                right: ar,
            },
            Expr::CompareWith {
                op: b,
                affinity: ba,
                collation: bc,
                left: bl,
                right: br,
            },
        ) => a == b && aa == ba && ac == bc && same_expr(al, bl) && same_expr(ar, br),
        _ => false,
    }
}
/// Maps a bound arithmetic operator onto the compiler's.
///
/// @param op - the planner's operator
fn arith_op(op: BinaryOp) -> DbResult<ArithOp> {
    match op {
        BinaryOp::Add => Ok(ArithOp::Add),
        BinaryOp::Subtract => Ok(ArithOp::Subtract),
        BinaryOp::Multiply => Ok(ArithOp::Multiply),
        other => unsupported(&format!("the operator {other:?}")),
    }
}
/// Maps a bound comparison onto the compiler's.
///
/// @param op - the planner's operator
fn compare_op(op: BinaryOp) -> DbResult<CompareOp> {
    match op {
        BinaryOp::Equal => Ok(CompareOp::Equal),
        BinaryOp::NotEqual => Ok(CompareOp::NotEqual),
        BinaryOp::Less => Ok(CompareOp::Less),
        BinaryOp::LessEqual => Ok(CompareOp::LessOrEqual),
        BinaryOp::Greater => Ok(CompareOp::Greater),
        BinaryOp::GreaterEqual => Ok(CompareOp::GreaterOrEqual),
        other => unsupported(&format!("the comparison {other:?}")),
    }
}
/// Returns a bound expression's variant name, for a refusal message.
///
/// @param expr - the expression
fn name_of(expr: &BoundExpr) -> &'static str {
    match expr {
        BoundExpr::Null => "NULL",
        BoundExpr::Integer(_) => "an integer literal",
        BoundExpr::Real(_) => "a real literal",
        BoundExpr::Text(_) => "a text literal",
        BoundExpr::Blob(_) => "a blob literal",
        BoundExpr::Parameter(_) => "a parameter",
        BoundExpr::Column { .. } => "a column",
        BoundExpr::Rowid { .. } => "a rowid",
        BoundExpr::Unary { .. } => "a unary operator",
        BoundExpr::Arithmetic { .. } => "an arithmetic operator",
        BoundExpr::Compare { .. } => "a comparison",
        BoundExpr::And(_, _) => "AND",
        BoundExpr::Or(_, _) => "OR",
        BoundExpr::Not(_) => "NOT",
        BoundExpr::IsNull { .. } => "IS NULL",
        BoundExpr::Aggregate { .. } => "an aggregate",
        BoundExpr::Function { .. } => "a function call",
        BoundExpr::External { .. } => "an application-defined function",
        // Everything else answers with its own variant name rather than with
        // "an expression". A refusal a reader cannot act on is a refusal that
        // costs a debugging session, and the first run of the Phase 2 gate
        // spent one on exactly this line.
        other => {
            let rendered = format!("{other:?}");
            let name = rendered
                .split(|c: char| !c.is_alphanumeric())
                .next()
                .unwrap_or("an expression");
            Box::leak(format!("a {name} expression").into_boxed_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two expressions are the same expression when every part of them is.
    ///
    /// **What decides whether a sort can be skipped (T3, task-1962).** The
    /// chain builder asks whether the `ORDER BY` it was given is the order the
    /// scan already produces; answering yes for two different expressions drops
    /// a sort the answer needed, and answering no for two identical ones pays
    /// for one it did not.
    #[test]
    fn the_same_expression_is_the_same_expression() {
        assert!(same_expr(&Expr::Column(3), &Expr::Column(3)));
        assert!(!same_expr(&Expr::Column(3), &Expr::Column(4)));
        assert!(same_expr(
            &Expr::Literal(OwnedDatum::Int(7)),
            &Expr::Literal(OwnedDatum::Int(7))
        ));
        assert!(!same_expr(
            &Expr::Literal(OwnedDatum::Int(7)),
            &Expr::Literal(OwnedDatum::Int(8))
        ));
        assert!(
            !same_expr(&Expr::Column(0), &Expr::Literal(OwnedDatum::Int(0))),
            "a column and a literal are never the same expression, whatever the column holds"
        );
    }

    /// An arithmetic node is the same only when the operator and both sides
    /// are.
    #[test]
    fn an_operator_is_part_of_the_expression() {
        let add = Expr::Arith(
            ArithOp::Add,
            Box::new(Expr::Column(0)),
            Box::new(Expr::Literal(OwnedDatum::Int(1))),
        );
        let same = Expr::Arith(
            ArithOp::Add,
            Box::new(Expr::Column(0)),
            Box::new(Expr::Literal(OwnedDatum::Int(1))),
        );
        let subtract = Expr::Arith(
            ArithOp::Subtract,
            Box::new(Expr::Column(0)),
            Box::new(Expr::Literal(OwnedDatum::Int(1))),
        );
        assert!(same_expr(&add, &same));
        assert!(!same_expr(&add, &subtract));
    }

    /// A refusal names the construct it will not translate.
    ///
    /// **A refusal a reader cannot act on costs a debugging session**, and the
    /// first run of the Phase 2 gate spent one on exactly this line: every
    /// unhandled node answered "an expression".
    #[test]
    fn a_refusal_names_the_kind_of_expression() {
        assert_eq!(name_of(&BoundExpr::Null), "NULL");
        assert_eq!(name_of(&BoundExpr::Integer(1)), "an integer literal");
        assert_eq!(name_of(&BoundExpr::Parameter(1)), "a parameter");
        assert_eq!(
            name_of(&BoundExpr::Not(Box::new(BoundExpr::Null))),
            "NOT",
            "an operator names itself rather than its category"
        );
    }

    /// The trim list is one column reader per column, in order.
    #[test]
    fn trimming_reads_each_column_once() {
        let types = vec![StaticType::Unknown; 3];
        let readers = trim(2, &types).expect("two of three columns compile");
        assert_eq!(
            readers.len(),
            2,
            "a trim to two columns reads two, which is what drops the \
             columns a join carried and the caller did not select"
        );
        assert!(
            trim(0, &types)
                .expect("trimming to nothing compiles")
                .is_empty(),
            "a statement that selects nothing reads nothing"
        );
    }
}
