//! The write-ahead log file format: its header, its frames, and the rolling
//! checksum that ties them together.
//!
//! Invariant: a frame is valid only as the continuation of every frame before
//! it. The checksum in a frame header is computed over the running pair the
//! previous frame left, so a frame lifted out of one log and dropped into
//! another fails, a frame whose page data was torn fails, and a log whose tail
//! was half-written stops being readable at exactly the first byte that was
//! not fully written. That is what makes recovery a scan with no ambiguity
//! rather than a judgement call.
//!
//! The salts are the second half of the argument. Every frame repeats the two
//! salt values from the file header, so a frame left over from before a log
//! was restarted - the file is reused in place, not deleted - is recognised
//! and stops the scan even when its checksum happens to chain correctly.
//!
//! Byte order is a property of the file, not of the reader. The magic's low
//! bit says whether the checksum words are read big-endian, so a log written
//! on one machine verifies on another. inillucent writes the host's own order,
//! which is what SQLite does, so that a log this engine wrote is byte-for-byte
//! the log SQLite would have written.
//!
//! Reference: <https://sqlite.org/fileformat2.html#walformat>.

use inillucent_base::checksum::{WalByteOrder, WalChecksum};
use inillucent_base::error::corrupt;
use inillucent_base::page::PageSize;
use inillucent_base::{bytes, DbResult};

/// The size of the log file header.
pub const WAL_HEADER_SIZE: usize = 32;

/// The size of the header on every frame.
pub const WAL_FRAME_HEADER_SIZE: usize = 24;

/// The magic that opens a log whose checksums are read little-endian.
pub const WAL_MAGIC_LITTLE: u32 = 0x377f_0682;

/// The magic that opens a log whose checksums are read big-endian.
pub const WAL_MAGIC_BIG: u32 = 0x377f_0683;

/// The file-format version inillucent writes and is willing to read.
pub const WAL_FORMAT_VERSION: u32 = 3_007_000;

/// Returns the byte order this host writes its checksums in.
///
/// SQLite stamps the building machine's endianness into the magic, so a log
/// written here has to declare the same thing to be the same bytes.
///
/// Coverage note: exactly one arm of this is reachable on any given build, and
/// the other is dead code the compiler keeps for the platform it is not. On the
/// little-endian targets this engine is qualified on - `windows-x86_64` and
/// `linux-x86_64` - the `Big` arm is unreachable and no test can take it. That
/// is a documented unreachable branch rather than a missing test; the branch it
/// guards is exercised from the other direction by the decoder, which reads
/// both magics and is tested against each.
pub fn host_byte_order() -> WalByteOrder {
    if cfg!(target_endian = "big") {
        WalByteOrder::Big
    } else {
        WalByteOrder::Little
    }
}

/// Returns the magic that declares a byte order.
pub fn magic_for(order: WalByteOrder) -> u32 {
    match order {
        WalByteOrder::Big => WAL_MAGIC_BIG,
        WalByteOrder::Little => WAL_MAGIC_LITTLE,
    }
}

/// The log file's header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalHeader {
    /// Which order the checksum words are read in.
    pub order: WalByteOrder,
    /// The declared file-format version.
    pub version: u32,
    /// The page size every frame in this log carries.
    pub page_size: PageSize,
    /// How many times the log has been restarted since it was created.
    pub checkpoint_sequence: u32,
    /// The first salt, which every frame repeats.
    pub salt: [u8; 8],
    /// The checksum of the first 24 bytes, which the first frame continues.
    pub checksum: WalChecksum,
}

impl WalHeader {
    /// Builds a header for a fresh log.
    ///
    /// The checksum is computed rather than passed in, because a header whose
    /// checksum did not describe its own bytes would be a log no reader could
    /// open, and there is no legitimate caller that wants one.
    pub fn new(
        page_size: PageSize,
        checkpoint_sequence: u32,
        salt: [u8; 8],
        order: WalByteOrder,
    ) -> DbResult<WalHeader> {
        let mut header = WalHeader {
            order,
            version: WAL_FORMAT_VERSION,
            page_size,
            checkpoint_sequence,
            salt,
            checksum: WalChecksum::default(),
        };
        let mut raw = [0u8; WAL_HEADER_SIZE];
        header.encode_fields(&mut raw)?;
        header.checksum = WalChecksum::default().extended(bytes::window(&raw, 0, 24)?, order)?;
        Ok(header)
    }

    /// Writes the first 24 bytes, which are everything the checksum covers.
    fn encode_fields(&self, target: &mut [u8]) -> DbResult<()> {
        bytes::write_u32(target, 0, magic_for(self.order))?;
        bytes::write_u32(target, 4, self.version)?;
        bytes::write_u32(target, 8, self.page_size.bytes())?;
        bytes::write_u32(target, 12, self.checkpoint_sequence)?;
        let salt = bytes::window_mut(target, 16, 8)?;
        salt.copy_from_slice(&self.salt);
        Ok(())
    }

    /// Encodes the whole 32-byte header.
    pub fn encode(&self) -> DbResult<[u8; WAL_HEADER_SIZE]> {
        let mut raw = [0u8; WAL_HEADER_SIZE];
        self.encode_fields(&mut raw)?;
        bytes::write_u32(&mut raw, 24, self.checksum.s0)?;
        bytes::write_u32(&mut raw, 28, self.checksum.s1)?;
        Ok(raw)
    }

    /// Decodes a header, or reports why the log cannot be read.
    ///
    /// A log that fails any of these checks is not corruption to report to the
    /// caller: SQLite treats an unreadable header as an empty log and starts
    /// again, because a log is a cache of a database that is itself complete.
    /// The error is therefore for the recovery path to interpret, not to
    /// propagate.
    pub fn decode(raw: &[u8]) -> DbResult<WalHeader> {
        if raw.len() < WAL_HEADER_SIZE {
            return Err(corrupt("a write-ahead log shorter than its header"));
        }
        let magic = bytes::read_u32(raw, 0)?;
        let order = match magic {
            WAL_MAGIC_BIG => WalByteOrder::Big,
            WAL_MAGIC_LITTLE => WalByteOrder::Little,
            _ => return Err(corrupt("a write-ahead log with an unrecognised magic")),
        };
        let version = bytes::read_u32(raw, 4)?;
        if version != WAL_FORMAT_VERSION {
            return Err(corrupt(format!(
                "a write-ahead log of format version {version}"
            )));
        }
        let page_size = PageSize::new(bytes::read_u32(raw, 8)?)?;
        let mut salt = [0u8; 8];
        salt.copy_from_slice(bytes::window(raw, 16, 8)?);
        let header = WalHeader {
            order,
            version,
            page_size,
            checkpoint_sequence: bytes::read_u32(raw, 12)?,
            salt,
            checksum: WalChecksum::new(bytes::read_u32(raw, 24)?, bytes::read_u32(raw, 28)?),
        };
        let computed = WalChecksum::default().extended(bytes::window(raw, 0, 24)?, order)?;
        if computed != header.checksum {
            return Err(corrupt("a write-ahead log header whose checksum is wrong"));
        }
        Ok(header)
    }

    /// Returns the salt values a frame of this log must repeat.
    pub fn salt(&self) -> [u8; 8] {
        self.salt
    }

    /// Returns the header a restart produces: one more checkpoint sequence
    /// number, the first salt incremented, and a fresh second salt.
    ///
    /// Incrementing rather than randomising the first salt is what SQLite
    /// does, and it is the more useful of the two: a log that was restarted is
    /// distinguishable from one that was created, and by how many times.
    pub fn restarted(&self, fresh_salt: [u8; 4]) -> DbResult<WalHeader> {
        let previous = bytes::read_u32(&self.salt, 0)?;
        let mut salt = [0u8; 8];
        bytes::write_u32(&mut salt, 0, previous.wrapping_add(1))?;
        let tail = bytes::window_mut(&mut salt, 4, 4)?;
        tail.copy_from_slice(&fresh_salt);
        WalHeader::new(
            self.page_size,
            self.checkpoint_sequence.wrapping_add(1),
            salt,
            self.order,
        )
    }
}

/// One frame's header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameHeader {
    /// Which database page the frame holds.
    pub page: u32,
    /// How many pages the database has once this frame is applied, or zero
    /// when the frame is not the last of a transaction.
    pub commit_page_count: u32,
    /// The salt values copied from the log header.
    pub salt: [u8; 8],
    /// The running checksum through the end of this frame's page data.
    pub checksum: WalChecksum,
}

impl FrameHeader {
    /// Encodes a frame header over its page image, continuing `previous`.
    pub fn encode(
        page: u32,
        commit_page_count: u32,
        salt: [u8; 8],
        image: &[u8],
        previous: WalChecksum,
        order: WalByteOrder,
    ) -> DbResult<[u8; WAL_FRAME_HEADER_SIZE]> {
        let mut raw = [0u8; WAL_FRAME_HEADER_SIZE];
        bytes::write_u32(&mut raw, 0, page)?;
        bytes::write_u32(&mut raw, 4, commit_page_count)?;
        let target = bytes::window_mut(&mut raw, 8, 8)?;
        target.copy_from_slice(&salt);
        let checksum = previous
            .extended(bytes::window(&raw, 0, 8)?, order)?
            .extended(image, order)?;
        bytes::write_u32(&mut raw, 16, checksum.s0)?;
        bytes::write_u32(&mut raw, 20, checksum.s1)?;
        Ok(raw)
    }

    /// Decodes a frame header without checking anything about it.
    pub fn decode(raw: &[u8]) -> DbResult<FrameHeader> {
        if raw.len() < WAL_FRAME_HEADER_SIZE {
            return Err(corrupt("a write-ahead log frame shorter than its header"));
        }
        let mut salt = [0u8; 8];
        salt.copy_from_slice(bytes::window(raw, 8, 8)?);
        Ok(FrameHeader {
            page: bytes::read_u32(raw, 0)?,
            commit_page_count: bytes::read_u32(raw, 4)?,
            salt,
            checksum: WalChecksum::new(bytes::read_u32(raw, 16)?, bytes::read_u32(raw, 20)?),
        })
    }

    /// Reports whether this frame is a valid continuation of `previous`.
    ///
    /// Three things have to hold at once: the salts must be the ones the
    /// current header carries, so a frame from before a restart is rejected;
    /// the page number must be a real one; and the checksum must continue the
    /// running pair. A frame that fails any of them ends the log, and every
    /// frame after it is unreachable whether or not it would have verified.
    pub fn continues(
        &self,
        raw: &[u8],
        image: &[u8],
        previous: WalChecksum,
        salt: [u8; 8],
        order: WalByteOrder,
    ) -> DbResult<bool> {
        if self.salt != salt || self.page == 0 {
            return Ok(false);
        }
        let computed = previous
            .extended(bytes::window(raw, 0, 8)?, order)?
            .extended(image, order)?;
        Ok(computed == self.checksum)
    }

    /// Reports whether the frame ends a transaction.
    pub fn is_commit(&self) -> bool {
        self.commit_page_count != 0
    }
}

/// Returns the byte offset of a frame's header within the log file.
///
/// Frames are numbered from one, because zero is the value the wal-index uses
/// for "no frame", and an off-by-one here would hand a reader the frame before
/// the one it asked for.
pub fn frame_offset(frame: u32, page_size: PageSize) -> DbResult<u64> {
    if frame == 0 {
        return Err(corrupt("frame numbers in a write-ahead log start at one"));
    }
    let stride = u64::from(page_size.bytes()).saturating_add(WAL_FRAME_HEADER_SIZE as u64);
    let index = u64::from(frame.saturating_sub(1));
    Ok((WAL_HEADER_SIZE as u64).saturating_add(index.saturating_mul(stride)))
}

/// Returns how many whole frames a log file of `bytes` bytes can hold.
pub fn frames_in_file(bytes: u64, page_size: PageSize) -> u32 {
    let stride = u64::from(page_size.bytes()).saturating_add(WAL_FRAME_HEADER_SIZE as u64);
    if stride == 0 || bytes <= WAL_HEADER_SIZE as u64 {
        return 0;
    }
    let payload = bytes.saturating_sub(WAL_HEADER_SIZE as u64);
    u32::try_from(payload / stride).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header round-trips, and its checksum covers the fields that decide
    /// how the rest of the file is read.
    #[test]
    fn a_header_round_trips_and_checks_itself() {
        let header = WalHeader::new(
            PageSize::new(4096).unwrap(),
            7,
            [1, 2, 3, 4, 5, 6, 7, 8],
            WalByteOrder::Little,
        )
        .unwrap();
        let raw = header.encode().unwrap();
        assert_eq!(raw.len(), WAL_HEADER_SIZE);
        let decoded = WalHeader::decode(&raw).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decoded.page_size.bytes(), 4096);
        assert_eq!(decoded.checkpoint_sequence, 7);
    }

    /// One flipped byte anywhere in the covered range fails the checksum.
    #[test]
    fn a_damaged_header_is_refused() {
        let header = WalHeader::new(
            PageSize::new(1024).unwrap(),
            0,
            [9; 8],
            WalByteOrder::Little,
        )
        .unwrap();
        for index in 0..24usize {
            let mut raw = header.encode().unwrap();
            if let Some(byte) = raw.get_mut(index) {
                *byte ^= 0x40;
            }
            assert!(
                WalHeader::decode(&raw).is_err(),
                "byte {index} was allowed to change"
            );
        }
    }

    /// The magic's low bit is the byte order, and the two spellings are the
    /// only ones a log may open with.
    #[test]
    fn the_magic_declares_the_byte_order() {
        for order in [WalByteOrder::Big, WalByteOrder::Little] {
            let header = WalHeader::new(PageSize::new(512).unwrap(), 1, [4; 8], order).unwrap();
            let raw = header.encode().unwrap();
            assert_eq!(bytes::read_u32(&raw, 0).unwrap(), magic_for(order));
            assert_eq!(WalHeader::decode(&raw).unwrap().order, order);
        }
        let mut raw = WalHeader::new(PageSize::DEFAULT, 0, [0; 8], WalByteOrder::Little)
            .unwrap()
            .encode()
            .unwrap();
        bytes::write_u32(&mut raw, 0, 0x1234_5678).unwrap();
        assert!(WalHeader::decode(&raw).is_err());
    }

    /// A frame verifies only as the continuation of the frame before it: the
    /// same bytes at a different point in the chain do not.
    #[test]
    fn a_frame_only_verifies_in_its_own_place() {
        let salt = [7u8; 8];
        let image = vec![0xabu8; 512];
        let first = WalChecksum::new(11, 22);
        let raw = FrameHeader::encode(3, 0, salt, &image, first, WalByteOrder::Little).unwrap();
        let header = FrameHeader::decode(&raw).unwrap();
        assert!(header
            .continues(&raw, &image, first, salt, WalByteOrder::Little)
            .unwrap());
        assert!(!header
            .continues(
                &raw,
                &image,
                WalChecksum::new(11, 23),
                salt,
                WalByteOrder::Little
            )
            .unwrap());
        assert!(!header
            .continues(&raw, &image, first, [8u8; 8], WalByteOrder::Little)
            .unwrap());
        let mut damaged = image.clone();
        if let Some(byte) = damaged.get_mut(100) {
            *byte ^= 1;
        }
        assert!(!header
            .continues(&raw, &damaged, first, salt, WalByteOrder::Little)
            .unwrap());
    }

    /// A commit frame is one with a page count, which is what ends a
    /// transaction during recovery.
    #[test]
    fn a_commit_frame_carries_the_new_page_count() {
        let raw = FrameHeader::encode(
            1,
            42,
            [0; 8],
            &[0u8; 512],
            WalChecksum::default(),
            WalByteOrder::Little,
        )
        .unwrap();
        let header = FrameHeader::decode(&raw).unwrap();
        assert!(header.is_commit());
        assert_eq!(header.commit_page_count, 42);
        let raw = FrameHeader::encode(
            1,
            0,
            [0; 8],
            &[0u8; 512],
            WalChecksum::default(),
            WalByteOrder::Little,
        )
        .unwrap();
        assert!(!FrameHeader::decode(&raw).unwrap().is_commit());
    }

    /// Frame offsets are the ones the format specifies, and frame zero does
    /// not exist.
    #[test]
    fn frame_offsets_follow_the_format() {
        let size = PageSize::new(1024).unwrap();
        assert!(frame_offset(0, size).is_err());
        assert_eq!(frame_offset(1, size).unwrap(), 32);
        assert_eq!(frame_offset(2, size).unwrap(), 32 + 1024 + 24);
        assert_eq!(frame_offset(3, size).unwrap(), 32 + 2 * (1024 + 24));
        assert_eq!(frames_in_file(32, size), 0);
        assert_eq!(frames_in_file(32 + 1048, size), 1);
        assert_eq!(frames_in_file(32 + 1048 + 10, size), 1);
        assert_eq!(frames_in_file(32 + 2 * 1048, size), 2);
    }

    /// A restart increments the first salt, replaces the second, and moves the
    /// checkpoint sequence on, so no frame of the old log can pass as one of
    /// the new.
    #[test]
    fn a_restart_changes_both_salts() {
        let header = WalHeader::new(
            PageSize::DEFAULT,
            2,
            [0, 0, 0, 5, 9, 9, 9, 9],
            WalByteOrder::Little,
        )
        .unwrap();
        let restarted = header.restarted([1, 2, 3, 4]).unwrap();
        assert_eq!(restarted.checkpoint_sequence, 3);
        assert_eq!(restarted.salt, [0, 0, 0, 6, 1, 2, 3, 4]);
        assert_ne!(restarted.checksum, header.checksum);
    }
    /// A buffer shorter than a header is not a header.
    ///
    /// The error path of the log's own decoder: a truncated log is the ordinary
    /// result of a crash while the header was being written, and it has to be
    /// reported as corruption rather than read out of whatever bytes are there.
    /// The branch was never taken by any test.
    #[test]
    fn a_log_header_shorter_than_a_header_is_corrupt() {
        for len in 0..WAL_HEADER_SIZE {
            let raw = vec![0u8; len];
            let failure = WalHeader::decode(&raw).expect_err("a short header cannot decode");
            assert_eq!(
                failure.code(),
                inillucent_base::error::PrimaryCode::Corrupt,
                "a {len}-byte log header is corrupt, not some other failure"
            );
        }
    }

    /// A buffer shorter than a frame header is not a frame header.
    ///
    /// The same for the frame decoder, and the same reason: a log whose last
    /// frame was half-written ends in exactly this.
    #[test]
    fn a_frame_header_shorter_than_a_frame_header_is_corrupt() {
        for len in 0..WAL_FRAME_HEADER_SIZE {
            let raw = vec![0u8; len];
            let failure = FrameHeader::decode(&raw).expect_err("a short frame cannot decode");
            assert_eq!(
                failure.code(),
                inillucent_base::error::PrimaryCode::Corrupt,
                "a {len}-byte frame header is corrupt, not some other failure"
            );
        }
    }

    /// The salt a header carries is the salt its frames must repeat.
    ///
    /// The accessor is what every frame is checked against, so a version
    /// returning something else would make a valid log look like one whose
    /// frames belong to a previous incarnation.
    #[test]
    fn the_header_reports_the_salt_its_frames_repeat() {
        let size = inillucent_base::page::PageSize::new(4096).expect("4096 is a page size");
        let salt = [9, 8, 7, 6, 5, 4, 3, 2];
        let header = WalHeader::new(size, 3, salt, WalByteOrder::Big).expect("the header is built");
        assert_eq!(header.salt(), salt);
        // And it survives the round trip, which is what a reader depends on.
        let raw = header.encode().expect("the header encodes");
        let read = WalHeader::decode(&raw).expect("the header decodes");
        assert_eq!(read.salt(), salt);
    }
}
