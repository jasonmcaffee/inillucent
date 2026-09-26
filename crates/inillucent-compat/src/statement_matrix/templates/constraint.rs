//! `constraint`: every constraint kind, broken by every kind of write, under
//! every conflict action, with foreign keys on and off and deferred.
//!
//! Invariant: **a case breaks its constraint in a statement that also writes
//! rows that do not break it, so the answer shows how much of the statement
//! each conflict action keeps.** `OR FAIL` keeps the rows before the bad one,
//! `ABORT` puts them back, `IGNORE` skips only the bad row and `REPLACE`
//! removes the row in the way; the counters after each are graded as well as
//! the rows (see `differential::observation_differences`).

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::templates::dml::TRANSACTIONS;
use crate::statement_matrix::templates::frame::record;
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The constraint family.
pub fn family() -> Family {
    Family {
        name: "constraint",
        axes: vec![
            Axis::new(
                "kind",
                &[
                    "not_null",
                    "unique",
                    "check",
                    "primary_key",
                    "fk_restrict",
                    "fk_cascade",
                    "fk_set_null",
                    "fk_set_default",
                    "fk_no_action",
                    "fk_deferred",
                ],
            ),
            Axis::new(
                "conflict",
                &["none", "ROLLBACK", "ABORT", "FAIL", "IGNORE", "REPLACE"],
            ),
            Axis::new(
                "operation",
                &["insert_one", "insert_many", "update", "delete_parent"],
            ),
            Axis::new("foreign_keys", &["on", "off", "deferred_pragma"]),
            Axis::new("transaction", TRANSACTIONS),
        ],
        allowed: |pick| {
            let foreign = pick.get("kind").is_some_and(|kind| kind.starts_with("fk"));
            // Deleting a parent row only means something for a foreign key.
            !(pick.get("operation") == Some("delete_parent")
                && pick.get("kind").is_some_and(|kind| !kind.starts_with("fk")))
                && !(pick.get("foreign_keys") == Some("deferred_pragma")
                    && pick.get("kind").is_some()
                    && !foreign)
        },
        build,
    }
}

/// The child table's declaration for a constraint kind.
fn child(kind: &str) -> &'static str {
    match kind {
        "not_null" => "CREATE TABLE c(k INTEGER, a NOT NULL, p INTEGER)",
        "unique" => "CREATE TABLE c(k INTEGER, a UNIQUE, p INTEGER)",
        "check" => "CREATE TABLE c(k INTEGER, a CHECK (a < 100), p INTEGER)",
        "primary_key" => "CREATE TABLE c(k INTEGER PRIMARY KEY, a, p INTEGER)",
        "fk_restrict" => "CREATE TABLE c(k INTEGER, a, p INTEGER REFERENCES pt(id) ON DELETE RESTRICT ON UPDATE RESTRICT)",
        "fk_cascade" => "CREATE TABLE c(k INTEGER, a, p INTEGER REFERENCES pt(id) ON DELETE CASCADE ON UPDATE CASCADE)",
        "fk_set_null" => "CREATE TABLE c(k INTEGER, a, p INTEGER REFERENCES pt(id) ON DELETE SET NULL)",
        "fk_set_default" => "CREATE TABLE c(k INTEGER, a, p INTEGER DEFAULT 1 REFERENCES pt(id) ON DELETE SET DEFAULT)",
        "fk_deferred" => "CREATE TABLE c(k INTEGER, a, p INTEGER REFERENCES pt(id) DEFERRABLE INITIALLY DEFERRED)",
        _ => "CREATE TABLE c(k INTEGER, a, p INTEGER REFERENCES pt(id))",
    }
}

/// The statement that breaks the constraint, among rows that do not.
fn breaking(kind: &str, operation: &str, conflict: &str) -> String {
    let or = if conflict == "none" {
        String::new()
    } else {
        format!(" OR {conflict}")
    };
    let bad = match kind {
        "not_null" => "(9, NULL, 1)",
        "unique" | "primary_key" => "(1, 1, 1)",
        "check" => "(9, 500, 1)",
        _ => "(9, 9, 99)",
    };
    match operation {
        "insert_many" => format!("INSERT{or} INTO c(k, a, p) VALUES (7, 7, 1), {bad}, (8, 8, 2)"),
        "update" => match kind {
            "not_null" => format!("UPDATE{or} c SET a = NULL WHERE k >= 2"),
            "unique" | "primary_key" => format!("UPDATE{or} c SET a = 1, k = 1 WHERE k >= 2"),
            "check" => format!("UPDATE{or} c SET a = a + 200 WHERE k >= 2"),
            _ => format!("UPDATE{or} c SET p = 99 WHERE k >= 2"),
        },
        "delete_parent" => "DELETE FROM pt WHERE id = 1".to_string(),
        _ => format!("INSERT{or} INTO c(k, a, p) VALUES {bad}"),
    }
}

/// Builds one constraint case.
fn build(pick: &Pick) -> Option<Case> {
    let kind = pick.value("kind");
    let mut case = Case::new("", "");
    case.setup.push(Record::ok(
        "CREATE TABLE pt(id INTEGER PRIMARY KEY, n TEXT)",
    ));
    case.setup
        .push(Record::ok("INSERT INTO pt VALUES (1, 'one'), (2, 'two')"));
    case.setup.push(Record::ok(child(kind)));
    case.setup.push(Record::ok(
        "INSERT INTO c(k, a, p) VALUES (1, 1, 1), (2, 2, 2), (3, 3, 1)",
    ));
    match pick.value("foreign_keys") {
        "on" => case.records.push(Record::ok("PRAGMA foreign_keys = ON")),
        "deferred_pragma" => {
            case.records.push(Record::ok("PRAGMA foreign_keys = ON"));
            case.records
                .push(Record::ok("PRAGMA defer_foreign_keys = ON"));
        }
        _ => {}
    }
    let (open, close): (Vec<&str>, Vec<&str>) = match pick.value("transaction") {
        "commit" => (vec!["BEGIN"], vec!["COMMIT"]),
        "rollback" => (vec!["BEGIN"], vec!["ROLLBACK"]),
        "savepoint_rollback" => (vec!["SAVEPOINT s"], vec!["ROLLBACK TO s", "RELEASE s"]),
        _ => (vec![], vec![]),
    };
    // The transaction statements are graded as queries so that a COMMIT
    // refused by a deferred foreign key is compared rather than expected.
    for statement in open {
        case.records
            .push(record(statement.to_string(), Sort::RowSort, &[]));
    }
    case.records.push(record(
        breaking(kind, pick.value("operation"), pick.value("conflict")),
        Sort::RowSort,
        &[],
    ));
    for statement in close {
        case.records
            .push(record(statement.to_string(), Sort::RowSort, &[]));
    }
    if pick.value("transaction") == "autocommit" {
        case.records.push(record(
            "PRAGMA foreign_key_check".to_string(),
            Sort::RowSort,
            &[],
        ));
    } else {
        // A deferred violation left by a failed COMMIT keeps the transaction
        // open; closing it here makes the reads below see the same state on
        // both engines whatever the COMMIT did.
        case.records
            .push(record("ROLLBACK".to_string(), Sort::RowSort, &[]));
    }
    case.records.push(record(
        "SELECT k, a, p FROM c".to_string(),
        Sort::RowSort,
        &[],
    ));
    case.records.push(record(
        "SELECT id, n FROM pt".to_string(),
        Sort::RowSort,
        &[],
    ));
    Some(case)
}
