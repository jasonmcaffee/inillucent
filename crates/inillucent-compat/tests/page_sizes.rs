//! Writing at every page size the engine accepts, not only the one it ships.
//!
//! Invariant: **an ordinary `INSERT` succeeds at every page size a database can
//! be created with, and the rows read back.** `Options::default` is 32,768 and
//! every published figure is taken there, so a defect that only appears below
//! it reaches an application rather than a gate - and an embedder who picks
//! SQLite's own 4,096 is not picking an exotic number.
//!
//! Before task-2033 nothing here passed except at 32,768 and 65,536. Two
//! hundred FTS5 documents answered `SQLITE_CORRUPT` on row 42 at 4,096, and on
//! rows 14, 37, 89 and 182 at 1,024, 2,048, 8,192 and 16,384; the R-Tree did
//! the same on row 155 at 1,024; and at 512 the *third* `CREATE TABLE` in any
//! database refused, because the schema catalog has ten columns and a
//! ten-column leaf holding one row has spent 384 of a 512-byte page before it
//! holds anything else.
//!
//! Every case closes the database and opens it again, because two of the three
//! defects task-2033 found were only visible on the second open: the writes
//! succeeded, `PRAGMA integrity_check` answered `ok`, and rows were gone.
//!
//! §1.2 of the testing standard: every case asserts the rows it wrote are the
//! rows a query answers with, so a build that writes nothing cannot pass here.

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// How many rows each case writes.
///
/// Two hundred, because row 182 is the latest any of the page sizes used to
/// reach, so a shorter run would pass at 16,384 without the fix.
const ROWS: usize = 200;

/// Every page size the engine accepts, smallest first.
///
/// 512 is SQLite's floor and 65,536 its ceiling; 32,768 is `Options::default`
/// and, until task-2033, the only one this suite covered.
const PAGE_SIZES: [usize; 8] = [512, 1_024, 2_048, 4_096, 8_192, 16_384, 32_768, 65_536];

/// Returns a fresh database at the given page size.
///
/// @param tag - what to name the file
/// @param page_size - the page size to create it with
fn database(tag: &str, page_size: usize) -> (ImportedDatabase, std::path::PathBuf) {
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("page-sizes")
        .join(format!("{tag}-{page_size}.rdb"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&path);
    let database =
        ImportedDatabase::create(path.clone(), page_size, 256).expect("a fresh database");
    (database, path)
}

/// Closes a database and opens the file again.
///
/// **Where two of task-2033's three defects were, and neither was visible
/// without this.** Writing a row and reading it back in the same session says
/// nothing about the log: the page is in the pool either way. The defects were
/// in what a second open could reconstruct - recovery learned each table's
/// shape only from the row records naming the schema tree, so rows written
/// into a table whose catalog row arrived any other way were dropped on
/// replay, silently, with `PRAGMA integrity_check` answering `ok` afterwards.
/// A case that did not reopen passed through all of it.
///
/// @param database - the database to close
/// @param path - the file it was opened on
/// @param page_size - the page size it was created with
fn reopened(
    database: ImportedDatabase,
    path: &std::path::Path,
    page_size: usize,
) -> ImportedDatabase {
    drop(database);
    ImportedDatabase::open(path.to_path_buf(), page_size, 256)
        .unwrap_or_else(|error| panic!("the {page_size}-byte database reopens: {error}"))
}

/// Runs one statement, naming the page size when it refuses.
///
/// @param database - the open database
/// @param page_size - the page size, for the message
/// @param sql - the statement
fn run(database: &mut ImportedDatabase, page_size: usize, sql: &str) {
    if let Err(error) = database.execute_any(sql, &Params::new()) {
        panic!(
            "at a {page_size}-byte page `{sql}` answered {error} ({:?})",
            error.detail()
        );
    }
}

/// Returns the single integer a one row, one column query answered with.
///
/// @param database - the open database
/// @param sql - the query, which must answer one row of one integer
fn count(database: &mut ImportedDatabase, sql: &str) -> i64 {
    let outcome = database
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| panic!("`{sql}` answers: {error}"));
    let row = outcome
        .rows
        .first()
        .unwrap_or_else(|| panic!("`{sql}` answers a row"));
    match row.first() {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("`{sql}` answers an integer, not {other:?}"),
    }
}

#[test]
fn an_ordinary_table_writes_two_hundred_rows_at_every_page_size() {
    for page_size in PAGE_SIZES {
        let (mut database, path) = database("ordinary", page_size);
        // **Four tables, because the defect at 512 was in the catalog rather
        // than in the table.** Each `CREATE TABLE` writes one ten-column row
        // into the schema tree, and it was the third of those that refused.
        for table in 0..4 {
            run(
                &mut database,
                page_size,
                &format!("CREATE TABLE t{table} (id INTEGER PRIMARY KEY, a TEXT, b TEXT, c TEXT)"),
            );
        }
        for at in 0..ROWS {
            run(
                &mut database,
                page_size,
                &format!(
                    "INSERT INTO t0 (id, a, b, c) \
                     VALUES ({at}, 'alpha {at}', 'beta {at}', 'gamma {at}')"
                ),
            );
        }
        assert_eq!(
            count(&mut database, "SELECT count(*) FROM t0"),
            ROWS as i64,
            "every row is in the table at {page_size}"
        );
        assert_eq!(
            count(
                &mut database,
                "SELECT count(*) FROM t0 WHERE a LIKE 'alpha 1%'"
            ),
            111,
            "the rows read back with their values at {page_size}"
        );
        let mut database = reopened(database, &path, page_size);
        assert_eq!(
            count(&mut database, "SELECT count(*) FROM t0"),
            ROWS as i64,
            "every row is still there after a reopen at {page_size}"
        );
        assert_eq!(
            count(
                &mut database,
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'"
            ),
            4,
            "all four tables are still in the catalog after a reopen at {page_size}"
        );
    }
}

#[test]
fn fts5_writes_two_hundred_documents_at_every_page_size() {
    for page_size in PAGE_SIZES {
        let (mut database, path) = database("fts5", page_size);
        run(
            &mut database,
            page_size,
            "CREATE VIRTUAL TABLE documents USING fts5(title, body)",
        );
        for at in 0..ROWS {
            run(
                &mut database,
                page_size,
                &format!(
                    "INSERT INTO documents(title, body) VALUES ('note {at}', \
                     'lorem ipsum dolor sit amet number {at} consectetur adipiscing elit')"
                ),
            );
        }
        assert_eq!(
            count(&mut database, "SELECT count(*) FROM documents"),
            ROWS as i64,
            "every document is in the table at {page_size}"
        );
        // The term every document holds is the one whose doclist grows with
        // every write, which is the row the ticket expected to be at fault.
        assert_eq!(
            count(
                &mut database,
                "SELECT count(*) FROM documents WHERE documents MATCH 'lorem'"
            ),
            ROWS as i64,
            "the index answers for a term every document holds at {page_size}"
        );
        assert_eq!(
            count(
                &mut database,
                "SELECT count(*) FROM documents WHERE documents MATCH 'number'"
            ),
            ROWS as i64,
            "the index answers for a second common term at {page_size}"
        );
        let mut database = reopened(database, &path, page_size);
        assert_eq!(
            count(
                &mut database,
                "SELECT count(*) FROM documents WHERE documents MATCH 'lorem'"
            ),
            ROWS as i64,
            "the index still answers after a reopen at {page_size}"
        );
    }
}

#[test]
fn rtree_writes_two_hundred_boxes_at_every_page_size() {
    for page_size in PAGE_SIZES {
        let (mut database, _path) = database("rtree", page_size);
        run(
            &mut database,
            page_size,
            "CREATE VIRTUAL TABLE boxes USING rtree(id, minX, maxX, minY, maxY)",
        );
        for at in 0..ROWS {
            run(
                &mut database,
                page_size,
                &format!(
                    "INSERT INTO boxes(id, minX, maxX, minY, maxY) \
                     VALUES ({at}, {at}, {}, {at}, {})",
                    at + 3,
                    at + 5
                ),
            );
        }
        assert_eq!(
            count(&mut database, "SELECT count(*) FROM boxes"),
            ROWS as i64,
            "every box is in the table at {page_size}"
        );
        assert_eq!(
            count(
                &mut database,
                "SELECT count(*) FROM boxes WHERE minX >= 10 AND maxX <= 23"
            ),
            11,
            "the index answers a window query at {page_size}"
        );
    }
}

/// A row that genuinely cannot fit a page is refused, and says why.
///
/// **The other half of not answering `SQLITE_CORRUPT`.** A key column is never
/// stored out of line, because a descent compares keys and a comparison that
/// read another page would turn every search into a chain of them - so a key
/// longer than a page cannot be stored at all. That is a statement this engine
/// will not run, not a damaged file, and it has to read as one: a corruption
/// code sends the reader to the integrity check instead of to their schema.
#[test]
fn a_key_longer_than_a_page_is_refused_rather_than_called_corrupt() {
    let (mut database, _path) = database("oversized-key", 512);
    run(
        &mut database,
        512,
        "CREATE TABLE t (k TEXT PRIMARY KEY, v TEXT)",
    );
    let key = "k".repeat(4_096);
    let error = database
        .execute_any(
            &format!("INSERT INTO t (k, v) VALUES ('{key}', 'v')"),
            &Params::new(),
        )
        .expect_err("a key eight times the page cannot be stored");
    assert_ne!(
        error.code(),
        inillucent_base::error::PrimaryCode::Corrupt,
        "a row the engine cannot store answered `{}` ({:?})",
        error.message(),
        error.detail()
    );
    let detail = error.detail().unwrap_or_default().to_string();
    assert!(
        detail.contains("key"),
        "the refusal said `{detail}`, which does not name the key column as the reason"
    );
}

/// A row written into a table created in the same transaction survives a
/// reopen, however the table's catalog row reached the page.
///
/// **The narrowest form of the third defect task-2033 found, and the one with
/// no virtual table in it.** Recovery learns each table's shape by watching the
/// `InsertRow` records on the schema tree go past, which is correct only while
/// every `CREATE TABLE` reaches the log as a row record. At a 512-byte page the
/// third `CREATE TABLE` in a transaction fills the catalog leaf, so its row is
/// placed by rebuilding the leaf around it and reaches the log inside a page
/// image instead. Recovery had then never heard of the table, the `INSERT` that
/// followed named a tree it had no shape for, and the record was dropped by the
/// tolerance that exists so a crash during `REINDEX` can still be opened. The
/// row was lost with no error at all and `PRAGMA integrity_check` said `ok`.
///
/// Three tables is where it started and seven is well past it; the insert goes
/// into the **last** table created, because the one before it is the one whose
/// catalog row the repack placed.
#[test]
fn a_row_in_a_table_created_in_the_same_transaction_survives_a_reopen() {
    for page_size in PAGE_SIZES {
        for tables in [3usize, 5, 7] {
            let (mut database, path) = database(&format!("same-txn-{tables}"), page_size);
            run(&mut database, page_size, "BEGIN");
            for at in 0..tables {
                run(
                    &mut database,
                    page_size,
                    &format!("CREATE TABLE t{at} (id INTEGER PRIMARY KEY, x TEXT)"),
                );
            }
            let last = tables.saturating_sub(1);
            run(
                &mut database,
                page_size,
                &format!("INSERT INTO t{last} (id, x) VALUES (1, 'one')"),
            );
            run(&mut database, page_size, "COMMIT");
            let query = format!("SELECT count(*) FROM t{last}");
            assert_eq!(
                count(&mut database, &query),
                1,
                "the row is in t{last} at {page_size} before the reopen"
            );
            let mut database = reopened(database, &path, page_size);
            assert_eq!(
                count(&mut database, &query),
                1,
                "the row written into t{last}, the last of {tables} tables created in one                  transaction, is gone after a reopen at a {page_size}-byte page"
            );
            assert_eq!(
                count(
                    &mut database,
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table'"
                ),
                tables as i64,
                "all {tables} tables are in the catalog after a reopen at {page_size}"
            );
        }
    }
}
