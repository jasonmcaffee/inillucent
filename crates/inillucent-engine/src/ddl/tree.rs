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
                undo: open.then_some(&self.writing.undo),
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
        self.writing
            .touched
            .set(self.writing.touched.get() | crate::schema_bit(at));
        Ok(page)
    }
    /// Gives one tree's pages back to the free map and forgets it.
    ///
    /// @param root - the identifier it is registered under
    pub(crate) fn release_tree(&mut self, root: u32) -> DbResult<()> {
        let at = self.session_state.schema_of(root);
        let owner = self
            .schema_file(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        let pages = match self.schema.trees.get(&root) {
            Some(tree) => tree.pages(owner.pool())?,
            None => Vec::new(),
        };
        let txn = self.current_txn();
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        for page in &pages {
            wal.append(txn, inillucent_wal::record::Body::FreePage { page: page.0 })?;
        }
        {
            let session = self.session_state.session.get();
            let database = crate::file_of(
                &mut self.storage.database,
                &mut self.session_state.attached,
                &mut self.session_state.temps,
                session,
                at,
            )?;
            for page in pages {
                database.release(page, 1)?;
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
