//! Prepare, step, reset, finalize: the statement lifecycle.
//!
//! Invariant: a statement handle counts against the connection that made it,
//! from `sqlite3_prepare_v2` until `sqlite3_finalize`, and that count is what
//! keeps the erased borrow in `handle.rs` honest. Every path that creates one
//! increments; every path that destroys one decrements and then closes a
//! connection `sqlite3_close_v2` left waiting.
//!
//! `sqlite3_step` is where SQLite's shape and this engine's differ most
//! visibly, and the difference is confined here: the engine's `step` returns a
//! boolean and an error, while C wants `SQLITE_ROW`, `SQLITE_DONE`, or a code.
//! Nothing else in this crate has to know that.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

use inillucent_legacy::Statement;

use crate::codes::{SQLITE_DONE, SQLITE_MISUSE, SQLITE_OK, SQLITE_ROW};
use crate::handle::{connection, counted, misuse, sqlite3, sqlite3_stmt, statement};

/// Prepares one statement, reporting where the next one starts.
///
/// # Safety
///
/// `sql` must point at `length` bytes, or at a NUL-terminated string when
/// `length` is negative. `out` receives a handle the caller must finalize, or
/// null when the text held no statement.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v2(
    handle: *mut sqlite3,
    sql: *const c_char,
    length: c_int,
    out: *mut *mut sqlite3_stmt,
    tail: *mut *const c_char,
) -> c_int {
    sqlite3_prepare_v3(handle, sql, length, 0, out, tail)
}

/// Prepares one statement, with flags.
///
/// The flags are accepted and have no effect: `SQLITE_PREPARE_PERSISTENT` is a
/// hint about a lookaside allocator this engine does not have, and
/// `SQLITE_PREPARE_NO_VTAB` exists to keep a schema-parsing statement from
/// reaching a module, which this engine does by other means. Accepting them is
/// right; acting on them would be inventing behaviour.
///
/// # Safety
///
/// As [`sqlite3_prepare_v2`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v3(
    handle: *mut sqlite3,
    sql: *const c_char,
    length: c_int,
    _flags: u32,
    out: *mut *mut sqlite3_stmt,
    tail: *mut *const c_char,
) -> c_int {
    let Some(out) = out.as_mut() else {
        return SQLITE_MISUSE;
    };
    *out = std::ptr::null_mut();
    let Some(database) = connection(handle) else {
        return misuse();
    };
    let Some(bytes) = counted(sql, length) else {
        return database.last.refuse(SQLITE_MISUSE, "no SQL was given");
    };
    let text = String::from_utf8_lossy(bytes).into_owned();
    // The borrow is erased here and nowhere else. It is sound because the
    // connection is boxed, never moves, and cannot be freed while
    // `open_statements` is non-zero - which this increments.
    let owner: &'static inillucent_legacy::Connection = std::mem::transmute(&*database.connection);
    match owner.prepare_with_tail(&text) {
        Err(error) => {
            if !tail.is_null() {
                *tail = sql;
            }
            database.fail(&error)
        }
        Ok((prepared, _consumed)) => {
            // The tail is a pointer *into the caller's buffer*, not a copy,
            // which is what lets a caller loop over a script by feeding the
            // tail back in. It is left just past the semicolon, which is where
            // the header says it goes and where the statement's own text ends.
            let used = prepared.sql_used().min(bytes.len());
            if !tail.is_null() {
                *tail = sql.add(used);
            }
            let sql_text =
                CString::new(bytes.get(..used).unwrap_or_default().to_vec()).unwrap_or_default();
            let parameters = prepared.parameter_count() as usize;
            database.open_statements = database.open_statements.saturating_add(1);
            let held = Box::new(sqlite3_stmt {
                owner: handle,
                statement: prepared,
                has_row: false,
                done: false,
                busy: false,
                row: Vec::new(),
                held: Vec::new(),
                values: Vec::new(),
                sql: sql_text,
                bound: vec![inillucent_legacy::Value::Null; parameters],
                destructors: (0..parameters).map(|_| None).collect(),
            });
            *out = Box::into_raw(held);
            database.succeed()
        }
    }
}

/// Runs the statement until it produces a row or finishes.
///
/// # Safety
///
/// The handle must be one prepared and not yet finalized.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_step(handle: *mut sqlite3_stmt) -> c_int {
    let Some(held) = statement(handle) else {
        return misuse();
    };
    held.busy = true;
    held.held.clear();
    match held.statement.step() {
        Ok(true) => {
            held.has_row = true;
            held.row = held.statement.row().to_vec();
            if let Some(database) = connection(held.owner) {
                database.succeed();
            }
            SQLITE_ROW
        }
        Ok(false) => {
            held.has_row = false;
            held.done = true;
            held.row.clear();
            if let Some(database) = connection(held.owner) {
                database.succeed();
            }
            SQLITE_DONE
        }
        Err(error) => {
            held.has_row = false;
            held.done = true;
            held.row.clear();
            match connection(held.owner) {
                Some(database) => database.fail(&error),
                None => error.code().value(),
            }
        }
    }
}

/// Puts the statement back at the start, keeping its bindings.
///
/// # Safety
///
/// As [`sqlite3_step`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_reset(handle: *mut sqlite3_stmt) -> c_int {
    let Some(held) = statement(handle) else {
        return misuse();
    };
    held.has_row = false;
    held.done = false;
    held.busy = false;
    held.row.clear();
    held.held.clear();
    match held.statement.reset() {
        Ok(()) => SQLITE_OK,
        Err(error) => match connection(held.owner) {
            Some(database) => database.fail(&error),
            None => error.code().value(),
        },
    }
}

/// Destroys a statement, releasing its hold on the connection.
///
/// # Safety
///
/// The handle must be one prepared and not already finalized. Null is a no-op,
/// which is what makes the `finalize` in an error path safe to write.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_finalize(handle: *mut sqlite3_stmt) -> c_int {
    if handle.is_null() {
        return SQLITE_OK;
    }
    let held = Box::from_raw(handle);
    let owner = held.owner;
    call_destructors(&held);
    let code = match held.statement.finalize() {
        Ok(()) => SQLITE_OK,
        Err(error) => error.code().value(),
    };
    let Some(database) = connection(owner) else {
        return code;
    };
    database.open_statements = database.open_statements.saturating_sub(1);
    if database.zombie && database.open_statements == 0 {
        // `sqlite3_close_v2` asked for this: the connection was told to go the
        // moment its last statement did.
        drop(Box::from_raw(owner));
        return code;
    }
    code
}

/// Calls every destructor a caller attached to a bound value.
///
/// # Safety
///
/// Each destructor must be one the caller passed to a `bind` entry point and
/// must not have been called already, which is what the `Option` records.
unsafe fn call_destructors(held: &sqlite3_stmt) {
    for (destructor, pointer) in held.destructors.iter().flatten() {
        destructor(*pointer);
    }
}

/// Returns the statement's SQL text.
///
/// # Safety
///
/// As [`sqlite3_step`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_sql(handle: *mut sqlite3_stmt) -> *const c_char {
    match statement(handle) {
        Some(held) => held.sql.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Returns the SQL with its parameters substituted, for the caller to free.
///
/// # Safety
///
/// As [`sqlite3_step`]. The result must be released with `sqlite3_free`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_expanded_sql(handle: *mut sqlite3_stmt) -> *mut c_char {
    let Some(held) = statement(handle) else {
        return std::ptr::null_mut();
    };
    let text = crate::value::expand(held.sql.as_bytes(), &held.bound);
    crate::memory::owned_c_string(&text)
}

/// Reports whether the statement only reads.
///
/// # Safety
///
/// As [`sqlite3_step`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_stmt_readonly(handle: *mut sqlite3_stmt) -> c_int {
    match statement(handle) {
        Some(held) => c_int::from(held.statement.is_readonly()),
        None => 1,
    }
}

/// Reports whether the statement has been stepped and not yet reset.
///
/// # Safety
///
/// As [`sqlite3_step`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_stmt_busy(handle: *mut sqlite3_stmt) -> c_int {
    match statement(handle) {
        Some(held) => c_int::from(held.busy && !held.done),
        None => 0,
    }
}

/// Returns the connection a statement was prepared on.
///
/// # Safety
///
/// As [`sqlite3_step`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_handle(handle: *mut sqlite3_stmt) -> *mut sqlite3 {
    match statement(handle) {
        Some(held) => held.owner,
        None => std::ptr::null_mut(),
    }
}

/// Runs a script, calling back once per row.
///
/// # Safety
///
/// `sql` must be a NUL-terminated script. `error_out`, when not null, receives
/// a message the caller must release with `sqlite3_free`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_exec(
    handle: *mut sqlite3,
    sql: *const c_char,
    callback: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
    >,
    context: *mut c_void,
    error_out: *mut *mut c_char,
) -> c_int {
    if !error_out.is_null() {
        *error_out = std::ptr::null_mut();
    }
    let Some(database) = connection(handle) else {
        return misuse();
    };
    let Some(bytes) = counted(sql, -1) else {
        return database.last.refuse(SQLITE_MISUSE, "no SQL was given");
    };
    let mut rest = String::from_utf8_lossy(bytes).into_owned();
    let owner: &'static inillucent_legacy::Connection = std::mem::transmute(&*database.connection);
    loop {
        if rest.trim().is_empty() {
            return database.succeed();
        }
        let (mut prepared, consumed) = match owner.prepare_with_tail(&rest) {
            Ok(pair) => pair,
            Err(error) => {
                let code = database.fail(&error);
                report(error_out, error.message());
                return code;
            }
        };
        if let Err(code) = run_one(&mut prepared, callback, context, database, error_out) {
            return code;
        }
        if consumed == 0 || consumed >= rest.len() {
            return database.succeed();
        }
        rest = rest.split_off(consumed);
    }
}

/// Steps one statement, feeding each row to a callback.
fn run_one(
    prepared: &mut Statement<'_>,
    callback: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
    >,
    context: *mut c_void,
    database: &mut sqlite3,
    error_out: *mut *mut c_char,
) -> Result<(), c_int> {
    let names: Vec<*mut c_char> = prepared
        .columns()
        .iter()
        .map(|column| crate::memory::owned_c_string(&column.name))
        .collect();
    let outcome = step_rows(prepared, callback, context, &names);
    // The name block is this call's to free, however the loop ended.
    for name in &names {
        // SAFETY: each was allocated just above by `owned_c_string`.
        unsafe { crate::memory::sqlite3_free(name.cast()) };
    }
    match outcome {
        Ok(()) => Ok(()),
        Err(Some(error)) => {
            let code = database.fail(&error);
            report(error_out, error.message());
            Err(code)
        }
        // The callback asked to stop, which SQLite reports as SQLITE_ABORT.
        Err(None) => {
            let code = database
                .last
                .refuse(crate::codes::SQLITE_ABORT, "query aborted");
            Err(code)
        }
    }
}

/// Steps every row, calling back for each. `Err(None)` means the caller stopped.
fn step_rows(
    prepared: &mut Statement<'_>,
    callback: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
    >,
    context: *mut c_void,
    names: &[*mut c_char],
) -> Result<(), Option<inillucent_legacy::DbError>> {
    loop {
        match prepared.step() {
            Err(error) => return Err(Some(error)),
            Ok(false) => return Ok(()),
            Ok(true) => {
                let Some(callback) = callback else {
                    continue;
                };
                let values: Vec<*mut c_char> = prepared
                    .row()
                    .iter()
                    .map(|value| match value {
                        inillucent_legacy::Value::Null => std::ptr::null_mut(),
                        other => crate::memory::owned_c_string(&crate::value::as_text(other)),
                    })
                    .collect();
                // SAFETY: the callback is the caller's, and the two blocks are
                // ours for the duration of the call; both are freed before this
                // function returns.
                let answer = unsafe {
                    callback(
                        context,
                        values.len() as c_int,
                        values.as_ptr().cast_mut(),
                        names.as_ptr().cast_mut(),
                    )
                };
                for value in &values {
                    // SAFETY: allocated just above.
                    unsafe { crate::memory::sqlite3_free(value.cast()) };
                }
                if answer != 0 {
                    return Err(None);
                }
            }
        }
    }
}

/// Writes a message into a caller's `char**`, allocated for it to free.
fn report(error_out: *mut *mut c_char, message: &str) {
    if error_out.is_null() {
        return;
    }
    // SAFETY: the caller promised one writable pointer.
    unsafe { *error_out = crate::memory::owned_c_string(message.as_bytes()) };
}
