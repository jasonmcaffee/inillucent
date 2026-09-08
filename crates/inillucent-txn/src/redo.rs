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

use std::collections::HashMap;

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;
use inillucent_pool::page::{self, header};
use inillucent_pool::{Database, PageId};
use inillucent_tree::datum::Datum;
use inillucent_tree::leaf::{LeafBuilder, LeafRef};
use inillucent_tree::mutate::{Applied, LeafMut};
use inillucent_tree::types::ColumnSpec;
use inillucent_tree::write::Located;
use inillucent_wal::record::{Body, Record};
use inillucent_wal::Redo;

/// Applies the three logical row records, which need a tree.
pub trait RowRedo {
    /// Puts a row back into a leaf.
    ///
    /// @param database - the file the leaf lives in
    /// @param tree - the tree the leaf belongs to
    /// @param page - the leaf's page number
    /// @param row - the row's encoded bytes
    /// @param lsn - the record's LSN, to stamp the page with
    fn insert_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        row: &[u8],
        lsn: u64,
    ) -> DbResult<()>;

    /// Takes a row back out of a leaf.
    ///
    /// @param database - the file the leaf lives in
    /// @param tree - the tree the leaf belongs to
    /// @param page - the leaf's page number
    /// @param key - the row's key
    /// @param lsn - the record's LSN, to stamp the page with
    fn delete_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        lsn: u64,
    ) -> DbResult<()>;

    /// Repacks a leaf's live rows, the way the write path did.
    ///
    /// The record that asks for this carries no page image: a compaction is
    /// deterministic given the page it starts from, and redo replays in LSN
    /// order, so the page is in that state when the record is reached. See
    /// `PagedTree::compact_into` for why the log is a great deal smaller for it.
    ///
    /// @param database - the file the leaf lives in
    /// @param tree - the tree the leaf belongs to
    /// @param page - the leaf's page number
    /// @param lsn - the record's LSN, to stamp the page with
    fn compact_leaf(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        lsn: u64,
    ) -> DbResult<()>;

    /// Overwrites one fixed-width slot of one row.
    ///
    /// @param database - the file the leaf lives in
    /// @param tree - the tree the leaf belongs to
    /// @param page - the leaf's page number
    /// @param key - the row's key
    /// @param column - which column changed
    /// @param value - the new value's slot bytes
    /// @param lsn - the record's LSN, to stamp the page with
    fn update_in_place(
        &mut self,
        database: &mut Database,
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
    fn compact_leaf(
        &mut self,
        _database: &mut Database,
        _tree: u64,
        page: PageId,
        _lsn: u64,
    ) -> DbResult<()> {
        Err(misuse(format!(
            "a CompactLeaf record for page {} needs a tree to repack it",
            page.0
        )))
    }

    fn insert_row(
        &mut self,
        _database: &mut Database,
        _tree: u64,
        page: PageId,
        _row: &[u8],
        _lsn: u64,
    ) -> DbResult<()> {
        Err(misuse(format!(
            "an InsertRow record for page {} needs a tree to apply into",
            page.0
        )))
    }

    fn delete_row(
        &mut self,
        _database: &mut Database,
        _tree: u64,
        page: PageId,
        _key: &[u8],
        _lsn: u64,
    ) -> DbResult<()> {
        Err(misuse(format!(
            "a DeleteRow record for page {} needs a tree to apply into",
            page.0
        )))
    }

    fn update_in_place(
        &mut self,
        _database: &mut Database,
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

/// The shape of one tree, as the replay needs to know it.
///
/// A leaf's page says how many columns it has and what each one's physical type
/// is, and it deliberately does *not* say what a column's collation is - the
/// catalog says that, and a page carrying its own could disagree with it. So
/// the shape is handed to the replay by whoever opens the database, which is
/// the thing that holds the schema.
#[derive(Clone, Debug)]
pub struct RedoTree {
    /// The column directory, in tree-column order.
    pub columns: Vec<ColumnSpec>,
    /// How many leading columns form the key.
    pub key_columns: usize,
}

/// A `RowRedo` that applies row records to the leaves they name.
///
/// **This is what makes a tree's writes recoverable.** The tree logs an
/// `InsertRow`, a `DeleteRow` or an `UpdateInPlace` for every change it makes to
/// a leaf, and until this existed the only thing that could be done with one of
/// those records on recovery was to refuse it - so a database whose write path
/// had run could be reopened only if a checkpoint had happened to write every
/// leaf it touched. That is not recovery; it is a coincidence.
///
/// ## Idempotence
///
/// Nothing here is idempotent on its own: applying an `InsertRow` twice would
/// put the row in twice. What makes the replay safe is the page-LSN rule above
/// it - [`Applier::page_lsn`] reports the leaf's stamp, the scan skips any
/// record at or below it, and every method here stamps the leaf with the
/// record's LSN before it returns. A method that forgot to stamp would be
/// replayed on every recovery, which is what the recover-twice-byte-identical
/// test exists to catch.
///
/// ## Where a key comes from
///
/// The record's own bytes, as tagged values - the same encoding the row records
/// use. The comparison encoding the descent uses is one way: it orders
/// correctly and cannot be read back, so a replay holding one could not find
/// the row it names.
#[derive(Debug, Default)]
pub struct TreeRows {
    /// The shape of each tree, by tree id.
    trees: HashMap<u64, RedoTree>,
}

impl TreeRows {
    /// Returns an applier that knows about no trees yet.
    pub fn new() -> TreeRows {
        TreeRows::default()
    }

    /// Returns an applier that also knows about one tree.
    ///
    /// @param tree - the tree's id
    /// @param columns - its column directory
    /// @param key_columns - how many leading columns form its key
    pub fn with_tree(
        mut self,
        tree: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
    ) -> TreeRows {
        self.trees.insert(
            tree,
            RedoTree {
                columns,
                key_columns,
            },
        );
        self
    }

    /// Returns one tree's shape, or says the replay was never told about it.
    ///
    /// @param tree - the tree's id
    fn shape(&self, tree: u64) -> DbResult<&RedoTree> {
        self.trees.get(&tree).ok_or_else(|| {
            misuse(format!(
                "the log names tree {tree}, which this recovery was not told the shape of"
            ))
        })
    }
}

/// Reports whether a logged row holds an out-of-line reference.
///
/// @param bytes - the record's bytes
/// @param columns - how many values the row holds
fn holds_extent(bytes: &[u8], columns: usize) -> DbResult<bool> {
    let mut at = 0usize;
    for _ in 0..columns {
        let rest = bytes.get(at..).unwrap_or(&[]);
        if rest.is_empty() {
            break;
        }
        if Datum::tag_of(rest)? == inillucent_tree::datum::tag::EXTENT {
            return Ok(true);
        }
        at = at.saturating_add(Datum::tagged_span(rest)?);
    }
    Ok(false)
}

/// Decodes a run of tagged values.
///
/// @param bytes - the record's bytes
/// @param most - how many values to read at most
fn decode_all(bytes: &[u8], most: usize) -> DbResult<Vec<Datum<'_>>> {
    let mut values = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() && values.len() < most {
        let (value, width) = Datum::decode_tagged(bytes.get(at..).unwrap_or(&[]))?;
        values.push(value);
        at = at.saturating_add(width);
    }
    Ok(values)
}

impl RowRedo for TreeRows {
    fn compact_leaf(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        lsn: u64,
    ) -> DbResult<()> {
        let shape = self.shape(tree)?.clone();
        let page_size = database.page_size();
        // Read the leaf, repack it, and put it back - the same three steps the
        // write path took, in the same order, with the same fill.
        let mut image = {
            let guard = database.pool().fetch(page)?;
            // The collations come from the column directory, which is what the
            // tree itself reads them from - a compaction sorts under them and a
            // replay that used BINARY would repack the rows in a different
            // order.
            let collations: Vec<inillucent_value::collation::Collation> = shape
                .columns
                .iter()
                .take(shape.key_columns)
                .map(|spec| spec.collation)
                .collect();
            // And the directions with them, for the same reason: a descending
            // key column's rows are packed the other way round, and a replay
            // that read them as ascending would compact them into an order the
            // tree's own searches do not agree with.
            let directions: Vec<bool> = shape
                .columns
                .iter()
                .take(shape.key_columns)
                .map(|spec| spec.descending)
                .collect();
            let leaf = LeafRef::parse(&guard)?
                .with_collations(&collations)
                .with_directions(&directions);
            let rows = leaf.live()?;
            let builder =
                LeafBuilder::new(page_size, tree, shape.columns.clone(), shape.key_columns)?;
            // **The same fill ladder the write path walked**, and for the same
            // reason it is a shared function: a compaction is logged without its
            // page when it is deterministic, and "deterministic" means this
            // replay lands on the same bytes. A leaf packed above `COMPACT_FILL`
            // by a bulk build compacts at `TIGHT_FILL` rather than splitting, and
            // a replay that only knew the first fill declared the file corrupt.
            let Some(mut image) = inillucent_tree::write::compact_image(&builder, &rows)? else {
                // The write path only logs a compaction when every live row
                // fits; a replay that cannot fit them is looking at a different
                // page than the one the record was written against.
                return Err(corrupt(format!(
                    "replaying a compaction of leaf {} could not fit its {} rows",
                    page.0,
                    rows.len()
                )));
            };
            inillucent_pool::page::set_right(&mut image, leaf.right_sibling())?;
            page::write_u64(
                &mut image,
                inillucent_tree::leaf::leaf_header::MAX_CTS,
                leaf.max_cts(),
            )?;
            image
        };
        page::write_u64(&mut image, header::LSN, lsn)?;
        database.install(page, &image)
    }

    fn insert_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        row: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        let shape = self.shape(tree)?.clone();
        // **Only the key is decoded.** A row's later columns can hold an
        // out-of-line reference, which is not a value a `Datum` carries - and
        // the replay does not need one, because it writes the record's bytes
        // back verbatim. Key columns are never written out of line, so the
        // front of the record always decodes.
        let key = decode_all(row, shape.key_columns)?;
        let spilled = holds_extent(row, shape.columns.len())?;
        database.pool().modify(page, |bytes| {
            let mut leaf = LeafMut::new(bytes)?;
            // An insert replaces, exactly as the write path's does: whatever the
            // key already named goes first. A replay that only added would put a
            // second row under a key the tree holds once.
            let located = leaf.view()?.locate(&key, shape.key_columns)?;
            match located {
                Located::Sorted(at) => {
                    leaf.set_tombstone(at)?;
                }
                Located::Delta(index) => leaf.remove_delta(index)?,
                Located::Absent => {}
            }
            if leaf.insert_delta_encoded(row)? == Applied::NoRoom {
                // The leaf had room when the record was written, so it has room
                // now unless the page in the file is not the page the record was
                // written against. Reporting it is the only honest answer; the
                // alternative is a replay that silently dropped a row.
                return Err(corrupt(format!(
                    "replaying InsertRow at {lsn} found no room in leaf {}",
                    page.0
                )));
            }
            if spilled {
                leaf.mark_extents()?;
            }
            leaf.set_lsn(lsn)
        })
    }

    fn delete_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        let shape = self.shape(tree)?.clone();
        let values = decode_all(key, shape.key_columns)?;
        database.pool().modify(page, |bytes| {
            let mut leaf = LeafMut::new(bytes)?;
            match leaf.view()?.locate(&values, shape.key_columns)? {
                Located::Sorted(at) => {
                    leaf.set_tombstone(at)?;
                }
                Located::Delta(index) => leaf.remove_delta(index)?,
                // The row is already gone, which is what a replay onto a page
                // that was checkpointed after the delete looks like. The page
                // LSN would normally have skipped this record; reaching here
                // means the leaf was rewritten with a lower stamp, and putting
                // the tombstone back is not possible when there is no row.
                Located::Absent => {}
            }
            leaf.set_lsn(lsn)
        })
    }

    fn update_in_place(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        column: u32,
        value: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        let shape = self.shape(tree)?.clone();
        let values = decode_all(key, shape.key_columns)?;
        let (new_value, _) = Datum::decode_tagged(value)?;
        database.pool().modify(page, |bytes| {
            let mut leaf = LeafMut::new(bytes)?;
            let Located::Sorted(row) = leaf.view()?.locate(&values, shape.key_columns)? else {
                // An in-place update is only ever logged against a row in the
                // sorted region - that is the condition the write path checks
                // before it takes this path at all - so anything else means the
                // page is not the one the record was written against.
                return Err(corrupt(format!(
                    "replaying UpdateInPlace at {lsn} found no sorted row in leaf {}",
                    page.0
                )));
            };
            leaf.update_slot(column as usize, row, &new_value)?;
            leaf.set_lsn(lsn)
        })
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
            // An empty image means "re-run the compaction"; one that carries a
            // page is copied, so a log written before this change still replays.
            Body::CompactLeaf { tree, page, image } if image.is_empty() => {
                self.rows
                    .compact_leaf(self.database, tree, PageId(page), lsn)?;
                self.stats.images = self.stats.images.saturating_add(1);
            }
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
                self.rows
                    .insert_row(self.database, tree, PageId(page), row, lsn)?;
                self.stats.rows = self.stats.rows.saturating_add(1);
            }
            Body::DeleteRow { tree, page, key } => {
                self.rows
                    .delete_row(self.database, tree, PageId(page), key, lsn)?;
                self.stats.rows = self.stats.rows.saturating_add(1);
            }
            Body::UpdateInPlace {
                tree,
                page,
                key,
                column,
                value,
            } => {
                self.rows.update_in_place(
                    self.database,
                    tree,
                    PageId(page),
                    key,
                    column,
                    value,
                    lsn,
                )?;
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
