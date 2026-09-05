//! The SQLite variable-length integer codec.
//!
//! Invariant: `decode` never reads past the slice it was given and never
//! panics, and `encode` always produces the shortest form for its value, so a
//! record written twice is byte-identical twice.
//!
//! The format is one to nine big-endian bytes. The first eight bytes each carry
//! seven payload bits with the high bit set to mean "another byte follows"; if
//! eight such bytes are not enough, a ninth byte carries a full eight bits, so
//! the widest form holds all 64 bits.

use crate::error::{corrupt, DbResult};

/// The largest number of bytes one varint can occupy.
pub const MAX_LEN: usize = 9;

/// The largest value that fits in the eight-byte form; anything above it needs
/// the nine-byte form whose last byte is not continuation-encoded.
const MAX_EIGHT_BYTE_VALUE: u64 = (1 << 56) - 1;

/// A decoded varint: its value and how many bytes it consumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decoded {
    /// The decoded value.
    pub value: u64,
    /// How many bytes of the input the varint occupied.
    pub len: usize,
}

/// Returns how many bytes `value` encodes to.
pub fn encoded_len(value: u64) -> usize {
    if value > MAX_EIGHT_BYTE_VALUE {
        return 9;
    }
    let mut remaining = value;
    let mut len: usize = 1;
    while remaining > 0x7f {
        remaining = remaining.wrapping_shr(7);
        len = len.saturating_add(1);
    }
    len
}

/// Encodes `value` at the front of `target`, returning how many bytes it used.
///
/// Fails with `SQLITE_CORRUPT` rather than truncating when the buffer is too
/// small, because a half-written varint would corrupt the record around it.
pub fn encode(target: &mut [u8], value: u64) -> DbResult<usize> {
    let len = encoded_len(value);
    let slot = target
        .get_mut(..len)
        .ok_or_else(|| corrupt("varint does not fit in the destination buffer"))?;
    if len == 9 {
        write_nine_byte_form(slot, value);
        return Ok(9);
    }
    let mut remaining = value;
    for index in (0..len).rev() {
        let Some(byte) = slot.get_mut(index) else {
            return Err(corrupt("varint slot shrank while writing"));
        };
        let payload = (remaining & 0x7f) as u8;
        *byte = if index.saturating_add(1) == len {
            payload
        } else {
            payload | 0x80
        };
        remaining = remaining.wrapping_shr(7);
    }
    Ok(len)
}

/// Writes the nine-byte form: eight continuation bytes of seven bits each,
/// then one byte carrying the low eight bits.
fn write_nine_byte_form(slot: &mut [u8], value: u64) {
    let mut remaining = value.wrapping_shr(8);
    for index in (0..8).rev() {
        if let Some(byte) = slot.get_mut(index) {
            *byte = ((remaining & 0x7f) as u8) | 0x80;
        }
        remaining = remaining.wrapping_shr(7);
    }
    if let Some(byte) = slot.get_mut(8) {
        *byte = (value & 0xff) as u8;
    }
}

/// Decodes the varint at the front of `source`.
///
/// A slice that ends inside a varint is `SQLITE_CORRUPT`; the decoder never
/// assumes a continuation byte it has not seen.
pub fn decode(source: &[u8]) -> DbResult<Decoded> {
    let mut value: u64 = 0;
    for index in 0..8 {
        let byte = *source
            .get(index)
            .ok_or_else(|| corrupt("varint ran off the end of the buffer"))?;
        value = value.wrapping_shl(7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Ok(Decoded {
                value,
                len: index.saturating_add(1),
            });
        }
    }
    let last = *source
        .get(8)
        .ok_or_else(|| corrupt("nine-byte varint ran off the end of the buffer"))?;
    Ok(Decoded {
        value: value.wrapping_shl(8) | u64::from(last),
        len: 9,
    })
}

/// Encodes a signed 64-bit integer by reinterpreting its bits, which is how
/// SQLite stores rowids and record header lengths that may be negative.
pub fn encode_i64(target: &mut [u8], value: i64) -> DbResult<usize> {
    encode(target, value as u64)
}

/// Decodes a signed 64-bit integer written by `encode_i64`.
pub fn decode_i64(source: &[u8]) -> DbResult<(i64, usize)> {
    let decoded = decode(source)?;
    Ok((decoded.value as i64, decoded.len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::PrimaryCode;
    use crate::rng::Rng;

    /// Encodes into a fresh buffer and decodes it back, asserting the length
    /// agrees in all three places that compute it.
    fn round_trip(value: u64) -> usize {
        let mut buffer = [0u8; MAX_LEN];
        let written = encode(&mut buffer, value).expect("nine bytes is always enough");
        assert_eq!(
            written,
            encoded_len(value),
            "encoded_len disagrees for {value}"
        );
        let decoded = decode(&buffer).expect("what we just wrote must decode");
        assert_eq!(decoded.value, value);
        assert_eq!(decoded.len, written);
        written
    }

    /// The documented width boundaries are where an off-by-one lives.
    #[test]
    fn width_boundaries_use_the_documented_number_of_bytes() {
        let cases: [(u64, usize); 11] = [
            (0, 1),
            (0x7f, 1),
            (0x80, 2),
            (0x3fff, 2),
            (0x4000, 3),
            (0x001f_ffff, 3),
            (0x0020_0000, 4),
            (0x000f_ffff_ffff_ffff, 8),
            (MAX_EIGHT_BYTE_VALUE, 8),
            (MAX_EIGHT_BYTE_VALUE + 1, 9),
            (u64::MAX, 9),
        ];
        for (value, expected) in cases {
            assert_eq!(round_trip(value), expected, "wrong width for {value}");
        }
    }

    /// The nine-byte form is the only one whose last byte has its high bit
    /// free, so check its exact bytes rather than only its round-trip.
    #[test]
    fn the_nine_byte_form_carries_a_full_last_byte() {
        let mut buffer = [0u8; MAX_LEN];
        encode(&mut buffer, u64::MAX).unwrap();
        assert_eq!(buffer, [0xff; 9]);
        assert_eq!(
            decode(&buffer).unwrap(),
            Decoded {
                value: u64::MAX,
                len: 9
            }
        );
    }

    /// Signed values round-trip through the bit-cast form, including the two
    /// values whose sign bit is the interesting part.
    #[test]
    fn signed_values_round_trip() {
        for value in [0_i64, -1, i64::MIN, i64::MAX, -4_294_967_296] {
            let mut buffer = [0u8; MAX_LEN];
            let written = encode_i64(&mut buffer, value).unwrap();
            let (decoded, len) = decode_i64(&buffer).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(len, written);
        }
    }

    /// A truncated varint is an error, not a partial value, at every width.
    #[test]
    fn truncated_input_is_an_error_at_every_width() {
        for value in [0x80_u64, 0x4000, 1 << 40, u64::MAX] {
            let mut buffer = [0u8; MAX_LEN];
            let written = encode(&mut buffer, value).unwrap();
            for prefix in 0..written {
                let error = decode(&buffer[..prefix]).expect_err("a truncated varint must fail");
                assert_eq!(error.code(), PrimaryCode::Corrupt);
            }
        }
    }

    /// Encoding refuses a buffer that is one byte short instead of writing a
    /// varint that decodes to something else.
    #[test]
    fn encoding_refuses_a_buffer_that_is_too_small() {
        let mut buffer = [0u8; 2];
        assert_eq!(
            encode(&mut buffer, 1 << 40).unwrap_err().code(),
            PrimaryCode::Corrupt
        );
    }

    /// Every value round-trips, and the encoding is canonical: the same value
    /// always produces the same bytes. Seeded so a failure replays.
    #[test]
    fn random_values_round_trip_canonically() {
        let mut rng = Rng::new(0x1782_0001);
        for _ in 0..crate::probe::sample_rounds(200_000) {
            let width = rng.below(64) as u32;
            let value = rng.next_u64() >> (63 - width.min(63));
            let mut first = [0u8; MAX_LEN];
            let mut second = [0u8; MAX_LEN];
            let a = encode(&mut first, value).unwrap();
            let b = encode(&mut second, value).unwrap();
            assert_eq!((a, first), (b, second));
            assert_eq!(decode(&first).unwrap().value, value);
        }
    }

    /// Arbitrary bytes must decode or fail, never panic. This is the fuzz
    /// surface a corrupt page hits first.
    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut rng = Rng::new(0x1782_0002);
        let mut buffer = [0u8; 12];
        // Miri interprets every instruction, so two hundred thousand rounds of
        // this take hours rather than milliseconds. The property being checked
        // is "no input panics", which a smaller sample still exercises against
        // the interpreter's much stricter memory model - and the full sample
        // still runs on every ordinary build.
        for _ in 0..crate::probe::sample_rounds(200_000) {
            rng.fill(&mut buffer);
            let len = rng.below(buffer.len() as u64 + 1) as usize;
            let _ = decode(&buffer[..len]);
        }
    }
}
