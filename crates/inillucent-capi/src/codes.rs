//! The constants, taken from the header a caller compiles against.
//!
//! Invariant: every value here is the value in `sqlite3.h`, and none of them is
//! computed. A constant this library invented would compile, link, and then
//! disagree with the caller's header at run time - which is the one failure the
//! ABI probes exist to catch, and the one that is invisible in a code review.
//!
//! The result codes are *not* here: they are generated from
//! `compat/errors.toml` into `inillucent_base`, which is where the engine already
//! keeps them, and this module re-exports what the boundary needs.

use inillucent_legacy::{DbError, ExtendedCode, PrimaryCode};

/// `SQLITE_OK`.
pub const SQLITE_OK: i32 = 0;
/// `SQLITE_ERROR`.
pub const SQLITE_ERROR: i32 = 1;
/// `SQLITE_NOMEM`.
pub const SQLITE_NOMEM: i32 = 7;
/// `SQLITE_READONLY`.
pub const SQLITE_READONLY: i32 = 8;
/// `SQLITE_INTERRUPT`.
pub const SQLITE_INTERRUPT: i32 = 9;
/// `SQLITE_NOTFOUND`.
pub const SQLITE_NOTFOUND: i32 = 12;
/// `SQLITE_TOOBIG`.
pub const SQLITE_TOOBIG: i32 = 18;
/// `SQLITE_MISUSE`.
pub const SQLITE_MISUSE: i32 = 21;
/// `SQLITE_RANGE`.
pub const SQLITE_RANGE: i32 = 25;
/// `SQLITE_ROW`.
pub const SQLITE_ROW: i32 = 100;
/// `SQLITE_DONE`.
pub const SQLITE_DONE: i32 = 101;
/// `SQLITE_BUSY`.
pub const SQLITE_BUSY: i32 = 5;
/// `SQLITE_ABORT`.
pub const SQLITE_ABORT: i32 = 4;
/// `SQLITE_CANTOPEN`.
pub const SQLITE_CANTOPEN: i32 = 14;

/// `SQLITE_INTEGER`, the storage class of a column or value.
pub const SQLITE_INTEGER: i32 = 1;
/// `SQLITE_FLOAT`.
pub const SQLITE_FLOAT: i32 = 2;
/// `SQLITE_TEXT`.
pub const SQLITE_TEXT: i32 = 3;
/// `SQLITE_BLOB`.
pub const SQLITE_BLOB: i32 = 4;
/// `SQLITE_NULL`.
pub const SQLITE_NULL: i32 = 5;

/// `SQLITE_UTF8`.
pub const SQLITE_UTF8: i32 = 1;
/// `SQLITE_UTF16LE`.
pub const SQLITE_UTF16LE: i32 = 2;
/// `SQLITE_UTF16BE`.
pub const SQLITE_UTF16BE: i32 = 3;
/// `SQLITE_UTF16`, meaning the machine's own byte order.
pub const SQLITE_UTF16: i32 = 4;
/// `SQLITE_DETERMINISTIC`, a flag on a function's encoding argument.
pub const SQLITE_DETERMINISTIC: i32 = 0x0000_0800;
/// `SQLITE_DIRECTONLY`.
pub const SQLITE_DIRECTONLY: i32 = 0x0008_0000;
/// `SQLITE_INNOCUOUS`.
pub const SQLITE_INNOCUOUS: i32 = 0x0020_0000;
/// `SQLITE_SUBTYPE`.
pub const SQLITE_SUBTYPE: i32 = 0x0010_0000;

/// `SQLITE_OPEN_READONLY`.
pub const SQLITE_OPEN_READONLY: i32 = 0x0000_0001;
/// `SQLITE_OPEN_READWRITE`.
pub const SQLITE_OPEN_READWRITE: i32 = 0x0000_0002;
/// `SQLITE_OPEN_CREATE`.
pub const SQLITE_OPEN_CREATE: i32 = 0x0000_0004;
/// `SQLITE_OPEN_URI`.
pub const SQLITE_OPEN_URI: i32 = 0x0000_0040;
/// `SQLITE_OPEN_MEMORY`.
pub const SQLITE_OPEN_MEMORY: i32 = 0x0000_0080;
/// `SQLITE_OPEN_NOMUTEX`.
pub const SQLITE_OPEN_NOMUTEX: i32 = 0x0000_8000;
/// `SQLITE_OPEN_FULLMUTEX`.
pub const SQLITE_OPEN_FULLMUTEX: i32 = 0x0001_0000;

/// `SQLITE_PREPARE_PERSISTENT`.
pub const SQLITE_PREPARE_PERSISTENT: u32 = 0x01;
/// `SQLITE_PREPARE_NO_VTAB`.
pub const SQLITE_PREPARE_NO_VTAB: u32 = 0x04;

/// `SQLITE_INSERT`, as reported to an update hook.
pub const SQLITE_INSERT: i32 = 18;
/// `SQLITE_DELETE`.
pub const SQLITE_DELETE: i32 = 9;
/// `SQLITE_UPDATE`.
pub const SQLITE_UPDATE: i32 = 23;

/// `SQLITE_DENY`, an authorizer's answer.
pub const SQLITE_DENY: i32 = 1;
/// `SQLITE_IGNORE`.
pub const SQLITE_IGNORE: i32 = 2;

/// `SQLITE_TRACE_STMT`.
pub const SQLITE_TRACE_STMT: u32 = 0x01;
/// `SQLITE_TRACE_PROFILE`.
pub const SQLITE_TRACE_PROFILE: u32 = 0x02;
/// `SQLITE_TRACE_ROW`.
pub const SQLITE_TRACE_ROW: u32 = 0x04;
/// `SQLITE_TRACE_CLOSE`.
pub const SQLITE_TRACE_CLOSE: u32 = 0x08;

/// `SQLITE_SERIALIZE_NOCOPY`.
pub const SQLITE_SERIALIZE_NOCOPY: u32 = 0x001;
/// `SQLITE_DESERIALIZE_FREEONCLOSE`.
pub const SQLITE_DESERIALIZE_FREEONCLOSE: u32 = 1;
/// `SQLITE_DESERIALIZE_RESIZEABLE`.
pub const SQLITE_DESERIALIZE_RESIZEABLE: u32 = 2;
/// `SQLITE_DESERIALIZE_READONLY`.
pub const SQLITE_DESERIALIZE_READONLY: u32 = 4;

/// The version this library reports, matching the pinned reference.
pub const SQLITE_VERSION: &str = "3.53.4";
/// The numeric form of [`SQLITE_VERSION`]: major\*1000000 + minor\*1000 + patch.
pub const SQLITE_VERSION_NUMBER: i32 = 3_053_004;
/// What `sqlite3_sourceid` reports.
///
/// SQLite reports the check-in that built it. This engine is not that build, so
/// it says whose ABI it implements and which engine implements it, rather than
/// forging a check-in identifier that would send a reader to the wrong tree.
pub const SQLITE_SOURCE_ID: &str = "inillucent (SQLite 3.53.4 ABI)";

/// Returns the numeric code a `DbError` reports to C, extended bits included.
pub fn extended_of(error: &DbError) -> i32 {
    error.extended().value()
}

/// Returns the primary code a `DbError` reports to C.
pub fn primary_of(error: &DbError) -> i32 {
    error.code().value()
}

/// Returns the message SQLite prints for a numeric code.
///
/// `sqlite3_errstr` answers for a code nobody has seen before as well, which is
/// why this falls back rather than refusing: a caller passing an extended code
/// this build does not know should still get a sentence.
pub fn message_for(code: i32) -> &'static str {
    if let Some(row) = ExtendedCode(code).row() {
        return row.message;
    }
    match PrimaryCode::from_value(code & 0xff) {
        Some(primary) => primary.message(),
        None => "unknown error",
    }
}
