//! The virtual-table contract, exercised through the two modules that have no
//! storage of their own.
//!
//! Invariant: a virtual table is a table. Everything above the module - the
//! binder, the planner, the compiler, the machine - treats it as one, and the
//! only place the difference is visible is in where the rows come from. So the
//! tests here are ordinary SQL: a scan, a join, a `WHERE` the module consumes
//! and a `WHERE` it does not, an `ORDER BY` it can satisfy, and the arguments a
//! table-valued function takes.
//!
//! `json_each` and `json_tree` are compared against the pinned SQLite, which
//! has both. `generate_series` is in SQLite's shell rather than its library, so
//! it is checked against its own definition instead - which is why it is the
//! module the *contract* is tested with: it exercises `best_index` returning an
//! ordering, a plan number that changes with the constraints, and hidden
//! columns that are really arguments.

use inillucent_compat::differential::{compare, compare_queries, start_inillucent, Step};
use inillucent_tree::datum::OwnedDatum;

/// Where this suite's scratch databases live.
const AREA: &str = "vtab";

/// Runs a list of queries against both engines, failing on any difference.
fn check(name: &str, queries: &[&'static str]) {
    let compared = compare_queries(AREA, name, queries);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, queries.len(), "every query was compared");
}

/// Returns one column of every row of a query, as text.
fn column(name: &str, sql: &str) -> Vec<String> {
    let connection = start_inillucent(AREA, name);
    let mut statement = connection.prepare(sql).expect("it prepares");
    let mut rows = Vec::new();
    while statement.step().expect("it steps") {
        let value = statement.row().first().expect("one column");
        rows.push(match value {
            OwnedDatum::Int(number) => number.to_string(),
            OwnedDatum::Text(text) => String::from_utf8_lossy(text).into_owned(),
            OwnedDatum::Null => "NULL".to_string(),
            other => format!("{other:?}"),
        });
    }
    rows
}

/// A table-valued function's arguments constrain its hidden columns.
#[test]
fn a_table_valued_function_takes_its_arguments() {
    assert_eq!(
        column("series-args", "SELECT value FROM generate_series(1,5)"),
        vec!["1", "2", "3", "4", "5"]
    );
    assert_eq!(
        column("series-args", "SELECT value FROM generate_series(1,10,3)"),
        vec!["1", "4", "7", "10"]
    );
    assert_eq!(
        column("series-args", "SELECT value FROM generate_series(5,1)"),
        Vec::<String>::new()
    );
}

/// The same arguments written as a `WHERE` reach the module the same way.
///
/// This is the mechanism rather than a convenience: an argument *is* an
/// equality on a hidden column, so the two spellings have to produce the same
/// plan and the same rows.
#[test]
fn an_argument_and_a_where_are_the_same_thing() {
    assert_eq!(
        column(
            "series-where",
            "SELECT value FROM generate_series WHERE start = 1 AND stop = 5"
        ),
        vec!["1", "2", "3", "4", "5"]
    );
}

/// A predicate the module does not consume is still applied.
#[test]
fn an_unconsumed_predicate_is_still_tested() {
    assert_eq!(
        column(
            "series-recheck",
            "SELECT value FROM generate_series(1,10) WHERE value % 3 = 0"
        ),
        vec!["3", "6", "9"]
    );
}

/// A module that reports the ordering satisfies it without a sorter.
#[test]
fn a_module_can_satisfy_the_ordering() {
    assert_eq!(
        column(
            "series-order",
            "SELECT value FROM generate_series(1,5) ORDER BY value DESC"
        ),
        vec!["5", "4", "3", "2", "1"]
    );
    let plan = column(
        "series-order",
        "EXPLAIN QUERY PLAN SELECT value FROM generate_series(1,5) ORDER BY value DESC",
    );
    assert!(
        !plan.iter().any(|line| line.contains("TEMP B-TREE")),
        "the sorter should have been skipped: {plan:?}"
    );
}

/// A virtual table joins like any other table.
#[test]
fn a_virtual_table_joins() {
    let connection = start_inillucent(AREA, "join");
    connection
        .execute_batch("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (2),(4);")
        .expect("the schema is made");
    let mut statement = connection
        .prepare(
            "SELECT t.a, s.value FROM t JOIN generate_series(1,5) AS s ON s.value = t.a ORDER BY t.a",
        )
        .expect("it prepares");
    let mut rows = Vec::new();
    while statement.step().expect("it steps") {
        let pair = statement.row();
        rows.push(format!(
            "{:?}/{:?}",
            as_integer(pair.first()),
            as_integer(pair.get(1))
        ));
    }
    assert_eq!(rows, vec!["Some(2)/Some(2)", "Some(4)/Some(4)"]);
}

/// Returns an `OwnedDatum` as a plain integer, when it is one.
///
/// @param value - the column value
fn as_integer(value: Option<&OwnedDatum>) -> Option<i64> {
    match value {
        Some(OwnedDatum::Int(number)) => Some(*number),
        _ => None,
    }
}

/// `SELECT *` shows the visible columns and not the arguments.
///
/// **Stepped once before `columns()` is read.** This statement materialises
/// on its first `step` and learns its shape from what came back - see
/// `Statement::columns`'s own doc comment - so a caller that asks first gets
/// an empty list, which is what this test did before it drove the old
/// engine, where `sqlite3_column_name` answers straight after `prepare`.
/// `generate_series(1,2)` produces at least one row, so one `step` is enough
/// to learn the shape without changing what the assertion is about.
#[test]
fn hidden_columns_stay_hidden() {
    let connection = start_inillucent(AREA, "hidden");
    let mut statement = connection
        .prepare("SELECT * FROM generate_series(1,2)")
        .expect("it prepares");
    assert!(statement.step().expect("it steps"));
    assert_eq!(statement.columns(), &["value".to_string()]);
}

/// `json_each` and `json_tree`, against the engine that defines them.
#[test]
fn the_json_walkers_match_the_pinned_release() {
    check(
        "json-walk",
        &[
            "SELECT key, value, type, atom, id, parent, fullkey, path FROM json_each('{\"a\":1,\"b\":[2,3]}')",
            "SELECT key, value, type, atom, id, parent, fullkey, path FROM json_tree('{\"a\":1,\"b\":[2,3]}')",
            "SELECT key, value, type, fullkey, path FROM json_each('{\"a\":[1]}','$.a')",
            "SELECT key, value, type, fullkey, path FROM json_tree('{\"a\":[1]}','$.a')",
            "SELECT count(*) FROM json_each('7')",
            "SELECT key, value, type, fullkey, path FROM json_each('7')",
            "SELECT value FROM json_each('[10,20,30]')",
            "SELECT sum(value) FROM json_each('[1,2,3]')",
            "SELECT fullkey FROM json_tree('{\"a b\":{\"c\":1}}')",
            "SELECT count(*) FROM json_each(NULL)",
            "SELECT value FROM json_each('[1,2,3]') WHERE key > 0",
            "SELECT type, count(*) FROM json_tree('[1,\"x\",null,true,{\"a\":[]}]') GROUP BY type ORDER BY type",
        ],
    );
}

/// A walker joined to a real table, which is what these functions are for.
#[test]
fn a_walker_joins_a_real_table() {
    let steps = [
        Step::Exec("CREATE TABLE d(id INTEGER PRIMARY KEY, doc TEXT)"),
        Step::Exec("INSERT INTO d VALUES (1,'{\"tags\":[\"a\",\"b\"]}'),(2,'{\"tags\":[\"c\"]}')"),
        Step::Query(
            "SELECT d.id, j.value FROM d, json_each(d.doc,'$.tags') AS j ORDER BY d.id, j.value",
        ),
        Step::Query("SELECT count(*) FROM d, json_each(d.doc,'$.tags')"),
        Step::Query(
            "SELECT d.id FROM d WHERE EXISTS (SELECT 1 FROM json_each(d.doc,'$.tags') WHERE value = 'c')",
        ),
    ];
    let compared = compare(AREA, "walk-join", &steps);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, steps.len(), "every step was compared");
}

/// A module the build does not have is a statement error, not a crash.
///
/// Compared against the pinned release, because the interesting part is the
/// *code*: an application distinguishes "this build has no FTS5" from "this
/// file is corrupt", and it does it by the result code.
#[test]
fn an_unknown_module_is_refused() {
    let steps = [Step::Exec("CREATE VIRTUAL TABLE t USING nosuchmodule(a,b)")];
    let compared = compare(AREA, "unknown", &steps);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, steps.len(), "the refusal was compared");
}

/// A `CREATE VIRTUAL TABLE` naming an eponymous-only module is refused.
#[test]
fn an_eponymous_only_module_cannot_be_created() {
    let steps = [Step::Exec("CREATE VIRTUAL TABLE t USING json_each")];
    let compared = compare(AREA, "epon-create", &steps);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, steps.len(), "the refusal was compared");
}
