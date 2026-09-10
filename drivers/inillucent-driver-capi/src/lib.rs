//! The C ABI over [`inillucent_driver`]: how every language that is not Rust
//! reaches the engine.
//!
//! Invariant: **this crate marshals and decides nothing.** Every behaviour a C
//! caller sees is `inillucent-driver`'s; what is here is pointers, lengths,
//! ownership and a panic guard. If a C caller and a Rust caller ever get
//! different answers, that is a defect in this file and not a property of it.
//!
//! `include/inillucent_driver.h` is the contract, and it is the file a binding
//! author reads. This one exists to make that file true.
//!
//! ## Why Rust does not go through here
//!
//! DuckDB routes even its own first-party Rust binding through its C ABI,
//! because its core is C++ and the C ABI is the narrowest thing it can be
//! stable across. Ours is a Rust core whose first consumer is Rust, so doing
//! the same would add a pointer round trip and a `catch_unwind` per call, lose
//! the type system across the seam, and put a Rust caller's errors through a C
//! integer and back - all of it to reach Rust. `inillucent-driver` is the
//! driver; this is the adapter.
//!
//! ## The lifetime problem, and how it is avoided rather than fought
//!
//! `Database` owns the engine, `Connection<'d>` borrows it and `Statement<'c>`
//! borrows that. C has no lifetimes, so a handle for each would mean three
//! heap allocations holding borrows of one another - which cannot be written
//! without laundering a lifetime through a transmute and then promising, in a
//! comment, that nothing frees them out of order.
//!
//! It is not necessary. `Database::connect` is cheap and the engine caches a
//! compiled statement by its text, so a connection handle holds a **pointer to
//! its database** and opens a short-lived `Connection` inside each call, and a
//! statement handle holds its SQL and its bindings and prepares inside each
//! execute. The engine's own statement cache makes the second one a hash
//! lookup rather than a compile. This is the arrangement
//! `inillucent-compat`'s facade already uses, for the same reason, and it
//! leaves this crate with no self-referential borrows to be careful about.
//!
//! What still has to be enforced by hand is that a database outlives its
//! connections, and [`inillucent_close`] does that by refusing while any are
//! open rather than by trusting the caller.

#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![allow(non_camel_case_types)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

use std::cell::Cell;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use inillucent_driver::capability::{Support, CAPABILITIES};
use inillucent_driver::{Database, Error, OpenOptions, Rows, Status, Value};

/// This ABI's version, as `major * 1_000_000 + minor * 1_000 + patch`.
///
/// The encoding is ADBC's, because a binding author who has met one of these
/// before has met that one.
pub const ABI_VERSION: u32 = 1_000_000;

// —— status codes, which the header freezes ————————————————————————

/// Nothing went wrong.
pub const INILLUCENT_OK: i32 = 0;
/// A rule of this API's own contract was broken by the caller.
pub const INILLUCENT_INVALID_STATE: i32 = 12;
/// A defect in the driver.
pub const INILLUCENT_INTERNAL: i32 = 13;

/// What a capability answers when this build has never heard of it.
pub const INILLUCENT_SUPPORT_UNKNOWN: i32 = -2;

/// Make the file when the path holds nothing.
pub const INILLUCENT_OPEN_CREATE: u32 = 0x0001;
/// Refuse anything that is not a query.
pub const INILLUCENT_OPEN_READONLY: u32 = 0x0002;
/// Let an error carry the engine's internal diagnostic text.
pub const INILLUCENT_OPEN_DIAGNOSTICS: u32 = 0x0004;

// —— handles ————————————————————————————————————————————————————————

/// An open database file.
pub struct inillucent_db {
    /// The driver's database.
    database: Database,
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
struct ConnState {
    /// The database it belongs to, which outlives it by [`inillucent_close`]'s
    /// refusal.
    database: *const inillucent_db,
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
    session: u64,
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
    /// What the connection is, shared with its statements and transactions.
    state: Rc<ConnState>,
}

/// A statement and the values bound to it.
pub struct inillucent_stmt {
    /// The connection it was prepared on, shared rather than pointed at.
    connection: Rc<ConnState>,
    /// The statement text.
    sql: String,
    /// The values bound so far, by one-based index.
    params: Vec<Value>,
}

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

/// An open transaction.
pub struct inillucent_txn {
    /// The connection it runs on, shared rather than pointed at.
    connection: Rc<ConnState>,
    /// Whether it has been committed or rolled back.
    ///
    /// A spent handle refuses further work rather than issuing a second
    /// `COMMIT`, which the engine would report as a puzzling error about there
    /// being no transaction.
    spent: bool,
}

/// One failure, with its strings already in C form.
pub struct inillucent_error {
    /// What kind of failure it was.
    status: i32,
    /// The safe message.
    message: CString,
    /// The construct the engine has not implemented, when that is why.
    feature: Option<CString>,
    /// The internal detail, when the caller asked for it.
    detail: Option<CString>,
    /// The byte offset into the statement, or -1.
    offset: i32,
}

// —— the plumbing ——————————————————————————————————————————————————

/// Runs an entry point's body, turning a panic into a status.
///
/// **A panic unwinding into a C caller is undefined behaviour**, so every entry
/// point wraps its body in this. It is a backstop and not a strategy: the
/// driver denies `unwrap`, `expect`, `panic` and slice indexing, so a panic
/// reaching here is a defect, and [`INILLUCENT_INTERNAL`] is the status that
/// says "report this" rather than "you did something wrong".
///
/// @param entry - the entry point's name, for the message
/// @param error - where to put a failure, if the caller wants one
/// @param body - what to run
fn guarded<F>(entry: &str, error: *mut *mut inillucent_error, body: F) -> i32
where
    F: FnOnce() -> i32,
{
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(_) => {
            report(
                error,
                &Error::said(
                    Status::Internal,
                    format!("{entry} failed in a way it has no handling for; this is a defect."),
                ),
                false,
            );
            INILLUCENT_INTERNAL
        }
    }
}

/// Writes a failure to the caller's out-parameter, if it wanted one.
///
/// A caller that passed NULL gets the status and nothing else, which is a
/// legitimate way to use the API for a call whose only interesting outcome is
/// whether it worked.
///
/// @param out - where the caller wants the failure, or null
/// @param failure - what went wrong
/// @param diagnostics - whether the internal detail may be carried
fn report(out: *mut *mut inillucent_error, failure: &Error, diagnostics: bool) {
    if out.is_null() {
        return;
    }
    let held = Box::new(inillucent_error {
        status: failure.status as i32,
        message: c_string(&failure.message),
        feature: failure.feature.as_deref().map(c_string),
        detail: match diagnostics {
            true => failure.detail.as_deref().map(c_string),
            false => None,
        },
        offset: failure
            .offset
            .map_or(-1, |at| i32::try_from(at).unwrap_or(-1)),
    });
    // SAFETY: `out` is a caller-supplied pointer the header requires to be
    // either null - checked above - or writable. Ownership of the box moves to
    // the caller, who frees it with `inillucent_error_free`.
    unsafe { *out = Box::into_raw(held) };
}

/// Builds a C string, replacing an interior NUL rather than failing.
///
/// A message with a NUL in it would otherwise be unreportable, and losing the
/// tail of an error message is a worse outcome than showing a `?` in it.
///
/// @param text - the text to convert
fn c_string(text: &str) -> CString {
    match CString::new(text) {
        Ok(built) => built,
        Err(_) => {
            let cleaned: String = text
                .chars()
                .map(|c| if c == '\0' { '?' } else { c })
                .collect();
            CString::new(cleaned).unwrap_or_else(|_| c"?".to_owned())
        }
    }
}

/// Reads a caller's C string as UTF-8.
///
/// @param text - the pointer the caller passed
///
/// # Safety
///
/// `text` must be null or a NUL-terminated string the caller owns for the
/// duration of the call.
unsafe fn borrowed(text: *const c_char) -> Option<&'static str> {
    if text.is_null() {
        return None;
    }
    CStr::from_ptr(text).to_str().ok()
}

/// Borrows a handle, or answers `None` for a null pointer.
///
/// @param handle - the caller's pointer
///
/// # Safety
///
/// `handle` must be null or a pointer this library returned and the caller has
/// not freed.
unsafe fn held<'a, T>(handle: *const T) -> Option<&'a T> {
    handle.as_ref()
}

/// Reports the failure a null or spent handle deserves.
///
/// @param entry - the entry point's name
/// @param error - where the caller wants the failure
fn misused(entry: &str, error: *mut *mut inillucent_error) -> i32 {
    report(
        error,
        &Error::said(
            Status::InvalidState,
            format!("{entry} was given a handle that is null or already freed."),
        ),
        false,
    );
    INILLUCENT_INVALID_STATE
}

/// Turns a driver result into a status, reporting the failure if there is one.
///
/// @param outcome - what the driver answered
/// @param error - where the caller wants a failure
/// @param diagnostics - whether the internal detail may be carried
fn finish(
    outcome: inillucent_driver::Result<()>,
    error: *mut *mut inillucent_error,
    diagnostics: bool,
) -> i32 {
    match outcome {
        Ok(()) => INILLUCENT_OK,
        Err(why) => {
            let status = why.status as i32;
            report(error, &why, diagnostics);
            status
        }
    }
}

// —— the library ————————————————————————————————————————————————————

/// Returns the ABI version a binding should check the major of.
#[no_mangle]
pub extern "C" fn inillucent_abi_version() -> u32 {
    ABI_VERSION
}

/// Returns what the driver calls itself, as a C string valid forever.
#[no_mangle]
pub extern "C" fn inillucent_version() -> *const c_char {
    // Built once and leaked on purpose: the header promises a pointer valid
    // for the life of the process, and one static allocation is the honest way
    // to keep that promise.
    static ONCE: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| c_string(inillucent_driver::version()))
        .as_ptr()
}

/// Returns how many capabilities this build declares.
#[no_mangle]
pub extern "C" fn inillucent_capability_count() -> usize {
    CAPABILITIES.len()
}

/// Reads one capability's name, state and note.
///
/// @param nth - which one, from zero
/// @param name - where to put its name, or null
/// @param state - where to put its state, or null
/// @param note - where to put its note, or null
///
/// # Safety
///
/// Each out-parameter must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn inillucent_capability(
    nth: usize,
    name: *mut *const c_char,
    state: *mut i32,
    note: *mut *const c_char,
) -> i32 {
    guarded("inillucent_capability", std::ptr::null_mut(), || {
        let Some(entry) = CAPABILITIES.get(nth) else {
            return INILLUCENT_INVALID_STATE;
        };
        if !name.is_null() {
            *name = interned(entry.name).as_ptr();
        }
        if !note.is_null() {
            *note = interned(entry.note).as_ptr();
        }
        if !state.is_null() {
            *state = entry.support as i32;
        }
        INILLUCENT_OK
    })
}

/// Reports whether the engine does something, by name.
///
/// @param name - the capability's name
///
/// # Safety
///
/// `name` must be null or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn inillucent_supports(name: *const c_char) -> i32 {
    let Some(name) = borrowed(name) else {
        return INILLUCENT_SUPPORT_UNKNOWN;
    };
    match inillucent_driver::supports(name) {
        Some(Support::Yes) => Support::Yes as i32,
        Some(Support::No) => Support::No as i32,
        Some(Support::Partial) => Support::Partial as i32,
        None => INILLUCENT_SUPPORT_UNKNOWN,
    }
}

/// Returns a C string for a static Rust string, made once and kept.
///
/// The capability table's names and notes are `&'static str` and are therefore
/// not NUL-terminated. Converting them on every call would hand back a pointer
/// into something freed on return, so each is converted once and kept for the
/// life of the process - which is what the header promises of them.
///
/// @param text - the static text
fn interned(text: &'static str) -> &'static CString {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static HELD: OnceLock<Mutex<HashMap<&'static str, &'static CString>>> = OnceLock::new();
    let table = HELD.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = match table.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    table
        .entry(text)
        .or_insert_with(|| Box::leak(Box::new(c_string(text))))
}

// —— a database ————————————————————————————————————————————————————

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
                    path: c_string(&database.path().display().to_string()),
                    database,
                    connections: Cell::new(0),
                });
                *out = Box::into_raw(held);
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
        drop(Box::from_raw(db));
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
    match held(db) {
        Some(database) => database.path.as_ptr(),
        None => std::ptr::null(),
    }
}

// —— a connection ——————————————————————————————————————————————————

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
        *out = Box::into_raw(Box::new(inillucent_conn {
            state: Rc::new(ConnState {
                database: db as *const inillucent_db,
                // Opened once, here, and continued by every call on this
                // handle and on everything prepared on it.
                session: database.database.connect().session(),
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
    if conn.is_null() {
        return;
    }
    drop(Box::from_raw(conn));
}

/// Returns the session a connection handle runs its calls in.
///
/// Zero for a null or dead handle, which is a session like any other: the
/// caller is about to be refused for the same reason the handle is not there.
///
/// @param conn - the connection handle
///
/// # Safety
///
/// `conn` must be null or a live handle.
unsafe fn session_of(conn: *const inillucent_conn) -> u64 {
    held(conn).map_or(0, |connection| connection.state.session)
}

/// Returns the database a connection belongs to.
///
/// @param conn - the connection
///
/// # Safety
///
/// `conn` must be null or a live handle.
unsafe fn database_of<'a>(conn: *const inillucent_conn) -> Option<&'a inillucent_db> {
    held(conn).and_then(|connection| database_in(&connection.state))
}

/// Returns the database a shared connection state belongs to.
///
/// The form a statement or a transaction asks in: it holds the state rather
/// than the connection handle, so that freeing the handle first cannot leave it
/// pointing at freed memory.
///
/// @param state - the shared connection state
///
/// # Safety
///
/// `state` must be a live state, which holding an `Rc` to it proves.
unsafe fn database_in<'a>(state: &ConnState) -> Option<&'a inillucent_db> {
    held(state.database)
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
        let connection = database.database.connect_as(session_of(conn));
        match connection.query(sql, &[], capped(limit)) {
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
                .connect_as(session_of(conn))
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
/// # Safety
///
/// `conn` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_last_insert_rowid(conn: *mut inillucent_conn) -> i64 {
    match database_of(conn) {
        Some(database) => database
            .database
            .connect_as(session_of(conn))
            .last_insert_rowid(),
        None => 0,
    }
}

/// Returns how many rows every statement so far has changed.
///
/// @param conn - the connection
///
/// # Safety
///
/// `conn` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_total_changes(conn: *mut inillucent_conn) -> i64 {
    match database_of(conn) {
        Some(database) => database
            .database
            .connect_as(session_of(conn))
            .total_changes(),
        None => 0,
    }
}

/// Reports whether a transaction is open.
///
/// @param conn - the connection
///
/// # Safety
///
/// `conn` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_in_transaction(conn: *mut inillucent_conn) -> i32 {
    match database_of(conn) {
        Some(database) => i32::from(
            database
                .database
                .connect_as(session_of(conn))
                .in_transaction(),
        ),
        None => 0,
    }
}

/// Returns the schema's generation.
///
/// @param conn - the connection
///
/// # Safety
///
/// `conn` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_schema_cookie(conn: *mut inillucent_conn) -> u64 {
    match database_of(conn) {
        Some(database) => database
            .database
            .connect_as(session_of(conn))
            .schema_cookie(),
        None => 0,
    }
}

/// Asks a running statement to stop, which this engine cannot do.
///
/// Always [`inillucent_driver::Status::Unsupported`], and
/// `inillucent_supports("cancel")` says so up front. It is present rather than
/// absent so a binding can wire it once and have it begin working the day the
/// capability flips.
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
            database.database.connect_as(session_of(conn)).cancel(),
            error,
            false,
        ),
    })
}

// —— a statement ————————————————————————————————————————————————————

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
        let connection = database.database.connect_as(session_of(conn));
        if let Err(why) = connection.prepare(sql) {
            let status = why.status as i32;
            report(error, &why, false);
            return status;
        }
        let Some(handle) = held(conn as *const inillucent_conn) else {
            return misused("inillucent_prepare", error);
        };
        *out = Box::into_raw(Box::new(inillucent_stmt {
            connection: Rc::clone(&handle.state),
            sql: sql.to_owned(),
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
    if !stmt.is_null() {
        drop(Box::from_raw(stmt));
    }
}

/// Binds one value at a one-based index, growing the list with NULLs.
///
/// Growing rather than refusing is what lets a caller bind `?3` before `?1`,
/// which a binding built from a dictionary will do.
///
/// @param stmt - the statement
/// @param index - the one-based parameter number
/// @param value - the value
///
/// # Safety
///
/// `stmt` must be a live handle.
unsafe fn bind(stmt: *mut inillucent_stmt, index: u32, value: Value) -> i32 {
    let Some(statement) = stmt.as_mut() else {
        return INILLUCENT_INVALID_STATE;
    };
    let Some(at) = (index as usize).checked_sub(1) else {
        return INILLUCENT_INVALID_STATE;
    };
    if at >= statement.params.len() {
        statement.params.resize(at.saturating_add(1), Value::Null);
    }
    match statement.params.get_mut(at) {
        Some(slot) => {
            *slot = value;
            INILLUCENT_OK
        }
        None => INILLUCENT_INVALID_STATE,
    }
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
    bind(stmt, index, Value::Null)
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
    bind(stmt, index, Value::Integer(value))
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
    bind(stmt, index, Value::Real(value))
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
    if value.is_null() {
        return bind(stmt, index, Value::Null);
    }
    let bytes = std::slice::from_raw_parts(value, len);
    bind(stmt, index, Value::Blob(bytes.to_vec()))
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
    if let Some(statement) = stmt.as_mut() {
        statement.params.clear();
    }
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
        let connection = database.database.connect_as(statement.connection.session);
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

// —— a result ————————————————————————————————————————————————————————

/// Turns a driver result into the handle, with its strings in C form.
///
/// The C strings are built **here rather than on demand**, because the header
/// promises a pointer valid until the result is freed and one built inside an
/// accessor would be freed when that accessor returned.
///
/// @param rows - the driver's result
fn built(rows: Rows) -> inillucent_rows {
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
fn capped(limit: u64) -> usize {
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
    if !rows.is_null() {
        drop(Box::from_raw(rows));
    }
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
    held(rows).map_or(0, |held| held.rows.columns.len())
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
    match held(rows).and_then(|held| held.names.get(nth)) {
        Some(name) => name.as_ptr(),
        None => std::ptr::null(),
    }
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
    match held(rows).and_then(|held| held.types.get(nth)) {
        Some(name) => name.as_ptr(),
        None => std::ptr::null(),
    }
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
    held(rows).map_or(0, |held| held.rows.rows.len())
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
    held(rows).map_or(0, |held| held.rows.total)
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
    held(rows).map_or(0, |held| i32::from(held.rows.more))
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
    held(rows).map_or(-1, |held| {
        held.rows
            .affected
            .map_or(-1, |count| i64::try_from(count).unwrap_or(i64::MAX))
    })
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
    held(rows).map_or(0, |held| {
        u64::try_from(held.rows.elapsed.as_micros()).unwrap_or(u64::MAX)
    })
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
    match held(rows) {
        Some(held) => held.tag.as_ptr(),
        None => std::ptr::null(),
    }
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
    match held(rows).and_then(|held| held.rows.value(row, column)) {
        Some(value) => value.kind() as i32,
        None => 0,
    }
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
    match held(rows).and_then(|held| held.rows.value(row, column)) {
        Some(Value::Integer(number)) => *number,
        Some(Value::Real(number)) => *number as i64,
        _ => 0,
    }
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
    match held(rows).and_then(|held| held.rows.value(row, column)) {
        Some(Value::Real(number)) => *number,
        Some(Value::Integer(number)) => *number as f64,
        _ => 0.0,
    }
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
}

// —— a transaction ——————————————————————————————————————————————————

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
                .connect_as(session_of(conn))
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
        *out = Box::into_raw(Box::new(inillucent_txn {
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
        let connection = database.database.connect_as(transaction.connection.session);
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
            .connect_as(transaction.connection.session)
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
    if txn.is_null() {
        return;
    }
    let transaction = Box::from_raw(txn);
    if transaction.spent {
        return;
    }
    if let Some(database) = database_in(&transaction.connection) {
        let _ = database
            .database
            .connect_as(transaction.connection.session)
            .execute_batch("ROLLBACK");
    }
}

// —— a failure ——————————————————————————————————————————————————————

/// Returns what kind of failure it was.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_status(error: *const inillucent_error) -> i32 {
    held(error).map_or(INILLUCENT_INVALID_STATE, |held| held.status)
}

/// Returns what happened, in the engine's own words.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_message(error: *const inillucent_error) -> *const c_char {
    match held(error) {
        Some(held) => held.message.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Returns the construct the engine has not implemented, or null.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_feature(error: *const inillucent_error) -> *const c_char {
    match held(error).and_then(|held| held.feature.as_ref()) {
        Some(feature) => feature.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Returns the internal diagnostic text, or null.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_detail(error: *const inillucent_error) -> *const c_char {
    match held(error).and_then(|held| held.detail.as_ref()) {
        Some(detail) => detail.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Returns the byte offset into the statement, or -1.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_offset(error: *const inillucent_error) -> i32 {
    held(error).map_or(-1, |held| held.offset)
}

/// Frees a failure.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_free(error: *mut inillucent_error) {
    if !error.is_null() {
        drop(Box::from_raw(error));
    }
}
