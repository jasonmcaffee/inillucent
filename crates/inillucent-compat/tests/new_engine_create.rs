//! Making a database from nothing, and building a schema in it.
//!
//! Invariant: **a database this engine created is a database this engine can
//! open**, with no import anywhere in the story. Every test before this one
//! started from a SQLite fixture, because `import` was the only way in; this is
//! the first that does not, and it is the primitive a re-rooted `Database::open`
//! needs — opening a path that does not exist has to create one.
//!
//! What is checked is the whole round trip rather than the call: create, run
//! DDL, write rows, close, open the file again from scratch, and read them back.
//! A `create` that produced a file only the process that wrote it could use
//! would pass a weaker test and fail the thing it exists for.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// How many frames these databases get; they are tiny.
const FRAMES: usize = 256;

/// The page size, which is the engine's default.
const PAGE_SIZE: usize = 32_768;

/// Returns a clean path for one test's database.
///
/// @param name - the test's name
fn scratch(name: &str) -> PathBuf {
    let area = workspace_root().join("target/scratch/create");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Returns the first column of every row, rendered as text.
///
/// @param rows - the rows a statement produced
fn first_column(rows: &[Vec<OwnedDatum>]) -> Vec<String> {
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

/// A database is created, filled and read back after a fresh open.
#[test]
fn a_created_database_survives_being_closed_and_opened() {
    let path = scratch("round-trip");
    {
        let mut database = ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES)
            .expect("a fresh database is created");
        for statement in [
            "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)",
            "CREATE INDEX note_by_body ON note(body)",
            "INSERT INTO note(id, body) VALUES (1, 'alpha')",
            "INSERT INTO note(id, body) VALUES (2, 'bravo')",
            "INSERT INTO note(id, body) VALUES (3, 'charlie')",
        ] {
            database
                .execute_any(statement, &Params::new())
                .unwrap_or_else(|error| {
                    panic!("{statement}: {}", error.detail().unwrap_or_default())
                });
        }
        database.check_trees().expect("every tree is intact");

        // **No checkpoint, on purpose.** Everything above is in the log and
        // not yet in the pages, which is the ordinary shape of a crash and is
        // exactly what `create` followed by DDL produces. Opening the file has
        // to replay it.
        //
        // This assertion has been three things in one sitting, and the history
        // is the point. It first read the rows back and got "no such table:
        // note" from a file that plainly had one, because `open` did not replay
        // and said nothing about it. Then it asserted a *refusal*, which was
        // honest but was not an open. Now it asserts the recovery.
    }

    // A different handle, over a file this process is no longer holding and
    // whose log still carries every one of those statements.
    let reopened = ImportedDatabase::open(path.clone(), PAGE_SIZE, FRAMES)
        .expect("the file opens, replaying its log");
    let (rows, _) = reopened
        .run("SELECT body FROM note ORDER BY id")
        .expect("the rows read back");
    assert_eq!(
        first_column(&rows),
        vec![
            "alpha".to_string(),
            "bravo".to_string(),
            "charlie".to_string()
        ],
        "the rows written before the close are the rows read after the open"
    );
    reopened.check_trees().expect("every tree is intact");
}

/// An empty database is a legal database, not an absent one.
///
/// The catalog tree has to exist with no rows in it, because every later
/// `CREATE TABLE` inserts into it. A `create` that left the catalog root
/// pointing nowhere would pass a smoke test and fail the first DDL statement.
#[test]
fn a_created_database_is_empty_rather_than_broken() {
    let path = scratch("empty");
    let database =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    let (rows, _) = database
        .run("SELECT name FROM sqlite_schema")
        .expect("the schema is queryable");
    assert!(
        rows.is_empty(),
        "a fresh database names no objects, it listed {rows:?}"
    );
    database.check_trees().expect("the catalog tree is intact");
    drop(database);

    // And it is a file on a disk that opens again.
    assert!(path.is_file(), "the database was written");
    let reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("an empty file opens");
    let (rows, _) = reopened
        .run("SELECT name FROM sqlite_schema")
        .expect("the schema is queryable after a reopen");
    assert!(rows.is_empty(), "still empty: {rows:?}");
}

/// The index a created database builds is used and gives the same answers.
///
/// A schema built through DDL rather than through the import takes a different
/// path into the catalog, so this checks that what comes out the other side is
/// a real index rather than a catalog row describing one.
#[test]
fn an_index_built_by_ddl_answers_the_same_as_a_scan() {
    let path = scratch("index");
    let mut database = ImportedDatabase::create(path, PAGE_SIZE, FRAMES).expect("a fresh database");
    database
        .execute_any(
            "CREATE TABLE point (id INTEGER PRIMARY KEY, kind INTEGER)",
            &Params::new(),
        )
        .expect("the table is created");
    for id in 1..=200i64 {
        database
            .execute_any(
                &format!("INSERT INTO point(id, kind) VALUES ({id}, {})", id % 7),
                &Params::new(),
            )
            .expect("the row is inserted");
    }
    let (before, _) = database
        .run("SELECT count(*) FROM point WHERE kind = 3")
        .expect("the scan answers");

    database
        .execute_any("CREATE INDEX point_by_kind ON point(kind)", &Params::new())
        .expect("the index is created");

    let (after, _) = database
        .run("SELECT count(*) FROM point WHERE kind = 3")
        .expect("the indexed query answers");
    assert_eq!(
        first_column(&before),
        first_column(&after),
        "the index changed the answer"
    );
    database.check_trees().expect("every tree is intact");
}
