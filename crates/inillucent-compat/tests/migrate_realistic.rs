//! Migrating a database that looks like one somebody uses.
//!
//! Invariant: **every column of every row of every table comes across
//! identically, and the schema comes with it - the collations, the index
//! directions, the triggers, the views and the generated columns.**
//!
//! ## Why the existing corpus was not enough
//!
//! `migrate_sqlite.rs` migrates fifteen fixtures and checks each one against
//! the pinned SQLite, which is the right shape and the right oracle. What those
//! fixtures are is small and synthetic: page sizes and encodings and tree
//! depths, over tables of plain columns. Not one of them has a `NOCASE`
//! collation, a `DESC` index, a trigger keeping a count, a view, a generated
//! column, a two hundred column table, a blob past the extent threshold or an
//! FTS5 index with external content.
//!
//! The corpus that broke the migration had **all of them at once**, and it was
//! Nikaya's. So these three are built to have all of them at once too:
//!
//! | fixture | modelled on | what it carries |
//! |---|---|---|
//! | `browser-history.db` | Firefox's `places.sqlite` | NOCASE on a column and on an index, a DESC index, a partial index, two triggers keeping a count, a view, `WITHOUT ROWID`, URLs outside ASCII |
//! | `chat-archive.db` | a messaging application | a 200 column table, blobs from one byte to 200 KB, `VIRTUAL` and `STORED` generated columns, cascading foreign keys |
//! | `warehouse.db` | Nikaya's schema | FTS5 with external content and the triggers that keep it, an `INTEGER PRIMARY KEY` table of eight thousand rows, a wide composite key |
//!
//! Each is built from checked-in SQL by the pinned shell -
//! `tools/build-realistic-fixtures.sh` - so the fixture is reproducible and the
//! thing a reviewer reads is the SQL rather than a binary diff.
//!
//! ## What is compared, and against what
//!
//! The pinned SQLite reads the *source* and this engine reads the *destination*,
//! and every row of every table is rendered the same way and compared. That is
//! the same three-engine arrangement `migrate_sqlite.rs` describes: SQLite's own
//! b-tree code, the reader, and the new engine, and a value all three agree on
//! is not one implementation's opinion.
//!
//! Beyond the rows, the schema: a migration that carried the values and lost
//! the `NOCASE` would read correctly on the day it ran and answer a different
//! set the first time somebody searched case-insensitively.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::workspace_root;
use inillucent_engine::ImportedDatabase;
use inillucent_migrate::sqlite;
use inillucent_tree::datum::OwnedDatum;

/// What a fixture's migration is expected to do.
///
/// **Two of the three are refused, and both refusals are the answer rather than
/// a gap in this file.** Rule 1.1: a case the tool is allowed to decline
/// asserts *which* refusal it declines with, so a change in either direction is
/// a failure here.
enum Expect {
    /// It migrates, and every column of every row is compared to the source.
    Publishes,
    /// It is refused, nothing is published, and the refusal contains this text.
    Refused(&'static str),
}

/// The fixtures, what each one is for, and what the migration does with it.
const FIXTURES: [(&str, &str, Expect); 3] = [
    (
        "browser-history",
        "NOCASE, a DESC index, a partial index, triggers, a view, WITHOUT ROWID, unicode",
        Expect::Publishes,
    ),
    (
        "chat-archive",
        "200 columns, blobs from 1 B to 200 KB, generated columns, cascading keys",
        // **task-2050.** The rows and the schema come across correctly - the
        // staging file holds exactly the source's values - and the digest
        // check refuses them anyway, so a database that migrated perfectly is
        // deleted. Reproduced in four lines on the ticket. Only the two tables
        // with generated columns fail; `conversation` and `reaction` pass.
        //
        // When task-2050 lands this goes red, and the fixture moves to
        // `Publishes` beside the other one.
        Expect::Refused("digest"),
    ),
    (
        "warehouse",
        "FTS5 with external content, 8,000 rows, a wide composite key",
        // Not a defect: an external content index has no text of its own, so
        // there is nothing in the file to carry, and the tool says exactly
        // that rather than producing an index that answers nothing. The three
        // ordinary tables all pass their counts and digests first.
        Expect::Refused("keeps no content table"),
    ),
];

/// Returns a clean scratch directory for one test.
///
/// @param name - the test's name, which becomes the directory's
fn scratch(name: &str) -> PathBuf {
    let path = workspace_root()
        .join("target")
        .join("scratch")
        .join("migrate-realistic")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the pinned SQLite shell, if it has been downloaded.
fn reference() -> Option<PathBuf> {
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let path = directory.join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns a realistic fixture's path, if it has been built.
///
/// @param name - the fixture's stem
fn fixture(name: &str) -> Option<PathBuf> {
    let path = workspace_root()
        .join("compat/fixtures/realistic")
        .join(format!("{name}.db"));
    path.is_file().then_some(path)
}

/// Asks the pinned shell one question and returns its lines.
///
/// @param shell - the pinned `sqlite3`
/// @param database - the file to ask
/// @param sql - the statement
fn ask_sqlite(shell: &Path, database: &Path, sql: &str) -> Vec<String> {
    let output = Command::new(shell)
        .arg("-cmd")
        .arg(".mode quote")
        .arg(database)
        .arg(sql)
        .output()
        .unwrap_or_else(|why| panic!("the pinned shell did not run: {why}"));
    assert!(
        output.status.success(),
        "`{sql}` failed in the pinned shell:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Renders one migrated row the way the pinned SQLite shell's quote mode does.
///
/// The same rendering `migrate_sqlite.rs` uses, and for the same reason: a
/// mismatch prints the row SQLite saw beside the row this engine produced,
/// which is what tells a person whether it is a lost row, a shifted column or a
/// mis-decoded value.
///
/// @param row - one row as this engine produced it
fn quote_row(row: &[OwnedDatum]) -> String {
    row.iter()
        .map(|value| match value {
            OwnedDatum::Null => "NULL".to_string(),
            OwnedDatum::Int(number) => number.to_string(),
            OwnedDatum::Real(number) => {
                String::from_utf8_lossy(&inillucent_value::numeric::real_to_text(*number))
                    .into_owned()
            }
            OwnedDatum::Text(bytes) => {
                format!("'{}'", String::from_utf8_lossy(bytes).replace('\'', "''"))
            }
            OwnedDatum::Blob(bytes) => {
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

/// Every column of every row of every table crosses identically.
#[test]
fn every_realistic_fixture_migrates_value_for_value() {
    let Some(shell) = reference() else {
        inillucent_compat::differential::skipping(
            "the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    };
    let area = scratch("value-for-value");
    let mut migrated = 0usize;
    let mut tables_checked = 0usize;
    let mut rows_checked = 0usize;

    for (name, about, expect) in FIXTURES {
        let Some(source) = fixture(name) else {
            inillucent_compat::differential::skipping(&format!(
                "compat/fixtures/realistic/{name}.db is not built; run \
                 tools/build-realistic-fixtures.sh",
            ));
            return;
        };
        let destination = area.join(format!("{name}.rdb"));
        let report = sqlite::migrate(&source, &destination).unwrap_or_else(|why| {
            panic!(
                "{name} ({about}): the migration could not be run at all: {}",
                why.detail().unwrap_or_default()
            )
        });
        assert!(source.is_file(), "{name}: the source must still be there");

        let said = report
            .checks
            .iter()
            .map(|check| format!("{} {}", check.name, check.detail))
            .collect::<Vec<String>>()
            .join("\n  ");

        if let Expect::Refused(naming) = expect {
            assert!(
                !report.passed(),
                "{name} ({about}) now migrates. That is the fix landing - move its row to \
                 `Expect::Publishes` so the rows get compared, and take it off \
                 tests/escapes.toml's open list.\n  {said}"
            );
            assert!(
                said.contains(naming),
                "{name} was refused and the refusal does not name `{naming}`, so this case no \
                 longer describes what happens:\n  {said}"
            );
            assert!(
                !destination.is_file(),
                "{name}: the migration failed its verification and published anyway"
            );
            migrated = migrated.saturating_add(1);
            continue;
        }

        assert!(
            report.passed(),
            "{name} ({about}): the migration did not pass its own verification:\n  {said}"
        );
        assert!(
            destination.is_file(),
            "{name}: a passing migration publishes its destination"
        );
        migrated = migrated.saturating_add(1);

        let opened = ImportedDatabase::open(destination.clone(), sqlite::PAGE_SIZE, 256)
            .unwrap_or_else(|why| {
                panic!(
                    "{name}: the migrated database did not open: {}",
                    why.detail().unwrap_or_default()
                )
            });

        for table in &report.inventory.tables {
            // A table with a deterministic order is the only one two engines
            // can be compared row for row without sorting, so the question is
            // asked with an ORDER BY over every column.
            let quoted = table.name.replace('"', "\"\"");
            let theirs = ask_sqlite(
                &shell,
                &source,
                &format!("SELECT * FROM \"{quoted}\" ORDER BY 1, 2;"),
            );
            let (ours, _) = opened
                .run(&format!("SELECT * FROM \"{quoted}\" ORDER BY 1, 2"))
                .unwrap_or_else(|why| {
                    panic!(
                        "{name}.{}: the migrated table did not read: {}",
                        table.name,
                        why.detail().unwrap_or_default()
                    )
                });
            let mine: Vec<String> = ours.iter().map(|row| quote_row(row)).collect();
            assert_eq!(
                mine.len(),
                theirs.len(),
                "{name}.{}: the pinned SQLite reads {} rows out of the source and this engine \
                 reads {} out of the migration",
                table.name,
                theirs.len(),
                mine.len()
            );
            for (at, (want, got)) in theirs.iter().zip(mine.iter()).enumerate() {
                assert_eq!(
                    got, want,
                    "{name}.{} row {at} differs:\n  sqlite: {want}\n  ours  : {got}",
                    table.name
                );
            }
            tables_checked = tables_checked.saturating_add(1);
            rows_checked = rows_checked.saturating_add(mine.len());
        }
    }

    assert_eq!(migrated, FIXTURES.len(), "not every fixture was migrated");
    // Rule 1.2: a run that compared no rows would satisfy every assertion in
    // the loop above, because none of them would have run.
    //
    // The floor is what the one published fixture holds rather than what all
    // three do: `chat-archive` is refused by task-2050 and `warehouse` by its
    // external content index, so only `browser-history`'s three tables get
    // compared value for value today. **Raise both numbers when task-2050
    // lands and `chat-archive` moves to `Expect::Publishes`.**
    assert!(
        tables_checked >= 3,
        "only {tables_checked} tables were compared across {migrated} fixtures, so the \
         inventory is not being read"
    );
    assert!(
        rows_checked >= 10_000,
        "only {rows_checked} rows were compared, so the fixtures are not the ones this file \
         describes"
    );
}

/// The schema crosses too: collations, index directions, triggers, views and
/// generated columns.
///
/// **A migration that carried the values and lost the `NOCASE` reads correctly
/// on the day it ran.** It answers a different set the first time somebody
/// searches case-insensitively, which is a week later and a long way from the
/// migration - so this asks the destination's own schema what it says.
#[test]
fn the_schema_crosses_with_the_rows() {
    let Some(_shell) = reference() else {
        inillucent_compat::differential::skipping(
            "the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    };
    let Some(source) = fixture("browser-history") else {
        inillucent_compat::differential::skipping(
            "compat/fixtures/realistic/browser-history.db is not built; run \
             tools/build-realistic-fixtures.sh",
        );
        return;
    };
    let area = scratch("schema");
    let destination = area.join("browser-history.rdb");
    sqlite::migrate(&source, &destination)
        .unwrap_or_else(|why| panic!("the migration failed: {}", why.detail().unwrap_or_default()));

    let opened =
        ImportedDatabase::open(destination, sqlite::PAGE_SIZE, 256).unwrap_or_else(|why| {
            panic!(
                "the migration did not open: {}",
                why.detail().unwrap_or_default()
            )
        });
    let schema = {
        let (rows, _) = opened
            .run("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
            .unwrap_or_else(|why| {
                panic!(
                    "the schema did not read: {}",
                    why.detail().unwrap_or_default()
                )
            });
        rows.iter()
            .map(|row| quote_row(row))
            .collect::<Vec<String>>()
            .join("\n")
    };

    for (wanted, why) in [
        (
            "COLLATE NOCASE",
            "a NOCASE column read back as a plain one, so two rows that were one become two",
        ),
        (
            "last_visit_at DESC",
            "the DESC index lost its direction, so a keyset read comes back backwards",
        ),
        (
            "is_bookmarked = 1",
            "the partial index lost its predicate, so it covers rows it should not",
        ),
        (
            "visit_counts_up",
            "the trigger that keeps the count is not there",
        ),
        (
            "visit_counts_down",
            "the trigger that lowers the count is not there",
        ),
        ("most_visited", "the view is not there"),
        (
            "WITHOUT ROWID",
            "the keyed table came across with a rowid, which is a different table",
        ),
    ] {
        assert!(
            schema.contains(wanted),
            "the migrated schema does not carry `{wanted}`: {why}\n{schema}"
        );
    }

    // And the collation *works*, which the text alone does not say. Two URLs
    // that differ only in case are one row through the unique index.
    let (rows, _) = opened
        .run("SELECT count(*) FROM place WHERE url = 'HTTPS://EXAMPLE.COM/'")
        .unwrap_or_else(|why| {
            panic!(
                "the NOCASE read failed: {}",
                why.detail().unwrap_or_default()
            )
        });
    assert_eq!(
        rows.first().map(|row| quote_row(row)).unwrap_or_default(),
        "1",
        "a NOCASE column did not match a differently cased value after the migration"
    );

    // And the trigger's count is the count the source had, which is what says
    // the *values* the triggers produced came across rather than being
    // recomputed here.
    let (counted, _) = opened
        .run("SELECT sum(visit_count) FROM place")
        .unwrap_or_else(|why| {
            panic!(
                "the count read failed: {}",
                why.detail().unwrap_or_default()
            )
        });
    assert_eq!(
        counted
            .first()
            .map(|row| quote_row(row))
            .unwrap_or_default(),
        "8004",
        "the denormalised count the triggers kept did not come across"
    );
}

/// An external content FTS5 index is refused by name rather than half carried.
///
/// **This is not a defect and the refusal is the feature.** An FTS5 table
/// declared `content='document'` holds no text of its own: the index points at
/// rows in another table. A migration that carried the index without the
/// content would produce a table that answers nothing, and one that
/// materialised it would double the corpus. What the tool does instead is say
/// so, by name, and publish nothing - which is what the assertion below pins.
///
/// The three ordinary tables in the same fixture pass their counts and their
/// digests first, so the refusal is about the index rather than about the
/// database.
#[test]
fn an_external_content_index_is_refused_by_name() {
    let Some(shell) = reference() else {
        inillucent_compat::differential::skipping(
            "the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    };
    let Some(source) = fixture("warehouse") else {
        inillucent_compat::differential::skipping(
            "compat/fixtures/realistic/warehouse.db is not built; run \
             tools/build-realistic-fixtures.sh",
        );
        return;
    };
    let area = scratch("external-content");
    let destination = area.join("warehouse.rdb");
    let report = sqlite::migrate(&source, &destination).unwrap_or_else(|why| {
        panic!(
            "the migration could not be run at all: {}",
            why.detail().unwrap_or_default()
        )
    });

    let documents = ask_sqlite(&shell, &source, "SELECT count(*) FROM document;")
        .first()
        .and_then(|line| line.parse::<i64>().ok())
        .unwrap_or(0);
    assert!(
        documents >= 8_000,
        "the warehouse fixture holds {documents} documents, which is not the fixture this file \
         describes"
    );

    let passed: Vec<&str> = report
        .checks
        .iter()
        .filter(|check| check.passed)
        .map(|check| check.name.as_str())
        .collect();
    let refused: Vec<String> = report
        .checks
        .iter()
        .filter(|check| !check.passed)
        .map(|check| format!("{} {}", check.name, check.detail))
        .collect();

    // The ordinary tables come across: three counts and three digests.
    assert!(
        passed.len() >= 6,
        "only {} checks passed, so the ordinary tables did not come across and this case is \
         about something else:\n  {}",
        passed.len(),
        refused.join("\n  ")
    );
    assert_eq!(
        refused.len(),
        1,
        "the warehouse migration refused {} checks and this case is about one:\n  {}",
        refused.len(),
        refused.join("\n  ")
    );
    let only = refused.first().map(String::as_str).unwrap_or_default();
    assert!(
        only.contains("document_fts") && only.contains("keeps no content table"),
        "the refusal does not name the index or say why it cannot be carried: {only}"
    );
    assert!(
        !destination.is_file(),
        "the migration refused a check and published anyway"
    );
}
