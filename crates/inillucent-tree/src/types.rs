//! The physical types a PAX column can hold, and the directory entry that
//! describes one.
//!
//! Invariant: the physical type is a statement about the *layout* of a column's
//! mini-column, not about what SQL says the column contains. SQLite is
//! dynamically typed, so a column declared `INTEGER` may hold a string; the
//! class array (see [`crate::leaf`]) records that per row as an *exception*, and
//! the physical type stays whatever the column's affinity made it. That
//! separation is what lets a scan of a leaf with no exceptions borrow a
//! contiguous run of 8-byte integers and a scan of a leaf with one exception
//! still return the right answer.

use inillucent_base::error::corrupt;
use inillucent_base::DbResult;
use inillucent_value::collation::Collation;

/// The layout of one column inside a leaf.
///
/// The discriminants are the on-disk encoding and are not to be renumbered:
/// `1` Int64, `2` Float64, `3` Text, `4` Blob, `5` Any.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PhysicalType {
    /// Eight-byte little-endian two's-complement integers.
    Int64,
    /// Eight-byte little-endian IEEE-754 binary64.
    Float64,
    /// UTF-8 bytes in the page heap, addressed by `(offset, length)`.
    Text,
    /// Uninterpreted bytes in the page heap, addressed by `(offset, length)`.
    Blob,
    /// Tagged values in the page heap, addressed by a `u32` offset.
    ///
    /// The escape hatch for a column with no useful affinity, where typing the
    /// mini-column would make every row an exception. A column of this type is
    /// never on the vectorised fast path.
    Any,
}

impl PhysicalType {
    /// Returns the byte that encodes this type in a column directory entry.
    pub fn code(self) -> u8 {
        match self {
            PhysicalType::Int64 => 1,
            PhysicalType::Float64 => 2,
            PhysicalType::Text => 3,
            PhysicalType::Blob => 4,
            PhysicalType::Any => 5,
        }
    }

    /// Decodes a directory entry's type byte.
    ///
    /// @param code - the byte read from the page
    pub fn from_code(code: u8) -> DbResult<PhysicalType> {
        match code {
            1 => Ok(PhysicalType::Int64),
            2 => Ok(PhysicalType::Float64),
            3 => Ok(PhysicalType::Text),
            4 => Ok(PhysicalType::Blob),
            5 => Ok(PhysicalType::Any),
            other => Err(corrupt(format!("physical type {other} is not a type"))),
        }
    }

    /// Returns the bytes one row occupies in the mini-column's value array.
    ///
    /// Variable-width columns hold an `(offset, length)` pair and `Any` holds a
    /// single offset, so every type has a fixed *slot* width even when the
    /// value it points at does not.
    pub fn slot_width(self) -> usize {
        match self {
            PhysicalType::Int64
            | PhysicalType::Float64
            | PhysicalType::Text
            | PhysicalType::Blob => 8,
            PhysicalType::Any => 4,
        }
    }

    /// Reports whether values of this type live in the value array itself.
    pub fn is_inline(self) -> bool {
        matches!(self, PhysicalType::Int64 | PhysicalType::Float64)
    }

    /// Returns every type, for exhaustive tests.
    pub fn all() -> [PhysicalType; 5] {
        [
            PhysicalType::Int64,
            PhysicalType::Float64,
            PhysicalType::Text,
            PhysicalType::Blob,
            PhysicalType::Any,
        ]
    }
}

/// Bit 0 of a directory entry's flag byte: the column admits NULLs.
pub const COLUMN_NULLABLE: u8 = 0b0000_0001;
/// Bit 1 of a directory entry's flag byte: the column is part of the leaf key.
pub const COLUMN_KEY: u8 = 0b0000_0010;

/// Bit 2 of a leaf's column directory entry: every value of this column, in
/// this leaf, is present and of the column's own type.
///
/// **This is a fact about the page, not about the schema**, which is what makes
/// it worth writing down. `COLUMN_NULLABLE` says what the catalog declared;
/// this says what the builder actually put on the page, and a reader that wants
/// to take a typed fast path needs the second question answered rather than the
/// first.
///
/// It is a cache of a walk over the column's class array. A leaf holding 1,782
/// index entries has a 446-byte class array, and `inillucent-probeprofile`
/// measured walking it at **22.2 ns** - paid once by every bound over an
/// integer column, which is once per index probe and twice per skip-scan seek.
///
/// A page whose bit is *clear* is read exactly as before: the walk still
/// happens and still gives the right answer, so a page written by anything that
/// does not set the bit is correct and only slower. A corrupted page whose bit
/// is wrongly *set* yields a wrong value rather than an unsafe read - every
/// accessor still bounds-checks its slice - which is the same class of damage
/// as a corrupted separator sending a descent to the wrong child, and is what
/// the checksum and the integrity checker are for.
pub const COLUMN_ALL_TYPED: u8 = 0b0000_0100;

/// One column of a leaf, as the column directory describes it.
///
/// The collation is part of the *column* rather than of a comparison, because
/// the tree is **stored** in its order. An index on a `COLLATE NOCASE` column
/// holds `Blue` between `blue` and `blue`, and a descent that compared those
/// three with `memcmp` would walk past the row it was looking for - which is
/// what `WHERE team = 'BLUE'` returning nothing looked like before this field
/// existed. The TDD asks for exactly this: "Collations other than BINARY are
/// encoded through the collation's key function... so that every interior
/// comparison is a `memcmp`."
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnSpec {
    /// The layout of the column's mini-column.
    pub physical: PhysicalType,
    /// [`COLUMN_NULLABLE`] and [`COLUMN_KEY`].
    pub flags: u8,
    /// The collation the column's text is ordered under.
    ///
    /// Not written to the page: the catalog says what a column's collation is,
    /// and a page that carried its own could disagree with it. It travels with
    /// the column directory the reader is handed.
    pub collation: Collation,
}

impl ColumnSpec {
    /// Returns a nullable column of the given type.
    ///
    /// @param physical - the mini-column layout
    pub fn new(physical: PhysicalType) -> ColumnSpec {
        ColumnSpec {
            physical,
            flags: COLUMN_NULLABLE,
            collation: Collation::Binary,
        }
    }

    /// Returns a key column of the given type.
    ///
    /// A key column is never NULL: the tree orders by it, and a NULL in a key
    /// would have no place in that order.
    ///
    /// @param physical - the mini-column layout
    pub fn key(physical: PhysicalType) -> ColumnSpec {
        ColumnSpec {
            physical,
            flags: COLUMN_KEY,
            collation: Collation::Binary,
        }
    }

    /// Returns the same column under a collation.
    ///
    /// @param collation - the order the column's text is stored in
    pub fn with_collation(mut self, collation: Collation) -> ColumnSpec {
        self.collation = collation;
        self
    }

    /// Reports whether the column admits NULLs.
    pub fn nullable(self) -> bool {
        self.flags & COLUMN_NULLABLE != 0
    }

    /// Reports whether the column is part of the leaf key.
    pub fn is_key(self) -> bool {
        self.flags & COLUMN_KEY != 0
    }
}

/// The class of one row's value in one column, two bits in the class array.
///
/// The discriminants are the on-disk encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueClass {
    /// SQL NULL. The value array's slot is not read.
    Null,
    /// A value of the column's physical type, in the value array.
    Typed,
    /// A value that is *not* of the column's physical type.
    ///
    /// The slot holds a `u32` heap offset of a tagged value. This is how a
    /// dynamically typed engine keeps a typed column layout: the exception is
    /// per row, and a leaf that has none says so in one header flag, so the
    /// vectorised scan pays for the possibility once per leaf rather than once
    /// per row.
    Exception,
    /// A value too large to keep in the leaf, held in a blob extent.
    ///
    /// The slot holds a `u32` heap offset of a sixteen-byte
    /// `(first page, total length)` reference. The bytes are on pages of their
    /// own, so the leaf's own accessor cannot return them - it has no pool -
    /// and a reader that sees this class asks the *tree* for the row instead.
    /// A leaf that holds one says so in a header flag, exactly as it does for
    /// an exception, so a scan pays for the possibility once per leaf.
    Extent,
}

impl ValueClass {
    /// Returns the two-bit code stored in the class array.
    pub fn code(self) -> u8 {
        match self {
            ValueClass::Null => 0,
            ValueClass::Typed => 1,
            ValueClass::Exception => 2,
            ValueClass::Extent => 3,
        }
    }

    /// Decodes a two-bit class code.
    ///
    /// All four codes are now spoken for. Code 3 was reserved when the class
    /// array was designed with two bits per value; the reservation was the
    /// design leaving room for exactly this.
    ///
    /// @param code - the two bits read from the class array
    pub fn from_code(code: u8) -> DbResult<ValueClass> {
        match code {
            0 => Ok(ValueClass::Null),
            1 => Ok(ValueClass::Typed),
            2 => Ok(ValueClass::Exception),
            3 => Ok(ValueClass::Extent),
            other => Err(corrupt(format!("value class {other} is not two bits"))),
        }
    }
}

/// Compares two values under a collation.
///
/// Text is compared by the collation's rule; everything else is compared the
/// way [`crate::datum::Datum::compare`] does, because a collation is a rule
/// about text and SQLite applies it to nothing else.
///
/// @param left - one value
/// @param right - the other
/// @param collation - the order to compare text under
pub fn compare_under(
    left: &crate::datum::Datum<'_>,
    right: &crate::datum::Datum<'_>,
    collation: Collation,
) -> std::cmp::Ordering {
    use crate::datum::Datum;
    if collation == Collation::Binary {
        return left.compare(right);
    }
    match (left, right) {
        (Datum::Text(a), Datum::Text(b)) => collation.compare_bytes(a, b),
        _ => left.compare(right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every type code round-trips, and nothing outside the set decodes.
    #[test]
    fn physical_type_codes_round_trip() {
        for physical in PhysicalType::all() {
            assert_eq!(PhysicalType::from_code(physical.code()).unwrap(), physical);
        }
        for code in [0u8, 6, 7, 128, 255] {
            assert!(PhysicalType::from_code(code).is_err(), "code {code}");
        }
    }

    /// Every class code round-trips, and anything wider than two bits is
    /// refused. All four codes are now spoken for: the reserved one became
    /// `Extent` when values began going out of line.
    #[test]
    fn value_class_codes_round_trip() {
        for class in [
            ValueClass::Null,
            ValueClass::Typed,
            ValueClass::Exception,
            ValueClass::Extent,
        ] {
            assert_eq!(ValueClass::from_code(class.code()).unwrap(), class);
        }
        assert!(ValueClass::from_code(4).is_err());
    }

    /// The slot widths are the ones the leaf codec lays out.
    #[test]
    fn slot_widths_match_the_layout() {
        assert_eq!(PhysicalType::Int64.slot_width(), 8);
        assert_eq!(PhysicalType::Float64.slot_width(), 8);
        assert_eq!(PhysicalType::Text.slot_width(), 8);
        assert_eq!(PhysicalType::Blob.slot_width(), 8);
        assert_eq!(PhysicalType::Any.slot_width(), 4);
        assert!(PhysicalType::Int64.is_inline());
        assert!(!PhysicalType::Text.is_inline());
    }

    /// A key column is not nullable and says so.
    #[test]
    fn column_flags_are_readable() {
        let key = ColumnSpec::key(PhysicalType::Int64);
        assert!(key.is_key());
        assert!(!key.nullable());
        let ordinary = ColumnSpec::new(PhysicalType::Text);
        assert!(!ordinary.is_key());
        assert!(ordinary.nullable());
    }
}
