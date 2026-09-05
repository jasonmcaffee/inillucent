//! The failure-injection campaign: fail the Nth storage operation, for every
//! N, and prove nothing was left behind.
//!
//! Invariant: a statement that fails changes nothing. Not "changes little", not
//! "changes something the next statement will fix" - nothing. After the
//! statement is rolled back the tree holds exactly the entries it held before
//! it started, and every page in the file is owned by exactly one thing.
//!
//! The campaign enumerates rather than samples. It fails the first operation,
//! then the second, then the third, and so on until an execution completes with
//! no failure injected, which is the point at which every reachable site has
//! been tried. That is what the TDD asks for and it is the only way to be sure:
//! a randomly chosen injection point tests the paths that are easy to reach,
//! and the interesting ones - a failure between allocating a page and linking
//! it, a failure between rewriting two of the four pages a balance touches -
//! are the ones that are hard to reach.
//!
//! Three things are asserted after every injected failure:
//!
//! 1. the operation returned an error rather than panicking or succeeding;
//! 2. rolling the statement back restores the exact pre-statement tree;
//! 3. the raw integrity check passes, which is where a leaked page - allocated
//!    and then abandoned - or a doubly-owned one would show up.

use std::collections::BTreeMap;

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::page::PageSize;
use inillucent_sim::failpoint::{Failure, Policy, Site};
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_storage::alloc;
use inillucent_storage::check::{self, CheckOptions};
use inillucent_storage::cursor::BTreeCursor;
use inillucent_storage::header::VacuumMode;
use inillucent_storage::mutate;
use inillucent_storage::pager::{FailSite, NewDatabase, Pager, PagerOptions};
use inillucent_storage::vacuum;
use inillucent_value::record::encode_record;
use inillucent_value::{BlobValue, TextEncoding, Value};
use inillucent_vfs::memory::MemoryVfs;
use inillucent_vfs::DbPath;

/// How far a campaign will count before it gives up on reaching the end.
///
/// Every iteration rebuilds the database, so the campaign is quadratic in the
/// number of sites and the statements it runs are sized to keep that bounded.
/// Reaching the cap is a failure of the *test*, not of the engine, and it is
/// reported as one so it cannot quietly stop covering the tail of an execution.
const CAMPAIGN_LIMIT: u64 = 4_000;

/// Builds a record holding one blob of `len` bytes derived from the key.
fn row(key: i64, len: usize) -> Vec<u8> {
    let filler: Vec<u8> = (0..len)
        .map(|index| (key as u8).wrapping_add(index as u8))
        .collect();
    encode_record(
        &[Value::Blob(BlobValue::borrowed(&filler))],
        TextEncoding::Utf8,
        4,
    )
    .expect("a record")
}

/// Reads a table's contents as a map, which is what "the same tree" means here.
fn contents(pager: &mut Pager, root: PageId) -> BTreeMap<i64, Vec<u8>> {
    let limits = Limits::default();
    let mut cursor = BTreeCursor::table(root);
    let mut rows = BTreeMap::new();
    let mut more = cursor.first(pager).expect("a first");
    while more {
        rows.insert(
            cursor.rowid().expect("a rowid"),
            cursor.payload(pager, &limits).expect("a payload"),
        );
        more = cursor.next(pager).expect("a next");
    }
    rows
}

/// What a campaign runs against.
struct Fixture {
    page_size: u32,
    vacuum: VacuumMode,
    /// How many rows the database starts with.
    seeded: i64,
    /// The payload length of the seeded rows.
    seed_len: usize,
}

/// Builds the database a campaign iteration starts from, and returns the root
/// and the committed contents.
fn seed(vfs: &MemoryVfs, fixture: &Fixture) -> (Pager, PageId, BTreeMap<i64, Vec<u8>>) {
    let mut pager = Pager::create(
        vfs,
        &DbPath::new("failure.db"),
        PagerOptions::default(),
        NewDatabase {
            page_size: PageSize::new(fixture.page_size).expect("a page size"),
            reserved_bytes: 0,
            text_encoding: TextEncoding::Utf8,
            vacuum_mode: fixture.vacuum,
        },
    )
    .expect("a new database");
    pager.begin_write().expect("a write transaction");
    let root = mutate::create_table(&mut pager).expect("a table");
    for key in 1..=fixture.seeded {
        mutate::insert_row(&mut pager, root, key, &row(key, fixture.seed_len)).expect("a row");
    }
    pager.commit().expect("a commit");
    let before = contents(&mut pager, root);
    (pager, root, before)
}

/// Runs the statement a campaign injects into.
///
/// It is one closure so that the campaign's two halves - run it with the
/// failpoint armed, and run it with the failpoint disarmed to find out how many
/// sites there are - cannot drift apart.
type Statement = fn(&mut Pager, PageId) -> Result<(), inillucent_base::DbError>;

/// Runs one campaign: fail the Nth site of `statement`, for every N.
fn campaign(name: &str, fixture: Fixture, statement: Statement) {
    let mut injected = 0u64;
    let mut nth = 1u64;
    while nth <= CAMPAIGN_LIMIT {
        let vfs = MemoryVfs::new();
        let (mut pager, root, before) = seed(&vfs, &fixture);

        pager.begin_write().expect("a write transaction");
        pager.begin_statement().expect("a statement");
        pager.fail_after(nth, None);
        let outcome = statement(&mut pager, root);
        let reached = pager.sites_reached();
        pager.clear_failpoint();

        match outcome {
            Ok(()) => {
                // The execution finished without the failpoint firing, which
                // means it has fewer than `nth` sites: every one has been tried.
                assert!(
                    reached < nth,
                    "{name}: the statement succeeded after reaching {reached} sites with the {nth}th armed"
                );
                assert!(
                    injected > 0,
                    "{name}: no failure was ever injected, so the campaign proved nothing"
                );
                pager.rollback().expect("a rollback");
                return;
            }
            Err(error) => {
                injected = injected.saturating_add(1);
                assert!(
                    error.detail().is_some_and(|detail| detail.contains("injected")),
                    "{name}: site {nth} failed with an error the campaign did not inject: {error:?}"
                );
            }
        }

        // The statement is undone, and what is left must be what was there.
        pager
            .rollback_statement()
            .unwrap_or_else(|error| panic!("{name}: site {nth}: rolling back: {error:?}"));
        let after = contents(&mut pager, root);
        assert_eq!(
            after, before,
            "{name}: site {nth} left the tree different from the one the statement started with"
        );

        let report =
            check::check_database_with_options(&mut pager, &CheckOptions::roots(vec![root]))
                .unwrap_or_else(|error| panic!("{name}: site {nth}: checking: {error:?}"));
        assert!(
            report.is_ok(),
            "{name}: site {nth} left the file unsound: {:?}",
            report.as_pragma_output()
        );

        // The transaction has to be able to carry on, and to commit what it
        // had before the statement that failed.
        pager
            .commit()
            .unwrap_or_else(|error| panic!("{name}: site {nth}: committing after: {error:?}"));
        let committed = contents(&mut pager, root);
        assert_eq!(
            committed, before,
            "{name}: site {nth} committed something the failed statement had done"
        );

        nth = nth.saturating_add(1);
    }
    panic!("{name}: the campaign reached its {CAMPAIGN_LIMIT}-site limit without finishing");
}

/// Inserting rows: allocation, splits, overflow chains, and root height.
fn insert_many(pager: &mut Pager, root: PageId) -> Result<(), inillucent_base::DbError> {
    for key in 1_000..1_030i64 {
        mutate::insert_row(pager, root, key, &row(key, 700))?;
    }
    Ok(())
}

/// Deleting rows: merges, freed overflow chains, and a growing freelist.
fn delete_many(pager: &mut Pager, root: PageId) -> Result<(), inillucent_base::DbError> {
    for key in (1..=60i64).step_by(2) {
        mutate::delete_row(pager, root, key)?;
    }
    Ok(())
}

/// Replacing rows: a delete and an insert of a different size, in one step.
fn replace_many(pager: &mut Pager, root: PageId) -> Result<(), inillucent_base::DbError> {
    for key in 1..=30i64 {
        mutate::insert_row(pager, root, key, &row(key, 3_000))?;
    }
    Ok(())
}

/// Vacuuming: relocation, truncation, and the freelist being consumed.
fn vacuum_some(pager: &mut Pager, root: PageId) -> Result<(), inillucent_base::DbError> {
    for key in (1..=80i64).step_by(2) {
        mutate::delete_row(pager, root, key)?;
    }
    vacuum::incremental_vacuum(pager, 20)?;
    Ok(())
}

/// Dropping a whole tree: every page it owns goes back at once.
fn clear_tree(pager: &mut Pager, root: PageId) -> Result<(), inillucent_base::DbError> {
    mutate::clear_tree(pager, root)
}

/// Every injection point in an insert leaves the tree exactly as it was.
#[test]
fn a_failed_insert_leaves_the_tree_untouched() {
    campaign(
        "insert at 512",
        Fixture {
            page_size: 512,
            vacuum: VacuumMode::None,
            seeded: 40,
            seed_len: 60,
        },
        insert_many,
    );
}

/// The same at a page size where a row fits comfortably, so the failures land
/// in different places.
#[test]
fn a_failed_insert_at_a_larger_page_size_leaves_the_tree_untouched() {
    campaign(
        "insert at 4096",
        Fixture {
            page_size: 4096,
            vacuum: VacuumMode::None,
            seeded: 60,
            seed_len: 200,
        },
        insert_many,
    );
}

/// Every injection point in a delete leaves the tree exactly as it was.
#[test]
fn a_failed_delete_leaves_the_tree_untouched() {
    campaign(
        "delete at 512",
        Fixture {
            page_size: 512,
            vacuum: VacuumMode::None,
            seeded: 80,
            seed_len: 300,
        },
        delete_many,
    );
}

/// Every injection point in a replace - which frees an overflow chain and
/// allocates a longer one - leaves the tree exactly as it was.
#[test]
fn a_failed_replace_leaves_the_tree_untouched() {
    campaign(
        "replace at 1024",
        Fixture {
            page_size: 1024,
            vacuum: VacuumMode::None,
            seeded: 40,
            seed_len: 100,
        },
        replace_many,
    );
}

/// Every injection point in a vacuum - relocation and truncation included -
/// leaves the tree exactly as it was.
#[test]
fn a_failed_vacuum_leaves_the_tree_untouched() {
    campaign(
        "vacuum at 512",
        Fixture {
            page_size: 512,
            vacuum: VacuumMode::Incremental,
            seeded: 90,
            seed_len: 200,
        },
        vacuum_some,
    );
}

/// Every injection point in clearing a tree leaves it exactly as it was.
#[test]
fn a_failed_clear_leaves_the_tree_untouched() {
    campaign(
        "clear at 512",
        Fixture {
            page_size: 512,
            vacuum: VacuumMode::None,
            seeded: 50,
            seed_len: 400,
        },
        clear_tree,
    );
}

/// A failure inside a *transaction* rolls the whole transaction back to the
/// last commit, and the file on disk is untouched because nothing reached it.
#[test]
fn a_failed_transaction_rolls_all_the_way_back() {
    let vfs = MemoryVfs::new();
    let fixture = Fixture {
        page_size: 512,
        vacuum: VacuumMode::None,
        seeded: 60,
        seed_len: 200,
    };
    let (mut pager, root, before) = seed(&vfs, &fixture);
    let image = vfs.snapshot(&DbPath::new("failure.db")).expect("a file");

    for nth in [1u64, 5, 17, 40, 91] {
        pager.begin_write().expect("a write transaction");
        pager.fail_after(nth, None);
        let outcome = insert_many(&mut pager, root);
        pager.clear_failpoint();
        assert!(outcome.is_err(), "site {nth} did not fail");
        pager.rollback().expect("a rollback");

        assert_eq!(contents(&mut pager, root), before, "site {nth}");
        assert_eq!(
            vfs.snapshot(&DbPath::new("failure.db")).expect("a file"),
            image,
            "site {nth} changed the file even though nothing was committed"
        );
    }
}

/// A failure at each individual site kind is reachable, so the campaign is
/// covering all of them rather than stopping at the first.
///
/// Reaching a site is not automatic, and two of them took arranging. Freeing a
/// page needs a page that actually empties - the occupancy policy leaves a
/// half-full page alone, so deleting alternate rows frees nothing. Relocating
/// one needs a live page at the *end* of the file with free space below it,
/// which is why this builds a second table after the first and then clears the
/// first: without that, a vacuum finds the trailing pages already free and
/// truncates them without moving anything.
#[test]
fn every_site_kind_is_reached_by_some_operation() {
    for site in FailSite::all() {
        let vfs = MemoryVfs::new();
        let fixture = Fixture {
            page_size: 512,
            vacuum: VacuumMode::Incremental,
            seeded: 90,
            seed_len: 200,
        };
        let (mut pager, root, _) = seed(&vfs, &fixture);
        pager.begin_write().expect("a write transaction");

        if site == FailSite::Commit {
            mutate::insert_row(&mut pager, root, 5_000, &row(5_000, 100)).expect("a row");
            pager.fail_after(1, Some(site));
            let outcome = pager.commit();
            pager.clear_failpoint();
            assert!(outcome.is_err(), "{site:?} was never reached");
            pager.rollback().expect("a rollback");
            continue;
        }

        // A second table, built after the first, so its pages sit at the end of
        // the file where a vacuum has to move them.
        let trailing = mutate::create_table(&mut pager).expect("a second table");
        for key in 1..=40i64 {
            mutate::insert_row(&mut pager, trailing, key, &row(key, 300)).expect("a row");
        }
        pager.commit().expect("a commit");

        pager.begin_write().expect("a write transaction");
        pager.begin_statement().expect("a statement");
        pager.fail_after(1, Some(site));
        let outcome = match site {
            FailSite::PageEdit | FailSite::Allocate => insert_many(&mut pager, root),
            FailSite::Free => mutate::clear_tree(&mut pager, root),
            FailSite::Relocate | FailSite::Truncate => mutate::clear_tree(&mut pager, root)
                .and_then(|()| vacuum::incremental_vacuum(&mut pager, 500).map(|_| ())),
            FailSite::Commit => Ok(()),
        };
        pager.clear_failpoint();
        assert!(outcome.is_err(), "{site:?} was never reached");
        assert!(
            outcome
                .err()
                .and_then(|error| error.detail().map(|detail| detail.contains("injected")))
                .unwrap_or(false),
            "{site:?} failed for a reason the test did not inject"
        );
        pager.rollback_statement().expect("a statement rollback");
        pager.rollback().expect("a rollback");
    }
}

/// A page that is allocated and then abandoned by a failure is not left
/// unreachable: the check that would catch it is the one the campaign runs.
///
/// This test does the leak on purpose, to prove the detector detects. Without
/// it, "the campaign found no leaks" could mean the campaign cannot see one.
#[test]
fn the_leak_detector_reports_a_page_that_belongs_to_nothing() {
    let vfs = MemoryVfs::new();
    let fixture = Fixture {
        page_size: 512,
        vacuum: VacuumMode::None,
        seeded: 20,
        seed_len: 60,
    };
    let (mut pager, root, _) = seed(&vfs, &fixture);
    pager.begin_write().expect("a write transaction");
    let orphan = alloc::allocate_page(&mut pager).expect("a page");
    let report = check::check_database_with_options(&mut pager, &CheckOptions::roots(vec![root]))
        .expect("a check");
    assert!(
        !report.is_ok(),
        "a page owned by nothing was not reported: {:?}",
        report.as_pragma_output()
    );
    assert!(
        check::mentions_page(&report, orphan.get()),
        "the report does not name page {}: {:?}",
        orphan.get(),
        report.as_pragma_output()
    );
    pager.rollback().expect("a rollback");
}

/// A write that fails at the file system is reported, and is sticky.
///
/// This is the boundary phase 4 stops at, and the test says so rather than
/// claiming more. A commit that fails partway leaves the *file* in a state this
/// phase does not defend: pages have reached the disk and the header may not
/// have, and there is no journal yet to put them back. What is defended, and
/// what is asserted here, is that the failure is reported rather than swallowed,
/// that the pager refuses to keep working as though nothing happened, and that
/// the transaction's own in-memory state is still the one the caller can roll
/// back. Recovering the file is phase 7's job, and it is the reason phase 7
/// exists.
#[test]
fn a_write_that_fails_at_the_file_system_is_reported_and_sticks() {
    for failure in [Failure::IoError, Failure::DiskFull, Failure::ShortWrite] {
        for nth in [1u64, 2, 3, 5] {
            let vfs = SimVfs::new(SimConfig::default());
            let path = DbPath::new("sim.db");
            let mut pager = Pager::create(
                &vfs,
                &path,
                PagerOptions::default(),
                NewDatabase {
                    page_size: PageSize::new(512).expect("a page size"),
                    reserved_bytes: 0,
                    text_encoding: TextEncoding::Utf8,
                    vacuum_mode: VacuumMode::None,
                },
            )
            .expect("a new database");
            pager.begin_write().expect("a write transaction");
            let root = mutate::create_table(&mut pager).expect("a table");
            for key in 1..=40i64 {
                mutate::insert_row(&mut pager, root, key, &row(key, 300)).expect("a row");
            }

            // The failpoint counts every write the run has made, and
            // creating the database made some, so the campaign counts from
            // where the run already is rather than from one.
            let already = vfs
                .failpoints()
                .counts()
                .get(&Site::Write)
                .copied()
                .unwrap_or(0);
            vfs.failpoints().set(
                Site::Write,
                Policy::Nth(already.saturating_add(nth), failure),
            );
            let outcome = pager.commit();
            vfs.failpoints().set(Site::Write, Policy::Off);

            match failure {
                // A short write reports success and stores fewer bytes, so the
                // commit does not fail - which is exactly why it is the failure
                // a journal exists for, and why this phase can only record that
                // it happened rather than survive it.
                Failure::ShortWrite => {
                    assert!(
                        outcome.is_ok() || pager.sticky_error().is_some(),
                        "a short write neither completed nor was reported"
                    );
                }
                _ => {
                    assert!(
                        outcome.is_err(),
                        "{failure:?} at write {nth} was not reported by the commit"
                    );
                    assert!(
                        pager.sticky_error().is_some(),
                        "{failure:?} at write {nth} left the pager willing to carry on"
                    );
                    // Every later call returns the same error rather than a
                    // partial answer that looks complete.
                    let again = pager.get_page(PageId::from_persisted(1).expect("page 1"));
                    assert!(
                        again.is_err(),
                        "{failure:?} at write {nth}: the pager answered a read after failing"
                    );
                }
            }
        }
    }
}
