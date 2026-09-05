//! Column affinity: classifying a declared type, and applying the result.
//!
//! Invariant: affinity is decided by SQLite's ordered substring rules and by
//! nothing else. There is no table of known type names here, because SQLite
//! does not have one either - `VARCHAR(20)` is text because it contains
//! `CHAR`, and `POINT` is an integer because it contains `INT`. A lookup table
//! would be right for the names someone thought of and wrong for the rest,
//! which is exactly the class of bug a parity engine cannot have.
//!
//! Applying an affinity is a separate step from classifying one, and is not
//! the same operation as `CAST`. Affinity is a preference: it converts text
//! that is a well-formed number and leaves everything else alone. `CAST` is a
//! command: it converts whatever it is given, producing zero when the text is
//! not a number at all.

use inillucent_base::DbResult;

use crate::encoding::TextEncoding;
use crate::numeric::{self, RealSyntax};
use crate::value::{Bytes, TextValue, Value};

/// The affinity of a column or an expression.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Affinity {
    /// No affinity at all: values are stored exactly as supplied. This is what
    /// SQLite calls BLOB affinity and what a column with no declared type has.
    Blob,
    /// Text affinity: numbers become text, blobs are left alone.
    Text,
    /// Numeric affinity: text that is a number becomes one, preferring an
    /// integer when the value is exactly an integer.
    Numeric,
    /// Integer affinity: identical to numeric in what it converts.
    Integer,
    /// Real affinity: numeric, and stored as a real.
    Real,
    /// Flexible numeric, which SQLite uses for view and subquery columns whose
    /// type came from an expression rather than from a declaration.
    FlexNum,
}

impl Affinity {
    /// Returns the byte SQLite uses for this affinity in a schema record and
    /// in a comparison opcode's operand.
    pub fn code(self) -> u8 {
        match self {
            Affinity::Blob => 0x41,
            Affinity::Text => 0x42,
            Affinity::Numeric => 0x43,
            Affinity::Integer => 0x44,
            Affinity::Real => 0x45,
            Affinity::FlexNum => 0x46,
        }
    }

    /// Returns the affinity a code names, if it names one.
    pub fn from_code(code: u8) -> Option<Affinity> {
        Some(match code {
            0x40 | 0x41 => Affinity::Blob,
            0x42 => Affinity::Text,
            0x43 => Affinity::Numeric,
            0x44 => Affinity::Integer,
            0x45 => Affinity::Real,
            0x46 => Affinity::FlexNum,
            _ => return None,
        })
    }

    /// Reports whether this affinity prefers a number.
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Affinity::Numeric | Affinity::Integer | Affinity::Real | Affinity::FlexNum
        )
    }

    /// Returns every affinity, for exhaustive matrices.
    pub fn all() -> [Affinity; 6] {
        [
            Affinity::Blob,
            Affinity::Text,
            Affinity::Numeric,
            Affinity::Integer,
            Affinity::Real,
            Affinity::FlexNum,
        ]
    }
}

/// Classifies a declared type into an affinity, by SQLite's ordered rules.
///
/// The rules are applied over a rolling window of the last characters seen, so
/// a substring anywhere in the name decides the answer:
///
/// 1. `INT` anywhere wins immediately and gives INTEGER.
/// 2. Otherwise `CHAR`, `CLOB` or `TEXT` gives TEXT.
/// 3. Otherwise `BLOB` gives BLOB, but only while nothing stronger was found.
/// 4. Otherwise `REAL`, `FLOA` or `DOUB` gives REAL.
/// 5. Otherwise NUMERIC.
///
/// A column declared with no type at all is not classified by this function at
/// all; SQLite gives it BLOB affinity directly, which `for_column` does.
pub fn affinity_of_declared_type(declared: &[u8]) -> Affinity {
    let mut affinity = Affinity::Numeric;
    let mut window: u32 = 0;
    for byte in declared {
        window = (window << 8) | u32::from(byte.to_ascii_lowercase());
        if window == word(b"char") || window == word(b"clob") || window == word(b"text") {
            affinity = Affinity::Text;
        } else if window == word(b"blob") && matches!(affinity, Affinity::Numeric | Affinity::Real)
        {
            affinity = Affinity::Blob;
        } else if (window == word(b"real") || window == word(b"floa") || window == word(b"doub"))
            && affinity == Affinity::Numeric
        {
            affinity = Affinity::Real;
        } else if window & 0x00ff_ffff == three(b"int") {
            return Affinity::Integer;
        }
    }
    affinity
}

/// Returns the affinity of a column with the given declared type.
///
/// An empty declaration is a column with no type, which has BLOB affinity.
pub fn for_column(declared: &[u8]) -> Affinity {
    if declared.is_empty() {
        Affinity::Blob
    } else {
        affinity_of_declared_type(declared)
    }
}

/// Packs a four-character word into the rolling window's representation.
fn word(text: &[u8; 4]) -> u32 {
    (u32::from(text[0]) << 24)
        | (u32::from(text[1]) << 16)
        | (u32::from(text[2]) << 8)
        | u32::from(text[3])
}

/// Packs a three-character word into the low three bytes of the window.
fn three(text: &[u8; 3]) -> u32 {
    (u32::from(text[0]) << 16) | (u32::from(text[1]) << 8) | u32::from(text[2])
}

/// Applies numeric affinity to a value, converting text that is a number.
///
/// `try_for_integer` is SQLite's `bTryForInt`: with it set, a value that ends
/// up real is narrowed to an integer when the two are the same value. It is
/// set on the storage path and clear where a real must stay a real.
pub fn apply_numeric_affinity<'a>(value: Value<'a>, try_for_integer: bool) -> Value<'a> {
    let Value::Text(text) = &value else {
        return value;
    };
    let raw = text.raw();
    let encoding = text.encoding();
    let parsed = numeric::atof(raw, encoding);
    if !parsed.is_number() {
        return value;
    }
    if parsed.syntax == RealSyntax::Integer {
        if let Some(integer) = also_an_integer(raw, encoding, parsed.value) {
            return Value::Integer(integer);
        }
    }
    let real = parsed.value;
    if try_for_integer {
        let candidate = numeric::real_to_i64(real);
        if numeric::real_same_as_int(real, candidate) {
            return Value::Integer(candidate);
        }
    }
    Value::Real(real)
}

/// Reports the integer a value with integer syntax should become.
///
/// SQLite prefers the double when the double is exactly the integer, and falls
/// back to reading the digits directly, which is what lets a nineteen-digit
/// literal keep every bit rather than going through a double first.
fn also_an_integer(raw: &[u8], encoding: TextEncoding, real: f64) -> Option<i64> {
    let candidate = numeric::real_to_i64(real);
    if numeric::real_same_as_int(real, candidate) {
        return Some(candidate);
    }
    let (integer, syntax) = numeric::atoi64(raw, encoding);
    syntax.is_exact().then_some(integer)
}

/// Narrows a real to an integer when the two are the same value.
///
/// This is `sqlite3VdbeIntegerAffinity`, the step that makes `2.0` stored in a
/// NUMERIC column come back as the integer 2.
///
/// Its test is *not* `real_same_as_int`, and the difference matters. SQLite has
/// two "is this double an integer" tests and they disagree above 2^51:
/// `sqlite3RealSameAsInt` is deliberately conservative because it decides
/// whether a comparison may treat the two as equal, while this one only has to
/// decide whether the value can be stored more compactly, so it accepts any
/// double that converts back to itself anywhere inside the `i64` range. The
/// bounds are strict because `real_to_i64` saturates, and without them every
/// value past the range would narrow to `i64::MAX`. Using the conservative
/// test here leaves `9007199254740992.0` a real in an INTEGER column, which is
/// what the differential run against 3.53.4 caught.
pub fn integer_affinity(value: Value<'_>) -> Value<'_> {
    let Value::Real(real) = value else {
        return value;
    };
    let candidate = numeric::real_to_i64(real);
    if real == candidate as f64 && candidate > i64::MIN && candidate < i64::MAX {
        Value::Integer(candidate)
    } else {
        Value::Real(real)
    }
}

/// Renders a number as text in `encoding`, leaving other classes alone.
///
/// This is `sqlite3VdbeMemStringify`, which text affinity uses. A blob is not
/// converted: text affinity has no effect on a blob, which is one of the two
/// places affinity and `CAST` visibly disagree.
pub fn stringify(value: Value<'_>, encoding: TextEncoding) -> DbResult<Value<'static>> {
    let rendered = match &value {
        Value::Integer(integer) => numeric::integer_to_text(*integer),
        Value::Real(real) => numeric::real_to_text(*real),
        _ => return value.into_owned(),
    };
    let converted = crate::encoding::from_utf8(&rendered, encoding);
    Ok(Value::Text(TextValue::new(
        Bytes::owned(converted.as_ref())?,
        encoding,
    )))
}

/// Applies an affinity to a value, as SQLite does before comparing or storing.
///
/// This is deliberately not a total conversion. Text that is not a number
/// stays text under a numeric affinity, and a blob stays a blob under a text
/// affinity, because affinity is a preference and `CAST` is the command.
pub fn apply_affinity<'a>(
    value: Value<'a>,
    affinity: Affinity,
    encoding: TextEncoding,
) -> DbResult<Value<'a>> {
    match affinity {
        Affinity::Blob => Ok(value),
        Affinity::Text => {
            if matches!(value, Value::Integer(_) | Value::Real(_)) {
                stringify(value, encoding)
            } else {
                Ok(value)
            }
        }
        Affinity::Numeric | Affinity::Integer | Affinity::Real | Affinity::FlexNum => match value {
            Value::Integer(_) => Ok(value),
            Value::Text(_) => Ok(apply_numeric_affinity(value, true)),
            Value::Real(_) if affinity != Affinity::FlexNum => Ok(integer_affinity(value)),
            other => Ok(other),
        },
    }
}

/// Applies the affinity a REAL column stores with.
///
/// A REAL column holds a real on disk even when the value narrows to an
/// integer, so the storage path widens it back after the shared affinity rules
/// have run. SQLite encodes such a value with the integer serial types and a
/// flag saying it is really a real; the widening here is the same decision
/// made where it is visible.
pub fn realify(value: Value<'_>) -> Value<'_> {
    match value {
        Value::Integer(integer) => Value::Real(integer as f64),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The examples SQLite's own documentation gives for the affinity rules,
    /// including the ones that surprise people.
    #[test]
    fn the_documented_declared_types_classify_as_documented() {
        let cases: [(&str, Affinity); 24] = [
            ("INT", Affinity::Integer),
            ("INTEGER", Affinity::Integer),
            ("TINYINT", Affinity::Integer),
            ("SMALLINT", Affinity::Integer),
            ("MEDIUMINT", Affinity::Integer),
            ("BIGINT", Affinity::Integer),
            ("UNSIGNED BIG INT", Affinity::Integer),
            ("INT2", Affinity::Integer),
            ("INT8", Affinity::Integer),
            ("CHARACTER(20)", Affinity::Text),
            ("VARCHAR(255)", Affinity::Text),
            ("VARYING CHARACTER(255)", Affinity::Text),
            ("NCHAR(55)", Affinity::Text),
            ("NATIVE CHARACTER(70)", Affinity::Text),
            ("NVARCHAR(100)", Affinity::Text),
            ("TEXT", Affinity::Text),
            ("CLOB", Affinity::Text),
            ("BLOB", Affinity::Blob),
            ("REAL", Affinity::Real),
            ("DOUBLE", Affinity::Real),
            ("DOUBLE PRECISION", Affinity::Real),
            ("FLOAT", Affinity::Real),
            ("NUMERIC", Affinity::Numeric),
            ("DECIMAL(10,5)", Affinity::Numeric),
        ];
        for (declared, expected) in cases {
            assert_eq!(
                affinity_of_declared_type(declared.as_bytes()),
                expected,
                "{declared}"
            );
        }
        assert_eq!(for_column(b"BOOLEAN"), Affinity::Numeric);
        assert_eq!(for_column(b"DATE"), Affinity::Numeric);
        assert_eq!(for_column(b"DATETIME"), Affinity::Numeric);
        assert_eq!(for_column(b"STRING"), Affinity::Numeric);
    }

    /// The rules are substring rules, so the surprising names have to come out
    /// the surprising way. These are the cases a name table would get wrong.
    #[test]
    fn the_surprising_substring_cases_follow_the_rules() {
        // POINT contains INT.
        assert_eq!(for_column(b"POINT"), Affinity::Integer);
        // So does a name that only mentions integers by accident.
        assert_eq!(for_column(b"PRINTABLE"), Affinity::Integer);
        // CHARINT: TEXT is found first, then INT overrides it and breaks out.
        assert_eq!(for_column(b"CHARINT"), Affinity::Integer);
        // INTCHAR: INT wins immediately and the CHAR is never reached.
        assert_eq!(for_column(b"INTCHAR"), Affinity::Integer);
        // TEXT beats a later BLOB, because BLOB only applies while the
        // affinity is still numeric or real.
        assert_eq!(for_column(b"TEXTBLOB"), Affinity::Text);
        // BLOB beats a later REAL for the same reason.
        assert_eq!(for_column(b"BLOBREAL"), Affinity::Blob);
        // REAL is found first, and BLOB may still override a real.
        assert_eq!(for_column(b"REALBLOB"), Affinity::Blob);
        // Case is folded.
        assert_eq!(for_column(b"vArChAr"), Affinity::Text);
        // No declared type at all is BLOB affinity.
        assert_eq!(for_column(b""), Affinity::Blob);
    }

    /// Numeric affinity converts text that is a whole number, and leaves text
    /// that is not a number completely alone.
    #[test]
    fn numeric_affinity_converts_only_well_formed_numbers() {
        let converted = apply_numeric_affinity(Value::text_utf8(b"42"), true);
        assert!(matches!(converted, Value::Integer(42)));
        let converted = apply_numeric_affinity(Value::text_utf8(b"  -17  "), true);
        assert!(matches!(converted, Value::Integer(-17)));
        let converted = apply_numeric_affinity(Value::text_utf8(b"2.5"), true);
        assert!(matches!(converted, Value::Real(value) if value == 2.5));
        for text in ["abc", "12abc", "", "1.5x", "0x10"] {
            let untouched = apply_numeric_affinity(Value::text_utf8(text.as_bytes()), true);
            assert!(matches!(untouched, Value::Text(_)), "{text} was converted");
        }
    }

    /// A real that is exactly an integer narrows, and one that is not does
    /// not. This is what makes `'2.0'` in a NUMERIC column an integer.
    #[test]
    fn a_real_narrows_to_an_integer_only_when_it_is_one() {
        let narrowed = apply_numeric_affinity(Value::text_utf8(b"2.0"), true);
        assert!(matches!(narrowed, Value::Integer(2)));
        let kept = apply_numeric_affinity(Value::text_utf8(b"2.5"), true);
        assert!(matches!(kept, Value::Real(_)));
        let kept = apply_numeric_affinity(Value::text_utf8(b"2.0"), false);
        assert!(matches!(kept, Value::Real(value) if value == 2.0));
        assert!(matches!(
            integer_affinity(Value::Real(3.0)),
            Value::Integer(3)
        ));
        assert!(matches!(integer_affinity(Value::Real(3.5)), Value::Real(_)));
        // Past 2^51 the two "is this double an integer" tests disagree, and
        // this one is the permissive one.
        let big = 9_007_199_254_740_992.0f64;
        assert!(!numeric::real_same_as_int(big, 9_007_199_254_740_992));
        assert!(matches!(
            integer_affinity(Value::Real(big)),
            Value::Integer(9_007_199_254_740_992)
        ));
        // A double outside the integer range must not narrow to the bound.
        assert!(matches!(
            integer_affinity(Value::Real(1e300)),
            Value::Real(_)
        ));
        assert!(matches!(
            integer_affinity(Value::Real(f64::INFINITY)),
            Value::Real(_)
        ));
    }

    /// A nineteen-digit integer must keep every bit rather than being routed
    /// through a double, which would lose the low bits.
    #[test]
    fn a_large_integer_keeps_every_bit() {
        let exact = apply_numeric_affinity(Value::text_utf8(b"9007199254740993"), true);
        assert!(
            matches!(exact, Value::Integer(9_007_199_254_740_993)),
            "{exact:?}"
        );
        let exact = apply_numeric_affinity(Value::text_utf8(b"9223372036854775807"), true);
        assert!(matches!(exact, Value::Integer(i64::MAX)), "{exact:?}");
    }

    /// Text affinity renders numbers and leaves blobs and NULL alone; this is
    /// where affinity and CAST visibly disagree.
    #[test]
    fn text_affinity_renders_numbers_and_ignores_blobs() {
        let rendered =
            apply_affinity(Value::Integer(42), Affinity::Text, TextEncoding::Utf8).unwrap();
        assert_eq!(
            rendered.as_text().map(|text| text.raw().to_vec()),
            Some(b"42".to_vec())
        );
        let rendered =
            apply_affinity(Value::Real(2.5), Affinity::Text, TextEncoding::Utf8).unwrap();
        assert_eq!(
            rendered.as_text().map(|text| text.raw().to_vec()),
            Some(b"2.5".to_vec())
        );
        let blob =
            apply_affinity(Value::blob(b"\x00\x01"), Affinity::Text, TextEncoding::Utf8).unwrap();
        assert!(matches!(blob, Value::Blob(_)));
        let null = apply_affinity(Value::Null, Affinity::Text, TextEncoding::Utf8).unwrap();
        assert!(null.is_null());
    }

    /// Blob affinity is the absence of one: nothing is converted at all.
    #[test]
    fn blob_affinity_converts_nothing() {
        for value in [
            Value::Null,
            Value::Integer(1),
            Value::Real(1.5),
            Value::text_utf8(b"1"),
            Value::blob(b"1"),
        ] {
            let class = value.storage_class();
            let after = apply_affinity(value, Affinity::Blob, TextEncoding::Utf8).unwrap();
            assert_eq!(after.storage_class(), class);
        }
    }

    /// Text affinity renders into the database's encoding, not always UTF-8.
    #[test]
    fn text_affinity_renders_in_the_database_encoding() {
        let rendered =
            apply_affinity(Value::Integer(-5), Affinity::Text, TextEncoding::Utf16Le).unwrap();
        let text = rendered.as_text().unwrap();
        assert_eq!(text.encoding(), TextEncoding::Utf16Le);
        assert_eq!(text.utf8_bytes().as_ref(), b"-5");
    }

    /// Every affinity code round-trips, because the code is what a schema
    /// record and a comparison opcode carry.
    #[test]
    fn affinity_codes_round_trip() {
        for affinity in Affinity::all() {
            assert_eq!(Affinity::from_code(affinity.code()), Some(affinity));
        }
        assert_eq!(Affinity::from_code(0x40), Some(Affinity::Blob));
        assert_eq!(Affinity::from_code(0x00), None);
    }

    /// Classifying arbitrary bytes must terminate and never panic; a declared
    /// type comes out of a schema record and is attacker-controlled.
    #[test]
    fn classifying_arbitrary_bytes_never_panics() {
        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..inillucent_base::probe::sample_rounds(20_000) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state % 20) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|index| (state >> (index % 8 * 8)) as u8)
                .collect();
            let _ = for_column(&bytes);
        }
    }
}
