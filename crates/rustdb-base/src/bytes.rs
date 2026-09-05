//! Checked big-endian integer codecs and bounds-checked cursors.
//!
//! Invariant: no function here reads or writes a byte it has not first proved
//! is inside the slice it was given. Every failure is a `DbError`, never a
//! panic, because the bytes come from a file another process may have
//! corrupted on purpose.
//!
//! The SQLite file format is big-endian throughout and uses 8, 16, 24, 32, 48
//! and 64-bit fields. The 24 and 48-bit widths have no Rust primitive, so they
//! are spelled out here rather than open-coded at each call site.

use crate::error::{corrupt, DbResult};

/// Returns the sub-slice at `offset` of length `len`, or `SQLITE_CORRUPT`.
///
/// This is the one place a range is turned into a slice, so the bounds check
/// exists exactly once instead of at every reader.
pub fn window(source: &[u8], offset: usize, len: usize) -> DbResult<&[u8]> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| corrupt("byte window offset overflowed"))?;
    source
        .get(offset..end)
        .ok_or_else(|| corrupt("byte window reaches past the end of the buffer"))
}

/// Returns the mutable sub-slice at `offset` of length `len`, or
/// `SQLITE_CORRUPT`.
pub fn window_mut(target: &mut [u8], offset: usize, len: usize) -> DbResult<&mut [u8]> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| corrupt("byte window offset overflowed"))?;
    target
        .get_mut(offset..end)
        .ok_or_else(|| corrupt("byte window reaches past the end of the buffer"))
}

/// Reads the unsigned byte at `offset`.
pub fn read_u8(source: &[u8], offset: usize) -> DbResult<u8> {
    let bytes = window(source, offset, 1)?;
    match bytes.first() {
        Some(byte) => Ok(*byte),
        None => Err(corrupt("one-byte window was empty")),
    }
}

/// Reads a big-endian 16-bit unsigned integer at `offset`.
pub fn read_u16(source: &[u8], offset: usize) -> DbResult<u16> {
    let bytes = window(source, offset, 2)?;
    Ok(fold_be(bytes) as u16)
}

/// Reads a big-endian 24-bit unsigned integer at `offset`.
///
/// The 24-bit width appears in the file format's payload fields, where a value
/// wider than 16 bits still has to fit in three bytes.
pub fn read_u24(source: &[u8], offset: usize) -> DbResult<u32> {
    let bytes = window(source, offset, 3)?;
    Ok(fold_be(bytes) as u32)
}

/// Reads a big-endian 32-bit unsigned integer at `offset`.
pub fn read_u32(source: &[u8], offset: usize) -> DbResult<u32> {
    let bytes = window(source, offset, 4)?;
    Ok(fold_be(bytes) as u32)
}

/// Reads a big-endian 48-bit unsigned integer at `offset`.
pub fn read_u48(source: &[u8], offset: usize) -> DbResult<u64> {
    let bytes = window(source, offset, 6)?;
    Ok(fold_be(bytes))
}

/// Reads a big-endian 64-bit unsigned integer at `offset`.
pub fn read_u64(source: &[u8], offset: usize) -> DbResult<u64> {
    let bytes = window(source, offset, 8)?;
    Ok(fold_be(bytes))
}

/// Reads a big-endian 64-bit signed integer at `offset`.
pub fn read_i64(source: &[u8], offset: usize) -> DbResult<i64> {
    Ok(read_u64(source, offset)? as i64)
}

/// Reads a big-endian IEEE-754 binary64 at `offset`, preserving the exact bit
/// pattern including signed zero, subnormals, and NaN payloads.
pub fn read_f64(source: &[u8], offset: usize) -> DbResult<f64> {
    Ok(f64::from_bits(read_u64(source, offset)?))
}

/// Folds up to eight big-endian bytes into a `u64`.
///
/// The caller has already sized the slice, so this cannot overflow: eight
/// shifts of eight bits stay inside 64.
fn fold_be(bytes: &[u8]) -> u64 {
    let mut value: u64 = 0;
    for byte in bytes.iter().take(8) {
        value = value.wrapping_shl(8) | u64::from(*byte);
    }
    value
}

/// Writes an unsigned byte at `offset`.
pub fn write_u8(target: &mut [u8], offset: usize, value: u8) -> DbResult<()> {
    let slot = window_mut(target, offset, 1)?;
    match slot.first_mut() {
        Some(byte) => {
            *byte = value;
            Ok(())
        }
        None => Err(corrupt("one-byte window was empty")),
    }
}

/// Writes a big-endian 16-bit unsigned integer at `offset`.
pub fn write_u16(target: &mut [u8], offset: usize, value: u16) -> DbResult<()> {
    spread_be(target, offset, 2, u64::from(value))
}

/// Writes a big-endian 24-bit unsigned integer at `offset`, rejecting a value
/// that does not fit rather than truncating it.
pub fn write_u24(target: &mut [u8], offset: usize, value: u32) -> DbResult<()> {
    if value > 0x00ff_ffff {
        return Err(corrupt("value does not fit in a 24-bit field"));
    }
    spread_be(target, offset, 3, u64::from(value))
}

/// Writes a big-endian 32-bit unsigned integer at `offset`.
pub fn write_u32(target: &mut [u8], offset: usize, value: u32) -> DbResult<()> {
    spread_be(target, offset, 4, u64::from(value))
}

/// Writes a big-endian 48-bit unsigned integer at `offset`, rejecting a value
/// that does not fit.
pub fn write_u48(target: &mut [u8], offset: usize, value: u64) -> DbResult<()> {
    if value > 0x0000_ffff_ffff_ffff {
        return Err(corrupt("value does not fit in a 48-bit field"));
    }
    spread_be(target, offset, 6, value)
}

/// Writes a big-endian 64-bit unsigned integer at `offset`.
pub fn write_u64(target: &mut [u8], offset: usize, value: u64) -> DbResult<()> {
    spread_be(target, offset, 8, value)
}

/// Writes a big-endian 64-bit signed integer at `offset`.
pub fn write_i64(target: &mut [u8], offset: usize, value: i64) -> DbResult<()> {
    write_u64(target, offset, value as u64)
}

/// Writes a big-endian IEEE-754 binary64 at `offset`, preserving the exact bit
/// pattern.
pub fn write_f64(target: &mut [u8], offset: usize, value: f64) -> DbResult<()> {
    write_u64(target, offset, value.to_bits())
}

/// Writes the low `width` bytes of `value` big-endian at `offset`.
fn spread_be(target: &mut [u8], offset: usize, width: usize, value: u64) -> DbResult<()> {
    let slot = window_mut(target, offset, width)?;
    for (index, byte) in slot.iter_mut().enumerate() {
        let shift = width
            .checked_sub(index)
            .and_then(|remaining| remaining.checked_sub(1))
            .and_then(|remaining| remaining.checked_mul(8))
            .ok_or_else(|| corrupt("byte shift overflowed"))?;
        *byte = ((value.wrapping_shr(shift as u32)) & 0xff) as u8;
    }
    Ok(())
}

/// A forward-only reader over a byte slice.
///
/// The pager and journal codecs walk records in order; a cursor keeps the
/// offset arithmetic in one place instead of at every field.
#[derive(Clone, Debug)]
pub struct ByteReader<'a> {
    source: &'a [u8],
    offset: usize,
}

impl<'a> ByteReader<'a> {
    /// Starts a reader at the beginning of `source`.
    pub fn new(source: &'a [u8]) -> ByteReader<'a> {
        ByteReader { source, offset: 0 }
    }

    /// Returns how many bytes have been consumed so far.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Returns how many bytes are left.
    pub fn remaining(&self) -> usize {
        self.source.len().saturating_sub(self.offset)
    }

    /// Returns the not-yet-consumed bytes without consuming them.
    pub fn rest(&self) -> &'a [u8] {
        self.source.get(self.offset..).unwrap_or(&[])
    }

    /// Consumes `len` bytes and returns them.
    pub fn take(&mut self, len: usize) -> DbResult<&'a [u8]> {
        let bytes = window(self.source, self.offset, len)?;
        self.offset = self
            .offset
            .checked_add(len)
            .ok_or_else(|| corrupt("reader offset overflowed"))?;
        Ok(bytes)
    }

    /// Consumes one byte.
    pub fn u8(&mut self) -> DbResult<u8> {
        let value = read_u8(self.source, self.offset)?;
        self.advance(1)?;
        Ok(value)
    }

    /// Consumes a big-endian 16-bit unsigned integer.
    pub fn u16(&mut self) -> DbResult<u16> {
        let value = read_u16(self.source, self.offset)?;
        self.advance(2)?;
        Ok(value)
    }

    /// Consumes a big-endian 32-bit unsigned integer.
    pub fn u32(&mut self) -> DbResult<u32> {
        let value = read_u32(self.source, self.offset)?;
        self.advance(4)?;
        Ok(value)
    }

    /// Consumes a big-endian 64-bit unsigned integer.
    pub fn u64(&mut self) -> DbResult<u64> {
        let value = read_u64(self.source, self.offset)?;
        self.advance(8)?;
        Ok(value)
    }

    /// Moves the cursor forward, refusing to move past the end.
    fn advance(&mut self, len: usize) -> DbResult<()> {
        let next = self
            .offset
            .checked_add(len)
            .ok_or_else(|| corrupt("reader offset overflowed"))?;
        if next > self.source.len() {
            return Err(corrupt("reader advanced past the end of the buffer"));
        }
        self.offset = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::PrimaryCode;
    use crate::rng::Rng;

    /// Every width must survive a write/read round-trip at its boundaries.
    #[test]
    fn widths_round_trip_at_their_boundaries() {
        let mut page = [0u8; 32];
        write_u8(&mut page, 0, u8::MAX).unwrap();
        write_u16(&mut page, 1, u16::MAX).unwrap();
        write_u24(&mut page, 3, 0x00ff_ffff).unwrap();
        write_u32(&mut page, 6, u32::MAX).unwrap();
        write_u48(&mut page, 10, 0x0000_ffff_ffff_ffff).unwrap();
        write_u64(&mut page, 16, u64::MAX).unwrap();
        assert_eq!(read_u8(&page, 0).unwrap(), u8::MAX);
        assert_eq!(read_u16(&page, 1).unwrap(), u16::MAX);
        assert_eq!(read_u24(&page, 3).unwrap(), 0x00ff_ffff);
        assert_eq!(read_u32(&page, 6).unwrap(), u32::MAX);
        assert_eq!(read_u48(&page, 10).unwrap(), 0x0000_ffff_ffff_ffff);
        assert_eq!(read_u64(&page, 16).unwrap(), u64::MAX);
    }

    /// A narrow field refuses a value that would silently lose its top bits.
    #[test]
    fn narrow_widths_reject_values_that_do_not_fit() {
        let mut page = [0u8; 16];
        assert_eq!(
            write_u24(&mut page, 0, 0x0100_0000).unwrap_err().code(),
            PrimaryCode::Corrupt
        );
        assert_eq!(
            write_u48(&mut page, 0, 0x0001_0000_0000_0000)
                .unwrap_err()
                .code(),
            PrimaryCode::Corrupt
        );
    }

    /// Reading past the end is an error, never a panic; the whole point of the
    /// module is that a corrupt page cannot take the process down.
    #[test]
    fn reads_past_the_end_are_errors() {
        let page = [0u8; 4];
        assert_eq!(read_u64(&page, 0).unwrap_err().code(), PrimaryCode::Corrupt);
        assert_eq!(read_u32(&page, 1).unwrap_err().code(), PrimaryCode::Corrupt);
        assert_eq!(read_u8(&page, 4).unwrap_err().code(), PrimaryCode::Corrupt);
        assert_eq!(
            window(&page, usize::MAX, 1).unwrap_err().code(),
            PrimaryCode::Corrupt
        );
    }

    /// Doubles keep their exact bits, which matters for signed zero and NaN
    /// payloads that a decimal round-trip would quietly change.
    #[test]
    fn doubles_keep_their_exact_bits() {
        let cases = [
            0.0_f64,
            -0.0_f64,
            f64::MIN_POSITIVE,
            f64::MAX,
            f64::NEG_INFINITY,
            f64::from_bits(0x7ff8_0000_0000_0001),
        ];
        let mut page = [0u8; 8];
        for case in cases {
            write_f64(&mut page, 0, case).unwrap();
            assert_eq!(read_f64(&page, 0).unwrap().to_bits(), case.to_bits());
        }
    }

    /// A reader consumes exactly as much as it reports and stops at the end.
    #[test]
    fn reader_tracks_its_offset_and_stops_at_the_end() {
        let source = [1u8, 2, 0, 3, 0, 0, 0, 4];
        let mut reader = ByteReader::new(&source);
        assert_eq!(reader.u8().unwrap(), 1);
        assert_eq!(reader.u16().unwrap(), 0x0200);
        assert_eq!(reader.offset(), 3);
        assert_eq!(reader.remaining(), 5);
        assert_eq!(reader.take(5).unwrap(), &[3, 0, 0, 0, 4]);
        assert_eq!(reader.remaining(), 0);
        assert_eq!(reader.u8().unwrap_err().code(), PrimaryCode::Corrupt);
    }

    /// Random offsets against a random buffer must either decode or fail; the
    /// seed is fixed so a failure is reproducible from the test name alone.
    #[test]
    fn random_offsets_never_panic() {
        let mut rng = Rng::new(0x5eed_1782);
        let mut buffer = vec![0u8; 512];
        for byte in buffer.iter_mut() {
            *byte = rng.next_u32() as u8;
        }
        // Miri interprets every instruction, so two hundred thousand rounds of
        // this take hours rather than milliseconds. The property being checked
        // is "no input panics", which a smaller sample still exercises against
        // the interpreter's much stricter memory model - and the full sample
        // still runs on every ordinary build.
        for _ in 0..crate::probe::sample_rounds(200_000) {
            let offset = rng.next_u32() as usize % 600;
            let _ = read_u8(&buffer, offset);
            let _ = read_u16(&buffer, offset);
            let _ = read_u24(&buffer, offset);
            let _ = read_u32(&buffer, offset);
            let _ = read_u48(&buffer, offset);
            let _ = read_u64(&buffer, offset);
            let _ = read_f64(&buffer, offset);
        }
    }
}
