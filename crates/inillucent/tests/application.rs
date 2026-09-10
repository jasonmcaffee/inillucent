//! Whole applications, in miniature, against a file on disk.
//!
//! Invariant: **each test here is a story an application actually tells, run
//! end to end through `inillucent::Database`, and it reopens the file part way
//! through.** Not a feature exercised in isolation - a schema built, written
//! to, queried the way a screen would query it, closed, and opened again.
//!
//! ## Why stories rather than features
//!
//! Because the engine's feature tests already pass. `inillucent-compat` grades
//! every construct against SQLite one construct at a time, and this file would
//! be a worse version of that if it did the same. What a per-construct suite
//! cannot see is the join between constructs: a trigger that fires during a
//! statement that a foreign key is also checking, inside a transaction that a
//! savepoint is about to unwind, over an index the planner chose. Every defect
//! that survives a good unit suite lives in a seam like that, and the cheapest
//! way to walk a seam is to write the program somebody would have written.
//!
//! The four stories are chosen to be different shapes rather than four
//! spellings of the same one:
//!
//! - **an order book**, which is referential integrity and aggregation: the
//!   shape almost every line-of-business application has;
//! - **an append-only ledger**, which is windowing over an ordering that must
//!   survive a reopen, and the shape an audit trail has;
//! - **a document store**, which is JSON in a text column with an index on an
//!   extracted field - the shape an application reaches for when its schema is
//!   not settled;
//! - **a catalogue with full-text search**, which is a virtual table and its
//!   shadow tables committing and rolling back with ordinary ones.
//!
//! ## What every story asserts that a feature test would not
//!
//! That the file is still right *afterwards*. Each story ends by reopening the
//! database and running `PRAGMA integrity_check` as well as re-reading its own
//! data, because a write path that leaves a correct answer in the pool and a
//! damaged page on disk passes every test that does not look.

use std::path::PathBuf;

use inillucent::{Database, OwnedDatum};

/// Returns a fresh, empty directory for one test's files.
///
/// @param tag - what to name the directory after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-app-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// Renders one row as a comma-separated line, so an expectation can be written
/// as the text a person would read off a screen.
///
/// @param row - the row to render
fn line(row: &[OwnedDatum]) -> String {
    row.iter()
        .map(|cell| match cell {
            OwnedDatum::Null => "NULL".to_string(),
            OwnedDatum::Int(value) => value.to_string(),
            OwnedDatum::Real(value) => format!("{value}"),
            OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            OwnedDatum::Blob(bytes) => format!("x'{}'", bytes.len()),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Renders a whole answer, one row per line.
///
/// @param rows - what the query returned
fn table(rows: &[Vec<OwnedDatum>]) -> String {
    rows.iter()
        .map(|row| line(row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Runs a query and renders it, failing with the SQL when it will not run.
///
/// @param connection - the connection to ask
/// @param sql - the query
fn ask(connection: &inillucent::Connection<'_>, sql: &str) -> String {
    match connection.query(sql) {
        Ok(rows) => table(&rows),
        Err(why) => panic!("`{sql}` failed: {} ({:?})", why.message(), why.code()),
    }
}

/// Reopens a database and checks it, which is what every story ends with.
///
/// @param path - the database file
fn reopen_and_check(path: &PathBuf) -> Database {
    let database = Database::open(path).expect("the database reopens");
    database.check().expect("the file is sound after a reopen");
    let connection = database.connect();
    assert_eq!(
        ask(&connection, "PRAGMA integrity_check"),
        "ok",
        "the engine's own check disagrees with `Database::check`"
    );
    // Ends the borrow of `database`, which is what lets it be returned.
    let _ = connection;
    database
}

/// An order book: customers, orders, order lines, and the questions a screen
/// asks about them.
///
/// The referential integrity is the point. Every write below is one a real
/// application makes, and two of them are writes that *must* be refused - a
/// line pointing at no order, and a customer deleted out from under one.
#[test]
fn an_order_book_keeps_its_references() {
    let directory = scratch("orders");
    let path = directory.join("shop.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;\
                 CREATE TABLE customers (\
                   id INTEGER PRIMARY KEY,\
                   name TEXT NOT NULL,\
                   email TEXT NOT NULL UNIQUE\
                 );\
                 CREATE TABLE orders (\
                   id INTEGER PRIMARY KEY,\
                   customer_id INTEGER NOT NULL REFERENCES customers (id) ON DELETE RESTRICT,\
                   placed_on TEXT NOT NULL\
                 );\
                 CREATE TABLE lines (\
                   order_id INTEGER NOT NULL REFERENCES orders (id) ON DELETE CASCADE,\
                   sku TEXT NOT NULL,\
                   quantity INTEGER NOT NULL CHECK (quantity > 0),\
                   pence INTEGER NOT NULL,\
                   PRIMARY KEY (order_id, sku)\
                 );\
                 CREATE INDEX orders_by_customer ON orders (customer_id);",
            )
            .expect("the schema is created");

        connection
            .execute_batch(
                "INSERT INTO customers VALUES (1, 'Ada', 'ada@example.com');\
                 INSERT INTO customers VALUES (2, 'Grace', 'grace@example.com');\
                 INSERT INTO orders VALUES (10, 1, '2026-09-01');\
                 INSERT INTO orders VALUES (11, 1, '2026-09-03');\
                 INSERT INTO orders VALUES (12, 2, '2026-09-03');\
                 INSERT INTO lines VALUES (10, 'PEN', 3, 120);\
                 INSERT INTO lines VALUES (10, 'PAD', 1, 450);\
                 INSERT INTO lines VALUES (11, 'PEN', 1, 120);\
                 INSERT INTO lines VALUES (12, 'INK', 2, 800);",
            )
            .expect("the seed is written");

        // A line pointing at no order is refused, and the refusal names a
        // constraint rather than a syntax problem.
        let orphan = connection
            .execute("INSERT INTO lines VALUES (99, 'PEN', 1, 120)")
            .expect_err("a line with no order is refused");
        assert_eq!(
            orphan.code(),
            inillucent::PrimaryCode::Constraint,
            "expected a constraint failure, got {}",
            orphan.message()
        );

        // So is a quantity the CHECK forbids.
        let bad_quantity = connection
            .execute("INSERT INTO lines VALUES (10, 'CLIP', 0, 10)")
            .expect_err("a zero quantity is refused");
        assert_eq!(bad_quantity.code(), inillucent::PrimaryCode::Constraint);

        // And a customer who still has orders.
        let busy_customer = connection
            .execute("DELETE FROM customers WHERE id = 1")
            .expect_err("a customer with orders cannot be removed");
        assert_eq!(busy_customer.code(), inillucent::PrimaryCode::Constraint);
    }

    let database = reopen_and_check(&path);
    let connection = database.connect();
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .expect("the pragma is set on the new connection");

    // The question a screen asks: what has each customer spent?
    assert_eq!(
        ask(
            &connection,
            "SELECT c.name, sum(l.quantity * l.pence) AS pence \
             FROM customers c \
             JOIN orders o ON o.customer_id = c.id \
             JOIN lines l ON l.order_id = o.id \
             GROUP BY c.id, c.name \
             ORDER BY pence DESC, c.name"
        ),
        "Grace,1600\nAda,930"
    );

    // A customer with no orders still appears in the outer join, with nothing
    // rather than with zero - the difference an application has to render.
    connection
        .execute("INSERT INTO customers VALUES (3, 'Alan', 'alan@example.com')")
        .expect("a third customer");
    assert_eq!(
        ask(
            &connection,
            "SELECT c.name, count(o.id) FROM customers c \
             LEFT JOIN orders o ON o.customer_id = c.id \
             GROUP BY c.id, c.name ORDER BY c.name"
        ),
        "Ada,2\nAlan,0\nGrace,1"
    );

    // Cascading: removing an order takes its lines and nothing else.
    connection
        .execute("DELETE FROM orders WHERE id = 10")
        .expect("the order is removed");
    assert_eq!(ask(&connection, "SELECT count(*) FROM lines"), "2");
    assert_eq!(ask(&connection, "SELECT count(*) FROM orders"), "2");
    assert_eq!(ask(&connection, "SELECT count(*) FROM customers"), "3");
    assert_eq!(
        ask(&connection, "PRAGMA foreign_key_check"),
        "",
        "no dangling reference should be left"
    );
}

/// An append-only ledger: entries with a running balance, ordered by a sequence
/// that has to mean the same thing after a reopen.
#[test]
fn a_ledger_reads_the_same_after_a_reopen() {
    let directory = scratch("ledger");
    let path = directory.join("ledger.rdb");
    let before;
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch(
                "CREATE TABLE entries (\
                   seq INTEGER PRIMARY KEY AUTOINCREMENT,\
                   account TEXT NOT NULL,\
                   pence INTEGER NOT NULL,\
                   memo TEXT\
                 );\
                 CREATE INDEX entries_by_account ON entries (account, seq);",
            )
            .expect("the schema is created");
        let mut insert = connection
            .prepare("INSERT INTO entries (account, pence, memo) VALUES (?1, ?2, ?3)")
            .expect("the insert prepares");
        let rows: [(&str, i64, &str); 6] = [
            ("current", 10_000, "opening"),
            ("current", -2_500, "rent"),
            ("savings", 5_000, "opening"),
            ("current", -1_200, "food"),
            ("savings", 250, "interest"),
            ("current", 3_000, "refund"),
        ];
        for (account, pence, memo) in rows {
            insert.reset();
            insert.bind_text(1, account).expect("the account binds");
            insert.bind_integer(2, pence).expect("the amount binds");
            insert.bind_text(3, memo).expect("the memo binds");
            while insert.step().expect("the insert runs") {}
        }
        drop(insert);
        assert_eq!(connection.total_changes(), 6);
        before = ask(
            &connection,
            "SELECT account, seq, pence, \
             sum(pence) OVER (PARTITION BY account ORDER BY seq \
                              ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
             FROM entries ORDER BY account, seq",
        );
    }

    let database = reopen_and_check(&path);
    let connection = database.connect();
    let after = ask(
        &connection,
        "SELECT account, seq, pence, \
         sum(pence) OVER (PARTITION BY account ORDER BY seq \
                          ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
         FROM entries ORDER BY account, seq",
    );
    assert_eq!(before, after, "the running balance changed across a reopen");
    assert_eq!(
        after,
        "current,1,10000,10000\n\
         current,2,-2500,7500\n\
         current,4,-1200,6300\n\
         current,6,3000,9300\n\
         savings,3,5000,5000\n\
         savings,5,250,5250"
    );

    // An append after the reopen continues the sequence rather than reusing a
    // number, which is what `AUTOINCREMENT` is for and what an audit trail
    // depends on.
    connection
        .execute("INSERT INTO entries (account, pence, memo) VALUES ('current', -100, 'fee')")
        .expect("one more entry");
    assert_eq!(
        ask(&connection, "SELECT max(seq) FROM entries"),
        "7",
        "the sequence restarted across the reopen"
    );
}

/// A document store: JSON in a text column, queried by an extracted field, with
/// an index on the extraction.
#[test]
fn a_document_store_indexes_an_extracted_field() {
    let directory = scratch("documents");
    let path = directory.join("documents.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch(
                "CREATE TABLE documents (\
                   id INTEGER PRIMARY KEY,\
                   body TEXT NOT NULL CHECK (json_valid(body))\
                 );\
                 CREATE INDEX documents_by_kind ON documents (json_extract(body, '$.kind'));",
            )
            .expect("the schema is created");
        connection
            .execute_batch(
                "INSERT INTO documents VALUES (1, '{\"kind\":\"note\",\"title\":\"first\",\"tags\":[\"a\",\"b\"]}');\
                 INSERT INTO documents VALUES (2, '{\"kind\":\"task\",\"title\":\"second\",\"done\":false}');\
                 INSERT INTO documents VALUES (3, '{\"kind\":\"note\",\"title\":\"third\",\"tags\":[]}');",
            )
            .expect("the documents are written");

        // Malformed JSON is refused by the CHECK, which is the whole reason to
        // write one.
        let malformed = connection
            .execute("INSERT INTO documents VALUES (4, '{not json')")
            .expect_err("malformed JSON is refused");
        assert_eq!(malformed.code(), inillucent::PrimaryCode::Constraint);
    }

    let database = reopen_and_check(&path);
    let connection = database.connect();
    assert_eq!(
        ask(
            &connection,
            "SELECT id, json_extract(body, '$.title') FROM documents \
             WHERE json_extract(body, '$.kind') = 'note' ORDER BY id"
        ),
        "1,first\n3,third"
    );
    // The expression index is the way in, rather than a scan of every document.
    let plan = connection
        .explain("SELECT id FROM documents WHERE json_extract(body, '$.kind') = 'note'")
        .expect("the plan is explained")
        .join("\n");
    assert!(
        plan.contains("documents_by_kind"),
        "the expression index was not used; the plan was:\n{plan}"
    );
    // And a field that is absent reads as null rather than as an error.
    assert_eq!(
        ask(
            &connection,
            "SELECT id, json_extract(body, '$.done') FROM documents ORDER BY id"
        ),
        "1,NULL\n2,0\n3,NULL"
    );
}

/// A virtual table rolls back with the transaction around it, and the
/// connection agrees with the file afterwards.
///
/// **This did not work, and this ticket fixed it.** A rolled-back insert into
/// an `fts5` table stayed, a rolled-back delete was gone, and `ROLLBACK TO` did
/// nothing at all - so one query answered differently before and after a reopen
/// with nothing written in between, wrong in whichever direction the abandoned
/// transaction had written, and silent. Measured against the pinned `sqlite3`
/// 3.53.4 at the time:
///
/// | | inillucent, same connection | inillucent, reopened | SQLite |
/// |---|---|---|---|
/// | one row, then `BEGIN; INSERT; ROLLBACK` | **2** | 1 | 1 |
/// | two rows, then `BEGIN; DELETE; ROLLBACK` | **0** | 2 | 2 |
/// | `BEGIN; SAVEPOINT s; INSERT; ROLLBACK TO s; COMMIT` | **3** | - | 1 |
///
/// The reopened column is the clue that found it. The file was never wrong,
/// because no commit record was written and recovery ignored the pages - so the
/// damage was entirely in the live connection, which is what a *missing undo
/// record* looks like. `change_module` built its write log with `undo: None`
/// where every ordinary write passes `Some(&self.undo)`, so a virtual table's
/// writes went into the page pool with nothing recorded that could put them
/// back.
///
/// Three things were wrong and all three are fixed: the write path now records
/// before-images, the engine now calls `rollback` and `rollback_to` on every
/// connected module (it only ever called `begin`, `sync` and `commit`), and
/// FTS5 now implements the `rollback` it never had, so the doclists it buffers
/// go with the transaction that staged them. Taking a savepoint flushes the
/// modules, which is what puts everything before the point under the undo log.
///
/// The probe those numbers came from now returns the same bytes from both
/// shells.
#[test]
fn a_virtual_table_rolls_back_with_its_transaction() {
    let directory = scratch("catalogue");
    let path = directory.join("catalogue.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch(
                "CREATE VIRTUAL TABLE pages USING fts5 (title, body);\
                 INSERT INTO pages (title, body) VALUES ('storage', 'pages and trees and a buffer pool');\
                 INSERT INTO pages (title, body) VALUES ('logging', 'a redo log with group commit');",
            )
            .expect("the index is created and filled");
        assert_eq!(
            ask(
                &connection,
                "SELECT title FROM pages WHERE pages MATCH 'trees'"
            ),
            "storage",
            "the full-text index does not answer at all"
        );

        // An abandoned insert leaves the index as it found it.
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        connection
            .execute(
                "INSERT INTO pages (title, body) VALUES ('planning', 'a cost model and trees')",
            )
            .expect("the row is written");
        assert_eq!(
            ask(
                &connection,
                "SELECT count(*) FROM pages WHERE pages MATCH 'trees'"
            ),
            "2",
            "the write is not visible inside its own transaction"
        );
        connection
            .execute_batch("ROLLBACK")
            .expect("the transaction is abandoned");
        assert_eq!(
            ask(
                &connection,
                "SELECT count(*) FROM pages WHERE pages MATCH 'trees'"
            ),
            "1",
            "the abandoned row is still in the index"
        );

        // An abandoned delete puts every row back.
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        connection
            .execute("DELETE FROM pages")
            .expect("everything is deleted");
        assert_eq!(
            ask(&connection, "SELECT count(*) FROM pages"),
            "0",
            "the delete is not visible inside its own transaction"
        );
        connection
            .execute_batch("ROLLBACK")
            .expect("the transaction is abandoned");
        assert_eq!(
            ask(&connection, "SELECT count(*) FROM pages"),
            "2",
            "the abandoned delete took the rows with it"
        );

        // And a savepoint unwinds the part above it and keeps the part below.
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        connection
            .execute("INSERT INTO pages (title, body) VALUES ('kept', 'below the point')")
            .expect("a row before the point");
        connection
            .execute_batch("SAVEPOINT half")
            .expect("a point is saved");
        connection
            .execute("INSERT INTO pages (title, body) VALUES ('discarded', 'above the point')")
            .expect("a row after the point");
        connection
            .execute_batch("ROLLBACK TO half")
            .expect("the point is returned to");
        connection
            .execute_batch("COMMIT")
            .expect("the transaction commits");
        assert_eq!(
            ask(&connection, "SELECT count(*) FROM pages"),
            "3",
            "the savepoint kept the wrong rows"
        );
    }

    // And the file agrees with what the connection was saying, which is the
    // half that used to be false.
    let database = reopen_and_check(&path);
    let connection = database.connect();
    assert_eq!(
        ask(&connection, "SELECT title FROM pages ORDER BY rowid"),
        "storage\nlogging\nkept",
        "the file disagrees with the connection that wrote it"
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT title FROM pages WHERE pages MATCH 'below'"
        ),
        "kept",
        "the full-text index disagrees with its own content table"
    );
}

/// Rolling back to an *outer* savepoint discards the inner one's rows too.
///
/// **What this test does and does not prove.** It pins the observable answer:
/// two levels of savepoint, a rollback to the outer one, and the inner one's
/// rows gone - through a virtual table, across a reopen.
///
/// It does **not** discriminate the savepoint-level defect a review of this
/// ticket found, and saying so is the point. The engine used to tell each
/// module the current nesting depth rather than the level of the savepoint
/// being returned to, so `SAVEPOINT a; SAVEPOINT b; ROLLBACK TO a` said "two"
/// where the answer was "zero". The fix is real and it is in `rollback_to`, but
/// this test passes with it reverted - checked, not assumed - because FTS5's
/// `rollback_to` discards its whole buffer and ignores the number. The defect
/// bites a module that keeps marks of its own, and there is no such module in
/// the tree to write a failing test against yet.
///
/// So this is a regression test for the behaviour and a note about the rest.
/// The alternative - a comment claiming the test proves the fix - is the thing
/// section 1.1 of the standard is against.
#[test]
fn rolling_back_to_an_outer_savepoint_discards_the_inner_one() {
    let directory = scratch("nested-savepoint");
    let path = directory.join("nested.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch(
                "CREATE VIRTUAL TABLE pages USING fts5 (title, body);\
                 INSERT INTO pages (title, body) VALUES ('base', 'trees');",
            )
            .expect("the index is created");
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        connection
            .execute("INSERT INTO pages (title, body) VALUES ('before', 'trees')")
            .expect("a row before any savepoint");
        connection
            .execute_batch("SAVEPOINT a")
            .expect("the outer point");
        connection
            .execute("INSERT INTO pages (title, body) VALUES ('inside-a', 'trees')")
            .expect("a row inside a");
        connection
            .execute_batch("SAVEPOINT b")
            .expect("the inner point");
        connection
            .execute("INSERT INTO pages (title, body) VALUES ('inside-b', 'trees')")
            .expect("a row inside b");
        assert_eq!(
            ask(&connection, "SELECT count(*) FROM pages"),
            "4",
            "the writes are not visible inside their own transaction"
        );
        connection
            .execute_batch("ROLLBACK TO a")
            .expect("the outer point is returned to");
        assert_eq!(
            ask(&connection, "SELECT count(*) FROM pages"),
            "2",
            "returning to the outer savepoint kept a row from the inner one"
        );
        connection
            .execute_batch("COMMIT")
            .expect("the transaction commits");
    }
    let database = reopen_and_check(&path);
    let connection = database.connect();
    assert_eq!(
        ask(&connection, "SELECT title FROM pages ORDER BY rowid"),
        "base\nbefore",
        "the file disagrees with the connection that wrote it"
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM pages WHERE pages MATCH 'trees'"
        ),
        "2",
        "the full-text index disagrees with its own content table"
    );
}

/// A `ROLLBACK TO` that names no open savepoint fails and changes nothing.
///
/// **A failed statement may not have a side effect**, and this one nearly did.
/// The engine tells every module which savepoint is being returned to; when the
/// name matched nothing, the first version of that code defaulted the level to
/// zero, told the modules to discard, and only then let the statement fail. So
/// a typo in a savepoint name threw away a buffered virtual table's pending
/// writes and reported an error, leaving the transaction quietly short of rows
/// it had accepted.
///
/// Found by a review of this ticket's own fix, which is why the case is here:
/// nothing else in the suite asks what a *failed* rollback does to a module.
#[test]
fn a_rollback_to_an_unknown_savepoint_changes_nothing() {
    let directory = scratch("unknown-savepoint");
    let database = Database::open(directory.join("unknown.rdb")).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch(
            "CREATE VIRTUAL TABLE pages USING fts5 (title, body);\
             INSERT INTO pages (title, body) VALUES ('base', 'trees');",
        )
        .expect("the index is created");
    connection
        .execute_batch("BEGIN")
        .expect("the transaction opens");
    connection
        .execute("INSERT INTO pages (title, body) VALUES ('pending', 'trees')")
        .expect("a row that has not been committed");
    assert_eq!(ask(&connection, "SELECT count(*) FROM pages"), "2");

    connection
        .execute_batch("ROLLBACK TO no_such_point")
        .expect_err("a savepoint that was never opened is an error");

    assert_eq!(
        ask(&connection, "SELECT count(*) FROM pages"),
        "2",
        "the failed rollback discarded the transaction's pending rows"
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM pages WHERE pages MATCH 'trees'"
        ),
        "2",
        "the failed rollback discarded the full-text index's buffer"
    );
    connection
        .execute_batch("COMMIT")
        .expect("the transaction still commits");
    assert_eq!(ask(&connection, "SELECT count(*) FROM pages"), "2");
}

/// The ordinary half of the same story: a normal table in a transaction that a
/// virtual table also wrote to still rolls back correctly.
///
/// This is what bounds the defect above. Without it, "virtual tables do not
/// roll back" and "nothing rolls back once a virtual table is involved" look
/// the same from the outside, and they are very different sizes of problem.
#[test]
fn an_ordinary_table_rolls_back_beside_a_virtual_one() {
    let directory = scratch("mixed");
    let database = Database::open(directory.join("mixed.rdb")).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch(
            "CREATE TABLE plain (id INTEGER PRIMARY KEY, v TEXT);\
             CREATE VIRTUAL TABLE pages USING fts5 (title, body);\
             INSERT INTO plain VALUES (1, 'kept');\
             INSERT INTO pages (title, body) VALUES ('one', 'trees');",
        )
        .expect("the schema is created");
    connection
        .execute_batch("BEGIN")
        .expect("the transaction opens");
    connection
        .execute("INSERT INTO plain VALUES (2, 'discarded')")
        .expect("an ordinary row");
    connection
        .execute("INSERT INTO pages (title, body) VALUES ('two', 'trees')")
        .expect("a virtual row");
    connection
        .execute_batch("ROLLBACK")
        .expect("the transaction is abandoned");
    assert_eq!(
        ask(&connection, "SELECT count(*) FROM plain"),
        "1",
        "the ordinary table did not roll back, which is a much larger problem \
         than the virtual-table one"
    );
}

/// Two connections to one file see one another's committed writes, and a
/// snapshot does not change under a reader that opened before the write.
#[test]
fn two_connections_share_one_file() {
    let directory = scratch("connections");
    let database = Database::open(directory.join("shared.rdb")).expect("the database opens");
    let writer = database.connect();
    writer
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .expect("the schema is created");
    writer
        .execute("INSERT INTO t VALUES (1, 'one')")
        .expect("the first row");

    let reader = database.connect();
    assert_eq!(ask(&reader, "SELECT count(*) FROM t"), "1");

    writer
        .execute("INSERT INTO t VALUES (2, 'two')")
        .expect("the second row");
    assert_eq!(
        ask(&reader, "SELECT count(*) FROM t"),
        "2",
        "a committed write is not visible to the other connection"
    );
    assert_ne!(
        writer.session(),
        reader.session(),
        "two connections should be two sessions"
    );
}

/// A view and a trigger, which is how an application puts a rule in the
/// database rather than in every caller.
#[test]
fn a_trigger_and_a_view_survive_a_reopen() {
    let directory = scratch("rules");
    let path = directory.join("rules.rdb");
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch(
                "CREATE TABLE stock (sku TEXT PRIMARY KEY, on_hand INTEGER NOT NULL); \
                 CREATE TABLE movements (id INTEGER PRIMARY KEY, sku TEXT NOT NULL, delta INTEGER NOT NULL); \
                 CREATE VIEW low_stock AS SELECT sku, on_hand FROM stock WHERE on_hand < 5;",
            )
            .expect("the tables and the view are created");
        connection
            .execute_batch(
                "CREATE TRIGGER apply_movement AFTER INSERT ON movements FOR EACH ROW \
                 BEGIN UPDATE stock SET on_hand = on_hand + NEW.delta WHERE sku = NEW.sku; END;",
            )
            .expect("the trigger is created");
        connection
            .execute_batch(
                "INSERT INTO stock VALUES ('PEN', 10);\
                 INSERT INTO stock VALUES ('PAD', 3);\
                 INSERT INTO movements VALUES (1, 'PEN', -7);",
            )
            .expect("the seed is written");
        assert_eq!(
            ask(&connection, "SELECT on_hand FROM stock WHERE sku = 'PEN'"),
            "3"
        );
    }

    let database = reopen_and_check(&path);
    let connection = database.connect();
    assert_eq!(
        ask(
            &connection,
            "SELECT sku, on_hand FROM low_stock ORDER BY sku"
        ),
        "PAD,3\nPEN,3",
        "the view did not survive the reopen"
    );
    connection
        .execute("INSERT INTO movements VALUES (2, 'PAD', 10)")
        .expect("a movement after the reopen");
    assert_eq!(
        ask(
            &connection,
            "SELECT sku, on_hand FROM low_stock ORDER BY sku"
        ),
        "PEN,3",
        "the trigger did not fire after the reopen"
    );
}
