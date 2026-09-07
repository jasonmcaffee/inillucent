//! `dbstat` and `sqlite_dbpage`: the two tables that describe the file itself.
//!
//! Invariant: both answer about *this* engine's file, honestly, in SQLite's
//! column shape. They are here rather than in `inillucent-ext` because a module
//! reaches its rows through a `Context` that has a shadow store and nothing
//! else - which is the right boundary for FTS5 and the R-Tree, and the wrong one
//! for a table whose whole subject is the pages underneath every tree. The
//! `pragma_*` functions are in the engine for exactly the same reason.
//!
//! # What "honestly" costs here
//!
//! SQLite's `dbstat` describes SQLite's file format: `pagetype` is `leaf`,
//! `internal` or `overflow`, `ncell` is a cell count, `payload` counts the bytes
//! of cell content on the page. This engine's pages are PAX leaves with a column
//! directory, a delta area and a heap - so the same columns are filled with this
//! format's own answers to the same questions. A reader asking "how many pages
//! does this table use and how full are they" gets a true answer; a reader
//! parsing `payload` as a SQLite cell total is reading a number about a
//! different format, and the column names are the reference's precisely so that
//! the first reader is served.

use inillucent_base::DbResult;
use inillucent_tree::datum::OwnedDatum;

use crate::ImportedDatabase;

/// The columns `dbstat` declares, in SQLite's order.
pub(crate) const DBSTAT_COLUMNS: &[&str] = &[
    "name",
    "path",
    "pageno",
    "pagetype",
    "ncell",
    "payload",
    "unused",
    "mx_payload",
    "pgoffset",
    "pgsize",
];

/// The columns `sqlite_dbpage` declares.
pub(crate) const DBPAGE_COLUMNS: &[&str] = &["pgno", "data"];

impl ImportedDatabase {
    /// Returns one row per page of every tree, for `dbstat`.
    ///
    /// The walk is per tree and then along its leaf chain, which is the order a
    /// reader wants: every page of one object together, objects in catalog
    /// order. A tree whose pages cannot be read is skipped rather than failing
    /// the query, because `dbstat` is a diagnostic and a diagnostic that
    /// refuses to run on a damaged database is the one you needed.
    pub(crate) fn dbstat_rows(&self) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let mut rows = Vec::new();
        for held in &self.entries {
            let entry = &held.entry;
            if held.root == 0 {
                continue;
            }
            let Some(tree) = self.trees.get(&held.root) else {
                continue;
            };
            let Ok(pool) = self.pool_of(held.root) else {
                continue;
            };
            let page_size = tree.page_size() as i64;
            let mut page = tree.first_leaf();
            let mut ordinal = 0usize;
            // A bound on the walk, because a damaged sibling chain that loops
            // would otherwise loop here too - and this is the command someone
            // runs *because* the file is damaged.
            let limit = pool.page_count().saturating_add(1);
            while page.0 != 0 && (ordinal as u64) < limit {
                let Ok(guard) = pool.fetch(page) else {
                    break;
                };
                let bytes = guard.bytes();
                let Ok(leaf) = inillucent_tree::leaf::LeafRef::parse(bytes) else {
                    break;
                };
                let cells = leaf.row_count().saturating_add(leaf.delta_count()) as i64;
                // The heap holds the variable-length payloads; everything from
                // its start to the end of the page is written bytes, and the
                // gap before it is what a further insert may still use.
                let used = page_size.saturating_sub(leaf.heap_start() as i64).max(0);
                rows.push(vec![
                    OwnedDatum::Text(entry.name.clone()),
                    OwnedDatum::Text(format!("/{ordinal:04}/").into_bytes()),
                    OwnedDatum::Int(page.0 as i64),
                    OwnedDatum::Text(b"leaf".to_vec()),
                    OwnedDatum::Int(cells),
                    OwnedDatum::Int(used),
                    OwnedDatum::Int(page_size.saturating_sub(used).max(0)),
                    OwnedDatum::Int(if cells > 0 { used / cells } else { 0 }),
                    OwnedDatum::Int((page.0 as i64).saturating_mul(page_size)),
                    OwnedDatum::Int(page_size),
                ]);
                let next = leaf.right_sibling();
                if next == page {
                    break;
                }
                page = next;
                ordinal = ordinal.saturating_add(1);
            }
        }
        Ok(rows)
    }

    /// Returns one row per page of the file, for `sqlite_dbpage`.
    ///
    /// Read-only. SQLite's is writable, and writing to it is how a person
    /// deliberately corrupts a database to test a recovery tool; that is a real
    /// use, and it is also the single most dangerous surface in its shell. This
    /// engine answers the read that a diagnostic needs and refuses the write,
    /// which the module's default `update` already does.
    pub(crate) fn dbpage_rows(&self) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let pool = self.database.pool();
        let mut rows = Vec::new();
        for page in 1..=pool.page_count() {
            let Ok(guard) = pool.fetch(inillucent_pool::PageId(page)) else {
                continue;
            };
            rows.push(vec![
                OwnedDatum::Int(page as i64),
                OwnedDatum::Blob(guard.bytes().to_vec()),
            ]);
        }
        Ok(rows)
    }
}
