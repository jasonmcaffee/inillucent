//! Functions and collations an application defines, compared against SQLite.
//!
//! Invariant: a registered function behaves like a built-in everywhere a
//! built-in works. The interesting cases are not "does it get called" but the
//! ones where a function's *kind* matters: an aggregate has to reach `GROUP BY`
//! and `ORDER BY`, a registration has to override a built-in of the same name,
//! and a collation has to change what an `ORDER BY` returns rather than only
//! what a comparison answers.

use std::sync::atomic::{AtomicI64, Ordering};
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

/// A registered scalar reaches the write path: an INSERT's `VALUES` row, an
/// `UPDATE`'s assignment, and an INSERT's `RETURNING` clause can all call one.
///
/// **This was refused, by name, until `docs/roadmap.md` item 13 was closed.**
/// The physical pass resolves a registered function's body through the
/// catalog, and the write path's `RowSpace` used to carry none at all - not
/// because it cannot hold one, but because a `RowSpace` is threaded through
/// about a dozen signatures and a borrowed field would put a lifetime on every
/// one of them. The fix is a catalog **parameter** on `RowSpace::compile` and
/// on the handful of callers that reach it, every one of which already holds a
/// `WriteTarget` and therefore `WriteTarget::catalog()` - the same view a
/// trigger body's own queries already used.
///
/// Each shape is checked against the same call made in a projection, which
/// already worked before this fix -
/// `a_registered_scalar_reaches_order_by_and_where` is that test - so a write
/// path that stored something arbitrary instead of the function's real answer
/// would be caught here rather than passing on a value nobody checked.
#[test]
fn a_registered_scalar_reaches_the_write_path() {
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
        .execute("CREATE TABLE note (id INTEGER PRIMARY KEY, body INTEGER, v INTEGER)")
        .expect("creates");

    // `INSERT INTO note (...) VALUES (..., negate(?1))`.
    connection
        .execute("INSERT INTO note (id, body, v) VALUES (1, 10, negate(10))")
        .expect("a VALUES row may call a registered scalar");
    assert_eq!(
        single(&connection, "SELECT v FROM note WHERE id = 1"),
        single(&connection, "SELECT negate(10)"),
        "the stored value is the same call made in a projection, not something arbitrary"
    );

    // `UPDATE note SET v = negate(body)`.
    connection
        .execute("UPDATE note SET v = negate(body) WHERE id = 1")
        .expect("an UPDATE assignment may call a registered scalar");
    assert_eq!(
        single(&connection, "SELECT v FROM note WHERE id = 1"),
        single(&connection, "SELECT negate(10)"),
        "negate(body) in the assignment read the row's own body, which is still 10"
    );

    // `INSERT INTO note (...) VALUES (...) RETURNING negate(body)`.
    let returned = connection
        .query("INSERT INTO note (id, body) VALUES (2, 5) RETURNING negate(body)")
        .expect("a RETURNING clause may call a registered scalar");
    let returned_value = returned
        .first()
        .and_then(|row| row.first())
        .and_then(Value::as_integer);
    assert_eq!(
        returned_value,
        single(&connection, "SELECT negate(5)"),
        "RETURNING answers the same call a projection would"
    );
}

/// A deterministic registered scalar over arguments that read no column is
/// called once - not once per row - and one that reads a column still runs
/// once for every row. `docs/roadmap.md` item 15: `embed('search_query: ' ||
/// ?1)` in an `ORDER BY` used to call the embedding model once for every one
/// of 2,661 rows of `examples/rag-agent`, 64 seconds of a 65-second query,
/// every call answering the same embedding - because nothing distinguished a
/// call whose arguments cannot vary by row from one that reads the row being
/// scanned.
///
/// A wall-clock reading is a measurement of the machine as much as of the
/// code - `tests/inillucent-testing-tdd.md` §1.7 - so this counts calls
/// instead of timing them: a registered function that stamps a shared counter
/// every time its body actually runs, read back after each shape.
#[test]
fn a_deterministic_scalar_is_called_once_unless_it_reads_a_column() {
    let connection = connect();
    connection
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER);
             INSERT INTO t (id, val) VALUES (1, 10), (2, 20), (3, 30), (4, 40);",
        )
        .expect("fills");
    let calls = Arc::new(AtomicI64::new(0));
    let counting = Arc::clone(&calls);
    connection
        .create_scalar_function(
            "counted",
            1,
            FunctionFlags {
                deterministic: true,
                ..FunctionFlags::external()
            },
            Arc::new(move |_arguments: &[Value<'static>]| {
                counting.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Integer(1))
            }),
        )
        .expect("registers");

    // All-literal arguments: a constant regardless of which execution asked,
    // so `translate` folds it once and the projection never calls it again.
    calls.store(0, Ordering::SeqCst);
    connection.query("SELECT counted(1) FROM t").expect("runs");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a literal argument is folded once, not once per one of the table's 4 rows"
    );

    // The same fold in the shape the roadmap measured: an ORDER BY key.
    calls.store(0, Ordering::SeqCst);
    connection
        .query("SELECT id FROM t ORDER BY counted(1) LIMIT 4")
        .expect("runs");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "an ORDER BY key over a literal argument is folded once, not once per row of the scan"
    );

    // A bound parameter: a constant for this execution, folded at execution
    // setup rather than run for every row - but not folded forever, which the
    // literal case above is.
    calls.store(0, Ordering::SeqCst);
    let mut statement = connection
        .prepare("SELECT counted(?1) FROM t")
        .expect("prepares");
    statement.bind_integer(1, 7).expect("binds");
    while statement.step().expect("steps") {}
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a bound parameter is folded once for this execution, not once per row"
    );

    // A column reference: genuinely different per row, so it must run once
    // per row - the one shape this may never fold.
    calls.store(0, Ordering::SeqCst);
    connection
        .query("SELECT counted(val) FROM t")
        .expect("runs");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "a column argument differs per row and has to be evaluated once for each of the table's 4 rows"
    );
}

/// A registered scalar that has not promised `FunctionFlags::deterministic`
/// is never folded, even when every one of its arguments is a literal.
///
/// The default a caller gets from `FunctionFlags::external()` is
/// `deterministic: false` - the safe assumption about a function nobody here
/// wrote - and folding one anyway would call something like `random()` once
/// for a whole scan instead of once per row. This is the same counting
/// scalar as `a_deterministic_scalar_is_called_once_unless_it_reads_a_column`,
/// with the one flag left off.
#[test]
fn a_non_deterministic_scalar_is_never_folded() {
    let connection = connect();
    connection
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY);
             INSERT INTO t (id) VALUES (1), (2), (3);",
        )
        .expect("fills");
    let calls = Arc::new(AtomicI64::new(0));
    let counting = Arc::clone(&calls);
    connection
        .create_scalar_function(
            "not_deterministic",
            1,
            FunctionFlags::external(),
            Arc::new(move |_arguments: &[Value<'static>]| {
                counting.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Integer(1))
            }),
        )
        .expect("registers");

    connection
        .query("SELECT not_deterministic(1) FROM t")
        .expect("runs");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "with no deterministic promise, even a literal argument runs once per row rather than being folded"
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

/// A vector index keeps the rows it was given, across a reopen.
///
/// **This asserted the opposite until task-1911**, because until task-1911 a
/// `CREATE INDEX ... USING inillucent_hnsw` over a full table built an index
/// that held every row in the session that built it and none the next time the
/// file was opened - and an empty vector index answers zero rows rather than
/// failing, so the documented semantic search silently stopped working. There
/// were two faults under it, both about a write that is never committed:
///
/// 1. `create_vector_index` returned without the `seal()` every other directive
///    ends with, so the backfill was logged and no commit record followed it.
/// 2. `ImportedDatabase::write` reads `next_txn` and moves it on at once, so
///    `current_txn()` answered the *next* number for the rest of the statement -
///    and `follow_vector_indexes`, which runs after the trees are released,
///    logged every index entry into a transaction nothing commits.
///
/// The second one is why "an insert loses its last row" was the shape this was
/// first seen in: inside a batch each statement's lost entry is rescued by the
/// following statement claiming that number, so only the last one stays lost.
/// Outside a batch **every** insert lost its entry, which is what
/// `every_insert_into_an_indexed_table_reaches_the_index` covers.
///
/// The reopen is the whole test. The first pass of it asserted inside the
/// session that created the index, saw the rows the backfill had just written,
/// and concluded the backfill worked.
#[test]
fn a_vector_index_survives_a_reopen() {
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
            "the backfill runs, and says so, in the session that ran it"
        );
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.connect().expect("connects");
    connection
        .create_scalar_function("toy_embed", 1, FunctionFlags::external(), toy_embedder())
        .expect("registers");
    assert_eq!(
        held_rows(&connection),
        "3",
        "the backfilled rows survive the reopen"
    );
    // **And the search answers them**, which is what a reader actually does.
    // The counter surviving while the probe returned nothing would be the same
    // wrong answer wearing a different number.
    let after = connection
        .query("SELECT id FROM p ORDER BY vector_distance_cos(v, toy_embed('alpha')) LIMIT 3")
        .expect("searches through the index");
    assert_eq!(
        after.len(),
        3,
        "the search through the index answers the rows the index holds"
    );
    assert_eq!(
        after
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(1),
        "the row embedded from the same word is still the nearest one"
    );
}

/// Every insert into an indexed table reaches the index, not all but the last.
///
/// **Each insert is its own statement and its own transaction**, which is the
/// case the reproduction in `docs/roadmap.md` item 14 never ran: it used one
/// batch, where each statement's lost index entry happens to be rescued by the
/// next statement claiming the transaction number it was logged under. That made
/// a fault which loses **every** entry look like one that loses the last. Five
/// inserts through five statements left the store reading `rows 0` before
/// task-1911.
///
/// The index is built first and empty here, so nothing a backfill does can hide
/// a write path that does not maintain it.
#[test]
fn every_insert_into_an_indexed_table_reaches_the_index() {
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
        connection
            .execute("CREATE INDEX p_v ON p USING inillucent_hnsw (v)")
            .expect("creates the index over an empty table");
        for (id, word) in [(1, "alpha"), (2, "zeta"), (3, "mu"), (4, "nu"), (5, "xi")] {
            connection
                .execute(&format!(
                    "INSERT INTO p (id, v) SELECT {id}, toy_embed('{word}')"
                ))
                .expect("inserts");
        }
        assert_eq!(held_rows(&connection), "5", "all five reach the store");
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.connect().expect("connects");
    assert_eq!(
        held_rows(&connection),
        "5",
        "all five survive the reopen - not four, which is what losing only the last would leave"
    );
}

/// A backfilled index takes the inserts that come after it.
///
/// `docs/roadmap.md` item 14 recorded that "a backfilled index does not recover,
/// either: later inserts into that table do not reach it, and it stays at zero
/// for good". That was the same fault seen from the other side - the backfill
/// was lost and so was every insert after it - rather than a third one, and this
/// asserts the pair works together: the rows that were there when the index was
/// built and the rows that arrived afterwards are all in it after a reopen.
#[test]
fn a_backfilled_index_takes_later_inserts() {
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
        for (id, word) in [(1, "alpha"), (2, "zeta")] {
            connection
                .execute(&format!(
                    "INSERT INTO p (id, v) SELECT {id}, toy_embed('{word}')"
                ))
                .expect("inserts");
        }
        connection
            .execute("CREATE INDEX p_v ON p USING inillucent_hnsw (v)")
            .expect("creates the index over two rows");
        for (id, word) in [(3, "mu"), (4, "nu")] {
            connection
                .execute(&format!(
                    "INSERT INTO p (id, v) SELECT {id}, toy_embed('{word}')"
                ))
                .expect("inserts after the backfill");
        }
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.connect().expect("connects");
    connection
        .create_scalar_function("toy_embed", 1, FunctionFlags::external(), toy_embedder())
        .expect("registers");
    assert_eq!(
        held_rows(&connection),
        "4",
        "the two backfilled rows and the two that came after are all in the index"
    );
    let found = connection
        .query("SELECT id FROM p ORDER BY vector_distance_cos(v, toy_embed('nu')) LIMIT 1")
        .expect("searches");
    assert_eq!(
        found
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(4),
        "a row inserted after the backfill is findable through the index"
    );
}

/// A delete takes its row out of the index, across a reopen.
///
/// The other half of what `follow_vector_indexes` does, and it went the same way
/// for the same reason. A removal that never commits leaves a row the table no
/// longer holds answerable by the index, which is a wrong answer rather than a
/// missing one.
#[test]
fn a_delete_leaves_the_index_without_the_row() {
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
        connection
            .execute("CREATE INDEX p_v ON p USING inillucent_hnsw (v)")
            .expect("creates the index");
        for (id, word) in [(1, "alpha"), (2, "zeta"), (3, "mu")] {
            connection
                .execute(&format!(
                    "INSERT INTO p (id, v) SELECT {id}, toy_embed('{word}')"
                ))
                .expect("inserts");
        }
        connection
            .execute("DELETE FROM p WHERE id = 2")
            .expect("deletes");
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.connect().expect("connects");
    assert_eq!(
        held_rows(&connection),
        "2",
        "the deleted row is out of the index after the reopen, not back in it"
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
