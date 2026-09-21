//! Which pages a tree holds, for the two callers that have to know.
//!
//! Invariant: **both walks descend the interior levels from the root, and what
//! separates them is what they report.** [`PagedTree::pages`] answers what a
//! `DROP` gives back to the free map - the interior pages and the leaves.
//! [`PagedTree::pages_occupied`] answers what the integrity checker has to
//! account for, which is those plus every page the leaves' out-of-line values
//! sit on.
//!
//! **The difference is a real one and the engine is on the wrong side of it in
//! one place.** `release_tree` calls the first, so `DROP TABLE` gives back a
//! table's leaves and keeps the pages its large values were on - a leak
//! task-2052 measured and task-2065 closes. They are in one file so that the
//! next caller has to choose between them with both in front of it.

use inillucent_base::DbResult;
use inillucent_pool::extent;
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

impl PagedTree {
    /// Returns every page the tree occupies, interior pages and leaves.
    ///
    /// For `DROP`, which gives them back to the free map. It walks the interior
    /// levels rather than following the sibling chain, because the chain only
    /// reaches the leaves and a dropped tree that left its interior pages behind
    /// would leak a page per fanout for the life of the file.
    ///
    /// The walk is level-order from the root, and a page that appears twice -
    /// which a corrupt file could produce - is returned once, because handing
    /// the same page to the free map twice is worse than leaking it.
    ///
    /// @param pool - the buffer pool the file is open through
    pub fn pages(&self, pool: &Pool) -> DbResult<Vec<PageId>> {
        let mut seen: Vec<PageId> = Vec::new();
        let mut frontier: Vec<PageId> = vec![self.root];
        while let Some(page) = frontier.pop() {
            if page.is_none() || seen.contains(&page) {
                continue;
            }
            seen.push(page);
            let image = {
                let guard = pool.fetch(page)?;
                guard.bytes().to_vec()
            };
            if page::kind_of(&image)? == PageKind::Leaf {
                continue;
            }
            let interior = InteriorRef::parse(&image)?;
            for child in 0..interior.children() {
                let swip = interior.swip(child)?;
                frontier.push(pool.page_of_swip(swip)?);
            }
        }
        Ok(seen)
    }

    /// Returns every page the tree occupies, including its values' pages.
    ///
    /// [`PagedTree::pages`] answers what a `DROP` gives back: the interior
    /// pages and the leaves. This answers what a page-ownership check has to
    /// account for, which is that plus every page the leaves' out-of-line
    /// values sit on - and those are most of a file holding large values, so a
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
        let mut seen: std::collections::HashSet<PageId> = std::collections::HashSet::new();
        let mut frontier: Vec<PageId> = vec![self.root];
        while let Some(page) = frontier.pop() {
            if page.is_none() || !seen.insert(page) {
                continue;
            }
            found.push((page, PageShare::Owned));
            // **The page is read under its guard and nothing is copied out of
            // it.** `PagedTree::pages` copies the image because it was written
            // for `DROP`, which walks a tree once; this walks every page of
            // every tree of the file, so a page-sized copy per page is a copy
            // of the whole database. What leaves the guard is the child list
            // or the extent references, both of which are small, and neither
            // is held across another fetch - the rule `count_leaves` follows.
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
