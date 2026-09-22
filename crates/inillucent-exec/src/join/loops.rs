//! The two nested loop joins: through an index, and over a materialised side.
//!
//! Invariant: **the inner side is read through the same cursor protocol a
//! scan uses.** There is no join-only path into a tree, so a defect in the
//! cursor is one a scan finds too.

use inillucent_base::DbResult;
use inillucent_pool::Pool;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::leaf::Hit;
use inillucent_tree::PagedTree;

use crate::batch::{Batch, Vector};
use crate::expr::Eval;
use crate::ops::{emit_rows, Flow, Sink};
use crate::scan::Projection;

use super::*;

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
    /// Probes the inner side once per outer row.
    ///
    /// **The loop over the outer batch, and nothing else (task-1962, A8).** It
    /// was 362 lines with no doc comment, holding three concerns at once: the
    /// key, the match, and what an outer row that matched nothing becomes.
    /// They are `with_seek_key`, `Probing::inner_matches` and
    /// `Probing::emit_unmatched` now - each private to this module - and the
    /// middle one is in turn the probe branch, the range branch and the anti
    /// branch.
    ///
    /// @param batch - the outer rows
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        // **Every batch, because the scan leaves are not enough (task-1932,
        // H11).** A join reads its probe side once and then does work
        // proportional to the matches, so a cross join over a small table
        // checks a few hundred times at the start and then runs for as long as
        // the product takes with nothing reading the cancellation flag. One
        // atomic load per batch is what makes a cancel reach the statement it
        // is about rather than the one after it.
        inillucent_base::budget::check()?;
        // Split the borrow so the probe below can hold the downstream sink
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
        let mut probing = Probing {
            kind: *kind,
            inner,
            pool,
            inner_projection,
            full_key: *full_key,
            width,
            inner_width,
            downstream: downstream.as_mut(),
            selection,
        };
        for nth in 0..batch.live() {
            // A join with *no* key at all is a cross product: every inner row
            // pairs with every outer one. It is the shape `CROSS JOIN` and a
            // `WHERE` with no usable equality both produce, and the only honest
            // way to run it is to read the inner tree once per outer row -
            // which is what SQLite does too.
            let flow = if outer_keys.is_empty() {
                probing.cross_product(batch, nth)?
            } else {
                with_seek_key(outer_keys, batch, nth, |key| {
                    probing.inner_matches(batch, nth, key)
                })?
            };
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

/// Evaluates one outer row's join key and hands it to `then`.
///
/// **The key lives on this function's stack, which is why it is a callback
/// (task-1962, A8).** The evaluated values own whatever had to be built and the
/// key borrows from them, so nothing here can be returned: a caller that held
/// the key would be holding a borrow of a local. Handing the key to a closure
/// keeps the storage alive for exactly as long as the probe needs it, and keeps
/// the two `Vec`s off the hot path - they were 80 ns of a 433 ns rowid lookup,
/// which is most of what the join adds on top of the descent it cannot avoid.
///
/// Four columns covers every index in the fixture and in the dialect's own
/// corpus; a wider key spills to the heap, which costs what every key used to.
///
/// @param outer_keys - the key expressions over the outer batch's columns
/// @param batch - the outer rows
/// @param nth - which of the live outer rows the key is for
/// @param then - what to do with the key
fn with_seek_key<T>(
    outer_keys: &[Box<dyn Eval>],
    batch: &Batch<'_>,
    nth: usize,
    then: impl FnOnce(&[Datum<'_>]) -> DbResult<T>,
) -> DbResult<T> {
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
    then(probe)
}

/// One inner side, and the sink the pairs it finds go into.
///
/// **A gathered context rather than a nine argument call (task-1962, A8).**
/// `push` destructures the join so the closures that read the tree can hold the
/// downstream sink mutably at the same time, and every function the split
/// produced needs the same eight pieces. This is them, built once per batch.
struct Probing<'a, 't> {
    /// How unmatched outer rows are treated.
    kind: JoinKind,
    /// The tree being probed.
    inner: &'t PagedTree,
    /// The pool the inner tree's pages live in.
    pool: &'t Pool,
    /// Which inner columns to emit, in order.
    inner_projection: &'a Projection,
    /// Whether the key is a full inner key (a probe) or a prefix (a range).
    full_key: bool,
    /// How many outer columns a joined row starts with.
    width: usize,
    /// How many inner columns follow them.
    inner_width: usize,
    /// What joined rows are pushed into.
    downstream: &'a mut (dyn Sink + 't),
    /// Reused across probes, so a join over a million outer rows makes one
    /// selection vector rather than a million.
    selection: &'a mut Vec<u32>,
}

impl Probing<'_, '_> {
    /// Pairs one outer row with every inner row.
    ///
    /// The shape a join with no usable equality takes. The inner tree is read
    /// once per outer row, which is what SQLite does too.
    ///
    /// @param batch - the outer rows
    /// @param nth - which of the live outer rows to pair
    fn cross_product(&mut self, batch: &Batch<'_>, nth: usize) -> DbResult<Flow> {
        let Probing {
            kind,
            inner,
            pool,
            inner_projection,
            width,
            inner_width,
            downstream,
            ..
        } = self;
        let (kind, width, inner_width) = (*kind, *width, *inner_width);
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
            if kind == JoinKind::Semi {
                return Ok(false);
            }
            reported = downstream.push(&Batch::new(live, columns))?;
            Ok(reported == Flow::Continue)
        })?;
        if let Some(answered) = self.emit_unmatched(batch, nth, seen)? {
            reported = answered;
        }
        Ok(reported)
    }

    /// Finds the inner rows one outer row's key matches, and emits the pairs.
    ///
    /// A NULL join key matches nothing, in every join kind, because SQL
    /// equality on NULL is never true.
    ///
    /// @param batch - the outer rows
    /// @param nth - which of the live outer rows the key came from
    /// @param probe - the key, as [`with_seek_key`] built it
    fn inner_matches(
        &mut self,
        batch: &Batch<'_>,
        nth: usize,
        probe: &[Datum<'_>],
    ) -> DbResult<Flow> {
        let null_key = probe.iter().any(|value| value.is_null());
        let mut matched = 0usize;
        let mut flow = Flow::Continue;
        if !null_key && self.kind != JoinKind::Anti {
            let (seen, reported) = if self.full_key {
                self.probe_one(batch, nth, probe)?
            } else {
                self.probe_range(batch, nth, probe)?
            };
            matched = seen;
            flow = reported;
        } else if self.kind == JoinKind::Anti && !null_key {
            matched = self.anti_matches(probe)?;
        }
        if let Some(reported) = self.emit_unmatched(batch, nth, matched)? {
            flow = reported;
        }
        Ok(flow)
    }

    /// Probes for the one inner row a full key names.
    ///
    /// @param batch - the outer rows
    /// @param nth - which of the live outer rows the key came from
    /// @param probe - the full inner key
    fn probe_one(
        &mut self,
        batch: &Batch<'_>,
        nth: usize,
        probe: &[Datum<'_>],
    ) -> DbResult<(usize, Flow)> {
        let Probing {
            kind,
            inner,
            pool,
            inner_projection,
            width,
            inner_width,
            downstream,
            selection,
            ..
        } = self;
        let (kind, width, inner_width) = (*kind, *width, *inner_width);
        let mut matched = 0usize;
        let mut flow = Flow::Continue;
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
            if kind != JoinKind::Semi {
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
                        Hit::Delta(index) => Vector::Const(leaf.delta_value(index, *column)?),
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
                Hit::Sorted(_) if leaf.has_extents() => downstream.push(&Batch::over(1, columns)),
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
        Ok((matched, flow))
    }

    /// Walks the span of inner rows a prefix key covers.
    ///
    /// @param batch - the outer rows
    /// @param nth - which of the live outer rows the key came from
    /// @param probe - the key prefix
    fn probe_range(
        &mut self,
        batch: &Batch<'_>,
        nth: usize,
        probe: &[Datum<'_>],
    ) -> DbResult<(usize, Flow)> {
        let Probing {
            kind,
            inner,
            pool,
            inner_projection,
            width,
            inner_width,
            downstream,
            selection,
            ..
        } = self;
        let (kind, width, inner_width) = (*kind, *width, *inner_width);
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
            //
            // **The probe's own span, not the whole leaf** (task-2066 §4.3.4).
            // This used to ask `live_between` with the probe as both bounds,
            // which materialises every row of the leaf, scans the delta area
            // once per delta entry and sorts the result before the bounds
            // throw most of it away. On an index built before its rows were
            // loaded - the ordinary application order - every leaf is delta,
            // and that ran per probe.
            // The same cap `PagedTree::visit_equal` gives `equal_run`, so the
            // span this merges and the span the visitor matched are found the
            // same way.
            const RUN_SCAN: usize = 8;
            let merged: Vec<Vec<Datum<'_>>> = if leaf.needs_materialising() {
                leaf.live_matching(probe, RUN_SCAN)?
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
                if kind == JoinKind::Semi {
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
            if kind == JoinKind::Semi {
                // A semi join wants one row per *outer* row, so
                // the first match is enough and the inner
                // columns are not emitted at all.
                return Ok(false);
            }
            reported = downstream.push(&span)?;
            Ok(reported == Flow::Continue)
        })?;
        Ok((seen, reported))
    }

    /// Counts, without emitting, whether an anti join's key matches anything.
    ///
    /// An anti join only needs to know *whether* there is a match, so neither
    /// branch builds a joined row.
    ///
    /// @param probe - the key
    fn anti_matches(&self, probe: &[Datum<'_>]) -> DbResult<usize> {
        let mut seen = 0usize;
        if self.full_key {
            if self.inner.probe(self.pool, probe, |_, _| Ok(()))?.is_some() {
                seen = 1;
            }
        } else {
            self.inner
                .visit_equal(self.pool, probe, &mut |_, start, end| {
                    seen = seen.saturating_add(end.saturating_sub(start));
                    Ok(false)
                })?;
        }
        Ok(seen)
    }

    /// Emits the row a join kind owes for an outer row, if it owes one.
    ///
    /// What the join kinds do about a row that matched nothing, or that matched
    /// and carries no inner columns. `None` means this kind owes nothing for
    /// this row, which is every row of an inner join.
    ///
    /// @param batch - the outer rows
    /// @param nth - which of the live outer rows to answer for
    /// @param matched - how many inner rows it paired with
    fn emit_unmatched(
        &mut self,
        batch: &Batch<'_>,
        nth: usize,
        matched: usize,
    ) -> DbResult<Option<Flow>> {
        let pad = match self.kind {
            // A semi join emits the outer row once when anything matched, with
            // no inner columns at all.
            JoinKind::Semi if matched > 0 => 0,
            JoinKind::Anti if matched == 0 => 0,
            // Nothing to pair with, so a left join keeps the outer row
            // null-extended.
            JoinKind::Left if matched == 0 => self.inner_width,
            _ => return Ok(None),
        };
        push_outer(batch, nth, self.width, pad, self.downstream).map(Some)
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
