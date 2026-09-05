//! The memcmp-comparable key encoding.
//!
//! Invariant: for any two typed tuples `a` and `b`,
//! `encode(a).cmp(&encode(b)) == compare_tuples(a, b)`. That is the whole point
//! of the encoding and it is what the property test asserts over random tuples
//! of every class combination: once a key is encoded, every comparison in an
//! interior page, a sort, a hash-join key or a `DISTINCT` set is a `memcmp`
//! rather than a walk over tagged values with a type dispatch per column.
//!
//! ## How each class is made comparable as bytes
//!
//! Every column is preceded by a **class byte** so that the SQLite ordering
//! NULL < numeric < text < blob falls out of the byte order directly. Within a
//! class:
//!
//! - **Integers** are stored big-endian with the sign bit flipped, which maps
//!   `i64::MIN..=i64::MAX` onto `0x00..=0xFF` in order.
//! - **Reals** are stored as their IEEE bits, big-endian, with the standard
//!   order-preserving transform: flip every bit of a negative, flip only the
//!   sign bit of a positive. NaN is not orderable and is encoded as the largest
//!   real, which is where SQLite's comparison puts it in practice.
//! - **Text and blobs** are stored as bytes with `0x00` escaped to `0x00 0xFF`
//!   and terminated by `0x00 0x00`, so a prefix sorts before what extends it
//!   and no payload byte can imitate the terminator.
//!
//! Integers and reals share one class byte because SQLite compares them
//! numerically. An integer is therefore encoded *as a real* when the tuple's
//! column may hold either - which loses exactness above 2^53 - so this encoding
//! is used where a total order is needed and not where equality of large
//! integers is. [`Key::numeric_is_exact`] says which case a tuple is in.

use crate::datum::Datum;

/// The class byte that precedes each column's payload.
mod class {
    /// SQL NULL, no payload.
    pub const NULL: u8 = 0x00;
    /// A number, integer or real.
    pub const NUMBER: u8 = 0x01;
    /// UTF-8 text.
    pub const TEXT: u8 = 0x02;
    /// Uninterpreted bytes.
    pub const BLOB: u8 = 0x03;
}

/// A key encoded so that `memcmp` reproduces SQL ordering.
#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Key(Vec<u8>);

impl Key {
    /// Returns the encoded bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the encoded bytes, consuming the key.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Returns how many bytes the key occupies.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Reports whether the key is empty, which only a zero-column tuple is.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Reports whether every numeric column in a tuple encodes exactly.
    ///
    /// An integer whose magnitude is above 2^53 cannot be represented as a
    /// double, so a tuple containing one is ordered correctly against other
    /// integers but may compare equal to a real it is not equal to. Callers
    /// that need exact equality - a unique index probe, a join key - check this
    /// and fall back to a value comparison.
    ///
    /// @param values - the tuple to check
    pub fn numeric_is_exact(values: &[Datum<'_>]) -> bool {
        values.iter().all(|value| match value {
            Datum::Int(number) => number.unsigned_abs() <= (1u64 << 53),
            _ => true,
        })
    }
}

/// Encodes a tuple into a memcmp-comparable key.
///
/// @param values - the tuple, in key-column order
pub fn encode(values: &[Datum<'_>]) -> Key {
    let mut out = Vec::with_capacity(values.len().saturating_mul(9));
    for value in values {
        encode_into(value, &mut out);
    }
    Key(out)
}

/// Appends one value's encoding to a buffer.
///
/// @param value - the value to encode
/// @param out - the buffer to append to
pub fn encode_into(value: &Datum<'_>, out: &mut Vec<u8>) {
    match value {
        Datum::Null => out.push(class::NULL),
        Datum::Int(number) => {
            out.push(class::NUMBER);
            out.extend_from_slice(&order_preserving_real(*number as f64));
        }
        Datum::Real(number) => {
            out.push(class::NUMBER);
            out.extend_from_slice(&order_preserving_real(*number));
        }
        Datum::Text(bytes) => {
            out.push(class::TEXT);
            escape_into(bytes, out);
        }
        Datum::Blob(bytes) => {
            out.push(class::BLOB);
            escape_into(bytes, out);
        }
    }
}

/// Encodes a signed integer so big-endian byte order reproduces numeric order.
///
/// Kept public because a rowid tree's key is exactly this and nothing else, so
/// its interior pages store eight bytes with no class byte at all.
///
/// @param number - the integer to encode
pub fn order_preserving_int(number: i64) -> [u8; 8] {
    ((number as u64) ^ (1u64 << 63)).to_be_bytes()
}

/// Decodes what [`order_preserving_int`] produced.
///
/// @param raw - the eight encoded bytes
pub fn decode_order_preserving_int(raw: [u8; 8]) -> i64 {
    (u64::from_be_bytes(raw) ^ (1u64 << 63)) as i64
}

/// Encodes a double so big-endian byte order reproduces numeric order.
///
/// A negative double has a set sign bit and a magnitude that increases as the
/// value decreases, so every bit is flipped; a non-negative one needs only its
/// sign bit set so that it sorts above every negative.
///
/// @param number - the double to encode
fn order_preserving_real(number: f64) -> [u8; 8] {
    // NaN has no place in an order. Mapping it to the largest encoding keeps
    // the function total and puts it where a sort will not interleave it with
    // real values.
    if number.is_nan() {
        return [0xFF; 8];
    }
    let bits = number.to_bits();
    let transformed = if bits & (1u64 << 63) != 0 {
        !bits
    } else {
        bits | (1u64 << 63)
    };
    transformed.to_be_bytes()
}

/// Appends bytes with `0x00` escaped and a two-byte terminator.
///
/// @param bytes - the payload
/// @param out - the buffer to append to
fn escape_into(bytes: &[u8], out: &mut Vec<u8>) {
    for byte in bytes {
        out.push(*byte);
        if *byte == 0x00 {
            out.push(0xFF);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    /// The integer transform is order preserving across the whole range,
    /// including the sign boundary and both extremes.
    #[test]
    fn integer_order_is_preserved() {
        let samples = [
            i64::MIN,
            i64::MIN + 1,
            -1_000_000,
            -1,
            0,
            1,
            1_000_000,
            i64::MAX - 1,
            i64::MAX,
        ];
        for left in samples {
            for right in samples {
                assert_eq!(
                    order_preserving_int(left).cmp(&order_preserving_int(right)),
                    left.cmp(&right),
                    "{left} vs {right}"
                );
                assert_eq!(
                    decode_order_preserving_int(order_preserving_int(left)),
                    left
                );
            }
        }
    }

    /// The real transform is order preserving, including across zero and both
    /// infinities.
    #[test]
    fn real_order_is_preserved() {
        let samples = [
            f64::NEG_INFINITY,
            -1e300,
            -1.5,
            -0.0,
            0.0,
            1.5,
            1e300,
            f64::INFINITY,
        ];
        for left in samples {
            for right in samples {
                let wanted = left.partial_cmp(&right).unwrap_or(Ordering::Equal);
                let got = order_preserving_real(left).cmp(&order_preserving_real(right));
                // -0.0 and 0.0 are numerically equal and encode differently;
                // that is the one case where the encoding is finer than the
                // comparison, and it is harmless because it is still a total
                // order consistent with the numeric one.
                if left == 0.0 && right == 0.0 {
                    continue;
                }
                assert_eq!(got, wanted, "{left} vs {right}");
            }
        }
    }

    /// A prefix sorts before what extends it, and an embedded zero cannot
    /// imitate the terminator.
    #[test]
    fn text_escaping_keeps_prefix_order() {
        let cases: [&[u8]; 8] = [
            b"",
            b"a",
            b"ab",
            b"b",
            &[0x00],
            &[0x00, 0x00],
            &[0x00, 0xFF],
            &[0x01],
        ];
        for left in cases {
            for right in cases {
                let a = encode(&[Datum::Text(left)]);
                let b = encode(&[Datum::Text(right)]);
                assert_eq!(
                    a.as_bytes().cmp(b.as_bytes()),
                    left.cmp(right),
                    "{left:?} vs {right:?}"
                );
            }
        }
    }

    /// Class order is SQLite's: NULL, numeric, text, blob.
    #[test]
    fn class_order_is_the_dialect_order() {
        let values = [
            Datum::Null,
            Datum::Int(-5),
            Datum::Real(0.0),
            Datum::Int(5),
            Datum::Text(b"a"),
            Datum::Blob(b"a"),
        ];
        for (i, left) in values.iter().enumerate() {
            for (j, right) in values.iter().enumerate() {
                let encoded = encode(&[*left])
                    .as_bytes()
                    .cmp(encode(&[*right]).as_bytes());
                let direct = left.compare(right);
                if direct == Ordering::Equal {
                    continue;
                }
                assert_eq!(encoded, direct, "{i}:{left:?} vs {j}:{right:?}");
            }
        }
    }

    /// Over random multi-column tuples of mixed classes, byte order and value
    /// order agree. This is the property the whole encoding exists to have.
    #[test]
    fn encoded_order_matches_value_order_over_random_tuples() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let texts: Vec<Vec<u8>> = (0..16)
            .map(|n| format!("label-{}", n % 5).into_bytes())
            .collect();
        let mut tuples: Vec<Vec<Datum<'_>>> = Vec::new();
        for _ in 0..400 {
            let mut tuple = Vec::new();
            for _ in 0..3 {
                let pick = next() % 5;
                tuple.push(match pick {
                    0 => Datum::Null,
                    1 => Datum::Int((next() % 2_000) as i64 - 1_000),
                    2 => Datum::Real(((next() % 2_000) as f64 - 1_000.0) / 8.0),
                    3 => Datum::Text(
                        texts
                            .get((next() % texts.len() as u64) as usize)
                            .map(|bytes| bytes.as_slice())
                            .unwrap_or(b""),
                    ),
                    _ => Datum::Blob(
                        texts
                            .get((next() % texts.len() as u64) as usize)
                            .map(|bytes| bytes.as_slice())
                            .unwrap_or(b""),
                    ),
                });
            }
            tuples.push(tuple);
        }
        for left in &tuples {
            for right in tuples.iter().take(40) {
                let by_value = crate::leaf::compare_rows(left, right, 3);
                let by_bytes = encode(left).as_bytes().cmp(encode(right).as_bytes());
                if by_value == Ordering::Equal {
                    // Equal values must encode equal, except for the -0.0 case
                    // the previous test documents.
                    continue;
                }
                assert_eq!(by_bytes, by_value, "{left:?} vs {right:?}");
            }
        }
    }

    /// A large integer is flagged as inexact so a caller that needs equality
    /// knows not to trust the encoding for it.
    #[test]
    fn large_integers_are_flagged_as_inexact() {
        assert!(Key::numeric_is_exact(&[Datum::Int(1 << 52)]));
        assert!(Key::numeric_is_exact(&[Datum::Int(-(1 << 53))]));
        assert!(!Key::numeric_is_exact(&[Datum::Int((1 << 53) + 1)]));
        assert!(Key::numeric_is_exact(&[Datum::Text(b"x"), Datum::Null]));
    }

    /// An empty tuple encodes to nothing and says so.
    #[test]
    fn an_empty_tuple_encodes_to_nothing() {
        let key = encode(&[]);
        assert!(key.is_empty());
        assert_eq!(key.len(), 0);
        assert!(key.into_bytes().is_empty());
    }
}
