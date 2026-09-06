//! What the new engine's SQL surface answers, and what it still refuses.
//!
//! Invariant: **the deletion in Part 4 may not happen while this file lists a
//! refusal a shipped caller depends on.** `inillucent-storage`, `-transaction`
//! and `-vm` are still the path `inillucent::Database` and `inillucent-cli`
//! take, and deleting them means the new engine answers everything they
//! answered. This file is the inventory of that difference, measured by running
//! each construct rather than by reading the code, so the list cannot drift
//! from the engine the way a comment does.
//!
//! It is written to fail in **both** directions. A construct that starts
//! working fails here and asks to be moved, which is how the remaining work
//! gets counted down; a construct that stops working fails here too. Either way
//! the inventory in `_agent_output/task-1834-phase5/README.md` is wrong and
//! says so.
//!
//! The refusals are all *named* - each says which construct it is and that the
//! engine does not run it yet - so nothing here is a silent wrong answer. That
//! distinction is why this is an inventory and not a bug list.

use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// Whether the engine is expected to run a construct.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answers {
    /// The engine runs it.
    Yes,
    /// The engine refuses it, by name.
    NotYet,
}

use Answers::{NotYet, Yes};

/// Returns a database holding one small table, at a path of its own.
///
/// Each case gets its own file because several of them change the schema -
/// `ALTER TABLE t RENAME` renamed the table out from under every later case the
/// first time this was written, and turned twelve working constructs into
/// twelve "unsupported" lines.
///
/// @param name - the case's name, which names its file
fn fresh(name: &str) -> Database {
    let area = workspace_root().join("target/scratch/task-1834/surface");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    let database = Database::open(&path).expect("a fresh database opens");
    {
        let connection = database.connect();
        connection
            .execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER); \
                 INSERT INTO t(id, a, b) VALUES (1, 'x', 10); \
                 INSERT INTO t(id, a, b) VALUES (2, 'y', 20)",
            )
            .expect("the fixture loads");
    }
    database
}

/// Returns the number of rows in the fixture table.
///
/// @param connection - the connection to ask
fn count(connection: &Connection<'_>) -> i64 {
    match connection
        .query("SELECT count(*) FROM t")
        .expect("counting works")
        .first()
        .and_then(|row| row.first())
    {
        Some(OwnedDatum::Int(number)) => *number,
        other => panic!("count(*) answered {other:?}"),
    }
}

/// Every construct the inventory covers, and whether the engine runs it.
///
/// Ordered by subject rather than by verdict, so adding a construct is adding a
/// line next to its relatives instead of choosing a list to put it in.
const SURFACE: &[(&str, &str, Answers)] = &[
    // Reads. The analytical shapes the release gate measures all work.
    ("select", "SELECT a FROM t", Yes),
    ("where", "SELECT a FROM t WHERE b > 5", Yes),
    ("orderby.limit", "SELECT a FROM t ORDER BY a LIMIT 1", Yes),
    ("groupby", "SELECT b, count(*) FROM t GROUP BY b", Yes),
    (
        "having",
        "SELECT b FROM t GROUP BY b HAVING count(*) > 0",
        Yes,
    ),
    ("join", "SELECT t.a FROM t JOIN t AS u ON t.id = u.id", Yes),
    ("union", "SELECT a FROM t UNION SELECT a FROM t", Yes),
    ("cte", "WITH q AS (SELECT a FROM t) SELECT * FROM q", Yes),
    (
        "window",
        "SELECT a, row_number() OVER (ORDER BY id) FROM t",
        Yes,
    ),
    (
        "subquery.from",
        "SELECT m FROM (SELECT max(b) AS m FROM t)",
        Yes,
    ),
    ("like", "SELECT a FROM t WHERE a LIKE 'x%'", Yes),
    ("cast", "SELECT CAST(b AS TEXT) FROM t", Yes),
    ("collate", "SELECT a FROM t ORDER BY a COLLATE NOCASE", Yes),
    (
        "function.scalar",
        "SELECT abs(-1), length('ab'), upper('a')",
        Yes,
    ),
    ("function.date", "SELECT date('now')", Yes),
    ("function.json", "SELECT json_type('{}')", Yes),
    ("sqlite_schema", "SELECT name FROM sqlite_schema", Yes),
    // Writes.
    ("insert", "INSERT INTO t(id, a, b) VALUES (3, 'z', 30)", Yes),
    ("update", "UPDATE t SET a = 'q' WHERE id = 1", Yes),
    ("delete", "DELETE FROM t WHERE id = 2", Yes),
    (
        "upsert",
        "INSERT INTO t(id, a, b) VALUES (1, 'y', 2) ON CONFLICT(id) DO UPDATE SET a = 'z'",
        Yes,
    ),
    (
        "returning",
        "INSERT INTO t(id, a, b) VALUES (9, 'r', 1) RETURNING id",
        Yes,
    ),
    (
        "subquery.values",
        "INSERT INTO t(id, a, b) VALUES ((SELECT max(id) FROM t) + 1, 'z', 1)",
        Yes,
    ),
    (
        "subquery.set",
        "UPDATE t SET a = (SELECT a FROM t WHERE id = 1) WHERE id = 2",
        Yes,
    ),
    // Schema.
    ("create.index", "CREATE INDEX ix ON t(a)", Yes),
    ("create.view", "CREATE VIEW v AS SELECT a FROM t", Yes),
    (
        "create.trigger",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END",
        Yes,
    ),
    ("drop.table", "DROP TABLE t", Yes),
    ("alter.rename", "ALTER TABLE t RENAME TO t2", Yes),
    ("alter.add", "ALTER TABLE t ADD COLUMN c TEXT", Yes),
    ("analyze", "ANALYZE", Yes),
    ("reindex", "REINDEX", Yes),
    // Pragmas.
    ("pragma.page_size", "PRAGMA page_size", Yes),
    ("pragma.journal_mode", "PRAGMA journal_mode", Yes),
    ("pragma.table_info", "PRAGMA table_info(t)", Yes),
    ("pragma.foreign_keys", "PRAGMA foreign_keys=ON", Yes),
    (
        "explain.query_plan",
        "EXPLAIN QUERY PLAN SELECT a FROM t",
        Yes,
    ),
    // Subqueries used as values. Uncorrelated ones are folded once per
    // execution; `crates/inillucent-compat/tests/new_engine_subquery.rs` is
    // where their answers are checked against SQLite's.
    ("subquery.scalar", "SELECT (SELECT max(b) FROM t) AS m", Yes),
    (
        "subquery.where",
        "SELECT a FROM t WHERE b = (SELECT max(b) FROM t)",
        Yes,
    ),
    (
        "subquery.in",
        "SELECT a FROM t WHERE id IN (SELECT id FROM t)",
        Yes,
    ),
    (
        "subquery.exists",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM t WHERE id = 1)",
        Yes,
    ),
    // The refusals. Each of these stands between Phase 5 and Part 4.
    (
        "subquery.correlated",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM t AS u WHERE u.id = t.id)",
        NotYet,
    ),
    ("rollback", "ROLLBACK", NotYet),
    ("savepoint", "SAVEPOINT s1", NotYet),
    ("release", "RELEASE s1", NotYet),
    ("attach", "ATTACH DATABASE ':memory:' AS other", NotYet),
    ("vacuum", "VACUUM", NotYet),
    // Plain EXPLAIN is refused for a reason that is not "yet": see the two
    // classes of refusal above.
    ("explain", "EXPLAIN SELECT a FROM t", NotYet),
];

/// Every construct answers the way the inventory says it does.
#[test]
fn the_new_engine_answers_what_the_inventory_says_it_answers() {
    let mut wrong = Vec::new();
    for (index, (name, sql, expected)) in SURFACE.iter().enumerate() {
        let database = fresh(&format!("case{index}"));
        let connection = database.connect();
        let actual = match connection.query(sql) {
            Ok(_) => Yes,
            Err(error) => {
                // A refusal has to name itself. An engine that answered
                // "something went wrong" would be one a caller could not act
                // on, and one this inventory could not tell apart from a bug.
                //
                // There are two classes of refusal here and the difference is
                // worth keeping. Most say "not yet" - the construct is
                // unimplemented and the phrase is a promise to whoever
                // implements it. Plain `EXPLAIN` says something else: it lists
                // a bytecode program's opcodes and this engine compiles no
                // bytecode, so it is not waiting on anybody. A refusal that
                // explains why it can never be answered in that form is a
                // better refusal, not a worse one, so it is accepted here on
                // its own terms rather than made to pretend it is pending.
                let text = format!("{error:?}");
                assert!(
                    text.contains("does not run yet")
                        || text.contains("does not handle")
                        || text.contains("Unsupported")
                        || text.contains("compiles no bytecode"),
                    "{name} was refused without naming what it refused: {text}"
                );
                NotYet
            }
        };
        if actual != *expected {
            wrong.push(format!(
                "  {name:24} inventory says {expected:?}, engine says {actual:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "the inventory in this file and in the Phase 5 report no longer matches the \
         engine.\nMove each of these rows and update the report's Part 4 table:\n{}",
        wrong.join("\n")
    );
}

/// `BEGIN` is accepted and `ROLLBACK` is refused, which is the load-bearing gap.
///
/// This is separate from the table above because it is not one more construct:
/// it is the reason Part 4 cannot proceed. `BEGIN` maps to the engine's *log
/// batching*, which groups writes so the log is flushed once. That is a
/// durability grouping, not atomicity, and it has nothing to undo with.
///
/// The consequence is a caller that can enter a transaction and cannot abandon
/// one, and `inillucent-cli`'s own import path is such a caller: it wraps a
/// load in `BEGIN` and issues `ROLLBACK` when a row fails to insert. Moving it
/// onto this engine would turn a failed import from "nothing happened" into
/// "half the file is in the table".
///
/// The gap is structural rather than unwired. `inillucent-txn` has the whole
/// mechanism - `savepoint`, `rollback_to`, `release`, `rollback`, an undo
/// buffer and an `UndoSink` - but `ImportedDatabase` does not route its writes
/// through it, and the write-ahead log is redo-only: `InsertRow`, `DeleteRow`
/// and `UpdateInPlace` carry an after-image and no before-image. Rolling back
/// therefore cannot be done by replaying the log backwards. It needs the undo
/// buffer populated on every write, and the catalog's own changes covered too.
#[test]
fn a_transaction_can_be_entered_and_not_abandoned() {
    let database = fresh("txn");
    let connection = database.connect();
    assert_eq!(count(&connection), 2);

    connection
        .execute_batch("BEGIN")
        .expect("BEGIN is accepted");
    connection
        .execute_batch("INSERT INTO t(id, a, b) VALUES (3, 'z', 30)")
        .expect("the insert applies");
    assert_eq!(
        count(&connection),
        3,
        "the write is visible inside the batch"
    );

    assert!(
        connection.query("ROLLBACK").is_err(),
        "ROLLBACK now works - implement the Part 4 inventory row and retire this test's premise"
    );
    assert_eq!(
        count(&connection),
        3,
        "the write the caller tried to abandon is still there"
    );

    // The other half: COMMIT is accepted, so the pair a caller writes is
    // half-implemented rather than absent. That asymmetry is the finding.
    connection
        .execute_batch("COMMIT")
        .expect("COMMIT is accepted");
    assert_eq!(count(&connection), 3);
}
