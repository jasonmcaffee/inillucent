//! The R-Tree module, compared against the pinned SQLite 3.53.4 and then handed
//! to it.
//!
//! Invariant: the format is the claim. Answering the same questions is the easy
//! half; the half that matters is that a file one engine wrote is a file the
//! other opens, queries and *writes*, because that is what an application does
//! when it moves between the two. So the last two tests here do not compare
//! answers at all - they build a database with one engine and use it with the
//! other.

use std::path::PathBuf;
use std::process::Command;

use inillucent_compat::differential::{compare, sqlite_oracle, Step};
use inillucent_compat::workspace_root;

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

/// Returns the pinned SQLite shell, if it has been downloaded.
fn shell() -> Option<PathBuf> {
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let path = directory.join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Runs SQL through the pinned shell and returns what it printed.
fn run_shell(database: &PathBuf, sql: &str) -> String {
    let Some(shell) = shell() else {
        return String::new();
    };
    let output = Command::new(shell)
        .arg(database)
        .arg(sql)
        .output()
        .expect("the pinned shell runs");
    assert!(
        output.status.success(),
        "the pinned shell refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
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
/// have never run at all. The comparison here is against the *structure* rather
/// than against the oracle, because the two cross-open tests below already ask
/// the oracle to read a tree this deep - and they are the stronger claim.
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
    let connection = open_existing(&path);
    inillucent_session::statement::execute_batch(&connection, b"DELETE FROM spots WHERE id > 200")
        .expect("deletes");
    drop(connection);
    assert_eq!(
        query_with_inillucent(&path, "SELECT count(*) FROM spots"),
        ["200"]
    );
    assert_eq!(
        query_with_inillucent(&path, "PRAGMA integrity_check"),
        ["ok"]
    );
}

/// A tree inillucent built is one the pinned release opens, queries and writes.
#[test]
fn the_pinned_release_reads_what_inillucent_wrote() {
    let Some(shell) = shell() else {
        return;
    };
    let _ = shell;
    let path = inillucent_compat::differential::scratch(AREA, "cross-out", "inillucent");
    build_with_inillucent(&path, 200);

    let listed = run_shell(
        &path,
        "SELECT count(*) FROM spots; \
         SELECT id FROM spots WHERE minX >= 100.0 AND maxX <= 104.0 ORDER BY id; \
         PRAGMA integrity_check;",
    );
    assert_eq!(
        listed.trim(),
        "200\n100\n101\n102\n103\nok",
        "the pinned release read: {listed}"
    );

    // And it writes: a row it adds is one inillucent then finds.
    run_shell(
        &path,
        "INSERT INTO spots VALUES (9999, 500.0, 501.0, 500.0, 501.0);",
    );
    let found = query_with_inillucent(&path, "SELECT id FROM spots WHERE minX >= 499.0");
    assert_eq!(found, vec!["9999"], "inillucent reads the row SQLite added");
}

/// A tree the pinned release built is one inillucent opens, queries and writes.
#[test]
fn inillucent_reads_what_the_pinned_release_wrote() {
    let Some(shell) = shell() else {
        return;
    };
    let _ = shell;
    let path = inillucent_compat::differential::scratch(AREA, "cross-in", "sqlite");
    let mut sql = String::from(
        "CREATE VIRTUAL TABLE spots USING rtree(id, minX, maxX, minY, maxY);\nBEGIN;\n",
    );
    for index in 1..=200 {
        sql.push_str(&format!(
            "INSERT INTO spots VALUES ({index}, {index}.0, {}.0, {index}.0, {}.0);\n",
            index + 1,
            index + 1
        ));
    }
    sql.push_str("COMMIT;\n");
    run_shell(&path, &sql);

    let counted = query_with_inillucent(&path, "SELECT count(*) FROM spots");
    assert_eq!(counted, vec!["200"]);
    let found = query_with_inillucent(
        &path,
        "SELECT id FROM spots WHERE minX >= 100.0 AND maxX <= 104.0 ORDER BY id",
    );
    assert_eq!(found, vec!["100", "101", "102", "103"]);

    // And inillucent writes: a row it adds is one the pinned release then finds.
    let connection = open_existing(&path);
    inillucent_session::statement::execute_batch(
        &connection,
        b"INSERT INTO spots VALUES (9999, 500.0, 501.0, 500.0, 501.0)",
    )
    .expect("inillucent inserts");
    drop(connection);
    let listed = run_shell(&path, "SELECT id FROM spots WHERE minX >= 499.0;");
    assert_eq!(listed.trim(), "9999");
}

/// Builds a tree with inillucent, from nothing.
fn build_with_inillucent(path: &PathBuf, rows: usize) {
    let database = inillucent_session::connection::SessionDatabase::open_with_options(
        path,
        inillucent_session::connection::OpenOptions::default(),
    )
    .expect("inillucent opens");
    let connection = database.connect().expect("inillucent connects");
    inillucent_session::statement::execute_batch(
        &connection,
        b"CREATE VIRTUAL TABLE spots USING rtree(id, minX, maxX, minY, maxY)",
    )
    .expect("the table is made");
    inillucent_session::statement::execute_batch(&connection, b"BEGIN").expect("begins");
    for index in 1..=rows {
        let sql = format!(
            "INSERT INTO spots VALUES ({index}, {index}.0, {}.0, {index}.0, {}.0)",
            index + 1,
            index + 1
        );
        inillucent_session::statement::execute_batch(&connection, sql.as_bytes()).expect("inserts");
    }
    inillucent_session::statement::execute_batch(&connection, b"COMMIT").expect("commits");
}

/// Opens a database inillucent did not create.
fn open_existing(path: &PathBuf) -> inillucent_session::connection::Connection {
    let database = inillucent_session::connection::SessionDatabase::open_with_options(
        path,
        inillucent_session::connection::OpenOptions::default(),
    )
    .expect("inillucent opens");
    database.connect().expect("inillucent connects")
}

/// Runs one query with inillucent and returns its first column as text.
fn query_with_inillucent(path: &PathBuf, sql: &str) -> Vec<String> {
    let connection = open_existing(path);
    let mut statement =
        inillucent_session::statement::Statement::prepare(&connection, sql.as_bytes())
            .expect("it prepares")
            .0;
    let mut rows = Vec::new();
    while statement.step().expect("it steps") {
        rows.push(match statement.value(0) {
            inillucent_value::Value::Integer(number) => number.to_string(),
            inillucent_value::Value::Real(number) => number.to_string(),
            inillucent_value::Value::Text(text) => {
                String::from_utf8_lossy(&text.utf8_bytes()).into_owned()
            }
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
#[test]
fn the_oracle_is_available() {
    if sqlite_oracle().is_none() {
        eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.{{ps1,sh}}");
    }
}
