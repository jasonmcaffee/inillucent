//! The opaque handles a C caller holds, and what keeps them sound.
//!
//! Invariant: a handle is a `Box` this library allocated and a C caller only
//! ever sees the pointer. The Rust objects inside are ordered - a `Database`
//! outlives its `Connection`, which outlives its `Statement`s - and the C API
//! has no way to express that, so the ordering is enforced here instead: a
//! connection counts its open statements, `sqlite3_close` refuses while any are
//! open, and `sqlite3_close_v2` waits for the last one. Those are SQLite's own
//! documented rules, and they are exactly what makes the borrow this file
//! erases safe to erase.
//!
//! # The one lifetime that is erased, and why
//!
//! `inillucent_legacy::Statement<'connection>` borrows the connection that prepared it. C
//! has no lifetimes, so a `sqlite3_stmt*` cannot carry one. The statement's
//! borrow is therefore transmuted to `'static` when it goes into the handle,
//! and the connection is kept behind a `Box` that is never moved and never
//! dropped while `open_statements` is non-zero. That count is the proof: as
//! long as it is honoured, no statement can observe a freed connection, and
//! every path that could free one checks it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

use inillucent_legacy::{Connection, Database, DbError, Statement, Value};

use crate::codes::{SQLITE_MISUSE, SQLITE_OK};

/// The C handle for an open database connection.
///
/// Named for the header rather than for Rust: a caller writes `sqlite3 *db`,
/// and a differently-spelled type would be a differently-spelled symbol in the
/// generated documentation an integrator reads.
#[allow(non_camel_case_types)]
pub struct sqlite3 {
    /// The database file, boxed so nothing moves while statements borrow it.
    pub(crate) database: Box<Database>,
    /// The connection, boxed for the same reason.
    pub(crate) connection: Box<Connection>,
    /// How many statements are prepared and not yet finalized.
    pub(crate) open_statements: usize,
    /// Whether `sqlite3_close_v2` has been called and is waiting.
    pub(crate) zombie: bool,
    /// The last error, kept because `sqlite3_errmsg` is called after the fact.
    pub(crate) last: ErrorSlot,
    /// The registered callbacks.
    pub(crate) hooks: crate::hooks::Hooks,
    /// The functions and collations a caller registered.
    pub(crate) registered: RefCell<HashMap<String, crate::function::Registration>>,
    /// Whether `sqlite3_load_extension` is allowed on this connection.
    pub(crate) extensions_enabled: bool,
    /// Strings handed to the caller that live as long as the connection.
    ///
    /// `sqlite3_table_column_metadata` promises exactly that lifetime, and it
    /// is a bounded set: one entry per distinct answer a caller asks for.
    pub(crate) remembered: Vec<Vec<u8>>,
}

/// The last error a connection or statement reported.
///
/// The message has to outlive the call that produced it, because `errmsg` is
/// what a caller reaches for *after* something failed. Keeping the `CString`
/// here is what makes the returned pointer valid until the next call, which is
/// the guarantee the header documents.
pub struct ErrorSlot {
    /// The numeric code, extended bits included.
    pub(crate) extended: c_int,
    /// The message, held so its pointer stays valid.
    pub(crate) message: CString,
}

impl Default for ErrorSlot {
    fn default() -> ErrorSlot {
        ErrorSlot {
            extended: SQLITE_OK,
            message: CString::default(),
        }
    }
}

impl ErrorSlot {
    /// Records a failure and returns the primary code to hand back.
    pub fn fail(&mut self, error: &DbError) -> c_int {
        self.extended = error.extended().value();
        self.message = CString::new(error.message()).unwrap_or_default();
        error.code().value()
    }

    /// Records success.
    pub fn succeed(&mut self) {
        self.extended = SQLITE_OK;
        self.message = CString::default();
    }

    /// Records a failure described by a code and a message of our own.
    pub fn refuse(&mut self, code: c_int, message: &str) -> c_int {
        self.extended = code;
        self.message = CString::new(message).unwrap_or_default();
        code
    }
}

impl sqlite3 {
    /// Keeps a NUL-terminated copy of some bytes and returns a pointer to it.
    pub(crate) fn remember(&mut self, bytes: &[u8]) -> *const c_char {
        if let Some(found) = self
            .remembered
            .iter()
            .find(|held| held.len() == bytes.len() + 1 && held.get(..bytes.len()) == Some(bytes))
        {
            return found.as_ptr().cast();
        }
        let mut owned = bytes.to_vec();
        owned.push(0);
        self.remembered.push(owned);
        match self.remembered.last() {
            Some(last) => last.as_ptr().cast(),
            None => std::ptr::null(),
        }
    }

    /// Records a failure on the connection and returns the code.
    pub(crate) fn fail(&mut self, error: &DbError) -> c_int {
        self.last.fail(error)
    }

    /// Records success on the connection.
    pub(crate) fn succeed(&mut self) -> c_int {
        self.last.succeed();
        SQLITE_OK
    }
}

/// The C handle for a prepared statement.
#[allow(non_camel_case_types)]
pub struct sqlite3_stmt {
    /// The connection that prepared it, so `sqlite3_db_handle` can answer and
    /// so finalizing can decrement the count that keeps the borrow honest.
    pub(crate) owner: *mut sqlite3,
    /// The borrow-erased statement. See the module comment.
    pub(crate) statement: Statement<'static>,
    /// Whether the last step produced a row.
    pub(crate) has_row: bool,
    /// Whether the statement has run to completion.
    pub(crate) done: bool,
    /// Whether the statement has been stepped since the last reset.
    pub(crate) busy: bool,
    /// The row the last step produced, owned so column pointers stay valid.
    pub(crate) row: Vec<Value<'static>>,
    /// C strings handed out for this row, freed on the next step.
    pub(crate) held: Vec<Vec<u8>>,
    /// Value wrappers handed out for this row, freed with the statement.
    ///
    /// **Boxed, and clippy is wrong about it.** `sqlite3_column_value` hands
    /// the caller a pointer into this list and the header promises it stays
    /// valid until the statement is finalised. Holding the values inline would
    /// move every one of them the next time the vector grew, and every pointer
    /// already handed out would dangle - which is a use-after-free in a
    /// language that cannot see it.
    #[allow(clippy::vec_box)]
    pub(crate) values: Vec<Box<crate::value::sqlite3_value>>,
    /// The SQL text, kept so `sqlite3_sql` can return a pointer to it.
    pub(crate) sql: CString,
    /// Parameter values, kept so a re-step uses them again.
    pub(crate) bound: Vec<Value<'static>>,
    /// Destructors the caller attached to bound values, called on rebind.
    pub(crate) destructors: Vec<Option<(Destructor, *mut c_void)>>,
}

/// A destructor a caller attaches to a bound value.
pub type Destructor = unsafe extern "C" fn(*mut c_void);

/// The C handle for an open blob.
#[allow(non_camel_case_types)]
pub struct sqlite3_blob {
    /// The connection it was opened on.
    pub(crate) owner: *mut sqlite3,
    /// The open blob, its borrow erased the same way a statement's is.
    pub(crate) blob: inillucent_legacy::Blob<'static>,
}

/// Turns a pointer a caller handed in into a reference, or reports misuse.
///
/// Every entry point starts with one of these. A null handle is `SQLITE_MISUSE`
/// rather than a crash, because that is what SQLite does and because a library
/// that segfaults on a null pointer is one nobody can debug from the C side.
///
/// # Safety
///
/// The pointer must be one this library returned and not yet freed.
pub(crate) unsafe fn connection<'a>(handle: *mut sqlite3) -> Option<&'a mut sqlite3> {
    handle.as_mut()
}

/// Turns a statement pointer into a reference, or reports misuse.
///
/// # Safety
///
/// The pointer must be one this library returned and not yet finalized.
pub(crate) unsafe fn statement<'a>(handle: *mut sqlite3_stmt) -> Option<&'a mut sqlite3_stmt> {
    handle.as_mut()
}

/// Returns the misuse code, for an entry point handed a null handle.
pub(crate) fn misuse() -> c_int {
    SQLITE_MISUSE
}

/// Reads a NUL-terminated C string as bytes, or returns `None` for null.
///
/// # Safety
///
/// The pointer must be null or point at a NUL-terminated string.
pub(crate) unsafe fn c_str<'a>(text: *const c_char) -> Option<&'a [u8]> {
    if text.is_null() {
        return None;
    }
    Some(std::ffi::CStr::from_ptr(text).to_bytes())
}

/// Reads a counted string the way SQLite's `_text` entry points define it.
///
/// A negative length means "up to the first NUL", which is the convention every
/// `sqlite3_bind_text`-shaped function uses and the one a caller gets wrong
/// most often.
///
/// # Safety
///
/// The pointer must be null, or point at `length` bytes, or - when `length` is
/// negative - at a NUL-terminated string.
pub(crate) unsafe fn counted<'a>(text: *const c_char, length: c_int) -> Option<&'a [u8]> {
    if text.is_null() {
        return None;
    }
    if length < 0 {
        return Some(std::ffi::CStr::from_ptr(text).to_bytes());
    }
    Some(std::slice::from_raw_parts(
        text.cast::<u8>(),
        length as usize,
    ))
}
