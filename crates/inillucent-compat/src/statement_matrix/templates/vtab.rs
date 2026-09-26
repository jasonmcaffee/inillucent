//! `vtab`: FTS5 with its query syntax and auxiliary functions, R*Tree,
//! `json_each` and `json_tree` joined laterally, and the `pragma_*` table
//! valued functions, each in every placement.
//!
//! Invariant: **a ranked answer is compared at six decimal places, and a
//! highlight or snippet exactly.** `bm25` is a real computed from counts, and
//! the two engines may add the same terms in a different order and differ in
//! the last bit; the rank order and six places are what a caller relies on.
//! The lateral `json_each` join is the escaped defect where `WHERE t.id =
//! j.value` returned NULL columns (section 1.1).

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::templates::frame::{constant, layer_three, place, Query, BINDINGS};
use crate::statement_matrix::templates::scene::Relation;
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The vtab family.
pub fn family() -> Family {
    Family {
        name: "vtab",
        axes: vec![
            Axis::new(
                "use",
                &[
                    "fts_match",
                    "fts_column",
                    "fts_prefix",
                    "fts_phrase",
                    "fts_bm25",
                    "fts_highlight",
                    "fts_snippet",
                    "fts_rank_limit",
                    "rtree_range",
                    "rtree_contains",
                    "json_each_lateral",
                    "json_tree",
                    "pragma_table_info",
                    "pragma_index_list",
                ],
            ),
            Axis::new(
                "placement",
                &[
                    "top",
                    "derived",
                    "cte",
                    "view",
                    "in",
                    "exists",
                    "scalar",
                    "update_from",
                    "insert_select",
                ],
            ),
            Axis::new("binding", BINDINGS),
            Axis::new("rows", &["few", "many"]),
        ],
        allowed: |pick| {
            !(pick
                .get("binding")
                .is_some_and(|binding| binding != "literal")
                && pick.get("placement") == Some("view"))
        },
        build,
    }
}

/// The documents an FTS5 case searches.
fn documents(rows: &str) -> Vec<Record> {
    let mut out = vec![
        Record::ok("CREATE VIRTUAL TABLE f USING fts5(title, body)"),
        Record::ok("INSERT INTO f(rowid, title, body) VALUES (1, 'release notes', 'the release ships in may'), (2, 'parser', 'parse the headers first'), (3, 'release calendar', 'next release date is fixed'), (4, 'notes', NULL)"),
    ];
    if rows == "many" {
        out.push(Record::ok(
            "INSERT INTO f(rowid, title, body) SELECT value + 10, 'extra ' || value, 'filler text ' || (value % 3) FROM json_each('[1,2,3,4,5,6,7,8,9,10,11,12]')",
        ));
    }
    out
}

/// The other tables the non-FTS uses read.
fn others(rows: &str) -> Vec<Record> {
    let mut out = vec![
        Record::ok("CREATE VIRTUAL TABLE rt USING rtree(id, x0, x1, y0, y1)"),
        Record::ok(
            "INSERT INTO rt VALUES (1, 0, 10, 0, 10), (2, 5, 15, 5, 15), (3, 20, 30, 20, 30)",
        ),
        Record::ok("CREATE TABLE j(id INTEGER PRIMARY KEY, doc TEXT)"),
        Record::ok(
            "INSERT INTO j VALUES (1, '[1, 2, 3]'), (2, '{\"a\": 2, \"b\": [4, 5]}'), (3, '[]')",
        ),
        Record::ok("CREATE TABLE ti(k INTEGER PRIMARY KEY, a TEXT NOT NULL DEFAULT 'x', b)"),
        Record::ok("CREATE INDEX ti_a ON ti(a)"),
    ];
    if rows == "many" {
        out.push(Record::ok("INSERT INTO rt SELECT value + 10, value, value + 1, value, value + 1 FROM json_each('[1,2,3,4,5,6,7,8]')"));
        out.push(Record::ok("INSERT INTO j SELECT value + 10, json_array(value, value * 2) FROM json_each('[1,2,3,4,5]')"));
    }
    out
}

/// The query for a use, with the constant in it.
fn query(usage: &str, c: &str) -> (String, usize, Sort) {
    let (sql, columns, sort) = match usage {
        "fts_match" => (format!("SELECT rowid AS c1, title AS c2 FROM f WHERE f MATCH 'release' AND rowid >= {c} - 1"), 2, Sort::RowSort),
        "fts_column" => (format!("SELECT rowid AS c1, title AS c2 FROM f WHERE f MATCH 'title:notes' AND rowid > {c} - 3"), 2, Sort::RowSort),
        "fts_prefix" => (format!("SELECT rowid AS c1, body AS c2 FROM f WHERE f MATCH 'rel*' AND rowid <> {c}"), 2, Sort::RowSort),
        "fts_phrase" => (format!("SELECT rowid AS c1, body AS c2 FROM f WHERE f MATCH '\"release ships\"' OR rowid = {c}"), 2, Sort::RowSort),
        "fts_bm25" => (format!("SELECT rowid AS c1, round(bm25(f), 6) AS c2 FROM f WHERE f MATCH 'release OR notes' AND rowid < {c} + 20"), 2, Sort::RowSort),
        "fts_highlight" => (format!("SELECT rowid AS c1, highlight(f, 1, '[', ']') AS c2 FROM f WHERE f MATCH 'release' AND rowid >= {c} - 2"), 2, Sort::RowSort),
        "fts_snippet" => (format!("SELECT rowid AS c1, snippet(f, 1, '<', '>', '...', 3) AS c2 FROM f WHERE f MATCH 'release' AND rowid >= {c} - 2"), 2, Sort::RowSort),
        "fts_rank_limit" => (format!("SELECT rowid AS c1, title AS c2 FROM f WHERE f MATCH 'release OR parse' ORDER BY rank, rowid LIMIT {c}"), 2, Sort::NoSort),
        "rtree_range" => (format!("SELECT id AS c1, x0 AS c2 FROM rt WHERE x0 >= {c} AND x1 <= 20"), 2, Sort::RowSort),
        "rtree_contains" => (format!("SELECT id AS c1, y1 AS c2 FROM rt WHERE x0 <= {c} + 5 AND x1 >= {c} + 5"), 2, Sort::RowSort),
        "json_each_lateral" => (format!("SELECT j.id AS c1, e.value AS c2 FROM j, json_each(j.doc) AS e WHERE j.id = e.value OR e.value > {c}"), 2, Sort::RowSort),
        "json_tree" => (format!("SELECT j.id AS c1, t.fullkey AS c2 FROM j, json_tree(j.doc) AS t WHERE t.atom IS NOT NULL AND j.id <= {c}"), 2, Sort::RowSort),
        "pragma_table_info" => (format!("SELECT cid AS c1, name AS c2 FROM pragma_table_info('ti') WHERE cid < {c} + 1"), 2, Sort::RowSort),
        _ => (format!("SELECT name AS c1, \"unique\" AS c2 FROM pragma_index_list('ti') WHERE seq < {c}"), 2, Sort::RowSort),
    };
    (sql, columns, sort)
}

/// Builds one vtab case.
fn build(pick: &Pick) -> Option<Case> {
    let usage = pick.value("use");
    let c = constant(pick.value("binding"), "2", "1");
    let (sql, columns, sort) = query(usage, &c.text);
    let mut case = Case::new("", "");
    case.setup = if usage.starts_with("fts") {
        documents(pick.value("rows"))
    } else {
        others(pick.value("rows"))
    };
    let generated = Query {
        sql,
        columns,
        sort,
        binds: c.binds.clone(),
        relation: Relation::default(),
        checkable: true,
        ..Query::default()
    };
    place(&mut case, &generated, pick.value("placement"), false);
    layer_three(&mut case, &generated, false);
    Some(case)
}
