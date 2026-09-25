//! What a statement is allowed to hold in memory while it answers.
//!
//! Invariant: **the request budget bounds what a statement materialises, not
//! only what it hands back.** Every case here answers with exactly one row, so
//! the result sink spends almost nothing; what each one costs is the table, the
//! set, the group map, the partition buffer or the accumulated answer it builds
//! on the way there. A budget that only counted the answer bounded none of it.
//!
//! ## What this suite is, and how it differs from `budgets.rs`
//!
//! `budgets.rs` drives the real `inillucent-mcp` binary over JSON-RPC and
//! asserts the *served* surface: that a confined server has a ceiling and that
//! hitting it is a structured refusal naming which ceiling. This suite arms a
//! budget directly on the thread and drives the engine, because the property is
//! about the operators rather than about the server: a join's build side, a
//! `DISTINCT` set, a group table, a set operation's key set, a window's
//! partition buffer and a recursive CTE's answer are six different places a
//! statement grows, and six separate `materialise` calls is what it takes to
//! bound them. Asserting them through MCP would say only that some ceiling was
//! reached somewhere.
//!
//! ## The defect (task-1932, H6)
//!
//! `Collect::push` was the engine's only `budget::spend`. It is the result
//! sink, so `Limits::served()`'s 10,000 rows and 256 MiB bounded what was
//! handed back and nothing else. Every case below completed before this ticket:
//! a join whose build side is the whole table and whose answer is one row spent
//! one row's worth of a 256 MiB cap, and the only backstop was the wall clock
//! at scan leaves.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use inillucent_base::budget::{self, Limits};
use inillucent_compat::differential::start_inillucent;
use inillucent_engine::connect::Connection;

/// Where this suite's scratch databases live.
const AREA: &str = "budget";

/// How many rows the wide table holds.
///
/// Each row carries a distinct 512-byte payload, so the table is about a
/// megabyte and every operator that materialises it is far past the budget the
/// cases arm. The number is deliberately small enough that the *unbounded* arm
/// of every case still finishes in well under a second: a bound that can only
/// be demonstrated on a table nobody wants to build is a bound nobody checks.
const ROWS: usize = 2_000;

/// The byte budget the bounded arm of each case runs under.
///
/// Under a kilobyte, and every case's answer is one integer, so the budget is
/// generous for the answer and far too small for anything the statement has to
/// hold to produce it. That gap is the whole property.
const SMALL_BYTES: u64 = 900;

/// Runs a statement for its effect.
fn exec(connection: &Connection<'_>, sql: &str) {
    connection
        .execute_batch(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
}

/// Returns a connection to a database holding one wide table.
///
/// @param name - the file's name, so two cases never share one
fn wide(name: &str) -> Connection<'static> {
    let connection = start_inillucent(AREA, name);
    exec(
        &connection,
        "CREATE TABLE wide(k INTEGER PRIMARY KEY, payload TEXT);",
    );
    exec(&connection, "BEGIN;");
    for row in 0..ROWS {
        exec(
            &connection,
            &format!("INSERT INTO wide VALUES ({row}, '{row:0>512}');"),
        );
    }
    exec(&connection, "COMMIT;");
    connection
}

/// Runs a query to completion and returns its first cell, or the refusal.
///
/// The statement is stepped to the end rather than prepared and dropped,
/// because a budget that refuses only at `prepare` would not be a budget on
/// what the statement holds while it runs.
///
/// @param connection - the connection to run on
/// @param sql - the query
fn answer(connection: &Connection<'_>, sql: &str) -> Result<String, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| error.message().to_string())?;
    let mut first = String::new();
    loop {
        match statement.step() {
            Ok(true) => {
                if first.is_empty() {
                    first = statement
                        .row()
                        .first()
                        .map(|value| format!("{value:?}"))
                        .unwrap_or_default();
                }
            }
            Ok(false) => return Ok(first),
            Err(error) => return Err(error.message().to_string()),
        }
    }
}

/// The six statements, each answering with exactly one row.
///
/// The name is what the operator is, so a failure says which of the six is
/// unbounded rather than which SQL string did not fail.
const CASES: [(&str, &str); 7] = [
    // **The two that were missing, which is what task-2066 §4.3.6 is about.**
    // Every other buffering operator has been on this list since task-1932;
    // `Sort` and `TopN` held every surviving row and `limit` of them and
    // charged nothing, so a sort larger than memory was an out-of-memory kill
    // where every shape below gets a refusal naming the byte budget.
    (
        "the sort's buffer",
        "SELECT count(*) FROM (SELECT payload FROM wide ORDER BY payload)",
    ),
    (
        "the bounded sort's heap",
        "SELECT count(*) FROM (SELECT payload FROM wide ORDER BY payload LIMIT 200)",
    ),
    (
        "the hash join's build side",
        "SELECT count(*) FROM wide AS a JOIN wide AS b ON a.payload = b.payload",
    ),
    (
        "the DISTINCT set",
        "SELECT count(*) FROM (SELECT DISTINCT payload FROM wide)",
    ),
    (
        "the group table",
        "SELECT count(*) FROM (SELECT payload, count(*) AS n FROM wide GROUP BY payload)",
    ),
    (
        "the set operation's key set",
        "SELECT count(*) FROM (SELECT payload FROM wide UNION SELECT payload FROM wide)",
    ),
    (
        "the recursive CTE's answer",
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 20000) \
         SELECT count(*) FROM n",
    ),
];

/// Every statement answers when nothing is armed.
///
/// The other half of the pair below. A refusal is only evidence that the budget
/// bounds something if the same statement succeeds without it - otherwise a
/// case that refuses because the SQL is wrong reads exactly the same.
#[test]
fn every_case_answers_with_one_row_when_no_budget_is_armed() {
    let connection = wide("unbounded");
    for (what, sql) in CASES {
        let outcome = answer(&connection, sql);
        assert!(
            outcome.is_ok(),
            "{what}: `{sql}` failed with no budget armed: {:?}",
            outcome.err()
        );
    }
}

/// A statement that has to hold more than its byte budget is refused, even when
/// its answer is one row.
///
/// **All six completed before this (task-1932, H6).** The refusal has to name
/// the byte budget rather than any other failure, because "it errored" is
/// satisfied by a typo in the SQL and by an engine that fell over.
#[test]
fn a_statement_that_materialises_more_than_its_budget_is_refused() {
    let connection = wide("bounded");

    let mut completed = Vec::new();
    let mut wrong = Vec::new();
    for (what, sql) in CASES {
        let guard = budget::arm(
            Limits::unbounded().with_bytes(Some(SMALL_BYTES)),
            Arc::new(AtomicBool::new(false)),
        );
        let outcome = answer(&connection, sql);
        drop(guard);
        match outcome {
            Ok(answered) => completed.push(format!("{what}: answered `{answered}`")),
            Err(message) => {
                if !message.contains("bytes") {
                    wrong.push(format!("{what}: refused, but said `{message}`"));
                }
            }
        }
    }

    assert!(
        completed.is_empty(),
        "these statements held more than {SMALL_BYTES} bytes and were allowed to finish:\n{}",
        completed.join("\n")
    );
    assert!(
        wrong.is_empty(),
        "these statements were refused for something other than the byte budget, so the \
         refusal does not tell a caller to ask for less:\n{}",
        wrong.join("\n")
    );
}

/// The window's partition buffer is bounded too.
///
/// **It needs its own case, because a window cannot answer with one row.** It
/// produces one row per input row, and this engine refuses a window inside a
/// derived table - `SELECT count(*) FROM (SELECT row_number() OVER (...) FROM
/// wide)` is answered by SQLite and refused here with "the new engine's
/// physical pass does not handle a window function reaching the pipeline
/// builder yet". That gap is outside this ticket and is reported on it.
///
/// So this case is built the other way round. It measures what the statement
/// spends with no ceiling, then arms a budget between what the answer costs and
/// what the whole pass costs, and asserts the pass is refused. Measuring rather
/// than writing a number down is what makes it hold when the row count or the
/// payload width changes.
///
/// A window pass holds the buffered input, a widened copy of it, and one
/// re-sorted copy per frame after the first. Before this ticket only the first
/// was charged, and the budget below is under what the pass holds and over what
/// the inner query buffers - so the statement completed.
#[test]
fn a_window_that_buffers_more_than_its_budget_is_refused() {
    let connection = wide("window");
    // Two frames, in two different orders, so the pass holds a re-sorted copy
    // as well as the widened one.
    let sql = "SELECT payload, row_number() OVER (ORDER BY k) AS up, \
               row_number() OVER (ORDER BY k DESC) AS down FROM wide";

    let measured = budget::arm(Limits::unbounded(), Arc::new(AtomicBool::new(false)));
    let unbounded = answer(&connection, sql);
    let (_, whole) = budget::spent();
    drop(measured);
    assert!(
        unbounded.is_ok(),
        "the window statement failed with no budget armed: {:?}",
        unbounded.err()
    );

    // What the inner query buffers, measured the same way: the same statement
    // with the window calls taken off, which is the one copy that was charged
    // before this ticket.
    let plain = budget::arm(Limits::unbounded(), Arc::new(AtomicBool::new(false)));
    let flat = answer(&connection, "SELECT payload FROM wide ORDER BY k");
    let (_, buffered) = budget::spent();
    drop(plain);
    assert!(flat.is_ok(), "the plain statement failed: {:?}", flat.err());

    assert!(
        whole > buffered.saturating_mul(2),
        "a window pass over the wide table spent {whole} bytes and the same rows without \
         the windows spent {buffered}; the pass is no longer holding the copies this case \
         is about"
    );

    // Between the two: more than the input the statement reads, less than the
    // copies the pass makes of it.
    let ceiling = buffered.saturating_mul(2);
    let bounded_guard = budget::arm(
        Limits::unbounded().with_bytes(Some(ceiling)),
        Arc::new(AtomicBool::new(false)),
    );
    let bounded = answer(&connection, sql);
    drop(bounded_guard);

    let message = bounded.err().unwrap_or_else(|| {
        panic!("a window pass that spends {whole} bytes finished under a {ceiling} byte budget")
    });
    assert!(
        message.contains("bytes"),
        "the window statement was refused for something other than the byte budget: \
         `{message}`"
    );
}

/// The row budget still counts only the rows a caller is handed.
///
/// **The half of H6 that is a non-change, and it needs a test of its own.**
/// Charging materialised rows against `Limits::rows` would have been the
/// obvious way to bound a join's build side, and it would have made
/// `Rows::total` mean "rows this statement touched" - so an MCP request asking
/// for ten rows out of a million-row join would be refused for exceeding a
/// ten-thousand row ceiling it never came near. The bytes are charged
/// everywhere; the rows are charged at the sink.
#[test]
fn the_row_budget_counts_the_answer_and_not_what_was_read() {
    let connection = wide("rows");

    let guard = budget::arm(
        Limits::unbounded().with_rows(Some(16)),
        Arc::new(AtomicBool::new(false)),
    );
    let outcome = answer(
        &connection,
        "SELECT count(*) FROM wide AS a JOIN wide AS b ON a.payload = b.payload",
    );
    let (rows, _) = budget::spent();
    drop(guard);

    assert!(
        outcome.is_ok(),
        "a join over {ROWS} rows answering one row was refused by a sixteen row budget: {:?}",
        outcome.err()
    );
    assert!(
        rows <= 16,
        "the row budget counted {rows} rows for a statement that answered with one, so \
         `Rows::total` no longer means the size of the answer"
    );
}
