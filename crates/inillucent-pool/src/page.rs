//! The page: a fixed-size byte buffer with the 32-byte common header every
//! page kind shares.
//!
//! Invariant: nothing above this module reads or writes the first 32 bytes of a
//! page directly. The header carries the page LSN that makes redo idempotent
//! and the checksum that makes a torn write detectable, and both are properties
//! of the whole page rather than of any one kind, so both live here.
//!
//! In Phase 1 a page was a heap allocation owned by the tree. In Phase 2 it is
//! a frame in the buffer pool, at a stable index so a swizzled swip stays
//! valid. Nothing in this module cares which, because a page is only ever a
//! `&[u8]` or a `&mut [u8]` of the right length - which is why this module
//! moved down a layer, from `inillucent-tree` to `inillucent-pool`, without a line of
//! it changing. `inillucent_tree::page` re-exports it, so every existing caller
//! still names the same path.
//!
//! ## The checksum, and when it is computed
//!
//! [`header::CHECKSUM`] is a crc32c over bytes 12..page size, written on
//! writeback and verified on every read *from disk*. It is deliberately not
//! maintained while a page sits dirty in a frame: a page that is modified a
//! thousand times before it is written would pay a thousand full-page
//! checksums to protect against nothing, since a frame that is wrong in memory
//! is a bug rather than a media fault. [`checksum_page`] and
//! [`verify_checksum`] are the two ends of that contract.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

/// A page identifier in the new file format.
///
/// A `u64` rather than the old engine's `NonZeroU32`, because the new format
/// addresses pages by 64-bit index and page 0 is the meta page rather than an
/// impossible value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct PageId(pub u64);

impl PageId {
    /// The identifier that means "no page": the meta page can never be a child.
    pub const NONE: PageId = PageId(0);

    /// Reports whether this is the "no page" identifier.
    pub fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// The page sizes a database may be created with.
///
/// The default is fixed by measurement rather than by preference: 16, 32, and
/// 64 KiB were measured against `read.analytical`, and 32 KiB won.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct PageSize(u32);

impl PageSize {
    /// 32 KiB, chosen by the measurement in this type's own documentation.
    ///
    /// It used to say "until the Phase 1 sweep replaces it". There is no such
    /// sweep anywhere in the repository and there never was one after the
    /// measurement above; a comment may only claim what its test proves, and
    /// that clause claimed a piece of work that does not exist (task-1961, D4).
    pub const DEFAULT: PageSize = PageSize(32_768);

    /// Returns the page size for a byte count, if it is one of the four legal
    /// sizes.
    ///
    /// @param bytes - 8192, 16384, 32768 or 65536
    pub fn new(bytes: u32) -> DbResult<PageSize> {
        match bytes {
            8_192 | 16_384 | 32_768 | 65_536 => Ok(PageSize(bytes)),
            other => Err(misuse(format!(
                "page size {other} is not one of 8192, 16384, 32768, 65536"
            ))),
        }
    }

    /// Returns the size in bytes.
    pub fn get(self) -> u32 {
        self.0
    }

    /// Returns the size as a `usize`, which is what every slice index wants.
    ///
    /// **There is no `is_empty` beside it, and there will not be.** A page size
    /// is one of four constants, none of them zero, so the question the
    /// companion method answers has one answer and asking it would suggest
    /// there was a case where a page had no bytes.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(self) -> usize {
        self.0 as usize
    }

    /// Returns the four legal sizes, for the sweep and for exhaustive tests.
    pub fn all() -> [PageSize; 4] {
        [
            PageSize(8_192),
            PageSize(16_384),
            PageSize(32_768),
            PageSize(65_536),
        ]
    }
}

/// The size of the header every page carries.
pub const COMMON_HEADER: usize = 32;

/// Byte offsets inside the common header.
pub mod header {
    /// The LSN of the last modification, 8 bytes.
    pub const LSN: usize = 0;
    /// crc32c over the rest of the page, 4 bytes.
    pub const CHECKSUM: usize = 8;
    /// The page kind, 1 byte.
    pub const KIND: usize = 12;
    /// Kind-specific flags, 1 byte.
    pub const FLAGS: usize = 13;
    /// Tree level, 0 for leaves, 2 bytes.
    pub const LEVEL: usize = 14;
    /// The tree this page belongs to, 8 bytes.
    pub const TREE: usize = 16;
    /// The right sibling page id, 8 bytes, 0 for none.
    pub const RIGHT: usize = 24;
}

/// What a page holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageKind {
    /// An interior B+tree page.
    Interior,
    /// A PAX leaf.
    Leaf,
    /// One page of a blob extent.
    BlobExtent,
    /// One page holding several small out-of-line values side by side.
    BlobShared,
    /// One page of the free-page bitmap.
    FreeMap,
    /// Allocated but not in use.
    Unused,
}

impl PageKind {
    /// Returns the byte that encodes this kind.
    pub fn code(self) -> u8 {
        match self {
            PageKind::Interior => 1,
            PageKind::Leaf => 2,
            PageKind::BlobExtent => 3,
            PageKind::BlobShared => 6,
            PageKind::FreeMap => 4,
            PageKind::Unused => 5,
        }
    }

    /// Decodes a kind byte.
    ///
    /// @param code - the byte read from the page
    pub fn from_code(code: u8) -> DbResult<PageKind> {
        match code {
            1 => Ok(PageKind::Interior),
            2 => Ok(PageKind::Leaf),
            3 => Ok(PageKind::BlobExtent),
            6 => Ok(PageKind::BlobShared),
            4 => Ok(PageKind::FreeMap),
            5 => Ok(PageKind::Unused),
            other => Err(corrupt(format!("page kind {other} is not a kind"))),
        }
    }
}

/// Reads a little-endian `u16` at an offset, or says the page was short.
///
/// @param page - the page bytes
/// @param at - the byte offset
pub fn read_u16(page: &[u8], at: usize) -> DbResult<u16> {
    let slice = page
        .get(at..at.saturating_add(2))
        .ok_or_else(|| corrupt(format!("page ends before offset {at}")))?;
    let mut raw = [0u8; 2];
    raw.copy_from_slice(slice);
    Ok(u16::from_le_bytes(raw))
}

/// Reads a little-endian `u32` at an offset, or says the page was short.
///
/// @param page - the page bytes
/// @param at - the byte offset
pub fn read_u32(page: &[u8], at: usize) -> DbResult<u32> {
    let slice = page
        .get(at..at.saturating_add(4))
        .ok_or_else(|| corrupt(format!("page ends before offset {at}")))?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(slice);
    Ok(u32::from_le_bytes(raw))
}

/// Reads a little-endian `u64` at an offset, or says the page was short.
///
/// @param page - the page bytes
/// @param at - the byte offset
pub fn read_u64(page: &[u8], at: usize) -> DbResult<u64> {
    let slice = page
        .get(at..at.saturating_add(8))
        .ok_or_else(|| corrupt(format!("page ends before offset {at}")))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(slice);
    Ok(u64::from_le_bytes(raw))
}

/// Writes a little-endian `u16` at an offset, or says the page was short.
///
/// @param page - the page bytes
/// @param at - the byte offset
/// @param value - what to write
pub fn write_u16(page: &mut [u8], at: usize, value: u16) -> DbResult<()> {
    let slice = page
        .get_mut(at..at.saturating_add(2))
        .ok_or_else(|| corrupt(format!("page ends before offset {at}")))?;
    slice.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Writes a little-endian `u32` at an offset, or says the page was short.
///
/// @param page - the page bytes
/// @param at - the byte offset
/// @param value - what to write
pub fn write_u32(page: &mut [u8], at: usize, value: u32) -> DbResult<()> {
    let slice = page
        .get_mut(at..at.saturating_add(4))
        .ok_or_else(|| corrupt(format!("page ends before offset {at}")))?;
    slice.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Writes a little-endian `u64` at an offset, or says the page was short.
///
/// @param page - the page bytes
/// @param at - the byte offset
/// @param value - what to write
pub fn write_u64(page: &mut [u8], at: usize, value: u64) -> DbResult<()> {
    let slice = page
        .get_mut(at..at.saturating_add(8))
        .ok_or_else(|| corrupt(format!("page ends before offset {at}")))?;
    slice.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Returns the kind byte a page declares.
///
/// @param page - the page bytes
pub fn kind_of(page: &[u8]) -> DbResult<PageKind> {
    let byte = page
        .get(header::KIND)
        .copied()
        .ok_or_else(|| corrupt("page is too short to have a kind"))?;
    PageKind::from_code(byte)
}

/// Returns the flag byte a page declares.
///
/// @param page - the page bytes
pub fn flags_of(page: &[u8]) -> DbResult<u8> {
    page.get(header::FLAGS)
        .copied()
        .ok_or_else(|| corrupt("page is too short to have flags"))
}

/// Writes the common header of a fresh page.
///
/// @param page - the page bytes, already sized
/// @param kind - what the page holds
/// @param level - the tree level, 0 for a leaf
/// @param tree - the tree the page belongs to
pub fn write_common(page: &mut [u8], kind: PageKind, level: u16, tree: u64) -> DbResult<()> {
    if page.len() < COMMON_HEADER {
        return Err(corrupt("page is shorter than the common header"));
    }
    write_u64(page, header::LSN, 0)?;
    write_u32(page, header::CHECKSUM, 0)?;
    let kind_slot = page
        .get_mut(header::KIND)
        .ok_or_else(|| corrupt("page has no kind byte"))?;
    *kind_slot = kind.code();
    let flags_slot = page
        .get_mut(header::FLAGS)
        .ok_or_else(|| corrupt("page has no flag byte"))?;
    *flags_slot = 0;
    write_u16(page, header::LEVEL, level)?;
    write_u64(page, header::TREE, tree)?;
    write_u64(page, header::RIGHT, PageId::NONE.0)?;
    Ok(())
}

/// Returns the crc32c a page's header should carry.
///
/// Computed over bytes 12..page size: everything except the LSN, which redo
/// rewrites, and the checksum field itself. A page whose LSN alone changed
/// still has to be re-checksummed on writeback, and [`checksum_page`] is
/// called there rather than being inferred from what changed.
///
/// @param page - the page bytes
pub fn page_checksum(page: &[u8]) -> DbResult<u32> {
    let body = page
        .get(header::KIND..)
        .ok_or_else(|| corrupt("page is too short to checksum"))?;
    Ok(inillucent_base::checksum::crc32(body))
}

/// Writes the checksum a page should carry into its header.
///
/// @param page - the page bytes
pub fn checksum_page(page: &mut [u8]) -> DbResult<()> {
    let sum = page_checksum(page)?;
    write_u32(page, header::CHECKSUM, sum)
}

/// Verifies a page read from disk, or says it is corrupt.
///
/// @param page - the page bytes
/// @param id - the page id, for the error message
pub fn verify_checksum(page: &[u8], id: PageId) -> DbResult<()> {
    let stored = read_u32(page, header::CHECKSUM)?;
    let computed = page_checksum(page)?;
    if stored == computed {
        return Ok(());
    }
    Err(corrupt(format!(
        "page {} checksum {stored:08x} is not the computed {computed:08x}",
        id.0
    )))
}

/// Returns the page LSN.
///
/// @param page - the page bytes
pub fn lsn_of(page: &[u8]) -> DbResult<u64> {
    read_u64(page, header::LSN)
}

/// Writes the page LSN.
///
/// @param page - the page bytes
/// @param lsn - the log sequence number of the modification
pub fn set_lsn(page: &mut [u8], lsn: u64) -> DbResult<()> {
    write_u64(page, header::LSN, lsn)
}

/// Returns the tree identifier a page declares.
///
/// @param page - the page bytes
pub fn tree_of(page: &[u8]) -> DbResult<u64> {
    read_u64(page, header::TREE)
}

/// Returns the right-sibling page id a page declares.
///
/// @param page - the page bytes
pub fn right_of(page: &[u8]) -> DbResult<PageId> {
    Ok(PageId(read_u64(page, header::RIGHT)?))
}

/// Writes the right-sibling page id.
///
/// @param page - the page bytes
/// @param right - the sibling, or [`PageId::NONE`]
pub fn set_right(page: &mut [u8], right: PageId) -> DbResult<()> {
    write_u64(page, header::RIGHT, right.0)
}

/// Returns the tree level a page declares; leaves are level zero.
///
/// @param page - the page bytes
pub fn level_of(page: &[u8]) -> DbResult<u16> {
    read_u16(page, header::LEVEL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the four declared page sizes are accepted.
    #[test]
    fn page_sizes_are_the_declared_four() {
        for size in PageSize::all() {
            assert_eq!(PageSize::new(size.get()).unwrap(), size);
        }
        for bad in [0u32, 1, 512, 4096, 24_576, 131_072, u32::MAX] {
            assert!(PageSize::new(bad).is_err(), "{bad}");
        }
    }

    /// Every page kind round-trips, and nothing outside the set decodes.
    #[test]
    fn page_kinds_round_trip() {
        let kinds = [
            PageKind::Interior,
            PageKind::Leaf,
            PageKind::BlobExtent,
            PageKind::FreeMap,
            PageKind::Unused,
            PageKind::BlobShared,
        ];
        for kind in kinds {
            assert_eq!(PageKind::from_code(kind.code()).unwrap(), kind);
        }
        for code in [0u8, 7, 200, 255] {
            assert!(PageKind::from_code(code).is_err(), "{code}");
        }
    }

    /// A fresh header reads back as what was written, and a short page is
    /// refused rather than indexed into.
    #[test]
    fn common_header_round_trips() {
        let mut page = vec![0u8; 8192];
        write_common(&mut page, PageKind::Leaf, 0, 42).unwrap();
        assert_eq!(kind_of(&page).unwrap(), PageKind::Leaf);
        assert_eq!(flags_of(&page).unwrap(), 0);
        assert_eq!(read_u16(&page, header::LEVEL).unwrap(), 0);
        assert_eq!(read_u64(&page, header::TREE).unwrap(), 42);
        assert_eq!(read_u64(&page, header::RIGHT).unwrap(), 0);

        let mut short = vec![0u8; COMMON_HEADER - 1];
        assert!(write_common(&mut short, PageKind::Leaf, 0, 1).is_err());
        assert!(kind_of(&[]).is_err());
        assert!(flags_of(&[]).is_err());
    }

    /// Every reader refuses an offset that runs past the end rather than
    /// panicking, at every offset in the last word of a buffer.
    #[test]
    fn readers_refuse_offsets_past_the_end() {
        let page = vec![0u8; 16];
        for at in 15..24 {
            assert!(read_u16(&page, at).is_err(), "u16 at {at}");
        }
        for at in 13..24 {
            assert!(read_u32(&page, at).is_err(), "u32 at {at}");
        }
        for at in 9..24 {
            assert!(read_u64(&page, at).is_err(), "u64 at {at}");
        }
        let mut page = vec![0u8; 16];
        assert!(write_u16(&mut page, 15, 0).is_err());
        assert!(write_u32(&mut page, 13, 0).is_err());
        assert!(write_u64(&mut page, 9, 0).is_err());
    }

    /// A checksum written over a page verifies, and any flipped byte after
    /// the LSN is caught. The LSN itself is deliberately outside the cover.
    #[test]
    fn a_page_checksum_covers_everything_but_the_lsn() {
        let mut page = vec![7u8; 512];
        write_common(&mut page, PageKind::Interior, 1, 5).unwrap();
        checksum_page(&mut page).unwrap();
        verify_checksum(&page, PageId(3)).unwrap();

        set_lsn(&mut page, 99).unwrap();
        verify_checksum(&page, PageId(3)).expect("the LSN is not covered");

        for index in [header::KIND, header::FLAGS, header::TREE, 200, 511] {
            let mut damaged = page.clone();
            damaged[index] ^= 0xFF;
            assert!(
                verify_checksum(&damaged, PageId(3)).is_err(),
                "byte {index} went unnoticed"
            );
        }
        assert!(page_checksum(&[]).is_err());
        assert!(verify_checksum(&[], PageId(0)).is_err());
    }

    /// The header accessors read back what the writers wrote.
    #[test]
    fn the_header_accessors_round_trip() {
        let mut page = vec![0u8; 256];
        write_common(&mut page, PageKind::Leaf, 0, 17).unwrap();
        assert_eq!(tree_of(&page).unwrap(), 17);
        assert_eq!(level_of(&page).unwrap(), 0);
        assert_eq!(right_of(&page).unwrap(), PageId::NONE);
        set_right(&mut page, PageId(42)).unwrap();
        assert_eq!(right_of(&page).unwrap(), PageId(42));
        set_lsn(&mut page, 12_345).unwrap();
        assert_eq!(lsn_of(&page).unwrap(), 12_345);
        assert!(lsn_of(&[]).is_err());
        assert!(tree_of(&[]).is_err());
        assert!(right_of(&[]).is_err());
        assert!(level_of(&[]).is_err());
        assert!(set_lsn(&mut [], 1).is_err());
        assert!(set_right(&mut [], PageId(1)).is_err());
    }

    /// `PageId::NONE` is the only identifier that is "no page".
    #[test]
    fn page_id_none_is_zero() {
        assert!(PageId::NONE.is_none());
        assert!(!PageId(1).is_none());
    }
}
