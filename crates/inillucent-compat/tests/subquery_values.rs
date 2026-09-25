//! A subquery used as a value on the write paths that do not plan their values.
//!
//! Invariant: **an uncorrelated subquery is answered before the statement that
//! holds it runs, on every path a value can reach.** A value list, an update's
//! assignments and a trigger body's statements are evaluated by the write path
//! directly rather than planned, so each one has to be folded on purpose - and
//! a slot that nothing folded is refused by the physical pass as "a correlated
//! subquery used as a value", whether the subquery is correlated or not.
//!
//! A consumer probing 0.1.8 reported `INSERT INTO probe (title, body) VALUES
//! ((SELECT title FROM shelf LIMIT 1), 'three')` refused that way. The same
//! insert into an ordinary table ran; it was refused into an FTS5 table, inside
//! a trigger body, and through a view's `INSTEAD OF` trigger. A trigger body was
//! refused a second way too: it was handed the firing statement's parameters,
//! and when that statement had folded a subquery of its own, the fold read the
//! filled table as "already done" and skipped the body's.

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// How many frames these databases get; they are tiny.
const FRAMES: usize = 256;

/// The page size, which is the engine's default.
const PAGE_SIZE: usize = 32_768;

/// Returns a fresh database holding the report's `shelf` table.
///
/// @param name - the test's name, for the scratch path
fn with_a_shelf(name: &str) -> ImportedDatabase {
    let area = workspace_root().join("_agent_output/subquery-values");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    let mut database =
        ImportedDatabase::create(path, PAGE_SIZE, FRAMES).expect("a fresh database is created");
    exec(&mut database, "CREATE TABLE shelf (title TEXT)");
    exec(&mut database, "INSERT INTO shelf VALUES ('dune')");
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

/// Returns the rows one query answers, each as text.
///
/// @param database - the connection
/// @param sql - the query
fn texts(database: &mut ImportedDatabase, sql: &str) -> Vec<Vec<String>> {
    database
        .run_with(sql, &Params::new())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.detail().unwrap_or_default()))
        .0
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    OwnedDatum::Text(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    OwnedDatum::Int(number) => number.to_string(),
                    other => format!("{other:?}"),
                })
                .collect()
        })
        .collect()
}

/// The report's statement, into an FTS5 table, stores the subquery's value.
///
/// `MATCH` on the stored word proves the value reached the module's index and
/// not only its content table.
#[test]
fn an_fts5_insert_stores_a_scalar_subquery_s_value() {
    let mut database = with_a_shelf("fts5-insert");
    exec(
        &mut database,
        "CREATE VIRTUAL TABLE probe USING fts5(title, body)",
    );
    exec(
        &mut database,
        "INSERT INTO probe (title, body) VALUES ((SELECT title FROM shelf LIMIT 1), 'three')",
    );
    assert_eq!(
        texts(
            &mut database,
            "SELECT title, body FROM probe WHERE probe MATCH 'dune'"
        ),
        vec![vec!["dune".to_string(), "three".to_string()]]
    );
}

/// An FTS5 update whose value is a scalar subquery writes that value.
#[test]
fn an_fts5_update_writes_a_scalar_subquery_s_value() {
    let mut database = with_a_shelf("fts5-update");
    exec(
        &mut database,
        "CREATE VIRTUAL TABLE probe USING fts5(title, body)",
    );
    exec(
        &mut database,
        "INSERT INTO probe (title, body) VALUES ('x', 'y')",
    );
    exec(
        &mut database,
        "UPDATE probe SET body = (SELECT upper(title) FROM shelf) WHERE rowid = 1",
    );
    assert_eq!(
        texts(
            &mut database,
            "SELECT body FROM probe WHERE probe MATCH 'DUNE'"
        ),
        vec![vec!["DUNE".to_string()]]
    );
}

/// A trigger body's `UPDATE ... SET x = (SELECT ...)` writes the running total.
///
/// This is the statement `compiled_chain_reuse.rs` used to record as refused.
#[test]
fn a_trigger_body_update_writes_a_scalar_subquery_s_value() {
    let mut database = with_a_shelf("trigger-update");
    exec(
        &mut database,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, value INTEGER NOT NULL, total INTEGER NOT NULL DEFAULT 0)",
    );
    exec(
        &mut database,
        "CREATE TRIGGER t_total AFTER INSERT ON t BEGIN \
           UPDATE t SET total = (SELECT sum(value) FROM t) WHERE id = new.id; \
         END",
    );
    exec(&mut database, "INSERT INTO t (id, value) VALUES (1, 10)");
    exec(&mut database, "INSERT INTO t (id, value) VALUES (2, 25)");
    assert_eq!(
        texts(&mut database, "SELECT id, total FROM t ORDER BY id"),
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "35".to_string()],
        ]
    );
}

/// A trigger fired by a statement that holds a subquery folds its own.
///
/// The firing insert's value is a subquery, so its parameters carry a filled
/// subquery table. The body's `INSERT ... VALUES ((SELECT count(*) ...))`
/// used to be skipped by the fold because of that table.
#[test]
fn a_trigger_fired_by_a_statement_with_a_subquery_folds_its_own() {
    let mut database = with_a_shelf("trigger-after-subquery");
    exec(&mut database, "CREATE TABLE log (n INTEGER, title TEXT)");
    exec(
        &mut database,
        "CREATE TRIGGER shelf_log AFTER INSERT ON shelf BEGIN \
           INSERT INTO log VALUES ((SELECT count(*) FROM shelf), new.title); \
         END",
    );
    exec(
        &mut database,
        "INSERT INTO shelf VALUES ((SELECT title || ' messiah' FROM shelf LIMIT 1))",
    );
    assert_eq!(
        texts(&mut database, "SELECT n, title FROM log"),
        vec![vec!["2".to_string(), "dune messiah".to_string()]]
    );
}

/// A `WHEN EXISTS (SELECT ...)` guard is answered when the firing statement
/// held a subquery of its own.
#[test]
fn a_when_guard_with_a_subquery_is_answered_after_a_firing_subquery() {
    let mut database = with_a_shelf("when-guard");
    exec(&mut database, "CREATE TABLE banned (title TEXT)");
    exec(&mut database, "INSERT INTO banned VALUES ('dune messiah')");
    exec(&mut database, "CREATE TABLE log (title TEXT)");
    exec(
        &mut database,
        "CREATE TRIGGER shelf_ban AFTER INSERT ON shelf \
           WHEN EXISTS (SELECT 1 FROM banned WHERE banned.title = new.title) BEGIN \
           INSERT INTO log VALUES (new.title); \
         END",
    );
    exec(
        &mut database,
        "INSERT INTO shelf VALUES ((SELECT title || ' messiah' FROM shelf LIMIT 1))",
    );
    exec(
        &mut database,
        "INSERT INTO shelf VALUES ((SELECT 'children of ' || min(title) FROM shelf))",
    );
    assert_eq!(
        texts(&mut database, "SELECT title FROM log"),
        vec![vec!["dune messiah".to_string()]],
        "the guard held for the banned title and not for the other"
    );
}

/// An `INSTEAD OF INSERT` trigger on a view, fired with a subquery value.
#[test]
fn a_view_insert_with_a_subquery_value_reaches_its_trigger() {
    let mut database = with_a_shelf("view-insert");
    exec(
        &mut database,
        "CREATE VIEW v AS SELECT title, 'x' AS body FROM shelf",
    );
    exec(
        &mut database,
        "CREATE TRIGGER v_insert INSTEAD OF INSERT ON v BEGIN \
           INSERT INTO shelf VALUES (new.title); \
         END",
    );
    exec(
        &mut database,
        "INSERT INTO v (title, body) VALUES ((SELECT max(title) || '!' FROM shelf), 'b')",
    );
    assert_eq!(
        texts(&mut database, "SELECT title FROM shelf ORDER BY title"),
        vec![vec!["dune".to_string()], vec!["dune!".to_string()]]
    );
}
