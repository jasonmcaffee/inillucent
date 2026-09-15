//! The write path: `INSERT`, `UPDATE` and `DELETE` over the paged trees.
//!
//! Invariant: a statement's reads all happen before any of its writes. Every
//! statement here decides which rows it is going to change, in full, and only
//! then changes them. That is not a shortcut around the push executor - it is
//! SQLite's own "collect the keys, then halt and modify" shape, and it is what
//! makes `UPDATE t SET k = k + 1 WHERE k < 5` terminate rather than meeting its
//! own new rows.
//!
//! It is also what the borrow checker asks for. Mutating a tree needs
//! `&mut PagedTree` *and* `&mut Database` at once, while reading needs a shared
//! borrow of both; a design that wrote as it scanned would have to thread a
//! mutable borrow of the whole file through every operator in [`crate::ops`].
//! Splitting the statement in two - [`keys_query`] under a shared borrow,
//! [`insert`], [`update`] and [`delete`] under a mutable one - keeps the read
//! half *exactly* the read path the gate measures, index selection and all.
//!
//! ## Expressions are the read path's expressions
//!
//! Nothing here evaluates a `BoundExpr`. A statement's `SET` values, its
//! `DEFAULT`s, its `VALUES` rows and its `excluded.*` references are all
//! translated by [`crate::physical`]'s own translator against a synthetic
//! column space and compiled by [`crate::expr::compile`], then run against a
//! one-row batch. A second evaluator here would agree with that one until the
//! first time somebody fixed an affinity rule in one of them - which is exactly
//! the bug the read path's own two-traversal translator had, by twenty-odd node
//! kinds.
//!
//! The synthetic space is what makes `excluded.body` and a trigger's `OLD.x`
//! fall out for free: the binder gives them source numbers no FROM term can
//! have ([`inillucent_sql::bind::EXCLUDED_SOURCE`] and its neighbours), so they
//! are simply further stages of the space, reading further rows of the batch.
//!
//! ## The order one row is written in
//!
//! For every changed row, in this order and for a reason each:
//!
//! 1. **Uniqueness is checked before anything is written**, through a point
//!    probe of the table key and of each unique index. A constraint checked
//!    afterwards is a constraint that has already corrupted the tree it was
//!    protecting.
//! 2. **The old index entries are removed before the new ones are added**, so
//!    an update that leaves an indexed column alone removes and re-adds the
//!    same entry rather than leaving two of it.
//! 3. **The table row goes last.** Either order is recoverable - the whole
//!    statement is one transaction and its records replay together - but doing
//!    the indexes first means a failure part-way leaves an index entry pointing
//!    at a row that is not there, which the integrity checker names, rather
//!    than a row no index can find, which it does not.

mod index;

use inillucent_tree::datum::OwnedDatum;

use crate::declared::IndexExprs;
use crate::physical::SourceLayout;
// **The six modules this file is made of (task-1962, A7).** It was 3,003 lines
// holding the write target, the key search and the four statements, and the
// order they are declared in is the order a reader meets them: where a row
// goes, how the rows to change are found, then each statement. Everything is
// re-exported under the path it had.
mod conflict;
mod delete;
mod insert;
mod keys;
mod target;
mod update;

pub(crate) use conflict::CompiledUpsert;
pub(crate) use conflict::{
    conflicting_row, matching_arm, resolution_for, resolution_for_arm, resolution_of,
    rowid_conflict, unwind_of, upsert_row, Resolution,
};
pub use delete::{delete, delete_at};
pub(crate) use delete::{remove_row, remove_with_triggers};
pub(crate) use insert::{declarations_are_met, outer_unwind, replace_row};
pub use insert::{insert, insert_at};
pub use keys::{keys_query, keys_query_joined, module_keys_query};
pub(crate) use target::{
    count_row, count_view_row, highest_rowid, key_columns, layout_of, missing_tree, read_row,
    row_exists, sources_for, Borrowed, Stored, Upsert, WriteRequest,
};
pub use target::{view_layout, Changes, RowSpace, Trees, WriteTarget};
pub(crate) use update::{difference, same_key, Difference};
pub use update::{update, update_at, update_at_cached, update_cached, UpdateSetup};

/// One row in a tree's own column order.
///
/// A table row is `[rowid] ++ [record slots except the rowid alias]`, which is
/// what [`SourceLayout`] describes; an index entry is the indexed columns
/// followed by the rowid. Both are this type, because both are just a tree's
/// row, and the write path never holds a row in any other shape.
pub type Row = Vec<OwnedDatum>;

/// Where a compiled `UPDATE` keeps its setup between executions.
///
/// `RefCell` because the compiled statement is shared behind an `Rc` and this is
/// the one part of it that fills in later.
pub type UpdateCache = std::cell::RefCell<Option<std::rc::Rc<UpdateSetup>>>;

/// The catalog rows the write path's own tests are written against.
///
/// Invariant: **a fixture here describes a table and nothing else.** It builds
/// a `TableInfo` and an `IndexInfo` with every field at the value a plain
/// `CREATE TABLE` would give it, so a test that cares about one field says so
/// by setting that one field. Nothing here reads a file or a catalog.
#[cfg(test)]
pub(crate) mod testing {
    use inillucent_sql::catalog_view::{
        ColumnInfo, IndexColumnInfo, IndexInfo, IndexOrigin, TableInfo, TableKind,
    };
    use inillucent_value::Affinity;

    /// Returns one column, declared with a type and nothing else.
    ///
    /// @param name - the column's name
    /// @param affinity - the affinity its declared type gives it
    pub(crate) fn a_column(name: &str, affinity: Affinity) -> ColumnInfo {
        ColumnInfo {
            name: name.as_bytes().to_vec(),
            folded: name.to_ascii_lowercase().into_bytes(),
            declared_type: b"INTEGER".to_vec(),
            affinity,
            collation: b"binary".to_vec(),
            not_null: false,
            not_null_conflict: None,
            primary_key_conflict: None,
            default_sql: None,
            primary_key_position: None,
            hidden: false,
            generated: false,
            stored: false,
            generated_sql: None,
        }
    }

    /// Returns a rowid table with the columns named, each with integer
    /// affinity.
    ///
    /// @param name - the table's name
    /// @param columns - the column names, in declaration order
    pub(crate) fn a_table(name: &str, columns: &[&str]) -> TableInfo {
        TableInfo {
            name: name.as_bytes().to_vec(),
            folded: name.to_ascii_lowercase().into_bytes(),
            database: 0,
            root: 2,
            columns: columns
                .iter()
                .map(|held| a_column(held, Affinity::Integer))
                .collect(),
            rowid_alias: None,
            without_rowid: false,
            strict: false,
            autoincrement: false,
            kind: TableKind::Table,
            create_sql: Vec::new(),
            indexes: Vec::new(),
            view: None,
            triggers: Vec::new(),
            analysed_rows: None,
            foreign_key_triggers: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            module: None,
        }
    }

    /// Returns an index over the named columns of a table.
    ///
    /// @param name - the index's name
    /// @param root - its tree, or zero for one with no tree of its own
    /// @param unique - whether it enforces uniqueness
    /// @param columns - which table columns it keys on, in key order
    pub(crate) fn an_index(name: &str, root: u32, unique: bool, columns: &[u16]) -> IndexInfo {
        IndexInfo {
            name: name.as_bytes().to_vec(),
            folded: name.to_ascii_lowercase().into_bytes(),
            root,
            unique,
            columns: columns
                .iter()
                .map(|held| IndexColumnInfo {
                    column: Some(*held),
                    expr_sql: None,
                    collation: b"binary".to_vec(),
                    descending: false,
                    declared_descending: false,
                })
                .collect(),
            partial_sql: None,
            origin: IndexOrigin::Created,
            conflict: None,
            prefix_rows: Vec::new(),
            analysed_rows: None,
            metric: None,
        }
    }
}
