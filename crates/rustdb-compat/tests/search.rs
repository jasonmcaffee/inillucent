//! `rustdb_search`: the retrieval engine as a transactional SQL table.
//!
//! Invariant: a search result is as transactional as a row. An insert that is
//! rolled back leaves no trace in the ranking; an insert that commits is found
//! by the next query; a savepoint takes back exactly what it covered; and a
//! crash leaves the rows and the index agreeing with each other, because they
//! were recovered by the same recovery.
//!
//! There is no SQLite oracle in this file and there should not be. SQLite has
//! no equivalent of this module, and comparing it against FTS5 would be
//! comparing two different retrieval engines and calling the difference a bug.
//! What is checked instead is the contract the module declares: the ordering it
//! promises, the visibility rules, and the equivalence between the SQL door and
//! the direct engine underneath it.

use rustdb_compat::differential::{scratch, start_rustdb};
use rustdb_session::statement::{execute_batch, Statement};
use rustdb_session::Connection;
use rustdb_value::Value;

/// Where this suite's scratch databases live.
const AREA: &str = "task-1790/search";

/// Runs a statement for its effect.
fn exec(connection: &Connection, sql: &str) {
    execute_batch(connection, sql.as_bytes())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
}

/// Runs a statement, returning whatever error it produced.
fn try_exec(connection: &Connection, sql: &str) -> Result<(), String> {
    execute_batch(connection, sql.as_bytes()).map_err(|error| error.message().to_string())
}

/// Returns every row of a query, each column rendered as text.
fn rows(connection: &Connection, sql: &str) -> Vec<Vec<String>> {
    let mut statement = Statement::prepare(connection, sql.as_bytes())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()))
        .0;
    let mut out = Vec::new();
    while statement
        .step()
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()))
    {
        let width = statement.column_count();
        let mut row = Vec::with_capacity(width);
        for index in 0..width {
            row.push(render(&statement.value(index)));
        }
        out.push(row);
    }
    out
}

/// Returns the first column of every row.
fn column(connection: &Connection, sql: &str) -> Vec<String> {
    rows(connection, sql)
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect()
}

/// Renders one value as text.
fn render(value: &Value<'static>) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => format!("{number:.6}"),
        Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
        Value::Blob(blob) => format!("blob:{}", blob.raw().len()),
    }
}

/// The corpus every test in this file searches.
///
/// Small and deliberately overlapping: three documents about eligibility, one
/// about something else, and one that shares a word with the first three
/// without answering the same question. That is what makes an ordering
/// meaningful rather than an accident of there being one match.
const CORPUS: &[(i64, &str, &str)] = &[
    (
        1,
        "Offer eligibility",
        "who qualifies for the launch discount offer",
    ),
    (
        2,
        "Discount rules",
        "the discount applies to eligible accounts only",
    ),
    (
        3,
        "Eligible accounts",
        "an account is eligible when it has been open a year",
    ),
    (4, "Weather", "the forecast for tomorrow is rain and wind"),
    (
        5,
        "Launch notes",
        "the launch shipped on a Tuesday with no discount",
    ),
];

/// Creates a lexical-only search table and fills it.
fn seed(connection: &Connection) {
    exec(
        connection,
        "CREATE VIRTUAL TABLE docs USING rustdb_search(title, body)",
    );
    for (id, title, body) in CORPUS {
        exec(
            connection,
            &format!("INSERT INTO docs(rowid, title, body) VALUES ({id}, '{title}', '{body}')"),
        );
    }
}

/// A search table is created, written and read like any other table.
#[test]
fn a_search_table_answers_a_match() {
    let connection = start_rustdb(AREA, "match");
    seed(&connection);
    let found = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'eligibility' ORDER BY rank",
    );
    assert!(
        found.contains(&"1".to_string()),
        "the document about eligibility is found: {found:?}"
    );
    assert!(
        !found.contains(&"4".to_string()),
        "the weather is not: {found:?}"
    );
}

/// The table-valued spelling and the `MATCH` spelling are the same query.
///
/// They have to be: an argument to a table-valued call *is* an equality on a
/// hidden column, so if the two ever disagreed one of them would be reaching
/// the module by a path the other does not.
#[test]
fn the_two_spellings_agree() {
    let connection = start_rustdb(AREA, "spellings");
    seed(&connection);
    let matched = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount' AND k = 3 ORDER BY rank",
    );
    let called = column(
        &connection,
        "SELECT rowid FROM docs('discount', 3) ORDER BY rank",
    );
    assert_eq!(matched, called);
    assert!(!matched.is_empty(), "the query found something");
}

/// `k` decides how deep the retrieval went, and it is not `LIMIT`.
#[test]
fn the_hit_count_is_the_modules_own_control() {
    let connection = start_rustdb(AREA, "depth");
    seed(&connection);
    let shallow = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 1",
    );
    let deep = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5",
    );
    assert_eq!(shallow.len(), 1);
    assert!(deep.len() > shallow.len(), "{deep:?}");
}

/// `ORDER BY rank` is best first, and the module says so rather than sorting.
#[test]
fn rank_orders_best_first() {
    let connection = start_rustdb(AREA, "rank");
    seed(&connection);
    let ranked = rows(
        &connection,
        "SELECT rowid, rank FROM docs WHERE docs MATCH 'eligible account' ORDER BY rank",
    );
    assert!(ranked.len() >= 2, "{ranked:?}");
    let scores: Vec<f64> = ranked
        .iter()
        .filter_map(|row| row.get(1))
        .filter_map(|text| text.parse::<f64>().ok())
        .collect();
    assert_eq!(scores.len(), ranked.len(), "every row has a rank");
    for pair in scores.windows(2) {
        if let [first, second] = pair {
            assert!(first <= second, "ranks ascend: {scores:?}");
        }
    }
}

/// A rolled-back insert leaves no trace in the ranking.
///
/// This is the acceptance criterion of the phase in one test: the search index
/// is undone by the same `ROLLBACK` that undoes the row, because it *is* rows.
#[test]
fn a_rolled_back_write_is_invisible_to_the_search() {
    let connection = start_rustdb(AREA, "rollback");
    seed(&connection);
    let before = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'",
    );
    assert!(before.is_empty(), "{before:?}");
    exec(&connection, "BEGIN");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (99, 'Trial', 'tirzepatide dosing schedule')",
    );
    let inside = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'",
    );
    assert_eq!(
        inside,
        vec!["99".to_string()],
        "a transaction sees its own writes"
    );
    exec(&connection, "ROLLBACK");
    let after = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'",
    );
    assert!(
        after.is_empty(),
        "the rollback took the hit back: {after:?}"
    );
    let stored = column(&connection, "SELECT count(*) FROM docs_content");
    assert_eq!(stored, vec![CORPUS.len().to_string()]);
}

/// A committed write is found by the next query, and by the next connection.
#[test]
fn a_committed_write_survives_reopening() {
    let path = scratch(AREA, "reopen", "rustdb");
    {
        let database = rustdb_session::connection::SessionDatabase::open(&path).expect("it opens");
        let connection = database.connect().expect("it connects");
        seed(&connection);
        exec(&connection, "BEGIN");
        exec(
            &connection,
            "INSERT INTO docs(rowid, title, body) VALUES (99, 'Trial', 'tirzepatide dosing schedule')",
        );
        exec(&connection, "COMMIT");
    }
    let database = rustdb_session::connection::SessionDatabase::open(&path).expect("it reopens");
    let connection = database.connect().expect("it connects");
    let found = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'",
    );
    assert_eq!(found, vec!["99".to_string()]);
}

/// A savepoint takes back exactly what it covered and no more.
#[test]
fn a_savepoint_takes_back_what_it_covered() {
    let connection = start_rustdb(AREA, "savepoint");
    seed(&connection);
    exec(&connection, "BEGIN");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (98, 'Kept', 'semaglutide dosing schedule')",
    );
    exec(&connection, "SAVEPOINT sp1");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (99, 'Dropped', 'tirzepatide dosing schedule')",
    );
    exec(&connection, "ROLLBACK TO sp1");
    exec(&connection, "COMMIT");
    assert_eq!(
        column(
            &connection,
            "SELECT rowid FROM docs WHERE docs MATCH 'semaglutide'"
        ),
        vec!["98".to_string()]
    );
    assert!(column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'"
    )
    .is_empty());
}

/// A delete removes the row from the ranking, not only from the table.
#[test]
fn a_delete_removes_the_row_from_the_ranking() {
    let connection = start_rustdb(AREA, "delete");
    seed(&connection);
    exec(&connection, "DELETE FROM docs WHERE rowid = 1");
    let found = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'eligibility'",
    );
    assert!(!found.contains(&"1".to_string()), "{found:?}");
    assert_eq!(
        column(&connection, "SELECT count(*) FROM docs_content"),
        vec![(CORPUS.len() - 1).to_string()]
    );
}

/// An update re-indexes the row rather than leaving the old terms behind.
#[test]
fn an_update_reindexes_the_row() {
    let connection = start_rustdb(AREA, "update");
    seed(&connection);
    exec(
        &connection,
        "UPDATE docs SET body = 'the forecast is now sunshine' WHERE rowid = 4",
    );
    assert!(
        column(
            &connection,
            "SELECT rowid FROM docs WHERE docs MATCH 'rain'"
        )
        .is_empty(),
        "the old terms are gone"
    );
    assert_eq!(
        column(
            &connection,
            "SELECT rowid FROM docs WHERE docs MATCH 'sunshine'"
        ),
        vec!["4".to_string()]
    );
}

/// Every change one transaction made carries one commit sequence.
#[test]
fn one_transaction_publishes_one_commit_sequence() {
    let connection = start_rustdb(AREA, "sequence");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING rustdb_search(title, body, compact = 0)",
    );
    exec(&connection, "BEGIN");
    for (id, title, body) in CORPUS {
        exec(
            &connection,
            &format!("INSERT INTO docs(rowid, title, body) VALUES ({id}, '{title}', '{body}')"),
        );
    }
    exec(&connection, "COMMIT");
    exec(&connection, "BEGIN");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (6, 'Later', 'a later document')",
    );
    exec(&connection, "COMMIT");
    let sequences = column(
        &connection,
        "SELECT DISTINCT commit_seq FROM docs_delta ORDER BY commit_seq",
    );
    assert_eq!(
        sequences,
        vec!["1".to_string(), "2".to_string()],
        "two transactions, two sequences"
    );
    let first = column(
        &connection,
        "SELECT count(*) FROM docs_delta WHERE commit_seq = 1",
    );
    assert_eq!(first, vec![CORPUS.len().to_string()]);
}

/// Compaction folds the log into a new generation and changes no answer.
#[test]
fn compaction_publishes_a_generation_and_changes_no_answer() {
    let connection = start_rustdb(AREA, "compact");
    seed(&connection);
    let before = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5 ORDER BY rank",
    );
    assert_eq!(
        column(
            &connection,
            "SELECT v FROM docs_state WHERE k = 'generation'"
        ),
        vec!["0".to_string()],
        "nothing has been compacted yet"
    );
    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    assert_eq!(
        column(
            &connection,
            "SELECT v FROM docs_state WHERE k = 'generation'"
        ),
        vec!["1".to_string()]
    );
    assert_eq!(
        column(&connection, "SELECT count(*) FROM docs_delta"),
        vec!["0".to_string()],
        "the log was folded in"
    );
    let after = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5 ORDER BY rank",
    );
    assert_eq!(before, after, "compaction changed no answer");
}

/// A generation is never removed by a write, only by asking.
#[test]
fn an_old_generation_survives_until_it_is_dropped() {
    let connection = start_rustdb(AREA, "generations");
    seed(&connection);
    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (6, 'More', 'another eligible account')",
    );
    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    let generations = column(
        &connection,
        "SELECT DISTINCT generation FROM docs_gen ORDER BY generation",
    );
    assert_eq!(
        generations,
        vec!["1".to_string(), "2".to_string()],
        "the superseded generation is still reachable"
    );
    exec(
        &connection,
        "INSERT INTO docs(docs) VALUES ('drop-old-generations')",
    );
    assert_eq!(
        column(
            &connection,
            "SELECT DISTINCT generation FROM docs_gen ORDER BY generation"
        ),
        vec!["2".to_string()]
    );
}

/// A rebuild reproduces the index from the rows alone.
#[test]
fn a_rebuild_reproduces_the_index_from_the_rows() {
    let connection = start_rustdb(AREA, "rebuild");
    seed(&connection);
    let before = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5 ORDER BY rank",
    );
    exec(&connection, "INSERT INTO docs(docs) VALUES ('rebuild')");
    let after = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5 ORDER BY rank",
    );
    assert_eq!(before, after);
}

/// A vector table finds the row nearest a query vector, exactly.
#[test]
fn a_vector_table_finds_the_nearest_row() {
    let connection = start_rustdb(AREA, "vector");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE points USING rustdb_search(label, dims = 4)",
    );
    // Unit vectors along each axis, so the nearest neighbour of an axis is
    // itself and the answer is not a matter of opinion.
    let axes: [(i64, &str, [f32; 4]); 4] = [
        (1, "x", [1.0, 0.0, 0.0, 0.0]),
        (2, "y", [0.0, 1.0, 0.0, 0.0]),
        (3, "z", [0.0, 0.0, 1.0, 0.0]),
        (4, "w", [0.0, 0.0, 0.0, 1.0]),
    ];
    for (id, label, vector) in axes {
        exec(
            &connection,
            &format!(
                "INSERT INTO points(rowid, label, vector) VALUES ({id}, '{label}', x'{}')",
                hex(&vector)
            ),
        );
    }
    let found = column(
        &connection,
        &format!(
            "SELECT label FROM points WHERE vector = x'{}' AND k = 1",
            hex(&[0.0, 0.0, 1.0, 0.0])
        ),
    );
    assert_eq!(found, vec!["z".to_string()]);
}

/// A vector of the wrong width is refused rather than resized.
#[test]
fn a_vector_of_the_wrong_width_is_refused() {
    let connection = start_rustdb(AREA, "vector-width");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE points USING rustdb_search(label, dims = 4)",
    );
    let refused = try_exec(
        &connection,
        &format!(
            "INSERT INTO points(rowid, label, vector) VALUES (1, 'x', x'{}')",
            hex(&[1.0, 0.0])
        ),
    );
    assert!(refused.is_err(), "{refused:?}");
}

/// A lexical-only table refuses a vector rather than ignoring it.
#[test]
fn a_lexical_table_refuses_a_vector() {
    let connection = start_rustdb(AREA, "no-vectors");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING rustdb_search(body)",
    );
    let refused = try_exec(
        &connection,
        &format!(
            "INSERT INTO docs(rowid, body, vector) VALUES (1, 'text', x'{}')",
            hex(&[1.0])
        ),
    );
    assert!(refused.is_err(), "{refused:?}");
}

/// The integrity check reports a shadow table that has been edited underneath
/// the module.
///
/// Shadow tables are ordinary tables, so anything that can write the database
/// can write one. The module cannot stop that; what it can do is notice.
#[test]
fn the_integrity_check_notices_an_edited_shadow_table() {
    let connection = start_rustdb(AREA, "integrity");
    seed(&connection);
    assert!(
        try_exec(
            &connection,
            "INSERT INTO docs(docs) VALUES ('integrity-check')"
        )
        .is_ok(),
        "a healthy index passes"
    );
    exec(&connection, "DELETE FROM docs_content WHERE id = 3");
    let complained = try_exec(
        &connection,
        "INSERT INTO docs(docs) VALUES ('integrity-check')",
    );
    assert!(complained.is_err(), "the check noticed: {complained:?}");
}

/// A command nobody implemented is an error, not a silent no-op.
#[test]
fn an_unknown_command_is_refused() {
    let connection = start_rustdb(AREA, "command");
    seed(&connection);
    assert!(try_exec(&connection, "INSERT INTO docs(docs) VALUES ('reticulate')").is_err());
}

/// The declaration a table was created with is what it reports.
#[test]
fn the_declaration_is_stored_and_read_back() {
    let connection = start_rustdb(AREA, "declaration");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING rustdb_search(body, dims = 8, mode = 'approximate')",
    );
    let stored = rows(&connection, "SELECT k, v FROM docs_config ORDER BY k");
    let pairs: Vec<(String, String)> = stored
        .into_iter()
        .filter_map(|row| match (row.first(), row.get(1)) {
            (Some(key), Some(value)) => Some((key.clone(), value.clone())),
            _ => None,
        })
        .collect();
    assert!(
        pairs.contains(&("dims".to_string(), "8".to_string())),
        "{pairs:?}"
    );
    assert!(pairs.contains(&("mode".to_string(), "approximate".to_string())));
    assert!(pairs.contains(&("metric".to_string(), "cosine".to_string())));
    assert!(pairs.contains(&("tokenize".to_string(), "porter".to_string())));
}

/// A distance this build cannot compute is refused when the table is made.
#[test]
fn an_unimplemented_metric_is_refused_at_create() {
    let connection = start_rustdb(AREA, "metric");
    assert!(try_exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING rustdb_search(body, metric = 'euclidean')"
    )
    .is_err());
}

/// A search table joins like a table, because it is one.
#[test]
fn a_search_table_joins() {
    let connection = start_rustdb(AREA, "join");
    seed(&connection);
    exec(
        &connection,
        "CREATE TABLE owner(id INTEGER PRIMARY KEY, who TEXT)",
    );
    exec(
        &connection,
        "INSERT INTO owner VALUES (1, 'ada'), (2, 'grace')",
    );
    let joined = rows(
        &connection,
        "SELECT owner.who FROM docs JOIN owner ON owner.id = docs.rowid \
         WHERE docs MATCH 'discount' AND k = 5 ORDER BY owner.who",
    );
    let names: Vec<String> = joined
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect();
    assert!(
        names.contains(&"ada".to_string()) || names.contains(&"grace".to_string()),
        "{names:?}"
    );
}

/// The auxiliary functions answer about the row the cursor is on.
#[test]
fn the_auxiliary_functions_describe_the_hit() {
    let connection = start_rustdb(AREA, "auxiliary");
    seed(&connection);
    let described = rows(
        &connection,
        "SELECT score(docs), confidence(docs), origin(docs) FROM docs \
         WHERE docs MATCH 'eligibility' AND k = 1",
    );
    let first = described.first().expect("one hit");
    assert_ne!(first.first().map(String::as_str), Some("NULL"));
    assert_eq!(first.get(2).map(String::as_str), Some("lexical"));
}

/// Returns a little-endian `f32` blob as hexadecimal, for an `x'...'` literal.
fn hex(vector: &[f32]) -> String {
    let mut out = String::new();
    for value in vector {
        for byte in value.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}
