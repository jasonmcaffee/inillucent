//! Mutating a B-tree: insert, replace, delete, and the balancing that follows.
//!
//! Invariant: after every operation the tree is one a reader would accept -
//! every page valid, every key in order, every leaf at the same depth, and
//! every page owned exactly once. Nothing here leaves a tree in a state that is
//! only legal until the next call fixes it, because the failure injection
//! campaign stops *between* any two calls and looks.
//!
//! # Why balancing is written on owned cells
//!
//! The hard part of a B-tree writer is not the insert, it is what happens when
//! the page is full: cells move between pages, pages appear and disappear, and
//! the parent's divider cells have to be rewritten to match. Doing that by
//! editing pages in place means holding several pages at once and reasoning
//! about offsets that move underneath you, and it is where B-tree
//! implementations traditionally go wrong.
//!
//! So this one does not. A balance decodes the pages it is going to touch into
//! an ordered list of owned [`Entry`] values, decides how to lay them out again,
//! and writes each page from scratch. No offset into a page survives a
//! reorganisation, because no offset is kept. The cost is a page rewrite where
//! SQLite would shuffle bytes; the benefit is that the partitioning step is a
//! function from a list to a list of lists, which can be reasoned about and
//! tested on its own.
//!
//! # The one asymmetry between table and index trees
//!
//! When a page splits, something has to become the divider in the parent. In an
//! *index* tree the divider is a real entry and it *moves* out of the child - an
//! index B-tree stores entries on interior pages, so the entry is still in the
//! tree, just one level up. In a *table* tree an interior cell carries only a
//! child pointer and the largest rowid below it, so the divider is *derived*
//! from the last entry of the page and the entry itself stays on the leaf.
//! Getting that backwards duplicates or loses a row per split, which is why it
//! is decided in one place, [`promotes_dividers`], and read from there.

use std::sync::Arc;

use inillucent_base::error::corrupt;
use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::{bytes, varint, DbResult};
use inillucent_value::record::{self, KeyInfo, RecordRef};
use inillucent_value::TextEncoding;

use crate::btree::{payload_split, BTreePage, PageKind, PageLayout};
use crate::cursor::TreeKind;
use crate::edit;
use crate::header::VacuumMode;
use crate::overflow;
use crate::pager::Pager;
use crate::{alloc, ptrmap};

/// How many sibling pages a balance considers at once.
///
/// Three is SQLite's number and it is a compromise rather than a law: two
/// cannot merge a page into its neighbours without cascading, and four reads
/// and writes more pages than the improvement in occupancy is worth.
const BALANCE_WINDOW: usize = 3;

/// A page is balanced when it is emptier than this fraction of its capacity.
///
/// The format imposes no minimum occupancy - a page holding one cell is a legal
/// page - so this is a policy, and a cheap one: a delete that leaves a page
/// nearly empty pays for a balance, and one that does not leaves the tree
/// alone. Without it a tree that is filled and then half emptied keeps every
/// page it ever allocated.
const UNDERFULL_NUMERATOR: usize = 1;
/// The denominator of the underfull fraction.
const UNDERFULL_DENOMINATOR: usize = 3;

/// The deepest path any legal B-tree can have, which bounds every descent.
const MAX_DEPTH: usize = 64;

/// Which tree is being mutated, and how its keys compare.
#[derive(Clone, Debug)]
pub struct Tree {
    /// The tree's root page.
    pub root: PageId,
    /// Whether it is keyed by rowid or by record.
    pub kind: TreeKind,
    /// The key ordering, for an index.
    pub key: KeyInfo,
}

impl Tree {
    /// Names a table B-tree.
    pub fn table(root: PageId) -> Tree {
        Tree {
            root,
            kind: TreeKind::Table,
            key: KeyInfo::default(),
        }
    }

    /// Names an index B-tree with the ordering its declaration gives it.
    pub fn index(root: PageId, key: KeyInfo) -> Tree {
        Tree {
            root,
            kind: TreeKind::Index,
            key,
        }
    }

    /// Returns the kind of page an interior page of this tree is.
    fn interior_kind(&self) -> PageKind {
        match self.kind {
            TreeKind::Table => PageKind::InteriorTable,
            TreeKind::Index => PageKind::InteriorIndex,
        }
    }
}

/// Reports whether a split of pages of this kind moves an entry up into the
/// parent, or derives the divider from an entry that stays put.
///
/// See the note at the top of the module: this is the one place the difference
/// between a table B-tree and an index B-tree is decided.
fn promotes_dividers(tree: &Tree, children_are_leaves: bool) -> bool {
    !(tree.kind == TreeKind::Table && children_are_leaves)
}

/// One entry as it moves between pages during a balance.
///
/// `body` is the entry in its *leaf* form: for a table leaf that is the whole
/// cell, for an index page it is everything after the child pointer, and for a
/// table interior page there is no body at all because the cell holds nothing
/// but a pointer and a key.
#[derive(Clone, Debug)]
struct Entry {
    /// The entry's bytes in leaf form.
    body: Vec<u8>,
    /// The subtree hanging below this entry, on an interior page.
    child: Option<PageId>,
    /// The key, in a table tree.
    rowid: Option<i64>,
}

/// What a page is about to hold.
#[derive(Clone, Debug)]
struct PageContent {
    /// Which kind of page it is.
    kind: PageKind,
    /// Its entries, in key order.
    entries: Vec<Entry>,
    /// The right-most child, on an interior page.
    right: Option<PageId>,
}

impl PageContent {
    /// Returns how many bytes these entries need on a page of this kind.
    fn size(&self) -> DbResult<usize> {
        let mut total = 0usize;
        for entry in &self.entries {
            total = total
                .saturating_add(edit::cell_footprint(entry_size(entry, self.kind)?))
                .saturating_add(2);
        }
        Ok(total)
    }

    /// Encodes every entry as a cell of this page's kind.
    fn cells(&self) -> DbResult<Vec<Vec<u8>>> {
        self.entries
            .iter()
            .map(|entry| encode_entry(entry, self.kind))
            .collect()
    }
}

/// One page on the path from the root to the page being changed.
#[derive(Clone, Copy, Debug)]
struct Step {
    /// The page.
    page: PageId,
    /// Which child of it the path descended through, or which cell the search
    /// stopped on when the page is where the entry lives.
    slot: usize,
}

/// Returns how long one entry's cell would be, without building it.
///
/// A balance asks this for every entry of every page in its window, twice, to
/// decide where the page boundaries fall. Answering it by encoding the cell
/// allocated and copied a whole entry per question - on a hundred thousand row
/// index build that was most of the cost of every page split, to learn a length
/// that is arithmetic.
/// @param entry - the entry to measure
/// @param kind - the kind of page it would live on
fn entry_size(entry: &Entry, kind: PageKind) -> DbResult<usize> {
    match kind {
        PageKind::LeafTable | PageKind::LeafIndex => Ok(entry.body.len()),
        PageKind::InteriorIndex => Ok(entry.body.len().saturating_add(4)),
        PageKind::InteriorTable => {
            let rowid = entry
                .rowid
                .ok_or_else(|| corrupt("a table interior entry with no rowid"))?;
            let mut scratch = [0u8; varint::MAX_LEN];
            let len = varint::encode_i64(&mut scratch, rowid)?;
            Ok(len.saturating_add(4))
        }
    }
}

/// Encodes one entry as a cell of the given page kind.
fn encode_entry(entry: &Entry, kind: PageKind) -> DbResult<Vec<u8>> {
    match kind {
        PageKind::LeafTable | PageKind::LeafIndex => Ok(entry.body.clone()),
        PageKind::InteriorIndex => {
            let child = entry
                .child
                .ok_or_else(|| corrupt("an index interior entry with no child"))?;
            let mut cell = Vec::with_capacity(entry.body.len().saturating_add(4));
            cell.extend_from_slice(&child.get().to_be_bytes());
            cell.extend_from_slice(&entry.body);
            Ok(cell)
        }
        PageKind::InteriorTable => {
            let child = entry
                .child
                .ok_or_else(|| corrupt("a table interior entry with no child"))?;
            let rowid = entry
                .rowid
                .ok_or_else(|| corrupt("a table interior entry with no rowid"))?;
            let mut cell = Vec::with_capacity(12);
            cell.extend_from_slice(&child.get().to_be_bytes());
            let mut scratch = [0u8; varint::MAX_LEN];
            let len = varint::encode_i64(&mut scratch, rowid)?;
            cell.extend_from_slice(
                scratch
                    .get(..len)
                    .ok_or_else(|| corrupt("a rowid varint width"))?,
            );
            Ok(cell)
        }
    }
}

/// Reads a page's validated layout.
fn read_layout(pager: &mut Pager, page: PageId) -> DbResult<Arc<PageLayout>> {
    let usable = pager.usable_size()?;
    let pin = pager.get_page(page)?;
    pin.layout(usable)
}

/// Reads a page's whole payload, following its overflow chain when it has one.
fn cell_payload(pager: &mut Pager, page: PageId, index: usize) -> DbResult<Vec<u8>> {
    let layout = read_layout(pager, page)?;
    let pin = pager.get_page(page)?;
    let view = BTreePage::new(pin.bytes(), &layout);
    let cell = view.cell(index)?;
    let local = cell.local_payload.to_vec();
    let total = cell.split.total;
    let head = cell.overflow;
    drop(pin);
    let limits = Limits::default();
    overflow::read_payload(pager, &local, total, head, &limits)
}

/// Decodes a page into the entries a balance moves around.
fn gather_page(pager: &mut Pager, page: PageId) -> DbResult<PageContent> {
    let layout = read_layout(pager, page)?;
    let pin = pager.get_page(page)?;
    let view = BTreePage::new(pin.bytes(), &layout);
    let mut entries = Vec::with_capacity(layout.cell_count);
    for index in 0..layout.cell_count {
        let cell = view.cell(index)?;
        let raw = bytes::window(pin.bytes(), cell.offset, cell.len)?;
        let body = match layout.kind {
            PageKind::LeafTable | PageKind::LeafIndex => raw.to_vec(),
            PageKind::InteriorIndex => raw
                .get(4..)
                .ok_or_else(|| corrupt("an index interior cell with no body"))?
                .to_vec(),
            PageKind::InteriorTable => Vec::new(),
        };
        entries.push(Entry {
            body,
            child: cell.left_child,
            rowid: cell.rowid,
        });
    }
    Ok(PageContent {
        kind: layout.kind,
        entries,
        right: layout.right_child,
    })
}

/// Writes a page from its content, and refreshes what it points at.
fn write_page(pager: &mut Pager, page: PageId, content: &PageContent) -> DbResult<()> {
    let usable = pager.usable_size()?;
    let base = if page.get() == 1 { 100 } else { 0 };
    let cells = content.cells()?;
    let kind = content.kind;
    let right = content.right;
    pager.edit_page(page, |raw| {
        edit::rewrite_page(raw, base, kind, usable, &cells, right)
    })?;
    ptrmap::refresh_btree_page(pager, page)
}

/// Returns how many bytes a page of this kind has for cells and pointers.
fn capacity_of(pager: &Pager, page: PageId, kind: PageKind) -> DbResult<usize> {
    let usable = pager.usable_size()?;
    let base = if page.get() == 1 { 100 } else { 0 };
    Ok(edit::capacity(base, kind, usable))
}

/// Creates an empty table B-tree and returns its root page.
pub fn create_table(pager: &mut Pager) -> DbResult<PageId> {
    create_tree(pager, PageKind::LeafTable)
}

/// Creates an empty index B-tree and returns its root page.
pub fn create_index(pager: &mut Pager) -> DbResult<PageId> {
    create_tree(pager, PageKind::LeafIndex)
}

/// Allocates a root page and writes an empty leaf into it.
fn create_tree(pager: &mut Pager, kind: PageKind) -> DbResult<PageId> {
    let root = if pager.header().vacuum_mode == VacuumMode::None {
        alloc::allocate_page(pager)?
    } else {
        allocate_low_root(pager)?
    };
    let usable = pager.usable_size()?;
    pager.edit_page(root, |raw| {
        edit::initialize_btree_page(raw, 0, kind, usable)
    })?;
    ptrmap::put(pager, root, ptrmap::Entry::root())?;
    if pager.header().vacuum_mode != VacuumMode::None {
        let mut header = *pager.header();
        if root.get() > header.largest_root {
            header.largest_root = root.get();
            pager.set_header(header)?;
        }
    }
    Ok(root)
}

/// Places a new root at the lowest page number that is not already a root.
///
/// Only an auto-vacuum database needs this, and it needs it badly: a root is
/// the one page a vacuum cannot move, because its number is written in
/// `sqlite_schema` and storage cannot rewrite a column. A root allocated at the
/// end of the file would therefore sit in front of every page a vacuum wants to
/// reclaim and stop the vacuum dead. Keeping roots packed at the bottom means
/// the trailing pages are always movable ones. SQLite does exactly this in
/// `sqlite3BtreeCreateTable`, and whatever is living at the wanted page gets
/// moved out of the way - which is the same relocation a vacuum performs.
fn allocate_low_root(pager: &mut Pager) -> DbResult<PageId> {
    let lock_byte = alloc::lock_byte_page(pager.page_size());
    let mut target = pager.header().largest_root.max(1).saturating_add(1);
    loop {
        let page = PageId::from_persisted(target)?;
        if ptrmap::is_map_page(pager, page)? || target == lock_byte {
            target = target.saturating_add(1);
            continue;
        }
        break;
    }
    let root = PageId::from_persisted(target)?;
    if target > pager.page_count() {
        alloc::allocate_exact(pager, root)?;
        return Ok(root);
    }
    match ptrmap::get(pager, root)? {
        Some(entry) if entry.kind == ptrmap::FREE_PAGE => alloc::allocate_exact(pager, root)?,
        _ => {
            let elsewhere = alloc::allocate_page(pager)?;
            crate::vacuum::relocate_page(pager, root, elsewhere)?;
            // Nothing points at the page any more, and it was never on the
            // freelist, so it is the caller's without going through either.
            pager.record_allocated(root);
            pager.count_allocation();
            pager.edit_page(root, |raw| {
                raw.fill(0);
                Ok(())
            })?;
        }
    }
    Ok(root)
}

/// Frees every page a tree owns, including its root.
pub fn drop_tree(pager: &mut Pager, root: PageId) -> DbResult<()> {
    clear_tree(pager, root)?;
    alloc::free_page(pager, root)
}

/// Frees everything below a tree's root, leaving an empty tree.
pub fn clear_tree(pager: &mut Pager, root: PageId) -> DbResult<()> {
    let kind = read_layout(pager, root)?.kind;
    free_subtree(pager, root, true)?;
    let usable = pager.usable_size()?;
    let leaf = if kind.is_table() {
        PageKind::LeafTable
    } else {
        PageKind::LeafIndex
    };
    let base = if root.get() == 1 { 100 } else { 0 };
    pager.edit_page(root, |raw| {
        edit::initialize_btree_page(raw, base, leaf, usable)
    })?;
    Ok(())
}

/// Frees a page's overflow chains and its children, and then the page itself
/// unless it is the root the caller is keeping.
fn free_subtree(pager: &mut Pager, page: PageId, is_root: bool) -> DbResult<()> {
    let layout = read_layout(pager, page)?;
    let pin = pager.get_page(page)?;
    let view = BTreePage::new(pin.bytes(), &layout);
    let mut children = Vec::new();
    let mut chains = Vec::new();
    for index in 0..layout.cell_count {
        let cell = view.cell(index)?;
        if let Some(child) = cell.left_child {
            children.push(child);
        }
        if cell.split.overflows {
            chains.push((cell.split.total, cell.split.local, cell.overflow));
        }
    }
    if let Some(right) = layout.right_child {
        children.push(right);
    }
    drop(pin);
    for (total, local, head) in chains {
        overflow::free_chain(pager, total, local, head)?;
    }
    for child in children {
        free_subtree(pager, child, false)?;
    }
    if !is_root {
        alloc::free_page(pager, page)?;
    }
    Ok(())
}

/// Inserts or replaces a row in a table B-tree.
pub fn insert_row(pager: &mut Pager, root: PageId, rowid: i64, payload: &[u8]) -> DbResult<()> {
    let tree = Tree::table(root);
    let (path, found) = find_rowid(pager, &tree, rowid)?;
    let leaf = path
        .last()
        .copied()
        .ok_or_else(|| corrupt("a descent that reached no page"))?;
    if found {
        remove_cell_at(pager, leaf.page, leaf.slot)?;
    }
    let cell = build_cell(pager, PageKind::LeafTable, None, Some(rowid), payload)?;
    place_cell(pager, &tree, path, cell)
}

/// Deletes a row from a table B-tree, reporting whether it was there.
pub fn delete_row(pager: &mut Pager, root: PageId, rowid: i64) -> DbResult<bool> {
    let tree = Tree::table(root);
    let (path, found) = find_rowid(pager, &tree, rowid)?;
    if !found {
        return Ok(false);
    }
    delete_at(pager, &tree, path)?;
    Ok(true)
}

/// Inserts an entry into an index B-tree, replacing an identical one.
pub fn insert_entry(
    pager: &mut Pager,
    root: PageId,
    key: &KeyInfo,
    payload: &[u8],
) -> DbResult<bool> {
    let tree = Tree::index(root, key.clone());
    #[cfg(feature = "opcode-probe")]
    let stage = std::time::Instant::now();
    let (path, found) = find_key(pager, &tree, payload)?;
    #[cfg(feature = "opcode-probe")]
    inillucent_base::probe::record_stage(5, stage.elapsed().as_nanos() as u64);
    if found {
        // The entry is where the search stopped, which for an index may be an
        // interior page; replacing it in place would need the same balancing as
        // a delete followed by an insert, so that is what this is.
        delete_at(pager, &tree, path)?;
        let (path, _) = find_key(pager, &tree, payload)?;
        let cell = build_cell(pager, PageKind::LeafIndex, None, None, payload)?;
        place_cell(pager, &tree, path, cell)?;
        return Ok(true);
    }
    #[cfg(feature = "opcode-probe")]
    let stage = std::time::Instant::now();
    let cell = build_cell(pager, PageKind::LeafIndex, None, None, payload)?;
    #[cfg(feature = "opcode-probe")]
    let stage = {
        inillucent_base::probe::record_stage(6, stage.elapsed().as_nanos() as u64);
        std::time::Instant::now()
    };
    place_cell(pager, &tree, path, cell)?;
    #[cfg(feature = "opcode-probe")]
    inillucent_base::probe::record_stage(7, stage.elapsed().as_nanos() as u64);
    Ok(false)
}

/// Deletes an entry from an index B-tree, reporting whether it was there.
pub fn delete_entry(
    pager: &mut Pager,
    root: PageId,
    key: &KeyInfo,
    payload: &[u8],
) -> DbResult<bool> {
    let tree = Tree::index(root, key.clone());
    let (path, found) = find_key(pager, &tree, payload)?;
    if !found {
        return Ok(false);
    }
    delete_at(pager, &tree, path)?;
    Ok(true)
}

/// Adds a row after every row already in a table B-tree, without comparing
/// keys.
///
/// The caller promises the rowid is larger than every one already there. That
/// promise is what makes a bulk copy possible: nothing is compared, so nothing
/// depends on knowing how the source's keys were declared to sort - which is
/// the one thing storage does not know and cannot look up.
pub fn append_row(pager: &mut Pager, root: PageId, rowid: i64, payload: &[u8]) -> DbResult<()> {
    let tree = Tree::table(root);
    let path = rightmost_path(pager, root)?;
    let cell = build_cell(pager, PageKind::LeafTable, None, Some(rowid), payload)?;
    place_cell(pager, &tree, path, cell)
}

/// Adds an entry after every entry already in an index B-tree, without
/// comparing keys.
pub fn append_entries(pager: &mut Pager, root: PageId, payloads: &[Vec<u8>]) -> DbResult<()> {
    let mut at = 0usize;
    while let Some(rest) = payloads.get(at..).filter(|rest| !rest.is_empty()) {
        let placed = append_run(pager, root, rest)?;
        if placed > 0 {
            at = at.saturating_add(placed);
            continue;
        }
        // Nothing fitted on the current rightmost leaf, so this one goes
        // through the ordinary path and takes the split with it.
        let Some(payload) = payloads.get(at) else {
            break;
        };
        append_entry(pager, root, payload)?;
        at = at.saturating_add(1);
    }
    Ok(())
}

/// Appends as many entries as the rightmost leaf holds, in one page edit.
///
/// Returns how many it placed, which is zero when the next entry does not fit
/// and the caller has to take a split.
///
/// One edit per *page* rather than one per entry is the whole point. A page
/// edit copies the page, re-derives its layout and publishes a new frame so
/// that anything holding the old one still sees a whole page - which is the
/// right thing to pay once for fifty entries and the wrong thing to pay fifty
/// times. Measured on a hundred thousand row index build: 33 microseconds per
/// entry became under one.
/// @param pager - the database being written
/// @param root - the index's root page
/// @param payloads - the entries still to place, in key order
fn append_run(pager: &mut Pager, root: PageId, payloads: &[Vec<u8>]) -> DbResult<usize> {
    let path = rightmost_path(pager, root)?;
    let Some(leaf) = path.last().copied() else {
        return Err(corrupt("a descent that reached no page"));
    };
    let usable = pager.usable_size()?;
    let layout = read_layout(pager, leaf.page)?;
    if !layout.kind.is_leaf() {
        return Ok(0);
    }
    // How many fit, decided before anything is written. `free_bytes` is what
    // `try_insert_in_place` asks, and asking it here for the whole run keeps
    // the two answers the same.
    let free = {
        let pin = pager.get_page(leaf.page)?;
        edit::free_bytes(&BTreePage::new(pin.bytes(), &layout))?
    };
    let mut taken = 0usize;
    let mut used = 0usize;
    for payload in payloads {
        // An entry that overflows is left to the ordinary path, which knows how
        // to build its chain.
        if would_overflow(usable, payload.len()) {
            break;
        }
        let footprint = edit::cell_footprint(payload.len()).saturating_add(2);
        if used.saturating_add(footprint) > free {
            break;
        }
        used = used.saturating_add(footprint);
        taken = taken.saturating_add(1);
    }
    if taken == 0 {
        return Ok(0);
    }

    let cells: Vec<Vec<u8>> = payloads
        .iter()
        .take(taken)
        .map(|payload| build_cell(pager, PageKind::LeafIndex, None, None, payload))
        .collect::<DbResult<Vec<Vec<u8>>>>()?;
    let placed = pager.edit_page(leaf.page, |raw| {
        let mut at = leaf.slot;
        let mut done = 0usize;
        for cell in &cells {
            // The layout moves with every insert, so it is re-derived rather
            // than remembered. It is a walk of the page's own pointer array,
            // which is cheap beside the copy and publish this loop exists to
            // avoid doing per entry.
            let layout = PageLayout::parse_edited(raw, leaf.page, usable)?;
            if !edit::insert_cell(raw, &layout, at, cell)? {
                break;
            }
            at = at.saturating_add(1);
            done = done.saturating_add(1);
        }
        Ok(done)
    })?;
    if placed > 0 {
        ptrmap::refresh_btree_page(pager, leaf.page)?;
    }
    Ok(placed)
}

/// Returns whether a payload of this size needs an overflow chain.
fn would_overflow(usable: u32, payload: usize) -> bool {
    crate::btree::PayloadWindow::new(usable, PageKind::LeafIndex)
        .and_then(|window| window.split(payload as u64))
        .map(|split| split.overflows)
        .unwrap_or(true)
}

/// Appends one entry after the last one in a tree.
pub fn append_entry(pager: &mut Pager, root: PageId, payload: &[u8]) -> DbResult<()> {
    let tree = Tree::index(root, KeyInfo::default());
    let path = rightmost_path(pager, root)?;
    let cell = build_cell(pager, PageKind::LeafIndex, None, None, payload)?;
    place_cell(pager, &tree, path, cell)
}

/// Returns the path to the position after the last entry in a tree.
fn rightmost_path(pager: &mut Pager, root: PageId) -> DbResult<Vec<Step>> {
    let mut path = Vec::new();
    let mut page = root;
    loop {
        let layout = read_layout(pager, page)?;
        if layout.kind.is_leaf() {
            path.push(Step {
                page,
                slot: layout.cell_count,
            });
            return Ok(path);
        }
        let slot = layout.cell_count;
        let pin = pager.get_page(page)?;
        let child = BTreePage::new(pin.bytes(), &layout).child_at(slot)?;
        drop(pin);
        path.push(Step { page, slot });
        page = child;
        if path.len() > MAX_DEPTH {
            return Err(corrupt("a B-tree path deeper than any legal tree"));
        }
    }
}

/// Returns the kind of tree a root page holds.
pub fn tree_kind(pager: &mut Pager, root: PageId) -> DbResult<TreeKind> {
    let layout = read_layout(pager, root)?;
    Ok(if layout.kind.is_table() {
        TreeKind::Table
    } else {
        TreeKind::Index
    })
}

/// Builds a cell, writing any part of its payload that does not fit locally
/// into a fresh overflow chain.
fn build_cell(
    pager: &mut Pager,
    kind: PageKind,
    child: Option<PageId>,
    rowid: Option<i64>,
    payload: &[u8],
) -> DbResult<Vec<u8>> {
    let usable = pager.usable_size()?;
    let split = payload_split(payload.len() as u64, usable, kind)?;
    let head = if split.overflows {
        let tail = payload
            .get(split.local..)
            .ok_or_else(|| corrupt("a payload shorter than its own local part"))?;
        overflow::write_chain(pager, tail)?
    } else {
        None
    };
    edit::encode_cell(kind, usable, child, rowid, payload, head)
}

/// Removes the cell at `index` from a page, freeing its overflow chain.
fn remove_cell_at(pager: &mut Pager, page: PageId, index: usize) -> DbResult<()> {
    let layout = read_layout(pager, page)?;
    let pin = pager.get_page(page)?;
    let view = BTreePage::new(pin.bytes(), &layout);
    let cell = view.cell(index)?;
    let chain = if cell.split.overflows {
        Some((cell.split.total, cell.split.local, cell.overflow))
    } else {
        None
    };
    drop(pin);
    if let Some((total, local, head)) = chain {
        overflow::free_chain(pager, total, local, head)?;
    }
    let layout = read_layout(pager, page)?;
    pager.edit_page(page, |raw| edit::remove_cell(raw, &layout, index))
}

/// Puts a cell on the leaf the path ends at, balancing when it does not fit.
fn place_cell(pager: &mut Pager, tree: &Tree, path: Vec<Step>, cell: Vec<u8>) -> DbResult<()> {
    let leaf = path
        .last()
        .copied()
        .ok_or_else(|| corrupt("a descent that reached no page"))?;
    if try_insert_in_place(pager, leaf.page, leaf.slot, &cell)? {
        ptrmap::refresh_btree_page(pager, leaf.page)?;
        #[cfg(feature = "opcode-probe")]
        inillucent_base::probe::record_stage(8, 0);
        return Ok(());
    }
    // Reached only when the cell does not fit, which is the balancing path.
    #[cfg(feature = "opcode-probe")]
    let stage = std::time::Instant::now();
    let mut content = gather_page(pager, leaf.page)?;
    let entry = decode_entry(&cell, content.kind)?;
    if leaf.slot > content.entries.len() {
        return Err(corrupt("an insertion point past the end of a page"));
    }
    content.entries.insert(leaf.slot, entry);
    let depth = path.len().saturating_sub(1);
    let outcome = balance(pager, tree, &path, depth, content);
    #[cfg(feature = "opcode-probe")]
    inillucent_base::probe::record_stage(9, stage.elapsed().as_nanos() as u64);
    outcome
}

/// Tries to put a cell on a page without moving anything between pages.
fn try_insert_in_place(
    pager: &mut Pager,
    page: PageId,
    index: usize,
    cell: &[u8],
) -> DbResult<bool> {
    let layout = read_layout(pager, page)?;
    let needed = edit::cell_footprint(cell.len()).saturating_add(2);
    let pin = pager.get_page(page)?;
    let free = edit::free_bytes(&BTreePage::new(pin.bytes(), &layout))?;
    drop(pin);
    if free < needed {
        return Ok(false);
    }
    let owned = cell.to_vec();
    let placed = pager.edit_page(page, |raw| edit::insert_cell(raw, &layout, index, &owned))?;
    if placed {
        return Ok(true);
    }
    // The room is there but no single hole holds it, which is what
    // defragmenting is for. SQLite does exactly this before it gives up.
    let usable = pager.usable_size()?;
    let owned = cell.to_vec();
    pager.edit_page(page, |raw| {
        edit::defragment(raw, &layout)?;
        let fresh = PageLayout::parse(raw, page, usable)?;
        edit::insert_cell(raw, &fresh, index, &owned)
    })
}

/// Decodes a freshly built cell back into an entry.
fn decode_entry(cell: &[u8], kind: PageKind) -> DbResult<Entry> {
    match kind {
        PageKind::LeafIndex => Ok(Entry {
            body: cell.to_vec(),
            child: None,
            rowid: None,
        }),
        PageKind::LeafTable => {
            let payload =
                varint::decode(cell).map_err(|_| corrupt("a truncated payload length"))?;
            let rest = cell
                .get(payload.len..)
                .ok_or_else(|| corrupt("a cell that ends inside its payload length"))?;
            let (rowid, _) =
                varint::decode_i64(rest).map_err(|_| corrupt("a truncated rowid varint"))?;
            Ok(Entry {
                body: cell.to_vec(),
                child: None,
                rowid: Some(rowid),
            })
        }
        _ => Err(corrupt("a leaf cell was expected")),
    }
}

/// Removes the entry the path ends on, rebalancing what that leaves behind.
///
/// A leaf entry is simply taken out. An entry on an *interior* page cannot be:
/// it is a divider, and removing it would leave two subtrees with nothing
/// between them. So it is replaced by its predecessor - the largest entry in the
/// subtree to its left, which is always on a leaf - and that leaf entry is
/// removed instead. Only an index tree reaches this case, because a table
/// tree's interior cells are not entries.
///
/// The predecessor is taken off its leaf *first*, and the divider is then found
/// again by key. Doing it the other way round looks simpler and is wrong:
/// settling the leaf can balance pages all the way to the root, and the divider
/// may be on a different page by the time the write lands. Re-finding it costs
/// one descent and cannot be stale.
fn delete_at(pager: &mut Pager, tree: &Tree, path: Vec<Step>) -> DbResult<()> {
    let target = path
        .last()
        .copied()
        .ok_or_else(|| corrupt("a descent that reached no page"))?;
    let layout = read_layout(pager, target.page)?;
    if layout.kind.is_leaf() {
        remove_cell_at(pager, target.page, target.slot)?;
        let depth = path.len().saturating_sub(1);
        // `remove_cell_at` has already edited the page in place, so the page on
        // disk is correct. The only question left is whether it is now empty
        // enough to be worth merging with a neighbour, and that is a comparison
        // of two integers the page header already knows.
        //
        // Reading it that way rather than through `gather_page` is what makes a
        // delete cost a memmove instead of a page rebuild. Gathering copies
        // every cell body into its own allocation and `settle` then writes the
        // whole page back from them: on an index leaf holding three hundred
        // entries that is three hundred allocations to remove one, which
        // measured at 29 microseconds a delete and made an `UPDATE` of one row
        // with two indexes cost 145 - against the reference's 30.
        if !is_underfull(pager, target.page, depth)? {
            if depth == 0 {
                return collapse_root(pager, tree);
            }
            return ptrmap::refresh_btree_page(pager, target.page);
        }
        let content = gather_page(pager, target.page)?;
        return settle(pager, tree, &path, depth, content);
    }

    // The key of the entry being deleted, so it can be found again afterwards.
    let divider_payload = cell_payload(pager, target.page, target.slot)?;

    // Walk down the left subtree of the divider to its rightmost leaf entry.
    let pin = pager.get_page(target.page)?;
    let child = BTreePage::new(pin.bytes(), &layout).cell_child(target.slot)?;
    drop(pin);
    let mut donor_path = path.clone();
    let mut current = child;
    loop {
        let layout = read_layout(pager, current)?;
        if layout.kind.is_leaf() {
            if layout.cell_count == 0 {
                return Err(corrupt("an empty leaf under an interior divider"));
            }
            donor_path.push(Step {
                page: current,
                slot: layout.cell_count.saturating_sub(1),
            });
            break;
        }
        let slot = layout.cell_count;
        let pin = pager.get_page(current)?;
        let next = BTreePage::new(pin.bytes(), &layout).child_at(slot)?;
        drop(pin);
        donor_path.push(Step {
            page: current,
            slot,
        });
        current = next;
        if donor_path.len() > MAX_DEPTH {
            return Err(corrupt("a B-tree path deeper than any legal tree"));
        }
    }

    let donor = donor_path
        .last()
        .copied()
        .ok_or_else(|| corrupt("a donor path that reached no page"))?;
    let mut donor_content = gather_page(pager, donor.page)?;
    if donor.slot >= donor_content.entries.len() {
        return Err(corrupt("a donor entry that is not on its page"));
    }
    // Removed through the entry list rather than through `remove_cell_at`,
    // because the entry is not being deleted: its bytes, and any overflow chain
    // they name, are about to become the divider.
    let donor_entry = donor_content.entries.remove(donor.slot);
    let donor_depth = donor_path.len().saturating_sub(1);
    settle(pager, tree, &donor_path, donor_depth, donor_content)?;

    let (again, found) = find_key(pager, tree, &divider_payload)?;
    if !found {
        return Err(corrupt(
            "the divider being deleted could not be found again",
        ));
    }
    let step = again
        .last()
        .copied()
        .ok_or_else(|| corrupt("a descent that reached no page"))?;
    let mut content = gather_page(pager, step.page)?;
    let replaced = content
        .entries
        .get_mut(step.slot)
        .ok_or_else(|| corrupt("a divider that is not on its page"))?;
    let old_body = std::mem::replace(&mut replaced.body, donor_entry.body);
    replaced.rowid = donor_entry.rowid;
    let kind = content.kind;
    free_body_chain(pager, &old_body, kind)?;
    let depth = again.len().saturating_sub(1);
    settle(pager, tree, &again, depth, content)
}

/// Frees the overflow chain of an entry body, if it has one.
fn free_body_chain(pager: &mut Pager, body: &[u8], kind: PageKind) -> DbResult<()> {
    if kind.is_table() && !kind.is_leaf() {
        return Ok(());
    }
    let usable = pager.usable_size()?;
    let leaf_kind = if kind.is_table() {
        PageKind::LeafTable
    } else {
        PageKind::LeafIndex
    };
    let payload = varint::decode(body).map_err(|_| corrupt("a truncated payload length"))?;
    let mut cursor = payload.len;
    if leaf_kind == PageKind::LeafTable {
        let rest = body
            .get(cursor..)
            .ok_or_else(|| corrupt("a table cell with no rowid"))?;
        let (_, width) = varint::decode_i64(rest).map_err(|_| corrupt("a truncated rowid"))?;
        cursor = cursor.saturating_add(width);
    }
    let split = payload_split(payload.value, usable, leaf_kind)?;
    if !split.overflows {
        return Ok(());
    }
    let at = cursor.saturating_add(split.local);
    let head = PageId::from_persisted(bytes::read_u32(body, at)?)
        .map_err(|_| corrupt("an overflow chain that starts at page zero"))?;
    overflow::free_chain(pager, payload.value, split.local, Some(head))
}

/// Writes a page's content back, balancing when it does not fit or when the
/// page has become too empty to be worth keeping on its own.
fn settle(
    pager: &mut Pager,
    tree: &Tree,
    path: &[Step],
    depth: usize,
    content: PageContent,
) -> DbResult<()> {
    let step = path
        .get(depth)
        .copied()
        .ok_or_else(|| corrupt("a path with no page at that depth"))?;
    let capacity = capacity_of(pager, step.page, content.kind)?;
    let size = content.size()?;
    let fits = size <= capacity;
    let underfull = depth > 0
        && size.saturating_mul(UNDERFULL_DENOMINATOR)
            < capacity.saturating_mul(UNDERFULL_NUMERATOR);
    if fits && !underfull {
        write_page(pager, step.page, &content)?;
        if depth == 0 {
            return collapse_root(pager, tree);
        }
        return Ok(());
    }
    balance(pager, tree, path, depth, content)
}

/// Reports whether a page now holds too little to stand on its own.
///
/// The same test `settle` makes, made against the page rather than against a
/// gathered copy of it. The two measures agree exactly: `PageContent::size` is
/// the sum of every cell's footprint plus two bytes of pointer each, and
/// `used_bytes` is that same sum plus the header - so subtracting the header
/// gives the number `settle` would have computed.
///
/// The root is never underfull: it has no sibling to merge with, and a root
/// that emptied is handled by `collapse_root` instead.
/// @param pager - the pager holding the page
/// @param page - the page that just lost a cell
/// @param depth - how far down the path the page sits, zero being the root
fn is_underfull(pager: &mut Pager, page: PageId, depth: usize) -> DbResult<bool> {
    if depth == 0 {
        return Ok(false);
    }
    let layout = read_layout(pager, page)?;
    let capacity = capacity_of(pager, page, layout.kind)?;
    let pin = pager.get_page(page)?;
    let view = BTreePage::new(pin.bytes(), &layout);
    // What `settle` calls the content's size is everything on the page except
    // its header, and that is the capacity minus what is free - which the
    // freeblock chain, the fragment count and the gap already say. Counting it
    // the other way round, by decoding every cell, is exact and costs four
    // microseconds on a full index leaf; this is exact too and costs a walk of
    // a chain that is almost always empty.
    //
    // The two agree whenever every cell is at least four bytes, because that
    // is the point below which a cell's *footprint* stops being its length.
    // Every cell any of the four page kinds can hold is longer than that, and
    // the debug build checks it rather than the comment asserting it.
    let free = edit::free_bytes(&view)?;
    let size = capacity.saturating_sub(free);
    #[cfg(debug_assertions)]
    {
        let counted = edit::used_bytes(&view)?.saturating_sub(layout.kind.header_len());
        debug_assert_eq!(
            counted,
            size,
            "page {} accounts for {counted} bytes by cell and {size} by free space",
            page.get()
        );
    }
    drop(pin);
    Ok(size.saturating_mul(UNDERFULL_DENOMINATOR) < capacity.saturating_mul(UNDERFULL_NUMERATOR))
}

/// Lays a page and its siblings out again so that the content fits.
fn balance(
    pager: &mut Pager,
    tree: &Tree,
    path: &[Step],
    depth: usize,
    content: PageContent,
) -> DbResult<()> {
    if depth == 0 {
        return balance_root(pager, tree, content);
    }
    let step = path
        .get(depth)
        .copied()
        .ok_or_else(|| corrupt("a path with no page at that depth"))?;
    let parent_step = path
        .get(depth.saturating_sub(1))
        .copied()
        .ok_or_else(|| corrupt("a path with no parent"))?;
    let parent = gather_page(pager, parent_step.page)?;
    let child_count = parent.entries.len().saturating_add(1);
    let index = parent_step.slot.min(parent.entries.len());

    // A window of up to three consecutive children containing the one that
    // changed, reaching left first because a merge is likelier to find room in
    // a page that already exists than in one that is about to.
    let last = child_count.saturating_sub(1);
    let start = index.saturating_sub(1);
    let end = start
        .saturating_add(BALANCE_WINDOW.saturating_sub(1))
        .min(last);
    let first = end
        .saturating_sub(BALANCE_WINDOW.saturating_sub(1))
        .min(index);

    let mut window = Vec::new();
    for slot in first..=end {
        let page = child_at(&parent, slot)?;
        window.push(page);
    }

    let content_kind = content.kind;
    let children_are_leaves = content_kind.is_leaf();
    let promote = promotes_dividers(tree, children_are_leaves);
    // Every entry in the window is moved into this run exactly once.
    //
    // It used to be copied into it, on top of the copy `gather_page` already
    // makes and the copy each output page took back out again - three owned
    // vectors per cell, and a balance spans three pages of a hundred and twenty
    // or more. One balance measured 131 microseconds, and 2,992 of them
    // accounted for 394 of the 451 milliseconds a twenty-thousand-row index
    // build spent placing cells. Nothing here needs a second copy: the run is
    // consumed by the partition below and the pages it came from are about to
    // be rewritten.
    let mut changed = Some(content);
    let mut entries: Vec<Entry> = Vec::new();
    let mut trailing: Option<PageId> = None;
    for (offset, page) in window.iter().copied().enumerate() {
        let slot = first.saturating_add(offset);
        let mut piece = if page == step.page {
            changed
                .take()
                .ok_or_else(|| corrupt("a balance window naming the changed page twice"))?
        } else {
            gather_page(pager, page)?
        };
        if piece.kind != content_kind {
            return Err(corrupt("a balance across pages of different kinds"));
        }
        entries.append(&mut piece.entries);
        if slot < end {
            let divider = parent
                .entries
                .get(slot)
                .ok_or_else(|| corrupt("a window without its divider"))?;
            if promote {
                entries.push(Entry {
                    body: divider.body.clone(),
                    child: piece.right,
                    rowid: divider.rowid,
                });
            }
        } else {
            trailing = piece.right;
        }
    }

    let capacity = capacity_of(
        pager,
        window.first().copied().unwrap_or(step.page),
        content_kind,
    )?;
    // At the right-hand edge of the tree, fill greedily rather than evenly.
    //
    // An even split is the right answer in the middle of a tree: it leaves both
    // pages with room, so the next insert either side of the boundary does not
    // split again. At the right edge it is the wrong answer, because the page
    // it half-fills is the one every subsequent append lands on - so the split
    // it just did happens again a handful of entries later, and again. Measured
    // on a hundred thousand row index build: 6,844 splits, each absorbing about
    // fifteen entries, where a page holds a hundred and twenty.
    //
    // This is the same idea as SQLite's `balance_quick`, taken as far as the
    // machinery here already goes: `partition` computes the greedy fill anyway
    // and then discards it, so the change is which of two answers is kept.
    let rightmost = end == last;
    let ranges = partition(&entries, content_kind, capacity, promote, rightmost)?;
    let wanted = ranges.len();

    // Reuse the window's pages first, then allocate, then free the surplus.
    let mut pages: Vec<PageId> = Vec::with_capacity(wanted);
    for slot in 0..wanted {
        match window.get(slot).copied() {
            Some(page) => pages.push(page),
            None => pages.push(alloc::allocate_page(pager)?),
        }
    }
    let surplus: Vec<PageId> = window
        .get(wanted..)
        .map(<[PageId]>::to_vec)
        .unwrap_or_default();

    // The run is consumed rather than copied out of. Each entry belongs to
    // exactly one output page, or is the divider between two of them, and the
    // ranges walk it in order - so every entry can be moved into the page that
    // is about to hold it. Holding the run as slots makes that a `take` rather
    // than a copy while still allowing the divider at `range.1` to be read for
    // its child before it is moved.
    let mut slots: Vec<Option<Entry>> = entries.into_iter().map(Some).collect();
    let mut dividers: Vec<Entry> = Vec::with_capacity(wanted.saturating_sub(1));
    for (slot, range) in ranges.iter().enumerate() {
        let page = pages
            .get(slot)
            .copied()
            .ok_or_else(|| corrupt("a partition without a page"))?;
        let taken = slots
            .get_mut(range.0..range.1)
            .ok_or_else(|| corrupt("a partition outside its entries"))?;
        let mut moved = Vec::with_capacity(taken.len());
        for entry in taken.iter_mut() {
            moved.push(
                entry
                    .take()
                    .ok_or_else(|| corrupt("a partition claiming an entry twice"))?,
            );
        }
        let mut piece = PageContent {
            kind: content_kind,
            entries: moved,
            right: None,
        };
        let is_last = slot.saturating_add(1) == wanted;
        if !children_are_leaves {
            piece.right = if is_last {
                trailing
            } else if promote {
                slots
                    .get(range.1)
                    .and_then(|entry| entry.as_ref())
                    .and_then(|entry| entry.child)
                    .ok_or_else(|| corrupt("a divider with no child"))
                    .map(Some)?
            } else {
                return Err(corrupt("a table interior page without a promoted divider"));
            };
        }
        if !is_last {
            let divider = if promote {
                let entry = slots
                    .get_mut(range.1)
                    .and_then(|entry| entry.take())
                    .ok_or_else(|| corrupt("a partition with no divider entry"))?;
                Entry {
                    body: entry.body,
                    child: Some(page),
                    rowid: entry.rowid,
                }
            } else {
                let entry = piece
                    .entries
                    .last()
                    .ok_or_else(|| corrupt("an empty page cannot supply a divider"))?;
                Entry {
                    body: Vec::new(),
                    child: Some(page),
                    rowid: entry.rowid,
                }
            };
            dividers.push(divider);
        }
        write_page(pager, page, &piece)?;
    }

    let last_page = pages
        .last()
        .copied()
        .ok_or_else(|| corrupt("a balance that produced no pages"))?;

    let mut rebuilt = PageContent {
        kind: parent.kind,
        entries: parent
            .entries
            .get(..first)
            .ok_or_else(|| corrupt("a window outside its parent"))?
            .to_vec(),
        right: parent.right,
    };
    rebuilt.entries.extend(dividers);
    let mut tail = parent
        .entries
        .get(end..)
        .ok_or_else(|| corrupt("a window outside its parent"))?
        .to_vec();
    match tail.first_mut() {
        Some(entry) => entry.child = Some(last_page),
        None => rebuilt.right = Some(last_page),
    }
    rebuilt.entries.extend(tail);

    for page in surplus {
        alloc::free_page(pager, page)?;
    }

    settle(pager, tree, path, depth.saturating_sub(1), rebuilt)
}

/// Returns the child a parent's slot selects.
fn child_at(parent: &PageContent, slot: usize) -> DbResult<PageId> {
    if slot == parent.entries.len() {
        return parent
            .right
            .ok_or_else(|| corrupt("a right-most child was asked for on a leaf page"));
    }
    parent
        .entries
        .get(slot)
        .and_then(|entry| entry.child)
        .ok_or_else(|| corrupt("an interior entry with no child"))
}

/// Splits a list of entries into the pages that will hold them.
///
/// Each range is half-open, and when dividers are promoted the entry *at* a
/// range's end is the divider between that page and the next: it belongs to the
/// parent and to no page. The split is even rather than greedy - a greedy fill
/// leaves the last page nearly empty, and a page that empty is a page the next
/// delete has to balance again.
fn partition(
    entries: &[Entry],
    kind: PageKind,
    capacity: usize,
    promote: bool,
    greedy_fill: bool,
) -> DbResult<Vec<(usize, usize)>> {
    let mut sizes = Vec::with_capacity(entries.len());
    for entry in entries {
        let size = edit::cell_footprint(entry_size(entry, kind)?).saturating_add(2);
        if size > capacity {
            return Err(corrupt(format!(
                "a cell of {size} bytes cannot fit a page holding {capacity}"
            )));
        }
        sizes.push(size);
    }
    let greedy = fill(&sizes, capacity, promote, usize::MAX)?;
    if greedy_fill || greedy.len() <= 1 {
        return Ok(greedy);
    }
    let total: usize = sizes.iter().copied().sum();
    let target = total
        .saturating_add(greedy.len().saturating_sub(1))
        .saturating_div(greedy.len())
        .min(capacity);
    match fill(&sizes, capacity, promote, target) {
        Ok(even) if even.len() == greedy.len() => Ok(even),
        _ => Ok(greedy),
    }
}

/// Fills pages up to `target` bytes each, never past `capacity`.
///
/// When dividers are promoted the arithmetic has one trap in it, and it costs
/// an entry every time it is missed: between any two pages exactly one entry
/// moves up to the parent, so `k` pages consume `k - 1` entries beyond what
/// they hold. Closing a page with exactly one entry left therefore promotes
/// that entry and leaves nothing for the page after it - so the entry is in no
/// page and no parent, and it is simply gone. The fix is to hand one entry back
/// to the page just closed, and where even that is impossible to admit the
/// empty page rather than lose the row.
fn fill(
    sizes: &[usize],
    capacity: usize,
    promote: bool,
    target: usize,
) -> DbResult<Vec<(usize, usize)>> {
    let len = sizes.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut index = 0usize;
    loop {
        let start = index;
        let mut used = 0usize;
        while index < len {
            let size = sizes.get(index).copied().unwrap_or(0);
            if used.saturating_add(size) > capacity {
                break;
            }
            if used >= target && index > start {
                break;
            }
            used = used.saturating_add(size);
            index = index.saturating_add(1);
        }
        if index == start && index < len {
            return Err(corrupt("an entry that fits no page"));
        }
        if promote && len.saturating_sub(index) == 1 && index > start.saturating_add(1) {
            // One entry left would be promoted with no page to follow it.
            index = index.saturating_sub(1);
        }
        ranges.push((start, index));
        if index >= len {
            break;
        }
        if !promote {
            continue;
        }
        // This entry becomes the divider in the parent and lives on no page.
        index = index.saturating_add(1);
        if index >= len {
            ranges.push((index, index));
            break;
        }
    }
    if ranges.is_empty() {
        ranges.push((0, 0));
    }
    Ok(ranges)
}

/// Balances the root, which is the only page that can change the tree's height.
fn balance_root(pager: &mut Pager, tree: &Tree, content: PageContent) -> DbResult<()> {
    let capacity = capacity_of(pager, tree.root, content.kind)?;
    if content.size()? <= capacity {
        write_page(pager, tree.root, &content)?;
        return collapse_root(pager, tree);
    }

    // The root cannot be split, because its page number is the tree's name and
    // is recorded in the schema. So the tree grows a level instead: the root's
    // contents move to a new child and the root becomes an interior page with
    // that child as its only pointer, which is then balanced normally.
    // The child is allocated and pointed at, but not written: the content that
    // did not fit the root does not fit the child either, and balancing it at
    // its new depth is what splits it across as many pages as it needs. The
    // page is never read before it is written, because the balance below is
    // handed the content rather than reading it back.
    let child = alloc::allocate_page(pager)?;
    let root_content = PageContent {
        kind: tree.interior_kind(),
        entries: Vec::new(),
        right: Some(child),
    };
    write_page(pager, tree.root, &root_content)?;
    let path = [
        Step {
            page: tree.root,
            slot: 0,
        },
        Step {
            page: child,
            slot: 0,
        },
    ];
    balance(pager, tree, &path, 1, content)
}

/// Pulls a root's only child up into it when the child fits, which is how a
/// tree loses a level.
fn collapse_root(pager: &mut Pager, tree: &Tree) -> DbResult<()> {
    loop {
        let root = gather_page(pager, tree.root)?;
        if root.kind.is_leaf() || !root.entries.is_empty() {
            return Ok(());
        }
        let Some(child) = root.right else {
            return Ok(());
        };
        let content = gather_page(pager, child)?;
        let capacity = capacity_of(pager, tree.root, content.kind)?;
        if content.size()? > capacity {
            // Page 1 has a hundred fewer usable bytes than any other page, so a
            // child that fits its own page may not fit the root. Leaving the
            // level in place is legal and costs one page read per lookup.
            return Ok(());
        }
        write_page(pager, tree.root, &content)?;
        alloc::free_page(pager, child)?;
    }
}

/// Descends to where a rowid is or would be, recording the path.
fn find_rowid(pager: &mut Pager, tree: &Tree, rowid: i64) -> DbResult<(Vec<Step>, bool)> {
    let mut path = Vec::new();
    let mut page = tree.root;
    loop {
        let layout = read_layout(pager, page)?;
        if !layout.kind.is_table() {
            return Err(corrupt("a table write reached an index page"));
        }
        let pin = pager.get_page(page)?;
        let view = BTreePage::new(pin.bytes(), &layout);
        let mut low = 0usize;
        let mut high = layout.cell_count;
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            if view.cell_rowid(middle)? < rowid {
                low = middle.saturating_add(1);
            } else {
                high = middle;
            }
        }
        if layout.kind == PageKind::LeafTable {
            let found = low < layout.cell_count && view.cell_rowid(low)? == rowid;
            drop(pin);
            path.push(Step { page, slot: low });
            return Ok((path, found));
        }
        let child = view.child_at(low)?;
        drop(pin);
        path.push(Step { page, slot: low });
        page = child;
        if path.len() > MAX_DEPTH {
            return Err(corrupt("a B-tree path deeper than any legal tree"));
        }
    }
}

/// Descends to where an index entry is or would be, recording the path.
///
/// Unlike a table tree, the entry may be on an interior page, and the descent
/// stops there when it is: an index B-tree keeps entries at every level, so the
/// first equal key found on the way down *is* the entry.
fn find_key(pager: &mut Pager, tree: &Tree, probe: &[u8]) -> DbResult<(Vec<Step>, bool)> {
    let encoding = pager.text_encoding();
    let limits = Limits::default();
    // The probe is parsed once for the whole descent rather than once per
    // comparison. It used to be re-parsed inside the comparison, so a binary
    // search over three levels of a twenty-thousand-entry index parsed the same
    // record fifteen times and allocated a span vector for each - and
    // `Limits::default()` was rebuilt beside it, which copies the whole limit
    // table behind an `Arc`. Neither depends on which cell is being compared.
    let mut probe_fields = Vec::new();
    let probe_header = RecordRef::parse_into(probe, &limits, &mut probe_fields)?;
    let probe_record = RecordRef::with_fields(probe, &probe_fields, probe_header, encoding);
    // Reused across every comparison of the descent: the spans of the cell being
    // compared, and a buffer that is only touched when a cell overflows.
    let mut cell_fields = Vec::new();
    let mut overflowed = Vec::new();
    let mut path = Vec::new();
    let mut page = tree.root;
    loop {
        let layout = read_layout(pager, page)?;
        if layout.kind.is_table() {
            return Err(corrupt("an index write reached a table page"));
        }
        let mut low = 0usize;
        let mut high = layout.cell_count;
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            let ordering = compare_cell_to(
                pager,
                page,
                middle,
                &probe_record,
                &tree.key,
                encoding,
                &limits,
                &mut cell_fields,
                &mut overflowed,
            )?;
            if ordering == std::cmp::Ordering::Less {
                low = middle.saturating_add(1);
            } else {
                high = middle;
            }
        }
        let equal = low < layout.cell_count
            && compare_cell_to(
                pager,
                page,
                low,
                &probe_record,
                &tree.key,
                encoding,
                &limits,
                &mut cell_fields,
                &mut overflowed,
            )? == std::cmp::Ordering::Equal;
        if equal {
            path.push(Step { page, slot: low });
            return Ok((path, true));
        }
        if layout.kind == PageKind::LeafIndex {
            path.push(Step { page, slot: low });
            return Ok((path, false));
        }
        let pin = pager.get_page(page)?;
        let child = BTreePage::new(pin.bytes(), &layout).child_at(low)?;
        drop(pin);
        path.push(Step { page, slot: low });
        page = child;
        if path.len() > MAX_DEPTH {
            return Err(corrupt("a B-tree path deeper than any legal tree"));
        }
    }
}

/// Compares one cell's key against an already-parsed probe record.
///
/// The cell's payload is borrowed straight out of the pinned page whenever the
/// whole of it is on that page, which is the ordinary case: an index entry
/// overflows only when it is bigger than about a quarter of a page. The owning
/// form below is what the overflow case falls back to, because following a
/// chain needs the pager mutably and the pin has to be dropped first.
///
/// This is the hot path of every index write. Copying the payload and parsing
/// both records per comparison made one index insert cost about five heap
/// allocations per comparison, and a binary search does a dozen or more of them.
/// @param probe - the record being searched for, parsed once by the caller
/// @param cell_fields - a span buffer reused across the whole descent
/// @param overflowed - a payload buffer only the overflow case fills
#[allow(clippy::too_many_arguments)]
fn compare_cell_to(
    pager: &mut Pager,
    page: PageId,
    index: usize,
    probe: &RecordRef<'_>,
    key: &KeyInfo,
    encoding: TextEncoding,
    limits: &Limits,
    cell_fields: &mut Vec<inillucent_value::record::FieldSpan>,
    overflowed: &mut Vec<u8>,
) -> DbResult<std::cmp::Ordering> {
    let layout = read_layout(pager, page)?;
    let (total, head, local_offset, local_len) = {
        let pin = pager.get_page(page)?;
        let view = BTreePage::new(pin.bytes(), &layout);
        let cell = view.cell(index)?;
        if cell.overflow.is_none() {
            cell_fields.clear();
            let header = RecordRef::parse_into(cell.local_payload, limits, cell_fields)?;
            let left = RecordRef::with_fields(cell.local_payload, cell_fields, header, encoding);
            return record::compare_records(&left, probe, key);
        }
        (
            cell.split.total,
            cell.overflow,
            cell.local_offset,
            cell.local_payload.len(),
        )
    };
    // The chain has to be followed, which needs the pager mutably, so the local
    // part is copied out first and the pin is gone by the time this runs.
    let local = {
        let pin = pager.get_page(page)?;
        bytes::window(pin.bytes(), local_offset, local_len)?.to_vec()
    };
    overflow::read_payload_into(pager, &local, total, head, limits, overflowed)?;
    cell_fields.clear();
    let header = RecordRef::parse_into(overflowed, limits, cell_fields)?;
    let left = RecordRef::with_fields(overflowed, cell_fields, header, encoding);
    record::compare_records(&left, probe, key)
}

/// Fills an index B-tree with an entry for every row of a table.
///
/// The values come straight out of each row's record, without affinity being
/// applied again: a stored value has already had its column's affinity applied
/// once, and applying it a second time is not idempotent for a text column
/// holding a number. The rowid alias is the exception - it is not in the
/// record at all, so it is read from the row's key.
///
/// Entries are appended in table order rather than inserted, which would be
/// wrong for a general index; they are sorted first, so the append is into a
/// tree that is already in key order. The sort is what makes the backfill of a
/// large table one pass over the data rather than one descent per row.
pub fn build_index(
    pager: &mut Pager,
    table_root: u32,
    index_root: u32,
    columns: &[u16],
    key: &KeyInfo,
    rowid_alias: Option<u16>,
    trailing: &[u16],
    table_key: Option<&KeyInfo>,
) -> DbResult<()> {
    let limits = Limits::default();
    let table = PageId::from_persisted(table_root)?;
    let index = PageId::from_persisted(index_root)?;
    let encoding = pager.text_encoding();
    let format = pager.header().schema_format.max(1);
    // The entries are held as values and encoded *after* they are sorted, not
    // before. Sorting encoded records meant re-parsing both sides on every
    // comparison - two record parses per comparison, n log n comparisons - and
    // that, rather than the writing, was what made building an index over a
    // hundred thousand rows take five seconds where the reference takes fifty
    // milliseconds. It also halves what the build holds: the values or the
    // bytes, not both.
    let mut entries: Vec<Vec<inillucent_value::Value<'static>>> = Vec::new();
    // `trailing` names the slots an entry ends with instead of a rowid, which
    // is how a WITHOUT ROWID table's secondary indexes locate a row. It also
    // says which kind of b-tree the table is, because such a table's root is an
    // index b-tree and a table cursor on it would ask its pages for rowids.
    let keyed = !trailing.is_empty();
    let mut cursor = match table_key {
        Some(key) => crate::cursor::BTreeCursor::index(table, key.clone()),
        None => crate::cursor::BTreeCursor::table(table),
    };
    let mut more = cursor.first(pager)?;
    while more {
        let rowid = if keyed { 0 } else { cursor.rowid()? };
        let values = cursor.record_values(pager, &limits)?;
        let mut fields: Vec<inillucent_value::Value<'static>> =
            Vec::with_capacity(columns.len() + 1);
        for column in columns {
            if rowid_alias == Some(*column) {
                fields.push(inillucent_value::Value::Integer(rowid));
                continue;
            }
            fields.push(
                values
                    .get(*column as usize)
                    .cloned()
                    .unwrap_or(inillucent_value::Value::Null),
            );
        }
        if keyed {
            for slot in trailing {
                fields.push(
                    values
                        .get(*slot as usize)
                        .cloned()
                        .unwrap_or(inillucent_value::Value::Null),
                );
            }
        } else {
            fields.push(inillucent_value::Value::Integer(rowid));
        }
        entries.push(fields);
        more = cursor.next(pager)?;
    }
    entries.sort_by(|left, right| compare_key_values(left, right, key));
    let encoded: Vec<Vec<u8>> = entries
        .iter()
        .map(|fields| record::encode_record(fields, encoding, format))
        .collect::<DbResult<Vec<Vec<u8>>>>()?;
    append_entries(pager, index, &encoded)?;
    Ok(())
}

/// Compares two index entries, as values, the way the index orders them.
///
/// The same field-by-field walk `compare_records` does, over values that are
/// already decoded. Every entry an index build produces has the same shape, so
/// there is nothing to be learned from the encoding that the values do not
/// already say - and a comparison that has to decode its operands first is a
/// comparison paid for `n log n` times.
/// @param left - one entry's fields
/// @param right - the other's
/// @param key - the collations and directions the index orders by
fn compare_key_values(
    left: &[inillucent_value::Value<'static>],
    right: &[inillucent_value::Value<'static>],
    key: &KeyInfo,
) -> std::cmp::Ordering {
    let shared = left.len().max(right.len());
    for index in 0..shared {
        let (Some(one), Some(other)) = (left.get(index), right.get(index)) else {
            // A shorter entry sorts first, which is what comparing a missing
            // field as NULL would say anyway.
            return left.len().cmp(&right.len());
        };
        let column = key.column(index);
        let ordering = inillucent_value::compare::compare_values(one, other, column.collation);
        let ordering = if column.descending {
            ordering.reverse()
        } else {
            ordering
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{self, CheckOptions};
    use crate::cursor::BTreeCursor;
    use crate::header::VacuumMode;
    use crate::pager::{NewDatabase, PagerOptions};
    use inillucent_base::page::PageSize;
    use inillucent_value::record::encode_record;
    use inillucent_value::Value;
    use inillucent_vfs::memory::MemoryVfs;
    use inillucent_vfs::DbPath;

    /// The page sizes every structural test runs at.
    ///
    /// They are the format's smallest, its default, SQLite's old default, and
    /// its largest. The two ends are where the arithmetic breaks: 512 has the
    /// tightest local-payload window and splits after a handful of rows, and
    /// 65536 is the one size whose content-area start does not fit in the two
    /// bytes the header gives it.
    const PAGE_SIZES: [u32; 4] = [512, 1024, 4096, 65_536];

    /// Creates an empty database in memory and opens it for writing.
    fn create(vfs: &MemoryVfs, page_size: u32, vacuum: VacuumMode) -> Pager {
        let path = DbPath::new(format!("p{page_size}.db"));
        Pager::create(
            vfs,
            &path,
            PagerOptions::default(),
            NewDatabase {
                page_size: PageSize::new(page_size).unwrap(),
                reserved_bytes: 0,
                text_encoding: TextEncoding::Utf8,
                vacuum_mode: vacuum,
            },
        )
        .unwrap()
    }

    /// Encodes a one-column record holding an integer.
    fn row(value: i64) -> Vec<u8> {
        encode_record(&[Value::Integer(value)], TextEncoding::Utf8, 4).unwrap()
    }

    /// Encodes a record whose blob is `len` bytes long, which is how a test
    /// asks for a payload on either side of the overflow threshold.
    fn blob_row(marker: u8, len: usize) -> Vec<u8> {
        let blob = vec![marker; len];
        encode_record(
            &[Value::Blob(inillucent_value::BlobValue::borrowed(&blob))],
            TextEncoding::Utf8,
            4,
        )
        .unwrap()
    }

    /// Encodes a two-field index entry: a key and the rowid that makes it
    /// unique, which is what SQLite puts in an index.
    fn index_entry(key: i64, rowid: i64) -> Vec<u8> {
        encode_record(
            &[Value::Integer(key), Value::Integer(rowid)],
            TextEncoding::Utf8,
            4,
        )
        .unwrap()
    }

    /// Reads every row of a table B-tree in cursor order.
    fn scan_table(pager: &mut Pager, root: PageId) -> Vec<(i64, Vec<u8>)> {
        let limits = Limits::default();
        let mut cursor = BTreeCursor::table(root);
        let mut rows = Vec::new();
        let mut more = cursor.first(pager).unwrap();
        while more {
            rows.push((
                cursor.rowid().unwrap(),
                cursor.payload(pager, &limits).unwrap(),
            ));
            more = cursor.next(pager).unwrap();
        }
        rows
    }

    /// Reads every entry of an index B-tree in cursor order.
    fn scan_index(pager: &mut Pager, root: PageId, key: &KeyInfo) -> Vec<Vec<u8>> {
        let limits = Limits::default();
        let mut cursor = BTreeCursor::index(root, key.clone());
        let mut entries = Vec::new();
        let mut more = cursor.first(pager).unwrap();
        while more {
            entries.push(cursor.payload(pager, &limits).unwrap());
            more = cursor.next(pager).unwrap();
        }
        entries
    }

    /// Runs the integrity check over the trees a test built, and fails with
    /// what it found rather than with a bare assertion.
    fn check_roots(pager: &mut Pager, roots: &[PageId]) {
        let report =
            check::check_database_with_options(pager, &CheckOptions::roots(roots.to_vec()))
                .unwrap();
        assert!(report.is_ok(), "{:#?}", report.as_pragma_output());
    }

    /// A freshly created database is one page holding an empty schema tree, and
    /// it passes the integrity check at every page size.
    #[test]
    fn a_created_database_is_a_valid_empty_database() {
        for size in PAGE_SIZES {
            let vfs = MemoryVfs::new();
            let mut pager = create(&vfs, size, VacuumMode::None);
            assert_eq!(pager.page_count(), 1, "{size}");
            assert_eq!(pager.header().page_size.bytes(), size);
            pager.begin_read().unwrap();
            let report = check::integrity_check(&mut pager).unwrap();
            assert!(report.is_ok(), "{size}: {:#?}", report.as_pragma_output());
        }
    }

    /// Rows inserted in ascending order read back in that order, at every page
    /// size, and the tree they build passes the integrity check.
    #[test]
    fn ascending_rows_read_back_in_order() {
        for size in PAGE_SIZES {
            let vfs = MemoryVfs::new();
            let mut pager = create(&vfs, size, VacuumMode::None);
            pager.begin_write().unwrap();
            let root = create_table(&mut pager).unwrap();
            for rowid in 1..=400i64 {
                insert_row(&mut pager, root, rowid, &row(rowid * 7)).unwrap();
            }
            pager.commit().unwrap();
            check_roots(&mut pager, &[root]);
            let rows = scan_table(&mut pager, root);
            assert_eq!(rows.len(), 400, "{size}");
            for (index, (rowid, payload)) in rows.iter().enumerate() {
                assert_eq!(*rowid, index as i64 + 1, "{size}");
                assert_eq!(payload, &row((index as i64 + 1) * 7), "{size}");
            }
        }
    }

    /// Rows inserted in an order that is not sorted still come back sorted,
    /// which is the property a descent and a split have to preserve together.
    #[test]
    fn rows_inserted_out_of_order_read_back_sorted() {
        for size in PAGE_SIZES {
            let vfs = MemoryVfs::new();
            let mut pager = create(&vfs, size, VacuumMode::None);
            pager.begin_write().unwrap();
            let root = create_table(&mut pager).unwrap();
            // A stride coprime with the count visits every rowid exactly once in
            // an order that is nothing like sorted.
            let mut expected = Vec::new();
            for step in 0..300i64 {
                let rowid = (step * 97) % 300 + 1;
                insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
                expected.push(rowid);
            }
            pager.commit().unwrap();
            check_roots(&mut pager, &[root]);
            expected.sort_unstable();
            let rows: Vec<i64> = scan_table(&mut pager, root)
                .into_iter()
                .map(|(rowid, _)| rowid)
                .collect();
            assert_eq!(rows, expected, "{size}");
        }
    }

    /// Inserting the same rowid twice replaces the row rather than adding one.
    #[test]
    fn inserting_the_same_rowid_replaces_the_row() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, 1024, VacuumMode::None);
        pager.begin_write().unwrap();
        let root = create_table(&mut pager).unwrap();
        for rowid in 1..=50i64 {
            insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
        }
        for rowid in 1..=50i64 {
            insert_row(&mut pager, root, rowid, &row(rowid * 1000)).unwrap();
        }
        pager.commit().unwrap();
        check_roots(&mut pager, &[root]);
        let rows = scan_table(&mut pager, root);
        assert_eq!(rows.len(), 50);
        for (index, (rowid, payload)) in rows.iter().enumerate() {
            assert_eq!(*rowid, index as i64 + 1);
            assert_eq!(payload, &row((index as i64 + 1) * 1000));
        }
    }

    /// Deleting every row leaves an empty tree, a valid database, and the pages
    /// the tree held on the freelist rather than owned by nothing.
    #[test]
    fn deleting_every_row_returns_its_pages_to_the_freelist() {
        for size in PAGE_SIZES {
            let vfs = MemoryVfs::new();
            let mut pager = create(&vfs, size, VacuumMode::None);
            pager.begin_write().unwrap();
            let root = create_table(&mut pager).unwrap();
            for rowid in 1..=250i64 {
                insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
            }
            pager.commit().unwrap();
            let grown = pager.page_count();

            pager.begin_write().unwrap();
            for rowid in 1..=250i64 {
                assert!(delete_row(&mut pager, root, rowid).unwrap(), "{size}");
            }
            pager.commit().unwrap();
            check_roots(&mut pager, &[root]);
            assert!(scan_table(&mut pager, root).is_empty(), "{size}");
            assert_eq!(
                pager.page_count(),
                grown,
                "{size}: the file should not shrink"
            );
            let free = alloc::free_count(&pager);
            assert_eq!(
                u64::from(free).saturating_add(2),
                u64::from(grown),
                "{size}: every page but page 1 and the root should be free"
            );
        }
    }

    /// Deleting in a scattered order exercises merging rather than the trailing
    /// collapse an in-order delete produces.
    #[test]
    fn deleting_out_of_order_keeps_the_tree_valid() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, 512, VacuumMode::None);
        pager.begin_write().unwrap();
        let root = create_table(&mut pager).unwrap();
        for rowid in 1..=300i64 {
            insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
        }
        pager.commit().unwrap();

        pager.begin_write().unwrap();
        let mut remaining: Vec<i64> = (1..=300).collect();
        for step in 0..300i64 {
            let rowid = (step * 73) % 300 + 1;
            if step % 3 == 0 {
                assert!(delete_row(&mut pager, root, rowid).unwrap());
                remaining.retain(|value| *value != rowid);
            }
        }
        pager.commit().unwrap();
        check_roots(&mut pager, &[root]);
        let rows: Vec<i64> = scan_table(&mut pager, root)
            .into_iter()
            .map(|(rowid, _)| rowid)
            .collect();
        assert_eq!(rows, remaining);
    }

    /// A payload on either side of the local threshold round-trips, and its
    /// overflow pages are freed when the row is deleted.
    #[test]
    fn payloads_across_the_overflow_threshold_round_trip() {
        for size in PAGE_SIZES {
            let vfs = MemoryVfs::new();
            let mut pager = create(&vfs, size, VacuumMode::None);
            let usable = pager.usable_size().unwrap();
            let max_local = usable.saturating_sub(35) as usize;
            let lengths = [
                1usize,
                max_local.saturating_sub(12),
                max_local,
                max_local.saturating_add(1),
                max_local.saturating_add(usable as usize),
                max_local.saturating_mul(3),
            ];
            pager.begin_write().unwrap();
            let root = create_table(&mut pager).unwrap();
            for (index, len) in lengths.iter().copied().enumerate() {
                insert_row(
                    &mut pager,
                    root,
                    index as i64 + 1,
                    &blob_row(index as u8, len),
                )
                .unwrap();
            }
            pager.commit().unwrap();
            check_roots(&mut pager, &[root]);
            let rows = scan_table(&mut pager, root);
            assert_eq!(rows.len(), lengths.len(), "{size}");
            for (index, len) in lengths.iter().copied().enumerate() {
                assert_eq!(
                    rows[index].1,
                    blob_row(index as u8, len),
                    "{size} len {len}"
                );
            }

            pager.begin_write().unwrap();
            for index in 0..lengths.len() {
                assert!(
                    delete_row(&mut pager, root, index as i64 + 1).unwrap(),
                    "{size}"
                );
            }
            pager.commit().unwrap();
            check_roots(&mut pager, &[root]);
            assert!(scan_table(&mut pager, root).is_empty(), "{size}");
        }
    }

    /// An index tree keeps its entries in key order across splits, and a delete
    /// of an entry that has been promoted to an interior page keeps the rest.
    #[test]
    fn index_entries_stay_in_key_order() {
        for size in PAGE_SIZES {
            let vfs = MemoryVfs::new();
            let mut pager = create(&vfs, size, VacuumMode::None);
            let key = KeyInfo::binary(2);
            pager.begin_write().unwrap();
            let root = create_index(&mut pager).unwrap();
            let mut expected = Vec::new();
            for step in 0..300i64 {
                let value = (step * 137) % 300;
                insert_entry(&mut pager, root, &key, &index_entry(value, value)).unwrap();
                expected.push(value);
            }
            pager.commit().unwrap();
            let report = check::check_database_with_options(
                &mut pager,
                &CheckOptions::roots(vec![root]).with_key(root, key.clone()),
            )
            .unwrap();
            assert!(report.is_ok(), "{size}: {:#?}", report.as_pragma_output());
            expected.sort_unstable();
            let entries = scan_index(&mut pager, root, &key);
            let found: Vec<Vec<u8>> = expected
                .iter()
                .map(|value| index_entry(*value, *value))
                .collect();
            assert_eq!(entries, found, "{size}");
        }
    }

    /// A batched append places every entry, in order, on a valid tree.
    ///
    /// The batch decides how many entries fit on the rightmost leaf and puts
    /// them there in one page edit, which is a second answer to a question
    /// `try_insert_in_place` already answers one at a time. Two answers is one
    /// too many unless they agree, so this asserts what a reader cannot check:
    /// the same entries, the same order, and a tree that still validates - at
    /// every page size, including the 512-byte one that splits after a handful
    /// of rows and the 65536-byte one whose content area does not fit in the
    /// two bytes its header gives it.
    #[test]
    fn a_batched_append_places_every_entry_in_order() {
        for size in PAGE_SIZES {
            let vfs = MemoryVfs::new();
            let mut pager = create(&vfs, size, VacuumMode::None);
            let key = KeyInfo::binary(2);
            pager.begin_write().unwrap();
            let root = create_index(&mut pager).unwrap();
            // Enough to fill many pages at every size, and a payload long
            // enough at 512 bytes to reach the overflow threshold on the way.
            let payloads: Vec<Vec<u8>> = (0..1_000i64)
                .map(|value| index_entry(value, value))
                .collect();
            append_entries(&mut pager, root, &payloads).unwrap();
            pager.commit().unwrap();

            let report = check::check_database_with_options(
                &mut pager,
                &CheckOptions::roots(vec![root]).with_key(root, key.clone()),
            )
            .unwrap();
            assert!(report.is_ok(), "{size}: {:#?}", report.as_pragma_output());
            assert_eq!(scan_index(&mut pager, root, &key), payloads, "{size}");
        }
    }

    /// An ascending build fills its pages rather than half-filling them.
    ///
    /// An even split is right in the middle of a tree and wrong at its right
    /// edge, where the half-filled page is the one every later append lands on:
    /// the split it just did happens again a handful of entries later. This
    /// pins the fix by counting pages, because the symptom is a tree that is
    /// correct in every way except that it is twice the size it should be - and
    /// nothing else in this file would notice.
    #[test]
    fn an_ascending_build_fills_its_pages() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, 4096, VacuumMode::None);
        let key = KeyInfo::binary(2);
        pager.begin_write().unwrap();
        let root = create_index(&mut pager).unwrap();
        let payloads: Vec<Vec<u8>> = (0..5_000i64)
            .map(|value| index_entry(value, value))
            .collect();
        // What the entries would occupy if every page were filled completely.
        let capacity = capacity_of(&pager, root, PageKind::LeafIndex).unwrap();
        let used: usize = payloads
            .iter()
            .map(|payload| edit::cell_footprint(payload.len()).saturating_add(2))
            .sum();
        let ideal = used.div_ceil(capacity);
        append_entries(&mut pager, root, &payloads).unwrap();
        pager.commit().unwrap();

        let report = check::check_database_with_options(
            &mut pager,
            &CheckOptions::roots(vec![root]).with_key(root, key.clone()),
        )
        .unwrap();
        assert!(report.is_ok(), "{:#?}", report.as_pragma_output());
        assert_eq!(scan_index(&mut pager, root, &key), payloads);
        // Page one is the header, and the interior pages are a few more. Even
        // splits made this about twice `ideal`; a quarter over is slack enough
        // for the tree above the leaves without admitting that.
        let pages = pager.page_count() as usize;
        assert!(
            pages <= ideal + ideal / 4 + 4,
            "{pages} pages for {ideal} pages of entries"
        );
    }

    /// A batch and one-at-a-time appends build the same tree.
    ///
    /// Not merely the same entries: the same *pages*, because a batch that
    /// packed its leaves differently would be a second B-tree shape reachable
    /// only through one code path, and the reader of a database has no way to
    /// know which path wrote it.
    #[test]
    fn a_batched_append_builds_the_same_tree_as_one_at_a_time() {
        let payloads: Vec<Vec<u8>> = (0..400i64).map(|value| index_entry(value, value)).collect();
        for size in PAGE_SIZES {
            let one = {
                let vfs = MemoryVfs::new();
                let mut pager = create(&vfs, size, VacuumMode::None);
                pager.begin_write().unwrap();
                let root = create_index(&mut pager).unwrap();
                for payload in &payloads {
                    append_entry(&mut pager, root, payload).unwrap();
                }
                pager.commit().unwrap();
                (root, pager.page_count())
            };
            let batched = {
                let vfs = MemoryVfs::new();
                let mut pager = create(&vfs, size, VacuumMode::None);
                pager.begin_write().unwrap();
                let root = create_index(&mut pager).unwrap();
                append_entries(&mut pager, root, &payloads).unwrap();
                pager.commit().unwrap();
                (root, pager.page_count())
            };
            assert_eq!(one, batched, "{size}");
        }
    }

    /// Deleting index entries one at a time, in an order unlike the key order,
    /// keeps every remaining entry and every structural invariant.
    #[test]
    fn index_entries_can_be_deleted_in_any_order() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, 512, VacuumMode::None);
        let key = KeyInfo::binary(2);
        pager.begin_write().unwrap();
        let root = create_index(&mut pager).unwrap();
        for value in 0..250i64 {
            insert_entry(&mut pager, root, &key, &index_entry(value, value)).unwrap();
        }
        pager.commit().unwrap();

        let mut remaining: Vec<i64> = (0..250).collect();
        pager.begin_write().unwrap();
        for step in 0..250i64 {
            let value = (step * 61) % 250;
            if step % 2 == 0 {
                assert!(
                    delete_entry(&mut pager, root, &key, &index_entry(value, value)).unwrap(),
                    "{value}"
                );
                remaining.retain(|entry| *entry != value);
            }
        }
        pager.commit().unwrap();
        let report = check::check_database_with_options(
            &mut pager,
            &CheckOptions::roots(vec![root]).with_key(root, key.clone()),
        )
        .unwrap();
        assert!(report.is_ok(), "{:#?}", report.as_pragma_output());
        let entries = scan_index(&mut pager, root, &key);
        let expected: Vec<Vec<u8>> = remaining
            .iter()
            .map(|value| index_entry(*value, *value))
            .collect();
        assert_eq!(entries, expected);
    }

    /// A rolled-back transaction leaves the database exactly as it was, byte
    /// for byte, including the pages a balance rewrote and the pages it grew.
    #[test]
    fn a_rollback_restores_every_byte() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("rollback.db");
        let mut pager = Pager::create(
            &vfs,
            &path,
            PagerOptions::default(),
            NewDatabase {
                page_size: PageSize::new(512).unwrap(),
                reserved_bytes: 0,
                text_encoding: TextEncoding::Utf8,
                vacuum_mode: VacuumMode::None,
            },
        )
        .unwrap();
        pager.begin_write().unwrap();
        let root = create_table(&mut pager).unwrap();
        for rowid in 1..=120i64 {
            insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
        }
        pager.commit().unwrap();
        let before = vfs.snapshot(&path).unwrap();

        pager.begin_write().unwrap();
        for rowid in 121..=400i64 {
            insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
        }
        for rowid in 1..=60i64 {
            assert!(delete_row(&mut pager, root, rowid).unwrap());
        }
        pager.rollback().unwrap();

        assert_eq!(vfs.snapshot(&path).unwrap(), before);
        check_roots(&mut pager, &[root]);
        let rows: Vec<i64> = scan_table(&mut pager, root)
            .into_iter()
            .map(|(rowid, _)| rowid)
            .collect();
        assert_eq!(rows, (1..=120).collect::<Vec<i64>>());
    }

    /// A statement rollback restores the tree the statement started with while
    /// leaving everything the transaction did before it alone.
    #[test]
    fn a_statement_rollback_restores_the_pre_statement_tree() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, 512, VacuumMode::None);
        pager.begin_write().unwrap();
        let root = create_table(&mut pager).unwrap();
        for rowid in 1..=100i64 {
            insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
        }
        let before: Vec<i64> = scan_table(&mut pager, root)
            .into_iter()
            .map(|(rowid, _)| rowid)
            .collect();

        pager.begin_statement().unwrap();
        for rowid in 101..=300i64 {
            insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
        }
        pager.rollback_statement().unwrap();

        let after: Vec<i64> = scan_table(&mut pager, root)
            .into_iter()
            .map(|(rowid, _)| rowid)
            .collect();
        assert_eq!(after, before);
        pager.commit().unwrap();
        check_roots(&mut pager, &[root]);
    }

    /// An auto-vacuum database keeps a pointer-map entry for every page, and
    /// the integrity check compares each one against the traversal.
    #[test]
    fn an_auto_vacuum_database_keeps_its_pointer_maps_correct() {
        for mode in [VacuumMode::Auto, VacuumMode::Incremental] {
            let vfs = MemoryVfs::new();
            let mut pager = create(&vfs, 512, mode);
            pager.begin_write().unwrap();
            let root = create_table(&mut pager).unwrap();
            for rowid in 1..=200i64 {
                insert_row(&mut pager, root, rowid, &blob_row(7, 900)).unwrap();
            }
            pager.commit().unwrap();
            check_roots(&mut pager, &[root]);

            pager.begin_write().unwrap();
            for rowid in (1..=200i64).step_by(2) {
                assert!(delete_row(&mut pager, root, rowid).unwrap());
            }
            pager.commit().unwrap();
            check_roots(&mut pager, &[root]);
        }
    }

    /// Dropping a tree returns every page it owned, including the pages of
    /// every overflow chain.
    #[test]
    fn dropping_a_tree_frees_every_page_it_owned() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, 512, VacuumMode::None);
        pager.begin_write().unwrap();
        let root = create_table(&mut pager).unwrap();
        for rowid in 1..=120i64 {
            insert_row(&mut pager, root, rowid, &blob_row(3, 1500)).unwrap();
        }
        pager.commit().unwrap();
        let grown = pager.page_count();

        pager.begin_write().unwrap();
        drop_tree(&mut pager, root).unwrap();
        pager.commit().unwrap();

        pager.begin_read().unwrap();
        let report = check::integrity_check(&mut pager).unwrap();
        assert!(report.is_ok(), "{:#?}", report.as_pragma_output());
        assert_eq!(
            u64::from(alloc::free_count(&pager)).saturating_add(1),
            u64::from(grown),
            "every page but page 1 should be on the freelist"
        );
    }

    /// A cursor left on a row survives a write that splits the page under it:
    /// its recorded versions go stale, it says so, and restoring puts it back
    /// on the same row.
    #[test]
    fn a_cursor_is_restored_across_a_write_that_moves_its_page() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, 512, VacuumMode::None);
        let limits = Limits::default();
        pager.begin_write().unwrap();
        let root = create_table(&mut pager).unwrap();
        for rowid in 1..=60i64 {
            insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
        }
        pager.commit().unwrap();

        let mut cursor = crate::cursor::BTreeCursor::table(root);
        assert!(cursor
            .seek_rowid(&mut pager, 30, crate::cursor::SeekBias::AtOrAfter)
            .unwrap());
        let saved = cursor.save_position(&mut pager, &limits).unwrap();
        assert_eq!(saved, crate::cursor::SavedPosition::Rowid(30));

        pager.begin_write().unwrap();
        for rowid in 61..=400i64 {
            insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
        }
        pager.commit().unwrap();

        assert!(
            cursor.needs_restore(&pager),
            "the pages under the cursor were rewritten, so it must know it is stale"
        );
        assert!(cursor.restore(&mut pager, &saved, &limits).unwrap());
        assert_eq!(cursor.rowid().unwrap(), 30);
        assert!(cursor.next(&mut pager).unwrap());
        assert_eq!(cursor.rowid().unwrap(), 31);
    }

    /// A cursor whose row is deleted lands on the next row rather than
    /// nowhere, which is what an interrupted scan needs.
    #[test]
    fn a_cursor_whose_row_was_deleted_lands_on_the_next_one() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, 512, VacuumMode::None);
        let limits = Limits::default();
        pager.begin_write().unwrap();
        let root = create_table(&mut pager).unwrap();
        for rowid in 1..=200i64 {
            insert_row(&mut pager, root, rowid, &row(rowid)).unwrap();
        }
        pager.commit().unwrap();

        let mut cursor = crate::cursor::BTreeCursor::table(root);
        assert!(cursor
            .seek_rowid(&mut pager, 100, crate::cursor::SeekBias::AtOrAfter)
            .unwrap());
        let saved = cursor.save_position(&mut pager, &limits).unwrap();

        pager.begin_write().unwrap();
        assert!(delete_row(&mut pager, root, 100).unwrap());
        pager.commit().unwrap();

        assert!(!cursor.restore(&mut pager, &saved, &limits).unwrap());
        assert_eq!(cursor.rowid().unwrap(), 101);
    }

    /// The partitioner never produces a page that does not fit, and never loses
    /// or duplicates an entry.
    #[test]
    fn partitioning_conserves_every_entry() {
        for capacity in [64usize, 200, 1000] {
            for count in [1usize, 2, 5, 17, 64] {
                let entries: Vec<Entry> = (0..count)
                    .map(|index| Entry {
                        body: vec![index as u8; 4 + (index % 11)],
                        child: None,
                        rowid: Some(index as i64),
                    })
                    .collect();
                for (promote, greedy) in
                    [(false, false), (true, false), (false, true), (true, true)]
                {
                    let ranges =
                        partition(&entries, PageKind::LeafTable, capacity, promote, greedy);
                    let Ok(ranges) = ranges else { continue };
                    let mut seen = Vec::new();
                    for (index, (start, end)) in ranges.iter().enumerate() {
                        assert!(start <= end);
                        let mut used = 0usize;
                        for entry in entries.get(*start..*end).unwrap() {
                            let cell = encode_entry(entry, PageKind::LeafTable).unwrap();
                            used += edit::cell_footprint(cell.len()) + 2;
                            seen.push(entry.rowid);
                        }
                        assert!(used <= capacity, "page {index} of {ranges:?} overflows");
                        if promote && index + 1 < ranges.len() {
                            seen.push(entries.get(*end).unwrap().rowid);
                        }
                    }
                    let expected: Vec<Option<i64>> =
                        (0..count).map(|index| Some(index as i64)).collect();
                    assert_eq!(seen, expected, "capacity {capacity} count {count}");
                }
            }
        }
    }
}
