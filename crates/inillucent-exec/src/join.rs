//! Joins, materialisation, and the row store the two share.
//!
//! Invariant: a joined row owns its values. A batch borrows the leaf it came
//! from and a join combines rows from two leaves that are not pinned at the
//! same moment, so the combination has to be materialised - there is no
//! arrangement of the borrow that avoids it, and pretending otherwise is how a
//! use-after-evict gets written. What the design *can* avoid is materialising
//! the side that does not need it, and it does: the probe side of a hash join
//! and the outer side of a nested loop both stay in their batches until a match
//! is found.
//!
//! ## The three join strategies, and when each is right
//!
//! | operator | when the physical pass chooses it |
//! |---|---|
//! | [`IndexNestedLoopJoin`] | the inner side has an index on the join key and the outer side is small - a selective join |
//! | [`HashJoin`] | no usable index, or the outer side is large enough that a build pays for itself |
//! | [`NestedLoopJoin`] | neither, and a cross product or a correlated condition is what is left |
//!
//! Both `read.join` workloads in the scorecard are index nested loops, and that
//! is not an accident of the harness: they are `WHERE main_table.id = ?1` and
//! `WHERE main_table.key BETWEEN ?1 AND ?1 + 200` against a side table indexed
//! on the join column, so the outer side is one row or two hundred and the
//! inner has an index. SQLite chooses the same shape. A hash join over two
//! hundred outer rows would build a hash table nobody needed, which is why the
//! cardinality threshold in the physical pass exists and why it is measured
//! rather than assumed.
//!
//! ## What an index nested loop copies, which is nothing
//!
//! The first version materialised every joined row: one `Vec` for the outer
//! half, another for the concatenation, and - because `range.lookaside` reads
//! `label` - a 45-byte string copied per row. It measured **765 ns per row**
//! against a 444 ns point probe, so more than a third of the time was the
//! copying.
//!
//! It now pushes a batch whose *outer* columns are constant vectors and whose
//! *inner* columns borrow the leaf the probe landed on. A constant vector is
//! exactly the right shape for the outer row of a join - one value repeated for
//! every match - and the inner side stays a real vector with a selection over
//! the matching span, so a prefix probe that finds forty entries is one batch
//! rather than forty rows. Nothing is copied at all.
//!
//! The outer side was always vectorised. What changed is that the *output* is
//! too.

use std::collections::HashMap;

use inillucent_base::DbResult;
use inillucent_pool::Pool;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::key;
use inillucent_tree::leaf::Hit;
use inillucent_tree::PagedTree;

use crate::batch::{Batch, Vector};
use crate::expr::Eval;
use crate::ops::{emit_rows, Flow, Sink};
use crate::scan::Projection;

/// How many join-key columns fit on the stack.
///
/// Four covers every index the dialect's own corpus builds and every one in the
/// scorecard fixture. A wider key spills to the heap, which is correct and only
/// costs what the previous version cost on every key.
const INLINE_KEYS: usize = 4;

/// How many joined columns a probe's column list keeps on the stack.
///
/// The scorecard's widest joined row is seven vectors - five table columns and
/// two index ones - and the array is filled in whether the row needs every slot
/// or not, so the size is a cost rather than a ceiling: a `Vector` is about
/// seventy bytes, and twelve slots was most of a kilobyte of stack stores per
/// probed row. A wider row spills to the heap, which costs what every row used
/// to cost.
const INLINE_COLUMNS: usize = 8;

/// A buffer of materialised rows, emitted as batches.
///
/// The shared machinery under [`Materialize`], the build side of
/// [`HashJoin`] and the output of every nested loop.
#[derive(Default)]
pub struct RowStore {
    rows: Vec<Vec<OwnedDatum>>,
}

impl RowStore {
    /// Returns an empty store.
    pub fn new() -> RowStore {
        RowStore { rows: Vec::new() }
    }

    /// Returns how many rows are held.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Reports whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Returns the rows.
    pub fn rows(&self) -> &[Vec<OwnedDatum>] {
        &self.rows
    }

    /// Returns the rows, consuming the store.
    ///
    /// A pipeline breaker that rewrites its rows - the window operator widens
    /// each one - wants them owned rather than borrowed, and there is no reason
    /// to copy a buffer that is about to be dropped.
    pub fn into_rows(self) -> Vec<Vec<OwnedDatum>> {
        self.rows
    }

    /// Adds one row.
    ///
    /// @param row - the row to keep
    pub fn push(&mut self, row: Vec<OwnedDatum>) {
        self.rows.push(row);
    }

    /// Forgets every row, so the operator holding this can run again.
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    /// Copies every live row of a batch into the store.
    ///
    /// @param batch - the batch to absorb
    pub fn absorb(&mut self, batch: &Batch<'_>) -> DbResult<()> {
        // **Every pipeline breaker that buffers a batch comes through here**,
        // which is why the charge is here rather than in each of them: the
        // window operator's partition buffer, the automatic index's inner side
        // and the sort's input are all this one copy (task-1932, H6).
        inillucent_base::budget::materialise(crate::ops::batch_bytes(batch))?;
        let width = batch.columns.len();
        self.rows.reserve(batch.live());
        for nth in 0..batch.live() {
            let mut row = Vec::with_capacity(width);
            for column in 0..width {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            self.rows.push(row);
        }
        Ok(())
    }

    /// Empties the store into a sink, in batches.
    ///
    /// @param downstream - what to push into
    pub fn drain_into(&mut self, downstream: &mut dyn Sink) -> DbResult<Flow> {
        let rows = std::mem::take(&mut self.rows);
        emit_rows(&rows, downstream)
    }
}

/// Buffers its input and replays it, which is what a CTE or an `IN (SELECT)`
/// set needs.
///
/// A pipeline breaker with no other job. It exists as its own operator rather
/// than as a special case of `Sort` because a subquery whose rows are read more
/// than once should not have to be re-run, and because `EXPLAIN` should say
/// that the plan materialised something.
pub struct Materialize<'s> {
    store: RowStore,
    downstream: Box<dyn Sink + 's>,
}

impl<'s> Materialize<'s> {
    /// Returns a materialiser.
    ///
    /// @param downstream - what to replay the rows into
    pub fn new(downstream: Box<dyn Sink + 's>) -> Materialize<'s> {
        Materialize {
            store: RowStore::new(),
            downstream,
        }
    }
}

impl Sink for Materialize<'_> {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        self.store.absorb(batch)?;
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        self.store.drain_into(self.downstream.as_mut())?;
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.store.clear();
        self.downstream.reset()
    }
}

/// Literal rows, as `VALUES (...), (...)` produces them.
pub struct ValuesScan {
    rows: Vec<Vec<OwnedDatum>>,
}

impl ValuesScan {
    /// Returns a source over literal rows.
    ///
    /// @param rows - the rows, already evaluated
    pub fn new(rows: Vec<Vec<OwnedDatum>>) -> ValuesScan {
        ValuesScan { rows }
    }

    /// Pushes every row downstream.
    ///
    /// @param downstream - the head of the operator chain
    pub fn run(&self, downstream: &mut dyn Sink) -> DbResult<()> {
        self.run_without_finish(downstream)?;
        downstream.finish()
    }

    /// Pushes every row downstream and leaves the chain open.
    ///
    /// For an operator that several sources feed in turn - a compound query's
    /// arms into one set operation - where finishing after the first source
    /// would tell the chain the input had ended when it had not.
    ///
    /// @param downstream - the head of the operator chain
    pub fn run_without_finish(&self, downstream: &mut dyn Sink) -> DbResult<()> {
        emit_rows(&self.rows, downstream)?;
        Ok(())
    }
}

/// A hash table over encoded join keys.
///
/// The key is the memcmp encoding rather than the values, for the same reason
/// an interior page's separator is: one `Vec<u8>` hashes and compares in one
/// call where a tuple of tagged values dispatches per column. The encoding is
/// exact - `inillucent-tree`'s key module documents how - so two rows hash together
/// exactly when SQL says their keys are equal.
struct HashTable {
    /// Encoded key to the positions in `rows` that carry it.
    index: HashMap<Vec<u8>, Vec<u32>>,
    /// The build side's rows.
    rows: Vec<Vec<OwnedDatum>>,
    /// Which build rows have been matched, for a right/full outer join.
    matched: Vec<bool>,
}

impl HashTable {
    /// Returns an empty table.
    fn new() -> HashTable {
        HashTable {
            index: HashMap::new(),
            rows: Vec::new(),
            matched: Vec::new(),
        }
    }

    /// Forgets every build row, so the join can be run again.
    fn clear(&mut self) {
        self.index.clear();
        self.rows.clear();
        self.matched.clear();
    }

    /// Adds one build row under an encoded key.
    ///
    /// A NULL in the key is dropped rather than indexed: SQL equality is never
    /// true for NULL, so a NULL-keyed build row can never match a probe, and
    /// keeping it in the table would only make it findable.
    ///
    /// @param key - the encoded join key
    /// @param row - the build row
    /// @param has_null - whether any key column was NULL
    fn insert(&mut self, key: Vec<u8>, row: Vec<OwnedDatum>, has_null: bool) {
        let position = self.rows.len() as u32;
        self.rows.push(row);
        self.matched.push(false);
        if has_null {
            return;
        }
        self.index.entry(key).or_default().push(position);
    }

    /// Returns the build rows carrying an encoded key.
    ///
    /// @param key - the encoded probe key
    fn probe(&self, key: &[u8]) -> &[u32] {
        self.index.get(key).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// How a join treats rows that find no partner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinKind {
    /// Only matching pairs.
    Inner,
    /// Every probe row, null-extended when it matches nothing.
    Left,
    /// Every *build* row, null-extended when it matches nothing.
    ///
    /// Only [`NestedLoopJoin`] answers this one, and the reason is the shape of
    /// that operator rather than a gap: it holds the build side as a
    /// materialised vector, so it can remember which of those rows matched and
    /// emit the rest when the input ends. An operator that sees the build side
    /// one tree descent at a time has nothing to keep that mark on.
    Right,
    /// Every probe row and every build row, each null-extended when it matched
    /// nothing.
    Full,
    /// One probe row per probe row that matched, with no build columns.
    Semi,
    /// One probe row per probe row that matched nothing, with no build columns.
    Anti,
}

impl JoinKind {
    /// Reports whether unmatched *probe* rows are kept, null-extended.
    pub fn keeps_probe(self) -> bool {
        matches!(self, JoinKind::Left | JoinKind::Full)
    }

    /// Reports whether unmatched *build* rows are kept, null-extended.
    pub fn keeps_build(self) -> bool {
        matches!(self, JoinKind::Right | JoinKind::Full)
    }
}

/// Builds a hash table from one side, then probes it with the other.
///
/// The build side is pushed in first through [`HashJoin::build`]; after that the
/// operator is an ordinary sink and every batch pushed into it is a probe.
pub struct HashJoin<'s> {
    kind: JoinKind,
    /// How wide the probe side is, learned from the first batch.
    ///
    /// Read at `finish`, where a RIGHT or FULL join null-extends the build rows
    /// nothing matched and there is no batch left to ask.
    probe_width: usize,
    /// The build side's key expressions, over the build row's columns.
    build_keys: Vec<Box<dyn Eval>>,
    /// The probe side's key expressions, over the probe batch's columns.
    probe_keys: Vec<Box<dyn Eval>>,
    table: HashTable,
    downstream: Box<dyn Sink + 's>,
    /// Reused between probes so a join of a million rows makes no allocation
    /// per row.
    scratch: Vec<u8>,
}

impl<'s> HashJoin<'s> {
    /// Returns a hash join with an empty table.
    ///
    /// @param kind - how unmatched rows are treated
    /// @param build_keys - the build side's key expressions
    /// @param probe_keys - the probe side's key expressions
    /// @param downstream - what to push joined rows into
    pub fn new(
        kind: JoinKind,
        build_keys: Vec<Box<dyn Eval>>,
        probe_keys: Vec<Box<dyn Eval>>,
        downstream: Box<dyn Sink + 's>,
    ) -> HashJoin<'s> {
        HashJoin {
            kind,
            probe_width: 0,
            build_keys,
            probe_keys,
            table: HashTable::new(),
            downstream,
            scratch: Vec::new(),
        }
    }

    /// Adds one batch of build rows to the table.
    ///
    /// @param batch - a batch from the build side
    pub fn build(&mut self, batch: &Batch<'_>) -> DbResult<()> {
        // **The build side is charged here (task-1932, H6).** It is the whole
        // of one input held in memory before a single output row exists, and
        // before this the request budget saw none of it: a join whose build
        // side is the large table and whose answer is one row spent one row's
        // worth of a 256 MiB cap.
        inillucent_base::budget::materialise(crate::ops::batch_bytes(batch))?;
        let width = batch.columns.len();
        for nth in 0..batch.live() {
            let mut row = Vec::with_capacity(width);
            for column in 0..width {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            let (encoded, has_null) = encode_row_key(&self.build_keys, batch, nth)?;
            self.table.insert(encoded, row, has_null);
        }
        Ok(())
    }

    /// Returns how many rows the build side put in the table.
    pub fn build_rows(&self) -> usize {
        self.table.rows.len()
    }

    /// Adds rows that were materialised rather than streamed.
    ///
    /// The automatic index's entry point: the inner side of a join has already
    /// been read into a vector by the time the operator is built, and turning
    /// it back into batches only so that `build` can take them apart again is
    /// work with no reader. The rows are fed a batch at a time even so, because
    /// the key expressions are compiled against a batch and evaluating them one
    /// row at a time would mean a second evaluator.
    ///
    /// @param rows - the inner side's rows, in the order they were read
    pub fn build_materialised(&mut self, rows: &[Vec<OwnedDatum>]) -> DbResult<()> {
        let width = rows.first().map(Vec::len).unwrap_or(0);
        if width == 0 {
            return Ok(());
        }
        let mut start = 0usize;
        while start < rows.len() {
            let end = start
                .saturating_add(crate::batch::BATCH_ROWS)
                .min(rows.len());
            let chunk = rows.get(start..end).unwrap_or(&[]);
            let mut held: Vec<Vec<Datum<'_>>> = Vec::with_capacity(width);
            for column in 0..width {
                held.push(
                    chunk
                        .iter()
                        .map(|row| {
                            row.get(column)
                                .map(OwnedDatum::borrow)
                                .unwrap_or(Datum::Null)
                        })
                        .collect(),
                );
            }
            let columns: Vec<Vector<'_>> = held
                .iter()
                .map(|values| Vector::Values(values.as_slice()))
                .collect();
            self.build(&Batch::new(chunk.len(), columns))?;
            start = end;
        }
        Ok(())
    }
}

impl Sink for HashJoin<'_> {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        // **Every batch, because the scan leaves are not enough (task-1932,
        // H11).** A join reads its probe side once and then does work
        // proportional to the matches, so a cross join over a small table
        // checks a few hundred times at the start and then runs for as long as
        // the product takes with nothing reading the cancellation flag. One
        // atomic load per batch is what makes a cancel reach the statement it
        // is about rather than the one after it.
        inillucent_base::budget::check()?;
        let HashJoin {
            kind,
            probe_width,
            probe_keys,
            table,
            downstream,
            scratch,
            ..
        } = self;
        let width = batch.columns.len();
        *probe_width = width;
        let build_width = table.rows.first().map(Vec::len).unwrap_or(0);
        let mut produced: Vec<Vec<OwnedDatum>> = Vec::new();
        for nth in 0..batch.live() {
            scratch.clear();
            let mut has_null = false;
            for expression in probe_keys.iter() {
                let value = expression.value(batch, nth)?;
                has_null |= value.is_null();
                key::encode_into(&value.get(), scratch);
            }
            // **Copied once per probe row, including when it matched
            // nothing (task-1932, M7).** `probe` answers a borrowed slice and
            // this cloned it so the loop below could hold it across the
            // `&mut self` a push needs. Reading the length first means a row
            // that matches nothing - the common case on a selective join -
            // allocates nothing at all.
            let found = if has_null {
                &[][..]
            } else {
                table.probe(scratch)
            };
            let matches: Vec<u32> = match found.is_empty() {
                true => Vec::new(),
                false => found.to_vec(),
            };
            match kind {
                JoinKind::Semi => {
                    if !matches.is_empty() {
                        produced.push(materialise(batch, nth, width)?);
                    }
                }
                JoinKind::Anti => {
                    if matches.is_empty() {
                        produced.push(materialise(batch, nth, width)?);
                    }
                }
                JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full => {
                    if matches.is_empty() {
                        if kind.keeps_probe() {
                            let mut row = materialise(batch, nth, width)?;
                            row.extend(std::iter::repeat_n(OwnedDatum::Null, build_width));
                            produced.push(row);
                        }
                    } else {
                        for position in matches {
                            if let Some(slot) = table.matched.get_mut(position as usize) {
                                *slot = true;
                            }
                            let mut row = materialise(batch, nth, width)?;
                            if let Some(build) = table.rows.get(position as usize) {
                                row.extend(build.iter().cloned());
                            }
                            produced.push(row);
                        }
                    }
                }
            }
        }
        if produced.is_empty() {
            return Ok(Flow::Continue);
        }
        emit_rows(&produced, downstream.as_mut())
    }

    fn finish(&mut self) -> DbResult<()> {
        // The build rows nothing matched, which only a RIGHT or FULL join
        // keeps and which only the end of the probe side can identify.
        if self.kind.keeps_build() {
            let mut produced: Vec<Vec<OwnedDatum>> = Vec::new();
            for (position, build) in self.table.rows.iter().enumerate() {
                if self.table.matched.get(position).copied().unwrap_or(false) {
                    continue;
                }
                let mut row: Vec<OwnedDatum> =
                    std::iter::repeat_n(OwnedDatum::Null, self.probe_width).collect();
                row.extend(build.iter().cloned());
                produced.push(row);
            }
            if !produced.is_empty() {
                emit_rows(&produced, self.downstream.as_mut())?;
            }
        }
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.table.clear();
        self.scratch.clear();
        self.probe_width = 0;
        self.downstream.reset()
    }
}

/// Probes an index once per outer row.
///
/// The inner side is a tree and a key built from the outer row, so the whole
/// operator is: evaluate the key, descend, emit the pairs. It is a sink rather
/// than a source because the outer side drives - which is what makes the outer
/// side stay vectorised.
pub struct IndexNestedLoopJoin<'t> {
    kind: JoinKind,
    /// The inner tree.
    inner: &'t PagedTree,
    /// The pool the inner tree's pages live in.
    pool: &'t Pool,
    /// The key expressions over the outer batch's columns.
    ///
    /// Shared rather than owned: a compiled chain rebuilds this join fresh
    /// every execution - see `inillucent_exec::compiled::JoinRecipe` - and a
    /// `Box<dyn Eval>` cannot be cloned, so the recipe and every execution's
    /// join share one `Rc` over the same compiled expressions instead of
    /// re-translating them.
    outer_keys: std::rc::Rc<[Box<dyn Eval>]>,
    /// Which inner columns to emit, in order.
    inner_projection: Projection,
    /// Whether the key is a full inner key (a probe) or a prefix (a range).
    full_key: bool,
    downstream: Box<dyn Sink + 't>,
    /// Reused across probes, so a join over a million outer rows makes one
    /// selection vector rather than a million.
    selection: Vec<u32>,
}

impl<'t> IndexNestedLoopJoin<'t> {
    /// Returns an index nested loop join.
    ///
    /// @param kind - how unmatched outer rows are treated
    /// @param inner - the tree to probe
    /// @param pool - the buffer pool
    /// @param outer_keys - the key expressions over the outer batch, shared
    ///   with whatever else is rebuilding this join across executions
    /// @param inner_projection - which inner columns to emit
    /// @param full_key - whether the key names every inner key column
    /// @param downstream - what to push joined rows into
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: JoinKind,
        inner: &'t PagedTree,
        pool: &'t Pool,
        outer_keys: impl Into<std::rc::Rc<[Box<dyn Eval>]>>,
        inner_projection: Projection,
        full_key: bool,
        downstream: Box<dyn Sink + 't>,
    ) -> IndexNestedLoopJoin<'t> {
        let outer_keys = outer_keys.into();
        IndexNestedLoopJoin {
            kind,
            inner,
            pool,
            outer_keys,
            inner_projection,
            full_key,
            downstream,
            selection: Vec::new(),
        }
    }
}

impl Sink for IndexNestedLoopJoin<'_> {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        // **Every batch, because the scan leaves are not enough (task-1932,
        // H11).** A join reads its probe side once and then does work
        // proportional to the matches, so a cross join over a small table
        // checks a few hundred times at the start and then runs for as long as
        // the product takes with nothing reading the cancellation flag. One
        // atomic load per batch is what makes a cancel reach the statement it
        // is about rather than the one after it.
        inillucent_base::budget::check()?;
        // Split the borrow so the closures below can hold the downstream sink
        // mutably while still reading the tree and the projection.
        let IndexNestedLoopJoin {
            kind,
            inner,
            pool,
            outer_keys,
            inner_projection,
            full_key,
            downstream,
            selection,
        } = self;
        let width = batch.columns.len();
        let inner_width = inner_projection.0.len();
        // The evaluated keys and the borrows of them live on the stack. Two
        // `Vec`s per outer row were 80 ns of a 433 ns rowid lookup, which is
        // most of what the join adds on top of the descent it cannot avoid.
        // Four columns covers every index in the fixture and in the dialect's
        // own corpus; a wider key spills, which costs what every key used to.
        for nth in 0..batch.live() {
            let count = outer_keys.len();
            // The spill path allocates; it is the path a key of more than four
            // columns takes and nothing in the fixture takes it.
            let mut spilled: Vec<crate::expr::Computed<'_>> = Vec::new();
            let mut spilled_keys: Vec<Datum<'_>> = Vec::new();
            let mut inline: [crate::expr::Computed<'_>; INLINE_KEYS] = [
                crate::expr::Computed::Borrowed(Datum::Null),
                crate::expr::Computed::Borrowed(Datum::Null),
                crate::expr::Computed::Borrowed(Datum::Null),
                crate::expr::Computed::Borrowed(Datum::Null),
            ];
            let mut keys: [Datum<'_>; INLINE_KEYS] = [Datum::Null; INLINE_KEYS];
            if count <= INLINE_KEYS {
                for (index, expression) in outer_keys.iter().enumerate() {
                    if let Some(slot) = inline.get_mut(index) {
                        *slot = expression.value(batch, nth)?;
                    }
                }
                for index in 0..count {
                    if let (Some(slot), Some(held)) = (keys.get_mut(index), inline.get(index)) {
                        *slot = held.get();
                    }
                }
            } else {
                spilled.reserve(count);
                for expression in outer_keys.iter() {
                    spilled.push(expression.value(batch, nth)?);
                }
                spilled_keys.extend(spilled.iter().map(crate::expr::Computed::get));
            }
            let probe: &[Datum<'_>] = if count <= INLINE_KEYS {
                keys.get(..count).unwrap_or(&[])
            } else {
                spilled_keys.as_slice()
            };
            // A join with *no* key at all is a cross product: every inner row
            // pairs with every outer one. It is the shape `CROSS JOIN` and a
            // `WHERE` with no usable equality both produce, and the only honest
            // way to run it is to read the inner tree once per outer row - which
            // is what SQLite does too.
            if outer_keys.is_empty() {
                let mut seen = 0usize;
                let mut reported = Flow::Continue;
                inner.visit_leaves(pool, &mut |leaf| {
                    let rows: Vec<Vec<Datum<'_>>> = if leaf.needs_materialising() {
                        leaf.live()?
                    } else {
                        Vec::new()
                    };
                    let live = if leaf.needs_materialising() {
                        rows.len()
                    } else {
                        leaf.row_count()
                    };
                    if live == 0 {
                        return Ok(true);
                    }
                    let total = width.saturating_add(inner_width);
                    let mut columns: Vec<Vector<'_>> = Vec::with_capacity(total);
                    for column in 0..width {
                        columns.push(Vector::Const(batch.value(nth, column)?));
                    }
                    let mut held: Vec<Vec<Datum<'_>>> = Vec::new();
                    if leaf.needs_materialising() {
                        for index in &inner_projection.0 {
                            held.push(
                                rows.iter()
                                    .map(|row| row.get(*index).copied().unwrap_or(Datum::Null))
                                    .collect(),
                            );
                        }
                    }
                    for (position, column) in inner_projection.0.iter().enumerate() {
                        columns.push(if leaf.needs_materialising() {
                            Vector::Values(held.get(position).map(Vec::as_slice).unwrap_or(&[]))
                        } else {
                            Vector::from_column(leaf.column(*column)?)
                        });
                    }
                    seen = seen.saturating_add(live);
                    if *kind == JoinKind::Semi {
                        return Ok(false);
                    }
                    reported = downstream.push(&Batch::new(live, columns))?;
                    Ok(reported == Flow::Continue)
                })?;
                match kind {
                    // Nothing to pair with, so an anti or left join keeps the
                    // outer row null-extended.
                    JoinKind::Anti if seen == 0 => {
                        reported = push_outer(batch, nth, width, 0, downstream.as_mut())?;
                    }
                    JoinKind::Left if seen == 0 => {
                        reported = push_outer(batch, nth, width, inner_width, downstream.as_mut())?;
                    }
                    // A semi join emits the outer row once when anything
                    // matched, which is what the early return above stopped at.
                    JoinKind::Semi if seen > 0 => {
                        reported = push_outer(batch, nth, width, 0, downstream.as_mut())?;
                    }
                    _ => {}
                }
                if reported == Flow::Stop {
                    return Ok(Flow::Stop);
                }
                continue;
            }
            // A NULL join key matches nothing, in every join kind, because SQL
            // equality on NULL is never true.
            let null_key = probe.iter().any(|value| value.is_null());
            let mut matched = 0usize;
            let mut flow = Flow::Continue;
            if !null_key && *kind != JoinKind::Anti {
                if *full_key {
                    let found = inner.probe(pool, probe, |leaf, hit| {
                        // The joined row's column list lives on the stack when
                        // it fits, and the *inner* columns are the leaf's own
                        // mini-columns under a one-row selection rather than
                        // values read out of them.
                        //
                        // **That second half is where a rowid lookup's time
                        // was going.** A stage is projected in full, so a probe
                        // into `main_table` read all five of its columns to
                        // answer a query that reads one - and a PAX leaf keeps
                        // each column's values in its own region, so five reads
                        // are five scattered cache lines where the directory
                        // entries that describe them are two contiguous ones.
                        // A column vector costs the directory entry; the value
                        // is read only if something downstream asks for it,
                        // which for `count(category)` and `sum(length(label))`
                        // is exactly one column. It is the same shape the
                        // prefix branch below already had.
                        let total = width.saturating_add(inner_width);
                        let mut inline: [Vector<'_>; INLINE_COLUMNS] =
                            [Vector::Const(Datum::Null); INLINE_COLUMNS];
                        let mut spilled: Vec<Vector<'_>> = Vec::new();
                        let mut at = 0usize;
                        let heap = total > INLINE_COLUMNS;
                        if heap {
                            spilled.reserve(total);
                        }
                        for column in 0..width {
                            let vector = Vector::Const(batch.value(nth, column)?);
                            if heap {
                                spilled.push(vector);
                            } else if let Some(slot) = inline.get_mut(at) {
                                *slot = vector;
                            }
                            at = at.saturating_add(1);
                        }
                        if *kind != JoinKind::Semi {
                            for column in &inner_projection.0 {
                                // A row in the leaf's *delta* area has no
                                // mini-column to borrow, so its values go
                                // downstream as constants. That is the only
                                // difference a write makes to this path, and
                                // the sorted-region case below is byte for
                                // byte the code that was benchmarked.
                                let vector = match hit {
                                    // A leaf with an out-of-line value cannot
                                    // lend its mini-column: the slot holds a
                                    // reference rather than the value.
                                    Hit::Sorted(_) if leaf.has_extents() => {
                                        Vector::Const(leaf.value_at(hit, *column)?)
                                    }
                                    Hit::Sorted(_) => Vector::Column(leaf.column(*column)?),
                                    Hit::Delta(index) => {
                                        Vector::Const(leaf.delta_value(index, *column)?)
                                    }
                                };
                                if heap {
                                    spilled.push(vector);
                                } else if let Some(slot) = inline.get_mut(at) {
                                    *slot = vector;
                                }
                                at = at.saturating_add(1);
                            }
                        }
                        let columns: &[Vector<'_>] = if heap {
                            spilled.as_slice()
                        } else {
                            inline.get(..at).unwrap_or(&[])
                        };
                        match hit {
                            Hit::Sorted(_) if leaf.has_extents() => {
                                downstream.push(&Batch::over(1, columns))
                            }
                            Hit::Sorted(row) => {
                                selection.clear();
                                selection.push(row as u32);
                                let mut one = Batch::over(leaf.row_count(), columns);
                                one.selection = Some(selection.as_slice());
                                downstream.push(&one)
                            }
                            // One dense row: every vector is a constant, so
                            // there is nothing for a selection to select from.
                            Hit::Delta(_) => downstream.push(&Batch::over(1, columns)),
                        }
                    })?;
                    if let Some(reported) = found {
                        matched = 1;
                        flow = reported;
                    }
                } else {
                    // A prefix key is a range: every inner entry sharing it.
                    // The whole span of one leaf goes downstream as *one*
                    // batch, with the outer row's values as constant vectors -
                    // which is exactly what a constant vector is for, and what
                    // makes the join's output as vectorised as its input.
                    let mut seen = 0usize;
                    let mut reported = Flow::Continue;
                    inner.visit_equal(pool, probe, &mut |leaf, start, end| {
                        let total = width.saturating_add(inner_width);
                        let mut inline: [Vector<'_>; INLINE_COLUMNS] =
                            [Vector::Const(Datum::Null); INLINE_COLUMNS];
                        let mut spilled: Vec<Vector<'_>> = Vec::new();
                        let mut at = 0usize;
                        let heap = total > INLINE_COLUMNS;
                        if heap {
                            spilled.reserve(total);
                        }
                        for column in 0..width {
                            let vector = Vector::Const(batch.value(nth, column)?);
                            if heap {
                                spilled.push(vector);
                            } else if let Some(slot) = inline.get_mut(at) {
                                *slot = vector;
                            }
                            at = at.saturating_add(1);
                        }
                        // A leaf that has been written to cannot be read as
                        // mini-columns: some of its rows are hidden and some are
                        // tagged bytes in the delta area, and the span the visitor
                        // computed is over the *sorted region* rather than over the
                        // live rows. So it is merged, filtered by the same prefix
                        // the visitor matched on, and pushed as values.
                        let merged: Vec<Vec<Datum<'_>>> = if leaf.needs_materialising() {
                            leaf.live_between(Some(probe), true, Some(probe), true)?
                        } else {
                            Vec::new()
                        };
                        let mut held: Vec<Vec<Datum<'_>>> = Vec::new();
                        if leaf.needs_materialising() {
                            for index in &inner_projection.0 {
                                held.push(
                                    merged
                                        .iter()
                                        .map(|row| row.get(*index).copied().unwrap_or(Datum::Null))
                                        .collect(),
                                );
                            }
                        }
                        for (position, column) in inner_projection.0.iter().enumerate() {
                            let vector = if leaf.needs_materialising() {
                                Vector::Values(held.get(position).map(Vec::as_slice).unwrap_or(&[]))
                            } else {
                                Vector::from_column(leaf.column(*column)?)
                            };
                            if heap {
                                spilled.push(vector);
                            } else if let Some(slot) = inline.get_mut(at) {
                                *slot = vector;
                            }
                            at = at.saturating_add(1);
                        }
                        let columns: &[Vector<'_>] = if heap {
                            spilled.as_slice()
                        } else {
                            inline.get(..at).unwrap_or(&[])
                        };
                        if leaf.needs_materialising() {
                            if merged.is_empty() {
                                return Ok(true);
                            }
                            seen = seen.saturating_add(merged.len());
                            if *kind == JoinKind::Semi {
                                return Ok(false);
                            }
                            reported = downstream.push(&Batch::over(merged.len(), columns))?;
                            return Ok(reported == Flow::Continue);
                        }
                        selection.clear();
                        selection.extend((start..end).map(|row| row as u32));
                        let mut span = Batch::over(leaf.row_count(), columns);
                        span.selection = Some(selection.as_slice());
                        seen = seen.saturating_add(end.saturating_sub(start));
                        if *kind == JoinKind::Semi {
                            // A semi join wants one row per *outer* row, so
                            // the first match is enough and the inner
                            // columns are not emitted at all.
                            return Ok(false);
                        }
                        reported = downstream.push(&span)?;
                        Ok(reported == Flow::Continue)
                    })?;
                    matched = seen;
                    flow = reported;
                }
            } else if *kind == JoinKind::Anti && !null_key {
                // An anti join only needs to know *whether* there is a match.
                let mut seen = 0usize;
                if *full_key {
                    if inner.probe(pool, probe, |_, _| Ok(()))?.is_some() {
                        seen = 1;
                    }
                } else {
                    inner.visit_equal(pool, probe, &mut |_, start, end| {
                        seen = seen.saturating_add(end.saturating_sub(start));
                        Ok(false)
                    })?;
                }
                matched = seen;
            }
            // What the join kinds do about a row that matched nothing, or that
            // matched and carries no inner columns.
            match kind {
                JoinKind::Semi if matched > 0 => {
                    flow = push_outer(batch, nth, width, 0, downstream.as_mut())?;
                }
                JoinKind::Anti if matched == 0 => {
                    flow = push_outer(batch, nth, width, 0, downstream.as_mut())?;
                }
                JoinKind::Left if matched == 0 => {
                    flow = push_outer(batch, nth, width, inner_width, downstream.as_mut())?;
                }
                _ => {}
            }
            if flow == Flow::Stop {
                return Ok(Flow::Stop);
            }
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.selection.clear();
        self.downstream.reset()
    }
}

/// Pushes one outer row, null-extended by `pad` columns.
///
/// The unmatched half of an outer join and the whole of a semi or anti one. It
/// borrows the outer batch rather than materialising, like everything else on
/// this path.
///
/// @param batch - the outer batch
/// @param nth - the row's position among the live rows
/// @param width - how many outer columns there are
/// @param pad - how many NULL columns to append
/// @param downstream - what to push into
fn push_outer(
    batch: &Batch<'_>,
    nth: usize,
    width: usize,
    pad: usize,
    downstream: &mut dyn Sink,
) -> DbResult<Flow> {
    let mut columns: Vec<Vector<'_>> = Vec::with_capacity(width.saturating_add(pad));
    for column in 0..width {
        columns.push(Vector::Const(batch.value(nth, column)?));
    }
    for _ in 0..pad {
        columns.push(Vector::Const(Datum::Null));
    }
    let one = Batch::new(1, columns);
    downstream.push(&one)
}

/// Pairs every outer row with every row of a materialised inner side.
///
/// The fallback: a cross product, or a join whose condition is not an equality
/// the other two strategies can key on. The inner side is materialised once
/// rather than re-scanned per outer row, because re-scanning a tree per outer
/// row is the one thing worse than a cross product.
pub struct NestedLoopJoin<'s> {
    kind: JoinKind,
    inner: Vec<Vec<OwnedDatum>>,
    /// Which inner rows have matched something, for a `RIGHT` or `FULL` join.
    ///
    /// Empty for every other kind, which is what makes those pay nothing for
    /// it. A `RIGHT` join's answer cannot be decided per probe batch - an inner
    /// row is unmatched only once the *whole* outer side has gone past - so the
    /// mark accumulates here and `finish` emits what is left.
    matched: Vec<bool>,
    /// How wide the outer side is, learned from the first batch.
    ///
    /// Needed at `finish`, where there is no batch to read it from and the
    /// unmatched inner rows still have to be null-extended to the width every
    /// other row of this join has.
    outer_width: usize,
    /// The join condition over the concatenated row, or `None` for a cross
    /// product.
    condition: Option<Box<dyn Eval>>,
    downstream: Box<dyn Sink + 's>,
    /// The concatenated row the condition is tested over, reused.
    ///
    /// One buffer rather than one allocation per candidate pair; see the note
    /// in `push` (task-1932, M7).
    scratch: Vec<OwnedDatum>,
}

impl<'s> NestedLoopJoin<'s> {
    /// Returns a nested loop join over an already-materialised inner side.
    ///
    /// @param kind - how unmatched outer rows are treated
    /// @param inner - every inner row
    /// @param condition - the join condition over the concatenated row
    /// @param downstream - what to push joined rows into
    pub fn new(
        kind: JoinKind,
        inner: Vec<Vec<OwnedDatum>>,
        condition: Option<Box<dyn Eval>>,
        downstream: Box<dyn Sink + 's>,
    ) -> NestedLoopJoin<'s> {
        NestedLoopJoin {
            matched: if kind.keeps_build() {
                vec![false; inner.len()]
            } else {
                Vec::new()
            },
            kind,
            inner,
            outer_width: 0,
            condition,
            downstream,
            scratch: Vec::new(),
        }
    }
}

impl Sink for NestedLoopJoin<'_> {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        // **Every batch, because the scan leaves are not enough (task-1932,
        // H11).** A join reads its probe side once and then does work
        // proportional to the matches, so a cross join over a small table
        // checks a few hundred times at the start and then runs for as long as
        // the product takes with nothing reading the cancellation flag. One
        // atomic load per batch is what makes a cancel reach the statement it
        // is about rather than the one after it.
        inillucent_base::budget::check()?;
        let width = batch.columns.len();
        self.outer_width = width;
        let inner_width = self.inner.first().map(Vec::len).unwrap_or(0);
        let mut produced: Vec<Vec<OwnedDatum>> = Vec::new();
        for nth in 0..batch.live() {
            let outer = materialise(batch, nth, width)?;
            let mut matched = 0usize;
            for (position, inner) in self.inner.iter().enumerate() {
                // **The condition is tested over a reused buffer and only a
                // surviving pair is cloned (task-1932, M7).** This used to
                // build the joined row for *every* inner row, test it, and drop
                // it - one allocation and a copy of both sides per candidate
                // pair, which on a cross join is one per row of the product.
                // `TopN::push` already had this shape; this is the same idea in
                // the place it costs most.
                self.scratch.clear();
                self.scratch.extend(outer.iter().cloned());
                self.scratch.extend(inner.iter().cloned());
                if !keeps(self.condition.as_deref(), &self.scratch)? {
                    continue;
                }
                matched = matched.saturating_add(1);
                if let Some(mark) = self.matched.get_mut(position) {
                    *mark = true;
                }
                match self.kind {
                    JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full => {
                        produced.push(self.scratch.clone())
                    }
                    JoinKind::Semi | JoinKind::Anti => break,
                }
            }
            match self.kind {
                JoinKind::Semi if matched > 0 => produced.push(outer),
                JoinKind::Anti if matched == 0 => produced.push(outer),
                _ if self.kind.keeps_probe() && matched == 0 => {
                    let mut row = outer;
                    row.extend(std::iter::repeat_n(OwnedDatum::Null, inner_width));
                    produced.push(row);
                }
                _ => {}
            }
        }
        if produced.is_empty() {
            return Ok(Flow::Continue);
        }
        emit_rows(&produced, self.downstream.as_mut())
    }

    fn finish(&mut self) -> DbResult<()> {
        // **The RIGHT half of the join happens here and nowhere else.** An
        // inner row is unmatched only once the whole outer side has gone past,
        // so this is the first moment the answer is known - and it is why a
        // RIGHT join needs the build side materialised rather than probed.
        if self.kind.keeps_build() {
            let mut produced: Vec<Vec<OwnedDatum>> = Vec::new();
            for (position, inner) in self.inner.iter().enumerate() {
                if self.matched.get(position).copied().unwrap_or(false) {
                    continue;
                }
                let mut row: Vec<OwnedDatum> =
                    std::iter::repeat_n(OwnedDatum::Null, self.outer_width).collect();
                row.extend(inner.iter().cloned());
                produced.push(row);
            }
            if !produced.is_empty() {
                emit_rows(&produced, self.downstream.as_mut())?;
            }
        }
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        for mark in &mut self.matched {
            *mark = false;
        }
        self.outer_width = 0;
        self.downstream.reset()
    }
}

/// Reports whether a concatenated row satisfies a join condition.
///
/// A free function rather than a method, so the loop above can hold the build
/// side and the condition at the same time without borrowing all of `self`.
///
/// @param condition - the join condition, or `None` for a cross product
/// @param joined - the outer row followed by the inner row
fn keeps(condition: Option<&dyn Eval>, joined: &[OwnedDatum]) -> DbResult<bool> {
    {
        let Some(condition) = condition else {
            return Ok(true);
        };
        let borrowed: Vec<Datum<'_>> = joined.iter().map(OwnedDatum::borrow).collect();
        let columns: Vec<Vector<'_>> = borrowed.iter().map(|value| Vector::Const(*value)).collect();
        let batch = Batch::new(1, columns);
        // SQL's three-valued logic: only a true keeps the row, so a NULL
        // condition drops it exactly as a false does.
        let verdict = condition.value(&batch, 0)?;
        Ok(matches!(verdict.get(), Datum::Int(number) if number != 0))
    }
}

/// Copies one live row of a batch into owned values.
///
/// @param batch - the batch to read
/// @param nth - the row's position among the live rows
/// @param width - how many columns to copy
fn materialise(batch: &Batch<'_>, nth: usize, width: usize) -> DbResult<Vec<OwnedDatum>> {
    let mut row = Vec::with_capacity(width);
    for column in 0..width {
        row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
    }
    Ok(row)
}

/// Encodes one row's key columns, reporting whether any was NULL.
///
/// The encoding is the same memcmp form an interior separator uses, and for the
/// same reason: one `Vec<u8>` hashes and compares in one call where a tuple of
/// tagged values dispatches per column. It is exact - `inillucent-tree`'s key
/// module documents how - so two rows hash together exactly when SQL says their
/// keys are equal.
///
/// @param keys - the key expressions
/// @param batch - the batch being read
/// @param nth - the row's position among the batch's live rows
fn encode_row_key(
    keys: &[Box<dyn Eval>],
    batch: &Batch<'_>,
    nth: usize,
) -> DbResult<(Vec<u8>, bool)> {
    let mut encoded = Vec::with_capacity(keys.len().saturating_mul(18));
    let mut has_null = false;
    for expression in keys {
        let value = expression.value(batch, nth)?;
        has_null |= value.is_null();
        key::encode_into(&value.get(), &mut encoded);
    }
    Ok((encoded, has_null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::{compile, Expr, StaticType};
    use crate::ops::CollectInto;
    use inillucent_pool::{Database, Options};
    use inillucent_tree::types::{ColumnSpec, PhysicalType};
    use inillucent_vfs::{DbPath, MemoryVfs};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A sink that keeps every row it is given.
    fn collector() -> (Rc<RefCell<Vec<Vec<OwnedDatum>>>>, Box<dyn Sink>) {
        let rows = Rc::new(RefCell::new(Vec::new()));
        let sink = Box::new(CollectInto::new(Rc::clone(&rows)));
        (rows, sink)
    }

    /// Returns a one-batch source over literal integer rows.
    fn push_rows(sink: &mut dyn Sink, rows: &[Vec<OwnedDatum>]) -> DbResult<()> {
        emit_rows(rows, sink)?;
        Ok(())
    }

    /// Returns rows of `(id, tag)`.
    fn pairs(values: &[(i64, i64)]) -> Vec<Vec<OwnedDatum>> {
        values
            .iter()
            .map(|(a, b)| vec![OwnedDatum::Int(*a), OwnedDatum::Int(*b)])
            .collect()
    }

    /// An inner hash join emits exactly the matching pairs, in probe order.
    #[test]
    fn an_inner_hash_join_pairs_matching_rows() {
        let (rows, sink) = collector();
        let build_key = compile(&Expr::Column(0), &[StaticType::Int, StaticType::Int]).unwrap();
        let probe_key = compile(&Expr::Column(0), &[StaticType::Int, StaticType::Int]).unwrap();
        let mut join = HashJoin::new(JoinKind::Inner, vec![build_key], vec![probe_key], sink);
        let build = pairs(&[(1, 10), (2, 20), (2, 21), (3, 30)]);
        let mut into = RowStore::new();
        for row in &build {
            into.push(row.clone());
        }
        // Feed the build side through the batch interface, as the planner does.
        struct Builder<'a, 's>(&'a mut HashJoin<'s>);
        impl Sink for Builder<'_, '_> {
            fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
                self.0.build(batch)?;
                Ok(Flow::Continue)
            }
            fn finish(&mut self) -> DbResult<()> {
                Ok(())
            }
            /// The test sinks hold no state that survives an execution.
            fn reset(&mut self) -> DbResult<()> {
                Ok(())
            }
        }
        {
            let mut builder = Builder(&mut join);
            push_rows(&mut builder, &build).unwrap();
        }
        assert_eq!(join.build_rows(), 4);
        push_rows(&mut join, &pairs(&[(2, 200), (4, 400), (1, 100)])).unwrap();
        join.finish().unwrap();
        let produced = rows.borrow();
        // (2,200) matches two build rows, (1,100) matches one, (4,400) none.
        assert_eq!(produced.len(), 3);
        assert_eq!(produced[0][0], OwnedDatum::Int(2));
        assert_eq!(produced[0][3], OwnedDatum::Int(20));
        assert_eq!(produced[1][3], OwnedDatum::Int(21));
        assert_eq!(produced[2][0], OwnedDatum::Int(1));
    }

    /// A left join null-extends what does not match; semi and anti keep the
    /// probe row alone.
    #[test]
    fn the_outer_kinds_treat_misses_as_they_should() {
        let types = [StaticType::Int, StaticType::Int];
        for (kind, wanted) in [
            (JoinKind::Left, vec![1i64, 4]),
            (JoinKind::Semi, vec![1]),
            (JoinKind::Anti, vec![4]),
        ] {
            let (rows, sink) = collector();
            let mut join = HashJoin::new(
                kind,
                vec![compile(&Expr::Column(0), &types).unwrap()],
                vec![compile(&Expr::Column(0), &types).unwrap()],
                sink,
            );
            struct Builder<'a, 's>(&'a mut HashJoin<'s>);
            impl Sink for Builder<'_, '_> {
                fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
                    self.0.build(batch)?;
                    Ok(Flow::Continue)
                }
                fn finish(&mut self) -> DbResult<()> {
                    Ok(())
                }
                /// The test sinks hold no state that survives an execution.
                fn reset(&mut self) -> DbResult<()> {
                    Ok(())
                }
            }
            {
                let mut builder = Builder(&mut join);
                push_rows(&mut builder, &pairs(&[(1, 10)])).unwrap();
            }
            push_rows(&mut join, &pairs(&[(1, 100), (4, 400)])).unwrap();
            join.finish().unwrap();
            let produced = rows.borrow();
            let ids: Vec<i64> = produced
                .iter()
                .filter_map(|row| match row.first() {
                    Some(OwnedDatum::Int(value)) => Some(*value),
                    _ => None,
                })
                .collect();
            assert_eq!(ids, wanted, "{kind:?}");
            if kind == JoinKind::Left {
                // The unmatched row is null-extended to the build width.
                assert_eq!(produced[1].len(), 4);
                assert_eq!(produced[1][2], OwnedDatum::Null);
            }
        }
    }

    /// A NULL join key matches nothing, on either side.
    #[test]
    fn a_null_key_matches_nothing() {
        let types = [StaticType::Unknown, StaticType::Int];
        let (rows, sink) = collector();
        let mut join = HashJoin::new(
            JoinKind::Inner,
            vec![compile(&Expr::Column(0), &types).unwrap()],
            vec![compile(&Expr::Column(0), &types).unwrap()],
            sink,
        );
        struct Builder<'a, 's>(&'a mut HashJoin<'s>);
        impl Sink for Builder<'_, '_> {
            fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
                self.0.build(batch)?;
                Ok(Flow::Continue)
            }
            fn finish(&mut self) -> DbResult<()> {
                Ok(())
            }
            /// The test sinks hold no state that survives an execution.
            fn reset(&mut self) -> DbResult<()> {
                Ok(())
            }
        }
        {
            let mut builder = Builder(&mut join);
            push_rows(&mut builder, &[vec![OwnedDatum::Null, OwnedDatum::Int(1)]]).unwrap();
        }
        push_rows(&mut join, &[vec![OwnedDatum::Null, OwnedDatum::Int(2)]]).unwrap();
        join.finish().unwrap();
        assert!(rows.borrow().is_empty(), "NULL = NULL is not true in SQL");
    }

    /// An index nested loop join probes the inner tree once per outer row.
    #[test]
    fn an_index_nested_loop_join_probes_per_outer_row() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("inlj.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let owned: Vec<Vec<OwnedDatum>> = (0..500i64)
            .map(|n| vec![OwnedDatum::Int(n), OwnedDatum::Int(n * 3)])
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let inner = PagedTree::bulk_build(&mut database, 4, columns, 1, &borrowed).unwrap();

        let (rows, sink) = collector();
        let types = [StaticType::Int];
        let mut join = IndexNestedLoopJoin::new(
            JoinKind::Inner,
            &inner,
            database.pool(),
            vec![compile(&Expr::Column(0), &types).unwrap()],
            Projection(vec![1]),
            true,
            sink,
        );
        let outer: Vec<Vec<OwnedDatum>> = [3i64, 7, 999, 11]
            .iter()
            .map(|n| vec![OwnedDatum::Int(*n)])
            .collect();
        push_rows(&mut join, &outer).unwrap();
        join.finish().unwrap();
        let produced = rows.borrow();
        assert_eq!(produced.len(), 3, "999 is not in the inner tree");
        assert_eq!(produced[0], vec![OwnedDatum::Int(3), OwnedDatum::Int(9)]);
        assert_eq!(produced[2], vec![OwnedDatum::Int(11), OwnedDatum::Int(33)]);
    }

    /// An index nested loop over a key *prefix* emits every inner row sharing
    /// it, which is the shape `join.range` has.
    #[test]
    fn an_index_nested_loop_over_a_prefix_emits_every_match() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("inljrange.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        // An index tree: (owner, rowid).
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        let owned: Vec<Vec<OwnedDatum>> = (0..600i64)
            .map(|n| vec![OwnedDatum::Int(n / 3), OwnedDatum::Int(n)])
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let inner = PagedTree::bulk_build(&mut database, 6, columns, 2, &borrowed).unwrap();

        let (rows, sink) = collector();
        let types = [StaticType::Int];
        let mut join = IndexNestedLoopJoin::new(
            JoinKind::Inner,
            &inner,
            database.pool(),
            vec![compile(&Expr::Column(0), &types).unwrap()],
            Projection(vec![1]),
            false,
            sink,
        );
        push_rows(&mut join, &[vec![OwnedDatum::Int(5)]]).unwrap();
        join.finish().unwrap();
        let produced = rows.borrow();
        assert_eq!(produced.len(), 3, "owner 5 has three rows");
        assert_eq!(produced[0][1], OwnedDatum::Int(15));
        assert_eq!(produced[2][1], OwnedDatum::Int(17));
    }

    /// A nested loop with no condition is a cross product; with one it filters.
    #[test]
    fn a_nested_loop_is_a_cross_product_until_a_condition_narrows_it() {
        let inner = pairs(&[(1, 10), (2, 20)]);
        let (rows, sink) = collector();
        let mut join = NestedLoopJoin::new(JoinKind::Inner, inner.clone(), None, sink);
        push_rows(&mut join, &[vec![OwnedDatum::Int(7)]]).unwrap();
        join.finish().unwrap();
        assert_eq!(rows.borrow().len(), 2);
        assert_eq!(rows.borrow()[0].len(), 3);

        // Now with a condition: outer column 0 equals inner column 0, which is
        // column 1 of the concatenation.
        let types = [StaticType::Int, StaticType::Int, StaticType::Int];
        let condition = compile(
            &Expr::Compare(
                crate::expr::CompareOp::Equal,
                Box::new(Expr::Column(0)),
                Box::new(Expr::Column(1)),
            ),
            &types,
        )
        .unwrap();
        let (rows, sink) = collector();
        let mut join = NestedLoopJoin::new(JoinKind::Inner, inner, Some(condition), sink);
        push_rows(&mut join, &[vec![OwnedDatum::Int(2)]]).unwrap();
        join.finish().unwrap();
        assert_eq!(rows.borrow().len(), 1);
        assert_eq!(rows.borrow()[0][2], OwnedDatum::Int(20));
    }

    /// A materialiser replays exactly what it absorbed.
    #[test]
    fn a_materialiser_replays_its_input() {
        let (rows, sink) = collector();
        let mut hold = Materialize::new(sink);
        let input = pairs(&[(1, 2), (3, 4), (5, 6)]);
        push_rows(&mut hold, &input).unwrap();
        assert!(rows.borrow().is_empty(), "a breaker emits nothing early");
        hold.finish().unwrap();
        assert_eq!(rows.borrow().len(), 3);
        assert_eq!(rows.borrow()[2][1], OwnedDatum::Int(6));
    }

    /// A values scan produces its literal rows and nothing else.
    #[test]
    fn a_values_scan_produces_its_rows() {
        let (rows, sink) = collector();
        let mut sink = sink;
        ValuesScan::new(pairs(&[(1, 2), (3, 4)]))
            .run(sink.as_mut())
            .unwrap();
        assert_eq!(rows.borrow().len(), 2);
        let (rows, mut sink) = collector();
        ValuesScan::new(Vec::new()).run(sink.as_mut()).unwrap();
        assert!(rows.borrow().is_empty());
    }

    /// The row store absorbs batches and drains them back unchanged.
    #[test]
    fn the_row_store_round_trips() {
        let mut store = RowStore::new();
        assert!(store.is_empty());
        let (rows, mut sink) = collector();
        struct Absorb<'a>(&'a mut RowStore);
        impl Sink for Absorb<'_> {
            fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
                self.0.absorb(batch)?;
                Ok(Flow::Continue)
            }
            fn finish(&mut self) -> DbResult<()> {
                Ok(())
            }
            /// The test sinks hold no state that survives an execution.
            fn reset(&mut self) -> DbResult<()> {
                Ok(())
            }
        }
        {
            let mut absorb = Absorb(&mut store);
            push_rows(&mut absorb, &pairs(&[(1, 2), (3, 4)])).unwrap();
        }
        assert_eq!(store.len(), 2);
        assert_eq!(store.rows()[1][0], OwnedDatum::Int(3));
        store.drain_into(sink.as_mut()).unwrap();
        assert_eq!(rows.borrow().len(), 2);
        assert!(store.is_empty());
    }
}
