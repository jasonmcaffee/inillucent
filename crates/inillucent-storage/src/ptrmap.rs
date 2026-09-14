//! Pointer maps: the reverse pointers an auto-vacuum database keeps.
//!
//! Invariant: in a file with a vacuum mode, every page that is not itself a
//! pointer map and is not page 1 has exactly one pointer-map entry, and that
//! entry names the page that points *at* it. That is the whole reason the maps
//! exist: moving a page means finding the one reference to it, and without a
//! reverse pointer that means searching the file.
//!
//! Page 2 is the first map, and every `entries_per_map + 1` page after it is
//! another; each map covers the pages that follow it. The arithmetic lives in
//! `DatabaseHeader::pointer_map_page` so that the reader and the writer
//! cannot disagree about which page covers which.
//!
//! An entry is five bytes: a one-byte kind and a four-byte parent. The kinds
//! are the file format's own, and each one says what "parent" means, which is
//! different in each case - for a B-tree child it is the page holding the
//! pointer, for the second and later pages of an overflow chain it is the
//! previous page in the chain, and for a root page there is no parent at all.

use inillucent_base::error::corrupt;
use inillucent_base::ids::PageId;
use inillucent_base::{bytes, DbResult};

use crate::btree::BTreePage;
use crate::header::VacuumMode;
use crate::pager::Pager;

/// The page is the root of a B-tree and nothing points at it.
pub const ROOT_PAGE: u8 = 1;
/// The page is on the freelist.
pub const FREE_PAGE: u8 = 2;
/// The page is the first page of an overflow chain; the parent is the B-tree
/// page holding the cell.
pub const OVERFLOW1: u8 = 3;
/// The page is a later page of an overflow chain; the parent is the page
/// before it in the chain.
pub const OVERFLOW2: u8 = 4;
/// The page is a B-tree page; the parent is the page holding the pointer.
pub const BTREE_CHILD: u8 = 5;

/// How many bytes one entry takes.
pub const ENTRY_SIZE: usize = 5;

/// One pointer-map entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry {
    /// What the page is being used for.
    pub kind: u8,
    /// The page that points at it, or zero when nothing does.
    pub parent: u32,
}

impl Entry {
    /// Builds an entry for a B-tree page whose parent holds the pointer.
    pub fn child_of(parent: PageId) -> Entry {
        Entry {
            kind: BTREE_CHILD,
            parent: parent.get(),
        }
    }

    /// Builds an entry for the first page of an overflow chain.
    pub fn overflow_head(parent: PageId) -> Entry {
        Entry {
            kind: OVERFLOW1,
            parent: parent.get(),
        }
    }

    /// Builds an entry for a later page of an overflow chain.
    pub fn overflow_next(previous: PageId) -> Entry {
        Entry {
            kind: OVERFLOW2,
            parent: previous.get(),
        }
    }

    /// Builds an entry for a B-tree root.
    pub fn root() -> Entry {
        Entry {
            kind: ROOT_PAGE,
            parent: 0,
        }
    }

    /// Builds an entry for a page on the freelist.
    pub fn free() -> Entry {
        Entry {
            kind: FREE_PAGE,
            parent: 0,
        }
    }

    /// Returns a name for a message.
    pub fn describe(&self) -> &'static str {
        match self.kind {
            ROOT_PAGE => "a B-tree root",
            FREE_PAGE => "a free page",
            OVERFLOW1 => "the head of an overflow chain",
            OVERFLOW2 => "a page inside an overflow chain",
            BTREE_CHILD => "a B-tree page",
            _ => "an unknown kind of page",
        }
    }
}

/// Reports whether the database keeps pointer maps at all.
pub fn is_enabled(pager: &Pager) -> bool {
    pager.header().vacuum_mode != VacuumMode::None
}

/// Returns how many entries one map page holds.
pub fn entries_per_map(pager: &Pager) -> DbResult<u32> {
    let usable = pager.usable_size()?;
    let entries = usable / ENTRY_SIZE as u32;
    if entries == 0 {
        return Err(corrupt("a page too small to hold a pointer-map entry"));
    }
    Ok(entries)
}

/// Returns where in the file `page`'s entry lives.
fn locate(pager: &Pager, page: PageId) -> DbResult<Option<(PageId, usize)>> {
    let Some(map_page) = pager.header().pointer_map_page(page)? else {
        return Ok(None);
    };
    let index = page
        .get()
        .checked_sub(map_page.get())
        .and_then(|offset| offset.checked_sub(1))
        .ok_or_else(|| corrupt("a page before the map that covers it"))?;
    let offset = (index as usize)
        .checked_mul(ENTRY_SIZE)
        .ok_or_else(|| corrupt("a pointer-map offset that overflows"))?;
    if offset.saturating_add(ENTRY_SIZE) > pager.usable_size()? as usize {
        return Err(corrupt(format!(
            "page {} has no entry on map page {}",
            page.get(),
            map_page.get()
        )));
    }
    Ok(Some((map_page, offset)))
}

/// Reads a page's pointer-map entry, or `None` when the file has no maps or
/// the page is not covered by one.
pub fn get(pager: &mut Pager, page: PageId) -> DbResult<Option<Entry>> {
    if !is_enabled(pager) {
        return Ok(None);
    }
    let Some((map_page, offset)) = locate(pager, page)? else {
        return Ok(None);
    };
    if map_page.get() > pager.page_count() {
        return Ok(None);
    }
    let pin = pager.get_page(map_page)?;
    let kind = bytes::read_u8(pin.bytes(), offset)?;
    let parent = bytes::read_u32(pin.bytes(), offset.saturating_add(1))?;
    Ok(Some(Entry { kind, parent }))
}

/// Writes a page's pointer-map entry, growing the file to hold the map page if
/// it does not exist yet.
pub fn put(pager: &mut Pager, page: PageId, entry: Entry) -> DbResult<()> {
    if !is_enabled(pager) {
        return Ok(());
    }
    let Some((map_page, offset)) = locate(pager, page)? else {
        return Ok(());
    };
    if map_page.get() > pager.page_count() {
        return Err(corrupt(format!(
            "page {} needs map page {}, which is outside the database",
            page.get(),
            map_page.get()
        )));
    }
    pager.edit_page(map_page, |raw| {
        bytes::write_u8(raw, offset, entry.kind)?;
        bytes::write_u32(raw, offset.saturating_add(1), entry.parent)
    })
}

/// Reports whether a page is itself a pointer map.
pub fn is_map_page(pager: &Pager, page: PageId) -> DbResult<bool> {
    if !is_enabled(pager) {
        return Ok(false);
    }
    pager.header().is_pointer_map_page(page)
}

/// Writes the entries that describe everything one B-tree page points at.
///
/// Every mutation of a B-tree page ends here. Calling it once, from one place,
/// after the page's bytes are final is what keeps a moved cell from leaving a
/// stale reverse pointer behind: the page is the authority on what it points
/// at, so its entries are derived from it rather than maintained alongside it.
pub fn refresh_btree_page(pager: &mut Pager, page: PageId) -> DbResult<()> {
    if !is_enabled(pager) {
        return Ok(());
    }
    let usable = pager.usable_size()?;
    let pin = pager.get_page(page)?;
    let layout = pin.layout(usable)?;
    let view = BTreePage::new(pin.bytes(), &layout);
    let mut updates: Vec<(PageId, Entry)> = Vec::new();
    for index in 0..layout.cell_count {
        let cell = view.cell(index)?;
        if let Some(child) = cell.left_child {
            updates.push((child, Entry::child_of(page)));
        }
        if let Some(head) = cell.overflow {
            updates.push((head, Entry::overflow_head(page)));
        }
    }
    if let Some(right) = layout.right_child {
        updates.push((right, Entry::child_of(page)));
    }
    drop(pin);
    for (target, entry) in updates {
        put(pager, target, entry)?;
    }
    Ok(())
}
