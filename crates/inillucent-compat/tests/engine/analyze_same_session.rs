//! `ANALYZE` on a connection that stays open, and the plan it is supposed to change.
//!
//! Invariant: **the connection that ran `ANALYZE` plans against the statistics it
//! just wrote.** Not the next connection; this one, immediately, with no reopen.
//!
//! The defect this file exists for (task-1946, H1): `analyze` in
//! `crates/inillucent-engine/src/analyze.rs` ended with `clear_stat1`,
//! `write_stat1` and `seal`, and never called `refresh_catalog` - which is the
//! only path to `republish_statistics`. Every other schema writing directive
//! ends with it. So the rows reached the file and the running connection kept
//! the catalog snapshot it took before they existed: every table's row count
//! stayed `DEFAULT_ROWS`, 1,048,576, and every index's `prefix_rows` stayed
//! `Some([])`, until the database was opened again.
//!
//! **The suite could not see it**, which is why it survived three reviews: every
//! case in `analyze_reopen.rs` reopens before it reads anything back, so all of
//! them measured the plan a *new* connection makes. What is asserted here is the
//! one thing those cannot ask - that the two connections agree.
//!
//! A user hits it the ordinary way: a long-lived process opens a database, runs
//! `ANALYZE` because its tables have grown, and plans every query for the rest of
//! its life against a guess of a million rows. Nothing fails, and nothing says so.
//!
//! **The schema is chosen so the plan discriminates.** Not every skew does: a
//! six hundred row table joined against a six row one is planned identically with
//! and without statistics, because the planner reaches the same order from the
//! index shapes alone. Two thousand rows against three is the ratio that moves
//! it, and the first assertion below is that it moved - a test whose fixture the
//! planner treats the same either way would report green with the fix reverted,
//! which is rule 1.5 of `tests/inillucent-testing-tdd.md`.

use std::path::PathBuf;

use inillucent_compat::facade::{Connection, Database};
use inillucent_value::Value;

/// Returns a scratch path nothing else in this file uses.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("analyze-same-session");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{tag}.rdb"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Renders one value the way the other planner tests do.
///
/// @param value - the value a row held
fn render(value: &Value<'static>) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(integer) => format!("int:{integer}"),
        Value::Real(real) => format!("real:{real:?}"),
        Value::Text(text) => format!("text:{}", String::from_utf8_lossy(&text.utf8_bytes())),
        Value::Blob(blob) => format!("blob:{}", blob.raw().len()),
    }
}

/// Runs a statement, returning its rows or its failure.
///
/// @param connection - the open connection
/// @param sql - the statement
fn run(connection: &Connection, sql: &str) -> Result<Vec<String>, String> {
    let mut statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(reason) => return Err(reason.message().to_string()),
    };
    let mut rows = Vec::new();
    loop {
        match statement.step() {
            Ok(true) => rows.push(
                statement
                    .row()
                    .iter()
                    .map(render)
                    .collect::<Vec<String>>()
                    .join("|"),
            ),
            Ok(false) => break,
            Err(reason) => return Err(reason.message().to_string()),
        }
    }
    Ok(rows)
}

/// Runs a script, asserting every statement succeeds.
///
/// @param connection - the open connection
/// @param script - the statements, in order
fn run_all(connection: &Connection, script: &[&str]) {
    for sql in script {
        run(connection, sql).unwrap_or_else(|reason| panic!("{sql}: {reason}"));
    }
}

/// Returns the `EXPLAIN QUERY PLAN` detail lines for a statement.
///
/// @param connection - the open connection
/// @param sql - the statement to explain
fn plan(connection: &Connection, sql: &str) -> Vec<String> {
    let explained = format!("EXPLAIN QUERY PLAN {sql}");
    run(connection, &explained)
        .unwrap_or_else(|reason| panic!("{explained}: {reason}"))
        .iter()
        .map(|row| {
            row.rsplit_once("text:")
                .map(|(_, detail)| detail.to_string())
                .unwrap_or_else(|| row.clone())
        })
        .collect()
}

/// The query whose plan the statistics decide.
const QUERY: &str = "SELECT count(*) FROM wide JOIN narrow ON wide.tag = narrow.tag";

/// Builds the skew the planner reacts to: two thousand rows against three.
///
/// `wide` holds fifty distinct tags over two thousand rows; `narrow` holds three
/// rows, one each for the first three of those tags. Driving the join from
/// `narrow` is three searches of an index over two thousand rows; driving it from
/// `wide` is two thousand searches of an index over three. Without statistics
/// both tables are `DEFAULT_ROWS`, so the planner takes them in the written
/// order and drives from `wide`.
///
/// @param connection - the open connection
fn build(connection: &Connection) {
    run_all(
        connection,
        &[
            "CREATE TABLE wide (id INTEGER PRIMARY KEY, tag TEXT)",
            "CREATE TABLE narrow (id INTEGER PRIMARY KEY, tag TEXT)",
            "CREATE INDEX wide_tag ON wide (tag)",
            "CREATE INDEX narrow_tag ON narrow (tag)",
            "BEGIN",
        ],
    );
    for row in 0..2000 {
        run_all(
            connection,
            &[format!("INSERT INTO wide VALUES ({row}, 't{}')", row % 50).as_str()],
        );
    }
    for row in 0..3 {
        run_all(
            connection,
            &[format!("INSERT INTO narrow VALUES ({row}, 't{row}')").as_str()],
        );
    }
    run_all(connection, &["COMMIT"]);
}

/// The plan the connection that ran `ANALYZE` makes is the plan a fresh
/// connection makes, with no reopen in between.
///
/// **Both halves are needed.** The reopened plan on its own proves nothing about
/// the running connection, and the same-session plan on its own could be right by
/// accident on a schema whose plan the statistics do not change. So the test
/// first establishes that the statistics *do* change the plan, and only then asks
/// whether the connection that wrote them agrees.
#[test]
fn analyze_changes_the_plan_on_the_connection_that_ran_it() {
    let path = scratch("join-order");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    build(&connection);

    let answer_before = run(&connection, QUERY).expect("the query answers");
    let plan_before = plan(&connection, QUERY);

    run_all(&connection, &["ANALYZE"]);
    let answer_after = run(&connection, QUERY).expect("the query answers");
    let plan_same_session = plan(&connection, QUERY);

    drop(connection);
    drop(database);

    let reopened = Database::open(&path).expect("the database reopens");
    let fresh = reopened.session().expect("the connection opens");
    let plan_reopened = plan(&fresh, QUERY);
    let answer_reopened = run(&fresh, QUERY).expect("the query answers");

    // Statistics are a hint and never an answer, so all three agree on the rows:
    // forty rows of `wide` carry each of `narrow`'s three tags.
    assert_eq!(answer_before, vec!["int:120".to_string()]);
    assert_eq!(answer_after, answer_before, "ANALYZE changed the answer");
    assert_eq!(
        answer_reopened, answer_before,
        "the reopen changed the answer"
    );

    // The statistics change the plan at all, on this schema. Without this the
    // assertion below could pass on a fixture the planner treats the same either
    // way, which is the failure `tests/inillucent-testing-tdd.md` rule 1.5 names.
    assert_ne!(
        plan_before, plan_reopened,
        "the statistics changed nothing, so this schema cannot show H1 either way: {plan_before:?}"
    );

    // And the connection that wrote them plans against them.
    assert_eq!(
        plan_same_session, plan_reopened,
        "ANALYZE wrote statistics the connection that ran it does not read.\n\
         before ANALYZE:  {plan_before:?}\n\
         same session:    {plan_same_session:?}\n\
         after a reopen:  {plan_reopened:?}"
    );

    // Named rather than only compared, so a change in the planner's wording says
    // which plan was expected: three rows of `narrow` driving a search of `wide`
    // beats two thousand rows of `wide` driving a search of three.
    assert!(
        plan_same_session
            .first()
            .is_some_and(|line| line.contains("narrow")),
        "{plan_same_session:?}"
    );
    assert!(
        plan_before
            .first()
            .is_some_and(|line| line.contains("wide")),
        "the unanalysed plan was already the analysed one: {plan_before:?}"
    );
}

/// `ANALYZE` of one named table leaves the other tables' statistics in the
/// running connection's catalog.
///
/// The narrower form of the same defect: `refresh_catalog` rebuilds the whole
/// snapshot, so a directive that measured one table has to republish all of it
/// or the rows an earlier `ANALYZE` wrote for the others are dropped from the
/// connection even though they are still on disk.
#[test]
fn analyzing_one_table_keeps_the_other_tables_statistics() {
    let path = scratch("one-table");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    build(&connection);

    run_all(&connection, &["ANALYZE"]);
    let plan_after_all = plan(&connection, QUERY);
    run_all(&connection, &["ANALYZE narrow"]);
    let plan_after_one = plan(&connection, QUERY);

    assert_eq!(
        plan_after_all, plan_after_one,
        "`ANALYZE narrow` dropped the statistics `ANALYZE` had written for `wide`"
    );

    // Both tables still have rows in `sqlite_stat1`, which is what makes the
    // plan above the same plan rather than two guesses that happen to match.
    let stats = run(
        &connection,
        "SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx",
    )
    .expect("the statistics read back");
    assert!(
        stats.iter().any(|row| row.contains("wide_tag")),
        "{stats:?}"
    );
    assert!(
        stats.iter().any(|row| row.contains("narrow_tag")),
        "{stats:?}"
    );
}
