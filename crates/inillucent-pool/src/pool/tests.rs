//! The pool's own tests.
//!
//! Invariant: **every assertion here is about a pool a reader can construct
//! from the helper above it**, so a failure names a state rather than a
//! sequence somebody has to replay. `pool_over` builds a memory file of
//! known pages and each test drives it to a condition it then asserts:
//! a second fetch is a hit, a dirtied frame counts once, a written-back
//! frame counts not at all.
//!
//! Split out of `pool.rs` because the module was 2,004 lines and 418 of them
//! were these (task-2066 §4.3.2 added the last of them). A child module sees
//! everything the parent holds privately, so nothing had to be made more
//! visible to move them here.

use super::*;
use inillucent_vfs::{DbPath, MemoryVfs, OpenOptions, Vfs};

/// Returns a pool over a memory file of `pages` zeroed, checksummed pages.
fn pool_over(page_size: usize, frames: usize, pages: u64) -> Pool {
    let vfs = MemoryVfs::new();
    let path = DbPath::new("pool-test.rdb");
    let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
    for page in 0..pages {
        let mut image = vec![0u8; page_size];
        page::write_common(&mut image, PageKind::Leaf, 0, 1).unwrap();
        page::write_u64(&mut image, 32, page).unwrap();
        page::checksum_page(&mut image).unwrap();
        file.write_all_at(page * page_size as u64, &image).unwrap();
    }
    Pool::new(file, page_size, frames, pages).unwrap()
}

/// A fetch of an absent page reads it; a second fetch does not.
#[test]
fn a_second_fetch_is_a_hit() {
    let pool = pool_over(512, 8, 10);
    {
        let guard = pool.fetch(PageId(3)).unwrap();
        assert_eq!(page::read_u64(&guard, 32).unwrap(), 3);
    }
    assert_eq!(pool.stats().misses, 1);
    assert_eq!(pool.stats().reads, 1);
    {
        let _guard = pool.fetch(PageId(3)).unwrap();
    }
    assert_eq!(pool.stats().misses, 1, "the second fetch read nothing");
    assert_eq!(pool.stats().hits, 1);
}

/// The dirty count follows the frames through dirtying, writing back and
/// eviction.
///
/// **`dirty_pages` answers from a counter now and not from a walk**
/// (task-2066 §4.3.2), which is worth about 2,265 nanoseconds on every
/// statement that ends outside a transaction. A counter is only as good as
/// what says it agrees with the thing it replaced: the debug assertion in
/// `dirty_pages` grades it on every call the whole suite makes, and this
/// grades the numbers themselves against states a reader can check by
/// hand.
#[test]
fn the_dirty_count_follows_the_frames() {
    let pool = pool_over(512, 8, 10);
    assert_eq!(pool.dirty_pages(), 0, "a fresh pool holds nothing dirty");

    for page in 1..=3u64 {
        pool.modify(PageId(page), |_| Ok(())).unwrap();
    }
    assert_eq!(pool.dirty_pages(), 3, "three pages were modified");

    // Modifying one of them again does not count it twice.
    pool.modify(PageId(1), |_| Ok(())).unwrap();
    assert_eq!(pool.dirty_pages(), 3, "the same page dirtied twice is one");

    // A write-back makes a frame clean without freeing it.
    pool.flush().unwrap();
    assert_eq!(pool.dirty_pages(), 0, "every frame was written back");

    // A frame that is evicted while dirty stops counting. Eight frames and
    // ten pages, so reading every page evicts.
    for page in 1..=3u64 {
        pool.modify(PageId(page), |_| Ok(())).unwrap();
    }
    assert_eq!(pool.dirty_pages(), 3);
    for page in 0..10u64 {
        let _ = pool.fetch(PageId(page));
    }
    assert_eq!(
        pool.dirty_pages(),
        pool.state.borrow().dirty_by_walking(),
        "the counter and the frames disagree after eviction"
    );
}

/// Two guards on one page coexist, and the pin count returns to zero.
#[test]
fn two_guards_on_one_page_coexist() {
    let pool = pool_over(512, 8, 10);
    let first = pool.fetch(PageId(4)).unwrap();
    let second = pool.fetch(PageId(4)).unwrap();
    assert_eq!(first.frame(), second.frame());
    drop(first);
    drop(second);
    let frame = pool.state.borrow().table[&PageId(4)];
    assert_eq!(pool.pins_of(frame), 0);
}

/// A pool smaller than the working set evicts, and every page still reads
/// correctly afterwards. This is the eviction campaign in miniature; the
/// 64-frame version lives in `tests/`.
#[test]
fn a_small_pool_evicts_and_stays_correct() {
    let pool = pool_over(512, 4, 40);
    for round in 0..3 {
        for page in 2..40u64 {
            let guard = pool.fetch(PageId(page)).unwrap();
            assert_eq!(
                page::read_u64(&guard, 32).unwrap(),
                page,
                "round {round} page {page}"
            );
        }
    }
    assert!(pool.stats().evicted > 0, "nothing was evicted");
    assert!(pool.resident() <= 4);
}

/// A frame every caller has pinned cannot be evicted, and the pool says so
/// rather than corrupting one.
#[test]
fn a_fully_pinned_pool_refuses_to_evict() {
    let pool = pool_over(512, 2, 10);
    let _a = pool.fetch(PageId(2)).unwrap();
    let _b = pool.fetch(PageId(3)).unwrap();
    let error = pool.fetch(PageId(4)).unwrap_err();
    assert!(error.detail().unwrap_or("").contains("pinned"), "{error:?}");
}

/// A dirty page survives eviction: it is written back and read again.
#[test]
fn a_dirty_page_is_written_back_before_it_is_evicted() {
    let pool = pool_over(512, 2, 12);
    pool.modify(PageId(5), |bytes| page::write_u64(bytes, 40, 0xABCD))
        .unwrap();
    for page in 6..12u64 {
        let _ = pool.fetch(PageId(page)).unwrap();
    }
    assert!(pool.stats().writes > 0, "nothing was written back");
    let guard = pool.fetch(PageId(5)).unwrap();
    assert_eq!(page::read_u64(&guard, 40).unwrap(), 0xABCD);
}

/// Fills a memory file with `pages` zeroed, checksummed pages.
///
/// Split out of [`pool_over`] so a test that needs the VFS afterwards - to
/// put a rollback journal beside the database - can keep it.
///
/// @param vfs - where the file lives
/// @param path - the database's name
/// @param page_size - how big a page is
/// @param frames - how many frames the pool holds
/// @param pages - how many pages to write
fn pool_beside(
    vfs: &Arc<MemoryVfs>,
    path: &DbPath,
    page_size: usize,
    frames: usize,
    pages: u64,
) -> Pool {
    let file = vfs.open(path, OpenOptions::main_db()).unwrap();
    for page in 0..pages {
        let mut image = vec![0u8; page_size];
        page::write_common(&mut image, PageKind::Leaf, 0, 1).unwrap();
        page::write_u64(&mut image, 32, page).unwrap();
        page::checksum_page(&mut image).unwrap();
        file.write_all_at(page * page_size as u64, &image).unwrap();
    }
    Pool::new(file, page_size, frames, pages).unwrap()
}

/// Stamps a page with an LSN and a marker, and opens a transaction under it.
///
/// The stamp is what `holds_uncommitted` reads, so a page stamped above the
/// watermark is one no-steal holds back.
///
/// @param pool - the pool
/// @param page - the page to dirty
fn dirty_under_an_open_transaction(pool: &Pool, page: PageId) {
    pool.modify(page, |bytes| {
        page::write_u64(bytes, page::header::LSN, 900)?;
        page::write_u64(bytes, 40, 0xABCD)
    })
    .unwrap();
    pool.set_uncommitted_lsn(800);
}

/// A page an open transaction changed is written, not dropped, when its
/// frame is evicted.
///
/// No-steal lets a **checkpoint** leave such a page out of the file, because
/// the frame keeps it and the next checkpoint writes it. An eviction does
/// not keep it, so the same skip there discards the change - which is what
/// a `CREATE INDEX` through a 64-frame pool did to 129 pages, and what
/// `inillucent-compat`'s `new_engine_log_lead` reported as
/// `page 597 checksum 00000000 is not the computed 8d1053d3`: the checksum
/// of a page of zeros, on a page nothing had ever written.
///
/// The pre-image goes to the rollback journal before the new image goes to
/// the file, so a crash before the transaction commits can still put the
/// page back. See [`Writing`].
#[test]
fn an_uncommitted_page_is_written_rather_than_dropped_when_its_frame_goes() {
    let vfs = Arc::new(MemoryVfs::new());
    let path = DbPath::new("steal-test.rdb");
    let pool = pool_beside(&vfs, &path, 512, 2, 12);
    pool.set_journal(Some(crate::journal::Journal::new(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        &path,
        crate::journal::JournalMode::Delete,
        512,
    )));
    dirty_under_an_open_transaction(&pool, PageId(5));
    for page in 6..12u64 {
        let _ = pool.fetch(PageId(page)).unwrap();
    }
    assert!(
        !pool.is_resident(PageId(5)),
        "the frame was never evicted, so this proves nothing"
    );
    let guard = pool.fetch(PageId(5)).unwrap();
    assert_eq!(
        page::read_u64(&guard, 40).unwrap(),
        0xABCD,
        "the change was thrown away with the frame"
    );
}

/// With no journal to undo a steal with, the frame is kept instead.
///
/// `memory` and `off` hold no pre-images on disk, so an eviction there has
/// no way to put an uncommitted page back after a crash and must not write
/// it. What it must also not do is free the frame: the page is still only
/// in memory, and emptying the frame would lose it. So the page stays
/// resident and the evictor takes another frame.
#[test]
fn an_uncommitted_page_keeps_its_frame_when_nothing_can_undo_a_steal() {
    let pool = pool_over(512, 2, 12);
    dirty_under_an_open_transaction(&pool, PageId(5));
    for page in 6..12u64 {
        let _ = pool.fetch(PageId(page));
    }
    assert!(
        pool.is_resident(PageId(5)),
        "the page with nowhere to go was evicted anyway"
    );
    let guard = pool.fetch(PageId(5)).unwrap();
    assert_eq!(
        page::read_u64(&guard, 40).unwrap(),
        0xABCD,
        "the change was thrown away with the frame"
    );
}

/// A pool with nothing left to give says which rule is refusing.
///
/// The documented limit of a no-steal policy is that a transaction cannot
/// dirty more pages than the pool holds, and a caller who has hit it needs
/// to be told that rather than told its frames are pinned - they are not,
/// and no number of released guards would help.
#[test]
fn a_pool_full_of_uncommitted_pages_says_so_rather_than_blaming_pins() {
    let pool = pool_over(512, 2, 12);
    for page in [PageId(5), PageId(6)] {
        pool.modify(page, |bytes| {
            page::write_u64(bytes, page::header::LSN, 900)?;
            page::write_u64(bytes, 40, 0xABCD)
        })
        .unwrap();
    }
    pool.set_uncommitted_lsn(800);
    let refusal = pool
        .fetch(PageId(7))
        .expect_err("a full pool has to refuse");
    let detail = refusal.detail().unwrap_or_default();
    assert!(
        detail.contains("the open transaction has changed 2"),
        "the refusal did not name no-steal: {detail}"
    );
}

/// A page installed by the loader is dirty, readable, and flushed.
#[test]
fn an_installed_page_is_dirty_and_flushes() {
    let pool = pool_over(512, 8, 4);
    let mut image = vec![0u8; 512];
    page::write_common(&mut image, PageKind::Leaf, 0, 9).unwrap();
    page::write_u64(&mut image, 32, 777).unwrap();
    pool.install(PageId(6), &image).unwrap();
    assert_eq!(pool.page_count(), 7);
    assert_eq!(pool.flush().unwrap(), 1);
    // Reading it back through a fresh pool proves the checksum was written.
    let guard = pool.fetch(PageId(6)).unwrap();
    assert_eq!(page::read_u64(&guard, 32).unwrap(), 777);
    assert!(pool.install(PageId(7), &[0u8; 8]).is_err());
}

/// The cooling FIFO takes frames, and a fetch of a cooling page rewarms it
/// without any I/O.
#[test]
fn a_cooling_page_rewarms_without_a_read() {
    let pool = pool_over(512, 16, 20);
    for page in 2..10u64 {
        let _ = pool.fetch(PageId(page)).unwrap();
    }
    let reads_before = pool.stats().reads;
    let cooled = pool.cool().unwrap();
    assert!(cooled > 0, "nothing cooled");
    let cooling: Vec<u32> = pool.state.borrow().cooling.iter().copied().collect();
    let frame = cooling[0];
    let page = pool.state.borrow().frames[frame as usize].page;
    assert_eq!(pool.frame_state(frame), Some(FrameState::Cooling));
    let _guard = pool.fetch(page).unwrap();
    assert_eq!(pool.frame_state(frame), Some(FrameState::Hot));
    assert_eq!(pool.stats().reads, reads_before, "a rewarm read nothing");
    assert!(pool.stats().rewarms > 0);
}

/// A corrupt page is refused rather than returned.
#[test]
fn a_corrupt_page_is_refused() {
    let vfs = MemoryVfs::new();
    let path = DbPath::new("corrupt.rdb");
    let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
    let mut image = vec![0u8; 512];
    page::write_common(&mut image, PageKind::Leaf, 0, 1).unwrap();
    page::checksum_page(&mut image).unwrap();
    image[100] ^= 0xFF;
    file.write_all_at(2 * 512, &image).unwrap();
    let pool = Pool::new(file, 512, 4, 3).unwrap();
    let error = pool.fetch(PageId(2)).unwrap_err();
    assert!(
        error.detail().unwrap_or("").contains("checksum"),
        "{error:?}"
    );
}

/// An observation of a frame is invalidated when the frame is loaded over,
/// which is the whole reason a descent validates rather than trusting what
/// it read.
#[test]
fn loading_over_a_frame_invalidates_an_observation() {
    let pool = pool_over(512, 1, 8);
    let frame = {
        let guard = pool.fetch(PageId(2)).unwrap();
        guard.frame()
    };
    let observed = pool.observe(frame).expect("a free frame admits a reader");
    assert!(pool.validate(frame, observed));
    // One frame, so fetching a different page must reuse this one.
    let _ = pool.fetch(PageId(3)).unwrap();
    assert!(
        !pool.validate(frame, observed),
        "the frame holds a different page and said so"
    );
    assert!(pool.observe(99).is_none());
    assert!(pool.latch(99).is_none());
}

/// A pool with no frames or no page size is a misuse, not a panic.
#[test]
fn a_degenerate_pool_is_refused() {
    let vfs = MemoryVfs::new();
    let path = DbPath::new("degenerate.rdb");
    let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
    assert!(Pool::new(file, 512, 0, 0).is_err());
    let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
    assert!(Pool::new(file, 0, 4, 0).is_err());
}

/// The reported size is frames times the page size, and the counters reset.
#[test]
fn the_pool_reports_its_own_size() {
    let pool = pool_over(512, 6, 8);
    assert_eq!(pool.frames(), 6);
    assert_eq!(pool.page_size(), 512);
    assert_eq!(pool.byte_size(), 6 * 512);
    let _ = pool.fetch(PageId(2)).unwrap();
    assert!(pool.stats().reads > 0);
    pool.reset_stats();
    assert_eq!(pool.stats(), PoolStats::default());
    assert!(pool.is_resident(PageId(2)));
    assert!(!pool.is_resident(PageId(7)));
    assert_eq!(pool.frame_state(99), None);
}

/// Growing hands out the page after the last one and moves the count.
#[test]
fn growing_hands_out_the_next_page() {
    let pool = pool_over(512, 4, 5);
    assert_eq!(pool.grow(), PageId(5));
    assert_eq!(pool.page_count(), 6);
    pool.set_page_count(2);
    assert_eq!(pool.grow(), PageId(2));
}

/// A checkpoint writes the meta page and its shadow, and both decode.
#[test]
fn a_checkpoint_writes_both_meta_pages() {
    let pool = pool_over(512, 4, 6);
    let mut meta = Meta::fresh(512, 1234);
    meta.page_count = 6;
    meta.generation = 2;
    pool.checkpoint(&mut meta).unwrap();
    let mut primary = vec![0u8; 512];
    let mut shadow = vec![0u8; 512];
    pool.read_raw(META_PAGE, &mut primary).unwrap();
    pool.read_raw(SHADOW_PAGE, &mut shadow).unwrap();
    assert_eq!(Meta::choose(&primary, &shadow).unwrap(), meta);
}

/// The watermark reports low only when the free and cooling frames really
/// are below the fraction.
#[test]
fn the_watermark_reports_a_full_pool() {
    let pool = pool_over(512, 4, 8);
    assert!(!pool.under_watermark(), "a fresh pool is all free");
    for page in 2..6u64 {
        let _ = pool.fetch(PageId(page)).unwrap();
    }
    assert!(pool.under_watermark());
}
