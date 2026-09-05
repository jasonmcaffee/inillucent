//! `CAST`, which is a command rather than a preference.
//!
//! Invariant: `CAST` always produces a value of the requested class. Where
//! affinity leaves `'abc'` as text under a numeric affinity, `CAST('abc' AS
//! INTEGER)` is the integer zero, and where affinity leaves a blob alone under
//! a text affinity, `CAST(x'616263' AS TEXT)` is `'abc'`. The two must not
//! share an implementation, because every place they differ is a place a
//! shared one would be wrong.
//!
//! Two details that a straightforward implementation gets wrong:
//!
//! - a numeric cast reads the *leading* number and ignores the rest, so
//!   `CAST('12abc' AS INTEGER)` is 12 rather than an error or a zero;
//! - a blob is cast by reinterpreting its bytes as text in the database
//!   encoding, so the same cast on a UTF-16 database produces different text.

use inillucent_base::DbResult;

use crate::affinity::{self, Affinity};
use crate::encoding::{self, TextEncoding};
use crate::numeric::{self, IntegerSyntax};
use crate::value::{BlobValue, Bytes, TextValue, Value};

/// Casts a value to the class an affinity names.
///
/// NULL casts to NULL for every target: `CAST(NULL AS INTEGER)` is NULL, not
/// zero, because a cast of the absence of a value cannot invent one.
pub fn cast_value<'a>(
    value: Value<'a>,
    target: Affinity,
    db_encoding: TextEncoding,
) -> DbResult<Value<'a>> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match target {
        Affinity::Blob => cast_to_blob(value, db_encoding),
        Affinity::Text => cast_to_text(value, db_encoding),
        Affinity::Integer => Ok(Value::Integer(integer_value(&value))),
        Affinity::Real => Ok(Value::Real(real_value(&value))),
        Affinity::Numeric | Affinity::FlexNum => Ok(numerify(value)),
    }
}

/// Returns the integer a value casts to, reading a leading number out of text.
///
/// This is `sqlite3VdbeIntValue`. A blob is read as though its bytes were
/// text, which is what makes `CAST(x'3132' AS INTEGER)` the number 12.
pub fn integer_value(value: &Value<'_>) -> i64 {
    match value {
        Value::Null => 0,
        Value::Integer(integer) => *integer,
        Value::Real(real) => numeric::real_to_i64(*real),
        Value::Text(text) => numeric::atoi64(text.raw(), text.encoding()).0,
        Value::Blob(blob) => numeric::atoi64(blob.raw(), TextEncoding::Utf8).0,
    }
}

/// Returns the double a value casts to, reading a leading number out of text.
///
/// This is `sqlite3VdbeRealValue`.
pub fn real_value(value: &Value<'_>) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Integer(integer) => *integer as f64,
        Value::Real(real) => *real,
        Value::Text(text) => numeric::atof(text.raw(), text.encoding()).value,
        Value::Blob(blob) => numeric::atof(blob.raw(), TextEncoding::Utf8).value,
    }
}

/// Casts to NUMERIC, which chooses between integer and real.
///
/// This is `sqlite3VdbeMemNumerify`. A value that already is a number is left
/// exactly as it is - `CAST(2.0 AS NUMERIC)` stays a real - and only text and
/// blobs are converted. The choice between the two numeric classes is the
/// double's: it becomes an integer when the double is exactly that integer and
/// the digits did not overflow, and a real otherwise.
pub fn numerify(value: Value<'_>) -> Value<'_> {
    match &value {
        Value::Integer(_) | Value::Real(_) | Value::Null => value,
        Value::Text(_) | Value::Blob(_) => {
            let (raw, text_encoding) = match &value {
                Value::Text(text) => (text.raw(), text.encoding()),
                Value::Blob(blob) => (blob.raw(), TextEncoding::Utf8),
                _ => (&[][..], TextEncoding::Utf8),
            };
            let parsed = numeric::atof(raw, text_encoding);
            let (from_digits, integer_syntax) = numeric::atoi64(raw, text_encoding);

            // Two independent routes to an integer, and SQLite takes either.
            //
            // The first is the digits themselves: when the text is integer
            // syntax - `rc & 2` clear - and reading it did not overflow, the
            // integer is what the digits say, whatever the double would have
            // rounded to. That is what keeps `9223372036854775807` exact
            // rather than turning it into 9223372036854775808.0.
            //
            // The second is the double: when the double is exactly an integer
            // inside the conservative 2^51 bound, that integer is used. This
            // is the route `'2.0'` takes, and the route `'abc'` takes to zero.
            let digits_are_usable = parsed.code() & 2 == 0
                && !matches!(
                    integer_syntax,
                    IntegerSyntax::Overflow | IntegerSyntax::TwoPow63
                );
            if digits_are_usable {
                return Value::Integer(from_digits);
            }
            let candidate = numeric::real_to_i64(parsed.value);
            if numeric::real_same_as_int(parsed.value, candidate) {
                Value::Integer(candidate)
            } else {
                Value::Real(parsed.value)
            }
        }
    }
}

/// Casts to TEXT, rendering numbers and reinterpreting blob bytes.
fn cast_to_text<'a>(value: Value<'a>, db_encoding: TextEncoding) -> DbResult<Value<'a>> {
    match value {
        Value::Blob(blob) => Ok(Value::Text(TextValue::new(
            Bytes::owned(blob.raw())?,
            db_encoding,
        ))),
        Value::Text(text) => Ok(Value::Text(text)),
        other => affinity::stringify(other, db_encoding),
    }
}

/// Casts to BLOB, rendering numbers as text first and taking the bytes.
fn cast_to_blob<'a>(value: Value<'a>, db_encoding: TextEncoding) -> DbResult<Value<'a>> {
    match value {
        Value::Blob(blob) => Ok(Value::Blob(blob)),
        Value::Text(text) => {
            let bytes = encoding::convert(text.raw(), text.encoding(), db_encoding);
            Ok(Value::Blob(BlobValue::new(Bytes::owned(bytes.as_ref())?)))
        }
        other => {
            let text = affinity::stringify(other, db_encoding)?;
            let bytes = text
                .as_text()
                .map(|text| text.raw().to_vec())
                .unwrap_or_default();
            Ok(Value::Blob(BlobValue::new(Bytes::owned(&bytes)?)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::StorageClass;

    /// Casting NULL never invents a value, whatever the target class is.
    #[test]
    fn casting_null_always_gives_null() {
        for target in Affinity::all() {
            let result = cast_value(Value::Null, target, TextEncoding::Utf8).unwrap();
            assert!(result.is_null(), "{target:?}");
        }
    }

    /// A cast always produces the requested class, which is the whole
    /// difference between a cast and an affinity.
    #[test]
    fn a_cast_always_produces_the_requested_class() {
        let inputs = [
            Value::Integer(7),
            Value::Real(7.5),
            Value::text_utf8(b"abc"),
            Value::blob(b"\x01\x02"),
        ];
        let expected = [
            (Affinity::Blob, StorageClass::Blob),
            (Affinity::Text, StorageClass::Text),
            (Affinity::Integer, StorageClass::Integer),
            (Affinity::Real, StorageClass::Real),
        ];
        for input in &inputs {
            for (target, class) in expected {
                let result = cast_value(input.clone(), target, TextEncoding::Utf8).unwrap();
                assert_eq!(result.storage_class(), class, "{input:?} as {target:?}");
            }
        }
    }

    /// A numeric cast reads the leading number and ignores the rest; text with
    /// no leading number at all becomes zero rather than failing.
    #[test]
    fn a_numeric_cast_reads_the_leading_number() {
        let cases: [(&str, i64, f64); 7] = [
            ("12abc", 12, 12.0),
            ("  -3xyz", -3, -3.0),
            ("abc", 0, 0.0),
            ("", 0, 0.0),
            ("1.9", 1, 1.9),
            ("1e3zz", 1, 1000.0),
            ("99999999999999999999", i64::MAX, 1e20),
        ];
        for (text, integer, real) in cases {
            let value = Value::text_utf8(text.as_bytes());
            assert_eq!(integer_value(&value), integer, "{text} as integer");
            assert_eq!(real_value(&value), real, "{text} as real");
        }
    }

    /// A blob is cast numerically by reading its bytes as text.
    #[test]
    fn a_blob_casts_numerically_through_its_bytes() {
        assert_eq!(integer_value(&Value::blob(b"12")), 12);
        assert_eq!(real_value(&Value::blob(b"2.5")), 2.5);
        assert_eq!(integer_value(&Value::blob(b"\x00\x01")), 0);
    }

    /// A real truncates toward zero on the way to an integer.
    #[test]
    fn casting_a_real_to_an_integer_truncates() {
        assert_eq!(integer_value(&Value::Real(1.9)), 1);
        assert_eq!(integer_value(&Value::Real(-1.9)), -1);
        assert_eq!(integer_value(&Value::Real(1e300)), i64::MAX);
    }

    /// A cast to NUMERIC leaves a number alone and chooses a class for text.
    #[test]
    fn a_numeric_cast_leaves_numbers_alone_and_chooses_for_text() {
        assert!(matches!(numerify(Value::Real(2.0)), Value::Real(_)));
        assert!(matches!(numerify(Value::Integer(2)), Value::Integer(2)));
        assert!(matches!(
            numerify(Value::text_utf8(b"2")),
            Value::Integer(2)
        ));
        assert!(matches!(
            numerify(Value::text_utf8(b"2.0")),
            Value::Integer(2)
        ));
        assert!(matches!(numerify(Value::text_utf8(b"2.5")), Value::Real(_)));
        assert!(matches!(
            numerify(Value::text_utf8(b"abc")),
            Value::Integer(0)
        ));
        // Integer syntax keeps every bit even past the range a double can
        // represent exactly.
        assert!(matches!(
            numerify(Value::text_utf8(b"9223372036854775807")),
            Value::Integer(i64::MAX)
        ));
        assert!(matches!(
            numerify(Value::text_utf8(b"9007199254740993")),
            Value::Integer(9_007_199_254_740_993)
        ));
        // One past the range overflows and falls back to the double.
        assert!(matches!(
            numerify(Value::text_utf8(b"9223372036854775808")),
            Value::Real(_)
        ));
        // Exponential syntax is not integer syntax, so it stays a real even
        // when the value is a whole number inside the range.
        assert!(matches!(
            numerify(Value::text_utf8(b"1e18")),
            Value::Real(_)
        ));
        // Integer syntax with trailing bytes still reads its leading digits.
        assert!(matches!(
            numerify(Value::text_utf8(b"12abc")),
            Value::Integer(12)
        ));
    }

    /// A blob casts to text by reinterpreting its bytes in the database
    /// encoding, so the same blob is different text in a UTF-16 database.
    #[test]
    fn a_blob_casts_to_text_in_the_database_encoding() {
        let utf8 = cast_value(Value::blob(b"abc"), Affinity::Text, TextEncoding::Utf8).unwrap();
        assert_eq!(utf8.as_text().unwrap().utf8_bytes().as_ref(), b"abc");
        let wide = cast_value(
            Value::blob(&[0x61, 0x00, 0x62, 0x00]),
            Affinity::Text,
            TextEncoding::Utf16Le,
        )
        .unwrap();
        assert_eq!(wide.as_text().unwrap().utf8_bytes().as_ref(), b"ab");
    }

    /// Text casts to a blob by taking its bytes in the database encoding, so
    /// a UTF-16 database produces the wide bytes.
    #[test]
    fn text_casts_to_a_blob_in_the_database_encoding() {
        let narrow =
            cast_value(Value::text_utf8(b"ab"), Affinity::Blob, TextEncoding::Utf8).unwrap();
        assert_eq!(narrow.as_blob().unwrap().raw(), b"ab");
        let wide = cast_value(
            Value::text_utf8(b"ab"),
            Affinity::Blob,
            TextEncoding::Utf16Le,
        )
        .unwrap();
        assert_eq!(wide.as_blob().unwrap().raw(), &[0x61, 0x00, 0x62, 0x00]);
    }

    /// A number casts to a blob through its text form.
    #[test]
    fn a_number_casts_to_a_blob_through_its_text() {
        let blob = cast_value(Value::Integer(42), Affinity::Blob, TextEncoding::Utf8).unwrap();
        assert_eq!(blob.as_blob().unwrap().raw(), b"42");
        let blob = cast_value(Value::Real(2.5), Affinity::Blob, TextEncoding::Utf8).unwrap();
        assert_eq!(blob.as_blob().unwrap().raw(), b"2.5");
    }

    /// Casting arbitrary bytes must never panic; a value comes off a page.
    #[test]
    fn casting_arbitrary_bytes_never_panics() {
        let mut state = 0xdead_beef_cafe_babeu64;
        for _ in 0..inillucent_base::probe::sample_rounds(20_000) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state % 18) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|index| (state >> (index % 8 * 8)) as u8)
                .collect();
            for target in Affinity::all() {
                for db_encoding in TextEncoding::all() {
                    let _ = cast_value(Value::blob(&bytes), target, db_encoding).unwrap();
                    let _ = cast_value(Value::text_utf8(&bytes), target, db_encoding).unwrap();
                }
            }
        }
    }
}
