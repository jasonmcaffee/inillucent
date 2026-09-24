//! The database handle, its connections, and its transactions.
//!
//! Invariant: **a connection outlives nothing.** `inillucent_conn` holds an
//! `Rc` of the state its database owns, so a connection freed after its
//! database is a live handle rather than a dangling one, and a database
//! freed while a connection is open is refused rather than accepted.

use std::cell::Cell;
use std::ffi::{c_char, CString};
use std::rc::Rc;

use inillucent_driver::{Database, Error, OpenOptions, Status};

use crate::capi::error::*;
use crate::capi::value::*;
use crate::*;

/// An open database file.
pub struct inillucent_db {
    /// The word this handle carries while it is alive - see [`Live`].
    live: Live,
    /// The driver's database.
    pub(crate) database: Database,
    /// The file, as a C string, so [`inillucent_path`] can hand one back.
    path: CString,
    /// How many connections are open on it.
    ///
    /// Counted so that [`inillucent_close`] can refuse rather than leave a
    /// connection holding a pointer to freed memory. A count is the whole
    /// mechanism by which this crate stays free of dangling handles, so it is
    /// incremented and decremented in exactly two places.
    connections: Cell<usize>,
}
/// What a connection *is*, shared by the handle and by everything prepared on
/// it.
///
/// **Statements and transactions hold this rather than a pointer to the
/// connection handle.** They used to hold the pointer, and
/// [`inillucent_conn_free`] released the handle without asking whether any
/// child still existed - so a caller who freed a connection and then stepped a
/// statement dereferenced freed memory, in a language that cannot see it. The
/// header did not forbid that order because the header did not mention it.
///
/// There is no order to get wrong now. The state lives as long as the last
/// thing that names it, whichever that turns out to be, and the database's
/// connection count follows the state rather than the handle - so
/// [`inillucent_close`] still refuses while a statement prepared on a freed
/// connection is alive, which is the case a count of *handles* would have
/// missed.
pub(crate) struct ConnState {
    /// The database it belongs to, which outlives it by [`inillucent_close`]'s
    /// refusal.
    pub(crate) database: *const inillucent_db,
    /// The engine session every call on this handle runs in.
    ///
    /// **This is what makes the handle a connection rather than a name for the
    /// database.** Every entry point below borrows the `Database` for the
    /// length of one call, because `Connection` borrows it and a C handle
    /// cannot hold a borrow - so each call used to open a *new*
    /// session, and everything a session scopes was gone by the next one: a
    /// `CREATE TEMP TABLE` did not survive the statement that made it, an
    /// `ATTACH` did not outlive its own call, and a connection pragma had to be
    /// re-applied every time.
    ///
    /// Keeping the number and reconnecting with it is the engine's own answer
    /// to this shape, and `Database::connect_as` is where the driver passes it
    /// on.
    pub(crate) session: u64,
}
impl Drop for ConnState {
    /// Gives the connection back to the database it was counted against.
    ///
    /// This runs when the *last* of the connection handle, its statements and
    /// its transactions is freed. The database cannot have been closed by then:
    /// [`inillucent_close`] refuses while the count this decrements is not
    /// zero.
    fn drop(&mut self) {
        // SAFETY: `database` was a live `inillucent_db` when this state was
        // made, and `inillucent_close` refuses to free one while any state
        // counted against it is alive - which this one is, until this line.
        if let Some(database) = unsafe { held(self.database) } {
            database
                .connections
                .set(database.connections.get().saturating_sub(1));
        }
    }
}
/// A connection to a database.
pub struct inillucent_conn {
    /// The word this handle carries while it is alive - see [`Live`].
    live: Live,
    /// What the connection is, shared with its statements and transactions.
    pub(crate) state: Rc<ConnState>,
}
impl Handle for inillucent_db {
    const MAGIC: u32 = 0x5244_4231;
    fn live(&self) -> &Live {
        &self.live
    }
}
impl Handle for inillucent_conn {
    const MAGIC: u32 = 0x5244_4232;
    fn live(&self) -> &Live {
        &self.live
    }
}

/// An open transaction.
pub struct inillucent_txn {
    /// The word this handle carries while it is alive - see [`Live`].
    live: Live,
    /// The connection it runs on, shared rather than pointed at.
    connection: Rc<ConnState>,
    /// Whether it has been committed or rolled back.
    ///
    /// A spent handle refuses further work rather than issuing a second
    /// `COMMIT`, which the engine would report as a puzzling error about there
    /// being no transaction.
    spent: bool,
}
/// Opens a database.
///
/// @param path - the file, UTF-8
/// @param flags - the `INILLUCENT_OPEN_*` bits
/// @param out - where the handle goes
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `path` must be a NUL-terminated string, and `out` must be writable.
#[no_mangle]
pub unsafe extern "C" fn inillucent_open(
    path: *const c_char,
    flags: u32,
    out: *mut *mut inillucent_db,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_open", error, || {
        let Some(path) = borrowed(path) else {
            return misused("inillucent_open", error);
        };
        if out.is_null() {
            return misused("inillucent_open", error);
        }
        let diagnostics = flags & INILLUCENT_OPEN_DIAGNOSTICS != 0;
        let options = OpenOptions {
            create: flags & INILLUCENT_OPEN_CREATE != 0,
            read_only: flags & INILLUCENT_OPEN_READONLY != 0,
            diagnostics,
            ..OpenOptions::default()
        };
        match Database::open_with(path, options) {
            Ok(database) => {
                let held = Box::new(inillucent_db {
                    live: Live::new(inillucent_db::MAGIC),
                    path: c_string(&database.path().display().to_string()),
                    database,
                    connections: Cell::new(0),
                });
                *out = publish(held);
                INILLUCENT_OK
            }
            Err(why) => {
                let status = why.status as i32;
                report(error, &why, diagnostics);
                status
            }
        }
    })
}
/// Checkpoints and closes a database.
///
/// Refuses while a connection is open on it, because freeing it then would
/// leave every connection pointing at freed memory - and a use-after-free in a
/// language that cannot see it is the worst thing this boundary can produce.
///
/// @param db - the database
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `db` must be null or a handle from [`inillucent_open`] not yet closed.
#[no_mangle]
pub unsafe extern "C" fn inillucent_close(
    db: *mut inillucent_db,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_close", error, || {
        let Some(database) = held(db as *const inillucent_db) else {
            return misused("inillucent_close", error);
        };
        if database.connections.get() > 0 {
            report(
                error,
                &Error::said(
                    Status::InvalidState,
                    format!(
                        "this database still has {} connection(s) open; free them before \
                         closing it.",
                        database.connections.get()
                    ),
                ),
                false,
            );
            return INILLUCENT_INVALID_STATE;
        }
        let outcome = database.database.checkpoint();
        // The handle goes whether or not the checkpoint worked: a caller told
        // "close failed" would have no way to try again, and the log is
        // replayed on the next open regardless.
        drop(reclaim(db));
        finish(outcome, error, false)
    })
}
/// Makes everything written so far durable.
///
/// @param db - the database
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_checkpoint(
    db: *mut inillucent_db,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_checkpoint", error, || {
        match held(db as *const inillucent_db) {
            None => misused("inillucent_checkpoint", error),
            Some(database) => finish(database.database.checkpoint(), error, false),
        }
    })
}
/// Walks every tree and reports the first thing that is wrong.
///
/// @param db - the database
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_integrity_check(
    db: *mut inillucent_db,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_integrity_check", error, || {
        match held(db as *const inillucent_db) {
            None => misused("inillucent_integrity_check", error),
            Some(database) => finish(database.database.integrity_check(), error, false),
        }
    })
}
/// Copies the database to a path, opening and checking the copy.
///
/// @param db - the database
/// @param path - where the copy goes
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `db` must be a live handle and `path` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn inillucent_backup_to(
    db: *mut inillucent_db,
    path: *const c_char,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_backup_to", error, || {
        match (held(db as *const inillucent_db), borrowed(path)) {
            (Some(database), Some(path)) => finish(database.database.backup_to(path), error, false),
            _ => misused("inillucent_backup_to", error),
        }
    })
}
/// Returns the file a database is in.
///
/// @param db - the database
///
/// # Safety
///
/// `db` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_path(db: *const inillucent_db) -> *const c_char {
    guarded_value(
        || match held(db) {
            Some(database) => database.path.as_ptr(),
            None => std::ptr::null(),
        },
        std::ptr::null(),
    )
}
/// Opens a connection.
///
/// @param db - the database
/// @param out - where the handle goes
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `db` must be a live handle and `out` writable.
#[no_mangle]
pub unsafe extern "C" fn inillucent_connect(
    db: *mut inillucent_db,
    out: *mut *mut inillucent_conn,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_connect", error, || {
        let Some(database) = held(db as *const inillucent_db) else {
            return misused("inillucent_connect", error);
        };
        if out.is_null() {
            return misused("inillucent_connect", error);
        }
        // Counted against the database *before* the state exists, and given
        // back by `ConnState::drop`. The count is of live states rather than
        // of live handles, which is what makes `inillucent_close` refuse while
        // a statement outlives the connection it was prepared on.
        database
            .connections
            .set(database.connections.get().saturating_add(1));
        *out = publish(Box::new(inillucent_conn {
            live: Live::new(inillucent_conn::MAGIC),
            state: Rc::new(ConnState {
                database: db as *const inillucent_db,
                // Opened once, here, and continued by every call on this
                // handle and on everything prepared on it.
                session: database.database.session().session(),
            }),
        }));
        INILLUCENT_OK
    })
}
/// Frees a connection.
///
/// **Any order is safe.** Freeing a connection while a statement or a
/// transaction prepared on it is still alive releases this handle and nothing
/// else: the state they share outlives it, so the child keeps working and the
/// database keeps refusing to close until the last of them is freed. This used
/// to release the state, and a statement stepped afterwards read freed memory.
///
/// @param conn - the connection
///
/// # Safety
///
/// `conn` must be null or a live handle, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn inillucent_conn_free(conn: *mut inillucent_conn) {
    guarded_value(
        || {
            if conn.is_null() {
                return;
            }
            if held(conn as *const inillucent_conn).is_none() {
                return;
            }
            drop(reclaim(conn));
        },
        (),
    )
}
/// Runs one statement and collects what it produced.
///
/// @param conn - the connection
/// @param sql - the statement
/// @param limit - how many rows to hand back
/// @param out - where the result goes
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `conn` must be a live handle, `sql` a NUL-terminated string, `out` writable.
#[no_mangle]
pub unsafe extern "C" fn inillucent_execute(
    conn: *mut inillucent_conn,
    sql: *const c_char,
    limit: u64,
    out: *mut *mut inillucent_rows,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_execute", error, || {
        let (Some(database), Some(sql)) = (database_of(conn), borrowed(sql)) else {
            return misused("inillucent_execute", error);
        };
        if out.is_null() {
            return misused("inillucent_execute", error);
        }
        let connection = database.database.session_as(session_of(conn));
        match connection.query(sql, &[], capped(limit)) {
            Ok(rows) => {
                *out = publish(Box::new(built(rows)));
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
/// Runs several statements separated by semicolons, for their effect.
///
/// @param conn - the connection
/// @param sql - the statements
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `conn` must be a live handle and `sql` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn inillucent_execute_batch(
    conn: *mut inillucent_conn,
    sql: *const c_char,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_execute_batch", error, || {
        let (Some(database), Some(sql)) = (database_of(conn), borrowed(sql)) else {
            return misused("inillucent_execute_batch", error);
        };
        finish(
            database
                .database
                .session_as(session_of(conn))
                .execute_batch(sql),
            error,
            false,
        )
    })
}
/// Returns the rowid the last `INSERT` assigned.
///
/// @param conn - the connection
///
/// A connection that is already running a statement answers zero, which is what
/// this returns for a null handle too: there is no number a reentrant read could
/// give that would be true, and the C ABI has no room for a second one.
///
/// # Safety
///
/// `conn` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_last_insert_rowid(conn: *mut inillucent_conn) -> i64 {
    guarded_value(
        || match database_of(conn) {
            Some(database) => database
                .database
                .session_as(session_of(conn))
                .last_insert_rowid()
                .unwrap_or(0),
            None => 0,
        },
        0,
    )
}
/// Returns how many rows every statement so far has changed.
///
/// @param conn - the connection
///
/// A connection that is already running a statement answers zero, which is what
/// this returns for a null handle too: there is no number a reentrant read could
/// give that would be true, and the C ABI has no room for a second one.
///
/// # Safety
///
/// `conn` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_total_changes(conn: *mut inillucent_conn) -> i64 {
    guarded_value(
        || match database_of(conn) {
            Some(database) => database
                .database
                .session_as(session_of(conn))
                .total_changes()
                .unwrap_or(0),
            None => 0,
        },
        0,
    )
}
/// Reports whether a transaction is open.
///
/// @param conn - the connection
///
/// A connection that is already running a statement answers zero, which is what
/// this returns for a null handle too: there is no number a reentrant read could
/// give that would be true, and the C ABI has no room for a second one.
///
/// # Safety
///
/// `conn` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_in_transaction(conn: *mut inillucent_conn) -> i32 {
    guarded_value(
        || match database_of(conn) {
            Some(database) => i32::from(
                database
                    .database
                    .session_as(session_of(conn))
                    .in_transaction()
                    .unwrap_or(false),
            ),
            None => 0,
        },
        0,
    )
}
/// Returns the schema's generation.
///
/// @param conn - the connection
///
/// A connection that is already running a statement answers zero, which is what
/// this returns for a null handle too: there is no number a reentrant read could
/// give that would be true, and the C ABI has no room for a second one.
///
/// # Safety
///
/// `conn` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_schema_cookie(conn: *mut inillucent_conn) -> u64 {
    guarded_value(
        || match database_of(conn) {
            Some(database) => database
                .database
                .session_as(session_of(conn))
                .schema_cookie()
                .unwrap_or(0),
            None => 0,
        },
        0,
    )
}
/// Asks a running statement to stop.
///
/// **This said "which this engine cannot do" and returned `Unsupported`, and
/// both had stopped being true (task-1932, M9).** It sets a flag the executor
/// reads at every leaf of a scan and every batch a result collects, so a long
/// scan, a large result and a slow join all stop with `interrupted` and leave
/// the connection usable. `inillucent_supports("cancel")` answers `partial`,
/// not `no`: what it does not interrupt is one operator part-way through one
/// indivisible piece of work, so a sort of what it has already read finishes.
/// That is a bound on how soon a cancel takes effect rather than on whether it
/// works.
///
/// @param conn - the connection
/// @param error - where the refusal goes, or null
///
/// # Safety
///
/// `conn` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_cancel(
    conn: *mut inillucent_conn,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_cancel", error, || match database_of(conn) {
        None => misused("inillucent_cancel", error),
        Some(database) => finish(
            database.database.session_as(session_of(conn)).cancel(),
            error,
            false,
        ),
    })
}
/// Opens a transaction.
///
/// @param conn - the connection
/// @param out - where the handle goes
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `conn` must be a live handle and `out` writable.
#[no_mangle]
pub unsafe extern "C" fn inillucent_txn_begin(
    conn: *mut inillucent_conn,
    out: *mut *mut inillucent_txn,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_txn_begin", error, || {
        let Some(database) = database_of(conn) else {
            return misused("inillucent_txn_begin", error);
        };
        if out.is_null() {
            return misused("inillucent_txn_begin", error);
        }
        let status = finish(
            database
                .database
                .session_as(session_of(conn))
                .execute_batch("BEGIN"),
            error,
            false,
        );
        if status != INILLUCENT_OK {
            return status;
        }
        let Some(handle) = held(conn as *const inillucent_conn) else {
            return misused("inillucent_txn_begin", error);
        };
        *out = publish(Box::new(inillucent_txn {
            live: Live::new(inillucent_txn::MAGIC),
            connection: Rc::clone(&handle.state),
            spent: false,
        }));
        INILLUCENT_OK
    })
}
/// Runs one statement inside a transaction.
///
/// On failure the transaction is rolled back before this returns, so a caller
/// that stops on the first error has already undone everything - which is what
/// makes "all or nothing" true without the caller having to remember it.
///
/// @param txn - the transaction
/// @param sql - the statement
/// @param affected - where the changed-row count goes, or null
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `txn` must be a live handle and `sql` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn inillucent_txn_execute(
    txn: *mut inillucent_txn,
    sql: *const c_char,
    affected: *mut u64,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_txn_execute", error, || {
        // **The handle is checked before it is dereferenced** (task-2066
        // §4.1.13). task-1979 gave every handle a magic word read through
        // `held()` and these three entry points were missed, so
        // `as_mut()` followed whatever the caller passed. Begin, roll back,
        // then commit the same pointer read a freed `Rc`, reached a live
        // database through it, and issued a real `COMMIT` - reporting a syntax
        // status. A Python `Transaction.__del__` after an explicit `commit()`
        // is that sequence, written by accident rather than on purpose.
        if held(txn as *const inillucent_txn).is_none() {
            return misused("inillucent_txn_execute", error);
        }
        let Some(transaction) = txn.as_mut() else {
            return misused("inillucent_txn_execute", error);
        };
        let (Some(database), Some(sql)) = (database_in(&transaction.connection), borrowed(sql))
        else {
            return misused("inillucent_txn_execute", error);
        };
        if transaction.spent {
            report(
                error,
                &Error::said(
                    Status::InvalidState,
                    "this transaction has already been committed or rolled back.",
                ),
                false,
            );
            return INILLUCENT_INVALID_STATE;
        }
        let connection = database.database.session_as(transaction.connection.session);
        match connection.query(sql, &[], 0) {
            Ok(rows) => {
                if !affected.is_null() {
                    *affected = rows.affected.unwrap_or(0);
                }
                INILLUCENT_OK
            }
            Err(why) => {
                let status = why.status as i32;
                let _ = connection.execute_batch("ROLLBACK");
                transaction.spent = true;
                report(error, &why, why.detail.is_some());
                status
            }
        }
    })
}
/// Commits a transaction. The handle is spent either way and must be freed.
///
/// @param txn - the transaction
/// @param error - where a failure goes, or null
///
/// # Safety
///
/// `txn` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_txn_commit(
    txn: *mut inillucent_txn,
    error: *mut *mut inillucent_error,
) -> i32 {
    guarded("inillucent_txn_commit", error, || {
        // **The handle is checked before it is dereferenced** (task-2066
        // §4.1.13). task-1979 gave every handle a magic word read through
        // `held()` and these three entry points were missed, so
        // `as_mut()` followed whatever the caller passed. Begin, roll back,
        // then commit the same pointer read a freed `Rc`, reached a live
        // database through it, and issued a real `COMMIT` - reporting a syntax
        // status. A Python `Transaction.__del__` after an explicit `commit()`
        // is that sequence, written by accident rather than on purpose.
        if held(txn as *const inillucent_txn).is_none() {
            return misused("inillucent_txn_commit", error);
        }
        let Some(transaction) = txn.as_mut() else {
            return misused("inillucent_txn_commit", error);
        };
        if transaction.spent {
            report(
                error,
                &Error::said(
                    Status::InvalidState,
                    "this transaction has already been committed or rolled back.",
                ),
                false,
            );
            return INILLUCENT_INVALID_STATE;
        }
        let Some(database) = database_in(&transaction.connection) else {
            return misused("inillucent_txn_commit", error);
        };
        transaction.spent = true;
        let outcome = database
            .database
            .session_as(transaction.connection.session)
            .execute_batch("COMMIT");
        finish(outcome, error, false)
    })
}
/// Rolls a transaction back and frees it.
///
/// @param txn - the transaction
///
/// # Safety
///
/// `txn` must be null or a live handle, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn inillucent_txn_rollback(txn: *mut inillucent_txn) {
    guarded_value(
        || {
            if txn.is_null() {
                return;
            }
            if held(txn as *const inillucent_txn).is_none() {
                return;
            }
            let transaction = reclaim(txn);
            if transaction.spent {
                return;
            }
            if let Some(database) = database_in(&transaction.connection) {
                let _ = database
                    .database
                    .session_as(transaction.connection.session)
                    .execute_batch("ROLLBACK");
            }
        },
        (),
    )
}

impl Handle for inillucent_txn {
    const MAGIC: u32 = 0x5244_4233;
    fn live(&self) -> &Live {
        &self.live
    }
}
