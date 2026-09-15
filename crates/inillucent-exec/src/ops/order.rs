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
/// Sorts every row, then emits.
pub struct Sort {
    keys: Vec<SortKey>,
    pub(crate) rows: Vec<Vec<OwnedDatum>>,
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

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.rows.clear();
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
    seen: std::collections::HashSet<Vec<u8>>,
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
                // A carried column is kept in the row and left out of the key,
                // so it reaches the sort without deciding what is a duplicate.
                if column < self.compared {
                    key::encode_into_with(
                        &value,
                        self.collations
                            .get(column)
                            .copied()
                            .unwrap_or(Collation::Binary),
                        &mut encoded,
                    );
                }
                row.push(OwnedDatum::from_datum(&value));
            }
            if self.seen.insert(encoded.clone()) {
                // **Both halves are charged (task-1932, H6).** `DISTINCT` holds
                // the key set *and* every surviving row until `finish`, so its
                // memory is decided by how many rows are distinct - which is
                // the input size whenever they all are.
                inillucent_base::budget::materialise(
                    owned_row_bytes(&row).saturating_add(encoded.len() as u64),
                )?;
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
