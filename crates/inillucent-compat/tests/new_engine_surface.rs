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
//! the recorded inventory of what still needs to move is wrong and says so.
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
    let area = workspace_root().join("target/scratch/surface");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    inillucent_base::testing::remove_database(&path);
    let database = Database::open(&path).expect("a fresh database opens");
    {
        let connection = database.session();
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
    // Was `Yes`, went to `NotYet` on the `inillucent-compat` re-point onto
    // `inillucent_engine::connect`, and is `Yes` again as of task-1932. The
    // inventory was right both times and it is worth saying what it actually
    // caught: not a missing evaluator, but a refusal one layer above one.
    // `compiled::try_compile` bailed out on `plan.compounds` and had no check
    // for `plan.select.windows`, so re-pointing onto `connect` - which goes
    // through the cached path, as every application does - started hitting
    // that refusal where `run_with` had gone to `run_windowed` and answered.
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
    ("pragma.database_list", "PRAGMA database_list", Yes),
    // The *table-valued* form, which is what a tool writes when it wants to
    // join against a pragma. `.databases` in the shell is written this way.
    // The eponymous form binds, and a `pragma_*`
    // function's rows come from the same `pragma_rows` the directive runs.
    (
        "pragma.table_valued",
        "SELECT name FROM pragma_database_list",
        Yes,
    ),
    // Abandoning a transaction. `crates/inillucent-compat/tests/new_engine_rollback.rs`
    // is where the undo is checked; these rows only record that the statements
    // are answered.
    ("savepoint", "SAVEPOINT s1", Yes),
    ("release", "SAVEPOINT s1; RELEASE s1", Yes),
    ("rollback", "BEGIN; ROLLBACK", Yes),
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
        Yes,
    ),
    ("attach", "ATTACH DATABASE ':memory:' AS other", Yes),
    // `VACUUM` folds the log into the file and `VACUUM INTO`
    // writes a verified copy, which is how a backup is taken.
    ("vacuum", "VACUUM", Yes),
    // Plain `EXPLAIN` answers. It lists the operator chain the
    // statement runs, in the eight columns SQLite lists opcodes in - see
    // `new_engine_explain::plain_explain_lists_the_chain_in_the_references_columns`.
    ("explain", "EXPLAIN SELECT a FROM t", Yes),
];

/// Every construct answers the way the inventory says it does.
#[test]
fn the_new_engine_answers_what_the_inventory_says_it_answers() {
    let mut wrong = Vec::new();
    for (index, (name, sql, expected)) in SURFACE.iter().enumerate() {
        let database = fresh(&format!("case{index}"));
        let connection = database.session();
        let actual = match connection.query(sql) {
            Ok(_) => Yes,
            Err(error) => {
                // **A refusal has to say something of its own.** The check used
                // to match a list of phrases, and the list grew every time a
                // refusal was worded well - "does not run yet", "does not
                // handle", "compiles no bytecode", "no such table" - which made
                // it a test of the wording rather than of the property.
                //
                // The property is that the engine said *something specific*. A
                // `DbError` carries the text of its primary code in `message`,
                // which for everything refused here is the same sentence -
                // "bad parameter or other API misuse" - and anything the engine
                // has to say about *this* statement is in `detail`. A refusal
                // with no detail is one a caller cannot act on and one this
                // inventory could not tell apart from a bug.
                assert!(
                    error.detail().is_some(),
                    "{name} was refused with nothing but its error code, which no caller can act on"
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

/// A trigger is refused, by name, rather than stored and never fired.
///
/// A trigger is stored **and fires**, which is what makes storing it honest.
///
/// **This was the one silent wrong answer the inventory found.** For a while
/// `CREATE TRIGGER` succeeded, `sqlite_schema` listed it, `new_engine_ddl.rs`
/// confirmed it was stored byte for byte as SQLite stores it - and an insert
/// did not run it, and nothing said so. Task-1834 turned that into a refusal,
/// on the reasoning that a database whose triggers do not fire is one whose
/// invariants are not being maintained and the application finds out from its
/// data.
///
/// Task-1838 built the firing point, so the refusal is gone and the storage is
/// no longer the interesting half: what this test asserts is that the trigger
/// *runs*. The same mechanism enforces foreign keys, because the binder turns
/// a `REFERENCES` clause into `CREATE TRIGGER` text - so there is one path and
/// `foreign_keys.rs` grades it against the pinned shell.
#[test]
fn a_trigger_is_stored_and_fires() {
    let database = fresh("trigger");
    let connection = database.session();
    // `fresh` already built `t` with two rows in it.
    connection
        .execute_batch("CREATE TABLE log (id INTEGER PRIMARY KEY, a INTEGER)")
        .expect("the log table is created");

    connection
        .query(
            "CREATE TRIGGER after_insert AFTER INSERT ON t BEGIN              INSERT INTO log(id, a) VALUES (NEW.id, NEW.b); END",
        )
        .expect("the trigger is stored");

    // It is in the schema, under the name it was written with.
    assert!(
        !connection
            .query("SELECT name FROM sqlite_schema WHERE name = 'after_insert'")
            .expect("the schema is queryable")
            .is_empty(),
        "the stored trigger is not in the schema"
    );

    // And it fires, with `NEW` carrying the row that was written.
    connection
        .execute_batch("INSERT INTO t(id, b) VALUES (7, 42)")
        .expect("the insert runs");
    assert_eq!(
        connection
            .query("SELECT id, a FROM log")
            .expect("the log reads"),
        vec![vec![OwnedDatum::Int(7), OwnedDatum::Int(42)]],
        "the trigger did not write what NEW held"
    );

    // Dropping it stops it firing, which is the other half of storing one.
    connection
        .execute_batch("DROP TRIGGER after_insert; INSERT INTO t(id, b) VALUES (8, 43)")
        .expect("the trigger is dropped and the insert runs");
    assert_eq!(
        count_of(&connection, "log"),
        1,
        "a dropped trigger still fired"
    );
}

/// Returns how many rows a table holds.
///
/// @param connection - the connection to ask
/// @param table - the table's name
fn count_of(connection: &Connection<'_>, table: &str) -> i64 {
    match connection
        .query(&format!("SELECT count(*) FROM {table}"))
        .expect("counting works")
        .first()
        .and_then(|row| row.first())
    {
        Some(OwnedDatum::Int(number)) => *number,
        other => panic!("count(*) answered {other:?}"),
    }
}

/// `PRAGMA table_info` answers nothing for a view.
///
/// A view's columns are the result columns of its `SELECT`, and the catalog
/// does not resolve them: `TableInfo.columns` is empty for a view, so the
/// pragma that reads it has nothing to report. Selecting *through* the view
/// works - the binder resolves the statement when it runs - so this is a gap in
/// what the schema can be *asked*, not in what it can answer.
///
/// It shows up as `.schema` printing `/* loud() */` where SQLite prints
/// `/* loud(shout) */`, which is how it was found.
#[test]
fn a_views_columns_are_reported() {
    let database = fresh("viewcols");
    let connection = database.session();
    connection
        .execute_batch("CREATE VIEW loud AS SELECT a AS shout FROM t")
        .expect("the view is created");

    assert_eq!(
        connection
            .query("SELECT shout FROM loud ORDER BY shout")
            .expect("the view is queryable")
            .len(),
        2,
        "the view itself does not resolve"
    );
    // **They are reported now.** `PRAGMA table_info` on a view answered nothing
    // at all, so an ORM reading it could not see a view's shape; it now binds
    // the view's `SELECT` and answers its columns, which is what the
    // reference does.
    assert_eq!(
        connection
            .query("PRAGMA table_info(loud)")
            .expect("the pragma runs")
            .len(),
        1,
        "a view has the columns its SELECT produces"
    );
}

/// A transaction can be entered and abandoned, and the abandonment sticks.
///
/// This file used to assert the opposite, and the reason is worth keeping. The
/// engine accepted `BEGIN` and refused `ROLLBACK`, because `BEGIN` mapped to
/// its *log batching* - a durability grouping that flushes the log once, with
/// nothing to undo with. A caller could enter a transaction and could not leave
/// one, and `inillucent-cli`'s import path is such a caller: it wraps a load in
/// `BEGIN` and issues `ROLLBACK` when a row fails. That was named here as the
/// thing standing between Phase 5 and Part 4.
///
/// It is answered now. The before-images are collected while a transaction is
/// open - the log is redo-only and cannot be replayed backwards - and the
/// restores are themselves logged, so the rollback survives the next open. The
/// details are in `new_engine_rollback.rs`; what this file records is that the
/// gap it was written to describe is closed.
#[test]
fn a_transaction_can_be_entered_and_abandoned() {
    let database = fresh("txn");
    let connection = database.session();
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
        "the write is visible inside the transaction"
    );

    connection
        .query("ROLLBACK")
        .expect("the transaction is abandoned");
    assert_eq!(
        count(&connection),
        2,
        "the write the caller abandoned is still there"
    );

    // And the other half, so a rollback-on-commit would not pass.
    connection
        .execute_batch("BEGIN")
        .expect("BEGIN is accepted");
    connection
        .execute_batch("INSERT INTO t(id, a, b) VALUES (4, 'w', 40)")
        .expect("the insert applies");
    connection
        .execute_batch("COMMIT")
        .expect("COMMIT is accepted");
    assert_eq!(count(&connection), 3);
}
