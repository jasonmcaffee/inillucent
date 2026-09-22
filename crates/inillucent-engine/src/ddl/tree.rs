//! Building a tree for a new object, and releasing one for a dropped one.
//!
//! Invariant: **a tree is built before its catalog row is written.** The row
//! names the root page, so writing the row first would name a page that does
//! not exist yet - and a crash between the two would leave a catalog pointing
//! at nothing.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_exec::physical::SourceLayout;
use inillucent_pool::PageId;
use inillucent_tree::datum::Datum;
use inillucent_tree::types::ColumnSpec;
use inillucent_tree::PagedTree;

use crate::*;

impl crate::ImportedDatabase {
    /// Builds one empty tree and registers it.
    ///
    /// @param root - the identifier to register it under
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    /// @param layout - how a bound expression finds its vector
    pub(crate) fn build_tree(
        &mut self,
        root: u32,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        layout: SourceLayout,
    ) -> DbResult<PageId> {
        self.build_tree_from::<Vec<Datum<'_>>>(root, columns, key_columns, layout, &[])
    }
    /// Builds one tree from rows already in key order, and registers it.
    ///
    /// @param root - the identifier to register it under
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    /// @param layout - how a bound expression finds its vector
    /// **Generic over the row's container, and it no longer copies.** It used
    /// to take owned rows and build a `Vec<Vec<Datum>>` of the whole input to
    /// hand the builder - one allocation per row, 4.2 ms of a 48 ms
    /// `CREATE INDEX` at a hundred thousand rows, and pure waste for a caller
    /// whose rows are already borrowed. A caller that holds `OwnedDatum` rows
    /// now does its own borrow, which is where that cost belongs.
    ///
    /// @param rows - the rows, sorted by the key columns
    pub(crate) fn build_tree_from<'d, R: AsRef<[Datum<'d>]>>(
        &mut self,
        root: u32,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        layout: SourceLayout,
        rows: &[R],
    ) -> DbResult<PageId> {
        self.build_tree_rows(
            root,
            columns,
            key_columns,
            layout,
            &inillucent_tree::leaf::RowSlice(rows),
        )
    }
    /// Builds a tree from a row source rather than from a slice of rows.
    ///
    /// The form `CREATE INDEX` uses, so its entries are packed straight out of
    /// the arena they were scanned into. Everything else about it is
    /// [`Self::build_tree_from`], which is now a one-line wrapper over it.
    ///
    /// @param root - the identifier the tree is registered under
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param layout - how the tree's columns map onto the table's
    /// @param rows - the rows, already in key order
    pub(crate) fn build_tree_rows<'d>(
        &mut self,
        root: u32,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        layout: SourceLayout,
        rows: &dyn inillucent_tree::leaf::Rows<'d>,
    ) -> DbResult<PageId> {
        let at = self.schema.ddl_schema;
        let local = self.local_of(at, root);
        let tree = {
            let (_, txn, open, wal, uncommitted) = self.catalog_write()?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                undo: open.then_some(self.writing.undo()),
                uncommitted,
            };
            let session = self.session_state.session.get();
            let database = crate::file_of(
                &mut self.storage.database,
                &mut self.session_state.attached,
                &mut self.session_state.temps,
                session,
                at,
            )?;
            PagedTree::bulk_build_rows(
                database,
                Some(&mut log),
                // **The file's own number, not the connection's.** Every record
                // this tree writes carries it, and the log outlives the process
                // that made the handle.
                local,
                columns,
                key_columns,
                rows,
            )?
        };
        let page = tree.root();
        self.schema.trees.insert(root, tree);
        self.schema.layouts.insert(root, std::rc::Rc::new(layout));
        // **Which file the handle belongs to is part of registering the tree
        // (task-2061).** `allocate_in` records it for a handle it has just
        // handed out, so a `CREATE TABLE` was already right. A *rebuild* does
        // not allocate: `ALTER TABLE ... ADD COLUMN` on an existing table
        // releases the old tree and builds a new one under the same handle, and
        // `release_tree` takes the handle out of `owner` on the way through.
        // Nothing put it back, so `schema_of` fell to its "not attached, so
        // `main`" answer and every later read of that table went to `main`'s
        // file at a page number belonging to another one -
        // `read 0 of 32768 bytes at 163840` for an `ALTER TABLE side.t ADD
        // COLUMN` and for the same statement on a `TEMP` table. `main`'s own
        // handles stay out of the map, which is what `schema_of` is allowed to
        // assume.
        if at != crate::MAIN {
            self.session_state.owner.insert(root, at);
        }
        // **Recorded so that a transaction which is abandoned can give the
        // pages back (task-2065).** The allocation above is not committed
        // until the transaction is, and the undo buffer is row-level, so
        // nothing else in this engine knows that these pages were free before
        // the statement ran. See [`crate::engine::state::BuiltTree`] for why
        // the root page is recorded beside the handle.
        self.writing
            .built()
            .borrow_mut()
            .push(crate::engine::state::BuiltTree {
                schema: at,
                root,
                page,
            });
        self.writing
            .set_touched(self.writing.touched() | crate::schema_bit(at));
        Ok(page)
    }
    /// Forgets one tree and puts its pages on the list the commit frees.
    ///
    /// **The pages are not given back here, and that is the fix for task-2043.**
    /// This used to call `database.release` for every page as it ran. The free
    /// map rewinds its allocator hint to the lowest page it is given back
    /// (`inillucent_pool::FreeMap::free`), so inside one transaction
    ///
    /// ```sql
    /// BEGIN; DROP TABLE p; CREATE TABLE p (...); ROLLBACK;
    /// ```
    ///
    /// handed p's own root page straight to the `CREATE`, which wrote an empty
    /// tree over it. The rollback then restored p's catalog row - a row is a
    /// row, and the undo buffer holds it - and `reattach_entries` attached p at
    /// the root page that row names, which by then was the empty tree. p came
    /// back with no rows, durably, and `PRAGMA integrity_check` said `ok`
    /// because an empty tree is a valid tree.
    ///
    /// A row-level undo buffer cannot repair that: it has no image of page P to
    /// put back. Holding the frees until the commit means it never has to - a
    /// transaction that is abandoned never gave the page away, so the pages are
    /// still exactly what the restored catalog row says they are.
    ///
    /// It also fixes the drop-alone case, which only looked correct. The
    /// rollback never put the page back in the free map, so
    /// `BEGIN; DROP TABLE p; ROLLBACK` answered `3` and left P marked free while
    /// p still pointed at it - and the next statement to allocate anything
    /// overwrote p's rows.
    ///
    /// The cost is that a transaction which drops a table and then creates one
    /// grows the file rather than reusing the space until it commits. That is
    /// the right trade: the space comes back at the commit, and the alternative
    /// is the lost rows above.
    ///
    /// **The tree's out-of-line values go on the list too (task-2065).** This
    /// used to record a walk that answered the tree's interior pages and its
    /// leaves and nothing else, so dropping a table of
    /// large values gave back its tree and kept every page those values sat on
    /// until a `VACUUM`. `paged::free_extent` is reached only from the tree's
    /// own write paths - a row deleted, a value replaced, two leaves merged -
    /// so a tree released whole never reached it, and `DELETE FROM t` before
    /// the drop was what gave the space back. That is why the walk now answers
    /// both: a value is recorded as the reference its leaf held rather than as
    /// a page number, because a small value shares its page with values from
    /// other trees and only the reference says which slot to clear.
    ///
    /// @param root - the identifier it is registered under
    pub(crate) fn release_tree(&mut self, root: u32) -> DbResult<()> {
        let at = self.session_state.schema_of(root);
        let owner = self
            .schema_file(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        let released = match self.schema.trees.get(&root) {
            Some(tree) => inillucent_tree::paged::released(owner.pool(), tree.root())?,
            None => inillucent_tree::paged::Released {
                pages: Vec::new(),
                values: Vec::new(),
            },
        };
        // The `FreePage` records go in with the release, at commit, for the same
        // reason: the log is redo-only, so a record that says a page is free is
        // only true of a transaction that committed, and appending it here would
        // describe a free that the rollback then did not do.
        {
            use crate::engine::state::{Freed, PendingFree};
            let mut waiting = self.writing.pending_frees().borrow_mut();
            for page in released.pages {
                waiting.push(PendingFree {
                    schema: at,
                    what: Freed::Page(page),
                });
            }
            for reference in released.values {
                waiting.push(PendingFree {
                    schema: at,
                    what: Freed::Value(reference),
                });
            }
        }
        self.session_state.owner.remove(&root);
        self.schema.trees.remove(&root);
        self.schema.layouts.remove(&root);
        self.schema.covering.remove(&root);
        for roots in self.schema.covering.values_mut() {
            roots.retain(|held| *held != root);
        }
        Ok(())
    }
}
