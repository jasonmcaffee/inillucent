//! `printf()` and its alias `format()`.
//!
//! Invariant: the conversions consume arguments in the order they appear, and a
//! conversion with no argument left produces what SQLite produces for a missing
//! argument rather than stopping. That matters because the format string is
//! data: it can come from a column, and a statement that errors on one row's
//! format and not another's is worse than one that renders both.
//!
//! The set implemented is the one SQLite documents for its own build: `d i u`
//! for integers, `f e E g G` for reals, `s` for text, `c` for a character, `x X
//! o` for the other bases, `q Q w` for the SQL-quoting conversions, `z` as a
//! synonym for `s`, `%` for a literal, and the flags `- + space 0 #` with a
//! width, a precision, and `*` to take either from an argument.

use inillucent_value::{cast, numeric, TextEncoding, Value};

/// Formats a call to `printf`/`format`.
///
/// The first argument is the format; the rest are consumed left to right.
pub fn format(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let Some(Value::Text(text)) = arguments.first() else {
        // A NULL or non-text format is NULL, and a call with no arguments at
        // all is NULL too.
        return Value::Null;
    };
    let template = text.utf8_bytes().to_vec();
    let mut out: Vec<u8> = Vec::new();
    let mut next = 1usize;
    let mut index = 0usize;
    while index < template.len() {
        let byte = template.get(index).copied().unwrap_or(0);
        index = index.saturating_add(1);
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let Some((spec, after)) = parse_spec(&template, index) else {
            out.push(b'%');
            continue;
        };
        index = after;
        if spec.conversion == b'%' {
            out.push(b'%');
            continue;
        }
        let mut spec = spec;
        if spec.width_from_argument {
            let width = integer_of(arguments.get(next));
            next = next.saturating_add(1);
            if width < 0 {
                spec.left = true;
                spec.width = width.unsigned_abs() as usize;
            } else {
                spec.width = width as usize;
            }
        }
        if spec.precision_from_argument {
            let precision = integer_of(arguments.get(next));
            next = next.saturating_add(1);
            spec.precision = (precision >= 0).then_some(precision as usize);
        }
        let argument = arguments.get(next);
        next = next.saturating_add(1);
        let rendered = render(&spec, argument, encoding);
        pad(&mut out, &rendered, &spec);
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// One conversion specification.
#[derive(Clone, Copy, Debug)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    zero: bool,
    alternate: bool,
    width: usize,
    width_from_argument: bool,
    precision: Option<usize>,
    precision_from_argument: bool,
    conversion: u8,
}

/// Parses a conversion specification, returning it and where it ended.
fn parse_spec(template: &[u8], start: usize) -> Option<(Spec, usize)> {
    let mut spec = Spec {
        left: false,
        plus: false,
        space: false,
        zero: false,
        alternate: false,
        width: 0,
        width_from_argument: false,
        precision: None,
        precision_from_argument: false,
        conversion: 0,
    };
    let mut index = start;
    loop {
        match template.get(index).copied() {
            Some(b'-') => spec.left = true,
            Some(b'+') => spec.plus = true,
            Some(b' ') => spec.space = true,
            Some(b'0') => spec.zero = true,
            Some(b'#') => spec.alternate = true,
            Some(b',') | Some(b'!') => {}
            _ => break,
        }
        index = index.saturating_add(1);
    }
    if template.get(index) == Some(&b'*') {
        spec.width_from_argument = true;
        index = index.saturating_add(1);
    } else {
        while template.get(index).is_some_and(u8::is_ascii_digit) {
            let digit = template.get(index).copied().unwrap_or(b'0');
            spec.width = spec
                .width
                .saturating_mul(10)
                .saturating_add(usize::from(digit.saturating_sub(b'0')));
            index = index.saturating_add(1);
        }
    }
    if template.get(index) == Some(&b'.') {
        index = index.saturating_add(1);
        if template.get(index) == Some(&b'*') {
            spec.precision_from_argument = true;
            index = index.saturating_add(1);
        } else {
            let mut precision = 0usize;
            while template.get(index).is_some_and(u8::is_ascii_digit) {
                let digit = template.get(index).copied().unwrap_or(b'0');
                precision = precision
                    .saturating_mul(10)
                    .saturating_add(usize::from(digit.saturating_sub(b'0')));
                index = index.saturating_add(1);
            }
            spec.precision = Some(precision);
        }
    }
    // Length modifiers are accepted and ignored: every integer here is 64-bit.
    while matches!(template.get(index), Some(b'l') | Some(b'h')) {
        index = index.saturating_add(1);
    }
    let conversion = template.get(index).copied()?;
    spec.conversion = conversion;
    Some((spec, index.saturating_add(1)))
}

/// Returns an argument as an integer, or zero when there is none.
fn integer_of(value: Option<&Value<'static>>) -> i64 {
    value.map_or(0, cast::integer_value)
}

/// Renders one conversion, before padding.
fn render(spec: &Spec, argument: Option<&Value<'static>>, encoding: TextEncoding) -> Vec<u8> {
    match spec.conversion {
        b'd' | b'i' | b'u' => integer(spec, integer_of(argument)),
        b'x' => based(spec, integer_of(argument), 16, false),
        b'X' => based(spec, integer_of(argument), 16, true),
        b'o' => based(spec, integer_of(argument), 8, false),
        b'f' | b'e' | b'E' | b'g' | b'G' => real(spec, argument),
        b'c' => {
            // `%c` is the *first character of the argument as text*, not a
            // character code: the pinned build renders `printf('%c', 65)` as
            // "6", because 65 becomes the text "65" and the first character of
            // that is a six.
            let text = text_of(argument, encoding);
            let mut characters = String::from_utf8_lossy(&text).into_owned();
            characters.truncate(
                characters
                    .char_indices()
                    .nth(1)
                    .map_or(characters.len(), |(at, _)| at),
            );
            characters.into_bytes()
        }
        b's' | b'z' => {
            let mut text = text_of(argument, encoding);
            if let Some(precision) = spec.precision {
                text.truncate(precision);
            }
            text
        }
        b'q' => quoted(argument, encoding, false),
        b'Q' => quoted(argument, encoding, true),
        b'w' => {
            // `%w` quotes an identifier: a double quote is doubled and the
            // whole is *not* wrapped, which is what makes it usable inside a
            // `CREATE` statement being assembled. A NULL is `(NULL)`, as it is
            // for `%q`.
            if matches!(argument, None | Some(Value::Null)) {
                return b"(NULL)".to_vec();
            }
            let text = text_of(argument, encoding);
            let mut out = Vec::new();
            for byte in text {
                if byte == b'"' {
                    out.push(b'"');
                }
                out.push(byte);
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Renders an integer conversion.
fn integer(spec: &Spec, value: i64) -> Vec<u8> {
    let negative = value < 0;
    let magnitude = value.unsigned_abs().to_string();
    let mut digits = magnitude.into_bytes();
    if let Some(precision) = spec.precision {
        while digits.len() < precision {
            digits.insert(0, b'0');
        }
    }
    let mut out = Vec::new();
    if negative {
        out.push(b'-');
    } else if spec.plus {
        out.push(b'+');
    } else if spec.space {
        out.push(b' ');
    }
    out.extend_from_slice(&digits);
    out
}

/// Renders a hexadecimal or octal conversion.
fn based(spec: &Spec, value: i64, base: u32, upper: bool) -> Vec<u8> {
    let unsigned = value as u64;
    let mut text = match base {
        16 if upper => format!("{unsigned:X}"),
        16 => format!("{unsigned:x}"),
        8 => format!("{unsigned:o}"),
        _ => unsigned.to_string(),
    };
    if let Some(precision) = spec.precision {
        while text.len() < precision {
            text.insert(0, '0');
        }
    }
    if spec.alternate && unsigned != 0 {
        match base {
            16 if upper => text.insert_str(0, "0X"),
            16 => text.insert_str(0, "0x"),
            8 => text.insert(0, '0'),
            _ => {}
        }
    }
    text.into_bytes()
}

/// Returns what `%g` would render for a number.
///
/// The one conversion another module needs on its own: `geopoly` writes its
/// coordinates with it, and re-deriving C's shorter-of-the-two rule beside it
/// would be a second implementation that could drift from this one.
///
/// @param value - the number
pub fn general(value: f64) -> String {
    let spec = Spec {
        left: false,
        plus: false,
        space: false,
        zero: false,
        alternate: false,
        width: 0,
        width_from_argument: false,
        precision: None,
        precision_from_argument: false,
        conversion: b'g',
    };
    String::from_utf8_lossy(&real(&spec, Some(&Value::Real(value)))).into_owned()
}

/// Renders a floating-point conversion.
fn real(spec: &Spec, argument: Option<&Value<'static>>) -> Vec<u8> {
    let value = argument.map_or(0.0, cast::real_value);
    let precision = spec.precision.unwrap_or(6);
    let body = match spec.conversion {
        b'e' => format!("{value:.precision$e}"),
        b'E' => format!("{value:.precision$e}").to_uppercase(),
        b'g' | b'G' => {
            // `%g` drops trailing zeros and chooses the shorter of fixed and
            // exponential. Rust has no `{:g}`, so the choice is made here on
            // the same rule C uses: the exponent decides.
            let exponent = if value == 0.0 {
                0
            } else {
                value.abs().log10().floor() as i32
            };
            let significant = if precision == 0 { 1 } else { precision };
            if exponent < -4 || exponent >= significant as i32 {
                let text = format!("{:.*e}", significant.saturating_sub(1), value);
                let text = trim_zeros(&text, true);
                if spec.conversion == b'G' {
                    text.to_uppercase()
                } else {
                    text
                }
            } else {
                let decimals = significant
                    .saturating_sub(1)
                    .saturating_sub(exponent.max(0) as usize);
                trim_zeros(&format!("{value:.decimals$}"), false)
            }
        }
        _ => fixed(value, precision),
    };
    // Rust writes `1e2` where C writes `1.000000e+02`, so the exponent is
    // normalised rather than the whole number being re-rendered.
    let body = normalise_exponent(&body);
    let mut out = Vec::new();
    if !body.starts_with('-') {
        if spec.plus {
            out.push(b'+');
        } else if spec.space {
            out.push(b' ');
        }
    }
    out.extend_from_slice(body.as_bytes());
    out
}

/// Renders a fixed-point number, rounding a half away from zero.
///
/// Rust rounds a half to even, so `{:.0}` of 2.5 is "2" where SQLite's printf
/// answers "3". The rounding is done on the *decimal expansion* rather than by
/// scaling and testing the fraction, because scaling cannot see the difference:
/// `0.35 * 10.0` is exactly 3.5 in binary - the product of a double slightly
/// below 0.35 rounds up to the midpoint - so a scaled test rounds 0.35 away
/// from zero and answers "0.4" where both C and SQLite answer "0.3".
///
/// Thirty digits past the cut is enough to decide. If they are all zeros the
/// value is exactly on the midpoint and rounds away; if they are all nines the
/// first dropped digit is a nine and rounds away too. Every other case is
/// decided by the first dropped digit alone.
fn fixed(value: f64, precision: usize) -> String {
    if !value.is_finite() {
        return format!("{value:.precision$}");
    }
    let negative = value < 0.0;
    let magnitude = value.abs();
    let extended = format!("{:.*}", precision.saturating_add(30), magnitude);
    let (whole, fraction) = match extended.split_once('.') {
        Some((whole, fraction)) => (whole.to_string(), fraction.to_string()),
        None => (extended.clone(), String::new()),
    };
    let kept = fraction.get(..precision).unwrap_or(&fraction).to_string();
    let first_dropped = fraction.as_bytes().get(precision).copied().unwrap_or(b'0');
    let mut digits: Vec<u8> = whole.into_bytes();
    digits.extend_from_slice(kept.as_bytes());
    if first_dropped >= b'5' {
        carry(&mut digits);
    }
    let mut text = String::from_utf8_lossy(&digits).into_owned();
    if precision > 0 {
        while text.len() <= precision {
            text.insert(0, '0');
        }
        text.insert(text.len().saturating_sub(precision), '.');
    }
    if negative {
        text.insert(0, '-');
    }
    text
}

/// Adds one to a string of decimal digits, in place.
fn carry(digits: &mut Vec<u8>) {
    let mut index = digits.len();
    while index > 0 {
        index = index.saturating_sub(1);
        let Some(digit) = digits.get_mut(index) else {
            return;
        };
        if *digit == b'9' {
            *digit = b'0';
            continue;
        }
        *digit = digit.saturating_add(1);
        return;
    }
    digits.insert(0, b'1');
}

/// Removes trailing zeros from a `%g` rendering.
fn trim_zeros(text: &str, exponential: bool) -> String {
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (text, None),
    };
    let mantissa = if mantissa.contains('.') {
        mantissa.trim_end_matches('0').trim_end_matches('.')
    } else {
        mantissa
    };
    match (exponent, exponential) {
        (Some(exponent), _) => format!("{mantissa}e{exponent}"),
        (None, _) => mantissa.to_string(),
    }
}

/// Rewrites Rust's exponent form into C's.
fn normalise_exponent(text: &str) -> String {
    let Some((mantissa, exponent)) = text.split_once(['e', 'E']) else {
        return text.to_string();
    };
    let upper = text.contains('E');
    let (sign, digits) = match exponent.strip_prefix('-') {
        Some(rest) => ('-', rest),
        None => ('+', exponent.strip_prefix('+').unwrap_or(exponent)),
    };
    let marker = if upper { 'E' } else { 'e' };
    format!("{mantissa}{marker}{sign}{digits:0>2}")
}

/// Returns an argument rendered as text.
pub fn rendered_text(argument: Option<&Value<'static>>, encoding: TextEncoding) -> Vec<u8> {
    text_of(argument, encoding)
}

/// Renders one value the way `%s` would.
fn text_of(argument: Option<&Value<'static>>, encoding: TextEncoding) -> Vec<u8> {
    let _ = encoding;
    match argument {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Integer(integer)) => numeric::integer_to_text(*integer),
        Some(Value::Real(real)) => numeric::real_to_text(*real),
        Some(Value::Text(text)) => text.utf8_bytes().to_vec(),
        Some(Value::Blob(blob)) => blob.raw().to_vec(),
    }
}

/// Renders `%q` or `%Q`: a string with its single quotes doubled.
///
/// `%Q` also wraps it in quotes and writes a bare `NULL` for a NULL, which is
/// what makes it safe to paste into generated SQL where `%q` is not.
fn quoted(argument: Option<&Value<'static>>, encoding: TextEncoding, wrap: bool) -> Vec<u8> {
    // A NULL is `NULL` unquoted for `%Q` and the literal text `(NULL)` for
    // `%q`, which is the marker SQLite writes wherever a string was expected
    // and none was given.
    if matches!(argument, None | Some(Value::Null)) {
        return if wrap {
            b"NULL".to_vec()
        } else {
            b"(NULL)".to_vec()
        };
    }
    let text = text_of(argument, encoding);
    let mut out = Vec::new();
    if wrap {
        out.push(b'\'');
    }
    for byte in text {
        if byte == b'\'' {
            out.push(b'\'');
        }
        out.push(byte);
    }
    if wrap {
        out.push(b'\'');
    }
    out
}

/// Pads a rendered conversion to the width the specification asks for.
fn pad(out: &mut Vec<u8>, body: &[u8], spec: &Spec) {
    if body.len() >= spec.width {
        out.extend_from_slice(body);
        return;
    }
    let fill = spec.width.saturating_sub(body.len());
    if spec.left {
        out.extend_from_slice(body);
        out.extend(core::iter::repeat_n(b' ', fill));
        return;
    }
    // Zero padding goes *after* a sign, not before it, so `%05d` of -42 is
    // `-0042` rather than `000-42`.
    //
    // **A precision does not switch the zero off.** C says the `0` flag is
    // ignored for `d`, `i`, `o`, `u`, `x` and `X` when a precision is given,
    // and SQLite's own printf does not implement that rule: the pinned 3.53.4
    // renders `%08.3d` of 42 as `00000042` and `%08.3x` of 255 as `000000ff`.
    // Applying the C rule here made `printf('%05.2f', 3.14159)` answer `3.14`
    // against the reference's `03.14` - and the guard was wrong for the integer
    // conversions it was written for as well.
    let numeric_conversion = matches!(
        spec.conversion,
        b'd' | b'i' | b'u' | b'x' | b'X' | b'o' | b'f' | b'e' | b'E' | b'g' | b'G'
    );
    if spec.zero && numeric_conversion {
        let signed = matches!(body.first(), Some(b'-') | Some(b'+') | Some(b' '));
        if signed {
            out.extend_from_slice(body.get(..1).unwrap_or(&[]));
            out.extend(core::iter::repeat_n(b'0', fill));
            out.extend_from_slice(body.get(1..).unwrap_or(&[]));
        } else {
            out.extend(core::iter::repeat_n(b'0', fill));
            out.extend_from_slice(body);
        }
        return;
    }
    out.extend(core::iter::repeat_n(b' ', fill));
    out.extend_from_slice(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Formats with owned arguments, returning the text.
    fn run(template: &str, arguments: Vec<Value<'static>>) -> String {
        let mut all = vec![Value::owned_text(template.as_bytes()).expect("the text builds")];
        all.extend(arguments);
        match format(&all, TextEncoding::Utf8) {
            Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn zero_padding_follows_the_sign() {
        assert_eq!(run("%05d", vec![Value::Integer(-42)]), "-0042");
        assert_eq!(run("%05d", vec![Value::Integer(42)]), "00042");
        assert_eq!(run("%-5d|", vec![Value::Integer(42)]), "42   |");
    }

    #[test]
    fn a_half_rounds_away_from_zero() {
        assert_eq!(run("%.0f", vec![Value::Real(2.5)]), "3");
        assert_eq!(run("%.0f", vec![Value::Real(-2.5)]), "-3");
        assert_eq!(run("%.1f", vec![Value::Real(0.25)]), "0.3");
        // Not a half at all: the nearest double to 0.35 is below the midpoint.
        assert_eq!(run("%.1f", vec![Value::Real(0.35)]), "0.3");
    }

    #[test]
    fn a_missing_argument_renders_rather_than_failing() {
        assert_eq!(run("%d-%d", vec![Value::Integer(1)]), "1-0");
        assert_eq!(run("%s!", Vec::new()), "!");
    }

    #[test]
    fn the_quoting_conversions_differ_on_null() {
        assert_eq!(run("%q", vec![Value::Null]), "(NULL)");
        assert_eq!(run("%Q", vec![Value::Null]), "NULL");
        assert_eq!(
            run("%q", vec![Value::owned_text(b"it's").expect("text")]),
            "it''s"
        );
    }
}
