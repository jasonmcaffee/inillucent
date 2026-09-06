//! Applying a log record to the data file.
//!
//! Invariant: **redo is idempotent by page LSN.** A record is applied to a page
//! only when the page's LSN is below the record's, and applying sets it to the
//! record's. `inillucent-wal`'s scan decides *which* records to apply; this
//! decides what applying one means, and the two are separate crates for the
//! reason the layering file gives - the log is written before any page exists,
//! so it does not depend on the crate that owns pages.
//!
//! ## The page LSN is read out of the page
//!
//! Not out of a table beside it. A page's LSN is a property of the bytes that
//! will be in the file, and any second copy of it is a second copy that can
//! disagree with the first. `inillucent-pool`'s common header has carried the
//! field since Phase 2 for exactly this.
//!
//! ## Two halves, and why one is a trait
//!
//! A `WritePage`, a `CompactLeaf` and a split all carry **whole page images**,
//! so applying one is a copy and this module can do it with nothing but the
//! buffer pool. An `InsertRow`, a `DeleteRow` and an `UpdateInPlace` are
//! **logical**: applying one means putting a row into a leaf's delta area or
//! setting a tombstone, which is the tree's business. [`RowRedo`] is that seam.
//! A caller that has no tree - the recovery *report*, a log inspector - passes
//! [`RefuseRows`] and finds out rather than being given a wrong answer.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;
use inillucent_pool::page::{self, header};
use inillucent_pool::{Database, PageId};
use inillucent_wal::record::{Body, Record};
use inillucent_wal::Redo;

/// Applies the three logical row records, which need a tree.
pub trait RowRedo {
    /// Puts a row back into a leaf.
    ///
    /// @param tree - the tree the leaf belongs to
    /// @param page - the leaf's page number
    /// @param row - the row's encoded bytes
    /// @param lsn - the record's LSN, to stamp the page with
    fn insert_row(&mut self, tree: u64, page: PageId, row: &[u8], lsn: u64) -> DbResult<()>;

    /// Takes a row back out of a leaf.
    ///
    /// @param tree - the tree the leaf belongs to
    /// @param page - the leaf's page number
    /// @param key - the row's key
    /// @param lsn - the record's LSN, to stamp the page with
    fn delete_row(&mut self, tree: u64, page: PageId, key: &[u8], lsn: u64) -> DbResult<()>;

    /// Overwrites one fixed-width slot of one row.
    ///
    /// @param tree - the tree the leaf belongs to
    /// @param page - the leaf's page number
    /// @param key - the row's key
    /// @param column - which column changed
    /// @param value - the new value's slot bytes
    /// @param lsn - the record's LSN, to stamp the page with
    fn update_in_place(
        &mut self,
        tree: u64,
        page: PageId,
        key: &[u8],
        column: u32,
        value: &[u8],
        lsn: u64,
    ) -> DbResult<()>;
}

/// A `RowRedo` that refuses, for a caller with no tree to apply into.
///
/// It refuses rather than ignoring. A log inspector that silently skipped every
/// row record would report a recovery that bears no relation to the one that
/// would happen, which is the shape of instrument this project has already been
/// bitten by: a number that is not wrong so much as about something else.
#[derive(Debug, Default)]
pub struct RefuseRows;

impl RowRedo for RefuseRows {
    fn insert_row(&mut self, _tree: u64, page: PageId, _row: &[u8], _lsn: u64) -> DbResult<()> {
        Err(misuse(format!(
            "an InsertRow record for page {} needs a tree to apply into",
            page.0
        )))
    }

    fn delete_row(&mut self, _tree: u64, page: PageId, _key: &[u8], _lsn: u64) -> DbResult<()> {
        Err(misuse(format!(
            "a DeleteRow record for page {} needs a tree to apply into",
            page.0
        )))
    }

    fn update_in_place(
        &mut self,
        _tree: u64,
        page: PageId,
        _key: &[u8],
        _column: u32,
        _value: &[u8],
        _lsn: u64,
    ) -> DbResult<()> {
        Err(misuse(format!(
            "an UpdateInPlace record for page {} needs a tree to apply into",
            page.0
        )))
    }
}

/// What a replay did, for the report and for the tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RedoStats {
    /// Whole page images copied.
    pub images: u64,
    /// Logical row records applied.
    pub rows: u64,
    /// Free-map bits set or cleared.
    pub allocations: u64,
    /// Pages skipped because their LSN was already at or above the record's.
    pub skipped: u64,
    /// The newest commit timestamp seen.
    pub latest_cts: u64,
    /// Whether a catalog change was replayed.
    pub catalog_changed: bool,
}

/// Applies records to a database file.
pub struct Applier<'a, R: RowRedo> {
    database: &'a mut Database,
    rows: R,
    stats: RedoStats,
    /// Pages the replay allocated, so the free map can be rebuilt.
    allocated: Vec<PageId>,
    /// Pages the replay freed.
    freed: Vec<PageId>,
}

impl<'a, R: RowRedo> Applier<'a, R> {
    /// Returns an applier over a database.
    ///
    /// @param database - the file to apply into
    /// @param rows - what to do with the logical row records
    pub fn new(database: &'a mut Database, rows: R) -> Applier<'a, R> {
        Applier {
            database,
            rows,
            stats: RedoStats::default(),
            allocated: Vec::new(),
            freed: Vec::new(),
        }
    }

    /// Returns what the replay did.
    pub fn stats(&self) -> RedoStats {
        self.stats
    }

    /// Returns the pages the replay allocated and freed, in that order.
    ///
    /// The TDD says recovery must "rebuild the free map if any `AllocPage` or
    /// `FreePage` was replayed", and this is what it rebuilds from. They are
    /// applied by the caller rather than here because the free map is behind
    /// `&mut Database` and so is every page write, and doing both inside one
    /// record's application would mean holding two mutable borrows of the same
    /// object.
    pub fn allocations(&self) -> (&[PageId], &[PageId]) {
        (&self.allocated, &self.freed)
    }

    /// Copies a whole page image into the file and stamps its LSN.
    ///
    /// @param page - the page's number
    /// @param image - the whole page
    /// @param lsn - the record's LSN
    fn put_image(&mut self, page: u64, image: &[u8], lsn: u64) -> DbResult<()> {
        let size = self.database.page_size();
        if image.len() != size {
            return Err(corrupt(format!(
                "a log record carries a {}-byte image for page {page} in a {size}-byte database",
                image.len()
            )));
        }
        let mut stamped = image.to_vec();
        page::write_u64(&mut stamped, header::LSN, lsn)?;
        self.database.install(PageId(page), &stamped)?;
        self.stats.images = self.stats.images.saturating_add(1);
        Ok(())
    }
}

impl<R: RowRedo> Redo for Applier<'_, R> {
    fn page_lsn(&mut self, page: u64) -> DbResult<Option<u64>> {
        let id = PageId(page);
        if page >= self.database.pool().page_count() {
            return Ok(None);
        }
        match self.database.pool().fetch(id) {
            Ok(guard) => Ok(Some(page::read_u64(&guard, header::LSN)?)),
            // A page the file does not hold, or holds unreadably, is one the
            // record is about to write in full. Treating it as "no LSN" is what
            // makes a replay onto a truncated file work, and it is safe because
            // the only records that reach a missing page are the ones carrying a
            // whole image - a logical row record against a page that is not
            // there fails in `RowRedo`, where it should.
            Err(_) => Ok(None),
        }
    }

    fn redo(&mut self, record: &Record<'_>, wanted: &[bool]) -> DbResult<()> {
        let lsn = record.lsn;
        match record.body {
            Body::WritePage { page, image } => self.put_image(page, image, lsn)?,
            Body::CompactLeaf { page, image, .. } => self.put_image(page, image, lsn)?,
            Body::Structural {
                left,
                right,
                parent,
                left_image,
                right_image,
                parent_image,
                ..
            } => {
                // Zipped rather than indexed by an enumerated slot: `wanted` has
                // exactly one entry per page the record names, so an
                // `unwrap_or(false)` would be a branch no input can take.
                for (take, (page, image)) in wanted.iter().zip([
                    (left, left_image),
                    (right, right_image),
                    (parent, parent_image),
                ]) {
                    if *take {
                        self.put_image(page, image, lsn)?;
                    } else {
                        self.stats.skipped = self.stats.skipped.saturating_add(1);
                    }
                }
            }
            Body::InsertRow { tree, page, row } => {
                self.rows.insert_row(tree, PageId(page), row, lsn)?;
                self.stats.rows = self.stats.rows.saturating_add(1);
            }
            Body::DeleteRow { tree, page, key } => {
                self.rows.delete_row(tree, PageId(page), key, lsn)?;
                self.stats.rows = self.stats.rows.saturating_add(1);
            }
            Body::UpdateInPlace {
                tree,
                page,
                key,
                column,
                value,
            } => {
                self.rows
                    .update_in_place(tree, PageId(page), key, column, value, lsn)?;
                self.stats.rows = self.stats.rows.saturating_add(1);
            }
            Body::AllocPage { page } => {
                self.allocated.push(PageId(page));
                self.stats.allocations = self.stats.allocations.saturating_add(1);
            }
            Body::FreePage { page } => {
                self.freed.push(PageId(page));
                self.stats.allocations = self.stats.allocations.saturating_add(1);
            }
            Body::Commit { cts } => self.stats.latest_cts = self.stats.latest_cts.max(cts),
            Body::Checkpoint { cts_watermark, .. } => {
                self.stats.latest_cts = self.stats.latest_cts.max(cts_watermark)
            }
            // An abort never reaches here: the scan does not replay one. It is
            // matched rather than left to a wildcard so that a kind added later
            // is a compile error instead of a silent no-op.
            Body::Abort => {}
            Body::CatalogChange { .. } => self.stats.catalog_changed = true,
        }
        Ok(())
    }
}
