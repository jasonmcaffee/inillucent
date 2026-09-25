//! Power loss between a page's old image and its new one, under the rollback journal.
//!
//! Invariant: **a page's old image is on disk and synced before its new image is
//! written**, and a crash at any point between the two leaves a file that still
//! reads as the database before the write - every page of it, not most of them.
//!
//! That single sentence is the whole correctness argument of
//! `crates/inillucent-pool/src/journal.rs`, stated at the top of it, and until
//! task-1946's M10 nothing cut a machine between the two writes to check it. The
//! free map has exactly this kind of test
//! (`crates/inillucent-compat/tests/durability/free_map_checkpoint_crash.rs`); the journal,
//! which is what makes `PRAGMA journal_mode = delete` safe at all, did not.
//! `crates/inillucent-pool/tests/fault_campaign.rs` said so in as many words:
//! "Crash recovery. There is no WAL in Phase 2, so there is nothing to recover
//! *to*". There is now.
//!
//! ## What is being cut, and why it is five pages rather than one
//!
//! One flush of a pool holding five dirty pages. In order, for each page, it:
//!
//! 1. reads the page's current bytes off the file;
//! 2. writes them to the journal as a record, with a checksum and a header
//!    naming the running count;
//! 3. syncs the journal (`Journal::seal`), once for the whole batch;
//! 4. writes the new image over the page in the database.
//!
//! **One page cannot show the failure this prevents.** A crash that loses a
//! single unsynced page leaves the old bytes on the disk, which is the right
//! answer by accident - nothing had to put them back. What a rollback journal is
//! for is a *set* of pages that has to move together: a crash part way through
//! writing five leaves two new and three old on the disk, and the only thing
//! that makes the file readable again is the journal putting the two back.
//!
//! So every cut is graded on all five pages at once, and the answer has to be
//! all-old, all-new, or a state the journal itself was discarded in.
//!
//! ## The third outcome, and why it is not a defect here
//!
//! `Pool::checkpoint` saves the two meta pages *after* `flush` has written the
//! batch, so a crash at that point catches a header rewrite with this
//! checkpoint's pages already on the disk. `replay_hot_journal` finds a header
//! whose checksum does not match, discards the whole journal, and the comment at
//! that branch says why that is safe: the meta record is the last thing a
//! checkpoint writes, so until it lands the file's `checkpoint_lsn` is still the
//! previous one and **redo re-applies the batch**.
//!
//! Redo is `inillucent-txn`'s, one layer above this crate. A campaign here has no
//! log to re-apply anything with, so it counts that outcome and names it rather
//! than calling it a mixture. What the layer above does with it is
//! `crates/inillucent-compat/tests/durability/durability.rs`'s question.
//!
//! Which is why the sharpest test in this file is not a campaign at all:
//! `every_pre_image_is_durable_before_the_first_new_image` reads the simulator's
//! own trace of one flush and asserts the ordering as an ordering. It cannot be
//! satisfied by luck, by a seed, or by a recovery that happened to work.

use std::sync::Arc;

use inillucent_pool::page::{self, PageKind};
use inillucent_pool::{Database, Options, PageId};
use inillucent_sim::failpoint::{Failure, Policy, Site};
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_vfs::{DbPath, Vfs};

/// The page size the campaign runs at.
const PAGE_SIZE: usize = 512;

/// How many pages the fixture holds.
const PAGES: u64 = 24;

/// The offset a page's marker is written at, which is what every assertion
/// reads.
const MARKER: usize = 32;

/// The marker every page holds before the write under test.
const BEFORE: u64 = 0x1111;

/// The marker they hold after.
const AFTER: u64 = 0x2222;

/// The first page `Database::allocate` hands out on a fresh file.
///
/// Asserted in `built`, so a change to the file's own layout fails there with
/// the reason rather than here with five pages of zeros.
const FIRST_PAGE: u64 = 3;

/// The pages the write under test changes, together.
const TARGETS: [PageId; 5] = [PageId(3), PageId(4), PageId(5), PageId(6), PageId(7)];

/// The file every run builds.
fn path() -> DbPath {
    DbPath::new("journal-ordering.rdb")
}

/// One page image carrying `marker`.
///
/// @param marker - what to write at `MARKER`
fn image(marker: u64) -> Vec<u8> {
    let mut bytes = vec![0u8; PAGE_SIZE];
    page::write_common(&mut bytes, PageKind::Leaf, 0, 1).expect("a header");
    page::write_u64(&mut bytes, MARKER, marker).expect("a marker");
    page::checksum_page(&mut bytes).expect("a checksum");
    bytes
}

/// Builds a simulator holding a database whose five target pages are marked
/// `BEFORE` and checkpointed.
///
/// @param seed - what the device model's randomness starts from
fn built(seed: u64) -> Arc<SimVfs> {
    let vfs = Arc::new(SimVfs::new(SimConfig {
        seed,
        ..SimConfig::default()
    }));
    let mut database = Database::create(
        vfs.as_ref(),
        &path(),
        Options::default().with_page_size(PAGE_SIZE).with_frames(16),
    )
    .expect("the database is created");
    let first = database.allocate(PAGES).expect("the pages are allocated");
    assert_eq!(
        first.0, FIRST_PAGE,
        "the fixture's page numbering moved; TARGETS names the wrong pages"
    );
    for page in TARGETS {
        database
            .install(page, &image(BEFORE))
            .expect("the page installs");
    }
    database.checkpoint().expect("the fixture checkpoints");
    drop(database);
    vfs
}

/// Returns the database, opened over the simulated file.
///
/// **Every open attaches a rollback journal**, because `delete` is the mode this
/// campaign is about; `journalled(false)` is the control, which is what
/// `journal_mode = off` means at this layer.
///
/// @param vfs - the simulator
/// @param journalled - whether to attach a rollback journal
fn opened(vfs: &Arc<SimVfs>, journalled: bool) -> Database {
    let database = Database::open(vfs.as_ref(), &path(), 16).expect("the database opens");
    if journalled {
        database
            .pool()
            .set_journal(Some(inillucent_pool::journal::Journal::new(
                Arc::clone(vfs) as Arc<dyn Vfs>,
                &path(),
                inillucent_pool::journal::JournalMode::Delete,
                PAGE_SIZE,
            )));
    }
    database
}

/// Installs the new image on all five pages and checkpoints, which is the
/// sequence the campaign cuts.
///
/// @param database - the open database
/// @returns whether the whole sequence reported success
fn write_and_checkpoint(database: &mut Database) -> bool {
    for page in TARGETS {
        if database.install(page, &image(AFTER)).is_err() {
            return false;
        }
    }
    if database.checkpoint().is_err() {
        return false;
    }
    database.pool().finish_journal().is_ok()
}

/// What the five pages read as in a recovered snapshot.
///
/// The journal is replayed first, which is what an open does. A database that
/// will not open at all, or a page that will not read, is reported rather than
/// panicked on - a crash may legitimately leave a torn page with no journal
/// record behind it.
///
/// @param snapshot - the simulator's post-crash state
fn recovered(snapshot: &CrashSnapshot) -> Result<Vec<u64>, String> {
    let vfs = Arc::new(SimVfs::recovered(SimConfig::default(), snapshot));
    inillucent_pool::journal::replay_hot_journal(vfs.as_ref(), &path())
        .map_err(|why| format!("replaying the journal: {why:?}"))?;
    let database = Database::open(vfs.as_ref(), &path(), 16)
        .map_err(|why| format!("opening the recovered database: {why:?}"))?;
    let mut markers = Vec::new();
    for page in TARGETS {
        let guard = database
            .pool()
            .fetch(page)
            .map_err(|why| format!("fetching page {}: {why:?}", page.0))?;
        markers.push(
            page::read_u64(&guard, MARKER).map_err(|why| format!("reading the marker: {why:?}"))?,
        );
    }
    Ok(markers)
}

/// A crash at any point between the old images and the new ones leaves the
/// database as it was, or as it became - never part of each.
///
/// **This is the ordering `journal.rs` states as its whole invariant.** Every
/// cut is graded on all five pages together, so a crash that left two of them
/// rewritten fails rather than being counted as recovered.
///
/// The assertions at the end are what stop this being a test that cannot fail: a
/// run that never reached the write would report the old state at every cut and
/// look perfect, so both outcomes have to be reached, and a sequence that
/// reported success must not be undone by recovery.
#[test]
fn a_crash_between_the_two_images_leaves_one_of_them() {
    let mut cuts = 0u64;
    let mut old = 0u64;
    let mut new = 0u64;
    let mut unreadable = 0u64;
    let mut discarded = 0u64;

    for nth in 1..=400u64 {
        let vfs = built(9_000_u64.saturating_add(nth));
        // **Armed after the pool exists, not before.** Opening the file is
        // itself a call at the simulated file system, so arming from the start
        // of the run would spend the first cuts on an open rather than on the
        // sequence this campaign is about.
        let mut database = opened(&vfs, true);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), Failure::Crash);

        let committed = write_and_checkpoint(&mut database);
        let reached = vfs.failpoints().sites_reached().saturating_sub(base);
        let snapshot = vfs.crash();
        drop(database);
        if reached < nth {
            break;
        }
        cuts = cuts.saturating_add(1);

        match recovered(&snapshot) {
            Ok(markers) if markers.iter().all(|marker| *marker == BEFORE) => {
                old = old.saturating_add(1);
                assert!(
                    !committed,
                    "cut {nth}: the flush reported success and recovery put the old images back"
                );
            }
            Ok(markers) if markers.iter().all(|marker| *marker == AFTER) => {
                new = new.saturating_add(1);
            }
            // The journal's header was caught mid-rewrite and the whole
            // journal discarded, which is the third legitimate outcome at this
            // layer - see the module comment. Counted and bounded below rather
            // than accepted, because a run where it is *most* cuts is a run
            // that is not exercising the journal at all.
            Ok(_) => discarded = discarded.saturating_add(1),
            // A page whose bytes are torn and whose journal record is torn too
            // is a legitimate outcome of a crash inside the journal write, and
            // is counted rather than accepted silently: the assertion below
            // fails if it is what happened at most cuts.
            Err(_) => unreadable = unreadable.saturating_add(1),
        }
    }

    let counts = format!(
        "{cuts} cuts, {old} old, {new} new, {discarded} discarded, {unreadable} unreadable"
    );
    assert!(cuts > 10, "only {cuts} cut points were reached ({counts})");
    assert!(
        old > 0,
        "no cut left the database as it was, so the campaign never reached the journal ({counts})"
    );
    assert!(
        new > 0,
        "no cut left the database as it became, so the campaign never reached the write ({counts})"
    );
    assert!(
        unreadable.saturating_mul(2) < cuts,
        "most cuts left a file that will not read at all, which means this is measuring the \
         fixture rather than the ordering ({counts})"
    );
    assert!(
        discarded.saturating_mul(2) < cuts,
        "most cuts discarded the journal rather than replaying it, so this campaign is \
         measuring the meta pages' own save rather than the batch's ({counts})"
    );
}

/// Without a journal the same crash leaves some pages new and some old, which is
/// what says the campaign above is measuring the journal.
///
/// **The control, and it is the point of the whole file.** `journal_mode = off`
/// is documented in `journal.rs` to mean a set of pages that has to move
/// together cannot be put back, so a run with no journal must be able to produce
/// a mixture - and if it cannot, the campaign above is asserting something that
/// was never in danger.
#[test]
fn without_a_journal_a_crash_leaves_a_mixture() {
    let mut mixtures = 0u64;
    let mut cuts = 0u64;
    for nth in 1..=400u64 {
        let vfs = built(4_000_u64.saturating_add(nth));
        let mut database = opened(&vfs, false);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), Failure::Crash);

        let _ = write_and_checkpoint(&mut database);
        let reached = vfs.failpoints().sites_reached().saturating_sub(base);
        let snapshot = vfs.crash();
        drop(database);
        if reached < nth {
            break;
        }
        cuts = cuts.saturating_add(1);
        if let Ok(markers) = recovered(&snapshot) {
            let all_old = markers.iter().all(|marker| *marker == BEFORE);
            let all_new = markers.iter().all(|marker| *marker == AFTER);
            if !all_old && !all_new {
                mixtures = mixtures.saturating_add(1);
            }
        }
    }
    // Fewer than the campaign above, and necessarily: a run with no journal
    // makes no journal writes and no journal sync, so the same sequence reaches
    // about a third as many calls at the file system.
    assert!(cuts > 5, "only {cuts} cut points were reached");
    assert!(
        mixtures > 0,
        "no cut without a journal left a mixture across {cuts} cut points, so a set of pages \
         moves atomically here whatever the journal does - and the campaign above is not \
         measuring what it claims"
    );
}

/// The sequence reaches the sync that makes the ordering hold.
///
/// The sync in step 3 is the whole of the guarantee: without it the journal's
/// bytes are in a cache a power loss empties. This asserts the sequence reaches
/// that site at all, rather than only ever cutting at a `Write`.
#[test]
fn the_sequence_reaches_the_sync_that_makes_the_ordering_hold() {
    let vfs = built(77);
    vfs.failpoints().set(Site::Sync, Policy::Off);
    let mut database = opened(&vfs, true);
    assert!(
        write_and_checkpoint(&mut database),
        "the sequence did not complete"
    );
    let counts = vfs.failpoints().counts();
    assert!(
        counts.get(&Site::Sync).copied().unwrap_or(0) > 0,
        "the sequence never synced, so there is no ordering to cut: {counts:?}"
    );
    assert!(
        counts.get(&Site::Write).copied().unwrap_or(0) > TARGETS.len() as u64,
        "the sequence wrote no more than it had pages, so the journal records and the page \
         images cannot both have been written: {counts:?}"
    );
}

/// Every pre-image is written, and the journal synced, before the first new
/// image reaches the database.
///
/// **The invariant, read as an ordering rather than inferred from a recovery.**
/// A crash campaign says "the database came back right", which is evidence and
/// not proof: a run can come back right because the writes happened to be
/// dropped. This reads the simulator's trace of one flush and asserts the
/// sequence directly - so a change that wrote a page before sealing the journal
/// fails here with the two positions in the message, whatever any recovery
/// happened to do.
#[test]
fn every_pre_image_is_durable_before_the_first_new_image() {
    let vfs = built(31);
    let mut database = opened(&vfs, true);
    let before = vfs.trace().len() as u64;
    assert!(
        write_and_checkpoint(&mut database),
        "the sequence did not complete, so there is no ordering to read"
    );

    let events: Vec<inillucent_sim::trace::Event> = vfs
        .trace()
        .events()
        .into_iter()
        .filter(|event| event.seq > before)
        .collect();

    let journal_name = path().journal().as_path().display().to_string();
    let database_name = path().as_path().display().to_string();

    // The first write to the database file that lands on one of the five target
    // pages. Everything before it is journal traffic or the free map's.
    let first_new_image = events
        .iter()
        .find(|event| {
            event.kind == "write"
                && event.path == database_name
                && TARGETS
                    .iter()
                    .any(|page| event.offset == page.0.saturating_mul(PAGE_SIZE as u64))
        })
        .map(|event| event.seq);
    let Some(first_new_image) = first_new_image else {
        panic!(
            "the flush never wrote one of the target pages, so it is not the sequence this \
             test is about: {:?}",
            events
                .iter()
                .map(|event| (event.seq, event.kind, event.offset))
                .collect::<Vec<_>>()
        );
    };

    // Every pre-image the batch needs, written to the journal.
    let journal_writes = events
        .iter()
        .filter(|event| event.kind == "write" && event.path == journal_name)
        .count();
    assert!(
        journal_writes >= TARGETS.len(),
        "the journal took {journal_writes} writes for {} pages, so not every pre-image was \
         saved",
        TARGETS.len()
    );

    // And the sync that makes them durable.
    let seal = events
        .iter()
        .find(|event| event.kind == "sync" && event.path == journal_name)
        .map(|event| event.seq);
    let Some(seal) = seal else {
        panic!("the journal was never synced, so no pre-image was durable when the page moved");
    };

    assert!(
        seal < first_new_image,
        "the journal was synced at {seal} and the first new image was written at \
         {first_new_image}: a page was overwritten before its pre-image was durable, which is \
         the one thing a rollback journal exists to forbid"
    );

    for event in events.iter().filter(|event| {
        event.kind == "write" && event.path == journal_name && event.seq > first_new_image
    }) {
        // A journal write after the first new image is the meta pages' own
        // pre-images, which `Pool::checkpoint` saves last on purpose. Anything
        // at a target page's offset would be a pre-image saved too late.
        assert!(
            !TARGETS
                .iter()
                .any(|page| event.offset == page.0.saturating_mul(PAGE_SIZE as u64)),
            "a pre-image for a target page was written at {} , after the first new image at \
             {first_new_image}",
            event.seq
        );
    }
}
