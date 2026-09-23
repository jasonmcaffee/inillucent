//! Text-to-number and number-to-text conversion, to SQLite's rules.
//!
//! Invariant: nothing here calls Rust's own `parse` or `to_string` on a whole
//! user string. Rust's float parser is correctly rounded over the entire input;
//! SQLite's reads at most nineteen significant digits into a 64-bit integer,
//! *truncates* the rest, and tracks a decimal exponent. On an input with more
//! than nineteen significant digits the two disagree, and a difference of one
//! ulp is a different value in an index and a different answer in a query. So
//! the scanner here is SQLite's scanner, digit for digit, and Rust's correctly
//! rounded conversion is used only for the final `significand x 10^exponent` -
//! the step SQLite performs with long-double or Dekker arithmetic to get the
//! same correctly rounded result.
//!
//! The functions are named after the SQLite ones they reproduce so that a
//! reader can put them side by side: `atoi64` is `sqlite3Atoi64`, `atof` is
//! `sqlite3AtoF`, `real_to_i64` is `sqlite3RealToI64`, `real_same_as_int` is
//! `sqlite3RealSameAsInt`, and `real_to_text` is `%!.17g`.

use std::borrow::Cow;

use crate::encoding::{self, TextEncoding};

/// The largest positive value an `i64` holds, as SQLite's `LARGEST_INT64`.
const LARGEST_INT64: i64 = i64::MAX;
/// The most negative value an `i64` holds, as SQLite's `SMALLEST_INT64`.
const SMALLEST_INT64: i64 = i64::MIN;
/// The point past which SQLite stops accumulating significand digits.
const SIGNIFICAND_CEILING: i64 = (LARGEST_INT64 - 9) / 10;
/// The bound inside which a double and an integer are held to be the same
/// value. SQLite uses 2^51 so that a double whose integer neighbours are more
/// than one apart is never called equal to one of them.
const SAME_AS_INT_BOUND: i64 = 2_251_799_813_685_248;

/// What `atoi64` found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegerSyntax {
    /// The whole input was an integer and it fits in an `i64`.
    Exact,
    /// There were no digits at all.
    NoDigits,
    /// There were digits, and then something that is not whitespace.
    TrailingBytes,
    /// The digits form a number too large for an `i64`.
    Overflow,
    /// The digits are exactly 9223372036854775808, which fits only as a
    /// negative number. SQLite keeps this case apart because the positive form
    /// overflows while the negative form is `i64::MIN` exactly.
    TwoPow63,
}

impl IntegerSyntax {
    /// Reports whether the input was a clean integer that fits.
    ///
    /// This is the `sqlite3Atoi64(...)==0` test that decides whether text gets
    /// integer rather than real affinity.
    pub fn is_exact(self) -> bool {
        matches!(self, IntegerSyntax::Exact)
    }
}

/// What `atof` found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealSyntax {
    /// Not a number: no digits, or a malformed exponent.
    NotNumeric,
    /// Digits only, with no decimal point and no exponent.
    Integer,
    /// A decimal point was present.
    Fractional,
    /// An exponent was present.
    Exponential,
}

/// The outcome of reading a real out of text.
#[derive(Clone, Copy, Debug)]
pub struct ParsedReal {
    /// The value, which is zero when the text was not numeric at all.
    pub value: f64,
    /// The syntax the scanner recognised.
    pub syntax: RealSyntax,
    /// Whether the whole input was consumed, ignoring surrounding whitespace.
    pub complete: bool,
    /// How many significand digits were read.
    pub digits: usize,
    /// Whether an exponent, if one was written, had digits after it.
    pub exponent_valid: bool,
}

impl ParsedReal {
    /// Reports whether the entire input was a well-formed number.
    ///
    /// This is `sqlite3AtoF(...) > 0`, the test that gates numeric affinity.
    /// Text with a numeric prefix and trailing bytes is deliberately not a
    /// number here: SQLite returns a negative code for it and its callers
    /// leave the value as text.
    pub fn is_number(self) -> bool {
        self.complete && self.syntax != RealSyntax::NotNumeric
    }

    /// Returns the code `sqlite3AtoF` returns for this input.
    ///
    /// The value matters because callers branch on more than "was it a
    /// number": `sqlite3VdbeMemNumerify` tests `rc & 2`, which separates
    /// *integer* syntax from anything with a decimal point or an exponent, and
    /// that single bit is why `CAST('1e18' AS NUMERIC)` is a real while
    /// `CAST('1000000000000000000' AS NUMERIC)` is an integer. The negative
    /// code is the "looks like a real but has trailing bytes" case, and it
    /// also has the bit set.
    ///
    ///  - `1`, `2`, `3` - the whole input was an integer, a fraction, or an
    ///    exponential form;
    ///  - `-1` - a fraction or an exponential form followed by something else;
    ///  - `0` - anything else, including text that is not a number at all.
    pub fn code(self) -> i32 {
        let kind = match self.syntax {
            RealSyntax::NotNumeric => return 0,
            RealSyntax::Integer => 1,
            RealSyntax::Fractional => 2,
            RealSyntax::Exponential => 3,
        };
        if self.digits == 0 {
            return 0;
        }
        if self.complete {
            return kind;
        }
        if kind >= 2 && (kind == 3 || self.exponent_valid) {
            return -1;
        }
        0
    }
}

/// Reports whether a byte is whitespace by SQLite's definition.
///
/// SQLite's `sqlite3Isspace` is space, tab, newline, vertical tab, form feed
/// and carriage return - the ASCII set, with no locale and no Unicode.
pub fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Reports whether a byte is an ASCII digit.
pub fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

/// Converts text bytes in `encoding` into the ASCII bytes the scanners read.
///
/// SQLite's scanners walk UTF-16 input two bytes at a time and treat a
/// non-zero high byte as "this is not a number", which is exactly what
/// dropping to the low byte and remembering that it happened reproduces.
///
/// **A `Cow` and not a `Vec`, because UTF-8 needs no view at all** (task-2006). This
/// returned `bytes.to_vec()` for UTF-8, so every call of `atoi64` and `atof` copied its
/// input to the heap in order to read it - on the compile path for every numeric literal,
/// and at run time for every text value given numeric affinity, compared with a number or
/// passed through `CAST`. A backtrace on each allocation a `SELECT 1` compile makes found
/// this one under `atoi64` for the single byte `1`. UTF-16 still builds a vector, because
/// there the low bytes are a new sequence rather than a window on an existing one.
///
/// @param bytes - the text, in `encoding`
/// @param encoding - how the text is encoded
fn ascii_view(bytes: &[u8], encoding: TextEncoding) -> (Cow<'_, [u8]>, bool) {
    match encoding {
        TextEncoding::Utf8 => (Cow::Borrowed(bytes), false),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let big_endian = encoding == TextEncoding::Utf16Be;
            let mut output = Vec::with_capacity(bytes.len() / 2);
            let mut non_ascii = false;
            let mut index = 0usize;
            while index.saturating_add(1) < bytes.len() {
                let (low, high) = if big_endian {
                    (
                        bytes.get(index.saturating_add(1)).copied().unwrap_or(0),
                        bytes.get(index).copied().unwrap_or(0),
                    )
                } else {
                    (
                        bytes.get(index).copied().unwrap_or(0),
                        bytes.get(index.saturating_add(1)).copied().unwrap_or(0),
                    )
                };
                if high != 0 {
                    non_ascii = true;
                }
                output.push(low);
                index = index.saturating_add(2);
            }
            (Cow::Owned(output), non_ascii)
        }
    }
}

/// Reads a signed 64-bit integer out of text, as `sqlite3Atoi64` does.
///
/// Returns the value and what the scanner found. On overflow the value is
/// saturated to the `i64` bound with the sign the text had, which is what
/// SQLite stores before reporting the overflow.
pub fn atoi64(bytes: &[u8], encoding: TextEncoding) -> (i64, IntegerSyntax) {
    let (text, non_ascii) = ascii_view(bytes, encoding);
    let mut index = 0usize;
    while text.get(index).copied().is_some_and(is_space) {
        index = index.saturating_add(1);
    }
    let negative = match text.get(index) {
        Some(b'-') => {
            index = index.saturating_add(1);
            true
        }
        Some(b'+') => {
            index = index.saturating_add(1);
            false
        }
        _ => false,
    };
    let digits_start = index;
    while text.get(index) == Some(&b'0') {
        index = index.saturating_add(1);
    }
    let significant_start = index;
    let mut accumulated: u64 = 0;
    while text.get(index).copied().is_some_and(is_digit) {
        let digit = u64::from(text.get(index).copied().unwrap_or(b'0').wrapping_sub(b'0'));
        accumulated = accumulated.wrapping_mul(10).wrapping_add(digit);
        index = index.saturating_add(1);
    }
    let digit_count = index.saturating_sub(significant_start);

    let mut value = if accumulated > LARGEST_INT64 as u64 {
        SMALLEST_INT64
    } else if negative {
        (accumulated as i64).wrapping_neg()
    } else {
        accumulated as i64
    };

    let mut syntax = IntegerSyntax::Exact;
    if digit_count == 0 && digits_start == index {
        syntax = IntegerSyntax::NoDigits;
    } else if non_ascii {
        syntax = IntegerSyntax::TrailingBytes;
    } else if index < text.len() {
        let mut scan = index;
        while scan < text.len() {
            if !text.get(scan).copied().is_some_and(is_space) {
                syntax = IntegerSyntax::TrailingBytes;
                break;
            }
            scan = scan.saturating_add(1);
        }
    }

    if digit_count >= 19 {
        let ordering = if digit_count > 19 {
            std::cmp::Ordering::Greater
        } else {
            compare_to_two_pow_63(text.get(significant_start..index).unwrap_or(&[]))
        };
        match ordering {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Greater => {
                value = if negative {
                    SMALLEST_INT64
                } else {
                    LARGEST_INT64
                };
                return (value, IntegerSyntax::Overflow);
            }
            std::cmp::Ordering::Equal => {
                if negative {
                    value = SMALLEST_INT64;
                } else {
                    value = LARGEST_INT64;
                    return (value, IntegerSyntax::TwoPow63);
                }
            }
        }
    }
    (value, syntax)
}

/// Compares nineteen digits against 9223372036854775808.
fn compare_to_two_pow_63(digits: &[u8]) -> std::cmp::Ordering {
    const TWO_POW_63: &[u8; 19] = b"9223372036854775808";
    for (index, expected) in TWO_POW_63.iter().enumerate() {
        let actual = digits.get(index).copied().unwrap_or(b'0');
        match actual.cmp(expected) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

/// Reads a double out of text, as `sqlite3AtoF` does.
pub fn atof(bytes: &[u8], encoding: TextEncoding) -> ParsedReal {
    let (text, non_ascii) = ascii_view(bytes, encoding);
    let not_a_number = ParsedReal {
        value: 0.0,
        syntax: RealSyntax::NotNumeric,
        complete: false,
        digits: 0,
        exponent_valid: true,
    };
    if text.is_empty() {
        return not_a_number;
    }

    let mut index = 0usize;
    while text.get(index).copied().is_some_and(is_space) {
        index = index.saturating_add(1);
    }
    if index >= text.len() {
        return not_a_number;
    }

    let mut sign = 1i32;
    match text.get(index) {
        Some(b'-') => {
            sign = -1;
            index = index.saturating_add(1);
        }
        Some(b'+') => index = index.saturating_add(1),
        _ => {}
    }

    // The significand, capped exactly where SQLite caps it, with `shift`
    // counting the decimal places the cap and the fraction move the point by.
    let mut significand: i64 = 0;
    let mut shift: i32 = 0;
    let mut digit_count = 0usize;
    let mut syntax = RealSyntax::Integer;

    while text.get(index).copied().is_some_and(is_digit) {
        let digit = i64::from(text.get(index).copied().unwrap_or(b'0').wrapping_sub(b'0'));
        significand = significand.wrapping_mul(10).wrapping_add(digit);
        index = index.saturating_add(1);
        digit_count = digit_count.saturating_add(1);
        if significand >= SIGNIFICAND_CEILING {
            while text.get(index).copied().is_some_and(is_digit) {
                index = index.saturating_add(1);
                shift = shift.saturating_add(1);
            }
            break;
        }
    }

    let mut exponent_valid = true;
    if text.get(index) == Some(&b'.') {
        index = index.saturating_add(1);
        syntax = RealSyntax::Fractional;
        while text.get(index).copied().is_some_and(is_digit) {
            if significand < SIGNIFICAND_CEILING {
                let digit = i64::from(text.get(index).copied().unwrap_or(b'0').wrapping_sub(b'0'));
                significand = significand.wrapping_mul(10).wrapping_add(digit);
                shift = shift.saturating_sub(1);
                digit_count = digit_count.saturating_add(1);
            }
            index = index.saturating_add(1);
        }
    }

    let mut exponent: i32 = 0;
    let mut exponent_sign: i32 = 1;
    if matches!(text.get(index), Some(b'e') | Some(b'E')) {
        index = index.saturating_add(1);
        exponent_valid = false;
        syntax = RealSyntax::Exponential;
        match text.get(index) {
            Some(b'-') => {
                exponent_sign = -1;
                index = index.saturating_add(1);
            }
            Some(b'+') => index = index.saturating_add(1),
            _ => {}
        }
        while text.get(index).copied().is_some_and(is_digit) {
            let digit = i32::from(text.get(index).copied().unwrap_or(b'0').wrapping_sub(b'0'));
            exponent = if exponent < 10_000 {
                exponent.saturating_mul(10).saturating_add(digit)
            } else {
                10_000
            };
            index = index.saturating_add(1);
            exponent_valid = true;
        }
    }

    while text.get(index).copied().is_some_and(is_space) {
        index = index.saturating_add(1);
    }

    let value = compute_real(
        sign,
        significand,
        exponent.saturating_mul(exponent_sign),
        shift,
    );
    let complete = index >= text.len() && digit_count > 0 && exponent_valid && !non_ascii;
    ParsedReal {
        value,
        syntax: if digit_count > 0 {
            syntax
        } else {
            RealSyntax::NotNumeric
        },
        complete,
        digits: digit_count,
        exponent_valid,
    }
}

/// Computes `sign * significand * 10^(exponent + shift)`.
///
/// SQLite first cancels the exponent against the significand where that is
/// exact - multiplying the significand up while it still fits, and dividing it
/// down while it ends in a zero - and only then performs one scaling. Doing
/// the same here keeps the easy cases exact and leaves one correctly rounded
/// conversion for the rest.
fn compute_real(sign: i32, significand: i64, exponent: i32, shift: i32) -> f64 {
    if significand == 0 {
        return if sign < 0 { -0.0 } else { 0.0 };
    }
    let mut value = significand;
    let mut scale = exponent.saturating_add(shift);
    while scale > 0 && value < LARGEST_INT64 / 10 {
        value = value.saturating_mul(10);
        scale = scale.saturating_sub(1);
    }
    while scale < 0 && value % 10 == 0 {
        value /= 10;
        scale = scale.saturating_add(1);
    }
    let magnitude = if scale == 0 {
        value as f64
    } else {
        // The one place a Rust conversion is used, and only on a string this
        // code built: a decimal significand and a decimal exponent, which is
        // the same quantity SQLite hands to its long-double or Dekker scaling.
        let rendered = format!("{value}e{scale}");
        rendered.parse::<f64>().unwrap_or(f64::INFINITY)
    };
    if sign < 0 {
        -magnitude
    } else {
        magnitude
    }
}

/// Truncates a double toward zero into an `i64`, saturating at the bounds.
///
/// This is `sqlite3RealToI64`. Rust's `as` conversion already saturates, but
/// the bound comparisons are written out because the saturation is the
/// documented behaviour rather than an implementation detail to inherit.
pub fn real_to_i64(value: f64) -> i64 {
    // The bounds are SQLite's literal ones rather than the `i64` extremes
    // written as doubles. 9223372036854774784 is the largest double below
    // 2^63, and comparing against it is what keeps the conversion inside the
    // range without the comparison itself rounding.
    const LARGEST_EXACT: f64 = 9_223_372_036_854_774_784.0;
    if value < -LARGEST_EXACT {
        return SMALLEST_INT64;
    }
    if value > LARGEST_EXACT {
        return LARGEST_INT64;
    }
    if value.is_nan() {
        return 0;
    }
    value as i64
}

/// Reports whether a double and an integer are the same value.
///
/// This is `sqlite3RealSameAsInt`. The 2^51 bound is what stops a large double,
/// whose integer neighbours are more than one apart, from being called equal to
/// the integer it happens to convert to.
pub fn real_same_as_int(real: f64, integer: i64) -> bool {
    if real == 0.0 {
        return true;
    }
    let round_trip = integer as f64;
    real.to_bits() == round_trip.to_bits()
        && (-SAME_AS_INT_BOUND..SAME_AS_INT_BOUND).contains(&integer)
}

/// Renders an integer as text.
pub fn integer_to_text(value: i64) -> Vec<u8> {
    value.to_string().into_bytes()
}

/// How many significant digits SQLite renders a double with.
///
/// `vdbeMemRenderNum` formats with `"%!.*g"` and `db->nFpDigit`, which is 17
/// unless an application changes it. Seventeen is what makes `CAST(1.0/3 AS
/// TEXT)` eighteen characters long rather than fifteen, and getting it wrong
/// changes the text of every REAL a query returns.
pub const FP_DIGITS: usize = 17;

/// Renders a double as text the way SQLite's `%!.17g` does.
///
/// This is `vdbeMemRenderNum`, and it goes through the same conversion
/// `printf()` uses, [`crate::fpdecode::render`], because SQLite's does. Four
/// parts of it are observable and none of them is what a general purpose float
/// formatter does:
///
/// 1. the digits come from `sqlite3FpDecode`, which scales the double by an
///    approximation of a power of ten rather than computing the exact decimal
///    expansion. Its eighteen digits are then rounded to seventeen. That is why
///    `1.0/3` renders as `0.33333333333333332` where a single correctly rounded
///    conversion gives `...331`, and why `1.1304293785495057e251` renders as
///    `...057e+251` where the exact expansion rounds to `...058` (task-2080);
/// 2. at exactly seventeen digits - which only `%!` asks for - SQLite tries to
///    *shorten* the result, and keeps the shorter form when it still converts
///    back to the same double. That is what makes `49.47` render as `49.47`
///    rather than as `49.469999999999999`;
/// 3. the choice between fixed and exponent notation is `exp < -4 || exp > 16`,
///    so `1e16` is `10000000000000000.0` and `1e17` is `1.0e+17`;
/// 4. the `!` flag forces a decimal point, so a whole number still reads as a
///    real, and trailing zeros are stripped after the point.
///
/// A negative zero renders as `0.0`, because SQLite decides the sign with
/// `r < 0.0`. Infinities render as `Inf` and `-Inf`. A NaN renders as `NaN`,
/// which is what `printf` produces; it cannot arrive from a stored value,
/// because SQLite stores a NaN double as NULL.
pub fn real_to_text(value: f64) -> Vec<u8> {
    let format = crate::fpdecode::Format {
        conversion: crate::fpdecode::Conversion::General,
        precision: Some(FP_DIGITS),
        prefix: None,
        alternate: false,
        alternate2: true,
        zero_pad: false,
        thousands: false,
        upper: false,
    };
    crate::fpdecode::render(value, &format)
}

/// Reports whether text holds a number, ignoring surrounding whitespace.
///
/// This is the test SQLite's comparison path uses to decide whether a text
/// operand may take numeric affinity.
pub fn looks_numeric(bytes: &[u8], encoding: TextEncoding) -> bool {
    atof(bytes, encoding).is_number()
}

/// Returns the number of code points in text, for `length()` and for limits.
pub fn character_count(bytes: &[u8], encoding: TextEncoding) -> usize {
    let utf8 = encoding::to_utf8(bytes, encoding);
    encoding::utf8_character_count(utf8.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clean cases: a whole string of digits that fits.
    #[test]
    fn a_plain_integer_is_read_exactly() {
        for (text, expected) in [
            ("0", 0i64),
            ("1", 1),
            ("-1", -1),
            ("+42", 42),
            ("  7  ", 7),
            ("0000000009", 9),
            ("9223372036854775807", i64::MAX),
            ("-9223372036854775808", i64::MIN),
        ] {
            let (value, syntax) = atoi64(text.as_bytes(), TextEncoding::Utf8);
            assert!(syntax.is_exact(), "{text} -> {syntax:?}");
            assert_eq!(value, expected, "{text}");
        }
    }

    /// The bound cases SQLite keeps apart: 2^63 fits only as a negative
    /// number, and anything past it overflows in both directions.
    #[test]
    fn the_two_pow_63_boundary_is_handled_on_both_signs() {
        let (value, syntax) = atoi64(b"9223372036854775808", TextEncoding::Utf8);
        assert_eq!(syntax, IntegerSyntax::TwoPow63);
        assert_eq!(value, i64::MAX);
        let (value, syntax) = atoi64(b"-9223372036854775808", TextEncoding::Utf8);
        assert!(syntax.is_exact());
        assert_eq!(value, i64::MIN);
        let (value, syntax) = atoi64(b"9223372036854775809", TextEncoding::Utf8);
        assert_eq!(syntax, IntegerSyntax::Overflow);
        assert_eq!(value, i64::MAX);
        let (value, syntax) = atoi64(b"-99999999999999999999", TextEncoding::Utf8);
        assert_eq!(syntax, IntegerSyntax::Overflow);
        assert_eq!(value, i64::MIN);
    }

    /// Anything that is not digits and surrounding whitespace is not a clean
    /// integer, which is what keeps `'12abc'` text rather than the number 12.
    #[test]
    fn trailing_bytes_and_missing_digits_are_reported() {
        for text in ["12abc", "1 2", "", "  ", "-", "+", "0x10", "1.0", "1e3"] {
            let (_, syntax) = atoi64(text.as_bytes(), TextEncoding::Utf8);
            assert!(!syntax.is_exact(), "{text} was accepted");
        }
    }

    /// Leading zeros are skipped before the nineteen-digit test, so a long
    /// string of them does not look like an overflow.
    #[test]
    fn leading_zeros_do_not_count_toward_the_digit_limit() {
        let padded = format!("{}{}", "0".repeat(40), 1234567890123456789i64);
        let (value, syntax) = atoi64(padded.as_bytes(), TextEncoding::Utf8);
        assert!(syntax.is_exact());
        assert_eq!(value, 1234567890123456789);
    }

    /// The real scanner accepts the forms SQLite accepts and rejects the rest.
    #[test]
    fn the_real_scanner_matches_sqlites_grammar() {
        for (text, expected) in [
            ("1", 1.0f64),
            ("1.", 1.0),
            (".5", 0.5),
            ("-.5", -0.5),
            ("1e3", 1000.0),
            ("1E3", 1000.0),
            ("1e+3", 1000.0),
            ("1e-3", 0.001),
            ("  2.5  ", 2.5),
            ("0.1", 0.1),
        ] {
            let parsed = atof(text.as_bytes(), TextEncoding::Utf8);
            assert!(parsed.is_number(), "{text} was refused");
            assert_eq!(parsed.value, expected, "{text}");
        }
        for text in ["", " ", "abc", "1e", "1e+", ".", "-", "1.5x", "e5", "0x10"] {
            assert!(
                !atof(text.as_bytes(), TextEncoding::Utf8).is_number(),
                "{text} was accepted"
            );
        }
    }

    /// A negative zero renders without its sign, which is what SQLite prints
    /// and is not what a faithful rendering of the bits would produce.
    #[test]
    fn a_negative_zero_renders_without_its_sign() {
        assert_eq!(real_to_text(-0.0), b"0.0");
        assert_eq!(real_to_text(0.0), b"0.0");
        // The value itself still keeps its sign; only the text drops it.
        assert!((-0.0f64).is_sign_negative());
    }

    /// A signed zero survives the scanner, because `-0.0` and `0.0` are
    /// different bit patterns and a record has to store the one it was given.
    #[test]
    fn a_signed_zero_keeps_its_sign() {
        let negative = atof(b"-0.0", TextEncoding::Utf8);
        assert!(negative.value.is_sign_negative());
        assert_eq!(negative.value, 0.0);
        let positive = atof(b"0.0", TextEncoding::Utf8);
        assert!(positive.value.is_sign_positive());
    }

    /// An exponent past the double range saturates to infinity rather than
    /// wrapping or erroring, which is what `SELECT 1e400` returns.
    #[test]
    fn an_enormous_exponent_becomes_infinity() {
        let parsed = atof(b"1e400", TextEncoding::Utf8);
        assert!(parsed.is_number());
        assert!(parsed.value.is_infinite() && parsed.value > 0.0);
        let parsed = atof(b"-1e400", TextEncoding::Utf8);
        assert!(parsed.value.is_infinite() && parsed.value < 0.0);
        let parsed = atof(b"1e-400", TextEncoding::Utf8);
        assert_eq!(parsed.value, 0.0);
    }

    /// The significand is truncated at nineteen digits, not rounded over the
    /// whole string. This is the case a Rust `parse` would get differently.
    #[test]
    fn the_significand_is_truncated_the_way_sqlite_truncates_it() {
        let long = "1.7976931348623157081e308";
        let parsed = atof(long.as_bytes(), TextEncoding::Utf8);
        assert!(parsed.is_number());
        assert!(parsed.value.is_finite(), "{}", parsed.value);
        // Twenty significant digits: the twentieth is dropped rather than
        // rounding the nineteenth up, so the result stays finite.
        assert_eq!(parsed.value, 1.7976931348623157e308);
    }

    /// UTF-16 text scans the same way, and a code unit with a non-zero high
    /// byte makes the text non-numeric rather than being read as its low byte.
    #[test]
    fn utf16_text_scans_as_ascii_and_refuses_wide_characters() {
        let wide = encoding::from_utf8(b"-12.5", TextEncoding::Utf16Le).into_owned();
        let parsed = atof(&wide, TextEncoding::Utf16Le);
        assert!(parsed.is_number());
        assert_eq!(parsed.value, -12.5);
        let (value, syntax) = atoi64(
            &encoding::from_utf8(b"-125", TextEncoding::Utf16Be),
            TextEncoding::Utf16Be,
        );
        assert!(syntax.is_exact());
        assert_eq!(value, -125);
        let snowman =
            encoding::from_utf8("1\u{2603}".as_bytes(), TextEncoding::Utf16Le).into_owned();
        assert!(!atof(&snowman, TextEncoding::Utf16Le).is_number());
    }

    /// The scanner's return code separates integer syntax from everything
    /// else, which is the bit `CAST(... AS NUMERIC)` branches on.
    #[test]
    fn the_scanner_reports_sqlites_own_return_code() {
        let code = |text: &str| atof(text.as_bytes(), TextEncoding::Utf8).code();
        assert_eq!(code("123"), 1);
        assert_eq!(code("  123  "), 1);
        assert_eq!(code("1.5"), 2);
        assert_eq!(code("1e18"), 3);
        assert_eq!(code("1E18"), 3);
        assert_eq!(code("abc"), 0);
        assert_eq!(code(""), 0);
        // Integer syntax with trailing bytes is not a number at all.
        assert_eq!(code("12abc"), 0);
        // A fraction or an exponent with trailing bytes is the -1 case.
        assert_eq!(code("1.5x"), -1);
        assert_eq!(code("1e3x"), -1);
        // The bit the numeric cast branches on.
        for (text, expected) in [("123", 0), ("1.5", 2), ("1e18", 2), ("abc", 0), ("1.5x", 2)] {
            assert_eq!(code(text) & 2, expected, "{text}");
        }
    }

    /// The 2^51 bound is the whole point of `real_same_as_int`: past it, a
    /// double and the integer it converts to are not the same value.
    #[test]
    fn a_real_equals_an_integer_only_inside_the_documented_bound() {
        assert!(real_same_as_int(1.0, 1));
        assert!(real_same_as_int(-0.0, 0));
        assert!(real_same_as_int(0.0, 0));
        assert!(!real_same_as_int(1.5, 1));
        let past = SAME_AS_INT_BOUND;
        assert!(!real_same_as_int(past as f64, past));
        assert!(real_same_as_int((past - 1) as f64, past - 1));
    }

    /// Truncation is toward zero and saturates at the bounds.
    #[test]
    fn converting_a_real_to_an_integer_truncates_and_saturates() {
        assert_eq!(real_to_i64(1.9), 1);
        assert_eq!(real_to_i64(-1.9), -1);
        assert_eq!(real_to_i64(f64::INFINITY), i64::MAX);
        assert_eq!(real_to_i64(f64::NEG_INFINITY), i64::MIN);
        assert_eq!(real_to_i64(1e300), i64::MAX);
        assert_eq!(real_to_i64(f64::NAN), 0);
        // The bound is the largest double below 2^63, not the `i64` extreme
        // rounded to a double, which is 2^63 exactly and out of range.
        assert_eq!(
            real_to_i64(9_223_372_036_854_774_784.0),
            9_223_372_036_854_774_784
        );
        assert_eq!(real_to_i64(9_223_372_036_854_775_808.0), i64::MAX);
        assert_eq!(real_to_i64(-9_223_372_036_854_775_808.0), i64::MIN);
    }

    /// The text form of a real is the one SQLite prints, including the forced
    /// decimal point and the two-digit exponent.
    #[test]
    fn reals_render_the_way_sqlite_prints_them() {
        // Every one of these was read back from SQLite 3.53.4 itself; they
        // are the cases that separate `%!.17g` from an ordinary formatter.
        for (value, expected) in [
            (1.0f64, "1.0"),
            (-1.0, "-1.0"),
            (0.0, "0.0"),
            (-0.0, "0.0"),
            (0.1, "0.1"),
            (0.5, "0.5"),
            (1.0 / 3.0, "0.33333333333333332"),
            (2.0 / 3.0, "0.66666666666666663"),
            (49.47, "49.47"),
            (100.0, "100.0"),
            (1e14, "100000000000000.0"),
            (1e15, "1000000000000000.0"),
            (1e16, "10000000000000000.0"),
            (1e17, "1.0e+17"),
            (1e20, "1.0e+20"),
            (123456789012345.0, "123456789012345.0"),
            (1234567890123456.0, "1234567890123456.0"),
            (12345678901234567.0, "12345678901234568.0"),
            (9007199254740992.0, "9007199254740992.0"),
            (1e300, "1.0e+300"),
            (1e308, "1.0e+308"),
            (1e-4, "0.0001"),
            (1e-5, "1.0e-05"),
            (0.00012345, "0.00012345"),
            (4.9e-324, "4.9406564584124654e-324"),
            (2.2250738585072014e-308, "2.2250738585072014e-308"),
            (0.0001, "0.0001"),
            (-2.5, "-2.5"),
            (f64::INFINITY, "Inf"),
            (f64::NEG_INFINITY, "-Inf"),
        ] {
            assert_eq!(
                String::from_utf8_lossy(&real_to_text(value)),
                expected,
                "{value}"
            );
        }
    }

    /// Rendering a real and reading it back must land on the same double for
    /// every value that fits fifteen significant digits, which is the property
    /// the text form of a REAL column depends on.
    #[test]
    fn rendering_and_rereading_a_real_is_stable() {
        let samples = [
            1.0f64,
            -1.0,
            0.1,
            0.5,
            2.5,
            49.47,
            1e14,
            1e15,
            1e-5,
            1e300,
            1e-300,
            123456.789,
            -98765.4321,
            1.0 / 3.0,
            f64::MAX,
            f64::MIN_POSITIVE,
        ];
        for value in samples {
            let text = real_to_text(value);
            let parsed = atof(&text, TextEncoding::Utf8);
            assert!(parsed.is_number(), "{value} rendered unreadably");
            let again = real_to_text(parsed.value);
            assert_eq!(text, again, "{value} did not render stably");
        }
    }

    /// Seventeen digits are enough to round-trip every double, so rendering a
    /// value and reading it back must land on the identical bit pattern. This
    /// is what stops a REAL that went through a TEXT column coming back as a
    /// different number.
    #[test]
    fn rendering_a_real_round_trips_to_the_same_bits() {
        let mut state = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..inillucent_base::probe::sample_rounds(200_000) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let candidate = f64::from_bits(state);
            if !candidate.is_finite() {
                continue;
            }
            let text = real_to_text(candidate);
            let parsed = atof(&text, TextEncoding::Utf8);
            assert!(
                parsed.is_number(),
                "{candidate} rendered as {} which does not scan",
                String::from_utf8_lossy(&text)
            );
            assert_eq!(
                parsed.value.to_bits(),
                candidate.to_bits(),
                "{candidate} rendered as {} and read back as {}",
                String::from_utf8_lossy(&text),
                parsed.value
            );
        }
    }

    /// Scanning arbitrary bytes must terminate and never panic, whatever the
    /// bytes are. Text arrives from a page and is attacker-controlled.
    #[test]
    fn scanning_arbitrary_bytes_never_panics() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..inillucent_base::probe::sample_rounds(50_000) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state % 24) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|index| {
                    let byte = (state >> (index % 8 * 8)) as u8;
                    // Bias toward the characters a number is made of so the
                    // scanner's interesting paths are actually reached.
                    match byte % 5 {
                        0 => b'0'.wrapping_add(byte % 10),
                        1 => b'.',
                        2 => b'e',
                        3 => b'-',
                        _ => byte,
                    }
                })
                .collect();
            for encoding in TextEncoding::all() {
                let _ = atof(&bytes, encoding);
                let _ = atoi64(&bytes, encoding);
                let _ = character_count(&bytes, encoding);
            }
        }
    }

    /// Every double that round-trips must render to text that scans back to
    /// the identical bit pattern when the value has few enough digits.
    #[test]
    fn integers_render_and_scan_back_unchanged() {
        for value in [
            0i64,
            1,
            -1,
            i64::MAX,
            i64::MIN,
            9_007_199_254_740_993,
            -9_007_199_254_740_993,
        ] {
            let text = integer_to_text(value);
            let (back, syntax) = atoi64(&text, TextEncoding::Utf8);
            assert!(syntax.is_exact(), "{value}");
            assert_eq!(back, value);
        }
    }
}
