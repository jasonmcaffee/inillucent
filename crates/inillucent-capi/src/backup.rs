//! `sqlite3_backup`: copying a database a few pages at a time.
//!
//! Invariant: a backup handle borrows two connections and neither may be closed
//! under it, which is enforced the same way a statement's borrow is - both ends
//! count against `open_statements`, so `sqlite3_close` refuses while a backup is
//! running. SQLite's own message for that case names backups alongside
//! statements, which is the tell that it does the same thing.

use std::os::raw::{c_char, c_int};

use crate::codes::{SQLITE_DONE, SQLITE_MISUSE, SQLITE_OK};
use crate::handle::{c_str, connection, sqlite3};

/// The C handle for a running backup.
#[allow(non_camel_case_types)]
pub struct sqlite3_backup {
    /// The source connection, counted against while this is open.
    source: *mut sqlite3,
    /// The destination connection, likewise.
    destination: *mut sqlite3,
    /// The backup itself, its borrows erased. See the module comment.
    inner: Option<inillucent_legacy::Backup<'static>>,
    /// What the last step reported, for `remaining` and `pagecount`.
    progress: inillucent_legacy::BackupProgress,
}

/// Begins a backup from one connection's database into another's.
///
/// # Safety
///
/// Both handles must be open, and the two name pointers null or NUL-terminated.
/// The result must be released with [`sqlite3_backup_finish`] before either
/// connection is closed.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_init(
    destination: *mut sqlite3,
    destination_name: *const c_char,
    source: *mut sqlite3,
    source_name: *const c_char,
) -> *mut sqlite3_backup {
    let (Some(to), Some(from)) = (destination.as_mut(), source.as_mut()) else {
        return std::ptr::null_mut();
    };
    let to_index = match index_of(to, c_str(destination_name).unwrap_or(b"main")) {
        Some(index) => index,
        None => {
            to.last
                .refuse(crate::codes::SQLITE_ERROR, "unknown database");
            return std::ptr::null_mut();
        }
    };
    let from_index = match index_of(from, c_str(source_name).unwrap_or(b"main")) {
        Some(index) => index,
        None => {
            to.last
                .refuse(crate::codes::SQLITE_ERROR, "unknown database");
            return std::ptr::null_mut();
        }
    };
    // Both borrows are erased, and both are held open by the counts raised
    // below. See the module comment.
    let source_connection: &'static inillucent_legacy::Connection =
        std::mem::transmute(&*from.connection);
    let destination_connection: &'static inillucent_legacy::Connection =
        std::mem::transmute(&*to.connection);
    match source_connection.backup_begin(from_index, destination_connection, to_index) {
        Err(error) => {
            to.fail(&error);
            std::ptr::null_mut()
        }
        Ok(backup) => {
            let progress = backup.progress();
            from.open_statements = from.open_statements.saturating_add(1);
            to.open_statements = to.open_statements.saturating_add(1);
            Box::into_raw(Box::new(sqlite3_backup {
                source,
                destination,
                inner: Some(backup),
                progress,
            }))
        }
    }
}

/// Returns the position of a named database on a connection.
///
/// # Safety
///
/// The handle must be open.
unsafe fn index_of(database: &mut sqlite3, name: &[u8]) -> Option<usize> {
    let wanted = name.to_ascii_lowercase();
    let rows = database.connection.query("PRAGMA database_list").ok()?;
    rows.iter().position(|row| {
        row.get(1)
            .and_then(inillucent_legacy::Value::as_text)
            .is_some_and(|text| text.raw().eq_ignore_ascii_case(&wanted))
    })
}

/// Copies up to `pages` pages, or all of them when `pages` is negative.
///
/// # Safety
///
/// The handle must be one [`sqlite3_backup_init`] returned and not finished.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_step(handle: *mut sqlite3_backup, pages: c_int) -> c_int {
    let Some(held) = handle.as_mut() else {
        return SQLITE_MISUSE;
    };
    let Some(backup) = held.inner.as_mut() else {
        return SQLITE_MISUSE;
    };
    match backup.step(pages) {
        Ok(progress) => {
            held.progress = progress;
            if progress.is_complete() {
                SQLITE_DONE
            } else {
                SQLITE_OK
            }
        }
        Err(error) => match connection(held.destination) {
            Some(database) => database.fail(&error),
            None => error.code().value(),
        },
    }
}

/// Finishes a backup, releasing both connections.
///
/// # Safety
///
/// The handle must be one [`sqlite3_backup_init`] returned, finished once.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_finish(handle: *mut sqlite3_backup) -> c_int {
    if handle.is_null() {
        return SQLITE_OK;
    }
    let mut held = Box::from_raw(handle);
    let code = match held.inner.take() {
        Some(backup) => match backup.finish() {
            Ok(()) => SQLITE_OK,
            Err(error) => error.code().value(),
        },
        None => SQLITE_OK,
    };
    for end in [held.source, held.destination] {
        let Some(database) = connection(end) else {
            continue;
        };
        database.open_statements = database.open_statements.saturating_sub(1);
        if database.zombie && database.open_statements == 0 {
            drop(Box::from_raw(end));
        }
    }
    code
}

/// Returns how many pages are still to copy.
///
/// # Safety
///
/// The handle must be one [`sqlite3_backup_init`] returned and not finished.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_remaining(handle: *mut sqlite3_backup) -> c_int {
    match handle.as_ref() {
        Some(held) => held.progress.remaining as c_int,
        None => 0,
    }
}

/// Returns how many pages the source database has.
///
/// # Safety
///
/// As [`sqlite3_backup_remaining`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_pagecount(handle: *mut sqlite3_backup) -> c_int {
    match handle.as_ref() {
        Some(held) => held.progress.page_count as c_int,
        None => 0,
    }
}
