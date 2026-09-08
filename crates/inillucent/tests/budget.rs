//! The cost guards: the regressions that a correctness suite cannot see.
//!
//! Invariant: **every guard here is a ratio between two things measured in the
//! same run, or a ceiling so loose that only a change of complexity can cross
//! it.** No guard is a stopwatch reading compared against a number somebody
//! wrote down on a different machine.
//!
//! ## Why ratios, and why that is not a dodge
//!
//! A test that asserts "this takes under 40 ms" is a test that fails on a busy
//! machine and passes on a fast one, and the usual response - raise the number
//! until it stops failing - ends with a guard that no longer guards anything.
//! This repository already has the instrument for absolute numbers: the gates
//! in `inillucent-compat` run thirty rounds against a pinned SQLite build and
//! report a geometric mean with a lower bound. That is where a speedup claim
//! belongs, and it is deliberately not a `cargo test`.
//!
//! What a test *can* hold, on any machine and under any load, is the shape of
//! the cost curve:
//!
//! - an index makes a point lookup cheaper than a scan, and the gap widens with
//!   the table - so a planner that stopped using an index fails here even
//!   though every answer it gives is still right;
//! - one transaction around ten thousand inserts costs far less than ten
//!   thousand transactions, because the second pays a commit each time - so a
//!   commit path that started flushing per statement fails here;
//! - preparing once and re-binding costs less than preparing each time, which
//!   is what the plan cache is for;
//! - the file is proportional to the data, not to the number of times it was
//!   written - so a free list that stopped being reused fails here.
//!
//! Each of those is a property of the design rather than of the hardware, and
//! each one is a real regression that leaves every test green.
//!
//! ## The margins are deliberately wide
//!
//! Every ratio below is asserted at a fraction of what it actually measures on
//! this machine, and the comment on each says what it measured. That is the
//! trade: the guard catches a change of *kind* - an index dropped, a commit per
//! row, a cache turned off - and deliberately does not notice a change of
//! twenty per cent. Something that fails one of these is broken rather than
//! slow, which is why it is allowed to be a test at all.
//!
//! This is the `perf` tier, and the runner will schedule it alongside
//! everything else. That is safe *because* the margins are wide; if a guard
//! here ever starts flapping under load, the answer is to widen it or move the
//! measurement to a gate, never to tighten it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use inillucent::{Database, OwnedDatum};

/// How many rows the guards build their table from.
///
/// Large enough that a scan and a seek are different orders of work, small
/// enough that the whole file stays in the page pool and the guard is measuring
/// the engine rather than the disk.
const ROWS: i64 = 20_000;

/// Returns a fresh, empty directory for one test's files.
///
/// @param tag - what to name the directory after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-budget-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// Builds a table of [`ROWS`] rows with an index on one column.
///
/// @param path - where the database goes
fn build(path: &PathBuf) -> Database {
    let database = Database::open(path).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT NOT NULL, bucket INTEGER NOT NULL);\
             CREATE UNIQUE INDEX t_email ON t (email);",
        )
        .expect("the schema is created");
    connection
        .execute_batch("BEGIN")
        .expect("the transaction opens");
    let mut insert = connection
        .prepare("INSERT INTO t VALUES (?1, ?2, ?3)")
        .expect("the insert prepares");
    for id in 0..ROWS {
        insert.reset();
        insert.bind_integer(1, id).expect("the id binds");
        insert
            .bind_text(2, &format!("person{id}@example.com"))
            .expect("the email binds");
        insert.bind_integer(3, id % 97).expect("the bucket binds");
        while insert.step().expect("the insert runs") {}
    }
    drop(insert);
    connection
        .execute_batch("COMMIT")
        .expect("the transaction commits");
    drop(connection);
    database
}

/// Times a closure, running it once first so a cold plan is not the measurement.
///
/// @param rounds - how many times to run it
/// @param work - what to time
fn measure(rounds: u32, mut work: impl FnMut()) -> Duration {
    work();
    let started = Instant::now();
    for _ in 0..rounds {
        work();
    }
    started.elapsed()
}

/// Returns the one integer a single-row, single-column answer holds.
///
/// @param rows - what the query returned
fn one(rows: &[Vec<OwnedDatum>]) -> i64 {
    match rows {
        [row] => match row.as_slice() {
            [OwnedDatum::Int(value)] => *value,
            other => panic!("expected one integer, got {other:?}"),
        },
        other => panic!("expected one row, got {}", other.len()),
    }
}

/// An index makes a point lookup cheaper than a scan of the same table.
///
/// Measured on a 24-core Windows machine at 20,000 rows: the scan costs about
/// **200 times** the seek. The guard asks for 5, which no correct planner using
/// the index can fail and no planner that stopped using it can pass.
///
/// The `+email` in the scan arm is what stops the planner using the index for
/// it: it is the same question, asked in a way the index cannot answer.
#[test]
fn an_index_beats_a_scan() {
    let directory = scratch("index");
    let database = build(&directory.join("b.rdb"));
    let connection = database.connect();

    let plan = connection
        .explain("SELECT id FROM t WHERE email = 'person9999@example.com'")
        .expect("the plan is explained")
        .join("\n");
    assert!(
        plan.contains("t_email"),
        "the index is not being used at all, so the ratio below would measure nothing:\n{plan}"
    );

    let seek = measure(20, || {
        let rows = connection
            .query("SELECT id FROM t WHERE email = 'person9999@example.com'")
            .expect("the seek runs");
        assert_eq!(one(&rows), 9_999);
    });
    let scan = measure(20, || {
        let rows = connection
            .query("SELECT id FROM t WHERE +email = 'person9999@example.com'")
            .expect("the scan runs");
        assert_eq!(one(&rows), 9_999);
    });
    let ratio = scan.as_secs_f64() / seek.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio >= 5.0,
        "a seek should be far cheaper than a scan of {ROWS} rows, but the scan \
         was only {ratio:.1}x the seek (seek {seek:?}, scan {scan:?})"
    );
}

/// One transaction around many writes costs far less than one transaction each.
///
/// Measured on an idle machine: about **40 times** less at 2,000 rows. What it
/// is protecting is the commit path - an engine that stopped batching, or
/// started an fsync per statement, would come out near 1.0x without any answer
/// changing.
///
/// **The threshold is 2, and it was 4 until this guard flapped.** Running under
/// the parallel runner with 24 binaries in flight it measured 3.5x, because
/// contention costs the batched arm proportionally more than the arm that is
/// already dominated by commits. That is exactly the failure this file's header
/// says to expect, and the rule it sets is to widen rather than tighten: the
/// number that matters is the distance from 1.0, not the distance from 40.
#[test]
fn one_transaction_beats_many() {
    let directory = scratch("commit");
    let database = Database::open(directory.join("b.rdb")).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch(
            "CREATE TABLE batched (id INTEGER PRIMARY KEY);\
             CREATE TABLE singly (id INTEGER PRIMARY KEY);",
        )
        .expect("the schema is created");

    let count = 2_000i64;
    let started = Instant::now();
    connection
        .execute_batch("BEGIN")
        .expect("one transaction opens");
    let mut insert = connection
        .prepare("INSERT INTO batched VALUES (?1)")
        .expect("the insert prepares");
    for id in 0..count {
        insert.reset();
        insert.bind_integer(1, id).expect("the id binds");
        while insert.step().expect("the insert runs") {}
    }
    drop(insert);
    connection
        .execute_batch("COMMIT")
        .expect("one transaction commits");
    let batched = started.elapsed();

    let started = Instant::now();
    let mut insert = connection
        .prepare("INSERT INTO singly VALUES (?1)")
        .expect("the insert prepares");
    for id in 0..count {
        insert.reset();
        insert.bind_integer(1, id).expect("the id binds");
        while insert.step().expect("the insert runs") {}
    }
    drop(insert);
    let singly = started.elapsed();

    assert_eq!(
        one(&connection
            .query("SELECT count(*) FROM batched")
            .expect("counted")),
        count
    );
    assert_eq!(
        one(&connection
            .query("SELECT count(*) FROM singly")
            .expect("counted")),
        count
    );
    let ratio = singly.as_secs_f64() / batched.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio >= 2.0,
        "committing each row should cost far more than committing once, but it \
         was only {ratio:.1}x (batched {batched:?}, singly {singly:?})"
    );
}

/// Preparing the same statement again is nearly free, and does not grow the
/// plan cache.
///
/// This guard was originally written the other way round - "re-binding must
/// beat re-preparing" - on the assumption that preparing recompiles. It does
/// not: measured here, re-preparing 200 times costs **1.08x** re-binding,
/// because the plan cache answers the second prepare from the first. The
/// assumption was wrong and the measurement is the better guard, so the test
/// asserts what is actually true and what would actually break.
///
/// Two things break it. A plan cache that stopped caching sends the ratio up,
/// because every prepare recompiles. A cache that stopped being *bounded* -
/// keyed on something that varies per call, say - leaves the ratio alone and
/// grows the cache without limit, which is a leak no timing notices. So both
/// are asserted.
#[test]
fn re_preparing_is_cached_rather_than_recompiled() {
    let directory = scratch("prepare");
    let database = build(&directory.join("b.rdb"));
    let connection = database.connect();

    let sql = "SELECT id FROM t WHERE email = ?1";
    let mut statement = connection.prepare(sql).expect("the statement prepares");
    let rebound = measure(200, || {
        statement.reset();
        statement
            .bind_text(1, "person12345@example.com")
            .expect("the email binds");
        let mut seen = 0;
        while statement.step().expect("the query steps") {
            seen += 1;
        }
        assert_eq!(seen, 1);
    });
    drop(statement);

    let before = connection.cached_plan_count();
    let reprepared = measure(200, || {
        let mut fresh = connection.prepare(sql).expect("the statement prepares");
        fresh
            .bind_text(1, "person12345@example.com")
            .expect("the email binds");
        let mut seen = 0;
        while fresh.step().expect("the query steps") {
            seen += 1;
        }
        assert_eq!(seen, 1);
    });
    let after = connection.cached_plan_count();

    let ratio = reprepared.as_secs_f64() / rebound.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio <= 4.0,
        "preparing the same statement again should be answered from the plan \
         cache, but it cost {ratio:.1}x re-binding (rebound {rebound:?}, \
         reprepared {reprepared:?}) - the cache is not being consulted"
    );
    assert!(
        after <= before + 1,
        "201 prepares of one statement grew the plan cache from {before} to \
         {after}; it is keyed on something that varies per call"
    );
}

/// The file is proportional to the data rather than to how often it was written.
///
/// Rewriting the same rows over and over must reuse the space it frees. A free
/// list that stopped being consulted grows the file without limit, and every
/// query still answers correctly - so nothing but a size guard notices.
#[test]
fn rewriting_does_not_grow_the_file_without_bound() {
    let directory = scratch("size");
    let path = directory.join("b.rdb");
    let after_first;
    {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, payload TEXT NOT NULL)")
            .expect("the table is created");
        let payload = "y".repeat(300);
        connection
            .execute_batch("BEGIN")
            .expect("the transaction opens");
        let mut insert = connection
            .prepare("INSERT INTO t VALUES (?1, ?2)")
            .expect("the insert prepares");
        for id in 0..5_000i64 {
            insert.reset();
            insert.bind_integer(1, id).expect("the id binds");
            insert.bind_text(2, &payload).expect("the payload binds");
            while insert.step().expect("the insert runs") {}
        }
        drop(insert);
        connection
            .execute_batch("COMMIT")
            .expect("the transaction commits");
        database.checkpoint().expect("the log is folded in");
        after_first = file_bytes(&path);

        // Now rewrite every row ten times over. The data is the same size at
        // the end as it was at the start.
        for round in 0..10 {
            connection.execute_batch("BEGIN").expect("a round opens");
            connection
                .execute(&format!(
                    "UPDATE t SET payload = '{}'",
                    "z".repeat(300 + round)
                ))
                .expect("every row is rewritten");
            connection.execute_batch("COMMIT").expect("a round commits");
            database.checkpoint().expect("the log is folded in");
        }
    }

    let after_rewrites = file_bytes(&path);
    let growth = after_rewrites as f64 / after_first as f64;
    assert!(
        growth <= 3.0,
        "ten rewrites of the same 5,000 rows grew the file {growth:.1}x \
         ({after_first} bytes to {after_rewrites}); space is not being reused"
    );
    let database = Database::open(&path).expect("the database reopens");
    database.check().expect("the file is sound");
}

/// Returns the size of a database file and its log together.
///
/// The log counts: a file that stayed small only because everything was left in
/// the log has not reused anything.
///
/// @param path - the database file
fn file_bytes(path: &PathBuf) -> u64 {
    let mut total = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let Some(directory) = path.parent() else {
        return total;
    };
    let Some(stem) = path.file_name().and_then(|name| name.to_str()) else {
        return total;
    };
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name != stem && name.starts_with(stem) {
                total += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// Reading every row of a large table finishes in a time only a change of
/// complexity could cross.
///
/// This is the one absolute ceiling in the file, and it is set at roughly a
/// hundred times what it measures - twenty thousand rows scanned in well under
/// a tenth of a second here. It exists to catch an accidental quadratic, which
/// no ratio in this file would notice because both of its arms would slow down
/// together.
#[test]
fn a_full_scan_is_linear_enough_to_finish() {
    let directory = scratch("scan");
    let database = build(&directory.join("b.rdb"));
    let connection = database.connect();
    let started = Instant::now();
    let rows = connection
        .query("SELECT count(*), sum(bucket) FROM t")
        .expect("the scan runs");
    let elapsed = started.elapsed();
    match rows.first().map(Vec::as_slice) {
        Some([OwnedDatum::Int(count), OwnedDatum::Int(_)]) => assert_eq!(*count, ROWS),
        other => panic!("expected a count and a sum, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(10),
        "scanning {ROWS} rows took {elapsed:?}, which is not a linear scan"
    );
}
