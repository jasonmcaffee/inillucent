//! The one conversion between a tree datum and a SQL value, in both directions.
//!
//! Invariant: **a value survives the round trip byte for byte, including a
//! `TEXT` payload that is not valid UTF-8.** This crate carried five
//! hand-written copies of this conversion before task-1961's A6, and they did
//! not agree: two read the text payload with `raw()` and one with
//! `utf8_bytes()`. On text that is valid UTF-8 the two are the same bytes, so
//! every test in the workspace passed; on the bytes SQLite happily stores in a
//! `TEXT` column and this engine stores too, `utf8_bytes()` on a value that had
//! been re-tagged would have replaced them with U+FFFD and the value would not
//! have come back. That is the case this file holds.

use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_value::{TextEncoding, Value};

/// Bytes no UTF-8 decoder accepts: a lone continuation byte, an unfinished
/// three-byte sequence, and a byte that starts nothing.
const NOT_UTF8: &[u8] = &[0x41, 0x80, 0xE2, 0x28, 0xA1, 0xFF, 0x42];

#[test]
fn text_that_is_not_utf8_round_trips_unchanged() {
    let stored = OwnedDatum::Text(NOT_UTF8.to_vec());
    let value = Value::from(&stored);
    let back = OwnedDatum::from(&value);
    assert_eq!(back, stored, "the payload was rewritten on the way through");
    match back {
        OwnedDatum::Text(bytes) => assert_eq!(bytes, NOT_UTF8),
        other => panic!("a text value came back as {other:?}"),
    }
}

#[test]
fn a_blob_round_trips_unchanged() {
    let stored = OwnedDatum::Blob(NOT_UTF8.to_vec());
    let back = OwnedDatum::from(&Value::from(&stored));
    assert_eq!(back, stored);
}

#[test]
fn every_storage_class_round_trips() {
    let cases = [
        OwnedDatum::Null,
        OwnedDatum::Int(i64::MIN),
        OwnedDatum::Int(0),
        OwnedDatum::Int(i64::MAX),
        OwnedDatum::Real(-0.0),
        OwnedDatum::Real(2.5),
        OwnedDatum::Text(Vec::new()),
        OwnedDatum::Text(b"hello".to_vec()),
        OwnedDatum::Blob(Vec::new()),
        OwnedDatum::Blob(vec![0, 1, 2, 255]),
    ];
    for stored in cases {
        let back = OwnedDatum::from(&Value::from(&stored));
        assert_eq!(back, stored, "{stored:?} did not survive the round trip");
    }
}

#[test]
fn a_negative_zero_keeps_its_sign() {
    // `-0.0 == 0.0` in IEEE-754, so `assert_eq!` on the datum would pass on a
    // conversion that lost the sign. The bits are what the tree stores.
    let back = OwnedDatum::from(&Value::from(&OwnedDatum::Real(-0.0)));
    match back {
        OwnedDatum::Real(number) => assert_eq!(number.to_bits(), (-0.0f64).to_bits()),
        other => panic!("a real came back as {other:?}"),
    }
}

#[test]
fn the_borrowed_datum_converts_to_a_value_that_borrows_the_page() {
    let page = NOT_UTF8.to_vec();
    let datum = Datum::Text(&page);
    let value = Value::from(&datum);
    match value {
        Value::Text(text) => {
            assert_eq!(text.raw(), NOT_UTF8);
            assert_eq!(text.encoding(), TextEncoding::Utf8);
        }
        other => panic!("a text datum came back as {other:?}"),
    }
}

#[test]
fn the_owning_form_agrees_with_the_borrowing_one() {
    let stored = OwnedDatum::Text(NOT_UTF8.to_vec());
    let value = Value::from(&stored);
    let by_reference = OwnedDatum::from(&value);
    let by_value = OwnedDatum::from(value);
    assert_eq!(by_reference, by_value);
}
