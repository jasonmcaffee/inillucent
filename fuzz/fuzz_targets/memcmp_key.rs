//! Fuzzes the memcmp key encoding against the value comparison it stands for.
//!
//! Invariant: for any two typed tuples, comparing their encodings as bytes
//! gives the same answer as comparing their values. That is the whole point of
//! the encoding: once a key is encoded, every comparison in an interior page, a
//! sort, a hash-join key or a `DISTINCT` set is a `memcmp`. An encoding that is
//! *coarser* than the comparison is not a slow key, it is a wrong answer - two
//! distinct rowids that encode alike route a descent to one leaf and collapse
//! into one group.
//!
//! The target builds two tuples out of the input bytes and asserts the two
//! orderings agree.

#![no_main]

use libfuzzer_sys::fuzz_target;
use inillucent_tree::datum::Datum;
use inillucent_tree::key;
use inillucent_tree::leaf::compare_rows;

/// Builds one value out of a slice of the input.
fn value(bytes: &[u8]) -> Datum<'_> {
    match bytes.first().copied().unwrap_or(0) % 5 {
        0 => Datum::Null,
        1 => {
            let mut raw = [0u8; 8];
            for (slot, byte) in raw.iter_mut().zip(bytes.iter().skip(1)) {
                *slot = *byte;
            }
            Datum::Int(i64::from_le_bytes(raw))
        }
        2 => {
            let mut raw = [0u8; 8];
            for (slot, byte) in raw.iter_mut().zip(bytes.iter().skip(1)) {
                *slot = *byte;
            }
            Datum::Real(f64::from_le_bytes(raw))
        }
        3 => Datum::Text(bytes.get(1..).unwrap_or(&[])),
        _ => Datum::Blob(bytes.get(1..).unwrap_or(&[])),
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let half = data.len() / 2;
    let (left_bytes, right_bytes) = data.split_at(half);
    let left = [value(left_bytes)];
    let right = [value(right_bytes)];
    let by_value = compare_rows(&left, &right, 1);
    let by_bytes = key::encode(&left)
        .as_bytes()
        .cmp(key::encode(&right).as_bytes());
    // NaN is not orderable and the encoding puts it at one end deliberately, so
    // a tuple holding one is excluded rather than asserted on.
    let has_nan = [left[0], right[0]]
        .iter()
        .any(|value| matches!(value, Datum::Real(number) if number.is_nan()));
    if !has_nan {
        assert_eq!(by_bytes, by_value, "{left:?} against {right:?}");
    }
});
