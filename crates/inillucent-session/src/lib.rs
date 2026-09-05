//! Connection-local state, statement lifecycle, hooks, PRAGMA state, and
//! interrupt handling.
//!
//! Invariant: a statement holds the catalog snapshot it was compiled against
//! for as long as it is running, and checks before its first step that the
//! snapshot is still the connection's. A schema that moved underneath a
//! prepared statement is therefore either recompiled or reported, never
//! silently executed against a table that no longer has the shape the program
//! assumes.
//!
//! The read transaction is owned here rather than by the statement: the first
//! statement to step takes it and the last to finish releases it, which is what
//! makes two statements stepped alternately see the same snapshot of the file.
//!
//! Module map:
//!
//! - [`connection`] - the connection, its catalog, and the read transaction;
//! - [`statement`] - prepare, bind, step, reset, and finalise;
//! - [`backup`] - copying one database into another a few pages at a time;
//! - [`blob`] - a handle on one value, read and written a range at a time;
//! - [`serialize`] - a database as a byte string, and back.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// Tests assert on exact values and are allowed to fail loudly; the bans above
// exist to keep panics out of paths that run user statements.
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
pub mod blob;
pub mod connection;
pub mod execute;
pub mod pragma;
pub mod pragma_vtab;
pub mod serialize;
pub mod settings;
pub mod statement;
pub mod vtab;

pub use backup::{Backup, BackupProgress};
pub use blob::Blob;
pub use connection::{
    Access, CommitHook, Connection, Hooks, OpenOptions, Outcome, RollbackHook, SessionDatabase,
    UpdateHook,
};
// The kind an update hook is told about is part of that hook's signature, so a
// caller that can name the hook has to be able to name this too.
pub use inillucent_sql::plan::Levers;
pub use inillucent_vm::program::RowChangeKind;
pub use serialize::{serialize, Deserialized};
// The file-system contract, so a caller of the facade can supply one. It is
// re-exported rather than duplicated: a second copy of the trait would be a
// second thing to keep in step with the pager.
pub use inillucent_vfs as vfs;
// The registries an application registers into, for the same reason.
pub use inillucent_ext::registry as extensions;
pub use inillucent_value::collation;
pub use statement::{ColumnMetadata, Statement};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str =
    "phase 6: catalog, binder, expression VM, and read-only SELECT";
