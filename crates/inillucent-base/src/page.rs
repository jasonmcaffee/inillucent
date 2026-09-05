//! Page-size, offset, and page-count arithmetic.
//!
//! Invariant: a byte offset into a database file is only ever produced by this
//! module, from a validated page size and a validated page number, using
//! checked arithmetic. Nothing multiplies a page number by a page size at a
//! call site.
//!
//! The rules come from the SQLite file format: the page size is a power of two
//! between 512 and 65536 stored in a 16-bit header field, where the value 1
//! means 65536 because 65536 does not fit; the reserved region at the end of
//! each page shrinks the usable size; and the usable size may not fall below
//! 480 bytes or the B-tree cell layout stops working.

use crate::error::{corrupt, too_big, DbResult};
use crate::ids::PageId;

/// The smallest page size the format allows.
pub const MIN_PAGE_SIZE: u32 = 512;

/// The largest page size the format allows.
pub const MAX_PAGE_SIZE: u32 = 65_536;

/// The smallest usable page size the B-tree layout tolerates.
pub const MIN_USABLE_SIZE: u32 = 480;

/// The size of the database header, which lives at the front of page 1.
pub const HEADER_SIZE: u32 = 100;

/// The encoded value the header uses to mean a 65536-byte page.
pub const ENCODED_MAX_PAGE_SIZE: u16 = 1;

/// A validated database page size.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PageSize(u32);

impl PageSize {
    /// The default page size a new database is created with.
    pub const DEFAULT: PageSize = PageSize(4096);

    /// Validates a page size in bytes.
    pub fn new(bytes: u32) -> DbResult<PageSize> {
        if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&bytes) {
            return Err(corrupt("page size is outside the 512..65536 range"));
        }
        if !bytes.is_power_of_two() {
            return Err(corrupt("page size is not a power of two"));
        }
        Ok(PageSize(bytes))
    }

    /// Decodes the 16-bit header field, where 1 means 65536.
    pub fn from_encoded(encoded: u16) -> DbResult<PageSize> {
        match encoded {
            ENCODED_MAX_PAGE_SIZE => Ok(PageSize(MAX_PAGE_SIZE)),
            other => PageSize::new(u32::from(other)),
        }
    }

    /// Encodes the page size for the 16-bit header field.
    pub fn to_encoded(self) -> u16 {
        if self.0 == MAX_PAGE_SIZE {
            return ENCODED_MAX_PAGE_SIZE;
        }
        self.0 as u16
    }

    /// Returns the page size in bytes.
    pub fn bytes(self) -> u32 {
        self.0
    }

    /// Returns the page size as a `usize` for buffer sizing.
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Returns the usable size once `reserved` trailing bytes are excluded.
    pub fn usable(self, reserved: u8) -> DbResult<u32> {
        let usable = self
            .0
            .checked_sub(u32::from(reserved))
            .ok_or_else(|| corrupt("reserved region is larger than the page"))?;
        if usable < MIN_USABLE_SIZE {
            return Err(corrupt("usable page size is below the 480-byte minimum"));
        }
        Ok(usable)
    }
}

/// Returns the byte offset of `page` in a file of this page size.
///
/// Page numbers are one-based, so page 1 starts at offset 0.
pub fn page_offset(size: PageSize, page: PageId) -> DbResult<u64> {
    u64::from(page.index())
        .checked_mul(u64::from(size.bytes()))
        .ok_or_else(|| too_big("page offset does not fit in a 64-bit file offset"))
}

/// Returns the byte offset of `offset` bytes into `page`.
pub fn offset_within_page(size: PageSize, page: PageId, offset: u32) -> DbResult<u64> {
    if offset >= size.bytes() {
        return Err(corrupt("offset reaches past the end of the page"));
    }
    page_offset(size, page)?
        .checked_add(u64::from(offset))
        .ok_or_else(|| too_big("offset within page does not fit in a file offset"))
}

/// Returns how many whole pages a file of `bytes` bytes contains.
///
/// A trailing partial page is not a page: the pager treats the file as ending
/// at the last whole page and recovery decides what to do with the remainder.
pub fn page_count(size: PageSize, bytes: u64) -> u32 {
    // The divisor is a validated `PageSize` and therefore never zero, but the
    // checked form says so to the compiler as well as to the reader.
    let pages = bytes.checked_div(u64::from(size.bytes())).unwrap_or(0);
    pages.min(u64::from(u32::MAX)) as u32
}

/// Returns the file size a database of `pages` pages occupies.
pub fn file_size(size: PageSize, pages: u32) -> DbResult<u64> {
    u64::from(pages)
        .checked_mul(u64::from(size.bytes()))
        .ok_or_else(|| too_big("database size does not fit in a 64-bit file offset"))
}

/// Reports whether a byte length ends exactly on a page boundary.
pub fn is_page_aligned(size: PageSize, bytes: u64) -> bool {
    bytes.is_multiple_of(u64::from(size.bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::PrimaryCode;

    /// Every legal page size is accepted and every illegal one is refused.
    #[test]
    fn page_sizes_follow_the_file_format_rules() {
        for shift in 9..=16 {
            let bytes = 1u32 << shift;
            assert_eq!(PageSize::new(bytes).unwrap().bytes(), bytes);
        }
        for bytes in [0, 1, 256, 511, 513, 1000, 65_535, 131_072] {
            assert_eq!(
                PageSize::new(bytes).unwrap_err().code(),
                PrimaryCode::Corrupt
            );
        }
    }

    /// The header field encodes 65536 as 1; both directions must agree, or a
    /// database created with the largest page size cannot be reopened.
    #[test]
    fn the_largest_page_size_round_trips_through_its_encoded_form() {
        let largest = PageSize::new(MAX_PAGE_SIZE).unwrap();
        assert_eq!(largest.to_encoded(), 1);
        assert_eq!(PageSize::from_encoded(1).unwrap(), largest);
        let ordinary = PageSize::new(4096).unwrap();
        assert_eq!(ordinary.to_encoded(), 4096);
        assert_eq!(PageSize::from_encoded(4096).unwrap(), ordinary);
        assert_eq!(
            PageSize::from_encoded(0).unwrap_err().code(),
            PrimaryCode::Corrupt
        );
    }

    /// The usable size shrinks with the reserved region and has a floor.
    #[test]
    fn usable_size_respects_the_reserved_region_and_its_floor() {
        let small = PageSize::new(512).unwrap();
        assert_eq!(small.usable(0).unwrap(), 512);
        assert_eq!(small.usable(32).unwrap(), 480);
        assert_eq!(small.usable(33).unwrap_err().code(), PrimaryCode::Corrupt);
        let large = PageSize::new(65_536).unwrap();
        assert_eq!(large.usable(255).unwrap(), 65_281);
    }

    /// Page 1 starts at zero, and offsets scale with the page number.
    #[test]
    fn page_offsets_are_one_based() {
        let size = PageSize::new(4096).unwrap();
        assert_eq!(page_offset(size, PageId::FIRST).unwrap(), 0);
        assert_eq!(page_offset(size, PageId::new(2).unwrap()).unwrap(), 4096);
        assert_eq!(
            offset_within_page(size, PageId::new(3).unwrap(), 100).unwrap(),
            8292
        );
        assert_eq!(
            offset_within_page(size, PageId::FIRST, 4096)
                .unwrap_err()
                .code(),
            PrimaryCode::Corrupt
        );
    }

    /// The largest page at the largest page size must not overflow, and the
    /// result must be the value a 64-bit file offset really holds.
    #[test]
    fn the_largest_addressable_page_does_not_overflow() {
        let size = PageSize::new(MAX_PAGE_SIZE).unwrap();
        let last = PageId::new(u32::MAX).unwrap();
        let expected = u64::from(u32::MAX - 1) * u64::from(MAX_PAGE_SIZE);
        assert_eq!(page_offset(size, last).unwrap(), expected);
        assert_eq!(
            file_size(size, u32::MAX).unwrap(),
            u64::from(u32::MAX) * 65_536
        );
    }

    /// A trailing partial page is not counted as a page.
    #[test]
    fn page_count_ignores_a_trailing_partial_page() {
        let size = PageSize::new(1024).unwrap();
        assert_eq!(page_count(size, 0), 0);
        assert_eq!(page_count(size, 1023), 0);
        assert_eq!(page_count(size, 1024), 1);
        assert_eq!(page_count(size, 2047), 1);
        assert!(is_page_aligned(size, 2048));
        assert!(!is_page_aligned(size, 2049));
    }
}
