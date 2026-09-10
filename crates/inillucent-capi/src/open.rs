//! Opening, closing, and asking what went wrong.
//!
//! Invariant: a handle this module hands out is either fully usable or null,
//! and never half-built. SQLite's `sqlite3_open` is the one entry point that
//! returns a handle *and* an error at the same time - it allocates the
//! connection even when the open fails, so that `sqlite3_errmsg` has somewhere
//! to read the reason from - and that is honoured here, because a caller that
//! follows the documented pattern would otherwise read a null pointer to find
//! out why it got null.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};

use inillucent_legacy::{Database, DbError};

use crate::codes::{
    message_for, SQLITE_BUSY, SQLITE_MISUSE, SQLITE_OK, SQLITE_OPEN_CREATE, SQLITE_OPEN_MEMORY,
    SQLITE_OPEN_READONLY, SQLITE_VERSION, SQLITE_VERSION_NUMBER,
};
use crate::handle::{c_str, connection, misuse, sqlite3, ErrorSlot};

/// Opens a database file, creating it if it is not there.
///
/// # Safety
///
/// `filename` must be null or a NUL-terminated path, and `out` must point at
/// one writable pointer. On return `*out` is a handle the caller must close
/// with [`sqlite3_close`] - *even when this returns an error*, which is the
/// part that surprises people.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_open(filename: *const c_char, out: *mut *mut sqlite3) -> c_int {
    sqlite3_open_v2(
        filename,
        out,
        SQLITE_OPEN_CREATE | crate::codes::SQLITE_OPEN_READWRITE,
        std::ptr::null(),
    )
}

/// Opens a database file with flags.
///
/// The VFS name is accepted and, when it names one this library knows, used.
/// An unknown name is `SQLITE_ERROR`, which is what SQLite does rather than
/// silently falling back to the default.
///
/// # Safety
///
/// As [`sqlite3_open`], plus: `vfs` must be null or a NUL-terminated name.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_open_v2(
    filename: *const c_char,
    out: *mut *mut sqlite3,
    flags: c_int,
    vfs: *const c_char,
) -> c_int {
    let Some(out) = out.as_mut() else {
        return SQLITE_MISUSE;
    };
    *out = std::ptr::null_mut();
    let name = match c_str(filename) {
        Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        None => String::new(),
    };
    // An empty name is a private temporary database, and SQLITE_OPEN_MEMORY
    // says so outright. Both become the in-memory database, which is what
    // SQLite gives a caller who asked for something with no name on disk.
    let path = if name.is_empty() || flags & SQLITE_OPEN_MEMORY != 0 {
        ":memory:".to_string()
    } else {
        name
    };
    let options = inillucent_legacy::ConnectionOptions {
        writable: flags & SQLITE_OPEN_READONLY == 0,
        ..inillucent_legacy::ConnectionOptions::default()
    };
    // A named VFS is the caller's, and an unknown name is an error rather than
    // a quiet fall back to the default - a caller that asked for its own file
    // system and silently got the operating system's would write the bytes
    // somewhere it is not looking.
    let named = c_str(vfs).map(|bytes| String::from_utf8_lossy(bytes).into_owned());
    // A built-in name means the engine's own file system, which is what the
    // ordinary path already opens: routing it back out through the C table
    // would be a longer way round to the same place.
    let named = named
        .filter(|name| !name.is_empty())
        .filter(|name| !crate::vfs::builtin_names().contains(&name.as_str()));
    if let Some(name) = named {
        let Some(file_system) = crate::vfs::adapter(&name) else {
            return SQLITE_MISUSE;
        };
        return match build_on(&path, options, file_system) {
            Ok(handle) => {
                *out = Box::into_raw(handle);
                SQLITE_OK
            }
            Err((Some(handle), code)) => {
                *out = Box::into_raw(handle);
                code
            }
            Err((None, code)) => code,
        };
    }
    match build(&path, options) {
        Ok(handle) => {
            *out = Box::into_raw(handle);
            SQLITE_OK
        }
        Err((Some(handle), code)) => {
            *out = Box::into_raw(handle);
            code
        }
        Err((None, code)) => code,
    }
}

/// Builds a connection handle, or one carrying the error that stopped it.
///
/// The second arm is the one worth reading. SQLite allocates the handle even
/// when the open fails, so that `sqlite3_errmsg(db)` has somewhere to read the
/// reason from - a caller who follows the documented pattern would otherwise
/// have to dereference the null pointer to find out why it got null. That
/// stand-in handle is an in-memory database, which cannot fail for the reasons
/// a file open does; if even that fails there is nothing left to hand back and
/// the caller gets a null handle and the code.
fn build(
    path: &str,
    options: inillucent_legacy::ConnectionOptions,
) -> Result<Box<sqlite3>, (Option<Box<sqlite3>>, c_int)> {
    match open_pair(path, options) {
        Ok((database, connection)) => Ok(assemble(database, connection, ErrorSlot::default())),
        Err(error) => {
            let mut slot = ErrorSlot::default();
            let code = slot.fail(&error);
            match open_pair(":memory:", inillucent_legacy::ConnectionOptions::default()) {
                Ok((database, connection)) => {
                    Err((Some(assemble(database, connection, slot)), code))
                }
                Err(second) => Err((None, second.code().value())),
            }
        }
    }
}

/// Puts a handle together around a database, a connection and an error slot.
fn assemble(
    database: Database,
    connection: inillucent_legacy::Connection,
    last: ErrorSlot,
) -> Box<sqlite3> {
    Box::new(sqlite3 {
        database: Box::new(database),
        connection: Box::new(connection),
        open_statements: 0,
        zombie: false,
        last,
        hooks: crate::hooks::Hooks::default(),
        registered: Default::default(),
        extensions_enabled: false,
        remembered: Vec::new(),
    })
}

/// Builds a handle over a file system the caller supplied.
fn build_on(
    path: &str,
    options: inillucent_legacy::ConnectionOptions,
    file_system: std::sync::Arc<dyn inillucent_legacy::vfs::Vfs>,
) -> Result<Box<sqlite3>, (Option<Box<sqlite3>>, c_int)> {
    let opened = Database::open_with_vfs(path, options, file_system)
        .and_then(|database| database.connect().map(|held| (database, held)));
    match opened {
        Ok((database, connection)) => Ok(assemble(database, connection, ErrorSlot::default())),
        Err(error) => {
            let mut slot = ErrorSlot::default();
            let code = slot.fail(&error);
            match open_pair(":memory:", inillucent_legacy::ConnectionOptions::default()) {
                Ok((database, connection)) => {
                    Err((Some(assemble(database, connection, slot)), code))
                }
                Err(second) => Err((None, second.code().value())),
            }
        }
    }
}

/// Opens the database and its first connection together.
fn open_pair(
    path: &str,
    options: inillucent_legacy::ConnectionOptions,
) -> Result<(Database, inillucent_legacy::Connection), DbError> {
    let database = Database::open_with(path, options)?;
    let connection = database.connect()?;
    Ok((database, connection))
}

/// Closes a connection, refusing while any statement is still prepared.
///
/// # Safety
///
/// The handle must be one [`sqlite3_open`] returned and not already closed.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_close(handle: *mut sqlite3) -> c_int {
    if handle.is_null() {
        return SQLITE_OK;
    }
    let Some(database) = connection(handle) else {
        return misuse();
    };
    if database.open_statements > 0 {
        // This is not a nicety: the statements inside hold a borrow on the
        // connection that C cannot see, and freeing it under them is exactly
        // the use-after-free the count exists to prevent.
        return database.last.refuse(
            SQLITE_BUSY,
            "unable to close due to unfinalized statements or unfinished backups",
        );
    }
    drop(Box::from_raw(handle));
    SQLITE_OK
}

/// Closes a connection, or marks it to close when the last statement goes.
///
/// # Safety
///
/// As [`sqlite3_close`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_close_v2(handle: *mut sqlite3) -> c_int {
    if handle.is_null() {
        return SQLITE_OK;
    }
    let Some(database) = connection(handle) else {
        return misuse();
    };
    if database.open_statements > 0 {
        database.zombie = true;
        return SQLITE_OK;
    }
    drop(Box::from_raw(handle));
    SQLITE_OK
}

/// Returns the message for the last failure on a connection.
///
/// The pointer is valid until the next call that touches this connection.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_errmsg(handle: *mut sqlite3) -> *const c_char {
    let Some(database) = connection(handle) else {
        return c"out of memory".as_ptr();
    };
    if database.last.extended == SQLITE_OK && database.last.message.is_empty() {
        database.last.message = CString::new("not an error").unwrap_or_default();
    }
    database.last.message.as_ptr()
}

/// Returns the primary code of the last failure on a connection.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_errcode(handle: *mut sqlite3) -> c_int {
    let Some(database) = connection(handle) else {
        return SQLITE_MISUSE;
    };
    database.last.extended & 0xff
}

/// Returns the extended code of the last failure on a connection.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_errcode(handle: *mut sqlite3) -> c_int {
    let Some(database) = connection(handle) else {
        return SQLITE_MISUSE;
    };
    database.last.extended
}

/// Turns extended result codes on or off. They are always on here.
///
/// The engine carries the extended code everywhere internally, so there is
/// nothing to switch: what the flag changes in SQLite is which of two numbers
/// `sqlite3_errcode` reports, and both are available whatever this is set to.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_result_codes(handle: *mut sqlite3, _on: c_int) -> c_int {
    if connection(handle).is_none() {
        return SQLITE_MISUSE;
    }
    SQLITE_OK
}

/// Returns the English message for a numeric result code.
#[no_mangle]
pub extern "C" fn sqlite3_errstr(code: c_int) -> *const c_char {
    // The strings are static and the table is small, so one leaked `CString`
    // per distinct code asked for is bounded by the number of codes and never
    // grows again. That is what lets this return a pointer with no owner.
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<std::collections::HashMap<c_int, &'static CString>>> =
        OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let Ok(mut seen) = seen.lock() else {
        return c"unknown error".as_ptr();
    };
    let held = seen.entry(code).or_insert_with(|| {
        let text = CString::new(message_for(code)).unwrap_or_default();
        Box::leak(Box::new(text))
    });
    held.as_ptr()
}

/// Returns the library version as a string.
#[no_mangle]
pub extern "C" fn sqlite3_libversion() -> *const c_char {
    version().as_ptr()
}

/// Returns the library version as a number.
#[no_mangle]
pub extern "C" fn sqlite3_libversion_number() -> c_int {
    SQLITE_VERSION_NUMBER
}

/// Returns the source identifier.
#[no_mangle]
pub extern "C" fn sqlite3_sourceid() -> *const c_char {
    source_id().as_ptr()
}

/// Reports whether the library was built thread-safe.
///
/// It reports zero, meaning single-threaded: a connection in this engine is not
/// `Send`, and saying otherwise would invite a caller to share one across
/// threads and find out the hard way.
#[no_mangle]
pub extern "C" fn sqlite3_threadsafe() -> c_int {
    0
}

/// Reports whether a connection has no open transaction.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_get_autocommit(handle: *mut sqlite3) -> c_int {
    let Some(database) = connection(handle) else {
        return SQLITE_MISUSE;
    };
    c_int::from(database.connection.autocommit())
}

/// Returns how many rows the most recent statement changed.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_changes(handle: *mut sqlite3) -> c_int {
    sqlite3_changes64(handle) as c_int
}

/// Returns how many rows the most recent statement changed, as 64 bits.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_changes64(handle: *mut sqlite3) -> i64 {
    match connection(handle) {
        Some(database) => database.connection.changes(),
        None => 0,
    }
}

/// Returns how many rows the connection has changed since it opened.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_total_changes(handle: *mut sqlite3) -> c_int {
    sqlite3_total_changes64(handle) as c_int
}

/// Returns the lifetime change count, as 64 bits.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_total_changes64(handle: *mut sqlite3) -> i64 {
    match connection(handle) {
        Some(database) => database.connection.total_changes(),
        None => 0,
    }
}

/// Returns the rowid the most recent insert allocated.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_last_insert_rowid(handle: *mut sqlite3) -> i64 {
    match connection(handle) {
        Some(database) => database.connection.last_insert_rowid(),
        None => 0,
    }
}

/// Asks the running statement to stop.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_interrupt(handle: *mut sqlite3) {
    if let Some(database) = connection(handle) {
        database.connection.interrupt();
    }
}

/// Reports whether an interrupt is pending.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_is_interrupted(handle: *mut sqlite3) -> c_int {
    let Some(database) = connection(handle) else {
        return 0;
    };
    let flag = database.connection.interrupt_flag();
    c_int::from(flag.load(std::sync::atomic::Ordering::Relaxed))
}

/// Reports whether a database is open read-only.
///
/// # Safety
///
/// The handle must be open and `name` null or a NUL-terminated name.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_readonly(handle: *mut sqlite3, name: *const c_char) -> c_int {
    let Some(database) = connection(handle) else {
        return -1;
    };
    let wanted = c_str(name).unwrap_or(b"main").to_ascii_lowercase();
    if !names(database).contains(&wanted) {
        // Minus one is "no such database", which is a different answer from
        // "not read-only" and the reason this cannot just be a boolean.
        return -1;
    }
    c_int::from(!database.connection.is_writable())
}

/// Returns the folded names of the databases attached to a connection.
fn names(database: &sqlite3) -> Vec<Vec<u8>> {
    let Ok(rows) = database.connection.query("PRAGMA database_list") else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| row.get(1))
        .filter_map(inillucent_legacy::Value::as_text)
        .map(|text| text.raw().to_ascii_lowercase())
        .collect()
}

/// Returns the file name a database was opened from.
///
/// # Safety
///
/// The handle must be open and `name` null or a NUL-terminated name.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_filename(
    handle: *mut sqlite3,
    name: *const c_char,
) -> *const c_char {
    let Some(database) = connection(handle) else {
        return std::ptr::null();
    };
    let wanted = c_str(name).unwrap_or(b"main").to_ascii_lowercase();
    let Ok(rows) = database.connection.query("PRAGMA database_list") else {
        return std::ptr::null();
    };
    for row in rows {
        let is_wanted = row
            .get(1)
            .and_then(inillucent_legacy::Value::as_text)
            .is_some_and(|text| text.raw().eq_ignore_ascii_case(&wanted));
        if !is_wanted {
            continue;
        }
        let file = row
            .get(2)
            .and_then(inillucent_legacy::Value::as_text)
            .map(|text| text.raw().to_vec())
            .unwrap_or_default();
        database.last.message = CString::new(file).unwrap_or_default();
        return database.last.message.as_ptr();
    }
    std::ptr::null()
}

/// Returns the version string, allocated once.
fn version() -> &'static CString {
    use std::sync::OnceLock;
    static TEXT: OnceLock<CString> = OnceLock::new();
    TEXT.get_or_init(|| CString::new(SQLITE_VERSION).unwrap_or_default())
}

/// Returns the source identifier, allocated once.
fn source_id() -> &'static CString {
    use std::sync::OnceLock;
    static TEXT: OnceLock<CString> = OnceLock::new();
    TEXT.get_or_init(|| CString::new(crate::codes::SQLITE_SOURCE_ID).unwrap_or_default())
}
