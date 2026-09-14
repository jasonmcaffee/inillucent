//! The pool's fault campaign: every I/O site, failed in turn.
//!
//! Invariant: an I/O failure is *reported*, never absorbed and never turned
//! into a wrong answer. A read that fails must not leave a frame holding a page
//! it does not hold; a write that fails must not clear the dirty bit; a short
//! read must be refused rather than believed.
//!
//! This is the crate the TDD names first for `inillucent-sim`: "`inillucent-sim`
//! becomes a dev-dependency of `inillucent-pool`, `inillucent-tree`, `inillucent-wal` and
//! `inillucent-txn` in Phase 2". It has to live inside the crate because what it
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

use inillucent_pool::interior::InteriorRef;
use inillucent_pool::page::{self, PageKind};
use inillucent_pool::{Database, Options, PageId, Pool};
use inillucent_sim::failpoint::{Failure, Policy, Site};
use inillucent_sim::{SimConfig, SimVfs};
use inillucent_vfs::{DbPath, OpenOptions, Vfs};

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

/// Rewriting an interior page does not leave a child pointing into it.
///
/// **The guard that existed checked the wrong half of the question.** A child
/// records where inside its parent its swip lives, so eviction can put a page id
/// back there, and `unswizzle_from_parent` checks that the parent's *frame* still
/// holds the parent's *page*. That catches a frame reused for a different page
/// and misses the same page rewritten with a different layout - which is exactly
/// what a B+tree split does to a parent, moving every slot after the insertion
/// point.
///
/// The failure has no error attached: eight bytes of page id land wherever the
/// new layout put the old offset. Found by a Phase 3 split campaign, where the
/// symptom was an interior page whose seventh key claimed to start at byte 23.
///
/// **The arms matter.** The pool's cooling clock is seeded deterministically, so
/// one arrangement gives one eviction order - and the first version of this test
/// used an arrangement where the *parent* was evicted first, which makes the
/// existing page check fire correctly and the bug invisible. Varying how many
/// pages are resident before the parent is installed varies which frame it lands
/// in and therefore the order, and some of those orders cool the child while the
/// parent is still in its frame. That is the order the bug needs.
#[test]
fn rewriting_an_interior_page_forgets_its_children() {
    use inillucent_pool::interior::InteriorBuilder;
    use inillucent_pool::Swip;

    let builder = InteriorBuilder::new(512, 1, 1).expect("a builder");
    let two = builder
        .build(
            &[b"kk"],
            &[Swip::unswizzled(PageId(10)), Swip::unswizzled(PageId(11))],
        )
        .expect("two children");
    // Three children rather than two, which is what a split leaves behind: the
    // new separator goes in the middle, so every slot after it moves.
    let three = builder
        .build(
            &[b"gg", b"kk"],
            &[
                Swip::unswizzled(PageId(10)),
                Swip::unswizzled(PageId(12)),
                Swip::unswizzled(PageId(11)),
            ],
        )
        .expect("three children");

    let mut evicted_arms = 0usize;
    for spacer in 0..8u64 {
        let (vfs, path) = populated(512, 60);
        let pool = pool_over(&vfs, &path, 512, 8, 60);
        for page in 40..(40 + spacer) {
            let _ = pool.fetch(PageId(page)).expect("a spacer read");
        }
        pool.install(PageId(5), &two).expect("the parent installs");

        // Swizzle child 11 into the parent, the way a descent does.
        let (parent_frame, at) = {
            let guard = pool.fetch(PageId(5)).expect("the parent");
            let interior = InteriorRef::parse(&guard).expect("an interior");
            (guard.frame(), interior.swip_offset(1).expect("an offset"))
        };
        let child_frame = {
            let guard = pool.fetch(PageId(11)).expect("the child");
            guard.frame()
        };
        pool.note_parent(child_frame, parent_frame, PageId(5), at);
        pool.swizzle_into(parent_frame, PageId(5), at, Swip::swizzled(child_frame))
            .expect("the swizzle lands");

        // The parent is rewritten with a layout that moves that slot.
        pool.install(PageId(5), &three)
            .expect("the rewrite installs");

        // Enough distinct pages to push the child out. Fetching is what drives
        // eviction: `claim_frame` cools and evicts and then *uses* the frame it
        // freed, so calling `evict_one` directly and dropping its answer leaks
        // the frame - which is what the first version of this test did, and the
        // pool ran out of frames with nothing resident.
        for page in 20..38u64 {
            let _ = pool.fetch(PageId(page)).expect("a clean read");
        }
        if !pool.is_resident(PageId(11)) {
            evicted_arms = evicted_arms.saturating_add(1);
        }

        let guard = pool.fetch(PageId(5)).expect("the parent still reads");
        let interior = InteriorRef::parse(&guard)
            .unwrap_or_else(|error| panic!("spacer {spacer}: the parent is damaged: {error:?}"));
        interior.validate().unwrap_or_else(|error| {
            panic!("spacer {spacer}: the parent's slot array is damaged: {error:?}")
        });
        assert_eq!(
            interior.count(),
            2,
            "spacer {spacer}: the rewrite's separators are gone"
        );
        assert_eq!(
            interior.key(0).expect("a key"),
            b"gg",
            "spacer {spacer}: an evicted child overwrote the parent's first key"
        );
        assert_eq!(
            interior.key(1).expect("a key"),
            b"kk",
            "spacer {spacer}: an evicted child overwrote the parent's second key"
        );
    }
    assert!(
        evicted_arms > 0,
        "no arm ever evicted the child, so none of them exercised the unswizzle"
    );
}

/// A pool that has seen more failed reads than it has frames still works.
///
/// **The leak this catches has the wrong error message, which is why it went
/// unnoticed.** `load` claimed a frame and then read into it, and every way the
/// read could fail - the buffer borrowed, the file short or unreadable, the
/// checksum wrong - returned the error while keeping the frame. A pool of eight
/// frames that saw eight failed reads had none left, and the ninth fetch
/// reported `every frame in the buffer pool is pinned; nothing can be evicted`,
/// which is a true statement about a state that should have been impossible and
/// says nothing about what caused it.
///
/// It survived the campaign above because that one fails a single read per run
/// against a four-frame pool, and one leaked frame out of four is invisible. It
/// was found by a Phase 3 recovery campaign probing for pages a truncated file
/// does not hold - which is what any reader does when it asks whether a page is
/// there, and what every fault campaign does by construction.
#[test]
fn a_pool_survives_more_failed_reads_than_it_has_frames() {
    let frames = 8usize;
    let (vfs, path) = populated(512, 40);
    let pool = pool_over(&vfs, &path, 512, frames, 40);

    // Every read fails, for four times as many fetches as there are frames.
    vfs.failpoints()
        .set(Site::Read, Policy::Always(Failure::IoError));
    for page in 2..(2 + frames as u64 * 4) {
        let outcome = pool.fetch(PageId(page));
        assert!(
            outcome.is_err(),
            "page {page} came back while every read was failing"
        );
        let detail = outcome
            .err()
            .and_then(|error| error.detail().map(str::to_string));
        assert!(
            !detail
                .clone()
                .unwrap_or_default()
                .contains("every frame in the buffer pool is pinned"),
            "the pool ran out of frames after failed reads: {detail:?}"
        );
    }
    vfs.failpoints().set(Site::Read, Policy::Off);

    // And the pool is exactly as usable as it was before.
    assert_eq!(pool.resident(), 0, "a failed read left a frame in use");
    for page in 2..40u64 {
        let guard = pool
            .fetch(PageId(page))
            .unwrap_or_else(|error| panic!("page {page} unreadable afterwards: {error:?}"));
        assert_eq!(page::read_u64(&guard, 32).expect("a marker"), page);
    }
}

/// A page whose checksum is wrong is refused and does not cost a frame.
///
/// The other half of the same leak: a read that *succeeds* and then fails
/// validation took the same path out.
#[test]
fn a_checksum_failure_does_not_cost_a_frame() {
    let frames = 4usize;
    let (vfs, path) = populated(512, 40);
    // Damage more pages than the pool has frames, so a leak exhausts it.
    let file = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the file opens");
    for page in 2..(2 + frames as u64 * 3) {
        file.write_all_at(page * 512 + 64, &[0xA5u8; 16])
            .expect("the damage lands");
    }
    drop(file);

    let pool = pool_over(&vfs, &path, 512, frames, 40);
    for page in 2..(2 + frames as u64 * 3) {
        let error = pool
            .fetch(PageId(page))
            .expect_err("a damaged page must be refused");
        assert!(
            !error
                .detail()
                .unwrap_or_default()
                .contains("every frame in the buffer pool is pinned"),
            "the pool ran out of frames after checksum failures"
        );
    }
    assert_eq!(pool.resident(), 0);
    // An undamaged page still reads.
    let guard = pool.fetch(PageId(38)).expect("an undamaged page");
    assert_eq!(page::read_u64(&guard, 32).expect("a marker"), 38);
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
    // Asking the file for its shared memory is what reaches `Shm`. The pool
    // never asks - the log's index is `inillucent-wal`'s, and this crate does
    // not depend on it - so the campaign asks the VFS directly rather than
    // claiming a site no caller here reaches.
    {
        let file = vfs.open(&path, OpenOptions::main_db()).expect("it opens");
        let _ = file.shared_memory();
    }
    // Deleting the file is what reaches `Delete`, and it is the last thing this
    // does because nothing can be read afterwards.
    let _ = vfs.delete(&path, true);
    let counts = vfs.failpoints().counts();
    let missed: Vec<Site> = Site::all()
        .into_iter()
        .filter(|site| counts.get(site).copied().unwrap_or(0) == 0)
        .collect();
    assert!(
        missed.is_empty(),
        "the campaign never reached {missed:?}: {counts:?}"
    );
}
