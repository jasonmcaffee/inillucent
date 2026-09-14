//! The swip: an eight-byte child reference that is either a page id or a
//! resident frame.
//!
//! Invariant: a swip written to disk is always a page id. Nothing else can be
//! true - a frame number means nothing to the next process to open the file -
//! and it is enforced in one place, `crate::pool::Pool::writeback`, which
//! translates every swizzled swip in the I/O copy and leaves the in-memory
//! frame alone. That is TDD invariant 6.
//!
//! ## Frame *index*, not frame pointer
//!
//! The TDD says "in memory it is either a frame pointer (low bit 0) or a page
//! id (low bit 1)". This implementation stores a frame **index** where the TDD
//! says pointer, and the difference is worth stating rather than glossing.
//!
//! A frame pointer saves one array index over a frame number; it costs
//! `unsafe`, because a `u64` reinterpreted as a `&[u8]` is not something safe
//! Rust can express, and the workspace forbids `unsafe` outside the OS
//! boundary. What swizzling is actually *for* - avoiding the page-table hash
//! lookup on every descent - is delivered identically by an index: the frame
//! array is allocated once at its full size and never resized, so
//! `frames[index]` is a bounds-checked add rather than a hash, and the address
//! is as stable as the reservation the TDD asks for.
//!
//! So this is the design's mechanism at the design's cost minus one
//! indirection, and the phase gate measures whether that indirection mattered.
//! It is not a stand-in for swizzling: the cooling FIFO, the parent
//! back-reference and the unswizzle-on-evict path are all real and are all
//! exercised by the 64-frame campaign.

use crate::PageId;

/// The low bit that says a swip holds a page id rather than a frame.
const UNSWIZZLED: u64 = 1;

/// A child reference in an interior page.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Default)]
pub struct Swip(u64);

impl Swip {
    /// The swip that means "no child".
    ///
    /// Page zero is the meta page and can never be a child, so the unswizzled
    /// encoding of page zero is free to mean absence.
    pub const NONE: Swip = Swip(UNSWIZZLED);

    /// Returns the swip that names a page not currently in a frame.
    ///
    /// @param page - the page id
    pub fn unswizzled(page: PageId) -> Swip {
        Swip((page.0 << 1) | UNSWIZZLED)
    }

    /// Returns the swip that names a resident frame.
    ///
    /// @param frame - the frame's index in the pool
    pub fn swizzled(frame: u32) -> Swip {
        Swip(u64::from(frame) << 1)
    }

    /// Returns the raw eight bytes, as the page holds them.
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Returns the swip a raw eight bytes encode.
    ///
    /// @param raw - the bytes read from a page
    pub fn from_raw(raw: u64) -> Swip {
        Swip(raw)
    }

    /// Reports whether this swip names a page id rather than a frame.
    pub fn is_unswizzled(self) -> bool {
        self.0 & UNSWIZZLED != 0
    }

    /// Reports whether this swip is the "no child" value.
    pub fn is_none(self) -> bool {
        self == Swip::NONE
    }

    /// Returns the page id, when the swip is unswizzled.
    pub fn page(self) -> Option<PageId> {
        if self.is_unswizzled() {
            Some(PageId(self.0 >> 1))
        } else {
            None
        }
    }

    /// Returns the frame index, when the swip is swizzled.
    pub fn frame(self) -> Option<u32> {
        if self.is_unswizzled() {
            None
        } else {
            u32::try_from(self.0 >> 1).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page id survives the encoding, and the encoding says which it is.
    #[test]
    fn a_page_id_round_trips() {
        for page in [1u64, 2, 3, 1023, 1 << 20, (1 << 40) - 1] {
            let swip = Swip::unswizzled(PageId(page));
            assert!(swip.is_unswizzled());
            assert_eq!(swip.page(), Some(PageId(page)));
            assert_eq!(swip.frame(), None);
            assert_eq!(Swip::from_raw(swip.raw()), swip);
        }
    }

    /// A frame index survives the encoding, and the encoding says which it is.
    #[test]
    fn a_frame_index_round_trips() {
        for frame in [0u32, 1, 63, 1024, u32::MAX] {
            let swip = Swip::swizzled(frame);
            assert!(!swip.is_unswizzled());
            assert_eq!(swip.frame(), Some(frame));
            assert_eq!(swip.page(), None);
            assert_eq!(Swip::from_raw(swip.raw()), swip);
        }
    }

    /// Frame zero and page zero are different swips, which is the whole reason
    /// the tag is the low bit rather than a zero check.
    #[test]
    fn frame_zero_is_not_no_child() {
        assert_ne!(Swip::swizzled(0), Swip::NONE);
        assert!(Swip::NONE.is_none());
        assert!(!Swip::swizzled(0).is_none());
        assert_eq!(Swip::NONE.page(), Some(PageId(0)));
    }

    /// The default swip is the encoding of page zero, so a zeroed page reads as
    /// "no child" rather than as frame zero.
    #[test]
    fn a_zeroed_swip_is_frame_zero_and_a_zeroed_page_says_so() {
        // A page of zero bytes decodes to `Swip(0)`, which is frame zero. That
        // is why an interior page's writer never leaves a slot zeroed: the
        // builder writes `Swip::NONE` explicitly. This test pins the fact that
        // the two differ so the builder's obligation cannot be forgotten.
        assert_eq!(Swip::from_raw(0), Swip::swizzled(0));
        assert_ne!(Swip::from_raw(0), Swip::NONE);
    }
}
