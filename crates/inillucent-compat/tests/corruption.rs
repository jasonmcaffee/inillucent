//! Reading databases that have been damaged at random.
//!
//! Invariant: a damaged file is refused or read, never crashed on, and either
//! way the reader ends in the state it started in. That second half is the
//! part a "does it panic" fuzz misses: an error raised halfway down a B-tree
//! leaves a cursor holding pins on three pages and a pager holding a lock, and
//! a reader that leaks those on every corrupt row will wedge the cache and
//! block every writer on the machine long before it crashes. So every
//! iteration here asserts the pin count is back to zero and the lock is back
//! to `None`, whatever happened in between.
//!
//! The corpus is deliberately *derived*: each case takes a real SQLite file
//! and damages it, so the bytes around the damage are still a plausible
//! database and the reader has to notice a lie rather than reject noise.

use std::io::Write;

use inillucent_base::PrimaryCode;
use inillucent_compat::corpus;
use inillucent_compat::fixtures::valid_fixtures;
use inillucent_compat::workspace_root;
use inillucent_storage::check::{self, CheckLevel};
use inillucent_storage::pager::{Pager, PagerOptions};
use inillucent_storage::schema;
use inillucent_vfs::{DbPath, FileLock, OsVfs};

/// How many damaged files each fixture contributes.
const CASES_PER_FIXTURE: u32 = 400;

/// A small deterministic generator, so a failure can be reproduced from its
/// seed rather than from "it happened once on a Tuesday".
struct Rng(u64);

impl Rng {
    /// Returns the next value.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Returns a value below `bound`.
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// Returns the directory damaged files are written to.
fn scratch() -> std::path::PathBuf {
    let directory = workspace_root().join("_agent_output/corruption");
    let _ = std::fs::create_dir_all(&directory);
    directory
}

/// Damages a file image in one of the ways a real fault produces.
///
/// The four shapes are the ones that actually happen: a single flipped bit
/// from media rot, a byte set to an extreme value, a whole 16-byte run
/// replaced as a partial sector write would, and a truncation.
fn damage(image: &mut Vec<u8>, rng: &mut Rng) -> &'static str {
    if image.is_empty() {
        return "empty";
    }
    match rng.next() % 4 {
        0 => {
            let at = rng.below(image.len());
            let bit = (rng.next() % 8) as u8;
            if let Some(byte) = image.get_mut(at) {
                *byte ^= 1 << bit;
            }
            "flipped bit"
        }
        1 => {
            let at = rng.below(image.len());
            let value = (rng.next() % 256) as u8;
            if let Some(byte) = image.get_mut(at) {
                *byte = value;
            }
            "byte replaced"
        }
        2 => {
            let at = rng.below(image.len());
            let end = at.saturating_add(16).min(image.len());
            let filler = (rng.next() % 256) as u8;
            if let Some(window) = image.get_mut(at..end) {
                window.fill(filler);
            }
            "sector run overwritten"
        }
        _ => {
            let keep = rng.below(image.len());
            image.truncate(keep);
            "truncated"
        }
    }
}

/// Every damaged file must be read or refused, never crashed on, and must
/// leave the pager exactly as it found it.
#[test]
fn a_damaged_database_is_refused_without_leaking_a_pin_or_a_lock() {
    let vfs = OsVfs::new();
    let path = scratch().join("fuzz.db");
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut refused = 0u32;
    let mut read_cleanly = 0u32;
    let mut shapes: std::collections::BTreeMap<&'static str, u32> =
        std::collections::BTreeMap::new();

    for fixture in valid_fixtures() {
        let original =
            std::fs::read(corpus::fixture_path(fixture.name)).expect("the fixture reads");
        for case in 0..CASES_PER_FIXTURE {
            let mut image = original.clone();
            let edits = 1 + (rng.next() % 3) as usize;
            for _ in 0..edits {
                let shape = damage(&mut image, &mut rng);
                *shapes.entry(shape).or_default() += 1;
            }
            {
                let mut file = std::fs::File::create(&path).expect("the scratch file is writable");
                file.write_all(&image).expect("the image writes");
                file.sync_all().expect("the image reaches the disk");
            }

            let opened = Pager::open_read_only(&vfs, &DbPath::new(&path), PagerOptions::default());
            let Ok(mut pager) = opened else {
                refused = refused.saturating_add(1);
                continue;
            };
            let cache = std::sync::Arc::clone(pager.cache());
            if pager.begin_read().is_err() {
                refused = refused.saturating_add(1);
                continue;
            }

            let mut failed = false;
            match schema::load_schema(&mut pager) {
                Ok(_) => {}
                Err(error) => {
                    failed = true;
                    assert_expected(&error, fixture.name, case);
                }
            }
            if !failed {
                match corpus::scan_database(&mut pager) {
                    Ok(_) => {}
                    Err(error) => {
                        failed = true;
                        assert_expected(&error, fixture.name, case);
                    }
                }
            }
            pager.clear_sticky_error();
            let report = check::check_database(&mut pager, CheckLevel::Integrity);
            match &report {
                Ok(report) => {
                    if !report.is_ok() {
                        failed = true;
                    }
                }
                Err(error) => {
                    failed = true;
                    assert_expected(error, fixture.name, case);
                }
            }
            if failed {
                refused = refused.saturating_add(1);
            } else {
                read_cleanly = read_cleanly.saturating_add(1);
            }

            // Whatever happened, nothing may be left holding a page.
            assert_eq!(
                cache.pinned_frames(),
                0,
                "{} case {case}: a pin was leaked",
                fixture.name
            );
            pager.end_read().ok();
            assert_eq!(
                pager.lock_level(),
                FileLock::None,
                "{} case {case}: a lock was leaked",
                fixture.name
            );
            assert_eq!(
                cache.counters().resident,
                0,
                "{} case {case}: pages were left resident",
                fixture.name
            );
        }
    }

    let total = refused.saturating_add(read_cleanly);
    assert_eq!(
        total,
        CASES_PER_FIXTURE * valid_fixtures().len() as u32,
        "not every case was accounted for"
    );
    // A damaged byte often lands somewhere harmless - in a freelist page's
    // unused tail, or in a text payload - so a clean read is expected and is
    // not a failure. What would be a failure is *never* refusing anything.
    assert!(
        refused > total / 20,
        "only {refused} of {total} damaged files were refused, which suggests the \
         reader is not checking"
    );
    assert_eq!(shapes.len(), 4, "not every damage shape was exercised");
    println!(
        "{total} damaged databases: {refused} refused, {read_cleanly} read without complaint; \
         shapes {shapes:?}"
    );
    let _ = std::fs::remove_file(&path);
}

/// Asserts an error is one of the families a damaged file may produce.
///
/// **The judgement moved to `inillucent_compat::damage`** (task-2066 section
/// 4.4.6). Three suites were making it three ways: this one over the SQLite
/// format through the storage pager, `phase2_campaigns` over the native
/// format's pages, and `fault_shapes` over a whole database. Three copies of a
/// rule is how a fourth caller comes to have none.
///
/// @param error - what the reader said
/// @param fixture - which fixture was damaged
/// @param case - which damaged copy of it
fn assert_expected(error: &inillucent_base::DbError, fixture: &str, case: u32) {
    inillucent_compat::damage::assert_expected(error, fixture, case);
}

/// A cursor abandoned mid-traversal must still release everything it held.
///
/// Dropping a cursor is what happens when a query is interrupted, and the pins
/// it was holding have to go with it - otherwise an interrupted query is a
/// permanent leak rather than a cancelled one.
#[test]
fn abandoning_a_cursor_releases_its_pins() {
    use inillucent_storage::cursor::BTreeCursor;

    let (_vfs, mut pager) = corpus::open_fixture("deep-p512-utf8.db").unwrap();
    let cache = std::sync::Arc::clone(pager.cache());
    let object = schema::find_object(&mut pager, "many").unwrap().unwrap();
    let root = object.root_page.unwrap();

    {
        let mut cursor = BTreeCursor::table(root);
        assert!(cursor.first(&mut pager).unwrap());
        for _ in 0..500 {
            if !cursor.next(&mut pager).unwrap() {
                break;
            }
        }
        assert!(cursor.depth() >= 2, "the cursor is not holding a path");
        assert!(cache.pinned_frames() >= 2, "the cursor holds no pins");
        // Dropped here, mid-traversal, without reset.
    }
    assert_eq!(
        cache.pinned_frames(),
        0,
        "an abandoned cursor left pins behind"
    );

    // And an explicit reset does the same thing.
    let mut cursor = BTreeCursor::table(root);
    assert!(cursor.first(&mut pager).unwrap());
    assert!(cache.pinned_frames() >= 1);
    cursor.reset();
    assert_eq!(cache.pinned_frames(), 0);
    assert_eq!(cursor.depth(), 0);
}

/// A cursor that hits a corrupt page mid-scan must stop rather than skip it.
///
/// Continuing past an unreadable page is the failure that produces a partial
/// answer which looks complete, and no caller can tell the difference.
#[test]
fn a_corrupt_page_stops_a_scan_rather_than_being_skipped() {
    use inillucent_storage::cursor::BTreeCursor;

    let vfs = OsVfs::new();
    let path = scratch().join("mid-scan.db");
    let original = std::fs::read(corpus::fixture_path("deep-p512-utf8.db")).unwrap();

    // Break the type byte of a page well inside the table's leaves.
    let mut image = original.clone();
    let page_size = 512usize;
    let victim = 40usize;
    if let Some(byte) = image.get_mut((victim - 1) * page_size) {
        *byte = 0x03;
    }
    std::fs::write(&path, &image).unwrap();

    let mut pager = Pager::open_read_only(&vfs, &DbPath::new(&path), PagerOptions::default())
        .expect("the header is still intact");
    let cache = std::sync::Arc::clone(pager.cache());
    pager.begin_read().unwrap();
    let object = schema::find_object(&mut pager, "many").unwrap().unwrap();
    let root = object.root_page.unwrap();

    let mut cursor = BTreeCursor::table(root);
    let mut seen = 0u64;
    let mut stopped = false;
    let mut more = cursor.first(&mut pager).unwrap_or(false);
    while more {
        seen = seen.saturating_add(1);
        match cursor.next(&mut pager) {
            Ok(next) => more = next,
            Err(error) => {
                assert_expected(&error, "mid-scan", 0);
                stopped = true;
                break;
            }
        }
    }
    assert!(
        stopped,
        "the scan finished despite a corrupt page ({seen} rows)"
    );
    assert!(seen > 0, "the scan never started");
    assert!(seen < 2_000, "the scan somehow read every row");
    cursor.reset();
    assert_eq!(cache.pinned_frames(), 0, "the failed scan leaked a pin");
    let _ = std::fs::remove_file(&path);
}
