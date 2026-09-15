//! What a database will tell you about itself without running a statement.
//!
//! Invariant: **nothing here changes anything.** Every method is a reader - the
//! catalog's view, a tree's root, the pool's counters, the file's size - and
//! they are together because a reader looking for one of them is looking for the
//! list rather than for a particular name.

use inillucent_base::DbResult;
use inillucent_sql::catalog_view::StaticCatalog;
use inillucent_tree::PagedTree;

impl crate::ImportedDatabase {
    /// Returns the catalog a statement is bound against.
    ///
    /// Exposed so an instrument can time binding on its own. `plan` is parse,
    /// bind and logical planning together, and knowing that the three of them
    /// are 64% of compiling `SELECT 1` does not say which of the three to
    /// change.
    pub fn catalog_view(&self) -> &StaticCatalog {
        &self.schema.catalog
    }

    /// Returns every object's name and the identifier its tree is known by.
    ///
    /// The identifier is what the log refers to a tree by, so it is the thing
    /// two processes have to agree about. This exposes it so that agreement can
    /// be *tested* rather than assumed - a writer and a reader that disagreed
    /// would corrupt a recovery quietly, and the only cheap way to catch a
    /// caller reintroducing a process-local number is to compare the two.
    pub fn tree_identifiers(&self) -> Vec<(String, u64)> {
        self.schema
            .entries
            .iter()
            .map(|held| {
                (
                    String::from_utf8_lossy(&held.entry.name).into_owned(),
                    held.entry.tree_id,
                )
            })
            .collect()
    }

    /// Returns the tables the import could not take.
    ///
    /// A caller that finds a query refused can tell "the engine does not do
    /// this yet" from "the table is not there" by looking here.
    pub fn skipped(&self) -> &[String] {
        &self.schema.skipped
    }

    /// Returns how many frames the pool holds.
    pub fn frames(&self) -> usize {
        self.storage.frames
    }

    /// Returns how many bytes the pool occupies.
    pub fn pool_bytes(&self) -> usize {
        self.storage.database.pool().byte_size()
    }

    /// Returns the database file the import wrote.
    pub fn file(&self) -> &std::path::Path {
        &self.storage.path
    }

    /// Returns how many pages the file holds.
    pub fn page_count(&self) -> u64 {
        self.storage.database.pool().page_count()
    }

    /// Returns how many pool frames hold a page right now.
    ///
    /// The measurable half of "what does the engine have in memory": the pool
    /// is where a database's pages live, and a frame count times the page size
    /// is the part of the resident set the engine chose rather than the part
    /// the allocator happens to be holding.
    pub fn frames_resident(&self) -> usize {
        self.storage.database.pool().resident()
    }

    /// Returns what the pool has done since the last reset.
    pub fn pool_stats(&self) -> inillucent_pool::PoolStats {
        self.storage.database.pool().stats()
    }

    /// Reads every page of every tree, so a measurement starts warm.
    ///
    /// A cold pool measures the file system, and neither engine's scorecard
    /// number is about that. SQLite's arm is warmed by the harness running the
    /// workload before it times it; this is the same courtesy on this side, and
    /// it is stated rather than left to the first round.
    pub fn warm(&self) -> DbResult<()> {
        for tree in self.schema.trees.values() {
            tree.visit_leaves(self.storage.database.pool(), &mut |_| Ok(true))?;
        }
        Ok(())
    }

    /// Returns the bytes one root's tree occupies.
    ///
    /// Reported beside a measurement so a reader can see how much data each
    /// engine's chosen structure actually reads.
    ///
    /// @param root - the root page id the fixture recorded
    pub fn byte_size(&self, root: u32) -> Option<usize> {
        self.schema.trees.get(&root).map(PagedTree::byte_size)
    }

    /// Returns the index roots that could cover a query over one table.
    ///
    /// @param table_root - the table's root page id
    pub fn candidates(&self, table_root: u32) -> Vec<u32> {
        self.schema
            .covering
            .get(&table_root)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns the page size the trees were built at.
    pub fn page_size(&self) -> usize {
        self.storage.page_size
    }

    /// Returns how many leaves one root's tree holds.
    ///
    /// @param root - the root page id the fixture recorded
    pub fn leaf_count(&self, root: u32) -> Option<usize> {
        self.schema
            .trees
            .get(&root)
            .map(|tree| tree.leaf_count() as usize)
    }

    /// Returns a table's root page id by name.
    ///
    /// The root page is the identifier everything else here is keyed by, and a
    /// measurement that wants "the main table" has only the name.
    ///
    /// @param name - the table's name
    pub fn table_root(&self, name: &str) -> Option<u32> {
        self.schema
            .layouts
            .iter()
            .filter(|(root, _)| self.schema.covering.contains_key(root))
            .map(|(root, _)| *root)
            .find(|root| self.catalog_name(*root).as_deref() == Some(name))
    }

    /// Returns the table name a root page belongs to.
    ///
    /// @param root - the root page id
    fn catalog_name(&self, root: u32) -> Option<String> {
        self.schema
            .catalog
            .tables
            .iter()
            .find(|table| table.root == root)
            .map(|table| String::from_utf8_lossy(&table.name).into_owned())
    }

    /// Returns every imported root, for reporting.
    pub fn roots(&self) -> Vec<u32> {
        let mut roots: Vec<u32> = self.schema.trees.keys().copied().collect();
        roots.sort_unstable();
        roots
    }
}
