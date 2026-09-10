//! Reading a row: the columns, their names, and where they came from.
//!
//! Invariant: every pointer this module returns points into the statement, and
//! is valid until the next `sqlite3_step`, `sqlite3_reset` or
//! `sqlite3_finalize` on it. That is SQLite's rule and it is the reason the
//! bytes are cached on the statement rather than allocated per call: a caller
//! that reads a column, steps, and then reads its pointer again is doing
//! something the header already told it not to do, and it should find the same
//! kind of wrongness in both engines rather than a fresh allocation in one.
//!
//! `sqlite3_column_bytes` and `sqlite3_column_text` are ordered in SQLite -
//! asking for the text first is what fixes the length - and here they are not,
//! because the value is already materialised. Callers written for SQLite work
//! either way; callers written for this would break on SQLite, which is why the
//! documentation still states the order.

use std::os::raw::{c_char, c_int, c_void};

use inillucent_legacy::Value;

use crate::codes::{SQLITE_NULL, SQLITE_OK};
use crate::handle::{sqlite3_stmt, statement};
use crate::value::{as_blob, as_integer, as_real, as_text, sqlite3_value, type_of};

/// Returns how many columns a statement's result has.
///
/// # Safety
///
/// The statement must be prepared and not finalized.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_count(handle: *mut sqlite3_stmt) -> c_int {
    match statement(handle) {
        Some(held) => held.statement.column_count() as c_int,
        None => 0,
    }
}

/// Returns how many values the current row holds.
///
/// It is zero when the statement has not produced a row, which is the whole
/// difference from `sqlite3_column_count`: the count is a property of the
/// statement and this is a property of where it currently is.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_data_count(handle: *mut sqlite3_stmt) -> c_int {
    match statement(handle) {
        Some(held) if held.has_row => held.row.len() as c_int,
        _ => 0,
    }
}

/// Returns the value at a column of the current row, or NULL past the end.
fn value_at(held: &sqlite3_stmt, index: c_int) -> Option<&Value<'static>> {
    if index < 0 {
        return None;
    }
    held.row.get(index as usize)
}

/// Returns the storage class of a column of the current row.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_type(handle: *mut sqlite3_stmt, index: c_int) -> c_int {
    let Some(held) = statement(handle) else {
        return SQLITE_NULL;
    };
    match value_at(held, index) {
        Some(value) => type_of(value),
        None => SQLITE_NULL,
    }
}

/// Returns a column as a 32-bit integer.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int(handle: *mut sqlite3_stmt, index: c_int) -> c_int {
    sqlite3_column_int64(handle, index) as c_int
}

/// Returns a column as a 64-bit integer.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int64(handle: *mut sqlite3_stmt, index: c_int) -> i64 {
    let Some(held) = statement(handle) else {
        return 0;
    };
    value_at(held, index).map(as_integer).unwrap_or(0)
}

/// Returns a column as a double.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_double(handle: *mut sqlite3_stmt, index: c_int) -> f64 {
    let Some(held) = statement(handle) else {
        return 0.0;
    };
    value_at(held, index).map(as_real).unwrap_or(0.0)
}

/// Returns a column as UTF-8 text, or null for SQL NULL.
///
/// The pointer is into the statement and is valid until the next step, reset or
/// finalize.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_text(handle: *mut sqlite3_stmt, index: c_int) -> *const u8 {
    let Some(held) = statement(handle) else {
        return std::ptr::null();
    };
    let Some(value) = value_at(held, index) else {
        return std::ptr::null();
    };
    if matches!(value, Value::Null) {
        return std::ptr::null();
    }
    let mut bytes = as_text(value);
    bytes.push(0);
    hold(held, bytes)
}

/// Returns a column's bytes, or null for SQL NULL.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_blob(
    handle: *mut sqlite3_stmt,
    index: c_int,
) -> *const c_void {
    let Some(held) = statement(handle) else {
        return std::ptr::null();
    };
    let Some(value) = value_at(held, index) else {
        return std::ptr::null();
    };
    if matches!(value, Value::Null) {
        return std::ptr::null();
    }
    let bytes = as_blob(value);
    if bytes.is_empty() {
        // SQLite returns null for a zero-length blob, and a caller that checks
        // the pointer instead of the length depends on it.
        return std::ptr::null();
    }
    hold(held, bytes).cast()
}

/// Returns how many bytes a column's text or blob holds.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_bytes(handle: *mut sqlite3_stmt, index: c_int) -> c_int {
    let Some(held) = statement(handle) else {
        return 0;
    };
    match value_at(held, index) {
        Some(Value::Null) | None => 0,
        Some(Value::Blob(blob)) => blob.raw().len() as c_int,
        Some(other) => as_text(other).len() as c_int,
    }
}

/// Returns a column as a protected value.
///
/// # Safety
///
/// As [`sqlite3_column_count`]. The result is owned by the statement.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_value(
    handle: *mut sqlite3_stmt,
    index: c_int,
) -> *mut sqlite3_value {
    let Some(held) = statement(handle) else {
        return std::ptr::null_mut();
    };
    let Some(value) = value_at(held, index).cloned() else {
        return std::ptr::null_mut();
    };
    // The wrapper lives on the statement, in a box whose address does not move
    // when the list grows, and goes when the statement does. That is the
    // lifetime the header promises for `sqlite3_column_value`.
    held.values.push(Box::new(sqlite3_value::new(value)));
    match held.values.last_mut() {
        Some(last) => std::ptr::from_mut(last.as_mut()),
        None => std::ptr::null_mut(),
    }
}

/// Keeps bytes alive on the statement and returns a pointer to them.
fn hold(held: &mut sqlite3_stmt, bytes: Vec<u8>) -> *const u8 {
    held.held.push(bytes);
    match held.held.last() {
        Some(last) => last.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Returns a column's name.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_name(
    handle: *mut sqlite3_stmt,
    index: c_int,
) -> *const c_char {
    metadata(handle, index, Part::Name)
}

/// Returns a column's declared type, or null when it has none.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_decltype(
    handle: *mut sqlite3_stmt,
    index: c_int,
) -> *const c_char {
    metadata(handle, index, Part::DeclaredType)
}

/// Returns the database a column came from, or null for an expression.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_database_name(
    handle: *mut sqlite3_stmt,
    index: c_int,
) -> *const c_char {
    metadata(handle, index, Part::Database)
}

/// Returns the table a column came from, or null for an expression.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_table_name(
    handle: *mut sqlite3_stmt,
    index: c_int,
) -> *const c_char {
    metadata(handle, index, Part::Table)
}

/// Returns the column a result column came from, or null for an expression.
///
/// # Safety
///
/// As [`sqlite3_column_count`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_origin_name(
    handle: *mut sqlite3_stmt,
    index: c_int,
) -> *const c_char {
    metadata(handle, index, Part::Origin)
}

/// Which piece of a column's metadata an entry point wants.
enum Part {
    /// The name the result column reports.
    Name,
    /// The declared type of the column it came from.
    DeclaredType,
    /// The database that column is in.
    Database,
    /// The table it is in.
    Table,
    /// Its name in that table.
    Origin,
}

/// Returns one piece of a column's metadata, cached on the statement.
///
/// # Safety
///
/// The statement must be prepared and not finalized.
unsafe fn metadata(handle: *mut sqlite3_stmt, index: c_int, part: Part) -> *const c_char {
    let Some(held) = statement(handle) else {
        return std::ptr::null();
    };
    if index < 0 {
        return std::ptr::null();
    }
    let Some(column) = held.statement.columns().get(index as usize) else {
        return std::ptr::null();
    };
    let bytes = match part {
        Part::Name => Some(column.name.clone()),
        Part::DeclaredType => {
            if column.declared_type.is_empty() {
                None
            } else {
                Some(column.declared_type.clone())
            }
        }
        Part::Database => column
            .origin
            .as_ref()
            .map(|(database, _, _)| database.clone()),
        Part::Table => column.origin.as_ref().map(|(_, table, _)| table.clone()),
        Part::Origin => column.origin.as_ref().map(|(_, _, name)| name.clone()),
    };
    let Some(mut bytes) = bytes else {
        return std::ptr::null();
    };
    bytes.push(0);
    hold(held, bytes).cast()
}

/// Reports what a table column is declared as, without preparing a statement.
///
/// # Safety
///
/// Every string pointer must be null or NUL-terminated. Each `out` pointer, if
/// not null, receives a pointer this library owns; each `out` flag receives a
/// zero or one.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn sqlite3_table_column_metadata(
    handle: *mut crate::handle::sqlite3,
    database: *const c_char,
    table: *const c_char,
    column: *const c_char,
    declared_type: *mut *const c_char,
    collation: *mut *const c_char,
    not_null: *mut c_int,
    primary_key: *mut c_int,
    autoincrement: *mut c_int,
) -> c_int {
    let Some(connection) = crate::handle::connection(handle) else {
        return crate::handle::misuse();
    };
    let schema = crate::handle::c_str(database)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_else(|| "main".to_string());
    let Some(table_name) = crate::handle::c_str(table) else {
        return connection
            .last
            .refuse(crate::codes::SQLITE_MISUSE, "no table was named");
    };
    let table_name = String::from_utf8_lossy(table_name).into_owned();
    let wanted = crate::handle::c_str(column).map(|bytes| bytes.to_ascii_lowercase());
    let sql = format!("PRAGMA {schema}.table_info({table_name})");
    let Ok(rows) = connection.connection.query(&sql) else {
        return connection
            .last
            .refuse(crate::codes::SQLITE_ERROR, "no such table");
    };
    if rows.is_empty() {
        return connection
            .last
            .refuse(crate::codes::SQLITE_ERROR, "no such table");
    }
    // With no column named, SQLite reports on the rowid, which every rowid
    // table has and no `table_info` row describes.
    let Some(wanted) = wanted else {
        write_out(declared_type, collation, connection, b"INTEGER", b"BINARY");
        write_flag(not_null, 1);
        write_flag(primary_key, 1);
        write_flag(autoincrement, 0);
        return SQLITE_OK;
    };
    for row in &rows {
        let name = row
            .get(1)
            .and_then(inillucent_legacy::Value::as_text)
            .map(|text| text.raw().to_ascii_lowercase())
            .unwrap_or_default();
        if name != wanted {
            continue;
        }
        let declared = row
            .get(2)
            .and_then(inillucent_legacy::Value::as_text)
            .map(|text| text.raw().to_vec())
            .unwrap_or_default();
        let nn = row
            .get(3)
            .and_then(inillucent_legacy::Value::as_integer)
            .unwrap_or(0);
        let pk = row
            .get(5)
            .and_then(inillucent_legacy::Value::as_integer)
            .unwrap_or(0);
        write_out(declared_type, collation, connection, &declared, b"BINARY");
        write_flag(not_null, c_int::from(nn != 0));
        write_flag(primary_key, c_int::from(pk != 0));
        write_flag(autoincrement, 0);
        return SQLITE_OK;
    }
    connection
        .last
        .refuse(crate::codes::SQLITE_ERROR, "no such column")
}

/// Writes the two string answers, keeping the bytes on the connection.
fn write_out(
    declared_type: *mut *const c_char,
    collation: *mut *const c_char,
    connection: &mut crate::handle::sqlite3,
    declared: &[u8],
    collating: &[u8],
) {
    // SAFETY: the caller promised each pointer is null or writable, and the
    // bytes behind the pointers live on the connection until it is closed.
    unsafe {
        if !declared_type.is_null() {
            *declared_type = connection.remember(declared);
        }
        if !collation.is_null() {
            *collation = connection.remember(collating);
        }
    }
}

/// Writes one flag, when the caller asked for it.
fn write_flag(slot: *mut c_int, value: c_int) {
    if slot.is_null() {
        return;
    }
    // SAFETY: the caller promised a writable `int`.
    unsafe { *slot = value };
}
