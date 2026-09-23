//! Sorting, top-n, and comparing two rows by a sort key.
//!
//! Invariant: **the comparison is the dialect's, not Rust's.** NULL sorts
//! where SQLite puts it, a collation decides text, and `NULLS FIRST` is a
//! property of the key rather than of the sorter.

use std::cmp::Ordering;

use inillucent_base::DbResult;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::key;
use inillucent_tree::types::compare_under;
use inillucent_value::collation::Collation;

use crate::batch::{Batch, DenseInts, Vector};

use super::*;

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
/// How many bytes `Sort` holds before it writes a run and starts again.
///
/// **Far above anything the gate reaches, on purpose** (task-2066 §4.3.6).
/// task-1869 measured a spill at 8 ms on a 27 ms `CREATE INDEX` and it put the
/// `schema` family under its floor, so a threshold a benchmark trips is a
/// threshold that makes every benchmark about the spill. The whole medium
/// fixture is 17 MB and `scan.sort` sorts a fraction of it; sixty-four
/// mebibytes is past every workload in the plan and far under the memory a
/// machine running one has.
///
/// It bounds what one `Sort` holds, not what a statement holds: a query with
/// two of them can hold twice this, which is what the byte budget is for.
const SPILL_BYTES: u64 = 64 * 1024 * 1024;

/// Sorts every row, then emits.
pub struct Sort {
    keys: Vec<SortKey>,
    pub(crate) rows: Vec<Vec<OwnedDatum>>,
    downstream: Box<dyn Sink>,
    /// Where runs go, when the caller gave this sort somewhere to put them.
    spill: Option<std::rc::Rc<dyn crate::spill::Spill>>,
    /// The file the runs are in, opened on the first spill and not before.
    file: Option<Box<dyn crate::spill::SpillFile>>,
    /// Every run written so far.
    runs: Vec<crate::spill::Run>,
    /// How many bytes the buffer holds, so the threshold is a comparison
    /// rather than a walk of the rows.
    held: u64,
    /// How many bytes this sort holds before it writes a run.
    ///
    /// `SPILL_BYTES` unless a test lowered it; see `spilling_at`.
    threshold: u64,
    /// How many runs were written, kept past the merge that clears them.
    ///
    /// **So a test can say whether it spilled at all.** Without it a case that
    /// set a threshold the rows never reached would compare an in-memory sort
    /// against an in-memory sort and pass, which is rule 1.2's failure exactly.
    written: usize,
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
            spill: None,
            file: None,
            runs: Vec::new(),
            held: 0,
            threshold: SPILL_BYTES,
            written: 0,
        }
    }

    /// How many runs this sort wrote.
    #[cfg(test)]
    pub(crate) fn runs_written(&self) -> usize {
        self.written
    }

    /// Returns this sort with a different spill threshold.
    ///
    /// **Only a test moves it.** The shipped threshold is far above anything a
    /// benchmark reaches, on purpose, and that is also far above anything a
    /// unit test can build in the milliseconds this file's cases take - so
    /// without this the merge would be code nothing runs.
    ///
    /// @param bytes - how many bytes to hold before writing a run
    #[cfg(test)]
    pub(crate) fn spilling_at(mut self, bytes: u64) -> Sort {
        self.threshold = bytes;
        self
    }

    /// Returns this sort with somewhere to spill to.
    ///
    /// **Without one it behaves exactly as it did before spilling existed**
    /// (task-2066 §4.3.6): every row is held and the byte budget refuses the
    /// statement if a budget was armed. `TreeCatalog::spill` answers `None` by
    /// default, so that is what an embedded caller and every test harness get
    /// until something hands one over.
    ///
    /// @param spill - where runs go
    pub fn spilling_to(mut self, spill: Option<std::rc::Rc<dyn crate::spill::Spill>>) -> Sort {
        self.spill = spill;
        self
    }

    /// Writes what is held as one run and empties the buffer.
    ///
    /// The rows are sorted first, because a run is read back in order and the
    /// merge assumes it.
    fn write_a_run(&mut self) -> DbResult<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let Some(spill) = self.spill.clone() else {
            return Ok(());
        };
        let keys = self.keys.clone();
        sort_rows(&mut self.rows, &keys);
        let bytes = crate::spill::encode_run(&self.rows);
        let rows = self.rows.len();
        if self.file.is_none() {
            self.file = Some(spill.open()?);
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let at = file.append(&bytes)?;
        self.runs.push(crate::spill::Run {
            at,
            bytes: bytes.len() as u64,
            rows,
        });
        self.rows.clear();
        self.held = 0;
        self.written = self.written.saturating_add(1);
        Ok(())
    }

    /// Emits every row of every run, in order, merging as it goes.
    ///
    /// **The comparison is `compare_by`, the same one the in-memory sort
    /// uses.** An external sort that ordered rows by any other rule would be a
    /// second answer to what `ORDER BY` means, and the two would agree until
    /// the first collation nobody tried.
    ///
    /// A linear scan of the fronts rather than a heap: the runs are the
    /// buffer's size over the input's, which is single figures for anything
    /// that fits on a disk, and a scan of eight is cheaper than maintaining a
    /// heap of eight.
    fn merge_the_runs(&mut self) -> DbResult<()> {
        let Some(file) = self.file.take() else {
            return Ok(());
        };
        let mut readers: Vec<crate::spill::RunReader> = self
            .runs
            .iter()
            .map(|run| crate::spill::RunReader::new(*run))
            .collect();
        let mut fronts: Vec<Option<Vec<OwnedDatum>>> = Vec::with_capacity(readers.len());
        for reader in &mut readers {
            fronts.push(reader.next(file.as_ref())?);
        }
        let keys = self.keys.clone();
        let mut batch: Vec<Vec<OwnedDatum>> = Vec::new();
        loop {
            let mut best: Option<usize> = None;
            for (index, front) in fronts.iter().enumerate() {
                let Some(row) = front.as_ref() else {
                    continue;
                };
                let better = match best.and_then(|at| fronts.get(at)).and_then(Option::as_ref) {
                    // **Strictly less, so a tie keeps the earlier run.** The
                    // runs were written in arrival order and `compare_by` is
                    // applied to a stable sort inside each one, so taking the
                    // lowest-numbered run on a tie is what makes the whole
                    // merge stable - which is what SQLite's sorter is, and
                    // what a digest comparison over equal keys depends on.
                    Some(held) => compare_by(row, held, &keys) == Ordering::Less,
                    None => true,
                };
                if better {
                    best = Some(index);
                }
            }
            let Some(at) = best else {
                break;
            };
            let Some(slot) = fronts.get_mut(at) else {
                break;
            };
            let Some(row) = slot.take() else {
                break;
            };
            batch.push(row);
            if let Some(reader) = readers.get_mut(at) {
                *slot = reader.next(file.as_ref())?;
            }
            // Emitted in batches rather than one at a time, and rather than
            // collected whole: collecting would put the answer back in memory,
            // which is what this exists to avoid.
            if batch.len() >= MERGE_BATCH {
                emit_rows(&batch, self.downstream.as_mut())?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            emit_rows(&batch, self.downstream.as_mut())?;
        }
        self.runs.clear();
        Ok(())
    }
}

/// How many merged rows are handed downstream at once.
///
/// The merge emits in batches so the answer is never resident, and a batch
/// this size is what `emit_rows` already turns into one `Batch` elsewhere.
const MERGE_BATCH: usize = 1024;
impl Sink for Sort {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        for nth in 0..batch.live() {
            let mut row = Vec::with_capacity(batch.columns.len());
            for column in 0..batch.columns.len() {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            // **Charged, because this holds every surviving row until
            // `finish`** (task-2066 §4.3.6). `Distinct` below has charged it
            // since task-1932; `Sort` and `TopN` were the only buffering
            // operators a budget could not reach, so a sort larger than memory
            // was an out-of-memory kill rather than the refusal every other
            // breaker gives. `materialise` is a no-op when no request armed a
            // budget, which is the library default, so an embedded caller is
            // unaffected.
            let bytes = owned_row_bytes(&row);
            inillucent_base::budget::materialise(bytes)?;
            self.rows.push(row);
            self.held = self.held.saturating_add(bytes);
            // **Only when there is somewhere to put it.** A sort with no spill
            // file holds everything, exactly as it did before this existed.
            if self.held >= self.threshold && self.spill.is_some() {
                self.write_a_run()?;
            }
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        // A sort that spilled finishes by merging what it wrote, with whatever
        // is still buffered written as one last run so there is one rule
        // rather than two.
        if !self.runs.is_empty() || (self.spill.is_some() && self.file.is_some()) {
            self.write_a_run()?;
            self.merge_the_runs()?;
            return self.downstream.finish();
        }
        let keys = self.keys.clone();
        sort_rows(&mut self.rows, &keys);
        let rows = std::mem::take(&mut self.rows);
        emit_rows(&rows, self.downstream.as_mut())?;
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.rows.clear();
        self.held = 0;
        self.written = 0;
        // The runs go with the file, and the file deletes itself on close.
        self.runs.clear();
        self.file = None;
        self.downstream.reset()
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
    pub(crate) best: Vec<Vec<OwnedDatum>>,
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
/// A sort key's column, resolved once for a whole batch.
///
/// **The rejection test is the whole of `scan.sort`.** `ORDER BY label LIMIT
/// 100` over 100,000 rows keeps a hundred of them and rejects the rest, so the
/// per-row cost of *deciding* to reject is what the workload measures. Reading
/// that value through `Batch::value` costs a bounds-checked column lookup, a
/// match on the selection vector and a match on the vector's variant - per row,
/// for a variant that cannot change inside a batch.
///
/// `inillucent-probeprofile` measured `ORDER BY id LIMIT 100` at 468 us and
/// `ORDER BY label LIMIT 100` at 1,109 us over the same 100,000 rows, against a
/// bare `count(*)` scan of 119 us. Matching the variant once per batch and
/// reading the row's bytes directly is what that difference is spent on.
///
/// The *ordering* still goes through [`order_under`]. Only the read is
/// specialised, because a second implementation of SQL ordering is exactly the
/// kind of duplicate this engine has already been bitten by: the covering rule
/// and the seek path disagreeing about affinity cost a wrong answer that every
/// test of either path passed.
enum KeyColumn<'p> {
    /// A fully typed integer column: one, two, four or eight bytes per row,
    /// no class array.
    Ints(DenseInts<'p>),
    /// A fully typed variable-width column: an offset and a length per row.
    Bytes {
        /// `rows * width` bytes of slots.
        slots: &'p [u8],
        /// How many bytes one slot occupies.
        width: usize,
        /// The page the slots address.
        page: &'p [u8],
        /// Whether the bytes are text rather than a blob.
        text: bool,
    },
    /// Anything else, read through the vector's own accessor.
    General(Vector<'p>),
}
impl<'p> KeyColumn<'p> {
    /// Resolves one column of a batch, once.
    ///
    /// @param batch - the batch being read
    /// @param column - which column the sort term names
    fn of(batch: &Batch<'p>, column: usize) -> KeyColumn<'p> {
        match batch.columns.get(column) {
            Some(
                vector @ Vector::Int64 {
                    bytes: _,
                    width: _,
                    base: _,
                    class: None,
                },
            ) => match vector.dense_ints() {
                Some(slots) => KeyColumn::Ints(slots),
                None => KeyColumn::General(*vector),
            },
            Some(Vector::Variable {
                slots,
                width,
                page,
                text,
            }) => KeyColumn::Bytes {
                slots,
                width: *width,
                page,
                text: *text,
            },
            Some(vector) => KeyColumn::General(*vector),
            None => KeyColumn::General(Vector::Const(Datum::Null)),
        }
    }

    /// Returns one row's value in this column.
    ///
    /// @param row - the row's position within the batch, after any selection
    fn at(&self, row: usize) -> DbResult<Datum<'p>> {
        match self {
            KeyColumn::Ints(slots) => Ok(if row < slots.len() {
                Datum::Int(slots.get(row))
            } else {
                Datum::Null
            }),
            KeyColumn::Bytes {
                slots,
                width,
                page,
                text,
            } => {
                let at = row.saturating_mul(*width);
                let Some(slot) = slots.get(at..at.saturating_add(*width)) else {
                    return Ok(Datum::Null);
                };
                let (offset, length) = inillucent_tree::types::read_heap_slot(slot);
                let bytes = page
                    .get(offset..offset.saturating_add(length))
                    .unwrap_or(&[]);
                Ok(if *text {
                    Datum::Text(bytes)
                } else {
                    Datum::Blob(bytes)
                })
            }
            KeyColumn::General(vector) => vector.at(row),
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
        let TopN {
            keys,
            limit,
            best,
            downstream: _,
        } = self;
        let width = batch.columns.len();
        // One sort term is the shape every `ORDER BY ... LIMIT n` in the
        // scorecard has, and the shape the fast rejection test needs. A
        // multi-term sort falls through to the general comparison, which is
        // what every term used to cost.
        let single = if keys.len() == 1 { keys.first() } else { None };
        let reader = single.map(|term| KeyColumn::of(batch, term.column));
        for nth in 0..batch.live() {
            // Compare before materialising. `ORDER BY label LIMIT 100` over
            // 100,000 rows keeps 100 of them, so copying every row into owned
            // storage first - a heap allocation per text value - does 1,000
            // times the work the answer needs. The comparison reads the sort
            // columns straight out of the batch, which is still borrowing the
            // page, and only a row that earns its place is copied.
            if best.len() >= *limit {
                let worse = match (best.last(), single, reader.as_ref()) {
                    (Some(worst), Some(term), Some(reader)) => {
                        let candidate = reader.at(batch.row_at(nth))?;
                        let held = worst
                            .get(term.column)
                            .map(OwnedDatum::borrow)
                            .unwrap_or(Datum::Null);
                        order_under(&candidate, &held, term) != Ordering::Less
                    }
                    (Some(worst), _, _) => {
                        compare_batch_row(batch, nth, worst, keys)? != Ordering::Less
                    }
                    (None, _, _) => false,
                };
                if worse {
                    continue;
                }
                best.pop();
            }
            let mut row = Vec::with_capacity(width);
            for column in 0..width {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            // Charged for the same reason `Sort` is, and only for a row that
            // earns its place: this holds `limit` rows rather than all of
            // them, so a bounded query stays bounded in the budget too.
            inillucent_base::budget::materialise(owned_row_bytes(&row))?;
            let at = best.partition_point(|held| compare_by(held, &row, keys) != Ordering::Greater);
            best.insert(at, row);
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> DbResult<()> {
        let rows = std::mem::take(&mut self.best);
        emit_rows(&rows, self.downstream.as_mut())?;
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.best.clear();
        self.downstream.reset()
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
    /// How many of the row's leading columns decide whether it is a duplicate.
    ///
    /// Every column, unless the statement is carrying an extra one through -
    /// `SELECT DISTINCT a FROM t ORDER BY b` sorts by a `b` that is not in the
    /// result and must not be part of what makes a row distinct. SQLite answers
    /// that query; this used to refuse it, because comparing the carried column
    /// too would have de-duplicated on something the caller never selected.
    compared: usize,
    /// The encoded keys already emitted.
    ///
    /// **The workspace's own hasher rather than SipHash** (task-2000, design 4).
    /// The keys are ones this operator encoded itself out of a page, so nothing
    /// here has to resist a chosen key - see `inillucent_base::table_hash` for what
    /// that buys and what it gives up.
    seen: inillucent_base::table_hash::TableSet<Vec<u8>>,
    pub(crate) rows: Vec<Vec<OwnedDatum>>,
    downstream: Box<dyn Sink>,
}
impl Distinct {
    /// Returns a de-duplicating operator over every column of the row.
    ///
    /// @param collations - the collation of each compared column
    /// @param downstream - what to push the surviving rows into
    pub fn new(collations: Vec<Collation>, downstream: Box<dyn Sink>) -> Distinct {
        Distinct::over(collations, usize::MAX, downstream)
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
    ) -> Distinct {
        Distinct {
            collations,
            compared,
            seen: inillucent_base::table_hash::TableSet::default(),
            rows: Vec::new(),
            downstream,
        }
    }
}
impl Sink for Distinct {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        let mut encoded = Vec::with_capacity(32);
        let columns = batch.columns.len();
        // A carried column is kept in the row and left out of the key, so it
        // reaches the sort without deciding what is a duplicate.
        let compared = self.compared.min(columns);
        for nth in 0..batch.live() {
            // **The key is built and asked about before anything is owned**
            // (task-2000, design 4). This used to build the whole owned row, then
            // the key, then clone the key into the set - for every row, duplicate
            // or not. `scan.distinct` over a hundred thousand rows with sixty-four
            // distinct values therefore built a hundred thousand owned rows and a
            // hundred thousand key clones to keep sixty-four of each. The key is
            // what decides, so the key goes first and the rest happens only for a
            // row that survives.
            encoded.clear();
            for column in 0..compared {
                let value = batch.value(nth, column)?;
                key::encode_into_with(
                    &value,
                    self.collations
                        .get(column)
                        .copied()
                        .unwrap_or(Collation::Binary),
                    &mut encoded,
                );
            }
            if self.seen.contains(&encoded) {
                continue;
            }
            let mut row = Vec::with_capacity(columns);
            for column in 0..columns {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            // **Both halves are charged (task-1932, H6).** `DISTINCT` holds
            // the key set *and* every surviving row until `finish`, so its
            // memory is decided by how many rows are distinct - which is
            // the input size whenever they all are.
            inillucent_base::budget::materialise(
                owned_row_bytes(&row).saturating_add(encoded.len() as u64),
            )?;
            self.seen.insert(encoded.clone());
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
        self.seen.clear();
        self.rows.clear();
        self.downstream.reset()
    }
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
/// Whether the encoded key's byte order is this term list's order.
///
/// Three things have to hold. **Every term ascending**, because a descending
/// term reverses the comparison and one buffer cannot carry two directions.
/// **NULLs first**, which is what an ascending term means by default and what
/// `class::NULL` being the lowest class byte gives for free - an explicit
/// `NULLS LAST` on an ascending term does not, and is why `nulls_first` is
/// carried at all. And **a collation whose order survives the key encoder**:
/// `BINARY`, `NOCASE` and `RTRIM` each have a byte transformation whose natural
/// order is the collation's order, and `decimal`, `uint` and an application
/// comparator have none - `"9"` sorts after `"10"` by bytes and before it by
/// value.
///
/// Anything else falls through to `compare_by`, which is what every sort used
/// before this and is still the only path that can answer those.
///
/// @param keys - the sort terms
fn the_encoding_orders_these_terms(keys: &[SortKey]) -> bool {
    !keys.is_empty()
        && keys.iter().all(|term| {
            !term.descending && term.nulls_first && term.collation.is_order_preserving_in_keys()
        })
}

/// Encodes one row's sort terms into a single comparable buffer.
///
/// The terms are concatenated because the encoding is self-delimiting - a text
/// or blob payload is escaped and terminated - which is the same property that
/// lets an index key hold several columns in one buffer.
///
/// @param row - the row
/// @param keys - the sort terms, in order
fn encode_sort_key(row: &[OwnedDatum], keys: &[SortKey]) -> Vec<u8> {
    let mut out = Vec::new();
    for term in keys {
        let value = row
            .get(term.column)
            .map(OwnedDatum::borrow)
            .unwrap_or(Datum::Null);
        key::encode_into_with(&value, term.collation, &mut out);
    }
    out
}

/// Sorts rows in place, through the encoded keys where that is the same order.
///
/// **One encode per row instead of a dispatch per comparison** (task-2066
/// §4.3.6, step 3). `compare_by` reads two `OwnedDatum`s out of their rows,
/// borrows each, and dispatches on the pair of discriminants - and for text
/// under `NOCASE` it folds both operands again - once for every one of the
/// `n log n` comparisons a sort makes. Encoding first pays that `n` times and
/// leaves the sort comparing byte strings.
///
/// **Stable, like the sort it replaces.** The row's position is the tie-break,
/// so `sort_unstable` over `(bytes, position)` produces exactly the order
/// `sort_by` did. SQLite's sorter is stable and a digest comparison over rows
/// with equal keys would otherwise differ for a reason that is not a defect in
/// either engine.
///
/// @param rows - the rows, reordered in place
/// @param keys - the sort terms
pub(crate) fn sort_rows(rows: &mut Vec<Vec<OwnedDatum>>, keys: &[SortKey]) {
    if rows.len() < 2 {
        return;
    }
    if !the_encoding_orders_these_terms(keys) {
        rows.sort_by(|left, right| compare_by(left, right, keys));
        return;
    }

    let mut encoded: Vec<(Vec<u8>, usize)> = rows
        .iter()
        .enumerate()
        .map(|(at, row)| (encode_sort_key(row, keys), at))
        .collect();
    encoded.sort_unstable();

    // The rows are moved rather than cloned: a clone here would copy every
    // text and blob in the result set, which is the allocation this whole
    // operator is built to avoid.
    let mut held: Vec<Option<Vec<OwnedDatum>>> = rows.drain(..).map(Some).collect();
    for (_, at) in encoded {
        if let Some(row) = held.get_mut(at).and_then(Option::take) {
            rows.push(row);
        }
    }
}

/// @param keys - the ordering terms
pub(crate) fn compare_by(left: &[OwnedDatum], right: &[OwnedDatum], keys: &[SortKey]) -> Ordering {
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
mod spill_tests {
    use super::*;
    use crate::spill::{Spill, SpillFile};
    use inillucent_base::DbResult;

    /// A spill file held in memory, so the merge can be exercised without a VFS.
    #[derive(Default)]
    struct InMemoryFile {
        bytes: Vec<u8>,
    }

    impl SpillFile for InMemoryFile {
        fn append(&mut self, bytes: &[u8]) -> DbResult<u64> {
            let at = self.bytes.len() as u64;
            self.bytes.extend_from_slice(bytes);
            Ok(at)
        }

        fn read_at(&self, offset: u64, out: &mut [u8]) -> DbResult<()> {
            let from = usize::try_from(offset).unwrap_or(usize::MAX);
            match self.bytes.get(from..from.saturating_add(out.len())) {
                Some(slice) => {
                    out.copy_from_slice(slice);
                    Ok(())
                }
                None => Err(inillucent_base::DbError::primary(
                    inillucent_base::PrimaryCode::Internal,
                )
                .with_message("a spill read ran past the file")),
            }
        }
    }

    /// Hands out in-memory spill files.
    struct InMemory;

    impl Spill for InMemory {
        fn open(&self) -> DbResult<Box<dyn SpillFile>> {
            Ok(Box::new(InMemoryFile::default()))
        }
    }

    /// Pushes rows through a sort one batch at a time and returns what came out.
    ///
    /// @param rows - the input, in the order it arrives
    /// @param keys - what to order by
    /// @param threshold - how many bytes to hold before writing a run, or
    ///   `None` for a sort with nowhere to spill
    fn sorted(
        rows: &[Vec<OwnedDatum>],
        keys: &[SortKey],
        threshold: Option<u64>,
    ) -> (Vec<Vec<OwnedDatum>>, usize) {
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = Box::new(crate::ops::CollectInto::new(std::rc::Rc::clone(&collected)));
        let mut sort = Sort::new(keys.to_vec(), sink);
        if let Some(bytes) = threshold {
            sort = sort
                .spilling_to(Some(std::rc::Rc::new(InMemory)))
                .spilling_at(bytes);
        }
        for row in rows {
            let borrowed: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
            let columns: Vec<Vector<'_>> =
                borrowed.iter().map(|held| Vector::Const(*held)).collect();
            sort.push(&Batch::new(1, columns)).expect("a row is pushed");
        }
        sort.finish().expect("the sort finishes");
        let held = collected.borrow().clone();
        (held, sort.runs_written())
    }

    /// **The external sort answers what the in-memory sort answers.**
    ///
    /// That is the whole claim (task-2066 §4.3.6). Both are run over the same
    /// rows with the same keys and the only difference is a threshold small
    /// enough to make the first one spill - so a disagreement is the merge and
    /// nothing else.
    ///
    /// The real threshold is sixty-four mebibytes and no unit test can reach
    /// it, which is why `spilling_at` exists: a merge nobody can exercise is a
    /// merge nobody has run.
    #[test]
    fn a_spilled_sort_answers_what_an_in_memory_sort_answers() {
        let keys = vec![SortKey {
            column: 0,
            descending: false,
            collation: Collation::Binary,
            nulls_first: true,
        }];
        // Deliberately not already sorted, and with duplicate keys, so a merge
        // that took runs in the wrong order or lost a tie would show.
        let rows: Vec<Vec<OwnedDatum>> = (0..2_000i64)
            .map(|n| {
                vec![
                    OwnedDatum::Int((n * 7919) % 1_000),
                    OwnedDatum::Text(format!("row-{n:05}").into_bytes()),
                ]
            })
            .collect();

        let (in_memory, never) = sorted(&rows, &keys, None);
        // Small enough to write a good many runs out of two thousand rows.
        let (spilled, runs) = sorted(&rows, &keys, Some(4 * 1024));

        assert_eq!(never, 0, "a sort with nowhere to spill wrote a run");
        assert!(
            runs > 1,
            "the threshold was never reached, so this compared an in-memory sort against an              in-memory sort and proved nothing: {runs} run(s)"
        );
        assert_eq!(in_memory.len(), rows.len(), "the in-memory sort lost rows");
        assert_eq!(
            spilled, in_memory,
            "the spilled sort and the in-memory sort disagree"
        );
    }

    /// Descending and NULLs travel through the merge unchanged.
    ///
    /// The merge compares with `compare_by`, which is what carries them, so
    /// this asks whether the merge really uses it rather than a comparison of
    /// its own.
    #[test]
    fn a_spilled_sort_keeps_the_terms_descending_and_nulls_last() {
        let keys = vec![SortKey {
            column: 0,
            descending: true,
            collation: Collation::Binary,
            nulls_first: false,
        }];
        let rows: Vec<Vec<OwnedDatum>> = (0..500i64)
            .map(|n| {
                if n % 11 == 0 {
                    vec![OwnedDatum::Null]
                } else {
                    vec![OwnedDatum::Int((n * 37) % 97)]
                }
            })
            .collect();
        let (in_memory, _) = sorted(&rows, &keys, None);
        let (spilled, runs) = sorted(&rows, &keys, Some(1024));
        assert!(runs > 1, "the threshold was never reached: {runs} run(s)");
        assert_eq!(
            spilled, in_memory,
            "the merge lost a descending term or a NULL's place"
        );
    }
}

#[cfg(test)]
mod encoded_key_tests {
    use super::*;
    use inillucent_base::rng::Rng;

    /// One generated value, across every type a sort term can meet.
    ///
    /// The mixture is the point: SQL orders NULL before numbers before text
    /// before blobs, and a comparison that only ever saw one type would agree
    /// with anything. The text values repeat and differ only in case, so a
    /// `NOCASE` term has ties to break and a `BINARY` one does not.
    ///
    /// @param rng - the generator
    fn a_value(rng: &mut Rng) -> OwnedDatum {
        match rng.below(6) {
            0 => OwnedDatum::Null,
            1 => OwnedDatum::Int(rng.below(40) as i64 - 20),
            2 => OwnedDatum::Real((rng.below(4000) as f64 - 2000.0) / 8.0),
            3 => OwnedDatum::Text(
                ["alpha", "ALPHA", "Beta", "beta ", "", "gamma"]
                    .get(rng.below(6) as usize)
                    .copied()
                    .unwrap_or("alpha")
                    .as_bytes()
                    .to_vec(),
            ),
            4 => OwnedDatum::Blob(vec![rng.below(4) as u8, rng.below(4) as u8]),
            _ => OwnedDatum::Int(rng.below(4) as i64),
        }
    }

    /// Builds a table of generated rows.
    ///
    /// @param seed - the seed, which a failure prints
    /// @param rows - how many rows
    /// @param columns - how many columns each row has
    fn a_table(seed: u64, rows: usize, columns: usize) -> Vec<Vec<OwnedDatum>> {
        let mut rng = Rng::new(seed);
        (0..rows)
            .map(|_| (0..columns).map(|_| a_value(&mut rng)).collect())
            .collect()
    }

    /// **The encoded path answers what the dispatching path answers.**
    ///
    /// `sort_rows` takes the encoded route only when every term is ascending
    /// with NULLs first under an encodable collation. That condition is the
    /// whole safety argument, so it is checked by running both routes over the
    /// same generated tables and comparing the rows - not the keys, the rows,
    /// including the columns the sort does not read, because a permutation that
    /// lost a row or duplicated one would still have the keys in order.
    #[test]
    fn the_encoded_sort_answers_what_the_comparing_sort_answers() {
        let collations = [Collation::Binary, Collation::NoCase, Collation::RTrim];
        for seed in 1..=24u64 {
            let table = a_table(seed, 60, 3);
            let mut rng = Rng::new(seed.wrapping_mul(7919));
            let terms = 1 + rng.below(3) as usize;
            let keys: Vec<SortKey> = (0..terms)
                .map(|_| SortKey {
                    column: rng.below(3) as usize,
                    descending: false,
                    collation: collations
                        .get(rng.below(3) as usize)
                        .copied()
                        .unwrap_or(Collation::Binary),
                    nulls_first: true,
                })
                .collect();
            assert!(
                the_encoding_orders_these_terms(&keys),
                "seed {seed}: the generated terms did not take the encoded route, so this \
                 case compares one route with itself"
            );

            let mut encoded = table.clone();
            sort_rows(&mut encoded, &keys);
            let mut compared = table.clone();
            compared.sort_by(|left, right| compare_by(left, right, &keys));
            assert_eq!(
                encoded, compared,
                "seed {seed}: the encoded sort and the comparing sort disagree"
            );
        }
    }

    /// **A term the encoding cannot order falls through, and still sorts.**
    ///
    /// Rule 1.5: the condition above is only worth having if the other side of
    /// it works. A descending term, an explicit `NULLS LAST` on an ascending
    /// one, and a collation with no byte transformation each have to be refused
    /// by `the_encoding_orders_these_terms` and then sorted correctly by the
    /// path that refusal sends them down.
    #[test]
    fn a_term_the_encoding_cannot_order_falls_through_and_still_sorts() {
        let plain = SortKey {
            column: 0,
            descending: false,
            collation: Collation::Binary,
            nulls_first: true,
        };
        assert!(the_encoding_orders_these_terms(&[plain.clone()]));

        for refused in [
            SortKey {
                descending: true,
                ..plain.clone()
            },
            SortKey {
                nulls_first: false,
                ..plain.clone()
            },
            SortKey {
                collation: Collation::Decimal,
                ..plain.clone()
            },
        ] {
            assert!(
                !the_encoding_orders_these_terms(&[refused.clone()]),
                "a term the key encoder cannot order was sent down the encoded route: \
                 {refused:?}"
            );
            let table = a_table(11, 40, 2);
            let mut ours = table.clone();
            sort_rows(&mut ours, &[refused.clone()]);
            let mut theirs = table.clone();
            theirs.sort_by(|left, right| compare_by(left, right, &[refused.clone()]));
            assert_eq!(
                ours, theirs,
                "the fall-through path did not sort: {refused:?}"
            );
        }

        // And an empty term list, which is what a `SELECT` with no `ORDER BY`
        // that still reaches this operator carries.
        assert!(!the_encoding_orders_these_terms(&[]));
    }

    /// **The sort is stable, on both routes.**
    ///
    /// The rows are built so that every one has the same key and a different
    /// second column, so the only thing a sort can do is keep them in order or
    /// not. `sort_unstable` over `(bytes, position)` is what makes the encoded
    /// route stable, and dropping the position would pass every other case here.
    #[test]
    fn rows_with_equal_keys_keep_the_order_they_arrived_in() {
        let keys = vec![SortKey {
            column: 0,
            descending: false,
            collation: Collation::Binary,
            nulls_first: true,
        }];
        let table: Vec<Vec<OwnedDatum>> = (0..50i64)
            .map(|nth| vec![OwnedDatum::Int(7), OwnedDatum::Int(nth)])
            .collect();
        let mut sorted = table.clone();
        sort_rows(&mut sorted, &keys);
        assert_eq!(
            sorted, table,
            "rows with equal keys came out in a different order, so the sort is not stable"
        );
    }
}
