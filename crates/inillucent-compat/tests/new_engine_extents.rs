//! Large values through blob extents, graded against pinned SQLite 3.53.4.
//!
//! Invariant: **a value stored out of line reads back byte for byte, and every
//! path that touches it agrees with SQLite about what it holds.** The extent is
//! a storage decision and nothing above the leaf is supposed to be able to tell
//! - so the test is not "the extent works", it is "the answers are the same",
//! asked of a scan, a point read, an update, a delete and a reopen.
//!
//! ## Why the page size is stated
//!
//! A value goes out of line when it is longer than `page_size / 8`. At the
//! 4 KiB page size these tests use that is 512 bytes, so the `body` column's
//! two-kilobyte values are out of line and its short ones are not - which is the
//! case worth testing, because a leaf holding both is a leaf whose class array
//! carries two different classes for one column.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// The page size these tests build at, which fixes the spill threshold at 512.
const PAGE_SIZE: usize = 4_096;

/// Returns the pinned SQLite oracle binary, if it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns a fresh directory for one test's files.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-extents-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// A fixture both engines have opened.
struct Pair {
    engine: ImportedDatabase,
    oracle: Driver,
    /// Kept so the directory outlives the test.
    _directory: PathBuf,
}

/// Builds a `wide`-shaped fixture with the oracle and imports it.
///
/// @param tag - what to name the scratch directory after
/// @param rows - how many rows to seed
fn pair(tag: &str, rows: usize) -> Option<Pair> {
    let program = oracle_path()?;
    let directory = scratch(tag);
    let path = directory.join("wide.db");
    let mut oracle = Driver::start("sqlite", &program).expect("the oracle starts");
    oracle
        .send(&Op::Open(path.to_string_lossy().into_owned()))
        .expect("the oracle opens the fixture");
    let schema = [
        "CREATE TABLE wide (id INTEGER PRIMARY KEY, tag TEXT, body TEXT)",
        "CREATE INDEX wide_tag ON wide (tag)",
    ];
    for sql in schema {
        let observed = oracle
            .send(&Op::Exec(sql.to_string()))
            .expect("the exec runs");
        assert!(observed.ok, "the fixture did not build: {sql}");
    }
    for nth in 1..=rows {
        // Every fourth row is *short*, so one leaf's `body` column carries both
        // classes and the reader has to consult the class array rather than the
        // leaf's flag alone.
        let body = if nth % 4 == 0 {
            format!("short {nth}")
        } else {
            format!("{nth}:{}", "x".repeat(2_048))
        };
        let sql = format!(
            "INSERT INTO wide VALUES ({nth}, 'tag{}', '{body}')",
            nth % 7
        );
        let observed = oracle.send(&Op::Exec(sql)).expect("the insert runs");
        assert!(observed.ok, "the seed did not insert: {}", observed.message);
    }
    let engine = ImportedDatabase::import_with(path, PAGE_SIZE, 4_096)
        .unwrap_or_else(|error| panic!("the fixture did not import: {:?}", error.detail()));
    Some(Pair {
        engine,
        oracle,
        _directory: directory,
    })
}

impl Pair {
    /// Runs one statement on both engines and asserts they agreed about it.
    ///
    /// @param sql - the statement
    fn both(&mut self, sql: &str) {
        let theirs = self
            .oracle
            .send(&Op::Exec(sql.to_string()))
            .expect("the oracle runs the statement");
        let ours = self.engine.execute_any(sql, &Params::new());
        assert_eq!(
            theirs.ok,
            ours.is_ok(),
            "{sql}: sqlite said {} ({}), inillucent said {} ({:?})",
            theirs.ok,
            theirs.message,
            ours.is_ok(),
            ours.as_ref().err().and_then(|error| error.detail())
        );
    }

    /// Asserts both engines answer one query identically.
    ///
    /// @param sql - the query
    fn answers_agree(&mut self, sql: &str) {
        let theirs = self
            .oracle
            .send(&Op::Query(sql.to_string()))
            .expect("the oracle runs the query");
        assert!(theirs.ok, "{sql}: the oracle refused: {}", theirs.message);
        let ours = self
            .engine
            .execute_any(sql, &Params::new())
            .unwrap_or_else(|error| panic!("{sql}: the new engine refused: {:?}", error.detail()));
        let mine: Vec<Vec<TaggedValue>> = ours
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        OwnedDatum::Null => TaggedValue::Null,
                        OwnedDatum::Int(number) => TaggedValue::Integer(*number),
                        OwnedDatum::Real(number) => TaggedValue::Real(*number),
                        OwnedDatum::Text(bytes) => TaggedValue::Text(bytes.clone()),
                        OwnedDatum::Blob(bytes) => TaggedValue::Blob(bytes.clone()),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(mine, theirs.rows, "{sql} disagreed");
    }

    /// Walks every tree and fails on the first broken invariant.
    ///
    /// @param after - what was just run, for the message
    fn is_intact(&self, after: &str) {
        self.engine
            .check_trees()
            .unwrap_or_else(|error| panic!("the file is not intact after `{after}`: {error:?}"));
    }
}

/// The questions every campaign is graded by.
///
/// A whole-value read, a length, a point read, a scan in index order and an
/// aggregate - so a value that came back short, long, or from the wrong row
/// fails at least one of them.
const PROBES: &[&str] = &[
    "SELECT id, body FROM wide ORDER BY id",
    "SELECT id, length(body) FROM wide ORDER BY id",
    "SELECT body FROM wide WHERE id = 3",
    "SELECT body FROM wide WHERE id = 4",
    "SELECT count(*), sum(length(body)) FROM wide",
    "SELECT id, tag FROM wide ORDER BY tag, id",
    "SELECT id FROM wide WHERE tag = 'tag3' ORDER BY id",
    "SELECT substr(body, 1, 12) FROM wide ORDER BY id",
];

/// Says the suite could not run, rather than passing quietly.
fn no_oracle() {
    eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.ps1");
}

#[test]
fn a_value_larger_than_the_threshold_reads_back_whole() {
    let Some(mut pair) = pair("read", 40) else {
        return no_oracle();
    };
    pair.is_intact("the import");
    for probe in PROBES {
        pair.answers_agree(probe);
    }
}

#[test]
fn writing_over_a_large_value_keeps_every_other_row() {
    let Some(mut pair) = pair("write", 40) else {
        return no_oracle();
    };
    for sql in [
        // Longer than it was, so the run it was in cannot be reused.
        "UPDATE wide SET body = body || body WHERE id = 5",
        // Shorter, and now under the threshold - the value comes back inline.
        "UPDATE wide SET body = 'tiny' WHERE id = 6",
        // A short value becomes a large one.
        "UPDATE wide SET body = replace(hex(zeroblob(1500)), '0', 'y') WHERE id = 8",
        // A fresh row whose value is out of line from the start.
        "INSERT INTO wide VALUES (100, 'tag1', 'z')",
        "UPDATE wide SET body = replace(hex(zeroblob(2000)), '0', 'w') WHERE id = 100",
        // And one that goes away.
        "DELETE FROM wide WHERE id = 7",
    ] {
        pair.both(sql);
        pair.is_intact(sql);
    }
    for probe in PROBES {
        pair.answers_agree(probe);
    }
    pair.answers_agree("SELECT id, length(body) FROM wide WHERE id IN (5, 6, 8, 100)");
}

#[test]
fn a_campaign_of_large_writes_leaves_the_file_intact() {
    let Some(mut pair) = pair("campaign", 60) else {
        return no_oracle();
    };
    // Enough writes to force compactions, splits and at least one merge, with
    // out-of-line values moving through all three.
    for nth in 1..=30 {
        pair.both(&format!(
            "UPDATE wide SET body = body || 'appended {nth}' WHERE id = {}",
            nth * 2
        ));
    }
    for nth in 1..=15 {
        pair.both(&format!("DELETE FROM wide WHERE id = {}", nth * 4));
    }
    for nth in 1..=15 {
        pair.both(&format!(
            "INSERT INTO wide VALUES ({}, 'tag2', '{}:{}')",
            200 + nth,
            nth,
            "q".repeat(1_800)
        ));
    }
    pair.is_intact("the campaign");
    for probe in PROBES {
        pair.answers_agree(probe);
    }
}

#[test]
fn out_of_line_values_survive_closing_and_reopening_the_file() {
    let Some(mut pair) = pair("reopen", 40) else {
        return no_oracle();
    };
    pair.both("UPDATE wide SET body = body || ' tail' WHERE id = 9");
    pair.both("INSERT INTO wide VALUES (101, 'tag5', 'a')");
    let before = pair
        .engine
        .execute_any(
            "SELECT id, length(body) FROM wide ORDER BY id",
            &Params::new(),
        )
        .expect("the lengths read")
        .rows;

    pair.engine.reopen().expect("the file reopens");

    pair.is_intact("a reopen");
    let after = pair
        .engine
        .execute_any(
            "SELECT id, length(body) FROM wide ORDER BY id",
            &Params::new(),
        )
        .expect("the lengths read again")
        .rows;
    assert_eq!(before, after, "the lengths changed across a reopen");
    for probe in PROBES {
        pair.answers_agree(probe);
    }
}

#[test]
fn a_created_index_over_a_table_of_large_values_is_correct() {
    let Some(mut pair) = pair("index", 40) else {
        return no_oracle();
    };
    // The index's own key is short, so nothing in it goes out of line - but the
    // build reads the table, whose values do.
    pair.both("CREATE INDEX wide_body_head ON wide (tag, id)");
    pair.is_intact("a CREATE INDEX over out-of-line values");
    pair.answers_agree("SELECT id FROM wide WHERE tag = 'tag3' ORDER BY id");
    pair.answers_agree("SELECT tag, count(*) FROM wide GROUP BY tag ORDER BY tag");
    for probe in PROBES {
        pair.answers_agree(probe);
    }
}
