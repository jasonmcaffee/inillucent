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
//! ## When a guard here flaps, count something instead (task-1884)
//!
//! This file used to say that a flapping guard should be widened, or moved to a
//! gate. It was widened - `one_transaction_beats_many` went from 4 to 2 - and it
//! flapped again anyway on a box that had four other jobs on it, having passed
//! three times out of three on the same commit run alone. Widening only moves
//! the guard closer to 1.0, which is where it stops guarding, so that answer has
//! a floor and reaches it.
//!
//! The old guard's ratio was then measured deliberately, in four conditions, on
//! the same commit and the same machine:
//!
//! | the machine | the old ratio |
//! |---|---|
//! | idle | about 40x |
//! | 20 processes spinning and calling `fsync` | 4.89 - 7.13 |
//! | 12 copies of this suite at once, on top of that | 14.52 - 59.22 |
//! | the same, with the load freshly started | **1.18 - 4.38, and five of the twelve failed** |
//!
//! One quantity, one commit, readings from 1.18 to 59.22. It moves in *both*
//! directions, because which arm a given kind of contention hurts more depends
//! on the kind: processor pressure costs the batched arm more, and a busy disk
//! costs the arm that syncs 2,000 times more. No threshold survives that, which
//! is why the answer is not a threshold.
//!
//! There are two better answers, and they are both used below.
//!
//! - **Count the work rather than timing it.** `one_transaction_beats_many` now
//!   asserts on log writes, 1 against 2,000, which is the same number on an idle
//!   machine and on a loaded one. Where a claim can be made as a count, it is
//!   made as a count.
//! - **Where it must stay a clock, interleave the arms and take a median.** The
//!   three ratios that are about which plan was chosen have no counter to move
//!   to, so they run five paired rounds through [`paired_ratio`] rather than
//!   timing arm A once and then arm B once. See that function for why.
//!
//! This is the `perf` tier, and the runner gives it the machine to itself among
//! the test binaries - but that says nothing about the rest of the box, which is
//! the load that actually broke this file.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use inillucent::{Connection, Database, OwnedDatum};

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

/// Returns the median of `rounds` ratios, each taken from one run of each arm.
///
/// **Why a median of paired rounds rather than one reading of each arm**
/// (task-1884). Timing arm A, then timing arm B, and dividing gives a number
/// that moves with whatever else the machine was doing between the two - and it
/// moves asymmetrically, because a scheduler takes more from an arm that is
/// spending its time on the processor than from one that is waiting on a file.
/// A guard built that way fails on a busy box and teaches everybody to re-run
/// it, which is worse than not having it.
///
/// Interleaving puts the two arms next to each other in time, so a load spike
/// that arrives partway through the test lands on both. Taking the median then
/// discards the round it landed hardest on. A real regression is in every round
/// and the median moves with it.
///
/// @param rounds - how many paired rounds to take; an odd number, so the median
///   is a reading rather than an average of two
/// @param repeats - how many times each arm runs inside one round
/// @param numerator - the arm on top of the ratio
/// @param denominator - the arm underneath it
fn paired_ratio(
    rounds: usize,
    repeats: u32,
    mut numerator: impl FnMut(),
    mut denominator: impl FnMut(),
) -> f64 {
    let mut ratios = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let top = measure(repeats, &mut numerator);
        let bottom = measure(repeats, &mut denominator);
        ratios.push(top.as_secs_f64() / bottom.as_secs_f64().max(f64::EPSILON));
    }
    ratios.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    match ratios.get(rounds / 2) {
        Some(ratio) => *ratio,
        None => panic!("a paired ratio needs at least one round"),
    }
}

/// Inserts `count` rows of one integer column, one statement per row.
///
/// Whether those rows land in one transaction or in `count` of them is the
/// caller's business: it is decided by whether a `BEGIN` is open around this.
///
/// @param connection - the connection to write through
/// @param table - the table to write into
/// @param count - how many rows to write
fn insert_rows(connection: &Connection<'_>, table: &str, count: i64) {
    let mut insert = connection
        .prepare(&format!("INSERT INTO {table} VALUES (?1)"))
        .expect("the insert prepares");
    for id in 0..count {
        insert.reset();
        insert.bind_integer(1, id).expect("the id binds");
        while insert.step().expect("the insert runs") {}
    }
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

    let ratio = paired_ratio(
        5,
        5,
        || {
            let rows = connection
                .query("SELECT id FROM t WHERE +email = 'person9999@example.com'")
                .expect("the scan runs");
            assert_eq!(one(&rows), 9_999);
        },
        || {
            let rows = connection
                .query("SELECT id FROM t WHERE email = 'person9999@example.com'")
                .expect("the seek runs");
            assert_eq!(one(&rows), 9_999);
        },
    );
    assert!(
        ratio >= 5.0,
        "a seek should be far cheaper than a scan of {ROWS} rows, but across five \
         paired rounds the scan was only {ratio:.1}x the seek"
    );
}

/// One transaction around many writes costs far less work than one each.
///
/// **This guard used to be a stopwatch, and task-1884 is why it is not one
/// now.** It timed the batched arm, then timed the autocommit arm, and asserted
/// a ratio between the two readings. That ratio measures the machine as much as
/// the engine, and it measures the two arms unequally: the batched arm spends
/// its time on the processor, which a busy scheduler takes away from it, and
/// the autocommit arm spends its time waiting on file syncs, which it does not.
/// So the threshold had already been walked from 4 down to 2 after the guard
/// flapped at 3.5x, the next step down was 1 - where it would have asserted
/// nothing - and it failed anyway on a box with four other jobs on it, having
/// passed three times out of three on the same commit when run alone.
///
/// It counts the work instead. One transaction hands the log its records once;
/// 2,000 autocommits hand them over 2,000 times. A count reads the same on an
/// idle machine and on a loaded one.
///
/// Measured here at 2,000 rows:
///
/// | arm | log records | log writes | log syncs |
/// |---|---|---|---|
/// | one transaction | 2,063 | 1 | 1 |
/// | 2,000 autocommits | 4,062 | 2,000 | 2,000 |
///
/// **Those six numbers were then read again with the machine at 100%, twelve
/// copies of this suite running at once, and were identical in all twelve** - in
/// the same runs where the old wall-clock guard read between 1.18 and 4.38 and
/// failed five times out of the twelve. See this file's header for the whole
/// table.
///
/// The page pool is deliberately not the instrument. Both arms fetch exactly
/// 8,372 pages and neither writes one, because the pool writes at a checkpoint
/// rather than at a commit - so `cache_stats` cannot tell these two arms apart
/// at all.
///
/// **`writes` and not `syncs`**, though the two moved together here. `syncs` is
/// a function of the `synchronous` policy: under `NORMAL` a commit is
/// acknowledged once its record has been written and the log syncs only every
/// 64 MiB, so asserting on syncs would fail this guard for a policy change that
/// has nothing to do with batching. A commit writes under every policy.
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
    let before = database.log_stats();
    connection
        .execute_batch("BEGIN")
        .expect("one transaction opens");
    insert_rows(&connection, "batched", count);
    connection
        .execute_batch("COMMIT")
        .expect("one transaction commits");
    let between = database.log_stats();
    insert_rows(&connection, "singly", count);
    let after = database.log_stats();

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

    let batched_writes = between.writes.saturating_sub(before.writes);
    let singly_writes = after.writes.saturating_sub(between.writes);
    let batched_syncs = between.syncs.saturating_sub(before.syncs);
    let singly_syncs = after.syncs.saturating_sub(between.syncs);

    // The instrument is live. If autocommit ever stopped committing per
    // statement, both arms would be one transaction, the ratio below would be
    // comparing a thing with itself, and this guard would pass while measuring
    // nothing - which is the failure §1.5 of the testing standard is about.
    assert!(
        singly_writes >= count as u64,
        "committing each of {count} rows on its own issued {singly_writes} log \
         write(s) rather than one each, so both arms are batched and the ratio \
         below would measure nothing"
    );
    assert!(
        batched_writes.saturating_mul(100) <= singly_writes,
        "one transaction around {count} rows issued {batched_writes} log write(s) \
         and {batched_syncs} sync(s), against {singly_writes} write(s) and \
         {singly_syncs} sync(s) for one transaction per row; the commit path has \
         stopped batching"
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
    let before = connection.cached_plan_count();
    let mut statement = connection.prepare(sql).expect("the statement prepares");
    let ratio = paired_ratio(
        5,
        40,
        || {
            let mut fresh = connection.prepare(sql).expect("the statement prepares");
            fresh
                .bind_text(1, "person12345@example.com")
                .expect("the email binds");
            let mut seen = 0;
            while fresh.step().expect("the query steps") {
                seen += 1;
            }
            assert_eq!(seen, 1);
        },
        || {
            statement.reset();
            statement
                .bind_text(1, "person12345@example.com")
                .expect("the email binds");
            let mut seen = 0;
            while statement.step().expect("the query steps") {
                seen += 1;
            }
            assert_eq!(seen, 1);
        },
    );
    drop(statement);
    let after = connection.cached_plan_count();

    assert!(
        ratio <= 4.0,
        "preparing the same statement again should be answered from the plan \
         cache, but across five paired rounds it cost {ratio:.1}x re-binding - \
         the cache is not being consulted"
    );
    assert!(
        after <= before + 1,
        "two hundred prepares of one statement grew the plan cache from {before} \
         to {after}; it is keyed on something that varies per call"
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

/// A keyset page costs the same wherever it starts, rather than growing with
/// what is left behind it.
///
/// **What this is a regression test for (task-1880 §10).** `WHERE key > ?
/// ORDER BY key LIMIT n` is how anything walks a table it cannot hold in
/// memory, and it was doing work proportional to the rows *after* the key
/// rather than to the limit. So the cost **fell** as the key advanced, and a
/// full walk of the table was quadratic in it: 601,862 rows at a page of 2,000
/// is 90 million row materialisations instead of 601,862, and it is what
/// stopped a real adoption pass finishing one table in twenty-five minutes.
///
/// The cause was not the source and not the `LIMIT` operator, both of which
/// stop when they are told to. It was that the ordering an index had been
/// *chosen for* was then not believed: a non-covering seek is two stages, the
/// index and the table fetch behind it, and the rule that decided "the rows
/// already arrive in this order" asked for exactly one stage. So `ORDER BY`
/// fell to a `TopN`, and a `TopN` is a pipeline breaker - it reads every row of
/// the range before it emits one.
///
/// **The shape is what is asserted, not a number.** A page at the start of the
/// table and a page near the end of it do the same amount of work, so their
/// costs are within a small factor of each other. Measured on a 60,000-row
/// fixture before the fix: 88.3 ms at the start against 3.7 ms near the end,
/// which is 24x the wrong way round. After it: 5.2 ms and 5.3 ms. The guard
/// asks for 4x, which no correct plan can fail and the quadratic one cannot
/// pass.
///
/// The projection is deliberately **not** covered by the index. A key-only
/// projection was always flat, because the index answers it without touching
/// the table - which is exactly why a first check for this defect came back
/// clean and it went unnoticed.
#[test]
fn a_keyset_page_costs_the_same_wherever_it_starts() {
    let directory = scratch("keyset");
    let path = directory.join("keyset.rdb");
    let database = Database::open(&path).expect("the database opens");
    let connection = database.connect();
    connection
        .execute_batch("CREATE TABLE chunk (id TEXT PRIMARY KEY, body TEXT NOT NULL);")
        .expect("the schema is created");
    connection
        .execute_batch("BEGIN")
        .expect("the transaction opens");
    let body = "x".repeat(200);
    let mut insert = connection
        .prepare("INSERT INTO chunk (id, body) VALUES (?1, ?2)")
        .expect("the insert prepares");
    for nth in 0..ROWS {
        insert.reset();
        insert
            .bind_text(1, &format!("chunk-{nth:09}"))
            .expect("the id binds");
        insert.bind_text(2, &body).expect("the body binds");
        while insert.step().expect("the insert runs") {}
    }
    drop(insert);
    connection
        .execute_batch("COMMIT")
        .expect("the transaction commits");

    let page = 500usize;
    let read = |after: i64| {
        let sql = format!(
            "SELECT id, body FROM chunk WHERE id > 'chunk-{after:09}' ORDER BY id LIMIT {page}"
        );
        let rows = connection.query(&sql).expect("the page reads");
        assert_eq!(rows.len(), page, "the page is full at {after}");
    };

    let ratio = paired_ratio(5, 1, || read(0), || read(ROWS - page as i64 - 1));
    assert!(
        ratio <= 4.0,
        "across five paired rounds a page at the start of the table cost \
         {ratio:.1}x one at the end; the work is proportional to the rows after \
         the key rather than to the limit"
    );
}
