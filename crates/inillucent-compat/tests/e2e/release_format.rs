//! The current build reading databases that earlier releases wrote.
//!
//! Invariant: **a file written by any published release from 0.1.1 on still
//! opens, still verifies, still answers every question the same way, and still
//! takes a write.** A format change that breaks one of those is a change that
//! costs an application its database, and the only way to know is to have that
//! release write the file rather than to reason about the format.
//!
//! ## What the fixtures are, and what a release cannot choose
//!
//! `tools/build-interop-fixture.ps1 <version>` downloads that release, verifies
//! it against the published `SHA256SUMS` and its minisign signature, runs
//! `tests/interop/build.sql` with that release's own `inillucent.exe`, and
//! checks in what it produced. Six releases have a fixture: 0.1.1, 0.1.2,
//! 0.1.3, 0.1.5, 0.1.6 and 0.1.7. There is no 0.1.4 - it was never published.
//!
//! The design asked for a fixture at 4,096 bytes and one at 32,768. **A
//! released binary cannot choose a page size**: it is an argument to
//! `Database::open_at`, the command line has no flag for it, and
//! `PRAGMA page_size` reports the page size rather than setting one. So every
//! fixture is at the engine's own 32,768, and the smaller page is covered where
//! it can be - by `matrix.rs`'s `sqlite_page` arm, which drives the library
//! directly. `tests/interop/README.md` records this.
//!
//! ## The row that only the log holds
//!
//! Each fixture was written, checkpointed, and then written to once more. That
//! last row is in `app.rdb-wal.*` and nowhere else, so a build that opened the
//! database and ignored the log would answer 120 rows where the fixture says
//! 121. Replaying an old release's log is the part of the format most likely to
//! move and the part an ordinary read would never exercise.
//!
//! ## Quick and full
//!
//! The quick form reads the newest fixture, which is the case a change to the
//! storage layer breaks first, and runs in under a second.
//! `INILLUCENT_SCENARIO=full` reads every fixture. The backward direction -
//! an earlier release opening a file this build wrote - is
//! `release_format_history.rs`, in the `nightly` tier, because it needs a
//! downloaded release binary that a fresh clone does not have.

use std::path::{Path, PathBuf};

use inillucent_compat::interop;
use inillucent_compat::matrix::{default_arm, Scale};
use inillucent_compat::stories::{ask, open, reopen_and_check, run};
use inillucent_compat::workspace_root;

/// Returns an empty scratch directory to stage a fixture in.
///
/// @param name - what to call it
fn scratch(name: &str) -> PathBuf {
    let path = workspace_root()
        .join("target")
        .join("scratch")
        .join("release-format")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Reads one staged fixture and compares every answer to what it recorded.
///
/// Rule 1.1: each question answers with a value, and the value is compared to
/// the one the release that wrote the file gave for the same question. Rule
/// 1.4: the file is opened, checked, and read by a handle that did not write
/// any of it - which for a fixture is every handle there is.
///
/// @param version - the release that wrote it
/// @param database - the staged copy to read
fn read_everything(version: &str, database: &Path) {
    let arm = default_arm();
    let database_handle = open(&arm, database);
    let connection = database_handle.session();

    assert_eq!(
        ask(&connection, "PRAGMA integrity_check"),
        "ok",
        "{version}: the file written by that release does not verify under this build"
    );

    let questions = interop::questions();
    let recorded = interop::expected(version);
    assert_eq!(
        questions.len(),
        recorded.len(),
        "{version}: verify.sql asks {} questions and expected.tsv records {}, so the fixture \
         was built from a different list - rebuild it with \
         `pwsh tools/build-interop-fixture.ps1 -Version {version}`",
        questions.len(),
        recorded.len()
    );

    for (question, (name, wanted)) in questions.iter().zip(recorded.iter()) {
        assert_eq!(
            &question.name, name,
            "{version}: expected.tsv is in a different order from verify.sql"
        );
        let got = ask(&connection, &question.sql);
        assert_eq!(
            &got, wanted,
            "{version}: `{}` answers differently under this build.\n  {version} said: {wanted}\n  \
             this build says: {got}",
            question.name
        );
    }
}

/// The current build reads what an earlier release wrote.
///
/// The quick form reads the newest fixture; `INILLUCENT_SCENARIO=full` reads
/// every one of them. A fixture that cannot be opened at all fails in `open`
/// with the file named, which is the failure a format change produces.
#[test]
fn the_current_build_reads_what_an_earlier_release_wrote() {
    let versions = interop::versions();
    assert!(
        versions.len() >= 6,
        "tests/interop holds {} fixtures and six releases have been published from 0.1.1 on; \
         build the missing ones with `pwsh tools/build-interop-fixture.ps1 -Version <version>`",
        versions.len()
    );

    let chosen: Vec<String> = match Scale::from_env() {
        Scale::Quick => versions.last().cloned().into_iter().collect(),
        Scale::Full => versions.clone(),
    };
    assert!(!chosen.is_empty(), "no fixture was chosen to read");

    for version in chosen {
        let area = scratch(&version);
        let database = interop::stage(&version, &area);
        assert!(
            database.is_file(),
            "{version}: the fixture did not stage into {}",
            area.display()
        );
        read_everything(&version, &database);
    }
}

/// A database an earlier release wrote still takes a write today.
///
/// Reading an old file is half of interop and the easier half. This is the
/// other half: the current build appends to a tree an earlier release built,
/// closes, and a handle that wrote none of it reads both rows back - the old
/// one and the new one. Rule 1.4, and the reason the assertion names the old
/// row as well as the new one is that a write that rebuilt the table instead of
/// appending to it would satisfy an assertion about the new row alone.
#[test]
fn a_database_an_earlier_release_wrote_still_takes_a_write() {
    let Some(oldest) = interop::versions().first().cloned() else {
        panic!("tests/interop holds no fixtures");
    };
    let area = scratch("written-to");
    let database = interop::stage(&oldest, &area);
    let arm = default_arm();

    {
        let handle = open(&arm, &database);
        let connection = handle.session();
        run(
            &connection,
            "INSERT INTO note (id, title, body, weight, tag) VALUES (9100, 'written by this \
             build', 'appended to a tree that 0.1.1 built', 1.5, 'Ledger')",
        );
    }

    let handle = reopen_and_check(&arm, &database);
    let connection = handle.session();
    assert_eq!(
        ask(
            &connection,
            "SELECT title FROM note WHERE id IN (9001, 9100) ORDER BY id"
        ),
        "the row that only the log holds\nwritten by this build",
        "{oldest}: the row this build appended, or the row that release left in its log, is not \
         there after a reopen"
    );
}

/// A database an earlier release wrote recovers this build's writes after a
/// crash, from every release.
///
/// **Format 2 changed the leaf's delta area and the page checksum (task-2074),
/// and every release before it wrote format 1.** This build reads format 1, and
/// it keeps writing a format 1 leaf by format 1's rules until something repacks
/// it - a compaction, a split - and logs that repack with the page's image. The
/// reason is recovery: it replays the log onto the pages the file holds, which
/// for a file an earlier release wrote are format 1 pages, and a replay that
/// followed different rules from the write would land on different bytes.
///
/// So this writes enough rows into the fixture's table and its index to take a
/// format 1 delta area past its 32 rows, which makes the first compactions of
/// format 1 leaves happen inside the log; kills the process with all of it
/// unfolded; and reopens. The old rows, the new ones, the update and the
/// deletes all have to be there, and the file has to pass the integrity check.
#[test]
fn an_earlier_releases_file_recovers_this_builds_writes_after_a_crash() {
    let shell = inillucent_compat::cliproc::program("inillucent-shell");
    let mut sql = String::new();
    for id in 20_000..20_300 {
        sql.push_str(&format!(
            "INSERT INTO note (id, title, body, weight, tag) VALUES ({id}, 'crash {id}', \
             'written by this build into a file an earlier release wrote', {}, 'Tag{}');\n",
            id % 17,
            id % 13
        ));
    }
    sql.push_str("UPDATE note SET weight = weight + 100 WHERE id % 3 = 0 AND id >= 20000;\n");
    sql.push_str("DELETE FROM note WHERE id BETWEEN 20100 AND 20109;\n");
    let expected_weight: i64 = (20_000i64..20_300)
        .filter(|id| !(20_100..=20_109).contains(id))
        .map(|id| id % 17 + if id % 3 == 0 { 100 } else { 0 })
        .sum();
    let arm = default_arm();
    for version in interop::versions() {
        let area = scratch(&format!("crashed-{version}"));
        let database = interop::stage(&version, &area);
        let said = inillucent_compat::cliproc::write_and_crash(&shell, &database, &sql);
        assert!(
            said.contains("written"),
            "{version}: the statements did not run before the process was killed:\n{said}"
        );
        let handle = reopen_and_check(&arm, &database);
        let connection = handle.session();
        assert_eq!(
            ask(&connection, "SELECT count(*) FROM note WHERE id >= 20000"),
            "290",
            "{version}: the rows this build wrote before the crash did not all come back"
        );
        assert_eq!(
            ask(
                &connection,
                "SELECT sum(weight) FROM note WHERE id >= 20000"
            ),
            expected_weight.to_string(),
            "{version}: the update this build made before the crash did not come back"
        );
        assert_eq!(
            ask(&connection, "SELECT title FROM note WHERE id = 9001"),
            "the row that only the log holds",
            "{version}: the row that release left in its own log is gone"
        );
        // The index over `title` holds exactly what the table does: a range the
        // index answers against the same range read off the table.
        assert_eq!(
            ask(
                &connection,
                "SELECT count(*) FROM note WHERE title >= 'crash' AND title < 'crasi'"
            ),
            "290",
            "{version}: the index over `title` disagrees with the table after recovery"
        );
        assert_eq!(
            ask(
                &connection,
                "SELECT count(*) FROM note WHERE +title >= 'crash' AND +title < 'crasi'"
            ),
            "290",
            "{version}: the table's own rows disagree with the count after recovery"
        );
    }
}

/// Every release answered the questions identically.
///
/// `build.sql` is deterministic, so six releases writing it produced six files
/// that hold the same values - and the six `expected.tsv` files are therefore
/// byte for byte the same. The comparison is worth making because the files
/// themselves are **not** the same: the six `app.rdb` differ byte for byte,
/// which is what makes reading them a test rather than a copy.
#[test]
fn every_release_recorded_the_same_answers() {
    let versions = interop::versions();
    let Some(first) = versions.first() else {
        panic!("tests/interop holds no fixtures");
    };
    let reference = interop::expected(first);
    assert!(
        reference.len() >= 11,
        "{first}'s expected.tsv records {} answers and verify.sql asks 11",
        reference.len()
    );
    for version in versions.iter().skip(1) {
        assert_eq!(
            interop::expected(version),
            reference,
            "{version} answered verify.sql differently from {first}, which is a difference \
             between two released builds rather than a stale fixture"
        );
    }
}

/// The release script builds a fixture for the version it is shipping.
///
/// Rule 1.5, and the thing it holds is a claim this file's own documentation
/// makes: that `tests/interop/` never lags a release. Nothing in the test suite
/// can make that true - it is true because `packaging/ship.ps1` calls the
/// script in its publish phase - so what is checked here is that the call is
/// still there and still passes the version being shipped.
#[test]
fn the_release_script_builds_the_shipped_versions_fixture() {
    let ship = workspace_root().join("packaging/ship.ps1");
    let text = std::fs::read_to_string(&ship).unwrap_or_default();
    assert!(
        !text.is_empty(),
        "packaging/ship.ps1 could not be read at {}",
        ship.display()
    );
    assert!(
        text.contains("build-interop-fixture.ps1"),
        "packaging/ship.ps1 no longer calls tools/build-interop-fixture.ps1, so tests/interop \
         will lag the next release and this suite will grade a format nobody ships"
    );
    let calls = text
        .lines()
        .filter(|line| line.contains("build-interop-fixture.ps1"))
        .collect::<Vec<&str>>()
        .join("\n");
    assert!(
        calls.contains("$Version"),
        "packaging/ship.ps1 calls build-interop-fixture.ps1 without passing the version being \
         shipped:\n{calls}"
    );
}
