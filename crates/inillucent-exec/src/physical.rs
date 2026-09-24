//! The physical pass: the existing planner's output becomes a pipeline.
//!
//! Invariant: this pass never changes what a query means, only how it is run.
//! Every construct it does not recognise is **refused** rather than
//! approximated - `unsupported` returns an error naming what was not handled,
//! so a query the new engine cannot run yet fails loudly instead of returning a
//! plausible wrong answer. That is the whole reason it is written as a
//! whitelist: a differential digest comparison catches a wrong answer, but only
//! for a query somebody thought to put in the corpus.
//!
//! ## Where this sits
//!
//! `inillucent-sql`'s lexer, parser, binder and planner survive the rearchitecture
//! unchanged - the TDD's component triage says so, and they are the part of the
//! old engine that was never the problem. What they produce is a
//! [`inillucent_sql::plan::PhysicalPlan`]: FROM terms with access paths,
//! residual predicates, an aggregation mode, and a bound result list. This
//! module turns that into the operator chain in [`crate::ops`],
//! [`crate::paged`] and [`crate::join`].
//!
//! ## Stages, and why a FROM term can be two of them
//!
//! The planner's unit is a FROM term. The executor's unit is a **stage**: one
//! tree, read one way, contributing a run of columns to the joined row. Most
//! terms are one stage, but a non-covering index seek is two - the index scan
//! that finds the rowids, and the table probe that fetches the rest of the row.
//! The TDD calls the second one `RowidLookup` and lists it as an operator; here
//! it is an [`crate::join::IndexNestedLoopJoin`] into the table tree keyed on
//! the index entry's rowid, because that is exactly what it is, and writing it
//! twice would be two chances to get the null handling different.
//!
//! Columns are numbered across the stages in order, so stage `i` owns
//! `offset[i] .. offset[i] + width[i]`, and a bound `Column { source, slot }`
//! resolves to whichever of that term's stages carries the slot - the table
//! stage if there is one, the index stage otherwise.
//!
//! ## How a bound column finds its vector
//!
//! A `BoundExpr::Column` carries both numbers a column has: its *declared*
//! position, which is what the schema, the index keys and every DML path name
//! it by, and its *record slot*, which is where a SQLite record would hold it.
//! The two differ the moment a table declares a `VIRTUAL` generated column,
//! because such a column takes no record field. The new engine's trees have
//! neither: they have mini-columns, and [`SourceLayout`] is the map onto them.
//!
//! **That map is indexed by the declared position**, and every builder of one -
//! `table_shape`, `keyed_table_shape`, `index_shape` - and every other reader of
//! one - the insert, update and delete paths, `CREATE INDEX`, `ALTER TABLE` -
//! already indexed it that way. This pass used to index it by the record slot
//! instead, which agreed with all of them exactly as long as no table had a
//! `VIRTUAL` column and returned the previous column's value for every column
//! after one as soon as a table did. `None` still means the tree does not carry
//! the column, which is how a covering index says so and how a `VIRTUAL` column
//! says it is computed rather than stored.

use inillucent_base::DbResult;
// `literal_value` is named by path from a dozen call sites in
// `inillucent-engine`, so it stays reachable here after the move to `constant`.
use crate::constant::constant_value;
pub use crate::constant::{literal_value, literal_value_in};
// `Compiled`, `Slot` and `try_compile` moved to `crate::compiled` to keep this
// file under its recorded ceiling; re-exported here so every existing
// `physical::Slot` / `physical::Compiled` / `physical::try_compile` reference
// - `inillucent-engine`'s `Cached::Select` among them - did not have to move
// with them.
pub use crate::compiled::{try_compile, Compiled, Slot};
use inillucent_sql::bind::{BoundExpr, BoundSelect};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::plan::{AccessPath, BoundKind, IndexSeekBranch, RangeBound};
use inillucent_tree::datum::OwnedDatum;
use inillucent_tree::PagedTree;
use inillucent_value::affinity::Affinity;

use crate::expr::Expr;
use crate::paged::SpanScan;
use crate::scan::Projection;

/// A catalog that also answers one recursive CTE's queue.
///
/// Everything else is delegated, so the step arm sees exactly the trees, the
/// layouts and the modules the statement sees. Wrapping rather than threading a
/// parameter through every builder is what keeps a recursive query from
/// changing the shape of a signature nothing else uses.
// **The seven modules this file is made of (task-1962, A7).** It was 5,708
// lines doing five jobs, and `physical/keys.rs` had already been carved out of
// it, so the directory was started and unfinished. Everything is re-exported
// under the paths it had, because a move that changes no behaviour has no
// business rewriting the call sites: `physical::Params`, `physical::prepare`
// and `physical::run` all still resolve.
mod chain;
mod joins;
mod params;
mod run;
mod stages;
mod translate;

mod catalog;

pub use catalog::{ForcePlan, SourceLayout, TreeCatalog};
pub(crate) use chain::Listing;
pub use chain::{build, build_prepared, build_prepared_described, build_statement, Statement};
pub use params::{Params, Slots, ENGINE_PARAMETER_BASE};
pub use run::{
    prepare_any, rowid_seek_key, run, run_any, run_any_prepared, run_any_prepared_limited,
    run_compound, run_prepared, run_prepared_limited,
};
pub use stages::prepare;
pub use stages::{AccessKind, Pipeline, Prepared, PreparedStage, Shape, Source, VirtualScanSource};
// The crate-internal names, re-exported so the seven modules reach each other
// through the paths they used when they were one file.
pub(crate) use catalog::WithQueue;
pub(crate) use chain::{build_upper, push_materialised, HeldSpace, Space};
pub(crate) use chain::{describe_source, source_for_run, source_pool, space_of};
pub(crate) use joins::{
    build_nested, is_scan_prefix, iterative_candidates, materialise_stage, order_equivalents,
    output_is_sorted_by, projected_prefix, reads_a_column, reads_a_parameter, skip_scan_applies,
};
pub(crate) use joins::{has_equi_key, join_kind_of};
pub(crate) use run::distinct_collations;
pub(crate) use run::row_offset;
pub(crate) use stages::{
    expression_collation, inillucent_exec_like_case_sensitive, refuse_unhandled, unsupported,
};
pub(crate) use translate::translate;
pub(crate) use translate::{
    aggregate_output_types, aggregate_specs, constant_count, constant_limit, constant_offset,
    same_expr, translate_post, translate_scan, trim,
};
mod integrity;
mod keys;

pub use integrity::ModuleIntegrity;
use keys::{index_union_keys, point_key, range_union_bounds, rowid_union_keys, span_bounds};
pub(crate) use keys::{nested_key, SpanBounds};

/// The values `?1`, `?2` ... hold for the execution now running.
///
/// **Shared with the compiled expression tree, which is what lets a chain built
/// once answer a different question on the next execution.** `translate` used to
/// fold `?2` into an `Expr::Literal`, so a chain was only ever correct for the
/// values it was built against - which is why [`Statement::rebindable`] existed
/// to refuse a re-run, and why nothing on the execution path could reuse a
/// chain. An `Expr::Parameter` reads this cell when it is evaluated instead.
///
/// **Behind an `Arc<Mutex<_>>` rather than an `Rc<RefCell<_>>`, because `Eval`
/// is `Send + Sync`.** The pipeline is single-threaded today and the trait does
/// not promise it will stay that way, which is the same reason `JsonCall`'s
/// parse cache is a `Mutex`. An uncontended lock is tens of nanoseconds and a
/// parameter is read once per row at worst.
pub type Bindings = std::sync::Arc<std::sync::Mutex<Slots>>;

/// How many seek-key columns a point probe borrows on the stack.
///
/// Four covers every rowid table and every index in the scorecard fixture and
/// in the dialect's own corpus; a wider key spills, which costs what every key
/// used to cost.
const POINT_KEY_INLINE: usize = 4;

/// How much wider each round of an iterative vector scan asks.
///
/// Four rather than two because a round costs a graph walk plus a descent per
/// candidate, and the number of rounds is what the query pays for: a filter
/// keeping one row in a hundred is reached in four rounds rather than seven.
const VECTOR_WIDEN: usize = 4;

/// The most candidates one iterative vector scan will ask an index for.
///
/// A stop that only matters if a store keeps answering with as many rows as it
/// was asked for however deep it is taken - which no finite table does, so
/// exhaustion is what ends the loop in practice. This is the backstop.
const VECTOR_CANDIDATE_CAP: usize = 1 << 24;

/// How many FROM terms an expression is asked about when looking for a column.
///
/// The limit on terms in one statement, which is what bounds the loop above.
const MAX_SOURCES: usize = 64;

/// Translates a bound expression that reads the scan's columns.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the bound parameters
/// How a bound expression's leaves are resolved.
///
/// The same expression means different things above and below an aggregate: a
/// `GROUP BY` key is a scan column on one side of the operator and output column
/// zero on the other. Making that a *parameter* of one traversal rather than two
/// traversals is what keeps the two from drifting - which they had, by twenty-odd
/// node kinds.
#[derive(Clone, Copy)]
pub(crate) enum Frame<'a> {
    /// Reading the scan's own columns.
    Scan,
    /// Reading the row an aggregate emitted: the keys, then the accumulators.
    Post {
        /// The bound statement, for the aggregate and `GROUP BY` lists.
        select: &'a BoundSelect,
        /// How many `GROUP BY` keys precede the accumulators.
        group_width: usize,
    },
    /// Reading the row a window pass emitted: the values it was given, then one
    /// per call in the order they were bound.
    ///
    /// The third frame, and the reason the traversal takes one rather than
    /// being written three times: a window's output space differs from the
    /// scan's in exactly the same way an aggregate's does - only at the leaves.
    Window {
        /// The expressions the buffered row holds, one per column.
        pre: &'a [BoundExpr],
        /// How many of those precede the appended window values.
        width: usize,
    },
}

/// What a negative `LIMIT` or `OFFSET` means.
///
/// **They mean different things and the difference is a wrong answer.** A
/// negative `LIMIT` is SQLite's way of saying "no limit"; a negative `OFFSET` is
/// treated as zero. The first version of this clamped both to zero, which turned
/// `LIMIT -1` into `LIMIT 0` - every row suppressed - and would have done the
/// same to a parameter somebody bound to -1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Negative {
    /// A negative count means there is no limit.
    NoLimit,
    /// A negative count means zero.
    Zero,
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_sql::bind::BoundExpr;
    use inillucent_value::{Affinity, Collation};

    /// A negative `LIMIT` and a negative `OFFSET` mean different things.
    ///
    /// **They mean different things and the difference is a wrong answer
    /// (T3, task-1962).** A negative `LIMIT` is SQLite's way of saying "no
    /// limit"; a negative `OFFSET` is treated as zero. The first version of
    /// this clamped both to zero, which turned `LIMIT -1` into `LIMIT 0` -
    /// every row suppressed.
    #[test]
    fn a_negative_limit_is_no_limit_and_a_negative_offset_is_zero() {
        let params = Params::new();
        let minus_one = BoundExpr::Unary {
            op: inillucent_sql::ast::UnaryOp::Negate,
            operand: Box::new(BoundExpr::Integer(1)),
        };
        assert_eq!(
            translate::constant_count(Some(&minus_one), &params, Negative::NoLimit)
                .expect("a negated literal is a constant"),
            None,
            "`LIMIT -1` means no limit"
        );
        assert_eq!(
            translate::constant_count(Some(&minus_one), &params, Negative::Zero)
                .expect("a negated literal is a constant"),
            Some(0),
            "`OFFSET -1` means no offset"
        );
    }

    /// A non-negative count is the count, and no clause at all is `None`.
    #[test]
    fn a_positive_count_is_itself() {
        let params = Params::new();
        let five = BoundExpr::Integer(5);
        assert_eq!(
            translate::constant_count(Some(&five), &params, Negative::NoLimit)
                .expect("a literal is a constant"),
            Some(5)
        );
        assert_eq!(
            translate::constant_count(None, &params, Negative::Zero)
                .expect("no clause is not an error"),
            None,
            "a statement with no LIMIT is not a statement with LIMIT 0"
        );
    }

    /// A `LIMIT` that is not a constant is refused rather than guessed at.
    #[test]
    fn a_limit_that_is_not_a_constant_is_refused() {
        let params = Params::new();
        let column = BoundExpr::Column {
            source: 0,
            column: 0,
            slot: 0,
            affinity: Affinity::Blob,
            collation: Collation::Binary,
        };
        let refused = translate::constant_count(Some(&column), &params, Negative::NoLimit);
        assert!(
            refused.is_err(),
            "a column is not a constant, so there is no count to return"
        );
    }
}
