//! Whole applications, in miniature, against a file on disk, at every
//! configuration in the matrix.
//!
//! Invariant: **each test here is a story an application actually tells, run
//! end to end through `inillucent_engine::connect::Database`, it reopens the
//! file part way through, and it runs once per arm of
//! `inillucent_compat::matrix`.** Not a feature exercised in isolation - a
//! schema built, written to, queried the way a screen would query it, closed,
//! and opened again.
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
//! The stories are chosen to be different shapes rather than several
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
//! ## Why every story runs six times
//!
//! Because until task-2036 every one of them ran at one page size. `scenario!`
//! expands one test per arm of the matrix, so
//! `an_order_book_keeps_its_references::sqlite_page` is its own test with its
//! own verdict at SQLite's 4,096 byte page size, and
//! `::small_pool` is the same story over a 64 frame buffer pool. The arms and
//! the reason each one is in the list are in
//! `crates/inillucent-compat/src/matrix.rs`.
//!
//! A story is therefore written as `fn story(arm: &Arm, area: &Path)`: the
//! configuration it is running at, and a scratch directory of its own whose
//! path carries the arm's name. It opens through `arm.open()` rather than
//! `Database::open`, because that is what carries the arm's page size into the
//! file, and it reopens through `reopen_and_check(arm, path)` for the same
//! reason - a reopen at the wrong geometry would be testing a different file
//! from the one the story wrote.
//!
//! ## What every story asserts that a feature test would not
//!
//! That the file is still right *afterwards*. Each story ends by reopening the
//! database from the path and asking the same questions again, so an answer
//! that lives only in the page pool fails here. That is rule 1.4 of
//! `tests/inillucent-testing-tdd.md`, and it is the rule that caught the
//! virtual-table rollback defect this file's fourth story records: the *file*
//! was correct throughout and only the live connection was wrong, so every
//! test that did not reopen agreed with the damage.

use std::path::{Path, PathBuf};

use inillucent_compat::matrix::Arm;
use inillucent_compat::scenario;
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

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
fn ask(connection: &inillucent_engine::connect::Connection<'_>, sql: &str) -> String {
    match connection.query(sql) {
        Ok(rows) => table(&rows),
        Err(why) => panic!("`{sql}` failed: {} ({:?})", why.message(), why.code()),
    }
}

/// Opens a story's database at the arm's geometry.
///
/// @param arm - the configuration this run is at
/// @param path - the database file
fn open(arm: &Arm, path: &Path) -> Database {
    match arm.open(path) {
        Ok(database) => database,
        Err(why) => panic!(
            "the database does not open at the {} arm: {}",
            arm.name,
            why.message()
        ),
    }
}

/// Reopens a database at the arm's geometry and checks it, which is what every
/// story ends with.
///
/// @param arm - the configuration this run is at
/// @param path - the database file
fn reopen_and_check(arm: &Arm, path: &PathBuf) -> Database {
    let database = open(arm, path);
    database.check().expect("the file is sound after a reopen");
    let connection = database.session();
    assert_eq!(
        ask(&connection, "PRAGMA integrity_check"),
        "ok",
        "the engine's own check disagrees with `Database::check` at the {} arm",
        arm.name
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
fn an_order_book_keeps_its_references(arm: &Arm, area: &Path) {
    let path = area.join("shop.rdb");
    {
        let database = open(arm, &path);
        let connection = database.session();
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
            inillucent_base::PrimaryCode::Constraint,
            "expected a constraint failure, got {}",
            orphan.message()
        );

        // So is a quantity the CHECK forbids.
        let bad_quantity = connection
            .execute("INSERT INTO lines VALUES (10, 'CLIP', 0, 10)")
            .expect_err("a zero quantity is refused");
        assert_eq!(
            bad_quantity.code(),
            inillucent_base::PrimaryCode::Constraint
        );

        // And a customer who still has orders.
        let busy_customer = connection
            .execute("DELETE FROM customers WHERE id = 1")
            .expect_err("a customer with orders cannot be removed");
        assert_eq!(
            busy_customer.code(),
            inillucent_base::PrimaryCode::Constraint
        );
    }

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
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

scenario!(
    an_order_book_keeps_its_references,
    an_order_book_keeps_its_references
);

/// The running balance, expressed as a correlated subquery rather than a
/// window function.
///
/// **This story used to write it as
/// `sum(pence) OVER (PARTITION BY account ORDER BY seq ROWS BETWEEN UNBOUNDED
/// PRECEDING AND CURRENT ROW)`.** The shipping engine refuses every window
/// function outright - "the new engine's physical pass does not handle a
/// window function reaching the pipeline builder yet" - a genuinely missing
/// capability, not a defect: recorded as `sql.select.window` and
/// `functions.window`, both `status = "missing"`, in
/// `compat/sqlite-3.53.4.toml`, and in `docs/feature-comparison.md`'s
/// "Window functions" section. This story is not about window syntax, though
/// - per this file's own header, it is about **an ordering that must survive
/// a reopen** - and a correlated subquery answers exactly the same running
/// total, checked byte for byte against the window form on the pinned
/// SQLite before this changed. Swap it back for the `OVER` form once window
/// functions ship, to start covering that too.
const RUNNING_BALANCE: &str = "SELECT account, seq, pence, \
     (SELECT sum(prior.pence) FROM entries prior \
      WHERE prior.account = entries.account AND prior.seq <= entries.seq) \
     FROM entries ORDER BY account, seq";

/// An append-only ledger: entries with a running balance, ordered by a sequence
/// that has to mean the same thing after a reopen.
fn a_ledger_reads_the_same_after_a_reopen(arm: &Arm, area: &Path) {
    let path = area.join("ledger.rdb");
    let before;
    {
        let database = open(arm, &path);
        let connection = database.session();
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
        assert_eq!(connection.total_changes().expect("the engine is free"), 6);
        before = ask(&connection, RUNNING_BALANCE);
    }

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    let after = ask(&connection, RUNNING_BALANCE);
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

scenario!(
    a_ledger_reads_the_same_after_a_reopen,
    a_ledger_reads_the_same_after_a_reopen
);

/// A document store: JSON in a text column, queried by an extracted field, with
/// an index on the extraction.
fn a_document_store_indexes_an_extracted_field(arm: &Arm, area: &Path) {
    let path = area.join("documents.rdb");
    {
        let database = open(arm, &path);
        let connection = database.session();
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
        assert_eq!(malformed.code(), inillucent_base::PrimaryCode::Constraint);
    }

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
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

scenario!(
    a_document_store_indexes_an_extracted_field,
    a_document_store_indexes_an_extracted_field
);

/// A virtual table rolls back with the transaction around it, and the
/// connection agrees with the file afterwards.
///
/// **This did not work, and task-1876 fixed it.** A rolled-back insert into
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
fn a_virtual_table_rolls_back_with_its_transaction(arm: &Arm, area: &Path) {
    let path = area.join("catalogue.rdb");
    {
        let database = open(arm, &path);
        let connection = database.session();
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
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
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

scenario!(
    a_virtual_table_rolls_back_with_its_transaction,
    a_virtual_table_rolls_back_with_its_transaction
);

/// Rolling back to an *outer* savepoint discards the inner one's rows too.
///
/// **What this test does and does not prove.** It pins the observable answer:
/// two levels of savepoint, a rollback to the outer one, and the inner one's
/// rows gone - through a virtual table, across a reopen.
///
/// It does **not** discriminate the savepoint-level defect a review of
/// task-1876 found, and saying so is the point. The engine used to tell each
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
fn rolling_back_to_an_outer_savepoint_discards_the_inner_one(arm: &Arm, area: &Path) {
    let path = area.join("nested.rdb");
    {
        let database = open(arm, &path);
        let connection = database.session();
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
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
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

scenario!(
    rolling_back_to_an_outer_savepoint_discards_the_inner_one,
    rolling_back_to_an_outer_savepoint_discards_the_inner_one
);

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
/// Found by a review of task-1876's own fix, which is why the case is here:
/// nothing else in the suite asks what a *failed* rollback does to a module.
fn a_rollback_to_an_unknown_savepoint_changes_nothing(arm: &Arm, area: &Path) {
    let database = open(arm, &area.join("unknown.rdb"));
    let connection = database.session();
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

scenario!(
    a_rollback_to_an_unknown_savepoint_changes_nothing,
    a_rollback_to_an_unknown_savepoint_changes_nothing
);

/// The ordinary half of the same story: a normal table in a transaction that a
/// virtual table also wrote to still rolls back correctly.
///
/// This is what bounds the defect above. Without it, "virtual tables do not
/// roll back" and "nothing rolls back once a virtual table is involved" look
/// the same from the outside, and they are very different sizes of problem.
fn an_ordinary_table_rolls_back_beside_a_virtual_one(arm: &Arm, area: &Path) {
    let database = open(arm, &area.join("mixed.rdb"));
    let connection = database.session();
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

scenario!(
    an_ordinary_table_rolls_back_beside_a_virtual_one,
    an_ordinary_table_rolls_back_beside_a_virtual_one
);

/// Two connections to one file see one another's committed writes, and a
/// snapshot does not change under a reader that opened before the write.
fn two_connections_share_one_file(arm: &Arm, area: &Path) {
    let database = open(arm, &area.join("shared.rdb"));
    let writer = database.session();
    writer
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .expect("the schema is created");
    writer
        .execute("INSERT INTO t VALUES (1, 'one')")
        .expect("the first row");

    let reader = database.session();
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

scenario!(
    two_connections_share_one_file,
    two_connections_share_one_file
);

/// A view and a trigger, which is how an application puts a rule in the
/// database rather than in every caller.
fn a_trigger_and_a_view_survive_a_reopen(arm: &Arm, area: &Path) {
    let path = area.join("rules.rdb");
    {
        let database = open(arm, &path);
        let connection = database.session();
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

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
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

scenario!(
    a_trigger_and_a_view_survive_a_reopen,
    a_trigger_and_a_view_survive_a_reopen
);
