//! The B-tree page codec: the four page kinds, their cells, and validation.
//!
//! Invariant: a page is validated once, completely, before any cell on it is
//! read, and the validation is of the page's *internal consistency* rather
//! than of the bytes a cell happens to contain. The header must fit, the cell
//! pointer array must fit after it, every cell pointer must land inside the
//! content area, every cell's own length arithmetic must stay inside the page,
//! the freeblock chain must be increasing and finite, and the fragment count
//! must be inside the sixty bytes the format allows. After `PageLayout::parse`
//! returns, a cell accessor indexes into ranges that were proved to exist.
//!
//! Validation and access are separate types on purpose. `PageLayout` is the
//! result of validating a page and owns nothing borrowed, so a cursor can hold
//! one alongside the page's pin for as long as it stays on that page;
//! `BTreePage` pairs a layout with the bytes it was validated against and
//! costs nothing to construct. Without the split, a cursor would either
//! re-validate the page on every cell it read or hold a self-referential
//! struct, and the first of those is quadratic in the cell count.
//!
//! The local-payload formulas are the subtlest part of the file format and the
//! easiest place to be quietly wrong: a cell whose payload is one byte over
//! the threshold stores a different number of bytes locally, and a reader that
//! computes the threshold differently from the writer reads the overflow
//! pointer out of the middle of the payload. They are written out here as the
//! format states them, with the table-leaf and index forms kept apart because
//! they use different maxima.

use inillucent_base::bytes;
use inillucent_base::error::corrupt;
use inillucent_base::ids::PageId;
use inillucent_base::varint;
use inillucent_base::DbResult;

/// Which of the four kinds of B-tree page this is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageKind {
    /// An interior page of an index B-tree. Type byte 0x02.
    InteriorIndex,
    /// An interior page of a table B-tree. Type byte 0x05.
    InteriorTable,
    /// A leaf page of an index B-tree. Type byte 0x0a.
    LeafIndex,
    /// A leaf page of a table B-tree. Type byte 0x0d.
    LeafTable,
}

impl PageKind {
    /// Returns the kind a type byte names.
    pub fn from_type_byte(byte: u8) -> DbResult<PageKind> {
        Ok(match byte {
            0x02 => PageKind::InteriorIndex,
            0x05 => PageKind::InteriorTable,
            0x0a => PageKind::LeafIndex,
            0x0d => PageKind::LeafTable,
            other => return Err(corrupt(format!("0x{other:02x} is not a B-tree page type"))),
        })
    }

    /// Returns the type byte for this kind.
    pub fn type_byte(self) -> u8 {
        match self {
            PageKind::InteriorIndex => 0x02,
            PageKind::InteriorTable => 0x05,
            PageKind::LeafIndex => 0x0a,
            PageKind::LeafTable => 0x0d,
        }
    }

    /// Reports whether this is a leaf.
    pub fn is_leaf(self) -> bool {
        matches!(self, PageKind::LeafIndex | PageKind::LeafTable)
    }

    /// Reports whether this page belongs to a table B-tree.
    pub fn is_table(self) -> bool {
        matches!(self, PageKind::InteriorTable | PageKind::LeafTable)
    }

    /// Returns the page header's length in bytes: eight on a leaf, twelve on
    /// an interior page, which carries a right-most child pointer as well.
    pub fn header_len(self) -> usize {
        if self.is_leaf() {
            8
        } else {
            12
        }
    }

    /// Returns every kind, for exhaustive matrices.
    pub fn all() -> [PageKind; 4] {
        [
            PageKind::InteriorIndex,
            PageKind::InteriorTable,
            PageKind::LeafIndex,
            PageKind::LeafTable,
        ]
    }
}

/// Byte offsets within a B-tree page header.
pub mod offsets {
    /// The page type byte.
    pub const TYPE: usize = 0;
    /// The offset of the first freeblock, or zero.
    pub const FIRST_FREEBLOCK: usize = 1;
    /// The number of cells on the page.
    pub const CELL_COUNT: usize = 3;
    /// The start of the cell content area, where zero means 65536.
    pub const CONTENT_START: usize = 5;
    /// The number of fragmented free bytes.
    pub const FRAGMENTS: usize = 7;
    /// The right-most child pointer, on an interior page only.
    pub const RIGHT_CHILD: usize = 8;
}

/// The most fragmented free bytes a legal page may report.
pub const MAX_FRAGMENTS: u8 = 60;

/// The fewest bytes a cell may occupy on a page.
///
/// A cell can decode to fewer (an index entry holding one NULL is three
/// bytes), but the space it occupies is rounded up to four, because a freeblock
/// has to hold a two-byte next pointer and a two-byte length and a shorter hole
/// could not be represented. SQLite does the same rounding inside its own cell
/// parser (`if( nSize<4 ) nSize = 4`), so a page laid out by either engine
/// accounts for the same bytes and each one's integrity check accepts the
/// other's pages.
pub const MIN_CELL_SIZE: usize = 4;

/// How much of a cell's payload is stored on the page itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayloadSplit {
    /// The payload's total length across the page and its overflow chain.
    pub total: u64,
    /// How many bytes are stored on this page.
    pub local: usize,
    /// Whether the cell has an overflow chain at all.
    pub overflows: bool,
}

/// The local-payload window for one kind of page at one usable size.
///
/// The three numbers behind a payload split depend only on the page, not on the
/// cell, and computing them costs two integer divisions. Doing that per cell
/// put two divisions in the innermost loop of page validation - which runs once
/// for every cell on every page a descent passes through, and an interior table
/// page holds hundreds. Computing them once per page and carrying them on the
/// layout is the same arithmetic in a place it is not repeated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayloadWindow {
    /// The most payload that may be stored on the page itself.
    pub max_local: u64,
    /// The least that may be, once any of it overflows.
    pub min_local: u64,
    /// The usable bytes per page.
    pub usable: u64,
}

impl PayloadWindow {
    /// Returns the window for a page kind at a usable size.
    pub fn new(usable: u32, kind: PageKind) -> DbResult<PayloadWindow> {
        let usable = u64::from(usable);
        if usable < 480 {
            return Err(corrupt(format!(
                "a usable page size of {usable} is too small"
            )));
        }
        let max_local = if kind == PageKind::LeafTable {
            usable.saturating_sub(35)
        } else {
            (usable.saturating_sub(12))
                .saturating_mul(64)
                .checked_div(255)
                .unwrap_or(0)
                .saturating_sub(23)
        };
        let min_local = (usable.saturating_sub(12))
            .saturating_mul(32)
            .checked_div(255)
            .unwrap_or(0)
            .saturating_sub(23);
        Ok(PayloadWindow {
            max_local,
            min_local,
            usable,
        })
    }

    /// Returns how a payload of `total` bytes splits inside this window.
    pub fn split(&self, total: u64) -> DbResult<PayloadSplit> {
        if total <= self.max_local {
            let local = usize::try_from(total)
                .map_err(|_| corrupt("a local payload longer than memory"))?;
            return Ok(PayloadSplit {
                total,
                local,
                overflows: false,
            });
        }
        let surplus = self.min_local.saturating_add(
            total
                .saturating_sub(self.min_local)
                .checked_rem(self.usable.saturating_sub(4))
                .unwrap_or(0),
        );
        let local = if surplus <= self.max_local {
            surplus
        } else {
            self.min_local
        };
        if local > total {
            return Err(corrupt(
                "a payload split produced more local bytes than payload",
            ));
        }
        Ok(PayloadSplit {
            total,
            local: usize::try_from(local)
                .map_err(|_| corrupt("a local payload longer than memory"))?,
            overflows: true,
        })
    }
}

/// Returns how a payload of `total` bytes splits on a page of `usable` bytes.
///
/// The formulas are the ones the file format states. The maximum local payload
/// is `U - 35` on a table leaf and `((U-12)*64/255)-23` on an index page; past
/// it, `M = ((U-12)*32/255)-23` is the smallest amount that may be stored
/// locally and `K = M + ((P-M) % (U-4))` is the amount that would be stored if
/// it fit, and the split takes `K` when `K` is no larger than the maximum and
/// `M` otherwise. The point of the `K` term is that it leaves the last
/// overflow page as full as possible rather than nearly empty.
pub fn payload_split(total: u64, usable: u32, kind: PageKind) -> DbResult<PayloadSplit> {
    PayloadWindow::new(usable, kind)?.split(total)
}

/// One cell on a page, decoded but with its payload still on the page.
#[derive(Clone, Copy, Debug)]
pub struct CellRef<'a> {
    /// The cell's offset within the page.
    pub offset: usize,
    /// The total length of the cell, in bytes, on this page.
    pub len: usize,
    /// The left child page, on an interior page.
    pub left_child: Option<PageId>,
    /// The rowid, on a table page.
    pub rowid: Option<i64>,
    /// How the payload splits between this page and an overflow chain.
    pub split: PayloadSplit,
    /// The payload bytes stored on this page.
    pub local_payload: &'a [u8],
    /// Where those bytes start within the page.
    ///
    /// The slice knows its length and not its address, and a caller that wants
    /// to *write* one of those bytes needs the offset to hand to `edit_page`.
    pub local_offset: usize,
    /// The first page of the overflow chain, when there is one.
    pub overflow: Option<PageId>,
}

impl CellRef<'_> {
    /// Reports whether the whole payload is on this page.
    pub fn is_complete(&self) -> bool {
        !self.split.overflows
    }

    /// Returns how many bytes of the page this cell occupies, which is its
    /// decoded length rounded up to [`MIN_CELL_SIZE`].
    pub fn footprint(&self) -> usize {
        self.len.max(MIN_CELL_SIZE)
    }
}

/// A freeblock in a page's free list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreeBlock {
    /// The block's offset within the page.
    pub offset: usize,
    /// The block's length in bytes.
    pub len: usize,
}

/// A validated page's structure, holding nothing borrowed.
#[derive(Clone, Debug)]
pub struct PageLayout {
    /// Where the B-tree header starts: 100 on page 1, zero elsewhere.
    pub base: usize,
    /// The usable bytes per page, after the reserved tail.
    pub usable: usize,
    /// Which kind of page this is.
    pub kind: PageKind,
    /// How many cells the page holds.
    pub cell_count: usize,
    /// Where the cell content area starts.
    pub content_start: usize,
    /// The first freeblock's offset, or zero.
    pub first_freeblock: usize,
    /// The number of fragmented free bytes.
    pub fragments: u8,
    /// The right-most child pointer, on an interior page.
    pub right_child: Option<PageId>,
    /// Each cell's offset, in the page's own order.
    pub cell_pointers: Vec<usize>,
    /// The local-payload window every cell on this page splits against.
    pub window: PayloadWindow,
}

impl PageLayout {
    /// Validates a page and records where everything on it lives.
    ///
    /// `page_number` decides where the B-tree header starts: on page 1 it
    /// follows the hundred-byte database header, and everywhere else it is at
    /// offset zero. `usable` is the page size less the reserved tail, which is
    /// what every calculation in the format is against.
    pub fn parse(bytes: &[u8], page_number: PageId, usable: u32) -> DbResult<PageLayout> {
        PageLayout::read(bytes, page_number, usable, true)
    }

    /// Parses a page this process wrote, without decoding every cell again.
    ///
    /// `parse` decodes every cell on the page to prove each one lies inside it.
    /// That is the right thing to do with bytes that came out of a file: they
    /// are the one input this engine cannot vouch for, and a cell pointing past
    /// the page is how a corrupt database turns into a read of somebody else's
    /// memory. It is also, measured, the single most expensive thing the
    /// storage layer does - a full index leaf holds four hundred cells and
    /// parsing it costs six microseconds, which is twenty times a rowid seek.
    ///
    /// Bytes this process has just written are a different input. They were
    /// validated when the page was read, and everything that has happened to
    /// them since came out of `edit::insert_cell`, `edit::remove_cell` or
    /// `edit::rewrite_page` - the only code in the workspace that writes a
    /// B-tree page, all of which measure before they move anything. Re-proving
    /// their output on every edit is proving this crate against itself, and it
    /// was costing a page parse per cell inserted or removed.
    ///
    /// What is *not* skipped: the header, the cell-pointer array bounds, the
    /// freeblock chain, and every later read of an individual cell, which is
    /// still checked by `cell`. And under `debug_assertions` - which is how the
    /// whole test suite runs - the full validation still happens, so a bug in
    /// an edit primitive fails a test rather than surviving to a release.
    /// @param bytes - the page as this process just wrote it
    /// @param page_number - which page it is
    /// @param usable - the usable bytes per page
    pub fn parse_edited(bytes: &[u8], page_number: PageId, usable: u32) -> DbResult<PageLayout> {
        PageLayout::read(bytes, page_number, usable, cfg!(debug_assertions))
    }

    /// Parses a page, validating every cell only when asked.
    fn read(
        bytes: &[u8],
        page_number: PageId,
        usable: u32,
        validate: bool,
    ) -> DbResult<PageLayout> {
        let base = if page_number.get() == 1 { 100 } else { 0 };
        let usable = usable as usize;
        if usable > bytes.len() {
            return Err(corrupt(format!(
                "a usable size of {usable} inside a {}-byte page",
                bytes.len()
            )));
        }
        if base >= usable {
            return Err(corrupt("page 1 is too small to hold a B-tree header"));
        }
        let kind = PageKind::from_type_byte(bytes::read_u8(bytes, base)?)?;
        let header_len = kind.header_len();
        let pointer_array = base
            .checked_add(header_len)
            .ok_or_else(|| corrupt("a page header that does not fit"))?;
        if pointer_array > usable {
            return Err(corrupt("a page header that runs past the usable area"));
        }

        let cell_count = usize::from(bytes::read_u16(
            bytes,
            base.saturating_add(offsets::CELL_COUNT),
        )?);
        let pointer_bytes = cell_count
            .checked_mul(2)
            .ok_or_else(|| corrupt("a cell count that overflows the pointer array"))?;
        let content_area = pointer_array
            .checked_add(pointer_bytes)
            .ok_or_else(|| corrupt("a cell pointer array that does not fit"))?;
        if content_area > usable {
            return Err(corrupt(format!(
                "{cell_count} cell pointers do not fit in {usable} usable bytes"
            )));
        }

        // A content start of zero means 65536, which is the one page size that
        // does not fit in the header's two bytes.
        let raw_content = usize::from(bytes::read_u16(
            bytes,
            base.saturating_add(offsets::CONTENT_START),
        )?);
        let content_start = if raw_content == 0 {
            65_536
        } else {
            raw_content
        };
        if content_start > usable {
            return Err(corrupt(format!(
                "a content area starting at {content_start} in {usable} usable bytes"
            )));
        }
        if cell_count > 0 && content_start < content_area {
            return Err(corrupt(
                "the cell content area overlaps the cell pointer array",
            ));
        }

        let fragments = bytes::read_u8(bytes, base.saturating_add(offsets::FRAGMENTS))?;
        if fragments > MAX_FRAGMENTS {
            return Err(corrupt(format!(
                "{fragments} fragmented bytes exceeds the {MAX_FRAGMENTS} the format allows"
            )));
        }

        let right_child = if kind.is_leaf() {
            None
        } else {
            let raw = bytes::read_u32(bytes, base.saturating_add(offsets::RIGHT_CHILD))?;
            Some(
                PageId::from_persisted(raw)
                    .map_err(|_| corrupt("an interior page whose right-most child is page zero"))?,
            )
        };

        let mut cell_pointers = Vec::with_capacity(cell_count.min(usable / 2));
        for index in 0..cell_count {
            let at = pointer_array
                .checked_add(index.saturating_mul(2))
                .ok_or_else(|| corrupt("a cell pointer past the end of memory"))?;
            let pointer = usize::from(bytes::read_u16(bytes, at)?);
            if pointer < content_area || pointer >= usable {
                return Err(corrupt(format!(
                    "cell {index} points at offset {pointer}, outside {content_area}..{usable}"
                )));
            }
            cell_pointers.push(pointer);
        }

        let layout = PageLayout {
            window: PayloadWindow::new(usable as u32, kind)?,
            base,
            usable,
            kind,
            cell_count,
            content_start,
            first_freeblock: usize::from(bytes::read_u16(
                bytes,
                base.saturating_add(offsets::FIRST_FREEBLOCK),
            )?),
            fragments,
            right_child,
            cell_pointers,
        };
        let page = BTreePage::new(bytes, &layout);
        page.free_blocks()?;
        if validate {
            page.validate_cells()?;
        }
        Ok(layout)
    }
}

/// A validated B-tree page: a layout, and the bytes it was validated against.
#[derive(Clone, Copy, Debug)]
pub struct BTreePage<'a> {
    bytes: &'a [u8],
    layout: &'a PageLayout,
}

impl<'a> BTreePage<'a> {
    /// Pairs a validated layout with the bytes it describes.
    ///
    /// The caller is responsible for passing the same bytes the layout was
    /// parsed from; every caller in this crate holds the page's pin across
    /// both, so the bytes cannot change in between.
    pub fn new(bytes: &'a [u8], layout: &'a PageLayout) -> BTreePage<'a> {
        BTreePage { bytes, layout }
    }

    /// Validates a page and returns both halves.
    ///
    /// This is the one-shot form, for a caller that reads a page once and does
    /// not hold it. A cursor keeps the layout instead.
    pub fn parse(bytes: &[u8], page_number: PageId, usable: u32) -> DbResult<PageLayout> {
        PageLayout::parse(bytes, page_number, usable)
    }

    /// Returns the page's layout.
    pub fn layout(&self) -> &'a PageLayout {
        self.layout
    }

    /// Returns the page's kind.
    pub fn kind(&self) -> PageKind {
        self.layout.kind
    }

    /// Returns how many cells the page holds.
    pub fn cell_count(&self) -> usize {
        self.layout.cell_count
    }

    /// Returns the right-most child pointer, on an interior page.
    pub fn right_child(&self) -> Option<PageId> {
        self.layout.right_child
    }

    /// Returns where the cell content area starts.
    pub fn content_start(&self) -> usize {
        self.layout.content_start
    }

    /// Returns the number of fragmented free bytes.
    pub fn fragments(&self) -> u8 {
        self.layout.fragments
    }

    /// Returns the usable size the page was validated against.
    pub fn usable(&self) -> usize {
        self.layout.usable
    }

    /// Returns the page's raw bytes.
    pub fn raw(&self) -> &'a [u8] {
        self.bytes
    }

    /// Walks the freeblock chain, which must increase and must terminate.
    ///
    /// An unsorted or cyclic chain is the classic way a corrupt page turns a
    /// defragmenting writer into an infinite loop, so the chain is checked
    /// before anything walks it.
    pub fn free_blocks(&self) -> DbResult<Vec<FreeBlock>> {
        let mut blocks = Vec::new();
        let mut offset = self.layout.first_freeblock;
        let mut previous_end = 0usize;
        // The chain cannot be longer than the page has room for four-byte
        // blocks, so this bound is a real one rather than an arbitrary guard.
        let limit = self.layout.usable / 4 + 1;
        let mut steps = 0usize;
        while offset != 0 {
            steps = steps.saturating_add(1);
            if steps > limit {
                return Err(corrupt("a freeblock chain that does not terminate"));
            }
            if offset
                < self
                    .layout
                    .base
                    .saturating_add(self.layout.kind.header_len())
                || offset.saturating_add(4) > self.layout.usable
            {
                return Err(corrupt(format!("a freeblock at {offset} outside the page")));
            }
            if offset < previous_end {
                return Err(corrupt(
                    "a freeblock chain that does not increase, which cannot be coalesced",
                ));
            }
            let next = usize::from(bytes::read_u16(self.bytes, offset)?);
            let len = usize::from(bytes::read_u16(self.bytes, offset.saturating_add(2))?);
            if len < 4 {
                return Err(corrupt(format!(
                    "a freeblock of {len} bytes, below the four the format requires"
                )));
            }
            let end = offset
                .checked_add(len)
                .ok_or_else(|| corrupt("a freeblock length that overflows"))?;
            if end > self.layout.usable {
                return Err(corrupt(format!(
                    "a freeblock ending at {end} in {} usable bytes",
                    self.layout.usable
                )));
            }
            blocks.push(FreeBlock { offset, len });
            previous_end = end;
            offset = next;
        }
        Ok(blocks)
    }

    /// Returns one cell, decoded.
    pub fn cell(&self, index: usize) -> DbResult<CellRef<'a>> {
        let offset = *self
            .layout
            .cell_pointers
            .get(index)
            .ok_or_else(|| corrupt(format!("cell {index} does not exist on this page")))?;
        self.decode_cell(offset)
    }

    /// Returns the rowid of a cell on a table page.
    ///
    /// Reading only the rowid avoids computing the payload split, which is
    /// what a binary search down an interior page wants: it touches every cell
    /// on the path and needs the key from each, and nothing else.
    pub fn cell_rowid(&self, index: usize) -> DbResult<i64> {
        let offset = *self
            .layout
            .cell_pointers
            .get(index)
            .ok_or_else(|| corrupt(format!("cell {index} does not exist on this page")))?;
        match self.layout.kind {
            PageKind::InteriorTable => {
                let window = self.window_from(offset.saturating_add(4))?;
                Ok(varint::decode_i64(window)
                    .map_err(|_| corrupt("a truncated rowid varint"))?
                    .0)
            }
            PageKind::LeafTable => {
                let window = self.window_from(offset)?;
                let payload =
                    varint::decode(window).map_err(|_| corrupt("a truncated payload length"))?;
                let rest = window
                    .get(payload.len..)
                    .ok_or_else(|| corrupt("a cell that ends inside its payload length"))?;
                Ok(varint::decode_i64(rest)
                    .map_err(|_| corrupt("a truncated rowid varint"))?
                    .0)
            }
            _ => Err(corrupt("a rowid was asked for on an index page")),
        }
    }

    /// Returns the left child of a cell on an interior page.
    pub fn cell_child(&self, index: usize) -> DbResult<PageId> {
        if self.layout.kind.is_leaf() {
            return Err(corrupt("a child pointer was asked for on a leaf page"));
        }
        let offset = *self
            .layout
            .cell_pointers
            .get(index)
            .ok_or_else(|| corrupt(format!("cell {index} does not exist on this page")))?;
        PageId::from_persisted(bytes::read_u32(self.bytes, offset)?)
            .map_err(|_| corrupt("an interior cell whose child is page zero"))
    }

    /// Returns the child pointer a slot selects, where the slot may be the
    /// cell count itself, meaning the right-most child.
    pub fn child_at(&self, slot: usize) -> DbResult<PageId> {
        if slot == self.layout.cell_count {
            return self
                .layout
                .right_child
                .ok_or_else(|| corrupt("a right-most child was asked for on a leaf page"));
        }
        self.cell_child(slot)
    }

    /// Returns the bytes from `offset` to the end of the usable area.
    fn window_from(&self, offset: usize) -> DbResult<&'a [u8]> {
        if offset > self.layout.usable {
            return Err(corrupt(format!(
                "an offset of {offset} in {} usable bytes",
                self.layout.usable
            )));
        }
        bytes::window(
            self.bytes,
            offset,
            self.layout.usable.saturating_sub(offset),
        )
    }

    /// Decodes one cell at a known-good offset.
    fn decode_cell(&self, offset: usize) -> DbResult<CellRef<'a>> {
        let window = self.window_from(offset)?;
        let mut cursor = 0usize;

        let left_child = if self.layout.kind.is_leaf() {
            None
        } else {
            let raw = bytes::read_u32(window, 0)?;
            cursor = 4;
            Some(
                PageId::from_persisted(raw)
                    .map_err(|_| corrupt("an interior cell whose child is page zero"))?,
            )
        };

        // A table interior cell is a child pointer and a rowid, and nothing
        // else: it carries no payload at all.
        if self.layout.kind == PageKind::InteriorTable {
            let rest = window
                .get(cursor..)
                .ok_or_else(|| corrupt("a table interior cell with no rowid"))?;
            let (rowid, width) =
                varint::decode_i64(rest).map_err(|_| corrupt("a truncated rowid varint"))?;
            return Ok(CellRef {
                offset,
                len: cursor.saturating_add(width),
                left_child,
                rowid: Some(rowid),
                split: PayloadSplit {
                    total: 0,
                    local: 0,
                    overflows: false,
                },
                local_payload: &[],
                local_offset: offset,
                overflow: None,
            });
        }

        let rest = window
            .get(cursor..)
            .ok_or_else(|| corrupt("a cell with no payload length"))?;
        let payload_len =
            varint::decode(rest).map_err(|_| corrupt("a truncated payload length varint"))?;
        cursor = cursor.saturating_add(payload_len.len);

        let rowid = if self.layout.kind == PageKind::LeafTable {
            let rest = window
                .get(cursor..)
                .ok_or_else(|| corrupt("a table leaf cell with no rowid"))?;
            let (rowid, width) =
                varint::decode_i64(rest).map_err(|_| corrupt("a truncated rowid varint"))?;
            cursor = cursor.saturating_add(width);
            Some(rowid)
        } else {
            None
        };

        let split = self.layout.window.split(payload_len.value)?;
        let local_offset = offset.saturating_add(cursor);
        let local = window
            .get(cursor..cursor.saturating_add(split.local))
            .ok_or_else(|| {
                corrupt(format!(
                    "a cell at {offset} claiming {} local bytes past the page",
                    split.local
                ))
            })?;
        cursor = cursor.saturating_add(split.local);

        let overflow = if split.overflows {
            let raw = bytes::read_u32(window, cursor)?;
            cursor = cursor.saturating_add(4);
            Some(
                PageId::from_persisted(raw)
                    .map_err(|_| corrupt("an overflow chain that starts at page zero"))?,
            )
        } else {
            None
        };

        Ok(CellRef {
            offset,
            len: cursor,
            left_child,
            rowid,
            split,
            local_payload: local,
            local_offset,
            overflow,
        })
    }

    /// Checks that every cell decodes and stays inside the page.
    fn validate_cells(&self) -> DbResult<()> {
        for index in 0..self.layout.cell_count {
            let cell = self.cell(index)?;
            let end = cell
                .offset
                .checked_add(cell.footprint())
                .ok_or_else(|| corrupt("a cell whose length overflows"))?;
            if end > self.layout.usable {
                return Err(corrupt(format!(
                    "cell {index} ends at {end} in {} usable bytes",
                    self.layout.usable
                )));
            }
        }
        Ok(())
    }

    /// Reports whether the cells on this page are laid out without overlapping
    /// each other or the freeblocks, and account for the whole content area.
    ///
    /// This is the expensive part of an integrity check and is deliberately
    /// separate from parsing: a traversal does not need it, because every cell
    /// it reads was already proved to be inside the page, and paying for it on
    /// every page read would make a scan quadratic in the cell count.
    pub fn check_layout(&self) -> DbResult<()> {
        let mut used: Vec<(usize, usize)> =
            Vec::with_capacity(self.layout.cell_count.saturating_add(8));
        for index in 0..self.layout.cell_count {
            let cell = self.cell(index)?;
            used.push((cell.offset, cell.offset.saturating_add(cell.footprint())));
        }
        for block in self.free_blocks()? {
            used.push((block.offset, block.offset.saturating_add(block.len)));
        }
        used.sort_unstable();
        let mut previous_end: Option<usize> = None;
        let mut covered = 0usize;
        for (start, end) in &used {
            if *start < self.layout.content_start {
                return Err(corrupt(format!(
                    "a cell or freeblock at {start} before the content area at {}",
                    self.layout.content_start
                )));
            }
            if let Some(previous) = previous_end {
                if *start < previous {
                    return Err(corrupt(format!(
                        "overlapping cell content at {start}, after something ending at {previous}"
                    )));
                }
            }
            previous_end = Some(*end);
            covered = covered.saturating_add(end.saturating_sub(*start));
        }
        let area = self.layout.usable.saturating_sub(self.layout.content_start);
        let accounted = covered.saturating_add(usize::from(self.layout.fragments));
        if accounted != area {
            return Err(corrupt(format!(
                "the content area holds {area} bytes but {accounted} are accounted for"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a page and returns both halves, for tests that read cells.
    fn parsed(raw: &[u8], page: u32, usable: u32) -> DbResult<PageLayout> {
        PageLayout::parse(raw, PageId::new(page).unwrap(), usable)
    }

    /// The payload thresholds are the file format's own worked numbers for a
    /// 4096-byte page with no reserved space.
    #[test]
    fn the_payload_thresholds_match_the_file_format() {
        let usable = 4096u32;
        // Table leaf: X = U - 35 = 4061.
        let split = payload_split(4061, usable, PageKind::LeafTable).unwrap();
        assert!(!split.overflows);
        assert_eq!(split.local, 4061);
        assert!(
            payload_split(4062, usable, PageKind::LeafTable)
                .unwrap()
                .overflows
        );

        // Index pages: X = ((U-12)*64/255)-23 = ((4084*64)/255)-23 = 1002.
        let index_max = ((4084u64 * 64) / 255) - 23;
        assert_eq!(index_max, 1002);
        for kind in [PageKind::LeafIndex, PageKind::InteriorIndex] {
            let split = payload_split(index_max, usable, kind).unwrap();
            assert!(!split.overflows, "{kind:?}");
            assert_eq!(split.local, index_max as usize);
            assert!(
                payload_split(index_max + 1, usable, kind)
                    .unwrap()
                    .overflows
            );
        }
    }

    /// The local payload must stay inside the format's window - at least the
    /// minimum and at most the maximum - for every payload length.
    #[test]
    fn the_local_payload_stays_between_the_minimum_and_the_maximum() {
        for usable in [512u32, 1024, 4096, 65_536] {
            let min_local = ((u64::from(usable) - 12) * 32 / 255) - 23;
            for kind in PageKind::all() {
                if kind == PageKind::InteriorTable {
                    continue;
                }
                let max_local = if kind == PageKind::LeafTable {
                    u64::from(usable) - 35
                } else {
                    ((u64::from(usable) - 12) * 64 / 255) - 23
                };
                for total in (0u64..200_000).step_by(37) {
                    let split = payload_split(total, usable, kind).unwrap();
                    if split.overflows {
                        assert!(
                            split.local as u64 >= min_local && split.local as u64 <= max_local,
                            "{kind:?} at usable {usable}, total {total}: local {}",
                            split.local
                        );
                        assert!((split.local as u64) < total);
                    } else {
                        assert_eq!(split.local as u64, total);
                        assert!(total <= max_local);
                    }
                }
            }
        }
    }

    /// Builds a leaf table page holding the given rows, so the codec can be
    /// tested against bytes this file laid out by hand.
    fn build_leaf_table_page(page_size: usize, rows: &[(i64, Vec<u8>)]) -> Vec<u8> {
        let mut page = vec![0u8; page_size];
        page[offsets::TYPE] = PageKind::LeafTable.type_byte();
        let mut content_start = page_size;
        let mut pointers = Vec::new();
        for (rowid, payload) in rows.iter().rev() {
            let mut cell = Vec::new();
            let mut buffer = [0u8; 9];
            let written = varint::encode(&mut buffer, payload.len() as u64).unwrap();
            cell.extend_from_slice(&buffer[..written]);
            let written = varint::encode_i64(&mut buffer, *rowid).unwrap();
            cell.extend_from_slice(&buffer[..written]);
            cell.extend_from_slice(payload);
            content_start -= cell.len();
            page[content_start..content_start + cell.len()].copy_from_slice(&cell);
            pointers.push(content_start as u16);
        }
        pointers.reverse();
        page[offsets::CELL_COUNT..offsets::CELL_COUNT + 2]
            .copy_from_slice(&(rows.len() as u16).to_be_bytes());
        page[offsets::CONTENT_START..offsets::CONTENT_START + 2]
            .copy_from_slice(&(content_start as u16).to_be_bytes());
        for (index, pointer) in pointers.iter().enumerate() {
            let at = 8 + index * 2;
            page[at..at + 2].copy_from_slice(&pointer.to_be_bytes());
        }
        page
    }

    /// A hand-built leaf page must decode into exactly the cells it holds.
    #[test]
    fn a_leaf_table_page_decodes_its_cells() {
        let rows = vec![
            (1i64, b"alpha".to_vec()),
            (7, b"bravo".to_vec()),
            (900, b"charlie".to_vec()),
        ];
        let raw = build_leaf_table_page(1024, &rows);
        let layout = parsed(&raw, 2, 1024).unwrap();
        let page = BTreePage::new(&raw, &layout);
        assert_eq!(page.kind(), PageKind::LeafTable);
        assert_eq!(page.cell_count(), 3);
        assert_eq!(page.right_child(), None);
        for (index, (rowid, payload)) in rows.iter().enumerate() {
            let cell = page.cell(index).unwrap();
            assert_eq!(cell.rowid, Some(*rowid));
            assert_eq!(cell.local_payload, payload.as_slice());
            assert!(cell.is_complete());
            assert_eq!(page.cell_rowid(index).unwrap(), *rowid);
        }
        assert!(page.cell(3).is_err());
        page.check_layout().unwrap();
    }

    /// Every structural lie about a page must be refused before any cell is
    /// read, and each one on its own.
    #[test]
    fn a_spoiled_page_is_refused() {
        let rows = vec![(1i64, b"alpha".to_vec()), (2, b"bravo".to_vec())];
        let good = build_leaf_table_page(1024, &rows);
        assert!(parsed(&good, 2, 1024).is_ok());

        // An unrecognised page type.
        let mut raw = good.clone();
        raw[0] = 0x03;
        assert!(parsed(&raw, 2, 1024).is_err());

        // A cell count that cannot fit its own pointer array.
        let mut raw = good.clone();
        raw[3..5].copy_from_slice(&600u16.to_be_bytes());
        assert!(parsed(&raw, 2, 1024).is_err());

        // A content area that starts inside the pointer array.
        let mut raw = good.clone();
        raw[5..7].copy_from_slice(&4u16.to_be_bytes());
        assert!(parsed(&raw, 2, 1024).is_err());

        // A content area past the end of the page.
        let mut raw = good.clone();
        raw[5..7].copy_from_slice(&2000u16.to_be_bytes());
        assert!(parsed(&raw, 2, 1024).is_err());

        // More fragmented bytes than the format allows.
        let mut raw = good.clone();
        raw[7] = 61;
        assert!(parsed(&raw, 2, 1024).is_err());

        // A cell pointer inside the header.
        let mut raw = good.clone();
        raw[8..10].copy_from_slice(&2u16.to_be_bytes());
        assert!(parsed(&raw, 2, 1024).is_err());

        // A cell pointer past the usable area.
        let mut raw = good.clone();
        raw[8..10].copy_from_slice(&1023u16.to_be_bytes());
        assert!(parsed(&raw, 2, 1024).is_err());

        // A cell claiming a payload longer than the page.
        let mut raw = good.clone();
        let pointer = u16::from_be_bytes([raw[8], raw[9]]) as usize;
        raw[pointer] = 0x7f;
        assert!(parsed(&raw, 2, 1024).is_err());
    }

    /// A freeblock chain that does not increase, is too short, runs past the
    /// page, or loops must all be refused - a writer walks this chain.
    #[test]
    fn a_bad_freeblock_chain_is_refused() {
        let rows = vec![(1i64, b"alpha".to_vec())];
        let good = build_leaf_table_page(1024, &rows);

        // A well-formed chain of two blocks.
        let mut raw = good.clone();
        raw[1..3].copy_from_slice(&500u16.to_be_bytes());
        raw[500..502].copy_from_slice(&600u16.to_be_bytes());
        raw[502..504].copy_from_slice(&8u16.to_be_bytes());
        raw[600..602].copy_from_slice(&0u16.to_be_bytes());
        raw[602..604].copy_from_slice(&8u16.to_be_bytes());
        let layout = parsed(&raw, 2, 1024).unwrap();
        assert_eq!(
            BTreePage::new(&raw, &layout).free_blocks().unwrap().len(),
            2
        );

        // The same chain, out of order.
        let mut raw = good.clone();
        raw[1..3].copy_from_slice(&600u16.to_be_bytes());
        raw[600..602].copy_from_slice(&500u16.to_be_bytes());
        raw[602..604].copy_from_slice(&8u16.to_be_bytes());
        raw[500..502].copy_from_slice(&0u16.to_be_bytes());
        raw[502..504].copy_from_slice(&8u16.to_be_bytes());
        assert!(parsed(&raw, 2, 1024).is_err());

        // A block shorter than the four bytes it needs for its own header.
        let mut raw = good.clone();
        raw[1..3].copy_from_slice(&500u16.to_be_bytes());
        raw[500..502].copy_from_slice(&0u16.to_be_bytes());
        raw[502..504].copy_from_slice(&3u16.to_be_bytes());
        assert!(parsed(&raw, 2, 1024).is_err());

        // A block that points at itself, which would loop forever.
        let mut raw = good.clone();
        raw[1..3].copy_from_slice(&500u16.to_be_bytes());
        raw[500..502].copy_from_slice(&500u16.to_be_bytes());
        raw[502..504].copy_from_slice(&8u16.to_be_bytes());
        assert!(parsed(&raw, 2, 1024).is_err());

        // A block running past the usable area.
        let mut raw = good.clone();
        raw[1..3].copy_from_slice(&1020u16.to_be_bytes());
        raw[1020..1022].copy_from_slice(&0u16.to_be_bytes());
        raw[1022..1024].copy_from_slice(&100u16.to_be_bytes());
        assert!(parsed(&raw, 2, 1024).is_err());
    }

    /// Page 1's B-tree header starts after the database header. The same
    /// bytes therefore mean different things depending on the page number,
    /// and reading page 1 at offset zero would find the magic string.
    #[test]
    fn page_one_starts_after_the_database_header() {
        let rows = vec![(1i64, b"alpha".to_vec()), (2, b"bravo".to_vec())];
        // A page-1 image: the database header, then the B-tree header at 100,
        // with every offset in whole-page coordinates as the format requires.
        let mut raw = build_leaf_table_page(1024, &rows);
        raw.copy_within(0..8, 100);
        let pointers: Vec<u8> = raw[8..12].to_vec();
        raw[108..112].copy_from_slice(&pointers);
        raw[..16].copy_from_slice(b"SQLite format 3 ");

        let layout = parsed(&raw, 1, 1024).unwrap();
        let page = BTreePage::new(&raw, &layout);
        assert_eq!(page.cell_count(), 2);
        assert_eq!(page.cell(0).unwrap().local_payload, b"alpha");

        // Read as any other page, byte zero is the magic's 'S', which is not
        // a page type at all.
        assert!(parsed(&raw, 2, 1024).is_err());
    }

    /// An interior table cell is a child pointer and a rowid and nothing else;
    /// decoding it as though it had a payload would read the next cell.
    #[test]
    fn an_interior_table_cell_has_no_payload() {
        let page_size = 1024usize;
        let mut raw = vec![0u8; page_size];
        raw[offsets::TYPE] = PageKind::InteriorTable.type_byte();
        raw[offsets::RIGHT_CHILD..offsets::RIGHT_CHILD + 4].copy_from_slice(&9u32.to_be_bytes());
        let mut cell = Vec::new();
        cell.extend_from_slice(&5u32.to_be_bytes());
        let mut buffer = [0u8; 9];
        let written = varint::encode_i64(&mut buffer, 4242).unwrap();
        cell.extend_from_slice(&buffer[..written]);
        let content_start = page_size - cell.len();
        raw[content_start..content_start + cell.len()].copy_from_slice(&cell);
        raw[offsets::CELL_COUNT..offsets::CELL_COUNT + 2].copy_from_slice(&1u16.to_be_bytes());
        raw[offsets::CONTENT_START..offsets::CONTENT_START + 2]
            .copy_from_slice(&(content_start as u16).to_be_bytes());
        raw[12..14].copy_from_slice(&(content_start as u16).to_be_bytes());

        let layout = parsed(&raw, 2, page_size as u32).unwrap();
        let page = BTreePage::new(&raw, &layout);
        assert_eq!(page.kind(), PageKind::InteriorTable);
        assert_eq!(page.right_child(), PageId::new(9));
        let cell = page.cell(0).unwrap();
        assert_eq!(cell.left_child, PageId::new(5));
        assert_eq!(cell.rowid, Some(4242));
        assert_eq!(cell.split.total, 0);
        assert!(cell.local_payload.is_empty());
        assert_eq!(page.child_at(0).unwrap(), PageId::new(5).unwrap());
        assert_eq!(page.child_at(1).unwrap(), PageId::new(9).unwrap());
        assert_eq!(page.cell_rowid(0).unwrap(), 4242);
    }

    /// An interior page whose right-most child is page zero is corrupt: page
    /// zero does not exist, and following it would read the header.
    #[test]
    fn an_interior_page_with_a_zero_child_is_refused() {
        let page_size = 1024usize;
        let mut raw = vec![0u8; page_size];
        raw[offsets::TYPE] = PageKind::InteriorTable.type_byte();
        raw[offsets::CONTENT_START..offsets::CONTENT_START + 2]
            .copy_from_slice(&(page_size as u16).to_be_bytes());
        assert!(parsed(&raw, 2, page_size as u32).is_err());
    }

    /// Parsing arbitrary bytes as a page must never panic, and every page it
    /// accepts must let every one of its cells be read.
    #[test]
    fn parsing_arbitrary_pages_never_panics() {
        let mut state = 0x5dee_ce66_dabc_1234u64;
        let mut accepted = 0usize;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut raw = vec![0u8; 512];
            for (index, byte) in raw.iter_mut().enumerate() {
                *byte = (state.rotate_left(index as u32 % 57)) as u8;
            }
            // Give the type byte a real chance of being valid.
            raw[0] = match state % 6 {
                0 => 0x02,
                1 => 0x05,
                2 => 0x0a,
                3 => 0x0d,
                other => other as u8,
            };
            if let Ok(layout) = parsed(&raw, 2, 512) {
                accepted = accepted.saturating_add(1);
                let page = BTreePage::new(&raw, &layout);
                for index in 0..page.cell_count() {
                    let _ = page.cell(index).unwrap();
                }
                let _ = page.free_blocks().unwrap();
                let _ = page.check_layout();
            }
        }
        assert!(accepted > 0, "the fuzz never produced a parseable page");
    }
}
