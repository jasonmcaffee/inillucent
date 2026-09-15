//! The corners: empty, absent, enormous, duplicated, and malformed.
//!
//! Invariant: **every case here has a definite expected answer, and a refusal
//! counts as one.** An edge case whose test says "it does not crash" is not a
//! test - it is a note that somebody once ran it. So each assertion below names
//! the value or the error code it wants, and the ones where the engine is
//! allowed to refuse say so explicitly rather than accepting either outcome.
//!
//! ## What counts as an edge here
//!
//! Not "unusual SQL" - the compat suites grade the whole SQL surface against
//! SQLite and do it better than this file could. What is here is the set of
//! places where an *application* meets the engine and the boundary is easy to
//! get wrong in a way no ordinary query would reveal:
//!
//! - the empty and the absent, which are different from each other and from
//!   zero, and which a layer that renders them the same has already lost;
//! - values at the edge of their type - the largest integer, the smallest, a
//!   float that is not a number, a blob with a zero byte in the middle of it;
//! - values too large for a page, which is the blob-extent path and the one
//!   most likely to differ between a fresh write and a rewrite;
//! - text that is not ASCII, and comparisons that are not byte comparisons;
//! - the statement lifecycle: binding out of range, stepping past the end,
//!   reusing a statement after the schema under it changed.
//!
//! ## Why a refusal is an acceptable answer but silence is not
//!
//! This engine refuses what it has not built, on purpose, and the driver has a
//! status of its own for it. A test that demanded an answer everywhere would be
//! asserting a roadmap. What it may never do is *quietly* return something
//! else, so the cases that are allowed to refuse assert that the refusal
//! carries a code and a message, and the cases that must answer assert the
//! value.

use std::path::PathBuf;

use inillucent_base::PrimaryCode;
use inillucent_engine::connect::Database;
use inillucent_tree::datum::OwnedDatum;

/// Returns a fresh, empty directory for one test's files.
///
/// @param tag - what to name the directory after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-edges-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// Opens a fresh database for one test.
///
/// @param tag - what to name the directory after
fn fresh(tag: &str) -> (PathBuf, Database) {
    let directory = scratch(tag);
    let path = directory.join("edges.rdb");
    let database = Database::open(&path).expect("the database opens");
    (path, database)
}

/// Returns the single cell a one-row, one-column answer holds.
///
/// @param rows - what the query returned
fn cell(rows: &[Vec<OwnedDatum>]) -> OwnedDatum {
    match rows {
        [row] => match row.as_slice() {
            [value] => value.clone(),
            other => panic!("expected one column, got {other:?}"),
        },
        other => panic!("expected one row, got {}", other.len()),
    }
}

/// An aggregate over no rows is not the same as no answer.
///
/// `count` answers zero and `sum` answers null, and an application that renders
/// those the same shows a balance of nought for an account it has never heard
/// of.
#[test]
fn aggregates_over_an_empty_table_are_defined() {
    let (_path, database) = fresh("empty-aggregate");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .expect("the table is created");
    assert_eq!(
        cell(&connection.query("SELECT count(*) FROM t").expect("counted")),
        OwnedDatum::Int(0)
    );
    assert_eq!(
        cell(&connection.query("SELECT sum(v) FROM t").expect("summed")),
        OwnedDatum::Null,
        "the sum of nothing is not zero"
    );
    assert_eq!(
        cell(&connection.query("SELECT max(v) FROM t").expect("maxed")),
        OwnedDatum::Null
    );
    assert_eq!(
        cell(
            &connection
                .query("SELECT total(v) FROM t")
                .expect("totalled")
        ),
        OwnedDatum::Real(0.0),
        "`total` is the one that answers zero, which is why it exists"
    );
}

/// A unique index lets several nulls through, because no null equals another.
///
/// A naive uniqueness check reports a collision here, and it is the single most
/// common way a constraint is implemented wrongly.
#[test]
fn a_unique_index_admits_many_nulls() {
    let (_path, database) = fresh("unique-null");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, alias TEXT);\
             CREATE UNIQUE INDEX t_alias ON t (alias);",
        )
        .expect("the schema is created");
    for id in 1..=3 {
        connection
            .execute(&format!("INSERT INTO t (id, alias) VALUES ({id}, NULL)"))
            .expect("a null alias is not a collision");
    }
    connection
        .execute("INSERT INTO t VALUES (4, 'taken')")
        .expect("a value");
    let clash = connection
        .execute("INSERT INTO t VALUES (5, 'taken')")
        .expect_err("but the same value twice is a collision");
    assert_eq!(clash.code(), PrimaryCode::Constraint);
    assert_eq!(
        cell(&connection.query("SELECT count(*) FROM t").expect("counted")),
        OwnedDatum::Int(4)
    );
}

/// Integers at the ends of their range survive a write and a read.
#[test]
fn integers_at_the_edges_round_trip() {
    let (path, database) = fresh("integer-edges");
    {
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
            .expect("the table is created");
        let mut insert = connection
            .prepare("INSERT INTO t VALUES (?1, ?2)")
            .expect("the insert prepares");
        for (id, value) in [(1i64, i64::MIN), (2, -1), (3, 0), (4, 1), (5, i64::MAX)] {
            insert.reset();
            insert.bind_integer(1, id).expect("the id binds");
            insert.bind_integer(2, value).expect("the value binds");
            while insert.step().expect("the insert runs") {}
        }
    }
    drop(database);
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.session();
    let rows = connection
        .query("SELECT v FROM t ORDER BY id")
        .expect("the values are read");
    let values: Vec<i64> = rows
        .iter()
        .map(|row| match row.as_slice() {
            [OwnedDatum::Int(value)] => *value,
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect();
    assert_eq!(values, vec![i64::MIN, -1, 0, 1, i64::MAX]);
}

/// A blob keeps every byte, including a zero in the middle and none at all.
#[test]
fn blobs_keep_their_bytes() {
    let (path, database) = fresh("blobs");
    let awkward: Vec<u8> = vec![0x00, 0xff, 0x00, 0x41, 0x00];
    {
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB)")
            .expect("the table is created");
        let mut insert = connection
            .prepare("INSERT INTO t VALUES (?1, ?2)")
            .expect("the insert prepares");
        insert.bind_integer(1, 1).expect("the id binds");
        insert.bind_blob(2, &awkward).expect("the blob binds");
        while insert.step().expect("the insert runs") {}
        insert.reset();
        insert.bind_integer(1, 2).expect("the id binds");
        insert.bind_blob(2, &[]).expect("an empty blob binds");
        while insert.step().expect("the insert runs") {}
    }
    drop(database);
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.session();
    let rows = connection
        .query("SELECT b FROM t ORDER BY id")
        .expect("the blobs are read");
    match rows.as_slice() {
        [first, second] => {
            assert_eq!(first.as_slice(), &[OwnedDatum::Blob(awkward.clone())]);
            assert_eq!(
                second.as_slice(),
                &[OwnedDatum::Blob(Vec::new())],
                "an empty blob must stay a blob rather than becoming null"
            );
        }
        other => panic!("expected two rows, got {}", other.len()),
    }
    assert_eq!(
        cell(
            &connection
                .query("SELECT length(b) FROM t WHERE id = 1")
                .expect("length")
        ),
        OwnedDatum::Int(5),
        "a zero byte must not end the value"
    );
}

/// A value wider than a page goes out to an extent and comes back whole.
#[test]
fn a_value_larger_than_a_page_round_trips() {
    let (path, database) = fresh("extents");
    // The default page is 32 KiB; a quarter of a megabyte is comfortably
    // several extents rather than one page with a little spilled.
    let long: String = "abcdefghij".repeat(26_214);
    {
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("the table is created");
        let mut insert = connection
            .prepare("INSERT INTO t VALUES (1, ?1)")
            .expect("the insert prepares");
        insert.bind_text(1, &long).expect("the text binds");
        while insert.step().expect("the insert runs") {}
    }
    drop(database);
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.session();
    assert_eq!(
        cell(&connection.query("SELECT length(v) FROM t").expect("length")),
        OwnedDatum::Int(long.len() as i64)
    );
    let rows = connection
        .query("SELECT v FROM t")
        .expect("the value is read");
    match cell(&rows) {
        OwnedDatum::Text(bytes) => {
            assert_eq!(bytes, long.as_bytes(), "the long value came back changed")
        }
        other => panic!("expected text, got {other:?}"),
    }
    database
        .check()
        .expect("the file is sound with an extent in it");
}

/// Text that is not ASCII survives, and its length is in characters.
#[test]
fn text_outside_ascii_survives() {
    let (path, database) = fresh("unicode");
    let samples = ["naïve", "日本語", "🛟 lifebuoy", "Straße"];
    {
        let connection = database.session();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("the table is created");
        let mut insert = connection
            .prepare("INSERT INTO t VALUES (?1, ?2)")
            .expect("the insert prepares");
        for (index, sample) in samples.iter().enumerate() {
            insert.reset();
            insert.bind_integer(1, index as i64).expect("the id binds");
            insert.bind_text(2, sample).expect("the text binds");
            while insert.step().expect("the insert runs") {}
        }
    }
    drop(database);
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.session();
    let rows = connection
        .query("SELECT v FROM t ORDER BY id")
        .expect("read back");
    let read: Vec<String> = rows
        .iter()
        .map(|row| match row.as_slice() {
            [OwnedDatum::Text(bytes)] => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect();
    assert_eq!(
        read,
        samples.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    );
    // `length` on text counts characters, not bytes - the distinction an
    // application hits the first time somebody types an accent.
    assert_eq!(
        cell(
            &connection
                .query("SELECT length(v) FROM t WHERE id = 1")
                .expect("length")
        ),
        OwnedDatum::Int(3),
        "three characters of Japanese, not nine bytes"
    );
}

/// `NOCASE` compares without case, and only for the column that declared it.
#[test]
fn a_collation_applies_where_it_was_declared() {
    let (_path, database) = fresh("collation");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, loose TEXT COLLATE NOCASE, tight TEXT);\
             INSERT INTO t VALUES (1, 'Ada', 'Ada');",
        )
        .expect("the schema is created");
    assert_eq!(
        cell(
            &connection
                .query("SELECT count(*) FROM t WHERE loose = 'ADA'")
                .expect("loose")
        ),
        OwnedDatum::Int(1)
    );
    assert_eq!(
        cell(
            &connection
                .query("SELECT count(*) FROM t WHERE tight = 'ADA'")
                .expect("tight")
        ),
        OwnedDatum::Int(0),
        "a column that did not declare NOCASE must still compare exactly"
    );
}

/// `LIMIT` and `OFFSET` at their boundaries.
#[test]
fn limit_and_offset_at_the_boundaries() {
    let (_path, database) = fresh("limits");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .expect("the table is created");
    for id in 1..=5 {
        connection
            .execute(&format!("INSERT INTO t VALUES ({id})"))
            .expect("a row");
    }
    let count = |sql: &str| connection.query(sql).expect("the query runs").len();
    assert_eq!(count("SELECT id FROM t LIMIT 0"), 0);
    assert_eq!(count("SELECT id FROM t LIMIT 100"), 5);
    assert_eq!(count("SELECT id FROM t LIMIT 2 OFFSET 4"), 1);
    assert_eq!(count("SELECT id FROM t LIMIT 2 OFFSET 99"), 0);
    assert_eq!(
        count("SELECT id FROM t LIMIT -1"),
        5,
        "a negative limit means no limit, which is SQLite's rule and an easy one to miss"
    );
}

/// Dividing by zero is null rather than an error, and the modulus agrees.
#[test]
fn division_by_zero_is_null() {
    let (_path, database) = fresh("divide");
    let connection = database.session();
    assert_eq!(
        cell(&connection.query("SELECT 1 / 0").expect("the query runs")),
        OwnedDatum::Null
    );
    assert_eq!(
        cell(&connection.query("SELECT 1 % 0").expect("the query runs")),
        OwnedDatum::Null
    );
    assert_eq!(
        cell(&connection.query("SELECT 1.0 / 0").expect("the query runs")),
        OwnedDatum::Null,
        "a float divided by zero is null too, rather than an infinity"
    );
}

/// An identifier that needs quoting keeps working, including a reserved word.
#[test]
fn quoted_identifiers_work() {
    let (_path, database) = fresh("quoting");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE \"select\" (\"order\" INTEGER PRIMARY KEY, \"a b\" TEXT);\
             INSERT INTO \"select\" VALUES (1, 'spaced');",
        )
        .expect("a reserved word is usable when quoted");
    assert_eq!(
        cell(
            &connection
                .query("SELECT \"a b\" FROM \"select\" WHERE \"order\" = 1")
                .expect("the query runs")
        ),
        OwnedDatum::Text(b"spaced".to_vec())
    );
}

/// Binding a parameter the statement does not have is refused, and index 0 is
/// out of range rather than an alias for `?1`.
///
/// **Both of these used to answer `Ok`, and this ticket fixed them.**
/// `Params::set` did `index.saturating_sub(1)` into a vector it then resized to
/// fit, so binding 9 on a one-parameter statement quietly made nine slots, and
/// binding 0 underflowed onto the slot `?1` uses.
///
/// The second was the dangerous one, because it loses data rather than time. A
/// caller who binds `?0` believing it a no-op had overwritten its first
/// parameter, the statement ran with a value the caller never chose, and
/// nothing in the result said so. SQLite answers `SQLITE_RANGE` to both, and so
/// does this now: `Connection::prepare` asks the parser how many parameters the
/// statement declared and `Statement::bind` checks against it.
#[test]
fn binding_out_of_range_is_refused() {
    let (_path, database) = fresh("binding");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .expect("the table is created");
    let mut statement = connection
        .prepare("INSERT INTO t VALUES (?1, ?2)")
        .expect("the insert prepares");

    // The two the statement has.
    statement.bind_integer(1, 100).expect("`?1` binds");
    statement.bind_text(2, "row").expect("`?2` binds");

    // And the two it does not.
    let above = statement
        .bind_integer(3, 1)
        .expect_err("a parameter past the last one is refused");
    assert_eq!(above.code(), PrimaryCode::Range);
    let zero = statement
        .bind_integer(0, 999)
        .expect_err("index zero is refused rather than aliasing `?1`");
    assert_eq!(zero.code(), PrimaryCode::Range);

    // And the refusals changed nothing: `?1` still holds what was bound to it.
    while statement.step().expect("the insert runs") {}
    drop(statement);
    assert_eq!(
        cell(
            &connection
                .query("SELECT id FROM t")
                .expect("the row is read")
        ),
        OwnedDatum::Int(100),
        "a refused bind of index 0 overwrote `?1` anyway"
    );
}

/// Clearing the bindings does not make the statement forget how many
/// parameters it has.
///
/// The count belongs to the statement and the values do not, so a cleared
/// statement that started accepting any index again would be the same defect
/// wearing a different hat - and it is exactly what a naive `clear` does.
#[test]
fn clearing_bindings_keeps_the_range_check() {
    let (_path, database) = fresh("clear-binding");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .expect("the table is created");
    let mut statement = connection
        .prepare("INSERT INTO t VALUES (?1)")
        .expect("the insert prepares");
    statement.clear_bindings();
    assert_eq!(
        statement
            .bind_integer(4, 1)
            .expect_err("still out of range after a clear")
            .code(),
        PrimaryCode::Range
    );
    statement.bind_integer(1, 1).expect("`?1` still binds");
}

/// A statement can be reset and re-run, and stepping past the end is not an
/// error - it just says there is no more.
#[test]
fn a_statement_resets_and_ends_cleanly() {
    let (_path, database) = fresh("lifecycle");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY);\
             INSERT INTO t VALUES (1);\
             INSERT INTO t VALUES (2);",
        )
        .expect("the schema is created");
    let mut statement = connection
        .prepare("SELECT id FROM t ORDER BY id")
        .expect("the query prepares");
    let mut first = Vec::new();
    while statement.step().expect("the query steps") {
        first.push(statement.row().to_vec());
    }
    assert_eq!(first.len(), 2);
    assert!(
        !statement
            .step()
            .expect("stepping past the end is not an error"),
        "a statement that is finished should keep saying so"
    );
    statement.reset();
    let mut again = Vec::new();
    while statement.step().expect("the query steps again") {
        again.push(statement.row().to_vec());
    }
    assert_eq!(first, again, "a reset statement answered differently");
}

/// A query naming a column that does not exist fails, says something, and
/// leaves the connection usable.
#[test]
fn a_bad_column_leaves_the_connection_usable() {
    let (_path, database) = fresh("bad-column");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .expect("the table is created");
    let failure = connection
        .query("SELECT no_such_column FROM t")
        .expect_err("an unknown column is an error");
    assert!(!failure.message().is_empty());
    assert_eq!(
        cell(
            &connection
                .query("SELECT count(*) FROM t")
                .expect("still usable")
        ),
        OwnedDatum::Int(0)
    );
}

/// An empty statement, and one that is only a comment, are not errors.
#[test]
fn an_empty_statement_is_not_an_error() {
    let (_path, database) = fresh("empty-statement");
    let connection = database.session();
    connection
        .execute_batch("")
        .expect("nothing is a valid batch");
    connection
        .execute_batch("-- just a comment\n")
        .expect("a comment is a valid batch");
    connection
        .execute_batch(";;;")
        .expect("empty statements are a valid batch");
}

/// `execute_batch` can create a trigger, because it splits by the grammar
/// rather than by a scan for semicolons.
///
/// **This used to fail, and this ticket fixed it.** `execute_batch` cut the
/// script at every semicolon outside a string literal, and a trigger body
/// contains one - so `CREATE TRIGGER ... BEGIN UPDATE ...; END` arrived as two
/// fragments and the first was an unterminated trigger the parser rightly
/// refused with `incomplete input`. An application could not create a trigger
/// in the same call that created the tables it is about, which is the natural
/// way to write a schema and the way `sqlite3_exec` accepts.
///
/// The parser already knew where the statement ended - `prepare_with_tail` has
/// long asked it exactly that - so the batch path asks the same question now,
/// and there is one opinion about statement boundaries instead of two.
#[test]
fn execute_batch_creates_a_trigger() {
    let (_path, database) = fresh("batch-trigger");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE stock (sku TEXT PRIMARY KEY, on_hand INTEGER NOT NULL);\
             CREATE TABLE movements (id INTEGER PRIMARY KEY, sku TEXT NOT NULL, delta INTEGER NOT NULL);\
             CREATE TRIGGER apply AFTER INSERT ON movements FOR EACH ROW \
             BEGIN UPDATE stock SET on_hand = on_hand + NEW.delta WHERE sku = NEW.sku; END;\
             INSERT INTO stock VALUES ('PEN', 10);\
             INSERT INTO movements VALUES (1, 'PEN', -7);",
        )
        .expect("the whole schema, trigger included, goes in as one batch");
    assert_eq!(
        cell(
            &connection
                .query("SELECT on_hand FROM stock")
                .expect("the stock is read")
        ),
        OwnedDatum::Int(3),
        "the trigger did not fire"
    );
}

/// And the things a batch is allowed to be that are not statements: nothing at
/// all, only comments, only separators.
///
/// These are the cases the old scan handled for free and a grammar-driven split
/// has to handle on purpose, because the parser refuses text that holds no
/// statement. All three are ordinary things for a caller to pass.
#[test]
fn a_batch_may_be_trivia() {
    let (_path, database) = fresh("batch-trivia");
    let connection = database.session();
    connection
        .execute_batch("")
        .expect("nothing is a valid batch");
    connection
        .execute_batch("   \n\t ")
        .expect("whitespace is a valid batch");
    connection
        .execute_batch(";;;")
        .expect("separators are a valid batch");
    connection
        .execute_batch("-- a comment\n")
        .expect("a line comment is a valid batch");
    connection
        .execute_batch("/* a block\n   comment */")
        .expect("a block comment is a valid batch");
    connection
        .execute_batch("CREATE TABLE t (a); -- trailing\n")
        .expect("a statement followed by a comment is a valid batch");
    connection
        .execute_batch("/* leading */ INSERT INTO t VALUES (1); ;")
        .expect("trivia around a statement is a valid batch");
    assert_eq!(
        cell(&connection.query("SELECT count(*) FROM t").expect("counted")),
        OwnedDatum::Int(1)
    );
}

/// A rowid table with no explicit key still gives every row a distinct one, and
/// the rowid survives a reopen.
#[test]
fn an_implicit_rowid_is_stable_across_a_reopen() {
    let (path, database) = fresh("rowid");
    {
        let connection = database.session();
        connection
            .execute_batch(
                "CREATE TABLE t (v TEXT);\
                 INSERT INTO t VALUES ('a');\
                 INSERT INTO t VALUES ('b');\
                 INSERT INTO t VALUES ('c');",
            )
            .expect("the schema is created");
        connection
            .execute("DELETE FROM t WHERE v = 'b'")
            .expect("the middle row goes");
    }
    drop(database);
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.session();
    let rows = connection
        .query("SELECT rowid, v FROM t ORDER BY rowid")
        .expect("the rowids are read");
    assert_eq!(rows.len(), 2);
    match rows.first().map(Vec::as_slice) {
        Some([OwnedDatum::Int(1), OwnedDatum::Text(_)]) => {}
        other => panic!("expected the first row to keep rowid 1, got {other:?}"),
    }
    match rows.get(1).map(Vec::as_slice) {
        Some([OwnedDatum::Int(3), OwnedDatum::Text(_)]) => {}
        other => panic!("expected the third row to keep rowid 3, got {other:?}"),
    }
}

/// A schema change is visible to a connection that was already open.
///
/// A cached plan built before the change would answer against a table shape
/// that no longer exists, which is a wrong answer rather than an error.
#[test]
fn a_schema_change_reaches_an_open_connection() {
    let (_path, database) = fresh("schema-change");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT); INSERT INTO t VALUES (1, 'x')",
        )
        .expect("the schema is created");
    assert_eq!(
        cell(
            &connection
                .query("SELECT a FROM t")
                .expect("the first shape")
        ),
        OwnedDatum::Text(b"x".to_vec())
    );
    connection
        .execute_batch("ALTER TABLE t ADD COLUMN b INTEGER DEFAULT 7")
        .expect("the column is added");
    let rows = connection
        .query("SELECT a, b FROM t")
        .expect("the new column is visible to the same connection");
    assert_eq!(
        rows.first().map(Vec::as_slice),
        Some([OwnedDatum::Text(b"x".to_vec()), OwnedDatum::Int(7)].as_slice())
    );
}
