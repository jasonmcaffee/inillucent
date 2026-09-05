//! The pool's fault campaign: every I/O site, failed in turn.
//!
//! Invariant: an I/O failure is *reported*, never absorbed and never turned
//! into a wrong answer. A read that fails must not leave a frame holding a page
//! it does not hold; a write that fails must not clear the dirty bit; a short
//! read must be refused rather than believed.
//!
//! This is the crate the TDD names first for `rustdb-sim`: "`rustdb-sim`
//! becomes a dev-dependency of `rustdb-pool`, `rustdb-tree`, `rustdb-wal` and
//! `rustdb-txn` in Phase 2". It has to live inside the crate because what it
//! asserts on is the pool's own frame table - whether a failed load left a
//! frame claiming a page, whether a failed writeback left the page clean - and
//! those are not public.
//!
//! ## What it does not cover, and why
//!
//! Crash recovery. There is no WAL in Phase 2, so there is nothing to recover
//! *to*: the TDD's lifecycle here is load, checkpoint, close. The crash and
//! torn-write campaigns arrive with the log in Phase 3, and the simulator's
//! `MediaModel` and `CrashSnapshot` are already wired to this VFS for them.

use rustdb_pool::page::{self, PageKind};
use rustdb_pool::{Database, Options, PageId, Pool};
use rustdb_sim::failpoint::{Failure, Policy, Site};
use rustdb_sim::{SimConfig, SimVfs};
use rustdb_vfs::{DbPath, OpenOptions, Vfs};

/// Builds a file of `pages` well-formed leaf pages and returns the VFS.
///
/// @param page_size - the page size to write at
/// @param pages - how many pages to write
fn populated(page_size: usize, pages: u64) -> (SimVfs, DbPath) {
    let vfs = SimVfs::new(SimConfig::default());
    let path = DbPath::new("faults.rdb");
    let file = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the simulated file opens");
    for page in 0..pages {
        let mut image = vec![0u8; page_size];
        page::write_common(&mut image, PageKind::Leaf, 0, 1).expect("a header");
        page::write_u64(&mut image, 32, page).expect("a marker");
        page::checksum_page(&mut image).expect("a checksum");
        file.write_all_at(page * page_size as u64, &image)
            .expect("the write lands");
    }
    drop(file);
    (vfs, path)
}

/// Returns a pool over the simulated file.
///
/// @param vfs - the simulator
/// @param path - the file
/// @param page_size - the page size
/// @param frames - how many frames
/// @param pages - how many pages the file holds
fn pool_over(vfs: &SimVfs, path: &DbPath, page_size: usize, frames: usize, pages: u64) -> Pool {
    let file = vfs
        .open(path, OpenOptions::main_db())
        .expect("the simulated file opens");
    Pool::new(file, page_size, frames, pages).expect("a pool")
}

/// A failed read is reported, and the frame it was going to fill is not left
/// claiming the page.
///
/// The failure that matters is the *second* fetch: if the first left a frame in
/// the page table pointing at a frame whose bytes are wrong, the second would
/// return them as if they were the page - a wrong answer with no error in
/// sight. So the campaign fetches again after every failure and checks the
/// bytes.
#[test]
fn a_failed_read_is_reported_and_leaves_nothing_behind() {
    for failure in [
        Failure::IoError,
        Failure::ShortRead,
        Failure::Interrupt,
        Failure::Permission,
    ] {
        let (vfs, path) = populated(512, 32);
        let pool = pool_over(&vfs, &path, 512, 8, 32);
        // Fail every read until it is disarmed. `fail_nth_call` counts from the
        // start of the *run*, and the run already opened a file and wrote
        // thirty-two pages, so an absolute index would have armed a call that
        // happened before the pool existed.
        vfs.failpoints().set(Site::Read, Policy::Always(failure));
        let outcome = pool.fetch(PageId(5));
        assert!(
            outcome.is_err(),
            "{failure:?} was absorbed rather than reported"
        );
        drop(outcome);
        assert!(
            !pool.is_resident(PageId(5)),
            "{failure:?} left page 5 in the page table"
        );
        vfs.failpoints().set(Site::Read, Policy::Off);
        // And the same page reads correctly once the failure is disarmed.
        let guard = pool
            .fetch(PageId(5))
            .unwrap_or_else(|error| panic!("{failure:?}: the retry failed too: {error:?}"));
        assert_eq!(page::read_u64(&guard, 32).expect("a marker"), 5);
    }
}

/// Failing every read in turn leaves the pool answering correctly afterwards.
///
/// The TDD's "fail-the-Nth-call" shape: count the sites a workload reaches,
/// then fail each in turn and check the engine still behaves. A pool's workload
/// is a scan, so every read of it is failed once.
#[test]
fn failing_each_read_in_turn_never_corrupts_the_pool() {
    // One clean pass to count the sites the workload reaches.
    let (vfs, path) = populated(512, 40);
    let pool = pool_over(&vfs, &path, 512, 4, 40);
    for page in 2..40u64 {
        let _ = pool.fetch(PageId(page)).expect("a clean read");
    }
    let sites = pool.stats().reads;
    assert!(sites > 0, "the clean pass read nothing");

    // Then each of them, failed in turn, each on its own simulator so the
    // per-site counter starts where the campaign thinks it does.
    for nth in 1..=sites {
        let (vfs, path) = populated(512, 40);
        let pool = pool_over(&vfs, &path, 512, 4, 40);
        vfs.failpoints()
            .set(Site::Read, Policy::Nth(nth, Failure::IoError));
        let mut failures = 0usize;
        for page in 2..40u64 {
            match pool.fetch(PageId(page)) {
                Ok(guard) => {
                    assert_eq!(
                        page::read_u64(&guard, 32).expect("a marker"),
                        page,
                        "read {nth}: page {page} came back as another page"
                    );
                }
                Err(_) => failures = failures.saturating_add(1),
            }
        }
        assert_eq!(failures, 1, "read {nth} should have failed exactly once");
        vfs.failpoints().set(Site::Read, Policy::Off);
        // Whatever happened, a fresh pass over the same file is clean.
        let after = pool_over(&vfs, &path, 512, 4, 40);
        for page in 2..40u64 {
            let guard = after
                .fetch(PageId(page))
                .unwrap_or_else(|error| panic!("read {nth}: recovery read failed: {error:?}"));
            assert_eq!(page::read_u64(&guard, 32).expect("a marker"), page);
        }
    }
}

/// A failed writeback is reported and the page stays dirty.
///
/// A pool that cleared the dirty bit on a failed write would lose the change
/// silently, which is the worst kind of durability bug: nothing complains at
/// the time. The retry is the check - it can only write the page again if the
/// failed attempt left the dirty bit alone.
#[test]
fn a_failed_writeback_keeps_the_page_dirty() {
    for failure in [Failure::IoError, Failure::DiskFull, Failure::Permission] {
        let (vfs, path) = populated(512, 16);
        let pool = pool_over(&vfs, &path, 512, 8, 16);
        pool.modify(PageId(4), |bytes| page::write_u64(bytes, 40, 0xDEAD_BEEF))
            .expect("the change lands in the frame");
        vfs.failpoints().set(Site::Write, Policy::Always(failure));
        assert!(
            pool.flush().is_err(),
            "{failure:?} was absorbed rather than reported"
        );
        vfs.failpoints().set(Site::Write, Policy::Off);
        let before = pool.stats().writes;
        assert!(pool.flush().is_ok(), "{failure:?}: the retry failed");
        assert!(
            pool.stats().writes > before,
            "{failure:?}: the retry wrote nothing, so the page was left clean"
        );
    }
}

/// A short write is invisible at the time and caught by the checksum.
///
/// This is the one failure the VFS reports as *success*, which is why the
/// simulator calls it "the nastiest": the pool cannot tell, clears the dirty
/// bit, and the damage is on the media. Nothing about the pool can catch it,
/// and the design does not pretend otherwise - the per-page crc32c is what
/// catches it, on the next read, which is what this asserts.
///
/// Stating it as its own test rather than folding it into the one above is the
/// point: a campaign that asserted "every write failure keeps the page dirty"
/// would be asserting something false and would have to be weakened to pass.
#[test]
fn a_short_write_is_caught_by_the_checksum_rather_than_by_the_pool() {
    let (vfs, path) = populated(512, 16);
    {
        let pool = pool_over(&vfs, &path, 512, 8, 16);
        // The change is in the *second* half of the page. The simulator's
        // short write keeps the first half, so a change in the first half
        // would land in full and the page on media would be correct - which is
        // a real outcome and not the one this test is about.
        pool.modify(PageId(4), |bytes| page::write_u64(bytes, 400, 0xDEAD_BEEF))
            .expect("the change lands in the frame");
        vfs.failpoints()
            .set(Site::Write, Policy::Always(Failure::ShortWrite));
        assert!(
            pool.flush().is_ok(),
            "a short write is reported as success, which is what makes it nasty"
        );
        vfs.failpoints().set(Site::Write, Policy::Off);
    }
    // A fresh pool reads the page off the media, where it is now half written.
    let pool = pool_over(&vfs, &path, 512, 8, 16);
    let error = pool
        .fetch(PageId(4))
        .expect_err("the half-written page must be refused");
    assert!(
        error.detail().unwrap_or("").contains("checksum"),
        "the refusal must say what was wrong: {error:?}"
    );
}

/// A page whose bytes were damaged on the way in is refused by its checksum.
///
/// The simulator writes what it is told; this damages the file directly, which
/// is the media fault a checksum exists for. What is checked is that the pool
/// *reports* it - a corrupt page must never reach a caller.
#[test]
fn a_damaged_page_is_refused_by_its_checksum() {
    let (vfs, path) = populated(512, 16);
    // Flip a byte of page 7 behind the pool's back.
    let file = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the file opens");
    let mut image = vec![0u8; 512];
    file.read_exact_at(7 * 512, &mut image).expect("a read");
    image[200] ^= 0xFF;
    file.write_all_at(7 * 512, &image).expect("a write");
    drop(file);

    let pool = pool_over(&vfs, &path, 512, 8, 16);
    let error = pool
        .fetch(PageId(7))
        .expect_err("a damaged page is refused");
    assert!(
        error.detail().unwrap_or("").contains("checksum"),
        "the refusal must say what was wrong: {error:?}"
    );
    // Every other page still reads.
    for page in [2u64, 6, 8, 15] {
        let guard = pool.fetch(PageId(page)).expect("an undamaged page");
        assert_eq!(page::read_u64(&guard, 32).expect("a marker"), page);
    }
}

/// A database created and checkpointed through the simulator reopens.
///
/// The Phase 2 lifecycle - load, checkpoint, close - over a VFS that is not the
/// operating system's, which is what makes the whole storage layer testable
/// deterministically when Phase 3 adds the log.
#[test]
fn a_database_round_trips_through_the_simulator() {
    let vfs = SimVfs::new(SimConfig::default());
    let path = DbPath::new("sim.rdb");
    let placed;
    {
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(32),
        )
        .expect("a fresh database");
        placed = database.allocate(3).expect("three pages");
        for offset in 0..3u64 {
            let mut image = vec![0u8; 512];
            page::write_common(&mut image, PageKind::Leaf, 0, 9).expect("a header");
            page::write_u64(&mut image, 32, offset).expect("a marker");
            database
                .install(PageId(placed.0 + offset), &image)
                .expect("an install");
        }
        database.set_catalog_root(placed);
        database.checkpoint().expect("a checkpoint");
    }
    let database = Database::open(&vfs, &path, 16).expect("it reopens");
    assert_eq!(database.catalog_root(), placed);
    for offset in 0..3u64 {
        let guard = database
            .pool()
            .fetch(PageId(placed.0 + offset))
            .expect("a page");
        assert_eq!(page::read_u64(&guard, 32).expect("a marker"), offset);
    }
}

/// Every failpoint site the pool can reach is reached, so the campaign is not
/// silently exercising three of nine.
#[test]
fn the_campaign_reaches_the_sites_it_claims_to() {
    let vfs = SimVfs::new(SimConfig::default());
    let path = DbPath::new("sites.rdb");
    {
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(8),
        )
        .expect("a fresh database");
        for _ in 0..40 {
            let page = database.allocate(1).expect("a page");
            let mut image = vec![0u8; 512];
            page::write_common(&mut image, PageKind::Leaf, 0, 1).expect("a header");
            database.install(page, &image).expect("an install");
        }
        database.checkpoint().expect("a checkpoint");
    }
    // Reopening and reading is what reaches `Read`: a database that was only
    // ever built installs pages and never fetches one.
    {
        let database = Database::open(&vfs, &path, 4).expect("it reopens");
        for page in 2..20u64 {
            let _ = database.pool().fetch(PageId(page));
        }
    }
    let counts = vfs.failpoints().counts();
    for site in [Site::Read, Site::Write, Site::Sync] {
        assert!(
            counts.get(&site).copied().unwrap_or(0) > 0,
            "the campaign never reached {site:?}: {counts:?}"
        );
    }
}
