//! The edge catalogue of the task-2035 TDD's section 6, one story per row, at
//! every arm.
//!
//! Invariant: **every case names the value or the status it wants, and a
//! refusal counts as a value.** An edge case whose assertion is "it did not
//! crash" is a note that somebody once ran it.
//!
//! ## What is here and what is not
//!
//! `crates/inillucent/tests/edges.rs` already covers the value and statement
//! corners - the largest integer, a blob with a zero byte in it, binding out of
//! range - and covers them well. It covers them at **one page size**, which is
//! the gap this file is for: every case here runs at all six arms, and several
//! of them are about geometry directly.
//!
//! The rows of section 6 that are not here are the ones that belong to another
//! surface, and each is named where it lives:
//!
//! | row | where |
//! |---|---|
//! | MCP | `crates/inillucent-compat/tests/mcp_session.rs` |
//! | CLI golden output, exit code 3 | `crates/inillucent-compat/tests/cli_commands.rs` |
//! | bindings, a temp table across two calls | `drivers/conformance/suite.json` |
//! | a writer killed holding the lock, a stale index | `crates/inillucent-compat/tests/process_campaign.rs` |
//! | FTS5 at 2,500 documents at 4 KiB | `crates/inillucent/tests/story_rag.rs` |
//!
//! ## The allow list
//!
//! `tests/workloads/edges/allow.list`, read the way `story_rag.rs` reads its
//! own: a case that is **known wrong** asserts the right answer and names the
//! ticket, and a case whose ticket has landed fails until its line is removed.
//! One line is in it today, task-2043, and it was found by writing this file.

use std::path::Path;

use inillucent_base::PrimaryCode;
use inillucent_compat::matrix::Arm;
use inillucent_compat::scenario;
use inillucent_compat::stories::{ask, body, open, reopen_and_check, run};
use inillucent_compat::workspace_root;
use inillucent_engine::connect::Connection;

/// Reads the edge catalogue's allow list.
///
/// @param key - `<story>::<arm>`
fn allow_listed(key: &str) -> Option<String> {
    let path = workspace_root().join("tests/workloads/edges/allow.list");
    let text = std::fs::read_to_string(&path).ok()?;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((listed, said)) = line.split_once('\t') else {
            panic!("{}: an allow line has no tab: {line}", path.display());
        };
        if listed.trim() == key {
            return Some(said.trim().to_string());
        }
    }
    None
}

/// Compares an answer to the one SQLite gives, honouring the allow list.
///
/// **Both directions.** A case that is wrong and listed prints what it got and
/// carries on; a case that is wrong and not listed fails; and a case that is
/// **right** and listed fails too, because a fixed defect left listed reads as
/// coverage and is not.
///
/// @param arm - the configuration this run is at
/// @param story - the story's name, which with the arm is the allow list key
/// @param got - what this engine answered
/// @param wanted - what the pinned SQLite answers
/// @param about - what the case is
fn as_sqlite_answers(arm: &Arm, story: &str, got: &str, wanted: &str, about: &str) {
    let key = format!("{story}::{}", arm.test_name());
    match (got == wanted, allow_listed(&key)) {
        (true, None) => {}
        (true, Some(said)) => panic!(
            "`{key}` is allow listed against `{said}` and now answers correctly. Delete the \
             line from tests/workloads/edges/allow.list."
        ),
        (false, Some(said)) => println!(
            "{key}: {about} answered {got:?} where SQLite answers {wanted:?}; allow listed \
             against {said}"
        ),
        (false, None) => panic!(
            "{key}: {about}\n  this engine: {got:?}\n  SQLite     : {wanted:?}\n\
             Fix it, or add `{key}` to tests/workloads/edges/allow.list with the ticket that \
             will."
        ),
    }
}

/// An empty string, an empty blob and a NULL are three different values.
///
/// The row an application renders the same has already lost: `''` is not NULL,
/// `x''` is not `''`, and a `WHERE` on each picks a different row.
fn the_empty_and_the_absent_are_different(arm: &Arm, area: &Path) {
    let path = area.join("empty.rdb");
    let database = open(arm, &path);
    let connection = database.session();
    run(
        &connection,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a, b, c);\
         INSERT INTO t VALUES (1, '', x'', NULL);\
         INSERT INTO t VALUES (2, 'x', x'00', 0);",
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT typeof(a), typeof(b), typeof(c) FROM t WHERE id = 1"
        ),
        "text,blob,null"
    );
    assert_eq!(ask(&connection, "SELECT id FROM t WHERE a = ''"), "1");
    assert_eq!(ask(&connection, "SELECT id FROM t WHERE c IS NULL"), "1");
    assert_eq!(ask(&connection, "SELECT id FROM t WHERE c = 0"), "2");
    assert_eq!(
        ask(
            &connection,
            "SELECT length(a), length(b) FROM t WHERE id = 1"
        ),
        "0,0"
    );

    drop(connection);
    drop(database);
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        ask(
            &connection,
            "SELECT typeof(a), typeof(b), typeof(c) FROM t WHERE id = 1"
        ),
        "text,blob,null",
        "an empty string, an empty blob and a NULL stopped being three things across a reopen, \
         at the {} arm",
        arm.name
    );
}

scenario!(
    the_empty_and_the_absent_are_different,
    the_empty_and_the_absent_are_different
);

/// Integer overflow is an error and a float comparison is not.
///
/// The three values SQLite has a definite answer for at the edge of `i64`:
/// `abs(i64::MIN)` overflows, a `SUM` past `i64::MAX` overflows, and
/// `9007199254740993 = 9007199254740992.0` is false because the float cannot
/// hold the odd integer.
fn integers_at_their_edges_overflow_rather_than_wrap(arm: &Arm, area: &Path) {
    let database = open(arm, &area.join("edges.rdb"));
    let connection = database.session();

    let overflowed = connection
        .query("SELECT abs(-9223372036854775808)")
        .expect_err("abs of the smallest integer has no answer in i64");
    assert_eq!(
        overflowed.code(),
        PrimaryCode::Error,
        "abs(i64::MIN) refused with {} ({:?}), which is not the documented refusal",
        overflowed.message(),
        overflowed.code()
    );

    run(
        &connection,
        "CREATE TABLE big (n INTEGER);\
         INSERT INTO big VALUES (9223372036854775807), (1);",
    );
    let summed = connection
        .query("SELECT sum(n) FROM big")
        .expect_err("a sum past i64::MAX has no answer in i64");
    assert_eq!(summed.code(), PrimaryCode::Error);

    // The odd integer above 2^53 and the float below it are not equal, which is
    // the comparison an application gets wrong when it stores an identifier as
    // a double.
    assert_eq!(
        ask(&connection, "SELECT 9007199254740993 = 9007199254740992.0"),
        "0"
    );
    assert_eq!(
        ask(&connection, "SELECT 9223372036854775807 + 0"),
        "9223372036854775807"
    );
}

scenario!(
    integers_at_their_edges_overflow_rather_than_wrap,
    integers_at_their_edges_overflow_rather_than_wrap
);

/// Text that is not ASCII reads back byte identical.
///
/// The composed and decomposed forms of the same word are different strings and
/// have to stay different: a layer that normalised one of them on the way in
/// would make two rows one, and nothing would say so.
fn text_outside_ascii_reads_back_byte_identical(arm: &Arm, area: &Path) {
    let path = area.join("text.rdb");
    let written = {
        let database = open(arm, &path);
        let connection = database.session();
        run(
            &connection,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);\
             INSERT INTO t VALUES (1, 'cafe' || char(769));\
             INSERT INTO t VALUES (2, char(99,97,102,233));\
             INSERT INTO t VALUES (3, char(128105,8205,128187));\
             INSERT INTO t VALUES (4, char(1502,1489,1495,1503));",
        );
        // Composed and decomposed are two rows, not one.
        assert_eq!(ask(&connection, "SELECT count(*) FROM t"), "4");
        assert_eq!(
            ask(
                &connection,
                "SELECT id FROM t WHERE v = char(99,97,102,233)"
            ),
            "2",
            "the decomposed form matched the composed one, at the {} arm",
            arm.name
        );
        // `length` counts characters, not bytes.
        assert_eq!(
            ask(&connection, "SELECT id, length(v) FROM t ORDER BY id"),
            "1,5\n2,4\n3,3\n4,4"
        );
        ask(&connection, "SELECT id, hex(v) FROM t ORDER BY id")
    };

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        ask(&connection, "SELECT id, hex(v) FROM t ORDER BY id"),
        written,
        "text changed bytes across a reopen, at the {} arm",
        arm.name
    );
}

scenario!(
    text_outside_ascii_reads_back_byte_identical,
    text_outside_ascii_reads_back_byte_identical
);

/// A 40 KB value in a column with no declared type round trips.
///
/// **The size is the point.** Forty kilobytes is larger than a 32,768 byte page
/// and ten times larger than a 4,096 byte one, so the value crosses the extent
/// threshold at every arm and crosses it by a different number of pages at
/// each. The column has no declared type, so nothing about the storage class
/// was decided by the schema.
fn a_value_larger_than_every_page_round_trips(arm: &Arm, area: &Path) {
    let path = area.join("large.rdb");
    let text = body(11, 40_000);
    {
        let database = open(arm, &path);
        let connection = database.session();
        run(&connection, "CREATE TABLE t (id INTEGER PRIMARY KEY, v)");
        let mut statement = connection
            .prepare("INSERT INTO t VALUES (1, ?1)")
            .expect("the insert prepares");
        statement.bind_text(1, &text).expect("the text binds");
        while statement.step().expect("the insert runs") {}
        drop(statement);
        assert_eq!(
            ask(&connection, "SELECT length(v), typeof(v) FROM t"),
            format!("40000,text")
        );
    }

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        ask(&connection, "SELECT length(v) FROM t"),
        "40000",
        "a 40 KB value did not survive a reopen at the {} arm",
        arm.name
    );
    let read = connection
        .query("SELECT v FROM t")
        .expect("the value reads back");
    let cell = read
        .first()
        .and_then(|row| row.first())
        .expect("one row, one column");
    match cell {
        inillucent_tree::datum::OwnedDatum::Text(bytes) => assert_eq!(
            String::from_utf8_lossy(bytes),
            text,
            "a 40 KB value came back with different bytes at the {} arm",
            arm.name
        ),
        other => panic!("a text column read back as {other:?}"),
    }
}

scenario!(
    a_value_larger_than_every_page_round_trips,
    a_value_larger_than_every_page_round_trips
);

/// A megabyte blob shrunk to a byte and grown again does not leave the file
/// three times the size it needs.
///
/// **A count of pages, not a clock and not a byte count.** The question is
/// whether the pages a shrunk value released are available again, and the
/// answer is how many pages the file has - which reads the same on an idle
/// machine and a busy one, and is rule 1.7's shape.
fn a_blob_that_shrinks_gives_its_pages_back(arm: &Arm, area: &Path) {
    let path = area.join("blob.rdb");
    let database = open(arm, &path);
    let connection = database.session();
    run(
        &connection,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v BLOB)",
    );

    let write = |connection: &Connection<'_>, bytes: usize| {
        let mut statement = connection
            .prepare("INSERT OR REPLACE INTO t VALUES (1, ?1)")
            .expect("the insert prepares");
        statement
            .bind_blob(1, &vec![0xABu8; bytes])
            .expect("the blob binds");
        while statement.step().expect("the insert runs") {}
    };
    let pages = |connection: &Connection<'_>| -> i64 {
        ask(connection, "PRAGMA page_count")
            .parse::<i64>()
            .unwrap_or(-1)
    };

    write(&connection, 1_048_576);
    run(&connection, "PRAGMA wal_checkpoint");
    let at_a_megabyte = pages(&connection);
    assert!(
        at_a_megabyte > 1,
        "a megabyte blob left the file at {at_a_megabyte} pages, so the page count is not being \
         read at the {} arm",
        arm.name
    );

    write(&connection, 1);
    run(&connection, "PRAGMA wal_checkpoint");
    write(&connection, 1_048_576);
    run(&connection, "PRAGMA wal_checkpoint");
    let after_the_round_trip = pages(&connection);
    assert_eq!(
        ask(&connection, "SELECT length(v) FROM t"),
        "1048576",
        "the blob did not come back at the {} arm",
        arm.name
    );
    assert!(
        after_the_round_trip <= at_a_megabyte.saturating_mul(3),
        "the file went from {at_a_megabyte} pages to {after_the_round_trip} shrinking a blob to \
         one byte and growing it again, at the {} arm - the pages the shrink released were not \
         reused",
        arm.name
    );
}

scenario!(
    a_blob_that_shrinks_gives_its_pages_back,
    a_blob_that_shrinks_gives_its_pages_back
);

/// An identifier that is not an identifier: a name with an accent and a space,
/// a column called `select`, and two hundred columns.
fn identifiers_that_need_quoting_survive_a_reopen(arm: &Arm, area: &Path) {
    let path = area.join("identifiers.rdb");
    let wide: Vec<String> = (0..200).map(|at| format!("\"c{at}\" INTEGER")).collect();
    {
        let database = open(arm, &path);
        let connection = database.session();
        run(
            &connection,
            &format!(
                "CREATE TABLE \"ordér 1\" (\"select\" INTEGER PRIMARY KEY, \"from\" TEXT);\
                 INSERT INTO \"ordér 1\" VALUES (7, 'seven');\
                 CREATE TABLE wide ({});\
                 INSERT INTO wide DEFAULT VALUES;",
                wide.join(", ")
            ),
        );
        assert_eq!(
            ask(&connection, "SELECT \"select\", \"from\" FROM \"ordér 1\""),
            "7,seven"
        );
    }

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        ask(&connection, "SELECT \"select\", \"from\" FROM \"ordér 1\""),
        "7,seven",
        "a quoted identifier with an accent in it did not survive a reopen at the {} arm",
        arm.name
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM pragma_table_info('wide')"
        ),
        "200",
        "a two hundred column table came back with a different number of columns at the {} arm",
        arm.name
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name"
        ),
        "ordér 1\nwide"
    );
}

scenario!(
    identifiers_that_need_quoting_survive_a_reopen,
    identifiers_that_need_quoting_survive_a_reopen
);

/// The statement shapes an application writes that a per-construct suite does
/// not: a compound `SELECT` as a derived table, `IN (subquery)`, a `LEFT JOIN`
/// with an aggregate, keyset pagination, and an upsert with `RETURNING`.
///
/// **The first of them is `2f820f3`**: the binder refused a compound `SELECT`
/// used as a derived table and Nikaya's document view answered HTTP 500. Every
/// one of these works on its own and it is the nesting that was missing.
fn the_statement_shapes_an_application_writes(arm: &Arm, area: &Path) {
    let database = open(arm, &area.join("shapes.rdb"));
    let connection = database.session();
    run(
        &connection,
        "CREATE TABLE p (id INTEGER PRIMARY KEY, n INTEGER, tag TEXT);\
         INSERT INTO p VALUES (1,10,'a'),(2,20,'b'),(3,30,'a'),(4,40,'c');\
         CREATE INDEX p_by_tag ON p (tag, id);",
    );

    // A compound SELECT as a derived table.
    assert_eq!(
        ask(
            &connection,
            "SELECT sum(n) FROM (SELECT n FROM p WHERE id = 1 UNION ALL SELECT n FROM p WHERE id = 3)"
        ),
        "40"
    );
    // A subquery on the right of IN.
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM p WHERE id IN (SELECT id FROM p WHERE n > 15)"
        ),
        "3"
    );
    // A LEFT JOIN with an aggregate over the missing side, which has to count
    // zero rather than one.
    assert_eq!(
        ask(
            &connection,
            "SELECT p.id, count(q.id) FROM p LEFT JOIN p q ON q.id = p.id + 1 \
             GROUP BY p.id ORDER BY p.id"
        ),
        "1,1\n2,1\n3,1\n4,0"
    );
    // Keyset pagination descending, which is the read every list screen does.
    assert_eq!(
        ask(
            &connection,
            "SELECT id FROM p WHERE (tag, id) < ('b', 99) ORDER BY tag DESC, id DESC LIMIT 2"
        ),
        "2\n3"
    );
    // An upsert that returns what it wrote.
    assert_eq!(
        ask(
            &connection,
            "INSERT INTO p VALUES (1, 99, 'a') ON CONFLICT(id) DO UPDATE SET n = 99 \
             RETURNING id, n"
        ),
        "1,99"
    );
}

scenario!(
    the_statement_shapes_an_application_writes,
    the_statement_shapes_an_application_writes
);

/// A statement at the size limits: ten thousand literals in an `IN` list, a
/// five thousand row `INSERT ... VALUES`, and two hundred nested parentheses.
///
/// **Each answers or refuses by name; none of them may end the process.** A
/// parser that recursed on nesting would fail here by overflowing the stack,
/// which is not a refusal and is not something the caller can act on.
fn statements_at_their_size_limits_answer_or_refuse(arm: &Arm, area: &Path) {
    let database = open(arm, &area.join("limits.rdb"));
    let connection = database.session();
    run(
        &connection,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    );

    let literals: Vec<String> = (1..=10_000).map(|at| at.to_string()).collect();
    let in_list = format!(
        "SELECT count(*) FROM t WHERE id IN ({})",
        literals.join(",")
    );
    match connection.query(&in_list) {
        Ok(rows) => assert_eq!(
            inillucent_compat::stories::table(&rows),
            "0",
            "an IN list of ten thousand literals over an empty table is zero rows"
        ),
        Err(why) => assert_eq!(
            why.code(),
            PrimaryCode::Error,
            "an IN list of ten thousand literals refused with {} ({:?})",
            why.message(),
            why.code()
        ),
    }

    let values: Vec<String> = (1..=5_000)
        .map(|at| format!("({at}, 'row {at}')"))
        .collect();
    let wide_insert = format!("INSERT INTO t VALUES {}", values.join(","));
    match connection.execute(&wide_insert) {
        Ok(_) => assert_eq!(
            ask(&connection, "SELECT count(*) FROM t"),
            "5000",
            "a five thousand row INSERT wrote a different number of rows at the {} arm",
            arm.name
        ),
        Err(why) => assert_eq!(
            why.code(),
            PrimaryCode::Error,
            "a five thousand row INSERT refused with {} ({:?})",
            why.message(),
            why.code()
        ),
    }

    let nested = format!("SELECT {}1{}", "(".repeat(200), ")".repeat(200));
    match connection.query(&nested) {
        Ok(rows) => assert_eq!(inillucent_compat::stories::table(&rows), "1"),
        Err(why) => assert!(
            matches!(why.code(), PrimaryCode::Error | PrimaryCode::TooBig),
            "two hundred nested parentheses refused with {} ({:?}), which is neither the \
             documented refusal nor an answer",
            why.message(),
            why.code()
        ),
    }
}

scenario!(
    statements_at_their_size_limits_answer_or_refuse,
    statements_at_their_size_limits_answer_or_refuse
);

/// `ALTER TABLE ADD COLUMN` then an index on the column it added, asked two
/// ways.
///
/// Rule 1.6 over the shape `003_embedded_flag.sql` is: an index built over a
/// column that did not exist when the table was written has to answer the same
/// question the table answers.
fn an_index_over_an_added_column_agrees_with_its_table(arm: &Arm, area: &Path) {
    let path = area.join("altered.rdb");
    {
        let database = open(arm, &path);
        let connection = database.session();
        run(
            &connection,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
        );
        let mut batch = String::from("BEGIN;");
        for at in 1..=500 {
            batch.push_str(&format!("INSERT INTO t VALUES ({at}, 'row {at}');"));
        }
        batch.push_str("COMMIT;");
        run(&connection, &batch);
        run(&connection, "ALTER TABLE t ADD COLUMN marked INTEGER");
        run(&connection, "CREATE INDEX t_by_marked ON t (marked)");
        run(&connection, "UPDATE t SET marked = id * 2 WHERE id % 3 = 0");
    }

    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    let through_the_table = ask(
        &connection,
        "SELECT count(*) FROM t WHERE id % 3 = 0 AND marked IS NOT NULL",
    );
    let through_the_index = ask(
        &connection,
        "SELECT count(*) FROM t WHERE marked IS NOT NULL",
    );
    assert_eq!(
        through_the_table, through_the_index,
        "the index over the added column disagrees with its table at the {} arm",
        arm.name
    );
    assert_eq!(
        through_the_table, "166",
        "500 rows, every third marked, is 166 - the count is {through_the_table} at the {} arm",
        arm.name
    );
    // And a covering read through the index answers the same values.
    assert_eq!(
        ask(
            &connection,
            "SELECT marked FROM t WHERE marked BETWEEN 6 AND 18 ORDER BY marked"
        ),
        "6\n12\n18"
    );
}

scenario!(
    an_index_over_an_added_column_agrees_with_its_table,
    an_index_over_an_added_column_agrees_with_its_table
);

/// A table dropped and recreated under the same name inside a transaction that
/// is then rolled back keeps its original rows.
///
/// **This is task-2043, and this test is how it was found.** A `DROP TABLE` on
/// its own rolls back correctly; a `DROP` followed by a `CREATE` of the same
/// name and then a `ROLLBACK` loses every row, the loss survives a reopen, and
/// `PRAGMA integrity_check` answers `ok` over it. The pinned SQLite answers
/// three rows to both.
///
/// The assertion is SQLite's answer, and the line in
/// `tests/workloads/edges/allow.list` is what records that this engine does not
/// give it yet. **When task-2043 lands, that line has to go or this test
/// fails**, which is what makes the entry a record of a defect rather than a
/// place it can hide.
fn a_dropped_and_recreated_table_rolls_back(arm: &Arm, area: &Path) {
    let path = area.join("recreated.rdb");
    {
        let database = open(arm, &path);
        let connection = database.session();
        run(
            &connection,
            "CREATE TABLE p (id INTEGER PRIMARY KEY, n INTEGER);\
             INSERT INTO p VALUES (1,10),(2,20),(3,30);",
        );
        assert_eq!(ask(&connection, "SELECT count(*) FROM p"), "3");

        // A drop on its own, rolled back: both engines answer three.
        run(&connection, "BEGIN");
        run(&connection, "DROP TABLE p");
        run(&connection, "ROLLBACK");
        assert_eq!(
            ask(&connection, "SELECT count(*) FROM p"),
            "3",
            "a DROP TABLE on its own did not roll back, at the {} arm",
            arm.name
        );

        // The drop and the recreate together.
        run(&connection, "BEGIN");
        run(&connection, "DROP TABLE p");
        run(
            &connection,
            "CREATE TABLE p (id INTEGER PRIMARY KEY, n INTEGER)",
        );
        run(&connection, "INSERT INTO p VALUES (9, 9)");
        run(&connection, "ROLLBACK");
        as_sqlite_answers(
            arm,
            "a_dropped_and_recreated_table_rolls_back",
            &ask(&connection, "SELECT id, n FROM p ORDER BY id"),
            "1,10\n2,20\n3,30",
            "a DROP and a CREATE of the same name inside a rolled back transaction",
        );
    }

    // And whatever it answers, the file opens and its own check passes - which
    // is the part that makes the loss silent.
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    as_sqlite_answers(
        arm,
        "a_dropped_and_recreated_table_rolls_back",
        &ask(&connection, "SELECT id, n FROM p ORDER BY id"),
        "1,10\n2,20\n3,30",
        "the same, after a reopen",
    );
}

scenario!(
    a_dropped_and_recreated_table_rolls_back,
    a_dropped_and_recreated_table_rolls_back
);

/// Each maintenance command, then a close, then an open.
///
/// **`bd16a3e` is why the reopen is the assertion.** `ANALYZE` on a migrated
/// database wrote a page stamped by an abandoned log stream and the file could
/// never be opened again, so what the command answered was not the problem -
/// the next open was.
fn every_maintenance_command_leaves_the_file_openable(arm: &Arm, area: &Path) {
    let path = area.join("maintained.rdb");
    let expected = {
        let database = open(arm, &path);
        let connection = database.session();
        run(
            &connection,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL, w INTEGER);\
             CREATE INDEX t_by_w ON t (w);",
        );
        let mut batch = String::from("BEGIN;");
        for at in 1..=300 {
            batch.push_str(&format!(
                "INSERT INTO t VALUES ({at}, 'row {at}', {});",
                at % 17
            ));
        }
        batch.push_str("COMMIT;");
        run(&connection, &batch);
        ask(&connection, "SELECT id, v, w FROM t ORDER BY id")
    };

    for command in ["ANALYZE", "REINDEX", "VACUUM", "PRAGMA wal_checkpoint"] {
        {
            let database = open(arm, &path);
            let connection = database.session();
            run(&connection, command);
        }
        let database = reopen_and_check(arm, &path);
        let connection = database.session();
        assert_eq!(
            ask(&connection, "SELECT id, v, w FROM t ORDER BY id"),
            expected,
            "`{command}` changed what the table reads as, at the {} arm",
            arm.name
        );
        // Through the index as well as through the table - rule 1.6, because a
        // REINDEX that rebuilt the index wrongly reads correctly from the table.
        assert_eq!(
            ask(&connection, "SELECT count(*) FROM t WHERE w = 3"),
            ask(&connection, "SELECT count(*) FROM t WHERE w + 0 = 3"),
            "`{command}` left the index disagreeing with its table, at the {} arm",
            arm.name
        );
    }
}

scenario!(
    every_maintenance_command_leaves_the_file_openable,
    every_maintenance_command_leaves_the_file_openable
);

/// A vector with a component that is not a number is refused by name, and an
/// unknown tokenizer is refused rather than substituted.
///
/// **Refused rather than substituted is the point.** A tokenizer nobody has is
/// a table whose terms would be produced by an analysis the caller did not
/// choose, and every query against it would answer something plausible.
fn retrieval_refuses_what_it_cannot_do_by_name(arm: &Arm, area: &Path) {
    let database = open(arm, &area.join("retrieval.rdb"));
    let connection = database.session();
    run(
        &connection,
        "CREATE TABLE v (id INTEGER PRIMARY KEY, e VECTOR(3))",
    );

    let not_a_number = connection
        .execute("INSERT INTO v VALUES (1, '[1,2,nan]')")
        .expect_err("a vector with a component that is not finite is refused");
    assert!(
        not_a_number.message().contains("finite"),
        "the refusal for a vector holding NaN was `{}`, which does not say what was wrong",
        not_a_number.message()
    );
    let wrong_width = connection
        .execute("INSERT INTO v VALUES (2, '[1,2]')")
        .expect_err("a vector of the wrong width is refused");
    assert!(
        wrong_width.message().contains("VECTOR(3)"),
        "the refusal for a two wide vector in a VECTOR(3) column was `{}`",
        wrong_width.message()
    );
    // And a good one is accepted, so the two refusals above are about the
    // values rather than about the column.
    run(&connection, "INSERT INTO v VALUES (3, '[0.1,0.2,0.3]')");
    assert_eq!(ask(&connection, "SELECT id FROM v"), "3");

    let unknown = connection
        .execute_batch("CREATE VIRTUAL TABLE ft USING fts5(a, tokenize='nosuchtokenizer')")
        .expect_err("an unknown tokenizer is refused");
    assert!(
        unknown.message().contains("nosuchtokenizer"),
        "the refusal for an unknown tokenizer was `{}`, which does not name it",
        unknown.message()
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM sqlite_master WHERE name = 'ft'"
        ),
        "0",
        "the table with the unknown tokenizer was created anyway, at the {} arm",
        arm.name
    );
}

scenario!(
    retrieval_refuses_what_it_cannot_do_by_name,
    retrieval_refuses_what_it_cannot_do_by_name
);

/// A database named without a directory, and one whose path holds a space and a
/// character outside ASCII.
///
/// **`066b484` is the first of these**: every delete of a file named without a
/// directory failed on Linux, because a bare name has no parent to contain it -
/// and the log segment beside the database is deleted by name at every
/// checkpoint.
fn a_path_that_is_awkward_still_opens_and_writes(arm: &Arm, area: &Path) {
    // A name with a space and a character outside ASCII, in a directory whose
    // own name has both.
    let awkward = area.join("données archivées");
    std::fs::create_dir_all(&awkward).expect("the directory is made");
    let path = awkward.join("le fichier.rdb");
    {
        let database = open(arm, &path);
        let connection = database.session();
        run(
            &connection,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);\
             INSERT INTO t VALUES (1, 'écrit');",
        );
        run(&connection, "PRAGMA wal_checkpoint");
    }
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        ask(&connection, "SELECT v FROM t"),
        "écrit",
        "a path with a space and an accent did not round trip at the {} arm",
        arm.name
    );
    drop(connection);
    drop(database);

    // A long path. Windows refuses a path past 260 characters without the long
    // path opt-in, so the case asserts that the engine either opens it or
    // refuses by name - never that it half opens one.
    let mut deep = area.to_path_buf();
    for _ in 0..8 {
        deep = deep.join("a-directory-with-quite-a-long-name-in-it");
    }
    let long = deep.join("database.rdb");
    let _ = std::fs::create_dir_all(&deep);
    assert!(
        long.to_string_lossy().len() > 270,
        "the long path case built a path of {} characters, which is not long",
        long.to_string_lossy().len()
    );
    match arm.open(&long) {
        Ok(database) => {
            let connection = database.session();
            run(&connection, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
            run(&connection, "INSERT INTO t VALUES (1)");
            assert_eq!(ask(&connection, "SELECT count(*) FROM t"), "1");
        }
        Err(why) => assert!(
            !why.message().is_empty(),
            "a path of {} characters was refused with an empty message at the {} arm",
            long.to_string_lossy().len(),
            arm.name
        ),
    }
}

scenario!(
    a_path_that_is_awkward_still_opens_and_writes,
    a_path_that_is_awkward_still_opens_and_writes
);
