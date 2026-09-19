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

/// The status a call that broke this API's own contract reports.
///
/// **The name the C header uses for `INILLUCENT_INVALID_STATE` when the
/// contract that was broken is a handle's (task-1979, D1).** A freed handle, a
/// double free and a bind index past the statement's parameter count all answer
/// this rather than doing something undefined, which is the role SQLite gives
/// `SQLITE_MISUSE`. It is the same number as `INILLUCENT_INVALID_STATE` and not
/// a fourteenth code, because it is the same answer: the call was wrong.
pub const INILLUCENT_MISUSE: i32 = INILLUCENT_INVALID_STATE;

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

/// A handle's liveness word.
///
/// **The word a freed handle no longer carries (task-1979, D1).** Freeing an
/// `inillucent_db`, `inillucent_conn` or `inillucent_error` twice used to
/// corrupt the heap and end the process, and using a freed handle was
/// undefined behaviour with no check at all: sometimes a silently wrong value -
/// an empty path, a query that returned OK against a freed connection -
/// sometimes an abort that the panic guard cannot intercept, because an abort
/// is not an unwind.
///
/// Every handle now begins with one of these, set to its own type's magic when
/// the handle is made and cleared to zero immediately before the box is
/// dropped. Every entry point reads it and answers `INILLUCENT_MISUSE` for a
/// handle that does not carry the right word.
///
/// **It is a check, not a guarantee, and SQLite's is the same.** Once the
/// allocation is freed the bytes belong to the allocator, and a later
/// allocation may happen to put the magic back. What it does catch is the two
/// cases that actually happen: a double free, where the memory is still
/// untouched, and a handle used after it was freed in the same breath. The
/// alternative is undefined behaviour with no diagnosis at all.
pub(crate) struct Live(std::cell::Cell<u32>);

impl Live {
    /// Returns a liveness word carrying a type's magic.
    ///
    /// @param magic - the type's own word
    pub(crate) fn new(magic: u32) -> Live {
        Live(std::cell::Cell::new(magic))
    }

    /// Returns whether the word is still the one this type was made with.
    ///
    /// @param magic - the type's own word
    pub(crate) fn is(&self, magic: u32) -> bool {
        self.0.get() == magic
    }

    /// Clears the word, which is what a free does before it drops the box.
    pub(crate) fn clear(&self) {
        self.0.set(0);
    }
}

/// A type C holds a pointer to.
pub(crate) trait Handle {
    /// The word this type's handles carry while they are alive.
    const MAGIC: u32;
    /// Returns the handle's own liveness word.
    fn live(&self) -> &Live;
}

/// Borrows a live handle, or answers `None` for a null or freed pointer.
///
/// @param handle - the caller's pointer
///
/// # Safety
///
/// `handle` must be null or a pointer this library returned. A pointer the
/// caller has freed is answered `None` rather than dereferenced further; see
/// [`Live`] for what that promise is worth and why it is still worth making.
pub(crate) unsafe fn held<'a, T: Handle>(handle: *const T) -> Option<&'a T> {
    let held = handle.as_ref()?;
    held.live().is(T::MAGIC).then_some(held)
}

/// Clears a handle's liveness word and hands back the box to drop.
///
/// The order matters: the word is cleared while the allocation is still ours,
/// so a second free of the same pointer reads a zero rather than a magic and is
/// refused by [`held`] before it reaches `Box::from_raw` a second time.
///
/// @param handle - the pointer the caller passed
///
/// # Safety
///
/// `handle` must be a live pointer this library returned.
pub(crate) unsafe fn reclaim<T: Handle>(handle: *mut T) -> Box<T> {
    let held = Box::from_raw(handle);
    held.live().clear();
    held
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
    // **The handle is checked before it is dereferenced (task-1979, D1).**
    // This took `as_mut()` on whatever the caller passed, so binding to a freed
    // statement wrote into freed memory and reported `INILLUCENT_OK`.
    if held(stmt as *const inillucent_stmt).is_none() {
        return INILLUCENT_MISUSE;
    }
    let Some(statement) = stmt.as_mut() else {
        return INILLUCENT_MISUSE;
    };
    let Some(at) = (index as usize).checked_sub(1) else {
        return INILLUCENT_MISUSE;
    };
    // **And the index against the statement's own parameter count (task-1979,
    // D3).** There was no upper bound at all, and the `resize` below grew the
    // list to whatever number arrived: an index near `u32::MAX` asked the
    // allocator for about 137 GB and stalled the process for tens of seconds.
    // A statement that declares three parameters cannot be bound at four,
    // which is what SQLite answers `SQLITE_RANGE` for.
    if index > statement.declared {
        return INILLUCENT_MISUSE;
    }
    if at >= statement.params.len() {
        statement.params.resize(at.saturating_add(1), Value::Null);
    }
    match statement.params.get_mut(at) {
        Some(slot) => {
            *slot = value;
            INILLUCENT_OK
        }
        None => INILLUCENT_MISUSE,
    }
}

// —— a result ————————————————————————————————————————————————————————

// —— a transaction ——————————————————————————————————————————————————

// —— a failure ——————————————————————————————————————————————————————
