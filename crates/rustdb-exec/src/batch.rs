//! The batch: up to 2048 rows as column vectors, borrowing a pinned leaf.
//!
//! Invariant: a vector never copies. A `TableScan` over a clean leaf hands
//! downstream operators slices of the page itself, and the batch's lifetime is
//! the leaf's, so the borrow checker refuses any operator that tries to keep a
//! value past the point where the page may be rewritten. An operator that needs
//! to outlive its input - a sort, a hash table, an aggregate's group keys -
//! must say so by copying into [`rustdb_tree::datum::OwnedDatum`], which is a
//! visible cost rather than an accidental one.
//!
//! ## Why the vectors hold bytes rather than typed slices
//!
//! A `Vector::Int64` holds `&[u8]` and reads values with `chunks_exact(8)`
//! rather than holding a `&[i64]`. Turning page bytes into a typed slice needs
//! `unsafe`, and the measurement in `_agent_output/task-1816-phase0/` shows it
//! buys nothing: 0.75 ns/row against 0.74 for the two-column aggregate, under
//! the workspace release profile. The whole raw scan measures 1.8-4.6 ns/row
//! this way.

use rustdb_base::DbResult;
use rustdb_tree::datum::Datum;
use rustdb_tree::leaf::MiniColumn;
use rustdb_tree::types::{PhysicalType, ValueClass};

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
        /// `rows * 8` bytes.
        bytes: &'p [u8],
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
    /// A borrowed mini-column of any type, read through the leaf's accessors.
    ///
    /// The general path: variable-width columns, `Any` columns, and any column
    /// of a leaf that has exceptions.
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
            Vector::Int64 { bytes, class } => match class_at(*class, row)? {
                ValueClass::Typed => Ok(Datum::Int(read_i64(bytes, row))),
                ValueClass::Null => Ok(Datum::Null),
                // An exception in a typed vector is impossible: the scan puts a
                // leaf with exceptions on the `Column` path instead.
                ValueClass::Exception => Ok(Datum::Null),
            },
            Vector::Float64 { bytes, class } => match class_at(*class, row)? {
                ValueClass::Typed => Ok(Datum::Real(f64::from_bits(read_i64(bytes, row) as u64))),
                _ => Ok(Datum::Null),
            },
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
    pub fn dense_int_bytes(&self) -> Option<&'p [u8]> {
        match self {
            Vector::Int64 { bytes, class: None } => Some(bytes),
            _ => None,
        }
    }

    /// Returns the contiguous double bytes, when this is a typed real vector
    /// with no NULLs.
    pub fn dense_real_bytes(&self) -> Option<&'p [u8]> {
        match self {
            Vector::Float64 { bytes, class: None } => Some(bytes),
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
                class: None,
            },
            PhysicalType::Float64 if column.all_typed() => Vector::Float64 {
                bytes: column.inline_bytes(),
                class: None,
            },
            _ => Vector::Column(column),
        }
    }
}

/// Reads one 8-byte slot as an integer.
///
/// @param bytes - the value array
/// @param row - the row's position
fn read_i64(bytes: &[u8], row: usize) -> i64 {
    let at = row.saturating_mul(8);
    match bytes.get(at..at.saturating_add(8)) {
        Some(slice) => i64::from_le_bytes(slice.try_into().unwrap_or([0; 8])),
        None => 0,
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
    pub columns: Vec<Vector<'p>>,
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
            columns,
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
    use rustdb_tree::leaf::{LeafBuilder, LeafRef};
    use rustdb_tree::types::ColumnSpec;

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
        let bytes = vector.dense_int_bytes().expect("should be dense");
        for (row, chunk) in bytes.chunks_exact(8).enumerate() {
            let dense = i64::from_le_bytes(chunk.try_into().unwrap());
            assert_eq!(dense, row as i64 * 7);
            assert_eq!(vector.at(row).unwrap().as_int().unwrap(), dense);
        }
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
        assert!(vector.dense_int_bytes().is_none());
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
    fn a_text_column_reads_through_the_general_path() {
        let rows = vec![
            vec![Datum::Int(1), Datum::Int(1), Datum::Text(b"alpha")],
            vec![Datum::Int(2), Datum::Int(2), Datum::Text(b"beta")],
        ];
        let page = leaf_page(rows);
        let leaf = LeafRef::parse(&page).unwrap();
        let vector = Vector::from_column(leaf.column(2).unwrap());
        assert!(matches!(vector, Vector::Column(_)));
        assert_eq!(vector.at(0).unwrap().as_bytes().unwrap(), b"alpha");
        assert_eq!(vector.at(1).unwrap().as_bytes().unwrap(), b"beta");
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
            columns: vec![Vector::Values(&values)],
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
