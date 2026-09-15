//! Where a value goes in a leaf page, and how wide it is there.
//!
//! Invariant: **a slot's width is decided once, by the column's physical
//! type and the page size, and read back by the same rule.** A reader that
//! computed a width differently from the writer would read the next value's
//! bytes and answer a number rather than an error, which is why the frame
//! and the narrowing rules are in one file with nothing else in it.
use inillucent_base::DbResult;

use crate::datum::Datum;
use crate::types::{ColumnSpec, PhysicalType, ValueClass};

use super::*;

/// The bytes a class array of `rows` rows occupies: two bits each, padded to
/// eight bytes so the value array that follows is 8-byte aligned.
///
/// @param rows - how many rows the leaf holds
pub fn class_bytes(rows: usize) -> usize {
    rows.saturating_mul(2)
        .saturating_add(63)
        .checked_div(64)
        .unwrap_or(0)
        .saturating_mul(8)
}
/// The bytes a tombstone bitmap of `rows` rows occupies, padded to eight.
///
/// @param rows - how many rows the leaf holds
pub fn tombstone_bytes(rows: usize) -> usize {
    rows.saturating_add(63)
        .checked_div(64)
        .unwrap_or(0)
        .saturating_mul(8)
}
/// Rounds an offset up to the next multiple of eight.
///
/// @param at - the offset to align
pub(crate) fn align8(at: usize) -> usize {
    at.saturating_add(7) & !7
}
/// How one leaf's columns are laid out: a slot width and a base each.
///
/// Derived from the values that leaf actually holds, by
/// [`LeafBuilder::fit_widths`] as it prices the page and by
/// `LeafBuilder::layout_over` when nothing has priced it yet. The two follow
/// the same rule over the same rows, which is what makes the page the sizing
/// pass measured the page the encoder writes.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// How many bytes one slot of each column occupies.
    pub(crate) widths: Vec<usize>,
    /// What each column's integer slots are measured from.
    pub(crate) bases: Vec<i64>,
}
impl Layout {
    /// Returns one column's slot width, or the type's when it is out of range.
    ///
    /// @param index - the column's position
    /// @param column - the column's directory entry
    pub(crate) fn width(&self, index: usize, column: &ColumnSpec) -> usize {
        self.widths
            .get(index)
            .copied()
            .unwrap_or_else(|| column.physical.slot_width())
    }

    /// Returns one column's base.
    ///
    /// @param index - the column's position
    pub(crate) fn base(&self, index: usize) -> i64 {
        self.bases.get(index).copied().unwrap_or(0)
    }

    /// Reports whether any column carries a base, and so needs a wide entry.
    pub(crate) fn has_bases(&self) -> bool {
        self.bases.iter().any(|base| *base != 0)
    }
}
/// What the values seen so far force on one column's layout.
///
/// Kept as a **span** rather than a width because that is what a frame of
/// reference makes of it: with a base, the width comes from `high - low` and not
/// from how large the numbers are.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Shape {
    /// The smallest and largest typed integer, when one has been seen.
    span: Option<(i64, i64)>,
    /// The widest slot a single value forces whatever the span is: four for an
    /// integer column's exception, whose slot holds a heap offset, and the heap
    /// pair's width for a text or a blob.
    floor: usize,
}
impl Shape {
    /// Returns the shape of a column nothing has been seen for.
    pub(crate) fn new() -> Shape {
        Shape {
            span: None,
            floor: 0,
        }
    }

    /// Widens the shape by one value.
    ///
    /// @param physical - the column's layout
    /// @param value - the value being placed
    /// @param class - the class it took
    /// @param page_size - the database's page size
    pub(crate) fn observe(
        &mut self,
        physical: PhysicalType,
        value: &Datum<'_>,
        class: ValueClass,
        page_size: usize,
    ) {
        match physical {
            PhysicalType::Int64 => match class {
                ValueClass::Typed => {
                    let number = value.as_int().unwrap_or(0);
                    self.span = Some(match self.span {
                        Some((low, high)) => (low.min(number), high.max(number)),
                        None => (number, number),
                    });
                }
                ValueClass::Null => {}
                // The slot holds a `u32` heap offset rather than a value.
                _ => self.floor = self.floor.max(4),
            },
            PhysicalType::Text | PhysicalType::Blob => {
                let longest = match class {
                    ValueClass::Typed => value.as_bytes().map(<[u8]>::len).unwrap_or(0),
                    _ => 0,
                };
                self.floor = self.floor.max(heap_slot_width(page_size, longest));
            }
            other => self.floor = self.floor.max(other.slot_width()),
        }
    }

    /// Returns the width and base this shape resolves to.
    ///
    /// **A base of zero means plain signed truncation**, which is what every
    /// page written before frames existed holds - so a column whose smallest
    /// value is zero takes the signed width rather than the unsigned one, and
    /// the reader needs no flag beyond the base itself to know which it is
    /// looking at.
    ///
    /// @param physical - the column's layout
    pub(crate) fn resolve(&self, physical: PhysicalType) -> (usize, i64) {
        if !NARROW_INT_SLOTS {
            return (physical.slot_width(), 0);
        }
        if physical != PhysicalType::Int64 {
            return (self.floor.max(narrow_floor(physical, 1)), 0);
        }
        let (width, base) = match self.span {
            None => (1, 0),
            Some((low, high)) if FRAME_OF_REFERENCE && low != 0 => (frame_width(low, high), low),
            Some((low, high)) => (int_slot_width(low).max(int_slot_width(high)), 0),
        };
        (width.max(self.floor).max(1), base)
    }
}
/// Reads one integer out of a slot measured from a base.
///
/// With no base the slot is the value, sign-extended - which is what every page
/// written before [`LEAF_WIDE_DIRECTORY`] holds. With one it is the **unsigned
/// distance** from the base, so a column whose values run 900,000..900,200 is
/// one byte a row where the values themselves need four.
///
/// @param base - the column's frame of reference
/// @param width - how many bytes the slot occupies
/// @param slot - exactly that many bytes
#[inline]
pub fn from_frame(base: i64, width: usize, slot: &[u8]) -> i64 {
    if base == 0 {
        return read_int_slot(slot);
    }
    // **A load, a zero-extend and an add, chosen by width.** The first version
    // of this went through a `[u8; 8]` and `i128`, and `read.analytical` fell
    // from 6.57x to 2.77x: a scan that adds a constant to a byte should not be
    // slower than one that does not, and it is not once the width is a
    // constant to the compiler.
    base.wrapping_add(match width {
        1 => i64::from(slot.first().copied().unwrap_or(0)),
        2 => {
            let mut raw = [0u8; 2];
            raw.copy_from_slice(slot.get(..2).unwrap_or(&[0; 2]));
            i64::from(u16::from_le_bytes(raw))
        }
        4 => {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(slot.get(..4).unwrap_or(&[0; 4]));
            i64::from(u32::from_le_bytes(raw))
        }
        _ => {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(slot.get(..8).unwrap_or(&[0; 8]));
            u64::from_le_bytes(raw) as i64
        }
    })
}
/// Reports whether one integer fits a slot of this width, from this base.
///
/// @param base - the column's frame of reference
/// @param width - how many bytes the slot occupies
/// @param value - the integer to store
pub fn fits_frame(base: i64, width: usize, value: i64) -> bool {
    if base == 0 {
        return int_slot_width(value) <= width;
    }
    let delta = (value as i128).saturating_sub(base as i128);
    if delta < 0 {
        return false;
    }
    match width {
        1 => delta <= i128::from(u8::MAX),
        2 => delta <= i128::from(u16::MAX),
        4 => delta <= i128::from(u32::MAX),
        _ => true,
    }
}
/// Writes one integer into a slot measured from a base.
///
/// @param base - the column's frame of reference
/// @param slot - exactly the column's slot width
/// @param value - the integer to store
pub fn write_frame(base: i64, slot: &mut [u8], value: i64) {
    if base == 0 {
        write_int_slot(slot, value);
        return;
    }
    let delta = ((value as i128) - (base as i128)) as u64;
    let raw = delta.to_le_bytes();
    let width = slot.len().min(8);
    if let (Some(target), Some(source)) = (slot.get_mut(..width), raw.get(..width)) {
        target.copy_from_slice(source);
    }
}
/// Returns the narrowest slot the values `low..=high` fit in, from a base.
///
/// The range decides it, not the magnitude: `low` becomes the base and every
/// slot holds `value - low` as an unsigned integer.
///
/// @param low - the smallest typed value in the column
/// @param high - the largest
pub(crate) fn frame_width(low: i64, high: i64) -> usize {
    let span = (high as i128).saturating_sub(low as i128);
    if span < 0 {
        return 8;
    }
    let span = span as u128;
    if span <= u128::from(u8::MAX) {
        1
    } else if span <= u128::from(u16::MAX) {
        2
    } else if span <= u128::from(u32::MAX) {
        4
    } else {
        8
    }
}
/// Reports whether a column laid out at this width holds `(u16, u16)` pairs.
///
/// @param physical - the column's layout
/// @param width - the width it was laid out at
pub(crate) fn narrow_pair_at(physical: PhysicalType, width: usize) -> bool {
    matches!(physical, PhysicalType::Text | PhysicalType::Blob) && width == 4
}
/// Returns the narrowest slot a column of this type may start out at.
///
/// Eight for every type that cannot narrow, and one for an integer column - so
/// a leaf of small integers, or of none at all, spends a byte a row.
///
/// @param physical - the column's layout
pub(crate) fn narrow_floor(physical: PhysicalType, page_size: usize) -> usize {
    if !NARROW_INT_SLOTS {
        return physical.slot_width();
    }
    match physical {
        PhysicalType::Int64 => 1,
        // A pair is four bytes or eight; whether four is admissible is a
        // property of the page size, so the floor already knows the answer for
        // every leaf this builder writes.
        PhysicalType::Text | PhysicalType::Blob => heap_slot_width(page_size, 0),
        other => other.slot_width(),
    }
}
/// Returns the class a value takes in a column of the given physical type.
///
/// @param physical - the column's layout
/// @param value - the value to classify
/// Returns the class of one value in a column, spilling past a threshold.
///
/// A `Text` or `Blob` value longer than `threshold` is stored out of line. Only
/// those two: a `PhysicalType::Any` column's slot is a tagged value whose class
/// is in the bytes, and an extent reference carries only a page and a length -
/// so a reader would have nothing to say whether it had found text or a blob.
/// An oversized value in an `Any` column therefore stays inline, and if the row
/// then does not fit its page the builder says `RowTooLarge` as it always has.
///
/// @param physical - the column's layout
/// @param value - the value being placed
/// @param threshold - the longest value kept in the leaf
pub(crate) fn classify_at(
    physical: PhysicalType,
    value: &Datum<'_>,
    threshold: usize,
) -> ValueClass {
    if matches!(physical, PhysicalType::Text | PhysicalType::Blob) {
        let spillable = match (physical, value) {
            (PhysicalType::Text, Datum::Text(bytes)) => Some(bytes.len()),
            (PhysicalType::Blob, Datum::Blob(bytes)) => Some(bytes.len()),
            _ => None,
        };
        if spillable.is_some_and(|length| length > threshold) {
            return ValueClass::Extent;
        }
    }
    match (physical, value) {
        (_, Datum::Null) => ValueClass::Null,
        (PhysicalType::Any, _) => ValueClass::Typed,
        (PhysicalType::Int64, Datum::Int(_)) => ValueClass::Typed,
        (PhysicalType::Float64, Datum::Real(_)) => ValueClass::Typed,
        // An integer in a column whose affinity is REAL is *converted*, not
        // excepted. That is what REAL affinity means in the dialect - SQLite
        // stores 7 in a REAL column as 7.0 - and it is also what keeps such a
        // column on the vectorised path instead of turning every whole-numbered
        // row into a tagged value in the heap. The encoder's Float64 arm has
        // always converted; this is the classification agreeing with it, which
        // it did not before and which left that arm unreachable.
        (PhysicalType::Float64, Datum::Int(_)) => ValueClass::Typed,
        (PhysicalType::Text, Datum::Text(_)) => ValueClass::Typed,
        (PhysicalType::Blob, Datum::Blob(_)) => ValueClass::Typed,
        _ => ValueClass::Exception,
    }
}
/// Returns the heap bytes one value costs, its class already known.
///
/// @param physical - the column's layout
/// @param value - the value to measure
/// @param class - the class the value classified as
pub(crate) fn heap_cost_of(physical: PhysicalType, value: &Datum<'_>, class: ValueClass) -> usize {
    match class {
        ValueClass::Null => 0,
        ValueClass::Extent => EXTENT_REF_BYTES,
        ValueClass::Exception => value.tagged_len(),
        ValueClass::Typed => match physical {
            PhysicalType::Int64 | PhysicalType::Float64 => 0,
            PhysicalType::Text | PhysicalType::Blob => value.as_bytes().unwrap_or(&[]).len(),
            PhysicalType::Any => value.tagged_len(),
        },
    }
}
/// Writes one class into a class array.
///
/// @param page - the page being built
/// @param base - where the class array starts
/// @param row - the row's position
/// @param class - what to record
pub(crate) fn set_class(
    page: &mut [u8],
    base: usize,
    row: usize,
    class: ValueClass,
) -> DbResult<()> {
    let at = base.saturating_add(row / 4);
    let byte = page
        .get_mut(at)
        .ok_or_else(|| misuse("the class array does not fit"))?;
    let shift = (row % 4).saturating_mul(2);
    *byte = (*byte & !(3u8 << shift)) | (class.code() << shift);
    Ok(())
}
/// Appends a tagged value to the heap, returning the new heap start.
///
/// @param page - the page being built
/// @param heap_end - where the heap currently starts
/// @param value - the value to write
pub(crate) fn write_tagged(page: &mut [u8], heap_end: usize, value: &Datum<'_>) -> DbResult<usize> {
    let mut encoded = Vec::with_capacity(value.tagged_len());
    value.encode_tagged(&mut encoded);
    let start = heap_end
        .checked_sub(encoded.len())
        .ok_or_else(|| misuse("the heap overflowed the page"))?;
    let target = page
        .get_mut(start..heap_end)
        .ok_or_else(|| misuse("the heap overflowed the page"))?;
    target.copy_from_slice(&encoded);
    Ok(start)
}
