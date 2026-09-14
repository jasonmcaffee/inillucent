//! Schema objects, the schema loader, cookies, invalidation, and statistics.
//!
//! Invariant: a catalog is a *snapshot*. It is built inside one read
//! transaction, from one consistent set of pages, and never changes afterwards;
//! a schema change produces a new snapshot with a new generation rather than
//! mutating the one prepared statements are holding. That is what makes
//! invalidation a comparison of two numbers.
//!
//! The catalog is where `sqlite_schema`'s stored CREATE text becomes something
//! the binder can resolve names against. It parses that text with the
//! first-party parser — the same one that parsed the user's statement, so a
//! schema SQLite wrote and a statement the user typed are read by one grammar —
//! and derives affinity, collation, the rowid alias and the primary key from
//! the declaration rather than from any side table.
//!
//! Module map:
//!
//! - [`load`] - reading `sqlite_schema` and building the snapshot;
//! - [`ddl`] - writing `sqlite_schema`, root pages, and the cookie;
//! - [`snapshot`] - the snapshot itself and the view the binder sees;
//! - [`paged`] - the same schema as a tree in the new engine's own file, which
//!   is what makes a database written by Phase 2 self-describing.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// Tests assert on exact values and are allowed to fail loudly; the bans above
// exist to keep panics and wrapping out of paths that read persistent bytes.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod analyze;
pub mod ddl;
pub mod load;
pub mod paged;
pub mod rename;
pub mod snapshot;

pub use ddl::{
    allocate_index_root, allocate_table_root, automatic_index_name, bump_schema_cookie,
    canonical_sql, delete_schema_rows, free_root, insert_schema_row, SchemaRow,
};
pub use load::{load_database_catalog, table_from_create_sql};
pub use snapshot::{CatalogSnapshot, DatabaseCatalog};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str =
    "phase 6: catalog, binder, expression VM, and read-only SELECT";
