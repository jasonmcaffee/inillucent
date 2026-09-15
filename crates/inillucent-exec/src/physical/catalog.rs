//! Where the executor finds its trees, its layouts and its pages.
//!
//! Invariant: **a plan names a tree by its root page and nothing else.**
//! The root is the identifier the binder and the executor already agree on,
//! because it is what the schema the plan was compiled against records; a name
//! lookup here would be a second opinion about which tree is which.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
// `literal_value` is named by path from a dozen call sites in
// `inillucent-engine`, so it stays reachable here after the move to `constant`.
// `Compiled`, `Slot` and `try_compile` moved to `crate::compiled` to keep this
// file under its recorded ceiling; re-exported here so every existing
// `physical::Slot` / `physical::Compiled` / `physical::try_compile` reference
// - `inillucent-engine`'s `Cached::Select` among them - did not have to move
// with them.
use inillucent_pool::Pool;
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::plan::AccessPath;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::PagedTree;

use crate::expr::StaticType;

/// A catalog that also answers one recursive CTE's queue.
///
/// Everything else is delegated, so the step arm sees exactly the trees, the
/// layouts and the modules the statement sees. Wrapping rather than threading a
/// parameter through every builder is what keeps a recursive query from
/// changing the shape of a signature nothing else uses.
use super::*;

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
    /// is [`crate::ops::Flow`]: `Flow::Stop` means the pipeline has what it needs,
    /// and the module's loop abandons the cursor - the same way `scan.rs` abandons a
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

    /// Returns what a module says about its own storage.
    ///
    /// **The route `rtreecheck` takes.** A module's `integrity` is reachable
    /// from the connection and from nowhere else, and the question is about a
    /// named table rather than about a row - so it is asked once while the
    /// statement is being prepared, where the catalog is in hand, and the
    /// answer is folded into the expression as a constant.
    ///
    /// @param name - the table's name, as written
    fn module_integrity(&self, name: &[u8]) -> DbResult<ModuleIntegrity> {
        let _ = name;
        Ok(ModuleIntegrity::NoSuchModule)
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
                    // **The sentence is the message as well as the detail
                    // (task-1962, T3).** `misuse` attaches what it is given as
                    // detail alone, so `PRAGMA inillucent.force_plan =
                    // 'tablescn'` reported `bad parameter or other API misuse`
                    // to the person who had just mistyped the operator - the
                    // one thing they needed to see was the only thing not
                    // there. `physical::stages::unsupported` was fixed the same
                    // way and for the same reason.
                    let said = format!("force_plan does not know the operator '{other}'");
                    return Err(misuse(said.clone()).with_message(said));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every lever the pragma names is set, and only that lever.
    ///
    /// **A misspelled lever is refused rather than ignored (T3, task-1962).**
    /// The levers exist so a measurement can ask "is the answer the same with
    /// this operator off"; a run that silently switched nothing off would
    /// answer that question with the default plan and report it as the forced
    /// one.
    #[test]
    fn a_forced_plan_sets_the_levers_it_names() {
        let forced = ForcePlan::parse("scan,hashgroup").expect("two known levers parse");
        assert!(forced.table_scan, "scan was named");
        assert!(forced.hash_group, "hashgroup was named");
        assert!(!forced.full_sort, "sort was not named");
        assert!(!forced.hash_distinct, "distinct was not named");
        assert!(!forced.no_skip_scan, "noskip was not named");
    }

    /// The spellings the pragma accepts are the spellings it documents.
    #[test]
    fn both_spellings_of_each_lever_parse() {
        for (one, two) in [
            ("scan", "tablescan"),
            ("distinct", "hashdistinct"),
            ("hashaggregate", "hashgroup"),
            ("noskipscan", "noskip"),
        ] {
            let left = ForcePlan::parse(one).expect("the first spelling parses");
            let right = ForcePlan::parse(two).expect("the second spelling parses");
            assert_eq!(
                (
                    left.table_scan,
                    left.full_sort,
                    left.hash_distinct,
                    left.hash_group,
                    left.no_skip_scan
                ),
                (
                    right.table_scan,
                    right.full_sort,
                    right.hash_distinct,
                    right.hash_group,
                    right.no_skip_scan
                ),
                "`{one}` and `{two}` name the same lever and should set the same one"
            );
        }
    }

    /// An empty string forces nothing, which is how a pragma is cleared, and
    /// whitespace around a name is not part of it.
    #[test]
    fn an_empty_force_plan_forces_nothing() {
        let forced = ForcePlan::parse("").expect("the empty string parses");
        assert!(!forced.table_scan);
        assert!(!forced.full_sort);
        assert!(!forced.hash_distinct);
        assert!(!forced.hash_group);
        assert!(!forced.no_skip_scan);
        let spaced = ForcePlan::parse("  scan ,  sort  ").expect("spaces are trimmed");
        assert!(spaced.table_scan && spaced.full_sort);
    }

    /// A name this build does not know is an error, not a no-op.
    #[test]
    fn an_unknown_lever_is_refused() {
        let refused = ForcePlan::parse("scan,nonsense");
        assert!(
            refused.is_err(),
            "`nonsense` is not a lever and parsing it answered a plan"
        );
        let error = refused.expect_err("an unknown operator is refused");
        assert!(
            error.message().contains("nonsense"),
            "the refusal should name the operator it did not know, in the              message a person reads; it said {:?}",
            error.message()
        );
        assert_eq!(
            error.detail(),
            Some("force_plan does not know the operator 'nonsense'"),
            "and in the detail, which is what a caller reading the error programmatically gets"
        );
    }

    /// The metamorphic sweep offers the default and one arm per lever.
    ///
    /// A sweep that forgot an arm would report "the answer is the same under
    /// every plan" having never built the plan it was added for.
    #[test]
    fn the_sweep_offers_one_arm_per_lever() {
        let arms = ForcePlan::alternatives();
        assert_eq!(arms.len(), 6, "the default and one per lever");
        let (name, default) = arms.first().expect("there is a first arm");
        assert_eq!(*name, "default");
        assert!(
            !default.table_scan
                && !default.full_sort
                && !default.hash_distinct
                && !default.hash_group
                && !default.no_skip_scan,
            "the first arm is the shipped planner with nothing forced"
        );
        let forced: usize = arms
            .iter()
            .skip(1)
            .map(|(_, plan)| {
                usize::from(plan.table_scan)
                    + usize::from(plan.full_sort)
                    + usize::from(plan.hash_distinct)
                    + usize::from(plan.hash_group)
                    + usize::from(plan.no_skip_scan)
            })
            .sum();
        assert_eq!(forced, 5, "each of the five arms forces exactly one lever");
    }
}
