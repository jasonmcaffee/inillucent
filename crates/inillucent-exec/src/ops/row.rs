//! Filtering and projecting: the two operators that change a row's shape.
//!
//! Invariant: **neither copies a value.** A filter hands on a selection
//! vector rather than moving rows, and a projection that permutes columns
//! rebuilds the batch out of the same borrowed vectors.

use inillucent_base::DbResult;
use inillucent_tree::datum::Datum;

use crate::batch::{Batch, Vector};
use crate::expr::Eval;

use super::*;

/// Applies a predicate, producing a selection vector rather than moving rows.
pub struct Filter {
    predicate: Box<dyn Eval>,
    downstream: Box<dyn Sink>,
    pub(crate) selection: Vec<u32>,
}
impl Filter {
    /// Returns a filter over a compiled predicate.
    ///
    /// @param predicate - the compiled predicate
    /// @param downstream - what to push the surviving rows into
    pub fn new(predicate: Box<dyn Eval>, downstream: Box<dyn Sink>) -> Filter {
        Filter {
            predicate,
            downstream,
            selection: Vec::with_capacity(crate::batch::BATCH_ROWS),
        }
    }
}
impl Sink for Filter {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        self.selection.clear();
        for nth in 0..batch.live() {
            // SQL's WHERE keeps a row only when the predicate is definitely
            // true: NULL is not true, and the three-valued logic in `expr`
            // produces NULL rather than false so the distinction survives to
            // here.
            let verdict = self.predicate.value(batch, nth)?;
            if crate::expr::truth(&verdict.get()) == Some(true) {
                self.selection.push(batch.row_at(nth) as u32);
            }
        }
        if self.selection.is_empty() {
            return Ok(Flow::Continue);
        }
        let filtered = Batch {
            rows: batch.rows,
            selection: Some(&self.selection),
            columns: batch.columns.clone(),
        };
        self.downstream.push(&filtered)
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
/// Evaluates a list of expressions into a new batch.
pub struct Project {
    expressions: Vec<Box<dyn Eval>>,
    downstream: Box<dyn Sink>,
}
impl Project {
    /// Returns a projection.
    ///
    /// @param expressions - one compiled expression per output column
    /// @param downstream - what to push the projected batch into
    pub fn new(expressions: Vec<Box<dyn Eval>>, downstream: Box<dyn Sink>) -> Project {
        Project {
            expressions,
            downstream,
        }
    }
}
impl Sink for Project {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let live = batch.live();
        // The permutation fast path. A projection that only reorders the
        // input's columns produces no values of its own, so the general path
        // below builds four `Vec`s to arrive at a list of vectors it already
        // had - once per batch, and a point probe's batch is one row.
        // `inillucent-probeprofile` measured `SELECT count(*) ... WHERE id = ?1`
        // at 0.87 us against a bare probe of about 0.24 us, and this is one of
        // the allocations in between.
        if self.expressions.len() <= INLINE_PROJECT {
            let mut inline: [Vector<'_>; INLINE_PROJECT] =
                [Vector::Const(Datum::Null); INLINE_PROJECT];
            let mut permutation = true;
            for (at, expression) in self.expressions.iter().enumerate() {
                match expression
                    .column()
                    .and_then(|column| batch.columns.get(column))
                {
                    Some(vector) => {
                        if let Some(slot) = inline.get_mut(at) {
                            *slot = *vector;
                        }
                    }
                    None => {
                        permutation = false;
                        break;
                    }
                }
            }
            if permutation {
                // The selection is carried through rather than applied: the
                // vectors are indexed by the input's row numbers, so a batch
                // that arrives selected has to leave selected. The first
                // version required a dense batch for exactly this reason, and
                // then a point probe handing one row of a leaf downstream could
                // not use it.
                let mut projected = Batch::over(
                    batch.rows,
                    inline.get(..self.expressions.len()).unwrap_or(&[]),
                );
                projected.selection = batch.selection;
                return self.downstream.push(&projected);
            }
        }
        // A projection that is a permutation of the input columns rebuilds the
        // batch out of the same borrowed vectors and copies nothing. Anything
        // computed is materialised into a scratch buffer whose lifetime is this
        // call, which is what the borrow below relies on.
        // An expression may *build* its answer - a scalar function returning
        // text owns bytes that were not in the page - so the computed columns
        // are held as `Computed` and borrowed in a second pass. The two passes
        // are not a cost: the first would have had to exist anyway, and the
        // second is a pointer per value over storage that does not move.
        let mut computed: Vec<Vec<crate::expr::Computed<'_>>> =
            Vec::with_capacity(self.expressions.len());
        let mut passthrough: Vec<Option<usize>> = Vec::with_capacity(self.expressions.len());
        for expression in &self.expressions {
            match expression.column() {
                Some(index) if batch.is_dense() => {
                    passthrough.push(Some(index));
                    computed.push(Vec::new());
                }
                _ => {
                    passthrough.push(None);
                    let mut values = Vec::with_capacity(live);
                    for nth in 0..live {
                        values.push(expression.value(batch, nth)?);
                    }
                    computed.push(values);
                }
            }
        }
        let borrowed: Vec<Vec<Datum<'_>>> = computed
            .iter()
            .map(|column| column.iter().map(crate::expr::Computed::get).collect())
            .collect();
        let mut columns = Vec::with_capacity(self.expressions.len());
        for (index, source) in passthrough.iter().enumerate() {
            columns.push(match source {
                Some(column) => batch
                    .columns
                    .get(*column)
                    .copied()
                    .unwrap_or(Vector::Const(Datum::Null)),
                None => Vector::Values(borrowed.get(index).map(|v| v.as_slice()).unwrap_or(&[])),
            });
        }
        let projected = Batch::new(live, columns);
        self.downstream.push(&projected)
    }

    fn finish(&mut self) -> DbResult<()> {
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.downstream.reset()
    }
}
/// Passes at most `limit` rows through, after skipping `offset`.
pub struct Limit {
    limit: usize,
    offset: usize,
    pub(crate) seen: usize,
    pub(crate) emitted: usize,
    downstream: Box<dyn Sink>,
}
impl Limit {
    /// Returns a limit.
    ///
    /// @param limit - how many rows to pass
    /// @param offset - how many to skip first
    /// @param downstream - what to push the surviving rows into
    pub fn new(limit: usize, offset: usize, downstream: Box<dyn Sink>) -> Limit {
        Limit {
            limit,
            offset,
            seen: 0,
            emitted: 0,
            downstream,
        }
    }
}
impl Sink for Limit {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let mut selection: Vec<u32> = Vec::new();
        for nth in 0..batch.live() {
            // The limit is checked before the row is counted, so `seen` reports
            // how many rows the operator actually consumed rather than how many
            // it looked at on its way to stopping.
            if self.emitted >= self.limit {
                break;
            }
            self.seen = self.seen.saturating_add(1);
            if self.seen <= self.offset {
                continue;
            }
            self.emitted = self.emitted.saturating_add(1);
            selection.push(batch.row_at(nth) as u32);
        }
        if !selection.is_empty() {
            let limited = Batch {
                rows: batch.rows,
                selection: Some(&selection),
                columns: batch.columns.clone(),
            };
            self.downstream.push(&limited)?;
        }
        Ok(if self.emitted >= self.limit {
            Flow::Stop
        } else {
            Flow::Continue
        })
    }

    fn finish(&mut self) -> DbResult<()> {
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.seen = 0;
        self.emitted = 0;
        self.downstream.reset()
    }
}
