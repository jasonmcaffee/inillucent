//! `inillucent-migrate` run as a process, against a tracked fixture.
//!
//! Invariant: **the counts and the digests the tool prints are the ones the
//! source holds, and a migration that does not publish writes nothing where the
//! destination was.** Both are read off a spawned binary's standard output
//! rather than out of the library, because the library is what every other
//! suite in this crate already drives.
//!
//! **It was the one of the four shipped binaries no test ever spawned
//! (task-1969, 5.1).** `inillucent` is spawned by seven suites,
//! `inillucent-shell` by every suite that goes through
//! `inillucent_compat::interchange`, and `inillucent-mcp` by three;
//! `grep -rn 'inillucent-migrate' crates/*/tests drivers/*/tests` returned doc
//! comment prose and nothing else. So argument parsing, the report's own
//! wording and the exit code had never been observed from outside the process,
//! in the program whose entire job is to be run from a script against somebody
//! else's database.
//!
//! **Why this file cannot reach the compat harness.** `inillucent-migrate` is a
//! production crate and the layering contract refuses it a dependency on the
//! test harness, even as a dev-dependency. The skip helper it does use lives in
//! `inillucent-base`, which is below it - see `docs/invariants/layering.toml`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The tracked SQLite fixture this file migrates.
///
/// One of the 35 files under `compat/fixtures/`, so it is in the repository and
/// this suite runs on a fresh clone. It holds two tables of different shapes -
/// one of people, one of column widths - which is what makes the per-table
/// counts below worth asserting rather than one number.
const FIXTURE: &str = "compat/fixtures/basic-p1024-utf16be.db";

/// What the fixture holds, checked against the tool's own report.
///
/// **Written here rather than read from the tool's output.** A test that took
/// both sides from the same run would pass whatever the run said, which is the
/// shape `tests/inillucent-testing-tdd.md` rule 1.2 names. These came from
/// `sqlite3 compat/fixtures/basic-p1024-utf16be.db 'SELECT count(*) FROM people'`
/// and the same for `widths`, and they change when the fixture changes - which
/// it does not, because it is tracked.
const TABLES: [(&str, u64); 2] = [("people", 7), ("widths", 20)];

/// Returns the workspace root.
fn workspace_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path
}

/// Returns a directory of this case's own, emptied first.
///
/// @param case - what to name it after
fn area(case: &str) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/migrate-cli")
        .join(case);
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a scratch directory");
    path
}

/// What a run of the tool produced.
struct Ran {
    /// Its exit code, or 130 for a run a signal ended.
    code: i32,
    /// Both streams together, with line endings normalised.
    said: String,
}

/// Runs the built `inillucent-migrate` and returns what it produced.
///
/// @param arguments - the command line
fn run(arguments: &[&str]) -> Ran {
    let program = env!("CARGO_BIN_EXE_inillucent-migrate");
    let output = Command::new(program)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("{program} did not start: {error}"));
    Ran {
        code: output.status.code().unwrap_or(130),
        said: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .replace("\r\n", "\n"),
    }
}

/// Returns the fixture's path, or announces a skip and returns nothing.
///
/// The fixture is tracked, so its absence is a checkout that has lost a file
/// rather than a fresh clone - which is why the row declares
/// `tracked-fixtures` rather than `fixtures`.
fn fixture() -> Option<PathBuf> {
    let path = workspace_root().join(FIXTURE);
    if path.is_file() {
        return Some(path);
    }
    inillucent_base::testing::skipping(&format!(
        "{FIXTURE} is not in this checkout, and it is a tracked file"
    ));
    None
}

/// The tool migrates a tracked fixture and reports its counts and digests.
#[test]
fn the_tool_migrates_a_tracked_fixture_and_reports_what_it_moved() {
    let Some(source) = fixture() else {
        return;
    };
    let destination = area("published").join("out.rdb");
    let ran = run(&[
        "--sqlite-file",
        &source.to_string_lossy(),
        &destination.to_string_lossy(),
    ]);
    assert_eq!(ran.code, 0, "the migration did not succeed:\n{}", ran.said);

    let rows: u64 = TABLES.iter().map(|(_, count)| count).sum();
    assert!(
        ran.said
            .contains(&format!("{} tables, {rows} rows", TABLES.len())),
        "the report does not say what it moved:\n{}",
        ran.said
    );

    // Every table's count, by name, and a digest of the right shape beside it.
    // The digest is not compared against a recorded value: what it is for is
    // proving the destination holds the same bytes as the source, and the tool
    // is the thing that compares them - so what this asserts is that the
    // comparison ran and passed for each table, and that the digest it printed
    // is a SHA-256 rather than an empty string.
    for (table, count) in TABLES {
        assert!(
            ran.said
                .contains(&format!("pass count.{table} {count} rows")),
            "the report does not carry a passing count for `{table}`:\n{}",
            ran.said
        );
        let marker = format!("pass digest.{table} ");
        let digest: String = ran
            .said
            .split(&marker)
            .nth(1)
            .unwrap_or("")
            .chars()
            .take_while(|character| character.is_ascii_hexdigit())
            .collect();
        assert_eq!(
            digest.len(),
            64,
            "the digest printed for `{table}` is {} characters rather than a SHA-256:\n{}",
            digest.len(),
            ran.said
        );
    }

    assert!(
        ran.said.contains("published:"),
        "the migration passed every check and did not say it published:\n{}",
        ran.said
    );
    assert!(
        destination.is_file(),
        "the tool said it published and there is no file at {}",
        destination.display()
    );

    // And the source is where it was. The program's own invariant is that it
    // reads the source and writes somewhere else; nothing else in this crate
    // asserts it across a process boundary.
    assert!(
        source.is_file(),
        "the migration removed the source file at {}",
        source.display()
    );
}

/// The published file is a database that opens and holds the rows.
///
/// The report says the counts agree; this reads them back out of the file the
/// report is about. A migration that verified an in-memory copy and published
/// something else would pass the case above.
#[test]
fn the_published_file_holds_the_rows_the_report_counted() {
    let Some(source) = fixture() else {
        return;
    };
    let destination = area("readable").join("out.rdb");
    let ran = run(&[
        "--sqlite-file",
        &source.to_string_lossy(),
        &destination.to_string_lossy(),
    ]);
    assert_eq!(ran.code, 0, "the migration did not succeed:\n{}", ran.said);

    let database = inillucent_engine::connect::Database::open(&destination)
        .unwrap_or_else(|error| panic!("the published file does not open: {}", error.message()));
    let connection = database.session();
    for (table, count) in TABLES {
        let answered = connection
            .query(&format!("SELECT count(*) FROM {table}"))
            .unwrap_or_else(|error| {
                panic!(
                    "counting `{table}` in the published file: {}",
                    error.message()
                )
            });
        let read = answered
            .first()
            .and_then(|row| row.first())
            .map(|value| format!("{value:?}"))
            .unwrap_or_default();
        assert!(
            read.contains(&count.to_string()),
            "the published file holds {read} rows in `{table}` and the source holds {count}"
        );
    }
}

/// A migration the tool refuses writes nothing where the destination was.
///
/// **This is the half of the program's invariant a test can reach
/// (task-1969, 5.7 and 6.3).** `main.rs`'s header used to say the SQLite path
/// "always leaves the staging file behind when it does not publish", and only
/// the legacy index path had a case for it (`migration.rs`). The staging file
/// exists between the build and the rename, so a run that is refused *before*
/// the build - which is every refusal a test can produce without a fault
/// injector - leaves no staging file, and the sentence now says so.
///
/// What is provable across a process boundary is the invariant that matters to
/// a caller: a refused migration does not put anything at the destination, and
/// does not touch the source.
#[test]
fn a_refused_migration_leaves_the_destination_and_the_source_alone() {
    let Some(source) = fixture() else {
        return;
    };
    let before = std::fs::metadata(&source)
        .map(|held| held.len())
        .unwrap_or_default();
    let directory = area("refused");
    let destination = directory.join("out.rdb");

    // A destination that is already there. The tool publishes by renaming and
    // refuses to overwrite, which is the refusal a caller is most likely to
    // meet, and the one where writing anyway would destroy a database.
    std::fs::write(&destination, b"this file was here first").expect("the blocker is written");
    let ran = run(&[
        "--sqlite-file",
        &source.to_string_lossy(),
        &destination.to_string_lossy(),
    ]);
    assert_ne!(
        ran.code, 0,
        "the tool overwrote a destination that already existed:\n{}",
        ran.said
    );
    assert!(
        ran.said.contains("already exists"),
        "the refusal does not say what was wrong:\n{}",
        ran.said
    );
    assert_eq!(
        std::fs::read(&destination).unwrap_or_default(),
        b"this file was here first",
        "the refused migration changed the file at the destination"
    );
    assert_eq!(
        std::fs::metadata(&source)
            .map(|held| held.len())
            .unwrap_or_default(),
        before,
        "the refused migration changed the source"
    );
    assert!(
        !staging_beside(&destination),
        "the migration was refused before it staged anything and left a staging file at \
         {}",
        destination.display()
    );
}

/// A source that is not a SQLite database is refused, by name.
#[test]
fn a_source_that_is_not_a_database_is_refused_by_name() {
    let directory = area("not-a-database");
    let source = directory.join("not-a-database.db");
    std::fs::write(&source, b"these are not the bytes of a database")
        .expect("the source is written");
    let ran = run(&[
        "--sqlite-file",
        &source.to_string_lossy(),
        &directory.join("out.rdb").to_string_lossy(),
    ]);
    assert_ne!(
        ran.code, 0,
        "the tool accepted a file that is not a database:\n{}",
        ran.said
    );
    assert!(
        ran.said.contains("not-a-database.db"),
        "the refusal does not name the file it could not read:\n{}",
        ran.said
    );
    assert!(
        !directory.join("out.rdb").exists(),
        "the refused migration left a destination file behind"
    );
}

/// Reports whether the staging file for a destination is beside it.
///
/// The name is the one `sqlite.rs`'s `staging_path` builds: a dot, the
/// destination's file name, and `.staging`. It is written out here rather than
/// imported, because it is a promise the program makes to whoever has to clean
/// up after a failed run, and a test that read it from the same function could
/// not notice the promise changing.
///
/// @param destination - where the migration would publish
fn staging_beside(destination: &Path) -> bool {
    let Some(name) = destination.file_name() else {
        return false;
    };
    let Some(directory) = destination.parent() else {
        return false;
    };
    directory
        .join(format!(".{}.staging", name.to_string_lossy()))
        .exists()
}
