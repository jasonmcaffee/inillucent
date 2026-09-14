//! The batch: up to 2048 rows as column vectors, borrowing a pinned leaf.
//!
//! Invariant: a vector never copies. A `TableScan` over a clean leaf hands
//! downstream operators slices of the page itself, and the batch's lifetime is
//! the leaf's, so the borrow checker refuses any operator that tries to keep a
//! value past the point where the page may be rewritten. An operator that needs
//! to outlive its input - a sort, a hash table, an aggregate's group keys -
//! must say so by copying into [`inillucent_tree::datum::OwnedDatum`], which is a
//! visible cost rather than an accidental one.
//!
//! ## Why the vectors hold bytes rather than typed slices
//!
//! A `Vector::Int64` holds `&[u8]` and reads values with `chunks_exact(8)`
//! rather than holding a `&[i64]`. Turning page bytes into a typed slice needs
//! `unsafe`, and the Phase 0 benchmark measurement shows it
//! buys nothing: 0.75 ns/row against 0.74 for the two-column aggregate, under
//! the workspace release profile. The whole raw scan measures 1.8-4.6 ns/row
//! this way.

use inillucent_base::DbResult;
use inillucent_tree::datum::Datum;
use inillucent_tree::leaf::MiniColumn;
use inillucent_tree::types::{PhysicalType, ValueClass};

/// The most rows one batch carries.
///
/// 2048 is the TDD's number and DuckDB's: large enough that per-batch overhead
/// disappears against per-row work, small enough that a batch's working set
/// stays in L1/L2 through a whole pipeline.
pub const BATCH_ROWS: usize = 2048;

/// One column of one batch.
#[derive(Clone, Copy, Debug)]
pub enum Vector<'p> {
    /// Eight-byte little-endian integers, with an optional class array.
    ///
    /// `class` is `None` when every row is a present, typed value, which the
    /// scan proves once per leaf. That is the fast path: the consumer reads the
    /// bytes with no per-row branch at all.
    Int64 {
        /// `rows * width` bytes.
        bytes: &'p [u8],
        /// How many bytes one slot occupies: 1, 2, 4 or 8.
        ///
        /// A leaf spends the narrowest width that holds its own values, so a
        /// column of small integers is a contiguous run of bytes rather than of
        /// eight-byte words. The run is still contiguous and still unmixed with
        /// anything else, which is the property the vectorised paths need; what
        /// changes is the stride, and every dense loop takes it as a parameter.
        width: usize,
        /// What the slots are measured from; see `LEAF_WIDE_DIRECTORY`.
        ///
        /// Zero for a column with no frame of reference, which is the case the
        /// decode has a branch-free path for.
        base: i64,
        /// Two bits per row, or `None` when every row is typed.
        class: Option<&'p [u8]>,
    },
    /// Eight-byte little-endian IEEE-754 doubles.
    Float64 {
        /// `rows * 8` bytes.
        bytes: &'p [u8],
        /// Two bits per row, or `None` when every row is typed.
        class: Option<&'p [u8]>,
    },
    /// A fully typed variable-width column: eight-byte `(offset, length)` slots
    /// into the page's heap.
    ///
    /// The same trade the two fixed-width variants make, extended to the
    /// columns that actually carry the bytes. A `label` read went through
    /// `MiniColumn::value`, which consults the class array for every row even
    /// when the scan has already proved that no row of the column is NULL or an
    /// exception: `inillucent-probeprofile` measured `count(label)` over 100,000
    /// rows at 1,309 us against `count(*)` at 105 us, and `ORDER BY label LIMIT
    /// 100` at 1,044 us against `ORDER BY id LIMIT 100` at 468 us. The
    /// difference between those two is what a text read costs over an integer
    /// one, and most of it was a class byte nobody needed to look at.
    Variable {
        /// `rows * width` bytes, each an offset and a length into the page.
        slots: &'p [u8],
        /// How many bytes one slot occupies: four of `u16`s or eight of `u32`s.
        width: usize,
        /// The whole page, which the slots address absolutely.
        page: &'p [u8],
        /// Whether the bytes are text rather than a blob.
        text: bool,
    },
    /// A borrowed mini-column of any type, read through the leaf's accessors.
    ///
    /// The general path: `Any` columns, and any column of a leaf that has
    /// NULLs or exceptions in it.
    Column(MiniColumn<'p>),
    /// Materialised values, produced by an expression or a pipeline breaker.
    Values(&'p [Datum<'p>]),
    /// One value repeated for every row.
    Const(Datum<'p>),
}

impl<'p> Vector<'p> {
    /// Returns one row's value.
    ///
    /// The general accessor. Operators that care about speed match on the
    /// variant once per batch and take a typed loop; this is what the ones that
    /// do not care use, and what every correctness test compares against.
    ///
    /// @param row - the row's position within the batch
    pub fn at(&self, row: usize) -> DbResult<Datum<'p>> {
        match self {
            Vector::Int64 {
                bytes,
                width,
                base,
                class,
            } => match class_at(*class, row)? {
                ValueClass::Typed => Ok(Datum::Int(read_slot(bytes, *width, *base, row))),
                ValueClass::Null => Ok(Datum::Null),
                // An exception in a typed vector is impossible: the scan puts a
                // leaf with exceptions on the `Column` path instead. So is an
                // out-of-line value, and for the same reason - a leaf with one
                // is not read as vectors at all.
                ValueClass::Exception | ValueClass::Extent => Ok(Datum::Null),
            },
            Vector::Float64 { bytes, class } => match class_at(*class, row)? {
                ValueClass::Typed => Ok(Datum::Real(f64::from_bits(
                    read_slot(bytes, 8, 0, row) as u64
                ))),
                _ => Ok(Datum::Null),
            },
            Vector::Variable {
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
            Vector::Column(column) => column.value(row),
            Vector::Values(values) => Ok(values.get(row).copied().unwrap_or(Datum::Null)),
            Vector::Const(value) => Ok(*value),
        }
    }

    /// Returns the contiguous integer bytes, when this is a typed integer
    /// vector with no NULLs.
    ///
    /// The fast path's entry point: a consumer that gets `Some` may walk the
    /// bytes with `chunks_exact(8)` and pay nothing per row.
    pub fn dense_ints(&self) -> Option<DenseInts<'p>> {
        match self {
            Vector::Int64 {
                bytes,
                width,
                base,
                class: None,
            } => Some(DenseInts {
                bytes,
                width: *width,
                base: *base,
            }),
            _ => None,
        }
    }

    /// Builds a vector over one leaf mini-column.
    ///
    /// Chooses the typed fast path when the column's physical type is inline
    /// and every row is a present value of it, and the general path otherwise.
    /// The `all_typed` test is one pass over a thirty-second of the column, and
    /// it is what turns dynamic typing into a per-leaf cost.
    ///
    /// @param column - the mini-column to wrap
    pub fn from_column(column: MiniColumn<'p>) -> Vector<'p> {
        match column.physical {
            PhysicalType::Int64 if column.all_typed() => Vector::Int64 {
                bytes: column.inline_bytes(),
                width: column.width,
                base: column.base,
                class: None,
            },
            PhysicalType::Float64 if column.all_typed() => Vector::Float64 {
                bytes: column.inline_bytes(),
                class: None,
            },
            PhysicalType::Text | PhysicalType::Blob if column.all_typed() => Vector::Variable {
                slots: column.inline_bytes(),
                width: column.width,
                page: column.page_bytes(),
                text: column.physical == PhysicalType::Text,
            },
            _ => Vector::Column(column),
        }
    }
}

/// Reads one slot as an integer, sign-extending a narrow one.
///
/// @param bytes - the value array
/// @param width - how many bytes one slot occupies
/// @param row - the row's position
fn read_slot(bytes: &[u8], width: usize, base: i64, row: usize) -> i64 {
    let at = row.saturating_mul(width);
    match bytes.get(at..at.saturating_add(width)) {
        Some(slice) => inillucent_tree::leaf::from_frame(base, width, slice),
        None => 0,
    }
}

/// A contiguous run of typed integers, at whatever width the leaf spent.
///
/// **The fast path's currency.** Every operator that used to take
/// `&[u8]` and walk it with `chunks_exact(8)` takes one of these instead and
/// walks it with [`DenseInts::for_each`], which dispatches on the width once and
/// then runs a fixed-stride loop - so the loop the optimiser sees is as tight as
/// the eight-byte one was, over a quarter or an eighth of the bytes.
#[derive(Clone, Copy, Debug)]
pub struct DenseInts<'p> {
    /// `rows * width` bytes of values and nothing else.
    bytes: &'p [u8],
    /// How many bytes one value occupies: 1, 2, 4 or 8.
    width: usize,
    /// What those values are measured from.
    base: i64,
}

impl<'p> DenseInts<'p> {
    /// Returns a run over raw bytes at a stated width.
    ///
    /// @param bytes - the value array
    /// @param width - how many bytes one value occupies
    pub fn new(bytes: &'p [u8], width: usize) -> DenseInts<'p> {
        DenseInts {
            bytes,
            width: width.max(1),
            base: 0,
        }
    }

    /// How many values the run holds.
    pub fn len(&self) -> usize {
        self.bytes.len() / self.width
    }

    /// Reports whether the run holds no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns one value, or zero when the row is past the end.
    ///
    /// @param row - the value's position in the run
    #[inline]
    pub fn get(&self, row: usize) -> i64 {
        read_slot(self.bytes, self.width, self.base, row)
    }

    /// Returns the sub-run `from..to`, clamped to what the run holds.
    ///
    /// @param from - the first value to keep
    /// @param to - one past the last
    pub fn range(&self, from: usize, to: usize) -> DenseInts<'p> {
        let start = from.saturating_mul(self.width).min(self.bytes.len());
        let end = to
            .saturating_mul(self.width)
            .min(self.bytes.len())
            .max(start);
        DenseInts {
            bytes: self.bytes.get(start..end).unwrap_or(&[]),
            width: self.width,
            base: self.base,
        }
    }

    /// Calls `visit` with every value, in order.
    ///
    /// The width is matched **once**, so each arm compiles to a loop over a
    /// fixed stride - which is what the eight-byte `chunks_exact(8)` loops were
    /// and is what keeps them vectorisable.
    ///
    /// @param visit - what to do with each value
    #[inline]
    pub fn for_each(&self, mut visit: impl FnMut(i64)) {
        // **The base is added once per value, and only when there is one** -
        // and the width is matched here rather than inside the loop, for the
        // same reason the unframed arms below do it. A version that called a
        // width-taking helper per value cost `read.analytical` more than half
        // its ratio.
        if self.base != 0 {
            let base = self.base;
            match self.width {
                1 => {
                    for byte in self.bytes {
                        visit(base.wrapping_add(i64::from(*byte)));
                    }
                }
                2 => {
                    for chunk in self.bytes.chunks_exact(2) {
                        visit(base.wrapping_add(i64::from(u16::from_le_bytes(
                            chunk.try_into().unwrap_or([0; 2]),
                        ))));
                    }
                }
                4 => {
                    for chunk in self.bytes.chunks_exact(4) {
                        visit(base.wrapping_add(i64::from(u32::from_le_bytes(
                            chunk.try_into().unwrap_or([0; 4]),
                        ))));
                    }
                }
                _ => {
                    for chunk in self.bytes.chunks_exact(8) {
                        visit(base.wrapping_add(u64::from_le_bytes(
                            chunk.try_into().unwrap_or([0; 8]),
                        ) as i64));
                    }
                }
            }
            return;
        }
        match self.width {
            1 => {
                for byte in self.bytes {
                    visit(i64::from(*byte as i8));
                }
            }
            2 => {
                for chunk in self.bytes.chunks_exact(2) {
                    visit(i64::from(i16::from_le_bytes(
                        chunk.try_into().unwrap_or([0; 2]),
                    )));
                }
            }
            4 => {
                for chunk in self.bytes.chunks_exact(4) {
                    visit(i64::from(i32::from_le_bytes(
                        chunk.try_into().unwrap_or([0; 4]),
                    )));
                }
            }
            _ => {
                for chunk in self.bytes.chunks_exact(8) {
                    visit(i64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])));
                }
            }
        }
    }
}

/// Returns one row's class, treating a missing class array as all-typed.
///
/// @param class - the class array, or `None`
/// @param row - the row's position
fn class_at(class: Option<&[u8]>, row: usize) -> DbResult<ValueClass> {
    match class {
        None => Ok(ValueClass::Typed),
        Some(bits) => {
            let byte = bits.get(row / 4).copied().unwrap_or(0);
            ValueClass::from_code((byte >> ((row % 4) * 2)) & 3)
        }
    }
}

/// A batch's column list, owned or borrowed.
///
/// **The borrowed form exists because of the joins.** An index nested loop
/// builds a batch per probe - one row, the outer columns as constants and the
/// inner ones borrowed from the leaf - and a `Vec` for that list is a heap
/// allocation and a free per probed row. `inillucent-probeprofile` measured a
/// rowid lookup at 236 ns bare and about 320 ns inside the join, and most of
/// the difference was this. A join whose column count fits on the stack now
/// puts the list there and hands the batch a borrow of it.
///
/// Everything reads a batch's columns through `Deref`, so the two forms are the
/// same list to every operator.
#[derive(Clone, Debug)]
pub enum Columns<'p> {
    /// A list the batch owns, which is what a scan or a projection produces.
    Owned(Vec<Vector<'p>>),
    /// A list somebody else holds for at least as long as the batch.
    Borrowed(&'p [Vector<'p>]),
}

impl<'p> std::ops::Deref for Columns<'p> {
    type Target = [Vector<'p>];

    fn deref(&self) -> &[Vector<'p>] {
        match self {
            Columns::Owned(held) => held.as_slice(),
            Columns::Borrowed(held) => held,
        }
    }
}

impl<'p> From<Vec<Vector<'p>>> for Columns<'p> {
    /// @param held - the owned column list
    fn from(held: Vec<Vector<'p>>) -> Columns<'p> {
        Columns::Owned(held)
    }
}

/// A batch of rows as column vectors.
#[derive(Clone, Debug)]
pub struct Batch<'p> {
    /// How many rows the vectors hold.
    pub rows: usize,
    /// Which of those rows are live, or `None` when all of them are.
    ///
    /// A `Filter` produces a selection vector rather than copying the surviving
    /// rows, so a predicate that keeps most rows costs one pass and no movement.
    pub selection: Option<&'p [u32]>,
    /// One vector per column, in the batch's own column order.
    pub columns: Columns<'p>,
}

impl<'p> Batch<'p> {
    /// Returns a batch with every row live.
    ///
    /// @param rows - how many rows the vectors hold
    /// @param columns - one vector per column
    pub fn new(rows: usize, columns: Vec<Vector<'p>>) -> Batch<'p> {
        Batch {
            rows,
            selection: None,
            columns: Columns::Owned(columns),
        }
    }

    /// Returns a batch over a column list the caller holds.
    ///
    /// @param rows - how many rows the vectors hold
    /// @param columns - one vector per column, borrowed for the batch's life
    pub fn over(rows: usize, columns: &'p [Vector<'p>]) -> Batch<'p> {
        Batch {
            rows,
            selection: None,
            columns: Columns::Borrowed(columns),
        }
    }

    /// Returns how many rows survive the selection vector.
    pub fn live(&self) -> usize {
        match self.selection {
            Some(selection) => selection.len(),
            None => self.rows,
        }
    }

    /// Returns the row index of the nth live row.
    ///
    /// @param nth - the position among the live rows
    pub fn row_at(&self, nth: usize) -> usize {
        match self.selection {
            Some(selection) => selection.get(nth).copied().unwrap_or(0) as usize,
            None => nth,
        }
    }

    /// Returns one live row's value in one column.
    ///
    /// @param nth - the position among the live rows
    /// @param column - which column to read
    pub fn value(&self, nth: usize, column: usize) -> DbResult<Datum<'p>> {
        match self.columns.get(column) {
            Some(vector) => vector.at(self.row_at(nth)),
            None => Ok(Datum::Null),
        }
    }

    /// Reports whether every row is live.
    pub fn is_dense(&self) -> bool {
        self.selection.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_tree::leaf::{LeafBuilder, LeafRef};
    use inillucent_tree::types::ColumnSpec;

    fn leaf_page(values: Vec<Vec<Datum<'static>>>) -> Vec<u8> {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ];
        LeafBuilder::new(8192, 1, columns, 1)
            .unwrap()
            .encode(&values)
            .unwrap()
    }

    /// A column with no NULLs becomes a dense integer vector, and the dense
    /// bytes read back the same values the general accessor gives.
    #[test]
    fn a_clean_integer_column_is_dense() {
        let rows: Vec<Vec<Datum<'static>>> = (0..100)
            .map(|n| vec![Datum::Int(n), Datum::Int(n * 7), Datum::Text(b"label")])
            .collect();
        let page = leaf_page(rows);
        let leaf = LeafRef::parse(&page).unwrap();
        let vector = Vector::from_column(leaf.column(1).unwrap());
        let slots = vector.dense_ints().expect("should be dense");
        assert_eq!(slots.len(), 100);
        let mut row = 0usize;
        slots.for_each(|value| {
            assert_eq!(value, row as i64 * 7);
            assert_eq!(vector.at(row).unwrap().as_int().unwrap(), value);
            assert_eq!(slots.get(row), value);
            row += 1;
        });
        assert_eq!(row, 100);
    }

    /// One NULL takes the column off the dense path, and the general accessor
    /// still returns every value correctly.
    #[test]
    fn one_null_leaves_the_dense_path() {
        let rows: Vec<Vec<Datum<'static>>> = (0..50)
            .map(|n| {
                vec![
                    Datum::Int(n),
                    if n == 17 {
                        Datum::Null
                    } else {
                        Datum::Int(n * 7)
                    },
                    Datum::Text(b"label"),
                ]
            })
            .collect();
        let page = leaf_page(rows);
        let leaf = LeafRef::parse(&page).unwrap();
        let vector = Vector::from_column(leaf.column(1).unwrap());
        assert!(vector.dense_ints().is_none());
        for row in 0..50usize {
            let value = vector.at(row).unwrap();
            if row == 17 {
                assert!(value.is_null());
            } else {
                assert_eq!(value.as_int().unwrap(), row as i64 * 7);
            }
        }
    }

    /// A text column always takes the general path and reads back exactly.
    #[test]
    fn a_text_column_reads_the_same_bytes_by_either_path() {
        // Fully typed: the dense variable-width vector, which skips the class
        // array because the scan has already proved there is nothing in it.
        let rows = vec![
            vec![Datum::Int(1), Datum::Int(1), Datum::Text(b"alpha")],
            vec![Datum::Int(2), Datum::Int(2), Datum::Text(b"beta")],
        ];
        let page = leaf_page(rows);
        let leaf = LeafRef::parse(&page).unwrap();
        let vector = Vector::from_column(leaf.column(2).unwrap());
        assert!(matches!(vector, Vector::Variable { text: true, .. }));
        assert_eq!(vector.at(0).unwrap().as_bytes().unwrap(), b"alpha");
        assert_eq!(vector.at(1).unwrap().as_bytes().unwrap(), b"beta");

        // One NULL puts the same column back on the general path, and the rows
        // that are present read identically. The two paths agreeing is the
        // whole of what makes the fast one safe to take.
        let mixed = vec![
            vec![Datum::Int(1), Datum::Int(1), Datum::Text(b"alpha")],
            vec![Datum::Int(2), Datum::Int(2), Datum::Null],
            vec![Datum::Int(3), Datum::Int(3), Datum::Text(b"beta")],
        ];
        let page = leaf_page(mixed);
        let leaf = LeafRef::parse(&page).unwrap();
        let vector = Vector::from_column(leaf.column(2).unwrap());
        assert!(matches!(vector, Vector::Column(_)));
        assert_eq!(vector.at(0).unwrap().as_bytes().unwrap(), b"alpha");
        assert!(vector.at(1).unwrap().is_null());
        assert_eq!(vector.at(2).unwrap().as_bytes().unwrap(), b"beta");
    }

    /// A selection vector renumbers the live rows without moving any data.
    #[test]
    fn a_selection_vector_renumbers_without_copying() {
        let values = [
            Datum::Int(10),
            Datum::Int(20),
            Datum::Int(30),
            Datum::Int(40),
        ];
        let selection = [1u32, 3];
        let batch = Batch {
            rows: 4,
            selection: Some(&selection),
            columns: vec![Vector::Values(&values)].into(),
        };
        assert_eq!(batch.live(), 2);
        assert!(!batch.is_dense());
        assert_eq!(batch.value(0, 0).unwrap().as_int().unwrap(), 20);
        assert_eq!(batch.value(1, 0).unwrap().as_int().unwrap(), 40);
    }

    /// A constant vector answers for every row and a missing column answers
    /// NULL rather than panicking.
    #[test]
    fn constants_and_missing_columns_answer() {
        let batch = Batch::new(3, vec![Vector::Const(Datum::Int(7))]);
        assert_eq!(batch.live(), 3);
        for row in 0..3 {
            assert_eq!(batch.value(row, 0).unwrap().as_int().unwrap(), 7);
        }
        assert!(batch.value(0, 9).unwrap().is_null());
    }
}
