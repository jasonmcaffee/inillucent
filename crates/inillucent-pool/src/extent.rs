//! Blob extents: values too large for a leaf, stored in runs of whole pages.
//!
//! Invariant: an extent reference in a leaf is exactly sixteen bytes -
//! `(u64 first page, u64 total length)` - and the pages it names hold that many
//! payload bytes and no more. A reader never trusts the length in the reference
//! against the lengths in the pages: it checks them, because the two disagreeing
//! is a corruption and reading the longer of the two would run off the end of an
//! extent chain.
//!
//! ## Why runs rather than a chain of single pages
//!
//! SQLite's overflow chain is one page per link and one four-byte pointer per
//! page, so reading a 1 MB value at 4 KiB pages is 256 dependent reads. The
//! allocator here asks the free map for a **contiguous run** big enough for the
//! whole value and only chains when the file has no run that long, so the common
//! case is one seek and one sequential read. That is the whole of the
//! `large.values` argument in the TDD, and it is why [`crate::freemap::FreeMap`]
//! is a bitmap.
//!
//! ## Layout
//!
//! Each extent page carries the 32-byte common header, then:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 32 | 8 | the next run's first page, or 0 |
//! | 40 | 4 | how many payload bytes this page holds |
//! | 44 | 4 | reserved, zero |
//! | 48 | .. | payload |

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

use crate::page::{self, PageKind};
use crate::PageId;

/// Byte offsets inside an extent page's own header.
mod at {
    /// The first page of the next run, 8 bytes.
    pub const NEXT: usize = 32;
    /// How many payload bytes this page holds, 4 bytes.
    pub const PAYLOAD: usize = 40;
    /// Where the payload begins; bytes 44..48 are reserved and zero.
    pub const BODY: usize = 48;
}

/// How many payload bytes one extent page holds.
///
/// @param page_size - the database's page size in bytes
pub fn payload_capacity(page_size: usize) -> usize {
    page_size.saturating_sub(at::BODY)
}

/// A value's out-of-line reference, as a leaf stores it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtentRef {
    /// The first page of the value.
    pub first: PageId,
    /// How many bytes the value holds in total.
    pub length: u64,
}

/// How many bytes an extent reference occupies inside a leaf.
pub const EXTENT_REF_BYTES: usize = 16;

impl ExtentRef {
    /// Returns the sixteen bytes a leaf stores.
    pub fn encode(&self) -> [u8; EXTENT_REF_BYTES] {
        let mut raw = [0u8; EXTENT_REF_BYTES];
        // `split_at_mut` rather than two `get_mut` ranges: the ranges are
        // statically inside a fixed-size array, so their `None` arms were
        // branches no input could take, and a branch no input can take is one
        // the coverage gate can only ever be lied to about.
        let (first, length) = raw.split_at_mut(8);
        first.copy_from_slice(&self.first.0.to_le_bytes());
        length.copy_from_slice(&self.length.to_le_bytes());
        raw
    }

    /// Reads a reference out of a leaf's heap.
    ///
    /// @param raw - the sixteen bytes
    pub fn decode(raw: &[u8]) -> DbResult<ExtentRef> {
        if raw.len() < EXTENT_REF_BYTES {
            return Err(corrupt("an extent reference is shorter than sixteen bytes"));
        }
        let mut first = [0u8; 8];
        let mut length = [0u8; 8];
        first.copy_from_slice(raw.get(0..8).unwrap_or(&[0; 8]));
        length.copy_from_slice(raw.get(8..16).unwrap_or(&[0; 8]));
        let first = PageId(u64::from_le_bytes(first));
        if first.is_none() {
            return Err(corrupt("an extent reference names page zero"));
        }
        Ok(ExtentRef {
            first,
            length: u64::from_le_bytes(length),
        })
    }
}

/// Returns how many pages a value of this length needs.
///
/// @param length - the value's length in bytes
/// @param page_size - the database's page size in bytes
pub fn pages_needed(length: u64, page_size: usize) -> u64 {
    let capacity = payload_capacity(page_size).max(1) as u64;
    length.saturating_add(capacity.saturating_sub(1)) / capacity
}

/// Writes a value into a run of page images.
///
/// Returns the images in page order, ready for the caller to install. The
/// caller has already allocated the run, which is why the first page id is an
/// argument rather than something this function decides.
///
/// @param value - the bytes to store
/// @param first - the first page of the run
/// @param page_size - the database's page size in bytes
/// @param tree - the tree the value belongs to
pub fn encode_run(
    value: &[u8],
    first: PageId,
    page_size: usize,
    tree: u64,
) -> DbResult<Vec<(PageId, Vec<u8>)>> {
    if first.is_none() {
        return Err(misuse("an extent cannot start at page zero"));
    }
    let capacity = payload_capacity(page_size);
    if capacity == 0 {
        return Err(misuse("the page size leaves no room for an extent payload"));
    }
    let mut images = Vec::new();
    let mut offset = 0usize;
    let mut index = 0u64;
    // A zero-length value still gets one page, so the loop runs at least once
    // and the `index == 0` disjunct is only ever read on that first pass.
    loop {
        let id = PageId(first.0.saturating_add(index));
        let take = capacity.min(value.len().saturating_sub(offset));
        let mut image = vec![0u8; page_size];
        page::write_common(&mut image, PageKind::BlobExtent, 0, tree)?;
        page::write_u32(&mut image, at::PAYLOAD, u32::try_from(take).unwrap_or(0))?;
        let body = image
            .get_mut(at::BODY..at::BODY.saturating_add(take))
            .ok_or_else(|| misuse("the extent payload does not fit its page"))?;
        body.copy_from_slice(
            value
                .get(offset..offset.saturating_add(take))
                .unwrap_or(&[]),
        );
        offset = offset.saturating_add(take);
        index = index.saturating_add(1);
        let more = offset < value.len();
        if more {
            page::write_u64(&mut image, at::NEXT, first.0.saturating_add(index))?;
        }
        images.push((id, image));
        if !more {
            break;
        }
    }
    Ok(images)
}

/// Returns one extent page's payload and the page after it.
///
/// @param image - the page bytes
pub fn read_page(image: &[u8]) -> DbResult<(&[u8], PageId)> {
    if page::kind_of(image)? != PageKind::BlobExtent {
        return Err(corrupt("page is not a blob extent"));
    }
    let length = page::read_u32(image, at::PAYLOAD)? as usize;
    if length > payload_capacity(image.len()) {
        return Err(corrupt(format!(
            "an extent page claims {length} payload bytes, more than it holds"
        )));
    }
    let body = image
        .get(at::BODY..at::BODY.saturating_add(length))
        .ok_or_else(|| corrupt("an extent payload runs past its page"))?;
    Ok((body, PageId(page::read_u64(image, at::NEXT)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assembles a value from a run of images the way a reader would.
    fn assemble(images: &[(PageId, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (_, image) in images {
            let (body, _) = read_page(image).unwrap();
            out.extend_from_slice(body);
        }
        out
    }

    /// A value shorter than one page occupies one page and reads back.
    #[test]
    fn a_short_value_is_one_page() {
        let value = b"a small out-of-line value".to_vec();
        let images = encode_run(&value, PageId(9), 512, 3).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].0, PageId(9));
        assert_eq!(assemble(&images), value);
        let (_, next) = read_page(&images[0].1).unwrap();
        assert!(next.is_none());
    }

    /// A value spanning several pages reads back byte for byte, and every page
    /// but the last names the one after it.
    #[test]
    fn a_long_value_spans_a_contiguous_run() {
        let value: Vec<u8> = (0..5_000u32).map(|n| (n % 251) as u8).collect();
        let images = encode_run(&value, PageId(20), 512, 3).unwrap();
        assert_eq!(images.len() as u64, pages_needed(value.len() as u64, 512));
        for (index, (id, image)) in images.iter().enumerate() {
            assert_eq!(*id, PageId(20 + index as u64));
            let (_, next) = read_page(image).unwrap();
            if index + 1 == images.len() {
                assert!(next.is_none());
            } else {
                assert_eq!(next, PageId(20 + index as u64 + 1));
            }
        }
        assert_eq!(assemble(&images), value);
    }

    /// An empty value still occupies one page, so a reference never names
    /// nothing.
    #[test]
    fn an_empty_value_still_has_a_page() {
        let images = encode_run(&[], PageId(4), 512, 1).unwrap();
        assert_eq!(images.len(), 1);
        assert!(assemble(&images).is_empty());
    }

    /// A reference round-trips and refuses page zero.
    #[test]
    fn a_reference_round_trips() {
        let reference = ExtentRef {
            first: PageId(77),
            length: 123_456,
        };
        let raw = reference.encode();
        assert_eq!(ExtentRef::decode(&raw).unwrap(), reference);
        assert!(ExtentRef::decode(&raw[..15]).is_err());
        assert!(ExtentRef::decode(&[0u8; 16]).is_err());
        assert_eq!(EXTENT_REF_BYTES, 16);
    }

    /// The page arithmetic matches what the encoder produces at the exact
    /// boundary, one below it and one above it.
    #[test]
    fn the_page_count_is_exact_at_the_boundary() {
        let capacity = payload_capacity(512) as u64;
        assert_eq!(pages_needed(0, 512), 0);
        assert_eq!(pages_needed(1, 512), 1);
        assert_eq!(pages_needed(capacity, 512), 1);
        assert_eq!(pages_needed(capacity + 1, 512), 2);
        let value = vec![7u8; capacity as usize];
        assert_eq!(encode_run(&value, PageId(2), 512, 1).unwrap().len(), 1);
        let value = vec![7u8; capacity as usize + 1];
        assert_eq!(encode_run(&value, PageId(2), 512, 1).unwrap().len(), 2);
    }

    /// A page of another kind, or one whose declared payload is impossible, is
    /// refused rather than read past.
    #[test]
    fn a_damaged_extent_page_is_refused() {
        let images = encode_run(b"payload", PageId(3), 512, 1).unwrap();
        let mut damaged = images[0].1.clone();
        damaged[12] = PageKind::Leaf.code();
        assert!(read_page(&damaged).is_err());

        let mut damaged = images[0].1.clone();
        page::write_u32(&mut damaged, at::PAYLOAD, 9_999).unwrap();
        assert!(read_page(&damaged).is_err());

        assert!(read_page(&[]).is_err());
        assert!(encode_run(b"x", PageId::NONE, 512, 1).is_err());
        assert!(encode_run(b"x", PageId(2), at::BODY, 1).is_err());
    }

    /// Every page of a run names its successor, and the last names nothing.
    ///
    /// A reader follows the header rather than assuming the next page is the
    /// one after this, so a run that had to be placed non-contiguously reads
    /// back the same as one that did not.
    #[test]
    fn the_chain_is_followed_rather_than_assumed() {
        let value = vec![7u8; 1400];
        let images = encode_run(&value, PageId(9), 512, 3).unwrap();
        assert!(images.len() > 1, "the value needs more than one page");
        for (nth, (id, image)) in images.iter().enumerate() {
            let (_, next) = read_page(image).unwrap();
            match images.get(nth.saturating_add(1)) {
                Some((following, _)) => assert_eq!(
                    next, *following,
                    "page {id:?} names the page the run continues on"
                ),
                None => assert!(next.is_none(), "the last page names nothing"),
            }
        }
    }
}
