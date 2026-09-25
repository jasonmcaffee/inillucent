//! The R-Tree module, compared against the pinned SQLite 3.53.4.
//!
//! Invariant: answering the same questions the pinned reference does is what
//! every scenario here asks, over `compare`'s statement-by-statement grading.
//!
//! **The two cross-open tests this file used to end with are gone.** They
//! built a database with one engine and handed the file to the other, because
//! the retired engine wrote SQLite's own on-disk format and the file was the
//! claim. The rearchitected engine makes no such claim: `docs/roadmap.md` and
//! `inillucent_engine::connect::Database::import`'s own doc comment say so -
//! file-format compatibility is not a goal of the rearchitecture, and a file
//! this engine writes is one only this engine reads. `Database::open` on a
//! SQLite file reports that neither meta page is readable, which is true and
//! is what it should say. So there is no "the pinned shell reads what this
//! engine wrote" left to assert, in either direction; a fixture built through
//! SQLite still reaches this engine, but only through `Database::import`,
//! which `crates/inillucent-compat/tests/differential/migrate_sqlite.rs` already covers.
//! `a_split_tree_answers_every_query` still proves the tree structure itself,
//! entirely against this engine.

use std::path::PathBuf;

use inillucent_compat::differential::{compare, sqlite_oracle, Step};
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

/// Where this suite's scratch databases live.
const AREA: &str = "rtree";

/// The schema every scenario starts from.
const SCHEMA: &[Step] = &[
    Step::Exec("CREATE VIRTUAL TABLE spots USING rtree(id, minX, maxX, minY, maxY)"),
    Step::Exec("INSERT INTO spots VALUES (1, 0.0, 1.0, 0.0, 1.0)"),
    Step::Exec("INSERT INTO spots VALUES (2, 5.0, 6.0, 5.0, 6.0)"),
    Step::Exec("INSERT INTO spots VALUES (3, 10.0, 11.0, 10.0, 11.0)"),
    Step::Exec("INSERT INTO spots VALUES (4, 15.0, 16.0, 0.0, 1.0)"),
    Step::Exec("INSERT INTO spots VALUES (5, -5.0, -4.0, -5.0, -4.0)"),
];

/// Runs the schema and then a list of steps, comparing every answer.
fn check(name: &str, steps: &[Step]) {
    let mut all = SCHEMA.to_vec();
    all.extend_from_slice(steps);
    let compared = compare(AREA, name, &all);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, all.len(), "every step was compared");
}

/// A table is created with the shadow tables the format needs.
#[test]
fn the_shadow_tables_are_the_documented_ones() {
    check(
        "shadow",
        &[Step::Query(
            "SELECT name FROM sqlite_schema WHERE name LIKE 'spots%' ORDER BY name",
        )],
    );
}

/// Every row comes back, and comes back the way it went in.
#[test]
fn every_row_round_trips() {
    check(
        "rows",
        &[
            Step::Query("SELECT id, minX, maxX, minY, maxY FROM spots ORDER BY id"),
            Step::Query("SELECT count(*) FROM spots"),
            Step::Query("SELECT id FROM spots WHERE id = 3"),
            Step::Query("SELECT id FROM spots WHERE id = 99"),
        ],
    );
}

/// A bounded query answers exactly the rows inside the box.
#[test]
fn a_bounded_query_answers_the_box() {
    check(
        "box",
        &[
            Step::Query("SELECT id FROM spots WHERE minX >= 4.0 AND maxX <= 12.0 ORDER BY id"),
            Step::Query("SELECT id FROM spots WHERE minX > 0.0 ORDER BY id"),
            Step::Query("SELECT id FROM spots WHERE maxY <= 1.0 ORDER BY id"),
            Step::Query(
                "SELECT id FROM spots WHERE minX >= 4.0 AND maxX <= 17.0 AND maxY <= 1.0 ORDER BY id",
            ),
            Step::Query("SELECT id FROM spots WHERE minX >= 1000.0 ORDER BY id"),
            Step::Query("SELECT id FROM spots WHERE minX = 5.0 ORDER BY id"),
        ],
    );
}

/// Writes go to the module and are visible to the next query.
#[test]
fn writes_reach_the_module() {
    check(
        "writes",
        &[
            Step::Exec("DELETE FROM spots WHERE id = 3"),
            Step::Query("SELECT id FROM spots ORDER BY id"),
            Step::Exec("UPDATE spots SET maxX = 99.0 WHERE id = 4"),
            Step::Query("SELECT id, maxX FROM spots ORDER BY id"),
            Step::Exec("INSERT INTO spots VALUES (9, 100.0, 101.0, 100.0, 101.0)"),
            Step::Query("SELECT id FROM spots WHERE minX >= 99.0 ORDER BY id"),
            Step::Exec("DELETE FROM spots"),
            Step::Query("SELECT count(*) FROM spots"),
        ],
    );
}

/// A tree deep enough to have split still answers every query.
///
/// The interesting number is the one that overflows a node: a tree that never
/// splits is one where the interior nodes, the parent table and the descent
/// have never run at all.
#[test]
fn a_split_tree_answers_every_query() {
    let path = inillucent_compat::differential::scratch(AREA, "deep", "inillucent");
    build_with_inillucent(&path, 400);
    assert_eq!(
        query_with_inillucent(&path, "SELECT count(*) FROM spots"),
        ["400"]
    );
    assert_eq!(
        query_with_inillucent(
            &path,
            "SELECT id FROM spots WHERE minX >= 100.0 AND maxX <= 104.0 ORDER BY id"
        ),
        ["100", "101", "102", "103"]
    );
    assert_eq!(
        query_with_inillucent(&path, "SELECT id FROM spots WHERE id = 250"),
        ["250"]
    );
    // The tree really did split: a tree of 400 rows in nodes of at most 51
    // cells cannot be one node, and the parent table records every node but the
    // root.
    let nodes = query_with_inillucent(&path, "SELECT count(*) FROM spots_node");
    assert!(
        nodes
            .first()
            .and_then(|count| count.parse::<i64>().ok())
            .unwrap_or(0)
            > 8,
        "the tree should have split: {nodes:?}"
    );
    {
        let database = open_existing(&path);
        let connection = database.session();
        connection
            .execute_batch("DELETE FROM spots WHERE id > 200")
            .expect("deletes");
    }
    assert_eq!(
        query_with_inillucent(&path, "SELECT count(*) FROM spots"),
        ["200"]
    );
    assert_eq!(
        query_with_inillucent(&path, "PRAGMA integrity_check"),
        ["ok"]
    );
}

/// Builds a tree with inillucent, from nothing.
fn build_with_inillucent(path: &PathBuf, rows: usize) {
    let database = Database::open(path).expect("inillucent opens");
    let connection = database.session();
    connection
        .execute_batch("CREATE VIRTUAL TABLE spots USING rtree(id, minX, maxX, minY, maxY)")
        .expect("the table is made");
    connection.execute_batch("BEGIN").expect("begins");
    for index in 1..=rows {
        let sql = format!(
            "INSERT INTO spots VALUES ({index}, {index}.0, {}.0, {index}.0, {}.0)",
            index + 1,
            index + 1
        );
        connection.execute_batch(&sql).expect("inserts");
    }
    connection.execute_batch("COMMIT").expect("commits");
}

/// Opens a database inillucent did not create.
fn open_existing(path: &PathBuf) -> Database {
    Database::open(path).expect("inillucent opens")
}

/// Runs one query with inillucent and returns its first column as text.
fn query_with_inillucent(path: &PathBuf, sql: &str) -> Vec<String> {
    let database = open_existing(path);
    let connection = database.session();
    let mut statement = connection.prepare(sql).expect("it prepares");
    let mut rows = Vec::new();
    while statement.step().expect("it steps") {
        rows.push(match statement.row().first() {
            Some(OwnedDatum::Int(number)) => number.to_string(),
            Some(OwnedDatum::Real(number)) => number.to_string(),
            Some(OwnedDatum::Text(text)) => String::from_utf8_lossy(text).into_owned(),
            other => format!("{other:?}"),
        });
    }
    rows
}

/// A coordinate too wide for a 32-bit float is rounded outwards, not to nearest.
///
/// A stored box promises that everything inside it is inside it. Rounding a
/// maximum to nearest can make it *smaller* than the value it bounds, and then
/// a query walks past the subtree holding the row - which is a lost row rather
/// than a slow one, and nothing about the file looks wrong.
///
/// The values here are the ones the performance scorecard found the two engines
/// disagreeing on, and they are chosen to land on both sides of the rounding:
/// 1103528600 rounds up by one step of the mantissa and 2061584312 by two,
/// because SQLite multiplies the double by one part in 2^23 before converting
/// rather than stepping to the next representable float.
#[test]
fn a_wide_coordinate_rounds_outwards() {
    check(
        "wide-coordinates",
        &[
            Step::Exec("INSERT INTO spots VALUES (10, 1103528590, 1103528600, 12345, 12355)"),
            Step::Exec("INSERT INTO spots VALUES (11, 2061584302, 2061584312, 1, 11)"),
            Step::Exec("INSERT INTO spots VALUES (12, -2061584312, -2061584302, -5.5, 5.5)"),
            Step::Query("SELECT id, minX, maxX, minY, maxY FROM spots ORDER BY id"),
            // The row has to be found by a query whose bounds are the values
            // that were inserted, which is the property the rounding exists for.
            Step::Query(
                "SELECT count(*) FROM spots WHERE minX >= 1103528590 AND maxX <= 1103528600",
            ),
            Step::Query("SELECT count(*) FROM spots WHERE minX > 2061584301 AND maxX < 2061584313"),
            Step::Query(
                "SELECT count(*) FROM spots WHERE minX >= -2061584312 AND maxX <= -2061584302",
            ),
        ],
    );
}

/// The oracle has to be present for the comparisons above to mean anything.
///
/// **This was the one `#[test]` in the workspace that could not fail
/// (task-1969, 4.7).** Its body printed a sentence and returned, so it passed
/// whether the oracle was there or not, while its name and its doc comment
/// both said it checked something. The row already declares
/// `requires = ["shell"]`, so a machine without the oracle is named by
/// `--strict` from the row rather than needing this case to stay quiet - which
/// makes the assertion the right form, not a stricter one.
#[test]
fn the_oracle_is_available() {
    assert!(
        sqlite_oracle().is_some(),
        "the pinned SQLite oracle is not built; run tools/sqlite-reference.{{ps1,sh}}"
    );
}
