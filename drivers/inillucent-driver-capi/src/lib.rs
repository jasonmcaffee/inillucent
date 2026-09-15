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

use std::ffi::{c_char, CStr, CString};

use inillucent_driver::capability::{Support, CAPABILITIES};
use inillucent_driver::Value;

// —— the entry points, by the handle each is about ————————————————————
//
// **Four modules rather than one 1,852 line file (task-1962, A8).** Fifty
// `extern "C"` functions in one file is a file nobody reads and everybody
// appends to. Each module is re-exported here, so every Rust path is what it
// was, and the C symbols never depended on the module: `#[no_mangle]` exports
// from wherever the function is written.
mod capi;
pub use capi::db::*;
pub use capi::error::*;
pub use capi::stmt::*;
pub use capi::value::*;

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

// —— the plumbing ——————————————————————————————————————————————————

/// Reads a caller's C string as UTF-8.
///
/// @param text - the pointer the caller passed
///
/// # Safety
///
/// `text` must be null or a NUL-terminated string the caller owns for the
/// duration of the call.
pub(crate) unsafe fn borrowed(text: *const c_char) -> Option<&'static str> {
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
pub(crate) unsafe fn held<'a, T>(handle: *const T) -> Option<&'a T> {
    handle.as_ref()
}

// —— the library ————————————————————————————————————————————————————

/// Returns the ABI version a binding should check the major of.
#[no_mangle]
pub extern "C" fn inillucent_abi_version() -> u32 {
    guarded_value(|| ABI_VERSION, 0)
}

/// Returns what the driver calls itself, as a C string valid forever.
#[no_mangle]
pub extern "C" fn inillucent_version() -> *const c_char {
    guarded_value(
        || {
            // Built once and leaked on purpose: the header promises a pointer valid
            // for the life of the process, and one static allocation is the honest way
            // to keep that promise.
            static ONCE: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
            ONCE.get_or_init(|| c_string(inillucent_driver::version()))
                .as_ptr()
        },
        std::ptr::null(),
    )
}

/// Returns how many capabilities this build declares.
#[no_mangle]
pub extern "C" fn inillucent_capability_count() -> usize {
    guarded_value(|| CAPABILITIES.len(), 0)
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
    guarded_value(
        || {
            let Some(name) = borrowed(name) else {
                return INILLUCENT_SUPPORT_UNKNOWN;
            };
            match inillucent_driver::supports(name) {
                Some(Support::Yes) => Support::Yes as i32,
                Some(Support::No) => Support::No as i32,
                Some(Support::Partial) => Support::Partial as i32,
                None => INILLUCENT_SUPPORT_UNKNOWN,
            }
        },
        0,
    )
}

/// Returns a C string for a static Rust string, made once and kept.
///
/// The capability table's names and notes are `&'static str` and are therefore
/// not NUL-terminated. Converting them on every call would hand back a pointer
/// into something freed on return, so each is converted once and kept for the
/// life of the process - which is what the header promises of them.
///
/// @param text - the static text
pub(crate) fn interned(text: &'static str) -> &'static CString {
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

// —— a connection ——————————————————————————————————————————————————

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
pub(crate) unsafe fn session_of(conn: *const inillucent_conn) -> u64 {
    held(conn).map_or(0, |connection| connection.state.session)
}

/// Returns the database a connection belongs to.
///
/// @param conn - the connection
///
/// # Safety
///
/// `conn` must be null or a live handle.
pub(crate) unsafe fn database_of<'a>(conn: *const inillucent_conn) -> Option<&'a inillucent_db> {
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
pub(crate) unsafe fn database_in<'a>(state: &ConnState) -> Option<&'a inillucent_db> {
    held(state.database)
}

// —— a statement ————————————————————————————————————————————————————

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
pub(crate) unsafe fn bind(stmt: *mut inillucent_stmt, index: u32, value: Value) -> i32 {
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

// —— a result ————————————————————————————————————————————————————————

// —— a transaction ——————————————————————————————————————————————————

// —— a failure ——————————————————————————————————————————————————————
