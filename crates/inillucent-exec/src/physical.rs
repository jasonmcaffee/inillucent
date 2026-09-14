//! The physical pass: the existing planner's output becomes a pipeline.
//!
//! Invariant: this pass never changes what a query means, only how it is run.
//! Every construct it does not recognise is **refused** rather than
//! approximated - [`unsupported`] returns an error naming what was not handled,
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
//! [`PhysicalPlan`]: FROM terms with access paths, residual predicates, an
//! aggregation mode, and a bound result list. This module turns that into the
//! operator chain in [`crate::ops`], [`crate::paged`] and [`crate::join`].
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

use inillucent_base::error::misuse;
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
use inillucent_pool::Pool;
use inillucent_sql::ast::{BinaryOp, NullOrder, PatternOp, SortOrder, UnaryOp};
use inillucent_sql::bind::{BoundExpr, BoundSelect, SubqueryKind};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::function::{AggregateFunc, ScalarFunc};
use inillucent_sql::plan::{
    AccessPath, AggregationMode, BoundKind, IndexSeekBranch, PhysicalPlan, RangeBound,
};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::PagedTree;
use inillucent_value::affinity::Affinity;
use inillucent_value::collation::Collation;

use crate::aggregate::{AggregateKind, Percentile};
use crate::batch::Batch;
use crate::expr::{compile, ArithOp, CompareOp, Expr, StaticType};
use crate::join::{IndexNestedLoopJoin, JoinKind, NestedLoopJoin, ValuesScan};
use crate::ops::{
    AdjacentDistinct, AggregateSpec, CollectInto, Distinct, Filter, Flow, HashAggregate, Limit,
    Project, SimpleAggregate, Sink, Sort, SortKey, StreamAggregate, TopN,
};
use crate::paged::{FullScan, PointProbe, ReverseScan, SkipScan, SpanScan};
use crate::scan::Projection;
use crate::setop::{SetKeys, SetKind, SetOp};

/// How one imported table's record slots map onto a tree's columns.
#[derive(Clone, Debug)]
pub struct SourceLayout {
    /// The tree holding the rows or entries.
    pub tree_key: u32,
    /// For each record slot, which tree column holds it.
    ///
    /// `None` means the tree does not carry that slot, which is how a covering
    /// index says it does not hold a column.
    pub slots: Vec<Option<usize>>,
    /// Which tree column holds the row's rowid.
    pub rowid: Option<usize>,
    /// The tree columns that identify the *table* row this one belongs to.
    ///
    /// **Not `key_columns`, and the difference is the whole of a `WITHOUT
    /// ROWID` index.** On a table layout this is the rowid's column, or - when
    /// the table has no rowid - the primary key's columns. On an index layout
    /// it is the trailing part of the entry: the rowid an ordinary index
    /// carries, or the primary key a `WITHOUT ROWID` table's index carries
    /// instead. That is what a non-covering seek probes the table with.
    ///
    /// `key_columns` cannot answer this. On an index layout it names the *whole*
    /// entry rather than the part that identifies the table row, and it is
    /// deliberately left empty when the tree is not "already sorted" - a `DESC`
    /// key column, a non-binary collation - which would make index maintenance
    /// silently wrong on exactly the tables that need it.
    ///
    /// Empty for a source that identifies no table row: a view's trigger row, a
    /// derived table, a virtual table.
    pub identity: Vec<usize>,
    /// The static type of each tree column, for the expression compiler.
    pub types: Vec<StaticType>,
    /// How many columns the tree has.
    pub width: usize,
    /// The tree columns the leaves are ordered by, in order.
    ///
    /// A scan of the tree therefore produces rows sorted by these, which is
    /// what lets `GROUP BY`, `DISTINCT` and `ORDER BY` over a prefix of them
    /// run as a streaming pass instead of building a hash table or a sorter.
    /// Getting this wrong would be a wrong answer rather than a slow one, so it
    /// is set by the import - which built the tree - and never inferred.
    pub key_columns: Vec<usize>,
}

/// Where the executor finds its trees, its layouts and its pages.
pub trait TreeCatalog {
    /// Returns the buffer pool one tree's pages live in.
    ///
    /// **Per tree, because a connection is a set of databases.** `ATTACH` gives
    /// a connection a second file with a second pool, and a join across the two
    /// reads both inside one statement - so the question "which pool" has no
    /// answer until a tree is named. There is deliberately no defaulted
    /// `pool()` to fall back on: a call site that could not say which tree it
    /// was about would be right only while there was one file, which is exactly
    /// the assumption this method exists to remove.
    ///
    /// `None` for a root no schema holds, which the caller turns into a refusal
    /// rather than reading somebody else's page two.
    ///
    /// @param root - the handle the plan named
    fn pool_for(&self, root: u32) -> Option<&Pool>;

    /// Returns the tree a plan's root page id refers to.
    ///
    /// The key is the SQLite root page from the fixture the data was imported
    /// from. That sounds like a leftover and is deliberate: the plan comes from
    /// a binder reading that fixture's schema, so the root page is the one
    /// identifier both sides already agree on, and using it means the import
    /// decides the mapping rather than a name lookup guessing at it.
    ///
    /// @param root - the root page id the plan named
    fn tree(&self, root: u32) -> Option<&PagedTree>;

    /// Returns the layout for a plan's root page id.
    ///
    /// @param root - the root page id the plan named
    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>>;

    /// Returns the rows a virtual table produces, when the caller has one.
    ///
    /// **The module runs on the caller's side of this trait, and only its rows
    /// come back.** The executor never learns what a module is: it does not know
    /// about `best_index`, cursors, shadow tables or a module registry, and it
    /// could not - `inillucent-exec` sits below the crate that registers
    /// modules, deliberately, because a pipeline is built against what the
    /// caller supplies rather than against names it resolves itself.
    ///
    /// It is also the shape the TDD's **batch-aware vtab contract** asks for. A
    /// row-at-a-time cursor pulled through the operator chain would put a
    /// virtual call between every row and every batch; producing a batch at a
    /// time keeps the module's own loop inside the module, where it can produce
    /// a run at a time.
    ///
    /// **And it pushes rather than materialising, which is Phase 3's Part C.**
    /// This used to be `virtual_rows`, returning `Option<Vec<Vec<OwnedDatum>>>`,
    /// whose doc comment argued that a materialised scan is safe "for the shapes
    /// a module answers - a MATCH, a bounding box - a result set that fits in
    /// memory by construction". `generate_series` ships in the same registry and
    /// is a counter-example: with no `stop` constraint it is 4,294,967,295 rows,
    /// so `SELECT value FROM gs LIMIT 3` neither returned nor could be stopped -
    /// three shapes measured past a 25-second timeout, one of them
    /// holding about 1.2 cores for ten minutes. A `LIMIT` above the scan cannot
    /// stop a scan that has already run to completion before the operator above
    /// it sees a row.
    ///
    /// So the rows go *down* the chain in batches and the answer that comes back
    /// is [`Flow`]: `Flow::Stop` means the pipeline has what it needs, and the
    /// module's loop abandons the cursor - the same way `scan.rs` abandons a
    /// b-tree scan.
    ///
    /// `Ok(false)` means the caller has no virtual tables at all, which is what
    /// makes this a defaulted method rather than one every catalog has to write.
    ///
    /// @param table - the FROM term's table, which names the module's instance
    /// @param path - the access path the planner chose for it
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param needed - which of the term's columns the query reads
    /// @param downstream - where the batches go
    fn virtual_cursor(
        &self,
        table: &TableInfo,
        path: &AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
        downstream: &mut dyn crate::ops::Sink,
    ) -> DbResult<bool> {
        let _ = (table, path, params, needed, downstream);
        Ok(false)
    }

    /// Returns every row of a virtual scan, materialised.
    ///
    /// For the one caller that genuinely needs the whole answer at once: a
    /// virtual table standing as a *materialised stage* of a join, which is read
    /// many times and so cannot be a cursor that is consumed once. It is written
    /// in terms of [`TreeCatalog::virtual_cursor`] rather than beside it, so
    /// there is one implementation of what a module's scan means.
    ///
    /// @param table - the FROM term's table, which names the module's instance
    /// @param path - the access path the planner chose for it
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param needed - which of the term's columns the query reads
    /// Returns every row of a virtual scan whose arguments came from outside.
    ///
    /// **What a lateral join needs.** An ordinary virtual scan folds its
    /// arguments from the statement - a literal, a parameter - and can do that
    /// once. A table-valued function whose argument reads an outer column has a
    /// different argument per outer row, and the value can only be known where
    /// that row is: in the operator above. So it is evaluated there and handed
    /// down here, one call per outer row.
    ///
    /// `supplied` is in the same order the module's `filter` will see, which is
    /// the order `best_index` asked for.
    ///
    /// @param table - the FROM term's table, which names the module's instance
    /// @param path - the access path the planner chose for it
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param needed - which of the term's columns the query reads
    /// @param supplied - the argument values, already evaluated
    fn virtual_rows_supplied(
        &self,
        table: &TableInfo,
        path: &AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
        supplied: &[OwnedDatum],
    ) -> DbResult<Option<Vec<Vec<OwnedDatum>>>> {
        let _ = supplied;
        self.virtual_rows(table, path, params, needed)
    }

    /// Returns what a module says about its own storage, or nothing.
    ///
    /// **The route `rtreecheck` takes.** A module's `integrity` is reachable
    /// from the connection and from nowhere else, and the question is about a
    /// named table rather than about a row - so it is asked once while the
    /// statement is being prepared, where the catalog is in hand, and the
    /// answer is folded into the expression as a constant.
    ///
    /// `None` means there is no such table or its module does not check
    /// itself; `Some(None)` means it checked and found nothing wrong.
    ///
    /// @param name - the table's name, as written
    fn module_integrity(&self, name: &[u8]) -> DbResult<Option<Option<String>>> {
        let _ = name;
        Ok(None)
    }

    /// Returns every row of a virtual scan, materialised.
    ///
    /// For the one caller that genuinely needs the whole answer at once: a
    /// virtual table standing as a *materialised stage* of a join, which is read
    /// many times and so cannot be a cursor that is consumed once. It is written
    /// in terms of [`TreeCatalog::virtual_cursor`] rather than beside it, so
    /// there is one implementation of what a module's scan means.
    ///
    /// @param table - the FROM term's table, which names the module's instance
    /// @param path - the access path the planner chose for it
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param needed - which of the term's columns the query reads
    fn virtual_rows(
        &self,
        table: &TableInfo,
        path: &AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
    ) -> DbResult<Option<Vec<Vec<OwnedDatum>>>> {
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut sink = crate::ops::CollectInto::new(std::rc::Rc::clone(&collected));
        if !self.virtual_cursor(table, path, params, needed, &mut sink)? {
            return Ok(None);
        }
        let rows = collected.borrow().clone();
        Ok(Some(rows))
    }

    /// Returns the index trees that might cover a query over one table.
    ///
    /// Smallest tree first, so the physical pass takes the cheapest structure
    /// that carries every column the query reads. This is the TDD's "covering
    /// when the projection is inside the index key" rule, and it is what makes
    /// the comparison against SQLite like for like: SQLite answers
    /// `count(*), sum(key), max(category) FROM main_table` from
    /// `main_category(category, key)` rather than from the table, and an engine
    /// measured on a 14 MB table scan against a 1.4 MB index scan is being
    /// measured on a different amount of work.
    ///
    /// @param table_root - the table's root page id
    fn covering_candidates(&self, table_root: u32) -> Vec<u32> {
        let _ = table_root;
        Vec::new()
    }

    /// Returns the body of a scalar an application registered, when there is
    /// one for this name and this many arguments.
    ///
    /// **The body, not a name to look up later.** A compiled chain that
    /// resolved per row would answer a registration made after it was compiled;
    /// registering or removing a function throws the compiled statements away,
    /// which is what makes resolving once correct.
    ///
    /// @param name - the folded name the call used
    /// @param argc - how many arguments the call passed
    fn user_scalar(&self, name: &[u8], argc: usize) -> Option<crate::expr::ScalarBody> {
        let _ = (name, argc);
        None
    }

    /// Reports whether a registered scalar promises `FunctionFlags::deterministic`
    /// - the same answer for the same arguments within one statement.
    ///
    /// **This is what tells a call worth folding apart from one that has to run
    /// per row.** `embed(TEXT)` is deterministic and `ORDER BY
    /// vector_distance_cos(v, embed('search_query: ' || ?1))` calls it with the
    /// same argument for every row of the scan - roadmap item 15 measured 2,661
    /// calls to embed the same sentence, 64 of 65 seconds, before anything read
    /// this flag. A function this answers `false` for - the default, and every
    /// registration until it opts in - is left alone and evaluated per row,
    /// which is the only correct answer for one that is not promised to repeat.
    ///
    /// @param name - the folded name the call used
    /// @param argc - how many arguments the call passed
    fn user_scalar_is_deterministic(&self, name: &[u8], argc: usize) -> bool {
        let _ = (name, argc);
        false
    }

    /// Returns the body of an aggregate an application registered.
    ///
    /// @param name - the folded name the call used
    /// @param argc - how many arguments the call passed
    fn user_aggregate(&self, name: &[u8], argc: usize) -> Option<crate::expr::AggregateBody> {
        let _ = (name, argc);
        None
    }

    /// Returns the rows a recursive CTE's queue is currently holding.
    ///
    /// **The one piece of state a recursive query has, handed in the same way a
    /// module's rows are.** A `WITH RECURSIVE` term reads *itself*: the step arm
    /// runs once per pass over the rows the previous pass produced, and that
    /// working set is neither a tree nor a plan - it is a buffer the fill loop
    /// owns. Asking for it through this trait is what lets the step arm be an
    /// ordinary plan run by the ordinary pipeline, with no second execution
    /// path and no recursion in the operator chain.
    ///
    /// `None` for every catalog that is not inside such a loop, which is every
    /// one of them except the wrapper `run_recursive` builds per pass.
    ///
    /// @param cte - the FROM term whose queue is wanted
    fn recursive_rows(&self, cte: usize) -> Option<&[Vec<OwnedDatum>]> {
        let _ = cte;
        None
    }

    /// Reports whether `LIKE` compares ASCII letters exactly on this connection.
    ///
    /// `PRAGMA case_sensitive_like`. It is asked here rather than carried on
    /// the parameters because it is a fact about the connection a statement is
    /// being compiled *for*, and the pragma empties the statement cache when it
    /// changes - the same contract `foreign_keys` has.
    fn like_is_case_sensitive(&self) -> bool {
        false
    }

    /// Returns the rowids an index a module owns says are nearest a vector.
    ///
    /// **The one thing the executor cannot work out for itself.** The index's
    /// rows live in a virtual table and the module that owns it is registered
    /// on the connection, which is above this layer - so the executor asks, the
    /// same way it asks for a module's rows, and gets back the row numbers of
    /// the *table* rather than anything module-shaped.
    ///
    /// `None` means there is no such index, which the caller turns into a
    /// refusal rather than an empty answer: a search that quietly found nothing
    /// is the worst of the three possible outcomes.
    ///
    /// @param _index - the store's name
    /// @param _probe - the vector to measure against
    /// @param _depth - how many candidates to ask for
    fn vector_candidates(
        &self,
        _index: &[u8],
        _probe: &Datum<'_>,
        _depth: usize,
    ) -> DbResult<Option<Vec<i64>>> {
        Ok(None)
    }
}

/// A catalog that also answers one recursive CTE's queue.
///
/// Everything else is delegated, so the step arm sees exactly the trees, the
/// layouts and the modules the statement sees. Wrapping rather than threading a
/// parameter through every builder is what keeps a recursive query from
/// changing the shape of a signature nothing else uses.
mod keys;
use keys::{index_union_keys, point_key, range_union_bounds, rowid_union_keys, span_bounds};
pub(crate) use keys::{nested_key, SpanBounds};

pub(crate) struct WithQueue<'a> {
    /// The catalog underneath, which answers everything but the queue.
    pub(crate) inner: &'a dyn TreeCatalog,
    /// The FROM term this queue belongs to.
    pub(crate) cte: usize,
    /// The rows the previous pass produced.
    pub(crate) rows: &'a [Vec<OwnedDatum>],
}

impl TreeCatalog for WithQueue<'_> {
    fn pool_for(&self, root: u32) -> Option<&Pool> {
        self.inner.pool_for(root)
    }

    fn tree(&self, root: u32) -> Option<&PagedTree> {
        self.inner.tree(root)
    }

    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>> {
        self.inner.layout(root)
    }

    fn covering_candidates(&self, table_root: u32) -> Vec<u32> {
        self.inner.covering_candidates(table_root)
    }

    fn virtual_cursor(
        &self,
        table: &TableInfo,
        path: &AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
        downstream: &mut dyn crate::ops::Sink,
    ) -> DbResult<bool> {
        self.inner
            .virtual_cursor(table, path, params, needed, downstream)
    }

    fn user_scalar(&self, name: &[u8], argc: usize) -> Option<crate::expr::ScalarBody> {
        self.inner.user_scalar(name, argc)
    }

    fn user_scalar_is_deterministic(&self, name: &[u8], argc: usize) -> bool {
        self.inner.user_scalar_is_deterministic(name, argc)
    }

    fn user_aggregate(&self, name: &[u8], argc: usize) -> Option<crate::expr::AggregateBody> {
        self.inner.user_aggregate(name, argc)
    }

    fn recursive_rows(&self, cte: usize) -> Option<&[Vec<OwnedDatum>]> {
        // An inner CTE's queue does not hide an outer one's: a query may hold
        // two recursive terms, and each pass wraps the catalog the other one is
        // already being read through.
        if cte == self.cte {
            return Some(self.rows);
        }
        self.inner.recursive_rows(cte)
    }
}

/// A physical choice a test or a `PRAGMA` can force.
///
/// The TDD's `PRAGMA inillucent.force_plan`, and the metamorphic tests' whole
/// mechanism: the same query is run under each applicable alternative and must
/// produce the same digest. A choice the plan cannot honour is an **error**,
/// not a silent fallback - a metamorphic test that quietly ran the default
/// twice would pass while proving nothing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ForcePlan {
    /// Read the table rather than any covering index.
    pub table_scan: bool,
    /// Sort rather than keep a bounded heap, even under a `LIMIT`.
    pub full_sort: bool,
    /// Build a hash set rather than de-duplicating adjacent rows.
    pub hash_distinct: bool,
    /// Build a hash table rather than streaming a grouped aggregate.
    pub hash_group: bool,
    /// Walk every row rather than seeking one per distinct key prefix.
    pub no_skip_scan: bool,
}

impl ForcePlan {
    /// Returns the choice a `PRAGMA inillucent.force_plan` string names.
    ///
    /// The string is a comma-separated list of operator names, matching the
    /// TDD's `'<operator list>'`. An unknown name is refused rather than
    /// ignored, because a test that misspelled its own lever would otherwise
    /// report a pass.
    ///
    /// @param text - the pragma's value
    pub fn parse(text: &str) -> DbResult<ForcePlan> {
        let mut forced = ForcePlan::default();
        for name in text.split(',') {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            match name.as_str() {
                "scan" | "tablescan" => forced.table_scan = true,
                "sort" => forced.full_sort = true,
                "distinct" | "hashdistinct" => forced.hash_distinct = true,
                "hashaggregate" | "hashgroup" => forced.hash_group = true,
                "noskipscan" | "noskip" => forced.no_skip_scan = true,
                other => {
                    return Err(misuse(format!(
                        "force_plan does not know the operator '{other}'"
                    )))
                }
            }
        }
        Ok(forced)
    }

    /// Returns every lever, for the metamorphic sweep.
    pub fn alternatives() -> Vec<(&'static str, ForcePlan)> {
        vec![
            ("default", ForcePlan::default()),
            (
                "scan",
                ForcePlan {
                    table_scan: true,
                    ..ForcePlan::default()
                },
            ),
            (
                "sort",
                ForcePlan {
                    full_sort: true,
                    ..ForcePlan::default()
                },
            ),
            (
                "distinct",
                ForcePlan {
                    hash_distinct: true,
                    ..ForcePlan::default()
                },
            ),
            (
                "hashgroup",
                ForcePlan {
                    hash_group: true,
                    ..ForcePlan::default()
                },
            ),
            (
                "noskip",
                ForcePlan {
                    no_skip_scan: true,
                    ..ForcePlan::default()
                },
            ),
        ]
    }
}

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
pub type Bindings = std::sync::Arc<std::sync::Mutex<Vec<OwnedDatum>>>;

/// The values bound to `?1`, `?2`, ... for one execution.
///
/// **`Clone` is written out rather than derived, and the reason is the cell.**
/// `Correlation::answer` clones the statement's parameters and writes an outer
/// row's columns into slots above the declared count, once per row. A derived
/// `Clone` would share the `Rc`, so those writes would land in the *outer*
/// statement's bindings and every row of the outer query would see the last
/// inner row's values. A copied set gets a cell of its own.
#[derive(Debug, Default)]
pub struct Params {
    values: Bindings,
    /// How many parameters the statement has, once it has been compiled.
    ///
    /// `None` until the binder says, because a caller may bind before the
    /// statement exists. See [`Params::try_set`].
    declared: Option<u32>,
    /// How many times a parameter has been read out of this set.
    ///
    /// The counter is what makes [`Statement`] safe. A statement may only be
    /// re-run against new parameters if nothing but its *source* looked at the
    /// old ones - a `LIMIT ?1`, a projected `?2` or a residual filter over a
    /// parameter is baked into the operator chain when the chain is built, and
    /// re-running that chain against different values would answer the previous
    /// question with the new question's parameters.
    ///
    /// Deciding that by inspecting the plan means a second, separate opinion
    /// about which constructs can carry a parameter, which is exactly the kind
    /// of duplicated judgement that goes stale when a construct is added.
    /// Counting the reads asks the builder instead: every path that consumes a
    /// parameter goes through [`Params::get`], so if the count does not move
    /// while everything except the source is built, nothing except the source
    /// read one.
    reads: std::cell::Cell<u64>,
    /// What each uncorrelated subquery in this statement answered.
    ///
    /// Indexed by the statement-wide number the binder gave the subquery, and
    /// empty when the statement has none. It rides here rather than in the plan
    /// because a folded subquery is true only of the data it was read from, and
    /// plans are cached by their text: a value baked into the plan would answer
    /// `SELECT (SELECT count(*) FROM t)` with the count from whenever the
    /// statement was first compiled.
    ///
    /// An entry is `None` when the subquery is correlated, which is the one
    /// case that has no single value. The physical pass refuses those by name.
    subqueries: Vec<Option<crate::subquery::Subvalue>>,
    /// What the connection's counters said when this statement began.
    ///
    /// `changes()`, `total_changes()`, `last_insert_rowid()` and the seed the
    /// random built-ins draw from. They ride here for the same reason a folded
    /// subquery does: a plan is cached by its text, so a value baked into the
    /// plan would answer `SELECT changes()` with the count from whenever the
    /// statement was first compiled.
    ///
    /// They are constants for the length of one statement, which is SQLite's
    /// own rule - the counters move when a statement *finishes* - so reading
    /// them once here is not an approximation.
    ///
    /// A `Cell` because the engine fills it on a `Params` the caller owns and
    /// lends: an application binds its values and hands over `&Params`, and the
    /// connection state is not the application's to supply.
    context: std::cell::Cell<crate::scalar::Context>,
    /// Whether a trigger's own writes fire triggers.
    ///
    /// `PRAGMA recursive_triggers`. It rides here rather than being compiled in
    /// because it is read where a body statement is *run* - see
    /// `crate::trigger::run_body` - and not where one is translated.
    recursive_triggers: std::cell::Cell<bool>,
}

/// Scrambles a seed into the next one.
///
/// `splitmix64`, which is the function library's own scrambler, so adjacent
/// seeds give unrelated streams rather than correlated ones.
///
/// @param seed - the seed to move on from
fn split_mix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Clone for Params {
    fn clone(&self) -> Params {
        Params {
            values: Bindings::new(std::sync::Mutex::new(self.held().clone())),
            declared: self.declared,
            reads: std::cell::Cell::new(self.reads.get()),
            context: self.context.clone(),
            recursive_triggers: self.recursive_triggers.clone(),
            subqueries: self.subqueries.clone(),
        }
    }
}

impl Params {
    /// Returns an empty parameter set.
    pub fn new() -> Params {
        Params {
            values: Bindings::default(),
            declared: None,
            reads: std::cell::Cell::new(0),
            context: std::cell::Cell::new(crate::scalar::Context::default()),
            recursive_triggers: std::cell::Cell::new(false),
            subqueries: Vec::new(),
        }
    }

    /// Returns a parameter set over a list of values, `?1` first.
    ///
    /// @param values - the values, in parameter order
    pub fn from_values(values: Vec<OwnedDatum>) -> Params {
        Params {
            values: Bindings::new(std::sync::Mutex::new(values)),
            declared: None,
            reads: std::cell::Cell::new(0),
            context: std::cell::Cell::new(crate::scalar::Context::default()),
            recursive_triggers: std::cell::Cell::new(false),
            subqueries: Vec::new(),
        }
    }

    /// Returns this set with room for a statement's folded subqueries.
    ///
    /// The bound values are carried over, because a subquery's own block may
    /// read `?1` and has to see the same binding the outer statement did.
    ///
    /// @param subqueries - one slot per subquery, by the binder's numbering
    pub fn with_subqueries(&self, subqueries: Vec<Option<crate::subquery::Subvalue>>) -> Params {
        Params {
            values: Bindings::new(std::sync::Mutex::new(self.held().clone())),
            declared: self.declared,
            reads: std::cell::Cell::new(self.reads.get()),
            context: self.context.clone(),
            recursive_triggers: self.recursive_triggers.clone(),
            subqueries,
        }
    }

    /// Returns this set with the folded-subquery table emptied.
    ///
    /// **For a statement run from inside another one's row.** A correlated
    /// block is a statement of its own and folds its own uncorrelated
    /// subqueries; carrying the outer table in would tell it the fold had
    /// already happened - `has_subqueries` is how `crate::subquery::fold`
    /// decides - and leave its slots unfilled, which reads from inside
    /// `translate` as "a correlated subquery": a true sentence about the slot
    /// and a false one about the query.
    ///
    /// The bound values are kept, because a nested block may read `?1` and has
    /// to see the same binding the outer statement did.
    pub fn without_subqueries(&self) -> Params {
        Params {
            values: Bindings::new(std::sync::Mutex::new(self.held().clone())),
            declared: self.declared,
            reads: std::cell::Cell::new(self.reads.get()),
            context: self.context.clone(),
            recursive_triggers: self.recursive_triggers.clone(),
            subqueries: Vec::new(),
        }
    }

    /// Records what one subquery answered.
    ///
    /// @param id - the binder's number for the subquery
    /// @param value - the rows it produced, read as one column
    pub fn set_subquery(&mut self, id: usize, value: crate::subquery::Subvalue) {
        if let Some(slot) = self.subqueries.get_mut(id) {
            *slot = Some(value);
        }
    }

    /// Returns whether this execution's subqueries have been folded already.
    pub fn has_subqueries(&self) -> bool {
        !self.subqueries.is_empty()
    }

    /// Returns what one subquery answered, or `None` when it is correlated.
    ///
    /// Counted as a parameter read for the same reason a `?1` is: a chain that
    /// baked this value in must not be re-run against a later state of the
    /// table, and the read counter is what already decides that.
    ///
    /// @param id - the binder's number for the subquery
    pub fn subquery(&self, id: usize) -> Option<&crate::subquery::Subvalue> {
        self.reads.set(self.reads.get().saturating_add(1));
        self.subqueries.get(id).and_then(|slot| slot.as_ref())
    }

    /// Returns a copy of the bound values, `?1` first.
    pub fn values(&self) -> Vec<OwnedDatum> {
        self.held()
    }

    /// Returns how many parameter reads this set has answered.
    pub fn reads(&self) -> u64 {
        self.reads.get()
    }

    /// Tells this set what the connection's counters say.
    ///
    /// Called once per statement, by the engine, before anything is compiled.
    ///
    /// @param context - the counters and the statement's random seed
    pub fn set_context(&self, context: crate::scalar::Context) {
        self.context.set(context);
    }

    /// Tells this set whether a trigger's own writes fire triggers.
    ///
    /// @param recursive - what `PRAGMA recursive_triggers` is set to
    pub fn set_recursive_triggers(&self, recursive: bool) {
        self.recursive_triggers.set(recursive);
    }

    /// Returns whether a trigger's own writes fire triggers.
    ///
    /// Not counted as a parameter read: nothing is compiled from it, so a chain
    /// built while it was one way is not stale when it is the other.
    pub fn recursive_triggers(&self) -> bool {
        self.recursive_triggers.get()
    }

    /// Returns what the connection's counters said.
    ///
    /// Counted as a parameter read for the same reason a `?1` is: a chain that
    /// baked these in must not be re-run against a later state of the
    /// connection, and the read counter is what already decides that.
    ///
    /// **The seed moves on every read, so two call sites in one statement do
    /// not share a stream.** `SELECT random(), random()` is two nodes, each
    /// with its own stream advanced per row; started from the same number they
    /// would answer the same pair, which is what SQLite does not do. The
    /// counters themselves are unchanged by the read - every `changes()` in one
    /// statement is the same number.
    pub fn context(&self) -> crate::scalar::Context {
        self.reads.set(self.reads.get().saturating_add(1));
        let held = self.context.get();
        self.context.set(crate::scalar::Context {
            seed: split_mix(held.seed),
            ..held
        });
        held
    }

    /// Replaces every bound value, reusing the buffer.
    ///
    /// A benchmark that re-binds a prepared statement per iteration should not
    /// allocate to do it - `sqlite3_bind_int64` does not - and building a fresh
    /// `Params` per execution was one `Vec` per execution on the arm being
    /// timed.
    ///
    /// @param values - the new values, `?1` first
    pub fn refill(&mut self, values: impl IntoIterator<Item = OwnedDatum>) {
        let Ok(mut held) = self.values.lock() else {
            return;
        };
        held.clear();
        held.extend(values);
    }

    /// Returns the value bound to a parameter.
    ///
    /// An unbound parameter is NULL, which is what SQLite does.
    ///
    /// @param index - the one-based parameter number
    pub fn get(&self, index: u32) -> OwnedDatum {
        self.reads.set(self.reads.get().saturating_add(1));
        self.held()
            .get(index.saturating_sub(1) as usize)
            .cloned()
            .unwrap_or(OwnedDatum::Null)
    }

    /// Returns the cell an `Expr::Parameter` reads when it is evaluated.
    ///
    /// **Not counted as a read.** Handing over the cell is the opposite of
    /// folding a value into the chain: the chain that holds this answers
    /// whatever is in it at the moment it runs, which is the property
    /// [`Statement::rebindable`] exists to establish.
    pub fn bindings(&self) -> Bindings {
        std::sync::Arc::clone(&self.values)
    }

    /// Returns a copy of the bound values, for the paths that need them all.
    ///
    /// A lock that cannot be taken reads as no values bound, which is what an
    /// unbound set is - a poisoned mutex here would otherwise turn a parameter
    /// read into a panic on a path that is not allowed to have one.
    fn held(&self) -> Vec<OwnedDatum> {
        self.values
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    /// Copies another set's values into this one's cell.
    ///
    /// The cell is shared with a compiled chain, so this is how a statement
    /// built against one execution's parameters is pointed at the next
    /// execution's without rebuilding anything.
    ///
    /// @param from - the set holding the new values
    pub fn adopt(&self, from: &Params) {
        let source = from.held();
        let Ok(mut held) = self.values.lock() else {
            return;
        };
        held.clear();
        held.extend_from_slice(&source);
    }

    /// Records that the chain being built folded in a value that is only true
    /// of this execution.
    ///
    /// **`now` is the one that made this necessary.** Every `now` in one
    /// statement is the same instant, which is SQLite's rule, so `translate`
    /// reads the clock once and puts the reading in the node - and a chain kept
    /// across executions would then answer `datetime('now')` with the instant it
    /// was built. Counting it as a read is what makes
    /// [`Statement::rebindable`] refuse to reuse such a chain, using the
    /// mechanism already there for a folded subquery and a folded `changes()`.
    pub fn note_execution_constant(&self) {
        self.reads.set(self.reads.get().saturating_add(1));
    }

    /// Binds one parameter by its one-based number.
    ///
    /// Parameters between the highest bound so far and this one become NULL,
    /// which is what an unbound parameter already is - so binding `?3` before
    /// `?1` leaves `?1` NULL rather than shifting it.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value to bind
    /// Binds one parameter with no range check, for the engine's own use.
    ///
    /// **Deliberately unchecked, and not what a caller's `bind` goes through.**
    /// The binder allocates parameter slots of its own above the ones the SQL
    /// wrote - `correlate::Correlation::answer` feeds an outer row's columns
    /// into a correlated block through exactly this method, at numbers past the
    /// statement's declared count. Routing this through the checked form made
    /// those writes vanish and every correlated `EXISTS` answered against an
    /// unbound slot, which is a wrong answer rather than an error.
    ///
    /// [`Params::try_set`] is the caller-facing one, and the split is the
    /// point: the engine may write any slot it invented, and an application may
    /// only write the ones its statement declared.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn set(&mut self, index: u32, value: OwnedDatum) {
        let at = index.saturating_sub(1) as usize;
        let Ok(mut held) = self.values.lock() else {
            return;
        };
        if held.len() <= at {
            held.resize(at.saturating_add(1), OwnedDatum::Null);
        }
        if let Some(slot) = held.get_mut(at) {
            *slot = value;
        }
    }

    /// Binds one parameter, reporting an index the statement does not have.
    ///
    /// **`index` is one-based, and zero is out of range rather than the first
    /// slot.** This used to be `index.saturating_sub(1)` into a vector that was
    /// resized to fit whatever it was given, which made every index legal: a
    /// bind of 9 on a one-parameter statement grew the set to nine slots and
    /// answered `Ok`, and a bind of **0 silently wrote over `?1`** - so a caller
    /// who believed index 0 was a no-op had replaced its first parameter and
    /// had nothing in the result to say so. SQLite answers `SQLITE_RANGE` to
    /// both, and now so does this.
    ///
    /// The set still grows, because it has to: a caller binds `?1` before the
    /// statement it belongs to has been compiled, so at bind time the number of
    /// parameters may not be known. What it will not do is grow past the count
    /// once one has been declared with [`Params::expect`].
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    ///
    /// **The error type is `()` on purpose.** There is exactly one way this
    /// fails - the index is zero or past a declared count - and the caller
    /// turns it into the engine's own refusal with the parameter number it
    /// already has. An error type carrying that number would be the same number
    /// twice.
    #[allow(clippy::result_unit_err)]
    pub fn try_set(&mut self, index: u32, value: OwnedDatum) -> Result<(), ()> {
        if index == 0 {
            return Err(());
        }
        if let Some(declared) = self.declared {
            if index > declared {
                return Err(());
            }
        }
        let at = (index - 1) as usize;
        let Ok(mut held) = self.values.lock() else {
            return Err(());
        };
        if held.len() <= at {
            held.resize(at.saturating_add(1), OwnedDatum::Null);
        }
        if let Some(slot) = held.get_mut(at) {
            *slot = value;
        }
        Ok(())
    }

    /// Tells this set how many parameters the statement it belongs to has.
    ///
    /// Called once the statement is compiled and the binder knows the answer.
    /// Until then the set has no upper bound to check against and only index
    /// zero is refused.
    ///
    /// @param count - the highest parameter number the statement uses
    pub fn expect(&mut self, count: u32) {
        self.declared = Some(count);
    }

    /// Returns how many parameters the statement was said to have.
    pub fn declared(&self) -> Option<u32> {
        self.declared
    }

    /// Unbinds every parameter.
    pub fn clear(&mut self) {
        if let Ok(mut held) = self.values.lock() {
            held.clear();
        }
    }

    /// Returns how many parameters are bound.
    pub fn len(&self) -> usize {
        self.values.lock().map(|held| held.len()).unwrap_or(0)
    }

    /// Reports whether nothing is bound.
    pub fn is_empty(&self) -> bool {
        self.values
            .lock()
            .map(|held| held.is_empty())
            .unwrap_or(true)
    }
}

/// How one stage reads its tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessKind {
    /// Every row of the tree, in key order.
    Full,
    /// A key range, forward.
    Span,
    /// A key range, backward.
    Reverse,
    /// One row per distinct key prefix.
    Skip,
    /// One row by key.
    Point,
    /// Several rows by key, several probes of the same tree concatenated -
    /// what an `IN` list becomes once the planner turns it into seeks.
    SeekUnion,
    /// Several key ranges over the same tree, concatenated in the order that
    /// keeps their combined output in the composite key's order - what a
    /// keyset page's `OR`-shaped tuple comparison becomes.
    RangeUnion,
    /// A probe of this tree once per row of the stage before it.
    Nested,
    /// The rows an index a module owns named, read out of the tree by rowid.
    ///
    /// The tree is the table's, so the layout and every column slot are the
    /// ordinary ones; what is different is *which* rows and in what order -
    /// the module chose them, and the plan's own `ORDER BY` then rescores them.
    Vector,
    /// The rows a nested query produced, materialised once before the pipeline
    /// runs.
    ///
    /// A `FROM (SELECT ...)` term reads no tree, so this stage's `root` names
    /// nothing and its layout is carried on the stage rather than looked up.
    /// Materialising rather than streaming is what a push executor can do
    /// without a coroutine: the inner pipeline runs to completion into a buffer
    /// and the buffer drives the outer one.
    Materialised,
}

impl AccessKind {
    /// Returns the name `EXPLAIN` prints.
    pub fn describe(self) -> &'static str {
        match self {
            AccessKind::Full => "SCAN",
            AccessKind::Vector => "VECTOR SEARCH",
            AccessKind::Span => "RANGE",
            AccessKind::Reverse => "RANGE REVERSE",
            AccessKind::Skip => "SKIP SCAN",
            AccessKind::Point => "POINT PROBE",
            AccessKind::SeekUnion => "SEEK UNION",
            AccessKind::RangeUnion => "RANGE UNION",
            AccessKind::Nested => "INDEX NESTED LOOP",
            AccessKind::Materialised => "SCAN SUBQUERY",
        }
    }
}

/// One stage's physical choice.
#[derive(Clone, Debug)]
pub struct PreparedStage {
    /// The module's auxiliary functions this stage materialises, in slot order.
    ///
    /// Empty for everything but a virtual scan. Their answers sit after the
    /// declared columns and the rowid, so a query reading `score(t)` finds it
    /// at a column the module filled rather than at an expression the pipeline
    /// has no way to evaluate.
    pub functions: Vec<(Vec<u8>, Vec<inillucent_sql::bind::BoundExpr>)>,
    /// The tree this stage reads.
    pub root: u32,
    /// How it reads it.
    pub kind: AccessKind,
    /// The **statement-wide** id every bound expression refers to this FROM
    /// term by.
    ///
    /// Not its position in `plan.sources`, and the two are different exactly
    /// when the planner reorders the join - which is the case this got wrong.
    /// A bound `Column { source, slot }` carries the binder's id, so matching it
    /// against a position silently resolved every column of a reordered join to
    /// the wrong stage: `SELECT people.name, teams.region FROM people JOIN
    /// teams` returned no rows while `FROM teams JOIN people` returned the
    /// right nine, because only the second one has position equal to id.
    pub source: usize,
    /// This stage's position in `plan.sources`, which is the visit order.
    ///
    /// The other half of the same distinction: the *plan's* own arrays are
    /// indexed by visit order, so a stage needs both numbers and conflating
    /// them is a wrong answer rather than an error.
    pub term: usize,
    /// Whether this stage is the table fetch behind a non-covering index seek.
    pub is_lookup: bool,
    /// The first column index this stage contributes to the joined row.
    pub offset: usize,
    /// How many columns it contributes.
    pub width: usize,
    /// The layout of a stage that reads no tree.
    ///
    /// `None` for every stage that reads one, whose layout the catalog holds.
    /// A materialised subquery has no entry in the catalog to hold it, and
    /// synthesising one *here* rather than registering it in the catalog is
    /// what keeps the catalog a description of the file.
    pub layout: Option<std::rc::Rc<SourceLayout>>,
}

/// What a statement's physical choices are, decided once.
///
/// The structural decisions - which tree to read, and therefore whether a sort,
/// a hash table or a set is needed at all - depend on the statement and the
/// schema and not on the data, so they belong to prepare rather than to
/// execution. Keeping them here is not only tidiness: the covering rule tries
/// candidate trees by *building* a pipeline over each, and doing that on every
/// execution made a 64-row query spend more time choosing than answering.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The stages, outermost first.
    pub stages: Vec<PreparedStage>,
    /// The levers this plan was prepared under.
    pub forced: ForcePlan,
}

impl Prepared {
    /// Returns the tree the outermost stage reads.
    ///
    /// Kept because the gate harness reports the structure each engine chose,
    /// and "which tree" is most of that answer.
    pub fn root(&self) -> u32 {
        self.stages.first().map(|stage| stage.root).unwrap_or(0)
    }

    /// Returns one line per stage, for `EXPLAIN`.
    pub fn describe(&self) -> Vec<String> {
        self.stages
            .iter()
            .map(|stage| {
                format!(
                    "{} tree {}{}",
                    stage.kind.describe(),
                    stage.root,
                    if stage.is_lookup {
                        " (rowid lookup)"
                    } else {
                        ""
                    }
                )
            })
            .collect()
    }
}

/// A built pipeline, ready to run.
pub struct Pipeline<'t> {
    /// What drives it.
    pub source: Source<'t>,
    /// The head of the operator chain.
    ///
    /// It borrows for `'t` because an index nested loop holds the inner tree
    /// and the pool, and it sits at the *bottom* of the chain - closest to the
    /// source - so everything above it is still an ordinary owned operator.
    /// That is why only this one box carries a lifetime and none of the
    /// operators in [`crate::ops`] had to grow one.
    pub head: Box<dyn Sink + 't>,
    /// The pool the source's pages live in, when the source reads a tree.
    ///
    /// `None` for a source that is already rows - a materialised subquery, a
    /// module's answer, a recursive queue, `VALUES`, or a query with no FROM
    /// term. Those read no page, so there is no file they belong to, and
    /// handing them some other schema's pool to ignore would be a lie the type
    /// could not catch.
    pub pool: Option<&'t Pool>,
}

impl Pipeline<'_> {
    /// Drives the pipeline to completion.
    pub fn run(&mut self) -> DbResult<()> {
        self.source.run(self.pool, self.head.as_mut())
    }
}

/// How many seek-key columns a point probe borrows on the stack.
///
/// Four covers every rowid table and every index in the scorecard fixture and
/// in the dialect's own corpus; a wider key spills, which costs what every key
/// used to cost.
const POINT_KEY_INLINE: usize = 4;

/// What drives a pipeline.
pub enum Source<'t> {
    /// Every row of a tree, in key order.
    Scan(FullScan<'t>),
    /// A key range, forward.
    Span(SpanScan<'t>),
    /// A key range, backward.
    Reverse(ReverseScan<'t>),
    /// One row per distinct value of a key prefix.
    Skip(SkipScan<'t>),
    /// One row by key.
    Point(PointProbe<'t>, Vec<OwnedDatum>),
    /// Several rows by key, several probes of the same tree - what
    /// `AccessKind::SeekUnion` runs. Every key has already been evaluated and
    /// de-duplicated (a literal repeat folded at plan time, a repeat only
    /// visible at run time folded here, against the concrete values this
    /// execution actually bound), so every probe here is worth making.
    SeekUnion(PointProbe<'t>, Vec<Vec<OwnedDatum>>),
    /// Several key ranges over the same tree, concatenated in the order that
    /// keeps their combined output in the composite key's order - what
    /// `AccessKind::RangeUnion` runs.
    RangeUnion(Vec<SpanScan<'t>>),
    /// The rows an index a module owns named, by rowid, in its order.
    Vector(PointProbe<'t>, Vec<i64>),
    /// Rows a nested query produced, already materialised.
    Rows(Vec<Vec<OwnedDatum>>),
    /// A module's scan, driven a batch at a time and abandoned on `Flow::Stop`.
    ///
    /// The catalog rather than the rows, because the rows do not exist yet -
    /// that is the whole point. `generate_series` with no `stop` constraint is
    /// 4,294,967,295 rows, and materialising it before the `LIMIT` above it runs
    /// is a query that does not return.
    Virtual(Box<VirtualScanSource<'t>>),
    /// A fixed number of rows of no columns at all.
    ///
    /// What drives `SELECT 1`, `SELECT date('now')` and every other query with
    /// no FROM term: there is exactly one row, it has no columns, and the whole
    /// answer comes out of the projection's constant expressions. A batch of one
    /// row and zero vectors is a perfectly ordinary batch - `Batch::live`
    /// returns the row count and every consumer's fast path is over the columns
    /// it was asked for, of which there are none.
    ///
    /// The Phase 2 pass refused these, and nineteen of the read-only SLT
    /// corpus's thirty-seven refusals were exactly this shape.
    Constant(usize),
}

/// Everything a module's scan needs, kept so it can be driven at run time.
///
/// Boxed inside [`Source::Virtual`] because it is the largest variant by a wide
/// margin and every other source is a handful of words; a `Source` that grew to
/// the size of a `TableInfo` would be copied around the hot read path for the
/// benefit of the one arm that reads a virtual table.
pub struct VirtualScanSource<'t> {
    /// Where the module is resolved from.
    pub catalog: &'t dyn TreeCatalog,
    /// The FROM term's table, which names the module's instance.
    pub table: TableInfo,
    /// The access path the planner chose, carrying the pushed-down offer.
    pub path: AccessPath,
    /// The values this execution bound.
    pub params: Params,
    /// Which of the term's columns the query reads.
    pub needed: inillucent_sql::bind::ColumnUse,
}

/// Reads one catalog's `case_sensitive_like`, as a function `is_some_and` takes.
///
/// @param catalog - the catalog the statement is compiled against
fn inillucent_exec_like_case_sensitive(catalog: &dyn TreeCatalog) -> bool {
    catalog.like_is_case_sensitive()
}

/// Returns the pool a source that reads a tree must have been given.
///
/// @param pool - what the pipeline carried
fn needs_pool(pool: Option<&Pool>) -> DbResult<&Pool> {
    pool.ok_or_else(|| misuse("this source reads a tree no attached database holds"))
}

impl Source<'_> {
    /// Drives the source until the pipeline is done.
    ///
    /// @param pool - the pool the source's tree lives in, when it reads one
    /// @param downstream - the head of the operator chain
    pub fn run(&self, pool: Option<&Pool>, downstream: &mut dyn Sink) -> DbResult<()> {
        // **Asked for by the arms that read pages, and by no others.** `Rows`
        // and `Constant` are already materialised, so a pool would be a
        // parameter they ignore; a source that does read a tree with no pool
        // behind it is a plan naming a schema this connection does not hold,
        // which is a refusal rather than a page read out of the wrong file.
        match self {
            Source::Scan(scan) => scan.run(needs_pool(pool)?, downstream),
            Source::Span(scan) => scan.run(needs_pool(pool)?, downstream),
            Source::Reverse(scan) => scan.run(needs_pool(pool)?, downstream),
            Source::Skip(scan) => scan.run(needs_pool(pool)?, downstream),
            Source::Point(probe, key) => {
                let pool = needs_pool(pool)?;
                // The borrows go on the stack. A seek key is one column in
                // every rowid table and at most a handful in any index, and
                // collecting them was one allocation per execution on the
                // shortest path the engine has.
                let mut inline: [Datum<'_>; POINT_KEY_INLINE] = [Datum::Null; POINT_KEY_INLINE];
                let spilled: Vec<Datum<'_>>;
                let borrowed: &[Datum<'_>] = if key.len() <= POINT_KEY_INLINE {
                    for (at, value) in key.iter().enumerate() {
                        if let Some(slot) = inline.get_mut(at) {
                            *slot = value.borrow();
                        }
                    }
                    inline.get(..key.len()).unwrap_or(&[])
                } else {
                    spilled = key.iter().map(OwnedDatum::borrow).collect();
                    spilled.as_slice()
                };
                probe.run(pool, borrowed, downstream)
            }
            Source::SeekUnion(probe, keys) => {
                let pool = needs_pool(pool)?;
                // One descent per key, exactly what running each branch's own
                // `RowidSeek`/`IndexSeek` in turn would cost - and a list long
                // enough to make that expensive is a list the planner already
                // prices against a scan and loses.
                let mut buffer: Vec<OwnedDatum> = Vec::new();
                let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(keys.len());
                for key in keys {
                    let borrowed: Vec<Datum<'_>> = key.iter().map(OwnedDatum::borrow).collect();
                    if probe.lookup(pool, &borrowed, &mut buffer)? {
                        rows.push(buffer.clone());
                    }
                }
                crate::ops::emit_rows(&rows, downstream)?;
                downstream.finish()
            }
            Source::RangeUnion(branches) => {
                let pool = needs_pool(pool)?;
                // Each branch is collected rather than streamed straight to
                // `downstream`: `SpanScan::run` finishes its sink when it
                // returns, and finishing `downstream` after the first branch
                // would tell it the whole union was done. Collecting loses a
                // downstream `LIMIT`'s ability to stop the *later* branches
                // early, which is the one thing this costs next to the
                // bytecode engine's branch-by-branch loop - the branches
                // still only cover the keyset page's own range, never the
                // whole table, so the seek this replaces a scan with is not
                // what the cost was paid for.
                let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let mut sink = crate::ops::CollectInto::new(std::rc::Rc::clone(&collected));
                for branch in branches {
                    branch.run(pool, &mut sink)?;
                }
                let rows = collected.borrow().clone();
                crate::ops::emit_rows(&rows, downstream)?;
                downstream.finish()
            }
            Source::Vector(probe, keys) => {
                let pool = needs_pool(pool)?;
                // One descent per candidate, and the candidates are already the
                // few the index chose - so this is `k` probes rather than a
                // scan, which is the whole point of the path.
                let mut buffer: Vec<OwnedDatum> = Vec::new();
                let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(keys.len());
                for key in keys {
                    if probe.lookup(pool, &[Datum::Int(*key)], &mut buffer)? {
                        rows.push(buffer.clone());
                    }
                }
                crate::ops::emit_rows(&rows, downstream)?;
                downstream.finish()
            }
            Source::Rows(rows) => {
                crate::ops::emit_rows(rows, downstream)?;
                downstream.finish()
            }
            Source::Virtual(scan) => {
                scan.catalog.virtual_cursor(
                    &scan.table,
                    &scan.path,
                    &scan.params,
                    &scan.needed,
                    downstream,
                )?;
                downstream.finish()
            }
            Source::Constant(rows) => {
                if *rows > 0 {
                    let batch = Batch::new(*rows, Vec::new());
                    downstream.push(&batch)?;
                }
                downstream.finish()
            }
        }
    }

    /// Names the source, for a plan description.
    pub fn describe(&self) -> &'static str {
        match self {
            Source::Scan(_) => "SCAN",
            Source::Span(_) => "RANGE",
            Source::Reverse(_) => "RANGE REVERSE",
            Source::Skip(_) => "SKIP SCAN",
            Source::Point(_, _) => "POINT PROBE",
            Source::SeekUnion(_, _) => "SEEK UNION",
            Source::RangeUnion(_) => "RANGE UNION",
            Source::Vector(_, _) => "VECTOR SEARCH",
            Source::Rows(_) => "SCAN SUBQUERY",
            Source::Virtual(_) => "SCAN VIRTUAL TABLE",
            Source::Constant(_) => "CONSTANT ROW",
        }
    }
}

/// What a built plan produces, so a caller can name its columns.
#[derive(Clone, Debug)]
pub struct Shape {
    /// The name of each output column, as the binder assigned it.
    pub names: Vec<Vec<u8>>,
    /// The operator chain, source first: the TDD's "`EXPLAIN` prints the
    /// physical operator tree".
    ///
    /// It is built as the chain is built rather than derived afterwards,
    /// because a description derived from the plan is a description of what the
    /// builder was *asked* for. This one says what it made. The first thing it
    /// showed was a `Filter` under a range scan whose bounds already excluded
    /// every row it was testing.
    pub operators: Vec<String>,
}

/// Returns an error naming what the physical pass will not run.
///
/// @param what - the construct, in words
pub(crate) fn unsupported<T>(what: &str) -> DbResult<T> {
    // **The sentence and the marker are written in the same place**, so a
    // caller asking `DbError::unsupported()` and a caller reading the message
    // cannot be told different things. The wording is unchanged from before
    // the marker existed, because assertions elsewhere quote it.
    let said = format!("the new engine's physical pass does not handle {what} yet");
    // The sentence is the message *and* the detail. `misuse` attaches what it is
    // given as detail alone, which left every refusal answering `message()` with
    // "bad parameter or other API misuse"; the detail is kept so that every
    // existing reader of it is unaffected.
    Err(misuse(said.clone())
        .with_message(said)
        .with_unsupported(what))
}

/// Chooses a statement's physical plan.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param forced - the levers a `PRAGMA` or a metamorphic test set
pub fn prepare(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    forced: ForcePlan,
) -> DbResult<Prepared> {
    let mut stages = plan_stages(plan, catalog, None)?;
    // The covering rule. A plain scan of a table is replaced by a scan of the
    // smallest index tree that carries every column the query reads, because
    // reading 2.66 MiB instead of 14.6 MiB is the single largest lever
    // available on an analytical query and it is the structure SQLite itself
    // chooses. The test for "carries every column" is not a heuristic: the
    // whole pipeline is built against the candidate's layout, and translation
    // fails by name on a column the tree does not hold. A candidate that
    // builds, covers.
    let single_scan = stages.len() == 1
        && stages
            .first()
            .map(|stage| stage.kind == AccessKind::Full)
            .unwrap_or(false)
        && matches!(
            plan.sources.first().map(|source| &source.path),
            Some(AccessPath::TableScan { .. })
        );
    if single_scan && !forced.table_scan && !order_sensitive(&plan.select) {
        let table_root = stages.first().map(|stage| stage.root).unwrap_or(0);
        for candidate in catalog.covering_candidates(table_root) {
            let trial = plan_stages(plan, catalog, Some(candidate))?;
            let attempt = Prepared {
                stages: trial,
                forced,
            };
            if build_prepared(plan, catalog, &attempt, &Params::new(), dummy_sink()).is_ok() {
                return Ok(attempt);
            }
        }
    }
    // A skip scan is a structural choice too, and it is decided here so that
    // execution never has to.
    if let Some(stage) = stages.first_mut() {
        if stage.kind == AccessKind::Full
            && !forced.no_skip_scan
            && skip_scan_applies(plan, catalog, stage.root)?
        {
            stage.kind = AccessKind::Skip;
        }
    }
    Ok(Prepared { stages, forced })
}

/// Returns the collation an expression is compared and ordered under.
///
/// SQLite's rule, in the part that matters here: an explicit `COLLATE` wins; a
/// column carries its own; everything else is BINARY. It is deliberately not a
/// full implementation of the rule - a `CASE` whose branches are columns has an
/// assignable collation in SQLite and BINARY here - because the conservative
/// answer is the one that sorts and groups by bytes, which is what an engine
/// that did not know about collations at all would do, and never a wrong answer
/// dressed as a right one.
///
/// @param expr - the bound expression
pub(crate) fn expression_collation(expr: &BoundExpr) -> Collation {
    match expr {
        BoundExpr::Collate { collation, .. } => *collation,
        BoundExpr::Column { collation, .. } => *collation,
        _ => Collation::Binary,
    }
}

/// Reports whether the statement's answer depends on the order its rows arrive.
///
/// The covering rule replaces a table scan with an index scan, which is the
/// single largest lever in the whole design - and it *changes the order the
/// rows reach the aggregate in*. Floating-point addition is not associative, so
/// that is not a free change: the SLT corpus has a `score` column holding
/// `-1e300`, nine ordinary values and `+1e300`, and
/// `SELECT sum(score) FROM people` is 124.25 in table order and **0.0** in
/// score order, because the small values are absorbed into the first huge one
/// and cancelled by the second. SQLite scans the table and gets 124.25; we
/// scanned `people_by_score` and got 0.0.
///
/// So the rule is skipped when a `sum`, `total` or `avg` has an argument that
/// is not statically an integer, and when a `group_concat` is present - it
/// concatenates in arrival order by definition. An integer sum accumulates in
/// `i128` and is exact, so its order does not matter, which is what keeps the
/// scorecard's `sum(key)` on the covering index where SQLite also puts it.
///
/// This is the Phase 1 lesson from the other side. Structure was the largest
/// lever there; here it is a wrong answer.
///
/// @param select - the bound statement
fn order_sensitive(select: &BoundSelect) -> bool {
    select.aggregates.iter().any(|call| match call.func {
        AggregateFunc::Sum | AggregateFunc::Total | AggregateFunc::Avg => call
            .arguments
            .first()
            .map(|argument| !integer_typed(argument))
            .unwrap_or(false),
        AggregateFunc::GroupConcat => true,
        _ => false,
    })
}

/// Reports whether a bound expression is statically an integer.
///
/// Conservative: anything it cannot prove is treated as not an integer, because
/// the cost of being wrong is a wrong answer and the cost of being cautious is
/// a table scan.
///
/// @param expr - the aggregate's argument
fn integer_typed(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Integer(_) => true,
        BoundExpr::Rowid { .. } => true,
        BoundExpr::Column { affinity, .. } => {
            *affinity == inillucent_value::affinity::Affinity::Integer
        }
        _ => false,
    }
}

/// Returns a sink that discards everything, for the covering-rule trial build.
///
/// The trial exists because "does this index cover the query" is answered by
/// building the pipeline rather than by a separate predicate that could drift
/// away from what the builder actually accepts. The trial's sink is never
/// pushed into.
fn dummy_sink() -> Box<dyn Sink> {
    Box::new(CollectInto::new(std::rc::Rc::new(std::cell::RefCell::new(
        Vec::new(),
    ))))
}

/// Turns the planner's FROM terms into stages.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param override_root - a covering index to read instead of the table
fn plan_stages(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    override_root: Option<u32>,
) -> DbResult<Vec<PreparedStage>> {
    refuse_unhandled(&plan.select)?;
    if !plan.compounds.is_empty() {
        return unsupported("a compound query");
    }
    // A query with no FROM term produces no stages at all, and that is a legal
    // plan rather than a refusal: `SELECT 1` reads no tree, so there is nothing
    // for a stage to describe. Every function below already loops over the
    // stages rather than indexing the first, except the two that build the
    // source and the space - and both now have an empty case.
    let mut stages: Vec<PreparedStage> = Vec::new();
    let mut offset = 0usize;
    let sensitive = order_sensitive(&plan.select);
    for (position, source) in plan.sources.iter().enumerate() {
        let outermost = position == 0;
        // **An outer join is answered by materialising the inner side.**
        //
        // It used to be refused, and the refusal was right while it stood: the
        // physical pass never looked at the join kind and always built
        // `JoinKind::Inner`, so a `LEFT JOIN` silently dropped the outer rows
        // that matched nothing - `SELECT people.team FROM people LEFT JOIN
        // teams ON ...` answered six rows as four nulls.
        //
        // What it needs that an index nested loop cannot give is the `ON`
        // condition evaluated per candidate *pair*: an index probe assumes the
        // key equality **is** the condition, and an outer join has to know that
        // a pair failed the condition in order to null-extend instead. So the
        // inner side is read once into a buffer and `NestedLoopJoin` evaluates
        // the condition over each pair - which also gives `RIGHT` and `FULL`,
        // because a materialised build side is the only thing that can remember
        // which of its rows matched (see `build_nested`).
        match &source.path {
            AccessPath::TableScan { root } => {
                let root = if outermost {
                    override_root.unwrap_or(*root)
                } else {
                    *root
                };
                push_stage(
                    &mut stages,
                    catalog,
                    root,
                    if outermost {
                        AccessKind::Full
                    } else {
                        // An inner term with no usable index is a cross
                        // product: every inner row pairs with every outer one,
                        // and any predicate over the pair is a residual. Phase 2
                        // refused it because the read families never produce
                        // one; the corpora do - `SELECT count(*) FROM people
                        // CROSS JOIN teams` - and refusing a shape the engine
                        // can answer is a gap rather than a policy.
                        AccessKind::Nested
                    },
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
            }
            AccessPath::RowidSeek { root, .. } => {
                push_stage(
                    &mut stages,
                    catalog,
                    *root,
                    if outermost {
                        AccessKind::Point
                    } else {
                        AccessKind::Nested
                    },
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
            }
            AccessPath::RowidRange { root, .. } => {
                let kind = if !outermost {
                    return unsupported("a rowid range as an inner join term");
                } else if plan.reverse {
                    AccessKind::Reverse
                } else {
                    AccessKind::Span
                };
                push_stage(
                    &mut stages,
                    catalog,
                    *root,
                    kind,
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
            }
            // A union of probes composes with a per-row nested loop exactly
            // as a lone probe does - one more of the same thing, at every
            // level - but this engine has no join operator that drives one
            // yet, so it is offered only where it drives the whole pipeline.
            // The bytecode engine does not share this limit: it compiles a
            // union's branches the same way at any level, one loop per
            // branch, which is why the same SQL runs on both engines while
            // only one of them takes the fast path everywhere the planner
            // found one.
            AccessPath::RowidSeekUnion { root, .. } => {
                if !outermost {
                    return unsupported("a seek union as an inner join term");
                }
                push_stage(
                    &mut stages,
                    catalog,
                    *root,
                    AccessKind::SeekUnion,
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
            }
            AccessPath::IndexSeekUnion {
                table_root,
                index_root,
                index_name,
                covering,
                branches,
                ..
            } => {
                if !outermost {
                    return unsupported("a seek union as an inner join term");
                }
                // The branches of an `IN` list are bare equalities and probed
                // like `RowidSeekUnion`'s; the branches of a keyset page are
                // ranges, and reconstructing the page's order depends on
                // walking each one and running them in the order they were
                // built in - two different sources for what is, at the plan
                // level, one shape.
                let kind = if probes_one_entry_each(
                    &source.table,
                    index_name,
                    *index_root,
                    *table_root,
                    branches,
                ) {
                    AccessKind::SeekUnion
                } else {
                    AccessKind::RangeUnion
                };
                push_stage(
                    &mut stages,
                    catalog,
                    *index_root,
                    kind,
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
                let is_the_table = *index_root == *table_root;
                if covering.is_none() && !is_the_table {
                    push_stage(
                        &mut stages,
                        catalog,
                        *table_root,
                        AccessKind::Nested,
                        source.id,
                        position,
                        true,
                        &mut offset,
                    )?;
                }
            }
            AccessPath::IndexSeek {
                table_root,
                index_root,
                covering,
                equalities,
                low,
                high,
                ..
            } => {
                // An index seek with no equality and no bound is a *scan* of
                // the index, not a range over it. The distinction is not
                // cosmetic: the covering rule and the skip-scan rule both key
                // on `Full`, and calling this a range left `scan.distinct`
                // reading every row of the index where SQLite seeks 64 times.
                let unbounded = equalities.is_empty() && low.is_none() && high.is_none();
                // The planner's *own* covering choice is subject to the same
                // rule the physical pass's covering rule is: reading fewer
                // bytes out of an index changes the order the rows reach an
                // aggregate in, and a floating-point sum is not associative.
                // `SELECT sum(score) FROM people` over `people_by_score` is
                // 0.0 where the table gives 124.25, because the corpus holds
                // `-1e300` and `+1e300` and the small values vanish between
                // them. SQLite reads the table here, and so must this.
                if unbounded && covering.is_some() && outermost && sensitive {
                    push_stage(
                        &mut stages,
                        catalog,
                        *table_root,
                        AccessKind::Full,
                        source.id,
                        position,
                        false,
                        &mut offset,
                    )?;
                    continue;
                }
                let kind = if outermost {
                    if plan.reverse {
                        AccessKind::Reverse
                    } else if unbounded {
                        AccessKind::Full
                    } else {
                        AccessKind::Span
                    }
                } else {
                    AccessKind::Nested
                };
                push_stage(
                    &mut stages,
                    catalog,
                    *index_root,
                    kind,
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
                // A `WITHOUT ROWID` table's primary-key index *is* the table:
                // one b-tree, reported at the table's own root page. So it
                // carries every column by construction and there is no rowid to
                // look anything up by - which is exactly what the lookup stage
                // below tried to do, five times over in the differential
                // corpus, with "the index entry carries no rowid".
                let is_the_table = *index_root == *table_root;
                if covering.is_none() && !is_the_table {
                    // The index does not carry every column the query reads, so
                    // the row is fetched from the table by rowid. That is the
                    // TDD's `RowidLookup`, expressed as what it is: a nested
                    // loop into the table tree keyed on the entry's rowid.
                    push_stage(
                        &mut stages,
                        catalog,
                        *table_root,
                        AccessKind::Nested,
                        source.id,
                        position,
                        true,
                        &mut offset,
                    )?;
                }
            }
            AccessPath::Subquery {
                width, correlated, ..
            } => {
                // An inner subquery is a nested loop over a materialised
                // buffer rather than over a tree, which is exactly what
                // `build_nested` builds for it. The rows are read once rather
                // than once per outer row: a derived table is a query with no
                // free variables, so re-running it would answer the same thing.
                if *correlated && outermost {
                    // A correlated subquery reads a FROM term outside itself,
                    // and the outermost term has nothing outside it - so this
                    // is a plan that should not exist rather than one to run.
                    return unsupported("a correlated subquery as the outermost term");
                }
                push_materialised(&mut stages, source.id, position, *width, &mut offset);
            }
            // A recursive CTE and the reference to the one being filled are
            // both *materialised* stages: the first is the fill loop's answer
            // and the second is the queue it is currently on, and neither is a
            // tree. `source_for` and `materialise_stage` produce the rows.
            AccessPath::Recursive { width, .. } => {
                push_materialised(&mut stages, source.id, position, *width, &mut offset);
            }
            AccessPath::RecursiveSelf { .. } => {
                let width = source.table.columns.len().max(1);
                push_materialised(&mut stages, source.id, position, width, &mut offset);
            }
            // A virtual table is a *materialised* stage: the module produces
            // its rows on the caller's side and the pipeline reads them, which
            // is the same shape a subquery already has.
            // The candidates come from the module, and the rows come out of
            // the table's own tree by rowid - so this is a table stage with an
            // unusual source rather than a materialised one.
            AccessPath::VectorProbe { root, .. } => {
                push_stage(
                    &mut stages,
                    catalog,
                    *root,
                    AccessKind::Vector,
                    source.id,
                    position,
                    false,
                    &mut offset,
                )?;
            }
            AccessPath::VirtualScan { .. } => {
                // A module is asked once, whether it is the outermost term or
                // an inner one: the plan it chose was chosen for one set of
                // constraints, and asking it again per outer row would be
                // asking a different question than the one it costed. As an
                // inner term its rows drive a `NestedLoopJoin`, like a
                // subquery's.
                let declared = source.table.columns.len().max(1);
                // **A module's row carries its rowid when the query asks for
                // one.** `SELECT rowid FROM t WHERE t MATCH ...` is the shape
                // every search adapter is written in - the rowid is the answer,
                // and the columns are what was searched - and it used to be
                // refused with "the tree read does not carry a rowid". The
                // module has always had it: `VirtualCursor::rowid` is on the
                // trait. It is appended after the declared columns rather than
                // put first, so every column keeps the slot it already had.
                let read = plan.select.columns_read(source.id);
                let carries_rowid = read.rowid;
                let functions = read.functions.clone();
                let width = declared
                    .saturating_add(usize::from(carries_rowid))
                    .saturating_add(functions.len());
                stages.push(PreparedStage {
                    functions: functions.clone(),
                    root: 0,
                    kind: AccessKind::Materialised,
                    source: source.id,
                    term: position,
                    is_lookup: false,
                    offset,
                    width,
                    // A module's row is its own record, exactly as a
                    // materialised subquery's is: slot `i` is column `i`, and
                    // nothing is known about the order.
                    layout: Some(std::rc::Rc::new(SourceLayout {
                        tree_key: 0,
                        slots: (0..declared).map(Some).collect(),
                        rowid: carries_rowid.then_some(declared),
                        // A module's rows are not a table's rows: there is
                        // nothing to probe a table with.
                        identity: Vec::new(),
                        types: vec![StaticType::Unknown; width],
                        width,
                        key_columns: Vec::new(),
                    })),
                });
                offset = offset.saturating_add(width);
            }
        }
    }
    Ok(stages)
}

/// Adds one stage and advances the column offset.
///
/// @param stages - the stages built so far
/// @param catalog - where the layouts come from
/// @param root - the tree this stage reads
/// @param kind - how it reads it
/// @param source - which planner FROM term it belongs to
/// @param is_lookup - whether it is the table fetch behind an index seek
/// @param offset - the next free column index, advanced
#[allow(clippy::too_many_arguments)]
/// Reports whether every branch of a seek union finds at most one entry.
///
/// **A point probe is only right when one entry per key is all there can be
/// (task-1932).** `PointProbe` finds the first entry with a key and stops,
/// which is what a rowid and a unique index guarantee and what no other index
/// does: on a non-unique one an equality is a *run* of entries, and probing it
/// answered one row of the run. `WHERE b IN (1, 2)` returned two rows where
/// `WHERE b = 1` alone returns twenty-one, and the pinned 3.53.4 answers
/// forty-two.
///
/// A branch that is not bare - one carrying a bound as well as its equalities -
/// is a range whatever the index guarantees, so it is not one entry either.
/// Everything this refuses goes to `RangeUnion`, which builds each branch as an
/// `IndexSeek` and takes its span: the same path a plain equality already
/// takes, so there is one definition of what an equality over an index means.
///
/// @param table - the table the union reads
/// @param index_name - the index the union seeks in
/// @param index_root - that index's tree
/// @param table_root - the table's own tree, which is the rowid case
/// @param branches - the union's branches
fn probes_one_entry_each(
    table: &TableInfo,
    index_name: &[u8],
    index_root: u32,
    table_root: u32,
    branches: &[inillucent_sql::plan::IndexSeekBranch],
) -> bool {
    let bare = branches
        .iter()
        .all(|branch| branch.low.is_none() && branch.high.is_none());
    let one_per_key = index_root == table_root
        || table
            .indexes
            .iter()
            .find(|held| held.name == *index_name)
            .is_some_and(|held| {
                held.unique
                    && branches
                        .iter()
                        .all(|branch| branch.equalities.len() >= held.columns.len())
            });
    bare && one_per_key
}

fn push_stage(
    stages: &mut Vec<PreparedStage>,
    catalog: &dyn TreeCatalog,
    root: u32,
    kind: AccessKind,
    source: usize,
    term: usize,
    is_lookup: bool,
    offset: &mut usize,
) -> DbResult<()> {
    let layout = catalog
        .layout(root)
        .ok_or_else(|| misuse(format!("no layout imported for root page {root}")))?;
    stages.push(PreparedStage {
        functions: Vec::new(),
        root,
        kind,
        source,
        term,
        is_lookup,
        offset: *offset,
        width: layout.width,
        layout: None,
    });
    *offset = offset.saturating_add(layout.width);
    Ok(())
}

/// Refuses the parts of a bound select the physical pass does not implement.
///
/// @param select - the bound statement
fn refuse_unhandled(select: &BoundSelect) -> DbResult<()> {
    // A window function is not refused here any more: `run_any` routes a
    // windowed query to `run_windowed` before a pipeline is prepared at all,
    // and a window that reached this point would be one nothing routed - which
    // is a bug in the dispatcher rather than a query the engine cannot answer.
    if !select.windows.is_empty() {
        return unsupported("a window function reaching the pipeline builder");
    }
    Ok(())
}

/// The column space one statement's stages define.
///
/// Every field is a borrow rather than an owned buffer. That is what lets a
/// [`Statement`] rebuild only its *source* on each execution: the space is
/// derived from the prepared stages and the catalog's layouts, neither of which
/// depends on the bound parameters, so it is computed once and viewed again
/// rather than rebuilt. When it owned its `types` and `layouts`, re-deriving it
/// per execution was three allocations that a re-run does not need.
pub(crate) struct Space<'c> {
    /// The stages, in order.
    pub(crate) stages: &'c [PreparedStage],
    /// Each stage's layout.
    pub(crate) layouts: &'c [std::rc::Rc<SourceLayout>],
    /// The static type of every column of the joined row.
    pub(crate) types: &'c [StaticType],
    /// The tree columns the *joined* rows arrive sorted by, when they do.
    pub(crate) order: &'c [usize],
    /// Where an application-registered function's body is looked up.
    ///
    /// `None` on the write path and on the two constant folds with no catalog in
    /// scope; a registered scalar there refuses by name - roadmap item 13.
    pub(crate) catalog: Option<&'c dyn TreeCatalog>,
    /// Which joined-row column each correlated subquery's answer sits in.
    ///
    /// Empty for every statement that has none, which is nearly all of them.
    /// A correlated block cannot be folded into a constant - it reads the row
    /// being tested - so `crate::correlate` computes it beside the row and this
    /// is the map an expression finds it through, exactly as a module's
    /// auxiliary functions are found.
    pub(crate) correlations: &'c [(usize, usize)],
}

impl Space<'_> {
    /// Returns the joined-row column a bound column reference names.
    ///
    /// A FROM term may be two stages, so the column is looked for in the table
    /// stage first and the index stage second: the table carries every column
    /// and the index only some, and preferring the table means a query that
    /// reads a column the index happens to hold still reads it from wherever
    /// the row was actually fetched.
    ///
    /// @param source - the planner FROM term
    /// @param declared - the column's declared position, which is what every
    ///   builder of a [`SourceLayout`] indexes its `slots` by
    /// Returns the joined-row column one correlated subquery's answer sits in.
    ///
    /// @param id - the binder's statement-wide number for the subquery
    fn correlated(&self, id: usize) -> Option<usize> {
        self.correlations
            .iter()
            .find(|(held, _)| *held == id)
            .map(|(_, column)| *column)
    }

    pub(crate) fn column(&self, source: usize, declared: usize) -> Option<usize> {
        let mut found = None;
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.source != source {
                continue;
            }
            let layout = self.layouts.get(index)?;
            if let Some(Some(tree_column)) = layout.slots.get(declared) {
                let resolved = stage.offset.saturating_add(*tree_column);
                if stage.is_lookup {
                    return Some(resolved);
                }
                found = Some(resolved);
            }
        }
        found
    }

    /// Returns the joined-row column holding a FROM term's rowid.
    ///
    /// @param source - the planner FROM term
    /// Returns the column one of a module's auxiliary functions was put in.
    ///
    /// @param source - the FROM term the call is about
    /// @param name - the function's folded name
    /// @param arguments - the arguments after the table, which are part of the
    ///   identity: two calls of one name with different arguments are two
    ///   answers and so two slots
    fn virtual_function(
        &self,
        source: usize,
        name: &[u8],
        arguments: &[inillucent_sql::bind::BoundExpr],
    ) -> Option<usize> {
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.source != source {
                continue;
            }
            let position = stage.functions.iter().position(|(held, held_arguments)| {
                held.as_slice() == name && held_arguments.as_slice() == arguments
            })?;
            let layout = self.layouts.get(index)?;
            // The functions sit after the declared columns and the rowid, in
            // the order the reads were met.
            let before = layout
                .slots
                .len()
                .saturating_add(usize::from(layout.rowid.is_some()));
            return Some(stage.offset.saturating_add(before).saturating_add(position));
        }
        None
    }

    pub(crate) fn rowid(&self, source: usize) -> Option<usize> {
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.source != source {
                continue;
            }
            let layout = self.layouts.get(index)?;
            if let Some(rowid) = layout.rowid {
                return Some(stage.offset.saturating_add(rowid));
            }
        }
        None
    }
}

/// Builds a pipeline for a planned statement.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
pub fn build<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<(Pipeline<'t>, Shape)> {
    let prepared = prepare(plan, catalog, ForcePlan::default())?;
    build_prepared(plan, catalog, &prepared, params, sink)
}

/// Builds a pipeline over already-chosen stages.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
pub fn build_prepared<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<(Pipeline<'t>, Shape)> {
    // Every uncorrelated subquery is answered once, here, before anything is
    // built over it. See `crate::subquery` for why it is per execution.
    let folded = crate::subquery::fold(plan, catalog, params)?;
    let params = folded.as_ref().unwrap_or(params);
    let held = space_of(catalog, prepared)?;
    let mut space = held.view(&prepared.stages);
    space.catalog = Some(catalog);
    let chain = build_chain(plan, catalog, prepared, &space, params, sink)?;
    let (source, description) = source_for(plan, catalog, &space, params, prepared, chain.limit)?;
    let mut operators = chain.operators;
    operators.push(description);
    operators.reverse();
    Ok((
        Pipeline {
            source,
            head: chain.head,
            pool: source_pool(catalog, prepared),
        },
        Shape {
            names: chain.names,
            operators,
        },
    ))
}

/// The layouts, types and key order a statement's stages define.
///
/// Held apart from [`Space`] because a [`Statement`] computes it once and takes
/// a view of it on every execution: none of it depends on the bound parameters,
/// so re-deriving it per execution would be three allocations spent to arrive
/// at the same answer.
pub(crate) struct HeldSpace {
    /// Each stage's layout.
    ///
    /// Owned rather than borrowed from the catalog, because a materialised
    /// subquery's layout is synthesised on its stage and there is nothing in the
    /// catalog to borrow it from - and a `Statement` owns both its `Prepared`
    /// and its space, which a borrow between them would make self-referential.
    /// It is built once per prepare and never per execution.
    pub(crate) layouts: Vec<std::rc::Rc<SourceLayout>>,
    /// The static type of every column of the joined row.
    pub(crate) types: Vec<StaticType>,
    /// The tree columns the joined rows arrive sorted by, when they do.
    pub(crate) order: Vec<usize>,
}

impl HeldSpace {
    /// Returns a view of this space over a statement's stages.
    ///
    /// @param stages - the prepared stages, outermost first
    pub(crate) fn view<'a>(&'a self, stages: &'a [PreparedStage]) -> Space<'a> {
        self.view_with(stages, &[])
    }

    /// Returns a view that also knows where the correlated answers sit.
    ///
    /// @param stages - the prepared stages, outermost first
    /// @param correlations - each block's number and the cell holding its answer
    pub(crate) fn view_with<'a>(
        &'a self,
        stages: &'a [PreparedStage],
        correlations: &'a [(usize, usize)],
    ) -> Space<'a> {
        Space {
            stages,
            layouts: &self.layouts,
            types: &self.types,
            order: &self.order,
            catalog: None,
            correlations,
        }
    }
}

/// Returns the column space a statement's stages define.
///
/// @param catalog - where the layouts come from
/// @param prepared - the structural choices [`prepare`] made
pub(crate) fn space_of(catalog: &dyn TreeCatalog, prepared: &Prepared) -> DbResult<HeldSpace> {
    let mut layouts = Vec::with_capacity(prepared.stages.len());
    let mut types: Vec<StaticType> = Vec::new();
    for stage in &prepared.stages {
        let layout = match &stage.layout {
            Some(held) => held,
            None => catalog.layout(stage.root).ok_or_else(|| {
                misuse(format!("no layout imported for root page {}", stage.root))
            })?,
        }
        .clone();
        types.extend(layout.types.iter().copied());
        layouts.push(layout);
    }
    // Only the outermost stage's key order survives into the joined row: a
    // nested loop emits its inner matches grouped by the outer row, which
    // preserves the outer order and destroys any inner one.
    //
    // **A table fetch behind a non-covering index seek is not a nested loop.**
    // It is one row per index entry, in the index's own order,
    // so it preserves the order rather than destroying it. This used to ask for
    // exactly one stage, which a non-covering seek never is - so the ordering an
    // index was chosen *for* was then not believed, `ORDER BY` fell to a `TopN`,
    // and `TopN` is a pipeline breaker: it consumes every row of the range
    // before it emits one. The measured shape is unmistakable, because the cost
    // falls as the starting key advances - on a 60,000-row table, the same
    // `WHERE id > ? ORDER BY id LIMIT 2000`:
    //
    // | starting after | before | after |
    // |---|---:|---:|
    // | row 1 | 88.3 ms | 1.1 ms |
    // | row 20,000 | 19.6 ms | 1.1 ms |
    // | row 40,000 | 11.0 ms | 1.1 ms |
    // | row 58,000 | 3.7 ms | 1.1 ms |
    //
    // Work proportional to what is *left* rather than to the limit, which makes
    // keyset paging quadratic in the table: 601,862 chunks at a page of 2,000 is
    // 90 million row materialisations instead of 601,862. It is what stopped
    // an early `inillucent migrate` run from finishing one table in 25 minutes.
    let ordered_stages = prepared.stages.iter().skip(1).all(|stage| stage.is_lookup);
    let order = match (prepared.stages.first(), layouts.first()) {
        (Some(stage), Some(layout)) if ordered_stages => {
            if stage.kind == AccessKind::Reverse {
                Vec::new()
            } else {
                layout.key_columns.clone()
            }
        }
        _ => Vec::new(),
    };
    Ok(HeldSpace {
        layouts,
        types,
        order,
    })
}

/// Everything a built operator chain is, short of the source that drives it.
struct Chain<'t> {
    /// The head of the chain: what the source pushes into.
    head: Box<dyn Sink + 't>,
    /// The operator descriptions, sink first; the source is appended last.
    operators: Vec<String>,
    /// The output column names.
    names: Vec<Vec<u8>>,
    /// The statement's constant `LIMIT`, which the source may use.
    limit: Option<usize>,
}

/// Builds every operator above the source.
///
/// Separated from [`build_prepared`] because a [`Statement`] builds this once
/// and rebuilds only the source per execution. The split is also what makes the
/// rebinding test possible: the parameter reads this function makes are the
/// ones that would be baked into the chain, and a statement is only re-runnable
/// when there are none.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
/// Everything [`build_upper`] built: the source-independent half of a chain.
///
/// Kept apart from [`Chain`] because every field here is genuinely `'static` -
/// which is what [`Compiled`] needs. [`Statement`] widens this into a chain
/// with the inner stages and any correlated block wrapped around it, which is
/// where a borrow of the catalog first appears.
pub(crate) struct Upper {
    /// Every operator above the source, holding no borrow of anything.
    pub(crate) head: Box<dyn Sink>,
    /// The operator descriptions, sink first.
    pub(crate) operators: Vec<String>,
    /// The output column names.
    pub(crate) names: Vec<Vec<u8>>,
    /// The statement's constant `LIMIT`, which the source may use.
    pub(crate) limit: Option<usize>,
    /// The statement's correlated blocks, prepared but not yet wrapped around
    /// `head` - building [`crate::correlate::Correlated`] needs a catalog
    /// borrowed for the chain's own lifetime, which is exactly what this
    /// function does not take.
    pub(crate) correlations: Vec<crate::correlate::Correlation>,
}

/// Builds every operator above the source, short of the inner join stages and
/// the correlation operator - the part of a chain that holds no borrow of the
/// catalog it was built against.
///
/// Split out of [`build_chain`] so [`Compiled`] - kept with no lifetime at all
/// so it can sit in an `Rc` across executions - can build this part once.
/// `catalog` is borrowed only long enough to resolve a function to its body
/// and translate a residual predicate; an index nested loop, a correlated
/// block or a lateral module - every place that would hold onto the borrow -
/// is built by [`build_chain`] instead, over what this returns.
///
/// @param plan - the planner's output
/// @param catalog - where a registered function's body comes from, borrowed
///   only for this call
/// @param prepared - the structural choices [`prepare`] made
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
pub(crate) fn build_upper(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    prepared: &Prepared,
    space: &Space<'_>,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<Upper> {
    let select = &plan.select;
    refuse_unhandled(select)?;
    // **A correlated block is answered beside the row, not inside an
    // expression.** Each one becomes a column appended to the joined row, and
    // `crate::correlate` is the operator that fills it - so the `WHERE` and the
    // projection read a column rather than reaching for a catalog that
    // `expr::Eval`'s `Send + Sync` bound puts out of reach. `correlations_of`
    // returns nothing for a statement with none, which is nearly all of them,
    // and the operator is then never built.
    let outer = Space {
        stages: space.stages,
        layouts: space.layouts,
        types: space.types,
        order: space.order,
        catalog: Some(catalog),
        correlations: &[],
    };
    let correlations = crate::correlate::correlations_of(plan, &|expr: &BoundExpr| match expr {
        BoundExpr::Column { source, column, .. } => outer.column(*source, *column as usize),
        BoundExpr::Rowid { source } => outer.rowid(*source),
        _ => None,
    })?;
    let joined_width = space.types.len();
    let correlation_columns: Vec<(usize, usize)> = correlations
        .iter()
        .enumerate()
        .map(|(position, correlation)| (correlation.id, joined_width.saturating_add(position)))
        .collect();
    // **Only widened when there is something to widen.** A correlated block adds
    // a column and every other statement adds none, so the common case borrows
    // the space's own types rather than copying them - `prepare.trivial` is
    // 1,337 ns end to end and a `Vec` per prepare is a measurable share of it.
    let widened_types: Vec<StaticType> = if correlations.is_empty() {
        Vec::new()
    } else {
        let mut widened = space.types.to_vec();
        widened.extend(std::iter::repeat_n(StaticType::Unknown, correlations.len()));
        widened
    };
    let scan_types: &[StaticType] = if correlations.is_empty() {
        space.types
    } else {
        &widened_types
    };
    let space = &Space {
        stages: space.stages,
        layouts: space.layouts,
        types: scan_types,
        order: space.order,
        catalog: Some(catalog),
        correlations: &correlation_columns,
    };
    let group_width = select.group_by.len();
    let skipping = prepared
        .stages
        .first()
        .map(|stage| stage.kind == AccessKind::Skip)
        .unwrap_or(false);

    // Result columns and ORDER BY terms, in the space that exists after any
    // aggregation. Terms that are not already result columns are carried
    // through the sort as extra columns and trimmed afterwards.
    let mut projected: Vec<Expr> = Vec::with_capacity(select.columns.len());
    for column in &select.columns {
        projected.push(translate_post(
            &column.expr,
            select,
            space,
            params,
            group_width,
        )?);
    }
    let result_width = projected.len();
    let mut sort_keys: Vec<SortKey> = Vec::new();
    for term in &select.order_by {
        let translated = translate_post(&term.expr, select, space, params, group_width)?;
        let existing = projected
            .iter()
            .position(|held| same_expr(held, &translated));
        let column = match existing {
            Some(index) => index,
            None => {
                // **Carried through the sort, and left out of what makes a row
                // distinct.** `SELECT DISTINCT a FROM t ORDER BY b` is an
                // ordinary query SQLite answers; refusing it was the safe thing
                // to do while the de-duplication compared every column of the
                // row, because the carried `b` would have made rows distinct
                // that the caller's select list does not. `Distinct::over`
                // compares the leading `result_width` columns instead.
                projected.push(translated);
                projected.len().saturating_sub(1)
            }
        };
        let descending = term.order == SortOrder::Descending;
        sort_keys.push(SortKey {
            column,
            descending,
            collation: term.collation,
            // SQLite's default is NULLS FIRST ascending and NULLS LAST
            // descending, which is what reversing an ordering that puts NULL
            // lowest already gives. An explicit clause is the case that has to
            // be carried, and the binder has already resolved the default.
            nulls_first: match term.nulls {
                NullOrder::First => true,
                NullOrder::Last => false,
            },
        });
    }
    let needs_trim = projected.len() > result_width;

    let scan_order = &order_equivalents(space.stages, space.layouts, space.order);
    let group_exprs = select
        .group_by
        .iter()
        .map(|expr| translate_scan(expr, space, params))
        .collect::<DbResult<Vec<Expr>>>()?;
    // `GROUP BY team` on a `COLLATE NOCASE` column has one group for `blue`
    // and `Blue`; grouping by bytes has two, and the counts are then wrong
    // rather than merely differently ordered.
    let group_collations: Vec<Collation> =
        select.group_by.iter().map(expression_collation).collect();
    // Whether the projected rows arrive in the order the ORDER BY asks for.
    let reversed = prepared
        .stages
        .first()
        .map(|stage| stage.kind == AccessKind::Reverse)
        .unwrap_or(false);
    // **Adjacency has no direction, and `space.order` deliberately does.** A
    // reverse walk brings each group's rows together exactly as a forward one
    // does, but `space_of` empties `order` for a reverse scan - correctly, since
    // the rows arrive in the *reverse* of that order and no rule reading it may
    // assume otherwise. Asking `is_scan_prefix` alone therefore said "not
    // grouped by the walk", the aggregate became a hash one, and it emitted its
    // groups in key order: `SELECT k, count(*) FROM t GROUP BY k ORDER BY k
    // DESC` came back *ascending*, with the planner having already skipped the
    // sorter because the walk was supposed to answer the ordering.
    //
    // So the adjacency question is asked of the planner for a reverse walk,
    // which decided it from the access path rather than from the direction.
    let grouped_walk = plan.aggregation == AggregationMode::Grouped
        && !prepared.forced.hash_group
        && (is_scan_prefix(&group_exprs, scan_order) || (reversed && plan.grouped_walk));
    // A non-default NULL placement is a real ordering requirement, and no scan
    // order satisfies it by accident.
    let default_nulls = sort_keys
        .iter()
        .all(|term| term.nulls_first != term.descending);
    let sorted_already = if !default_nulls {
        false
    } else if reversed {
        // A reverse scan produces descending key order, so a descending
        // ORDER BY over the key is satisfied by the direction rather than by a
        // sorter. `plan.reverse` is only ever set when the planner already
        // decided that, which is why the condition is the planner's answer
        // rather than a second derivation of it.
        !sort_keys.is_empty() && !plan.needs_sort
    } else {
        !sort_keys.is_empty()
            && sort_keys.iter().all(|term| !term.descending)
            && output_is_sorted_by(&sort_keys, &projected, plan, scan_order, grouped_walk)
    };
    // **A skip scan produces the distinct prefix in *ascending* order**, which
    // answers an ascending `ORDER BY` over that prefix and nothing else. This
    // line used to say only "skipping", and `SELECT k, count(*) FROM t GROUP BY
    // k ORDER BY k DESC` therefore skipped its sorter and came back ascending -
    // a wrong answer rather than a slow one, and one no single-direction test
    // could see. The same two conditions the forward branch above applies are
    // applied here, because it is the same claim about the same walk.
    // **A skip scan produces the distinct prefix in *ascending* order**, which
    // answers an ascending `ORDER BY` over that prefix and nothing else. This
    // line used to say only "skipping", and `SELECT k, count(*) FROM t GROUP BY
    // k ORDER BY k DESC` therefore skipped its sorter and came back ascending -
    // a wrong answer rather than a slow one, and one no single-direction test
    // could see. The same two conditions the forward branch above applies are
    // applied here, because it is the same claim about the same walk.
    let sorted_already = sorted_already
        || (skipping
            && !sort_keys.is_empty()
            && sort_keys.iter().all(|term| !term.descending)
            && output_is_sorted_by(&sort_keys, &projected, plan, scan_order, grouped_walk));

    // Built bottom-up, because each operator owns the one below it. The
    // description is collected in the same order and reversed at the end, so it
    // reads source-first the way a plan should.
    let mut operators: Vec<String> = Vec::new();
    let mut chain: Box<dyn Sink> = sink;

    let limit = constant_limit(select, params)?;
    let offset = constant_offset(select, params)?.unwrap_or(0);
    // **What the *source* may stop after, which is not the statement's LIMIT.**
    // A source that stops early is only right when nothing between it and the
    // `Limit` operator changes how many rows there are: a residual filter drops
    // some, a join multiplies them, `DISTINCT` and an aggregate collapse them,
    // and an `OFFSET` throws the first ones away - so `LIMIT 2 OFFSET 1` needs
    // three rows read and returned one.
    //
    // It was the bare `LIMIT`, which made `WHERE id <= 5 ORDER BY id DESC LIMIT
    // 2 OFFSET 1` answer one row instead of two.
    let source_limit = limit.filter(|_| {
        plan.residuals.iter().all(Option::is_none)
            && plan.constant_filter.is_none()
            && prepared.stages.len() == 1
            && !select.distinct
            && plan.aggregation == AggregationMode::None
            && select.windows.is_empty()
    });
    if sort_keys.is_empty() || sorted_already {
        if let Some(limit) = limit {
            chain = Box::new(Limit::new(limit, offset, chain));
            operators.push(format!("LIMIT {limit} OFFSET {offset}"));
        }
        if needs_trim {
            chain = Box::new(Project::new(trim(result_width, scan_types)?, chain));
            operators.push("TRIM".to_string());
        }
    } else if let Some(limit) = limit {
        if needs_trim {
            chain = Box::new(Project::new(trim(result_width, scan_types)?, chain));
            operators.push("TRIM".to_string());
        }
        let bounded = limit.saturating_add(offset);
        if bounded <= TopN::MAX_LIMIT && !prepared.forced.full_sort {
            if offset > 0 {
                chain = Box::new(Limit::new(limit, offset, chain));
                operators.push(format!("LIMIT {limit} OFFSET {offset}"));
            }
            chain = Box::new(TopN::new(sort_keys.clone(), bounded, chain));
            operators.push(format!("TOP {bounded}"));
        } else {
            chain = Box::new(Limit::new(limit, offset, chain));
            chain = Box::new(Sort::new(sort_keys.clone(), chain));
            operators.push(format!("LIMIT {limit} OFFSET {offset}"));
            operators.push("SORT".to_string());
        }
    } else {
        if needs_trim {
            chain = Box::new(Project::new(trim(result_width, scan_types)?, chain));
            operators.push("TRIM".to_string());
        }
        chain = Box::new(Sort::new(sort_keys.clone(), chain));
        operators.push("SORT".to_string());
    }

    // The collation of each output column, for `DISTINCT`. A `DISTINCT` over a
    // `COLLATE NOCASE` column keeps one of `blue` and `Blue`, and one that
    // compared bytes keeps both.
    let output_collations: Vec<Collation> = select
        .columns
        .iter()
        .map(|column| expression_collation(&column.expr))
        .collect();
    if select.distinct && !skipping {
        if plan.aggregation == AggregationMode::None
            && !prepared.forced.hash_distinct
            && is_scan_prefix(&projected, scan_order)
        {
            chain = Box::new(AdjacentDistinct::over(
                output_collations.clone(),
                result_width,
                chain,
            ));
            operators.push("DISTINCT ADJACENT".to_string());
        } else {
            chain = Box::new(Distinct::over(
                output_collations.clone(),
                result_width,
                chain,
            ));
            operators.push("DISTINCT HASH".to_string());
        }
    }

    let projection_input_types = if plan.aggregation == AggregationMode::None {
        scan_types.to_vec()
    } else {
        aggregate_output_types(select, space, params)?
    };
    // A skip scan hands up exactly the projected key columns, already in
    // output order, so the projection over it reads column i for column i.
    let projected = if skipping {
        (0..projected.len()).map(Expr::Column).collect()
    } else {
        projected
    };
    let compiled_projection = projected
        .iter()
        .map(|expr| compile(expr, &projection_input_types))
        .collect::<DbResult<Vec<_>>>()?;
    chain = Box::new(Project::new(compiled_projection, chain));
    operators.push("PROJECT".to_string());

    // `HAVING` filters *groups*, so it sits between the aggregate and the
    // projection: it reads accumulators and `GROUP BY` keys, which is the same
    // space a result column reads, and it runs before the projection throws
    // away the columns it needs. Building it here rather than beside the
    // `WHERE` filters is the whole of the difference between the two clauses.
    if let Some(having) = &select.having {
        let translated = translate_post(having, select, space, params, group_width)?;
        chain = Box::new(Filter::new(
            compile(&translated, &projection_input_types)?,
            chain,
        ));
        operators.push("FILTER HAVING".to_string());
    }

    match plan.aggregation {
        AggregationMode::None => {}
        AggregationMode::Whole => {
            chain = Box::new(SimpleAggregate::new(
                aggregate_specs(select, space, params, scan_types)?,
                chain,
            ));
            operators.push("AGGREGATE".to_string());
        }
        AggregationMode::Grouped => {
            let keys = group_exprs
                .iter()
                .map(|expr| compile(expr, scan_types))
                .collect::<DbResult<Vec<_>>>()?;
            let specs = aggregate_specs(select, space, params, scan_types)?;
            chain = if grouped_walk {
                operators.push("GROUP STREAM".to_string());
                Box::new(StreamAggregate::new(
                    keys,
                    group_collations.clone(),
                    specs,
                    chain,
                ))
            } else {
                operators.push("GROUP HASH".to_string());
                Box::new(HashAggregate::new(
                    keys,
                    group_collations.clone(),
                    specs,
                    chain,
                ))
            };
        }
    }

    // `select.filter` is the *whole* `WHERE`, and `plan.residuals` is what the
    // access paths did not consume. Testing both re-tests every predicate the
    // planner turned into a seek or a range - `WHERE key BETWEEN ?1 AND ?1+200`
    // was evaluated once per row of a range whose bounds already excluded
    // everything outside it - so only the residuals are tested here. That is
    // also what the bytecode VM does, and it is not merely a speed question: a
    // predicate with `random()` in it would answer differently the second time.
    //
    // The operator chain in `Shape::operators` is what showed this: it printed
    // `RANGE tree 3 -> FILTER -> AGGREGATE` and the `FILTER` had nothing to do.
    if let Some(constant) = &plan.constant_filter {
        let translated = translate_scan(constant, space, params)?;
        chain = Box::new(Filter::new(compile(&translated, scan_types)?, chain));
        operators.push("FILTER CONSTANT".to_string());
    }
    for residual in plan.residuals.iter().flatten() {
        let translated = translate_scan(residual, space, params)?;
        chain = Box::new(Filter::new(compile(&translated, scan_types)?, chain));
        operators.push("FILTER RESIDUAL".to_string());
    }

    let names = select
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();

    Ok(Upper {
        head: chain,
        operators,
        names,
        limit: source_limit.map(|limit| limit.saturating_add(offset)),
        correlations,
    })
}

/// Builds every operator above the source.
///
/// Separated from [`build_prepared`] because a [`Statement`] builds this once
/// and rebuilds only the source per execution. The split is also what makes
/// the rebinding test possible: the parameter reads this function makes are
/// the ones baked into the chain, and a statement is only re-runnable when
/// there are none.
///
/// Everything that holds no borrow of `catalog` is [`build_upper`]'s to
/// build; this adds the two things that do - the correlation operator and the
/// inner join stages - which is where the chain widens from `'static` to `'t`.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
/// @param sink - the end of the pipeline
fn build_chain<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    space: &Space<'_>,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<Chain<'t>> {
    let upper = build_upper(plan, catalog, prepared, space, params, sink)?;
    let mut operators = upper.operators;
    // The inner stages, innermost first, so each ends up above the one before
    // it in the chain the source pushes into. The chain widens from `'static`
    // to `'t` here and only here: an index nested loop borrows its inner tree,
    // and it wraps everything built so far rather than being wrapped by it.
    let mut chain: Box<dyn Sink + 't> = upper.head;
    // The correlation operator goes *below* every join and *above* every
    // filter: the value it computes reads the whole joined row, and the `WHERE`
    // that tests it runs after the last join has widened that row.
    if !upper.correlations.is_empty() {
        operators.push("CORRELATED SUBQUERY".to_string());
        chain = Box::new(crate::correlate::Correlated::new(
            upper.correlations,
            catalog,
            params,
            chain,
        ));
    }
    for index in (1..prepared.stages.len()).rev() {
        let stage = prepared
            .stages
            .get(index)
            .ok_or_else(|| misuse("a stage vanished while building"))?;
        chain = build_nested(plan, catalog, space, params, stage, index, chain)?;
        operators.push(format!(
            "{} tree {}{}",
            stage.kind.describe(),
            stage.root,
            if stage.is_lookup {
                " (rowid lookup)"
            } else {
                ""
            }
        ));
    }

    Ok(Chain {
        head: chain,
        operators,
        names: upper.names,
        limit: upper.limit,
    })
}

/// A prepared statement: an operator chain built once and run many times.
///
/// **This is the difference between preparing a plan and preparing a
/// statement, and the gate was measuring the first while calling it the
/// second.** A scorecard workload with `prepare_each: false` binds new
/// parameters and runs again; SQLite's arm answers that with
/// `sqlite3_reset`, `sqlite3_bind_*` and `sqlite3_step` over a VDBE program it
/// compiled once. Ours re-translated every projected expression, re-boxed every
/// operator and re-formatted the plan description on each execution, and
/// `inillucent-probeprofile` measured that at 0.52 us against a 0.70 us
/// `point.rowid` - 42% of the workload, and 71% of `point.miss`.
///
/// So a `Statement` holds the chain and rebuilds only the *source*, whose key
/// or bounds are the one part of a plan that the parameters decide. Between
/// executions the chain is [`Sink::reset`]: every accumulator, sorter,
/// hash table and limit counter returns to its pre-input state.
///
/// ## Why a statement can refuse to be re-run
///
/// A parameter that reaches anything *other* than the source - `LIMIT ?1`, a
/// projected `?2`, a residual filter - is folded into the chain when the chain
/// is built, and re-running that chain against new values would answer the old
/// question. [`Statement::rebindable`] says whether that happened, and it is
/// decided by counting the parameter reads the chain's construction made rather
/// than by a second opinion about which constructs may carry one.
pub struct Statement<'t> {
    /// The planner's output, which the source is rebuilt from.
    plan: &'t PhysicalPlan,
    /// Where the trees and layouts come from.
    catalog: &'t dyn TreeCatalog,
    /// The structural choices, owned so the statement is self-contained.
    prepared: Prepared,
    /// The layouts and types, computed once.
    held: HeldSpace,
    /// The operator chain, built once.
    head: Box<dyn Sink + 't>,
    /// The pool the source's pages live in, when the source reads a tree.
    pool: Option<&'t Pool>,
    /// The statement's constant `LIMIT`, which the source may use.
    limit: Option<usize>,
    /// What the statement produces.
    shape: Shape,
    /// Whether anything but the source read a parameter while building.
    rebindable: bool,
    /// The cell every `Expr::Parameter` in the chain reads.
    ///
    /// **The chain holds the cell it was built with, and the caller hands a
    /// different `Params` to every execution**, so the two have to be joined up
    /// before the chain runs. Leaving this out is not a slow statement, it is a
    /// wrong answer: `SELECT category, count(*) FROM t WHERE id >= ?1 GROUP BY
    /// category` answered its *first* execution's question on every later one,
    /// and `a_reused_statement_answers_what_a_rebuilt_pipeline_does` is the test
    /// that said so.
    bindings: Bindings,
}

impl<'t> Statement<'t> {
    /// Reports whether this statement may be run again with new parameters.
    pub fn rebindable(&self) -> bool {
        self.rebindable
    }

    /// Returns what the statement produces.
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    /// Runs the statement against one parameter set.
    ///
    /// **Folds this execution's uncorrelated subqueries first, every time.**
    /// The chain was folded once at [`build_statement`] time, which is correct
    /// for anything baked into the chain - a folded value read there is
    /// counted against [`Statement::rebindable`]. It is *not* correct for the
    /// **source**: a seek key from `WHERE id = (SELECT max(id) FROM t)` calls
    /// [`source_for_run`] on every run, which used to see the raw `params` this
    /// method was handed - subquery slots empty, nothing having folded them
    /// since the one-time pass - and answered "a correlated subquery used as a
    /// value" for a block that was never correlated. Folding costs about 40 ns
    /// and no allocation on the ordinary statement, which has none.
    ///
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn run(&mut self, params: &Params) -> DbResult<()> {
        if !self.rebindable {
            return Err(misuse(
                "this statement folded a parameter into its operator chain and cannot be re-run                  against different values",
            ));
        }
        let folded = crate::subquery::fold(self.plan, self.catalog, params)?;
        let params = folded.as_ref().unwrap_or(params);
        // The chain reads the cell it was built with; this is where that cell
        // learns what this execution bound. See `Statement::bindings`.
        let source = params.bindings();
        if !std::sync::Arc::ptr_eq(&self.bindings, &source) {
            if let (Ok(from), Ok(mut held)) = (source.lock(), self.bindings.lock()) {
                held.clear();
                held.extend_from_slice(&from);
            }
        }
        let source = {
            let mut space = self.held.view(&self.prepared.stages);
            space.catalog = Some(self.catalog);
            source_for_run(
                self.plan,
                self.catalog,
                &space,
                params,
                &self.prepared,
                self.limit,
            )?
        };
        self.head.reset()?;
        source.run(self.pool, self.head.as_mut())
    }
}

/// Builds a statement that can be run many times.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param params - the values the first execution binds
/// @param sink - the end of the pipeline, which the statement keeps
pub fn build_statement<'t>(
    plan: &'t PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
    sink: Box<dyn Sink>,
) -> DbResult<Statement<'t>> {
    // Every uncorrelated subquery is answered once, here, before anything is
    // built over it. See `crate::subquery` for why it is per execution.
    let folded = crate::subquery::fold(plan, catalog, params)?;
    let params = folded.as_ref().unwrap_or(params);
    let prepared = prepared.clone();
    let held = space_of(catalog, &prepared)?;
    // The reads the chain makes are the parameters it bakes in. The source's
    // are made after this window closes and are recomputed on every execution,
    // so they do not count against re-running.
    let before = params.reads();
    let chain = {
        let mut space = held.view(&prepared.stages);
        space.catalog = Some(catalog);
        build_chain(plan, catalog, &prepared, &space, params, sink)?
    };
    let rebindable = params.reads() == before;
    let bindings = params.bindings();
    let mut operators = chain.operators;
    operators.push(describe_source(&prepared));
    operators.reverse();
    let names = chain.names;
    let pool = source_pool(catalog, &prepared);
    Ok(Statement {
        bindings,
        plan,
        catalog,
        prepared,
        held,
        head: chain.head,
        pool,
        limit: chain.limit,
        shape: Shape { names, operators },
        rebindable,
    })
}

/// Returns the pool the source stage's tree lives in, when it reads one.
///
/// **The source is a stage, so its pool travels with it like every other
/// stage's.** A pipeline has exactly one source and therefore exactly one
/// source pool; every other stage that touches a tree - the inner side of an
/// index nested loop, a materialised subquery - asks for its own.
///
/// @param catalog - where the trees and their pools come from
/// @param prepared - the structural choices `prepare` made
pub(crate) fn source_pool<'t>(
    catalog: &'t dyn TreeCatalog,
    prepared: &Prepared,
) -> Option<&'t Pool> {
    catalog.pool_for(prepared.stages.first()?.root)
}

/// Returns what drives a pipeline, and the line `EXPLAIN` prints for it.
///
/// The one place that decides, so the three callers - a one-shot run, a reused
/// statement's rebuild, and a statement's construction - cannot disagree about a
/// plan with no stages.
///
/// @param plan - the planner's output
/// @param catalog - where the trees come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param prepared - the structural choices `prepare` made
/// @param limit - the statement's `LIMIT`, when it has a constant one
fn source_for<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    prepared: &Prepared,
    limit: Option<usize>,
) -> DbResult<(Source<'t>, String)> {
    let source = source_for_run(plan, catalog, space, params, prepared, limit)?;
    Ok((source, describe_source(prepared)))
}

/// Returns what drives a pipeline, without the `EXPLAIN` line.
///
/// The same three shapes [`source_for`] builds, for a caller that would
/// otherwise format and throw away a `String` every execution - which is
/// exactly what [`Statement::run`] used to do. `source_for` is this plus
/// [`describe_source`], so the two answers about what a plan with no stages
/// drives cannot drift apart.
///
/// @param plan - the planner's output
/// @param catalog - where the trees come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param prepared - the structural choices `prepare` made
/// @param limit - the statement's `LIMIT`, when it has a constant one
pub(crate) fn source_for_run<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    prepared: &Prepared,
    limit: Option<usize>,
) -> DbResult<Source<'t>> {
    match prepared.stages.first() {
        // A materialised subquery: the inner pipeline runs to completion into a
        // buffer, and the buffer drives the outer one. It is built here rather
        // than in `build_source` because it needs the plan and the catalog
        // rather than a tree.
        Some(stage) if stage.kind == AccessKind::Materialised => {
            let term = plan
                .sources
                .get(stage.term)
                .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
            if let AccessPath::VirtualScan { .. } = &term.path {
                // **The module is only asked for the columns the query reads.**
                // A materialised virtual scan used to ask the cursor for every
                // declared column of every row, so `SELECT count(*) FROM t
                // WHERE t MATCH 'x'` read the content row and scored the rank
                // column for five hundred rows it then counted. This is the
                // same question a covering index is chosen by, asked of the
                // same bound statement, so a column that is read is a column
                // that is materialised.
                let needed = plan.select.columns_read(term.id);
                return Ok(Source::Virtual(Box::new(VirtualScanSource {
                    catalog,
                    table: term.table.clone(),
                    path: term.path.clone(),
                    params: params.clone(),
                    needed,
                })));
            }
            let rows = materialise_stage(plan, catalog, params, stage, limit)?;
            Ok(Source::Rows(rows))
        }
        Some(stage) => build_source(plan, catalog, space, params, stage, limit),
        // A `VALUES` arm has no FROM term either, and its rows *are* its
        // answer: every expression is a constant, so they are evaluated once
        // here rather than projected out of an empty row.
        None if !plan.select.values.is_empty() => {
            let empty = Space {
                stages: &[],
                layouts: &[],
                types: &[],
                order: &[],
                catalog: None,
                correlations: &[],
            };
            let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(plan.select.values.len());
            for row in &plan.select.values {
                let mut out = Vec::with_capacity(row.len());
                for expr in row {
                    out.push(constant_value(expr, &empty, params, None)?);
                }
                rows.push(out);
            }
            Ok(Source::Rows(rows))
        }
        // A query with no FROM term: one row of no columns, and the whole
        // answer comes out of the projection.
        None => Ok(Source::Constant(1)),
    }
}

/// Pushes one stage whose rows the caller produces rather than a tree.
///
/// A derived table, a recursive CTE, the queue that CTE is being filled from,
/// and a virtual table's rows are all this shape: the pipeline reads a buffer,
/// not pages. A materialised row is its own record - slot `i` is column `i`,
/// there is no rowid, and nothing is known about the order, so no streaming
/// rule may assume one.
///
/// @param stages - the stages built so far
/// @param source - the binder's number for the FROM term
/// @param term - the term's position in the plan's own arrays
/// @param width - how many columns a row holds
/// @param offset - the first joined-row column this stage fills, advanced here
fn push_materialised(
    stages: &mut Vec<PreparedStage>,
    source: usize,
    term: usize,
    width: usize,
    offset: &mut usize,
) {
    stages.push(PreparedStage {
        functions: Vec::new(),
        root: 0,
        kind: AccessKind::Materialised,
        source,
        term,
        is_lookup: false,
        offset: *offset,
        width,
        layout: Some(std::rc::Rc::new(SourceLayout {
            tree_key: 0,
            slots: (0..width).map(Some).collect(),
            rowid: None,
            // Rows read once into a buffer: a derived table, a recursive CTE.
            // None of them identifies a stored row to probe a table with.
            identity: Vec::new(),
            types: vec![StaticType::Unknown; width],
            width,
            key_columns: Vec::new(),
        })),
    });
    *offset = offset.saturating_add(width);
}

/// Returns the `EXPLAIN` line for whatever drives a plan.
///
/// @param prepared - the structural choices `prepare` made
pub(crate) fn describe_source(prepared: &Prepared) -> String {
    match prepared.stages.first() {
        Some(stage) => format!("{} tree {}", stage.kind.describe(), stage.root),
        None => "SCAN CONSTANT ROW".to_string(),
    }
}

/// Builds the driving source for the outermost stage.
///
/// @param plan - the planner's output
/// @param catalog - where the trees come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the outermost stage
/// @param limit - the statement's `LIMIT`, when it has a constant one
fn build_source<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    limit: Option<usize>,
) -> DbResult<Source<'t>> {
    let tree = catalog
        .tree(stage.root)
        .ok_or_else(|| misuse(format!("no tree imported for root page {}", stage.root)))?;
    let projection = Projection::all(stage.width);
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    let path = &source_term.path;
    let table = &source_term.table;
    match stage.kind {
        AccessKind::Full => Ok(Source::Scan(FullScan::new(tree, projection))),
        AccessKind::Skip => {
            let prefix = space
                .order
                .len()
                .min(projected_prefix(plan, space, params)?);
            Ok(Source::Skip(SkipScan::new(tree, prefix.max(1))))
        }
        AccessKind::Point => {
            let key = point_key(path, space, params)?;
            Ok(Source::Point(PointProbe::new(tree, projection), key))
        }
        AccessKind::Span => {
            let bounds = span_bounds(path, table, space, params)?;
            Ok(Source::Span(SpanScan::new(
                tree,
                projection,
                bounds.low,
                bounds.low_inclusive,
                bounds.high,
                bounds.high_inclusive,
            )))
        }
        AccessKind::Reverse => {
            let bounds = span_bounds(path, table, space, params)?;
            Ok(Source::Reverse(ReverseScan::new(
                tree, projection, bounds, limit,
            )))
        }
        AccessKind::Vector => {
            let AccessPath::VectorProbe {
                index,
                probe,
                depth,
                ..
            } = path
            else {
                return Err(misuse("a vector stage over a path that is not one"));
            };
            // The catalog goes in because the probe vector is very often
            // `embed('search_query: ...')` - a registered function, whose body
            // only this can resolve. See `literal_value_in`.
            let wanted = literal_value_in(probe, params, Some(catalog))?;
            let probe_over = PointProbe::new(tree, projection);
            let keys = iterative_candidates(
                plan,
                catalog,
                space,
                params,
                stage,
                index,
                &wanted.borrow(),
                *depth,
                limit,
                &probe_over,
            )?;
            Ok(Source::Vector(probe_over, keys))
        }
        AccessKind::SeekUnion => {
            let probe_over = PointProbe::new(tree, projection);
            let keys = match path {
                AccessPath::RowidSeekUnion { keys, .. } => rowid_union_keys(keys, space, params)?,
                AccessPath::IndexSeekUnion {
                    branches, columns, ..
                } => index_union_keys(branches, table, columns, space, params)?,
                _ => return Err(misuse("a seek-union stage over a path that is not one")),
            };
            Ok(Source::SeekUnion(probe_over, keys))
        }
        AccessKind::RangeUnion => {
            let AccessPath::IndexSeekUnion {
                table_root,
                index_root,
                index_name,
                branches,
                collations,
                descending,
                columns,
                without_rowid,
                key_entry_slots,
                ..
            } = path
            else {
                return Err(misuse("a range-union stage over a path that is not one"));
            };
            let scans = range_union_bounds(
                tree,
                projection,
                *table_root,
                *index_root,
                index_name,
                *without_rowid,
                key_entry_slots,
                branches,
                collations,
                descending,
                columns,
                table,
                space,
                params,
            )?;
            Ok(Source::RangeUnion(scans))
        }
        AccessKind::Nested => Err(misuse("a nested stage cannot drive a pipeline")),
        // Unreachable: `source_for` answers a materialised stage before it gets
        // here, because building one needs the plan and the catalog rather than
        // a tree. Stated rather than folded into the arm above, so that a stage
        // kind added later is a compile error.
        AccessKind::Materialised => Err(misuse(
            "a materialised stage is built by `source_for`, not from a tree",
        )),
    }
}

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

/// Returns the candidate rowids a filtered vector search must look at.
///
/// **An approximate index probed for `k` and then filtered returns fewer than
/// `k` rows, and nothing says so.** 400 vectors, a predicate keeping 5% of
/// them and `LIMIT 10` returned one row where the exhaustive plan returns ten:
/// recall 0.1, silently. The index was asked for ten neighbours and nine of
/// them failed the `WHERE`, so nine of the answer's rows were never candidates
/// at all.
///
/// So the probe is *iterative*, which is what pgvector's `hnsw.iterative_scan`
/// is: ask for `k`, test the residual over what came back, and if fewer than
/// `k` rows survive, ask deeper - until enough survive or the store is
/// exhausted, which it is the moment it answers with fewer rows than it was
/// asked for. A query with no residual is the case this whole function skips:
/// there is nothing to lose, so one round is the answer.
///
/// The cost of a round the predicate rejects is one descent per candidate, and
/// those descents are made twice - once here to count, once in
/// [`Source::Vector`] to produce. That is the price of keeping the predicate
/// where the pipeline already tests it rather than pushing a second copy of
/// the expression evaluator into the store's cursor, and it is paid only by a
/// vector query that carries a `WHERE`.
///
/// @param plan - the planner's output, for the residual predicates
/// @param catalog - where the index and the pool come from
/// @param space - the joined column space the predicates are compiled over
/// @param params - the bound parameters
/// @param stage - the vector stage, for its tree's pool
/// @param index - the store's name
/// @param wanted - the vector to measure against
/// @param depth - the `LIMIT`, which is the first round's `k`
/// @param limit - the statement's row limit, when it has a constant one
/// @param probe_over - the probe the counting rounds read rows with
#[allow(clippy::too_many_arguments)]
fn iterative_candidates(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    index: &[u8],
    wanted: &Datum<'_>,
    depth: usize,
    limit: Option<usize>,
    probe_over: &PointProbe<'_>,
) -> DbResult<Vec<i64>> {
    let ask = |k: usize| -> DbResult<Vec<i64>> {
        catalog.vector_candidates(index, wanted, k)?.ok_or_else(|| {
            misuse(format!(
                "no vector index named {}",
                String::from_utf8_lossy(index)
            ))
        })
    };
    let predicates = residual_programs(plan, space, params)?;
    let mut keys = ask(depth)?;
    if predicates.is_empty() {
        return Ok(keys);
    }
    // The rows the statement is asking for. `LIMIT` is what a vector path is
    // chosen by, so this is nearly always `depth` - but a plan that arrived
    // here with a smaller chain limit should stop at the smaller number.
    let target = limit.unwrap_or(depth).min(depth).max(1);
    let Some(pool) = catalog.pool_for(stage.root) else {
        return Ok(keys);
    };
    let mut want = depth;
    loop {
        // The store answering with fewer rows than it was asked for is the only
        // honest signal that there is nothing more to find, and it is checked
        // before the count so an exhausted graph ends the loop even when the
        // predicate rejects everything.
        let exhausted = keys.len() < want;
        let survivors = surviving_rows(&keys, pool, probe_over, &predicates)?;
        if survivors >= target || exhausted || want >= VECTOR_CANDIDATE_CAP {
            return Ok(keys);
        }
        want = want.saturating_mul(VECTOR_WIDEN).min(VECTOR_CANDIDATE_CAP);
        keys = ask(want)?;
    }
}

/// Compiles the predicates the pipeline will test over a stage's own rows.
///
/// The whole `WHERE` minus what the access path already consumed, which is
/// exactly what [`build_chain`] hangs `Filter` operators for - built a second
/// time here rather than shared, because the chain's copies are boxed into an
/// operator tree the source cannot reach into.
///
/// @param plan - the planner's output
/// @param space - the joined column space
/// @param params - the bound parameters
fn residual_programs(
    plan: &PhysicalPlan,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Vec<Box<dyn crate::expr::Eval>>> {
    let mut built: Vec<Box<dyn crate::expr::Eval>> = Vec::new();
    for expr in plan
        .constant_filter
        .iter()
        .chain(plan.residuals.iter().flatten())
    {
        let translated = translate_scan(expr, space, params)?;
        built.push(compile(&translated, space.types)?);
    }
    Ok(built)
}

/// Counts how many of a candidate list's rows pass every predicate.
///
/// @param keys - the candidate rowids
/// @param pool - the buffer pool the rows are read through
/// @param probe_over - the probe the rows are read with
/// @param predicates - the compiled residuals, all of which must hold
fn surviving_rows(
    keys: &[i64],
    pool: &Pool,
    probe_over: &PointProbe<'_>,
    predicates: &[Box<dyn crate::expr::Eval>],
) -> DbResult<usize> {
    let mut buffer: Vec<OwnedDatum> = Vec::new();
    let mut seen = 0usize;
    for key in keys {
        if !probe_over.lookup(pool, &[Datum::Int(*key)], &mut buffer)? {
            continue;
        }
        let borrowed: Vec<Datum<'_>> = buffer.iter().map(OwnedDatum::borrow).collect();
        let columns: Vec<crate::batch::Vector<'_>> = borrowed
            .iter()
            .map(|value| crate::batch::Vector::Values(std::slice::from_ref(value)))
            .collect();
        let batch = Batch::new(1, columns);
        let mut held = true;
        for predicate in predicates {
            let verdict = predicate.value(&batch, 0)?;
            if crate::expr::truth(&verdict.get()) != Some(true) {
                held = false;
                break;
            }
        }
        if held {
            seen = seen.saturating_add(1);
        }
    }
    Ok(seen)
}

/// Builds one inner stage as an index nested loop join.
///
/// @param plan - the planner's output
/// @param catalog - where the trees come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the inner stage
/// @param index - the stage's position
/// @param downstream - what to push joined rows into
#[allow(clippy::too_many_arguments)]
fn build_nested<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    index: usize,
    downstream: Box<dyn Sink + 't>,
) -> DbResult<Box<dyn Sink + 't>> {
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    // **Two shapes of inner term, and the join kind is what picks between
    // them.** An inner join over a tree probes it once per outer row and never
    // materialises anything, which is the shape every read family measures. An
    // outer join, and any term that is not a tree at all, reads its rows once
    // into a buffer and pairs them: an outer join has to know that a pair
    // *failed* the condition in order to null-extend instead of dropping, and a
    // probe cannot tell that from a key that was not there.
    // **A table-valued function whose argument reads an outer column.** It has
    // a different answer per outer row, so it is driven per outer row; see
    // `crate::lateral` for why materialising it once would be wrong rather than
    // slow. This is checked before the materialised path, which is where such a
    // term would otherwise go.
    if let AccessPath::VirtualScan { offer, .. } = &source_term.path {
        if offer.iter().any(|held| reads_a_column(&held.value)) {
            return build_lateral_join(plan, catalog, space, params, stage, downstream);
        }
    }
    // **An outer join whose key is its whole condition is an index nested
    // loop.** The materialised shape below reads the inner side
    // once into a buffer, which is linear in the inner table however few outer
    // rows there are: `LEFT JOIN chunk c ON c.document_id = d.id` for one
    // document read all 60,000 chunks, at 138.2 ms against 0.5 ms for the same
    // join written `JOIN`. `IndexNestedLoopJoin` already answers `JoinKind::Left`
    // - it null-extends an outer row whose probe found nothing - so what was
    // missing was the permission to use it.
    //
    // The permission is `on_enforced`: the planner says so only when every
    // conjunct of the `ON` became part of the key. That is the condition this
    // operator needs, because it has nowhere to test what the key did not
    // capture and a pair that failed such a test has to null-extend rather than
    // vanish.
    let outer_by_probe = inillucent_sql::plan::is_outer(source_term.join)
        && source_term.on_enforced
        && stage.kind != AccessKind::Materialised;
    if (inillucent_sql::plan::is_outer(source_term.join) && !outer_by_probe)
        || stage.kind == AccessKind::Materialised
    {
        return build_materialised_join(plan, catalog, space, params, stage, index, downstream);
    }
    let tree = catalog
        .tree(stage.root)
        .ok_or_else(|| misuse(format!("no tree imported for root page {}", stage.root)))?;
    let outer_types: Vec<StaticType> = space
        .types
        .get(..stage.offset)
        .map(<[StaticType]>::to_vec)
        .unwrap_or_default();
    let (keys, full_key) = if stage.is_lookup {
        // **What the index entry carries to find the table row with**, read out
        // of the previous stage's row: a rowid for an ordinary table, and a
        // `WITHOUT ROWID` table's primary key - which is several columns, in the
        // table's own key order - for one of those. `identity` is the field that
        // says which, and it exists because `key_columns` on an index layout
        // names the whole entry rather than this part of it.
        let previous = space
            .stages
            .get(index.saturating_sub(1))
            .ok_or_else(|| misuse("a table lookup with no index stage before it"))?;
        let previous_layout = space
            .layouts
            .get(index.saturating_sub(1))
            .ok_or_else(|| misuse("a table lookup with no layout before it"))?;
        if previous_layout.identity.is_empty() {
            return Err(misuse(
                "the index entry carries nothing to look the table row up by",
            ));
        }
        (
            previous_layout
                .identity
                .iter()
                .map(|slot| Expr::Column(previous.offset.saturating_add(*slot)))
                .collect(),
            true,
        )
    } else {
        nested_key(&source_term.path, &source_term.table, space, params)?
    };
    // **And a third shape: an inner term with nothing to seek on.** An empty key
    // list means the loop below walks the whole inner tree once per outer row,
    // which is a join that is rows times rows. That is exactly the case
    // `PRAGMA automatic_index` is about, in SQLite and here: read the inner side
    // once instead, key it on the join expression, and probe. The test is only
    // whether there is a key to build the table on - `build_materialised_join`
    // is where it is built.
    if keys.is_empty()
        && plan
            .levers
            .has(inillucent_sql::plan::Levers::AUTOMATIC_INDEX)
        && has_equi_key(plan, space, params, stage, index)?
    {
        return build_materialised_join(plan, catalog, space, params, stage, index, downstream);
    }
    let compiled = keys
        .iter()
        .map(|expr| compile(expr, &outer_types))
        .collect::<DbResult<Vec<_>>>()?;
    Ok(Box::new(IndexNestedLoopJoin::new(
        // **The lookup stage carries the same kind as the seek that fed it.**
        // A null-extended index row probes the table with a NULL identity and
        // finds nothing; an `Inner` lookup would drop it, which would lose the
        // very row the outer join produced it for.
        join_kind_of(source_term.join),
        tree,
        // **This stage's pool, not the pipeline's.** The inner side of an index
        // nested loop is where a join across two databases reaches the second
        // file, so the pool travels with the stage that reads it.
        catalog.pool_for(stage.root).ok_or_else(|| {
            misuse("the inner side of a join names a database this connection does not hold")
        })?,
        compiled,
        Projection::all(stage.width),
        full_key,
        downstream,
    )))
}

/// Reports whether an inner stage's condition can key a hash table.
///
/// Asked *before* the join shape is chosen, because the answer is what chooses
/// it: without a key there is nothing to build and the nested loop is the only
/// shape left. It runs the same extraction the builder does, which is one
/// translation and one walk of a condition - paid once per stage at compile
/// time, against a join it is about to make linear.
///
/// @param plan - the planner's output
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the inner stage
/// @param index - the stage's position, which names its residual
pub(crate) fn has_equi_key(
    plan: &PhysicalPlan,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    index: usize,
) -> DbResult<bool> {
    let Some(source_term) = plan.sources.get(stage.term) else {
        return Ok(false);
    };
    let Some(expr) = source_term
        .on
        .as_ref()
        .or_else(|| plan.residuals.get(index).and_then(Option::as_ref))
    else {
        return Ok(false);
    };
    let translated = translate_scan(expr, space, params)?;
    Ok(crate::autoindex::equi_keys(&translated, stage.offset, stage.width).is_some())
}

/// Reports whether an expression reads any column at all.
///
/// The test that separates a table-valued function's *constant* argument -
/// `json_each('[1,2]')`, `generate_series(1, 10)` - from one that reads the row
/// beside it. The first can be folded once; the second cannot be folded at all.
///
/// @param expr - the argument expression
pub(crate) fn reads_a_column(expr: &BoundExpr) -> bool {
    let mut used = inillucent_sql::bind::ColumnUse::default();
    // Asked about *every* source: an argument reading this term's own column
    // would be a cycle the binder does not produce, so any column at all means
    // an outer one.
    for source in 0..MAX_SOURCES {
        expr.columns_read(source, &mut used);
        // A rowid read is still a column read. `ColumnUse` keeps it in its own
        // `rowid` flag rather than in `columns` - see `BoundExpr::columns_read`
        // - because a rowid is not one of the term's declared slots, and
        // dropping it here answered `false` for `docs JOIN owner ON owner.id =
        // docs.rowid`: `owner.id` is `owner`'s rowid alias, so the join's own
        // key read only set `used.rowid`, this function said the module's term
        // read no outer column, and a value that only exists per outer row was
        // then folded once as if it were a statement-wide constant.
        if used.opaque || used.rowid || !used.columns.is_empty() {
            return true;
        }
    }
    false
}

/// How many FROM terms an expression is asked about when looking for a column.
///
/// The limit on terms in one statement, which is what bounds the loop above.
const MAX_SOURCES: usize = 64;

/// Reports whether an expression reads a bound parameter anywhere in it.
///
/// The line between the two folds a deterministic registered function's
/// argument gets - `docs/roadmap.md` item 15's table. An argument that is
/// every literal is a constant regardless of which execution asked, so
/// [`translate`] folds it once and never again. An argument that reads `?N`
/// is a constant only for the execution now binding it, and folding it the
/// same way would bake one execution's answer into a chain a later execution
/// could reuse - so [`translate`] calls [`Params::note_execution_constant`]
/// whenever this answers `true`, the same guard a folded `now()` already
/// relies on to keep such a chain from being re-run against new values.
///
/// @param expr - the argument expression
fn reads_a_parameter(expr: &BoundExpr) -> bool {
    if matches!(expr, BoundExpr::Parameter(_)) {
        return true;
    }
    expr.children().iter().any(|child| reads_a_parameter(child))
}

/// Builds an inner stage as a module driven once per outer row.
///
/// @param plan - the planner's output
/// @param catalog - where the module's rows come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the inner stage
/// @param downstream - what to push joined rows into
fn build_lateral_join<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    downstream: Box<dyn Sink + 't>,
) -> DbResult<Box<dyn Sink + 't>> {
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    let AccessPath::VirtualScan { offer, chosen, .. } = &source_term.path else {
        return Err(misuse("a lateral join over a term that is not a module"));
    };
    let outer_types: Vec<StaticType> = space
        .types
        .get(..stage.offset)
        .map(<[StaticType]>::to_vec)
        .unwrap_or_default();
    // **In the order the module asked for them.** `best_index` chose which
    // offered constraints feed `filter` and in what order; the values handed
    // down have to arrive in that order, because the module reads them
    // positionally.
    let order: Vec<usize> = match chosen {
        Some(choice) => choice.arguments.clone(),
        None => (0..offer.len()).collect(),
    };
    let mut arguments = Vec::with_capacity(order.len());
    for position in order {
        let Some(constraint) = offer.get(position) else {
            continue;
        };
        let translated = translate_scan(&constraint.value, space, params)?;
        arguments.push(compile(&translated, &outer_types)?);
    }
    Ok(Box::new(crate::lateral::LateralModule::new(
        source_term.table.clone(),
        source_term.path.clone(),
        params.clone(),
        plan.select.columns_read(source_term.id),
        arguments,
        catalog,
        stage.width,
        downstream,
    )))
}

/// Builds one inner stage as a nested loop over rows read once.
///
/// **The one shape that can answer an outer join.** Its build side is a vector,
/// so it can evaluate the `ON` condition over each candidate pair - which is
/// what distinguishes "no partner" from "a partner that failed the condition",
/// the whole difference between an inner join and a `LEFT` one - and it can
/// remember which build rows matched, which is the whole of `RIGHT` and `FULL`.
///
/// It is also what a term that is not a tree gets: a derived table, a recursive
/// CTE and a virtual table each produce rows rather than pages, and reading
/// them once rather than once per outer row is correct because none of them has
/// a free variable to re-evaluate.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param stage - the inner stage
/// @param downstream - what to push joined rows into
fn build_materialised_join<'t>(
    plan: &PhysicalPlan,
    catalog: &'t dyn TreeCatalog,
    space: &Space<'_>,
    params: &Params,
    stage: &PreparedStage,
    index: usize,
    downstream: Box<dyn Sink + 't>,
) -> DbResult<Box<dyn Sink + 't>> {
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    // An inner term's rows are all read: the `LIMIT` above counts *joined*
    // rows, and a join may drop any of them, so stopping the inner side early
    // would be stopping it on a count that is not the one the statement asked
    // about.
    let rows = materialise_stage(plan, catalog, params, stage, None)?;
    // The condition is compiled over the *joined* row - every column produced
    // so far, then this stage's - which is exactly the space the pipeline
    // already describes, so an `ON` naming either side needs no special case.
    let joined_types: Vec<StaticType> = space
        .types
        .get(..stage.offset.saturating_add(stage.width))
        .map(<[StaticType]>::to_vec)
        .unwrap_or_else(|| space.types.to_vec());
    let condition = match &source_term.on {
        Some(expr) => {
            let translated = translate_scan(expr, space, params)?;
            Some(compile(&translated, &joined_types)?)
        }
        None => None,
    };
    // **The automatic index.** When the condition is a conjunction of plain
    // equalities with one side of the join per term, the inner rows go into a
    // hash table keyed on the inner halves and every outer row probes it -
    // which turns a join that was rows-times-rows into rows-plus-rows. SQLite
    // builds a transient b-tree for the same reason and puts it under the same
    // switch; `crate::autoindex` says why the structures differ and the switch
    // does not.
    //
    // The nested loop below is what `PRAGMA automatic_index = off` selects, and
    // it is also what a condition this cannot key on gets - which is most of
    // them, deliberately: a residual predicate the hash key did not capture
    // would have to be re-tested per pair, and there is nowhere here to do it.
    if plan
        .levers
        .has(inillucent_sql::plan::Levers::AUTOMATIC_INDEX)
    {
        // **`ON` for an outer join, the residual for an inner one.** A non-outer
        // join's constraint is split into the planner's term list before paths
        // are chosen, so what is left of `b.p = a.x` arrives as this stage's
        // residual and `source_term.on` is empty - which is exactly the case
        // this optimisation exists for.
        //
        // The residual is left in place rather than removed. It is applied as a
        // `Filter` above every join, so re-testing a condition the hash key has
        // already enforced costs a comparison per surviving row and cannot
        // change an answer; removing it would mean proving that the key
        // captured the whole predicate, and this operator has no way to prove
        // that about an expression it declined to look inside.
        let keyed = source_term
            .on
            .as_ref()
            .or_else(|| plan.residuals.get(index).and_then(Option::as_ref));
        if let Some(expr) = keyed {
            let translated = translate_scan(expr, space, params)?;
            if let Some(keys) = crate::autoindex::equi_keys(&translated, stage.offset, stage.width)
            {
                let outer_types: Vec<StaticType> = space
                    .types
                    .get(..stage.offset)
                    .map(<[StaticType]>::to_vec)
                    .unwrap_or_default();
                let inner_types: Vec<StaticType> = space
                    .types
                    .get(stage.offset..stage.offset.saturating_add(stage.width))
                    .map(<[StaticType]>::to_vec)
                    .unwrap_or_default();
                let probe = keys
                    .probe
                    .iter()
                    .map(|expr| compile(expr, &outer_types))
                    .collect::<DbResult<Vec<_>>>()?;
                let build = keys
                    .build
                    .iter()
                    .map(|expr| compile(expr, &inner_types))
                    .collect::<DbResult<Vec<_>>>()?;
                let mut join = crate::join::HashJoin::new(
                    join_kind_of(source_term.join),
                    build,
                    probe,
                    downstream,
                );
                join.build_materialised(&rows)?;
                return Ok(Box::new(join));
            }
        }
    }
    Ok(Box::new(NestedLoopJoin::new(
        join_kind_of(source_term.join),
        rows,
        condition,
        downstream,
    )))
}

/// Returns the executor's join kind for the one the statement wrote.
///
/// `CROSS` and a comma are inner joins that differ only in whether the planner
/// may reorder them, which it decided before this pass ran.
///
/// @param join - the join as the statement wrote it
pub(crate) fn join_kind_of(join: inillucent_sql::ast::JoinKind) -> JoinKind {
    match join {
        inillucent_sql::ast::JoinKind::Left => JoinKind::Left,
        inillucent_sql::ast::JoinKind::Right => JoinKind::Right,
        inillucent_sql::ast::JoinKind::Full => JoinKind::Full,
        inillucent_sql::ast::JoinKind::Comma
        | inillucent_sql::ast::JoinKind::Inner
        | inillucent_sql::ast::JoinKind::Cross => JoinKind::Inner,
    }
}

/// Reads one stage's rows into a buffer, whatever kind of source it is.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the bound parameters
/// @param stage - the stage to read
/// @param limit - the rows the statement above will keep, when it says
fn materialise_stage(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
    stage: &PreparedStage,
    limit: Option<usize>,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let source_term = plan
        .sources
        .get(stage.term)
        .ok_or_else(|| misuse("a stage names a FROM term the plan does not have"))?;
    match &source_term.path {
        AccessPath::Subquery { plan: inner, .. } => {
            // **A compound is run as a compound.**
            // `SELECT ... FROM (a UNION ALL b)` is an ordinary derived table
            // whose inner plan happens to have arms, and `prepare` refuses a
            // plan with arms by name - so the whole statement came back
            // `the new engine's physical pass does not handle a compound query
            // yet` for a shape the executor could already run. `run_compound`
            // is what the top level uses for exactly this plan, and a
            // materialised term wants what it produces: the rows, once.
            if inner.compounds.is_empty() {
                let prepared = prepare(inner, catalog, ForcePlan::default())?;
                Ok(run_prepared(inner, catalog, &prepared, params)?.0)
            } else {
                Ok(run_compound(inner, catalog, params)?.0)
            }
        }
        AccessPath::VirtualScan { .. } => {
            let needed = plan.select.columns_read(source_term.id);
            catalog
                .virtual_rows(&source_term.table, &source_term.path, params, &needed)?
                .ok_or_else(|| misuse("a virtual table the caller does not have"))
        }
        AccessPath::Recursive {
            seeds,
            steps,
            width,
        } => crate::recursive::run_recursive(
            source_term.id,
            seeds,
            steps,
            *width,
            catalog,
            params,
            limit,
        ),
        // The queue the fill loop is on, handed in by `run_recursive` through a
        // catalog that answers it. A plan reaching this outside such a loop is
        // a plan the binder should not have produced.
        AccessPath::RecursiveSelf { cte } => catalog
            .recursive_rows(*cte)
            .map(<[Vec<OwnedDatum>]>::to_vec)
            .ok_or_else(|| misuse("a reference to a recursive CTE outside the loop that fills it")),
        // Every remaining path reads a tree, and an outer term's path is a
        // plain scan of it by construction - the planner does not let a
        // predicate become a seek on a side that has to null-extend.
        _ => {
            let tree = catalog
                .tree(stage.root)
                .ok_or_else(|| misuse(format!("no tree imported for root page {}", stage.root)))?;
            let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut sink = CollectInto::new(std::rc::Rc::clone(&collected));
            let pool = catalog.pool_for(stage.root).ok_or_else(|| {
                misuse("a materialised stage names a database this connection does not hold")
            })?;
            FullScan::new(tree, Projection::all(stage.width)).run(pool, &mut sink)?;
            let rows = collected.borrow().clone();
            Ok(rows)
        }
    }
}

/// How many passes a recursive CTE may make before the engine refuses.
///
/// A recursion whose step arm never stops producing rows is a query that does
/// not end, and the only difference between that and a slow one is a number, so
/// there is a number. SQLite's own guard is the same idea under a different
/// name: it stops when the queue is empty, and a `LIMIT` is what a person adds
/// Reports whether a query is the shape a skip scan answers.
///
/// Every condition is load bearing:
///
/// - `DISTINCT` with no aggregation, because a skip scan produces one row per
///   distinct prefix and nothing else;
/// - no `WHERE`, because a skipped row might have been the one that passed it;
/// - one stage, because a skip scan produces representatives rather than rows
///   and a join over representatives is not the query;
/// - the projected columns are exactly a prefix of the tree's key order,
///   because that is what makes "one row per distinct value" the same set as
///   the query's;
/// - every ordering term ascending, so the rows the seek produces are the
///   answer in the order asked for.
///
/// @param plan - the planner's output
/// @param catalog - where the layouts come from
/// @param root - the tree the outermost stage reads
fn skip_scan_applies(plan: &PhysicalPlan, catalog: &dyn TreeCatalog, root: u32) -> DbResult<bool> {
    let select = &plan.select;
    if !select.distinct
        || plan.aggregation != AggregationMode::None
        || select.filter.is_some()
        || plan.constant_filter.is_some()
        || plan.residuals.iter().any(Option::is_some)
        || select.limit.is_some()
        || plan.sources.len() != 1
        || select
            .order_by
            .iter()
            .any(|term| term.order == SortOrder::Descending)
    {
        return Ok(false);
    }
    let Some(layout) = catalog.layout(root) else {
        return Ok(false);
    };
    // The projected columns must be exactly the leading key columns.
    if select.columns.len() > layout.key_columns.len() {
        return Ok(false);
    }
    for (position, column) in select.columns.iter().enumerate() {
        let declared = match &column.expr {
            BoundExpr::Column { column, .. } => *column as usize,
            _ => return Ok(false),
        };
        let tree_column = match layout.slots.get(declared) {
            Some(Some(tree_column)) => *tree_column,
            _ => return Ok(false),
        };
        if layout.key_columns.get(position) != Some(&tree_column) {
            return Ok(false);
        }
    }
    Ok(!select.columns.is_empty())
}

/// Returns how many leading key columns a skip scan produces.
///
/// @param plan - the planner's output
/// @param space - the joined column space
/// @param params - the bound parameters
fn projected_prefix(plan: &PhysicalPlan, space: &Space<'_>, params: &Params) -> DbResult<usize> {
    let _ = (space, params);
    Ok(plan.select.columns.len())
}

/// Reports whether a list of expressions is a prefix of the scan's key order.
///
/// Every expression must be a bare column reference, and the columns must be
/// exactly the scan order's leading columns in the same order - not a subset
/// and not a permutation, because "the rows arrive sorted by these" is only
/// true of a prefix.
///
/// @param exprs - the expressions to test
/// @param scan_order - the tree columns the leaves are ordered by
fn is_scan_prefix(exprs: &[Expr], scan_order: &[Vec<usize>]) -> bool {
    if exprs.is_empty() || exprs.len() > scan_order.len() {
        return false;
    }
    exprs.iter().enumerate().all(|(position, expr)| match expr {
        Expr::Column(index) => scan_order
            .get(position)
            .is_some_and(|held| held.contains(index)),
        _ => false,
    })
}

/// Returns, for each column the walk is ordered by, every joined column that
/// carries that value.
///
/// **A non-covering seek carries its key twice.** The index
/// stage holds the key columns it is ordered by, and the table fetch behind it
/// holds the same values again under the table's own column numbers - and it is
/// the table's numbers a select list resolves to, because that is what the
/// caller wrote. Asking whether the ORDER BY is the index's own column
/// therefore answered no for every query of the form `SELECT <a column the
/// index does not cover> FROM t WHERE key > ? ORDER BY key`, which is keyset
/// paging, which is how anything walks a large table.
///
/// The index layout's `slots` is the map: `slots[t] = Some(p)` says the table's
/// column `t` sits at the index's tree column `p`. The lookup stage contributes
/// the table's columns starting at its own offset, so the same value is at
/// `offset + t`.
///
/// @param stages - the prepared stages, outermost first
/// @param layouts - each stage's layout
/// @param order - the tree columns the outermost walk is ordered by
fn order_equivalents(
    stages: &[PreparedStage],
    layouts: &[std::rc::Rc<SourceLayout>],
    order: &[usize],
) -> Vec<Vec<usize>> {
    let mut classes: Vec<Vec<usize>> = order.iter().map(|column| vec![*column]).collect();
    let Some(index_layout) = layouts.first() else {
        return classes;
    };
    for (stage, layout) in stages.iter().zip(layouts.iter()).skip(1) {
        if !stage.is_lookup {
            continue;
        }
        for (position, column) in order.iter().enumerate() {
            for (slot, held) in index_layout.slots.iter().enumerate() {
                if *held != Some(*column) {
                    continue;
                }
                // **Through the lookup's own slot map, not the record slot.**
                // A table tree carries its rowid first, so the record's slot 0
                // is its tree column 1 - and `offset + slot` names the rowid
                // rather than the value the index is ordered by.
                let Some(Some(tree_column)) = layout.slots.get(slot) else {
                    continue;
                };
                if let Some(class) = classes.get_mut(position) {
                    class.push(stage.offset.saturating_add(*tree_column));
                }
            }
        }
    }
    classes
}

/// Reports whether the projected rows already arrive in the ORDER BY's order.
///
/// @param sort_keys - the ordering terms, in output-column space
/// @param projected - the output expressions
/// @param plan - the planner's output
/// @param scan_order - the tree columns the leaves are ordered by
/// @param grouped_walk - whether a streaming grouped aggregate is in the chain
fn output_is_sorted_by(
    sort_keys: &[SortKey],
    projected: &[Expr],
    plan: &PhysicalPlan,
    scan_order: &[Vec<usize>],
    grouped_walk: bool,
) -> bool {
    match plan.aggregation {
        AggregationMode::Grouped => {
            grouped_walk
                && sort_keys.iter().enumerate().all(|(position, term)| {
                    matches!(projected.get(term.column), Some(Expr::Column(index)) if *index == position)
                })
        }
        AggregationMode::Whole => false,
        AggregationMode::None => {
            let ordered: Vec<Expr> = sort_keys
                .iter()
                .filter_map(|term| projected.get(term.column).cloned())
                .collect();
            ordered.len() == sort_keys.len() && is_scan_prefix(&ordered, scan_order)
        }
    }
}

/// Builds a plan and runs it, returning the rows.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let prepared = prepare(plan, catalog, ForcePlan::default())?;
    run_prepared(plan, catalog, &prepared, params)
}

/// Runs an already-prepared statement and returns the rows.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choices [`prepare`] made
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run_prepared(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let rows = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = Box::new(CollectInto::new(std::rc::Rc::clone(&rows)));
    let (mut pipeline, shape) = build_prepared(plan, catalog, prepared, params, sink)?;
    pipeline.run()?;
    // **Taken, not cloned.** The sink is dropped with the pipeline and nothing
    // reads the buffer again, so cloning it copied every row of every answer to
    // hand back a second copy of what was about to be freed. On a one-row answer
    // that is two allocations of the twenty-two an already-prepared `SELECT 1`
    // makes; on a scan it is the whole result set, twice.
    let collected = std::mem::take(&mut *rows.borrow_mut());
    Ok((collected, shape))
}

/// Runs a compound query: two or more arms joined by a set operator.
///
/// **Each arm is an ordinary plan and is run as one.** The set operation is
/// [`crate::setop::SetOp`], the same operator a hand-built pipeline would use,
/// so there is one implementation of what `EXCEPT` means rather than two. What
/// this function adds is the plumbing the push executor cannot express on its
/// own: several sources feeding one sink, and - for `EXCEPT` and `INTERSECT` -
/// the right arm having to be complete before the left arm's first row can be
/// judged.
///
/// The arms are materialised between steps. A compound is a pipeline breaker in
/// every engine that has one, because three of the four operators are set
/// operations over whole rows and a set operation cannot stream; `UNION ALL`
/// could, and running it the same way costs a buffer and keeps the four arms of
/// this function from being four different shapes.
///
/// ## Where the ORDER BY lives
///
/// On the **head** arm's `select`, not on the compound. That is the binder's
/// doing and it is right - SQL does not let an arm of a compound carry its own
/// `ORDER BY`, so the one that is written belongs to the whole - but it means
/// the head arm has to be run with its ordering and its limit *removed*, or
/// `SELECT a FROM t UNION SELECT b FROM u LIMIT 3` would take three rows from
/// the first arm and then union them.
///
/// @param plan - the head arm, carrying the rest in `compounds`
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run_compound(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    // Every uncorrelated subquery is answered once, here, before anything is
    // built over it. See `crate::subquery` for why it is per execution.
    let folded = crate::subquery::fold(plan, catalog, params)?;
    let params = folded.as_ref().unwrap_or(params);
    let collations: Vec<Collation> = plan
        .select
        .columns
        .iter()
        .map(|column| inillucent_sql::bind::result_collation(&column.expr))
        .collect();
    let (mut rows, shape) = run_arm(plan, catalog, params)?;
    for (op, arm) in &plan.compounds {
        if !arm.compounds.is_empty() {
            // The binder flattens a chain of compounds onto the head, so an arm
            // carrying its own is a shape this has never been handed. Refusing
            // is what the physical pass does with a shape it has not seen.
            return unsupported("a compound query nested inside a compound arm");
        }
        let (right, _) = run_arm(arm, catalog, params)?;
        rows = combine(kind_of(*op), &collations, rows, right)?;
    }
    let ordered = order_compound(&plan.select, rows, &collations, params)?;
    Ok((ordered, shape))
}

/// Runs one arm of a compound, without the compound's own ordering or limit.
///
/// @param plan - the arm
/// @param catalog - where the trees and layouts come from
/// @param params - the bound parameters
fn run_arm(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let mut arm = plan.clone();
    arm.compounds.clear();
    arm.select.order_by.clear();
    arm.select.limit = None;
    arm.select.offset = None;
    arm.needs_sort = false;
    arm.reverse = false;
    run_any(&arm, catalog, params)
}

/// Runs any planned query, whichever of the three shapes it is.
///
/// The one entry point that knows a compound is several plans and a window
/// query is a plan with a pass on top. Every caller that just wants an answer
/// goes through here; `prepare` and `run_prepared` stay the single-pipeline
/// pair they were, because a `Prepared` is the structural choice for *one*
/// pipeline and neither of the other two shapes has only one.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run_any(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    let prepared = prepare_any(plan, catalog)?;
    run_any_prepared(plan, catalog, &prepared, params)
}

/// Returns the one key a plan looks up, when the whole plan is a rowid seek.
///
/// **A write whose `WHERE` is a rowid equality does not need a pipeline to find
/// one row.** `UPDATE t SET ... WHERE id = ?1` plans to a single-stage
/// `RowidSeek`, and running it as a query builds a source, a projection and a
/// collecting sink to hand back one integer the plan already contains. The write
/// gate measured that at 0.89 us of a 1.85 us update, and 17 us of a 47 us one -
/// about half of each, spent deciding something already decided.
///
/// `None` for every other shape, and the caller runs the query. The conditions
/// are all of them: one FROM term, a rowid seek, no residual predicate, no
/// constant filter, no limit and no offset. A plan with any of those does more
/// than name a row, and answering it from the key alone would be answering a
/// different question.
///
/// @param plan - the planner's output
/// @param params - the values bound to `?1`, `?2`, ...
pub fn rowid_seek_key(plan: &PhysicalPlan, params: &Params) -> DbResult<Option<OwnedDatum>> {
    if plan.sources.len() != 1
        || plan.constant_filter.is_some()
        || plan.select.limit.is_some()
        || plan.select.offset.is_some()
        || !plan.compounds.is_empty()
        || plan.residuals.iter().any(Option::is_some)
    {
        return Ok(None);
    }
    let Some(source) = plan.sources.first() else {
        return Ok(None);
    };
    let AccessPath::RowidSeek { key, .. } = &source.path else {
        return Ok(None);
    };
    let held = HeldSpace {
        layouts: Vec::new(),
        types: Vec::new(),
        order: Vec::new(),
    };
    let space = held.view(&[]);
    // Integer affinity, because that is what a rowid comparison applies -
    // `WHERE id = '4'` finds row 4 - and the seek path already applies it. A
    // shortcut that skipped it would answer a question the pipeline would not.
    match constant_value(key, &space, params, Some(Affinity::Integer)) {
        Ok(value) => Ok(Some(value)),
        // The key reads a column, which a rowid seek's should not; the pipeline
        // is the honest answer rather than a guess about what it meant.
        Err(_) => Ok(None),
    }
}

/// Makes the structural choice for any planned query, once.
///
/// **Everything that does not depend on the bound parameters belongs here**, so
/// that a caller re-running a statement pays for the parameters and the work and
/// not for the decision. A compound and a windowed query have no single set of
/// stages - a compound has one per arm, and a window pass plans an inner query
/// of its own - so they get an empty `Prepared` and are re-decided per
/// execution. That is honest rather than tidy: the alternative is a `Prepared`
/// that describes one of several pipelines and is silently wrong about the rest.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
pub fn prepare_any(plan: &PhysicalPlan, catalog: &dyn TreeCatalog) -> DbResult<Prepared> {
    if !plan.compounds.is_empty() || !plan.select.windows.is_empty() {
        return Ok(Prepared {
            stages: Vec::new(),
            forced: ForcePlan::default(),
        });
    }
    prepare(plan, catalog, ForcePlan::default())
}

/// Runs any planned query against a choice [`prepare_any`] already made.
///
/// @param plan - the planner's output
/// @param catalog - where the trees and layouts come from
/// @param prepared - the structural choice
/// @param params - the values bound to `?1`, `?2`, ...
pub fn run_any_prepared(
    plan: &PhysicalPlan,
    catalog: &dyn TreeCatalog,
    prepared: &Prepared,
    params: &Params,
) -> DbResult<(Vec<Vec<OwnedDatum>>, Shape)> {
    if !plan.compounds.is_empty() {
        return run_compound(plan, catalog, params);
    }
    if !plan.select.windows.is_empty() {
        return crate::windowpass::run_windowed(plan, catalog, params);
    }
    run_prepared(plan, catalog, prepared, params)
}

/// Returns the set operation a compound operator names.
///
/// @param op - the binder's operator
fn kind_of(op: inillucent_sql::ast::CompoundOp) -> SetKind {
    match op {
        inillucent_sql::ast::CompoundOp::Union => SetKind::Union,
        inillucent_sql::ast::CompoundOp::UnionAll => SetKind::UnionAll,
        inillucent_sql::ast::CompoundOp::Except => SetKind::Except,
        inillucent_sql::ast::CompoundOp::Intersect => SetKind::Intersect,
    }
}

/// Applies one set operation to two already-materialised arms.
///
/// @param kind - which of the four
/// @param collations - the collation of each result column
/// @param left - the rows so far
/// @param right - the arm being folded in
fn combine(
    kind: SetKind,
    collations: &[Collation],
    left: Vec<Vec<OwnedDatum>>,
    right: Vec<Vec<OwnedDatum>>,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut keys = SetKeys::new(collations.to_vec());
    if kind.needs_right_first() {
        // `EXCEPT` and `INTERSECT` cannot judge a left row until the right arm
        // is complete, which is what `needs_right_first` says and why the right
        // arm is reduced to its keys before the left arm is pushed at all.
        ValuesScan::new(right.clone()).run(&mut keys)?;
    }
    let mut operation = SetOp::new(
        kind,
        collations.to_vec(),
        keys,
        Box::new(CollectInto::new(std::rc::Rc::clone(&collected))),
    );
    ValuesScan::new(left).run_without_finish(&mut operation)?;
    if !kind.needs_right_first() {
        // Both unions push both arms through the same operator, because neither
        // needs to know about the other in advance.
        ValuesScan::new(right).run_without_finish(&mut operation)?;
    }
    operation.finish()?;
    let answer = collected.borrow().clone();
    Ok(answer)
}

/// Applies a compound's own `ORDER BY`, `LIMIT` and `OFFSET`.
///
/// @param select - the head arm's bound statement, which carries them
/// @param rows - the combined rows
/// @param collations - the collation of each result column
/// @param params - the bound parameters
fn order_compound(
    select: &BoundSelect,
    rows: Vec<Vec<OwnedDatum>>,
    collations: &[Collation],
    params: &Params,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    if select.order_by.is_empty() && select.limit.is_none() && select.offset.is_none() {
        return Ok(rows);
    }
    let mut keys = Vec::with_capacity(select.order_by.len());
    for term in &select.order_by {
        // A compound's ordering terms name *output* columns - SQL does not let
        // one reach into an arm's FROM clause - so the binder has already
        // resolved each to an ordinal. Anything else is a shape this cannot
        // order and is refused rather than approximated.
        let BoundExpr::SorterColumn { column } = &term.expr else {
            return unsupported("a compound query ordered by an expression");
        };
        let descending = term.order == SortOrder::Descending;
        keys.push(SortKey {
            column: usize::from(*column),
            descending,
            collation: collations
                .get(usize::from(*column))
                .copied()
                .unwrap_or(term.collation),
            // The binder has already resolved the default, which is NULLS
            // FIRST ascending and NULLS LAST descending.
            nulls_first: match term.nulls {
                NullOrder::First => true,
                NullOrder::Last => false,
            },
        });
    }
    let limit = constant_count(select.limit.as_ref(), params, Negative::NoLimit)?;
    let offset = constant_count(select.offset.as_ref(), params, Negative::Zero)?;
    let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let collect: Box<dyn Sink> = Box::new(CollectInto::new(std::rc::Rc::clone(&collected)));
    let sink: Box<dyn Sink> = match (limit, offset) {
        (None, None) => collect,
        (limit, offset) => Box::new(Limit::new(
            limit.unwrap_or(usize::MAX),
            offset.unwrap_or(0),
            collect,
        )),
    };
    let mut head: Box<dyn Sink> = if keys.is_empty() {
        sink
    } else {
        Box::new(Sort::new(keys, sink))
    };
    ValuesScan::new(rows).run(head.as_mut())?;
    let answer = collected.borrow().clone();
    Ok(answer)
}

/// Returns the collation each result column is compared under by `DISTINCT`.
///
/// @param select - the bound statement
pub(crate) fn distinct_collations(select: &BoundSelect) -> Vec<Collation> {
    select
        .columns
        .iter()
        .map(|column| inillucent_sql::bind::result_collation(&column.expr))
        .collect()
}

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

/// Translates a bound expression that reads the scan's columns.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the bound parameters
/// Returns the expression `sqlite_offset(X)` compiles to.
///
/// The argument has to be a column of a table the statement reads, which is
/// SQLite's own rule - anything else answers NULL there and answers NULL here.
/// What is compiled is a lookup of the row's key in a table of leaf boundaries:
/// one entry per leaf, holding the lowest key on it and the offset of its page
/// in the file. The boundaries are read once, while the statement is being
/// prepared, and the cost is one page read per leaf rather than one per row.
///
/// @param arguments - the call's single argument
/// @param space - the joined column space
/// @param params - the statement's bound parameters
/// @param frame - which of the two row shapes the expression is over
fn row_offset(
    arguments: &[BoundExpr],
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Expr> {
    let Some(BoundExpr::Column { source, .. }) = arguments.first() else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let source = *source;
    let Some(catalog) = space.catalog else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let Some(stage) = space.stages.iter().find(|stage| stage.term == source) else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let Some(layout) = space.layouts.get(source) else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let Some(rowid_column) = layout.rowid else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let (Some(tree), Some(pool)) = (catalog.tree(stage.root), catalog.pool_for(stage.root)) else {
        return Ok(Expr::Literal(OwnedDatum::Null));
    };
    let boundaries = leaf_boundaries(tree, pool, rowid_column)?;
    let rowid = translate(&BoundExpr::Rowid { source }, space, params, frame)?;
    Ok(Expr::RowOffset {
        boundaries: std::sync::Arc::new(boundaries),
        rowid: Box::new(rowid),
    })
}

/// Returns the lowest key on each leaf and where that leaf sits in the file.
///
/// Sorted by key, so a lookup is a binary search. A leaf covers a contiguous
/// run of keys, so the boundary is the whole of what a lookup needs.
///
/// @param tree - the table's tree
/// @param pool - the pool its pages live in
/// @param rowid_column - which tree column holds the row's key
fn leaf_boundaries(
    tree: &PagedTree,
    pool: &Pool,
    rowid_column: usize,
) -> DbResult<Vec<(i64, i64)>> {
    let page_size = tree.page_size() as i64;
    let mut boundaries: Vec<(i64, i64)> = Vec::new();
    let mut page = tree.first_leaf();
    let mut seen = 0u64;
    while !page.is_none() {
        let guard = pool.fetch(page)?;
        let leaf = inillucent_tree::leaf::LeafRef::parse(&guard)?;
        let mut lowest: Option<i64> = None;
        for row in 0..leaf.row_count() {
            if let Ok(inillucent_tree::datum::Datum::Int(key)) =
                leaf.value_at(inillucent_tree::leaf::Hit::Sorted(row), rowid_column)
            {
                lowest = Some(lowest.map_or(key, |held: i64| held.min(key)));
            }
        }
        for row in 0..leaf.delta_count() {
            if let Ok(inillucent_tree::datum::Datum::Int(key)) =
                leaf.value_at(inillucent_tree::leaf::Hit::Delta(row), rowid_column)
            {
                lowest = Some(lowest.map_or(key, |held: i64| held.min(key)));
            }
        }
        boundaries.push((
            lowest.unwrap_or(i64::MIN),
            (page.0 as i64).saturating_mul(page_size),
        ));
        let next = leaf.right_sibling();
        drop(guard);
        page = next;
        seen = seen.saturating_add(1);
        // The same guard every walk in the tree crate carries: a sibling chain
        // that pointed at itself would otherwise not come back.
        if seen > tree.leaf_count().saturating_add(2) {
            break;
        }
    }
    boundaries.sort_by_key(|(key, _)| *key);
    Ok(boundaries)
}

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
        Some(None) => Ok(OwnedDatum::Text(b"ok".to_vec())),
        Some(Some(report)) => Ok(OwnedDatum::Text(report.into_bytes())),
        None => Err(inillucent_base::error::misuse(format!(
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

pub(crate) fn translate(
    expr: &BoundExpr,
    space: &Space<'_>,
    params: &Params,
    frame: Frame<'_>,
) -> DbResult<Expr> {
    // The leaves, which are the only thing the two frames disagree about. Every
    // node below this point recurses with the same frame, which is what makes
    // this one traversal rather than two that have to be kept in step - and
    // keeping them in step is exactly what failed: the post-aggregation copy
    // handled six node kinds and refused the rest, so `length(group_concat(x))`
    // was "a function call outside an aggregate" and `HAVING` had nowhere to be
    // translated at all.
    if let Frame::Window { pre, width } = frame {
        if let BoundExpr::WindowRef { slot } = expr {
            return Ok(Expr::Column(width.saturating_add(*slot)));
        }
        // A whole sub-expression the pass already computed, which is how a
        // window's own argument resolves without being recomputed.
        if let Some(position) = pre.iter().position(|held| held == expr) {
            return Ok(Expr::Column(position));
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
            return Ok(Expr::Column(group_width.saturating_add(*slot)));
        }
        if let Some(position) = select.group_by.iter().position(|key| key == expr) {
            return Ok(Expr::Column(position));
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
            return Ok(Expr::Column(
                group_width
                    .saturating_add(select.aggregates.len())
                    .saturating_add(at),
            ));
        }
    }
    Ok(match expr {
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
                return Ok(Expr::Literal(crate::constant::evaluated_constant(&folded)?));
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
                return row_offset(arguments, space, params, frame);
            }
            // **Folded here, where the catalog is.** See `ScalarFunc::RTreeCheck`.
            if *func == ScalarFunc::RTreeCheck {
                return Ok(Expr::Literal(rtree_check(arguments, space)?));
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
                return Ok(match kind {
                    SubqueryKind::Exists | SubqueryKind::Scalar => Expr::Column(column),
                    SubqueryKind::In => {
                        return unsupported("a correlated IN subquery");
                    }
                });
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
    })
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
fn translate_post(
    expr: &BoundExpr,
    select: &BoundSelect,
    space: &Space<'_>,
    params: &Params,
    group_width: usize,
) -> DbResult<Expr> {
    if select.aggregates.is_empty() {
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
fn aggregate_output_types(
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
fn aggregate_specs(
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
fn trim(width: usize, types: &[StaticType]) -> DbResult<Vec<Box<dyn crate::expr::Eval>>> {
    (0..width)
        .map(|index| compile(&Expr::Column(index), types))
        .collect()
}

/// Returns the statement's `LIMIT`, when it is a constant.
///
/// @param select - the bound statement
/// @param params - the bound parameters
fn constant_limit(select: &BoundSelect, params: &Params) -> DbResult<Option<usize>> {
    constant_count(select.limit.as_ref(), params, Negative::NoLimit)
}

/// Returns the statement's `OFFSET`, when it is a constant.
///
/// @param select - the bound statement
/// @param params - the bound parameters
fn constant_offset(select: &BoundSelect, params: &Params) -> DbResult<Option<usize>> {
    constant_count(select.offset.as_ref(), params, Negative::Zero)
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
fn same_expr(left: &Expr, right: &Expr) -> bool {
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

/// Returns the flow a sink reports, for the `Flow` re-export.
///
/// Kept so that a caller of this module does not have to reach into
/// [`crate::ops`] for the one type a custom sink needs.
pub fn continue_flow() -> Flow {
    Flow::Continue
}
