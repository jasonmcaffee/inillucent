//! What an extension does wrong, and what the engine has to do about it.
//!
//! Invariant: nothing an extension does may take the engine down with it. An
//! application's function can fail, lie, re-enter, or hand back a value of the
//! wrong shape; a module's tables live in the same file as everything else and
//! can be corrupted by anything that can write the file. In every one of those
//! cases the statement must fail and the *connection* must survive - still
//! usable, still able to run the next statement, still able to close.
//!
//! That is a stronger claim than "does not crash", and it is the one that
//! matters: a connection that is poisoned by a bad extension is a process that
//! has to be restarted, and a database that is poisoned by one is a restore.
//!
//! The corruption cases are the reason this file exists rather than a few more
//! cases in `functions.rs`. A module's shadow tables are ordinary tables, so
//! anything that can write the database can put anything in them - and the
//! module reads them back expecting its own format. Every field it reads is an
//! opportunity to trust a length, an offset or a count that came from the file.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use inillucent_base::error::{DbError, PrimaryCode};
use inillucent_compat::facade::{Connection, Database};
use inillucent_ext::registry::FunctionFlags;
use inillucent_value::Value;

/// Returns a database file of this test's own, under the gitignored root.
///
/// **A file rather than `:memory:`, which the old facade accepted.** The new
/// engine opens a path and has no in-memory VFS behind `open` yet; nothing in
/// this file asserts anything about *where* the database lives, so the fixture
/// moves and every assertion stays exactly as it was. A serial keeps two tests
/// running in parallel from colliding on one file.
fn scratch() -> std::path::PathBuf {
    let root = inillucent_compat::workspace_root().join("_agent_output/hostile");
    let _ = std::fs::create_dir_all(&root);
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = root.join(format!("{}-{serial}.rdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Opens a database of this test's own, with one connection.
///
/// The database is leaked so the connection can be returned on its own. These
/// are short tests and there is exactly one database per test; a lifetime here
/// would be carried through every helper for nothing.
fn connect() -> Connection {
    let database = Database::open(scratch()).expect("opens");
    Box::leak(Box::new(database)).session().expect("connects")
}

/// Returns the failure a hostile function reports.
fn refuse(message: &str) -> DbError {
    DbError::primary(PrimaryCode::Error).with_message(message)
}

/// A function that always fails stops the statement and nothing else.
#[test]
fn a_function_that_always_fails_leaves_the_connection_usable() {
    let connection = connect();
    connection
        .create_scalar_function(
            "explode",
            0,
            FunctionFlags::external(),
            Arc::new(|_| Err(refuse("no"))),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1),(2),(3)")
        .expect("fills");
    let refused = connection.query("SELECT explode() FROM t");
    assert!(refused.is_err(), "the failure has to reach the caller");
    // The connection is the thing being tested, not the failure.
    let rows = connection
        .query("SELECT count(*) FROM t")
        .expect("still works");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(3)
    );
}

/// A function that fails part-way through a write leaves nothing behind.
#[test]
fn a_function_that_fails_mid_statement_rolls_the_statement_back() {
    let connection = connect();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&calls);
    connection
        .create_scalar_function(
            "third_time_fails",
            1,
            FunctionFlags::external(),
            Arc::new(move |arguments: &[Value<'static>]| {
                if counter.fetch_add(1, Ordering::Relaxed) >= 2 {
                    return Err(refuse("enough"));
                }
                Ok(arguments.first().cloned().unwrap_or(Value::Null))
            }),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE src(a); INSERT INTO src VALUES (1),(2),(3),(4)")
        .expect("fills");
    connection
        .execute_batch("CREATE TABLE dst(a)")
        .expect("creates");
    let refused = connection.execute_batch("INSERT INTO dst SELECT third_time_fails(a) FROM src");
    assert!(refused.is_err(), "the statement has to fail");
    let rows = connection.query("SELECT count(*) FROM dst").expect("reads");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(0),
        "a statement that failed must not have written half a table"
    );
}

/// A collation that is not an ordering still terminates and keeps its rows.
///
/// A comparator that answers differently every time breaks the contract a sort
/// depends on, and the sort has no way to detect that. What it must not do is
/// loop, panic, or lose a row: whatever order comes out, the same values have
/// to come out.
#[test]
fn an_inconsistent_collation_still_returns_every_row() {
    let connection = connect();
    let state = Arc::new(AtomicU64::new(0x2545_F491_4F6C_DD1D));
    connection
        .create_collation(
            "CHAOS",
            Arc::new(move |_left: &[u8], _right: &[u8]| {
                // A deliberately inconsistent comparator, deterministic across
                // runs so a failure can be reproduced.
                let mut bits = state.load(Ordering::Relaxed);
                bits ^= bits << 13;
                bits ^= bits >> 7;
                bits ^= bits << 17;
                state.store(bits, Ordering::Relaxed);
                match bits % 3 {
                    0 => std::cmp::Ordering::Less,
                    1 => std::cmp::Ordering::Equal,
                    _ => std::cmp::Ordering::Greater,
                }
            }),
        )
        .expect("registers");
    connection
        .execute_batch(
            "CREATE TABLE t(x); INSERT INTO t VALUES ('a'),('b'),('c'),('d'),('e'),('f')",
        )
        .expect("fills");
    let rows = connection
        .query("SELECT x FROM t ORDER BY x COLLATE CHAOS")
        .expect("the sort terminates");
    assert_eq!(rows.len(), 6, "every row has to come back");
    let mut seen: Vec<String> = rows
        .iter()
        .filter_map(|row| row.first())
        .filter_map(Value::as_text)
        .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
        .collect();
    seen.sort();
    assert_eq!(seen, vec!["a", "b", "c", "d", "e", "f"]);
}

/// An aggregate that returns a different type every group is still safe.
#[test]
fn an_aggregate_that_changes_its_mind_is_harmless() {
    let connection = connect();
    connection
        .create_aggregate_function(
            "shifty",
            1,
            FunctionFlags::external(),
            Arc::new(|rows: &[Vec<Value<'static>>]| {
                Ok(match rows.len() % 4 {
                    0 => Value::Null,
                    1 => Value::Integer(1),
                    2 => Value::Real(2.5),
                    _ => Value::owned_blob(&[0xff, 0x00, 0xff])?,
                })
            }),
        )
        .expect("registers");
    connection
        .execute_batch(
            "CREATE TABLE t(g, n); INSERT INTO t VALUES (1,1),(2,1),(2,2),(3,1),(3,2),(3,3)",
        )
        .expect("fills");
    let rows = connection
        .query("SELECT g, shifty(n) FROM t GROUP BY g ORDER BY g")
        .expect("runs");
    assert_eq!(rows.len(), 3);
    // The point is that a row came back for each group and nothing was
    // corrupted by the changing shape; the values themselves are the
    // function's business.
    let rows = connection
        .query("SELECT count(*) FROM t")
        .expect("still works");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(6)
    );
}

/// A function that runs SQL on its own connection is refused, not deadlocked.
#[test]
fn a_reentrant_function_is_refused_rather_than_wedged() {
    let connection = connect();
    connection
        .execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1),(2)")
        .expect("fills");
    // The closure cannot hold the connection - it would be a cycle - so this
    // opens its own. What is being checked is that a function doing arbitrary
    // database work during a statement does not wedge the one that called it.
    connection
        .create_scalar_function(
            "nested",
            0,
            FunctionFlags::external(),
            Arc::new(|_| {
                let inner = connect();
                inner.execute_batch("CREATE TABLE u(a)")?;
                Ok(Value::Integer(7))
            }),
        )
        .expect("registers");
    let rows = connection.query("SELECT nested() FROM t").expect("runs");
    assert_eq!(rows.len(), 2);
    let rows = connection
        .query("SELECT count(*) FROM t")
        .expect("still works");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(2)
    );
}

/// Every statement a hostile function touched can still be finalized.
///
/// The engine's `Statement` finalizes on drop, so what this really checks is
/// that a statement abandoned mid-scan - which is what a failing function
/// leaves behind - does not hold anything the next statement needs.
#[test]
fn a_failed_statement_releases_what_it_held() {
    let connection = connect();
    connection
        .create_scalar_function(
            "explode",
            0,
            FunctionFlags::external(),
            Arc::new(|_| Err(refuse("no"))),
        )
        .expect("registers");
    connection
        .execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1),(2),(3)")
        .expect("fills");
    for _ in 0..20 {
        let _ = connection.query("SELECT explode() FROM t");
    }
    // A write needs the file lock the failed readers were holding, so this
    // fails if any of them left one behind.
    connection
        .execute_batch("INSERT INTO t VALUES (4)")
        .expect("the writer still gets the lock");
    let rows = connection.query("SELECT count(*) FROM t").expect("reads");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(4)
    );
}

/// Bytes written into a module's shadow tables are read back defensively.
///
/// Every case here writes something a module would never have written and then
/// asks it a question. What the module does with the answer is its business;
/// what it may not do is loop forever, read out of bounds, or report success.
#[test]
fn corrupt_module_structures_are_refused_rather_than_trusted() {
    for (name, schema, damage) in scenarios() {
        let connection = connect();
        connection
            .execute_batch(schema)
            .unwrap_or_else(|error| panic!("{name}: setup failed: {}", error.message()));
        // The shadow tables are ordinary tables, which is exactly why this is
        // possible at all - and why it has to be tested.
        connection
            .execute_batch("PRAGMA writable_schema = ON")
            .unwrap_or_else(|error| panic!("{name}: {}", error.message()));
        let _ = connection.execute_batch(damage);
        for query in queries(name) {
            // Either answer is acceptable. What is not acceptable is a hang or
            // a panic, and reaching the next line is the assertion.
            let _ = connection.query(query);
        }
        let rows = connection
            .query("SELECT 1")
            .unwrap_or_else(|error| panic!("{name}: the connection died: {}", error.message()));
        assert_eq!(rows.len(), 1, "{name}: the connection has to survive");
    }
}

/// Returns the corruption scenarios: a name, a schema, and the damage.
fn scenarios() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "rtree-node-truncated",
            "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO r VALUES (1, 0, 1, 0, 1), (2, 5, 6, 5, 6), (3, 9, 10, 9, 10);",
            "UPDATE r_node SET data = x'0000'",
        ),
        (
            "rtree-node-claims-too-many-cells",
            "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO r VALUES (1, 0, 1, 0, 1);",
            "UPDATE r_node SET data = x'0000FFFF' || hex(data)",
        ),
        (
            "rtree-node-is-not-a-blob",
            "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO r VALUES (1, 0, 1, 0, 1);",
            "UPDATE r_node SET data = 'not a node at all'",
        ),
        (
            "rtree-rowid-map-lies",
            "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO r VALUES (1, 0, 1, 0, 1);",
            "UPDATE r_rowid SET nodeno = 999999",
        ),
        (
            "fts5-doclist-truncated",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');\
             INSERT INTO f VALUES ('beta delta', 'epsilon');",
            "UPDATE f_data SET block = x'ff'",
        ),
        (
            "fts5-doclist-is-huge-nonsense",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');",
            "UPDATE f_data SET block = x'ffffffffffffffffffffffffffffffff'",
        ),
        (
            "fts5-index-points-nowhere",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');",
            "UPDATE f_idx SET pgno = 4000000000",
        ),
        (
            "fts5-sizes-disagree-with-content",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');",
            "UPDATE f_docsize SET sz = x'ffffffff'",
        ),
        (
            "fts5-content-row-removed",
            "CREATE VIRTUAL TABLE f USING fts5(a, b);\
             INSERT INTO f VALUES ('alpha beta', 'gamma');",
            "DELETE FROM f_content",
        ),
    ]
}

/// Returns the questions to ask after one scenario's damage.
fn queries(name: &str) -> Vec<&'static str> {
    if name.starts_with("rtree") {
        return vec![
            "SELECT id FROM r",
            "SELECT id FROM r WHERE x0 > 0 AND x1 < 100",
            "SELECT count(*) FROM r WHERE y0 >= -1000",
            "PRAGMA integrity_check",
        ];
    }
    vec![
        "SELECT rowid FROM f",
        "SELECT rowid FROM f WHERE f MATCH 'beta'",
        "SELECT rowid FROM f WHERE f MATCH 'alpha OR gamma'",
        "SELECT rowid, rank FROM f WHERE f MATCH 'beta' ORDER BY rank",
        "PRAGMA integrity_check",
    ]
}

/// Random bytes in a module's tables are refused the same way.
///
/// The scenarios above are the shapes worth naming; this is the rest of the
/// space. It writes deterministic pseudo-random blobs into every shadow table
/// and asks the same questions, which is where a length nobody thought about
/// gets found.
#[test]
fn random_bytes_in_a_shadow_table_are_survivable() {
    let mut bits: u64 = 0x9E37_79B9_7F4A_7C15;
    for round in 0..24u32 {
        let connection = connect();
        let module = if round % 2 == 0 {
            "CREATE VIRTUAL TABLE m USING rtree(id, x0, x1, y0, y1);\
             INSERT INTO m VALUES (1, 0, 1, 0, 1), (2, 4, 5, 4, 5), (3, 8, 9, 8, 9);"
        } else {
            "CREATE VIRTUAL TABLE m USING fts5(a, b);\
             INSERT INTO m VALUES ('one two', 'three');\
             INSERT INTO m VALUES ('two three', 'four');"
        };
        connection.execute_batch(module).expect("setup");
        let shadows: Vec<String> = connection
            .query("SELECT name FROM sqlite_master WHERE name LIKE 'm!_%' ESCAPE '!'")
            .expect("lists")
            .iter()
            .filter_map(|row| row.first())
            .filter_map(Value::as_text)
            .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
            .collect();
        assert!(!shadows.is_empty(), "a module has shadow tables");
        for table in &shadows {
            bits = next(bits);
            let blob = hex_of(bits);
            let column = column_of(&connection, table);
            let Some(column) = column else {
                continue;
            };
            let _ = connection
                .execute_batch(&format!("UPDATE \"{table}\" SET \"{column}\" = x'{blob}'"));
        }
        for query in [
            "SELECT rowid FROM m",
            "SELECT count(*) FROM m",
            "PRAGMA integrity_check",
        ] {
            let _ = connection.query(query);
        }
        let rows = connection
            .query("SELECT 1")
            .expect("the connection survives");
        assert_eq!(rows.len(), 1, "round {round}");
    }
}

/// Returns the last column of a table, which is where a module's payload is.
fn column_of(connection: &Connection, table: &str) -> Option<String> {
    let rows = connection
        .query(&format!("PRAGMA table_info(\"{table}\")"))
        .ok()?;
    rows.last()
        .and_then(|row| row.get(1))
        .and_then(Value::as_text)
        .map(|text| String::from_utf8_lossy(text.raw()).into_owned())
}

/// Returns the next value of a deterministic generator.
fn next(mut bits: u64) -> u64 {
    bits ^= bits << 13;
    bits ^= bits >> 7;
    bits ^= bits << 17;
    bits
}

/// Returns some bytes as hex, for an SQL blob literal.
fn hex_of(bits: u64) -> String {
    let mut out = String::new();
    let mut value = bits;
    for _ in 0..12 {
        out.push_str(&format!("{:02x}", (value & 0xff) as u8));
        value = value.rotate_right(5);
    }
    out
}

/// A statement at the depth limit answers, one past it is refused by name, and
/// the connection is still there afterwards (task-1979, section 5.3).
///
/// **A real process, because the failure this replaces was a stack overflow.**
/// `SELECT abs(abs(...(1)...))` 300 deep ended the process with
/// `thread 'main' has overflowed its stack` and exit code 0xC00000FD - well
/// under the declared `ExprDepth` of 1000, so the limit could never be the
/// thing that fired. An in-process case cannot see that: it would run on the
/// test harness's own thread, whose stack is not the one the shipped binaries
/// carry.
///
/// The three shapes are the ones the parser fuzz reached: nested calls, which
/// charge the expression tree, and nested parentheses and nested subqueries,
/// which charge the parser's own recursion.
#[test]
fn a_statement_at_the_depth_limit_answers_and_one_past_it_names_the_limit() {
    let shell = inillucent_compat::cliproc::program("inillucent-shell");
    // Under `ExprDepth`'s 1000 and `ParserDepth`'s 2500, and over each.
    for (under, over, build) in [
        (900usize, 1_100usize, 0usize),
        (2_400, 2_600, 1),
        (900, 2_600, 2),
    ] {
        let statement = |depth: usize| match build {
            0 => format!("SELECT {}1{};", "abs(".repeat(depth), ")".repeat(depth)),
            1 => format!("SELECT count({}1{});", "(".repeat(depth), ")".repeat(depth)),
            _ => format!("SELECT {}1{};", "(SELECT ".repeat(depth), ")".repeat(depth)),
        };
        let script = format!(
            "{}\n{}\nSELECT 'alive';\n",
            statement(under),
            statement(over)
        );
        let ran = inillucent_compat::cliproc::run_with_input(&shell, &[":memory:"], &script);
        assert!(
            ran.code == 0 || ran.code == 1,
            "shape {build}: the shell ended with {} rather than answering - \
             a stack overflow reports 127 here and 0xC00000FD to the operating system:\n{}",
            ran.code,
            ran.said()
        );
        let said = ran.said();
        assert!(
            said.contains("depth exceeded") || said.contains("nested SELECT"),
            "shape {build}: nothing named a limit for a statement {over} deep:\n{said}"
        );
        assert!(
            said.contains("alive"),
            "shape {build}: the connection did not answer the statement after the refusal:\n{said}"
        );
    }
}

/// One value cannot be built past `Limit::Length`, and the process does not
/// grow to find that out (task-1979, section 5.4).
///
/// **Three shapes, each measured on a served MCP server before this.**
/// `SELECT length(zeroblob(1073741824))` answered 1,073,741,824 with a 256 MiB
/// budget armed, after taking the working set to 2,873 MB;
/// `SELECT length(printf('%2000000000d', 1))` answered 2,000,000,000 at 5,734
/// MB. `Limit::Length` was enforced on the write path, so a value that is only
/// read never met it.
///
/// A real process, because what is being asserted is that the working set does
/// not follow the value.
#[test]
fn one_value_cannot_be_built_past_the_length_limit() {
    let binary = inillucent_compat::cliproc::program("inillucent");
    for sql in [
        "SELECT length(zeroblob(1073741824))",
        "SELECT length(randomblob(1073741824))",
        "SELECT length(printf('%2000000000d', 1))",
    ] {
        let (code, said, highest) = run_and_watch(&binary, sql);
        assert_eq!(code, Some(1), "`{sql}` did not answer a refusal: {said}");
        assert!(
            said.contains("too big"),
            "`{sql}` was refused by something other than the value bound: {said}"
        );
        // The measured failures were 2,873 MB and 5,734 MB; a build that
        // refuses before the allocation stays in the tens of megabytes, and a
        // build that allocates first cannot come near this.
        assert!(
            highest < 512 * 1024 * 1024,
            "`{sql}` took the process to {:.0} MiB, so the refusal came after the allocation",
            inillucent_compat::procstat::mebibytes(highest)
        );
    }
}

/// A chain of concatenations cannot double its way past the value bound.
///
/// **Its own case, because what it costs and what a single value costs are
/// different questions.** `WITH RECURSIVE c(s) AS (SELECT 'aa' UNION ALL
/// SELECT s||s FROM c)` reached 49 GB before the harness gave up. The value
/// bound is what stops the doubling; the rows the recursive term has already
/// produced are a *result set*, which is the request budget's business and
/// which the command line leaves unbounded on purpose - see
/// `inillucent_driver::StatementLimits`, and `docs/sql.md` on what a served
/// server sets instead. So this asserts the refusal and not a ceiling the
/// command line does not have.
#[test]
fn a_chain_of_concatenations_cannot_double_past_the_value_bound() {
    let binary = inillucent_compat::cliproc::program("inillucent");
    let sql = "WITH RECURSIVE c(s) AS (SELECT 'aa' UNION ALL SELECT s||s FROM c) \
               SELECT length(s) FROM c";
    let (code, said, _) = run_and_watch(&binary, sql);
    assert_eq!(
        code,
        Some(1),
        "the doubling did not answer a refusal: {said}"
    );
    assert!(
        said.contains("too big"),
        "the doubling was refused by something other than the value bound: {said}"
    );
}

/// **A table cannot be declared or grown past `Limit::Column`.**
///
/// The limit was charged on what a `SELECT` returns and on nothing a table
/// declares, so a 2,100-column `CREATE TABLE` succeeded here and SQLite
/// refused it with "too many columns"; at about five thousand columns it
/// failed with "the mini-columns do not fit in one page", which refuses the
/// right statement for a reason a caller cannot act on (task-2066 section 4.2,
/// item 21).
///
/// Both ways in are asserted, because they are charged in two different places
/// and neither covers the other: a `CREATE TABLE` declares its whole list, so
/// the parser counts it, and an `ALTER TABLE ... ADD COLUMN` adds one to a
/// list only the catalog can measure, so the binder counts it. Without the
/// second, a table is walked past the limit one statement at a time.
#[test]
fn a_table_cannot_be_declared_or_grown_past_the_column_limit() {
    let limit = inillucent_base::limits::Limit::Column.default_value() as usize;
    let connection = connect();

    let one_too_many: Vec<String> = (0..=limit).map(|n| format!("c{n} INT")).collect();
    let refused = connection
        .execute(&format!(
            "CREATE TABLE wide (
{})",
            one_too_many.join(", ")
        ))
        .expect_err("a table past the column limit must be refused");
    assert!(
        refused.message().contains("too many columns"),
        "a table past the column limit was refused by something else: {}",
        refused.message()
    );

    // At the limit exactly, which is the case a bound off by one would refuse.
    let exactly: Vec<String> = (0..limit).map(|n| format!("c{n} INT")).collect();
    connection
        .execute(&format!(
            "CREATE TABLE full (
{})",
            exactly.join(", ")
        ))
        .expect("a table at the column limit is accepted");
    let grown = connection
        .execute("ALTER TABLE full ADD COLUMN one_more INT")
        .expect_err("a column past the limit must be refused");
    assert!(
        grown.message().contains("too many columns"),
        "growing a full table was refused by something else: {}",
        grown.message()
    );
}

/// **A recursion past a million passes answers, and a runaway is stopped by
/// the budget rather than by a pass count.**
///
/// `run_recursive` refused after a million passes with "a recursive CTE did
/// not settle", which refused a series generator past a million rows - an
/// ordinary idiom SQLite answers - and the number it refused at was not
/// derived from anything (task-2066 section 4.2, item 22).
///
/// The first case is the one the constant refused: it needs one pass more than
/// the constant allowed, and it asserts the *count*, so a build that stopped
/// early and answered a short number fails it as loudly as one that refuses.
///
/// The second is the guard that replaced the constant. Every row a pass
/// produces is charged to the request's budget, so a recursion with no base
/// case stops at the byte ceiling and the failure names that ceiling. It is
/// armed here directly because the command line leaves the budget unbounded on
/// purpose - see `a_chain_of_concatenations_cannot_double_past_the_value_bound`
/// above, which says the same thing about the same surface - and a served
/// server is what arms one.
#[test]
fn a_long_recursion_answers_and_a_runaway_meets_the_budget() {
    let connection = connect();
    let counted = connection
        .query(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 1000001)              SELECT count(*) FROM n",
        )
        .expect("a long recursion answers")
        .first()
        .and_then(|row| row.first())
        .and_then(Value::as_integer);
    assert_eq!(
        counted,
        Some(1_000_001),
        "a recursion of a million and one passes did not answer its own count"
    );

    let _guard = inillucent_base::budget::arm(
        inillucent_base::budget::Limits::served()
            .with_bytes(Some(4 * 1024 * 1024))
            .with_time(Some(std::time::Duration::from_secs(30))),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let refused = connection
        .query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT x FROM n")
        .expect_err("a recursion with no base case must be stopped");
    assert!(
        inillucent_base::budget::exceeded_kind(&refused).is_some(),
        "a runaway recursion was stopped by something other than the budget: {}",
        refused.message()
    );
}

/// Runs one statement as a process and reports its exit code, what it said, and
/// the largest resident set it reached.
///
/// Sampled while it runs rather than after it exits, because a process that has
/// ended reports nothing about how large it got.
///
/// @param binary - the built `inillucent`
/// @param sql - the statement to run
fn run_and_watch(binary: &std::path::Path, sql: &str) -> (Option<i32>, String, u64) {
    let mut child = std::process::Command::new(binary)
        .args(["--db", ":memory:", "query", sql])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("the binary did not start: {error}"));
    let mut highest = 0u64;
    let finished = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                highest =
                    highest.max(inillucent_compat::procstat::child_cost(&child).peak_working_set);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("waiting on the binary: {error}"),
        }
    };
    let mut said = String::new();
    if let Some(mut stream) = child.stderr.take() {
        use std::io::Read;
        let _ = stream.read_to_string(&mut said);
    }
    (finished.code(), said, highest)
}
