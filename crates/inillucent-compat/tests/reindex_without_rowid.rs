//! `REINDEX` on a database holding a `WITHOUT ROWID` table.
//!
//! Invariant: **a `REINDEX` rebuilds every index that is a tree of its own,
//! and a `WITHOUT ROWID` table's primary key is not one - it is the table.**
//!
//! The catalog loader lists that key among the table's indexes, pointed at the
//! table's own root, because that is what lets the planner seek on it. SQLite
//! writes no `sqlite_schema` row for it, and neither does this engine. So a
//! bare `REINDEX` reached the key, looked for its catalog row, found none and
//! refused with `the index has no catalog row` under the `syntax` class - on a
//! database whose only unusual feature was one `WITHOUT ROWID` table. A
//! consumer probing 0.1.8 against SQLite 3.51.0 reported it; SQLite runs the
//! same two statements silently.
//!
//! Each test checks what the statement was run for, not only that it ran: the
//! secondary index still answers a seek, and the file passes
//! `PRAGMA integrity_check`.

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// How many frames these databases get; they are tiny.
const FRAMES: usize = 256;

/// The page size, which is the engine's default.
const PAGE_SIZE: usize = 32_768;

/// Returns a fresh database for one test.
///
/// @param name - the test's name, for the scratch path
fn fresh(name: &str) -> ImportedDatabase {
    let area = workspace_root().join("_agent_output/reindex-without-rowid");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    ImportedDatabase::create(path, PAGE_SIZE, FRAMES).expect("a fresh database is created")
}

/// Runs one statement, panicking with its text and detail on refusal.
///
/// @param database - the connection
/// @param sql - the statement text
fn exec(database: &mut ImportedDatabase, sql: &str) {
    database
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.detail().unwrap_or_default()));
}

/// Returns the rows one query answers.
///
/// @param database - the connection
/// @param sql - the query
fn rows(database: &mut ImportedDatabase, sql: &str) -> Vec<Vec<OwnedDatum>> {
    database
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.detail().unwrap_or_default()))
        .rows
}

/// Builds the report's schema: a `WITHOUT ROWID` table with a secondary index
/// and a few rows, beside an ordinary table with an index of its own.
///
/// The ordinary table is there so the test can see that a bare `REINDEX` still
/// rebuilds the indexes it should, rather than passing by rebuilding nothing.
///
/// @param database - the connection
fn the_reported_schema(database: &mut ImportedDatabase) {
    exec(
        database,
        "CREATE TABLE w (a TEXT PRIMARY KEY, b INT) WITHOUT ROWID",
    );
    exec(database, "CREATE INDEX w_b ON w(b)");
    exec(
        database,
        "INSERT INTO w VALUES ('x', 2), ('y', 1), ('z', 3)",
    );
    exec(
        database,
        "CREATE TABLE plain (id INTEGER PRIMARY KEY, c TEXT)",
    );
    exec(database, "CREATE INDEX plain_c ON plain(c)");
    exec(database, "INSERT INTO plain (c) VALUES ('q'), ('p')");
}

/// Asserts that both secondary indexes answer and the file is sound.
///
/// @param database - the connection
fn assert_the_indexes_answer(database: &mut ImportedDatabase) {
    assert_eq!(
        rows(database, "SELECT a FROM w INDEXED BY w_b WHERE b = 1"),
        vec![vec![OwnedDatum::Text(b"y".to_vec())]],
        "the WITHOUT ROWID table's secondary index finds its row after the REINDEX"
    );
    assert_eq!(
        rows(
            database,
            "SELECT id FROM plain INDEXED BY plain_c WHERE c = 'p'"
        ),
        vec![vec![OwnedDatum::Int(2)]],
        "the ordinary table's index finds its row after the REINDEX"
    );
    assert_eq!(
        rows(database, "SELECT a FROM w WHERE a = 'z'"),
        vec![vec![OwnedDatum::Text(b"z".to_vec())]],
        "the primary key, which is the table, still answers a seek"
    );
    assert_eq!(
        rows(database, "PRAGMA integrity_check"),
        vec![vec![OwnedDatum::Text(b"ok".to_vec())]]
    );
}

/// A bare `REINDEX` runs on a database holding a `WITHOUT ROWID` table.
///
/// This is the report's reproduction. Before the fix it answered
/// `the index has no catalog row`.
#[test]
fn a_bare_reindex_runs_beside_a_without_rowid_table() {
    let mut database = fresh("bare");
    the_reported_schema(&mut database);
    exec(&mut database, "REINDEX");
    assert_the_indexes_answer(&mut database);
}

/// `REINDEX` naming the `WITHOUT ROWID` table rebuilds its secondary index.
///
/// The report only tried the bare form. Naming the table took the same path
/// through the table's index list, so it failed the same way.
#[test]
fn reindex_naming_the_without_rowid_table_runs() {
    let mut database = fresh("named-table");
    the_reported_schema(&mut database);
    exec(&mut database, "REINDEX w");
    assert_the_indexes_answer(&mut database);
}

/// The rebuild survives closing and opening the file again.
///
/// A `REINDEX` moves an index onto a new tree and rewrites its catalog row, so
/// the proof that the right rows were rewritten is a fresh open that reads
/// every index through the catalog alone.
#[test]
fn the_rebuilt_indexes_survive_a_reopen() {
    let area = workspace_root().join("_agent_output/reindex-without-rowid");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join("reopen.rdb");
    let _ = std::fs::remove_file(&path);
    {
        let mut database = ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES)
            .expect("a fresh database is created");
        the_reported_schema(&mut database);
        exec(&mut database, "REINDEX");
    }
    let mut database = ImportedDatabase::open(path, PAGE_SIZE, FRAMES)
        .expect("the reindexed database opens again");
    assert_the_indexes_answer(&mut database);
}
