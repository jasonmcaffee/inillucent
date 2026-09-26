//! `transaction`: every way to open and close a transaction, around every
//! kind of write, on every kind of table, with a statement that fails in the
//! middle.
//!
//! Invariant: **each case reads the table inside the transaction and again
//! after it ends, and the case's reopen reads it a third time from the file,
//! so a rollback that answered correctly and left its rows on disk fails.**
//! `autocommit` is compared after every statement, which is what shows a
//! failed statement that ended a transaction it should not have.

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::templates::frame::record;
use crate::statement_matrix::templates::scene::relation;
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The transaction family.
pub fn family() -> Family {
    Family {
        name: "transaction",
        axes: vec![
            Axis::new(
                "begin",
                &["deferred", "immediate", "exclusive", "savepoint"],
            ),
            Axis::new(
                "write",
                &[
                    "insert",
                    "update",
                    "delete",
                    "create_table",
                    "drop_table",
                    "create_index",
                ],
            ),
            Axis::new(
                "end",
                &[
                    "commit",
                    "rollback",
                    "rollback_to",
                    "release",
                    "nested_rollback",
                    "failing_then_commit",
                ],
            ),
            Axis::new(
                "target",
                &["rowid", "without_rowid", "temp", "attached", "fts5"],
            ),
            Axis::new("data", &["empty", "one", "duplicates", "mixed"]),
        ],
        allowed: |pick| {
            !pick.forbids("target", "data", &[("fts5", "mixed")])
                && !pick.forbids("target", "write", &[("fts5", "create_index")])
                // ROLLBACK TO and RELEASE name a savepoint the case opened.
                && !(matches!(pick.get("end"), Some("rollback_to") | Some("release") | Some("nested_rollback"))
                    && pick.get("begin").is_some_and(|begin| begin != "savepoint"))
        },
        build,
    }
}

/// The write statement for a write kind on the target table.
fn write(kind: &str, table: &str, key: &str) -> String {
    match kind {
        "update" => format!("UPDATE {table} SET b = 'changed' WHERE {key} <= 2"),
        "delete" => format!("DELETE FROM {table} WHERE {key} <= 2"),
        "create_table" => "CREATE TABLE made(x)".to_string(),
        "drop_table" => "DROP TABLE side".to_string(),
        "create_index" => match table.strip_prefix("aux.") {
            Some(name) => format!("CREATE INDEX aux.made_ix ON {name}(b)"),
            None => format!("CREATE INDEX made_ix ON {table}(b)"),
        },
        _ => format!("INSERT INTO {table}({key}, a, b) VALUES (50, 'new', 'n'), (51, NULL, 'm')"),
    }
}

/// Builds one transaction case.
fn build(pick: &Pick) -> Option<Case> {
    let scene = relation(pick.value("target"), "none", pick.value("data"), "none");
    let table = scene.table.clone().unwrap_or_else(|| "t0".to_string());
    let key = if pick.value("target") == "fts5" {
        "rowid"
    } else {
        "k"
    };
    let mut case = Case::new("", "");
    case.setup = scene.setup.clone();
    case.setup.push(Record::ok("CREATE TABLE side(x)"));
    case.records = scene.prelude.clone();
    let open = match pick.value("begin") {
        "immediate" => "BEGIN IMMEDIATE",
        "exclusive" => "BEGIN EXCLUSIVE",
        "savepoint" => "SAVEPOINT outer_sp",
        _ => "BEGIN DEFERRED",
    };
    let read = format!("SELECT {key}, a, b FROM {table}");
    let mut add = |sql: String| case.records.push(record(sql, Sort::RowSort, &[]));
    add(open.to_string());
    add(write(pick.value("write"), &table, key));
    add(read.clone());
    let savepoint = pick.value("begin") == "savepoint";
    match pick.value("end") {
        "rollback" => add("ROLLBACK".to_string()),
        "rollback_to" => {
            add("ROLLBACK TO outer_sp".to_string());
            add(read.clone());
            add("RELEASE outer_sp".to_string());
        }
        "release" => add("RELEASE outer_sp".to_string()),
        "nested_rollback" => {
            add("SAVEPOINT inner_sp".to_string());
            add(format!("DELETE FROM {table}"));
            add("ROLLBACK TO inner_sp".to_string());
            add(read.clone());
            add("RELEASE outer_sp".to_string());
        }
        "failing_then_commit" => {
            add(format!("INSERT INTO {table}(nonexistent) VALUES (1)"));
            add(read.clone());
            add(if savepoint {
                "RELEASE outer_sp"
            } else {
                "COMMIT"
            }
            .to_string());
        }
        _ => add(if savepoint {
            "RELEASE outer_sp"
        } else {
            "COMMIT"
        }
        .to_string()),
    }
    add(read);
    add("SELECT name FROM sqlite_schema WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite%'".to_string());
    Some(case)
}
