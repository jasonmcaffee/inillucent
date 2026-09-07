//! The free map: one bit per page, spread over kind-`4` pages chained by the
//! common header's sibling field.
//!
//! Invariant: a page is allocated if and only if its bit is set, and the map
//! covers every page in the file. "Covers" is the load-bearing half: a file
//! that grew past the last map page has pages nobody can describe, so growing
//! the file and extending the chain are one operation ([`FreeMap::ensure`])
//! rather than two that could be interleaved.
//!
//! Bit `n` of the map is page `n` of the file, counting from page zero. The two
//! meta pages and the map pages themselves are allocated from the moment they
//! exist, so nothing can hand them out.
//!
//! ## Why a bitmap and not a free list
//!
//! A free list is one page write per allocation and gives no way to find a
//! *contiguous run*, which is what a blob extent wants. A bitmap answers both:
//! allocation is a scan from a hint, and a run of `k` pages is a run of `k`
//! clear bits. At 32 KiB pages one map page describes 261,888 pages, which is
//! 8 GiB of file - so the chain is one page long for every database this engine
//! is going to see, and the scan is over a resident page.

use inillucent_base::error::corrupt;
use inillucent_base::DbResult;

use crate::page::{self, PageKind, COMMON_HEADER};
use crate::PageId;

/// How many pages one map page can describe.
///
/// @param page_size - the database's page size in bytes
pub fn pages_per_map(page_size: usize) -> usize {
    page_size.saturating_sub(COMMON_HEADER).saturating_mul(8)
}

/// The free map over a run of map pages held as page images.
///
/// The map is read and written through the pool like any other page; this type
/// is the codec and the allocation policy, and it never touches a file.
pub struct FreeMap {
    /// The page size, which fixes how many pages one map page describes.
    page_size: usize,
    /// The map pages in chain order, with their page ids.
    pages: Vec<(PageId, Vec<u8>)>,
    /// Where the next allocation scan begins, as a global page index.
    hint: u64,
}

impl FreeMap {
    /// Returns an empty map with no pages yet.
    ///
    /// @param page_size - the database's page size in bytes
    pub fn new(page_size: usize) -> FreeMap {
        FreeMap {
            page_size,
            pages: Vec::new(),
            hint: 0,
        }
    }

    /// Returns how many map pages the chain holds.
    pub fn chain_len(&self) -> usize {
        self.pages.len()
    }

    /// Returns the first map page's id, or [`PageId::NONE`] for an empty chain.
    pub fn first(&self) -> PageId {
        self.pages
            .first()
            .map(|(id, _)| *id)
            .unwrap_or(PageId::NONE)
    }

    /// Returns the map pages in chain order, for writing back.
    pub fn pages(&self) -> impl Iterator<Item = (PageId, &[u8])> {
        self.pages.iter().map(|(id, bytes)| (*id, bytes.as_slice()))
    }

    /// Adds a map page read from the file to the end of the chain.
    ///
    /// @param id - the map page's id
    /// @param bytes - the page image
    pub fn push_page(&mut self, id: PageId, bytes: Vec<u8>) -> DbResult<()> {
        if bytes.len() != self.page_size {
            return Err(corrupt(format!(
                "free-map page {} is {} bytes, not {}",
                id.0,
                bytes.len(),
                self.page_size
            )));
        }
        if page::kind_of(&bytes)? != PageKind::FreeMap {
            return Err(corrupt(format!("page {} is not a free-map page", id.0)));
        }
        self.pages.push((id, bytes));
        Ok(())
    }

    /// Returns the bit position of a page within the chain.
    ///
    /// @param page - the page to locate
    fn locate(&self, page: PageId) -> (usize, usize, u8) {
        let per = pages_per_map(self.page_size).max(1) as u64;
        let map = (page.0 / per) as usize;
        let within = (page.0 % per) as usize;
        (
            map,
            COMMON_HEADER.saturating_add(within / 8),
            1u8 << (within % 8),
        )
    }

    /// Reports whether a page is allocated.
    ///
    /// A page beyond the chain is reported allocated, because a page the map
    /// cannot describe is a page nothing may hand out.
    ///
    /// @param page - the page to test
    pub fn is_allocated(&self, page: PageId) -> bool {
        let (map, byte, mask) = self.locate(page);
        match self.pages.get(map).and_then(|(_, bytes)| bytes.get(byte)) {
            Some(value) => value & mask != 0,
            None => true,
        }
    }

    /// Marks a page allocated.
    ///
    /// @param page - the page to claim
    pub fn allocate_at(&mut self, page: PageId) -> DbResult<()> {
        self.set(page, true)
    }

    /// Marks a page free.
    ///
    /// @param page - the page to release
    pub fn free(&mut self, page: PageId) -> DbResult<()> {
        if page.0 < crate::meta::FIRST_DATA_PAGE.0 {
            return Err(corrupt(format!("page {} is a meta page", page.0)));
        }
        self.set(page, false)?;
        self.hint = self.hint.min(page.0);
        Ok(())
    }

    /// Sets or clears one bit.
    ///
    /// @param page - the page the bit describes
    /// @param allocated - what the bit should say
    fn set(&mut self, page: PageId, allocated: bool) -> DbResult<()> {
        let (map, byte, mask) = self.locate(page);
        let slot = self
            .pages
            .get_mut(map)
            .and_then(|(_, bytes)| bytes.get_mut(byte))
            .ok_or_else(|| corrupt(format!("page {} is outside the free map", page.0)))?;
        if allocated {
            *slot |= mask;
        } else {
            *slot &= !mask;
        }
        Ok(())
    }

    /// Grows the chain until it describes `pages` pages, taking new map pages
    /// from the run being described.
    ///
    /// Returns the ids of the map pages it created, which the caller writes.
    ///
    /// @param pages - how many pages the file will hold
    /// @param next_page - where the file currently ends, updated as it grows
    pub fn ensure(&mut self, pages: u64, next_page: &mut u64) -> DbResult<Vec<PageId>> {
        let per = pages_per_map(self.page_size).max(1) as u64;
        let mut created = Vec::new();
        while (self.pages.len() as u64).saturating_mul(per) < pages.max(*next_page) {
            // The map page is itself a page of the file, taken from the end.
            let id = PageId(*next_page);
            *next_page = next_page.saturating_add(1);
            let mut bytes = vec![0u8; self.page_size];
            page::write_common(&mut bytes, PageKind::FreeMap, 0, 0)?;
            if let Some((_, previous)) = self.pages.last_mut() {
                page::set_right(previous, id)?;
            }
            self.pages.push((id, bytes));
            created.push(id);
            // Everything up to and including the new map page is allocated:
            // the meta pages, whatever the caller has already placed, and the
            // map pages themselves.
            for claimed in 0..*next_page {
                self.set(PageId(claimed), true).ok();
            }
        }
        Ok(created)
    }

    /// Returns a run of `count` contiguous free pages, marking them allocated.
    ///
    /// Returns `None` when the map has no run that long, which the caller
    /// answers by growing the file.
    ///
    /// @param count - how many contiguous pages are wanted
    pub fn allocate_run(&mut self, count: u64) -> DbResult<Option<PageId>> {
        if count == 0 {
            return Ok(None);
        }
        let total = (self.pages.len() as u64).saturating_mul(pages_per_map(self.page_size) as u64);
        let mut start = self.hint.max(crate::meta::FIRST_DATA_PAGE.0);
        while start.saturating_add(count) <= total {
            let mut run = 0u64;
            while run < count && !self.is_allocated(PageId(start.saturating_add(run))) {
                run = run.saturating_add(1);
            }
            if run == count {
                for offset in 0..count {
                    self.set(PageId(start.saturating_add(offset)), true)?;
                }
                self.hint = start.saturating_add(count);
                return Ok(Some(PageId(start)));
            }
            // Skip past the page that broke the run rather than retrying at
            // `start + 1`, which would rescan the same clear bits.
            start = start.saturating_add(run).saturating_add(1);
        }
        Ok(None)
    }

    /// Returns how many pages the chain can describe at all.
    ///
    /// A file may never grow past this without the chain growing first, which
    /// is what [`FreeMap::ensure`] arranges.
    pub fn described_pages(&self) -> u64 {
        (self.pages.len() as u64).saturating_mul(pages_per_map(self.page_size) as u64)
    }

    /// Returns how many pages the map describes as free.
    pub fn free_count(&self) -> u64 {
        let total = (self.pages.len() as u64).saturating_mul(pages_per_map(self.page_size) as u64);
        self.free_count_below(total)
    }

    /// Returns how many pages *of the file* the map describes as free.
    ///
    /// **The number `PRAGMA freelist_count` wants, and the reason it answered
    /// 261,883 on a five-page database.** [`FreeMap::free_count`] counts every
    /// bit the map has room for, and one map page over a 4 KiB file describes
    /// 32,736 pages whether or not the file has them - so it was reporting the
    /// map's capacity, and a `DELETE` could not move it. Free means a page the
    /// file has and nothing is using; a page id past the end of the file is not
    /// free, it does not exist.
    ///
    /// @param page_count - how many pages the file holds
    pub fn free_count_below(&self, page_count: u64) -> u64 {
        let total = (self.pages.len() as u64).saturating_mul(pages_per_map(self.page_size) as u64);
        (0..total.min(page_count))
            .filter(|page| !self.is_allocated(PageId(*page)))
            .count() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a map over a file of `pages` pages with the meta pages claimed.
    fn map_over(page_size: usize, pages: u64) -> (FreeMap, u64) {
        let mut map = FreeMap::new(page_size);
        let mut next = crate::meta::FIRST_DATA_PAGE.0;
        map.ensure(pages, &mut next).unwrap();
        (map, next)
    }

    /// One map page describes the whole page minus the header, eight pages per
    /// byte.
    #[test]
    fn a_map_page_describes_the_page_in_bits() {
        assert_eq!(pages_per_map(512), (512 - 32) * 8);
        assert_eq!(pages_per_map(32_768), (32_768 - 32) * 8);
        assert_eq!(pages_per_map(0), 0);
    }

    /// The meta pages and the map page itself are allocated from the start, so
    /// nothing can hand them out.
    #[test]
    fn the_meta_pages_and_the_map_are_never_free() {
        let (map, next) = map_over(512, 100);
        assert!(map.is_allocated(PageId(0)));
        assert!(map.is_allocated(PageId(1)));
        assert!(map.is_allocated(PageId(2)), "the map page itself");
        assert_eq!(next, 3);
        assert!(!map.is_allocated(PageId(3)));
        assert_eq!(map.chain_len(), 1);
        assert_eq!(map.first(), PageId(2));
    }

    /// Allocation hands out the first free page and freeing gives it back.
    #[test]
    fn allocation_and_freeing_round_trip() {
        let (mut map, _) = map_over(512, 100);
        let first = map.allocate_run(1).unwrap().unwrap();
        assert_eq!(first, PageId(3));
        let second = map.allocate_run(1).unwrap().unwrap();
        assert_eq!(second, PageId(4));
        assert!(map.is_allocated(first));
        map.free(first).unwrap();
        assert!(!map.is_allocated(first));
        assert_eq!(map.allocate_run(1).unwrap(), Some(first));
    }

    /// A contiguous run skips over an allocated page rather than straddling it.
    #[test]
    fn a_run_is_contiguous() {
        let (mut map, _) = map_over(512, 100);
        map.allocate_at(PageId(5)).unwrap();
        let run = map.allocate_run(4).unwrap().unwrap();
        assert_eq!(run, PageId(6), "3 and 4 are free but 5 is not");
        for offset in 0..4 {
            assert!(map.is_allocated(PageId(6 + offset)));
        }
    }

    /// A run longer than the map has room for is refused rather than
    /// half-allocated.
    #[test]
    fn a_run_that_does_not_fit_is_refused() {
        let (mut map, _) = map_over(512, 40);
        let total = pages_per_map(512) as u64;
        assert_eq!(map.allocate_run(total + 1).unwrap(), None);
        assert_eq!(map.allocate_run(0).unwrap(), None);
        // Nothing was claimed by the refusal.
        assert!(!map.is_allocated(PageId(3)));
    }

    /// A page beyond the chain reads as allocated, so an allocator can never
    /// hand out a page the map does not describe.
    #[test]
    fn a_page_outside_the_map_is_allocated() {
        let (map, _) = map_over(512, 10);
        let beyond = pages_per_map(512) as u64 + 1;
        assert!(map.is_allocated(PageId(beyond)));
        let mut map = map;
        assert!(map.free(PageId(beyond)).is_err());
        assert!(map.allocate_at(PageId(beyond)).is_err());
    }

    /// Freeing a meta page is refused.
    #[test]
    fn a_meta_page_cannot_be_freed() {
        let (mut map, _) = map_over(512, 10);
        assert!(map.free(PageId(0)).is_err());
        assert!(map.free(PageId(1)).is_err());
        assert!(map.free(PageId(2)).is_ok(), "the map page is ordinary");
    }

    /// The chain grows to cover a file bigger than one map page, and the new
    /// map page is linked from the one before it.
    #[test]
    fn the_chain_grows_and_links() {
        let per = pages_per_map(512) as u64;
        let (map, _) = map_over(512, per * 2 + 5);
        assert!(map.chain_len() >= 2, "chain is {}", map.chain_len());
        let ids: Vec<PageId> = map.pages().map(|(id, _)| id).collect();
        let images: Vec<&[u8]> = map.pages().map(|(_, bytes)| bytes).collect();
        for index in 0..ids.len().saturating_sub(1) {
            assert_eq!(
                page::right_of(images[index]).unwrap(),
                ids[index + 1],
                "map page {index} does not link to the next"
            );
        }
        assert_eq!(page::right_of(images[ids.len() - 1]).unwrap(), PageId::NONE);
    }

    /// A map page read back from bytes must be a map page.
    #[test]
    fn a_pushed_page_must_be_a_map_page() {
        let mut map = FreeMap::new(512);
        let mut wrong = vec![0u8; 512];
        page::write_common(&mut wrong, PageKind::Leaf, 0, 0).unwrap();
        assert!(map.push_page(PageId(2), wrong).is_err());
        let mut short = vec![0u8; 256];
        page::write_common(&mut short, PageKind::FreeMap, 0, 0).unwrap();
        assert!(map.push_page(PageId(2), short).is_err());
        let mut right = vec![0u8; 512];
        page::write_common(&mut right, PageKind::FreeMap, 0, 0).unwrap();
        assert!(map.push_page(PageId(2), right).is_ok());
    }

    /// The free count matches what allocation and freeing did to it.
    #[test]
    fn the_free_count_tracks_allocation() {
        let (mut map, _) = map_over(512, 20);
        let before = map.free_count();
        map.allocate_run(3).unwrap();
        assert_eq!(map.free_count(), before - 3);
        map.free(PageId(3)).unwrap();
        assert_eq!(map.free_count(), before - 2);
    }
}
