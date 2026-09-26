//! The schema families: `ddl_table`, `ddl_index` and `ddl_view`.
//!
//! Invariant: **every schema change is read back three ways: the rows, the
//! engine's own description of the object (`PRAGMA table_info`,
//! `index_list`), and `sqlite_schema`, and then the file is reopened and
//! checked.** A schema change that answers `ok` and leaves a catalogue the
//! reopen reads differently is the defect these families are for.
//!
//! `sqlite_schema.sql` is not compared: SQLite stores the statement's text as
//! written and rewrites it on `ALTER TABLE`, and a difference in that text is
//! not a difference a caller's query sees. Its `type`, `name` and `tbl_name`
//! are compared.

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::properties::Property;
use crate::statement_matrix::templates::frame::record;
use crate::statement_matrix::templates::scene::{
    declared, insert_rows, insert_rows_typed, AFFINITIES, DATA,
};
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// Every schema family.
pub fn families() -> Vec<Family> {
    vec![ddl_table(), ddl_index(), ddl_view()]
}

/// A query record compared as a set of rows.
fn read(sql: impl Into<String>) -> Record {
    record(sql.into(), Sort::RowSort, &[])
}

/// The reads that describe a table after a change.
fn describe(table: &str) -> Vec<Record> {
    vec![
        read(format!("SELECT * FROM {table}")),
        read(format!("PRAGMA table_info({table})")),
        read("SELECT type, name, tbl_name FROM sqlite_schema"),
    ]
}

/// `ddl_table`: column constraints, table forms and every `ALTER TABLE`
/// action.
fn ddl_table() -> Family {
    Family {
        name: "ddl_table",
        axes: vec![
            Axis::new(
                "constraint",
                &[
                    "none",
                    "not_null",
                    "unique",
                    "check",
                    "default",
                    "collate",
                    "generated_virtual",
                    "generated_stored",
                    "primary_key",
                    "references",
                ],
            ),
            Axis::new(
                "form",
                &[
                    "plain",
                    "without_rowid",
                    "strict",
                    "temp",
                    "if_not_exists",
                    "as_select",
                ],
            ),
            Axis::new(
                "alter",
                &[
                    "none",
                    "add_column",
                    "add_column_default",
                    "rename_column",
                    "rename_table",
                    "drop_column",
                ],
            ),
            Axis::new("data", DATA),
            Axis::new("affinity", AFFINITIES),
        ],
        allowed: |pick| {
            // A table made by AS SELECT has no constraints to declare.
            !(pick.get("form") == Some("as_select") && pick.get("constraint").is_some_and(|c| c != "none"))
                // A WITHOUT ROWID table's key cannot also be the column under test's.
                && !pick.forbids("form", "constraint", &[("without_rowid", "primary_key")])
                // A column a generated column reads cannot be dropped.
                && !(pick.get("alter") == Some("drop_column")
                    && pick.get("constraint").is_some_and(|c| c.starts_with("generated")))
        },
        build: build_table,
    }
}

/// The column definition of `a` for a constraint.
fn column_a(constraint: &str, kind: &str) -> String {
    match constraint {
        "not_null" => format!("a {kind} NOT NULL"),
        "unique" => format!("a {kind} UNIQUE"),
        "check" => format!("a {kind} CHECK (a IS NULL OR a <> 3)"),
        "default" => format!("a {kind} DEFAULT 42"),
        "collate" => format!("a {kind} COLLATE NOCASE"),
        // `INT` rather than `INTEGER`: an `INTEGER PRIMARY KEY` is the rowid,
        // which refuses a value that is not an integer whatever the conflict
        // clause says, and this case is about a key that is not the rowid.
        "primary_key" => format!(
            "a {} PRIMARY KEY",
            if kind == "INTEGER" { "INT" } else { kind }
        ),
        "references" => format!("a {kind} REFERENCES r(x)"),
        _ => format!("a {kind}"),
    }
}

/// Builds one `ddl_table` case.
fn build_table(pick: &Pick) -> Option<Case> {
    let form = pick.value("form");
    let constraint = pick.value("constraint");
    let kind = declared(pick.value("affinity"), form == "strict");
    let mut columns = vec![
        "k INTEGER".to_string(),
        column_a(constraint, kind),
        "b TEXT".to_string(),
    ];
    match constraint {
        "generated_virtual" => columns.push("g TEXT AS (a || 'x') VIRTUAL".to_string()),
        "generated_stored" => columns.push("g TEXT AS (a || 'x') STORED".to_string()),
        _ => {}
    }
    if form == "without_rowid" {
        columns.push("PRIMARY KEY (k)".to_string());
    }
    let tail = match form {
        "without_rowid" => " WITHOUT ROWID",
        "strict" => " STRICT",
        _ => "",
    };
    let temp = if form == "temp" { "TEMP " } else { "" };
    let exists = if form == "if_not_exists" {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let mut case = Case::new("", "");
    case.records
        .push(Record::ok("CREATE TABLE r(x PRIMARY KEY)"));
    if form == "as_select" {
        case.records.push(Record::ok(format!(
            "CREATE TABLE src0(k INTEGER, a {kind}, b TEXT)"
        )));
        case.records
            .extend(insert_rows("src0", "k, a, b", pick.value("data")));
        case.records
            .push(Record::ok("CREATE TABLE t AS SELECT k, a, b FROM src0"));
    } else {
        case.records.push(Record::ok(format!(
            "CREATE {temp}TABLE {exists}t({}){tail}",
            columns.join(", ")
        )));
        let typed = if form == "strict" { kind } else { "ANY" };
        case.records
            .extend(insert_rows_typed("t", "k, a, b", pick.value("data"), typed));
        if form == "if_not_exists" {
            case.records
                .push(Record::ok("CREATE TABLE IF NOT EXISTS t(z)"));
        }
    }
    let (statement, table) = match pick.value("alter") {
        "add_column" => (Some("ALTER TABLE t ADD COLUMN c2 TEXT"), "t"),
        "add_column_default" => (
            Some("ALTER TABLE t ADD COLUMN c3 INTEGER NOT NULL DEFAULT 7"),
            "t",
        ),
        "rename_column" => (Some("ALTER TABLE t RENAME COLUMN b TO bb"), "t"),
        "rename_table" => (Some("ALTER TABLE t RENAME TO t_new"), "t_new"),
        "drop_column" => (Some("ALTER TABLE t DROP COLUMN b"), "t"),
        _ => (None, "t"),
    };
    if let Some(sql) = statement {
        case.records.push(read(sql));
    }
    case.records.extend(describe(table));
    case.records.push(read(format!(
        "SELECT count(*), count(a) FROM {table} WHERE a IS NOT NULL"
    )));
    Some(case)
}

/// `ddl_index`: every kind of index, made before or after the rows, and the
/// queries that can use it, checked against the same query with the index
/// refused.
fn ddl_index() -> Family {
    Family {
        name: "ddl_index",
        axes: vec![
            Axis::new(
                "index",
                &[
                    "plain",
                    "unique",
                    "partial",
                    "expression",
                    "collate",
                    "desc",
                    "multi",
                    "generated_virtual",
                    "generated_stored",
                    "if_not_exists",
                ],
            ),
            Axis::new(
                "table",
                &["rowid", "ipk", "without_rowid", "strict", "temp"],
            ),
            Axis::new("when", &["before_rows", "after_rows"]),
            Axis::new(
                "query",
                &[
                    "point",
                    "range",
                    "order_by",
                    "is_null",
                    "collated",
                    "expression",
                ],
            ),
            Axis::new("data", DATA),
            Axis::new("affinity", AFFINITIES),
        ],
        allowed: |_| true,
        build: build_index,
    }
}

/// The `CREATE INDEX` statement for an index kind.
fn index_statement(kind: &str) -> &'static str {
    match kind {
        "unique" => "CREATE UNIQUE INDEX ix ON t(a)",
        "partial" => "CREATE INDEX ix ON t(a) WHERE a > 1",
        "expression" => "CREATE INDEX ix ON t(coalesce(a, 0) + 1)",
        "collate" => "CREATE INDEX ix ON t(a COLLATE NOCASE)",
        "desc" => "CREATE INDEX ix ON t(a DESC, k)",
        "multi" => "CREATE INDEX ix ON t(b, a)",
        "generated_virtual" | "generated_stored" => "CREATE INDEX ix ON t(g)",
        "if_not_exists" => "CREATE INDEX IF NOT EXISTS ix ON t(a)",
        _ => "CREATE INDEX ix ON t(a)",
    }
}

/// Builds one `ddl_index` case.
fn build_index(pick: &Pick) -> Option<Case> {
    let table = pick.value("table");
    let index = pick.value("index");
    let kind = declared(pick.value("affinity"), table == "strict");
    let key = match table {
        "ipk" | "without_rowid" => "k INTEGER PRIMARY KEY",
        _ => "k INTEGER",
    };
    let generated = match index {
        "generated_virtual" => ", g TEXT AS (a || 'g') VIRTUAL",
        "generated_stored" => ", g TEXT AS (a || 'g') STORED",
        _ => "",
    };
    let tail = match table {
        "without_rowid" => " WITHOUT ROWID",
        "strict" => " STRICT",
        _ => "",
    };
    let temp = if table == "temp" { "TEMP " } else { "" };
    let mut case = Case::new("", "");
    case.records.push(Record::ok(format!(
        "CREATE {temp}TABLE t({key}, a {kind}, b TEXT{generated}){tail}"
    )));
    let typed = if table == "strict" { kind } else { "ANY" };
    let rows = insert_rows_typed("t", "k, a, b", pick.value("data"), typed);
    let create = read(index_statement(index));
    if pick.value("when") == "before_rows" {
        case.records.push(create);
        case.records.extend(rows);
    } else {
        case.records.extend(rows);
        case.records.push(create);
    }
    let query = match pick.value("query") {
        "point" => "SELECT k, a FROM t WHERE a = 2",
        "range" => "SELECT k, a FROM t WHERE a > 1 AND a < 'z'",
        "order_by" => "SELECT k, a FROM t ORDER BY a DESC, k",
        "is_null" => "SELECT k, a FROM t WHERE a IS NULL",
        "collated" => "SELECT k, a FROM t WHERE a = 'abc' COLLATE NOCASE",
        _ => "SELECT k, a FROM t WHERE coalesce(a, 0) + 1 > 2",
    };
    let sort = if pick.value("query") == "order_by" {
        Sort::NoSort
    } else {
        Sort::RowSort
    };
    case.records.push(record(query.to_string(), sort, &[]));
    case.records.push(read("PRAGMA index_list(t)"));
    case.records.push(read("PRAGMA index_info(ix)"));
    case.properties.push(Property::Same {
        name: "index agreement".to_string(),
        queries: vec![
            query.to_string(),
            query.replacen("FROM t", "FROM t NOT INDEXED", 1),
        ],
    });
    Some(case)
}

/// `ddl_view`: view bodies, a column list, and the queries a caller asks of a
/// view, including a filter on a view's column (the defect `f593b836` fixed).
fn ddl_view() -> Family {
    Family {
        name: "ddl_view",
        axes: vec![
            Axis::new(
                "body",
                &[
                    "plain",
                    "join",
                    "compound",
                    "cte",
                    "aggregate",
                    "window",
                    "distinct",
                ],
            ),
            Axis::new("columns", &["none", "list"]),
            Axis::new(
                "query",
                &["all", "filter", "order", "join", "count", "nested_view"],
            ),
            Axis::new("table", &["rowid", "without_rowid", "strict"]),
            Axis::new("data", DATA),
            Axis::new("affinity", AFFINITIES),
        ],
        allowed: |_| true,
        build: build_view,
    }
}

/// Builds one `ddl_view` case.
fn build_view(pick: &Pick) -> Option<Case> {
    let table = pick.value("table");
    let kind = declared(pick.value("affinity"), table == "strict");
    let (key, tail) = match table {
        "without_rowid" => ("k INTEGER PRIMARY KEY", " WITHOUT ROWID"),
        "strict" => ("k INTEGER", " STRICT"),
        _ => ("k INTEGER", ""),
    };
    let body = match pick.value("body") {
        "join" => "SELECT t.k AS x, t.a AS y, r.w AS z FROM t JOIN r ON r.k = t.k",
        "compound" => "SELECT k AS x, a AS y, b AS z FROM t UNION ALL SELECT k, w, 'r' FROM r",
        "cte" => "WITH c AS (SELECT k, a, b FROM t WHERE a IS NOT NULL) SELECT k AS x, a AS y, b AS z FROM c",
        "aggregate" => "SELECT b AS x, count(*) AS y, max(a) AS z FROM t GROUP BY b",
        "window" => "SELECT k AS x, a AS y, row_number() OVER (ORDER BY a, k) AS z FROM t",
        "distinct" => "SELECT DISTINCT a AS x, b AS y, 1 AS z FROM t",
        _ => "SELECT k AS x, a AS y, b AS z FROM t",
    };
    let names = if pick.value("columns") == "list" {
        "(x, y, z)"
    } else {
        ""
    };
    let body = if names.is_empty() {
        body.to_string()
    } else {
        body.replace(" AS x", "")
            .replace(" AS y", "")
            .replace(" AS z", "")
    };
    let mut case = Case::new("", "");
    case.setup.push(Record::ok(format!(
        "CREATE TABLE t({key}, a {kind}, b TEXT){tail}"
    )));
    let typed = if table == "strict" { kind } else { "ANY" };
    case.setup
        .extend(insert_rows_typed("t", "k, a, b", pick.value("data"), typed));
    case.setup
        .push(Record::ok("CREATE TABLE r(k INTEGER, w TEXT)"));
    case.setup.push(Record::ok(
        "INSERT INTO r VALUES (1, 'one'), (2, 'two'), (9, 'nine')",
    ));
    case.setup
        .push(Record::ok(format!("CREATE VIEW v{names} AS {body}")));
    let (query, sort) = match pick.value("query") {
        "filter" => ("SELECT * FROM v WHERE y > 1 OR z = 'q'", Sort::RowSort),
        "order" => ("SELECT x, y, z FROM v ORDER BY y, x, z", Sort::NoSort),
        "join" => (
            "SELECT v.x, v.y, r.w FROM v LEFT JOIN r ON r.k = v.x",
            Sort::RowSort,
        ),
        "count" => ("SELECT count(*), count(y) FROM v", Sort::RowSort),
        "nested_view" => (
            "SELECT * FROM (SELECT x, y FROM v WHERE y IS NOT NULL) AS d WHERE d.x > 1",
            Sort::RowSort,
        ),
        _ => ("SELECT * FROM v", Sort::RowSort),
    };
    case.records.push(record(query.to_string(), sort, &[]));
    case.records.push(read("PRAGMA table_info(v)"));
    case.properties.push(Property::Same {
        name: "view against its body".to_string(),
        queries: vec![
            "SELECT * FROM v".to_string(),
            format!("SELECT * FROM ({body}) AS body"),
        ],
    });
    Some(case)
}
