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

    /// Copies every live row of a batch into the store.
    ///
    /// @param batch - the batch to absorb
    pub fn absorb(&mut self, batch: &Batch<'_>) -> DbResult<()> {
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
        emit_rows(&self.rows, downstream)?;
        downstream.finish()
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
    /// One probe row per probe row that matched, with no build columns.
    Semi,
    /// One probe row per probe row that matched nothing, with no build columns.
    Anti,
}

/// Builds a hash table from one side, then probes it with the other.
///
/// The build side is pushed in first through [`HashJoin::build`]; after that the
/// operator is an ordinary sink and every batch pushed into it is a probe.
pub struct HashJoin<'s> {
    kind: JoinKind,
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
}

impl Sink for HashJoin<'_> {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let HashJoin {
            kind,
            probe_keys,
            table,
            downstream,
            scratch,
            ..
        } = self;
        let width = batch.columns.len();
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
            let matches: Vec<u32> = if has_null {
                Vec::new()
            } else {
                table.probe(scratch).to_vec()
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
                JoinKind::Inner | JoinKind::Left => {
                    if matches.is_empty() {
                        if *kind == JoinKind::Left {
                            let mut row = materialise(batch, nth, width)?;
                            row.extend(std::iter::repeat(OwnedDatum::Null).take(build_width));
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
        self.downstream.finish()
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
    outer_keys: Vec<Box<dyn Eval>>,
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
    /// @param outer_keys - the key expressions over the outer batch
    /// @param inner_projection - which inner columns to emit
    /// @param full_key - whether the key names every inner key column
    /// @param downstream - what to push joined rows into
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: JoinKind,
        inner: &'t PagedTree,
        pool: &'t Pool,
        outer_keys: Vec<Box<dyn Eval>>,
        inner_projection: Projection,
        full_key: bool,
        downstream: Box<dyn Sink + 't>,
    ) -> IndexNestedLoopJoin<'t> {
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
            // A NULL join key matches nothing, in every join kind, because SQL
            // equality on NULL is never true.
            let null_key = probe.iter().any(|value| value.is_null());
            let mut matched = 0usize;
            let mut flow = Flow::Continue;
            if !null_key && *kind != JoinKind::Anti {
                if *full_key {
                    let found = inner.probe(pool, probe, |leaf, row| {
                        let mut columns: Vec<Vector<'_>> =
                            Vec::with_capacity(width.saturating_add(inner_width));
                        for column in 0..width {
                            columns.push(Vector::Const(batch.value(nth, column)?));
                        }
                        if *kind != JoinKind::Semi {
                            for column in &inner_projection.0 {
                                columns.push(Vector::Const(leaf.value(row, *column)?));
                            }
                        }
                        let one = Batch::new(1, columns);
                        downstream.push(&one)
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
                        let mut columns: Vec<Vector<'_>> =
                            Vec::with_capacity(width.saturating_add(inner_width));
                        for column in 0..width {
                            columns.push(Vector::Const(batch.value(nth, column)?));
                        }
                        for column in &inner_projection.0 {
                            columns.push(Vector::from_column(leaf.column(*column)?));
                        }
                        selection.clear();
                        selection.extend((start..end).map(|row| row as u32));
                        let mut span = Batch::new(leaf.row_count(), columns);
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
    /// The join condition over the concatenated row, or `None` for a cross
    /// product.
    condition: Option<Box<dyn Eval>>,
    downstream: Box<dyn Sink + 's>,
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
            kind,
            inner,
            condition,
            downstream,
        }
    }
}

impl Sink for NestedLoopJoin<'_> {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let width = batch.columns.len();
        let inner_width = self.inner.first().map(Vec::len).unwrap_or(0);
        let mut produced: Vec<Vec<OwnedDatum>> = Vec::new();
        for nth in 0..batch.live() {
            let outer = materialise(batch, nth, width)?;
            let mut matched = 0usize;
            for inner in &self.inner {
                let mut joined = outer.clone();
                joined.extend(inner.iter().cloned());
                if !self.keeps(&joined)? {
                    continue;
                }
                matched = matched.saturating_add(1);
                match self.kind {
                    JoinKind::Inner | JoinKind::Left => produced.push(joined),
                    JoinKind::Semi | JoinKind::Anti => break,
                }
            }
            match self.kind {
                JoinKind::Semi if matched > 0 => produced.push(outer),
                JoinKind::Anti if matched == 0 => produced.push(outer),
                JoinKind::Left if matched == 0 => {
                    let mut row = outer;
                    row.extend(std::iter::repeat(OwnedDatum::Null).take(inner_width));
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
        self.downstream.finish()
    }
}

impl NestedLoopJoin<'_> {
    /// Reports whether a concatenated row satisfies the join condition.
    ///
    /// @param joined - the outer row followed by the inner row
    fn keeps(&self, joined: &[OwnedDatum]) -> DbResult<bool> {
        let Some(condition) = &self.condition else {
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
