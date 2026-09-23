//! A `DELETE` reads each leaf of its table and of its indexes about once,
//! whatever order the query that chose its rows returned them in.
//!
//! Invariant: **the pages a delete reads are bounded by the trees, not by the
//! rows it deletes,** and the order rows are removed in is the query's order
//! whenever anything can see it.
//!
//! ## The defect (task-2077)
//!
//! `story_large_table_nightly` deleted every other row of a table past an
//! 8 MiB pool and took 5.4 times as long per deleted row at the engine's own
//! 32,768 byte page as at SQLite's 4,096. It was not work inside a leaf. The
//! keys query for `DELETE FROM big WHERE a % 2 = 0` is `SCAN big USING
//! COVERING INDEX big_d`, so the keys came back in `(d, a)` order and the
//! delete looked each one up in the table in that order: every key a
//! different leaf, and one value of `d` visiting more leaves than the pool
//! held. `inillucent-writeprofile --spread 500000 8` measured 176,503 reads
//! and 175,507 writes for 250,000 deletes on a table of 558 leaves.
//!
//! Putting the keys in the table's order was half of the fix. Removing each
//! row together with its entry in `big_d` then visited the index out of order
//! instead, and the story read 285,098 pages of a 2,221 page file, 280,611 of
//! them leaves of the index. The delete now removes the rows in the table's
//! order and then every index's entries in that index's order.
//!
//! ## Why a count and not a clock
//!
//! The defect is a read per deleted row where there should be a read per
//! leaf, which is a factor of the rows per leaf and is visible in the pool's
//! own counter on any machine under any load. `crates/inillucent/tests/
//! budget.rs` gives the argument at length: a ratio of counts taken in one run
//! is what a test can hold, and a stopwatch is not.
//!
//! The table here is sized so both halves show in a debug build in seconds,
//! at a 4,096 byte page. A leaf holds about 113 rows and `d` repeats every
//! 128, so the rows of one value of `d` are each in a different leaf, about
//! 312 of them, and an index leaf holds one or two values of `d`. The pool
//! keeps 16 pages, far fewer than either of those.
//!
//! **Measured three ways on the same build**, deleting 20,000 rows of a 406
//! page file:
//!
//! | the delete | reads | this test |
//! |---|---|---|
//! | rows in the table's order, then each index's entries in its order | 524 | passes |
//! | rows in the table's order, each with its index entries | 23,295 | fails |
//! | rows in the order the query returned them | 25,198 | fails |
//!
//! The first sizing tried, 20,000 rows and a 96 page pool, read 286 pages in
//! the query's order and passed, which is why the sizes are what they are.

use std::path::PathBuf;

use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// How many rows the table holds.
const ROWS: i64 = 40_000;

/// The period of the indexed column, larger than the rows a leaf holds.
///
/// Larger than the 113 rows a leaf holds is the condition: consecutive keys of
/// one `d` are then in different leaves, so an index order delete visits a
/// different leaf for every row.
const PERIOD: i64 = 128;

/// How many pages `PRAGMA cache_size` lets the pool keep.
///
/// Far fewer than the 312 leaves one value of `d` visits, so a delete in index
/// order cannot keep them, and fewer than the leaves of `big_d`, so a delete
/// that visits the index in the table's order cannot keep those either.
const POOL_PAGES: i64 = 16;

/// How many reads per page of the file the delete may cost.
///
/// Measured at 1.3 (524 reads, 406 pages). The two orders this guards against
/// read 57 and 62 per page, so the bound catches a change of kind and nothing
/// smaller.
const READS_PER_PAGE: u64 = 4;

/// Returns a fresh database path in a directory of its own.
///
/// A directory rather than a file, removed whole: a database is a file plus
/// log segments named after it, and a segment left from an earlier run beside a
/// fresh file would be read as part of it.
///
/// @param name - the case, so two cases never share a file
fn scratch(name: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("delete-order")
        .join(name);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory.join("delete.db")
}

/// Runs a statement for its effect.
///
/// @param connection - the database
/// @param sql - the statement
fn exec(connection: &Connection<'_>, sql: &str) {
    connection
        .execute_batch(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
}

/// Runs a query and returns every row.
///
/// @param connection - the database
/// @param sql - the query
fn rows(connection: &Connection<'_>, sql: &str) -> Vec<Vec<OwnedDatum>> {
    connection
        .query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()))
}

/// Runs a query whose rows are one integer each, and returns them.
///
/// @param connection - the database
/// @param sql - the query
fn integers(connection: &Connection<'_>, sql: &str) -> Vec<i64> {
    rows(connection, sql)
        .iter()
        .map(|row| match row.first() {
            Some(OwnedDatum::Int(value)) => *value,
            other => panic!("{sql} returned {other:?} where an integer was expected"),
        })
        .collect()
}

/// Returns the plan the engine chose for a statement, one line per step.
///
/// @param connection - the database
/// @param sql - the statement
fn plan(connection: &Connection<'_>, sql: &str) -> String {
    rows(connection, &format!("EXPLAIN QUERY PLAN {sql}"))
        .iter()
        .filter_map(|row| match row.last() {
            Some(OwnedDatum::Text(text)) => Some(String::from_utf8_lossy(text).to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Builds `big(a INTEGER PRIMARY KEY, b TEXT, d INTEGER)` with `big_d ON
/// big(d)`, the shape of `story_large_table_nightly`, with `d = a % period`.
///
/// @param connection - the database
/// @param rows - how many rows to write
/// @param period - how often `d` repeats
fn build(connection: &Connection<'_>, rows: i64, period: i64) {
    exec(
        connection,
        "CREATE TABLE big(a INTEGER PRIMARY KEY, b TEXT, d INTEGER); CREATE INDEX big_d ON big(d); BEGIN;",
    );
    let mut at = 1i64;
    while at <= rows {
        let end = (at + 499).min(rows);
        let values: Vec<String> = (at..=end)
            .map(|key| format!("({key}, 'row-{key}-padding-padding', {})", key % period))
            .collect();
        exec(
            connection,
            &format!("INSERT INTO big VALUES {};", values.join(", ")),
        );
        at = end + 1;
    }
    exec(connection, "COMMIT;");
}

/// A delete whose keys come from a covering index scan reads each leaf of the
/// table and of the index about once, not once per deleted row.
#[test]
fn a_delete_in_index_order_reads_each_leaf_about_once() {
    let path = scratch("index-order");
    let database = Database::open_at(&path, 4_096, inillucent_engine::DEFAULT_FRAMES as usize)
        .expect("the database opens");
    let connection = database.session();
    exec(&connection, &format!("PRAGMA cache_size = {POOL_PAGES}"));
    build(&connection, ROWS, PERIOD);

    // **The case under test, checked rather than assumed.** A planner that
    // stopped choosing the covering index would hand the delete its keys in
    // table order, and this test would pass without testing anything.
    let delete = "DELETE FROM big WHERE a % 2 = 0";
    let chosen = plan(&connection, delete);
    assert!(
        chosen.contains("COVERING INDEX big_d"),
        "the keys of `{delete}` are no longer gathered from the covering index (`{chosen}`), \
         so this test no longer puts them out of table order"
    );
    let pages = integers(&connection, "PRAGMA page_count")
        .first()
        .copied()
        .unwrap_or(0);
    let pages = u64::try_from(pages).unwrap_or(0);
    assert!(
        pages > u64::try_from(POOL_PAGES).unwrap_or(0),
        "the file has {pages} pages, which the pool of {POOL_PAGES} could hold"
    );

    let before = database.cache_stats();
    let deleted = connection
        .execute(delete)
        .unwrap_or_else(|error| panic!("{delete}: {}", error.message()));
    let reads = database.cache_stats().reads.saturating_sub(before.reads);
    println!("delete order | {deleted} rows | {pages} pages | {reads} reads");

    assert_eq!(deleted, ROWS / 2, "every even key is deleted");
    assert!(
        reads <= pages.saturating_mul(READS_PER_PAGE),
        "deleting {deleted} rows read {reads} pages of a {pages} page file, more than \
         {READS_PER_PAGE} per page: the delete is visiting the table or `big_d` in an order \
         other than the one the tree holds (task-2077)"
    );
    let left = integers(&connection, "SELECT count(*) FROM big");
    assert_eq!(
        left.first().copied(),
        Some(ROWS / 2),
        "half the rows are left"
    );
    let odd_sum = (ROWS / 2) * (ROWS / 2);
    let sums = rows(&connection, "SELECT sum(a) FROM big");
    assert_eq!(
        sums.first().and_then(|row| row.first()),
        Some(&OwnedDatum::Int(odd_sum)),
        "the rows left are the odd keys"
    );
    let probe = integers(&connection, "SELECT count(*) FROM big WHERE d = 3");
    let expected = (1..=ROWS)
        .filter(|key| key % 2 == 1 && key % PERIOD == 3)
        .count();
    assert_eq!(
        probe.first().copied(),
        Some(expected as i64),
        "the index agrees with the table"
    );
    let check = rows(&connection, "PRAGMA integrity_check");
    assert_eq!(
        check.first().and_then(|row| row.first()),
        Some(&OwnedDatum::Text(b"ok".to_vec())),
        "integrity_check after the delete: {check:?}"
    );
}

/// With a trigger or `RETURNING`, rows go in the order the query produced them.
///
/// The reordering is only allowed where nothing can see it. A trigger sees the
/// rows one at a time and `RETURNING` lists them in the order they went, so
/// both have to see the order the plain `SELECT` returns, which here is index
/// order and not key order.
#[test]
fn a_trigger_and_returning_see_the_query_order() {
    let path = scratch("observable");
    let database = Database::open_at(&path, 4_096, inillucent_engine::DEFAULT_FRAMES as usize)
        .expect("the database opens");
    let connection = database.session();
    build(&connection, 60, 7);
    let order = integers(&connection, "SELECT a FROM big WHERE a % 2 = 0");
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_ne!(
        order, sorted,
        "the query returns the keys in key order, so this test cannot tell a reordered delete \
         from one that kept the query's order"
    );

    exec(
        &connection,
        "CREATE TABLE seen(n INTEGER PRIMARY KEY, a INTEGER);
         CREATE TRIGGER record AFTER DELETE ON big BEGIN INSERT INTO seen(a) VALUES (old.a); END;",
    );
    exec(
        &connection,
        "SAVEPOINT trial; DELETE FROM big WHERE a % 2 = 0;",
    );
    let fired = integers(&connection, "SELECT a FROM seen ORDER BY n");
    assert_eq!(
        fired, order,
        "the trigger saw the rows in the query's order"
    );
    exec(
        &connection,
        "ROLLBACK TO trial; RELEASE trial; DROP TRIGGER record;",
    );

    let returned = integers(&connection, "DELETE FROM big WHERE a % 2 = 0 RETURNING a");
    assert_eq!(
        returned, order,
        "RETURNING listed the rows in the query's order"
    );
}
