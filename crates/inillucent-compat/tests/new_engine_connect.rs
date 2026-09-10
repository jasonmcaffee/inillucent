//! Opening a database by path and using it, with no import in the story.
//!
//! Invariant: **`open` on a path that does not exist creates one, and on a path
//! that does opens and recovers it.** That is what a caller means by `open`, it
//! is what `inillucent-session` does today, and it is the behaviour every caller
//! being re-rooted onto `inillucent-engine` depends on. A connection that only
//! opened existing files would move none of them.
//!
//! This is the shape the re-rooting moves onto — `Database::open`, `connect`,
//! `execute_batch`, `query` — exercised end to end rather than unit-tested,
//! because what is being checked is that the pieces underneath it (create, open,
//! recovery, tree identity) hold together when a caller drives them the way a
//! caller will.

use std::path::PathBuf;

use inillucent_compat::workspace_root;
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

/// Returns a clean path for one test's database.
///
/// @param name - the test's name
fn scratch(name: &str) -> PathBuf {
    let area = workspace_root().join("target/scratch/connect");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Returns the first column of every row as text.
///
/// @param rows - the rows a query produced
fn column(rows: &[Vec<OwnedDatum>]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| row.first())
        .map(|value| match value {
            OwnedDatum::Null => "NULL".to_string(),
            OwnedDatum::Int(number) => number.to_string(),
            OwnedDatum::Real(number) => number.to_string(),
            OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            OwnedDatum::Blob(bytes) => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        })
        .collect()
}

/// A database is opened by path, filled, closed, and opened again.
#[test]
fn a_database_opens_by_path_and_keeps_what_was_written() {
    let path = scratch("by-path");
    {
        let database = Database::open(&path).expect("a path with nothing in it is created");
        let connection = database.connect();
        connection
            .execute_batch(
                "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT); \
                 CREATE INDEX note_by_body ON note(body); \
                 INSERT INTO note(id, body) VALUES (1, 'alpha'); \
                 INSERT INTO note(id, body) VALUES (2, 'bravo')",
            )
            .expect("the batch runs");
        assert_eq!(
            column(
                &connection
                    .query("SELECT body FROM note ORDER BY id")
                    .unwrap()
            ),
            vec!["alpha".to_string(), "bravo".to_string()]
        );
        database.check().expect("every tree is intact");
    }

    // Opened again, by the same call, with no checkpoint in between - so this is
    // the recovery path as well as the open path.
    let database = Database::open(&path).expect("a path with a database in it is opened");
    let connection = database.connect();
    assert_eq!(
        column(
            &connection
                .query("SELECT body FROM note ORDER BY id")
                .unwrap()
        ),
        vec!["alpha".to_string(), "bravo".to_string()],
        "what was written before the close is not what came back after the open"
    );
    database.check().expect("every tree is intact");
}

/// A statement written after a reopen lands beside the ones before it.
///
/// The sharper half of the round trip: a database that reopened *readably* but
/// could not be written to again would pass the test above and fail the first
/// thing a caller did next. It also exercises the identifier the catalog now
/// carries, because a `CREATE TABLE` after an open has to number its tree
/// without colliding with one already in the file.
#[test]
fn a_reopened_database_can_be_written_to_again() {
    let path = scratch("write-after-open");
    {
        let database = Database::open(&path).expect("created");
        database
            .connect()
            .execute_batch("CREATE TABLE first (id INTEGER PRIMARY KEY, a TEXT)")
            .expect("the first table is created");
        database.checkpoint().expect("checkpointed");
    }
    {
        let database = Database::open(&path).expect("opened");
        let connection = database.connect();
        connection
            .execute_batch(
                "CREATE TABLE second (id INTEGER PRIMARY KEY, b TEXT); \
                 INSERT INTO first(id, a) VALUES (1, 'one'); \
                 INSERT INTO second(id, b) VALUES (1, 'two')",
            )
            .expect("a table made after the open works too");
        assert_eq!(
            column(&connection.query("SELECT a FROM first").unwrap()),
            vec!["one".to_string()]
        );
        assert_eq!(
            column(&connection.query("SELECT b FROM second").unwrap()),
            vec!["two".to_string()]
        );
        database.check().expect("every tree is intact");
    }

    // And once more, so the second table survives the same trip the first did.
    let database = Database::open(&path).expect("opened again");
    let connection = database.connect();
    assert_eq!(
        column(&connection.query("SELECT b FROM second").unwrap()),
        vec!["two".to_string()],
        "a table created after an open did not survive the next one"
    );
}

/// An empty database opens, answers, and is not mistaken for a broken one.
#[test]
fn a_new_database_is_usable_immediately() {
    let path = scratch("fresh");
    let database = Database::open(&path).expect("created");
    let connection = database.connect();
    assert!(
        connection
            .query("SELECT name FROM sqlite_schema")
            .expect("the schema is queryable")
            .is_empty(),
        "a fresh database names no objects"
    );
    assert_eq!(database.path(), path.as_path());
    database.check().expect("the catalog tree is intact");
}

/// `PRAGMA cache_size` grows the pool, rather than being accepted and clamped.
///
/// **What this is a regression test for.** The pool's frames were
/// allocated at open and nothing could add to them, so `PRAGMA cache_size` could
/// only lower a ceiling inside what was already there: setting it larger read
/// back the default, and neither the shell nor the command line has a flag for
/// the pool either. A two-gigabyte pool was therefore reachable only from a
/// program that linked the crate and called `Database::open_with` - which is
/// what stopped a crash already hit on a live database from being reduced,
/// because the pool size it ran under could not be reproduced.
///
/// Both spellings are checked, because they are the two SQLite has and they
/// take different arithmetic: a negative number is kibibytes and a positive one
/// is pages.
#[test]
fn cache_size_grows_the_pool_and_reads_back_what_was_asked_for() {
    let path = scratch("cache-size");
    let database = Database::open(&path).expect("a fresh database opens");
    let connection = database.connect();
    connection
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)")
        .expect("the schema is created");

    let default = connection
        .query("PRAGMA cache_size")
        .expect("the pragma is read");
    let default = match default.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("expected an integer, got {other:?}"),
    };
    assert!(
        default < 0,
        "the default is stated in kibibytes, and read back {default}"
    );

    // Ten times the pool it opened with, in kibibytes.
    let asked = default.saturating_mul(10);
    connection
        .execute_batch(&format!("PRAGMA cache_size = {asked}"))
        .expect("the pragma is set");
    let after = connection
        .query("PRAGMA cache_size")
        .expect("the pragma is read back");
    let after = match after.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("expected an integer, got {other:?}"),
    };
    assert_eq!(
        after, asked,
        "a larger cache is granted rather than clamped to {default}"
    );

    // And in pages, which is the other spelling.
    connection
        .execute_batch("PRAGMA cache_size = 200000")
        .expect("the pragma is set in pages");
    let pages = connection
        .query("PRAGMA cache_size")
        .expect("the pragma is read back");
    assert_eq!(
        pages.first().and_then(|row| row.first()),
        Some(&OwnedDatum::Int(200_000)),
        "a cache stated in pages is granted too"
    );

    // The database still works, which is what says the grow did not disturb
    // what was already resident: the schema above is in the pool right now.
    connection
        .execute_batch("INSERT INTO t (body) VALUES ('after the grow')")
        .expect("a write still lands");
    let rows = connection
        .query("SELECT body FROM t")
        .expect("the row reads back");
    assert_eq!(column(&rows), vec!["after the grow".to_string()]);
}
