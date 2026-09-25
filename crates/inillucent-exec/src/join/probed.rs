//! The left join whose inner side is sought by index and then tested.
//!
//! Invariant: **an outer row is null extended exactly when no inner row it
//! was paired with passed the whole `ON` condition.** The probe narrows the
//! candidates and never stands in for the test.

use inillucent_base::DbResult;
use inillucent_tree::datum::OwnedDatum;

use crate::batch::Batch;
use crate::ops::{emit_rows, Flow, Sink};

use super::materialise;

/// The rows one outer row's probe produced, shared with the sink that
/// collects them.
type Found = std::rc::Rc<std::cell::RefCell<Vec<Vec<OwnedDatum>>>>;

/// A `LEFT JOIN` whose inner term is read through a seek on part of its `ON`.
///
/// **Why neither existing shape can answer it.** `LEFT JOIN todo t ON
/// t.list_id = l.id AND t.parent_id IS NULL` seeks `todo` by an index on
/// `list_id`, and the seek answers only the first conjunct. An
/// [`super::IndexNestedLoopJoin`] of kind `Left` null extends on an empty
/// probe, so it has nowhere to test the second conjunct. The materialised
/// join reads one stage into a buffer, and a non covering seek is two stages,
/// an index read and a table fetch: both were read whole and joined one after
/// the other, so the `ON` was tested over a row that held the index entry and
/// not the table row, and every `todo` column came back NULL.
///
/// So the stages of the term run as inner joins inside this operator, one
/// outer row at a time, with the whole `ON` as a filter over what they
/// produce. The outer row is emitted with every pair that survived, or once
/// with NULLs when none did. The probe per outer row is the same descent the
/// inner join version of the query makes.
pub struct ProbedOuterJoin<'t> {
    /// The term's stages as inner joins, then the `ON` filter, then a sink
    /// that appends into `found`.
    probe: Box<dyn Sink + 't>,
    /// What `probe` produced for the outer row being answered.
    found: Found,
    /// How many columns the term adds to the joined row, across its stages.
    pad: usize,
    /// What to push joined rows into.
    downstream: Box<dyn Sink + 't>,
}

impl<'t> ProbedOuterJoin<'t> {
    /// Returns a left join over a probe chain built by the caller.
    ///
    /// @param probe - the term's stages as inner joins, ending in a filter on
    ///   the `ON` condition and a collector appending into `found`
    /// @param found - the buffer that collector appends into
    /// @param pad - how many NULL columns an unmatched outer row gets
    /// @param downstream - what to push joined rows into
    pub fn new(
        probe: Box<dyn Sink + 't>,
        found: Found,
        pad: usize,
        downstream: Box<dyn Sink + 't>,
    ) -> ProbedOuterJoin<'t> {
        ProbedOuterJoin {
            probe,
            found,
            pad,
            downstream,
        }
    }
}

impl Sink for ProbedOuterJoin<'_> {
    /// Answers each outer row with its surviving pairs, or with NULLs.
    ///
    /// @param batch - the outer rows
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        inillucent_base::budget::check()?;
        let width = batch.columns.len();
        let mut produced: Vec<Vec<OwnedDatum>> = Vec::new();
        for nth in 0..batch.live() {
            let outer = materialise(batch, nth, width)?;
            self.found.borrow_mut().clear();
            emit_rows(std::slice::from_ref(&outer), self.probe.as_mut())?;
            let mut matched = std::mem::take(&mut *self.found.borrow_mut());
            if matched.is_empty() {
                let mut row = outer;
                row.extend(std::iter::repeat_n(OwnedDatum::Null, self.pad));
                produced.push(row);
            } else {
                produced.append(&mut matched);
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

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.found.borrow_mut().clear();
        self.probe.reset()?;
        self.downstream.reset()
    }
}
