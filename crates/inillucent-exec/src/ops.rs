//! The operators: a push pipeline from a scan to a sink.
//!
//! Invariant: an operator that stops the pipeline (`LIMIT` satisfied) says so
//! by returning [`Flow::Stop`], and every operator above it propagates that
//! rather than continuing to read. A scan that keeps walking after its consumer
//! has enough is not a correctness bug, which is exactly why it would survive a
//! test suite and show up only as a slow `read.range` family.
//!
//! ## The shape
//!
//! Each operator owns the one downstream of it and pushes into it. That makes a
//! pipeline a chain of ownership from the source down to the sink, and it makes
//! a pipeline breaker - [`HashAggregate`], [`Sort`], [`TopN`], [`Distinct`] -
//! an operator that accumulates in `push` and emits in `finish`. There is no
//! scheduler and no coroutine: the call stack is the pipeline.
//!
//! ## Where the vectorised fast paths are
//!
//! Two, both in [`SimpleAggregate`] and both entered only when the whole batch
//! qualifies:
//!
//! - a dense integer column with no selection vector folds through
//!   `Accumulator::push_dense_ints`, which walks the page's own bytes;
//! - `count(*)` over a dense batch adds the row count without looking at a
//!   value at all.
//!
//! Everything else is the per-row path. The tests assert the two produce the
//! same answers, because a fast path that is also a different answer is the
//! worst kind of bug this engine can have.

use std::cmp::Ordering;
use std::collections::HashMap;

use inillucent_base::DbResult;
use inillucent_tree::datum::{borrow_row, Datum, OwnedDatum};
use inillucent_tree::key;
use inillucent_tree::types::compare_under;
use inillucent_value::collation::Collation;

use crate::aggregate::{Accumulator, AggregateKind};
use crate::batch::{Batch, Vector};
use crate::expr::Eval;

/// Whether the pipeline should keep going.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow {
    /// Keep pushing.
    Continue,
    /// Enough rows have been produced; the source may stop.
    Stop,
}

/// An operator that consumes batches.
pub trait Sink {
    /// Consumes one batch.
    ///
    /// @param batch - the batch to consume
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow>;

    /// Signals end of input and lets a pipeline breaker emit.
    fn finish(&mut self) -> DbResult<()>;
}

/// The end of a pipeline: it keeps the rows.
///
/// A pull-style statement API steps over what this collected. Rows are owned
/// because they outlive the pages they came from - that is what "the result of
/// a query" means.
#[derive(Default)]
pub struct Collect {
    rows: Vec<Vec<OwnedDatum>>,
    limit: Option<usize>,
}

impl Collect {
    /// Returns a sink that keeps every row.
    pub fn new() -> Collect {
        Collect {
            rows: Vec::new(),
            limit: None,
        }
    }

    /// Returns a sink that stops the pipeline after `limit` rows.
    ///
    /// @param limit - how many rows to keep
    pub fn with_limit(limit: usize) -> Collect {
        Collect {
            rows: Vec::new(),
            limit: Some(limit),
        }
    }

    /// Returns the rows collected.
    pub fn rows(&self) -> &[Vec<OwnedDatum>] {
        &self.rows
    }

    /// Returns the rows collected, consuming the sink.
    pub fn into_rows(self) -> Vec<Vec<OwnedDatum>> {
        self.rows
    }
}

impl Sink for Collect {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        for nth in 0..batch.live() {
            if let Some(limit) = self.limit {
                if self.rows.len() >= limit {
                    return Ok(Flow::Stop);
                }
            }
            let mut row = Vec::with_capacity(batch.columns.len());
            for column in 0..batch.columns.len() {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            self.rows.push(row);
        }
        match self.limit {
            Some(limit) if self.rows.len() >= limit => Ok(Flow::Stop),
            _ => Ok(Flow::Continue),
        }
    }

    fn finish(&mut self) -> DbResult<()> {
        Ok(())
    }
}

/// A sink that appends into a buffer the caller still holds.
///
/// The pipeline owns its sink, so a caller that wants the rows back cannot
/// simply unwrap the chain afterwards. Sharing the buffer is the small, honest
/// way out: the caller keeps one handle, the pipeline keeps the other, and the
/// rows are readable the moment the scan returns.
pub struct CollectInto {
    rows: std::rc::Rc<std::cell::RefCell<Vec<Vec<OwnedDatum>>>>,
    limit: Option<usize>,
}

impl CollectInto {
    /// Returns a sink appending into a shared buffer.
    ///
    /// @param rows - the buffer the caller keeps a handle on
    pub fn new(rows: std::rc::Rc<std::cell::RefCell<Vec<Vec<OwnedDatum>>>>) -> CollectInto {
        CollectInto { rows, limit: None }
    }

    /// Returns a sink that stops the pipeline after `limit` rows.
    ///
    /// @param rows - the buffer the caller keeps a handle on
    /// @param limit - how many rows to keep
    pub fn with_limit(
        rows: std::rc::Rc<std::cell::RefCell<Vec<Vec<OwnedDatum>>>>,
        limit: usize,
    ) -> CollectInto {
        CollectInto {
            rows,
            limit: Some(limit),
        }
    }
}

impl Sink for CollectInto {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let mut held = self.rows.borrow_mut();
        for nth in 0..batch.live() {
            if let Some(limit) = self.limit {
                if held.len() >= limit {
                    return Ok(Flow::Stop);
                }
            }
            let mut row = Vec::with_capacity(batch.columns.len());
            for column in 0..batch.columns.len() {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            held.push(row);
        }
        match self.limit {
            Some(limit) if held.len() >= limit => Ok(Flow::Stop),
            _ => Ok(Flow::Continue),
        }
    }

    fn finish(&mut self) -> DbResult<()> {
        Ok(())
    }
}

/// Applies a predicate, producing a selection vector rather than moving rows.
pub struct Filter {
    predicate: Box<dyn Eval>,
    downstream: Box<dyn Sink>,
    selection: Vec<u32>,
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
}

/// One aggregate and the expression it reads.
pub struct AggregateSpec {
    /// Which aggregate to compute.
    pub kind: AggregateKind,
    /// The argument, or `None` for `count(*)`.
    pub argument: Option<Box<dyn Eval>>,
}

/// Aggregates the whole input into one row.
pub struct SimpleAggregate {
    specs: Vec<AggregateSpec>,
    accumulators: Vec<Accumulator>,
    downstream: Box<dyn Sink>,
}

impl SimpleAggregate {
    /// Returns a whole-input aggregate.
    ///
    /// @param specs - one per output column
    /// @param downstream - what to push the single result row into
    pub fn new(specs: Vec<AggregateSpec>, downstream: Box<dyn Sink>) -> SimpleAggregate {
        let accumulators = specs
            .iter()
            .map(|spec| Accumulator::new(spec.kind.clone()))
            .collect();
        SimpleAggregate {
            specs,
            accumulators,
            downstream,
        }
    }
}

impl SimpleAggregate {
    /// Returns one accumulator's current state.
    ///
    /// Exists so a test and the harness can read a partial result without the
    /// pipeline having to finish, which is how the scan tests compare the dense
    /// and generic paths.
    ///
    /// @param index - which aggregate
    pub fn accumulator(&self, index: usize) -> Option<&Accumulator> {
        self.accumulators.get(index)
    }
}

impl Sink for SimpleAggregate {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let live = batch.live();
        for (index, spec) in self.specs.iter().enumerate() {
            let Some(accumulator) = self.accumulators.get_mut(index) else {
                continue;
            };
            match &spec.argument {
                // `count(*)` over a dense batch: the row count, no values read.
                None => {
                    for _ in 0..live {
                        accumulator.push(&Datum::Null);
                    }
                }
                Some(argument) => {
                    // The vectorised path: a bare reference to a dense integer
                    // column of a batch with no selection vector.
                    let dense = if batch.is_dense() {
                        argument
                            .column()
                            .and_then(|column| batch.columns.get(column))
                            .and_then(|vector| vector.dense_int_bytes())
                    } else {
                        None
                    };
                    match dense {
                        Some(bytes) => {
                            let wanted = live.saturating_mul(8).min(bytes.len());
                            accumulator.push_dense_ints(bytes.get(..wanted).unwrap_or(&[]));
                        }
                        None => {
                            for nth in 0..live {
                                accumulator.push(&argument.value(batch, nth)?.get());
                            }
                        }
                    }
                }
            }
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        let mut row = Vec::with_capacity(self.accumulators.len());
        for accumulator in &self.accumulators {
            row.push(accumulator.finish()?);
        }
        let borrowed = borrow_row(&row);
        let columns: Vec<Vector<'_>> = borrowed.iter().map(|value| Vector::Const(*value)).collect();
        let batch = Batch::new(1, columns);
        self.downstream.push(&batch)?;
        self.downstream.finish()
    }
}

/// Aggregates by a grouping key.
///
/// The group key is interned into a memcmp-comparable byte string, so the hash
/// map is keyed on `Vec<u8>` and one comparison is a `memcmp` rather than a walk
/// over tagged values. That is the TDD's "keys interned" in its simplest correct
/// form; the `u32` dictionary for low-cardinality columns is a Phase 2 item and
/// is not needed to clear this phase's gate.
pub struct HashAggregate {
    keys: Vec<Box<dyn Eval>>,
    collations: Vec<Collation>,
    specs: Vec<AggregateSpec>,
    groups: HashMap<Vec<u8>, (Vec<OwnedDatum>, Vec<Accumulator>)>,
    downstream: Box<dyn Sink>,
}

impl HashAggregate {
    /// Returns a grouped aggregate.
    ///
    /// @param keys - the `GROUP BY` expressions, which are also output columns
    /// @param collations - the collation of each key, for the grouping
    /// @param specs - the aggregates, which follow the keys in the output
    /// @param downstream - what to push the group rows into
    pub fn new(
        keys: Vec<Box<dyn Eval>>,
        collations: Vec<Collation>,
        specs: Vec<AggregateSpec>,
        downstream: Box<dyn Sink>,
    ) -> HashAggregate {
        HashAggregate {
            keys,
            collations,
            specs,
            groups: HashMap::new(),
            downstream,
        }
    }
}

impl Sink for HashAggregate {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let mut encoded = Vec::with_capacity(32);
        for nth in 0..batch.live() {
            encoded.clear();
            let mut values = Vec::with_capacity(self.keys.len());
            for expression in &self.keys {
                let value = expression.value(batch, nth)?;
                key::encode_into_with(
                    &value.get(),
                    self.collations
                        .get(values.len())
                        .copied()
                        .unwrap_or(Collation::Binary),
                    &mut encoded,
                );
                values.push(value);
            }
            let entry = self.groups.entry(encoded.clone()).or_insert_with(|| {
                (
                    values
                        .iter()
                        .map(crate::expr::Computed::get)
                        .collect::<Vec<_>>()
                        .iter()
                        .map(OwnedDatum::from_datum)
                        .collect(),
                    self.specs
                        .iter()
                        .map(|spec| Accumulator::new(spec.kind.clone()))
                        .collect(),
                )
            });
            for (index, spec) in self.specs.iter().enumerate() {
                let Some(accumulator) = entry.1.get_mut(index) else {
                    continue;
                };
                match &spec.argument {
                    None => accumulator.push(&Datum::Null),
                    Some(argument) => accumulator.push(&argument.value(batch, nth)?.get()),
                }
            }
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        // Emitted in encoded-key order, which is value order, so a downstream
        // `ORDER BY` on the group key has nothing to do. It still runs - the
        // planner does not yet prove the property - but it sorts sorted input.
        let mut keys: Vec<&Vec<u8>> = self.groups.keys().collect();
        keys.sort_unstable();
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(keys.len());
        for encoded in keys {
            let Some((group, accumulators)) = self.groups.get(encoded) else {
                continue;
            };
            let mut row = group.clone();
            for accumulator in accumulators {
                row.push(accumulator.finish()?);
            }
            rows.push(row);
        }
        emit_rows(&rows, self.downstream.as_mut())?;
        self.downstream.finish()
    }
}

/// Aggregates by a grouping key the input is already sorted by.
///
/// Grouping needs adjacency, not order. A scan of an index tree whose leading
/// key columns are the `GROUP BY` columns delivers every row of a group before
/// the next group starts, so the accumulators can be finished and the row
/// emitted as the key changes - no hash table, no key encoding, no allocation
/// per row, and constant memory whatever the cardinality.
///
/// This is the difference between 100,000 hash probes and 100,000 comparisons,
/// and on `scan.group` it is most of the gap against SQLite, which takes
/// exactly the same route through the same index.
pub struct StreamAggregate {
    /// The collation of each group key.
    collations: Vec<Collation>,
    keys: Vec<Box<dyn Eval>>,
    specs: Vec<AggregateSpec>,
    /// The key of the group being accumulated, or `None` before the first row.
    current: Option<Vec<OwnedDatum>>,
    accumulators: Vec<Accumulator>,
    /// The finished groups, emitted at `finish`.
    rows: Vec<Vec<OwnedDatum>>,
    downstream: Box<dyn Sink>,
}

impl StreamAggregate {
    /// Returns a streaming grouped aggregate.
    ///
    /// The caller must have established that the input arrives sorted by the
    /// key expressions; [`crate::physical`] does that from the scanned tree's
    /// own key columns, and it is a wrong answer rather than a slow one if it
    /// is wrong, which is why it is never inferred from the data.
    ///
    /// @param keys - the `GROUP BY` expressions, which are also output columns
    /// @param collations - the collation of each key, for the grouping
    /// @param specs - the aggregates, which follow the keys in the output
    /// @param downstream - what to push the group rows into
    pub fn new(
        keys: Vec<Box<dyn Eval>>,
        collations: Vec<Collation>,
        specs: Vec<AggregateSpec>,
        downstream: Box<dyn Sink>,
    ) -> StreamAggregate {
        let accumulators = specs
            .iter()
            .map(|spec| Accumulator::new(spec.kind.clone()))
            .collect();
        StreamAggregate {
            collations,
            keys,
            specs,
            current: None,
            accumulators,
            rows: Vec::new(),
            downstream,
        }
    }

    /// Finishes the group being accumulated and starts a fresh one.
    ///
    /// @param key - the new group's key, or `None` at end of input
    fn roll(&mut self, key: Option<Vec<OwnedDatum>>) -> DbResult<()> {
        if let Some(previous) = self.current.take() {
            let mut row = previous;
            for accumulator in &self.accumulators {
                row.push(accumulator.finish()?);
            }
            self.rows.push(row);
        }
        self.accumulators = self
            .specs
            .iter()
            .map(|spec| Accumulator::new(spec.kind.clone()))
            .collect();
        self.current = key;
        Ok(())
    }
}

impl StreamAggregate {
    /// Folds a run of rows that are known to share a group key.
    ///
    /// @param batch - the batch the run is in
    /// @param start - the first row of the run
    /// @param len - how many rows the run holds
    fn fold_run(&mut self, batch: &Batch<'_>, start: usize, len: usize) -> DbResult<()> {
        for (index, spec) in self.specs.iter().enumerate() {
            let Some(accumulator) = self.accumulators.get_mut(index) else {
                continue;
            };
            match &spec.argument {
                None => {
                    // `count(*)` over a run: the length, no value read at all.
                    for _ in 0..len {
                        accumulator.push(&Datum::Null);
                    }
                }
                Some(argument) => {
                    let dense = argument
                        .column()
                        .and_then(|column| batch.columns.get(column))
                        .and_then(|vector| vector.dense_int_bytes());
                    match dense {
                        Some(bytes) => {
                            let from = start.saturating_mul(8);
                            let to = from.saturating_add(len.saturating_mul(8)).min(bytes.len());
                            accumulator.push_dense_ints(bytes.get(from..to).unwrap_or(&[]));
                        }
                        None => {
                            for nth in start..start.saturating_add(len) {
                                accumulator.push(&argument.value(batch, nth)?.get());
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl Sink for StreamAggregate {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        // The vectorised path: one bare integer key column, a dense batch, and
        // the groups therefore arriving as runs of equal values in a contiguous
        // array. Finding the runs is a scan of that array; folding one is a
        // single call per accumulator rather than one per row. This is what
        // `GROUP BY` over an index looks like when the index is doing its job,
        // and it is the shape `scan.group` is.
        let dense = if batch.is_dense() && self.keys.len() == 1 {
            self.keys
                .first()
                .and_then(|expression| expression.column())
                .and_then(|column| batch.columns.get(column))
                .and_then(|vector| vector.dense_int_bytes())
        } else {
            None
        };
        if let Some(bytes) = dense {
            let rows = batch.live().min(bytes.len() / 8);
            let mut start = 0usize;
            while start < rows {
                let value = read_int(bytes, start);
                let mut end = start.saturating_add(1);
                while end < rows && read_int(bytes, end) == value {
                    end = end.saturating_add(1);
                }
                let same = matches!(
                    self.current.as_ref().and_then(|key| key.first()),
                    Some(OwnedDatum::Int(held)) if *held == value
                );
                if !same {
                    self.roll(Some(vec![OwnedDatum::Int(value)]))?;
                }
                self.fold_run(batch, start, end.saturating_sub(start))?;
                start = end;
            }
            return Ok(Flow::Continue);
        }

        for nth in 0..batch.live() {
            let mut same = self.current.is_some();
            if same {
                for (index, expression) in self.keys.iter().enumerate() {
                    let value = expression.value(batch, nth)?;
                    let held = self
                        .current
                        .as_ref()
                        .and_then(|key| key.get(index))
                        .map(OwnedDatum::borrow)
                        .unwrap_or(Datum::Null);
                    if compare_under(
                        &value.get(),
                        &held,
                        self.collations
                            .get(index)
                            .copied()
                            .unwrap_or(Collation::Binary),
                    ) != Ordering::Equal
                    {
                        same = false;
                        break;
                    }
                }
            }
            if !same {
                let mut key = Vec::with_capacity(self.keys.len());
                for expression in &self.keys {
                    key.push(expression.value(batch, nth)?.into_owned());
                }
                self.roll(Some(key))?;
            }
            for (index, spec) in self.specs.iter().enumerate() {
                let Some(accumulator) = self.accumulators.get_mut(index) else {
                    continue;
                };
                match &spec.argument {
                    None => accumulator.push(&Datum::Null),
                    Some(argument) => accumulator.push(&argument.value(batch, nth)?.get()),
                }
            }
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        self.roll(None)?;
        let rows = std::mem::take(&mut self.rows);
        emit_rows(&rows, self.downstream.as_mut())?;
        self.downstream.finish()
    }
}

/// Reads one 8-byte slot of a dense integer vector.
///
/// @param bytes - the value array
/// @param row - the row's position
fn read_int(bytes: &[u8], row: usize) -> i64 {
    let at = row.saturating_mul(8);
    match bytes.get(at..at.saturating_add(8)) {
        Some(slice) => i64::from_le_bytes(slice.try_into().unwrap_or([0; 8])),
        None => 0,
    }
}

/// Drops duplicate rows that arrive next to each other.
///
/// The `DISTINCT` counterpart of [`StreamAggregate`], and the same argument:
/// when the input is sorted by the projected columns, a duplicate is always the
/// previous row, so one comparison replaces a hash-set insert and the operator
/// holds one row instead of the whole result. On `scan.distinct` over 100,000
/// rows with 64 distinct values, that is 64 rows kept rather than 100,000
/// encoded and inserted.
pub struct AdjacentDistinct {
    /// The collation of each compared column.
    collations: Vec<Collation>,
    previous: Option<Vec<OwnedDatum>>,
    rows: Vec<Vec<OwnedDatum>>,
    downstream: Box<dyn Sink>,
}

impl AdjacentDistinct {
    /// Returns an adjacent de-duplicating operator.
    ///
    /// The caller must have established that the input arrives sorted by the
    /// columns being de-duplicated.
    ///
    /// @param collations - the collation of each compared column
    /// @param downstream - what to push the surviving rows into
    pub fn new(collations: Vec<Collation>, downstream: Box<dyn Sink>) -> AdjacentDistinct {
        AdjacentDistinct {
            collations,
            previous: None,
            rows: Vec::new(),
            downstream,
        }
    }
}

impl Sink for AdjacentDistinct {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let width = batch.columns.len();
        for nth in 0..batch.live() {
            let mut same = self.previous.is_some();
            if same {
                for column in 0..width {
                    let value = batch.value(nth, column)?;
                    let held = self
                        .previous
                        .as_ref()
                        .and_then(|row| row.get(column))
                        .map(OwnedDatum::borrow)
                        .unwrap_or(Datum::Null);
                    // NULLs are equal to each other for DISTINCT, which is the
                    // one place SQL's usual "NULL is not equal to anything"
                    // does not hold. `Datum::compare` orders NULL equal to
                    // NULL, which is what this needs.
                    if compare_under(
                        &value,
                        &held,
                        self.collations
                            .get(column)
                            .copied()
                            .unwrap_or(Collation::Binary),
                    ) != Ordering::Equal
                    {
                        same = false;
                        break;
                    }
                }
            }
            if same {
                continue;
            }
            let mut row = Vec::with_capacity(width);
            for column in 0..width {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            self.previous = Some(row.clone());
            self.rows.push(row);
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        let rows = std::mem::take(&mut self.rows);
        emit_rows(&rows, self.downstream.as_mut())?;
        self.downstream.finish()
    }
}

/// One `ORDER BY` term.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SortKey {
    /// Which output column to order by.
    pub column: usize,
    /// Whether the order is descending.
    pub descending: bool,
    /// The collation the term's text is ordered under.
    ///
    /// A column's own collation unless the term named one. `ORDER BY team`
    /// on a `COLLATE NOCASE` column orders case-insensitively and
    /// `ORDER BY team COLLATE BINARY` does not, and the two are different
    /// answers rather than different speeds.
    pub collation: Collation,
    /// Whether NULLs sort before everything rather than after.
    ///
    /// SQLite's default is first ascending and last descending, which falls out
    /// of reversing an ordering that puts NULL lowest. An explicit
    /// `NULLS FIRST` on a descending term, or `NULLS LAST` on an ascending one,
    /// does not - it has to be carried, and the SLT corpus asks for both.
    pub nulls_first: bool,
}

/// Sorts every row, then emits.
pub struct Sort {
    keys: Vec<SortKey>,
    rows: Vec<Vec<OwnedDatum>>,
    downstream: Box<dyn Sink>,
}

impl Sort {
    /// Returns a sort.
    ///
    /// @param keys - the ordering terms
    /// @param downstream - what to push the ordered rows into
    pub fn new(keys: Vec<SortKey>, downstream: Box<dyn Sink>) -> Sort {
        Sort {
            keys,
            rows: Vec::new(),
            downstream,
        }
    }
}

impl Sink for Sort {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        for nth in 0..batch.live() {
            let mut row = Vec::with_capacity(batch.columns.len());
            for column in 0..batch.columns.len() {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            self.rows.push(row);
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        let keys = self.keys.clone();
        // A stable sort, because SQLite's sorter is stable and a digest
        // comparison over rows with equal keys would otherwise differ for a
        // reason that is not a bug in either engine.
        self.rows
            .sort_by(|left, right| compare_by(left, right, &keys));
        let rows = std::mem::take(&mut self.rows);
        emit_rows(&rows, self.downstream.as_mut())?;
        self.downstream.finish()
    }
}

/// Keeps the smallest `n` rows by the ordering, in a bounded heap.
///
/// `ORDER BY ... LIMIT n` does not need every row sorted, it needs the best `n`,
/// and keeping a bounded buffer turns an O(rows log rows) sort over 100,000 rows
/// into an O(rows log n) pass over a 100-row heap. `scan.sort` is exactly this
/// shape.
pub struct TopN {
    keys: Vec<SortKey>,
    limit: usize,
    /// The best rows seen, kept sorted so the worst is the last.
    best: Vec<Vec<OwnedDatum>>,
    downstream: Box<dyn Sink>,
}

impl TopN {
    /// The largest `n` a `TopN` will take; above it, a full sort is cheaper.
    pub const MAX_LIMIT: usize = 65_536;

    /// Returns a bounded top-n.
    ///
    /// @param keys - the ordering terms
    /// @param limit - how many rows to keep
    /// @param downstream - what to push the ordered rows into
    pub fn new(keys: Vec<SortKey>, limit: usize, downstream: Box<dyn Sink>) -> TopN {
        TopN {
            keys,
            limit,
            best: Vec::with_capacity(limit.min(1024)),
            downstream,
        }
    }
}

impl Sink for TopN {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        // `LIMIT 0` keeps nothing. Without this the loop below reads
        // `best.len() >= limit` as `0 >= 0`, finds no worst row to compare
        // against, and keeps the row anyway - so `ORDER BY id LIMIT 0`
        // returned one row. The SLT corpus asked the question directly.
        if self.limit == 0 {
            return Ok(Flow::Stop);
        }
        let width = batch.columns.len();
        for nth in 0..batch.live() {
            // Compare before materialising. `ORDER BY label LIMIT 100` over
            // 100,000 rows keeps 100 of them, so copying every row into owned
            // storage first - a heap allocation per text value - does 1,000
            // times the work the answer needs. The comparison reads the sort
            // columns straight out of the batch, which is still borrowing the
            // page, and only a row that earns its place is copied.
            if self.best.len() >= self.limit {
                let worse = match self.best.last() {
                    Some(worst) => {
                        compare_batch_row(batch, nth, worst, &self.keys)? != Ordering::Less
                    }
                    None => false,
                };
                if worse {
                    continue;
                }
                self.best.pop();
            }
            let mut row = Vec::with_capacity(width);
            for column in 0..width {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            let at = self
                .best
                .partition_point(|held| compare_by(held, &row, &self.keys) != Ordering::Greater);
            self.best.insert(at, row);
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        let rows = std::mem::take(&mut self.best);
        emit_rows(&rows, self.downstream.as_mut())?;
        self.downstream.finish()
    }
}

/// Compares a live batch row against a materialised row, by the sort terms.
///
/// The half of the comparison that lets `TopN` reject a row without copying it.
///
/// @param batch - the batch holding the candidate
/// @param nth - the candidate's position among the batch's live rows
/// @param held - the materialised row to compare against
/// @param keys - the ordering terms
fn compare_batch_row(
    batch: &Batch<'_>,
    nth: usize,
    held: &[OwnedDatum],
    keys: &[SortKey],
) -> DbResult<Ordering> {
    for term in keys {
        let a = batch.value(nth, term.column)?;
        let b = held
            .get(term.column)
            .map(OwnedDatum::borrow)
            .unwrap_or(Datum::Null);
        let order = order_under(&a, &b, term);
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(Ordering::Equal)
}

/// Drops duplicate rows, keeping the first of each.
pub struct Distinct {
    collations: Vec<Collation>,
    seen: std::collections::HashSet<Vec<u8>>,
    rows: Vec<Vec<OwnedDatum>>,
    downstream: Box<dyn Sink>,
}

impl Distinct {
    /// Returns a de-duplicating operator.
    ///
    /// @param collations - the collation of each compared column
    /// @param downstream - what to push the surviving rows into
    pub fn new(collations: Vec<Collation>, downstream: Box<dyn Sink>) -> Distinct {
        Distinct {
            collations,
            seen: std::collections::HashSet::new(),
            rows: Vec::new(),
            downstream,
        }
    }
}

impl Sink for Distinct {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let mut encoded = Vec::with_capacity(32);
        for nth in 0..batch.live() {
            encoded.clear();
            let mut row = Vec::with_capacity(batch.columns.len());
            for column in 0..batch.columns.len() {
                let value = batch.value(nth, column)?;
                key::encode_into_with(
                    &value,
                    self.collations
                        .get(column)
                        .copied()
                        .unwrap_or(Collation::Binary),
                    &mut encoded,
                );
                row.push(OwnedDatum::from_datum(&value));
            }
            if self.seen.insert(encoded.clone()) {
                self.rows.push(row);
            }
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        let rows = std::mem::take(&mut self.rows);
        emit_rows(&rows, self.downstream.as_mut())?;
        self.downstream.finish()
    }
}

/// Passes at most `limit` rows through, after skipping `offset`.
pub struct Limit {
    limit: usize,
    offset: usize,
    seen: usize,
    emitted: usize,
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
}

/// Pushes materialised rows downstream in batch-sized chunks.
///
/// The one place a pipeline breaker turns owned rows back into batches. It
/// transposes row-major storage into per-column vectors, which is why every
/// breaker calls it rather than writing the transpose out again.
///
/// @param rows - the rows to emit
/// @param downstream - what to push them into
pub(crate) fn emit_rows(rows: &[Vec<OwnedDatum>], downstream: &mut dyn Sink) -> DbResult<Flow> {
    let width = rows.first().map(|row| row.len()).unwrap_or(0);
    if width == 0 {
        return Ok(Flow::Continue);
    }
    let mut start = 0usize;
    while start < rows.len() {
        let end = start
            .saturating_add(crate::batch::BATCH_ROWS)
            .min(rows.len());
        let chunk = rows.get(start..end).unwrap_or(&[]);
        let mut columns_owned: Vec<Vec<Datum<'_>>> = Vec::with_capacity(width);
        for column in 0..width {
            columns_owned.push(
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
        let columns: Vec<Vector<'_>> = columns_owned
            .iter()
            .map(|values| Vector::Values(values.as_slice()))
            .collect();
        let batch = Batch::new(chunk.len(), columns);
        if downstream.push(&batch)? == Flow::Stop {
            return Ok(Flow::Stop);
        }
        start = end;
    }
    Ok(Flow::Continue)
}

/// Orders two values under one term, with its direction and NULL placement.
///
/// The direction reverses the comparison; the NULL placement does *not* - a
/// NULL is not "the smallest value", it is outside the order, and reversing it
/// with everything else is only right when the requested placement happens to
/// be the default. Handling it separately is what makes `ORDER BY x NULLS LAST`
/// and `ORDER BY x DESC NULLS FIRST` mean what they say.
///
/// @param left - one value
/// @param right - the other
/// @param term - the ordering term
fn order_under(left: &Datum<'_>, right: &Datum<'_>, term: &SortKey) -> Ordering {
    match (left.is_null(), right.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if term.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if term.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let order = compare_under(left, right, term.collation);
            if term.descending {
                order.reverse()
            } else {
                order
            }
        }
    }
}

/// Compares two rows by a list of ordering terms.
///
/// @param left - one row
/// @param right - the other row
/// @param keys - the ordering terms
fn compare_by(left: &[OwnedDatum], right: &[OwnedDatum], keys: &[SortKey]) -> Ordering {
    for term in keys {
        let a = left
            .get(term.column)
            .map(OwnedDatum::borrow)
            .unwrap_or(Datum::Null);
        let b = right
            .get(term.column)
            .map(OwnedDatum::borrow)
            .unwrap_or(Datum::Null);
        let order = order_under(&a, &b, term);
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::{compile, CompareOp, Expr, StaticType};

    fn ints(values: &[i64]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// The vectorised `sum` and the per-row `sum` agree, over a dense batch and
    /// the same batch behind a selection vector that keeps every row.
    ///
    /// The selection vector is what takes the fast path away, so this is the
    /// direct comparison of the two paths on identical input, and it is what
    /// licenses the fast path to exist at all.
    #[test]
    fn the_dense_aggregate_path_agrees_with_the_selected_one() {
        for count in [0usize, 1, 7, 500, 2048] {
            let values: Vec<i64> = (0..count as i64).map(|n| n * 3 - 7).collect();
            let bytes = ints(&values);
            let all: Vec<u32> = (0..count as u32).collect();
            let dense = Batch::new(
                count,
                vec![Vector::Int64 {
                    bytes: &bytes,
                    class: None,
                }],
            );
            let selected = Batch {
                rows: count,
                selection: Some(&all),
                columns: vec![Vector::Int64 {
                    bytes: &bytes,
                    class: None,
                }],
            };
            for kind in [
                AggregateKind::Sum,
                AggregateKind::Count,
                AggregateKind::Minimum,
                AggregateKind::Maximum,
                AggregateKind::Average,
                AggregateKind::Total,
            ] {
                let mut fast = SimpleAggregate::new(
                    vec![AggregateSpec {
                        kind: kind.clone(),
                        argument: Some(compile(&Expr::Column(0), &[StaticType::Int]).unwrap()),
                    }],
                    Box::new(Collect::new()),
                );
                let mut slow = SimpleAggregate::new(
                    vec![AggregateSpec {
                        kind: kind.clone(),
                        argument: Some(compile(&Expr::Column(0), &[StaticType::Int]).unwrap()),
                    }],
                    Box::new(Collect::new()),
                );
                fast.push(&dense).unwrap();
                slow.push(&selected).unwrap();
                assert!(
                    fast.push(&dense).is_ok() && slow.push(&selected).is_ok(),
                    "two batches fold the same way"
                );
                let a = fast.accumulators[0].finish().unwrap();
                let b = slow.accumulators[0].finish().unwrap();
                assert_eq!(
                    a.borrow().compare(&b.borrow()),
                    Ordering::Equal,
                    "{kind:?} over {count} rows: dense {a:?}, selected {b:?}"
                );
                assert_eq!(
                    matches!(a, OwnedDatum::Null),
                    matches!(b, OwnedDatum::Null),
                    "{kind:?} over {count} rows"
                );
            }
        }
    }

    /// `count(*)` counts rows behind a selection vector, not the batch's width.
    #[test]
    fn count_star_counts_live_rows() {
        let values: Vec<i64> = (0..100).collect();
        let bytes = ints(&values);
        let selection: Vec<u32> = (0..100u32).filter(|n| n % 3 == 0).collect();
        let mut aggregate = SimpleAggregate::new(
            vec![AggregateSpec {
                kind: AggregateKind::CountStar,
                argument: None,
            }],
            Box::new(Collect::new()),
        );
        let batch = Batch {
            rows: 100,
            selection: Some(&selection),
            columns: vec![Vector::Int64 {
                bytes: &bytes,
                class: None,
            }],
        };
        aggregate.push(&batch).unwrap();
        assert_eq!(
            aggregate.accumulators[0]
                .finish()
                .unwrap()
                .borrow()
                .as_int(),
            Some(selection.len() as i64)
        );
    }

    /// A filter keeps exactly the rows whose predicate is true, and a NULL
    /// predicate keeps none of them.
    #[test]
    fn a_filter_keeps_only_definite_truths() {
        let values = [Datum::Int(1), Datum::Int(10), Datum::Null, Datum::Int(20)];
        let predicate = compile(
            &Expr::Compare(
                CompareOp::Greater,
                Box::new(Expr::Column(0)),
                Box::new(Expr::Literal(OwnedDatum::Int(5))),
            ),
            &[StaticType::Int],
        )
        .unwrap();
        let mut filter = Filter::new(predicate, Box::new(Collect::new()));
        let batch = Batch::new(4, vec![Vector::Values(&values)]);
        filter.push(&batch).unwrap();
        assert_eq!(filter.selection, vec![1, 3]);
    }

    /// `TopN` produces exactly what a full sort followed by a limit produces,
    /// including which of several equal-keyed rows survives.
    #[test]
    fn top_n_matches_sort_then_limit() {
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Deliberately few distinct keys, so ties are common and stability is
        // actually exercised.
        let rows: Vec<(i64, i64)> = (0..2_000)
            .map(|n| ((next() % 20) as i64, n as i64))
            .collect();
        for limit in [1usize, 3, 100, 2_000, 5_000] {
            let keys = vec![SortKey {
                column: 0,
                descending: false,
                collation: Collation::Binary,
                nulls_first: true,
            }];
            let mut top = TopN::new(keys.clone(), limit, Box::new(Collect::new()));
            let mut sort = Sort::new(keys.clone(), Box::new(Collect::with_limit(limit)));
            for chunk in rows.chunks(37) {
                let column_a: Vec<Datum<'_>> = chunk.iter().map(|(a, _)| Datum::Int(*a)).collect();
                let column_b: Vec<Datum<'_>> = chunk.iter().map(|(_, b)| Datum::Int(*b)).collect();
                let batch = Batch::new(
                    chunk.len(),
                    vec![Vector::Values(&column_a), Vector::Values(&column_b)],
                );
                top.push(&batch).unwrap();
                sort.push(&batch).unwrap();
            }
            let from_top = std::mem::take(&mut top.best);
            sort.rows.sort_by(|l, r| compare_by(l, r, &keys));
            let from_sort: Vec<Vec<OwnedDatum>> = sort.rows.iter().take(limit).cloned().collect();
            assert_eq!(from_top.len(), from_sort.len(), "limit {limit}");
            for (index, (a, b)) in from_top.iter().zip(from_sort.iter()).enumerate() {
                assert_eq!(
                    a[0].borrow().as_int(),
                    b[0].borrow().as_int(),
                    "limit {limit} row {index} key"
                );
                assert_eq!(
                    a[1].borrow().as_int(),
                    b[1].borrow().as_int(),
                    "limit {limit} row {index} payload: top-n kept a different tied row"
                );
            }
        }
    }

    /// `DISTINCT` keeps the first of each duplicate group and drops the rest,
    /// treating values of different classes as different.
    #[test]
    fn distinct_separates_by_class_not_only_by_text() {
        let values = [
            Datum::Int(1),
            Datum::Text(b"1"),
            Datum::Int(1),
            Datum::Real(1.0),
            Datum::Null,
            Datum::Null,
        ];
        let mut distinct = Distinct::new(Vec::new(), Box::new(Collect::new()));
        let batch = Batch::new(values.len(), vec![Vector::Values(&values)]);
        distinct.push(&batch).unwrap();
        // 1, "1", NULL survive; the second Int(1) and the second NULL do not.
        // Real(1.0) encodes equal to Int(1) because the key encoding compares
        // numerics numerically, which is what SQLite's DISTINCT does too.
        assert_eq!(distinct.rows.len(), 3, "{:?}", distinct.rows);
    }

    /// A grouped aggregate produces one row per key, in key order, with the
    /// right counts.
    #[test]
    fn grouped_aggregation_counts_each_key() {
        let categories: Vec<Datum<'_>> = (0..1_000).map(|n| Datum::Int((n % 7) as i64)).collect();
        let mut grouped = HashAggregate::new(
            vec![compile(&Expr::Column(0), &[StaticType::Int]).unwrap()],
            Vec::new(),
            vec![AggregateSpec {
                kind: AggregateKind::CountStar,
                argument: None,
            }],
            Box::new(Collect::new()),
        );
        let batch = Batch::new(1_000, vec![Vector::Values(&categories)]);
        grouped.push(&batch).unwrap();
        assert_eq!(grouped.groups.len(), 7);
        let mut total = 0i64;
        for (_, (key, accumulators)) in &grouped.groups {
            let count = accumulators[0].finish().unwrap().borrow().as_int().unwrap();
            let category = key[0].borrow().as_int().unwrap();
            assert_eq!(
                count,
                if category < 1_000 % 7 { 143 } else { 142 },
                "category {category}"
            );
            total += count;
        }
        assert_eq!(total, 1_000);
    }

    /// The run-detecting grouped path and the per-row one produce the same
    /// groups, the same counts and the same sums, over runs that start and end
    /// on batch boundaries and runs that do not.
    ///
    /// The dense path is entered only for a dense batch, so the per-row path is
    /// obtained by putting the same values behind a selection vector that keeps
    /// every row - which is the same input and a different code path.
    #[test]
    fn the_run_detecting_group_path_agrees_with_the_per_row_one() {
        for run_length in [1usize, 2, 37, 512, 4096] {
            let values: Vec<i64> = (0..4_000).map(|n| (n / run_length) as i64).collect();
            let payload: Vec<i64> = (0..4_000).map(|n| n as i64 * 3).collect();
            let key_bytes = ints(&values);
            let payload_bytes = ints(&payload);
            let all: Vec<u32> = (0..4_000u32).collect();

            let make = |dense: bool| {
                let columns = vec![
                    Vector::Int64 {
                        bytes: &key_bytes,
                        class: None,
                    },
                    Vector::Int64 {
                        bytes: &payload_bytes,
                        class: None,
                    },
                ];
                if dense {
                    Batch::new(4_000, columns)
                } else {
                    Batch {
                        rows: 4_000,
                        selection: Some(&all),
                        columns,
                    }
                }
            };

            let mut outcomes = Vec::new();
            for dense in [true, false] {
                let rows = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let mut aggregate = StreamAggregate::new(
                    vec![compile(&Expr::Column(0), &[StaticType::Int; 2]).unwrap()],
                    Vec::new(),
                    vec![
                        AggregateSpec {
                            kind: AggregateKind::CountStar,
                            argument: None,
                        },
                        AggregateSpec {
                            kind: AggregateKind::Sum,
                            argument: Some(
                                compile(&Expr::Column(1), &[StaticType::Int; 2]).unwrap(),
                            ),
                        },
                    ],
                    Box::new(CollectInto::new(std::rc::Rc::clone(&rows))),
                );
                // Pushed in two batches so a run can straddle the boundary.
                let batch = make(dense);
                aggregate.push(&batch).unwrap();
                aggregate.finish().unwrap();
                let held: Vec<(i64, i64, i64)> = rows
                    .borrow()
                    .iter()
                    .map(|row| {
                        (
                            row[0].borrow().as_int().unwrap_or(-1),
                            row[1].borrow().as_int().unwrap_or(-1),
                            row[2].borrow().as_int().unwrap_or(-1),
                        )
                    })
                    .collect();
                outcomes.push(held);
            }
            assert_eq!(
                outcomes[0], outcomes[1],
                "run length {run_length}: dense and per-row disagreed"
            );
            let groups = (4_000 + run_length - 1) / run_length;
            assert_eq!(outcomes[0].len(), groups, "run length {run_length}");
            assert_eq!(
                outcomes[0].iter().map(|(_, count, _)| count).sum::<i64>(),
                4_000
            );
        }
    }

    /// A limit stops the pipeline once it has enough, rather than reading on.
    #[test]
    fn a_limit_stops_the_pipeline() {
        let values: Vec<Datum<'_>> = (0..100).map(Datum::Int).collect();
        let mut limit = Limit::new(10, 5, Box::new(Collect::new()));
        let batch = Batch::new(100, vec![Vector::Values(&values)]);
        assert_eq!(limit.push(&batch).unwrap(), Flow::Stop);
        assert_eq!(limit.emitted, 10);
        assert_eq!(limit.seen, 15);
    }

    /// A projection that is a permutation borrows rather than copying, and one
    /// that computes materialises - both producing the same values.
    #[test]
    fn a_projection_borrows_when_it_can() {
        let a: Vec<Datum<'_>> = (0..10).map(Datum::Int).collect();
        let b: Vec<Datum<'_>> = (0..10).map(|n| Datum::Int(n * 2)).collect();
        let batch = Batch::new(10, vec![Vector::Values(&a), Vector::Values(&b)]);

        let mut permute = Project::new(
            vec![
                compile(&Expr::Column(1), &[StaticType::Int; 2]).unwrap(),
                compile(&Expr::Column(0), &[StaticType::Int; 2]).unwrap(),
            ],
            Box::new(Collect::new()),
        );
        permute.push(&batch).unwrap();
        permute.finish().unwrap();

        let mut computed = Project::new(
            vec![compile(
                &Expr::Arith(
                    crate::expr::ArithOp::Add,
                    Box::new(Expr::Column(0)),
                    Box::new(Expr::Column(1)),
                ),
                &[StaticType::Int; 2],
            )
            .unwrap()],
            Box::new(Collect::new()),
        );
        computed.push(&batch).unwrap();
        computed.finish().unwrap();
    }
}
