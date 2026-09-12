//! Functions and collations an application defines, compared against SQLite.
//!
//! Invariant: a registered function behaves like a built-in everywhere a
//! built-in works. The interesting cases are not "does it get called" but the
//! ones where a function's *kind* matters: an aggregate has to reach `GROUP BY`
//! and `ORDER BY`, a registration has to override a built-in of the same name,
//! and a collation has to change what an `ORDER BY` returns rather than only
//! what a comparison answers.

use std::sync::Arc;

use inillucent_compat::facade::Database;
use inillucent_ext::registry::FunctionFlags;
use inillucent_value::Value;

/// Returns the first column of the first row, as an integer.
fn single(connection: &inillucent_compat::facade::Connection, sql: &str) -> Option<i64> {
    let rows = connection.query(sql).expect("runs");
    rows.first()
        .and_then(|row| row.first())
        .and_then(Value::as_integer)
}

/// Returns a database file of this test's own, under the gitignored root.
///
/// **A file rather than `:memory:`, which the old facade accepted.** The new
/// engine opens a path and has no in-memory VFS behind `open` yet; nothing in
/// this file asserts anything about *where* the database lives, so the fixture
/// moves and every assertion stays exactly as it was. A serial keeps two tests
/// running in parallel from colliding on one file.
fn scratch() -> std::path::PathBuf {
    let root = inillucent_compat::workspace_root().join("_agent_output/functions");
    let _ = std::fs::create_dir_all(&root);
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = root.join(format!("{}-{serial}.rdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Opens a database of this test's own, with one connection.
fn connect() -> inillucent_compat::facade::Connection {
    let database = Database::open(scratch()).expect("opens");
    Box::leak(Box::new(database)).connect().expect("connects")
}

/// A scalar an application registered is callable from SQL.
#[test]
fn a_registered_scalar_is_callable() {
    let connection = connect();
    connection
        .create_scalar_function(
            "twice",
            1,
            FunctionFlags::external(),
            Arc::new(|arguments: &[Value<'static>]| {
                let value = arguments.first().and_then(Value::as_integer).unwrap_or(0);
                Ok(Value::Integer(value * 2))
            }),
        )
        .expect("registers");
    assert_eq!(single(&connection, "SELECT twice(21)"), Some(42));
}

/// A registered scalar is callable from `ORDER BY` and from `WHERE`, not only
/// from the projection.
///
/// **This was refused until task-1900.** The physical pass looks a registered
/// function's body up through the catalog, and the space a statement's stages
/// are viewed through carried no catalog - so the call worked in a projection,
/// which builds its space elsewhere, and failed everywhere else with
/// `unsupported`. Nobody had noticed because `embed(TEXT)` is the only
/// registered function this workspace ships and it had only ever been called as
/// `SELECT embed('...')`.
///
/// `ORDER BY` is the shape that matters: `ORDER BY vector_distance_cos(v,
/// embed('a question')) LIMIT k` is what a semantic search *is*, and without
/// this the function could not be used for the thing it exists for.
#[test]
fn a_registered_scalar_reaches_order_by_and_where() {
    let connection = connect();
    connection
        .create_scalar_function(
            "negate",
            1,
            FunctionFlags::external(),
            Arc::new(|arguments: &[Value<'static>]| {
                let value = arguments.first().and_then(Value::as_integer).unwrap_or(0);
                Ok(Value::Integer(-value))
            }),
        )
        .expect("registers");
    connection
        .execute("CREATE TABLE n (id INTEGER PRIMARY KEY, weight INTEGER)")
        .expect("creates");
    for (id, weight) in [(1, 10), (2, 30), (3, 20)] {
        connection
            .execute(&format!(
                "INSERT INTO n (id, weight) VALUES ({id}, {weight})"
            ))
            .expect("inserts");
    }

    // Ordering by the function's value, which reverses the natural order of the
    // weights: 30, 20, 10 becomes -30, -20, -10.
    let rows = connection
        .query("SELECT id FROM n ORDER BY negate(weight) LIMIT 3")
        .expect("orders by the registered function");
    let ordered: Vec<i64> = rows
        .iter()
        .filter_map(|row| row.first().and_then(Value::as_integer))
        .collect();
    assert_eq!(ordered, vec![2, 3, 1], "the heaviest row sorts first");

    // And in a predicate, where the same lookup happens.
    assert_eq!(
        single(
            &connection,
            "SELECT count(*) FROM n WHERE negate(weight) < -15"
        ),
        Some(2)
    );

    // And in an INSERT that takes its rows from a SELECT, which is the shape
    // `docs/embeddings.md` documents for writing a computed vector.
    connection
        .execute("CREATE TABLE m (id INTEGER PRIMARY KEY, weight INTEGER)")
        .expect("creates");
    connection
        .execute("INSERT INTO m (id, weight) SELECT id, negate(weight) FROM n")
        .expect("inserts from a select");
    assert_eq!(single(&connection, "SELECT sum(weight) FROM m"), Some(-60));
}

/// A registered scalar in an INSERT's `VALUES` is refused by name rather than
/// answered wrongly.
///
/// The write path builds its row space from a layout rather than from a
/// catalog, so there is no body to look up, and it says so with the
/// `unsupported` status rather than treating the function as absent. That is a
/// gap rather than a design - `docs/roadmap.md` records it - and this test is
/// here so that closing it is a deliberate change to a named expectation rather
/// than something that quietly starts working.
#[test]
fn a_registered_scalar_in_a_values_row_refuses_by_name() {
    let connection = connect();
    connection
        .create_scalar_function(
            "negate",
            1,
            FunctionFlags::external(),
            Arc::new(|arguments: &[Value<'static>]| {
                let value = arguments.first().and_then(Value::as_integer).unwrap_or(0);
                Ok(Value::Integer(-value))
            }),
        )
        .expect("registers");
    connection
        .execute("CREATE TABLE n (id INTEGER PRIMARY KEY, weight INTEGER)")
        .expect("creates");

    let refused = connection
        .execute("INSERT INTO n (id, weight) VALUES (1, negate(10))")
        .expect_err("the write path has no catalog to resolve the body through");
    let said = format!("{refused:?}");
    assert!(
        said.contains("negate"),
        "the refusal names the function: {said}"
    );
}

/// A registration replaces a built-in of the same name and arity.
#[test]
fn a_registration_overrides_a_builtin() {
    let connection = connect();
    connection
        .create_scalar_function(
            "abs",
            1,
            FunctionFlags::external(),
            Arc::new(|_| Ok(Value::Integer(-1))),
        )
        .expect("registers");
    assert_eq!(single(&connection, "SELECT abs(-5)"), Some(-1));
}

/// An aggregate reduces a group, and `GROUP BY` splits the groups.
#[test]
fn a_registered_aggregate_reduces_a_group() {
    let connection = connect();
    connection
        .create_aggregate_function(
            "total2",
            1,
            FunctionFlags::external(),
            Arc::new(|rows: &[Vec<Value<'static>>]| {
                let sum: i64 = rows
                    .iter()
                    .filter_map(|row| row.first())
                    .filter_map(Value::as_integer)
                    .sum();
                Ok(Value::Integer(sum))
            }),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE t(g, n); INSERT INTO t VALUES (1,10),(1,5),(2,7)")
        .expect("fills");
    let rows = connection
        .query("SELECT g, total2(n) FROM t GROUP BY g ORDER BY g")
        .expect("runs");
    let totals: Vec<i64> = rows
        .iter()
        .filter_map(|row| row.get(1))
        .filter_map(Value::as_integer)
        .collect();
    assert_eq!(totals, vec![15, 7]);
}

/// A collation an application defines changes what `ORDER BY` returns.
#[test]
fn a_registered_collation_orders_rows() {
    let connection = connect();
    connection
        .create_collation(
            "BYLEN",
            Arc::new(|left: &[u8], right: &[u8]| {
                left.len().cmp(&right.len()).then_with(|| left.cmp(right))
            }),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE w(x); INSERT INTO w VALUES ('bbb'),('a'),('cc')")
        .expect("fills");
    let rows = connection
        .query("SELECT x FROM w ORDER BY x COLLATE BYLEN")
        .expect("runs");
    let order: Vec<String> = rows
        .iter()
        .filter_map(|row| row.first())
        .filter_map(Value::as_text)
        .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
        .collect();
    assert_eq!(order, vec!["a", "cc", "bbb"]);
}

/// A collation a column declares is used by the index built over it.
#[test]
fn a_declared_collation_reaches_the_column() {
    let connection = connect();
    connection
        .create_collation(
            "REVERSED",
            Arc::new(|left: &[u8], right: &[u8]| right.cmp(left)),
        )
        .expect("registers");
    connection
        .execute_batch(
            "CREATE TABLE w(x TEXT COLLATE REVERSED); INSERT INTO w VALUES ('a'),('b'),('c')",
        )
        .expect("fills");
    let rows = connection
        .query("SELECT x FROM w ORDER BY x")
        .expect("runs");
    let order: Vec<String> = rows
        .iter()
        .filter_map(|row| row.first())
        .filter_map(Value::as_text)
        .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
        .collect();
    assert_eq!(order, vec!["c", "b", "a"]);
}

/// Removing a function makes its name unknown again.
#[test]
fn a_removed_function_is_unknown_again() {
    let connection = connect();
    connection
        .create_scalar_function(
            "gone",
            0,
            FunctionFlags::external(),
            Arc::new(|_| Ok(Value::Integer(1))),
        )
        .expect("registers");
    assert!(connection.query("SELECT gone()").is_ok());
    assert!(
        connection.remove_function("gone", 0),
        "the function was removed"
    );
    let refused = connection.query("SELECT gone()");
    assert!(refused.is_err(), "the name should be unknown again");
}

/// Returns a registration that turns a word into a four-component unit vector.
///
/// It stands in for `embed(TEXT)` in a test that must not depend on 522 MB of
/// weights being installed: the same shape - text in, a `VECTOR(N)` blob out -
/// through the same registration path.
///
/// The vector points at `[a, 1-a, 0, 0]` where `a` is the first byte of the
/// text divided by 255, so two different words give two different directions
/// and the same word always gives the same one.
fn toy_embedder(
) -> Arc<dyn Fn(&[Value<'static>]) -> inillucent_base::DbResult<Value<'static>> + Send + Sync> {
    Arc::new(|arguments: &[Value<'static>]| {
        let seed = match arguments.first() {
            Some(Value::Text(text)) => f32::from(text.raw().first().copied().unwrap_or(0)) / 255.0,
            _ => 0.0,
        };
        let mut bytes = Vec::with_capacity(16);
        for component in [seed, 1.0 - seed, 0.0, 0.0] {
            bytes.extend_from_slice(&component.to_bits().to_le_bytes());
        }
        Value::owned_blob(&bytes)
    })
}

/// A registered scalar can be the probe vector of a vector index.
///
/// **This was refused until task-1907, and the refusal appeared when the index
/// did.** `ORDER BY vector_distance_cos(v, embed('a question')) LIMIT k` over a
/// plain column works - `a_registered_scalar_reaches_order_by_and_where` is
/// that case. Put an `inillucent_hnsw` index on the column and the planner
/// turns the same query into a probe of that index, whose probe vector is
/// folded through a space that carried no catalog, so the function's body could
/// not be looked up and the whole statement came back `unsupported`. Creating
/// the index broke the query the index exists for, which is why this test
/// builds one.
#[test]
fn a_registered_scalar_is_the_probe_of_a_vector_index() {
    let connection = connect();
    connection
        .create_scalar_function("toy_embed", 1, FunctionFlags::external(), toy_embedder())
        .expect("registers");
    // The index is created before the rows on purpose: an index built over rows
    // that are already there is empty, which is `docs/roadmap.md` item 14 and
    // is pinned by the test below.
    connection
        .execute_batch(
            "CREATE TABLE p (id INTEGER PRIMARY KEY, word TEXT, v VECTOR(4));
             CREATE INDEX p_v ON p USING inillucent_hnsw (v);",
        )
        .expect("creates");
    for (id, word) in [(1, "alpha"), (2, "zeta"), (3, "mu"), (4, "spare")] {
        connection
            .execute(&format!(
                "INSERT INTO p (id, word, v) SELECT {id}, '{word}', toy_embed('{word}')"
            ))
            .expect("inserts");
    }

    let rows = connection
        .query("SELECT id FROM p ORDER BY vector_distance_cos(v, toy_embed('alpha')) LIMIT 1")
        .expect("a registered function resolves in an index probe");
    let nearest: Vec<i64> = rows
        .iter()
        .filter_map(|row| row.first().and_then(Value::as_integer))
        .collect();
    assert_eq!(
        nearest,
        vec![1],
        "the row embedded from the same word is the nearest one"
    );
}

/// A vector index does not keep the rows it was given, once the file is reopened.
///
/// **The wrong answer here is zero rows, and nothing raises.** `CREATE INDEX ...
/// USING inillucent_hnsw` is what `docs/vector-search.md` tells a reader to
/// build; `create_vector_index` reads the rows already in the table back and
/// applies them; and the store reports, in that same session, that it holds
/// them. Open the file again and it holds none. The planner then turns the
/// documented semantic search into a probe of an empty index, and an empty
/// probe answers zero rows rather than failing - so a search that worked before
/// the index existed silently stops working after.
///
/// The reopen is the whole test. The first pass of it asserted inside the
/// session that created the index, saw the rows the backfill had just written,
/// and concluded the backfill worked. `docs/roadmap.md` item 14 carries the
/// command-line reproduction, and the same shape at twenty rows, where an
/// insert keeps nineteen of them.
///
/// This asserts the wrong behaviour, the way
/// `a_registered_scalar_in_a_values_row_refuses_by_name` does, so that fixing it
/// is a deliberate change to a named expectation rather than something that
/// quietly starts working.
#[test]
fn a_vector_index_does_not_survive_a_reopen() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.connect().expect("connects");
        connection
            .create_scalar_function("toy_embed", 1, FunctionFlags::external(), toy_embedder())
            .expect("registers");
        connection
            .execute("CREATE TABLE p (id INTEGER PRIMARY KEY, v VECTOR(4))")
            .expect("creates");
        for (id, word) in [(1, "alpha"), (2, "zeta"), (3, "mu")] {
            connection
                .execute(&format!(
                    "INSERT INTO p (id, v) SELECT {id}, toy_embed('{word}')"
                ))
                .expect("inserts");
        }

        let before = connection
            .query("SELECT id FROM p ORDER BY vector_distance_cos(v, toy_embed('alpha')) LIMIT 3")
            .expect("runs without an index");
        assert_eq!(before.len(), 3, "an exhaustive scan answers every row");

        connection
            .execute("CREATE INDEX p_v ON p USING inillucent_hnsw (v)")
            .expect("creates the index");
        assert_eq!(
            held_rows(&connection),
            "3",
            "the backfill does run, and says so, in the session that ran it"
        );
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.connect().expect("connects");
    assert_eq!(
        held_rows(&connection),
        "0",
        "roadmap item 14: the backfilled rows do not survive the reopen. If this now reports          3 the defect is fixed - delete this test, and take the warning out of          examples/rag-agent and out of the roadmap"
    );
}

/// Returns what a vector index's own counters say it holds, rendered as text.
///
/// Rendered rather than read as one type: a check that asked for text and got
/// something else reported an empty string, which compared unequal to every
/// expectation and said nothing at all about the index.
///
/// @param connection - a connection to the database holding `p_v`
fn held_rows(connection: &inillucent_compat::facade::Connection) -> String {
    connection
        .query("SELECT v FROM p_v_state WHERE k = 'rows'")
        .expect("the store keeps its own counters")
        .first()
        .and_then(|row| row.first())
        .map(|value| match value {
            Value::Text(text) => String::from_utf8_lossy(text.raw()).into_owned(),
            Value::Integer(number) => number.to_string(),
            other => format!("{other:?}"),
        })
        .unwrap_or_else(|| "no counter at all".to_string())
}
