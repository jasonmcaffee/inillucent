//! `trigger`: every timing and event, bodies holding each write form, `RAISE`,
//! a write to a virtual table, a scalar subquery value, and recursion.
//!
//! Invariant: **a trigger case fires its trigger with a statement that
//! touches more than one row, then reads every table the body could have
//! written.** A trigger's write to a virtual table was lost, and a scalar
//! subquery in a trigger body was refused, while each worked outside a
//! trigger (section 1.1); the body axis puts each write form there.

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::templates::frame::record;
use crate::statement_matrix::templates::scene::insert_rows;
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The trigger family.
pub fn family() -> Family {
    Family {
        name: "trigger",
        axes: vec![
            Axis::new(
                "event",
                &[
                    "before_insert",
                    "after_insert",
                    "before_update",
                    "after_update",
                    "update_of",
                    "before_delete",
                    "after_delete",
                    "instead_insert",
                    "instead_update",
                    "instead_delete",
                ],
            ),
            Axis::new(
                "body",
                &[
                    "insert_log",
                    "update_other",
                    "delete_other",
                    "raise_ignore",
                    "raise_abort",
                    "insert_fts",
                    "scalar_value",
                    "select",
                    "two_statements",
                ],
            ),
            Axis::new("when", &["none", "when"]),
            Axis::new("recursive", &["off", "on"]),
            Axis::new("fire", &["one_row", "many_rows", "select"]),
            Axis::new("data", &["one", "nulls", "duplicates", "mixed"]),
        ],
        allowed: |pick| {
            // A trigger on a view fires only for INSTEAD OF, and the insert
            // forms of firing belong to insert events.
            !(pick.get("fire") == Some("select")
                && pick
                    .get("event")
                    .is_some_and(|event| !event.ends_with("insert")))
        },
        build,
    }
}

/// The trigger's `NEW` or `OLD` row for an event.
fn row_of(event: &str) -> &'static str {
    if event.ends_with("delete") {
        "OLD"
    } else {
        "NEW"
    }
}

/// The trigger body for a body kind.
fn body(kind: &str, row: &str) -> String {
    match kind {
        "update_other" => format!("UPDATE o SET n = n + 1 WHERE o.k = {row}.k;"),
        "delete_other" => format!("DELETE FROM o WHERE o.k = {row}.k;"),
        "raise_ignore" => format!("SELECT RAISE(IGNORE) WHERE {row}.a IS NULL;"),
        "raise_abort" => format!("SELECT RAISE(ABORT, 'refused ' || {row}.k) WHERE {row}.a = 3;"),
        "insert_fts" => format!("INSERT INTO f(rowid, body) VALUES ({row}.k + 100, {row}.b);"),
        "scalar_value" => format!(
            "INSERT INTO log(e, k, a) VALUES ('scalar', {row}.k, (SELECT count(*) FROM o WHERE o.k <= {row}.k));"
        ),
        "select" => format!("SELECT {row}.k;"),
        "two_statements" => format!(
            "INSERT INTO log(e, k, a) VALUES ('one', {row}.k, {row}.a); UPDATE o SET n = n + 10 WHERE o.k = {row}.k;"
        ),
        _ => format!("INSERT INTO log(e, k, a) VALUES ('fired', {row}.k, {row}.a);"),
    }
}

/// The trigger's timing and event clause, and the table or view it is on.
fn timing(event: &str) -> (&'static str, &'static str) {
    match event {
        "before_insert" => ("BEFORE INSERT", "t"),
        "after_insert" => ("AFTER INSERT", "t"),
        "before_update" => ("BEFORE UPDATE", "t"),
        "after_update" => ("AFTER UPDATE", "t"),
        "update_of" => ("AFTER UPDATE OF a", "t"),
        "before_delete" => ("BEFORE DELETE", "t"),
        "after_delete" => ("AFTER DELETE", "t"),
        "instead_insert" => ("INSTEAD OF INSERT", "v"),
        "instead_update" => ("INSTEAD OF UPDATE", "v"),
        _ => ("INSTEAD OF DELETE", "v"),
    }
}

/// The statement that fires the trigger.
fn fire(event: &str, how: &str, on: &str) -> String {
    if event.ends_with("insert") {
        return match how {
            "many_rows" => format!(
                "INSERT INTO {on}(k, a, b) VALUES (10, 3, 'x'), (11, NULL, 'y'), (12, 5, 'z')"
            ),
            "select" => format!("INSERT INTO {on}(k, a, b) SELECT k + 20, a, b FROM o"),
            _ => format!("INSERT INTO {on}(k, a, b) VALUES (10, 3, 'x')"),
        };
    }
    let filter = if how == "one_row" { "k = 1" } else { "k <= 3" };
    if event.ends_with("delete") {
        format!("DELETE FROM {on} WHERE {filter}")
    } else {
        format!("UPDATE {on} SET a = 3, b = 'u' WHERE {filter}")
    }
}

/// Builds one trigger case.
fn build(pick: &Pick) -> Option<Case> {
    let event = pick.value("event");
    let (clause, on) = timing(event);
    let row = row_of(event);
    let when = if pick.value("when") == "when" {
        format!(" WHEN {row}.k > 1")
    } else {
        String::new()
    };
    let mut case = Case::new("", "");
    if event.starts_with("instead") {
        case.capabilities.push("writing_to_a_view".to_string());
    }
    case.setup
        .push(Record::ok("CREATE TABLE t(k INTEGER, a, b TEXT)"));
    case.setup
        .extend(insert_rows("t", "k, a, b", pick.value("data")));
    case.setup.push(Record::ok(
        "CREATE TABLE o(k INTEGER PRIMARY KEY, n INTEGER)",
    ));
    case.setup.push(Record::ok(
        "INSERT INTO o VALUES (1, 0), (2, 0), (3, 0), (10, 0)",
    ));
    case.setup
        .push(Record::ok("CREATE TABLE log(e TEXT, k, a)"));
    case.setup
        .push(Record::ok("CREATE VIRTUAL TABLE f USING fts5(body)"));
    case.setup
        .push(Record::ok("CREATE VIEW v AS SELECT k, a, b FROM t"));
    case.setup.push(Record::ok(format!(
        "CREATE TRIGGER tg {clause} ON {on} FOR EACH ROW{when} BEGIN {} END",
        body(pick.value("body"), row)
    )));
    if pick.value("recursive") == "on" {
        case.records
            .push(Record::ok("PRAGMA recursive_triggers = ON"));
    }
    case.records.push(record(
        fire(event, pick.value("fire"), on),
        Sort::RowSort,
        &[],
    ));
    for table in [
        "SELECT * FROM t",
        "SELECT * FROM o",
        "SELECT * FROM log",
        "SELECT rowid, body FROM f",
    ] {
        case.records
            .push(record(table.to_string(), Sort::RowSort, &[]));
    }
    Some(case)
}
