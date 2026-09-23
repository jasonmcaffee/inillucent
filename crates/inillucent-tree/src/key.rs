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
//! numerically, and a number's payload is **seventeen bytes** rather than
//! eight: the order-preserving double, then a three-way range bucket, then the
//! exact integer. The double alone would be an ordering that is *coarser* than
//! the comparison it stands for - every integer above 2^53 rounds onto a double
//! it shares with its neighbours - and a coarser ordering is not a slower
//! encoding, it is a wrong answer. Two rowids that encode alike route a descent
//! to one leaf and collapse into one group in `DISTINCT`.
//!
//! The tail closes that exactly:
//!
//! - the double orders every pair whose values differ enough to round apart,
//!   and rounding is monotone, so it never disagrees with the numeric order;
//! - when two encodings share a double, both values are integral - a
//!   non-integral double has magnitude below 2^52 and rounds apart from every
//!   integer - so the exact integer tail orders them;
//! - the bucket byte separates the two doubles that sit outside `i64` from the
//!   integers that round onto them, which is the one pair the tail alone would
//!   tie.
//!
//! An integer and a real of the same value therefore encode **identically**,
//! which is what SQLite's comparison says they are. [`Key::numeric_is_exact`]
//! remains, and now reports what it always meant: whether the *double* half of
//! the encoding is exact, which is what a caller comparing against a foreign
//! encoding wants to know.
//!
//! A rowid tree does not pay any of this. Its key is one integer, so its
//! interior pages store [`order_preserving_int`] and nothing else: eight bytes,
//! no class byte, no tail.

use inillucent_value::collation::{nocase_key_bytes, Collation};

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

    /// Reports whether every numeric column's *double* half is exact.
    ///
    /// The encoding as a whole is exact for every value - the tail after the
    /// double sees to that, and this module's own documentation says how - so
    /// this is no longer a warning about the key's ordering. It reports the
    /// narrower fact a caller comparing against a foreign encoding wants:
    /// whether the value survives a round trip through `f64`. An integer above
    /// 2^53 does not.
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

/// Encodes a tuple whose columns have collations.
///
/// A collation is an order over text, and a key encoding exists to turn an
/// order into `memcmp`. So the two have to meet: `NOCASE` folds the bytes
/// before they are escaped, `RTRIM` strips the trailing spaces, and `BINARY`
/// does neither. An index on a `COLLATE NOCASE` column stores `Blue` between
/// two `blue`s, and a separator encoded without the fold would sort it
/// somewhere else - which is a descent that walks past the row it wants.
///
/// @param values - the tuple, in key-column order
/// @param collations - one per column; a short list means BINARY for the rest
pub fn encode_with(values: &[Datum<'_>], collations: &[Collation]) -> Key {
    let mut out = Vec::with_capacity(values.len().saturating_mul(9));
    for (index, value) in values.iter().enumerate() {
        let collation = collations.get(index).copied().unwrap_or(Collation::Binary);
        encode_into_with(value, collation, &mut out);
    }
    Key(out)
}

/// Appends one value's encoding to a buffer, under a collation.
///
/// @param value - the value to encode
/// @param collation - the order its text is compared under
/// @param out - the buffer to append to
pub fn encode_into_with(value: &Datum<'_>, collation: Collation, out: &mut Vec<u8>) {
    match (value, collation) {
        (Datum::Text(bytes), Collation::NoCase) => {
            out.push(class::TEXT);
            // Folded straight into the buffer. Collecting a `Vec` first was one
            // heap allocation per value, which on the index-build path is one
            // per row. `nocase_key_bytes` also applies SQLite's rule for a NUL,
            // which is a length after the first NUL rather than the bytes after
            // it (task-2079); its own comment says why that is still an order.
            let start = out.len();
            if nocase_key_bytes(bytes, out) {
                escape_in_place(out, start);
            } else {
                // No NUL in the value means no zero byte in what was appended,
                // so only the terminator is needed.
                out.push(0x00);
                out.push(0x00);
            }
        }
        (Datum::Text(bytes), Collation::RTrim) => {
            out.push(class::TEXT);
            let trimmed = trim_trailing_spaces(bytes);
            escape_into(trimmed, out);
        }
        _ => encode_into(value, out),
    }
}

/// Returns the bytes with trailing spaces removed, which is `RTRIM`'s rule.
///
/// @param bytes - the text
fn trim_trailing_spaces(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && bytes.get(end.saturating_sub(1)) == Some(&b' ') {
        end = end.saturating_sub(1);
    }
    bytes.get(..end).unwrap_or(&[])
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
            out.push(RANGE_WITHIN);
            out.extend_from_slice(&order_preserving_int(*number));
        }
        Datum::Real(number) => {
            out.push(class::NUMBER);
            out.extend_from_slice(&order_preserving_real(*number));
            let (bucket, exact) = real_tail(*number);
            out.push(bucket);
            out.extend_from_slice(&order_preserving_int(exact));
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

/// The bucket byte for a number below every `i64`.
const RANGE_BELOW: u8 = 0;
/// The bucket byte for a number an `i64` could hold.
const RANGE_WITHIN: u8 = 1;
/// The bucket byte for a number above every `i64`.
const RANGE_ABOVE: u8 = 2;

/// Returns the exact tail a real's encoding carries after its double.
///
/// The bucket separates reals outside the `i64` range from the integers that
/// round onto the same double: `i64::MAX` rounds *up* to 2^63, so it shares a
/// double with the real 2^63 while being smaller than it, and the bucket is the
/// only thing that can say so. Within the range the tail is the value truncated
/// toward zero, which is exact whenever it matters, because two encodings share
/// a double only when both values are integral.
///
/// @param number - the real being encoded
fn real_tail(number: f64) -> (u8, i64) {
    if number.is_nan() {
        return (RANGE_ABOVE, i64::MAX);
    }
    // 2^63 as a double, which is one above `i64::MAX` and is exactly
    // representable. `i64::MAX as f64` rounds to the same value, so the
    // comparison has to be written against the power of two rather than
    // against the integer bound.
    const ABOVE: f64 = 9_223_372_036_854_775_808.0;
    if number >= ABOVE {
        return (RANGE_ABOVE, i64::MAX);
    }
    if number < -ABOVE {
        return (RANGE_BELOW, i64::MIN);
    }
    (RANGE_WITHIN, number as i64)
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
    // Negative zero and positive zero are numerically equal, so they must
    // encode alike. Their bit patterns differ, and left alone negative zero
    // would sort below every other value including positive zero.
    let number = if number == 0.0 { 0.0 } else { number };
    let bits = number.to_bits();
    let transformed = if bits & (1u64 << 63) != 0 {
        !bits
    } else {
        bits | (1u64 << 63)
    };
    transformed.to_be_bytes()
}

/// Escapes the bytes already appended from `start`, and terminates them.
///
/// The folding collations write their transformed payload into the buffer to
/// avoid a temporary, and then need the same escaping every other payload gets.
/// A `0x00` in text is rare enough that the scan below almost always finds
/// none and the work is the two terminator bytes.
///
/// @param out - the buffer, holding the payload from `start` to its end
/// @param start - where the payload begins
fn escape_in_place(out: &mut Vec<u8>, start: usize) {
    let has_zero = out
        .get(start..)
        .is_some_and(|payload| payload.contains(&0x00));
    if has_zero {
        let payload: Vec<u8> = out.get(start..).unwrap_or(&[]).to_vec();
        out.truncate(start);
        escape_into(&payload, out);
        return;
    }
    out.push(0x00);
    out.push(0x00);
}

/// Appends bytes with `0x00` escaped and a two-byte terminator.
///
/// @param bytes - the payload
/// @param out - the buffer to append to
fn escape_into(bytes: &[u8], out: &mut Vec<u8>) {
    // **Copied in runs, not byte by byte.** Only `0x00` needs escaping and
    // text almost never holds one, so the ordinary payload is a single
    // `extend_from_slice` - one bounds check and one `memcpy` - where the loop
    // this replaces did a `push` per byte, each with its own capacity check.
    // A `CREATE INDEX` over a hundred thousand forty-byte labels encodes four
    // million bytes through here, and that difference was measurable in the
    // `schema` family.
    out.reserve(bytes.len().saturating_add(2));
    let mut rest = bytes;
    while let Some(at) = rest.iter().position(|byte| *byte == 0x00) {
        out.extend_from_slice(rest.get(..at).unwrap_or(&[]));
        out.push(0x00);
        out.push(0xFF);
        rest = rest.get(at.saturating_add(1)..).unwrap_or(&[]);
    }
    out.extend_from_slice(rest);
    out.push(0x00);
    out.push(0x00);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    /// `RTRIM` drops trailing spaces before the text is escaped, so two values
    /// that differ only in them encode identically.
    ///
    /// The coverage run found `trim_trailing_spaces` never called by any test -
    /// both of its branches at zero. A tree on an `RTRIM` column is *stored* in
    /// this order, so an encoding that kept the spaces would place `'a '` after
    /// `'a'` and a seek for either would miss the other.
    #[test]
    fn rtrim_ignores_trailing_spaces_and_nothing_else() {
        let plain = encode_with(&[Datum::Text(b"abc")], &[Collation::RTrim]);
        let padded = encode_with(&[Datum::Text(b"abc   ")], &[Collation::RTrim]);
        assert_eq!(plain.as_bytes(), padded.as_bytes());

        // Only *trailing* spaces go. A leading or interior one is part of the
        // value, and RTRIM does not touch it.
        let leading = encode_with(&[Datum::Text(b" abc")], &[Collation::RTrim]);
        let interior = encode_with(&[Datum::Text(b"a bc")], &[Collation::RTrim]);
        assert_ne!(leading.as_bytes(), plain.as_bytes());
        assert_ne!(interior.as_bytes(), plain.as_bytes());

        // A value that is nothing but spaces trims to empty, and the loop has
        // to run all the way down rather than stopping at the first byte.
        let spaces = encode_with(&[Datum::Text(b"    ")], &[Collation::RTrim]);
        let empty = encode_with(&[Datum::Text(b"")], &[Collation::RTrim]);
        assert_eq!(spaces.as_bytes(), empty.as_bytes());

        // And under BINARY none of this happens.
        let binary_plain = encode_with(&[Datum::Text(b"abc")], &[Collation::Binary]);
        let binary_padded = encode_with(&[Datum::Text(b"abc   ")], &[Collation::Binary]);
        assert_ne!(binary_plain.as_bytes(), binary_padded.as_bytes());
    }

    /// A NOCASE key orders text holding a NUL as SQLite's NOCASE does, with a
    /// second column after it (task-2079).
    ///
    /// The length written after the first NUL is eight bytes, several of them
    /// zero, and they are escaped like any payload byte. The rowid column after
    /// the text is what shows the escaped length does not run into the next
    /// column: two values equal under NOCASE must still be ordered by it.
    #[test]
    fn a_nocase_key_orders_an_embedded_nul_as_sqlite_does() {
        let texts: [&[u8]; 9] = [
            b"", b"\0", b"\0a", b"\0B", b"\0\0y", b"a", b"A\0z", b"a\0bc", b"ab",
        ];
        let mut tuples: Vec<(&[u8], i64)> = Vec::new();
        for text in texts {
            for rowid in [-1_i64, 0, 256] {
                tuples.push((text, rowid));
            }
        }
        for (left_text, left_rowid) in &tuples {
            let left = encode_with(
                &[Datum::Text(left_text), Datum::Int(*left_rowid)],
                &[Collation::NoCase],
            );
            for (right_text, right_rowid) in &tuples {
                let right = encode_with(
                    &[Datum::Text(right_text), Datum::Int(*right_rowid)],
                    &[Collation::NoCase],
                );
                let expected = Collation::NoCase
                    .compare_bytes(left_text, right_text)
                    .then(left_rowid.cmp(right_rowid));
                assert_eq!(
                    left.as_bytes().cmp(right.as_bytes()),
                    expected,
                    "{left_text:?}/{left_rowid} against {right_text:?}/{right_rowid}"
                );
            }
        }
    }

    /// A NaN encodes above every real value, and does so consistently in both
    /// halves of the payload.
    ///
    /// A NaN cannot be stored in a SQLite database - it is written as NULL -
    /// but it can arrive from arithmetic in a seek key, and the encoding has to
    /// stay a total order when it does. Both NaN branches were untaken by any
    /// test, which means the one property that makes the function safe to call
    /// on any double was unverified.
    #[test]
    fn a_nan_encodes_above_every_other_number() {
        let nan = encode(&[Datum::Real(f64::NAN)]);
        let infinity = encode(&[Datum::Real(f64::INFINITY)]);
        let big = encode(&[Datum::Real(f64::MAX)]);
        let negative = encode(&[Datum::Real(f64::NEG_INFINITY)]);
        assert_eq!(
            nan.as_bytes().cmp(infinity.as_bytes()),
            Ordering::Greater,
            "a NaN sorts above infinity"
        );
        assert_eq!(infinity.as_bytes().cmp(big.as_bytes()), Ordering::Greater);
        assert_eq!(big.as_bytes().cmp(negative.as_bytes()), Ordering::Greater);
        // A negative NaN is still a NaN and lands in the same place, which is
        // what "no place in an order" has to mean if the encoding is a
        // function.
        let negative_nan = encode(&[Datum::Real(-f64::NAN)]);
        assert_eq!(negative_nan.as_bytes(), nan.as_bytes());
        // The order transform and the exact tail agree about it: the tail is
        // the above-range bucket, not a truncation of a value that has none.
        assert_eq!(real_tail(f64::NAN), (RANGE_ABOVE, i64::MAX));
        assert_eq!(order_preserving_real(f64::NAN), [0xFF; 8]);
    }

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
                assert_eq!(by_bytes, by_value, "{left:?} vs {right:?}");
            }
        }
    }

    /// A large integer round-trips through `f64` or it does not, and
    /// `numeric_is_exact` says which.
    #[test]
    fn large_integers_are_flagged_as_inexact_through_the_double() {
        assert!(Key::numeric_is_exact(&[Datum::Int(1 << 52)]));
        assert!(Key::numeric_is_exact(&[Datum::Int(-(1 << 53))]));
        assert!(!Key::numeric_is_exact(&[Datum::Int((1 << 53) + 1)]));
        assert!(Key::numeric_is_exact(&[Datum::Text(b"x"), Datum::Null]));
    }

    /// Integers that share a double still encode in order. This is the bug the
    /// tail exists to close: without it, two distinct rowids above 2^53 encode
    /// alike, which routes a descent to the wrong leaf and collapses two groups
    /// into one in `DISTINCT`.
    #[test]
    fn integers_that_share_a_double_still_encode_apart() {
        let base = 1i64 << 60;
        let samples = [
            i64::MIN,
            i64::MIN + 1,
            -base - 1,
            -base,
            -1,
            0,
            1,
            base,
            base + 1,
            base + 2,
            (1i64 << 53) + 1,
            i64::MAX - 1,
            i64::MAX,
        ];
        for left in samples {
            for right in samples {
                let a = encode(&[Datum::Int(left)]);
                let b = encode(&[Datum::Int(right)]);
                assert_eq!(
                    a.as_bytes().cmp(b.as_bytes()),
                    left.cmp(&right),
                    "{left} vs {right}"
                );
            }
        }
    }

    /// An integer and a real of the same value encode identically, because
    /// SQLite says they compare equal.
    #[test]
    fn an_integer_and_an_equal_real_encode_alike() {
        for value in [-1_000_000i64, -1, 0, 1, 1_000_000, 1i64 << 52] {
            let as_int = encode(&[Datum::Int(value)]);
            let as_real = encode(&[Datum::Real(value as f64)]);
            assert_eq!(as_int, as_real, "{value}");
        }
        assert_eq!(encode(&[Datum::Real(-0.0)]), encode(&[Datum::Real(0.0)]));
        assert_eq!(encode(&[Datum::Real(0.0)]), encode(&[Datum::Int(0)]));
    }

    /// The two doubles just outside the `i64` range sort outside every integer,
    /// which the bucket byte is the only thing that can express: `i64::MAX`
    /// rounds *up* onto 2^63 and would otherwise tie with it.
    #[test]
    fn reals_outside_the_integer_range_sort_outside_it() {
        const ABOVE: f64 = 9_223_372_036_854_775_808.0;
        let bigger = encode(&[Datum::Real(ABOVE)]);
        let largest = encode(&[Datum::Int(i64::MAX)]);
        assert!(
            largest.as_bytes() < bigger.as_bytes(),
            "i64::MAX must sort below 2^63"
        );
        let smaller = encode(&[Datum::Real(-ABOVE * 2.0)]);
        let smallest = encode(&[Datum::Int(i64::MIN)]);
        assert!(smaller.as_bytes() < smallest.as_bytes());
        // The negative bound is exactly representable, so it *is* i64::MIN.
        assert_eq!(encode(&[Datum::Real(-ABOVE)]), smallest);
        // Infinities and NaN stay at the ends and do not panic.
        let inf = encode(&[Datum::Real(f64::INFINITY)]);
        let nan = encode(&[Datum::Real(f64::NAN)]);
        assert!(inf.as_bytes() > largest.as_bytes());
        assert!(nan.as_bytes() >= inf.as_bytes());
        assert!(encode(&[Datum::Real(f64::NEG_INFINITY)]).as_bytes() < smallest.as_bytes());
    }

    /// A number's payload is the documented seventeen bytes after its class.
    #[test]
    fn a_number_encodes_to_seventeen_bytes_plus_its_class() {
        assert_eq!(encode(&[Datum::Int(1)]).len(), 18);
        assert_eq!(encode(&[Datum::Real(1.5)]).len(), 18);
        assert_eq!(encode(&[Datum::Null]).len(), 1);
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
