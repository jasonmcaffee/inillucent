//! The freelist allocator: where a new page comes from, and where a dead one
//! goes.
//!
//! Invariant: a page has exactly one owner. It is in a B-tree, or in an
//! overflow chain, or on the freelist, or it is a pointer map, or it is page 1
//! or the lock-byte page - never two of those, and never none of them. Every
//! rule in this module exists to keep that true: a page cannot be freed twice,
//! a page that something still points at cannot be freed, and a page that is
//! reserved by the format cannot be handed out.
//!
//! The freelist itself is a chain of trunk pages, each holding a next pointer,
//! a count, and that many leaf page numbers. Allocation prefers a leaf of the
//! first trunk, then the trunk itself once it is empty, and only then grows the
//! file - which is the order that keeps a database from growing while it has
//! space inside it.
//!
//! The eight slots held back at the end of a trunk are SQLite's, and the reason
//! is compatibility rather than arithmetic: older versions computed the
//! capacity slightly differently, and a file that filled a trunk to the last
//! slot could not be read by them. Two slots are the trunk's own header and six
//! are the margin.

use inillucent_base::error::corrupt;
use inillucent_base::ids::PageId;
use inillucent_base::page::PageSize;
use inillucent_base::{bytes, DbResult};

use crate::header::VacuumMode;
use crate::pager::{FailSite, Pager};
use crate::ptrmap;

/// The byte offset SQLite reserves for its locking protocol.
///
/// The page holding it is never part of any structure, in any database, at any
/// page size. It is only reachable in a file larger than a gigabyte, which is
/// exactly why it has to be handled here rather than discovered later.
pub const PENDING_BYTE: u64 = 0x4000_0000;

/// Returns the page number that holds the lock byte.
pub fn lock_byte_page(page_size: PageSize) -> u32 {
    let page = PENDING_BYTE / u64::from(page_size.bytes());
    u32::try_from(page.saturating_add(1)).unwrap_or(u32::MAX)
}

/// Reports whether a page is one the format reserves.
pub fn is_reserved(pager: &Pager, page: PageId) -> DbResult<bool> {
    if page.get() == 1 {
        return Ok(true);
    }
    if page.get() == lock_byte_page(pager.page_size()) {
        return Ok(true);
    }
    ptrmap::is_map_page(pager, page)
}

/// Returns how many pages are on the freelist.
pub fn free_count(pager: &Pager) -> u32 {
    pager.header().freelist_count
}

/// Returns how many leaf slots one trunk page may hold.
///
/// Eight slots are held back: two are the trunk's own next pointer and count,
/// and six are the margin older SQLite versions need. Filling them would make a
/// file some readers refuse.
fn trunk_capacity(usable: u32) -> u32 {
    (usable / 4).saturating_sub(8)
}

/// Takes a page for a new use, from the freelist if it can and by growing the
/// file if it cannot.
///
/// The page comes back zeroed and dirty, and with no pointer-map entry: what it
/// is about to become is the caller's business, and writing an entry that says
/// "B-tree page" before it is one would leave a lie behind if the caller then
/// failed.
pub fn allocate_page(pager: &mut Pager) -> DbResult<PageId> {
    pager.reach_failpoint(FailSite::Allocate)?;
    let page = match take_from_freelist(pager)? {
        Some(page) => page,
        None => grow(pager)?,
    };
    if is_reserved(pager, page)? {
        return Err(corrupt(format!(
            "page {} is reserved by the file format and cannot be allocated",
            page.get()
        )));
    }
    pager.edit_page(page, |raw| {
        raw.fill(0);
        Ok(())
    })?;
    pager.record_allocated(page);
    pager.count_allocation();
    Ok(page)
}

/// Takes the first page off the freelist, or `None` when it is empty.
fn take_from_freelist(pager: &mut Pager) -> DbResult<Option<PageId>> {
    let mut header = *pager.header();
    if header.freelist_count == 0 || header.freelist_head == 0 {
        return Ok(None);
    }
    let trunk = PageId::from_persisted(header.freelist_head)
        .map_err(|_| corrupt("a freelist head of page zero"))?;
    if trunk.get() > pager.page_count() {
        return Err(corrupt(format!(
            "the freelist head is page {} outside a {}-page database",
            trunk.get(),
            pager.page_count()
        )));
    }
    let usable = pager.usable_size()?;
    let pin = pager.get_page(trunk)?;
    let next = bytes::read_u32(pin.bytes(), 0)?;
    let leaves = bytes::read_u32(pin.bytes(), 4)?;
    drop(pin);
    if leaves > trunk_capacity(usable).saturating_add(8) {
        return Err(corrupt(format!(
            "freelist trunk page {} claims {leaves} leaves",
            trunk.get()
        )));
    }

    if leaves == 0 {
        // The trunk itself is the page: it holds nothing, so taking it costs
        // one link change and returns the freelist to its shorter form.
        header.freelist_head = next;
        header.freelist_count = header.freelist_count.saturating_sub(1);
        pager.set_header(header)?;
        return Ok(Some(trunk));
    }

    let index = leaves.saturating_sub(1);
    let offset = 8usize.saturating_add((index as usize).saturating_mul(4));
    let pin = pager.get_page(trunk)?;
    let leaf = bytes::read_u32(pin.bytes(), offset)?;
    drop(pin);
    let leaf = PageId::from_persisted(leaf).map_err(|_| corrupt("a freelist leaf of page zero"))?;
    if leaf.get() > pager.page_count() {
        return Err(corrupt(format!(
            "freelist trunk page {} lists page {} outside the database",
            trunk.get(),
            leaf.get()
        )));
    }
    pager.edit_page(trunk, |raw| bytes::write_u32(raw, 4, index))?;
    header.freelist_count = header.freelist_count.saturating_sub(1);
    pager.set_header(header)?;
    Ok(Some(leaf))
}

/// Makes the database one page longer, skipping the pages the format reserves.
///
/// A pointer map that the new page needs is created here, because the map has
/// to exist before anything can record what the page is for, and the page after
/// it is the one the caller gets.
fn grow(pager: &mut Pager) -> DbResult<PageId> {
    let lock_byte = lock_byte_page(pager.page_size());
    for _ in 0..4 {
        let candidate = pager.page_count().saturating_add(1);
        // The limit an application set with `PRAGMA max_page_count` is what
        // bounds a runaway statement's effect on the disk, so it is enforced
        // here rather than by whoever set it: this is the only place the file
        // can get bigger.
        if candidate > pager.max_page_count() {
            return Err(inillucent_base::error::too_big(
                "database or disk is full: the page limit was reached",
            ));
        }
        let page = PageId::from_persisted(candidate)?;
        pager.set_page_count(candidate)?;
        if candidate == lock_byte {
            // The lock-byte page exists in the file and belongs to nothing.
            pager.edit_page(page, |raw| {
                raw.fill(0);
                Ok(())
            })?;
            continue;
        }
        if ptrmap::is_map_page(pager, page)? {
            pager.edit_page(page, |raw| {
                raw.fill(0);
                Ok(())
            })?;
            continue;
        }
        return Ok(page);
    }
    Err(corrupt("growing the database found no usable page"))
}

/// Takes one specific page for a new use.
///
/// Auto-vacuum needs this twice. A tree's root has to land at a low page
/// number, because a root is the one thing incremental vacuum cannot move - its
/// page number is the tree's name and only the catalog above can rewrite it -
/// so a root allocated at the end of the file would wedge every later vacuum.
/// And a vacuum step that finds the last page already free has to take *that*
/// page off the freelist rather than any page, because it is about to be
/// truncated away.
///
/// The page comes back zeroed and dirty, like any other allocation.
pub fn allocate_exact(pager: &mut Pager, target: PageId) -> DbResult<()> {
    pager.reach_failpoint(FailSite::Allocate)?;
    if is_reserved(pager, target)? {
        return Err(corrupt(format!(
            "page {} is reserved by the file format and cannot be allocated",
            target.get()
        )));
    }
    if target.get() > pager.page_count() {
        // Grow one page at a time so the pointer maps in between are created.
        while pager.page_count() < target.get() {
            let candidate = pager.page_count().saturating_add(1);
            let page = PageId::from_persisted(candidate)?;
            pager.set_page_count(candidate)?;
            pager.edit_page(page, |raw| {
                raw.fill(0);
                Ok(())
            })?;
        }
    } else {
        take_named_page_off_the_freelist(pager, target)?;
    }
    pager.edit_page(target, |raw| {
        raw.fill(0);
        Ok(())
    })?;
    pager.record_allocated(target);
    pager.count_allocation();
    Ok(())
}

/// Removes one named page from the freelist, wherever in the chain it is.
///
/// A page named as a leaf is swapped with the last leaf of its trunk, which
/// keeps the trunk's slots packed without moving the rest. A page that is
/// itself a *trunk* is harder: it holds the chain's links, so the links have to
/// go somewhere first. They are copied to a freshly allocated page, which then
/// takes the trunk's place in the chain - which is exactly the relocation
/// vacuum performs on any other page, done here without the pointer map because
/// a freelist trunk has no parent to fix up.
fn take_named_page_off_the_freelist(pager: &mut Pager, target: PageId) -> DbResult<()> {
    let usable = pager.usable_size()?;
    let mut previous_trunk: Option<PageId> = None;
    let mut next = pager.header().freelist_head;
    let limit = pager.page_count();
    let mut visited = 0u32;

    while next != 0 {
        visited = visited.saturating_add(1);
        if visited > limit {
            return Err(corrupt("a freelist chain longer than the database"));
        }
        let trunk = PageId::from_persisted(next)?;
        let pin = pager.get_page(trunk)?;
        let following = bytes::read_u32(pin.bytes(), 0)?;
        let leaves = bytes::read_u32(pin.bytes(), 4)?;
        if leaves > trunk_capacity(usable).saturating_add(8) {
            return Err(corrupt(format!(
                "freelist trunk page {} claims {leaves} leaves",
                trunk.get()
            )));
        }
        let mut slot: Option<u32> = None;
        for index in 0..leaves {
            let offset = 8usize.saturating_add((index as usize).saturating_mul(4));
            if bytes::read_u32(pin.bytes(), offset)? == target.get() {
                slot = Some(index);
                break;
            }
        }
        let last_leaf = if leaves == 0 {
            0
        } else {
            let offset =
                8usize.saturating_add((leaves.saturating_sub(1) as usize).saturating_mul(4));
            bytes::read_u32(pin.bytes(), offset)?
        };
        drop(pin);

        if let Some(index) = slot {
            let offset = 8usize.saturating_add((index as usize).saturating_mul(4));
            pager.edit_page(trunk, |raw| {
                bytes::write_u32(raw, offset, last_leaf)?;
                bytes::write_u32(raw, 4, leaves.saturating_sub(1))
            })?;
            let mut header = *pager.header();
            header.freelist_count = header.freelist_count.saturating_sub(1);
            pager.set_header(header)?;
            return Ok(());
        }

        if trunk == target {
            return unlink_trunk(pager, target, previous_trunk, following, leaves);
        }
        previous_trunk = Some(trunk);
        next = following;
    }
    Err(corrupt(format!(
        "page {} was asked for by number but is not on the freelist",
        target.get()
    )))
}

/// Takes a trunk page out of the chain, moving its links if it holds any.
fn unlink_trunk(
    pager: &mut Pager,
    trunk: PageId,
    previous: Option<PageId>,
    following: u32,
    leaves: u32,
) -> DbResult<()> {
    let mut header = *pager.header();
    if leaves == 0 {
        match previous {
            Some(before) => {
                pager.edit_page(before, |raw| bytes::write_u32(raw, 0, following))?;
            }
            None => {
                header.freelist_head = following;
            }
        }
        header.freelist_count = header.freelist_count.saturating_sub(1);
        pager.set_header(header)?;
        return Ok(());
    }

    // The trunk still holds leaf numbers, so its contents move to another page
    // and that page takes its place in the chain. The replacement comes off the
    // freelist itself, which is why the count does not change here: one page
    // leaves the list as the replacement and one leaves it as the target.
    let replacement = allocate_page(pager)?;
    let pin = pager.get_page(trunk)?;
    let image = pin.bytes().to_vec();
    drop(pin);
    pager.edit_page(replacement, |raw| {
        let target = bytes::window_mut(raw, 0, image.len())?;
        target.copy_from_slice(&image);
        Ok(())
    })?;
    match previous {
        Some(before) => {
            pager.edit_page(before, |raw| bytes::write_u32(raw, 0, replacement.get()))?;
        }
        None => {
            let mut header = *pager.header();
            header.freelist_head = replacement.get();
            pager.set_header(header)?;
        }
    }
    let mut header = *pager.header();
    header.freelist_count = header.freelist_count.saturating_sub(1);
    pager.set_header(header)?;
    ptrmap::put(pager, replacement, ptrmap::Entry::free())?;
    Ok(())
}

/// Returns a page to the freelist.
///
/// Everything that could still point at the page must already have stopped:
/// this is the last step of a delete, never the first. What it can check, it
/// does - the page is not reserved, is inside the database, and is not already
/// free - and the rest is what the integrity check is for.
pub fn free_page(pager: &mut Pager, page: PageId) -> DbResult<()> {
    pager.reach_failpoint(FailSite::Free)?;
    if page.get() == 1 {
        return Err(corrupt("page 1 cannot be freed"));
    }
    if page.get() > pager.page_count() {
        return Err(corrupt(format!(
            "page {} is outside a {}-page database and cannot be freed",
            page.get(),
            pager.page_count()
        )));
    }
    if is_reserved(pager, page)? {
        return Err(corrupt(format!(
            "page {} is reserved by the file format and cannot be freed",
            page.get()
        )));
    }
    if let Some(entry) = ptrmap::get(pager, page)? {
        if entry.kind == ptrmap::FREE_PAGE {
            return Err(corrupt(format!(
                "page {} is already on the freelist",
                page.get()
            )));
        }
    }
    pager.record_freed(page)?;

    let usable = pager.usable_size()?;
    let mut header = *pager.header();
    if cfg!(debug_assertions) {
        // Poisoning a freed page turns a use-after-free into a page that fails
        // validation immediately instead of one that reads plausibly stale
        // cells. It is a test-build cost only: nothing depends on the contents
        // of a page on the freelist.
        pager.edit_page(page, |raw| {
            raw.fill(0);
            Ok(())
        })?;
    }

    if header.freelist_head != 0 {
        let trunk = PageId::from_persisted(header.freelist_head)
            .map_err(|_| corrupt("a freelist head of page zero"))?;
        if trunk.get() <= pager.page_count() {
            let pin = pager.get_page(trunk)?;
            let leaves = bytes::read_u32(pin.bytes(), 4)?;
            drop(pin);
            if leaves < trunk_capacity(usable) {
                let offset = 8usize.saturating_add((leaves as usize).saturating_mul(4));
                let number = page.get();
                pager.edit_page(trunk, |raw| {
                    bytes::write_u32(raw, offset, number)?;
                    bytes::write_u32(raw, 4, leaves.saturating_add(1))
                })?;
                header.freelist_count = header.freelist_count.saturating_add(1);
                pager.set_header(header)?;
                ptrmap::put(pager, page, ptrmap::Entry::free())?;
                pager.count_free();
                return Ok(());
            }
        }
    }

    // Either there is no freelist or its first trunk is full, so the page
    // becomes a trunk of its own at the head of the chain.
    let next = header.freelist_head;
    pager.edit_page(page, |raw| {
        bytes::write_u32(raw, 0, next)?;
        bytes::write_u32(raw, 4, 0)
    })?;
    header.freelist_head = page.get();
    header.freelist_count = header.freelist_count.saturating_add(1);
    pager.set_header(header)?;
    ptrmap::put(pager, page, ptrmap::Entry::free())?;
    pager.count_free();
    Ok(())
}

/// Returns every page on the freelist, in the order the chain names them.
///
/// This is a diagnostic and a test helper, and it is bounded by the database's
/// own page count so a cyclic chain reports corruption instead of hanging.
pub fn freelist_pages(pager: &mut Pager) -> DbResult<Vec<PageId>> {
    let mut pages = Vec::new();
    let mut next = pager.header().freelist_head;
    let limit = pager.page_count();
    let mut trunks = 0u32;
    while next != 0 {
        trunks = trunks.saturating_add(1);
        if trunks > limit {
            return Err(corrupt("a freelist chain longer than the database"));
        }
        let trunk = PageId::from_persisted(next)?;
        if trunk.get() > limit {
            return Err(corrupt(format!(
                "the freelist names page {} outside the database",
                trunk.get()
            )));
        }
        pages.push(trunk);
        let pin = pager.get_page(trunk)?;
        let following = bytes::read_u32(pin.bytes(), 0)?;
        let leaves = bytes::read_u32(pin.bytes(), 4)?;
        for index in 0..leaves {
            let offset = 8usize.saturating_add((index as usize).saturating_mul(4));
            let leaf = bytes::read_u32(pin.bytes(), offset)?;
            pages.push(PageId::from_persisted(leaf)?);
        }
        drop(pin);
        next = following;
    }
    Ok(pages)
}

/// Reports whether the database keeps a pointer map, which decides whether a
/// freed page can be told from a live one without a traversal.
pub fn tracks_ownership(pager: &Pager) -> bool {
    pager.header().vacuum_mode != VacuumMode::None
}
