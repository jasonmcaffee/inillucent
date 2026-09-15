//! Holding one side of a join, and the rows a `VALUES` clause supplies.
//!
//! Invariant: **a materialised side is read many times and built once.**
//! That is the whole difference between it and a scan, and it is why a
//! store copies where a scan borrows.

use inillucent_base::DbResult;
use inillucent_tree::datum::OwnedDatum;
use inillucent_tree::key;

use crate::batch::Batch;
use crate::expr::Eval;
use crate::ops::{emit_rows, Flow, Sink};

/// A buffer of materialised rows, emitted as batches.
///
/// The shared machinery under [`Materialize`], the build side of
/// [`super::hash::HashJoin`] and the output of every nested loop.
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
/// Copies one live row of a batch into owned values.
///
/// @param batch - the batch to read
/// @param nth - the row's position among the live rows
/// @param width - how many columns to copy
pub(crate) fn materialise(
    batch: &Batch<'_>,
    nth: usize,
    width: usize,
) -> DbResult<Vec<OwnedDatum>> {
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
pub(crate) fn encode_row_key(
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
