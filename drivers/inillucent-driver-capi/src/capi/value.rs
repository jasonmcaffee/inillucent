//! The result set handle, and reading one value out of it.
//!
//! Invariant: **a pointer a reader hands back borrows the result set.** The
//! text and blob accessors return a pointer into the rows the handle owns,
//! which is live until that handle is freed and not one moment longer.

use std::ffi::{c_char, CString};

use inillucent_driver::{Rows, Value};

use crate::capi::error::*;
use crate::*;

/// A materialised result.
pub struct inillucent_rows {
    /// The rows themselves.
    rows: Rows,
    /// Column names as C strings, built once so a pointer into one is stable.
    names: Vec<CString>,
    /// Declared types as C strings, likewise.
    types: Vec<CString>,
    /// The completion tag as a C string.
    tag: CString,
}
/// Turns a driver result into the handle, with its strings in C form.
///
/// The C strings are built **here rather than on demand**, because the header
/// promises a pointer valid until the result is freed and one built inside an
/// accessor would be freed when that accessor returned.
///
/// @param rows - the driver's result
pub(crate) fn built(rows: Rows) -> inillucent_rows {
    let names = rows.columns.iter().map(|c| c_string(&c.name)).collect();
    let types = rows
        .columns
        .iter()
        .map(|c| c_string(&c.declared_type))
        .collect();
    let tag = c_string(&rows.tag);
    inillucent_rows {
        rows,
        names,
        types,
        tag,
    }
}
/// Turns a C caller's `uint64_t` limit into a `usize`, saturating.
///
/// A 32-bit build cannot hold `UINT64_MAX`, and the shape a caller writes for
/// "no limit" is exactly that - so saturating is what makes `~0ull` mean what
/// it obviously means rather than wrapping to nothing.
///
/// @param limit - what the caller asked for
pub(crate) fn capped(limit: u64) -> usize {
    usize::try_from(limit).unwrap_or(usize::MAX)
}
/// Frees a result.
///
/// @param rows - the result
///
/// # Safety
///
/// `rows` must be null or a live handle, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_free(rows: *mut inillucent_rows) {
    guarded_value(
        || {
            if !rows.is_null() {
                drop(Box::from_raw(rows));
            }
        },
        (),
    )
}
/// Returns how many columns a result has.
///
/// @param rows - the result
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_column_count(rows: *const inillucent_rows) -> usize {
    guarded_value(|| held(rows).map_or(0, |held| held.rows.columns.len()), 0)
}
/// Returns one column's name.
///
/// @param rows - the result
/// @param nth - the column, from zero
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_column_name(
    rows: *const inillucent_rows,
    nth: usize,
) -> *const c_char {
    guarded_value(
        || match held(rows).and_then(|held| held.names.get(nth)) {
            Some(name) => name.as_ptr(),
            None => std::ptr::null(),
        },
        std::ptr::null(),
    )
}
/// Returns one column's declared type, or the empty string for an expression.
///
/// @param rows - the result
/// @param nth - the column, from zero
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_column_type(
    rows: *const inillucent_rows,
    nth: usize,
) -> *const c_char {
    guarded_value(
        || match held(rows).and_then(|held| held.types.get(nth)) {
            Some(name) => name.as_ptr(),
            None => std::ptr::null(),
        },
        std::ptr::null(),
    )
}
/// Returns how many rows the caller was handed.
///
/// @param rows - the result
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_count(rows: *const inillucent_rows) -> usize {
    guarded_value(|| held(rows).map_or(0, |held| held.rows.rows.len()), 0)
}
/// Returns how many rows the statement produced, exactly.
///
/// @param rows - the result
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_total(rows: *const inillucent_rows) -> usize {
    guarded_value(|| held(rows).map_or(0, |held| held.rows.total), 0)
}
/// Reports whether the limit cut anything off.
///
/// @param rows - the result
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_more(rows: *const inillucent_rows) -> i32 {
    guarded_value(|| held(rows).map_or(0, |held| i32::from(held.rows.more)), 0)
}
/// Returns how many rows the statement changed, or -1 for a query.
///
/// @param rows - the result
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_affected(rows: *const inillucent_rows) -> i64 {
    guarded_value(
        || {
            held(rows).map_or(-1, |held| {
                held.rows
                    .affected
                    .map_or(-1, |count| i64::try_from(count).unwrap_or(i64::MAX))
            })
        },
        0,
    )
}
/// Returns how long the statement took, in microseconds.
///
/// @param rows - the result
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_elapsed_us(rows: *const inillucent_rows) -> u64 {
    guarded_value(
        || {
            held(rows).map_or(0, |held| {
                u64::try_from(held.rows.elapsed.as_micros()).unwrap_or(u64::MAX)
            })
        },
        0,
    )
}
/// Returns the completion tag.
///
/// @param rows - the result
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_rows_tag(rows: *const inillucent_rows) -> *const c_char {
    guarded_value(
        || match held(rows) {
            Some(held) => held.tag.as_ptr(),
            None => std::ptr::null(),
        },
        std::ptr::null(),
    )
}
/// Returns what kind of value a cell holds.
///
/// @param rows - the result
/// @param row - the row, from zero
/// @param column - the column, from zero
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_value_type(
    rows: *const inillucent_rows,
    row: usize,
    column: usize,
) -> i32 {
    guarded_value(
        || match held(rows).and_then(|held| held.rows.value(row, column)) {
            Some(value) => value.kind() as i32,
            None => 0,
        },
        0,
    )
}
/// Returns a cell as an integer, or zero when it is not one.
///
/// @param rows - the result
/// @param row - the row, from zero
/// @param column - the column, from zero
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_value_int(
    rows: *const inillucent_rows,
    row: usize,
    column: usize,
) -> i64 {
    guarded_value(
        || match held(rows).and_then(|held| held.rows.value(row, column)) {
            Some(Value::Integer(number)) => *number,
            Some(Value::Real(number)) => *number as i64,
            _ => 0,
        },
        0,
    )
}
/// Returns a cell as a float, or zero when it is not one.
///
/// @param rows - the result
/// @param row - the row, from zero
/// @param column - the column, from zero
///
/// # Safety
///
/// `rows` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_value_real(
    rows: *const inillucent_rows,
    row: usize,
    column: usize,
) -> f64 {
    guarded_value(
        || match held(rows).and_then(|held| held.rows.value(row, column)) {
            Some(Value::Real(number)) => *number,
            Some(Value::Integer(number)) => *number as f64,
            _ => 0.0,
        },
        0.0,
    )
}
/// Returns a cell's bytes and their length.
///
/// **Not NUL-terminated**, because a text value may contain a NUL byte and
/// truncating there would lose data with nothing to say it had.
///
/// @param rows - the result
/// @param row - the row, from zero
/// @param column - the column, from zero
/// @param len - where the length goes
///
/// # Safety
///
/// `rows` must be null or a live handle and `len` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn inillucent_value_bytes(
    rows: *const inillucent_rows,
    row: usize,
    column: usize,
    len: *mut usize,
) -> *const u8 {
    guarded_value(
        || {
            let bytes = held(rows)
                .and_then(|held| held.rows.value(row, column))
                .and_then(Value::bytes);
            match bytes {
                Some(bytes) => {
                    if !len.is_null() {
                        *len = bytes.len();
                    }
                    bytes.as_ptr()
                }
                None => {
                    if !len.is_null() {
                        *len = 0;
                    }
                    std::ptr::null()
                }
            }
        },
        std::ptr::null(),
    )
}
