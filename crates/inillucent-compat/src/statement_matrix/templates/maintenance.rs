//! `maintenance`: `VACUUM`, `VACUUM INTO`, `ANALYZE` and `REINDEX` in each of
//! their forms, over every kind of table and index, and `EXPLAIN QUERY PLAN`.
//!
//! Invariant: **a maintenance statement is followed by the queries its
//! objects answer, and the case's reopen and integrity check read the file it
//! left.** A maintenance statement that answers `ok` is only half of the
//! question; the other half is that nothing it rebuilt answers differently.
//! Bare `REINDEX` failing on a database holding a `WITHOUT ROWID` table is the
//! escaped defect `8b9725ac` fixed, and the operation and table axes cross it.

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::templates::frame::record;
use crate::statement_matrix::templates::scene::insert_rows;
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The maintenance family.
pub fn family() -> Family {
    Family {
        name: "maintenance",
        axes: vec![
            Axis::new(
                "operation",
                &[
                    "vacuum",
                    "vacuum_into",
                    "analyze",
                    "analyze_table",
                    "analyze_index",
                    "reindex",
                    "reindex_table",
                    "reindex_index",
                    "reindex_collation",
                    "explain_query_plan",
                ],
            ),
            Axis::new(
                "table",
                &[
                    "rowid",
                    "without_rowid",
                    "strict",
                    "generated",
                    "fts5",
                    "temp",
                    "nocase_column",
                ],
            ),
            Axis::new(
                "index",
                &[
                    "none",
                    "plain",
                    "unique",
                    "partial",
                    "expression",
                    "collate",
                ],
            ),
            Axis::new("data", &["empty", "one", "duplicates", "mixed", "nocase"]),
        ],
        allowed: |pick| {
            !(pick.get("table") == Some("fts5") && pick.get("index").is_some_and(|index| index != "none"))
                && !(pick.get("table") == Some("fts5") && pick.get("data") == Some("mixed"))
                && !(matches!(pick.get("operation"), Some("analyze_index") | Some("reindex_index"))
                    && pick.get("index") == Some("none"))
                && !(pick.get("index") == Some("unique") && pick.get("data") == Some("duplicates"))
                // An index operation needs an index, and an FTS5 table has none.
                && !pick.forbids("operation", "table", &[("analyze_index", "fts5"), ("reindex_index", "fts5")])
        },
        build,
    }
}

/// The table's declaration.
fn table(kind: &str) -> String {
    match kind {
        "without_rowid" => {
            "CREATE TABLE t(k INTEGER PRIMARY KEY, a, b TEXT) WITHOUT ROWID".to_string()
        }
        "strict" => "CREATE TABLE t(k INTEGER, a ANY, b TEXT) STRICT".to_string(),
        "generated" => {
            "CREATE TABLE t(k INTEGER, a, b TEXT, g AS (lower(b)) VIRTUAL, h AS (a || b) STORED)"
                .to_string()
        }
        "fts5" => "CREATE VIRTUAL TABLE t USING fts5(k, a, b)".to_string(),
        "temp" => "CREATE TEMP TABLE t(k INTEGER, a, b TEXT)".to_string(),
        "nocase_column" => "CREATE TABLE t(k INTEGER, a TEXT COLLATE NOCASE, b TEXT)".to_string(),
        _ => "CREATE TABLE t(k INTEGER, a, b TEXT)".to_string(),
    }
}

/// The index statement for an index kind.
fn index(kind: &str, table: &str) -> Option<String> {
    let target = if table == "generated" { "g" } else { "a" };
    Some(match kind {
        "plain" => format!("CREATE INDEX ix ON t({target})"),
        "unique" => "CREATE UNIQUE INDEX ix ON t(k)".to_string(),
        "partial" => format!("CREATE INDEX ix ON t({target}) WHERE {target} IS NOT NULL"),
        "expression" => "CREATE INDEX ix ON t(lower(b))".to_string(),
        "collate" => format!("CREATE INDEX ix ON t({target} COLLATE NOCASE)"),
        _ => return None,
    })
}

/// The maintenance statement.
fn operation(kind: &str) -> String {
    match kind {
        "vacuum" => "VACUUM".to_string(),
        "vacuum_into" => "VACUUM INTO '%SCRATCH%/copy.db'".to_string(),
        "analyze" => "ANALYZE".to_string(),
        "analyze_table" => "ANALYZE t".to_string(),
        "analyze_index" => "ANALYZE ix".to_string(),
        "reindex" => "REINDEX".to_string(),
        "reindex_table" => "REINDEX t".to_string(),
        "reindex_index" => "REINDEX ix".to_string(),
        "reindex_collation" => "REINDEX NOCASE".to_string(),
        _ => "EXPLAIN QUERY PLAN SELECT k, a FROM t WHERE a > 1 ORDER BY b".to_string(),
    }
}

/// Builds one maintenance case.
fn build(pick: &Pick) -> Option<Case> {
    let kind = pick.value("table");
    let mut case = Case::new("", "");
    case.records.push(Record::ok(table(kind)));
    case.records
        .extend(insert_rows("t", "k, a, b", pick.value("data")));
    if let Some(sql) = index(pick.value("index"), kind) {
        case.records.push(Record::ok(sql));
    }
    // A second table WITHOUT ROWID beside the one under test, so a bare
    // REINDEX or ANALYZE meets one whatever the table axis is.
    case.records.push(Record::ok(
        "CREATE TABLE w(k INTEGER PRIMARY KEY, v TEXT) WITHOUT ROWID",
    ));
    case.records
        .push(Record::ok("INSERT INTO w VALUES (1, 'a'), (2, 'B')"));
    case.records.push(record(
        operation(pick.value("operation")),
        Sort::RowSort,
        &[],
    ));
    for read in [
        "SELECT k, a, b FROM t WHERE a IS NOT NULL",
        "SELECT k, a FROM t WHERE a = 'abc' OR a > 1",
        "SELECT count(*) FROM t",
        "SELECT k, v FROM w WHERE v > 'a'",
    ] {
        case.records
            .push(record(read.to_string(), Sort::RowSort, &[]));
    }
    if pick.value("operation").starts_with("analyze") {
        case.records.push(record(
            "SELECT tbl, idx FROM sqlite_stat1".to_string(),
            Sort::RowSort,
            &[],
        ));
    }
    Some(case)
}
