//! `DELETE` and `UPDATE` with `ORDER BY`, `LIMIT` and `OFFSET`.
//!
//! Invariant: **a limited write changes exactly the rows a `SELECT` with the
//! same `WHERE`, `ORDER BY`, `LIMIT` and `OFFSET` would return, and no
//! others.** That is how SQLite defines the clause: with
//! `SQLITE_ENABLE_UPDATE_DELETE_LIMIT` it rewrites the statement to
//! `WHERE rowid IN (SELECT rowid ... ORDER BY ... LIMIT ...)`.
//!
//! These were refused in the pinned reference's words, because the build the
//! differential suites compare against is compiled without the option. A
//! consumer on macOS, whose `sqlite3` has it, reported the refusal as a gap:
//! `DELETE ... LIMIT 1000` in a loop is the ordinary way to trim a large table
//! without one large transaction. The parser already accepted the clause and
//! the executor already honoured `LIMIT`, but nothing bound `ORDER BY`, so
//! lifting the refusal alone would have deleted arbitrary rows. Every test
//! here asserts which rows changed, not only that the statement ran.
//!
//! No oracle: the pinned reference refuses every one of these statements, so
//! there is nothing to compare against. `semantics.rs` records that as a
//! difference.

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// How many frames these databases get; they are tiny.
const FRAMES: usize = 256;

/// The page size, which is the engine's default.
const PAGE_SIZE: usize = 32_768;

/// Returns a fresh database holding the five row table every test starts from.
///
/// `a` has a tie at 20 on purpose, so a test that orders by it has to add a
/// second key to be deterministic, which is what a real batch loop does too.
///
/// @param name - the test's name, for the scratch path
fn five_rows(name: &str) -> ImportedDatabase {
    let area = workspace_root().join("_agent_output/limited-writes");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    let mut database =
        ImportedDatabase::create(path, PAGE_SIZE, FRAMES).expect("a fresh database is created");
    exec(
        &mut database,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
    );
    exec(
        &mut database,
        "INSERT INTO t VALUES (1, 10, 'p'), (2, 20, 'q'), (3, 30, 'r'), (4, 20, 's'), (5, 50, 't')",
    );
    database
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

/// Returns the rows one statement answers.
///
/// @param database - the connection
/// @param sql - the statement
fn rows(database: &mut ImportedDatabase, sql: &str) -> Vec<Vec<OwnedDatum>> {
    database
        .run_with(sql, &Params::new())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.detail().unwrap_or_default()))
        .0
}

/// Returns the ids left in `t`, in order, as one comma separated string.
///
/// @param database - the connection
fn ids(database: &mut ImportedDatabase) -> String {
    text(
        database,
        "SELECT group_concat(id) FROM (SELECT id FROM t ORDER BY id)",
    )
}

/// Returns the single text value a query answers.
///
/// @param database - the connection
/// @param sql - a query answering one row of one column
fn text(database: &mut ImportedDatabase, sql: &str) -> String {
    match rows(database, sql).first().and_then(|row| row.first()) {
        Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        Some(OwnedDatum::Int(number)) => number.to_string(),
        other => panic!("{sql} answered {other:?}"),
    }
}

/// `ORDER BY ... DESC LIMIT 2` deletes the two largest rows and no others.
#[test]
fn an_ordered_limited_delete_removes_the_rows_the_order_puts_first() {
    let mut database = five_rows("ordered-delete");
    exec(&mut database, "DELETE FROM t ORDER BY a DESC LIMIT 2");
    assert_eq!(ids(&mut database), "1,2,4", "a = 50 and a = 30 are gone");
}

/// A `WHERE` with a `LIMIT` deletes one matching row and leaves the other.
#[test]
fn a_limit_takes_one_of_the_rows_the_where_matched() {
    let mut database = five_rows("where-limit");
    exec(&mut database, "DELETE FROM t WHERE a = 20 LIMIT 1");
    assert_eq!(
        text(&mut database, "SELECT count(*) FROM t WHERE a = 20"),
        "1",
        "one of the two rows with a = 20 is left"
    );
    assert_eq!(
        text(&mut database, "SELECT count(*) FROM t"),
        "4",
        "and nothing else was deleted"
    );
}

/// `OFFSET` passes over rows in the order before the limit counts.
#[test]
fn an_offset_skips_the_first_ordered_rows() {
    let mut database = five_rows("offset");
    exec(
        &mut database,
        "DELETE FROM t ORDER BY a, id LIMIT 2 OFFSET 1",
    );
    // In `a, id` order the rows are 1 (10), 2 (20), 4 (20), 3 (30), 5 (50).
    // Skipping one and taking two removes 2 and 4.
    assert_eq!(ids(&mut database), "1,3,5");
}

/// `UPDATE ... ORDER BY ... LIMIT 1` changes only the row the order puts first.
#[test]
fn an_ordered_limited_update_changes_one_row() {
    let mut database = five_rows("ordered-update");
    exec(
        &mut database,
        "UPDATE t SET b = 'z' ORDER BY a DESC LIMIT 1",
    );
    assert_eq!(
        text(
            &mut database,
            "SELECT group_concat(b) FROM (SELECT b FROM t ORDER BY id)"
        ),
        "p,q,r,s,z",
        "only the row with a = 50 changed"
    );
}

/// `UPDATE ... FROM` with a limit changes the right rows with the joined values.
///
/// The keys query for an `UPDATE ... FROM` is built separately from the plain
/// one, because it projects the assigned values beside each key, so the order
/// has to reach it on its own.
#[test]
fn a_limited_update_from_writes_the_joined_values_into_the_ordered_rows() {
    let mut database = five_rows("update-from");
    exec(&mut database, "CREATE TABLE s (a INTEGER, label TEXT)");
    exec(
        &mut database,
        "INSERT INTO s VALUES (10, 'ten'), (20, 'twenty'), (30, 'thirty'), (50, 'fifty')",
    );
    exec(
        &mut database,
        "UPDATE t SET b = s.label FROM s WHERE s.a = t.a ORDER BY t.a DESC, t.id LIMIT 2",
    );
    assert_eq!(
        text(
            &mut database,
            "SELECT group_concat(b) FROM (SELECT b FROM t ORDER BY id)"
        ),
        "p,q,thirty,s,fifty"
    );
}

/// The batch loop the clause exists for deletes every flagged row and no other.
///
/// `DELETE ... WHERE flag = 1 LIMIT 3`, repeated until `changes()` is zero,
/// against a table of twenty rows with eleven flagged. Four passes: three,
/// three, three, two, and a fifth that changes nothing.
#[test]
fn the_batched_delete_loop_removes_every_flagged_row() {
    let mut database = five_rows("batch-loop");
    exec(
        &mut database,
        "CREATE TABLE big (id INTEGER PRIMARY KEY, flag INTEGER)",
    );
    for id in 1..=20 {
        let flag = i32::from(id % 2 == 1 || id == 20);
        exec(
            &mut database,
            &format!("INSERT INTO big VALUES ({id}, {flag})"),
        );
    }
    let mut passes = 0;
    loop {
        let outcome = database
            .execute_any("DELETE FROM big WHERE flag = 1 LIMIT 3", &Params::new())
            .unwrap_or_else(|error| {
                panic!("the batch delete: {}", error.detail().unwrap_or_default())
            });
        passes += 1;
        assert!(
            outcome.changes.rows <= 3,
            "one pass deleted {} rows",
            outcome.changes.rows
        );
        if outcome.changes.rows == 0 {
            break;
        }
        assert!(passes < 10, "the loop did not finish");
    }
    assert_eq!(
        passes, 5,
        "four passes that deleted rows and one that did not"
    );
    assert_eq!(
        text(&mut database, "SELECT count(*) FROM big WHERE flag = 1"),
        "0"
    );
    assert_eq!(
        text(&mut database, "SELECT count(*) FROM big"),
        "9",
        "the nine unflagged rows are all still there"
    );
}

/// `RETURNING` on a limited delete returns the deleted rows.
#[test]
fn returning_reports_the_rows_a_limited_delete_removed() {
    let mut database = five_rows("returning");
    let sql = "DELETE FROM t RETURNING id ORDER BY a DESC LIMIT 2";
    let mut returned = database
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.detail().unwrap_or_default()))
        .rows;
    returned.sort_by_key(|row| match row.first() {
        Some(OwnedDatum::Int(id)) => *id,
        _ => 0,
    });
    assert_eq!(
        returned,
        vec![vec![OwnedDatum::Int(3)], vec![OwnedDatum::Int(5)]]
    );
    assert_eq!(ids(&mut database), "1,2,4");
}

/// `ORDER BY` without `LIMIT` is refused in SQLite's words.
///
/// A build compiled with the option refuses it, because sorting the rows a
/// statement changes every one of changes nothing.
#[test]
fn an_order_with_nothing_to_limit_is_refused() {
    let mut database = five_rows("order-without-limit");
    for (sql, statement) in [
        ("DELETE FROM t ORDER BY a", "DELETE"),
        ("UPDATE t SET b = 'x' ORDER BY a", "UPDATE"),
    ] {
        let refused = database
            .execute_any(sql, &Params::new())
            .expect_err("an ORDER BY with no LIMIT is refused");
        let detail = refused.detail().unwrap_or_default();
        assert!(
            detail.contains(&format!("ORDER BY without LIMIT on {statement}")),
            "{sql}: {detail}"
        );
    }
    assert_eq!(ids(&mut database), "1,2,3,4,5", "and nothing was deleted");
}

/// A limited delete on an FTS5 table deletes the number of rows it names.
///
/// A module's rows are found by a query of their own, which is built apart
/// from the ordinary one, so the order and the limit have to reach it too.
#[test]
fn a_limited_delete_reaches_a_virtual_table() {
    let mut database = five_rows("fts5");
    exec(&mut database, "CREATE VIRTUAL TABLE f USING fts5(body)");
    exec(
        &mut database,
        "INSERT INTO f(body) VALUES ('one apple'), ('two apples'), ('three apples')",
    );
    exec(&mut database, "DELETE FROM f ORDER BY rowid DESC LIMIT 2");
    assert_eq!(
        text(&mut database, "SELECT group_concat(body) FROM f"),
        "one apple",
        "the two highest rowids are gone"
    );
}

/// A limited delete through a view's `INSTEAD OF` trigger fires once per kept row.
#[test]
fn a_limited_delete_on_a_view_fires_its_trigger_for_the_kept_rows_only() {
    let mut database = five_rows("view");
    exec(&mut database, "CREATE VIEW v AS SELECT id, a FROM t");
    exec(
        &mut database,
        "CREATE TRIGGER v_delete INSTEAD OF DELETE ON v BEGIN DELETE FROM t WHERE id = old.id; END",
    );
    exec(
        &mut database,
        "DELETE FROM v WHERE a >= 20 ORDER BY a DESC LIMIT 2",
    );
    assert_eq!(ids(&mut database), "1,2,4");
}
