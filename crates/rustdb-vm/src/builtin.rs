//! The built-in scalar functions.
//!
//! Invariant: a function returns what the pinned release returns, including
//! where that is surprising. `length()` of a blob is its byte count and of text
//! is its *character* count; `substr()` counts from one and accepts a negative
//! start; `round()` rounds half away from zero rather than to even; `max()` of
//! any NULL argument is NULL. Each of those differs from the obvious
//! implementation, and each has a case in the tests below.

use rustdb_sql::function::ScalarFunc;
use rustdb_value::{cast, compare, numeric, Affinity, Collation, TextEncoding, Value};

use crate::eval;

/// Calls a scalar function.
pub fn call(
    func: ScalarFunc,
    arguments: &[Value<'static>],
    collation: Collation,
    encoding: TextEncoding,
) -> Value<'static> {
    match func {
        ScalarFunc::Abs => unary(arguments, absolute),
        ScalarFunc::Char => char_of(arguments),
        ScalarFunc::Coalesce => coalesce(arguments),
        ScalarFunc::Concat => concat(arguments, None, encoding),
        ScalarFunc::ConcatWs => concat_with_separator(arguments, encoding),
        ScalarFunc::Glob => pattern_call(arguments, false, encoding),
        // `hex`, `quote`, `typeof` and `zeroblob` are the four built-ins that
        // answer a question *about* their argument rather than computing with
        // it, so a NULL argument has an answer instead of poisoning the result.
        ScalarFunc::Hex => hex(
            &arguments.first().cloned().unwrap_or(Value::Null),
            TextEncoding::Utf8,
        ),
        ScalarFunc::IfNull => coalesce(arguments),
        ScalarFunc::Iif => iif(arguments),
        ScalarFunc::Instr => instr(arguments, encoding),
        ScalarFunc::Length => unary(arguments, length),
        ScalarFunc::Like => pattern_call(arguments, true, encoding),
        ScalarFunc::Likelihood => arguments.first().cloned().unwrap_or(Value::Null),
        ScalarFunc::Lower => unary(arguments, |value| change_case(&value, false, encoding)),
        ScalarFunc::LTrim => trim(arguments, true, false, encoding),
        ScalarFunc::Max => extreme(arguments, collation, true),
        ScalarFunc::Min => extreme(arguments, collation, false),
        ScalarFunc::NullIf => null_if(arguments, collation),
        ScalarFunc::Quote => quote(
            &arguments.first().cloned().unwrap_or(Value::Null),
            TextEncoding::Utf8,
        ),
        ScalarFunc::Replace => replace(arguments, encoding),
        ScalarFunc::Round => round(arguments),
        ScalarFunc::RTrim => trim(arguments, false, true, encoding),
        ScalarFunc::Sign => unary(arguments, sign),
        ScalarFunc::Substr => substring(arguments, encoding),
        ScalarFunc::Trim => trim(arguments, true, true, encoding),
        ScalarFunc::TypeOf => Value::owned_text(
            arguments
                .first()
                .cloned()
                .unwrap_or(Value::Null)
                .storage_class()
                .typeof_name()
                .as_bytes(),
        )
        .unwrap_or(Value::Null),
        ScalarFunc::Unhex => unhex(arguments, encoding),
        ScalarFunc::Unicode => unary(arguments, |value| unicode(&value, encoding)),
        ScalarFunc::Upper => unary(arguments, |value| change_case(&value, true, encoding)),
        ScalarFunc::ZeroBlob => zero_blob(arguments.first().cloned().unwrap_or(Value::Null)),
        ScalarFunc::Version => Value::owned_text(rustdb_base::REFERENCE_SQLITE_VERSION.as_bytes())
            .unwrap_or(Value::Null),
    }
}

/// Applies a one-argument function, propagating NULL.
fn unary(
    arguments: &[Value<'static>],
    body: impl Fn(Value<'static>) -> Value<'static>,
) -> Value<'static> {
    let Some(value) = arguments.first() else {
        return Value::Null;
    };
    if value.is_null() {
        return Value::Null;
    }
    body(value.clone())
}

/// `abs(x)`.
fn absolute(value: Value<'static>) -> Value<'static> {
    match cast::numerify(value) {
        Value::Integer(integer) => match integer.checked_abs() {
            Some(absolute) => Value::Integer(absolute),
            // `abs(-9223372036854775808)` overflows, and SQLite reports it as
            // an error rather than returning a wrapped negative.
            None => Value::Real(-(integer as f64)),
        },
        Value::Real(real) => Value::Real(real.abs()),
        _ => Value::Integer(0),
    }
}

/// `sign(x)`.
fn sign(value: Value<'static>) -> Value<'static> {
    match cast::numerify(value) {
        Value::Integer(integer) => Value::Integer(integer.signum()),
        Value::Real(real) if real > 0.0 => Value::Integer(1),
        Value::Real(real) if real < 0.0 => Value::Integer(-1),
        Value::Real(real) if real == 0.0 => Value::Integer(0),
        _ => Value::Null,
    }
}

/// `length(x)`: characters for text, bytes for a blob.
fn length(value: Value<'static>) -> Value<'static> {
    match &value {
        Value::Blob(blob) => Value::Integer(blob.len() as i64),
        Value::Text(text) => {
            Value::Integer(numeric::character_count(text.raw(), text.encoding()) as i64)
        }
        _ => {
            let rendered = eval::text_bytes(&value, TextEncoding::Utf8);
            Value::Integer(numeric::character_count(&rendered, TextEncoding::Utf8) as i64)
        }
    }
}

/// `coalesce(...)` and `ifnull(a, b)`.
fn coalesce(arguments: &[Value<'static>]) -> Value<'static> {
    for argument in arguments {
        if !argument.is_null() {
            return argument.clone();
        }
    }
    Value::Null
}

/// `iif(condition, then, otherwise)`.
fn iif(arguments: &[Value<'static>]) -> Value<'static> {
    let condition = arguments.first().cloned().unwrap_or(Value::Null);
    let index = usize::from(eval::truth(&condition) != compare::Truth::True);
    arguments
        .get(index.saturating_add(1))
        .cloned()
        .unwrap_or(Value::Null)
}

/// `nullif(a, b)`.
fn null_if(arguments: &[Value<'static>], collation: Collation) -> Value<'static> {
    let left = arguments.first().cloned().unwrap_or(Value::Null);
    let right = arguments.get(1).cloned().unwrap_or(Value::Null);
    if left.is_null() {
        return Value::Null;
    }
    if !right.is_null()
        && compare::compare_values(&left, &right, collation) == std::cmp::Ordering::Equal
    {
        return Value::Null;
    }
    left
}

/// `min(...)` and `max(...)`, the scalar forms.
///
/// Any NULL argument makes the whole answer NULL, which is the opposite of
/// what the aggregates do and is the single most common surprise here.
fn extreme(arguments: &[Value<'static>], collation: Collation, want_max: bool) -> Value<'static> {
    let mut best: Option<Value<'static>> = None;
    for argument in arguments {
        if argument.is_null() {
            return Value::Null;
        }
        best = Some(match best {
            None => argument.clone(),
            Some(current) => {
                let ordering = compare::compare_values(argument, &current, collation);
                let replace = if want_max {
                    ordering == std::cmp::Ordering::Greater
                } else {
                    ordering == std::cmp::Ordering::Less
                };
                if replace {
                    argument.clone()
                } else {
                    current
                }
            }
        });
    }
    best.unwrap_or(Value::Null)
}

/// `lower(x)` and `upper(x)`, which fold ASCII only.
fn change_case(value: &Value<'_>, upper: bool, encoding: TextEncoding) -> Value<'static> {
    let bytes = eval::text_bytes(value, encoding);
    let folded: Vec<u8> = bytes
        .iter()
        .map(|byte| {
            if upper {
                byte.to_ascii_uppercase()
            } else {
                byte.to_ascii_lowercase()
            }
        })
        .collect();
    Value::owned_text(&folded).unwrap_or(Value::Null)
}

/// `hex(x)`, which renders bytes as upper-case hexadecimal.
fn hex(value: &Value<'_>, encoding: TextEncoding) -> Value<'static> {
    let bytes = match value {
        Value::Blob(blob) => blob.raw().to_vec(),
        other => eval::text_bytes(other, encoding),
    };
    let mut out = Vec::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// Returns the upper-case hexadecimal digit for a nibble.
fn hex_digit(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0'.saturating_add(nibble),
        _ => b'A'.saturating_add(nibble.saturating_sub(10)),
    }
}

/// `unhex(x[, ignored])`.
fn unhex(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let Some(value) = arguments.first() else {
        return Value::Null;
    };
    if value.is_null() {
        return Value::Null;
    }
    let ignored = arguments
        .get(1)
        .map(|value| eval::text_bytes(value, encoding))
        .unwrap_or_default();
    let bytes = eval::text_bytes(value, encoding);
    let filtered: Vec<u8> = bytes
        .into_iter()
        .filter(|byte| !ignored.contains(byte))
        .collect();
    if filtered.len() % 2 != 0 {
        return Value::Null;
    }
    let mut out = Vec::with_capacity(filtered.len() / 2);
    let mut index = 0usize;
    while index < filtered.len() {
        let (Some(high), Some(low)) = (
            filtered.get(index).and_then(|byte| nibble(*byte)),
            filtered
                .get(index.saturating_add(1))
                .and_then(|byte| nibble(*byte)),
        ) else {
            return Value::Null;
        };
        out.push((high << 4) | low);
        index = index.saturating_add(2);
    }
    Value::owned_blob(&out).unwrap_or(Value::Null)
}

/// Returns the value of a hexadecimal digit.
fn nibble(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

/// `quote(x)`, which renders a value as SQL literal text.
fn quote(value: &Value<'_>, encoding: TextEncoding) -> Value<'static> {
    let rendered = match value {
        Value::Null => b"NULL".to_vec(),
        Value::Integer(_) | Value::Real(_) => eval::text_bytes(value, encoding),
        Value::Blob(blob) => {
            let mut out = b"X'".to_vec();
            for byte in blob.raw() {
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
            out.push(b'\'');
            out
        }
        Value::Text(text) => {
            let mut out = vec![b'\''];
            for byte in text.utf8_bytes().iter() {
                if *byte == b'\'' {
                    out.push(b'\'');
                }
                out.push(*byte);
            }
            out.push(b'\'');
            out
        }
    };
    Value::owned_text(&rendered).unwrap_or(Value::Null)
}

/// `char(...)`, which builds text from Unicode code points.
fn char_of(arguments: &[Value<'static>]) -> Value<'static> {
    let mut out = String::new();
    for argument in arguments {
        let code = cast::integer_value(argument);
        let code = u32::try_from(code).unwrap_or(0xfffd);
        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
    }
    Value::owned_text(out.as_bytes()).unwrap_or(Value::Null)
}

/// `unicode(x)`, which returns the first code point.
fn unicode(value: &Value<'_>, encoding: TextEncoding) -> Value<'static> {
    let bytes = eval::text_bytes(value, encoding);
    let text = String::from_utf8_lossy(&bytes);
    match text.chars().next() {
        Some(character) => Value::Integer(u32::from(character) as i64),
        None => Value::Null,
    }
}

/// `concat(...)`, which skips NULLs rather than propagating them.
fn concat(
    arguments: &[Value<'static>],
    separator: Option<&[u8]>,
    encoding: TextEncoding,
) -> Value<'static> {
    let mut out = Vec::new();
    let mut first = true;
    for argument in arguments {
        if argument.is_null() {
            continue;
        }
        if !first {
            if let Some(separator) = separator {
                out.extend_from_slice(separator);
            }
        }
        first = false;
        out.extend_from_slice(&eval::text_bytes(argument, encoding));
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// `concat_ws(separator, ...)`.
fn concat_with_separator(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let Some(separator) = arguments.first() else {
        return Value::Null;
    };
    if separator.is_null() {
        return Value::Null;
    }
    let separator = eval::text_bytes(separator, encoding);
    concat(
        arguments.get(1..).unwrap_or(&[]),
        Some(&separator),
        encoding,
    )
}

/// `instr(haystack, needle)`, counting characters from one.
fn instr(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let (Some(haystack), Some(needle)) = (arguments.first(), arguments.get(1)) else {
        return Value::Null;
    };
    if haystack.is_null() || needle.is_null() {
        return Value::Null;
    }
    let blobs = matches!(haystack, Value::Blob(_)) && matches!(needle, Value::Blob(_));
    let haystack_bytes = eval::text_bytes(haystack, encoding);
    let needle_bytes = eval::text_bytes(needle, encoding);
    let Some(offset) = find(&haystack_bytes, &needle_bytes) else {
        return Value::Integer(0);
    };
    if blobs {
        return Value::Integer(offset.saturating_add(1) as i64);
    }
    let prefix = haystack_bytes.get(..offset).unwrap_or(&[]);
    Value::Integer(numeric::character_count(prefix, TextEncoding::Utf8).saturating_add(1) as i64)
}

/// Returns the byte offset of a needle in a haystack.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len().saturating_sub(needle.len()))
        .find(|offset| haystack.get(*offset..offset.saturating_add(needle.len())) == Some(needle))
}

/// `replace(text, from, to)`.
fn replace(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let (Some(subject), Some(from), Some(to)) =
        (arguments.first(), arguments.get(1), arguments.get(2))
    else {
        return Value::Null;
    };
    if subject.is_null() || from.is_null() || to.is_null() {
        return Value::Null;
    }
    let subject = eval::text_bytes(subject, encoding);
    let from = eval::text_bytes(from, encoding);
    let to = eval::text_bytes(to, encoding);
    if from.is_empty() {
        return Value::owned_text(&subject).unwrap_or(Value::Null);
    }
    let mut out = Vec::with_capacity(subject.len());
    let mut index = 0usize;
    while index < subject.len() {
        let rest = subject.get(index..).unwrap_or(&[]);
        if rest.starts_with(&from) {
            out.extend_from_slice(&to);
            index = index.saturating_add(from.len());
            continue;
        }
        if let Some(byte) = subject.get(index) {
            out.push(*byte);
        }
        index = index.saturating_add(1);
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// `substr(x, start[, length])`, counting characters from one.
fn substring(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let Some(subject) = arguments.first() else {
        return Value::Null;
    };
    if subject.is_null() {
        return Value::Null;
    }
    let is_blob = matches!(subject, Value::Blob(_));
    let bytes = eval::text_bytes(subject, encoding);
    let units: Vec<Vec<u8>> = if is_blob {
        bytes.iter().map(|byte| vec![*byte]).collect()
    } else {
        characters(&bytes)
    };
    let total = units.len() as i64;
    let Some(start_value) = arguments.get(1) else {
        return Value::Null;
    };
    if start_value.is_null() {
        return Value::Null;
    }
    let mut start = cast::integer_value(start_value);
    let mut count = match arguments.get(2) {
        Some(value) if value.is_null() => return Value::Null,
        Some(value) => cast::integer_value(value),
        None => total,
    };
    // A negative start counts back from the end; a negative length runs
    // backwards from the start. Both are SQLite behaviours a naive slice gets
    // wrong by silently returning nothing.
    if start < 0 {
        start = total.saturating_add(start).saturating_add(1);
        if start < 1 {
            count = count.saturating_add(start).saturating_sub(1);
            start = 1;
        }
    } else if start == 0 {
        count = count.saturating_sub(1);
        start = 1;
    }
    if count < 0 {
        start = start.saturating_add(count);
        count = count.saturating_neg();
        if start < 1 {
            count = count.saturating_add(start).saturating_sub(1);
            start = 1;
        }
    }
    let first = start.saturating_sub(1).max(0) as usize;
    let last = first.saturating_add(count.max(0) as usize).min(units.len());
    let mut out = Vec::new();
    for unit in units.get(first.min(units.len())..last).unwrap_or(&[]) {
        out.extend_from_slice(unit);
    }
    if is_blob {
        return Value::owned_blob(&out).unwrap_or(Value::Null);
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// Splits UTF-8 bytes into characters.
fn characters(bytes: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(bytes);
    text.chars()
        .map(|character| character.to_string().into_bytes())
        .collect()
}

/// `trim(x[, chars])`, and its one-sided forms.
fn trim(
    arguments: &[Value<'static>],
    left: bool,
    right: bool,
    encoding: TextEncoding,
) -> Value<'static> {
    let Some(subject) = arguments.first() else {
        return Value::Null;
    };
    if subject.is_null() {
        return Value::Null;
    }
    let cutset = match arguments.get(1) {
        Some(value) if value.is_null() => return Value::Null,
        Some(value) => characters(&eval::text_bytes(value, encoding)),
        None => vec![b" ".to_vec()],
    };
    let mut units = characters(&eval::text_bytes(subject, encoding));
    if left {
        while units.first().is_some_and(|unit| cutset.contains(unit)) {
            units.remove(0);
        }
    }
    if right {
        while units.last().is_some_and(|unit| cutset.contains(unit)) {
            units.pop();
        }
    }
    let out: Vec<u8> = units.concat();
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// `round(x[, digits])`, which rounds half away from zero.
fn round(arguments: &[Value<'static>]) -> Value<'static> {
    let Some(value) = arguments.first() else {
        return Value::Null;
    };
    if value.is_null() {
        return Value::Null;
    }
    let digits = match arguments.get(1) {
        Some(value) if value.is_null() => return Value::Null,
        Some(value) => cast::integer_value(value).clamp(0, 30),
        None => 0,
    };
    let real = cast::real_value(value);
    if !real.is_finite() {
        return Value::Real(real);
    }
    let factor = 10f64.powi(digits as i32);
    let scaled = real * factor;
    // `f64::round` already rounds half away from zero, which is what SQLite
    // does and is not what "round half to even" would do.
    Value::Real(scaled.round() / factor)
}

/// `zeroblob(n)`.
fn zero_blob(value: Value<'static>) -> Value<'static> {
    let length = cast::integer_value(&value).clamp(0, 1_000_000_000) as usize;
    Value::owned_blob(&vec![0u8; length]).unwrap_or(Value::Null)
}

/// `like(pattern, text[, escape])` and `glob(pattern, text)`.
fn pattern_call(
    arguments: &[Value<'static>],
    is_like: bool,
    encoding: TextEncoding,
) -> Value<'static> {
    let (Some(pattern), Some(subject)) = (arguments.first(), arguments.get(1)) else {
        return Value::Null;
    };
    if pattern.is_null() || subject.is_null() {
        return Value::Null;
    }
    let escape = match arguments.get(2) {
        Some(value) if value.is_null() => return Value::Null,
        Some(value) => eval::text_bytes(value, encoding).first().copied(),
        None => None,
    };
    let pattern_bytes = eval::text_bytes(pattern, encoding);
    let subject_bytes = eval::text_bytes(subject, encoding);
    let matched = if is_like {
        crate::pattern::like(&pattern_bytes, &subject_bytes, escape)
    } else {
        crate::pattern::glob(&pattern_bytes, &subject_bytes)
    };
    Value::Integer(i64::from(matched))
}

/// Returns the affinity a scalar function's result should be given, if any.
///
/// Nothing in the built-in set needs one; the hook exists so a registered
/// function can later declare one without the machine having to special-case
/// it.
pub fn result_affinity(_func: ScalarFunc) -> Option<Affinity> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calls a function with owned arguments, for the tests.
    fn run(func: ScalarFunc, arguments: Vec<Value<'static>>) -> Value<'static> {
        call(func, &arguments, Collation::Binary, TextEncoding::Utf8)
    }

    /// `length` counts characters in text and bytes in a blob, which is the
    /// distinction people most often get wrong.
    #[test]
    fn length_counts_characters_in_text_and_bytes_in_a_blob() {
        let text = Value::owned_text("héllo".as_bytes()).expect("owned");
        assert_same!(run(ScalarFunc::Length, vec![text]), Value::Integer(5));
        let blob = Value::owned_blob("héllo".as_bytes()).expect("owned");
        assert_same!(run(ScalarFunc::Length, vec![blob]), Value::Integer(6));
    }

    /// `substr` counts from one, and its negative forms work.
    #[test]
    fn substr_counts_from_one_and_accepts_negatives() {
        let text = || Value::owned_text(b"abcdef").expect("owned");
        assert_same!(
            run(
                ScalarFunc::Substr,
                vec![text(), Value::Integer(2), Value::Integer(3)]
            ),
            Value::owned_text(b"bcd").expect("owned")
        );
        assert_same!(
            run(ScalarFunc::Substr, vec![text(), Value::Integer(-2)]),
            Value::owned_text(b"ef").expect("owned")
        );
        assert_same!(
            run(
                ScalarFunc::Substr,
                vec![text(), Value::Integer(4), Value::Integer(-2)]
            ),
            Value::owned_text(b"bc").expect("owned")
        );
    }

    /// The scalar `max` is NULL if any argument is NULL, which is the opposite
    /// of the aggregate.
    #[test]
    fn scalar_max_is_null_if_any_argument_is_null() {
        assert_same!(
            run(
                ScalarFunc::Max,
                vec![Value::Integer(1), Value::Null, Value::Integer(3)]
            ),
            Value::Null
        );
        assert_same!(
            run(ScalarFunc::Max, vec![Value::Integer(1), Value::Integer(3)]),
            Value::Integer(3)
        );
    }

    /// `concat` skips NULLs instead of propagating them, unlike `||`.
    #[test]
    fn concat_skips_nulls() {
        assert_same!(
            run(
                ScalarFunc::Concat,
                vec![
                    Value::owned_text(b"a").expect("owned"),
                    Value::Null,
                    Value::owned_text(b"b").expect("owned")
                ]
            ),
            Value::owned_text(b"ab").expect("owned")
        );
    }

    /// `round` rounds half away from zero.
    #[test]
    fn round_rounds_half_away_from_zero() {
        assert_same!(
            run(ScalarFunc::Round, vec![Value::Real(2.5)]),
            Value::Real(3.0)
        );
        assert_same!(
            run(ScalarFunc::Round, vec![Value::Real(-2.5)]),
            Value::Real(-3.0)
        );
        assert_same!(
            run(
                ScalarFunc::Round,
                vec![Value::Real(2.345), Value::Integer(2)]
            ),
            Value::Real(2.35)
        );
    }

    /// `quote` renders each class the way the SQL literal for it is written.
    #[test]
    fn quote_renders_sql_literals() {
        assert_same!(
            run(ScalarFunc::Quote, vec![Value::Null]),
            Value::owned_text(b"NULL").expect("owned")
        );
        assert_same!(
            run(
                ScalarFunc::Quote,
                vec![Value::owned_text(b"it's").expect("owned")]
            ),
            Value::owned_text(b"'it''s'").expect("owned")
        );
        assert_same!(
            run(
                ScalarFunc::Quote,
                vec![Value::owned_blob(&[0x41, 0x0a]).expect("owned")]
            ),
            Value::owned_text(b"X'410A'").expect("owned")
        );
    }

    /// `typeof` names the storage class, not the declared type - and it names
    /// it for NULL too, which is why it does not propagate NULL.
    #[test]
    fn typeof_names_the_storage_class() {
        assert_same!(
            run(ScalarFunc::TypeOf, vec![Value::Integer(1)]),
            Value::owned_text(b"integer").expect("owned")
        );
        assert_same!(
            run(ScalarFunc::TypeOf, vec![Value::Null]),
            Value::owned_text(b"null").expect("owned")
        );
    }

    /// The four built-ins that answer a question about their argument answer it
    /// for NULL as well, rather than returning NULL.
    #[test]
    fn the_reflective_builtins_do_not_propagate_null() {
        assert_same!(
            run(ScalarFunc::Quote, vec![Value::Null]),
            Value::owned_text(b"NULL").expect("owned")
        );
        assert_same!(
            run(ScalarFunc::Hex, vec![Value::Null]),
            Value::owned_text(b"").expect("owned")
        );
        assert_same!(
            run(ScalarFunc::ZeroBlob, vec![Value::Null]),
            Value::owned_blob(b"").expect("owned")
        );
        // And the ones that compute *with* their argument still do propagate.
        assert_same!(run(ScalarFunc::Abs, vec![Value::Null]), Value::Null);
        assert_same!(run(ScalarFunc::Lower, vec![Value::Null]), Value::Null);
    }

    /// `instr` counts characters from one and returns zero when absent.
    #[test]
    fn instr_counts_characters_from_one() {
        let haystack = Value::owned_text("héllo".as_bytes()).expect("owned");
        let needle = Value::owned_text(b"llo").expect("owned");
        assert_same!(
            run(ScalarFunc::Instr, vec![haystack, needle]),
            Value::Integer(3)
        );
        assert_same!(
            run(
                ScalarFunc::Instr,
                vec![
                    Value::owned_text(b"abc").expect("owned"),
                    Value::owned_text(b"z").expect("owned")
                ]
            ),
            Value::Integer(0)
        );
    }
}
