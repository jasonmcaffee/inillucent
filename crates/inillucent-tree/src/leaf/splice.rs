//! Compacting a leaf by splicing its delta rows into the packed page it has.
//!
//! Invariant: **a spliced page reads exactly as a repacked one would, and it is
//! a function of the page it was made from and nothing else.** The first half
//! is what makes it a compaction. The second is what lets recovery replay one:
//! a `CompactLeaf` record without an image is replayed by running the
//! compaction again over the page as it stands at that point in the log, so
//! whether a splice or a repack runs, and what either produces, may depend on
//! the page alone - never on the row the write was making room for.
//!
//! ## What a splice keeps, and what it costs
//!
//! A repack reads every live row, prices the page again from the values - the
//! slot width and frame of reference of every column - and writes every value
//! again. A splice keeps the page's widths and bases and its heap exactly as
//! they are. A packed row's slot bytes and class bits are copied, which is
//! correct because a text slot names a heap offset and the heap has not moved;
//! only the delta rows' values are written, into the same widths, with their
//! text appended below the heap already there. task-2066's audit priced the
//! repack's sizing, materialising and encoding at 5.28 ms of the 10.57 ms
//! `write.insert.batch` spent making room (C2).
//!
//! The cost is on the page. **A width never narrows again**, because nothing
//! re-prices it, and **a heap value a removed row left behind is never
//! reclaimed**. Both are bounded: the page counts the splices it has taken in
//! bits 5 to 7 of its flag byte ([`LEAF_SPLICES_SHIFT`]), and the compaction
//! after [`SPLICE_LIMIT`] of them is a full repack that sets the count back to
//! zero. A splice is also refused whenever a delta value does not fit the
//! width it would be written into, so it never widens anything either.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;

use crate::datum::Datum;
use crate::types::{write_slot_offset, PhysicalType, ValueClass, COLUMN_ALL_TYPED};

use super::encode::write_typed_value;
use super::layout::*;
use super::read::{live_value, LiveOrder, LiveRow, MiniColumn};
use super::*;

/// What one column of a spliced page is written from.
struct SplicedColumn<'o, 'p> {
    /// The old page's view of the column: its type, width, base and slots.
    held: &'o MiniColumn<'p>,
    /// The directory entry's flag byte on the old page.
    flags: u8,
}

/// Returns a leaf compacted by splicing its delta rows into its packed region.
///
/// `None` when a splice is not allowed, and then the caller repacks. It is not
/// allowed on a leaf with out-of-line values, which is repacked with the file
/// in hand; on a leaf that has already been spliced [`SPLICE_LIMIT`] times in a
/// row; when any delta value would not fit the slot width and base its column
/// already has; and when the page it would produce is fuller than `fill`.
///
/// @param leaf - the leaf being compacted, parsed with its tree's collations
/// @param order - its live rows in key order, from [`LeafRef::live_order`]
/// @param page_size - the database's page size
/// @param fill - the fraction of the page the result may occupy
pub fn splice_image(
    leaf: &LeafRef<'_>,
    order: &LiveOrder<'_>,
    page_size: usize,
    fill: f64,
) -> DbResult<Option<Vec<u8>>> {
    if leaf.has_extents() || splices_of(leaf) >= SPLICE_LIMIT {
        return Ok(None);
    }
    let rows = order.len();
    if rows == 0 || rows > u16::MAX as usize || leaf.bytes().len() != page_size {
        return Ok(None);
    }
    let Some(added_heap) = delta_heap_if_it_fits(order, page_size)? else {
        return Ok(None);
    };
    let fixed = fixed_bytes(leaf, order.columns(), rows);
    let kept_heap = page_size.saturating_sub(leaf.heap_start());
    let size = fixed.saturating_add(kept_heap).saturating_add(added_heap);
    let budget = ((page_size as f64) * fill.clamp(0.05, 1.0)) as usize;
    if size > budget {
        return Ok(None);
    }
    write_spliced(leaf, order, page_size, fixed).map(Some)
}

/// Returns how many compactions in a row have spliced a leaf.
///
/// @param leaf - the leaf
pub fn splices_of(leaf: &LeafRef<'_>) -> u8 {
    (page::flags_of(leaf.bytes()).unwrap_or(0) & LEAF_SPLICES_MASK) >> LEAF_SPLICES_SHIFT
}

/// Returns the heap bytes the delta rows add, or `None` when one of them does
/// not fit its column's existing slot.
///
/// A value fits when writing it into the slot the column already has loses
/// nothing: an integer inside the width measured from the base, a text or a
/// blob whose length the column's pair can hold, and an exception only where
/// the slot is wide enough to hold a heap offset. A value that would be stored
/// out of line never fits, because a leaf with extents is repacked with the
/// file in hand.
///
/// @param order - the live rows, which name the delta rows among them
/// @param page_size - the database's page size
fn delta_heap_if_it_fits(order: &LiveOrder<'_>, page_size: usize) -> DbResult<Option<usize>> {
    let mut heap = 0usize;
    for at in order.order() {
        let LiveRow::Delta(index) = at else {
            continue;
        };
        let Some(values) = order.delta().get(*index as usize) else {
            return Err(misuse("a live row names a delta row that was not decoded"));
        };
        for (column, value) in order.columns().iter().zip(values.iter()) {
            let class = classify_at(column.physical, value, usize::MAX);
            if !fits_slot(column, value, class, page_size) {
                return Ok(None);
            }
            heap = heap.saturating_add(heap_cost_of(column.physical, value, class));
        }
    }
    Ok(Some(heap))
}

/// Reports whether one value can be written into a column's existing slot.
///
/// @param column - the column as the page lays it out
/// @param value - the value
/// @param class - the class it takes in that column
/// @param page_size - the database's page size
fn fits_slot(
    column: &MiniColumn<'_>,
    value: &Datum<'_>,
    class: ValueClass,
    page_size: usize,
) -> bool {
    match (class, column.physical) {
        (ValueClass::Null, _) => true,
        (ValueClass::Extent, _) => false,
        (ValueClass::Typed, PhysicalType::Int64) => value
            .as_int()
            .is_some_and(|number| fits_frame(column.base, column.width, number)),
        (ValueClass::Typed, PhysicalType::Float64) => column.width == 8,
        (ValueClass::Typed, PhysicalType::Text | PhysicalType::Blob) => {
            let length = value.as_bytes().map(<[u8]>::len).unwrap_or(0);
            crate::types::heap_slot_width(page_size, length) <= column.width
        }
        (ValueClass::Typed, PhysicalType::Any) => column.width >= 4,
        // An exception's slot holds a heap offset: four bytes, or the offset
        // half of a narrow text pair.
        (ValueClass::Exception, _) => column.width >= 4,
    }
}

/// Returns the bytes a page of `rows` rows spends before its heap, at the old
/// page's widths.
///
/// The same arithmetic as `LeafBuilder::fixed_size_with`, over the widths the
/// page already has: the header, the column directory at the page's own entry
/// size, then each mini-column's class array and slots, 8-byte aligned.
///
/// @param leaf - the old page
/// @param columns - its columns
/// @param rows - how many rows the spliced page holds
fn fixed_bytes(leaf: &LeafRef<'_>, columns: &[MiniColumn<'_>], rows: usize) -> usize {
    let mut fixed = align8(
        leaf_header::DIRECTORY.saturating_add(
            leaf.column_count()
                .saturating_mul(leaf.directory_entry_size()),
        ),
    );
    for column in columns {
        fixed = align8(
            fixed
                .saturating_add(class_bytes(rows))
                .saturating_add(rows.saturating_mul(column.width)),
        );
    }
    fixed
}

/// Writes the spliced page.
///
/// The header and the column directory are the old page's, with the offsets,
/// the counts and the flags recomputed; the heap is the old page's, byte for
/// byte, at the same offsets; and each mini-column is rebuilt row by row in key
/// order, copying a packed row's class and slot and writing a delta row's value.
///
/// @param leaf - the old page
/// @param order - its live rows in key order
/// @param page_size - the database's page size
/// @param fixed - where the mini-columns end, from `fixed_bytes`
fn write_spliced(
    leaf: &LeafRef<'_>,
    order: &LiveOrder<'_>,
    page_size: usize,
    fixed: usize,
) -> DbResult<Vec<u8>> {
    let old = leaf.bytes();
    let rows = order.len();
    let mut page = vec![0u8; page_size];
    page::write_common(&mut page, PageKind::Leaf, 0, page::tree_of(old)?)?;
    let heap_start = leaf.heap_start();
    let heap = old
        .get(heap_start..page_size)
        .ok_or_else(|| misuse("the heap runs past the page"))?;
    page.get_mut(heap_start..page_size)
        .ok_or_else(|| misuse("the heap runs past the page"))?
        .copy_from_slice(heap);
    let stride = leaf.directory_entry_size();
    let mut columns = Vec::with_capacity(leaf.column_count());
    for (index, held) in order.columns().iter().enumerate() {
        let entry = leaf_header::DIRECTORY.saturating_add(index.saturating_mul(stride));
        let flags = old.get(entry.saturating_add(1)).copied().unwrap_or(0);
        columns.push(SplicedColumn { held, flags });
    }
    let mut cursor =
        align8(leaf_header::DIRECTORY.saturating_add(leaf.column_count().saturating_mul(stride)));
    let mut heap_end = heap_start;
    let mut has_exceptions = false;
    for (index, column) in columns.iter().enumerate() {
        let entry = leaf_header::DIRECTORY.saturating_add(index.saturating_mul(stride));
        let (next_heap, all_typed, exceptions) =
            write_column(&mut page, column, order, index, cursor, heap_end)?;
        heap_end = next_heap;
        has_exceptions = has_exceptions || exceptions;
        write_directory_entry(
            &mut page,
            old,
            entry,
            stride,
            cursor,
            column.flags,
            all_typed,
        )?;
        cursor = align8(
            cursor
                .saturating_add(class_bytes(rows))
                .saturating_add(rows.saturating_mul(column.held.width)),
        );
    }
    if cursor != fixed || heap_end < cursor {
        return Err(misuse("a spliced page's regions overlap"));
    }
    write_spliced_header(&mut page, leaf, order, heap_end, has_exceptions)?;
    Ok(page)
}

/// Rebuilds one mini-column of the spliced page.
///
/// @param page - the page being written
/// @param column - the column, as the old page lays it out
/// @param order - the live rows in key order
/// @param index - the column's position
/// @param base - where its class array starts on the new page
/// @param heap_end - where the heap currently starts
/// @returns where the heap starts afterwards, whether every value was typed,
///   and whether any was an exception
fn write_column(
    page: &mut [u8],
    column: &SplicedColumn<'_, '_>,
    order: &LiveOrder<'_>,
    index: usize,
    base: usize,
    mut heap_end: usize,
) -> DbResult<(usize, bool, bool)> {
    let held = column.held;
    let width = held.width;
    let values_at = base.saturating_add(class_bytes(order.len()));
    let mut all_typed = true;
    let mut exceptions = false;
    let positions = order.order();
    let mut row = 0usize;
    while let Some(at) = positions.get(row).copied() {
        let slot = values_at.saturating_add(row.saturating_mul(width));
        let LiveRow::Sorted(first) = at else {
            let value = live_value(order.columns(), order.delta(), at, index)?;
            let class = classify_at(held.physical, &value, usize::MAX);
            heap_end = write_delta_value(page, held, &value, class, slot, heap_end)?;
            set_class(page, base, row, class)?;
            all_typed = all_typed && class == ValueClass::Typed;
            exceptions = exceptions || class == ValueClass::Exception;
            row = row.saturating_add(1);
            continue;
        };
        // **A run of packed rows is one copy.** Between two delta rows the
        // packed rows that survive are consecutive on the old page as well, so
        // their slots are one contiguous slice there and here - the per-column
        // memmove task-2066's audit priced the splice at (C2). Only the class
        // bits are moved a row at a time, because two bits a row do not line up
        // with a byte once the rows ahead of them have moved.
        let first = first as usize;
        let mut length = 1usize;
        while let Some(LiveRow::Sorted(next)) = positions.get(row.saturating_add(length)) {
            if *next as usize != first.saturating_add(length) {
                break;
            }
            length = length.saturating_add(1);
        }
        let source = held
            .values
            .get(first.saturating_mul(width)..first.saturating_add(length).saturating_mul(width))
            .ok_or_else(|| misuse("a packed run runs past its column"))?;
        page.get_mut(slot..slot.saturating_add(source.len()))
            .ok_or_else(|| misuse("a spliced run runs past the page"))?
            .copy_from_slice(source);
        for offset in 0..length {
            let class = held.class_at(first.saturating_add(offset))?;
            set_class(page, base, row.saturating_add(offset), class)?;
            all_typed = all_typed && class == ValueClass::Typed;
            exceptions = exceptions || class == ValueClass::Exception;
        }
        row = row.saturating_add(length);
    }
    Ok((heap_end, all_typed, exceptions))
}

/// Writes one delta value into its slot, and its heap bytes below the heap.
///
/// The encoder's per-value write, class by class, at the width the column
/// already has - `delta_heap_if_it_fits` has proved every value fits.
///
/// @param page - the page being written
/// @param column - the column, as the old page lays it out
/// @param value - the value
/// @param class - the class it takes
/// @param slot - where its slot is
/// @param heap_end - where the heap currently starts
fn write_delta_value(
    page: &mut [u8],
    column: &MiniColumn<'_>,
    value: &Datum<'_>,
    class: ValueClass,
    slot: usize,
    heap_end: usize,
) -> DbResult<usize> {
    match class {
        ValueClass::Null => Ok(heap_end),
        ValueClass::Typed => write_typed_value(
            page,
            slot,
            column.width,
            column.base,
            column.physical,
            value,
            heap_end,
        ),
        ValueClass::Exception => {
            let start = write_tagged(page, heap_end, value)?;
            let narrow = narrow_pair_at(column.physical, column.width);
            let target = page
                .get_mut(slot..slot.saturating_add(column.width))
                .ok_or_else(|| misuse("a slot runs past the page"))?;
            write_slot_offset(target, start, narrow);
            Ok(start)
        }
        ValueClass::Extent => Err(unreachable_branch("a spliced value out of line")),
    }
}

/// Writes one column directory entry of the spliced page.
///
/// The type, the width and the base are copied from the old entry; the offset
/// is the new one; and the flag byte is the old one with its all-typed bit set
/// from what this splice wrote, because a row that made the column untyped may
/// have been the one removed.
///
/// @param page - the page being written
/// @param old - the old page
/// @param entry - where the entry sits
/// @param stride - how many bytes one entry occupies
/// @param offset - where the column's class array now starts
/// @param flags - the old entry's flag byte
/// @param all_typed - whether every value this column now holds is typed
fn write_directory_entry(
    page: &mut [u8],
    old: &[u8],
    entry: usize,
    stride: usize,
    offset: usize,
    flags: u8,
    all_typed: bool,
) -> DbResult<()> {
    let source = old
        .get(entry..entry.saturating_add(stride))
        .ok_or_else(|| misuse("the directory runs past the page"))?;
    page.get_mut(entry..entry.saturating_add(stride))
        .ok_or_else(|| misuse("the directory runs past the page"))?
        .copy_from_slice(source);
    let flag_slot = page
        .get_mut(entry.saturating_add(1))
        .ok_or_else(|| misuse("the directory runs past the page"))?;
    *flag_slot = match all_typed {
        true => flags | COLUMN_ALL_TYPED,
        false => flags & !COLUMN_ALL_TYPED,
    };
    page::write_u32(page, entry.saturating_add(4), offset as u32)
}

/// Writes the leaf header of the spliced page.
///
/// @param page - the page being written
/// @param leaf - the old page
/// @param order - the live rows in key order
/// @param heap_end - where the heap starts
/// @param has_exceptions - whether any column holds an exception
fn write_spliced_header(
    page: &mut [u8],
    leaf: &LeafRef<'_>,
    order: &LiveOrder<'_>,
    heap_end: usize,
    has_exceptions: bool,
) -> DbResult<()> {
    page::write_u16(page, leaf_header::ROW_COUNT, order.len() as u16)?;
    page::write_u16(page, leaf_header::DELTA_COUNT, 0)?;
    page::write_u16(page, leaf_header::COLUMN_COUNT, leaf.column_count() as u16)?;
    page::write_u16(page, leaf_header::KEY_COLUMNS, leaf.key_columns() as u16)?;
    page::write_u32(page, leaf_header::HEAP_START, heap_end as u32)?;
    page::write_u32(page, leaf_header::DELTA_START, heap_end as u32)?;
    page::write_u64(page, leaf_header::MAX_CTS, 0)?;
    let low_fence = match order.order().first() {
        Some(first) => live_value(order.columns(), order.delta(), *first, 0)?
            .as_int()
            .unwrap_or(0),
        None => 0,
    };
    page::write_u64(page, leaf_header::LOW_FENCE, low_fence as u64)?;
    let mut flags = ((splices_of(leaf).saturating_add(1) << LEAF_SPLICES_SHIFT)
        & LEAF_SPLICES_MASK)
        | LEAF_DELTA_DIRECTORY;
    if leaf.directory_entry_size() == DIRECTORY_ENTRY_WIDE {
        flags |= LEAF_WIDE_DIRECTORY;
    }
    if has_exceptions {
        flags |= LEAF_HAS_EXCEPTIONS;
    }
    let slot = page
        .get_mut(header::FLAGS)
        .ok_or_else(|| misuse("the page has no flag byte"))?;
    *slot = flags;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutate::{Applied, LeafMut};
    use crate::types::ColumnSpec;

    /// A key, a framed integer, a text, a real and a column of no type.
    fn columns() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
            ColumnSpec::new(PhysicalType::Float64),
            ColumnSpec::new(PhysicalType::Any),
        ]
    }

    /// Builds a packed leaf of `count` rows at even keys, with a NULL and an
    /// exception among them so the class arrays are not all typed.
    ///
    /// @param count - how many rows
    fn packed(count: i64) -> Vec<u8> {
        let labels: Vec<String> = (0..count).map(|key| format!("label {key}")).collect();
        let rows: Vec<Vec<Datum<'_>>> = (0..count)
            .map(|key| {
                let value = match key {
                    3 => Datum::Null,
                    5 => Datum::Text(b"an exception"),
                    _ => Datum::Int(1_000 + key),
                };
                vec![
                    Datum::Int(key * 2),
                    value,
                    Datum::Text(labels[key as usize].as_bytes()),
                    Datum::Real(key as f64 / 4.0),
                    Datum::Int(key),
                ]
            })
            .collect();
        LeafBuilder::new(8_192, 1, columns(), 1)
            .expect("a builder")
            .encode(&rows)
            .expect("a leaf")
    }

    /// Inserts rows into a leaf's delta area.
    ///
    /// @param page - the leaf
    /// @param rows - the rows, one per insert
    fn insert(page: &mut [u8], rows: &[Vec<Datum<'_>>]) {
        let mut leaf = LeafMut::new(page).expect("a leaf");
        for row in rows {
            assert_eq!(
                leaf.insert_delta(&columns(), row).expect("an insert"),
                Applied::Yes
            );
        }
    }

    /// Returns every live row of a leaf, as text, for comparing two leaves.
    ///
    /// @param page - the leaf
    fn live_text(page: &[u8]) -> String {
        format!(
            "{:?}",
            LeafRef::parse(page)
                .expect("a leaf")
                .live()
                .expect("live rows")
        )
    }

    /// Returns one row of the five columns.
    ///
    /// @param key - the key
    /// @param value - the framed integer column's value
    /// @param text - the text column's value
    fn row<'v>(key: i64, value: Datum<'v>, text: &'v [u8]) -> Vec<Datum<'v>> {
        vec![
            Datum::Int(key),
            value,
            Datum::Text(text),
            Datum::Real(0.5),
            Datum::Int(key),
        ]
    }

    /// A spliced page holds exactly the rows a repack of the same leaf holds, in
    /// the same order, and passes the integrity check.
    ///
    /// The leaf has everything a splice has to carry across: tombstones, a
    /// delta row that shadows a packed one, delta rows for new keys at both
    /// ends and between, a NULL, an exception in a typed column, and text in
    /// the heap - which is copied as bytes and has to be where every copied slot
    /// says it is.
    #[test]
    fn a_splice_holds_the_rows_a_repack_holds() {
        let mut page = packed(40);
        {
            let mut leaf = LeafMut::new(&mut page).expect("a leaf");
            assert_eq!(leaf.set_tombstone(4).expect("a tombstone"), Applied::Yes);
            assert_eq!(leaf.set_tombstone(17).expect("a tombstone"), Applied::Yes);
        }
        // Key 10 is packed at row 5 and is shadowed; the others are new.
        insert(
            &mut page,
            &[
                row(-1, Datum::Int(1_001), b"first"),
                row(11, Datum::Null, b"odd"),
                row(10, Datum::Text(b"another exception"), b"shadow"),
                row(77, Datum::Int(1_039), b"last"),
            ],
        );
        let leaf = LeafRef::parse(&page).expect("a leaf");
        let order = leaf.live_order().expect("a merge");
        let spliced = splice_image(&leaf, &order, 8_192, 0.95)
            .expect("a splice")
            .expect("these rows fit the page's widths");
        let repacked = LeafBuilder::new(8_192, 1, columns(), 1)
            .expect("a builder")
            .pack_all_rows(&leaf.live_source().expect("live rows"), 0.95)
            .expect("a repack")
            .expect("these rows fit a page");
        assert_eq!(
            live_text(&spliced),
            live_text(&page),
            "the splice changed a row"
        );
        assert_eq!(
            live_text(&spliced),
            live_text(&repacked),
            "the splice and the repack disagree"
        );
        let view = LeafRef::parse(&spliced).expect("the spliced page parses");
        view.integrity().expect("the spliced page is sound");
        assert_eq!(view.delta_count(), 0);
        assert!(!view.has_tombstones());
        assert_eq!(
            view.row_count(),
            41,
            "40 packed, two tombstoned, three new keys"
        );
        assert_eq!(splices_of(&view), 1);
        assert!(view.has_exceptions());
        // The widths are the old page's, which is what a splice keeps.
        for column in 0..5 {
            assert_eq!(
                view.column_width(column).expect("a width"),
                leaf.column_width(column).expect("a width"),
                "column {column}"
            );
        }
    }

    /// A delta value that does not fit its column's slot sends the leaf to a
    /// repack rather than being truncated.
    ///
    /// The integer column holds an exception, so its slots are four bytes wide
    /// and framed at a base near 1,000; nine billion does not fit in four bytes
    /// from that base.
    #[test]
    fn a_value_too_wide_for_its_slot_is_not_spliced() {
        let mut page = packed(40);
        let leaf = LeafRef::parse(&page).expect("a leaf");
        assert_eq!(
            leaf.column_width(1).expect("a width"),
            4,
            "an exception forces four bytes"
        );
        insert(&mut page, &[row(1, Datum::Int(9_000_000_000), b"x")]);
        let leaf = LeafRef::parse(&page).expect("a leaf");
        let order = leaf.live_order().expect("a merge");
        assert!(splice_image(&leaf, &order, 8_192, 0.95)
            .expect("an answer")
            .is_none());
    }

    /// The compaction after `SPLICE_LIMIT` splices in a row is a repack, so a
    /// page's widths and its heap's leftovers are recovered on a fixed period.
    #[test]
    fn a_leaf_spliced_seven_times_is_repacked_on_the_eighth() {
        let mut page = packed(40);
        for round in 0..=SPLICE_LIMIT {
            let key = 1 + 2 * i64::from(round);
            insert(&mut page, &[row(key, Datum::Int(1_000), b"r")]);
            let leaf = LeafRef::parse(&page).expect("a leaf");
            let order = leaf.live_order().expect("a merge");
            let spliced = splice_image(&leaf, &order, 8_192, 0.95).expect("an answer");
            if round == SPLICE_LIMIT {
                assert!(spliced.is_none(), "splice {} was allowed", round + 1);
                return;
            }
            page = spliced.unwrap_or_else(|| panic!("splice {} was refused", round + 1));
            assert_eq!(
                splices_of(&LeafRef::parse(&page).expect("a leaf")),
                round + 1
            );
        }
        panic!("the loop ended without reaching the limit");
    }

    /// A leaf that says it holds an out-of-line value is never spliced.
    #[test]
    fn a_leaf_with_extents_is_not_spliced() {
        let mut page = packed(10);
        insert(&mut page, &[row(1, Datum::Int(1_000), b"x")]);
        LeafMut::new(&mut page)
            .expect("a leaf")
            .mark_extents()
            .expect("a flag");
        let leaf = LeafRef::parse(&page).expect("a leaf");
        let order = leaf.live_order().expect("a merge");
        assert!(splice_image(&leaf, &order, 8_192, 0.95)
            .expect("an answer")
            .is_none());
    }
}
