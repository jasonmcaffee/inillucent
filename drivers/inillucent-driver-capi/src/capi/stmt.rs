//! The prepared statement handle, and the bindings it carries.
//!
//! Invariant: **a binding index is one-based, as SQL writes it.** `?1` is
//! index 1, and index 0 is refused rather than silently treated as the
//! first parameter.

use std::ffi::c_char;
use std::rc::Rc;

use inillucent_driver::Value;

use crate::capi::db::*;
use crate::capi::error::*;
use crate::capi::value::*;
use crate::*;

/// A statement and the values bound to it.
pub struct inillucent_stmt {
    /// The word this handle carries while it is alive - see [`Live`].
    live: Live,
    /// The connection it was prepared on, shared rather than pointed at.
    connection: Rc<ConnState>,
    /// The statement text.
    sql: String,
    /// How many parameters the statement declares.
    ///
    /// The bound a bind index is checked against - see
    /// [`inillucent_driver::Connection::parameter_count`] for what an unbounded
    /// one cost.
    pub(crate) declared: u32,
    /// The values bound so far, by one-based index.
    pub(crate) params: Vec<Value>,
}
/// Prepares a statement, checking it compiles before the handle exists.
///
/// **It compiles now rather than at the first execute**, so that a bad
/// statement is a failure of `prepare` where a caller is looking for one. The
/// compiled form is the engine's, cached by the statement's text, so preparing
/// again inside execute is a hash lookup rather than a second compile.
///
/// @param conn - the connection
/// @param sql - the statement
/// @param out - where the handle goes
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `conn` must be a live handle, `sql` a NUL-terminated string, `out` writable.
#[no_mangle]
pub unsafe extern "C" fn inillucent_prepare(
    conn: *mut inillucent_conn,
    sql: *const c_char,
    out: *mut *mut inillucent_stmt,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_prepare", error, || {
        let (Some(database), Some(sql)) = (database_of(conn), borrowed(sql)) else {
            return misused("inillucent_prepare", error);
        };
        if out.is_null() {
            return misused("inillucent_prepare", error);
        }
        let connection = database.database.session_as(session_of(conn));
        if let Err(why) = connection.prepare(sql) {
            let status = why.status as i32;
            report(error, &why, false);
            return status;
        }
        // The count the bind index is checked against, read from the statement
        // that has just compiled - see `inillucent_stmt::declared`.
        let declared = match connection.parameter_count(sql) {
            Ok(declared) => declared,
            Err(why) => {
                let status = why.status as i32;
                report(error, &why, false);
                return status;
            }
        };
        let Some(handle) = held(conn as *const inillucent_conn) else {
            return misused("inillucent_prepare", error);
        };
        *out = Box::into_raw(Box::new(inillucent_stmt {
            live: Live::new(inillucent_stmt::MAGIC),
            connection: Rc::clone(&handle.state),
            sql: sql.to_owned(),
            declared,
            params: Vec::new(),
        }));
        INILLUCENT_OK
    })
}
/// Frees a statement.
///
/// @param stmt - the statement
///
/// # Safety
///
/// `stmt` must be null or a live handle, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn inillucent_stmt_free(stmt: *mut inillucent_stmt) {
    guarded_value(
        || {
            if !stmt.is_null() {
                if held(stmt as *const inillucent_stmt).is_none() {
                    return;
                }
                drop(reclaim(stmt));
            }
        },
        (),
    )
}
/// Binds NULL.
///
/// @param stmt - the statement
/// @param index - the one-based parameter number
///
/// # Safety
///
/// `stmt` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_bind_null(stmt: *mut inillucent_stmt, index: u32) -> i32 {
    guarded_value(|| bind(stmt, index, Value::Null), 0)
}
/// Binds an integer.
///
/// @param stmt - the statement
/// @param index - the one-based parameter number
/// @param value - the value
///
/// # Safety
///
/// `stmt` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_bind_int(
    stmt: *mut inillucent_stmt,
    index: u32,
    value: i64,
) -> i32 {
    guarded_value(|| bind(stmt, index, Value::Integer(value)), 0)
}
/// Binds a float.
///
/// @param stmt - the statement
/// @param index - the one-based parameter number
/// @param value - the value
///
/// # Safety
///
/// `stmt` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_bind_real(
    stmt: *mut inillucent_stmt,
    index: u32,
    value: f64,
) -> i32 {
    guarded_value(|| bind(stmt, index, Value::Real(value)), 0)
}
/// Binds text, copying it.
///
/// @param stmt - the statement
/// @param index - the one-based parameter number
/// @param value - the bytes
/// @param len - how many bytes
///
/// # Safety
///
/// `stmt` must be a live handle and `value` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn inillucent_bind_text(
    stmt: *mut inillucent_stmt,
    index: u32,
    value: *const c_char,
    len: usize,
) -> i32 {
    guarded_value(
        || {
            if value.is_null() {
                return bind(stmt, index, Value::Null);
            }
            let bytes = std::slice::from_raw_parts(value as *const u8, len);
            // Text that is not UTF-8 is bound as a blob rather than lossily converted,
            // for the reason `Value::from_engine` gives: a replacement character is a
            // value nobody passed.
            match std::str::from_utf8(bytes) {
                Ok(text) => bind(stmt, index, Value::Text(text.to_owned())),
                Err(_) => bind(stmt, index, Value::Blob(bytes.to_vec())),
            }
        },
        0,
    )
}
/// Binds bytes, copying them.
///
/// @param stmt - the statement
/// @param index - the one-based parameter number
/// @param value - the bytes
/// @param len - how many bytes
///
/// # Safety
///
/// `stmt` must be a live handle and `value` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn inillucent_bind_blob(
    stmt: *mut inillucent_stmt,
    index: u32,
    value: *const u8,
    len: usize,
) -> i32 {
    guarded_value(
        || {
            if value.is_null() {
                return bind(stmt, index, Value::Null);
            }
            let bytes = std::slice::from_raw_parts(value, len);
            bind(stmt, index, Value::Blob(bytes.to_vec()))
        },
        0,
    )
}
/// Unbinds every parameter.
///
/// @param stmt - the statement
///
/// # Safety
///
/// `stmt` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_clear_bindings(stmt: *mut inillucent_stmt) {
    guarded_value(
        || {
            if let Some(statement) = stmt.as_mut() {
                statement.params.clear();
            }
        },
        (),
    )
}
/// Runs a statement with what is bound.
///
/// @param stmt - the statement
/// @param limit - how many rows to hand back
/// @param out - where the result goes
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `stmt` must be a live handle and `out` writable.
#[no_mangle]
pub unsafe extern "C" fn inillucent_stmt_execute(
    stmt: *mut inillucent_stmt,
    limit: u64,
    out: *mut *mut inillucent_rows,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_stmt_execute", error, || {
        let Some(statement) = held(stmt as *const inillucent_stmt) else {
            return misused("inillucent_stmt_execute", error);
        };
        let Some(database) = database_in(&statement.connection) else {
            return misused("inillucent_stmt_execute", error);
        };
        if out.is_null() {
            return misused("inillucent_stmt_execute", error);
        }
        let connection = database.database.session_as(statement.connection.session);
        match connection.query(&statement.sql, &statement.params, capped(limit)) {
            Ok(rows) => {
                *out = Box::into_raw(Box::new(built(rows)));
                INILLUCENT_OK
            }
            Err(why) => {
                let status = why.status as i32;
                report(error, &why, why.detail.is_some());
                status
            }
        }
    })
}

impl Handle for inillucent_stmt {
    const MAGIC: u32 = 0x5244_4234;
    fn live(&self) -> &Live {
        &self.live
    }
}
