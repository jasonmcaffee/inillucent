//! Newtype identifiers.
//!
//! Invariant: an identifier is constructed only from a value that has already
//! been range-checked, so a page number read off disk cannot be used as a page
//! number until it has been proved to be one.
//!
//! The engine has a dozen different one-based and zero-based counters. Passing
//! them as bare integers is the single easiest way to write a bug that a type
//! checker would have caught, so every one of them is a newtype and none of
//! them convert into each other implicitly.

use core::fmt;
use core::num::NonZeroU32;

use crate::error::{corrupt, DbResult};

/// The index of an attached database within a connection: 0 is `main`, 1 is
/// `temp`, and 2 and above are `ATTACH`ed files.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DatabaseId(pub u32);

/// A one-based page number in a database, journal, or WAL file.
///
/// Page zero exists in the file format only as a null pointer, which is why the
/// inner value is a `NonZeroU32`: a null pointer cannot be mistaken for a page.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PageId(NonZeroU32);

impl PageId {
    /// The first page of a database file, which holds the 100-byte header.
    pub const FIRST: PageId = PageId(NonZeroU32::MIN);

    /// Builds a page number from a one-based value, rejecting zero.
    pub fn new(value: u32) -> Option<PageId> {
        NonZeroU32::new(value).map(PageId)
    }

    /// Builds a page number from bytes read off disk, mapping zero to
    /// `SQLITE_CORRUPT` rather than to a panic or a silent default.
    pub fn from_persisted(value: u32) -> DbResult<PageId> {
        PageId::new(value).ok_or_else(|| corrupt("page number zero where a page was required"))
    }

    /// Returns the one-based page number.
    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// Returns the zero-based index of this page, used for offset arithmetic.
    pub fn index(self) -> u32 {
        self.0.get().saturating_sub(1)
    }

    /// Returns the next page number, or `None` at the end of the page space.
    pub fn next(self) -> Option<PageId> {
        self.0.get().checked_add(1).and_then(PageId::new)
    }
}

impl fmt::Display for PageId {
    /// Writes the one-based page number.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0.get())
    }
}

/// The page a table or index B-tree is rooted at.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RootPageId(pub PageId);

impl RootPageId {
    /// Returns the underlying page number.
    pub fn page(self) -> PageId {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::PrimaryCode;

    /// Page zero is the format's null pointer and must never become a `PageId`.
    #[test]
    fn page_zero_is_rejected() {
        assert_eq!(PageId::new(0), None);
        let error = PageId::from_persisted(0).expect_err("zero must not become a page");
        assert_eq!(error.code(), PrimaryCode::Corrupt);
    }

    /// Page numbers are one-based and their index is zero-based; getting this
    /// wrong is an off-by-one in every offset the pager computes.
    #[test]
    fn page_numbering_is_one_based() {
        let first = PageId::from_persisted(1).expect("page one is valid");
        assert_eq!(first, PageId::FIRST);
        assert_eq!(first.get(), 1);
        assert_eq!(first.index(), 0);
    }

    /// The page space ends rather than wrapping.
    #[test]
    fn page_numbers_do_not_wrap_at_the_top_of_the_space() {
        let last = PageId::new(u32::MAX).expect("u32::MAX is a valid page number");
        assert_eq!(last.next(), None);
    }
}
