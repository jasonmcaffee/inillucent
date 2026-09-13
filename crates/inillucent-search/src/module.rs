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

use std::collections::BTreeSet;
use std::sync::Arc;

use inillucent_base::DbResult;
use inillucent_value::Value;

use inillucent_ext::vtab::{
    constraint, failure, Change, ConstraintOp, Context, Declaration, DeclaredColumn, FilterPlan,
    IndexQuery, Module, ModuleArguments, ShadowTable, VirtualCursor, VirtualTable, ROWID_COLUMN,
};

use crate::merge::{self, Cache, Hit, Request};
use crate::options::{self, Options, DEFAULT_K};
use crate::store::{state, Delta, MergeState, Op, Row, SegmentMeta, Store};

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
                let live = merge::live_segments(context, &self.store)?;
                let mut ids: Vec<i64> = live.iter().map(|segment| segment.id).collect();
                // Every in-flight merge's checkpoint is a real segment that
                // is not yet named by the manifest - dropping one here would
                // strand its `state::MERGE` entry pointing at a `%_gen` id no
                // longer there, and the next commit's resume would fail to
                // load it. Since task-1911's segment delta format, that
                // checkpoint can itself be a chain of small links each
                // pointing at the one before it, so the whole chain has to
                // be protected, not only the tip `MergeState` names.
                for merge_state in self.store.read_merge_states(context)? {
                    ids.extend(merge::chain_ids(
                        context,
                        &self.store,
                        merge_state.accumulator,
                    )?);
                }
                self.store.drop_generations_except(context, &ids)?;
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

    /// Allocates a fresh, never before used `%_gen` key for a new segment.
    ///
    /// Separate from [`state::GENERATION`], which counts build events rather
    /// than storage slots - see the constant's own doc comment for why a
    /// merge must never touch that counter. A table with no
    /// [`state::SEGMENT_ID`] row yet is one of two things: brand new, in
    /// which case the first id this hands out is `1`, exactly as
    /// `GENERATION` used to allocate it directly; or migrating up from a
    /// single generation, in which case the highest number already sitting in
    /// `%_gen` - which a table that never ran `drop-old-generations` can still
    /// hold several of - has to be read back so the first new segment cannot
    /// collide with one of them.
    /// @param context - the module's reach into the database
    fn next_segment_id(&mut self, context: &mut Context<'_>) -> DbResult<i64> {
        let seeded = self.store.state(context, state::SEGMENT_ID)?;
        let seed = if seeded == 0 {
            let from_counter = self.store.state(context, state::GENERATION)?;
            let from_table = self
                .store
                .generations(context)?
                .into_iter()
                .max()
                .unwrap_or(0);
            from_counter.max(from_table)
        } else {
            seeded
        };
        let next = seed.saturating_add(1);
        self.store.set_state(context, state::SEGMENT_ID, next)?;
        Ok(next)
    }

    /// Flushes the delta log into a brand new segment, at commit.
    ///
    /// **No existing segment is read or rewritten here.** The pending batch is
    /// built into an index of its own - one insert per row the batch touches,
    /// not one per row the table holds - and appended to the manifest as a new
    /// level zero segment. This is the change task-1911 makes: before it, a
    /// commit that crossed the threshold loaded the single published
    /// generation and inserted the batch into it, which bounded the graph work
    /// but not the bytes, because publishing meant re-serialising the whole
    /// generation however few rows had changed
    /// (`docs/relational-architecture.md#10-keeping-a-vector-index-current`).
    /// A flush's bytes are the batch's own segment, so publishing here costs
    /// the batch on both counts.
    ///
    /// Every write below is inside the caller's transaction: the segment's
    /// bytes land before the manifest names it, so a crash between the two
    /// leaves bytes nothing reads rather than a manifest entry pointing at
    /// nothing - the same ordering the single generation this replaces always
    /// kept. [`Self::merge_cascade`] runs immediately after, still inside this
    /// commit, because a level that has just grown to its cap is exactly the
    /// level this transaction is responsible for shrinking back down.
    /// @param context - the module's reach into the database
    fn flush(&mut self, context: &mut Context<'_>) -> DbResult<()> {
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
        let (index, inserted, tombstoned) =
            merge::build_segment_from_batch(context, &self.store, &self.options, &pending)?;
        let id = self.next_segment_id(context)?;
        let chunks = index.store().n_chunks() as i64;
        let mut bytes = Vec::new();
        inillucent_core::persist::write_index(&index, &mut bytes).map_err(|error| {
            failure(format!(
                "inillucent_search: cannot write a segment: {error}"
            ))
        })?;
        self.store.write_generation(context, id, &bytes)?;
        let mut segments = merge::live_segments(context, &self.store)?;
        segments.push(SegmentMeta {
            id,
            level: 0,
            covers_from: covered,
            covers_to: highest,
            chunks,
            tombstoned,
        });
        self.store.write_segments(context, &segments)?;
        let generation = self
            .store
            .state(context, state::GENERATION)?
            .saturating_add(1);
        self.store
            .set_state(context, state::GENERATION, generation)?;
        let folds = self.store.state(context, state::FOLDS)?.saturating_add(1);
        self.store.set_state(context, state::FOLDS, folds)?;
        self.store
            .set_state(context, state::INSERTED, inserted as i64)?;
        self.store.set_state(context, state::COVERED, highest)?;
        self.store
            .set_state(context, state::CHUNKS, total_chunks(&segments))?;
        self.store.forget_deltas(context, highest)?;
        self.store.set_state(context, state::BUILD, highest)?;
        self.cache.forget();
        self.merge_cascade(context)
    }

    /// Advances segment merging by at most one commit's worth of work,
    /// resuming every merge left checkpointed before starting any new one.
    ///
    /// **Why a level at all, rather than merging every live segment on every
    /// flush.** A query folds every live segment together, so the count of
    /// live segments is the count a query pays to reconstruct its answer from
    /// scratch, and merging on every flush would hold that count at one -
    /// which is exactly the whole-corpus rewrite this ticket removes,
    /// happening again under a different name. Bucketing by level bounds the
    /// live count instead of collapsing it: [`Options::segment_fanin`]
    /// segments accumulate at level zero before they become one segment at
    /// level one, that level accumulates the same number before becoming one
    /// at level two, and so on - `automerge` in FTS5's own vocabulary, and the
    /// same size tiered shape an LSM tree merges its own levels with.
    ///
    /// **Bounded by chunks, never by a clock.** A commit's own share of merge
    /// work is [`Options::merge_budget_chunks`], shared across every level
    /// that needs it and spent one input segment at a time by
    /// [`Self::continue_merge`] - a segment folds for what its own
    /// [`SegmentMeta::chunks`] already says it costs, so the budget never has
    /// to load a segment just to find out what it is worth. A merge that
    /// cannot finish inside its share checkpoints the accumulator it has
    /// built so far under a fresh `%_gen` id, records how far it got in
    /// [`state::MERGE`], and this function moves on to the next candidate -
    /// the next commit that reaches here resumes that exact state rather than
    /// starting the level over. The one thing that ignores the budget is a
    /// level whose segment count has reached [`Options::crisis_at`]: it runs
    /// to completion in this commit regardless, because a level that far
    /// behind is already costing every query more than one expensive commit
    /// costs once - see that method's own doc comment for the number.
    ///
    /// **More than one level can be merging at once, and this is not a
    /// corner case - it is what made the tail worth fixing in the first
    /// place.** An earlier version of this function tracked a single
    /// in-flight merge and always resumed it before considering anything
    /// else, which meant a large merge several levels up - the exact case the
    /// budget exists to spread out - blocked every *lower* level's own merge
    /// for as many commits as the big one took to finish. Fresh flushes kept
    /// landing at level zero the whole time, so that level's own segment
    /// count climbed toward its own crisis threshold while the bounded
    /// mechanism sat idle for it, which is why the worst commit measured on
    /// the 100,000 document arm of `write_latency` came down from 49 s to
    /// about 16 s under that version and no further - it had cut the single
    /// worst merge down but was still occasionally forced into an unbounded
    /// crisis merge for a *different* level that starved in the meantime.
    /// Advancing every checkpointed merge first, lowest level before higher,
    /// closes that: level zero is never blocked on level two's progress.
    ///
    /// **Invisible to `GENERATION`, `FOLDS` and `INSERTED`.** Those three count
    /// *build events an application asked for or that a write triggered
    /// directly* - a flush, a `compact`, a `rebuild` - and a merge is none of
    /// those; it is bookkeeping this table would eventually have needed
    /// regardless of which batch triggered it. Folding a merge into those
    /// counters would make `folds` depend on `segment_fanin`, which nothing
    /// about a fold count should.
    /// @param context - the module's reach into the database
    fn merge_cascade(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        let mut budget = self.options.merge_budget_chunks();
        let mut spent: u64 = 0;
        let crisis_at = self.options.crisis_at();

        // Phase one: advance every merge already checkpointed, lowest source
        // level first, so a level that has been waiting longest is not
        // starved by a bigger one still in progress above it. Segment counts
        // are re-read each time through the loop rather than once up front,
        // because an earlier iteration finishing can itself add a fresh
        // segment to a level a later iteration is about to judge.
        let mut in_flight = self.store.read_merge_states(context)?;
        in_flight.sort_by_key(|state| state.source_level);
        let mut kept: Vec<MergeState> = Vec::with_capacity(in_flight.len());
        for mut state in in_flight {
            let segments = merge::live_segments(context, &self.store)?;
            let crisis = count_at(&segments, state.source_level) >= crisis_at;
            if self.continue_merge(context, &mut state, &mut budget, &mut spent, crisis)? {
                self.finish_merge(context, state)?;
            } else {
                kept.push(state);
            }
        }

        // Phase two: start a new merge for any level whose *unclaimed*
        // segments - the ones no merge kept from phase one already owns -
        // have reached `segment_fanin`, lowest level first, until nothing
        // more qualifies or there is nothing left to spend. A level a merge
        // is already resuming can still start a second one once enough fresh
        // segments have landed on it since that merge began; the two never
        // fight over the same input because `begin_merge` only ever looks at
        // what is not already claimed.
        loop {
            let claimed: BTreeSet<i64> = kept
                .iter()
                .flat_map(|state| state.inputs.iter().map(|segment| segment.id))
                .collect();
            let segments = merge::live_segments(context, &self.store)?;
            let fanin = self.options.segment_fanin();
            let mut levels: Vec<i32> = segments
                .iter()
                .filter(|segment| !claimed.contains(&segment.id))
                .map(|segment| segment.level)
                .collect();
            levels.sort_unstable();
            levels.dedup();
            let Some(level) = levels.into_iter().find(|level| {
                segments
                    .iter()
                    .filter(|segment| segment.level == *level && !claimed.contains(&segment.id))
                    .count()
                    >= fanin
            }) else {
                break;
            };
            let crisis = count_at(&segments, level) >= crisis_at;
            if !crisis && budget == 0 {
                break;
            }
            let mut state = self.begin_merge(context, level, &claimed)?;
            if self.continue_merge(context, &mut state, &mut budget, &mut spent, crisis)? {
                self.finish_merge(context, state)?;
            } else {
                kept.push(state);
            }
        }

        // `state::MERGE_WORK` is what a test reads back to check the bound
        // was respected without a clock - see its own doc comment.
        self.store
            .set_state(context, state::MERGE_WORK, spent as i64)?;
        self.store.write_merge_states(context, &kept)
    }

    /// Starts a merge of one level's unclaimed segments, without folding any
    /// of them yet.
    ///
    /// The oldest unclaimed input becomes the accumulator directly and free
    /// of charge: `folded` starts at one and `accumulator` names that
    /// input's own existing `%_gen` id rather than a fresh copy of it,
    /// because that input's bytes already are exactly what a merge with
    /// nothing folded into it yet would be. [`Self::continue_merge`] is what
    /// does the paid work of folding the rest.
    /// @param context - the module's reach into the database
    /// @param level - the level to collapse
    /// @param claimed - segment ids another in-flight merge already owns,
    ///   left alone so two merges never fold the same input
    fn begin_merge(
        &mut self,
        context: &mut Context<'_>,
        level: i32,
        claimed: &BTreeSet<i64>,
    ) -> DbResult<MergeState> {
        let segments = merge::live_segments(context, &self.store)?;
        let mut inputs: Vec<SegmentMeta> = segments
            .into_iter()
            .filter(|segment| segment.level == level && !claimed.contains(&segment.id))
            .collect();
        inputs.sort_by_key(|segment| segment.covers_from);
        let accumulator = inputs.first().map(|segment| segment.id).unwrap_or(0);
        Ok(MergeState {
            source_level: level,
            target_level: level.saturating_add(1),
            inputs,
            folded: 1,
            accumulator,
        })
    }

    /// Folds as many of an in-flight merge's remaining inputs into its
    /// accumulator as the budget allows, checkpointing the result under a
    /// fresh `%_gen` id, and returns whether every input is now folded in.
    ///
    /// **At least one input folds even with no budget left** - the check is
    /// only made after the first fold of this call - so a merge that keeps
    /// getting resumed always moves forward rather than stalling on a commit
    /// that had nothing left to spend, the same progress guarantee an LSM's
    /// own bounded merge makes. A crisis merge never makes that check at all,
    /// which is what lets it run to completion in one call without a second
    /// code path: `crisis` simply disables the early exit.
    ///
    /// **The checkpoint this writes is bounded by what this call folded, not
    /// by the accumulator's total size.** Before this (task-1911's own
    /// follow-on), a checkpoint reloaded the accumulator into memory and then
    /// re-serialised the *whole thing* with `persist::write_index`, however
    /// little this call added to it - so a merge several levels up, whose
    /// accumulator is already a large fraction of the corpus, paid that
    /// whole size on every checkpoint regardless of how finely the fold
    /// itself was budgeted. `merge::fold_segment_recording` instead records
    /// exactly the chunks, vectors and tombstones this call folded, and
    /// `persist::write_segment_delta` writes only those - plus a pointer to
    /// the accumulator this checkpoint continues - as a new, small link in a
    /// chain. Reading the accumulator back (`merge::load_segment_resumable`,
    /// just below) still costs the chain's full accumulated size, the same
    /// as reloading a single blob always did; what changed is that writing a
    /// checkpoint no longer does.
    ///
    /// The chain grows by at most one link per call to this function, and a
    /// merge calls it at most [`Options::segment_fanin`] times over its whole
    /// life, so a segment's chain depth is bounded the same way the number of
    /// checkpoints already was - see `write_segment_delta`'s own doc comment
    /// for why the chain never needs flattening back into one blob.
    /// @param context - the module's reach into the database
    /// @param state - the merge being advanced, updated in place
    /// @param budget - how many more chunks this commit may still fold,
    ///   debited as they are spent (saturates at zero, so it only answers
    ///   "is there any left", not "how much was spent")
    /// @param spent - every chunk actually folded this commit, across every
    ///   merge advanced, added up uncapped - what `state::MERGE_WORK` reports
    /// @param crisis - whether this merge ignores the budget and runs to
    ///   completion regardless
    fn continue_merge(
        &mut self,
        context: &mut Context<'_>,
        state: &mut MergeState,
        budget: &mut u64,
        spent: &mut u64,
        crisis: bool,
    ) -> DbResult<bool> {
        if state.folded >= state.inputs.len() {
            return Ok(true);
        }
        let previous = state.accumulator;
        let mut accumulator =
            merge::load_segment_resumable(context, &self.store, &self.options, previous)?;
        // Recording turns this checkpoint's real fold - the one below, the
        // only expensive part of any of this - into content this checkpoint
        // can write out directly. Nothing here re-runs it: the graph and the
        // lexical index are copied from what recording captured, never
        // reinserted or re-tokenised on a later reload. See
        // `inillucent_core::persist`'s segment delta section for why that
        // distinction is the whole point.
        let before_nodes = accumulator.graph_shape().1;
        accumulator.start_recording();
        let mut batches: Vec<inillucent_core::persist::DeltaBatch> = Vec::new();
        let mut folded_any = false;
        while state.folded < state.inputs.len() {
            if folded_any && !crisis && *budget == 0 {
                break;
            }
            // The loop's own condition proves the index is in range; `get`
            // says it in the form the compiler keeps true (task-1932, H9 -
            // reported once `inillucent-ext` was made to deny the four lints,
            // because clippy then walked this crate too).
            let Some(input) = state.inputs.get(state.folded).cloned() else {
                break;
            };
            let segment = merge::load_segment(context, &self.store, &self.options, input.id)?;
            let (_inserted, recorded) =
                merge::fold_segment_recording(&mut accumulator, &segment, &input.tombstoned)?;
            batches.push(inillucent_core::persist::DeltaBatch {
                puts: recorded.puts,
                tombstoned: recorded.tombstoned,
            });
            let cost = input.chunks.max(0) as u64;
            *budget = budget.saturating_sub(cost);
            *spent = spent.saturating_add(cost);
            state.folded = state.folded.saturating_add(1);
            folded_any = true;
        }
        let done = state.folded >= state.inputs.len();
        let (entry, _, layers_len) = accumulator.graph_shape();
        let graph = inillucent_core::persist::GraphRecording {
            entry,
            layers_len,
            node_top_tail: accumulator.graph_node_top_tail(before_nodes),
            touched: accumulator.drain_graph_recording(),
        };
        let lexical = accumulator.drain_lexical_recording();
        let seal = done.then(|| {
            (
                accumulator.store().n_chunks() as u64,
                accumulator.store().n_documents() as u64,
            )
        });
        let mut bytes = Vec::new();
        inillucent_core::persist::write_segment_delta(
            &mut bytes,
            Some(previous),
            &batches,
            &graph,
            lexical.as_ref(),
            seal,
        )
        .map_err(|error| {
            failure(format!(
                "inillucent_search: cannot write a segment delta: {error}"
            ))
        })?;
        let id = self.next_segment_id(context)?;
        self.store.write_generation(context, id, &bytes)?;
        state.accumulator = id;
        Ok(done)
    }

    /// Publishes a finished merge's accumulator as the live segment at its
    /// target level, in place of every input it replaced, and clears the
    /// resumable state.
    ///
    /// Recomputes the tombstoned list here by reloading every original input
    /// fresh ([`merge::touched_and_dead`]), rather than carrying one forward
    /// from `continue_merge`: those inputs are untouched on disk until this
    /// moment - only the manifest names what is live, and every original
    /// input stays named there until this call runs - so the reload costs
    /// exactly what re-reading a level's own segments once already costs, and
    /// nothing is gained by threading a running set through every checkpoint
    /// instead.
    ///
    /// **Read safety across the swap.** Everything up to and including
    /// `write_segments` below runs inside the caller's transaction, so a
    /// crash before it leaves every original input still named and the
    /// finished accumulator an orphaned, unnamed segment nothing reads; a
    /// crash after it leaves the new segment named and the originals simply
    /// unreferenced, cleaned up later by `drop-old-generations`. A query
    /// reading concurrently through [`merge::live_segments`] therefore always
    /// sees one manifest or the other, in full, never a mixture that drops or
    /// doubles a row.
    /// @param context - the module's reach into the database
    /// @param state - the finished merge
    fn finish_merge(&mut self, context: &mut Context<'_>, state: MergeState) -> DbResult<()> {
        let accumulator =
            merge::load_segment(context, &self.store, &self.options, state.accumulator)?;
        let mut originals = Vec::with_capacity(state.inputs.len());
        for input in &state.inputs {
            let loaded = merge::load_segment(context, &self.store, &self.options, input.id)?;
            originals.push((input.clone(), loaded));
        }
        let tombstoned = merge::touched_and_dead(&accumulator, &originals);
        let chunks = accumulator.store().n_chunks() as i64;
        let covers_from = state
            .inputs
            .iter()
            .map(|segment| segment.covers_from)
            .min()
            .unwrap_or(0);
        let covers_to = state
            .inputs
            .iter()
            .map(|segment| segment.covers_to)
            .max()
            .unwrap_or(0);
        let merged_ids: Vec<i64> = state.inputs.iter().map(|segment| segment.id).collect();
        let mut segments = merge::live_segments(context, &self.store)?;
        segments.retain(|segment| !merged_ids.contains(&segment.id));
        segments.push(SegmentMeta {
            id: state.accumulator,
            level: state.target_level,
            covers_from,
            covers_to,
            chunks,
            tombstoned,
        });
        self.store.write_segments(context, &segments)?;
        self.store
            .set_state(context, state::CHUNKS, total_chunks(&segments))?;
        // `state::MERGE` itself is not touched here: `merge_cascade` is what
        // owns the list of every in-flight merge, and this one's absence
        // from it is simply this merge never being added back to `kept`.
        self.cache.forget();
        Ok(())
    }

    /// Builds a new segment in one pass over every row, discarding every
    /// other live segment.
    ///
    /// **This is the batch rebuild workflow, and it is explicit.** It is what
    /// `INSERT INTO docs(docs) VALUES('compact')` runs, and it costs the whole
    /// corpus: every row is read, every chunk is inserted into a fresh graph,
    /// and the tombstoned chunks that folding and merging both leave behind
    /// are gone. Those chunks going away is what an application is buying
    /// when it schedules this, and it is the one operation that still
    /// collapses the whole manifest to one segment - segmented generations
    /// bound an ordinary write's cost, they do not change what a caller who
    /// explicitly asks for the clean graph gets.
    ///
    /// It is a write like any other, so it lands in the caller's transaction
    /// and is atomic with it: the new segment is either the whole manifest or
    /// the old one still is, and a crash leaves the old one in place.
    ///
    /// **An empty delta log is not a reason to refuse.** A table whose log has
    /// just been flushed away is exactly the table whose graph has the most
    /// tombstoned chunks in it, and refusing there would leave an application
    /// no way to ask for the clean graph at all.
    /// @param context - the module's reach into the database
    fn compact(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        let covered = self.store.state(context, state::COVERED)?;
        let pending = self.store.deltas_above(context, covered)?;
        let highest = highest_of(&pending, covered);
        let (index, rows) = merge::build_from_rows(context, &self.store, &self.options)?;
        self.store.set_state(context, state::ROWS, rows as i64)?;
        self.publish_full(context, &index, highest, rows)
    }

    /// Rebuilds the whole index from the rows, discarding every generation.
    ///
    /// The recovery path when a segment is unreadable, and the way an index
    /// built by an older layout is brought forward. `%_content` is the
    /// authoritative copy of every row, so this needs nothing the database does
    /// not already hold.
    fn rebuild(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        let (index, rows) = merge::build_from_rows(context, &self.store, &self.options)?;
        let ordinal = self.store.state(context, state::ORDINAL)?;
        self.store.set_state(context, state::ROWS, rows as i64)?;
        self.publish_full(context, &index, ordinal, rows)
    }

    /// Writes one built index out as the sole live segment, replacing the
    /// whole manifest.
    ///
    /// Used by `compact` and `rebuild`, and only by them - a flush appends a
    /// segment and a merge replaces a level's worth, but neither ever throws
    /// away a segment it was not itself built from, because a snapshot opened
    /// before either ran may still be reading one. This is the one path that
    /// deliberately does, because collapsing to a single clean segment is the
    /// entire point of asking for it. The order matters: the segment's rows
    /// are written before any state row names them, so a crash between the two
    /// leaves rows nothing reads rather than a manifest pointing at a segment
    /// that is not there.
    /// @param context - the module's reach into the database
    /// @param index - the index to publish
    /// @param highest - the delta sequence this segment now covers
    /// @param inserted - how many chunks the build inserted into the graph
    fn publish_full(
        &mut self,
        context: &mut Context<'_>,
        index: &inillucent_core::index::Index,
        highest: i64,
        inserted: usize,
    ) -> DbResult<()> {
        let mut bytes = Vec::new();
        inillucent_core::persist::write_index(index, &mut bytes).map_err(|error| {
            failure(format!(
                "inillucent_search: cannot write a generation: {error}"
            ))
        })?;
        let id = self.next_segment_id(context)?;
        self.store.write_generation(context, id, &bytes)?;
        let chunks = index.store().n_chunks() as i64;
        let segments = vec![SegmentMeta {
            id,
            level: 0,
            covers_from: 0,
            covers_to: highest,
            chunks,
            tombstoned: Vec::new(),
        }];
        self.store.write_segments(context, &segments)?;
        let generation = self
            .store
            .state(context, state::GENERATION)?
            .saturating_add(1);
        self.store
            .set_state(context, state::GENERATION, generation)?;
        self.store.set_state(context, state::COVERED, highest)?;
        self.store
            .set_state(context, state::INSERTED, inserted as i64)?;
        self.store.set_state(context, state::FOLDS, 0)?;
        self.store.set_state(context, state::CHUNKS, chunks)?;
        self.store.forget_deltas(context, highest)?;
        self.store.set_state(context, state::BUILD, highest)?;
        // Every merge in flight was only ever collapsing part of the
        // manifest this call just replaced whole - its inputs may no longer
        // exist by the time a later commit would have resumed it, so the
        // resumable state has to go with them rather than be left dangling.
        self.store.clear_merge_states(context)?;
        self.cache.forget();
        Ok(())
    }
}

/// Returns how many segments a manifest holds at one level.
/// @param segments - the live manifest
/// @param level - the level to count
fn count_at(segments: &[SegmentMeta], level: i32) -> usize {
    segments
        .iter()
        .filter(|segment| segment.level == level)
        .count()
}

/// Returns the aggregate chunk count `state::CHUNKS` reports: the sum of
/// every live segment's own count.
/// @param segments - the live manifest
fn total_chunks(segments: &[SegmentMeta]) -> i64 {
    segments.iter().map(|segment| segment.chunks).sum()
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

    /// Flushes the delta log into a new segment, when the transaction made it
    /// long enough.
    ///
    /// This runs before the engine writes its commit marker, so the new
    /// segment's rows and the rows that made it necessary land in one
    /// transaction. Doing it here rather than inside the `INSERT` is what keeps
    /// the cost of a write bounded and predictable: a thousand-row transaction
    /// flushes once, not a thousand times.
    ///
    /// [`SearchTable::flush`] is what it calls, and it is bounded by the rows
    /// this transaction wrote. The single-pass build over the whole corpus is
    /// never reached from here.
    fn sync(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        if !self.touched {
            return Ok(());
        }
        self.flush(context)
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

    /// Checks that the rows, the log and every live segment agree.
    fn integrity(&mut self, context: &mut Context<'_>) -> DbResult<Option<String>> {
        let mut problems: Vec<String> = Vec::new();
        for segment in merge::live_segments(context, &self.store)? {
            // Goes through the strict reader rather than `persist::read_index`
            // directly, because a live segment's own bytes may now be a
            // segment delta chain (task-1911) - `read_index` only understands
            // the older, monolithic stream, and would misreport every merged
            // segment as unreadable rather than checking what it actually is.
            if let Err(error) = merge::load_segment(context, &self.store, &self.options, segment.id)
            {
                problems.push(format!("segment {} is unreadable: {error}", segment.id));
            }
        }
        for merge_state in self.store.read_merge_states(context)? {
            if self
                .store
                .read_generation(context, merge_state.accumulator)?
                .is_none()
            {
                problems.push(format!(
                    "an in-flight merge's checkpoint {} is named but not stored",
                    merge_state.accumulator
                ));
            }
            if merge_state.folded > merge_state.inputs.len() {
                problems.push(format!(
                    "an in-flight merge claims {} inputs folded of only {}",
                    merge_state.folded,
                    merge_state.inputs.len()
                ));
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
