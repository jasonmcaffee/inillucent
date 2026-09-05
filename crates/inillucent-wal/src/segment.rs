//! Segments: the files the log is written into, and the header that says which
//! database each one belongs to.
//!
//! Invariant: a segment whose magic, format, uuid or sequence does not match
//! what the caller expects is **refused**, not skipped and not read anyway. A
//! log is the one structure in the engine whose contents are applied to a file
//! without anybody looking at them, so a segment from another database - a
//! leftover from a restore, a file copied beside the wrong `.rdb` - is the
//! worst input the engine can be handed and the cheapest one to reject.
//!
//! ## The layout
//!
//! ```text
//! 0   8 bytes  magic  b"RDBWAL01"
//! 8   u32      format version
//! 12  u32      crc32c over bytes 16..48
//! 16  u64      segment sequence
//! 24  u64      the lsn of the first record in this segment
//! 32  u128     the database uuid
//! 48           reserved, zero, to the end of the header
//! 64           the first record
//! ```
//!
//! The checksum sits before the fields it covers rather than after them, which
//! is the opposite of the record header's arrangement and is deliberate: a
//! segment header is a fixed size, so there is no length in front of the
//! checksum for a damaged value to invalidate, and putting the checksum at a
//! fixed early offset means the whole header is one contiguous checked region.

use inillucent_base::checksum::crc32;
use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

/// The eight bytes every WAL segment begins with.
pub const MAGIC: [u8; 8] = *b"RDBWAL01";

/// The format version this build writes and is the only one it reads.
pub const FORMAT_VERSION: u32 = 1;

/// The size of a segment header, including its reserved tail.
///
/// Forty-eight bytes are used and sixty-four are reserved, so that the first
/// record starts on a cache line as well as on the eight-byte boundary the
/// record format requires.
pub const HEADER_BYTES: usize = 64;

/// How large a segment is allowed to grow before the log rolls to the next one.
///
/// The TDD's 64 MiB. It is a policy rather than a format constant - a reader
/// finds the end of a segment by decoding until a record fails, not by reaching
/// this number - so a database written by a build with a different value still
/// reads.
pub const SEGMENT_BYTES: u64 = 64 << 20;

/// Byte offsets inside a segment header.
mod at {
    /// The magic, 8 bytes.
    pub const MAGIC: usize = 0;
    /// The format version, 4 bytes.
    pub const FORMAT: usize = 8;
    /// The checksum over [`CHECKSUMMED`], 4 bytes.
    pub const CHECKSUM: usize = 12;
    /// The segment's sequence number, 8 bytes.
    pub const SEQUENCE: usize = 16;
    /// The lsn of the segment's first record, 8 bytes.
    pub const FIRST_LSN: usize = 24;
    /// The database uuid, 16 bytes.
    pub const UUID: usize = 32;
    /// The range the checksum covers.
    pub const CHECKSUMMED: std::ops::Range<usize> = 16..48;
}

/// What a segment header says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentHeader {
    /// Which segment of the chain this is, counting from one.
    pub sequence: u64,
    /// The lsn of the first record in the segment.
    pub first_lsn: u64,
    /// The database this segment belongs to.
    pub uuid: u128,
}

impl SegmentHeader {
    /// Writes the header into a buffer at least [`HEADER_BYTES`] long.
    ///
    /// @param out - the buffer to write into
    pub fn encode(&self, out: &mut [u8]) -> DbResult<()> {
        let header = out
            .get_mut(..HEADER_BYTES)
            .ok_or_else(|| misuse("a segment header needs 64 bytes"))?;
        header.fill(0);
        write_at(header, at::MAGIC, &MAGIC)?;
        write_at(header, at::FORMAT, &FORMAT_VERSION.to_le_bytes())?;
        write_at(header, at::SEQUENCE, &self.sequence.to_le_bytes())?;
        write_at(header, at::FIRST_LSN, &self.first_lsn.to_le_bytes())?;
        write_at(header, at::UUID, &self.uuid.to_le_bytes())?;
        let sum = crc32(
            header
                .get(at::CHECKSUMMED)
                .ok_or_else(|| misuse("a segment header needs 64 bytes"))?,
        );
        write_at(header, at::CHECKSUM, &sum.to_le_bytes())?;
        Ok(())
    }

    /// Reads a header, refusing anything that is not one.
    ///
    /// @param bytes - the segment's first bytes
    pub fn decode(bytes: &[u8]) -> DbResult<SegmentHeader> {
        let header = bytes
            .get(..HEADER_BYTES)
            .ok_or_else(|| corrupt("a WAL segment is shorter than its own header"))?;
        if header.get(at::MAGIC..at::MAGIC.saturating_add(8)) != Some(&MAGIC[..]) {
            return Err(corrupt("a WAL segment does not begin with the WAL magic"));
        }
        let format = read_u32(header, at::FORMAT)?;
        if format != FORMAT_VERSION {
            return Err(corrupt(format!(
                "a WAL segment is format {format} and this build reads {FORMAT_VERSION}"
            )));
        }
        let stated = read_u32(header, at::CHECKSUM)?;
        let computed = crc32(
            header
                .get(at::CHECKSUMMED)
                .ok_or_else(|| corrupt("a WAL segment is shorter than its own header"))?,
        );
        if stated != computed {
            return Err(corrupt(
                "a WAL segment header's checksum does not match its bytes",
            ));
        }
        Ok(SegmentHeader {
            sequence: read_u64(header, at::SEQUENCE)?,
            first_lsn: read_u64(header, at::FIRST_LSN)?,
            uuid: read_u128(header, at::UUID)?,
        })
    }

    /// Refuses a header that does not belong to the database being opened.
    ///
    /// The two checks are separate errors because they mean different things: a
    /// uuid mismatch is a segment from another database and is never
    /// recoverable, while a sequence mismatch is a gap in the chain and says
    /// which segment is missing.
    ///
    /// @param uuid - the database's uuid, from its meta page
    /// @param sequence - the sequence this segment must have
    pub fn belongs_to(&self, uuid: u128, sequence: u64) -> DbResult<()> {
        if self.uuid != uuid {
            return Err(corrupt(format!(
                "WAL segment {} belongs to another database",
                self.sequence
            )));
        }
        if self.sequence != sequence {
            return Err(corrupt(format!(
                "the WAL chain wants segment {sequence} and this file is segment {}",
                self.sequence
            )));
        }
        Ok(())
    }
}

/// Returns the file name of one segment.
///
/// The sequence is zero-padded so that a directory listing sorts into log
/// order, which matters only to a person reading one - the chain is followed by
/// sequence number, never by name order.
///
/// @param base - the database file's name
/// @param sequence - which segment
pub fn segment_name(base: &str, sequence: u64) -> String {
    format!("{base}-wal.{sequence:010}")
}

/// Writes `value` into `buffer` at `at`.
///
/// @param buffer - the bytes
/// @param at - the offset
/// @param value - what to write
fn write_at(buffer: &mut [u8], at: usize, value: &[u8]) -> DbResult<()> {
    let slice = buffer
        .get_mut(at..at.saturating_add(value.len()))
        .ok_or_else(|| misuse(format!("a segment header ends before offset {at}")))?;
    slice.copy_from_slice(value);
    Ok(())
}

/// Reads a little-endian `u32`.
///
/// @param buffer - the bytes
/// @param at - the offset
fn read_u32(buffer: &[u8], at: usize) -> DbResult<u32> {
    let slice = buffer
        .get(at..at.saturating_add(4))
        .ok_or_else(|| corrupt(format!("a segment header ends before offset {at}")))?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(slice);
    Ok(u32::from_le_bytes(raw))
}

/// Reads a little-endian `u64`.
///
/// @param buffer - the bytes
/// @param at - the offset
fn read_u64(buffer: &[u8], at: usize) -> DbResult<u64> {
    let slice = buffer
        .get(at..at.saturating_add(8))
        .ok_or_else(|| corrupt(format!("a segment header ends before offset {at}")))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(slice);
    Ok(u64::from_le_bytes(raw))
}

/// Reads a little-endian `u128`.
///
/// @param buffer - the bytes
/// @param at - the offset
fn read_u128(buffer: &[u8], at: usize) -> DbResult<u128> {
    let slice = buffer
        .get(at..at.saturating_add(16))
        .ok_or_else(|| corrupt(format!("a segment header ends before offset {at}")))?;
    let mut raw = [0u8; 16];
    raw.copy_from_slice(slice);
    Ok(u128::from_le_bytes(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header round trips through its own bytes.
    #[test]
    fn a_header_round_trips() {
        let header = SegmentHeader {
            sequence: 3,
            first_lsn: 4_096,
            uuid: 0x0123_4567_89AB_CDEF_0123_4567_89AB_CDEF,
        };
        let mut bytes = vec![0xFFu8; HEADER_BYTES];
        header.encode(&mut bytes).unwrap();
        assert_eq!(SegmentHeader::decode(&bytes).unwrap(), header);
        // The reserved tail is zero, so a field added later cannot collide with
        // whatever happened to be in the buffer.
        assert!(bytes.get(48..).unwrap().iter().all(|byte| *byte == 0));
    }

    /// A buffer that is not a segment is refused at each of its four gates.
    #[test]
    fn a_file_that_is_not_a_segment_is_refused() {
        let header = SegmentHeader {
            sequence: 1,
            first_lsn: 1,
            uuid: 9,
        };
        let mut good = vec![0u8; HEADER_BYTES];
        header.encode(&mut good).unwrap();

        assert!(SegmentHeader::decode(&good[..32]).is_err(), "too short");

        let mut wrong_magic = good.clone();
        wrong_magic[0] = b'X';
        assert!(SegmentHeader::decode(&wrong_magic).is_err(), "magic");

        let mut wrong_format = good.clone();
        wrong_format[at::FORMAT] = 9;
        assert!(SegmentHeader::decode(&wrong_format).is_err(), "format");

        let mut wrong_sum = good.clone();
        wrong_sum[at::SEQUENCE] ^= 0xFF;
        assert!(SegmentHeader::decode(&wrong_sum).is_err(), "checksum");
    }

    /// A segment from another database, and one out of sequence, are refused
    /// with different messages.
    #[test]
    fn a_foreign_or_out_of_order_segment_is_refused() {
        let header = SegmentHeader {
            sequence: 4,
            first_lsn: 1,
            uuid: 11,
        };
        assert!(header.belongs_to(11, 4).is_ok());
        let foreign = header.belongs_to(12, 4).expect_err("a foreign segment");
        assert!(foreign
            .detail()
            .unwrap_or_default()
            .contains("another database"));
        let gap = header.belongs_to(11, 5).expect_err("a gap in the chain");
        assert!(gap.detail().unwrap_or_default().contains("wants segment 5"));
    }

    /// The encoder refuses a buffer that cannot hold a header.
    #[test]
    fn the_encoder_refuses_a_short_buffer() {
        let header = SegmentHeader {
            sequence: 1,
            first_lsn: 1,
            uuid: 1,
        };
        let mut short = [0u8; 16];
        assert!(header.encode(&mut short).is_err());
    }

    /// Segment names sort into log order.
    #[test]
    fn segment_names_sort_into_log_order() {
        let mut names = vec![
            segment_name("db.rdb", 10),
            segment_name("db.rdb", 2),
            segment_name("db.rdb", 1),
        ];
        names.sort();
        assert_eq!(
            names,
            vec![
                "db.rdb-wal.0000000001",
                "db.rdb-wal.0000000002",
                "db.rdb-wal.0000000010"
            ]
        );
    }
}
