//! Blob extents: values too large for a leaf, stored in runs of whole pages.
//!
//! Invariant: an extent reference in a leaf is exactly sixteen bytes -
//! `(u64 first page, u64 total length)`, each word carrying flags in the bits
//! neither number can reach - and the pages it names hold that many payload
//! bytes and no more. A reader never trusts the length in the reference
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
//!
//! ## Why there is a second kind of extent page
//!
//! A run is whole pages, and a value one byte over the spill threshold is
//! therefore a whole page. Measured at the 32 KiB default, 1,000 rows per case,
//! file size after a checkpoint:
//!
//! | value | bytes on disk per row | ratio |
//! |---:|---:|---:|
//! | 3,000 B (inline) | 3,801 | 1.27x |
//! | 4,096 B (inline) | 5,636 | 1.38x |
//! | **4,200 B** | **33,980** | **8.09x** |
//! | 9,513 B | 33,980 | 3.57x |
//! | 20,000 B | 33,980 | 1.70x |
//!
//! Everything between 4,097 and 32,768 bytes cost 32 KiB, and that is the band
//! a lot of ordinary data lives in: extracted document text, a JSON payload, a
//! rendered vector. It is why migrating Nikaya's 5,852 MB PostgreSQL database
//! produced a 25.66 GB staged file - 601,862 rendered `halfvec` values at 9,513
//! bytes each were 20.5 GB of it.
//!
//! So a value that fits inside **one** page is not given a page of its own: it
//! goes into a [`PageKind::BlobShared`] page beside other values, with a slot
//! directory growing forward from the header and payload growing backward from
//! the end. A value that needs more than one page still gets a contiguous run,
//! because that is the whole of the `large.values` argument and packing would
//! not help it.
//!
//! | offset | size | field, on a shared page |
//! |---|---|---|
//! | 32 | 4 | how many slots the directory holds |
//! | 36 | 4 | how many of them are live |
//! | 40 | 4 | where the payload starts; everything above it is in use |
//! | 44 | 4 | reserved, zero |
//! | 48 | 8 each | the directory: `(offset, length)` per slot |
//!
//! **A reference says which of the two it is in bits the length has never
//! used.** A packed reference sets the top bit of its length word and carries
//! the slot number in the fifteen bits below it, so a database written before
//! this existed decodes exactly as it always did - every one of its references
//! has those bits clear and is read as the run it is.
//!
//! ## What the value reads back as
//!
//! A page and a length say nothing about whether the bytes are text or a blob,
//! and until task-1986 the column's declaration was the only thing that could
//! answer. That is why a column declared `BLOB` holding a text, or declared
//! nothing at all, could not have a value out of line: nothing would know what
//! to call it on the way back, so it stayed inline and a value larger than a
//! page could not be stored at all.
//!
//! A reference can now say, in the two bits above a page number: [`CLASS_STATED`]
//! and, beside it, [`CLASS_TEXT`]. **It says so only when the column's own
//! answer would be wrong**, so a reference in a column that already answers
//! correctly is the same sixteen bytes it has always been, and a reference that
//! carries the new bits exists only in a file holding a value an earlier build
//! refused to write. See [`ExtentClass`].
//!
//! **A slot's bytes are never reused while the page lives.** Clearing a slot
//! decrements the live count and nothing else; the page goes back to the free
//! map when the count reaches zero. Compacting a page in place would move a
//! value whose reference is in a leaf this function cannot see, and the space a
//! dead slot holds is bounded by the page it is in.

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

/// What a reference says its value reads back as.
///
/// A reference carries a page and a length, and those say nothing about whether
/// the bytes are text or a blob. Until this existed the column's declaration was
/// the only thing that could answer, so a column that does not answer - one
/// declared `BLOB` holding a text, or declared nothing at all - could not have a
/// value out of line at all, and a value larger than a page in such a column
/// could not be stored (task-1986).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtentClass {
    /// The reference does not say; the column decides, as it always did.
    ///
    /// This is what every reference written before task-1986 is, and it is what
    /// a reference written now still is whenever the column's own answer is the
    /// right one - see [`ExtentRef::stating`].
    Unstated,
    /// The value reads back as text whatever the column is declared.
    Text,
    /// The value reads back as a blob whatever the column is declared.
    Blob,
}

/// A value's out-of-line reference, as a leaf stores it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtentRef {
    /// The first page of the value, or the shared page it has a slot on.
    pub first: PageId,
    /// How many bytes the value holds in total.
    pub length: u64,
    /// Which slot of a shared page holds it, when it is packed into one.
    ///
    /// `None` is a run of whole pages, which is what every reference written
    /// before shared blob pages existed is and what a value too large for one
    /// page still is.
    pub slot: Option<u16>,
    /// What the value reads back as, when the reference says.
    pub class: ExtentClass,
}

/// How many bytes an extent reference occupies inside a leaf.
pub const EXTENT_REF_BYTES: usize = 16;

/// The bit of the length word that says the value is packed into a shared page.
const PACKED: u64 = 1 << 63;

/// The bit of the *page* word that says the reference states its value's class.
///
/// **In the page word rather than beside `PACKED` in the length word, because
/// of what a build that does not know about it does with one.** The length
/// word's spare bits are above `MAX_LENGTH` and below `PACKED`, so a build
/// written before task-1986 would take a run reference carrying one of them,
/// read the page and the length out of it correctly, and hand the bytes back
/// labelled by the column - which for the values this exists for is the wrong
/// label on the right bytes, the one failure this file's `decode` is otherwise
/// written to avoid. A page number of 2^63 is not a page any file has: the
/// fetch fails and the older build says so.
const CLASS_STATED: u64 = 1 << 63;

/// The bit of the page word that says the stated class is text.
///
/// Read only when [`CLASS_STATED`] is set; clear beside it means a blob.
const CLASS_TEXT: u64 = 1 << 62;

/// The bits of the page word that are the page number.
const PAGE_MASK: u64 = !(CLASS_STATED | CLASS_TEXT);

/// Where the slot number sits in the length word.
const SLOT_SHIFT: u32 = 48;

/// How many slots a shared page's directory can be addressed by.
///
/// Fifteen bits. A page holds one eight-byte directory entry per slot on top of
/// the value itself, so a 64 KiB page cannot reach a fraction of this even with
/// empty values; the bound exists so the length still has 48 bits, which is
/// 256 TiB and more than a value can be.
pub const MAX_SLOTS: u16 = 0x7FFF;

/// The largest length a reference can carry.
const MAX_LENGTH: u64 = (1 << 48) - 1;

impl ExtentRef {
    /// Returns a reference to a value stored as a run of whole pages.
    ///
    /// @param first - the run's first page
    /// @param length - the value's length
    pub fn run(first: PageId, length: u64) -> ExtentRef {
        ExtentRef {
            first,
            length,
            slot: None,
            class: ExtentClass::Unstated,
        }
    }

    /// Returns a reference to a value packed into one slot of a shared page.
    ///
    /// @param page - the shared page
    /// @param slot - which slot holds it
    /// @param length - the value's length
    pub fn packed(page: PageId, slot: u16, length: u64) -> ExtentRef {
        ExtentRef {
            first: page,
            length,
            slot: Some(slot),
            class: ExtentClass::Unstated,
        }
    }

    /// Returns the same reference, saying what its value reads back as.
    ///
    /// **A caller states the class only when the column's own answer would be
    /// the wrong one**, which is what keeps this additive: a reference whose
    /// column already answers correctly is encoded byte for byte as it was
    /// before task-1986, so every file an earlier build could have written is
    /// still written the same way and still read the same way. The tree decides
    /// which case a value is in - see `inillucent_tree::leaf::extent_class_for`,
    /// which is written beside the reader that consumes it.
    ///
    /// @param class - what the value reads back as
    pub fn stating(mut self, class: ExtentClass) -> ExtentRef {
        self.class = class;
        self
    }

    /// Returns the sixteen bytes a leaf stores.
    pub fn encode(&self) -> [u8; EXTENT_REF_BYTES] {
        let mut raw = [0u8; EXTENT_REF_BYTES];
        // `split_at_mut` rather than two `get_mut` ranges: the ranges are
        // statically inside a fixed-size array, so their `None` arms were
        // branches no input could take, and a branch no input can take is one
        // the coverage gate can only ever be lied to about.
        let (first, length) = raw.split_at_mut(8);
        let page = (self.first.0 & PAGE_MASK)
            | match self.class {
                ExtentClass::Unstated => 0,
                ExtentClass::Text => CLASS_STATED | CLASS_TEXT,
                ExtentClass::Blob => CLASS_STATED,
            };
        first.copy_from_slice(&page.to_le_bytes());
        let mut held = self.length & MAX_LENGTH;
        if let Some(slot) = self.slot {
            held |= PACKED | (u64::from(slot & MAX_SLOTS) << SLOT_SHIFT);
        }
        length.copy_from_slice(&held.to_le_bytes());
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
        let page = u64::from_le_bytes(first);
        let first = PageId(page & PAGE_MASK);
        if first.is_none() {
            return Err(corrupt("an extent reference names page zero"));
        }
        let class = match (page & CLASS_STATED != 0, page & CLASS_TEXT != 0) {
            (false, _) => ExtentClass::Unstated,
            (true, true) => ExtentClass::Text,
            (true, false) => ExtentClass::Blob,
        };
        let held = u64::from_le_bytes(length);
        let slot =
            (held & PACKED != 0).then(|| ((held >> SLOT_SHIFT) & u64::from(MAX_SLOTS)) as u16);
        Ok(ExtentRef {
            first,
            length: held & MAX_LENGTH,
            slot,
            class,
        })
    }

    /// Reports whether the value is packed into a shared page.
    pub fn is_packed(&self) -> bool {
        self.slot.is_some()
    }
}

/// The shared page: several small out-of-line values in one page.
///
/// A slotted page. The directory grows forward from [`shared::at::DIRECTORY`]
/// and the payload grows backward from the end, which is the arrangement that
/// lets one number - the payload's low water mark - say how much room is left
/// without walking anything.
pub mod shared {
    use super::{corrupt, misuse, DbResult, PageId, MAX_SLOTS};
    use crate::page::{self, PageKind};

    /// Byte offsets inside a shared extent page's own header.
    pub mod at {
        /// How many slots the directory holds, 4 bytes.
        pub const SLOTS: usize = 32;
        /// How many of those slots are live, 4 bytes.
        pub const LIVE: usize = 36;
        /// Where the payload begins; everything from here to the end is in use.
        pub const FLOOR: usize = 40;
        /// The directory; bytes 44..48 are reserved and zero.
        pub const DIRECTORY: usize = 48;
    }

    /// How many bytes one directory entry takes.
    const ENTRY: usize = 8;

    /// Writes an empty shared page into a buffer.
    ///
    /// @param image - the page buffer, exactly one page long
    /// @param tree - the tree the values belong to
    pub fn initialise(image: &mut [u8], tree: u64) -> DbResult<()> {
        let size = image.len();
        if size <= at::DIRECTORY {
            return Err(misuse("the page size leaves no room for a shared extent"));
        }
        for byte in image.iter_mut() {
            *byte = 0;
        }
        page::write_common(image, PageKind::BlobShared, 0, tree)?;
        page::write_u32(image, at::SLOTS, 0)?;
        page::write_u32(image, at::LIVE, 0)?;
        page::write_u32(image, at::FLOOR, u32::try_from(size).unwrap_or(u32::MAX))?;
        Ok(())
    }

    /// Returns how many slots a page's directory holds.
    ///
    /// @param image - the page bytes
    pub fn slots(image: &[u8]) -> DbResult<u32> {
        page::read_u32(image, at::SLOTS)
    }

    /// Returns how many of a page's slots still hold a value.
    ///
    /// @param image - the page bytes
    pub fn live(image: &[u8]) -> DbResult<u32> {
        page::read_u32(image, at::LIVE)
    }

    /// Returns how many bytes a value of this length would need on a page.
    ///
    /// The value itself plus the directory entry that finds it.
    ///
    /// @param length - the value's length
    pub fn cost(length: usize) -> usize {
        length.saturating_add(ENTRY)
    }

    /// Returns whether a value of this length fits in what is left of a page.
    ///
    /// @param image - the page bytes
    /// @param length - the value's length
    pub fn has_room(image: &[u8], length: usize) -> DbResult<bool> {
        let count = slots(image)? as usize;
        if count >= MAX_SLOTS as usize {
            return Ok(false);
        }
        let floor = page::read_u32(image, at::FLOOR)? as usize;
        if floor > image.len() {
            return Err(corrupt(
                "a shared extent page's payload starts past its end",
            ));
        }
        let directory = at::DIRECTORY.saturating_add(count.saturating_add(1).saturating_mul(ENTRY));
        Ok(floor.saturating_sub(directory) >= length)
    }

    /// Returns the longest value a *fresh* shared page can hold.
    ///
    /// **A shared page holds less than a dedicated one, and the difference is
    /// where a bound TEXT of 32,700 bytes was lost (task-1979, section 10,
    /// D2).** [`super::payload_capacity`] is what an extent page of its own
    /// holds - the page minus its header - and a shared page spends another
    /// forty eight bytes on its own header and eight on each directory entry.
    /// `write_extent` routed by the first number and then placed by the second,
    /// so a value between the two reached [`place`] on a page freshly made for
    /// it, was refused with `a shared extent page has no room for this value`,
    /// and the caller read `bad parameter or other API misuse`. Measured on the
    /// default 32,768 byte page: 32,000 bytes stored and 32,700 did not.
    ///
    /// @param page_size - the database's page size in bytes
    pub fn capacity(page_size: usize) -> usize {
        page_size.saturating_sub(at::DIRECTORY.saturating_add(ENTRY))
    }

    /// Places a value on a page and returns the slot it went into.
    ///
    /// @param image - the page bytes
    /// @param value - the bytes to store
    pub fn place(image: &mut [u8], value: &[u8]) -> DbResult<u16> {
        if !has_room(image, value.len())? {
            return Err(misuse("a shared extent page has no room for this value"));
        }
        let count = slots(image)?;
        let floor = page::read_u32(image, at::FLOOR)? as usize;
        let start = floor.saturating_sub(value.len());
        image
            .get_mut(start..floor)
            .ok_or_else(|| corrupt("a shared extent payload runs past its page"))?
            .copy_from_slice(value);
        let entry = at::DIRECTORY.saturating_add((count as usize).saturating_mul(ENTRY));
        page::write_u32(image, entry, u32::try_from(start).unwrap_or(0))?;
        page::write_u32(
            image,
            entry.saturating_add(4),
            u32::try_from(value.len()).unwrap_or(0),
        )?;
        page::write_u32(image, at::SLOTS, count.saturating_add(1))?;
        page::write_u32(image, at::LIVE, live(image)?.saturating_add(1))?;
        page::write_u32(image, at::FLOOR, u32::try_from(start).unwrap_or(0))?;
        u16::try_from(count).map_err(|_| corrupt("a shared extent page has too many slots"))
    }

    /// Returns one slot's bytes.
    ///
    /// @param image - the page bytes
    /// @param slot - which slot
    pub fn read(image: &[u8], slot: u16) -> DbResult<&[u8]> {
        if page::kind_of(image)? != PageKind::BlobShared {
            return Err(corrupt("page is not a shared blob extent"));
        }
        let count = slots(image)?;
        if u32::from(slot) >= count {
            return Err(corrupt(format!(
                "a reference names slot {slot} of a shared page that holds {count}"
            )));
        }
        let entry = at::DIRECTORY.saturating_add((slot as usize).saturating_mul(ENTRY));
        let start = page::read_u32(image, entry)? as usize;
        let length = page::read_u32(image, entry.saturating_add(4))? as usize;
        if start == 0 && length == 0 {
            return Err(corrupt(format!(
                "a reference names slot {slot} of a shared page, which has been freed"
            )));
        }
        image
            .get(start..start.saturating_add(length))
            .ok_or_else(|| corrupt("a shared extent slot runs past its page"))
    }

    /// Clears one slot and returns how many are still live.
    ///
    /// The bytes are left where they are: compacting a page would move a value
    /// whose reference lives in a leaf this function cannot see. The page goes
    /// back to the free map when nothing on it is live, which is the caller's
    /// decision to make from the number this returns.
    ///
    /// @param image - the page bytes
    /// @param slot - which slot
    pub fn clear(image: &mut [u8], slot: u16) -> DbResult<u32> {
        if page::kind_of(image)? != PageKind::BlobShared {
            return Err(corrupt("page is not a shared blob extent"));
        }
        let count = slots(image)?;
        if u32::from(slot) >= count {
            return Err(corrupt(format!(
                "a free names slot {slot} of a shared page that holds {count}"
            )));
        }
        let entry = at::DIRECTORY.saturating_add((slot as usize).saturating_mul(ENTRY));
        let length = page::read_u32(image, entry.saturating_add(4))?;
        let start = page::read_u32(image, entry)?;
        if start == 0 && length == 0 {
            // Already cleared. Freeing twice is not damage - a repack that
            // carried a reference and a caller that freed it can arrive here in
            // either order - so the live count is left alone.
            return live(image);
        }
        page::write_u32(image, entry, 0)?;
        page::write_u32(image, entry.saturating_add(4), 0)?;
        let remaining = live(image)?.saturating_sub(1);
        page::write_u32(image, at::LIVE, remaining)?;
        Ok(remaining)
    }

    /// Returns whether a page is a shared extent page.
    ///
    /// @param image - the page bytes
    pub fn is_shared(image: &[u8]) -> bool {
        matches!(page::kind_of(image), Ok(PageKind::BlobShared))
    }

    /// The page a shared reference names, for a caller reading one.
    ///
    /// @param reference - the reference
    pub fn page_of(reference: &super::ExtentRef) -> PageId {
        reference.first
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
        let reference = ExtentRef::run(PageId(77), 123_456);
        let raw = reference.encode();
        assert_eq!(ExtentRef::decode(&raw).unwrap(), reference);
        assert!(ExtentRef::decode(&raw[..15]).is_err());
        assert!(ExtentRef::decode(&[0u8; 16]).is_err());
        assert_eq!(EXTENT_REF_BYTES, 16);
    }

    /// A reference that states its class round-trips, and one that does not is
    /// byte for byte what it was before a class could be stated.
    ///
    /// **The second assertion is the compatibility promise.** A build written
    /// before task-1986 reads every reference this one writes for a column that
    /// answers correctly, because those sixteen bytes did not change; what it
    /// cannot read is a reference for a column that does not, and that is a
    /// reference for a value it refused to write in the first place.
    #[test]
    fn a_stated_class_round_trips_and_an_unstated_one_is_unchanged() {
        for (class, packed) in [
            (ExtentClass::Unstated, false),
            (ExtentClass::Text, false),
            (ExtentClass::Blob, false),
            (ExtentClass::Unstated, true),
            (ExtentClass::Text, true),
            (ExtentClass::Blob, true),
        ] {
            let base = if packed {
                ExtentRef::packed(PageId(0x1234_5678), 41, 1_048_576)
            } else {
                ExtentRef::run(PageId(0x1234_5678), 1_048_576)
            };
            let reference = base.stating(class);
            let raw = reference.encode();
            let read = ExtentRef::decode(&raw).unwrap();
            assert_eq!(read, reference);
            assert_eq!(read.class, class);
            assert_eq!(read.first, PageId(0x1234_5678));
            assert_eq!(read.length, 1_048_576);
            assert_eq!(read.slot, base.slot);
            if class == ExtentClass::Unstated {
                assert_eq!(raw, base.encode());
            } else {
                assert_ne!(raw, base.encode());
            }
        }
    }

    /// A build that does not know about the class bits meets one as a page
    /// number it cannot fetch rather than as the wrong label on right bytes.
    ///
    /// The page word is checked rather than the length word for exactly this:
    /// the spare bits in the length word are above `MAX_LENGTH` and below
    /// `PACKED`, so an older build would have read the page and the length out
    /// of a stated run reference correctly and answered with the column's own
    /// idea of what the bytes were.
    #[test]
    fn an_older_build_cannot_read_a_stated_reference_as_a_page_it_has() {
        let raw = ExtentRef::run(PageId(9), 40_000)
            .stating(ExtentClass::Text)
            .encode();
        let mut word = [0u8; 8];
        word.copy_from_slice(&raw[0..8]);
        // What the decode written before task-1986 did: the whole word.
        let as_an_older_build_reads_it = u64::from_le_bytes(word);
        assert_eq!(as_an_older_build_reads_it, 9 | CLASS_STATED | CLASS_TEXT);
        assert!(as_an_older_build_reads_it > 1 << 62);
    }

    /// A packed reference round-trips, and a run's bytes still read as a run.
    ///
    /// **The compatibility claim, checked rather than argued.**
    /// A packed reference marks itself in the top bit of its length word and
    /// carries its slot in the fifteen bits below; a reference written before
    /// that existed has those bits clear, so the bytes a database already holds
    /// decode as the run they are. The second half of this asserts exactly that
    /// against bytes assembled by hand rather than by `encode`, because the
    /// point is what an *older* writer left behind.
    #[test]
    fn a_packed_reference_round_trips_and_a_plain_one_is_still_a_run() {
        let packed = ExtentRef::packed(PageId(9), 5, 4_200);
        let raw = packed.encode();
        let read = ExtentRef::decode(&raw).unwrap();
        assert_eq!(read, packed);
        assert_eq!(read.slot, Some(5));
        assert_eq!(read.length, 4_200);
        assert!(read.is_packed());

        // The highest slot and the largest length either bit field can hold.
        let edge = ExtentRef::packed(PageId(3), MAX_SLOTS, MAX_LENGTH);
        assert_eq!(ExtentRef::decode(&edge.encode()).unwrap(), edge);

        // Sixteen bytes an older build would have written: page 77, length
        // 123,456, and nothing in the top sixteen bits.
        let mut older = [0u8; EXTENT_REF_BYTES];
        older[0..8].copy_from_slice(&77u64.to_le_bytes());
        older[8..16].copy_from_slice(&123_456u64.to_le_bytes());
        let read = ExtentRef::decode(&older).unwrap();
        assert_eq!(read, ExtentRef::run(PageId(77), 123_456));
        assert!(!read.is_packed(), "a plain reference is a run");
    }

    /// A shared page holds several values, reads each back, and frees by slot.
    #[test]
    fn a_shared_page_packs_several_values_and_frees_them_one_at_a_time() {
        let mut image = vec![0u8; 4_096];
        shared::initialise(&mut image, 3).unwrap();
        assert_eq!(shared::slots(&image).unwrap(), 0);
        assert_eq!(shared::live(&image).unwrap(), 0);

        let first = vec![1u8; 600];
        let second = vec![2u8; 1_200];
        let third = vec![3u8; 900];
        let a = shared::place(&mut image, &first).unwrap();
        let b = shared::place(&mut image, &second).unwrap();
        let c = shared::place(&mut image, &third).unwrap();
        assert_eq!((a, b, c), (0, 1, 2));
        assert_eq!(shared::live(&image).unwrap(), 3);
        assert_eq!(shared::read(&image, a).unwrap(), first.as_slice());
        assert_eq!(shared::read(&image, b).unwrap(), second.as_slice());
        assert_eq!(shared::read(&image, c).unwrap(), third.as_slice());

        // Freeing the middle one leaves the others where they are, which is the
        // property that lets a slot's bytes never move: their references are in
        // leaves this page knows nothing about.
        assert_eq!(shared::clear(&mut image, b).unwrap(), 2);
        assert!(shared::read(&image, b).is_err());
        assert_eq!(shared::read(&image, a).unwrap(), first.as_slice());
        assert_eq!(shared::read(&image, c).unwrap(), third.as_slice());

        // Freeing it again is not damage and does not double-count.
        assert_eq!(shared::clear(&mut image, b).unwrap(), 2);
        assert_eq!(shared::clear(&mut image, a).unwrap(), 1);
        assert_eq!(shared::clear(&mut image, c).unwrap(), 0);

        // And a slot the page does not have is refused rather than read past.
        assert!(shared::read(&image, 9).is_err());
        assert!(shared::clear(&mut image, 9).is_err());
    }

    /// A shared page refuses a value once its directory and payload would meet.
    ///
    /// The arithmetic is the point: the directory grows forward and the payload
    /// backward, so the room left is the gap between them, and a check that
    /// forgot the directory entry would overwrite the last slot it wrote.
    #[test]
    fn a_shared_page_says_when_it_is_full() {
        let size = 1_024usize;
        let mut image = vec![0u8; size];
        shared::initialise(&mut image, 1).unwrap();
        let mut placed = 0usize;
        let value = vec![7u8; 100];
        while shared::has_room(&image, value.len()).unwrap() {
            shared::place(&mut image, &value).unwrap();
            placed = placed.saturating_add(1);
        }
        assert!(placed >= 8, "only {placed} values fitted a 1 KiB page");
        assert!(!shared::has_room(&image, value.len()).unwrap());
        // Everything placed still reads back, which is what says the page was
        // filled rather than overrun.
        for slot in 0..placed {
            let held = shared::read(&image, slot as u16).unwrap();
            assert_eq!(held, value.as_slice(), "slot {slot}");
        }
        assert_eq!(shared::live(&image).unwrap(), placed as u32);
    }

    /// A page that is not a shared extent is refused rather than read as one.
    #[test]
    fn a_page_of_another_kind_is_not_read_as_a_shared_extent() {
        let images = encode_run(b"payload", PageId(3), 512, 1).unwrap();
        assert!(shared::read(&images[0].1, 0).is_err());
        assert!(!shared::is_shared(&images[0].1));
        let mut shared_page = vec![0u8; 512];
        shared::initialise(&mut shared_page, 1).unwrap();
        assert!(shared::is_shared(&shared_page));
        // And the run reader refuses a shared page, for the same reason.
        assert!(read_page(&shared_page).is_err());
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
