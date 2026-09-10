//! `sqlite3_serialize` and `sqlite3_deserialize`: a database as bytes.
//!
//! Invariant: what comes out is the file. Not a dump, not a re-encoding - the
//! same bytes a reader would find on disk, so that serializing here and writing
//! the result to a path produces a database the pinned SQLite opens. That claim
//! is what makes the pair worth having, and it belongs to `inillucent_session`;
//! this module only decides who owns the buffer afterwards.
//!
//! Ownership is the whole of the C-side difference. `sqlite3_serialize` returns
//! a block the caller frees with `sqlite3_free`, and `sqlite3_deserialize` takes
//! one whose ownership depends on a flag. Getting either wrong is a leak or a
//! double free, so both are stated in the entry points below.

use std::os::raw::{c_char, c_int};

use crate::codes::{SQLITE_ERROR, SQLITE_OK, SQLITE_SERIALIZE_NOCOPY};
use crate::handle::{c_str, connection, misuse, sqlite3};

/// Returns the bytes of a database, for the caller to free.
///
/// `SQLITE_SERIALIZE_NOCOPY` asks for a pointer into the engine's own memory
/// rather than a copy. This engine keeps its pages in a cache rather than one
/// contiguous image, so there is nothing to point at and the flag returns null -
/// which is what SQLite itself does for a database that is not memory-backed,
/// and what every caller of that flag is already required to handle.
///
/// # Safety
///
/// The handle must be open and `name` null or NUL-terminated. `size_out`, when
/// not null, receives the length. The result must be released with
/// `sqlite3_free` unless it is null.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_serialize(
    handle: *mut sqlite3,
    name: *const c_char,
    size_out: *mut i64,
    flags: u32,
) -> *mut u8 {
    if !size_out.is_null() {
        *size_out = 0;
    }
    let Some(database) = connection(handle) else {
        return std::ptr::null_mut();
    };
    if flags & SQLITE_SERIALIZE_NOCOPY != 0 {
        return std::ptr::null_mut();
    }
    let wanted = c_str(name).unwrap_or(b"main");
    if !wanted.eq_ignore_ascii_case(b"main") {
        // The engine serializes the main database; an attached one would need
        // its own pager walk, and returning null is how SQLite reports a name
        // it cannot serialize rather than silently giving back the wrong file.
        database
            .last
            .refuse(SQLITE_ERROR, "only the main database can be serialized");
        return std::ptr::null_mut();
    }
    match database.connection.serialize() {
        Err(error) => {
            database.fail(&error);
            std::ptr::null_mut()
        }
        Ok(bytes) => {
            if !size_out.is_null() {
                *size_out = bytes.len() as i64;
            }
            database.succeed();
            crate::memory::owned_bytes(&bytes)
        }
    }
}

/// Replaces a connection's database with the bytes of one.
///
/// The bytes are copied, and `SQLITE_DESERIALIZE_FREEONCLOSE` is honoured by
/// freeing the caller's block *here* rather than at close: the copy means there
/// is nothing left to hold, and freeing early is what the flag asks for as far
/// as the caller can observe. `SQLITE_DESERIALIZE_RESIZEABLE` is accepted and
/// has no effect, because the copy is already the engine's to grow.
///
/// # Safety
///
/// The handle must be open. `bytes` must point at `size` readable bytes, and
/// must be one `sqlite3_malloc` returned if `SQLITE_DESERIALIZE_FREEONCLOSE` is
/// set. Every statement on the connection must already be finalized.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_deserialize(
    handle: *mut sqlite3,
    name: *const c_char,
    bytes: *mut u8,
    size: i64,
    _capacity: i64,
    flags: u32,
) -> c_int {
    let Some(database) = connection(handle) else {
        return misuse();
    };
    let wanted = c_str(name).unwrap_or(b"main");
    if !wanted.eq_ignore_ascii_case(b"main") {
        return database
            .last
            .refuse(SQLITE_ERROR, "only the main database can be deserialized");
    }
    if database.open_statements > 0 {
        return database.last.refuse(
            crate::codes::SQLITE_BUSY,
            "unable to deserialize due to unfinalized statements",
        );
    }
    if bytes.is_null() || size < 0 {
        return database.last.refuse(SQLITE_ERROR, "no image was given");
    }
    let image = std::slice::from_raw_parts(bytes, size as usize).to_vec();
    let options = inillucent_legacy::ConnectionOptions {
        writable: flags & crate::codes::SQLITE_DESERIALIZE_READONLY == 0,
        ..inillucent_legacy::ConnectionOptions::default()
    };
    let outcome = inillucent_legacy::Database::deserialize_with(&image, options)
        .and_then(|opened| opened.connect().map(|connected| (opened, connected)));
    if flags & crate::codes::SQLITE_DESERIALIZE_FREEONCLOSE != 0 {
        crate::memory::sqlite3_free(bytes.cast());
    }
    match outcome {
        Err(error) => database.fail(&error),
        Ok((opened, connected)) => {
            // The order matters: the connection borrows nothing from the old
            // pair, so replacing both at once is safe, and there are no
            // statements left to observe the swap because of the check above.
            *database.connection = connected;
            *database.database = opened;
            database.hooks = crate::hooks::Hooks::default();
            database.succeed();
            SQLITE_OK
        }
    }
}
