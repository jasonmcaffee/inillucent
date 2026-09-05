//! The SQLite C ABI: the same symbols, the same structs, the same ownership.
//!
//! Invariant: this crate is an adapter and nothing else. Every behaviour it
//! exposes is implemented by an inner crate, so a function here may marshal
//! pointers, cache a C string, or decide who frees what - and may never decide
//! what a statement *means*. If a C caller sees a different answer than the
//! Rust facade would give, that is a bug in this crate, not a feature of it.
//!
//! # What "the same ABI" has to mean
//!
//! The probes in `tests/` compile against the *official* `sqlite3.h` and link
//! against this library. That is the whole claim and it is worth stating
//! plainly: nothing here may declare its own idea of a constant, a struct
//! layout, or a calling convention, because the header a caller compiles
//! against is not ours. Every constant is the one in the header, every struct
//! that crosses the boundary is `#[repr(C)]` with the header's field order, and
//! every entry point is `extern "C"` with the header's signature.
//!
//! # Ownership, which is the part that actually breaks
//!
//! Three rules, and they are the ones SQLite documents:
//!
//! - **A pointer this library returns is owned by this library** unless the
//!   function says otherwise. `sqlite3_column_text` hands back a pointer into
//!   the statement, valid until the next `sqlite3_step`, `sqlite3_reset` or
//!   `sqlite3_finalize` on that statement. `sqlite3_errmsg` is valid until the
//!   next call that touches the connection.
//! - **A pointer this library allocates for the caller to free** comes from
//!   `sqlite3_malloc` and must go back to `sqlite3_free`: `sqlite3_mprintf`,
//!   `sqlite3_serialize` without the no-copy flag, and `sqlite3_expanded_sql`.
//! - **A pointer the caller hands in** is copied unless the caller passes
//!   `SQLITE_STATIC`, in which case the caller promises it outlives the
//!   statement, or a destructor, which this library calls exactly once.
//!
//! # Unsafe
//!
//! Every entry point takes raw pointers a C caller owns, so `unsafe` here is
//! not an exception to the engine's rule - it is the boundary the rule exists
//! to keep everything else on the safe side of. The safety argument is the one
//! above, made once, and each entry point documents what it does with the
//! pointers it is given rather than repeating it.

#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod backup;
pub mod bind;
pub mod blob;
pub mod codes;
pub mod column;
pub mod function;
pub mod handle;
pub mod hooks;
pub mod memory;
pub mod open;
pub mod serialize;
pub mod stmt;
pub mod value;
pub mod vfs;

pub use handle::{sqlite3, sqlite3_blob, sqlite3_stmt};
pub use value::{sqlite3_context, sqlite3_value};

/// The implementation phase that fills this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 12: C ABI and CLI completion";
