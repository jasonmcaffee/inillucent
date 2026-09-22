//! Which pages a tree holds, for the two callers that have to know.
//!
//! Invariant: **both walks descend the interior levels from the root, and what
//! separates them is what they report.** [`released`] answers what a `DROP`
//! gives back to the free map. [`PagedTree::pages_occupied`] answers what the
//! integrity checker has to account for. Both reach an out-of-line value's
//! pages; they differ in what they hand back about one, because the checker
//! only has to name the pages and the `DROP` has to free them, and freeing a
//! small value's page is not the same as freeing its slot.
//!
//! **The two used to differ in something else, and that was a leak.** The walk
//! a `DROP` used reported the interior pages and the leaves and nothing else,
//! so `release_tree` gave back a table's tree and kept every page its large
//! values sat on - measured by task-2052 and closed by task-2065. They are in
//! one file so that the next caller has to choose between them with both in
//! front of it.

use std::collections::HashSet;

use inillucent_base::DbResult;
use inillucent_pool::extent::{self, ExtentRef};
use inillucent_pool::interior::InteriorRef;
use inillucent_pool::page::{self, PageKind};
use inillucent_pool::{PageId, Pool};

use crate::leaf::LeafRef;

use super::PagedTree;

/// Whether a page [`PagedTree::pages_occupied`] reports may also belong to
/// another tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageShare {
    /// The tree's alone. Another tree naming it is corruption.
    Owned,
    /// A shared extent page, which holds small out-of-line values from
    /// whichever trees wrote them. Several trees naming one is ordinary.
    Shared,
}

/// Everything a dropped tree gives back to the free map.
///
/// **Two lists because the two are given back differently.** A page of the
/// tree itself belongs to that tree alone, so it goes straight to the free
/// map. A page an out-of-line value sits on may not: a value small enough to
/// be packed shares its page with values from whatever other trees wrote
/// there, because the hint that finds such a page is on the `Database` and not
/// on the tree. Freeing that page because this tree used it would free a page
/// another tree's value is still on. `paged::free_extent` is what knows the
/// difference, and it needs the reference rather than the page number, so the
/// reference is what is carried here.
pub struct Released {
    /// The tree's own pages - its interior pages and its leaves.
    pub pages: Vec<PageId>,
    /// One reference per out-of-line value the leaves hold.
    pub values: Vec<ExtentRef>,
}

/// Returns everything a tree rooted at one page gives back when it is dropped.
///
/// **It takes the root page rather than a [`PagedTree`], because the caller
/// that needs it most no longer holds one.** A `CREATE TABLE` abandoned by a
/// rollback has to give its tree's pages back after the handle has gone from
/// the schema - and a tree dropped and then rebuilt under the same handle by
/// an `ALTER TABLE` leaves that handle holding a different tree entirely. The
/// root page is the one thing that still identifies the tree in either case.
///
/// The walk is level-order from the root and visits each page once, so a
/// corrupt file whose interior pages point at each other terminates rather
/// than running for ever, and a page that appears twice is returned once -
/// handing the same page to the free map twice is worse than leaking it.
///
/// @param pool - the buffer pool the file is open through
/// @param root - the page the tree's root sits on
pub fn released(pool: &Pool, root: PageId) -> DbResult<Released> {
    let mut found = Released {
        pages: Vec::new(),
        values: Vec::new(),
    };
    let mut seen: HashSet<PageId> = HashSet::new();
    // **One value can be named twice by one leaf**: a row and a delta standing
    // over it carry the same reference until the leaf is repacked, and
    // `extent_refs` reports both because it reports what the leaf holds. A run
    // freed twice hands the same pages to the free map twice, so the reference
    // is what is deduplicated rather than the page - a packed value's page is
    // legitimately named by every value on it.
    let mut held: HashSet<(u64, Option<u16>)> = HashSet::new();
    let mut frontier: Vec<PageId> = vec![root];
    while let Some(page) = frontier.pop() {
        if page.is_none() || !seen.insert(page) {
            continue;
        }
        found.pages.push(page);
        // The page is read under its guard and nothing is copied out of it,
        // for the reason `pages_occupied` gives: what leaves the guard is the
        // child list or the extent references, both of which are small.
        let references = {
            let guard = pool.fetch(page)?;
            let bytes = guard.bytes();
            if page::kind_of(bytes)? != PageKind::Leaf {
                let interior = InteriorRef::parse(bytes)?;
                let mut children = Vec::with_capacity(interior.children());
                for child in 0..interior.children() {
                    children.push(interior.swip(child)?);
                }
                drop(guard);
                for swip in children {
                    frontier.push(pool.page_of_swip(swip)?);
                }
                continue;
            }
            let leaf = LeafRef::parse(bytes)?;
            PagedTree::extent_refs(&leaf)?
        };
        for reference in references {
            if held.insert((reference.first.0, reference.slot)) {
                found.values.push(reference);
            }
        }
    }
    Ok(found)
}

impl PagedTree {
    /// Returns every page the tree occupies, including its values' pages.
    ///
    /// [`released`] answers what a `DROP` gives back. This answers what a
    /// page-ownership check has to account for, which is the same set of pages
    /// said a different way - every page the leaves' out-of-line values sit on
    /// is included, and those are most of a file holding large values, so a
    /// check that left them out would call every one of them unused.
    ///
    /// Each page is said to be the tree's alone or shared, and the distinction
    /// is not a nicety: a shared extent page holds small values from whatever
    /// tree wrote them, because the hint that finds one is on the `Database`
    /// and not on the tree (see `write_packed_extent`). Two trees naming one is
    /// ordinary; two trees naming a leaf is the corruption the check exists
    /// for.
    ///
    /// The walk visits each page once, so a corrupt file whose interior pages
    /// point at each other terminates rather than running for ever.
    ///
    /// @param pool - the buffer pool the file is open through
    pub fn pages_occupied(&self, pool: &Pool) -> DbResult<Vec<(PageId, PageShare)>> {
        let page_size = pool.page_size();
        let mut found: Vec<(PageId, PageShare)> = Vec::new();
        let mut seen: HashSet<PageId> = HashSet::new();
        let mut frontier: Vec<PageId> = vec![self.root];
        while let Some(page) = frontier.pop() {
            if page.is_none() || !seen.insert(page) {
                continue;
            }
            found.push((page, PageShare::Owned));
            // **The page is read under its guard and nothing is copied out of
            // it.** This walks every page of every tree of the file, so a
            // page-sized copy per page is a copy of the whole database. What
            // leaves the guard is the child list or the extent references,
            // both of which are small, and neither is held across another
            // fetch - the rule `count_leaves` follows.
            let references = {
                let guard = pool.fetch(page)?;
                let bytes = guard.bytes();
                if page::kind_of(bytes)? != PageKind::Leaf {
                    let interior = InteriorRef::parse(bytes)?;
                    let mut children = Vec::with_capacity(interior.children());
                    for child in 0..interior.children() {
                        children.push(interior.swip(child)?);
                    }
                    drop(guard);
                    for swip in children {
                        frontier.push(pool.page_of_swip(swip)?);
                    }
                    continue;
                }
                let leaf = LeafRef::parse(bytes)?;
                PagedTree::extent_refs(&leaf)?
            };
            for reference in references {
                if reference.slot.is_some() {
                    found.push((reference.first, PageShare::Shared));
                    continue;
                }
                // A run is contiguous from its first page, and `free_extent`
                // gives back exactly this many - so the two agree about which
                // pages the value holds by computing it the same way.
                let pages = extent::pages_needed(reference.length, page_size).max(1);
                for offset in 0..pages {
                    found.push((
                        PageId(reference.first.0.saturating_add(offset)),
                        PageShare::Owned,
                    ));
                }
            }
        }
        Ok(found)
    }
}
