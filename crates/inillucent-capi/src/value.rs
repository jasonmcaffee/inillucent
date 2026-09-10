//! `sqlite3_value`, `sqlite3_context`, and the conversions C expects.
//!
//! Invariant: a conversion here is SQLite's, not Rust's. `sqlite3_value_int` on
//! the text `'42abc'` is 42 and on `'abc'` is 0, because C callers have relied
//! on that since 2004 and a stricter answer would be a different database. The
//! rules live in `inillucent_value` where the engine already applies them; this
//! module only chooses which one each entry point asks for.
//!
//! A `sqlite3_value*` handed to a caller is owned by whoever produced it - a
//! statement's row, or the argument array of a function call - and is valid for
//! exactly as long as that thing is. `sqlite3_value_dup` is how a caller keeps
//! one longer, and `sqlite3_value_free` is how it gives it back.

use std::os::raw::{c_char, c_int, c_void};

use inillucent_legacy::{DbError, Value};

use crate::codes::{SQLITE_BLOB, SQLITE_FLOAT, SQLITE_INTEGER, SQLITE_NULL, SQLITE_TEXT};

/// One value, as C sees it.
#[allow(non_camel_case_types)]
pub struct sqlite3_value {
    /// The value itself.
    pub(crate) inner: Value<'static>,
    /// Bytes handed out for it, kept so the pointer stays valid.
    pub(crate) held: Vec<u8>,
    /// The subtype a function set, which `sqlite3_value_subtype` reports.
    pub(crate) subtype: u32,
}

impl sqlite3_value {
    /// Wraps a value for a caller to read.
    pub(crate) fn new(inner: Value<'static>) -> sqlite3_value {
        sqlite3_value {
            inner,
            held: Vec::new(),
            subtype: 0,
        }
    }
}

/// The context a function implementation writes its answer into.
#[allow(non_camel_case_types)]
pub struct sqlite3_context {
    /// What the function decided.
    pub(crate) result: Result<Value<'static>, DbError>,
    /// The subtype it marked the answer with.
    pub(crate) subtype: u32,
    /// The pointer the function was registered with.
    pub(crate) user_data: *mut c_void,
    /// The connection the call is on.
    pub(crate) owner: *mut crate::handle::sqlite3,
    /// An aggregate's accumulator, allocated on first use.
    pub(crate) aggregate: *mut c_void,
    /// How big that accumulator is.
    pub(crate) aggregate_bytes: usize,
}

/// Returns the storage class C reports for a value.
pub(crate) fn type_of(value: &Value<'_>) -> c_int {
    match value {
        Value::Null => SQLITE_NULL,
        Value::Integer(_) => SQLITE_INTEGER,
        Value::Real(_) => SQLITE_FLOAT,
        Value::Text(_) => SQLITE_TEXT,
        Value::Blob(_) => SQLITE_BLOB,
    }
}

/// Returns the integer C reads out of a value, converting as SQLite does.
pub(crate) fn as_integer(value: &Value<'_>) -> i64 {
    inillucent_legacy::cast::integer_value(value)
}

/// Returns the double C reads out of a value.
pub(crate) fn as_real(value: &Value<'_>) -> f64 {
    inillucent_legacy::cast::real_value(value)
}

/// Returns the text C reads out of a value.
///
/// A NULL has no text at all, which is why this returns an empty vector and the
/// entry point that uses it returns a null pointer rather than an empty string:
/// the difference is the whole reason `sqlite3_column_type` exists.
pub(crate) fn as_text(value: &Value<'_>) -> Vec<u8> {
    match value {
        Value::Null => Vec::new(),
        Value::Text(text) => text.raw().to_vec(),
        Value::Blob(blob) => blob.raw().to_vec(),
        other => text_of(other),
    }
}

/// Renders a number the way the engine writes it into a text column.
fn text_of(value: &Value<'_>) -> Vec<u8> {
    let cast = inillucent_legacy::cast::cast_value(
        value.clone(),
        inillucent_legacy::Affinity::Text,
        inillucent_legacy::TextEncoding::Utf8,
    );
    match cast {
        Ok(Value::Text(text)) => text.raw().to_vec(),
        Ok(Value::Blob(blob)) => blob.raw().to_vec(),
        _ => Vec::new(),
    }
}

/// Returns the bytes C reads out of a value as a blob.
pub(crate) fn as_blob(value: &Value<'_>) -> Vec<u8> {
    match value {
        Value::Blob(blob) => blob.raw().to_vec(),
        other => as_text(other),
    }
}

/// Renders a value the way `sqlite3_expanded_sql` writes a bound parameter.
fn literal(value: &Value<'_>) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(number) => number.to_string(),
        Value::Real(_) => String::from_utf8_lossy(&text_of(value)).into_owned(),
        Value::Text(text) => {
            let body = String::from_utf8_lossy(text.raw()).replace('\'', "''");
            format!("'{body}'")
        }
        Value::Blob(blob) => {
            let mut out = String::from("x'");
            for byte in blob.raw() {
                out.push_str(&format!("{byte:02X}"));
            }
            out.push('\'');
            out
        }
    }
}

/// Substitutes bound values into SQL text, the way `expanded_sql` does.
///
/// It walks the text rather than re-parsing it, and it skips string and blob
/// literals so that a `?` inside a quoted string is left alone - which is the
/// only subtlety, and the one a naive replace gets wrong.
pub(crate) fn expand(sql: &[u8], bound: &[Value<'static>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sql.len());
    let mut next = 0usize;
    let mut quote: Option<u8> = None;
    let mut at = 0usize;
    while at < sql.len() {
        let Some(byte) = sql.get(at).copied() else {
            break;
        };
        match quote {
            Some(open) => {
                out.push(byte);
                if byte == open {
                    quote = None;
                }
                at += 1;
            }
            None if byte == b'\'' || byte == b'"' || byte == b'`' => {
                quote = Some(byte);
                out.push(byte);
                at += 1;
            }
            None if byte == b'?' => {
                // A digit after the mark names the index outright; without one
                // the marks are numbered in the order they appear.
                let mut digits = String::new();
                let mut scan = at + 1;
                while let Some(digit) = sql.get(scan) {
                    if !digit.is_ascii_digit() {
                        break;
                    }
                    digits.push(*digit as char);
                    scan += 1;
                }
                let index = if digits.is_empty() {
                    next += 1;
                    next
                } else {
                    digits.parse::<usize>().unwrap_or(0)
                };
                let value = bound.get(index.saturating_sub(1)).cloned();
                out.extend_from_slice(literal(&value.unwrap_or(Value::Null)).as_bytes());
                at = scan;
            }
            None if byte == b':' || byte == b'@' || byte == b'$' => {
                let mut scan = at + 1;
                while let Some(character) = sql.get(scan) {
                    if !character.is_ascii_alphanumeric() && *character != b'_' {
                        break;
                    }
                    scan += 1;
                }
                if scan == at + 1 {
                    out.push(byte);
                    at += 1;
                    continue;
                }
                next += 1;
                let value = bound.get(next.saturating_sub(1)).cloned();
                out.extend_from_slice(literal(&value.unwrap_or(Value::Null)).as_bytes());
                at = scan;
            }
            None => {
                out.push(byte);
                at += 1;
            }
        }
    }
    out
}

/// Returns the storage class of a value.
///
/// # Safety
///
/// The pointer must be one this library produced and still valid.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_type(value: *mut sqlite3_value) -> c_int {
    match value.as_ref() {
        Some(held) => type_of(&held.inner),
        None => SQLITE_NULL,
    }
}

/// Returns a value as a 32-bit integer.
///
/// # Safety
///
/// As [`sqlite3_value_type`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_int(value: *mut sqlite3_value) -> c_int {
    sqlite3_value_int64(value) as c_int
}

/// Returns a value as a 64-bit integer.
///
/// # Safety
///
/// As [`sqlite3_value_type`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_int64(value: *mut sqlite3_value) -> i64 {
    match value.as_ref() {
        Some(held) => as_integer(&held.inner),
        None => 0,
    }
}

/// Returns a value as a double.
///
/// # Safety
///
/// As [`sqlite3_value_type`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_double(value: *mut sqlite3_value) -> f64 {
    match value.as_ref() {
        Some(held) => as_real(&held.inner),
        None => 0.0,
    }
}

/// Returns a value as UTF-8 text, or null for NULL.
///
/// The pointer belongs to the value and is valid as long as it is.
///
/// # Safety
///
/// As [`sqlite3_value_type`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_text(value: *mut sqlite3_value) -> *const u8 {
    let Some(held) = value.as_mut() else {
        return std::ptr::null();
    };
    if matches!(held.inner, Value::Null) {
        return std::ptr::null();
    }
    held.held = as_text(&held.inner);
    held.held.push(0);
    held.held.as_ptr()
}

/// Returns a value's bytes, or null for NULL.
///
/// # Safety
///
/// As [`sqlite3_value_type`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_blob(value: *mut sqlite3_value) -> *const c_void {
    let Some(held) = value.as_mut() else {
        return std::ptr::null();
    };
    if matches!(held.inner, Value::Null) {
        return std::ptr::null();
    }
    held.held = as_blob(&held.inner);
    held.held.as_ptr().cast()
}

/// Returns how many bytes a value's text or blob holds.
///
/// # Safety
///
/// As [`sqlite3_value_type`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_bytes(value: *mut sqlite3_value) -> c_int {
    match value.as_ref() {
        Some(held) => as_text(&held.inner).len() as c_int,
        None => 0,
    }
}

/// Returns the subtype a function marked a value with.
///
/// # Safety
///
/// As [`sqlite3_value_type`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_subtype(value: *mut sqlite3_value) -> u32 {
    match value.as_ref() {
        Some(held) => held.subtype,
        None => 0,
    }
}

/// Copies a value so it outlives what produced it.
///
/// # Safety
///
/// As [`sqlite3_value_type`]. The copy must be released with
/// [`sqlite3_value_free`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_dup(value: *const sqlite3_value) -> *mut sqlite3_value {
    let Some(held) = value.as_ref() else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(sqlite3_value {
        inner: held.inner.clone(),
        held: Vec::new(),
        subtype: held.subtype,
    }))
}

/// Releases a value [`sqlite3_value_dup`] produced.
///
/// # Safety
///
/// The pointer must be one `sqlite3_value_dup` returned, freed once.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_free(value: *mut sqlite3_value) {
    if value.is_null() {
        return;
    }
    drop(Box::from_raw(value));
}

/// Reports whether a value is one no row supplied.
///
/// Nothing in this engine produces an unbound value - it exists for a virtual
/// table's `xUpdate`, where SQLite marks a column the statement did not set -
/// so this is always false, which is what a caller checking it will expect from
/// an engine that always supplies every column.
///
/// # Safety
///
/// As [`sqlite3_value_type`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_nochange(_value: *mut sqlite3_value) -> c_int {
    0
}

/// Reports whether a value came straight from a column rather than an
/// expression.
///
/// # Safety
///
/// As [`sqlite3_value_type`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_frombind(_value: *mut sqlite3_value) -> c_int {
    0
}

/// Sets a function's answer to an integer.
///
/// # Safety
///
/// The context must be one passed to a function implementation.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_int(context: *mut sqlite3_context, value: c_int) {
    if let Some(held) = context.as_mut() {
        held.result = Ok(Value::Integer(i64::from(value)));
    }
}

/// Sets a function's answer to a 64-bit integer.
///
/// # Safety
///
/// As [`sqlite3_result_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_int64(context: *mut sqlite3_context, value: i64) {
    if let Some(held) = context.as_mut() {
        held.result = Ok(Value::Integer(value));
    }
}

/// Sets a function's answer to a double.
///
/// # Safety
///
/// As [`sqlite3_result_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_double(context: *mut sqlite3_context, value: f64) {
    if let Some(held) = context.as_mut() {
        held.result = Ok(Value::Real(value));
    }
}

/// Sets a function's answer to NULL.
///
/// # Safety
///
/// As [`sqlite3_result_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_null(context: *mut sqlite3_context) {
    if let Some(held) = context.as_mut() {
        held.result = Ok(Value::Null);
    }
}

/// Sets a function's answer to text, copying it.
///
/// # Safety
///
/// `text` must point at `length` bytes, or at a NUL-terminated string when
/// `length` is negative. The destructor, when it is neither sentinel, is called
/// before returning, because the bytes are copied here.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_text(
    context: *mut sqlite3_context,
    text: *const c_char,
    length: c_int,
    destructor: Option<crate::handle::Destructor>,
) {
    let Some(held) = context.as_mut() else {
        return;
    };
    match crate::handle::counted(text, length) {
        Some(bytes) => {
            held.result = Value::owned_text(bytes);
        }
        None => held.result = Ok(Value::Null),
    }
    finish(text.cast(), destructor);
}

/// Sets a function's answer to a blob, copying it.
///
/// # Safety
///
/// As [`sqlite3_result_text`], except that the length is not optional.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_blob(
    context: *mut sqlite3_context,
    bytes: *const c_void,
    length: c_int,
    destructor: Option<crate::handle::Destructor>,
) {
    let Some(held) = context.as_mut() else {
        return;
    };
    if bytes.is_null() {
        held.result = Ok(Value::Null);
    } else {
        let slice = std::slice::from_raw_parts(bytes.cast::<u8>(), length.max(0) as usize);
        held.result = Value::owned_blob(slice);
    }
    finish(bytes, destructor);
}

/// Sets a function's answer to a copy of a value.
///
/// # Safety
///
/// As [`sqlite3_result_int`], and `value` must be valid.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_value(
    context: *mut sqlite3_context,
    value: *mut sqlite3_value,
) {
    let Some(held) = context.as_mut() else {
        return;
    };
    held.result = Ok(match value.as_ref() {
        Some(source) => source.inner.clone(),
        None => Value::Null,
    });
}

/// Makes a function's answer an error with a message.
///
/// # Safety
///
/// As [`sqlite3_result_text`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error(
    context: *mut sqlite3_context,
    message: *const c_char,
    length: c_int,
) {
    let Some(held) = context.as_mut() else {
        return;
    };
    let text = crate::handle::counted(message, length)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_else(|| "error".to_string());
    held.result = Err(DbError::primary(inillucent_legacy::PrimaryCode::Error).with_message(text));
}

/// Makes a function's answer an error with a numeric code.
///
/// # Safety
///
/// As [`sqlite3_result_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_code(context: *mut sqlite3_context, code: c_int) {
    let Some(held) = context.as_mut() else {
        return;
    };
    let message = crate::codes::message_for(code);
    held.result = Err(DbError::new(inillucent_legacy::ExtendedCode(code)).with_message(message));
}

/// Makes a function's answer an out-of-memory error.
///
/// # Safety
///
/// As [`sqlite3_result_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_nomem(context: *mut sqlite3_context) {
    sqlite3_result_error_code(context, crate::codes::SQLITE_NOMEM);
}

/// Makes a function's answer a "string or blob too big" error.
///
/// # Safety
///
/// As [`sqlite3_result_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_toobig(context: *mut sqlite3_context) {
    sqlite3_result_error_code(context, crate::codes::SQLITE_TOOBIG);
}

/// Marks a function's answer with a subtype.
///
/// # Safety
///
/// As [`sqlite3_result_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_subtype(context: *mut sqlite3_context, subtype: u32) {
    if let Some(held) = context.as_mut() {
        held.subtype = subtype;
    }
}

/// Sets a function's answer to a blob of zero bytes.
///
/// # Safety
///
/// As [`sqlite3_result_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_zeroblob(context: *mut sqlite3_context, length: c_int) {
    let Some(held) = context.as_mut() else {
        return;
    };
    held.result = Value::owned_blob(&vec![0u8; length.max(0) as usize]);
}

/// Calls a caller's destructor, unless it is one of the two sentinels.
///
/// # Safety
///
/// The destructor must be null, a sentinel, or callable once with `pointer`.
unsafe fn finish(pointer: *const c_void, destructor: Option<crate::handle::Destructor>) {
    let Some(destructor) = destructor else {
        return;
    };
    // **The sentinels are the reason this cast exists.** SQLite's ABI passes
    // `SQLITE_STATIC` and `SQLITE_TRANSIENT` in the destructor argument as the
    // integers 0 and -1, so telling them from a real function pointer means
    // comparing the pointer's address - there is no other place the
    // distinction lives.
    #[allow(clippy::fn_to_numeric_cast_any, clippy::fn_to_numeric_cast)]
    let address = destructor as isize;
    if address == crate::bind::SQLITE_STATIC_VALUE || address == crate::bind::SQLITE_TRANSIENT_VALUE
    {
        return;
    }
    destructor(pointer.cast_mut());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parameter is substituted, and a question mark inside a string is not.
    #[test]
    fn expanding_skips_question_marks_inside_strings() {
        let bound = vec![Value::Integer(7)];
        let out = expand(b"SELECT '?', ?", &bound);
        assert_eq!(String::from_utf8_lossy(&out), "SELECT '?', 7");
    }

    /// Text is quoted and its own quotes are doubled.
    #[test]
    fn expanding_quotes_text() {
        let bound = vec![Value::owned_text(b"it's").expect("owns")];
        let out = expand(b"SELECT ?", &bound);
        assert_eq!(String::from_utf8_lossy(&out), "SELECT 'it''s'");
    }

    /// A named parameter is substituted in the order it appears.
    #[test]
    fn expanding_handles_a_named_parameter() {
        let bound = vec![Value::Integer(1), Value::Integer(2)];
        let out = expand(b"SELECT :a, :b", &bound);
        assert_eq!(String::from_utf8_lossy(&out), "SELECT 1, 2");
    }
}
