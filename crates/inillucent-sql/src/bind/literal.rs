//! Turning a literal's source text into the value it denotes.
//!
//! Invariant: **the scanner is SQLite's, and the text is read where it lies.** A literal's
//! bytes are in the parse arena and stay there: nothing here copies them to decide what
//! number they are, because a compile that allocates per literal is a compile whose cost
//! is its allocator (task-2006). The conversions themselves are
//! `inillucent_value::numeric`'s, which reproduce `sqlite3Atoi64` and `sqlite3AtoF` digit
//! for digit.

use std::borrow::Cow;

use crate::bind::BoundExpr;

/// Converts an integer literal's text into a bound value.
///
/// A decimal literal too large for `i64` becomes a real, which is what SQLite
/// does rather than failing, and a hexadecimal literal wraps into `i64`, which
/// is also what SQLite does.
///
/// @param text - the literal as it was written, with a leading `-` if it was negated
pub(crate) fn integer_literal(text: &[u8]) -> BoundExpr {
    // The sign is read off first, so `0x` is still recognised under one: the
    // binder folds a unary minus into the literal (see `Expr::Unary`), and
    // `-0x10` arrives here as `-0x10` rather than as an operator over `0x10`.
    let (negative, digits) = match text.first() {
        Some(b'-') => (true, text.get(1..).unwrap_or(&[])),
        _ => (false, text),
    };
    if digits.len() > 2
        && digits.first() == Some(&b'0')
        && digits
            .get(1)
            .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'x'))
    {
        let mut value: u64 = 0;
        for byte in digits.get(2..).unwrap_or(&[]) {
            let digit = (*byte as char).to_digit(16).unwrap_or(0) as u64;
            value = value.wrapping_mul(16).wrapping_add(digit);
        }
        let value = value as i64;
        return BoundExpr::Integer(if negative {
            value.wrapping_neg()
        } else {
            value
        });
    }
    // **The separator is stripped only when there is one** (task-2006). This collected
    // every literal into a new `Vec<u8>` to remove the `_`s, and almost no literal has
    // one - so `SELECT 1` allocated eight bytes to copy the byte `1` unchanged. A
    // backtrace on each allocation the compile makes found this one here. Scanning for
    // the separator reads the same bytes the filter would have read, and allocates
    // nothing when it finds none.
    let cleaned: Cow<'_, [u8]> = match text.contains(&b'_') {
        true => Cow::Owned(text.iter().copied().filter(|byte| *byte != b'_').collect()),
        false => Cow::Borrowed(text),
    };
    let (value, syntax) =
        inillucent_value::numeric::atoi64(&cleaned, inillucent_value::TextEncoding::Utf8);
    if syntax.is_exact() {
        return BoundExpr::Integer(value);
    }
    let parsed = inillucent_value::numeric::atof(&cleaned, inillucent_value::TextEncoding::Utf8);
    BoundExpr::Real(parsed.value)
}
