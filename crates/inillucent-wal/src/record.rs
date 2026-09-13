//! The record codec: what one entry in the log is, in bytes.
//!
//! Invariant: `decode` is a decoder reading bytes a crash wrote. Every field it
//! reads is bounds checked against the record's own declared length before it
//! is used, every length it reads is checked against what remains, and no
//! input (truncated, reordered, or hostile) produces a panic or a value derived
//! from bytes outside the record. The acceptance for this module is **100%
//! branch coverage**, held to the same bar as the interior and key codecs,
//! because a branch here is only ever taken by a file that has already been
//! damaged.
//!
//! ## The layout
//!
//! ```text
//! 0   u32  total length, including this header and the trailing padding
//! 4   u32  crc32c over bytes 8..total length
//! 8   u64  lsn
//! 16  u64  txn id
//! 24  u8   kind
//! 25  7 bytes zero
//! 32       payload
//! ..       zero padding to an 8-byte boundary
//! ```
//!
//! The checksum starts at byte 8 rather than at byte 0 for the ordinary reason:
//! it cannot cover itself, and the length in front of it is what says where it
//! stops. A damaged length is therefore caught by the length checks rather than
//! by the checksum, which is why [`Record::decode`] validates the length
//! against the buffer *and* against the fixed minimum before it computes a
//! checksum over anything.
//!
//! ## Why the padding is inside the checksum
//!
//! Records are 8-byte aligned, so most of them carry one to seven zero bytes
//! after the payload. Those bytes are covered by the checksum and are required
//! to be zero, which costs nothing to write and turns "a record whose padding
//! holds the tail of the record that used to be there" into a decode error
//! rather than into bytes nobody looks at. A torn tail is exactly that
//! situation, and it is the situation recovery has to recognise.
//!
//! ## Page numbers are `u64`, not `PageId`
//!
//! `PageId` lives in `inillucent-pool`, one layer up. The log is a byte stream
//! that is written before any page is, so it does not depend on the thing that
//! owns pages; a record carries the page's number and the caller that owns
//! pages turns it back into one. That is the whole reason this crate can sit
//! beside the buffer pool rather than above it.

use inillucent_base::checksum::crc32;
use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

/// The size of a record header in bytes.
pub const HEADER_BYTES: usize = 32;

/// The alignment every record starts and ends on.
pub const ALIGN: usize = 8;

/// The largest record this codec will encode or accept.
///
/// A structural record carries three whole page images, so the ceiling has to
/// clear three times the largest page size with room for the header; four
/// mebibytes does, and it is small enough that a corrupt length is rejected
/// long before an allocation is attempted.
pub const MAX_RECORD_BYTES: usize = 4 << 20;

/// Byte offsets inside a record header.
mod at {
    /// The record's total length including header and padding, 4 bytes.
    pub const LENGTH: usize = 0;
    /// crc32c over everything from [`CHECKSUMMED_FROM`] on, 4 bytes.
    pub const CHECKSUM: usize = 4;
    /// The log sequence number, 8 bytes.
    pub const LSN: usize = 8;
    /// The transaction this record belongs to, 8 bytes.
    pub const TXN: usize = 16;
    /// The record kind, 1 byte.
    pub const KIND: usize = 24;
    /// The seven bytes that must be zero.
    pub const RESERVED: usize = 25;
    /// Where the checksum's coverage begins.
    pub const CHECKSUMMED_FROM: usize = 8;
}

/// Which of the two structural rearrangements a record describes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Structural {
    /// One leaf became two.
    Split,
    /// Two leaves became one.
    Merge,
}

/// What a record says happened.
///
/// The borrowed slices point into the buffer the record was decoded from, so
/// replaying a segment copies a page image once - into the page - rather than
/// once into a record and again into the page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Body<'a> {
    /// A row was inserted into a leaf, through the delta area or the sorted
    /// region.
    InsertRow {
        /// The tree the leaf belongs to.
        tree: u64,
        /// The leaf's page number.
        page: u64,
        /// The row, in the leaf's tagged encoding.
        row: &'a [u8],
    },
    /// A row was removed from a leaf, by tombstone or by delta removal.
    DeleteRow {
        /// The tree the leaf belongs to.
        tree: u64,
        /// The leaf's page number.
        page: u64,
        /// The key of the row that went away.
        key: &'a [u8],
    },
    /// One fixed-width slot of one row was overwritten.
    UpdateInPlace {
        /// The tree the leaf belongs to.
        tree: u64,
        /// The leaf's page number.
        page: u64,
        /// The key of the row that changed.
        key: &'a [u8],
        /// Which column changed.
        column: u32,
        /// The new value's bytes, in the column's slot encoding.
        value: &'a [u8],
    },
    /// A leaf's delta area was folded back into its sorted region.
    CompactLeaf {
        /// The tree the leaf belongs to.
        tree: u64,
        /// The leaf's page number.
        page: u64,
        /// The whole page after the compaction.
        image: &'a [u8],
        /// The LSN the page carried **before** the compaction ran.
        ///
        /// **A diagnosis, not a gate.** A compaction with no
        /// image asks recovery to re-run the pack over the page's own live
        /// rows, on the argument that redo replays in LSN order and the page is
        /// therefore in the state the record was written against. When that
        /// argument fails, what recovery sees is a pack that will not fit and no
        /// way to tell whether the page is wrong or the derivation is - and the
        /// database is unopenable while it works that out. This says which page
        /// the writer had, so the replay can name both stamps.
        ///
        /// Zero when the record was written before this field existed. The
        /// padding rule in `Record::decode` allows fewer than eight spare bytes,
        /// so eight trailing bytes are always this field and never padding.
        from_lsn: u64,
    },
    /// A split or a merge, as the three page images it produced.
    Structural {
        /// Which of the two it was.
        kind: Structural,
        /// The tree the leaves belong to.
        tree: u64,
        /// The left leaf's page number.
        left: u64,
        /// The right leaf's page number.
        right: u64,
        /// The parent interior page's number.
        parent: u64,
        /// The left leaf's whole page.
        left_image: &'a [u8],
        /// The right leaf's whole page.
        right_image: &'a [u8],
        /// The parent's whole page.
        parent_image: &'a [u8],
    },
    /// A whole page was written: bulk build, free map, interior rewrite.
    WritePage {
        /// The page's number.
        page: u64,
        /// The whole page.
        image: &'a [u8],
    },
    /// A page was taken out of the free map.
    AllocPage {
        /// The page's number.
        page: u64,
    },
    /// A page was given back to the free map.
    FreePage {
        /// The page's number.
        page: u64,
    },
    /// The transaction in the header committed.
    Commit {
        /// The commit timestamp it was assigned.
        cts: u64,
    },
    /// The transaction in the header rolled back after its records were
    /// flushed, so recovery must not replay them.
    Abort,
    /// A checkpoint completed; recovery starts here.
    Checkpoint {
        /// Every page write at or below this LSN is in the data file.
        checkpoint_lsn: u64,
        /// The commit timestamp watermark at the checkpoint.
        cts_watermark: u64,
    },
    /// The catalog changed, so plan caches built before this point are stale.
    ///
    /// The catalog's *rows* travel as ordinary row records into the catalog
    /// tree; this record exists so that replay knows to invalidate, which is
    /// something no row record can say.
    CatalogChange {
        /// The serialised delta, opaque to this crate.
        delta: &'a [u8],
    },
    /// Filler, belonging to no transaction, so the next real record starts on
    /// a fresh device sector rather than the tail of one an earlier,
    /// already-durable record ends in.
    ///
    /// See `writer::SECTOR_ALIGN`'s own comment for the failure this closes:
    /// a write that begins mid-sector puts the *whole* sector back in the
    /// media's unsynced cache, so a later sync failure or crash can garble
    /// bytes an earlier, independently acknowledged commit already had. The
    /// payload is exactly as many zero bytes as needed to reach that
    /// boundary; recovery reads and checksums them like any other record and
    /// otherwise ignores them.
    Pad {
        /// How many zero bytes of filler this record carries.
        len: u32,
    },
}

impl Body<'_> {
    /// Returns the byte that names this body's kind in a header.
    pub fn kind(&self) -> u8 {
        match self {
            Body::InsertRow { .. } => kind::INSERT_ROW,
            Body::DeleteRow { .. } => kind::DELETE_ROW,
            Body::UpdateInPlace { .. } => kind::UPDATE_IN_PLACE,
            Body::CompactLeaf { .. } => kind::COMPACT_LEAF,
            Body::Structural {
                kind: Structural::Split,
                ..
            } => kind::SPLIT_LEAF,
            Body::Structural {
                kind: Structural::Merge,
                ..
            } => kind::MERGE_LEAF,
            Body::WritePage { .. } => kind::WRITE_PAGE,
            Body::AllocPage { .. } => kind::ALLOC_PAGE,
            Body::FreePage { .. } => kind::FREE_PAGE,
            Body::Commit { .. } => kind::COMMIT,
            Body::Abort => kind::ABORT,
            Body::Checkpoint { .. } => kind::CHECKPOINT,
            Body::CatalogChange { .. } => kind::CATALOG_CHANGE,
            Body::Pad { .. } => kind::PAD,
        }
    }
}

/// The kind bytes, which are part of the on-disk format and never renumbered.
pub mod kind {
    /// [`super::Body::InsertRow`].
    pub const INSERT_ROW: u8 = 1;
    /// [`super::Body::DeleteRow`].
    pub const DELETE_ROW: u8 = 2;
    /// [`super::Body::UpdateInPlace`].
    pub const UPDATE_IN_PLACE: u8 = 3;
    /// [`super::Body::CompactLeaf`].
    pub const COMPACT_LEAF: u8 = 4;
    /// [`super::Body::Structural`] with [`super::Structural::Split`].
    pub const SPLIT_LEAF: u8 = 5;
    /// [`super::Body::Structural`] with [`super::Structural::Merge`].
    pub const MERGE_LEAF: u8 = 6;
    /// [`super::Body::WritePage`].
    pub const WRITE_PAGE: u8 = 7;
    /// [`super::Body::AllocPage`].
    pub const ALLOC_PAGE: u8 = 8;
    /// [`super::Body::FreePage`].
    pub const FREE_PAGE: u8 = 9;
    /// [`super::Body::Commit`].
    pub const COMMIT: u8 = 10;
    /// [`super::Body::Abort`].
    pub const ABORT: u8 = 11;
    /// [`super::Body::Checkpoint`].
    pub const CHECKPOINT: u8 = 12;
    /// [`super::Body::CatalogChange`].
    pub const CATALOG_CHANGE: u8 = 13;
    /// [`super::Body::Pad`].
    pub const PAD: u8 = 14;
}

/// One decoded log record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record<'a> {
    /// The record's log sequence number.
    pub lsn: u64,
    /// The transaction it belongs to; zero for records no transaction owns.
    pub txn: u64,
    /// What it says happened.
    pub body: Body<'a>,
    /// How many bytes it occupies, header and padding included.
    pub length: usize,
}

impl<'a> Record<'a> {
    /// Returns the page numbers this record's redo would write, in order.
    ///
    /// Recovery applies a record per page under the page-LSN rule, so it needs
    /// to know which pages a record touches without matching on the body at
    /// every step. A record that writes no page returns an empty list, which is
    /// what makes `Commit`, `Abort` and `Checkpoint` free of the rule rather
    /// than exempt from it.
    pub fn pages(&self) -> PageList {
        match self.body {
            Body::InsertRow { page, .. }
            | Body::DeleteRow { page, .. }
            | Body::UpdateInPlace { page, .. }
            | Body::CompactLeaf { page, .. }
            | Body::WritePage { page, .. } => PageList::one(page),
            Body::Structural {
                left,
                right,
                parent,
                ..
            } => PageList::three(left, right, parent),
            Body::AllocPage { .. }
            | Body::FreePage { .. }
            | Body::Commit { .. }
            | Body::Abort
            | Body::Checkpoint { .. }
            | Body::CatalogChange { .. }
            | Body::Pad { .. } => PageList::none(),
        }
    }

    /// Appends the record to a buffer and returns how many bytes it added.
    ///
    /// @param out - the buffer to append to
    pub fn encode(&self, out: &mut Vec<u8>) -> DbResult<usize> {
        let start = out.len();
        out.extend_from_slice(&[0u8; HEADER_BYTES]);
        encode_body(&self.body, out)?;
        while !(out.len().saturating_sub(start)).is_multiple_of(ALIGN) {
            out.push(0);
        }
        let length = out.len().saturating_sub(start);
        if length > MAX_RECORD_BYTES {
            out.truncate(start);
            return Err(misuse(format!(
                "a log record of {length} bytes is past the {MAX_RECORD_BYTES}-byte ceiling"
            )));
        }
        let record = out
            .get_mut(start..)
            .ok_or_else(|| misuse("the record vanished from the buffer it was written into"))?;
        put_u32(record, at::LENGTH, length as u32)?;
        put_u64(record, at::LSN, self.lsn)?;
        put_u64(record, at::TXN, self.txn)?;
        put_u8(record, at::KIND, self.body.kind())?;
        let sum = crc32(
            record
                .get(at::CHECKSUMMED_FROM..)
                .ok_or_else(|| misuse("the record is shorter than its own header"))?,
        );
        put_u32(record, at::CHECKSUM, sum)?;
        Ok(length)
    }

    /// Decodes the record at the front of a buffer.
    ///
    /// Returns `Ok(None)` when the buffer holds no record at all - it is empty,
    /// or it is all zeroes, which is what an unwritten region of a preallocated
    /// segment looks like. Every other failure is an error naming what was
    /// wrong, because a partially-written record and a corrupt one are the same
    /// thing to a reader and both stop the scan.
    ///
    /// @param buffer - the bytes starting at the record's first byte
    pub fn decode(buffer: &'a [u8]) -> DbResult<Option<Record<'a>>> {
        if buffer.len() < HEADER_BYTES {
            // A tail shorter than a header cannot be a record. It is not an
            // error in a log - it is where the log stops - so the caller is
            // told "nothing here" and decides what that means.
            return Ok(None);
        }
        let length = read_u32(buffer, at::LENGTH)? as usize;
        if length == 0 {
            // An unwritten region. Distinguished from a damaged length because
            // a preallocated segment is full of them and reading one is the
            // ordinary way a scan finds the end.
            return Ok(None);
        }
        if length < HEADER_BYTES || !length.is_multiple_of(ALIGN) || length > MAX_RECORD_BYTES {
            return Err(corrupt(format!(
                "a log record declares {length} bytes, which is not a legal record length"
            )));
        }
        let record = buffer
            .get(..length)
            .ok_or_else(|| corrupt("a log record declares more bytes than the segment holds"))?;
        let stated = read_u32(record, at::CHECKSUM)?;
        let computed = crc32(
            record
                .get(at::CHECKSUMMED_FROM..)
                .ok_or_else(|| corrupt("a log record is shorter than its own header"))?,
        );
        if stated != computed {
            return Err(corrupt(format!(
                "a log record's checksum is {stated:#010x} and its bytes say {computed:#010x}"
            )));
        }
        let reserved = record
            .get(at::RESERVED..at::RESERVED.saturating_add(7))
            .ok_or_else(|| corrupt("a log record is shorter than its own header"))?;
        if reserved.iter().any(|byte| *byte != 0) {
            return Err(corrupt("a log record's reserved bytes are not zero"));
        }
        let lsn = read_u64(record, at::LSN)?;
        let txn = read_u64(record, at::TXN)?;
        let kind = read_u8(record, at::KIND)?;
        let payload = record
            .get(HEADER_BYTES..)
            .ok_or_else(|| corrupt("a log record is shorter than its own header"))?;
        let (body, used) = decode_body(kind, payload)?;
        let padding = payload
            .get(used..)
            .ok_or_else(|| corrupt("a log record's payload decoded past its own end"))?;
        if padding.len() >= ALIGN {
            return Err(corrupt(format!(
                "a log record declares {length} bytes and its payload uses {used}, \
                 which leaves {} bytes of padding",
                padding.len()
            )));
        }
        if padding.iter().any(|byte| *byte != 0) {
            return Err(corrupt("a log record's padding is not zero"));
        }
        Ok(Some(Record {
            lsn,
            txn,
            body,
            length,
        }))
    }
}

/// Up to three page numbers, without an allocation.
///
/// A structural record names three pages and every other record names at most
/// one, so the list is a fixed array. Returning a `Vec` here would allocate on
/// the recovery path once per record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageList {
    pages: [u64; 3],
    used: usize,
}

impl PageList {
    /// Returns the empty list.
    fn none() -> PageList {
        PageList::default()
    }

    /// Returns a list of one page.
    ///
    /// @param page - the page number
    fn one(page: u64) -> PageList {
        PageList {
            pages: [page, 0, 0],
            used: 1,
        }
    }

    /// Returns a list of three pages.
    ///
    /// @param first - the first page number
    /// @param second - the second page number
    /// @param third - the third page number
    fn three(first: u64, second: u64, third: u64) -> PageList {
        PageList {
            pages: [first, second, third],
            used: 3,
        }
    }

    /// Returns the page numbers, in order.
    pub fn as_slice(&self) -> &[u64] {
        self.pages.get(..self.used).unwrap_or(&[])
    }
}

/// Writes one body's payload.
///
/// @param body - what to write
/// @param out - the buffer to append to
fn encode_body(body: &Body<'_>, out: &mut Vec<u8>) -> DbResult<()> {
    match body {
        Body::InsertRow { tree, page, row } => {
            put_tree_page_bytes(out, *tree, *page, row);
        }
        Body::DeleteRow { tree, page, key } => {
            put_tree_page_bytes(out, *tree, *page, key);
        }
        Body::UpdateInPlace {
            tree,
            page,
            key,
            column,
            value,
        } => {
            out.extend_from_slice(&tree.to_le_bytes());
            out.extend_from_slice(&page.to_le_bytes());
            out.extend_from_slice(&column.to_le_bytes());
            put_bytes(out, key);
            put_bytes(out, value);
        }
        Body::CompactLeaf {
            tree,
            page,
            image,
            from_lsn,
        } => {
            put_tree_page_bytes(out, *tree, *page, image);
            out.extend_from_slice(&from_lsn.to_le_bytes());
        }
        Body::Structural {
            kind: _,
            tree,
            left,
            right,
            parent,
            left_image,
            right_image,
            parent_image,
        } => {
            out.extend_from_slice(&tree.to_le_bytes());
            out.extend_from_slice(&left.to_le_bytes());
            out.extend_from_slice(&right.to_le_bytes());
            out.extend_from_slice(&parent.to_le_bytes());
            put_bytes(out, left_image);
            put_bytes(out, right_image);
            put_bytes(out, parent_image);
        }
        Body::WritePage { page, image } => {
            out.extend_from_slice(&page.to_le_bytes());
            put_bytes(out, image);
        }
        Body::AllocPage { page } | Body::FreePage { page } => {
            out.extend_from_slice(&page.to_le_bytes());
        }
        Body::Commit { cts } => {
            out.extend_from_slice(&cts.to_le_bytes());
        }
        Body::Abort => {}
        Body::Checkpoint {
            checkpoint_lsn,
            cts_watermark,
        } => {
            out.extend_from_slice(&checkpoint_lsn.to_le_bytes());
            out.extend_from_slice(&cts_watermark.to_le_bytes());
        }
        Body::CatalogChange { delta } => {
            put_bytes(out, delta);
        }
        Body::Pad { len } => {
            out.resize(out.len().saturating_add(*len as usize), 0);
        }
    }
    Ok(())
}

/// Reads one body's payload and says how many bytes it used.
///
/// @param kind - the header's kind byte
/// @param payload - the bytes after the header
fn decode_body(kind: u8, payload: &[u8]) -> DbResult<(Body<'_>, usize)> {
    let mut cursor = Cursor::new(payload);
    let body = match kind {
        kind::INSERT_ROW => {
            let (tree, page, row) = cursor.tree_page_bytes()?;
            Body::InsertRow { tree, page, row }
        }
        kind::DELETE_ROW => {
            let (tree, page, key) = cursor.tree_page_bytes()?;
            Body::DeleteRow { tree, page, key }
        }
        kind::UPDATE_IN_PLACE => {
            let tree = cursor.u64()?;
            let page = cursor.u64()?;
            let column = cursor.u32()?;
            let key = cursor.bytes()?;
            let value = cursor.bytes()?;
            Body::UpdateInPlace {
                tree,
                page,
                key,
                column,
                value,
            }
        }
        kind::COMPACT_LEAF => {
            let (tree, page, image) = cursor.tree_page_bytes()?;
            // A trailing eight bytes are the page's stamp before the
            // compaction. A record written before that field existed has none,
            // and `Record::decode` refuses eight or more bytes of padding - so
            // there is no length at which the two are ambiguous.
            let from_lsn = if cursor.remaining() >= 8 {
                cursor.u64()?
            } else {
                0
            };
            Body::CompactLeaf {
                tree,
                page,
                image,
                from_lsn,
            }
        }
        kind::SPLIT_LEAF | kind::MERGE_LEAF => {
            let structural = if kind == kind::SPLIT_LEAF {
                Structural::Split
            } else {
                Structural::Merge
            };
            let tree = cursor.u64()?;
            let left = cursor.u64()?;
            let right = cursor.u64()?;
            let parent = cursor.u64()?;
            let left_image = cursor.bytes()?;
            let right_image = cursor.bytes()?;
            let parent_image = cursor.bytes()?;
            Body::Structural {
                kind: structural,
                tree,
                left,
                right,
                parent,
                left_image,
                right_image,
                parent_image,
            }
        }
        kind::WRITE_PAGE => {
            let page = cursor.u64()?;
            let image = cursor.bytes()?;
            Body::WritePage { page, image }
        }
        kind::ALLOC_PAGE => Body::AllocPage {
            page: cursor.u64()?,
        },
        kind::FREE_PAGE => Body::FreePage {
            page: cursor.u64()?,
        },
        kind::COMMIT => Body::Commit { cts: cursor.u64()? },
        kind::ABORT => Body::Abort,
        kind::CHECKPOINT => Body::Checkpoint {
            checkpoint_lsn: cursor.u64()?,
            cts_watermark: cursor.u64()?,
        },
        kind::CATALOG_CHANGE => Body::CatalogChange {
            delta: cursor.bytes()?,
        },
        kind::PAD => Body::Pad {
            len: cursor.zero_rest()?,
        },
        other => {
            return Err(corrupt(format!(
                "a log record has kind {other}, which this format does not define"
            )))
        }
    };
    Ok((body, cursor.used()))
}

/// Writes the `tree, page, length-prefixed bytes` shape three kinds share.
///
/// Shared rather than written three times because a duplicated decoder is a
/// duplicated set of bounds checks, and the coverage bar on this module is
/// every branch: three copies of the same check are three chances for one of
/// them to be subtly different and for the difference to be invisible.
///
/// @param out - the buffer to append to
/// @param tree - the tree id
/// @param page - the page number
/// @param bytes - the payload
fn put_tree_page_bytes(out: &mut Vec<u8>, tree: u64, page: u64, bytes: &[u8]) {
    out.extend_from_slice(&tree.to_le_bytes());
    out.extend_from_slice(&page.to_le_bytes());
    put_bytes(out, bytes);
}

/// Writes a `u32` length followed by the bytes.
///
/// @param out - the buffer to append to
/// @param bytes - what to write
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// A bounds-checked forward reader over a record payload.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    /// Returns a cursor at the front of a payload.
    ///
    /// @param bytes - the payload
    fn new(bytes: &'a [u8]) -> Cursor<'a> {
        Cursor { bytes, at: 0 }
    }

    /// Returns how many bytes have been consumed.
    fn used(&self) -> usize {
        self.at
    }

    /// Returns how many bytes are left.
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    /// Takes `count` bytes.
    ///
    /// @param count - how many
    fn take(&mut self, count: usize) -> DbResult<&'a [u8]> {
        let end = self.at.saturating_add(count);
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| corrupt("a log record's payload ends before its fields do"))?;
        self.at = end;
        Ok(slice)
    }

    /// Takes a little-endian `u32`.
    fn u32(&mut self) -> DbResult<u32> {
        let slice = self.take(4)?;
        let mut raw = [0u8; 4];
        raw.copy_from_slice(slice);
        Ok(u32::from_le_bytes(raw))
    }

    /// Takes a little-endian `u64`.
    fn u64(&mut self) -> DbResult<u64> {
        let slice = self.take(8)?;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(slice);
        Ok(u64::from_le_bytes(raw))
    }

    /// Takes a `u32` length and that many bytes.
    fn bytes(&mut self) -> DbResult<&'a [u8]> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    /// Takes the `tree, page, bytes` shape.
    fn tree_page_bytes(&mut self) -> DbResult<(u64, u64, &'a [u8])> {
        let tree = self.u64()?;
        let page = self.u64()?;
        let bytes = self.bytes()?;
        Ok((tree, page, bytes))
    }

    /// Takes every remaining byte as pure filler and returns how many there
    /// were.
    ///
    /// Every one of them must be zero: a `Pad` record's whole reason to exist
    /// is to be inert, so bytes that are not zero are not filler, they are
    /// damage - the same rule [`Record::decode`] already applies to a
    /// record's alignment padding, extended to cover a record whose entire
    /// payload is padding.
    fn zero_rest(&mut self) -> DbResult<u32> {
        let rest = self.take(self.remaining())?;
        if rest.iter().any(|byte| *byte != 0) {
            return Err(corrupt("a log record's padding is not zero"));
        }
        Ok(rest.len() as u32)
    }
}

/// Reads a `u8` at an offset.
///
/// @param buffer - the bytes
/// @param at - the offset
fn read_u8(buffer: &[u8], at: usize) -> DbResult<u8> {
    buffer
        .get(at)
        .copied()
        .ok_or_else(|| corrupt(format!("a log record ends before offset {at}")))
}

/// Reads a little-endian `u32` at an offset.
///
/// @param buffer - the bytes
/// @param at - the offset
fn read_u32(buffer: &[u8], at: usize) -> DbResult<u32> {
    let slice = buffer
        .get(at..at.saturating_add(4))
        .ok_or_else(|| corrupt(format!("a log record ends before offset {at}")))?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(slice);
    Ok(u32::from_le_bytes(raw))
}

/// Reads a little-endian `u64` at an offset.
///
/// @param buffer - the bytes
/// @param at - the offset
fn read_u64(buffer: &[u8], at: usize) -> DbResult<u64> {
    let slice = buffer
        .get(at..at.saturating_add(8))
        .ok_or_else(|| corrupt(format!("a log record ends before offset {at}")))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(slice);
    Ok(u64::from_le_bytes(raw))
}

/// Writes a `u8` at an offset.
///
/// @param buffer - the bytes
/// @param at - the offset
/// @param value - what to write
fn put_u8(buffer: &mut [u8], at: usize, value: u8) -> DbResult<()> {
    *buffer
        .get_mut(at)
        .ok_or_else(|| misuse(format!("a log record ends before offset {at}")))? = value;
    Ok(())
}

/// Writes a little-endian `u32` at an offset.
///
/// @param buffer - the bytes
/// @param at - the offset
/// @param value - what to write
fn put_u32(buffer: &mut [u8], at: usize, value: u32) -> DbResult<()> {
    let slice = buffer
        .get_mut(at..at.saturating_add(4))
        .ok_or_else(|| misuse(format!("a log record ends before offset {at}")))?;
    slice.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Writes a little-endian `u64` at an offset.
///
/// @param buffer - the bytes
/// @param at - the offset
/// @param value - what to write
fn put_u64(buffer: &mut [u8], at: usize, value: u64) -> DbResult<()> {
    let slice = buffer
        .get_mut(at..at.saturating_add(8))
        .ok_or_else(|| misuse(format!("a log record ends before offset {at}")))?;
    slice.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One record of every kind, for the sweeps that must cover all of them.
    fn every_body() -> Vec<Body<'static>> {
        vec![
            Body::InsertRow {
                tree: 7,
                page: 9,
                row: b"row-bytes",
            },
            Body::DeleteRow {
                tree: 7,
                page: 9,
                key: b"key",
            },
            Body::UpdateInPlace {
                tree: 7,
                page: 9,
                key: b"key",
                column: 3,
                value: b"12345678",
            },
            Body::CompactLeaf {
                tree: 7,
                page: 9,
                image: b"a whole page, in miniature",
                from_lsn: 4_096,
            },
            Body::Structural {
                kind: Structural::Split,
                tree: 7,
                left: 9,
                right: 10,
                parent: 4,
                left_image: b"left",
                right_image: b"right!",
                parent_image: b"parent",
            },
            Body::Structural {
                kind: Structural::Merge,
                tree: 7,
                left: 9,
                right: 10,
                parent: 4,
                left_image: b"left",
                right_image: b"right!",
                parent_image: b"parent",
            },
            Body::WritePage {
                page: 12,
                image: b"page",
            },
            Body::AllocPage { page: 13 },
            Body::FreePage { page: 14 },
            Body::Commit { cts: 99 },
            Body::Abort,
            Body::Checkpoint {
                checkpoint_lsn: 500,
                cts_watermark: 40,
            },
            Body::CatalogChange { delta: b"delta" },
            Body::Pad { len: 16 },
        ]
    }

    /// Encodes one body at a fixed lsn and txn.
    ///
    /// @param body - what to encode
    fn encoded(body: Body<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        Record {
            lsn: 42,
            txn: 8,
            body,
            length: 0,
        }
        .encode(&mut out)
        .expect("the record encodes");
        out
    }

    /// Every kind survives a round trip, and every record is 8-byte aligned.
    #[test]
    fn every_kind_round_trips() {
        for body in every_body() {
            let bytes = encoded(body);
            assert_eq!(bytes.len() % ALIGN, 0, "{body:?} is not aligned");
            let record = Record::decode(&bytes)
                .expect("the record decodes")
                .expect("the record is there");
            assert_eq!(record.lsn, 42);
            assert_eq!(record.txn, 8);
            assert_eq!(record.body, body);
            assert_eq!(record.length, bytes.len());
            assert_eq!(record.body.kind(), body.kind());
        }
    }

    /// Every kind names the pages its redo would write.
    #[test]
    fn the_page_list_names_what_redo_touches() {
        let mut with_pages = 0usize;
        for body in every_body() {
            let bytes = encoded(body);
            let record = Record::decode(&bytes).unwrap().unwrap();
            let pages = record.pages();
            match body {
                Body::InsertRow { page, .. }
                | Body::DeleteRow { page, .. }
                | Body::UpdateInPlace { page, .. }
                | Body::CompactLeaf { page, .. }
                | Body::WritePage { page, .. } => {
                    assert_eq!(pages.as_slice(), &[page]);
                    with_pages += 1;
                }
                Body::Structural {
                    left,
                    right,
                    parent,
                    ..
                } => {
                    assert_eq!(pages.as_slice(), &[left, right, parent]);
                    with_pages += 1;
                }
                _ => assert!(pages.as_slice().is_empty(), "{body:?} named a page"),
            }
        }
        assert!(with_pages >= 7, "only {with_pages} kinds name a page");
    }

    /// Every truncation of every record is refused, and none of them panics.
    ///
    /// This is the sweep that reaches the bounds check in every field of every
    /// kind: a prefix one byte short of some field's end is exactly the input
    /// that check exists for, and there is one such prefix per field.
    #[test]
    fn every_truncation_is_refused_and_none_panics() {
        for body in every_body() {
            let bytes = encoded(body);
            for cut in 0..bytes.len() {
                let prefix = bytes.get(..cut).unwrap();
                match Record::decode(prefix) {
                    // A prefix shorter than a header is "no record", which is
                    // how a scan finds the end of a segment.
                    Ok(None) => assert!(cut < HEADER_BYTES, "{body:?} cut at {cut} read as empty"),
                    Ok(Some(_)) => panic!("{body:?} cut at {cut} decoded as a whole record"),
                    Err(_) => assert!(cut >= HEADER_BYTES, "{body:?} cut at {cut} errored early"),
                }
            }
        }
    }

    /// A byte flipped anywhere in a record is caught by the checksum.
    #[test]
    fn a_flipped_byte_is_caught() {
        for body in every_body() {
            let bytes = encoded(body);
            for index in at::CHECKSUMMED_FROM..bytes.len() {
                let mut damaged = bytes.clone();
                // Indexed rather than `get_mut`, and that is not a shortcut.
                // `index` came from a range over this buffer's own length, so
                // the `None` arm of a `get_mut` is a branch no input can take -
                // and a branch no input can take is one the coverage gate can
                // only ever be lied to about. The test module is held to the
                // same rule as the code it tests.
                damaged[index] ^= 0xFF;
                assert!(
                    Record::decode(&damaged).is_err(),
                    "{body:?} survived a flip at {index}"
                );
            }
        }
    }

    /// An empty buffer and an all-zero buffer are both "no record".
    #[test]
    fn nothing_and_zeroes_are_both_the_end_of_the_log() {
        assert_eq!(Record::decode(&[]).unwrap(), None);
        assert_eq!(Record::decode(&[0u8; 7]).unwrap(), None);
        assert_eq!(Record::decode(&[0u8; 64]).unwrap(), None);
    }

    /// A length that is not a legal record length is refused before anything
    /// is read from it.
    #[test]
    fn an_illegal_length_is_refused() {
        let good = encoded(Body::Commit { cts: 1 });
        for length in [1u32, 31, 33, 35, (MAX_RECORD_BYTES + 8) as u32] {
            let mut bytes = good.clone();
            put_u32(&mut bytes, at::LENGTH, length).unwrap();
            let error = Record::decode(&bytes).expect_err("an illegal length is refused");
            assert!(error
                .detail()
                .unwrap_or_default()
                .contains("not a legal record length"));
        }
    }

    /// A length longer than the buffer is refused rather than read past.
    #[test]
    fn a_length_past_the_buffer_is_refused() {
        let good = encoded(Body::Commit { cts: 1 });
        let mut bytes = good.clone();
        put_u32(&mut bytes, at::LENGTH, (good.len() + 8) as u32).unwrap();
        let error = Record::decode(&bytes).expect_err("a long length is refused");
        assert!(error
            .detail()
            .unwrap_or_default()
            .contains("more bytes than the segment holds"));
    }

    /// A reserved byte that is not zero is refused.
    #[test]
    fn a_dirty_reserved_byte_is_refused() {
        for offset in 0..7usize {
            let mut bytes = encoded(Body::Commit { cts: 1 });
            bytes[at::RESERVED + offset] = 1;
            // Re-checksum so the reserved check is what fires, not the crc.
            let sum = crc32(bytes.get(at::CHECKSUMMED_FROM..).unwrap());
            put_u32(&mut bytes, at::CHECKSUM, sum).unwrap();
            let error = Record::decode(&bytes).expect_err("a dirty reserved byte is refused");
            assert!(error
                .detail()
                .unwrap_or_default()
                .contains("reserved bytes are not zero"));
        }
    }

    /// Padding that is not zero is refused.
    ///
    /// `Commit` is four bytes short of an alignment boundary, so it always has
    /// padding to damage; a record whose payload lands on a boundary has none,
    /// and that is why this test names its record rather than sweeping.
    #[test]
    fn dirty_padding_is_refused() {
        let mut bytes = encoded(Body::CatalogChange { delta: b"abc" });
        let last = bytes.len().saturating_sub(1);
        bytes[last] = 0xAB;
        let sum = crc32(bytes.get(at::CHECKSUMMED_FROM..).unwrap());
        put_u32(&mut bytes, at::CHECKSUM, sum).unwrap();
        let error = Record::decode(&bytes).expect_err("dirty padding is refused");
        assert!(error.detail().unwrap_or_default().contains("padding"));
    }

    /// A record that declares more bytes than its payload uses is refused.
    ///
    /// The padding may be up to seven bytes; eight or more means the length and
    /// the payload disagree about where the record ends, which is a different
    /// damage from a flipped byte and has its own message.
    #[test]
    fn a_payload_that_underfills_its_length_is_refused() {
        let mut bytes = encoded(Body::Commit { cts: 1 });
        bytes.extend_from_slice(&[0u8; 8]);
        let length = bytes.len() as u32;
        put_u32(&mut bytes, at::LENGTH, length).unwrap();
        let sum = crc32(bytes.get(at::CHECKSUMMED_FROM..).unwrap());
        put_u32(&mut bytes, at::CHECKSUM, sum).unwrap();
        let error = Record::decode(&bytes).expect_err("an underfilled record is refused");
        assert!(error.detail().unwrap_or_default().contains("padding"));
    }

    /// A kind byte the format does not define is refused, and every kind byte
    /// the format does define is not.
    #[test]
    fn an_unknown_kind_is_refused_and_a_known_one_is_not() {
        let defined: Vec<u8> = every_body().iter().map(Body::kind).collect();
        for candidate in 0u8..=255 {
            let mut bytes = encoded(Body::Structural {
                kind: Structural::Split,
                tree: 1,
                left: 2,
                right: 3,
                parent: 4,
                left_image: b"aaaaaaaa",
                right_image: b"bbbbbbbb",
                parent_image: b"cccccccc",
            });
            put_u8(&mut bytes, at::KIND, candidate).unwrap();
            let sum = crc32(bytes.get(at::CHECKSUMMED_FROM..).unwrap());
            put_u32(&mut bytes, at::CHECKSUM, sum).unwrap();
            let outcome = Record::decode(&bytes);
            if defined.contains(&candidate) {
                // A defined kind may still fail: the payload was written for a
                // structural record and another kind reads it differently. What
                // matters is that it never reports the *unknown kind* error.
                if let Err(error) = outcome {
                    assert!(
                        !error
                            .detail()
                            .unwrap_or_default()
                            .contains("this format does not define"),
                        "kind {candidate} is defined and was called unknown"
                    );
                }
            } else {
                let error = outcome.expect_err("an unknown kind is refused");
                assert!(error
                    .detail()
                    .unwrap_or_default()
                    .contains("this format does not define"));
            }
        }
    }

    /// A record past the ceiling is refused by the encoder rather than written.
    #[test]
    fn an_oversized_record_is_refused_by_the_encoder() {
        let image = vec![0u8; MAX_RECORD_BYTES];
        let mut out = vec![0xAAu8; 16];
        let error = Record {
            lsn: 1,
            txn: 1,
            body: Body::WritePage {
                page: 1,
                image: &image,
            },
            length: 0,
        }
        .encode(&mut out)
        .expect_err("an oversized record is refused");
        assert!(error.detail().unwrap_or_default().contains("ceiling"));
        assert_eq!(out, vec![0xAAu8; 16], "the buffer was left as it was found");
    }

    /// Records encode back to back and decode back to back.
    #[test]
    fn records_pack_end_to_end() {
        let mut out = Vec::new();
        let bodies = every_body();
        for (index, body) in bodies.iter().enumerate() {
            Record {
                lsn: index as u64 + 1,
                txn: 3,
                body: *body,
                length: 0,
            }
            .encode(&mut out)
            .unwrap();
        }
        let mut at = 0usize;
        let mut seen = 0usize;
        while let Some(record) = Record::decode(out.get(at..).unwrap_or(&[])).unwrap() {
            assert_eq!(record.lsn, seen as u64 + 1);
            assert_eq!(record.body, *bodies.get(seen).unwrap());
            at = at.saturating_add(record.length);
            seen = seen.saturating_add(1);
        }
        assert_eq!(seen, bodies.len());
        assert_eq!(at, out.len());
    }

    /// The write helpers refuse an offset past the buffer rather than panic.
    ///
    /// They are only ever called on a buffer this module just sized, so the
    /// failure is unreachable in the encoder - which is precisely why it is
    /// asserted here rather than left as a branch nothing takes.
    #[test]
    fn the_header_writers_refuse_a_short_buffer() {
        let mut short = [0u8; 2];
        assert!(put_u8(&mut short, 8, 1).is_err());
        assert!(put_u32(&mut short, 0, 1).is_err());
        assert!(put_u64(&mut short, 0, 1).is_err());
        assert!(read_u8(&short, 8).is_err());
        assert!(read_u32(&short, 0).is_err());
        assert!(read_u64(&short, 0).is_err());
    }

    /// The stable counterpart of the `wal_record` fuzz target.
    ///
    /// Walks a damaged buffer as a *stream*, the way recovery does, and holds
    /// the target's distinctive claim: **anything that decodes re-encodes to
    /// the bytes it came from**. A decoder that accepted a record its own
    /// encoder could not have written would be accepting a shape the format
    /// does not define, and no amount of "it returned an error eventually"
    /// would make that safe.
    #[test]
    fn a_damaged_record_stream_never_panics_and_never_decodes_a_shape_we_cannot_write() {
        let mut stream = Vec::new();
        for (index, body) in every_body().into_iter().enumerate() {
            Record {
                lsn: 8 + index as u64 * 64,
                txn: index as u64 % 3,
                body,
                length: 0,
            }
            .encode(&mut stream)
            .unwrap();
        }
        let mut decoded_anything = 0usize;
        for index in 0..stream.len() {
            for delta in [0x01u8, 0x5A, 0xFF] {
                let mut damaged = stream.clone();
                damaged[index] = damaged[index].wrapping_add(delta);
                let mut at = 0usize;
                let mut seen = 0usize;
                while let Ok(Some(record)) = Record::decode(damaged.get(at..).unwrap_or(&[])) {
                    assert!(record.length >= HEADER_BYTES);
                    let _ = record.pages();
                    let mut again = Vec::new();
                    // Not `if ... .is_ok()`: a record that decoded is already
                    // inside the ceiling the encoder checks, so a failure here
                    // is impossible rather than merely unlikely, and writing it
                    // as a condition would add a branch nothing can take.
                    record
                        .encode(&mut again)
                        .expect("a decoded record re-encodes");
                    assert_eq!(
                        again.as_slice(),
                        damaged.get(at..at + record.length).unwrap_or(&[]),
                        "a record at {at} did not re-encode to its own bytes"
                    );
                    at = at.saturating_add(record.length);
                    seen = seen.saturating_add(1);
                    decoded_anything = decoded_anything.saturating_add(1);
                    assert!(seen < 1_000, "the stream walk did not terminate");
                }
            }
        }
        assert!(
            decoded_anything > 100,
            "only {decoded_anything} records decoded across the sweep, so it barely ran"
        );
    }

    /// An empty page list is empty and a full one is three long.
    #[test]
    fn the_page_list_holds_none_one_or_three() {
        assert!(PageList::none().as_slice().is_empty());
        assert_eq!(PageList::one(5).as_slice(), &[5]);
        assert_eq!(PageList::three(1, 2, 3).as_slice(), &[1, 2, 3]);
    }
}
