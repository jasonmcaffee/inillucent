//! Vectors as SQL values: the three distance functions, graded against an
//! exhaustive computation done outside the engine.
//!
//! Invariant: **the engine's answer is checked against arithmetic this file
//! does itself, never against the engine's own other path.** There is no
//! reference to be differential against - SQLite has no vector functions, which
//! is the whole reason Part 7 exists - so the oracle here is a cosine written
//! out in Rust over the same bytes, and the test is that the top ten rows and
//! their order agree.
//!
//! A vector is a blob of little-endian `f32`, the same bytes
//! `inillucent_search` stores, so a column of them and the retrieval engine's
//! own copies are the same thing seen from two places.

use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// How many dimensions the graded corpus uses.
const DIMENSIONS: usize = 32;

/// How many vectors the graded corpus holds.
const VECTORS: usize = 500;

/// Returns a fresh database in the test area.
///
/// @param name - the test's name, which names its file
fn database(name: &str) -> Database {
    let area = workspace_root().join("target/scratch/task-1838/vector");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    Database::open(&path).expect("a fresh database opens")
}

/// Returns the first column of every row, as reals.
///
/// @param connection - the connection to ask
/// @param sql - the query
fn reals(connection: &Connection<'_>, sql: &str) -> Vec<Option<f64>> {
    connection
        .query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        .iter()
        .map(|row| match row.first() {
            Some(OwnedDatum::Real(number)) => Some(*number),
            Some(OwnedDatum::Int(number)) => Some(*number as f64),
            Some(OwnedDatum::Null) => None,
            other => panic!("{sql} answered {other:?}"),
        })
        .collect()
}

/// Returns the first column of every row, as integers.
///
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
///
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

/// Returns one deterministic pseudo-random vector.
///
/// A fixed generator rather than a crate: the corpus has to be the same on
/// every machine for a failure to mean anything.
///
/// @param seed - which vector
fn vector_of(seed: u64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    (0..DIMENSIONS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f32 / (1u64 << 53) as f32) - 0.5
        })
        .collect()
}

/// Returns the cosine distance, computed here rather than by the engine.
///
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

/// The three measures answer what the arithmetic says they answer.
#[test]
fn the_distance_functions_answer_the_arithmetic() {
    let held = database("measures");
    let connection = held.connect();
    let east = literal(&[1.0, 0.0]);
    let north = literal(&[0.0, 1.0]);
    let half = literal(&[1.0, 0.5]);
    let other = literal(&[0.5, 1.0]);

    assert_eq!(
        reals(
            &connection,
            &format!("SELECT vector_distance_cos({east}, {east})")
        ),
        vec![Some(0.0)],
        "a vector is at no distance from itself"
    );
    assert_eq!(
        reals(
            &connection,
            &format!("SELECT vector_distance_cos({east}, {north})")
        ),
        vec![Some(1.0)],
        "two orthogonal vectors are one apart"
    );
    let l2 = reals(
        &connection,
        &format!("SELECT vector_distance_l2({east}, {north})"),
    );
    assert!(
        l2.first()
            .and_then(|held| *held)
            .is_some_and(|value| (value - std::f64::consts::SQRT_2).abs() < 1e-9),
        "the Euclidean distance between the unit axes is root two, got {l2:?}"
    );
    assert_eq!(
        reals(&connection, &format!("SELECT vector_dot({half}, {other})")),
        vec![Some(1.0)],
        "the dot product is the sum of the products"
    );
}

/// Anything that is not a pair of same-width vectors answers NULL.
#[test]
fn a_measure_of_something_that_is_not_a_vector_is_null() {
    let held = database("null");
    let connection = held.connect();
    let east = literal(&[1.0, 0.0]);
    let wide = literal(&[1.0, 0.0, 0.0]);

    for sql in [
        format!("SELECT vector_distance_cos('text', {east})"),
        format!("SELECT vector_distance_cos({east}, NULL)"),
        format!("SELECT vector_distance_cos({east}, 7)"),
        format!("SELECT vector_distance_cos({east}, {wide})"),
        format!("SELECT vector_distance_cos({east}, x'00')"),
        format!(
            "SELECT vector_distance_cos({}, {east})",
            literal(&[0.0, 0.0])
        ),
    ] {
        assert_eq!(reals(&connection, &sql), vec![None], "{sql}");
    }
}

/// `ORDER BY vector_distance_cos(...) LIMIT k` returns exactly the rows an
/// exhaustive cosine picks, in the same order.
///
/// This is the graded case the TDD asks for: five hundred vectors, ten queries,
/// and the top ten of each compared against the answer computed here.
#[test]
fn ordering_by_cosine_agrees_with_an_exhaustive_search() {
    let held = database("graded");
    let connection = held.connect();
    connection
        .execute_batch("CREATE TABLE embedding (id INTEGER PRIMARY KEY, v BLOB NOT NULL)")
        .expect("the table is created");
    let corpus: Vec<Vec<f32>> = (0..VECTORS).map(|index| vector_of(index as u64)).collect();
    let mut batch = String::new();
    for (index, vector) in corpus.iter().enumerate() {
        batch.push_str(&format!(
            "INSERT INTO embedding(id, v) VALUES ({}, {}); ",
            index,
            literal(vector)
        ));
    }
    connection.execute_batch(&batch).expect("the corpus loads");

    for query in 0..10u64 {
        let probe = vector_of(10_000 + query);
        let mut expected: Vec<(usize, f64)> = corpus
            .iter()
            .enumerate()
            .map(|(index, held)| (index, cosine(held, &probe)))
            .collect();
        // By distance, then by id, which is the order the engine's own `ORDER
        // BY d, id` asks for. Two vectors at the same distance would otherwise
        // make the comparison a coin toss rather than a check.
        expected.sort_by(|left, right| {
            left.1
                .partial_cmp(&right.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left.0.cmp(&right.0))
        });
        let wanted: Vec<i64> = expected
            .iter()
            .take(10)
            .map(|(index, _)| *index as i64)
            .collect();
        let got = integers(
            &connection,
            &format!(
                "SELECT id FROM embedding \
                 ORDER BY vector_distance_cos(v, {}), id LIMIT 10",
                literal(&probe)
            ),
        );
        assert_eq!(got, wanted, "query {query}'s top ten");
    }
}

/// A `VECTOR(N)` column refuses anything that is not N floats.
#[test]
fn a_vector_column_refuses_the_wrong_width() {
    let held = database("width");
    let connection = held.connect();
    connection
        .execute_batch("CREATE TABLE e (id INTEGER PRIMARY KEY, v VECTOR(4))")
        .expect("the table is created");
    connection
        .execute_batch(&format!(
            "INSERT INTO e(id, v) VALUES (1, {})",
            literal(&[1.0, 2.0, 3.0, 4.0])
        ))
        .expect("a vector of the declared width is stored");
    connection
        .execute_batch("INSERT INTO e(id, v) VALUES (2, NULL)")
        .expect("a NULL is not a wrong-width vector");
    for wrong in [
        format!("INSERT INTO e(id, v) VALUES (3, {})", literal(&[1.0, 2.0])),
        "INSERT INTO e(id, v) VALUES (4, 'text')".to_string(),
        "INSERT INTO e(id, v) VALUES (5, 7)".to_string(),
        format!("UPDATE e SET v = {} WHERE id = 1", literal(&[1.0])),
    ] {
        assert!(
            connection.execute_batch(&wrong).is_err(),
            "{wrong} should have been refused"
        );
    }
    assert_eq!(
        integers(&connection, "SELECT count(*) FROM e"),
        vec![2],
        "only the two legal rows are there"
    );
}

/// A vector column survives a write and comes back byte for byte.
#[test]
fn a_vector_column_round_trips() {
    let held = database("roundtrip");
    let connection = held.connect();
    connection
        .execute_batch("CREATE TABLE e (id INTEGER PRIMARY KEY, v BLOB)")
        .expect("the table is created");
    let vector = vector_of(7);
    connection
        .execute_batch(&format!(
            "INSERT INTO e(id, v) VALUES (1, {})",
            literal(&vector)
        ))
        .expect("the row is written");
    // Zero distance from itself, read out of the column rather than written
    // twice into the statement, is the round trip: a byte that changed would
    // move the cosine.
    assert_eq!(
        reals(
            &connection,
            &format!("SELECT vector_distance_cos(v, {}) FROM e", literal(&vector))
        ),
        vec![Some(0.0)],
        "a stored vector is the vector that was stored"
    );
}
