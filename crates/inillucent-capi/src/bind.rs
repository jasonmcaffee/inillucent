//! Binding parameters, and the destructor contract that comes with it.
//!
//! Invariant: what the engine holds is always a value it owns. C's `bind_text`
//! and `bind_blob` take a pointer and a destructor, and the destructor is the
//! caller's statement about how long the pointer lives - `SQLITE_STATIC` means
//! forever, `SQLITE_TRANSIENT` means copy it now, and anything else means call
//! me when you are done. This module answers all three the same way, by copying
//! at once, and then honours the promise it made about the destructor: it is
//! called exactly once, when the binding is replaced or the statement is
//! finalized. Copying under `SQLITE_STATIC` costs a memcpy and removes a whole
//! class of dangling-pointer bug from every caller at once.

use std::os::raw::{c_char, c_int, c_void};

use inillucent_legacy::Value;

use crate::codes::{SQLITE_OK, SQLITE_RANGE};
use crate::handle::{c_str, counted, misuse, sqlite3_stmt, statement, Destructor};

/// `SQLITE_STATIC`: the caller's pointer outlives the statement.
///
/// It is a null function pointer in the header, which is why it is written as
/// an address here rather than a value.
pub const SQLITE_STATIC_VALUE: isize = 0;
/// `SQLITE_TRANSIENT`: copy the bytes before returning.
pub const SQLITE_TRANSIENT_VALUE: isize = -1;

/// Records a bound value at a one-based index.
///
/// # Safety
///
/// The statement must be prepared and not finalized.
unsafe fn place(handle: *mut sqlite3_stmt, index: c_int, value: Value<'static>) -> c_int {
    let Some(held) = statement(handle) else {
        return misuse();
    };
    if index < 1 {
        return SQLITE_RANGE;
    }
    let slot = (index - 1) as usize;
    if slot >= held.bound.len() {
        return SQLITE_RANGE;
    }
    release(held, slot);
    match held.statement.bind(index as u32, value.clone()) {
        Ok(()) => {
            if let Some(existing) = held.bound.get_mut(slot) {
                *existing = value;
            }
            SQLITE_OK
        }
        Err(error) => error.code().value(),
    }
}

/// Calls and forgets the destructor attached to one binding.
///
/// # Safety
///
/// The destructor must be one the caller supplied and not yet called.
unsafe fn release(held: &mut sqlite3_stmt, slot: usize) {
    if let Some(entry) = held.destructors.get_mut(slot) {
        if let Some((destructor, pointer)) = entry.take() {
            destructor(pointer);
        }
    }
}

/// Attaches a caller's destructor to a binding, honouring the two sentinels.
///
/// # Safety
///
/// `destructor` must be null, one of the two sentinels, or a function that may
/// be called once with `pointer`.
unsafe fn attach(
    handle: *mut sqlite3_stmt,
    index: c_int,
    pointer: *const c_void,
    destructor: Option<Destructor>,
) {
    let Some(held) = statement(handle) else {
        return;
    };
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
    if address == SQLITE_STATIC_VALUE || address == SQLITE_TRANSIENT_VALUE {
        // Neither sentinel is a function. Calling one would jump to address
        // zero or to minus one, which is the crash this check exists for.
        return;
    }
    if index < 1 {
        return;
    }
    if let Some(entry) = held.destructors.get_mut((index - 1) as usize) {
        *entry = Some((destructor, pointer.cast_mut()));
    }
}

/// Binds a 32-bit integer.
///
/// # Safety
///
/// The statement must be prepared and not finalized.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int(
    handle: *mut sqlite3_stmt,
    index: c_int,
    value: c_int,
) -> c_int {
    place(handle, index, Value::Integer(i64::from(value)))
}

/// Binds a 64-bit integer.
///
/// # Safety
///
/// As [`sqlite3_bind_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int64(
    handle: *mut sqlite3_stmt,
    index: c_int,
    value: i64,
) -> c_int {
    place(handle, index, Value::Integer(value))
}

/// Binds a double.
///
/// # Safety
///
/// As [`sqlite3_bind_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_double(
    handle: *mut sqlite3_stmt,
    index: c_int,
    value: f64,
) -> c_int {
    place(handle, index, Value::Real(value))
}

/// Binds SQL NULL.
///
/// # Safety
///
/// As [`sqlite3_bind_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_null(handle: *mut sqlite3_stmt, index: c_int) -> c_int {
    place(handle, index, Value::Null)
}

/// Binds text.
///
/// # Safety
///
/// `text` must point at `length` bytes, or at a NUL-terminated string when
/// `length` is negative. `destructor` must be null, `SQLITE_STATIC`,
/// `SQLITE_TRANSIENT`, or a function callable once with `text`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text(
    handle: *mut sqlite3_stmt,
    index: c_int,
    text: *const c_char,
    length: c_int,
    destructor: Option<Destructor>,
) -> c_int {
    let Some(bytes) = counted(text, length) else {
        return sqlite3_bind_null(handle, index);
    };
    let Ok(owned) = Value::owned_text(bytes) else {
        return crate::codes::SQLITE_NOMEM;
    };
    let code = place(handle, index, owned);
    attach(handle, index, text.cast(), destructor);
    code
}

/// Binds text, taking a 64-bit length and an encoding.
///
/// # Safety
///
/// As [`sqlite3_bind_text`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text64(
    handle: *mut sqlite3_stmt,
    index: c_int,
    text: *const c_char,
    length: u64,
    destructor: Option<Destructor>,
    encoding: u8,
) -> c_int {
    if encoding as i32 != crate::codes::SQLITE_UTF8 {
        return crate::codes::SQLITE_MISUSE;
    }
    let Ok(length) = c_int::try_from(length) else {
        return crate::codes::SQLITE_TOOBIG;
    };
    sqlite3_bind_text(handle, index, text, length, destructor)
}

/// Binds a blob.
///
/// # Safety
///
/// As [`sqlite3_bind_text`], except that a negative length is not allowed - a
/// blob has no terminator to find its end by.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob(
    handle: *mut sqlite3_stmt,
    index: c_int,
    bytes: *const c_void,
    length: c_int,
    destructor: Option<Destructor>,
) -> c_int {
    if bytes.is_null() {
        return sqlite3_bind_null(handle, index);
    }
    let slice = std::slice::from_raw_parts(bytes.cast::<u8>(), length.max(0) as usize);
    let Ok(owned) = Value::owned_blob(slice) else {
        return crate::codes::SQLITE_NOMEM;
    };
    let code = place(handle, index, owned);
    attach(handle, index, bytes, destructor);
    code
}

/// Binds a blob, taking a 64-bit length.
///
/// # Safety
///
/// As [`sqlite3_bind_blob`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob64(
    handle: *mut sqlite3_stmt,
    index: c_int,
    bytes: *const c_void,
    length: u64,
    destructor: Option<Destructor>,
) -> c_int {
    let Ok(length) = c_int::try_from(length) else {
        return crate::codes::SQLITE_TOOBIG;
    };
    sqlite3_bind_blob(handle, index, bytes, length, destructor)
}

/// Binds a blob of `length` zero bytes.
///
/// # Safety
///
/// As [`sqlite3_bind_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_zeroblob(
    handle: *mut sqlite3_stmt,
    index: c_int,
    length: c_int,
) -> c_int {
    let zeros = vec![0u8; length.max(0) as usize];
    let Ok(owned) = Value::owned_blob(&zeros) else {
        return crate::codes::SQLITE_NOMEM;
    };
    place(handle, index, owned)
}

/// Binds a blob of zero bytes, taking a 64-bit length.
///
/// # Safety
///
/// As [`sqlite3_bind_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_zeroblob64(
    handle: *mut sqlite3_stmt,
    index: c_int,
    length: u64,
) -> c_int {
    let Ok(length) = c_int::try_from(length) else {
        return crate::codes::SQLITE_TOOBIG;
    };
    sqlite3_bind_zeroblob(handle, index, length)
}

/// Binds a copy of a protected value, as an aggregate or a hook hands one out.
///
/// # Safety
///
/// `value` must be one this library produced and still valid.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_value(
    handle: *mut sqlite3_stmt,
    index: c_int,
    value: *mut crate::value::sqlite3_value,
) -> c_int {
    let Some(held) = value.as_ref() else {
        return sqlite3_bind_null(handle, index);
    };
    place(handle, index, held.inner.clone())
}

/// Clears every binding back to NULL.
///
/// # Safety
///
/// As [`sqlite3_bind_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_clear_bindings(handle: *mut sqlite3_stmt) -> c_int {
    let Some(held) = statement(handle) else {
        return misuse();
    };
    for slot in 0..held.bound.len() {
        release(held, slot);
        if let Some(value) = held.bound.get_mut(slot) {
            *value = Value::Null;
        }
    }
    held.statement.clear_bindings();
    SQLITE_OK
}

/// Returns the highest parameter index the statement uses.
///
/// # Safety
///
/// As [`sqlite3_bind_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_count(handle: *mut sqlite3_stmt) -> c_int {
    match statement(handle) {
        Some(held) => held.statement.parameter_count() as c_int,
        None => 0,
    }
}

/// Returns the name of a parameter, or null when it has none.
///
/// The pointer is into the statement and is valid until it is finalized.
///
/// # Safety
///
/// As [`sqlite3_bind_int`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_name(
    handle: *mut sqlite3_stmt,
    index: c_int,
) -> *const c_char {
    let Some(held) = statement(handle) else {
        return std::ptr::null();
    };
    let Some((name, _)) = held
        .statement
        .parameter_names()
        .iter()
        .find(|(_, at)| *at == index.max(0) as u32)
    else {
        return std::ptr::null();
    };
    let name = name.clone();
    // The statement keeps the bytes alive for the caller, which is what makes
    // the returned pointer good until it is finalized.
    held.held.push({
        let mut owned = name;
        owned.push(0);
        owned
    });
    match held.held.last() {
        Some(last) => last.as_ptr().cast(),
        None => std::ptr::null(),
    }
}

/// Returns the index a named parameter was given, or zero.
///
/// # Safety
///
/// `name` must be null or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_index(
    handle: *mut sqlite3_stmt,
    name: *const c_char,
) -> c_int {
    let Some(held) = statement(handle) else {
        return 0;
    };
    let Some(wanted) = c_str(name) else {
        return 0;
    };
    held.statement
        .parameter_names()
        .iter()
        .find(|(candidate, _)| candidate == wanted)
        .map(|(_, index)| *index as c_int)
        .unwrap_or(0)
}
