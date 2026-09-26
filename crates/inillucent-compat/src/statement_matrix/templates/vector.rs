//! `vector`: `VECTOR(N)` columns, the distance functions and operators, and
//! nearest neighbour search by scan, by an `inillucent_hnsw` index in exact
//! and approximate mode, and through an `inillucent_search` table.
//!
//! Invariant: **every answer here is computed in Rust from the vectors the
//! case stores, never by asking the engine under test, and a case is
//! `oracle none` because the pinned SQLite has no vectors.** Distances are
//! compared at three decimal places, the precision the case asks for with
//! `round(..., 3)`, because the engine stores 32 bit components and this
//! module computes in 64 bits. The corpora have no ties at any `k` a case
//! asks for, so the nearest neighbour lists are unique, and an approximate
//! search over eight vectors must find them exactly (section 6.2 of the
//! design grades recall; at this size the recall must be all of them).

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::properties::Property;
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The two corpora: eight vectors each, chosen so no two distances to the
/// query vector are equal and none is near a rounding boundary.
const CORPORA: &[&[[f64; 3]]] = &[
    &[
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [1.0, 1.0, 0.0],
        [2.0, 0.5, 0.25],
        [0.5, 2.0, 1.0],
        [3.0, 1.0, 2.0],
        [0.25, 0.25, 4.0],
    ],
    &[
        [0.9, 0.1, 0.0],
        [0.2, 0.7, 0.1],
        [0.1, 0.1, 0.8],
        [0.6, 0.6, 0.0],
        [4.0, 1.0, 0.5],
        [1.0, 4.0, 2.0],
        [2.5, 2.0, 1.0],
        [0.5, 0.5, 5.0],
    ],
];

/// The query vector.
const QUERY: [f64; 3] = [1.0, 0.25, 0.5];

/// The vector family.
pub fn family() -> Family {
    Family {
        name: "vector",
        axes: vec![
            Axis::new(
                "operation",
                &[
                    "distance_function",
                    "distance_operator",
                    "norm_dims",
                    "knn_scan",
                    "knn_hnsw_exact",
                    "knn_hnsw_approximate",
                    "search_table",
                ],
            ),
            Axis::new("metric", &["cosine", "l2", "dot"]),
            Axis::new("placement", &["top", "derived", "cte", "view"]),
            Axis::new("k", &["1", "3"]),
            Axis::new("corpus", &["first", "second"]),
        ],
        allowed: |pick| {
            // An HNSW index and a search table rank by cosine or L2 only.
            !(matches!(
                pick.get("operation"),
                Some("knn_hnsw_exact") | Some("knn_hnsw_approximate") | Some("search_table")
            ) && pick.get("metric") == Some("dot"))
        },
        build,
    }
}

/// A vector as the text SQL writes it.
fn text(vector: &[f64; 3]) -> String {
    format!("[{}, {}, {}]", vector[0], vector[1], vector[2])
}

/// The length of a vector.
fn norm(vector: &[f64; 3]) -> f64 {
    vector.iter().map(|x| x * x).sum::<f64>().sqrt()
}

/// The distance under a metric, as the engine defines it.
fn distance(metric: &str, left: &[f64; 3], right: &[f64; 3]) -> f64 {
    let dot: f64 = left.iter().zip(right.iter()).map(|(a, b)| a * b).sum();
    match metric {
        "l2" => left
            .iter()
            .zip(right.iter())
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f64>()
            .sqrt(),
        "dot" => dot,
        _ => 1.0 - dot / (norm(left) * norm(right)),
    }
}

/// The SQL function and operator for a metric.
fn spelled(metric: &str) -> (&'static str, &'static str) {
    match metric {
        "l2" => ("vector_distance_l2", "<->"),
        "dot" => ("vector_dot", "<#>"),
        _ => ("vector_distance_cos", "<=>"),
    }
}

/// Renders a real the way a recorded answer holds it.
fn three(value: f64) -> String {
    crate::slt::format_real((value * 1000.0).round() / 1000.0)
}

/// The ids of the `k` nearest vectors, nearest first.
fn nearest(metric: &str, corpus: &[[f64; 3]], k: usize) -> Vec<String> {
    let mut ranked: Vec<(f64, usize)> = corpus
        .iter()
        .enumerate()
        .map(|(at, vector)| {
            let d = distance(metric, vector, &QUERY);
            // `<#>` is the negative inner product, so a larger product is nearer.
            (if metric == "dot" { -d } else { d }, at + 1)
        })
        .collect();
    ranked.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
    ranked
        .into_iter()
        .take(k)
        .map(|(_, id)| id.to_string())
        .collect()
}

/// The query and its recorded answer for one operation.
fn question(pick: &Pick, corpus: &[[f64; 3]]) -> (String, Vec<Vec<String>>) {
    let metric = pick.value("metric");
    let (function, operator) = spelled(metric);
    let q = text(&QUERY);
    let k: usize = pick.value("k").parse().unwrap_or(1);
    match pick.value("operation") {
        "distance_function" | "distance_operator" => {
            let call = if pick.value("operation") == "distance_function" {
                format!("{function}(v, '{q}')")
            } else if metric == "dot" {
                format!("-(v {operator} '{q}')")
            } else {
                format!("v {operator} '{q}'")
            };
            let answer = corpus
                .iter()
                .enumerate()
                .map(|(at, vector)| vec![(at + 1).to_string(), three(distance(metric, vector, &QUERY))])
                .collect();
            (format!("SELECT id AS c1, round({call}, 3) AS c2 FROM p ORDER BY id, c2"), answer)
        }
        "norm_dims" => {
            let answer = corpus
                .iter()
                .enumerate()
                .map(|(at, vector)| vec![(at + 1).to_string(), "3".to_string(), three(norm(vector))])
                .collect();
            (
                "SELECT id AS c1, vector_dims(v) AS c2, round(vector_norm(v), 3) AS c3 FROM p ORDER BY id, c2, c3".to_string(),
                answer,
            )
        }
        "search_table" => (
            format!("SELECT rowid AS c1 FROM docs WHERE vector = '{q}' AND k = {k} ORDER BY rank, rowid"),
            nearest(metric, corpus, k).into_iter().map(|id| vec![id]).collect(),
        ),
        _ => {
            let order = if metric == "dot" {
                format!("v {operator} '{q}'")
            } else {
                format!("{function}(v, '{q}')")
            };
            (
                format!("SELECT id AS c1 FROM p ORDER BY {order}, id LIMIT {k}"),
                nearest(metric, corpus, k).into_iter().map(|id| vec![id]).collect(),
            )
        }
    }
}

/// Builds one vector case.
fn build(pick: &Pick) -> Option<Case> {
    let corpus: &[[f64; 3]] = if pick.value("corpus") == "second" {
        CORPORA.get(1)?
    } else {
        CORPORA.first()?
    };
    let metric = pick.value("metric");
    let mut case = Case::new("", "");
    case.oracle = false;
    case.setup.push(Record::ok(
        "CREATE TABLE p(id INTEGER PRIMARY KEY, v VECTOR(3))",
    ));
    let rows: Vec<String> = corpus
        .iter()
        .enumerate()
        .map(|(at, vector)| format!("({}, '{}')", at + 1, text(vector)))
        .collect();
    case.setup.push(Record::ok(format!(
        "INSERT INTO p VALUES {}",
        rows.join(", ")
    )));
    let metric_option = if metric == "l2" { "l2" } else { "cosine" };
    match pick.value("operation") {
        "knn_hnsw_exact" => case.setup.push(Record::ok(format!(
            "CREATE INDEX p_v ON p USING inillucent_hnsw (v) WITH (mode = 'exact', metric = '{metric_option}')"
        ))),
        "knn_hnsw_approximate" => case.setup.push(Record::ok(format!(
            "CREATE INDEX p_v ON p USING inillucent_hnsw (v) WITH (metric = '{metric_option}')"
        ))),
        "search_table" => {
            case.setup.push(Record::ok(format!(
                "CREATE VIRTUAL TABLE docs USING inillucent_search(body, dims = 3, metric = '{metric_option}')"
            )));
            case.setup.push(Record::ok(
                "INSERT INTO docs(rowid, body, vector) SELECT id, 'doc ' || id, v FROM p",
            ));
        }
        _ => {}
    }
    let (sql, answer) = question(pick, corpus);
    let placed = match pick.value("placement") {
        "derived" => format!("SELECT * FROM ({sql}) AS d"),
        "cte" => format!("WITH d AS ({sql}) SELECT * FROM d"),
        "view" => {
            case.setup
                .push(Record::ok(format!("CREATE VIEW pv AS {sql}")));
            "SELECT * FROM pv".to_string()
        }
        _ => sql.clone(),
    };
    // Only the top level query asked for an order; a wrapped one is compared
    // as a set of rows.
    let (sort, expected) = if pick.value("placement") == "top" {
        (Sort::NoSort, answer.into_iter().flatten().collect())
    } else {
        let mut rows = answer;
        rows.sort();
        (Sort::RowSort, rows.into_iter().flatten().collect())
    };
    case.records.push(Record::Query {
        types: "T".to_string(),
        sort,
        expected: Some(expected),
        binds: Vec::new(),
        sql: placed.clone(),
    });
    case.properties.push(Property::Same {
        name: "vector placement equivalence".to_string(),
        queries: vec![
            sql.clone(),
            format!("SELECT * FROM ({sql}) AS d"),
            format!("WITH d AS ({sql}) SELECT * FROM d"),
        ],
    });
    Some(case)
}
