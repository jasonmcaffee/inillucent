//! Re-pointing a connection's tree handles at what the catalog now says.
//!
//! Invariant: **a tree handle is only ever as current as the catalog row it was
//! attached from.** A `PagedTree` holds the root page, the leftmost leaf, the
//! leaf count and the row count it was built with, and none of those is read
//! again while the handle lives. So the two ways the file can move under a
//! connection - another process committing, and this connection closing and
//! opening the file again - both end here, because both leave every handle
//! describing a tree that is no longer the one on the disk.
//!
//! Split out of `lib.rs` in task-1980, which added the first of the two. The
//! module ceiling in `crates/inillucent-compat/tests/policy.rs` is what asked
//! for the split rather than for a larger number, and the two functions belong
//! together: they answer the same question from opposite ends, one rebuilding
//! every handle from the catalog and one re-pointing the handles that are
//! already there.

use std::collections::HashMap;

use inillucent_base::DbResult;
use inillucent_pool::{Database, PageId};
use inillucent_tree::PagedTree;
use inillucent_vfs::DbPath;
use inillucent_wal::{Wal, WalOptions, FIRST_LSN};

use crate::{
    attach_catalog, let_the_pool_ask_the_log, read_catalog, ImportedDatabase, Recorded, MAIN,
    SCHEMA_VIEW_ROOT,
};

impl ImportedDatabase {
    /// Re-points every tree handle this connection holds at the catalog's
    /// current numbers.
    ///
    /// **`reload_catalog` reads the rows and this reads the trees, and both are
    /// needed after another process has written (task-1979, section 4).** A
    /// `PagedTree` holds the root page, the leftmost leaf, the leaf count and
    /// the row count it was attached with; `reattach_entries` deliberately
    /// skips a handle the connection already has, because its case is a
    /// rollback restoring a tree this connection lost. After another process
    /// commits, every one of those numbers has moved on disk and every handle
    /// still holds the old ones - so a scan reads the tree that was there
    /// before and a write splits a page nobody owns.
    ///
    /// The shape - the columns and the key columns - comes from the handle
    /// rather than from the catalog text, because a shape cannot change under
    /// a connection without a DDL statement, and `reload_catalog` has already
    /// rebuilt any handle whose object is new.
    pub(crate) fn reattach_every_tree(&mut self) -> DbResult<()> {
        for at in self.schema_numbers() {
            let wanted: Vec<(u32, PageId, u64, u64)> = self
                .entries_of(at)
                .iter()
                .filter(|held| held.root != 0 && !held.entry.root.is_none())
                .map(|held| {
                    (
                        held.root,
                        held.entry.root,
                        held.entry.stats.leaf_count,
                        held.entry.stats.row_count,
                    )
                })
                .collect();
            for (root, leftmost, leaf_count, row_count) in wanted {
                let Some(existing) = self.schema.trees.get(&root) else {
                    continue;
                };
                let columns = existing.columns().to_vec();
                let key_columns = existing.key_columns();
                let Some(database) = self.schema_file(at) else {
                    continue;
                };
                let tree = PagedTree::attach(
                    database.pool(),
                    u64::from(root),
                    leftmost,
                    columns,
                    key_columns,
                    leaf_count,
                    row_count,
                )?;
                self.schema.trees.insert(root, tree);
            }
            // The catalog itself, which the meta page points at rather than a
            // row of its own.
            if let Some(database) = self.schema_file(at) {
                let catalog_tree = attach_catalog(database.pool(), database.catalog_root())?;
                if at == MAIN {
                    self.schema.trees.insert(SCHEMA_VIEW_ROOT, catalog_tree);
                }
            }
        }
        self.rebuild_tables()?;
        self.refresh_catalog();
        Ok(())
    }

    /// Closes the file and opens it again, from the catalog alone.
    ///
    /// **The test that makes the persisted statistics load-bearing.** Every tree
    /// handle is rebuilt from the catalog row's leftmost leaf, leaf count and
    /// row count rather than from anything this process remembers, so a file
    /// whose statistics were wrong answers differently after a reopen - which is
    /// the failure the numbers exist to prevent, made visible.
    ///
    /// It is on the harness rather than in the engine because the engine's own
    /// open path is Phase 5's consumer story. What this proves is that the
    /// *format* carries what an open needs, which is the part Phase 4 owes.
    pub fn reopen(&mut self) -> DbResult<()> {
        self.checkpoint()?;
        let path = self.storage.path.clone();
        let frames = self.storage.frames;
        let vfs = std::sync::Arc::clone(&self.storage.vfs);
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        // **The old handle lets the file go before the new one asks for it.**
        // Since the engine takes real file locks, a handle that is still holding
        // one is a writer as far as the open path is concerned, and the open
        // would wait out its whole busy budget and then report the file busy -
        // against a lock this same call is about to drop. The checkpoint above
        // has already made the file current, so there is nothing left for the
        // lock to protect.
        self.storage.database.end_access()?;
        // The old handle's file is closed before the new one opens it, because
        // two `Database`s over one path is two page caches over one file.
        let database = {
            let replacement = Database::open(vfs.as_ref(), &db_path, frames.max(64))?;
            std::mem::replace(&mut self.storage.database, replacement)
        };
        drop(database);

        let stored = read_catalog(
            self.storage.database.pool(),
            &attach_catalog(
                self.storage.database.pool(),
                self.storage.database.catalog_root(),
            )?,
        )?;
        let mut trees = HashMap::new();
        let mut entries: Vec<Recorded> = Vec::new();
        for (position, entry) in stored.into_iter().enumerate() {
            let rowid = position.saturating_add(1) as i64;
            // The identifier this tree is registered under, carried across so the
            // plans and layouts this handle already holds keep pointing at the
            // same trees.
            //
            // **This comment used to say the identifier was "this process's own
            // bookkeeping and is not in the file", and that is no longer true.**
            // It was never quite true: every logical row record
            // in the log carries it, so a reader that numbered trees differently
            // would send recovery's records to the wrong tree. It is in the
            // catalog now, `open` reads it from there, and the lookup below
            // agrees with what `open` would derive rather than merely with what
            // this handle happens to remember.
            let root = self
                .schema
                .entries
                .iter()
                .find(|held| held.entry.kind == entry.kind && held.entry.name == entry.name)
                .map(|held| held.root)
                .unwrap_or(0);
            if entry.root.is_none() || root == 0 {
                entries.push(Recorded { rowid, root, entry });
                continue;
            }
            let Some(columns) = self
                .schema
                .trees
                .get(&root)
                .map(|tree| tree.columns().to_vec())
            else {
                entries.push(Recorded { rowid, root, entry });
                continue;
            };
            let key_columns = self
                .schema
                .trees
                .get(&root)
                .map(PagedTree::key_columns)
                .unwrap_or(1);
            let tree = PagedTree::attach(
                self.storage.database.pool(),
                u64::from(root),
                entry.root,
                columns,
                key_columns,
                entry.stats.leaf_count,
                entry.stats.row_count,
            )?;
            trees.insert(root, tree);
            entries.push(Recorded { rowid, root, entry });
        }
        // The catalog itself, which the meta page points at rather than a row.
        let catalog_tree = attach_catalog(
            self.storage.database.pool(),
            self.storage.database.catalog_root(),
        )?;
        trees.insert(SCHEMA_VIEW_ROOT, catalog_tree);
        self.schema.trees = trees;
        self.schema.entries = entries;
        self.storage.wal = std::rc::Rc::new(Wal::open(
            std::sync::Arc::clone(&self.storage.vfs),
            &db_path,
            self.storage.database.uuid(),
            FIRST_LSN,
            1,
            WalOptions::default(),
        )?);
        self.storage
            .database
            .pool()
            .set_durable_lsn(self.storage.wal.write_ahead_point());
        let_the_pool_ask_the_log(self.storage.database.pool(), &self.storage.wal);
        self.rebuild_tables()?;
        self.refresh_catalog();
        Ok(())
    }
}
