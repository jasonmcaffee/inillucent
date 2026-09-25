//! The three aggregating operators, and the specification they share.
//!
//! Invariant: **which one the planner chooses is a claim about the input's
//! order.** `StreamAggregate` and `AdjacentDistinct` are correct only when
//! equal keys arrive together, which is what an ordered walk promises and
//! what the `ORDERED_WALK` lever exists to be able to take away.

use std::cmp::Ordering;
use std::collections::HashMap;

use inillucent_base::DbResult;
use inillucent_tree::datum::{borrow_row, Datum, OwnedDatum};
use inillucent_tree::key;
use inillucent_tree::types::compare_under;
use inillucent_value::collation::Collation;
use inillucent_value::Value;

use crate::aggregate::{Accumulator, AggregateKind, SortTerm};
use crate::batch::{Batch, Vector};
use crate::expr::Eval;

use super::*;

/// One aggregate and the expression it reads.
pub struct AggregateSpec {
    /// Which aggregate to compute.
    pub kind: AggregateKind,
    /// The argument, or `None` for `count(*)`.
    pub argument: Option<Box<dyn Eval>>,
    /// The arguments after the first, for a registered aggregate.
    ///
    /// Empty for every built-in: each of those reduces one value per row, and
    /// `group_concat`'s separator is a constant the kind already carries. A
    /// registered aggregate is handed the whole row, so an `agg(a, b)` needs
    /// `b` as well - and it is kept beside `argument` rather than replacing it
    /// so the vectorised single-value path stays exactly as it was.
    pub extra: Vec<Box<dyn Eval>>,
    /// The collation each distinct value is compared under, when the call said
    /// `DISTINCT`.
    pub distinct: Option<Collation>,
    /// The call's `FILTER (WHERE ...)`, when it had one.
    ///
    /// A row it does not keep is not folded in at all: it does not count, it
    /// does not sum, and it does not appear in a `group_concat`.
    pub filter: Option<Box<dyn Eval>>,
    /// The call's own `ORDER BY`, as one expression per term with its
    /// direction.
    ///
    /// Empty for nearly every call. When it is not, the accumulator collects
    /// its rows and sorts them before folding, because the answer of a
    /// `group_concat` or a `json_group_array` is the order the rows arrived in.
    pub order_by: Vec<(Box<dyn Eval>, SortTerm)>,
    /// The collation `min` and `max` compare their argument under, and a bare
    /// column compares the `min` or `max` it follows under.
    ///
    /// SQLite compares with the argument's collation, so `max(name)` over a
    /// `NOCASE` column is `Zebra` where a byte comparison says `outdoor`.
    /// `BINARY` for everything else, which never reads it.
    pub collation: Collation,
}
impl AggregateSpec {
    /// Returns a fresh accumulator for this aggregate.
    ///
    /// One place, because three operators build them and a `DISTINCT` the
    /// stream aggregate forgot would count duplicates in exactly the groups
    /// nobody looked at.
    pub fn accumulator(&self) -> Accumulator {
        let mut built = self.fresh();
        if !self.order_by.is_empty() {
            built.sort_by(
                self.argument.is_some() as usize + self.extra.len(),
                self.order_by.iter().map(|(_, term)| *term).collect(),
            );
        }
        built
    }

    /// Returns a fresh accumulator, before the sort is attached.
    fn fresh(&self) -> Accumulator {
        let mut built = match self.distinct {
            Some(collation) => Accumulator::distinct(self.kind.clone(), collation),
            None => Accumulator::new(self.kind.clone()),
        };
        built.compare_under(self.collation);
        built
    }

    /// Reports whether this call needs the whole row rather than one value.
    ///
    /// Only a registered aggregate of more than one argument does. Asking here
    /// keeps the three operators that feed accumulators from each carrying a
    /// copy of the rule.
    pub fn takes_whole_row(&self) -> bool {
        // A JSON group aggregate too, whatever its arity: a NULL is a member of
        // the document it builds rather than a row to skip, and the
        // single-value path drops NULLs before the accumulator sees them. And
        // any call with its own `ORDER BY`, whose rows have to be kept until
        // there is something to sort them by.
        !self.extra.is_empty()
            || !self.order_by.is_empty()
            || matches!(
                self.kind,
                AggregateKind::JsonGroupArray(_)
                    | AggregateKind::JsonGroupObject(_)
                    | AggregateKind::Bare(_)
            )
    }

    /// Folds one row into an accumulator, applying the call's own clauses.
    ///
    /// **The one place the three aggregate operators agree.** Each of them used
    /// to choose between the whole-row path and the single-value one for
    /// itself; adding `FILTER` to three copies of that choice is how one of
    /// them ends up not applying it.
    ///
    /// @param accumulator - the group's accumulator
    /// @param batch - the batch being consumed
    /// @param nth - the row's position among the live rows
    pub fn feed(
        &self,
        accumulator: &mut Accumulator,
        batch: &Batch<'_>,
        nth: usize,
    ) -> DbResult<()> {
        if let Some(filter) = &self.filter {
            let verdict = filter.value(batch, nth)?;
            if crate::expr::truth(&verdict.get()) != Some(true) {
                return Ok(());
            }
        }
        if self.takes_whole_row() {
            return self.feed_row(accumulator, batch, nth);
        }
        match &self.argument {
            None => accumulator.push(&Datum::Null),
            Some(argument) => accumulator.push(&argument.value(batch, nth)?.get()),
        }
        Ok(())
    }

    /// Folds one row of arguments into an accumulator.
    ///
    /// @param accumulator - the group's accumulator
    /// @param batch - the batch being consumed
    /// @param nth - the row's position among the live rows
    pub fn feed_row(
        &self,
        accumulator: &mut Accumulator,
        batch: &Batch<'_>,
        nth: usize,
    ) -> DbResult<()> {
        let mut values = Vec::with_capacity(self.extra.len().saturating_add(1));
        // **The JSON subtype of each argument, for the JSON group aggregates.**
        // `json_value` answers it from the JSON function that produced the
        // value; every other expression answers no.
        let json = matches!(
            self.kind,
            AggregateKind::JsonGroupArray(_) | AggregateKind::JsonGroupObject(_)
        );
        let mut marks = 0u32;
        for argument in self.argument.iter().chain(self.extra.iter()) {
            let (value, marked) = if json {
                argument.json_value(batch, nth)?
            } else {
                (argument.value(batch, nth)?, false)
            };
            if marked {
                marks |= 1u32.checked_shl(values.len() as u32).unwrap_or(0);
            }
            values.push(Value::from(&value.get()).into_owned()?);
        }
        // The sort keys go on the end, where `Accumulator::finish` knows to
        // find them: it is told how many there are when the accumulator is
        // built.
        for (key, _) in &self.order_by {
            values.push(Value::from(&key.value(batch, nth)?.get()).into_owned()?);
        }
        accumulator.push_values_marked(values, marks);
        Ok(())
    }
}
/// Aggregates the whole input into one row.
pub struct SimpleAggregate {
    specs: Vec<AggregateSpec>,
    pub(crate) accumulators: Vec<Accumulator>,
    downstream: Box<dyn Sink>,
}
impl SimpleAggregate {
    /// Returns a whole-input aggregate.
    ///
    /// @param specs - one per output column
    /// @param downstream - what to push the single result row into
    pub fn new(specs: Vec<AggregateSpec>, downstream: Box<dyn Sink>) -> SimpleAggregate {
        let accumulators = specs.iter().map(|spec| spec.accumulator()).collect();
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
            // A filter or a whole-row call is fed a row at a time; the
            // vectorised paths below read a whole mini-column and cannot skip
            // rows.
            if spec.takes_whole_row() || spec.filter.is_some() {
                for nth in 0..live {
                    spec.feed(accumulator, batch, nth)?;
                }
                continue;
            }
            match &spec.argument {
                // **`count(*)` over a batch: one addition** (task-2000, design 4).
                // This was a loop of `live` calls, each of which compared a
                // discriminant and added one. There is no value to read, so the
                // whole batch is one call. See `Accumulator::push_count`.
                None => accumulator.push_count(live),
                Some(argument) => {
                    // The vectorised path: a bare reference to a dense integer
                    // column of a batch with no selection vector.
                    // A `DISTINCT` accumulator has to look at every value to
                    // decide whether it has seen it, so the vectorised path is
                    // not available to it - and one that took it anyway would
                    // count duplicates. The accumulator is asked rather than
                    // the operator remembering, because there are three
                    // operators and one accumulator.
                    let dense = if batch.is_dense() && accumulator.takes_dense() {
                        argument
                            .column()
                            .and_then(|column| batch.columns.get(column))
                            .and_then(|vector| vector.dense_ints())
                    } else {
                        None
                    };
                    match dense {
                        Some(slots) => {
                            accumulator.push_dense_ints(slots.range(0, live));
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

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        for (index, spec) in self.specs.iter().enumerate() {
            if let Some(slot) = self.accumulators.get_mut(index) {
                *slot = spec.accumulator();
            }
        }
        self.downstream.reset()
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
    pub(crate) groups: HashMap<Vec<u8>, (Vec<OwnedDatum>, Vec<Accumulator>)>,
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
            // **The group table is charged as it grows (task-1932, H6).**
            // A new key is memory the statement holds until it finishes, and
            // the number of them is decided by the data rather than by the
            // query - `GROUP BY` over a column with a hundred million distinct
            // values builds a hundred million groups to answer with a hundred
            // million rows. The input is deliberately not charged: a `GROUP BY`
            // over a large table that answers in four rows is the shape this
            // operator exists for.
            let fresh = !self.groups.contains_key(&encoded);
            if fresh {
                let held = values
                    .iter()
                    .map(crate::expr::Computed::get)
                    .map(|value| datum_bytes(&value))
                    .sum::<u64>();
                inillucent_base::budget::materialise(held.saturating_add(encoded.len() as u64))?;
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
                    self.specs.iter().map(|spec| spec.accumulator()).collect(),
                )
            });
            for (index, spec) in self.specs.iter().enumerate() {
                let Some(accumulator) = entry.1.get_mut(index) else {
                    continue;
                };
                spec.feed(accumulator, batch, nth)?;
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

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.groups.clear();
        self.downstream.reset()
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
        let accumulators = specs.iter().map(|spec| spec.accumulator()).collect();
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
        self.accumulators = self.specs.iter().map(|spec| spec.accumulator()).collect();
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
            // A filter or a whole-row call is fed a row at a time; the
            // vectorised paths below read a whole mini-column and cannot skip
            // rows.
            if spec.takes_whole_row() || spec.filter.is_some() {
                for nth in start..start.saturating_add(len) {
                    spec.feed(accumulator, batch, nth)?;
                }
                continue;
            }
            match &spec.argument {
                // `count(*)` over a run: the length, no value read at all, and
                // one addition rather than `len` of them (task-2000, design 4).
                // A run of 1,562 rows in `scan.group` is now one subtraction the
                // run finder already did plus one add.
                None => accumulator.push_count(len),
                Some(argument) => {
                    // As above: a `DISTINCT` accumulator sees every value.
                    let dense = if accumulator.takes_dense() {
                        argument
                            .column()
                            .and_then(|column| batch.columns.get(column))
                            .and_then(|vector| vector.dense_ints())
                    } else {
                        None
                    };
                    match dense {
                        Some(slots) => {
                            accumulator
                                .push_dense_ints(slots.range(start, start.saturating_add(len)));
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
                .and_then(|vector| vector.dense_ints())
        } else {
            None
        };
        if let Some(slots) = dense {
            let rows = batch.live().min(slots.len());
            let mut start = 0usize;
            while start < rows {
                let value = slots.get(start);
                let mut end = start.saturating_add(1);
                while end < rows && slots.get(end) == value {
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
                // **Through `feed`, which is where `FILTER` is applied
                // (task-1932).** This loop used to push the argument straight
                // into the accumulator, so an aggregate's `FILTER (WHERE ...)`
                // was ignored outright by this path - `SELECT team, count(*)
                // FILTER (WHERE score > 0) FROM a GROUP BY team` counted every
                // row of every group. The dense integer-key path above already
                // went through `fold_run`, which checks it, so the same
                // statement answered correctly over an integer group key and
                // wrongly over a text one, which is why nothing caught it.
                // `feed` also carries the whole-row and inner-`ORDER BY` cases
                // this loop had no idea about.
                spec.feed(accumulator, batch, nth)?;
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

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.current = None;
        self.rows.clear();
        for (index, spec) in self.specs.iter().enumerate() {
            if let Some(slot) = self.accumulators.get_mut(index) {
                *slot = spec.accumulator();
            }
        }
        self.downstream.reset()
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
    /// How many of the row's leading columns decide duplication.
    ///
    /// See [`Distinct::over`] for why a row may be wider than that.
    compared: usize,
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
        AdjacentDistinct::over(collations, usize::MAX, downstream)
    }

    /// Returns one that compares only the row's leading columns.
    ///
    /// @param collations - the collation of each compared column
    /// @param compared - how many leading columns decide duplication
    /// @param downstream - what to push the surviving rows into
    pub fn over(
        collations: Vec<Collation>,
        compared: usize,
        downstream: Box<dyn Sink>,
    ) -> AdjacentDistinct {
        AdjacentDistinct {
            collations,
            compared,
            previous: None,
            rows: Vec::new(),
            downstream,
        }
    }
}
impl Sink for AdjacentDistinct {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let width = batch.columns.len();
        let compared = width.min(self.compared);
        for nth in 0..batch.live() {
            let mut same = self.previous.is_some();
            if same {
                for column in 0..compared {
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

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.previous = None;
        self.rows.clear();
        self.downstream.reset()
    }
}
