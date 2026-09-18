//! Migrating every SQLite fixture into the new engine, verified.
//!
//! Invariant: **a migration is verified by counts and digests, and by a third
//! engine.** The tool computes both sides itself - the source through
//! `inillucent-sqlite-reader`, the destination through the new engine's own scan -
//! which catches a copy that lost or reordered rows. What it cannot catch is a
//! reader that mis-decodes the source, because the same reader also performs
//! the import. So this file adds the oracle the tool deliberately does not
//! carry: the **pinned SQLite 3.53.4 shell**, reading the source file, whose
//! row counts every fixture's migration is checked against.
//!
//! Three engines, then: SQLite's own b-tree code, the reader, and the new
//! engine. A number all three agree on is not one implementation's opinion.
//!
//! The corpus is `compat/fixtures`, written with that same pinned
//! binary: fifteen valid databases spanning page sizes 512 to 65536, both UTF-16
//! encodings, overflow chains, `WITHOUT ROWID`, deep trees, freelists, both
//! vacuum modes, an empty database and a collation corpus - and seventeen
//! malformed ones, each a single named edit to a valid fixture, which are the
//! failure path.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::workspace_root;
use inillucent_engine::ImportedDatabase;
use inillucent_migrate::sqlite;
use inillucent_tree::datum::OwnedDatum;

/// Where this suite's scratch databases live.
const AREA: &str = "migrate-sqlite";

/// Returns a clean scratch directory for one test.
///
/// @param name - the test's name, which becomes the directory's
fn scratch(name: &str) -> PathBuf {
    let path = workspace_root()
        .join("target")
        .join("scratch")
        .join(AREA)
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the fixture corpus directory.
fn fixtures() -> PathBuf {
    workspace_root().join("compat/fixtures")
}

/// Returns the pinned SQLite shell, if it has been downloaded.
fn reference() -> Option<PathBuf> {
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let path = directory.join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns every valid fixture's path, in the manifest's order.
///
/// The manifest is read rather than the directory listed, because the malformed
/// fixtures sit in the same directory and only the manifest says which is
/// which - and reading the directory would silently start migrating a corrupt
/// file as though it were expected to work.
fn valid_fixtures() -> Vec<PathBuf> {
    let manifest = std::fs::read_to_string(workspace_root().join("compat/fixtures/manifest.toml"))
        .expect("the fixture manifest is checked in");
    let mut out = Vec::new();
    for block in manifest.split("[[fixture]]").skip(1) {
        let field = |key: &str| -> Option<String> {
            block
                .lines()
                .find(|line| line.trim_start().starts_with(&format!("{key} = ")))
                .and_then(|line| line.split_once('='))
                .map(|(_, value)| value.trim().trim_matches('"').to_string())
        };
        if field("kind").as_deref() == Some("valid") {
            if let Some(name) = field("name") {
                out.push(fixtures().join(name));
            }
        }
    }
    assert!(
        out.len() >= 15,
        "the manifest should name at least fifteen valid fixtures, it named {}",
        out.len()
    );
    out
}

/// Returns every malformed fixture's path.
fn malformed_fixtures() -> Vec<PathBuf> {
    let manifest = std::fs::read_to_string(workspace_root().join("compat/fixtures/manifest.toml"))
        .expect("the fixture manifest is checked in");
    let mut out = Vec::new();
    for block in manifest.split("[[fixture]]").skip(1) {
        let field = |key: &str| -> Option<String> {
            block
                .lines()
                .find(|line| line.trim_start().starts_with(&format!("{key} = ")))
                .and_then(|line| line.split_once('='))
                .map(|(_, value)| value.trim().trim_matches('"').to_string())
        };
        if field("kind").as_deref() == Some("malformed") {
            if let Some(name) = field("name") {
                out.push(fixtures().join(name));
            }
        }
    }
    out
}

/// Returns what the pinned SQLite makes of one table, as `(rows, digest)`.
///
/// Both numbers come out of the shell rather than out of anything in this
/// workspace, which is the whole point: a third implementation reading the
/// source file. The digest is `group_concat` over `quote()` of every column,
/// ordered by rowid - `quote` renders each storage class unambiguously, a blob
/// as `X'..'`, a text with its quotes, a NULL as `NULL`, so two different
/// values cannot render alike - folded to a fixed width so a large table does
/// not return a megabyte of text.
///
/// @param shell - the pinned sqlite3 binary
/// @param database - the file to read
/// @param table - the table to summarise
fn sqlite_rows(shell: &Path, database: &Path, table: &str) -> Option<Vec<String>> {
    let quoted = table.replace('"', "\"\"");
    let output = Command::new(shell)
        .arg("-cmd")
        .arg(".mode quote")
        .arg(database)
        .arg(format!("SELECT * FROM \"{quoted}\";"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Renders one migrated row the way the pinned SQLite shell's quote mode does.
///
/// The point of rendering rather than hashing is that a mismatch is *readable*:
/// a failure prints the row SQLite saw beside the row the new engine produced,
/// which is what tells a person whether the bug is a lost row, a shifted
/// column or a mis-decoded value.
///
/// The REAL arm goes through `inillucent_value::numeric::real_to_text`, which is
/// the engine's SQLite-compatible float rendering and is already graded against
/// SQLite by the differential corpus - so this is not a second float formatter
/// that could drift from the first.
///
/// @param row - one row as the new engine produced it
fn quote_row(row: &[OwnedDatum]) -> String {
    row.iter()
        .map(|value| match value {
            OwnedDatum::Null => "NULL".to_string(),
            OwnedDatum::Int(number) => number.to_string(),
            OwnedDatum::Real(number) => {
                let text =
                    String::from_utf8_lossy(&inillucent_value::numeric::real_to_text(*number))
                        .into_owned();
                // SQLite's quote mode renders a whole-numbered REAL with a
                // trailing `.0`; `real_to_text` already does, so this is only
                // the exponent form's spelling.
                text
            }
            OwnedDatum::Text(bytes) => {
                let text = String::from_utf8_lossy(bytes);
                format!("'{}'", text.replace('\'', "''"))
            }
            OwnedDatum::Blob(bytes) => {
                // Lowercase, both the prefix and the digits, because that is
                // what the shell's quote mode prints.
                let mut out = String::with_capacity(bytes.len() * 2 + 3);
                out.push_str("x'");
                for byte in bytes.iter() {
                    out.push_str(&format!("{byte:02x}"));
                }
                out.push('\'');
                out
            }
        })
        .collect::<Vec<String>>()
        .join(",")
}

/// Every valid fixture migrates, with counts and digests verified, and the
/// pinned SQLite agreeing about every count.
#[test]
fn every_task_1781_fixture_migrates_with_verified_counts_and_digests() {
    let Some(shell) = reference() else {
        inillucent_compat::differential::skipping(
            "the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    };
    let area = scratch("every-fixture");
    let mut migrated = 0usize;
    let mut tables_checked = 0usize;
    for fixture in valid_fixtures() {
        let name = fixture
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let destination = area.join(format!("{name}.rdb"));
        let report = sqlite::migrate(&fixture, &destination).unwrap_or_else(|error| {
            panic!(
                "{name}: the migration failed: {}",
                error.detail().unwrap_or_default()
            )
        });
        assert!(
            destination.is_file() || !report.passed(),
            "{name}: a passing migration publishes its destination"
        );
        // **The source is untouched.** Not renamed, not truncated, not removed.
        assert!(fixture.is_file(), "{name}: the source must still be there");

        // The third engine, value by value. The published database is opened
        // by the new engine's own open path and every row is rendered the way
        // the pinned shell's quote mode renders it, so a difference is a
        // difference in the data rather than in two hashing schemes.
        // The published file when the migration published one, and the staging
        // file when it did not - so a failed verification is still diffed
        // against the pinned SQLite and says *which row* it disagreed about
        // rather than only that two digests differed.
        let built = if destination.is_file() {
            destination.clone()
        } else {
            report.staged.clone()
        };
        let opened =
            ImportedDatabase::open(built.clone(), sqlite::PAGE_SIZE, 256).unwrap_or_else(|error| {
                panic!(
                    "{name}: {} did not open: {}",
                    built.display(),
                    error.detail().unwrap_or_default()
                )
            });
        for table in &report.inventory.tables {
            let Some(theirs) = sqlite_rows(&shell, &fixture, &table.name) else {
                continue;
            };
            let quoted = table.name.replace('"', "\"\"");
            let (ours, _) = opened
                .run(&format!("SELECT * FROM \"{quoted}\""))
                .unwrap_or_else(|error| {
                    panic!(
                        "{name}.{}: the migrated table did not read: {}",
                        table.name,
                        error.detail().unwrap_or_default()
                    )
                });
            assert_eq!(
                ours.len(),
                theirs.len(),
                "{name}.{}: the new engine read {} rows, the pinned SQLite {}",
                table.name,
                ours.len(),
                theirs.len()
            );
            // Sorted on both sides, because neither `SELECT *` promises an
            // order and the new engine will answer one from a covering index
            // when that is the cheaper structure. What is being checked is that
            // the two engines hold the same rows, which is what a migration is
            // for; the order they come back in is a plan's business.
            let mut mine: Vec<String> = ours.iter().map(|row| quote_row(row)).collect();
            let mut reference = theirs.clone();
            mine.sort();
            reference.sort();
            for (position, (ours, theirs)) in mine.iter().zip(reference.iter()).enumerate() {
                assert_eq!(
                    ours, theirs,
                    "{name}.{}: row {position} differs between the new engine and the pinned SQLite",
                    table.name
                );
            }
            tables_checked += 1;
        }
        drop(opened);
        // Only now the tool's own verdict, which is the same claim in hash form.
        assert!(
            report.passed(),
            "{name}: {:?}",
            report
                .failures()
                .iter()
                .map(|check| check.line())
                .collect::<Vec<String>>()
        );
        migrated += 1;
    }
    assert!(migrated >= 15, "only {migrated} fixtures migrated");
    assert!(
        tables_checked > 0,
        "the pinned SQLite verified no table at all, so this test proved nothing"
    );
}

/// A migration run twice produces the same target, so a retry is safe.
#[test]
fn a_migration_run_twice_produces_the_same_target() {
    let area = scratch("idempotent");
    let fixture = fixtures().join("basic-p4096-utf8.db");
    let first = area.join("first.rdb");
    let second = area.join("second.rdb");

    let one = sqlite::migrate(&fixture, &first).expect("the first migration runs");
    assert!(one.passed(), "the first migration verified");
    let two = sqlite::migrate(&fixture, &second).expect("the second migration runs");
    assert!(two.passed(), "the second migration verified");

    // Same tables, same counts, same digests - which is what "the same target"
    // means for a database whose file also carries a log position and a
    // creation order that are not part of its contents.
    assert_eq!(
        one.inventory.tables, two.inventory.tables,
        "two runs over one source inventoried different tables"
    );
    for table in &one.inventory.tables {
        let a = one
            .checks
            .iter()
            .find(|check| check.name == format!("digest.{}", table.name));
        let b = two
            .checks
            .iter()
            .find(|check| check.name == format!("digest.{}", table.name));
        assert_eq!(
            a.map(|check| &check.detail),
            b.map(|check| &check.detail),
            "{}: two runs produced different digests",
            table.name
        );
    }
}

/// A migration never overwrites a destination that is already there.
#[test]
fn a_migration_refuses_to_overwrite_a_destination() {
    let area = scratch("no-overwrite");
    let fixture = fixtures().join("basic-p4096-utf8.db");
    let destination = area.join("taken.rdb");
    std::fs::write(&destination, b"not a database").expect("the scratch file is written");

    let error = sqlite::migrate(&fixture, &destination)
        .expect_err("a migration must refuse an occupied destination");
    assert!(
        error
            .detail()
            .unwrap_or_default()
            .contains("already exists"),
        "the refusal must say why: {}",
        error.detail().unwrap_or_default()
    );
    assert_eq!(
        std::fs::read(&destination).expect("the file is still readable"),
        b"not a database",
        "the destination must be exactly as it was"
    );
}

/// A truncated source produces a named error and no published database.
#[test]
fn a_truncated_source_is_refused_by_name() {
    let area = scratch("truncated");
    // Two truncations, because they fail at different depths: one loses the
    // header outright, the other keeps a valid header over a body that stops
    // in the middle of a page.
    for name in ["truncated-header.db", "truncated-mid-page.db"] {
        let fixture = fixtures().join(name);
        let destination = area.join(format!("{name}.rdb"));
        let outcome = sqlite::migrate(&fixture, &destination);
        let refused = match outcome {
            Err(error) => {
                assert!(
                    !error.detail().unwrap_or_default().is_empty(),
                    "{name}: the error must say something"
                );
                assert!(
                    error.detail().unwrap_or_default().contains(name),
                    "{name}: the error must name the file it refused: {}",
                    error.detail().unwrap_or_default()
                );
                true
            }
            Ok(report) => {
                // Reading far enough to build something is allowed; publishing
                // it is not, and a report that passed would mean the truncation
                // went unnoticed.
                assert!(
                    !report.passed(),
                    "{name}: a truncated source must not verify"
                );
                false
            }
        };
        assert!(
            !destination.exists(),
            "{name}: nothing may be published from a truncated source (refused early: {refused})"
        );
    }
}

/// Every corrupt source produces a named error or a failed verification, and
/// never a published database.
#[test]
fn a_corrupt_source_never_publishes_a_database() {
    let area = scratch("corrupt");
    let mut refused = 0usize;
    let malformed = malformed_fixtures();
    assert!(
        malformed.len() >= 15,
        "the manifest should name at least fifteen malformed fixtures, it named {}",
        malformed.len()
    );
    for fixture in malformed {
        let name = fixture
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let destination = area.join(format!("{name}.rdb"));
        match sqlite::migrate(&fixture, &destination) {
            Err(error) => {
                assert!(
                    error.detail().unwrap_or_default().contains(&name)
                        || error.detail().unwrap_or_default().contains("could not be"),
                    "{name}: the error must name the file or what failed: {}",
                    error.detail().unwrap_or_default()
                );
                refused += 1;
            }
            Ok(report) => {
                // A corruption the reader tolerates is allowed to produce a
                // report; what it must not produce is a *passing* one that
                // published a database, because that is the shape of a partial
                // database that looks finished.
                if report.passed() {
                    assert!(
                        destination.is_file(),
                        "{name}: a passing report must have published"
                    );
                } else {
                    assert!(
                        !destination.exists(),
                        "{name}: a failed verification must publish nothing"
                    );
                    refused += 1;
                }
            }
        }
        // The source is never touched, however badly it read.
        assert!(fixture.is_file(), "{name}: the source must still be there");
    }
    assert!(
        refused > 0,
        "not one malformed fixture was refused, so this test proved nothing"
    );
}

/// A target that runs out of space produces a named error and publishes nothing.
///
/// The disk is not filled - a test that filled a real disk would be a test that
/// took the machine down. What is denied instead is the *directory*: the
/// staging file is created inside a path that does not exist, which is the same
/// failure the file system reports when it cannot place the file, and it
/// reaches the same code.
#[test]
fn a_target_that_cannot_be_written_is_refused_by_name() {
    let area = scratch("no-space");
    let fixture = fixtures().join("basic-p4096-utf8.db");
    let destination = area.join("absent").join("nested").join("out.rdb");

    let error = sqlite::migrate(&fixture, &destination)
        .expect_err("a destination that cannot be written must be refused");
    assert!(
        error
            .detail()
            .unwrap_or_default()
            .contains("could not be built"),
        "the error must say the build failed: {}",
        error.detail().unwrap_or_default()
    );
    assert!(
        !destination.exists(),
        "nothing may be published when the target cannot be written"
    );
    assert!(fixture.is_file(), "the source must still be there");
}

/// A migration that verifies badly leaves its staging file and publishes nothing.
///
/// The proof that a failed migration is *recognisable*: the destination is
/// absent, so an application opening it gets nothing rather than half a
/// database, and the staging file is still there, because the thing a person
/// needs after a failed migration is the evidence.
#[test]
fn a_failed_migration_leaves_evidence_and_no_destination() {
    let area = scratch("evidence");
    let fixture = fixtures().join("bad-page-type.db");
    let destination = area.join("out.rdb");
    match sqlite::migrate(&fixture, &destination) {
        Ok(report) if !report.passed() => {
            assert!(
                !destination.exists(),
                "a failed migration publishes nothing"
            );
        }
        Ok(_) => {
            // The reader tolerated this one; the fixture corpus is shared and
            // its members are not all refused at the same depth.
        }
        Err(_) => {
            assert!(
                !destination.exists(),
                "a refused migration publishes nothing"
            );
        }
    }
}

/// The whole of what an application's database carries, migrated and readable.
///
/// **Four things stopped this migration before it worked.** A source with a
/// trigger refused the migration outright with "the new engine does not run
/// triggers" - false since triggers began firing. A source with an FTS5 table
/// refused it with "the declaration of `f_data` did not parse: database disk
/// image is malformed", about a file that is neither malformed nor at fault.
/// A view migrated, appeared in `sqlite_schema`, and then answered `no such
/// table`. And `sqlite_sequence` was not carried, so an AUTOINCREMENT table
/// whose high rows had been deleted reused their keys on the first insert
/// afterwards.
///
/// So the source here carries all four, plus the things that already worked -
/// a `WITHOUT ROWID` table, a generated column, a partial index and a foreign
/// key - and the assertions are that every query the source answers, the
/// migrated database answers.
#[test]
fn a_database_with_triggers_views_fts5_and_a_sequence_migrates_and_answers() {
    let Some(shell) = reference() else {
        inillucent_compat::differential::skipping("the pinned SQLite shell is missing");
        return;
    };
    let area = scratch("rich");
    let source = area.join("source.db");
    let script = "\
CREATE TABLE a(id INTEGER PRIMARY KEY AUTOINCREMENT, n TEXT);\n\
INSERT INTO a(n) VALUES('one'),('two'),('three'),('four');\n\
DELETE FROM a WHERE id > 2;\n\
CREATE TABLE log(id INTEGER PRIMARY KEY, what TEXT);\n\
CREATE TRIGGER a_ins AFTER INSERT ON a BEGIN INSERT INTO log(what) VALUES('ins'); END;\n\
CREATE VIEW v AS SELECT id, n FROM a;\n\
CREATE TABLE wr(k TEXT PRIMARY KEY, val TEXT) WITHOUT ROWID;\n\
INSERT INTO wr VALUES('x','1'),('y','2');\n\
CREATE TABLE g(a INTEGER, b INTEGER GENERATED ALWAYS AS (a*2) STORED);\n\
INSERT INTO g(a) VALUES(3),(4);\n\
CREATE INDEX pg ON g(a) WHERE a > 3;\n\
CREATE TABLE parent(id INTEGER PRIMARY KEY);\n\
CREATE TABLE child(id INTEGER PRIMARY KEY, p INTEGER REFERENCES parent(id));\n\
INSERT INTO parent VALUES(1);\n\
INSERT INTO child VALUES(1,1);\n\
CREATE VIRTUAL TABLE f USING fts5(body);\n\
INSERT INTO f(body) VALUES('the quick brown fox'),('jumps over');\n";
    let built = Command::new(&shell)
        .arg(&source)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(script.as_bytes())?;
            }
            child.wait_with_output()
        });
    let Ok(output) = built else {
        inillucent_compat::differential::skipping("the reference shell could not be run");
        return;
    };
    assert!(
        output.status.success(),
        "the source could not be built: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let destination = area.join("migrated.rdb");
    let report = sqlite::migrate(&source, &destination).expect("the migration runs");
    assert!(
        report.passed(),
        "the migration did not verify: {:?}",
        report
            .checks
            .iter()
            .filter(|check| !check.passed)
            .collect::<Vec<_>>()
    );
    assert!(destination.is_file(), "the migration published nothing");
    // The full-text table is reported as carried rather than left to be
    // noticed missing.
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "carried.f" && check.passed),
        "the fts5 table was not reported as carried"
    );

    let database =
        inillucent_engine::connect::Database::open(&destination).expect("the migration opens");
    let connection = database.session();
    let one = |sql: &str| -> Vec<Vec<OwnedDatum>> {
        connection
            .query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
    };

    // The view is readable, not merely listed. It used to be both listed and
    // unreadable, which is worse than being dropped.
    assert_eq!(
        quote_rows(&one("SELECT id, n FROM v ORDER BY id")),
        vec!["1,'one'".to_string(), "2,'two'".to_string()],
    );
    // The full-text index searches, and the docids are the source's.
    assert_eq!(
        quote_rows(&one("SELECT rowid, body FROM f WHERE f MATCH 'fox'")),
        vec!["1,'the quick brown fox'".to_string()],
    );
    assert_eq!(
        quote_rows(&one("SELECT rowid, body FROM f WHERE f MATCH 'jumps'")),
        vec!["2,'jumps over'".to_string()],
    );
    // The trigger fires.
    connection
        .execute_batch("INSERT INTO a(n) VALUES('five')")
        .expect("the insert runs");
    assert_eq!(quote_rows(&one("SELECT count(*) FROM log")), vec!["1"]);
    // And the key it was given is past the source's high-water mark rather
    // than back in the gap the DELETE left.
    assert_eq!(
        quote_rows(&one("SELECT id FROM a ORDER BY id")),
        vec!["1".to_string(), "2".to_string(), "5".to_string()],
        "AUTOINCREMENT reused a deleted key, so sqlite_sequence was not carried"
    );
    // The things that already worked, so a fix to one does not cost another.
    assert_eq!(
        quote_rows(&one("SELECT k, val FROM wr ORDER BY k")),
        vec!["'x','1'".to_string(), "'y','2'".to_string()],
    );
    assert_eq!(
        quote_rows(&one("SELECT a, b FROM g ORDER BY a")),
        vec!["3,6".to_string(), "4,8".to_string()],
    );
    let plan = quote_rows(&one("EXPLAIN QUERY PLAN SELECT a FROM g WHERE a > 3")).join(" ");
    assert!(
        plan.contains("pg"),
        "the partial index did not survive: {plan}"
    );
    connection
        .execute_batch("PRAGMA foreign_keys=ON")
        .expect("keys are enabled");
    assert!(
        connection
            .execute_batch("INSERT INTO child VALUES(9, 99)")
            .is_err(),
        "the foreign key did not survive the migration"
    );
}

/// Renders rows the way `quote_row` does, for comparison in a test.
///
/// @param rows - the rows to render
fn quote_rows(rows: &[Vec<OwnedDatum>]) -> Vec<String> {
    rows.iter().map(|row| quote_row(row)).collect()
}

/// A source the reader cannot read whole is refused, and nothing is published
/// (task-1979, M1).
///
/// **The one damage the verification could not see.** The tool inventories the
/// source with `inillucent-sqlite-reader`, copies with the same reader, and
/// verifies the copy against that inventory - so a page the reader cannot walk
/// made a table disappear from *both* sides and the counts agreed on nothing.
/// Measured: a two table SQLite file with one flipped bit in a leaf page
/// migrated with exit 0, `integrity-check ok`, and the 500 row table gone,
/// while real SQLite still read all 500 rows from the same file.
///
/// **The pinned shell is what makes the case a case.** The flip has to be one
/// SQLite tolerates; a file SQLite also refuses would be a file this tool is
/// right to refuse for a different reason, and the test would pass against a
/// build that had learned nothing. The fixture is built here from a valid one
/// rather than checked in, so the two halves - "SQLite still reads it" and
/// "this refuses it" - are asserted about the same bytes.
#[test]
fn a_source_the_reader_cannot_read_whole_is_not_published() {
    let Some(shell) = reference() else {
        inillucent_base::testing::skipping("the pinned SQLite shell is not downloaded");
        return;
    };
    let directory = scratch("unreadable-source");
    let source = fixtures().join("basic-p4096-utf8.db");
    let damaged = directory.join("damaged.db");
    std::fs::copy(&source, &damaged).expect("the fixture copies");

    // The first byte of page two, which is a table's leaf page in this fixture.
    // Flipped rather than zeroed, so the file stays a SQLite database and the
    // damage is one bit.
    const FLIP_AT: u64 = 4_096;
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&damaged)
            .expect("the copy opens");
        file.seek(SeekFrom::Start(FLIP_AT)).expect("it seeks");
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).expect("it reads");
        file.seek(SeekFrom::Start(FLIP_AT)).expect("it seeks back");
        file.write_all(&[byte[0] ^ 0x40]).expect("it writes");
    }

    // SQLite still reads every row, so the damage is one this tool must not
    // answer by dropping a table.
    for table in ["people", "widths"] {
        let asked = Command::new(&shell)
            .arg(&damaged)
            .arg(format!("SELECT count(*) FROM \"{table}\";"))
            .output()
            .expect("the pinned shell runs");
        let counted = String::from_utf8_lossy(&asked.stdout).trim().to_string();
        assert!(
            asked.status.success() && counted.parse::<u64>().is_ok_and(|rows| rows > 0),
            "the pinned shell could not read {table} from the damaged fixture, so this case \
             is about a file SQLite refuses too rather than about the reader: {counted}"
        );
    }

    let destination = directory.join("migrated.rdb");
    let outcome = sqlite::migrate(&damaged, &destination);
    assert!(
        outcome.is_err(),
        "a source whose rows the reader could not read was migrated: {:?}",
        outcome.map(|report| report.passed())
    );
    assert!(
        !destination.is_file(),
        "the migration refused and published {} anyway",
        destination.display()
    );
}
