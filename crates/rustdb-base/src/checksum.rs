//! Checksums used by the file formats rust-db reads and writes.
//!
//! Invariant: a checksum function is a pure function of its input bytes and
//! their declared byte order. It never depends on the host's endianness, so a
//! file written on one machine verifies on another.
//!
//! Two algorithms live here. The WAL checksum is the one the SQLite file format
//! specifies for write-ahead log frames, and it is not a general-purpose hash:
//! it is a pair of 32-bit accumulators over 8-byte blocks, chosen for speed on
//! a write path. CRC-32 is used by rust-db's own artifacts, where the format is
//! ours and a standard check value is worth more than a fast one.

use crate::error::{corrupt, DbResult};

/// The byte order a WAL file declares in its header magic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalByteOrder {
    /// Words are read big-endian.
    Big,
    /// Words are read little-endian.
    Little,
}

/// The running pair of accumulators for the WAL frame checksum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct WalChecksum {
    /// The first accumulator, `s0` in the file-format description.
    pub s0: u32,
    /// The second accumulator, `s1` in the file-format description.
    pub s1: u32,
}

impl WalChecksum {
    /// Starts a checksum from an explicit pair, which is how a frame continues
    /// the checksum of the frame before it.
    pub fn new(s0: u32, s1: u32) -> WalChecksum {
        WalChecksum { s0, s1 }
    }

    /// Folds `data` into the checksum.
    ///
    /// The input must be a whole number of 8-byte blocks; the format never
    /// checksums a partial block, so a caller that asks to is confused about
    /// what it is checksumming and gets an error instead of a padded answer.
    pub fn update(&mut self, data: &[u8], order: WalByteOrder) -> DbResult<()> {
        if !data.len().is_multiple_of(8) {
            return Err(corrupt(
                "WAL checksum input is not a whole number of 8-byte blocks",
            ));
        }
        let (blocks, _) = data.as_chunks::<8>();
        for block in blocks {
            let (first, second) = split_block(block, order);
            self.s0 = self.s0.wrapping_add(first).wrapping_add(self.s1);
            self.s1 = self.s1.wrapping_add(second).wrapping_add(self.s0);
        }
        Ok(())
    }

    /// Folds `data` in and returns the new checksum, leaving the input alone.
    pub fn extended(self, data: &[u8], order: WalByteOrder) -> DbResult<WalChecksum> {
        let mut next = self;
        next.update(data, order)?;
        Ok(next)
    }
}

/// Splits an 8-byte block into its two 32-bit words in the declared order.
fn split_block(block: &[u8; 8], order: WalByteOrder) -> (u32, u32) {
    let word = |bytes: &[u8]| -> u32 {
        let mut value = [0u8; 4];
        for (slot, byte) in value.iter_mut().zip(bytes.iter()) {
            *slot = *byte;
        }
        match order {
            WalByteOrder::Big => u32::from_be_bytes(value),
            WalByteOrder::Little => u32::from_le_bytes(value),
        }
    };
    (
        word(block.get(..4).unwrap_or(&[])),
        word(block.get(4..).unwrap_or(&[])),
    )
}

/// The reflected CRC-32 lookup table for the ISO-HDLC polynomial.
///
/// Built at compile time so the binary carries no initialisation code and the
/// table cannot be corrupted at run time.
const CRC32_TABLE: [u32; 256] = build_crc32_table();

/// Builds the CRC-32 table with the reflected polynomial `0xedb88320`.
///
/// The loop counters and the table index are ordinary arithmetic rather than
/// the checked form the rest of the crate uses. This function runs at compile
/// time over a fixed 256-entry table, so an overflow or an out-of-range index
/// here is a build error rather than something a corrupt file could reach.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0usize;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

/// Computes CRC-32/ISO-HDLC over `data`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_continue(0, data)
}

/// Continues a CRC-32 from a previous result, for data arriving in pieces.
pub fn crc32_continue(previous: u32, data: &[u8]) -> u32 {
    let mut crc = !previous;
    for byte in data {
        let index = ((crc ^ u32::from(*byte)) & 0xff) as usize;
        let entry = match CRC32_TABLE.get(index) {
            Some(entry) => *entry,
            None => 0,
        };
        crc = entry ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::PrimaryCode;
    use crate::rng::Rng;

    /// The published check value for CRC-32/ISO-HDLC over "123456789".
    #[test]
    fn crc32_matches_the_published_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// Continuing a CRC over pieces must equal computing it over the whole.
    #[test]
    fn crc32_is_the_same_whether_it_arrives_whole_or_in_pieces() {
        let mut rng = Rng::new(0x1782_0003);
        let mut data = vec![0u8; 4096];
        rng.fill(&mut data);
        let whole = crc32(&data);
        let mut running = 0;
        for chunk in data.chunks(97) {
            running = crc32_continue(running, chunk);
        }
        assert_eq!(running, whole);
    }

    /// A one-bit change must change the CRC; this is the property the check
    /// value alone does not prove.
    #[test]
    fn crc32_detects_single_bit_flips() {
        let mut rng = Rng::new(0x1782_0004);
        let mut data = vec![0u8; 512];
        rng.fill(&mut data);
        let baseline = crc32(&data);
        // Every byte on an ordinary build; every sixteenth under Miri, which
        // interprets each of the four thousand checksums this otherwise takes
        // and turns a millisecond into many minutes. The property - a single
        // bit flip anywhere is visible - is what is being checked, and a
        // stride still checks it across the whole buffer.
        let stride = if cfg!(miri) { 16 } else { 1 };
        for index in (0..data.len()).step_by(stride) {
            for bit in 0..8 {
                data[index] ^= 1 << bit;
                assert_ne!(
                    crc32(&data),
                    baseline,
                    "flip at {index}:{bit} was invisible"
                );
                data[index] ^= 1 << bit;
            }
        }
    }

    /// The WAL checksum reads the same bytes differently in each byte order,
    /// which is exactly why the header records which one a file uses.
    #[test]
    fn wal_checksum_depends_on_the_declared_byte_order() {
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let big = WalChecksum::default()
            .extended(&data, WalByteOrder::Big)
            .unwrap();
        let little = WalChecksum::default()
            .extended(&data, WalByteOrder::Little)
            .unwrap();
        assert_ne!(big, little);
        assert_eq!(big, WalChecksum::new(0x0102_0304, 0x0608_0a0c));
    }

    /// Checksumming a partial block is a caller mistake, not a padded answer.
    #[test]
    fn wal_checksum_refuses_a_partial_block() {
        let error = WalChecksum::default()
            .update(&[0u8; 5], WalByteOrder::Big)
            .expect_err("five bytes is not a whole block");
        assert_eq!(error.code(), PrimaryCode::Corrupt);
    }

    /// Folding in two halves must equal folding the whole, because a frame's
    /// checksum continues the one before it rather than restarting.
    #[test]
    fn wal_checksum_chains_across_calls() {
        let mut rng = Rng::new(0x1782_0005);
        let mut data = vec![0u8; 1024];
        rng.fill(&mut data);
        let whole = WalChecksum::default()
            .extended(&data, WalByteOrder::Big)
            .unwrap();
        let mut running = WalChecksum::default();
        running.update(&data[..512], WalByteOrder::Big).unwrap();
        running.update(&data[512..], WalByteOrder::Big).unwrap();
        assert_eq!(running, whole);
    }
}
