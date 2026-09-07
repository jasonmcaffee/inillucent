//! `sqlite3_blob`: reading and writing one value without materialising it.
//!
//! Invariant: a blob handle's length is fixed when it is opened, and a read or
//! write outside it is `SQLITE_ERROR` rather than a short transfer. That is the
//! rule the engine's own `Blob` keeps and the reason the C surface is thin here:
//! the interesting decision - that the row is looked up once and the handle does
//! not follow it afterwards - was already made underneath.

use std::os::raw::{c_char, c_int, c_void};

use crate::codes::{SQLITE_ERROR, SQLITE_MISUSE, SQLITE_OK};
use crate::handle::{c_str, connection, sqlite3, sqlite3_blob};

/// Opens a handle on one value of one row.
///
/// # Safety
///
/// The connection must be open and every name pointer NUL-terminated. `out`
/// receives a handle the caller must release with [`sqlite3_blob_close`].
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn sqlite3_blob_open(
    handle: *mut sqlite3,
    database: *const c_char,
    table: *const c_char,
    column: *const c_char,
    rowid: i64,
    writable: c_int,
    out: *mut *mut sqlite3_blob,
) -> c_int {
    let Some(out) = out.as_mut() else {
        return SQLITE_MISUSE;
    };
    *out = std::ptr::null_mut();
    let Some(connection_handle) = connection(handle) else {
        return crate::handle::misuse();
    };
    let schema = text_or(database, "main");
    let (Some(table), Some(column)) = (c_str(table), c_str(column)) else {
        return connection_handle
            .last
            .refuse(SQLITE_MISUSE, "no table or column was named");
    };
    let table = String::from_utf8_lossy(table).into_owned();
    let column = String::from_utf8_lossy(column).into_owned();
    // The borrow is erased and held open by the count, exactly as a statement's
    // is: a blob handle keeps the connection alive until it is closed.
    let owner: &'static inillucent_legacy::Connection =
        std::mem::transmute(&*connection_handle.connection);
    match owner.blob_open(&schema, &table, &column, rowid, writable != 0) {
        Err(error) => connection_handle.fail(&error),
        Ok(blob) => {
            connection_handle.open_statements = connection_handle.open_statements.saturating_add(1);
            *out = Box::into_raw(Box::new(sqlite3_blob {
                owner: handle,
                blob,
            }));
            connection_handle.succeed()
        }
    }
}

/// Points an open handle at the same column of a different row.
///
/// # Safety
///
/// The handle must be one [`sqlite3_blob_open`] returned and not closed.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_reopen(handle: *mut sqlite3_blob, rowid: i64) -> c_int {
    let Some(held) = handle.as_mut() else {
        return SQLITE_MISUSE;
    };
    match held.blob.reopen(rowid) {
        Ok(()) => SQLITE_OK,
        Err(error) => match connection(held.owner) {
            Some(database) => database.fail(&error),
            None => error.code().value(),
        },
    }
}

/// Returns how many bytes the value holds.
///
/// # Safety
///
/// As [`sqlite3_blob_reopen`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_bytes(handle: *mut sqlite3_blob) -> c_int {
    match handle.as_ref() {
        Some(held) => held.blob.len() as c_int,
        None => 0,
    }
}

/// Reads `length` bytes from `offset` into the caller's buffer.
///
/// # Safety
///
/// As [`sqlite3_blob_reopen`], and `buffer` must have room for `length` bytes.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_read(
    handle: *mut sqlite3_blob,
    buffer: *mut c_void,
    length: c_int,
    offset: c_int,
) -> c_int {
    let Some(held) = handle.as_mut() else {
        return SQLITE_MISUSE;
    };
    if buffer.is_null() || !fits(held, length, offset) {
        return SQLITE_ERROR;
    }
    let slice = std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize);
    match held.blob.read_at(offset as u32, slice) {
        Ok(()) => SQLITE_OK,
        Err(error) => match connection(held.owner) {
            Some(database) => database.fail(&error),
            None => error.code().value(),
        },
    }
}

/// Writes `length` bytes at `offset` from the caller's buffer.
///
/// # Safety
///
/// As [`sqlite3_blob_read`], and `buffer` must hold `length` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_write(
    handle: *mut sqlite3_blob,
    buffer: *const c_void,
    length: c_int,
    offset: c_int,
) -> c_int {
    let Some(held) = handle.as_mut() else {
        return SQLITE_MISUSE;
    };
    if buffer.is_null() || !fits(held, length, offset) {
        return SQLITE_ERROR;
    }
    let slice = std::slice::from_raw_parts(buffer.cast::<u8>(), length as usize);
    match held.blob.write_at(offset as u32, slice) {
        Ok(()) => SQLITE_OK,
        Err(error) => match connection(held.owner) {
            Some(database) => database.fail(&error),
            None => error.code().value(),
        },
    }
}

/// Reports whether a range lies inside the value.
///
/// SQLite answers `SQLITE_ERROR` for a read or write that runs past the end -
/// not a short transfer, and not a misuse - because the length was fixed when
/// the handle was opened and the caller could have asked.
fn fits(held: &sqlite3_blob, length: c_int, offset: c_int) -> bool {
    if length < 0 || offset < 0 {
        return false;
    }
    let end = (offset as i64).saturating_add(length as i64);
    end <= i64::from(held.blob.len())
}

/// Closes a blob handle, releasing its hold on the connection.
///
/// # Safety
///
/// The handle must be one [`sqlite3_blob_open`] returned, closed once. Null is
/// a no-op.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_close(handle: *mut sqlite3_blob) -> c_int {
    if handle.is_null() {
        return SQLITE_OK;
    }
    let held = Box::from_raw(handle);
    let owner = held.owner;
    drop(held);
    let Some(database) = connection(owner) else {
        return SQLITE_OK;
    };
    database.open_statements = database.open_statements.saturating_sub(1);
    if database.zombie && database.open_statements == 0 {
        drop(Box::from_raw(owner));
    }
    SQLITE_OK
}

/// Returns a NUL-terminated string, or a default when the pointer is null.
///
/// # Safety
///
/// The pointer must be null or NUL-terminated.
unsafe fn text_or(text: *const c_char, fallback: &str) -> String {
    match c_str(text) {
        Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        None => fallback.to_string(),
    }
}
