//! Building a whole tree from rows that are already in key order.
//!
//! Invariant: **a bulk build writes each page once and never reads it back.** Rows
//! arrive sorted, so a leaf is filled, closed and written, and the interior levels
//! are built from the separators the closed leaves produced - which is why this is a
//! different code path from `write.rs`'s one row at a time and not a loop over it.
//!
//! Split out of `paged.rs` in task-2006, which had grown past its recorded ceiling
//! while design 2 of task-2000 stopped the build going through the buffer pool. The
//! ratchet in `policy.rs` asks for an extraction rather than a raised number, and
//! the bulk build is a coherent unit: one question, how a sorted run of rows becomes
//! a tree. Nothing changed in the move.
//!
//! `log_allocated_page` stayed in `paged.rs`, because the ordinary allocation path
//! wants it as well; `write_built_page` came here, because design 2's direct write
//! is the bulk build's own.

use super::*;

/// Records one bulk-built page's allocation and writes the page itself straight
/// into the data file.
///
/// **Design 2 of task-2000: a bulk built page is written once.** This used to log
/// the page's whole image as a `WritePage` and hand it to `Database::install`,
/// which marks the frame dirty - so the next fold wrote the same page a second
/// time, and the frame stayed resident until it did. `CREATE INDEX` over 100,000
/// text values logged 6.2 MB, wrote 6.2 MB, and then wrote 6.2 MB again, with
/// `pack` at 11.4 ms of a 26 ms statement.
///
/// **The `AllocPage` record stays and the `WritePage` goes.** The allocation still
/// has to be in the log, because it is what stops a later allocation handing the
/// same page out twice; the contents do not, because the caller syncs the data
/// file before the statement commits - see [`PagedTree::bulk_build_rows`] for the
/// order and `inillucent_wal::record::Body::BulkBuilt` for why that makes an
/// image unnecessary rather than merely cheaper.
///
/// **A logged build stamps the page with its `AllocPage` record's LSN**, exactly
/// as `log_allocated_page` stamps an ordinary page with its `WritePage` record's.
/// The page used to be left at zero, on the argument that no record names a built
/// page so the page-LSN rule has nothing to order it against. That argument holds
/// for the records this build writes and not for the ones the page's previous life
/// wrote: a page the free map hands back is some other tree's old leaf, and the
/// records that filled it sit in the log until a checkpoint moves past them.
/// Recovery replays a record onto any page stamped below it, and zero is below
/// everything, so a `CREATE INDEX` built on pages an `ALTER TABLE` had just freed
/// came back as pieces of the table it replaced (task-2055). See
/// [`inillucent_pool::Pool::write_built_page`] for the whole of it.
///
/// **An unlogged build still writes a zero stamp**, which is what the unlogged
/// builder has always produced and what keeps `ImportedDatabase::import_with`'s
/// round trip meaningful. It builds into a file nothing has read and checkpoints
/// before anything opens it, so there is no log window and no record to order a
/// stamp against.
///
/// **A page the rollback journal can put back is logged instead of written
/// straight in, and that is the rest of task-2055.** A built page's contents are
/// in the data file and in no record, so anything that moves the file backwards
/// past the build destroys them with nothing able to rebuild them - and a
/// rollback journal is exactly a thing that moves the file backwards. The
/// journal holds a pre-image of every page an eviction has written since the
/// last checkpoint, and after an `ALTER TABLE` frees a populated table's pages
/// the build is handed those very pages, so at a small buffer pool most of the
/// index landed on pages the journal could put back. The next open replayed the
/// journal, the log replayed forward and rebuilt the table, and the index came
/// back as the table's old leaves.
///
/// So the page goes through [`log_allocated_page`] in that case, which is what
/// every built page did before design 2 of task-2000: an `AllocPage`, a
/// `WritePage` carrying the whole image, and an `install`. Design 2's saving is
/// kept for every page the journal cannot put back, which is every page of a
/// build onto fresh space and the whole of the measurement it was made against.
///
/// @param log - where the allocation record goes, when there is one
/// @param database - the file the page is written into
/// @param id - the page, already allocated
/// @param image - the page bytes, stamped and checksummed in place
fn write_built_page(
    log: &mut Option<&mut dyn crate::write::TreeLog>,
    database: &mut Database,
    id: PageId,
    image: &mut [u8],
) -> DbResult<()> {
    if log.is_some() && database.pool().journal_holds(id) {
        return log_allocated_page(log, database, id, image);
    }
    let mut stamp = 0u64;
    if let Some(log) = log.as_mut() {
        stamp = log.log(inillucent_wal::record::Body::AllocPage { page: id.0 })?;
    }
    database.pool().write_built_page(id, image, stamp)
}

/// How many rows each leaf takes, and the separator that opens each one.
///
/// Pass one of the build. Nothing is encoded, allocated or written: this only asks
/// the leaf builder how many of the rows at a given offset fit, and encodes the
/// first key of each group as the separator the level above will carry.
///
/// An empty run still produces one boundary and one separator, because an empty
/// tree is still a tree: one empty leaf, so every reader has a page to land on and
/// nothing has to special-case a root that does not exist. `empty` is what tells
/// pass two to write that leaf rather than encode no rows into it.
///
/// @param builder - the leaf builder, which owns the page size and the layout
/// @param rows - the rows, already in key order
/// @param key_columns - how many leading columns are the key
/// @param encoding - how a key tuple becomes comparable bytes
/// @param collations - one per key column
/// @param directions - one per key column, true for descending
fn plan_leaves<'d>(
    builder: &LeafBuilder,
    rows: &dyn crate::leaf::Rows<'d>,
    key_columns: usize,
    encoding: KeyEncoding,
    collations: &[Collation],
    directions: &[bool],
) -> DbResult<LeafPlan> {
    let mut boundaries: Vec<usize> = Vec::new();
    let mut separators: Vec<Vec<u8>> = Vec::new();
    let mut head: Vec<Datum<'d>> = Vec::with_capacity(key_columns);
    let mut at = 0usize;
    let mut row_count = 0u64;
    let total = rows.len();
    while at < total {
        let placed = builder.fit(rows, at, BULK_FILL, true);
        if placed == 0 {
            // Every oversized text and blob would have gone out of line, so what is
            // left is keys, fixed-width slots and sixteen bytes per reference. A row
            // that still does not fit is one whose *key* is most of a page.
            return Err(misuse(
                "a row's keys and fixed-width columns alone are larger than a page",
            ));
        }
        head.clear();
        for column in 0..key_columns {
            head.push(rows.value(at, column));
        }
        let mut separator = Vec::new();
        encoding.encode_into(&head, collations, directions, &mut separator);
        separators.push(separator);
        boundaries.push(placed);
        at = at.saturating_add(placed);
        row_count = row_count.saturating_add(placed as u64);
    }
    let empty = boundaries.is_empty();
    if empty {
        boundaries.push(0);
        separators.push(Vec::new());
    }
    Ok(LeafPlan {
        boundaries,
        separators,
        row_count,
        empty,
    })
}

/// What pass one worked out about the leaves.
struct LeafPlan {
    /// How many rows each leaf holds, in order.
    boundaries: Vec<usize>,
    /// Each leaf's first key, encoded, one per boundary.
    separators: Vec<Vec<u8>>,
    /// How many rows the tree will hold.
    row_count: u64,
    /// Whether the run was empty and the single leaf is the empty one.
    empty: bool,
}

/// The leaves pass one planned, written as one run of consecutive pages.
///
/// Allocated as one run so the sibling chain is also the file's page order, which is
/// what makes a full scan sequential. One image is live at a time.
///
/// **The spiller is always supplied, logged or not.** The import builds unlogged -
/// it writes into a file nothing has read and checkpoints it - and it has to produce
/// the same tree the DDL path produces from the same rows, because
/// `ImportedDatabase::import_with` reads the catalog back and refuses if it differs
/// from what it wrote. A builder that spilled only when it had a log would make the
/// two disagree about where a four-kilobyte value lives.
///
/// @param database - the file being built into
/// @param log - where the records go, when there are any
/// @param tree_id - the tree these pages belong to
/// @param builder - the leaf builder
/// @param rows - the rows, already in key order
/// @param plan - what pass one worked out
fn write_leaf_run<'d>(
    database: &mut Database,
    log: &mut Option<&mut dyn crate::write::TreeLog>,
    tree_id: u64,
    builder: &LeafBuilder,
    rows: &dyn crate::leaf::Rows<'d>,
    plan: &LeafPlan,
) -> DbResult<LeafRun> {
    let mut nowhere = crate::write::NoLog::default();
    let leaf_pages = plan.boundaries.len();
    let first_leaf = database.allocate(leaf_pages as u64)?;
    let mut pages_written = 0u64;
    let mut leaves: Vec<PageId> = Vec::with_capacity(leaf_pages);
    let mut placed_at = 0usize;
    for (index, placed) in plan.boundaries.iter().copied().enumerate() {
        let mut image = if plan.empty {
            builder.encode_empty()?
        } else {
            let mut spiller = Extender {
                database,
                log: match log.as_deref_mut() {
                    Some(log) => log,
                    None => &mut nowhere,
                },
                tree_id,
                written: Vec::new(),
            };
            builder.encode_rows(rows, placed_at, placed, Some(&mut spiller))?
        };
        placed_at = placed_at.saturating_add(placed);
        let id = PageId(first_leaf.0.saturating_add(index as u64));
        let right = if index.saturating_add(1) < leaf_pages {
            PageId(id.0.saturating_add(1))
        } else {
            PageId::NONE
        };
        page::set_right(&mut image, right)?;
        write_built_page(log, database, id, &mut image)?;
        pages_written = pages_written.saturating_add(1);
        leaves.push(id);
    }
    Ok(LeafRun {
        leaves,
        first_leaf,
        pages_written,
    })
}

/// The leaves a build wrote.
struct LeafRun {
    /// Every leaf, in key order, which is also file order.
    leaves: Vec<PageId>,
    /// The first of them, which `Body::BulkBuilt` records.
    first_leaf: PageId,
    /// How many pages this pass wrote.
    pages_written: u64,
}

/// The interior levels, built bottom up from the leaves and their separators.
///
/// Each level groups as many children as one interior page can address, takes the
/// first child's separator as its own, and becomes the next level's children. The
/// loop ends when one page addresses the whole level, and that page is the root.
///
/// Counted rather than derived, because these pages are allocated one at a time out
/// of the free map and the run they land in is not guaranteed to follow the leaves.
/// See `Body::BulkBuilt`: `count` is how many pages the build wrote in total, and
/// the `AllocPage` records are the authority on which pages they were.
///
/// @param database - the file being built into
/// @param log - where the records go, when there are any
/// @param tree_id - the tree these pages belong to
/// @param page_size - the file's page size
/// @param leaves - the leaves, in key order
/// @param separators - each leaf's first key, one per leaf
fn build_interior_levels(
    database: &mut Database,
    log: &mut Option<&mut dyn crate::write::TreeLog>,
    tree_id: u64,
    page_size: usize,
    leaves: &[PageId],
    separators: Vec<Vec<u8>>,
) -> DbResult<Levels> {
    let mut level = 1u16;
    let mut children: Vec<PageId> = leaves.to_vec();
    let mut child_keys = separators;
    let mut root = *leaves.first().unwrap_or(&PageId::NONE);
    let mut height = 0u16;
    let mut pages_written = 0u64;
    while children.len() > 1 {
        let interior = InteriorBuilder::new(page_size, tree_id, level)?;
        let mut parents: Vec<PageId> = Vec::new();
        let mut parent_keys: Vec<Vec<u8>> = Vec::new();
        let mut cursor = 0usize;
        while cursor < children.len() {
            let remaining_keys: Vec<&[u8]> = child_keys
                .get(cursor.saturating_add(1)..)
                .unwrap_or(&[])
                .iter()
                .map(|key| key.as_slice())
                .collect();
            let fit = interior
                .capacity(&remaining_keys)
                .min(children.len().saturating_sub(cursor))
                .max(1);
            let group: Vec<Swip> = children
                .get(cursor..cursor.saturating_add(fit))
                .unwrap_or(&[])
                .iter()
                .map(|page| Swip::unswizzled(*page))
                .collect();
            let group_separators: Vec<&[u8]> = child_keys
                .get(cursor.saturating_add(1)..cursor.saturating_add(fit))
                .unwrap_or(&[])
                .iter()
                .map(|key| key.as_slice())
                .collect();
            let mut image = interior.build(&group_separators, &group)?;
            let id = database.allocate(1)?;
            write_built_page(log, database, id, &mut image)?;
            pages_written = pages_written.saturating_add(1);
            parents.push(id);
            parent_keys.push(child_keys.get(cursor).cloned().unwrap_or_default());
            cursor = cursor.saturating_add(fit);
        }
        children = parents;
        child_keys = parent_keys;
        height = level;
        level = level.saturating_add(1);
        root = *children.first().unwrap_or(&PageId::NONE);
        if level > 32 {
            return Err(corrupt(
                "the bulk builder made a tree deeper than 32 levels",
            ));
        }
    }
    Ok(Levels {
        root,
        height,
        pages_written,
    })
}

/// What the interior pass produced.
struct Levels {
    /// The page every descent starts at.
    root: PageId,
    /// How many interior levels there are above the leaves.
    height: u16,
    /// How many pages this pass wrote.
    pages_written: u64,
}

impl PagedTree {
    /// Builds a tree bottom-up from rows already sorted by key.
    ///
    /// Leaves are packed left to right at [`BULK_FILL`] and never revisited,
    /// then each interior level is built from the level below it, then the root
    /// is whatever the last level left. One pass, no per-key descent - the
    /// TDD's bulk builder, and what the fixture import uses.
    ///
    /// @param database - the file the pages are allocated and installed in
    /// @param tree_id - the identifier stamped into every page
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param rows - the rows, already sorted by the key columns
    pub fn bulk_build<'d, R: AsRef<[Datum<'d>]>>(
        database: &mut Database,
        tree_id: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        rows: &[R],
    ) -> DbResult<PagedTree> {
        PagedTree::bulk_build_logged(database, None, tree_id, columns, key_columns, rows)
    }

    /// Builds a tree bottom-up, describing every page in the log first.
    ///
    /// The same builder as [`PagedTree::bulk_build`], with the write-ahead rule
    /// applied to it: every page it allocates is an `AllocPage` record and every
    /// page it packs is a `WritePage` record carrying the whole image, so redo
    /// is a copy and a `CREATE INDEX` that crashed half-way is either wholly
    /// there after recovery or wholly absent.
    ///
    /// A whole-image record per page is the right record here and a small one
    /// would be wrong. The logical-redo argument that made a compaction
    /// twenty-four bytes in Phase 3 relies on the page already being in the
    /// state the operation started from; a bulk build's pages did not exist
    /// before it, so there is no such state and the image *is* the instruction.
    ///
    /// `None` for the log is the import's case: it builds into a file nothing
    /// has read, checkpoints it, and reopens it, so there is no window in which
    /// a log would be consulted. Passing `None` writes the same bytes the
    /// unlogged builder always wrote, LSN field included, which is what keeps
    /// the import's byte-for-byte round-trip check meaningful.
    ///
    /// @param database - the file the pages are allocated and installed in
    /// @param log - where the records go, when the build is inside a transaction
    /// @param tree_id - the identifier stamped into every page
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param rows - the rows, already sorted by the key columns
    pub fn bulk_build_logged<'d, R: AsRef<[Datum<'d>]>>(
        database: &mut Database,
        log: Option<&mut dyn crate::write::TreeLog>,
        tree_id: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        rows: &[R],
    ) -> DbResult<PagedTree> {
        PagedTree::bulk_build_rows(
            database,
            log,
            tree_id,
            columns,
            key_columns,
            &crate::leaf::RowSlice(rows),
        )
    }

    /// Builds a tree bottom-up from a row source, holding one leaf at a time.
    ///
    /// **Two passes over the sizing arithmetic, and one leaf image in memory.**
    /// The builder allocates its leaves as one contiguous run, so it has to know
    /// the leaf count before it writes the first one. It used to find
    /// that out by packing every leaf into a `Vec<Vec<u8>>` and taking its
    /// length, which is a whole second copy of the tree held live for the sake
    /// of one integer: 6.2 MiB of a `CREATE INDEX` whose entire resident cost
    /// was 28.9 MiB.
    ///
    /// [`LeafBuilder::fit`] answers the same question without encoding
    /// anything, so the count is taken first, the run is allocated, and each
    /// leaf is then packed into a buffer that is logged and dropped before the
    /// next one is made. The two passes see the same values and price the same
    /// spill threshold, so they agree by construction rather than by luck - and
    /// the counting pass writes nothing, which is what makes running it twice
    /// safe.
    ///
    /// One thing does move: an oversized value's extent pages are now allocated
    /// **after** the leaf run rather than interleaved with it, because the run
    /// is claimed before any packing happens. The leaves are therefore *more*
    /// contiguous than they were, which is the property the run exists for.
    ///
    /// @param database - the file the pages are allocated and installed in
    /// @param log - where the records go, when the build is inside a transaction
    /// @param tree_id - the identifier stamped into every page
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param rows - the rows, already sorted by the key columns
    pub fn bulk_build_rows<'d>(
        database: &mut Database,
        mut log: Option<&mut dyn crate::write::TreeLog>,
        tree_id: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        rows: &dyn crate::leaf::Rows<'d>,
    ) -> DbResult<PagedTree> {
        let page_size = database.page_size();
        let encoding = KeyEncoding::choose(&columns, key_columns);
        let collations = collations_of(&columns, key_columns);
        let directions = directions_of(&columns, key_columns);
        let builder = LeafBuilder::new(page_size, tree_id, columns.clone(), key_columns)?;

        let plan = plan_leaves(
            &builder,
            rows,
            key_columns,
            encoding,
            &collations,
            &directions,
        )?;
        let row_count = plan.row_count;
        let run = write_leaf_run(database, &mut log, tree_id, &builder, rows, &plan)?;
        let leaves = run.leaves;
        let first_leaf = run.first_leaf;
        let levels = build_interior_levels(
            database,
            &mut log,
            tree_id,
            page_size,
            &leaves,
            plan.separators,
        )?;
        let root = levels.root;
        let height = levels.height;
        let pages_written = run.pages_written.saturating_add(levels.pages_written);
        // **The data file is synced before the statement's own records are, and
        // that is the whole of design 2's safety argument** (task-2000). Every page
        // this build wrote went straight into the file with no log record behind
        // it, so the pages have to be on the media before the commit that names
        // the root is. A crash before the commit's log sync leaves a catalog that
        // never named the root and `AllocPage` records belonging to an uncommitted
        // transaction, which are not replayed - so the pages are still free. A
        // crash after it leaves pages that were durable first. A torn page cannot
        // exist at the commit point, because this sync preceded it.
        database.pool().sync_data_file()?;
        // And one record naming what happened, which recovery applies nothing for.
        // Without it the log holds `AllocPage` records for a run of pages whose
        // contents no record describes, which a reader cannot tell from a gap. See
        // `inillucent_wal::record::Body::BulkBuilt`.
        if let Some(log) = log.as_mut() {
            log.log(inillucent_wal::record::Body::BulkBuilt {
                root: root.0,
                first: first_leaf.0,
                count: pages_written,
            })?;
        }
        let collations = collations_of(&columns, key_columns);
        let directions = directions_of(&columns, key_columns);
        Ok(PagedTree {
            tree_id,
            root,
            height,
            columns,
            key_columns,
            page_size,
            encoding,
            collations,
            directions,
            first_leaf: *leaves.first().unwrap_or(&PageId::NONE),
            leaf_count: leaves.len() as u64,
            row_count,
            scratch: RefCell::new(Vec::new()),
            leaf_hints: std::cell::RefCell::new(Vec::new()),
            hint_victim: std::cell::Cell::new(0),
            stats: std::cell::Cell::new(crate::write::WriteStats::default()),
        })
    }
}
