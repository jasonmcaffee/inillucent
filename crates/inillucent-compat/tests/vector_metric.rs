//! A second metric on the vector index: `docs/roadmap.md` item 11.
//!
//! Invariant: **the graph a `metric = 'l2'` index builds actually minimises L2,
//! not cosine wearing an L2 label.** The distance functions `vector_distance_cos`
//! and `vector_distance_l2` have always answered for any pair of vectors - what
//! was missing was the index, which used to plan cosine only. Every test here
//! is graded against arithmetic this file computes itself, the same discipline
//! `vector.rs` uses for the functions.
//!
//! The corpus in each test is built around two vectors chosen so cosine and L2
//! disagree about which is nearer a fixed query: one is perfectly aligned with
//! the query but twice as far away in space, the other sits close to the query
//! but slightly off its axis. Cosine prefers the aligned one; L2 prefers the
//! close one. A corpus that could not produce this disagreement would let a
//! cosine graph masquerade as an L2 one and pass every test anyway - which is
//! exactly the defect `docs/roadmap.md` item 11 describes.

use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// How many dimensions the corpus uses.
const DIMENSIONS: usize = 4;

/// Returns a fresh database in the test area.
/// @param name - the test's name, which names its file
fn database(name: &str) -> Database {
    let area = workspace_root().join("target/scratch/vector_metric");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    Database::open(&path).expect("a fresh database opens")
}

/// Returns the first column of every row, as integers.
/// @param connection - the connection to ask
/// @param sql - the query
fn integers(connection: &Connection<'_>, sql: &str) -> Vec<i64> {
    connection
        .query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        .iter()
        .map(|row| match row.first() {
            Some(OwnedDatum::Int(number)) => *number,
            other => panic!("{sql} answered {other:?}"),
        })
        .collect()
}

/// Renders a vector as the SQL blob literal a statement can carry.
/// @param values - the vector
fn literal(values: &[f32]) -> String {
    let mut out = String::from("x'");
    for value in values {
        for byte in value.to_bits().to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out.push('\'');
    out
}

/// One deterministic pseudo-random filler vector - far enough from the query
/// under both metrics that it never contends with the two rows the tests are
/// actually about, on any machine.
/// @param seed - which filler
fn filler_of(seed: u64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    (0..DIMENSIONS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // In [-3, -1] or [1, 3]: never near the query's own axis, and
            // large enough that its L2 distance to the query dwarfs the two
            // rows under test, whose components are all within [0, 2].
            let magnitude = 1.0 + ((state >> 11) as f32 / (1u64 << 53) as f32) * 2.0;
            if state & 1 == 0 {
                magnitude
            } else {
                -magnitude
            }
        })
        .collect()
}

/// Cosine distance, computed here rather than by the engine.
/// @param left - one vector
/// @param right - the other
fn cosine(left: &[f32], right: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut one = 0.0f64;
    let mut two = 0.0f64;
    for (a, b) in left.iter().zip(right.iter()) {
        dot += f64::from(*a) * f64::from(*b);
        one += f64::from(*a) * f64::from(*a);
        two += f64::from(*b) * f64::from(*b);
    }
    1.0 - dot / (one.sqrt() * two.sqrt())
}

/// Euclidean distance, computed here rather than by the engine - matching
/// `vector_distance_l2`, which takes the square root (`vector.rs` proves that
/// against the unit axes, which are root two apart).
/// @param left - one vector
/// @param right - the other
fn euclidean(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right.iter())
        .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// How many filler rows the corpus carries, beside the two rows under test.
///
/// The planner's own cost model prices a vector probe against `k` candidates
/// versus a full scan (`crates/inillucent-sql/src/plan.rs::path_cost`), and a
/// probe only wins once the table is meaningfully larger than `LIMIT`. 300 is
/// the same order of magnitude `vector.rs` uses for the same reason - fewer
/// rows and `EXPLAIN QUERY PLAN` correctly prefers the scan on cost alone,
/// which would make a test asserting index use fail for a reason that has
/// nothing to do with metrics.
const FILLERS: u64 = 300;

/// The corpus every test in this file shares: two rows built to disagree
/// under cosine and L2, and enough fillers that the planner's own cost model
/// prefers the index over a scan.
///
/// Row 1 is aligned with `query()` but twice as far away in space; row 2 sits
/// close to the query but slightly off its axis. Ids start at 1 so `rowid`
/// and the row's own identity are the same number throughout.
fn corpus() -> Vec<(i64, Vec<f32>)> {
    let mut rows = vec![
        (1i64, vec![2.0f32, 0.0, 0.0, 0.0]),
        (2i64, vec![0.9f32, 0.1, 0.0, 0.0]),
    ];
    for seed in 0..FILLERS {
        rows.push((3 + seed as i64, filler_of(seed)));
    }
    rows
}

/// The query every test asks against.
fn query() -> Vec<f32> {
    vec![1.0f32, 0.0, 0.0, 0.0]
}

/// Loads the shared corpus into a fresh `VECTOR(4)` table.
/// @param connection - the connection to load it into
fn load_corpus(connection: &Connection<'_>) {
    connection
        .execute_batch(&format!(
            "CREATE TABLE e (id INTEGER PRIMARY KEY, v VECTOR({DIMENSIONS}))"
        ))
        .expect("the table is created");
    let mut batch = String::new();
    for (id, vector) in corpus() {
        batch.push_str(&format!(
            "INSERT INTO e(id, v) VALUES ({id}, {});",
            literal(&vector)
        ));
    }
    connection.execute_batch(&batch).expect("the corpus loads");
}

/// Returns row ids in ascending order of a distance function computed here,
/// breaking ties by id the way the engine's own `ORDER BY d, id` does.
/// @param distance - the metric to rank by
fn exhaustive_order(distance: impl Fn(&[f32], &[f32]) -> f64) -> Vec<i64> {
    let probe = query();
    let mut ranked: Vec<(i64, f64)> = corpus()
        .into_iter()
        .map(|(id, vector)| (id, distance(&vector, &probe)))
        .collect();
    ranked.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    ranked.into_iter().map(|(id, _)| id).collect()
}

/// Returns the `EXPLAIN QUERY PLAN` detail lines for one query.
/// @param connection - the connection to ask
/// @param sql - the query to explain
fn explain(connection: &Connection<'_>, sql: &str) -> Vec<String> {
    connection
        .query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("the plan is explained")
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<String>>()
                .join(" ")
        })
        .collect()
}

/// The corpus actually produces the disagreement the rest of this file
/// depends on. If this fails, every other test here would be checking
/// nothing: a cosine graph reporting L2 could still pass them by accident.
#[test]
fn the_corpus_makes_cosine_and_l2_disagree() {
    let cosine_order = exhaustive_order(cosine);
    let l2_order = exhaustive_order(euclidean);
    assert_eq!(
        cosine_order.first(),
        Some(&1),
        "cosine should prefer the aligned row"
    );
    assert_eq!(l2_order.first(), Some(&2), "L2 should prefer the close row");
}

/// An index declared `metric = 'l2'`, probed by `vector_distance_l2`, returns
/// exactly the exhaustive L2 order - the graph is minimising L2, not cosine
/// wearing an L2 label.
#[test]
fn an_l2_index_matches_the_exhaustive_l2_order() {
    let held = database("l2-matches-exhaustive");
    let connection = held.session();
    load_corpus(&connection);
    connection
        .execute_batch("CREATE INDEX ix ON e USING inillucent_hnsw (v) WITH (metric = 'l2')")
        .expect("the L2 index is built");

    let sql = format!(
        "SELECT id FROM e ORDER BY vector_distance_l2(v, {}) LIMIT 10",
        literal(&query())
    );
    let plan = explain(&connection, &sql);
    assert!(
        plan.iter()
            .any(|line| line.contains("USING VECTOR INDEX ix")),
        "an l2 query over an l2 index should use the index: {plan:?}"
    );

    let got = integers(&connection, &sql);
    let wanted: Vec<i64> = exhaustive_order(euclidean).into_iter().take(10).collect();
    assert_eq!(got, wanted, "the l2 index did not answer the l2 order");
    assert_eq!(
        got.first(),
        Some(&2),
        "the nearest row by L2 should be the close-but-off-axis one"
    );
}

/// The same `metric = 'l2'` index, probed by `vector_distance_cos` instead,
/// falls back to the exhaustive scan - and the scan still answers the correct
/// cosine order, because the distance function itself has never been the
/// limit, only the index probe.
#[test]
fn an_l2_index_falls_back_to_the_scan_for_a_cosine_order() {
    let held = database("l2-falls-back-for-cosine");
    let connection = held.session();
    load_corpus(&connection);
    connection
        .execute_batch("CREATE INDEX ix ON e USING inillucent_hnsw (v) WITH (metric = 'l2')")
        .expect("the L2 index is built");

    let sql = format!(
        "SELECT id FROM e ORDER BY vector_distance_cos(v, {}) LIMIT 10",
        literal(&query())
    );
    let plan = explain(&connection, &sql);
    assert!(
        plan.iter().all(|line| !line.contains("USING VECTOR INDEX")),
        "a cosine query over an l2 index must not use the index: {plan:?}"
    );

    let got = integers(&connection, &sql);
    let wanted: Vec<i64> = exhaustive_order(cosine).into_iter().take(10).collect();
    assert_eq!(
        got, wanted,
        "the fallback scan did not answer the cosine order"
    );
    assert_eq!(
        got.first(),
        Some(&1),
        "the nearest row by cosine should be the aligned-but-far one"
    );
}

/// The reverse direction: a cosine index (the default, naming no metric at
/// all) falls back to the scan for `vector_distance_l2`, and the scan still
/// answers the correct L2 order. This is also the declaration-time default:
/// a `CREATE` that never named a metric behaves as cosine, the same claim
/// `options.rs::an_index_declares_cosine_by_default` makes at the
/// declaration layer and `persist.rs::a_generation_with_no_stored_metric_reads_as_cosine`
/// makes at the generation layer.
#[test]
fn a_default_cosine_index_falls_back_to_the_scan_for_an_l2_order() {
    let held = database("cosine-falls-back-for-l2");
    let connection = held.session();
    load_corpus(&connection);
    connection
        .execute_batch("CREATE INDEX ix ON e USING inillucent_hnsw (v)")
        .expect("the default index is built");

    // First, the default is really cosine: a cosine query uses the index.
    let cosine_sql = format!(
        "SELECT id FROM e ORDER BY vector_distance_cos(v, {}) LIMIT 10",
        literal(&query())
    );
    let cosine_plan = explain(&connection, &cosine_sql);
    assert!(
        cosine_plan
            .iter()
            .any(|line| line.contains("USING VECTOR INDEX ix")),
        "a cosine query over the default index should use it: {cosine_plan:?}"
    );
    assert_eq!(
        integers(&connection, &cosine_sql),
        exhaustive_order(cosine)
            .into_iter()
            .take(10)
            .collect::<Vec<i64>>()
    );

    // Then, an l2 query over that same undeclared-metric index falls back.
    let l2_sql = format!(
        "SELECT id FROM e ORDER BY vector_distance_l2(v, {}) LIMIT 10",
        literal(&query())
    );
    let l2_plan = explain(&connection, &l2_sql);
    assert!(
        l2_plan
            .iter()
            .all(|line| !line.contains("USING VECTOR INDEX")),
        "an l2 query over a cosine-default index must not use the index: {l2_plan:?}"
    );
    let got = integers(&connection, &l2_sql);
    let wanted: Vec<i64> = exhaustive_order(euclidean).into_iter().take(10).collect();
    assert_eq!(got, wanted, "the fallback scan did not answer the l2 order");
}

/// `inillucent_search(...)` itself, not only `USING inillucent_hnsw`, takes
/// `metric = 'l2'` and keeps the vectors it is given at their own magnitude -
/// which is what the table form and the index form both promise, being two
/// spellings of the same store.
#[test]
fn the_search_table_form_takes_l2_too() {
    let held = database("search-table-l2");
    let connection = held.session();
    connection
        .execute_batch(&format!(
            "CREATE VIRTUAL TABLE store USING inillucent_search(body, dims = {DIMENSIONS}, metric = 'l2')"
        ))
        .expect("the l2 search table is created");
    for (id, vector) in corpus() {
        connection
            .execute_batch(&format!(
                "INSERT INTO store(body, vector) VALUES ('{id}', {})",
                literal(&vector)
            ))
            .expect("a row is written");
    }

    let hit = connection
        .query(&format!(
            "SELECT body FROM store WHERE store MATCH '' AND vector = {} AND k = 1 ORDER BY rank",
            literal(&query())
        ))
        .expect("the store answers")
        .into_iter()
        .filter_map(|row| match row.first() {
            Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).parse::<i64>().ok(),
            _ => None,
        })
        .next();
    assert_eq!(
        hit,
        Some(2),
        "the l2 search store should answer the l2-nearest row, not the cosine-nearest one"
    );
}
