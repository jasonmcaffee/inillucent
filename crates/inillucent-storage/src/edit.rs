//! Editing one page: building cells, and putting them on and taking them off.
//!
//! Invariant: every function here leaves the page a [`PageLayout::parse`] would
//! accept, or changes nothing at all. That is what makes the layer above
//! tractable: a balance can rewrite four pages and know that if the third
//! rewrite fails the first two are still valid pages, because a failed edit
//! never half-wrote one.
//!
//! Two shapes of edit live here and they exist for different reasons.
//! [`insert_cell`] and [`remove_cell`] change a page in place, reusing its
//! freeblocks; they are what an insert or a delete that fits does, and they
//! touch a few bytes. [`rewrite_page`] throws the page away and lays it out
//! again from a list of owned cells; it is what balancing does, because a
//! balance moves cells between pages and no in-place edit can express that.
//! In-place is the fast path and rewrite is the correct-by-construction one.
//!
//! The four-byte minimum is the subtlety worth stating. SQLite pads a cell's
//! recorded size to four bytes (`if( nSize<4 ) nSize = 4`), because a freeblock
//! needs four bytes to hold its own next-pointer and length, so a three-byte
//! hole could not be represented. A three-byte cell is reachable - an index
//! entry holding a single NULL is a two-byte record behind a one-byte length -
//! so the padding is not theoretical, and a writer that allocated three bytes
//! would produce a page SQLite's own integrity check calls fragmented.

use inillucent_base::error::corrupt;
use inillucent_base::ids::PageId;
use inillucent_base::{bytes, varint, DbResult};

use crate::btree::payload_split;
use crate::btree::{offsets, BTreePage, PageKind, PageLayout, MAX_FRAGMENTS, MIN_CELL_SIZE};

/// Writes an empty B-tree page header of the given kind.
///
/// `base` is where the header starts: a hundred on page 1, zero everywhere
/// else. The content area starts at the end of the usable area, which for a
/// 65536-byte page is written as zero because that is the one size the header's
/// two bytes cannot hold.
pub fn initialize_btree_page(
    page: &mut [u8],
    base: usize,
    kind: PageKind,
    usable: u32,
) -> DbResult<()> {
    let usable = usable as usize;
    if base.saturating_add(kind.header_len()) > usable || usable > page.len() {
        return Err(corrupt("a page too small to hold a B-tree header"));
    }
    let area = bytes::window_mut(page, base, usable.saturating_sub(base))?;
    area.fill(0);
    bytes::write_u8(page, base.saturating_add(offsets::TYPE), kind.type_byte())?;
    bytes::write_u16(page, base.saturating_add(offsets::FIRST_FREEBLOCK), 0)?;
    bytes::write_u16(page, base.saturating_add(offsets::CELL_COUNT), 0)?;
    bytes::write_u16(
        page,
        base.saturating_add(offsets::CONTENT_START),
        encode_content_start(usable)?,
    )?;
    bytes::write_u8(page, base.saturating_add(offsets::FRAGMENTS), 0)?;
    if !kind.is_leaf() {
        bytes::write_u32(page, base.saturating_add(offsets::RIGHT_CHILD), 0)?;
    }
    Ok(())
}

/// Returns the two-byte form of a content-area start.
fn encode_content_start(offset: usize) -> DbResult<u16> {
    if offset == 65_536 {
        return Ok(0);
    }
    u16::try_from(offset).map_err(|_| corrupt(format!("a content area starting at {offset}")))
}

/// Builds the bytes of one cell.
///
/// `payload` is the whole payload; how much of it goes on the page is decided
/// by the format's own split, and `overflow` is the head of the chain the
/// caller has already written the rest into. A table interior cell has no
/// payload at all - it is a child pointer and the largest rowid below it - so
/// `payload` is ignored for that kind.
pub fn encode_cell(
    kind: PageKind,
    usable: u32,
    left_child: Option<PageId>,
    rowid: Option<i64>,
    payload: &[u8],
    overflow: Option<PageId>,
) -> DbResult<Vec<u8>> {
    let mut cell = Vec::with_capacity(payload.len().saturating_add(16));
    if !kind.is_leaf() {
        let child = left_child.ok_or_else(|| corrupt("an interior cell with no child"))?;
        cell.extend_from_slice(&child.get().to_be_bytes());
    } else if left_child.is_some() {
        return Err(corrupt("a leaf cell with a child pointer"));
    }

    if kind == PageKind::InteriorTable {
        let rowid = rowid.ok_or_else(|| corrupt("a table interior cell with no rowid"))?;
        push_varint_i64(&mut cell, rowid)?;
        return Ok(cell);
    }

    let total = payload.len() as u64;
    push_varint(&mut cell, total)?;
    if kind == PageKind::LeafTable {
        let rowid = rowid.ok_or_else(|| corrupt("a table leaf cell with no rowid"))?;
        push_varint_i64(&mut cell, rowid)?;
    } else if rowid.is_some() {
        return Err(corrupt("an index cell with a rowid"));
    }

    let split = payload_split(total, usable, kind)?;
    let local = payload
        .get(..split.local)
        .ok_or_else(|| corrupt("a payload shorter than its own local part"))?;
    cell.extend_from_slice(local);
    match (split.overflows, overflow) {
        (true, Some(head)) => cell.extend_from_slice(&head.get().to_be_bytes()),
        (true, None) => return Err(corrupt("a cell that overflows with no chain")),
        (false, Some(_)) => return Err(corrupt("a cell that fits with an overflow chain")),
        (false, None) => {}
    }
    Ok(cell)
}

/// Appends a varint to a cell under construction.
fn push_varint(cell: &mut Vec<u8>, value: u64) -> DbResult<()> {
    let mut scratch = [0u8; varint::MAX_LEN];
    let len = varint::encode(&mut scratch, value)?;
    cell.extend_from_slice(
        scratch
            .get(..len)
            .ok_or_else(|| corrupt("a varint width"))?,
    );
    Ok(())
}

/// Appends a signed varint to a cell under construction.
fn push_varint_i64(cell: &mut Vec<u8>, value: i64) -> DbResult<()> {
    let mut scratch = [0u8; varint::MAX_LEN];
    let len = varint::encode_i64(&mut scratch, value)?;
    cell.extend_from_slice(
        scratch
            .get(..len)
            .ok_or_else(|| corrupt("a varint width"))?,
    );
    Ok(())
}

/// Returns the number of bytes a cell occupies on a page.
pub fn cell_footprint(len: usize) -> usize {
    len.max(MIN_CELL_SIZE)
}

/// Returns how many free bytes a page holds, counting freeblocks, fragments,
/// and the gap between the cell pointer array and the content area.
pub fn free_bytes(page: &BTreePage<'_>) -> DbResult<usize> {
    let layout = page.layout();
    let array_end = layout
        .base
        .saturating_add(layout.kind.header_len())
        .saturating_add(layout.cell_count.saturating_mul(2));
    let gap = layout.content_start.saturating_sub(array_end);
    let mut free = gap.saturating_add(usize::from(layout.fragments));
    for block in page.free_blocks()? {
        free = free.saturating_add(block.len);
    }
    Ok(free)
}

/// Returns how many bytes of a page its cells and its header occupy.
pub fn used_bytes(page: &BTreePage<'_>) -> DbResult<usize> {
    let layout = page.layout();
    let mut used = layout
        .kind
        .header_len()
        .saturating_add(layout.cell_count.saturating_mul(2));
    for index in 0..layout.cell_count {
        used = used.saturating_add(cell_footprint(page.cell(index)?.len));
    }
    Ok(used)
}

/// Returns how many bytes a page of this kind has for cells and pointers.
pub fn capacity(base: usize, kind: PageKind, usable: u32) -> usize {
    (usable as usize)
        .saturating_sub(base)
        .saturating_sub(kind.header_len())
}

/// Lays a page out again from an ordered list of cells.
///
/// This is the operation balancing is written in terms of. It cannot fail
/// halfway: the cells are measured first, and a set that does not fit is
/// refused before a byte of the page changes.
pub fn rewrite_page(
    page: &mut [u8],
    base: usize,
    kind: PageKind,
    usable: u32,
    cells: &[Vec<u8>],
    right_child: Option<PageId>,
) -> DbResult<()> {
    let usable_usize = usable as usize;
    if usable_usize > page.len() {
        return Err(corrupt("a usable size larger than the page"));
    }
    let mut wanted = cells.len().saturating_mul(2);
    for cell in cells {
        wanted = wanted.saturating_add(cell_footprint(cell.len()));
    }
    if wanted > capacity(base, kind, usable) {
        return Err(corrupt(format!(
            "{} cells need {wanted} bytes in {} available",
            cells.len(),
            capacity(base, kind, usable)
        )));
    }

    initialize_btree_page(page, base, kind, usable)?;
    if let Some(child) = right_child {
        if kind.is_leaf() {
            return Err(corrupt("a leaf page with a right-most child"));
        }
        bytes::write_u32(page, base.saturating_add(offsets::RIGHT_CHILD), child.get())?;
    } else if !kind.is_leaf() {
        return Err(corrupt("an interior page with no right-most child"));
    }

    // Cells are written from the end of the page backwards so their content is
    // contiguous, which is what a freshly written page looks like and what
    // makes the first insert into it cheap.
    let mut content = usable_usize;
    let array = base.saturating_add(kind.header_len());
    for (index, cell) in cells.iter().enumerate() {
        let footprint = cell_footprint(cell.len());
        content = content
            .checked_sub(footprint)
            .ok_or_else(|| corrupt("a cell that does not fit a page it was measured for"))?;
        let target = bytes::window_mut(page, content, cell.len())?;
        target.copy_from_slice(cell);
        bytes::write_u16(
            page,
            array.saturating_add(index.saturating_mul(2)),
            u16::try_from(content).map_err(|_| corrupt("a cell offset past 65535"))?,
        )?;
    }
    bytes::write_u16(
        page,
        base.saturating_add(offsets::CELL_COUNT),
        u16::try_from(cells.len()).map_err(|_| corrupt("more cells than a page can name"))?,
    )?;
    bytes::write_u16(
        page,
        base.saturating_add(offsets::CONTENT_START),
        encode_content_start(content)?,
    )?;
    Ok(())
}

/// Moves every cell to the end of the page, leaving one contiguous gap.
///
/// This is what a page does when a cell will not fit despite there being room
/// for it: the room is in freeblocks and fragments that no single cell can use.
pub fn defragment(page: &mut [u8], layout: &PageLayout) -> DbResult<()> {
    let view = BTreePage::new(page, layout);
    let mut cells = Vec::with_capacity(layout.cell_count);
    for index in 0..layout.cell_count {
        let cell = view.cell(index)?;
        let raw = bytes::window(page, cell.offset, cell.len)?;
        cells.push(raw.to_vec());
    }
    rewrite_page(
        page,
        layout.base,
        layout.kind,
        layout.usable as u32,
        &cells,
        layout.right_child,
    )
}

/// Inserts a cell at `index`, returning false when the page has no room.
///
/// The page is left untouched when the answer is false, so the caller can
/// defragment and ask again, or balance if that does not help either.
pub fn insert_cell(
    page: &mut [u8],
    layout: &PageLayout,
    index: usize,
    cell: &[u8],
) -> DbResult<bool> {
    if index > layout.cell_count {
        return Err(corrupt(format!(
            "cell {index} cannot be inserted into a page holding {}",
            layout.cell_count
        )));
    }
    let footprint = cell_footprint(cell.len());
    let array = layout.base.saturating_add(layout.kind.header_len());
    let array_end = array.saturating_add(layout.cell_count.saturating_mul(2));
    let new_array_end = array_end.saturating_add(2);

    let Some(offset) = allocate_space(page, layout, footprint, new_array_end)? else {
        return Ok(false);
    };

    // Shift the pointer array right by two bytes from `index` on, then write
    // the new pointer into the hole.
    let mut cursor = layout.cell_count;
    while cursor > index {
        let from = array.saturating_add(cursor.saturating_sub(1).saturating_mul(2));
        let value = bytes::read_u16(page, from)?;
        bytes::write_u16(page, from.saturating_add(2), value)?;
        cursor = cursor.saturating_sub(1);
    }
    bytes::write_u16(
        page,
        array.saturating_add(index.saturating_mul(2)),
        u16::try_from(offset).map_err(|_| corrupt("a cell offset past 65535"))?,
    )?;
    let target = bytes::window_mut(page, offset, cell.len())?;
    target.copy_from_slice(cell);
    bytes::write_u16(
        page,
        layout.base.saturating_add(offsets::CELL_COUNT),
        u16::try_from(layout.cell_count.saturating_add(1))
            .map_err(|_| corrupt("more cells than a page can name"))?,
    )?;
    Ok(true)
}

/// Finds `size` contiguous bytes for a cell, updating the page's bookkeeping.
///
/// The freeblock chain is searched before the gap between the cell pointer
/// array and the content area, which is the order SQLite uses and is the right
/// way round: the gap is the only space a *new* pointer can come from, so
/// spending it on cell content while a hole sits unused makes a page reject a
/// cell it had room for. The search is first fit, and the slot is carved off
/// the end of the block it comes from so the block keeps its offset and its
/// place in the sorted chain.
fn allocate_space(
    page: &mut [u8],
    layout: &PageLayout,
    size: usize,
    array_end: usize,
) -> DbResult<Option<usize>> {
    let top = layout.content_start;
    if array_end <= top && layout.first_freeblock != 0 {
        if let Some(offset) = find_slot(page, layout, size)? {
            return Ok(Some(offset));
        }
    }
    if top.saturating_sub(array_end) >= size {
        let offset = top.saturating_sub(size);
        bytes::write_u16(
            page,
            layout.base.saturating_add(offsets::CONTENT_START),
            encode_content_start(offset)?,
        )?;
        return Ok(Some(offset));
    }
    Ok(None)
}

/// Returns the first freeblock large enough for `size`, carving it.
fn find_slot(page: &mut [u8], layout: &PageLayout, size: usize) -> DbResult<Option<usize>> {
    let view = BTreePage::new(page, layout);
    let blocks = view.free_blocks()?;
    let mut previous: Option<usize> = None;
    for block in blocks {
        if block.len >= size {
            let leftover = block.len.saturating_sub(size);
            if leftover < MIN_CELL_SIZE {
                // What is left is too small to be a freeblock, so the whole
                // block leaves the chain and the remainder becomes fragments.
                let fragments =
                    bytes::read_u8(page, layout.base.saturating_add(offsets::FRAGMENTS))?;
                let fragments = usize::from(fragments).saturating_add(leftover);
                if fragments > usize::from(MAX_FRAGMENTS) {
                    // A page this fragmented has to be rebuilt before it can
                    // take another cell; refusing is what makes the caller
                    // defragment rather than write an illegal fragment count.
                    return Ok(None);
                }
                let next = bytes::read_u16(page, block.offset)?;
                match previous {
                    Some(before) => bytes::write_u16(page, before, next)?,
                    None => bytes::write_u16(
                        page,
                        layout.base.saturating_add(offsets::FIRST_FREEBLOCK),
                        next,
                    )?,
                }
                bytes::write_u8(
                    page,
                    layout.base.saturating_add(offsets::FRAGMENTS),
                    u8::try_from(fragments).unwrap_or(MAX_FRAGMENTS),
                )?;
                return Ok(Some(block.offset));
            }
            bytes::write_u16(
                page,
                block.offset.saturating_add(2),
                u16::try_from(leftover).map_err(|_| corrupt("a freeblock too long"))?,
            )?;
            return Ok(Some(block.offset.saturating_add(leftover)));
        }
        previous = Some(block.offset);
    }
    Ok(None)
}

/// Removes the cell at `index`, returning its bytes to the page's free space.
pub fn remove_cell(page: &mut [u8], layout: &PageLayout, index: usize) -> DbResult<()> {
    let view = BTreePage::new(page, layout);
    let cell = view.cell(index)?;
    let start = cell.offset;
    let size = cell_footprint(cell.len);

    let array = layout.base.saturating_add(layout.kind.header_len());
    let mut cursor = index;
    while cursor.saturating_add(1) < layout.cell_count {
        let from = array.saturating_add(cursor.saturating_add(1).saturating_mul(2));
        let value = bytes::read_u16(page, from)?;
        bytes::write_u16(page, from.saturating_sub(2), value)?;
        cursor = cursor.saturating_add(1);
    }
    bytes::write_u16(
        page,
        layout.base.saturating_add(offsets::CELL_COUNT),
        u16::try_from(layout.cell_count.saturating_sub(1))
            .map_err(|_| corrupt("a cell count that does not fit"))?,
    )?;
    free_space(page, layout, start, size)
}

/// Returns a span of the content area to the freeblock chain, coalescing it
/// with the blocks on either side.
///
/// The chain has to stay sorted and non-adjacent: `PageLayout::parse` refuses a
/// chain that does not increase, and two adjacent blocks that were never merged
/// are two holes a cell cannot use even though together they would hold it.
fn free_space(page: &mut [u8], layout: &PageLayout, start: usize, size: usize) -> DbResult<()> {
    if size < MIN_CELL_SIZE {
        return Err(corrupt("a freed span smaller than a freeblock"));
    }
    let end = start.saturating_add(size);
    if end > layout.usable {
        return Err(corrupt("a freed span that runs past the page"));
    }

    let view = BTreePage::new(page, layout);
    let blocks = view.free_blocks()?;
    let mut previous: Option<usize> = None;
    let mut next: Option<usize> = None;
    for block in &blocks {
        if block.offset < start {
            previous = Some(block.offset);
        } else if next.is_none() {
            next = Some(block.offset);
        }
    }

    let mut start = start;
    let mut end = end;
    let mut following = next.unwrap_or(0);

    // Merge with the block that follows, if it is adjacent.
    if let Some(after) = next {
        let after_len = usize::from(bytes::read_u16(page, after.saturating_add(2))?);
        if end == after {
            end = after.saturating_add(after_len);
            following = usize::from(bytes::read_u16(page, after)?);
        }
    }
    // Merge with the block before, if it is adjacent.
    let mut link_from = previous;
    if let Some(before) = previous {
        let before_len = usize::from(bytes::read_u16(page, before.saturating_add(2))?);
        if before.saturating_add(before_len) == start {
            start = before;
            link_from = blocks
                .iter()
                .rev()
                .find(|block| block.offset < before)
                .map(|block| block.offset);
        }
    }

    bytes::write_u16(page, start, u16::try_from(following).unwrap_or(0))?;
    bytes::write_u16(
        page,
        start.saturating_add(2),
        u16::try_from(end.saturating_sub(start)).map_err(|_| corrupt("a freeblock too long"))?,
    )?;
    match link_from {
        Some(before) => bytes::write_u16(page, before, u16::try_from(start).unwrap_or(0))?,
        None => bytes::write_u16(
            page,
            layout.base.saturating_add(offsets::FIRST_FREEBLOCK),
            u16::try_from(start).unwrap_or(0),
        )?,
    }

    // A freed span that touches the content area's start moves the boundary
    // instead of becoming a freeblock, which is how a page that empties ends up
    // with no chain at all rather than one enormous block.
    if start == layout.content_start {
        bytes::write_u16(
            page,
            layout.base.saturating_add(offsets::CONTENT_START),
            encode_content_start(end)?,
        )?;
        let following_value = u16::try_from(following).unwrap_or(0);
        match link_from {
            Some(before) => bytes::write_u16(page, before, following_value)?,
            None => bytes::write_u16(
                page,
                layout.base.saturating_add(offsets::FIRST_FREEBLOCK),
                following_value,
            )?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_base::ids::PageId;

    /// Builds an empty page of a kind and returns its bytes.
    fn empty(kind: PageKind, usable: u32) -> Vec<u8> {
        let mut page = vec![0u8; usable as usize];
        initialize_btree_page(&mut page, 0, kind, usable).unwrap();
        if !kind.is_leaf() {
            let layout = PageLayout::parse(&page, PageId::new(2).unwrap(), usable);
            // An interior page with a zero right child does not parse, so the
            // pointer is set before anything reads it back.
            assert!(layout.is_err());
            bytes::write_u32(&mut page, offsets::RIGHT_CHILD, 9).unwrap();
        }
        page
    }

    /// Parses a page that is not page 1.
    fn parse(page: &[u8], usable: u32) -> PageLayout {
        PageLayout::parse(page, PageId::new(2).unwrap(), usable).unwrap()
    }

    /// An empty page of every kind parses, and holds nothing.
    #[test]
    fn an_initialised_page_of_every_kind_parses() {
        for usable in [512u32, 1024, 4096, 65_536] {
            for kind in PageKind::all() {
                let page = empty(kind, usable);
                let layout = parse(&page, usable);
                assert_eq!(layout.kind, kind);
                assert_eq!(layout.cell_count, 0);
                assert_eq!(layout.content_start, usable as usize);
                assert_eq!(layout.first_freeblock, 0);
                assert_eq!(layout.fragments, 0);
            }
        }
    }

    /// Cells inserted in key order come back in key order, and the page stays
    /// valid after every one of them.
    #[test]
    fn inserted_cells_come_back_in_order() {
        let usable = 1024u32;
        let mut page = empty(PageKind::LeafTable, usable);
        let mut layout = parse(&page, usable);
        for rowid in 0..20i64 {
            let payload = vec![0x01u8];
            let cell = encode_cell(
                PageKind::LeafTable,
                usable,
                None,
                Some(rowid),
                &payload,
                None,
            )
            .unwrap();
            let index = layout.cell_count;
            assert!(insert_cell(&mut page, &layout, index, &cell).unwrap());
            layout = parse(&page, usable);
        }
        assert_eq!(layout.cell_count, 20);
        let view = BTreePage::new(&page, &layout);
        for index in 0..20usize {
            assert_eq!(view.cell_rowid(index).unwrap(), index as i64);
        }
        view.check_layout().unwrap();
    }

    /// Removing every cell one at a time leaves a page that still parses, still
    /// tiles its content area, and ends up empty.
    #[test]
    fn removing_every_cell_leaves_a_valid_empty_page() {
        let usable = 512u32;
        let mut page = empty(PageKind::LeafTable, usable);
        let mut layout = parse(&page, usable);
        for rowid in 0..12i64 {
            let payload = vec![0x03u8, 0x01, (rowid as u8)];
            let cell = encode_cell(
                PageKind::LeafTable,
                usable,
                None,
                Some(rowid),
                &payload,
                None,
            )
            .unwrap();
            assert!(insert_cell(&mut page, &layout, layout.cell_count, &cell).unwrap());
            layout = parse(&page, usable);
        }
        while layout.cell_count > 0 {
            remove_cell(&mut page, &layout, 0).unwrap();
            layout = parse(&page, usable);
            BTreePage::new(&page, &layout).check_layout().unwrap();
        }
        assert_eq!(layout.cell_count, 0);
    }

    /// Removing from the middle and inserting again reuses the freeblock, which
    /// is the whole point of the chain.
    #[test]
    fn a_freed_block_is_reused() {
        let usable = 1024u32;
        let mut page = empty(PageKind::LeafTable, usable);
        let mut layout = parse(&page, usable);
        for rowid in 0..8i64 {
            let payload = vec![0x05u8, 0x01, 0x01, 1, 2];
            let cell = encode_cell(
                PageKind::LeafTable,
                usable,
                None,
                Some(rowid),
                &payload,
                None,
            )
            .unwrap();
            assert!(insert_cell(&mut page, &layout, layout.cell_count, &cell).unwrap());
            layout = parse(&page, usable);
        }
        let before = layout.content_start;
        remove_cell(&mut page, &layout, 3).unwrap();
        layout = parse(&page, usable);
        let payload = vec![0x05u8, 0x01, 0x01, 9, 9];
        let cell =
            encode_cell(PageKind::LeafTable, usable, None, Some(99), &payload, None).unwrap();
        assert!(insert_cell(&mut page, &layout, 3, &cell).unwrap());
        layout = parse(&page, usable);
        assert_eq!(
            layout.content_start, before,
            "the reinserted cell should have used the hole rather than the gap"
        );
        BTreePage::new(&page, &layout).check_layout().unwrap();
    }

    /// A page that is full refuses a cell rather than corrupting itself.
    #[test]
    fn a_full_page_refuses_a_cell() {
        let usable = 512u32;
        let mut page = empty(PageKind::LeafTable, usable);
        let mut layout = parse(&page, usable);
        let payload = vec![0u8; 100];
        let mut placed = 0;
        loop {
            let cell = encode_cell(
                PageKind::LeafTable,
                usable,
                None,
                Some(placed as i64),
                &payload,
                None,
            )
            .unwrap();
            if !insert_cell(&mut page, &layout, layout.cell_count, &cell).unwrap() {
                break;
            }
            layout = parse(&page, usable);
            placed += 1;
            assert!(placed < 100);
        }
        assert!(placed >= 3);
        BTreePage::new(&page, &layout).check_layout().unwrap();
    }

    /// Defragmenting keeps every cell and its order, and leaves no holes.
    #[test]
    fn defragmenting_keeps_every_cell() {
        let usable = 1024u32;
        let mut page = empty(PageKind::LeafTable, usable);
        let mut layout = parse(&page, usable);
        for rowid in 0..10i64 {
            let payload = vec![0x07u8, 0x01, 0x01, 0x01, 1, 2, 3];
            let cell = encode_cell(
                PageKind::LeafTable,
                usable,
                None,
                Some(rowid),
                &payload,
                None,
            )
            .unwrap();
            assert!(insert_cell(&mut page, &layout, layout.cell_count, &cell).unwrap());
            layout = parse(&page, usable);
        }
        for index in [7usize, 4, 1] {
            remove_cell(&mut page, &layout, index).unwrap();
            layout = parse(&page, usable);
        }
        let before: Vec<i64> = (0..layout.cell_count)
            .map(|index| BTreePage::new(&page, &layout).cell_rowid(index).unwrap())
            .collect();
        defragment(&mut page, &layout).unwrap();
        layout = parse(&page, usable);
        let after: Vec<i64> = (0..layout.cell_count)
            .map(|index| BTreePage::new(&page, &layout).cell_rowid(index).unwrap())
            .collect();
        assert_eq!(before, after);
        assert_eq!(layout.first_freeblock, 0);
        assert_eq!(layout.fragments, 0);
        BTreePage::new(&page, &layout).check_layout().unwrap();
    }

    /// A cell whose payload does not fit locally carries the overflow pointer
    /// and exactly the local bytes the format's split names.
    #[test]
    fn an_overflowing_cell_carries_its_pointer() {
        let usable = 512u32;
        let payload = vec![0xabu8; 2000];
        let head = PageId::new(7).unwrap();
        let cell = encode_cell(
            PageKind::LeafTable,
            usable,
            None,
            Some(1),
            &payload,
            Some(head),
        )
        .unwrap();
        let split = payload_split(2000, usable, PageKind::LeafTable).unwrap();
        assert!(split.overflows);
        let tail = cell.get(cell.len() - 4..).unwrap();
        assert_eq!(u32::from_be_bytes(tail.try_into().unwrap()), 7);
        assert_eq!(cell.len(), 1 + 2 + split.local + 4);
    }
}
