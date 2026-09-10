//! The `inillucent_search` virtual table: its declaration, its access path, its
//! cursor, its writes and its maintenance commands.
//!
//! Invariant: a write to a search table is an ordinary write to ordinary rows,
//! and nothing about a search index is published by any other means. An
//! `INSERT` writes the row into `%_content` and one entry into `%_delta`, both
//! inside whatever transaction the statement is running in. There is no
//! background flush, no separate durability decision and no moment where the
//! rows and the index disagree - so `ROLLBACK` undoes the search state for
//! exactly the same reason it undoes the row, and a power cut is recovered by
//! exactly the same recovery.
//!
//! The declared shape:
//!
//! ```sql
//! CREATE VIRTUAL TABLE docs USING inillucent_search(
//!     title, body,             -- the indexed text columns
//!     dims = 768,              -- the vector width, or absent for lexical only
//!     mode = 'exact',          -- or 'approximate'
//!     metric = 'cosine'
//! );
//! ```
//!
//! and the hidden columns that follow them, which are both the query interface
//! and the table-valued argument list:
//!
//! | hidden column | as a constraint | as a table-valued argument |
//! |---|---|---|
//! | `docs` (the table's own name) | `docs MATCH 'text'` | `docs('text')` |
//! | `k` | `k = 20` | `docs('text', 20)` |
//! | `vector` | `vector = :embedding` | `docs('text', 20, :embedding)` |
//! | `recall` | `recall = 0.9` | `docs('text', 20, :embedding, 0.9)` |
//! | `rank` | `ORDER BY rank` | - |
//!
//! `rank` is negated, so `ORDER BY rank` ascending is best-first. That is
//! FTS5's convention and there is no reason to have two.

use std::sync::Arc;

use inillucent_base::DbResult;
use inillucent_value::Value;

use inillucent_ext::vtab::{
    constraint, failure, Change, ConstraintOp, Context, Declaration, DeclaredColumn, FilterPlan,
    IndexQuery, Module, ModuleArguments, ShadowTable, VirtualCursor, VirtualTable, ROWID_COLUMN,
};

use crate::merge::{self, Cache, Hit, Request};
use crate::options::{self, Options, DEFAULT_K};
use crate::store::{state, Delta, Op, Row, Store};

/// The plan number for a scan of every row.
const PLAN_SCAN: i32 = 0;
/// The plan number for a lookup by rowid.
const PLAN_ROWID: i32 = 1;
/// The plan number for a search.
const PLAN_SEARCH: i32 = 2;
/// The plan bit that says the rows already come back ranked.
const PLAN_RANKED: i32 = 4;

/// What one claimed constraint carries, recorded in the plan string.
const ROLE_QUERY: char = 'q';
/// The hit count.
const ROLE_LIMIT: char = 'k';
/// The query vector.
const ROLE_VECTOR: char = 'v';
/// The recall target.
const ROLE_RECALL: char = 'r';
/// The rowid, for a point lookup.
const ROLE_ROWID: char = 'i';

/// The module.
#[derive(Clone, Copy, Debug, Default)]
pub struct SearchModule;

impl Module for SearchModule {
    /// Returns the module's name.
    fn name(&self) -> &str {
        "inillucent_search"
    }

    /// Returns the five shadow tables a search index lives in.
    fn shadow_tables(&self, arguments: &ModuleArguments) -> DbResult<Vec<ShadowTable>> {
        let declared = options::parse(&arguments.arguments)?;
        Ok(crate::store::shadow_tables(declared.columns.len()))
    }

    /// Connects to a table, whether it is being created or reopened.
    fn connect(
        &self,
        arguments: &ModuleArguments,
        creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        let declared = options::parse(&arguments.arguments)?;
        let store = Store::of(arguments, declared.columns.len())?;
        Ok(Box::new(SearchTable {
            declaration: declaration_of(&declared, &arguments.table),
            options: declared,
            store,
            cache: Arc::new(Cache::new()),
            creating,
            pending: None,
            marks: Vec::new(),
            touched: false,
        }))
    }
}

/// Returns the columns a declaration produces, visible ones first.
fn declaration_of(options: &Options, table: &[u8]) -> Declaration {
    let mut columns: Vec<DeclaredColumn> = options
        .columns
        .iter()
        .map(|name| DeclaredColumn::visible(&String::from_utf8_lossy(name)))
        .collect();
    columns.push(DeclaredColumn::hidden(&String::from_utf8_lossy(table)));
    columns.push(DeclaredColumn::hidden("k").typed("INTEGER"));
    columns.push(DeclaredColumn::hidden("vector").typed("BLOB"));
    columns.push(DeclaredColumn::hidden("recall").typed("REAL"));
    columns.push(DeclaredColumn::hidden("rank").typed("REAL"));
    Declaration {
        columns,
        without_rowid: false,
    }
}

/// One connected search table.
struct SearchTable {
    options: Options,
    store: Store,
    declaration: Declaration,
    cache: Arc<Cache>,
    creating: bool,
    /// The commit sequence this transaction is publishing under, once it has
    /// written anything.
    pending: Option<i64>,
    /// The delta-log ordinal each open savepoint started at.
    marks: Vec<(i32, i64)>,
    /// Whether this transaction has written to the index at all.
    touched: bool,
}

impl SearchTable {
    /// Returns which declared column is the table's own hidden query column.
    fn query_column(&self) -> i32 {
        self.options.columns.len() as i32
    }

    /// Returns which declared column carries the hit count.
    fn limit_column(&self) -> i32 {
        self.query_column().saturating_add(1)
    }

    /// Returns which declared column carries a vector.
    fn vector_column(&self) -> i32 {
        self.query_column().saturating_add(2)
    }

    /// Returns which declared column carries the recall target.
    fn recall_column(&self) -> i32 {
        self.query_column().saturating_add(3)
    }

    /// Returns which declared column carries the score.
    fn rank_column(&self) -> i32 {
        self.query_column().saturating_add(4)
    }

    /// Returns the commit sequence this transaction publishes under.
    ///
    /// Allocated once, on the transaction's first write, and reused by every
    /// later write it makes - so every change one transaction published carries
    /// one number, which is what makes the shared commit sequence a property a
    /// test can read back rather than a claim.
    ///
    /// It re-checks the stored sequence rather than trusting the remembered
    /// one, because a connection whose transaction boundaries were never
    /// announced would otherwise keep publishing under a number another
    /// transaction has since passed.
    fn commit_sequence(&mut self, context: &mut Context<'_>) -> DbResult<i64> {
        let published = self.store.state(context, state::SEQUENCE)?;
        if let Some(pending) = self.pending {
            if pending == published {
                return Ok(pending);
            }
        }
        let next = published.saturating_add(1);
        self.store.set_state(context, state::SEQUENCE, next)?;
        self.pending = Some(next);
        Ok(next)
    }

    /// Appends one entry to the delta log under this transaction's sequence.
    fn log(&mut self, context: &mut Context<'_>, id: i64, op: Op, digest: i64) -> DbResult<()> {
        let commit = self.commit_sequence(context)?;
        let ordinal = self.store.state(context, state::ORDINAL)?.saturating_add(1);
        self.store.set_state(context, state::ORDINAL, ordinal)?;
        self.store.append_delta(
            context,
            Delta {
                sequence: ordinal,
                commit,
                id,
                op,
                digest,
            },
        )?;
        self.touched = true;
        self.cache.forget();
        Ok(())
    }

    /// Reads the options the stored configuration says the table has.
    fn reconcile(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        let rows = self.store.read_config(context)?;
        if rows.is_empty() {
            return Ok(());
        }
        self.options = options::from_config(&rows, &self.options)?;
        Ok(())
    }

    /// Turns the values of an insert into a stored row.
    fn row_of(&self, values: &[Value<'static>]) -> DbResult<Row> {
        let mut columns = Vec::with_capacity(self.options.columns.len());
        for index in 0..self.options.columns.len() {
            columns.push(text_of(values.get(index)).unwrap_or_default());
        }
        let vector = match values.get(self.vector_column() as usize) {
            Some(Value::Null) | None => Vec::new(),
            Some(value) => {
                if !self.options.has_vectors() {
                    return Err(constraint(
                        "inillucent_search: this table was declared without a vector width",
                    ));
                }
                crate::store::vector_of(value, self.options.dims)?
            }
        };
        Ok(Row { columns, vector })
    }

    /// Writes one row and logs it.
    fn put(&mut self, context: &mut Context<'_>, id: i64, row: &Row) -> DbResult<()> {
        let existed = self.store.read_row(context, id)?.is_some();
        self.store.write_row(context, id, row)?;
        if !existed {
            let rows = self.store.state(context, state::ROWS)?.saturating_add(1);
            self.store.set_state(context, state::ROWS, rows)?;
        }
        self.log(context, id, Op::Put, row.digest())
    }

    /// Removes one row and logs it.
    fn remove(&mut self, context: &mut Context<'_>, id: i64) -> DbResult<()> {
        if self.store.read_row(context, id)?.is_none() {
            return Ok(());
        }
        self.store.delete_row(context, id)?;
        let rows = self.store.state(context, state::ROWS)?.saturating_sub(1);
        self.store.set_state(context, state::ROWS, rows.max(0))?;
        self.log(context, id, Op::Delete, 0)
    }

    /// Runs one of the maintenance commands.
    ///
    /// Written as `INSERT INTO docs(docs) VALUES('compact')`, which is FTS5's
    /// spelling for the same idea: a command is a write, so it takes the write
    /// lock and lands in the transaction like any other.
    fn command(&mut self, context: &mut Context<'_>, command: &str) -> DbResult<()> {
        match command.trim().to_ascii_lowercase().as_str() {
            "compact" => self.compact(context),
            "rebuild" => self.rebuild(context),
            "drop-old-generations" => {
                let current = self.store.state(context, state::GENERATION)?;
                self.store.drop_generations_below(context, current)?;
                Ok(())
            }
            "integrity-check" => match self.integrity(context)? {
                None => Ok(()),
                Some(problem) => Err(failure(problem)),
            },
            other => Err(failure(format!(
                "inillucent_search: no such command: {other}"
            ))),
        }
    }

    /// Folds the delta log into a new immutable generation, at commit.
    ///
    /// **The graph is not rebuilt here.** The built generation
    /// is loaded and each pending entry is inserted into it, so the graph work
    /// this commit pays is one insert per delta entry rather than one insert
    /// per row in the table. This path used to build the whole graph in one
    /// pass, which made an ordinary `INSERT` pay nine and a half minutes on the
    /// 598,560 chunk corpus this engine is deployed on - work an application
    /// cannot schedule and cannot interrupt.
    ///
    /// The delta log's length is what the bound is stated in, and
    /// [`Options::compact_threshold`] is what sets it: `compact = N` pins it at
    /// `N` entries, so a table declared that way pays `N` graph inserts per
    /// published generation however large it grows.
    ///
    /// The single-pass build is still the better graph and it is still
    /// reachable; it is now only ever asked for, by the `compact` command or by
    /// `rebuild`. [`Self::compact`] says what the difference costs.
    ///
    /// Every step is an ordinary write inside the caller's transaction: the new
    /// generation's rows are appended, the state rows are moved to name it, and
    /// the folded delta rows are removed. A crash at any point leaves the old
    /// generation named by the old state rows and the delta log untouched -
    /// which is the same index, reachable by exactly the same reads. The
    /// previous generation's rows are left where they are; reclaiming them is
    /// the separate `drop-old-generations` command, because a snapshot opened
    /// before the swap is still reading them.
    /// @param context - the module's reach into the database
    fn fold(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        let covered = self.store.state(context, state::COVERED)?;
        let pending = self.store.deltas_above(context, covered)?;
        if pending.is_empty() {
            return Ok(());
        }
        let rows = self.store.state(context, state::ROWS)?.max(0) as u64;
        let Some(threshold) = self.options.compact_threshold(rows) else {
            return Ok(());
        };
        if (pending.len() as u64) < threshold {
            return Ok(());
        }
        let highest = highest_of(&pending, covered);
        let generation = self.store.state(context, state::GENERATION)?;
        let (index, inserted) =
            merge::fold_generation(context, &self.store, &self.options, generation, &pending)?;
        let folds = self.store.state(context, state::FOLDS)?.saturating_add(1);
        self.publish(context, &index, highest, inserted, folds)
    }

    /// Builds a new generation in one pass over every row.
    ///
    /// **This is the batch rebuild workflow, and it is explicit.** It is what
    /// `INSERT INTO docs(docs) VALUES('compact')` runs, and it costs the whole
    /// corpus: every row is read, every chunk is inserted into a fresh graph,
    /// and the tombstoned chunks that folding left behind are gone. Those chunks
    /// going away is what an application is buying when it schedules this.
    ///
    /// It is a write like any other, so it lands in the caller's transaction
    /// and is atomic with it: the new generation is either named by the state
    /// rows or it is not, and a crash leaves the old one in place.
    ///
    /// **An empty delta log is not a reason to refuse.** It was, until M8, and
    /// that was harmless while every automatic compaction also built in one
    /// pass - there was nothing left to clean. Now that a commit folds, a table
    /// whose log has just been folded away is exactly the table whose graph has
    /// the most tombstoned chunks in it, and refusing there would leave an
    /// application no way to ask for the clean graph at all.
    /// @param context - the module's reach into the database
    fn compact(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        let covered = self.store.state(context, state::COVERED)?;
        let pending = self.store.deltas_above(context, covered)?;
        let highest = highest_of(&pending, covered);
        let (index, rows) = merge::build_from_rows(context, &self.store, &self.options)?;
        self.store.set_state(context, state::ROWS, rows as i64)?;
        self.publish(context, &index, highest, rows, 0)
    }

    /// Rebuilds the whole index from the rows, discarding every generation.
    ///
    /// The recovery path when a generation is unreadable, and the way an index
    /// built by an older layout is brought forward. `%_content` is the
    /// authoritative copy of every row, so this needs nothing the database does
    /// not already hold.
    fn rebuild(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        let (index, rows) = merge::build_from_rows(context, &self.store, &self.options)?;
        let ordinal = self.store.state(context, state::ORDINAL)?;
        self.store.set_state(context, state::ROWS, rows as i64)?;
        self.publish(context, &index, ordinal, rows, 0)
    }

    /// Writes one built index out as the next generation and names it.
    ///
    /// The one place a generation is published, so folding and building differ
    /// in how they produce the index and in nothing else. The order matters:
    /// the generation's rows are written before any state row names them, so a
    /// crash between the two leaves rows nothing reads rather than a state row
    /// pointing at a generation that is not there.
    /// @param context - the module's reach into the database
    /// @param index - the index to publish
    /// @param highest - the delta sequence this generation now covers
    /// @param inserted - how many chunks the build inserted into the graph
    /// @param folds - how many folds this lineage has taken, zero for a build
    fn publish(
        &mut self,
        context: &mut Context<'_>,
        index: &inillucent_core::index::Index,
        highest: i64,
        inserted: usize,
        folds: i64,
    ) -> DbResult<()> {
        let mut bytes = Vec::new();
        inillucent_core::persist::write_index(index, &mut bytes).map_err(|error| {
            failure(format!(
                "inillucent_search: cannot write a generation: {error}"
            ))
        })?;
        let generation = self
            .store
            .state(context, state::GENERATION)?
            .saturating_add(1);
        self.store.write_generation(context, generation, &bytes)?;
        self.store
            .set_state(context, state::GENERATION, generation)?;
        self.store.set_state(context, state::COVERED, highest)?;
        self.store
            .set_state(context, state::INSERTED, inserted as i64)?;
        self.store.set_state(context, state::FOLDS, folds)?;
        self.store
            .set_state(context, state::CHUNKS, index.store().n_chunks() as i64)?;
        self.store.forget_deltas(context, highest)?;
        self.store.set_state(context, state::BUILD, highest)?;
        self.cache.forget();
        Ok(())
    }
}

/// Returns the highest sequence a batch of pending deltas carries.
///
/// The log is read in order, so this is the last entry's sequence; it is
/// computed rather than assumed because a generation that claimed to cover a
/// sequence the log never reached would drop rows on the next fold.
/// @param pending - the delta entries about to be folded or built in
/// @param covered - what the current generation already covers
fn highest_of(pending: &[Delta], covered: i64) -> i64 {
    pending
        .iter()
        .map(|entry| entry.sequence)
        .max()
        .unwrap_or(covered)
}

impl VirtualTable for SearchTable {
    /// Returns the text columns, then the query, count, vector, recall and rank.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Chooses between a search, a rowid lookup and a scan.
    ///
    /// The query constraint is claimed with `omit` set, because the engine
    /// cannot evaluate it: `docs MATCH 'text'` means nothing outside the module,
    /// and a module that claimed it without applying it would return every row.
    /// `k`, `vector` and `recall` are claimed the same way - they are arguments,
    /// not predicates, and leaving them in the residual would have the engine
    /// compare a hit count against a column that has no value.
    fn best_index(&self, query: &mut IndexQuery) -> DbResult<()> {
        let mut roles = String::new();
        let mut searching = false;
        for index in 0..query.constraints.len() {
            let Some(spec) = query.constraints.get(index).copied() else {
                continue;
            };
            if !spec.usable {
                continue;
            }
            let matching = matches!(spec.op, ConstraintOp::Match | ConstraintOp::Eq);
            if matching && spec.column == self.query_column() {
                query.use_constraint(index, true);
                roles.push(ROLE_QUERY);
                searching = true;
            }
        }
        for index in 0..query.constraints.len() {
            let Some(spec) = query.constraints.get(index).copied() else {
                continue;
            };
            if !spec.usable || spec.op != ConstraintOp::Eq {
                continue;
            }
            if spec.column == self.limit_column() {
                query.use_constraint(index, true);
                roles.push(ROLE_LIMIT);
            } else if spec.column == self.vector_column() {
                query.use_constraint(index, true);
                roles.push(ROLE_VECTOR);
                searching = true;
            } else if spec.column == self.recall_column() {
                query.use_constraint(index, true);
                roles.push(ROLE_RECALL);
            }
        }
        if !searching {
            for index in 0..query.constraints.len() {
                let Some(spec) = query.constraints.get(index).copied() else {
                    continue;
                };
                if spec.usable && spec.op == ConstraintOp::Eq && spec.column == ROWID_COLUMN {
                    query.use_constraint(index, true);
                    query.index_number = PLAN_ROWID;
                    query.index_string = ROLE_ROWID.to_string();
                    query.estimated_cost = 1.0;
                    query.estimated_rows = 1;
                    return Ok(());
                }
            }
        }
        let mut plan = if searching { PLAN_SEARCH } else { PLAN_SCAN };
        if plan == PLAN_SEARCH {
            if let Some(order) = query.order_by.first() {
                if query.order_by.len() == 1
                    && order.column == self.rank_column()
                    && !order.descending
                {
                    query.ordered = true;
                    plan |= PLAN_RANKED;
                }
            }
        }
        query.index_number = plan;
        query.index_string = roles;
        // A search costs what a search costs and returns what it was asked for;
        // a scan of a search table is the fallback that reads every row, and the
        // planner should prefer any other path over it.
        query.estimated_cost = if searching { 25.0 } else { 1.0e6 };
        query.estimated_rows = if searching { DEFAULT_K } else { 1_000 };
        Ok(())
    }

    /// Opens a cursor.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(SearchCursor {
            options: self.options.clone(),
            store: self.store.clone(),
            cache: Arc::clone(&self.cache),
            query_column: self.query_column(),
            limit_column: self.limit_column(),
            vector_column: self.vector_column(),
            recall_column: self.recall_column(),
            rank_column: self.rank_column(),
            rows: Vec::new(),
            at: 0,
            current: None,
            request: Request::default(),
        }))
    }

    /// Writes the configuration when the table is created, and reads it back
    /// every other time.
    fn begin(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        self.pending = None;
        self.marks.clear();
        self.touched = false;
        if self.creating {
            self.creating = false;
            let rows = self.options.config_rows();
            self.store.write_config(context, &rows)?;
            self.store.set_state(context, state::SEQUENCE, 0)?;
            self.store.set_state(context, state::ORDINAL, 0)?;
            self.store.set_state(context, state::GENERATION, 0)?;
            self.store.set_state(context, state::COVERED, 0)?;
            self.store.set_state(context, state::ROWS, 0)?;
            self.store.set_state(context, state::BUILD, 0)?;
            return Ok(());
        }
        self.reconcile(context)
    }

    /// Folds the delta log in, when the transaction made it long enough.
    ///
    /// This runs before the engine writes its commit marker, so the new
    /// generation's rows and the rows that made it necessary land in one
    /// transaction. Doing it here rather than inside the `INSERT` is what keeps
    /// the cost of a write bounded and predictable: a thousand-row transaction
    /// folds once, not a thousand times.
    ///
    /// [`SearchTable::fold`] is what it calls, and it is bounded by the rows
    /// this transaction wrote. The single-pass build over the whole corpus is
    /// never reached from here.
    fn sync(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        if !self.touched {
            return Ok(());
        }
        self.fold(context)
    }

    /// Ends the transaction.
    fn commit(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.pending = None;
        self.marks.clear();
        self.touched = false;
        self.cache.forget();
        Ok(())
    }

    /// Abandons the transaction.
    ///
    /// The durable state needs nothing done to it - it is being undone by the
    /// pager, page by page, exactly as the relational rows are. What has to go
    /// is the in-memory merge, because it was built from rows that are about to
    /// stop existing.
    fn rollback(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.pending = None;
        self.marks.clear();
        self.touched = false;
        self.cache.forget();
        Ok(())
    }

    /// Remembers where the delta log stood when a savepoint opened.
    fn savepoint(&mut self, context: &mut Context<'_>, number: i32) -> DbResult<()> {
        let ordinal = self.store.state(context, state::ORDINAL)?;
        self.marks.retain(|(level, _)| *level < number);
        self.marks.push((number, ordinal));
        Ok(())
    }

    /// Forgets the marks above a released savepoint.
    fn release(&mut self, _context: &mut Context<'_>, number: i32) -> DbResult<()> {
        self.marks.retain(|(level, _)| *level < number);
        Ok(())
    }

    /// Drops the merge after a partial rollback.
    ///
    /// The rows themselves are put back by the pager's own savepoint, so there
    /// is nothing durable to undo here. The cached index, though, was built
    /// over rows that have just been withdrawn, and its key would still match
    /// if the withdrawn entries happened to be the ones it had not applied yet.
    fn rollback_to(&mut self, _context: &mut Context<'_>, number: i32) -> DbResult<()> {
        self.marks.retain(|(level, _)| *level <= number);
        self.pending = None;
        self.cache.forget();
        Ok(())
    }

    /// Checks that the rows, the log and the generation agree.
    fn integrity(&mut self, context: &mut Context<'_>) -> DbResult<Option<String>> {
        let mut problems: Vec<String> = Vec::new();
        let generation = self.store.state(context, state::GENERATION)?;
        if generation > 0 {
            match self.store.read_generation(context, generation)? {
                None => problems.push(format!("generation {generation} is named but not stored")),
                Some(bytes) => {
                    if let Err(error) = inillucent_core::persist::read_index(&mut bytes.as_slice())
                    {
                        problems.push(format!("generation {generation} is unreadable: {error}"));
                    }
                }
            }
        }
        let covered = self.store.state(context, state::COVERED)?;
        let ordinal = self.store.state(context, state::ORDINAL)?;
        if covered > ordinal {
            problems.push(format!(
                "the base generation claims to cover sequence {covered}, past the log's own {ordinal}"
            ));
        }
        let mut counted = 0i64;
        self.store.scan_rows(context, |_, row| {
            counted = counted.saturating_add(1);
            if self.options.has_vectors()
                && !row.vector.is_empty()
                && row.vector.len() != self.options.dims
            {
                problems.push(format!(
                    "a row holds {} dimensions where the table declares {}",
                    row.vector.len(),
                    self.options.dims
                ));
            }
            Ok(problems.len() < 20)
        })?;
        let recorded = self.store.state(context, state::ROWS)?;
        if recorded != counted {
            problems.push(format!(
                "the row count says {recorded} and the table holds {counted}"
            ));
        }
        for entry in self.store.deltas_above(context, covered)? {
            if entry.op == Op::Put && self.store.read_row(context, entry.id)?.is_none() {
                problems.push(format!(
                    "the log says row {} was written and it is not there",
                    entry.id
                ));
            }
            if problems.len() >= 20 {
                break;
            }
        }
        if problems.is_empty() {
            return Ok(None);
        }
        problems.truncate(20);
        Ok(Some(problems.join("; ")))
    }

    /// Applies one insert, update or delete.
    fn update(&mut self, context: &mut Context<'_>, change: &Change) -> DbResult<Option<i64>> {
        match change {
            Change::Delete(rowid) => {
                let Some(id) = rowid.as_integer() else {
                    return Ok(None);
                };
                self.remove(context, id)?;
                Ok(None)
            }
            Change::Insert { rowid, values } => {
                if let Some(command) = values
                    .get(self.query_column() as usize)
                    .filter(|value| !matches!(value, Value::Null))
                    .and_then(|value| text_of(Some(value)))
                {
                    self.command(context, &command)?;
                    return Ok(None);
                }
                let id = match rowid.as_integer() {
                    Some(id) => id,
                    None => self.store.max_row_id(context)?.saturating_add(1),
                };
                if self.store.read_row(context, id)?.is_some() {
                    return Err(constraint(
                        "UNIQUE constraint failed: the rowid is already in the index",
                    ));
                }
                let row = self.row_of(values)?;
                self.put(context, id, &row)?;
                Ok(Some(id))
            }
            Change::Update {
                old_rowid,
                new_rowid,
                values,
            } => {
                let Some(old) = old_rowid.as_integer() else {
                    return Ok(None);
                };
                let new = new_rowid.as_integer().unwrap_or(old);
                let row = self.row_of(values)?;
                if new != old {
                    self.remove(context, old)?;
                }
                let existed = self.store.read_row(context, new)?.is_some();
                self.store.write_row(context, new, &row)?;
                if !existed {
                    let rows = self.store.state(context, state::ROWS)?.saturating_add(1);
                    self.store.set_state(context, state::ROWS, rows)?;
                }
                self.log(context, new, Op::Put, row.digest())?;
                Ok(Some(new))
            }
        }
    }
}

/// One cursor over a search table.
struct SearchCursor {
    options: Options,
    store: Store,
    cache: Arc<Cache>,
    query_column: i32,
    limit_column: i32,
    vector_column: i32,
    recall_column: i32,
    rank_column: i32,
    /// The rows this cursor will produce, with their scores when it searched.
    rows: Vec<(i64, Option<Hit>)>,
    at: usize,
    current: Option<Row>,
    request: Request,
}

impl SearchCursor {
    /// Returns the row the cursor is on, reading it once.
    fn row(&mut self, context: &mut Context<'_>) -> DbResult<Row> {
        if let Some(row) = self.current.clone() {
            return Ok(row);
        }
        let Some((id, _)) = self.rows.get(self.at).copied() else {
            return Ok(Row::default());
        };
        let row = self.store.read_row(context, id)?.unwrap_or_default();
        self.current = Some(row.clone());
        Ok(row)
    }
}

impl VirtualCursor for SearchCursor {
    /// Positions the cursor on the first row of a plan.
    fn filter(&mut self, context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        self.rows.clear();
        self.at = 0;
        self.current = None;
        self.request = Request {
            limit: DEFAULT_K as usize,
            ..Request::default()
        };
        let mut wanted_rowid: Option<i64> = None;
        for (role, value) in plan.index_string.chars().zip(plan.arguments.iter()) {
            match role {
                ROLE_QUERY => self.request.text = text_of(Some(value)),
                ROLE_LIMIT => {
                    let limit = value.as_integer().unwrap_or(DEFAULT_K);
                    self.request.limit = limit.clamp(1, 1_000_000) as usize;
                }
                ROLE_VECTOR => {
                    self.request.vector = crate::store::vector_of(value, self.options.dims.max(1))?;
                }
                ROLE_RECALL => {
                    self.request.recall = value
                        .as_real()
                        .map(|real| real as f32)
                        .or_else(|| value.as_integer().map(|whole| whole as f32));
                }
                ROLE_ROWID => wanted_rowid = value.as_integer(),
                _ => {}
            }
        }
        if plan.index_number == PLAN_ROWID {
            if let Some(id) = wanted_rowid {
                if self.store.read_row(context, id)?.is_some() {
                    self.rows.push((id, None));
                }
            }
            return Ok(());
        }
        if plan.index_number & PLAN_SEARCH != 0 {
            if !self.options.has_vectors() && !self.request.vector.is_empty() {
                return Err(failure(
                    "inillucent_search: this table was declared without a vector width",
                ));
            }
            let hits = self
                .cache
                .search(context, &self.store, &self.options, &self.request)?;
            self.rows = hits.into_iter().map(|hit| (hit.id, Some(hit))).collect();
            return Ok(());
        }
        self.rows = self
            .store
            .row_ids(context)?
            .into_iter()
            .map(|id| (id, None))
            .collect();
        Ok(())
    }

    /// Moves to the next row.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        self.current = None;
        Ok(())
    }

    /// Returns whether the cursor is past the last row.
    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// Returns one column of the current row.
    fn column(&mut self, context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        let position = index as i32;
        if position == self.query_column {
            return Ok(Value::Null);
        }
        if position == self.limit_column {
            return Ok(Value::Integer(self.request.limit as i64));
        }
        if position == self.recall_column {
            return Ok(match self.request.recall {
                Some(recall) => Value::Real(f64::from(recall)),
                None => Value::Null,
            });
        }
        if position == self.rank_column {
            // Negated, so `ORDER BY rank` ascending is best-first. FTS5's own
            // `rank` is negative bm25 for exactly this reason, and having two
            // conventions in one engine would be worse than either.
            return Ok(match self.rows.get(self.at).and_then(|(_, hit)| *hit) {
                Some(hit) => Value::Real(-f64::from(hit.score)),
                None => Value::Null,
            });
        }
        let row = self.row(context)?;
        if position == self.vector_column {
            return crate::store::encode_vector(&row.vector);
        }
        match row.columns.get(index) {
            Some(text) => Value::owned_text(text.as_bytes()),
            None => Ok(Value::Null),
        }
    }

    /// Returns the current row's rowid.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self
            .rows
            .get(self.at)
            .map(|(id, _)| *id)
            .unwrap_or_default())
    }

    /// Answers one of the module's auxiliary functions on the current row.
    ///
    /// `score(docs)` is the fused score the ranking used and `confidence(docs)`
    /// is how good the hit is in absolute terms - two different questions that
    /// the same number cannot answer, which is why the engine computes both.
    /// `origin(docs)` says which branch found the row.
    fn auxiliary(
        &mut self,
        _context: &mut Context<'_>,
        name: &[u8],
        _arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        let hit = self.rows.get(self.at).and_then(|(_, hit)| *hit);
        match name.to_ascii_lowercase().as_slice() {
            b"score" => Ok(match hit {
                Some(hit) => Value::Real(f64::from(hit.score)),
                None => Value::Null,
            }),
            b"confidence" => Ok(match hit {
                Some(hit) => Value::Real(f64::from(hit.confidence)),
                None => Value::Null,
            }),
            b"origin" => Ok(match hit {
                Some(hit) => Value::owned_text(origin_name(hit.origin).as_bytes())?,
                None => Value::Null,
            }),
            other => Err(failure(format!(
                "no such function: {}",
                String::from_utf8_lossy(other)
            ))),
        }
    }
}

/// Returns the name one hit origin reports.
fn origin_name(origin: inillucent_core::rank::HitOrigin) -> &'static str {
    match origin {
        inillucent_core::rank::HitOrigin::Vector => "vector",
        inillucent_core::rank::HitOrigin::Lexical => "lexical",
        inillucent_core::rank::HitOrigin::Both => "both",
    }
}

/// Returns the text of a value, or nothing when it is NULL.
fn text_of(value: Option<&Value<'static>>) -> Option<String> {
    match value {
        Some(Value::Text(text)) => Some(String::from_utf8_lossy(text.raw()).into_owned()),
        Some(Value::Blob(blob)) => Some(String::from_utf8_lossy(blob.raw()).into_owned()),
        Some(Value::Integer(number)) => Some(number.to_string()),
        Some(Value::Real(number)) => Some(number.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hidden columns come in the order a table-valued call passes them.
    #[test]
    fn the_hidden_columns_are_the_argument_list() {
        let declared = options::parse(&[b"body".to_vec(), b"dims = 4".to_vec()]).expect("parsed");
        let declaration = declaration_of(&declared, b"docs");
        let names: Vec<String> = declaration
            .columns
            .iter()
            .map(|column| String::from_utf8_lossy(&column.name).into_owned())
            .collect();
        assert_eq!(names, vec!["body", "docs", "k", "vector", "recall", "rank"]);
        assert!(!declaration.columns.first().expect("body").hidden);
        assert!(declaration.columns.get(1).expect("docs").hidden);
    }

    /// A module names itself the same way the SQL that creates it does.
    #[test]
    fn the_module_is_named_inillucent_search() {
        assert_eq!(SearchModule.name(), "inillucent_search");
        assert!(!SearchModule.eponymous());
        assert!(SearchModule.constructible());
    }
}
