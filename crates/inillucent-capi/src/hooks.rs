//! Callbacks: update, commit, rollback, busy, progress, authorizer, trace.
//!
//! Invariant: a callback a caller installs is called with the connection in a
//! state that callback is allowed to touch, and is never called after it has
//! been replaced. SQLite's contract is that each of these returns the *previous*
//! user-data pointer, and a caller uses that to free what it registered - so a
//! hook that was quietly kept after being replaced would be one the caller has
//! already freed the argument of. Every setter here swaps and returns.
//!
//! The engine's hooks are Rust closures, so what each setter really does is
//! wrap a C function pointer and its user data in a closure, and keep the raw
//! pair beside it so the next setter can hand it back.

use std::os::raw::{c_char, c_int, c_void};

use crate::codes::{SQLITE_DELETE, SQLITE_INSERT, SQLITE_OK, SQLITE_UPDATE};
use crate::handle::{connection, misuse, sqlite3};

/// The C function an update hook is.
pub type UpdateCallback =
    unsafe extern "C" fn(*mut c_void, c_int, *const c_char, *const c_char, i64);
/// The C function a commit hook is.
pub type CommitCallback = unsafe extern "C" fn(*mut c_void) -> c_int;
/// The C function a rollback hook is.
pub type RollbackCallback = unsafe extern "C" fn(*mut c_void);
/// The C function a busy handler is.
pub type BusyCallback = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
/// The C function a progress handler is.
pub type ProgressCallback = unsafe extern "C" fn(*mut c_void) -> c_int;
/// The C function an authorizer is.
pub type AuthorizerCallback = unsafe extern "C" fn(
    *mut c_void,
    c_int,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
) -> c_int;
/// The C function a tracer is.
pub type TraceCallback = unsafe extern "C" fn(u32, *mut c_void, *mut c_void, *mut c_void) -> c_int;

/// What a connection remembers about the callbacks a caller installed.
#[derive(Default)]
pub struct Hooks {
    /// The update hook and its user data.
    pub(crate) update: Option<(UpdateCallback, *mut c_void)>,
    /// The commit hook and its user data.
    pub(crate) commit: Option<(CommitCallback, *mut c_void)>,
    /// The rollback hook and its user data.
    pub(crate) rollback: Option<(RollbackCallback, *mut c_void)>,
    /// The busy handler and its user data.
    pub(crate) busy: Option<(BusyCallback, *mut c_void)>,
    /// The progress handler and its user data.
    pub(crate) progress: Option<(ProgressCallback, *mut c_void)>,
    /// The authorizer and its user data.
    pub(crate) authorizer: Option<(AuthorizerCallback, *mut c_void)>,
    /// The tracer, the mask it asked for, and its user data.
    pub(crate) trace: Option<(TraceCallback, u32, *mut c_void)>,
}

/// A pointer carried into a closure that outlives this call.
///
/// A raw pointer is not `Send`, and the hook types the engine takes are, so it
/// has to be wrapped to cross. The promise being made is the caller's own: the
/// pointer it registered stays valid until it replaces or removes the hook, and
/// a connection is never used from two threads at once in this engine.
#[derive(Clone, Copy)]
struct Carried(*mut c_void);

// SAFETY: see the type comment. The pointer is opaque to this library, is only
// ever passed back to the callback that came with it, and a connection is not
// shared between threads.
unsafe impl Send for Carried {}
// SAFETY: as above.
unsafe impl Sync for Carried {}

/// Installs the callback fired once per changed row.
///
/// # Safety
///
/// The handle must be open. `data` must stay valid until the hook is replaced
/// or removed, and is returned then so the caller can free it.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_update_hook(
    handle: *mut sqlite3,
    callback: Option<UpdateCallback>,
    data: *mut c_void,
) -> *mut c_void {
    let Some(database) = connection(handle) else {
        return std::ptr::null_mut();
    };
    let previous = database.hooks.update.take().map(|(_, data)| data);
    match callback {
        None => database.connection.set_update_hook(None),
        Some(callback) => {
            database.hooks.update = Some((callback, data));
            let carried = Carried(data);
            database.connection.set_update_hook(Some(Box::new(
                move |kind, schema, table, rowid| {
                    // The whole wrapper, not its field: a raw pointer is
                    // neither `Send` nor `Sync`, and `Carried` is what promises
                    // it may cross anyway.
                    let carried = carried;
                    let operation = match kind {
                        inillucent_legacy::RowChangeKind::Insert => SQLITE_INSERT,
                        inillucent_legacy::RowChangeKind::Delete => SQLITE_DELETE,
                        inillucent_legacy::RowChangeKind::Update => SQLITE_UPDATE,
                    };
                    let schema = terminated(schema);
                    let table = terminated(table);
                    // SAFETY: the callback and its data are the caller's, and
                    // the two names live until this call returns.
                    unsafe {
                        callback(
                            carried.0,
                            operation,
                            schema.as_ptr().cast(),
                            table.as_ptr().cast(),
                            rowid,
                        );
                    }
                },
            )))
        }
    };
    previous.unwrap_or(std::ptr::null_mut())
}

/// Installs the callback fired before a commit.
///
/// Returning non-zero from it turns the commit into a rollback, which is
/// SQLite's inversion and not this engine's.
///
/// # Safety
///
/// As [`sqlite3_update_hook`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_commit_hook(
    handle: *mut sqlite3,
    callback: Option<CommitCallback>,
    data: *mut c_void,
) -> *mut c_void {
    let Some(database) = connection(handle) else {
        return std::ptr::null_mut();
    };
    let previous = database.hooks.commit.take().map(|(_, data)| data);
    match callback {
        None => database.connection.set_commit_hook(None),
        Some(callback) => {
            database.hooks.commit = Some((callback, data));
            let carried = Carried(data);
            database.connection.set_commit_hook(Some(Box::new(move || {
                let carried = carried;
                // SAFETY: the callback and its data are the caller's.
                unsafe { callback(carried.0) != 0 }
            })))
        }
    };
    previous.unwrap_or(std::ptr::null_mut())
}

/// Installs the callback fired after a rollback.
///
/// # Safety
///
/// As [`sqlite3_update_hook`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_rollback_hook(
    handle: *mut sqlite3,
    callback: Option<RollbackCallback>,
    data: *mut c_void,
) -> *mut c_void {
    let Some(database) = connection(handle) else {
        return std::ptr::null_mut();
    };
    let previous = database.hooks.rollback.take().map(|(_, data)| data);
    match callback {
        None => database.connection.set_rollback_hook(None),
        Some(callback) => {
            database.hooks.rollback = Some((callback, data));
            let carried = Carried(data);
            database
                .connection
                .set_rollback_hook(Some(Box::new(move || {
                    let carried = carried;
                    // SAFETY: the callback and its data are the caller's.
                    unsafe { callback(carried.0) }
                })))
        }
    };
    previous.unwrap_or(std::ptr::null_mut())
}

/// Installs the callback asked whether to keep waiting for a lock.
///
/// # Safety
///
/// As [`sqlite3_update_hook`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_busy_handler(
    handle: *mut sqlite3,
    callback: Option<BusyCallback>,
    data: *mut c_void,
) -> c_int {
    let Some(database) = connection(handle) else {
        return misuse();
    };
    database.hooks.busy = callback.map(|callback| (callback, data));
    SQLITE_OK
}

/// Sets how long a locked database is retried for.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_busy_timeout(handle: *mut sqlite3, milliseconds: c_int) -> c_int {
    let Some(database) = connection(handle) else {
        return misuse();
    };
    let sql = format!("PRAGMA busy_timeout = {}", milliseconds.max(0));
    match database.connection.execute_batch(&sql) {
        Ok(()) => database.succeed(),
        Err(error) => database.fail(&error),
    }
}

/// Installs the callback a long statement is asked to stop by.
///
/// # Safety
///
/// As [`sqlite3_update_hook`]. The handler takes effect for statements prepared
/// after it is installed, which is the same rule the Rust facade states.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_progress_handler(
    handle: *mut sqlite3,
    every: c_int,
    callback: Option<ProgressCallback>,
    data: *mut c_void,
) {
    let Some(database) = connection(handle) else {
        return;
    };
    match callback {
        None => {
            database.hooks.progress = None;
            database.connection.set_progress_handler(0, None);
        }
        Some(callback) => {
            database.hooks.progress = Some((callback, data));
            let carried = Carried(data);
            database.connection.set_progress_handler(
                every.max(1) as u64,
                Some(std::sync::Arc::new(move || {
                    let carried = carried;
                    // SAFETY: the callback and its data are the caller's.
                    unsafe { callback(carried.0) != 0 }
                })),
            );
        }
    }
}

/// Installs the authorizer consulted while a statement is prepared.
///
/// It is recorded and returned, and it is *not* consulted: this engine
/// authorizes through its own registry, which the C surface has no way to name.
/// Recording it means `sqlite3_set_authorizer(db, NULL, NULL)` still hands the
/// caller back what it registered, which is what a caller frees.
///
/// # Safety
///
/// As [`sqlite3_update_hook`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_authorizer(
    handle: *mut sqlite3,
    callback: Option<AuthorizerCallback>,
    data: *mut c_void,
) -> c_int {
    let Some(database) = connection(handle) else {
        return misuse();
    };
    database.hooks.authorizer = callback.map(|callback| (callback, data));
    SQLITE_OK
}

/// Installs the tracer, and says which events it wants.
///
/// # Safety
///
/// As [`sqlite3_update_hook`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_trace_v2(
    handle: *mut sqlite3,
    mask: u32,
    callback: Option<TraceCallback>,
    data: *mut c_void,
) -> c_int {
    let Some(database) = connection(handle) else {
        return misuse();
    };
    database.hooks.trace = callback.map(|callback| (callback, mask, data));
    SQLITE_OK
}

/// Returns a NUL-terminated copy of some bytes.
fn terminated(bytes: &[u8]) -> Vec<u8> {
    let mut owned = bytes.to_vec();
    owned.push(0);
    owned
}
