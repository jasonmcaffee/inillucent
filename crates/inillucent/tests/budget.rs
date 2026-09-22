//! The cost guards: the regressions that a correctness suite cannot see.
//!
//! Invariant: **every guard here is a ratio between two counts taken in the
//! same run, or a ceiling so loose that only a change of complexity can cross
//! it.** Nothing here reads a clock and compares it against a number somebody
//! wrote down, and after task-1886 nothing here reads a clock at all except the
//! one ceiling that says so in its own name.
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
//! - preparing the same statement two hundred times compiles it once, because
//!   the plan cache answers the other hundred and ninety-nine - so a cache that
//!   stopped being consulted fails here;
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
//! ## Nothing here is a stopwatch, and that took three attempts
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
//! **The second attempt kept the clock and made it fairer.** The three ratios
//! about which plan was chosen ran five interleaved rounds and took the median,
//! so a load spike landed on both arms and the worst round was discarded. That
//! is a better clock and it is still a clock: the next contended run moved the
//! failure to `re_preparing_is_cached_rather_than_recompiled`, which read 6.6x
//! against a bound of 4 and announced that the plan cache was not being
//! consulted. It was. The reprepare arm had been descheduled.
//!
//! **So the third attempt has no clock in it.** Every ratio below is a ratio of
//! counts:
//!
//! | guard | what it counts | idle | broken |
//! |---|---|---|---|
//! | `an_index_beats_a_scan` | pages fetched | 33 against 2 | 33 against 33 |
//! | `one_transaction_beats_many` | log writes | 1 against 2,000 | 1 against 1 |
//! | `re_preparing_is_cached_rather_than_recompiled` | statements compiled | 1 for 200 prepares | 200 |
//! | `a_keyset_page_costs_the_same_wherever_it_starts` | pages fetched | 502 against 502 | ~20,000 against 502 |
//!
//! A count reads the same on an idle machine and on a loaded one, which a
//! processor-time reading does not quite: the arm that touches more memory
//! loses more cycles under a 24-wide test pool, and the two arms of these guards
//! touch very different amounts of memory. `tasks/task-1886-a-guard-the-box-cannot-decide-tdd.md`
//! has the whole argument, including why the processor-time primitive that
//! `inillucent-compat` already owns was not moved down into `inillucent-base` to
//! serve this file.
//!
//! One clock is left, in `a_full_scan_is_linear_enough_to_finish`. Its own doc
//! comment says what it is for - a quadratic that does no extra I/O, which no
//! count in this file can see - and why 3 ms against a ten-second bound is not a
//! number this box decides.
//!
//! ## An assertion says what it measured, and stops
//!
//! No message here names a cause. `- the cache is not being consulted` was
//! printed by a guard whose cache was fine, and whoever read it would have gone
//! looking for a defect that did not exist. A guard reports the two numbers and
//! what it expected of them; working out why is the reader's job and the reader
//! has the whole repository to do it with.
//!
//! This is the `perf` tier, and the runner gives it the machine to itself among
//! the test binaries and runs it with one test thread, so its own guards do not
//! contend with each other either.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use inillucent_engine::connect::{Connection, Database, Statement};
use inillucent_sql::plan::Levers;
use inillucent_tree::datum::OwnedDatum;

thread_local! {
    /// How many allocations the calling thread has made.
    ///
    /// **Per thread rather than per process, so another test cannot move it.**
    /// A `#[global_allocator]` sees every allocation in the binary, and this
    /// file's guards run one at a time only because the runner asks for one
    /// test thread. A counter that depended on that would read whatever a
    /// parallel `cargo test --test budget` happened to be doing, which is the
    /// class of number this file exists to refuse.
    ///
    /// `const`-initialised and holding a type with no destructor, so reading it
    /// from inside the allocator cannot itself allocate.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

/// An allocator that counts per thread, and otherwise delegates to the system one.
struct CountingAllocator;

// SAFETY: every method forwards to the system allocator with the same
// arguments; the counter is the only addition and it touches no memory the
// allocator owns.
unsafe impl GlobalAlloc for CountingAllocator {
    // SAFETY: the layout is the caller's, forwarded unchanged.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_one();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    // SAFETY: the pointer and layout are the ones this allocator handed out.
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.dealloc(pointer, layout) }
    }

    // SAFETY: the pointer and layout are the ones this allocator handed out.
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_one();
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

/// Adds one to the calling thread's count, if the thread still has one.
///
/// `try_with` rather than `with`, because a thread being torn down has already
/// dropped its locals and an allocation made after that must not panic inside
/// the allocator.
fn count_one() {
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get().saturating_add(1)));
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Returns how many allocations the calling thread made while running a closure.
///
/// @param body - the work to count
fn allocations(body: impl FnOnce()) -> u64 {
    let before = ALLOCATIONS.with(Cell::get);
    body();
    ALLOCATIONS.with(Cell::get).saturating_sub(before)
}

/// How many times a statement run outside a transaction may read both meta
/// slots in full.
///
/// **Zero, and the margin is deliberately absent.** A statement outside a
/// transaction takes the file lock, asks whether another process has folded
/// since this connection last held it, and gives the lock back. Asking that
/// question by reading both slots in full allocates and zeroes a buffer one
/// page long for each slot, reads a whole page into each, and checksums both,
/// and nothing
/// about a file that has not changed needs any of it - the record's own 116
/// bytes, compared against the ones this connection last read and verified,
/// answer it. One is not a rounding of zero here: one means a caller went back
/// to the full read, and the only reason to is that the file moved.
const META_READS_PER_UNCHANGED_STATEMENT: u64 = 0;

/// How many times a statement run outside a transaction may read the record's
/// own bytes.
///
/// One per statement, because one lock acquisition is one chance for another
/// process to have written. Two means the two callers that ask - the pool's
/// `begin_read` and the engine's `the_meta_moved` - have stopped sharing the
/// answer they both make under the same SHARED lock, which no writer can hold
/// at the same time.
const META_PROBES_PER_UNCHANGED_STATEMENT: u64 = 1;

/// How many statements each arm of
/// [`a_statement_outside_a_transaction_rereads_nothing`] runs.
///
/// Enough that one stray read would be visible against a bound of zero, and
/// small enough that the guard costs nothing.
const RUNS: u64 = 200;

/// How many rows the guards build their table from.
///
/// Large enough that a scan and a seek are different orders of work, small
/// enough that the whole file stays in the page pool and the guard is measuring
/// the engine rather than the disk.
const ROWS: i64 = 20_000;

/// How many allocations compiling `SELECT 1` may make on a warm connection.
///
/// **Exactly what this reads, with no margin, and that is deliberate.** Three
/// runs of the guard taken while four agents were building this repository
/// returned 15 every time - an allocation count is a property of the code path
/// rather than of the machine, so there is no run-to-run movement for a margin
/// to absorb. A margin would only buy room for a regression to hide in: each of
/// the four changes task-2026 made is worth between one and three allocations
/// here, so a bound of 18 would let two of them be undone silently.
///
/// The standard library decides how a vector grows, and `rust-toolchain.toml`
/// pins the toolchain, so an upgrade is the one thing that can move this
/// number without the engine changing. It is a deliberate edit when it
/// happens, and the assertion prints what it read.
/// **Sixteen rather than fifteen since task-2066 §4.3.5.** A compiled `SELECT`
/// carries its result column names now, decoded once, behind an `Rc` - and
/// that `Rc` is the one allocation. What it buys is the `Vec` and the `String`
/// per column that every *execution* used to build: `SELECT 1` stepped through
/// a connection made 11 allocations a step before and makes 8 now, which
/// [`STEP_ALLOCATIONS`] records. A statement stepped twice is already ahead.
const TRIVIAL_COMPILE_ALLOCATIONS: u64 = 16;

/// How many allocations one step of `SELECT 1` may make through a connection.
///
/// **Eight, and it was eleven until task-2066 section 4.3.5.** A cached
/// `SELECT` rebuilt its result column names on every execution - a `Vec` and a
/// `String` per column - and they are `plan.select.columns`'s own names, which
/// cannot change between executions of one compiled statement. They are
/// decoded once at compile now and handed out behind an `Rc`.
///
/// Exactly what this reads, with no margin, for the reason
/// [`TRIVIAL_COMPILE_ALLOCATIONS`] gives: an allocation count is a property of
/// the code path rather than of the machine, so there is no run-to-run movement
/// for a margin to absorb, and a margin is room for a regression to hide in.
const STEP_ALLOCATIONS: u64 = 8;

/// How many allocations compiling `SELECT id FROM t WHERE email = ?1` may make
/// on a warm connection.
///
/// Higher than [`TRIVIAL_COMPILE_ALLOCATIONS`] because this statement names
/// things and `SELECT 1` does not.
///
/// **120 when this guard was written, 92 since task-2039.** Interning a name
/// cost four allocations - the written spelling copied out of the source, the
/// folded key, and the map's owned key cloned twice - and it cost them on a
/// name the arena already held, because the lookup had to build the key it
/// looked up with. `Ast::interned` is keyed on a hash of the spelling and
/// quote form now, so a hit allocates nothing, and `Ast::clear` keeps the
/// names' byte buffers instead of dropping them, so a warm compile does not
/// buy them again.
///
/// **Twenty-eight rather than the fourteen the statement's own three names
/// account for**, because compiling it also reparses the `CREATE TABLE` text
/// the catalog stores, and that declaration names every column of the table.
/// It is the same saving twice over, and it is why this guard is worth more
/// than the profile of one statement: `inillucent-prepareprofile` reads 93 to
/// 79 on the gate's path, and the path an application takes saves twice that.
///
/// Read exactly, with no margin, for the reason
/// [`TRIVIAL_COMPILE_ALLOCATIONS`] gives.
/// **Ninety-three rather than ninety-two since task-2066 section 4.3.5**, for
/// the same one `Rc` [`TRIVIAL_COMPILE_ALLOCATIONS`] gained and the same reason.
const POINT_COMPILE_ALLOCATIONS: u64 = 93;

/// How many more allocations a compile makes when the in-process embedder is
/// compiled in.
///
/// **The two bounds above are not one number, they are two, and which one
/// applies is decided by a cargo feature (task-2039).** `inillucent-engine/embed`
/// registers the retrieval engine's embedder so `embed(TEXT)` answers, and a
/// compile then costs three allocations more - the same three whatever the
/// statement is, so `SELECT 1` reads 15 without it and 18 with it, the point
/// statement reads 92 and 95, and even the cold first compile reads 674 and
/// 677.
///
/// It is not academic and it is why this is here rather than in a comment.
/// `inillucent-testrun` builds the workspace twice: once with default features,
/// and once more with every feature the selected suites ask for, which for any
/// ordinary selection includes `inillucent-engine/embed`. The binary it then
/// runs this guard from is the second one. So the numbers a developer sees from
/// `cargo test -p inillucent --test budget` and the numbers the runner sees are
/// three apart, and a bound written from either one alone is wrong in the other
/// - which is how this guard came to fail in a 158-target run while passing
/// every way it was checked by hand.
///
/// Widening the bounds by three to cover both was the other option and is worse:
/// it would leave three allocations of room for a regression to hide in on the
/// build most people run, which is the argument
/// [`TRIVIAL_COMPILE_ALLOCATIONS`] already makes against a margin. So the guard
/// asks the engine which build it is in and stays exact in both.
const EMBEDDER_COMPILE_ALLOCATIONS: u64 = 3;

/// Reports whether this build has the in-process embedder compiled in.
///
/// Asked of the running engine rather than read from a `cfg`, because the
/// feature belongs to `inillucent-engine` and this crate's tests are compiled
/// without it being named on *this* package - `--features inillucent-engine/embed`
/// turns the engine's embedder on and sets no `cfg` here at all, so
/// `#[cfg(feature = "embed")]` would read false in exactly the build that needs
/// it to read true.
///
/// `embed(TEXT)` is the function the feature registers, so a statement that
/// calls it binds in one build and fails to bind in the other. It is asked on
/// its own connection so that the arena this warms is not the one the guard is
/// about to measure a cold compile on.
///
/// @param database - the database the guard is running against
fn embedder_is_compiled_in(database: &Database) -> bool {
    let connection = database.session();
    connection.prepare("SELECT embed('probe')").is_ok()
}

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
    let connection = database.session();
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
    let _ = connection;
    database
}

/// Returns how many pages the pool has been asked for since the file opened.
///
/// **Fetches rather than reads.** `reads` counts the ones that missed the pool
/// and went to the file, so a guard built on it would be asserting something
/// about the pool's size: every arm below reads 0, because these fixtures fit
/// in the pool. `hits + misses` is every page the engine asked for, which is
/// the work the engine did rather than the work the disk did, and it is the
/// same number on an idle machine and on a loaded one.
///
/// @param database - the database to ask
fn fetches(database: &Database) -> u64 {
    let stats = database.cache_stats();
    stats.hits.saturating_add(stats.misses)
}

/// Runs a statement from the beginning and reads its row.
///
/// @param statement - the prepared statement
fn step_once(statement: &mut Statement<'_>) {
    statement.reset();
    assert!(statement.step().expect("the statement steps"));
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
/// **What is counted, and why it is not the clock any more.** Both arms ask the
/// same question of the same 20,000 rows, and the guard is the number of pages
/// the pool was asked for while each one answered. Measured here, with both
/// arms run once first so neither pays for a plan the other already has:
/// **33 fetches for the scan and 2 for the seek**. The scan's count grows with
/// the table and the seek's does not - it is an index descent and one row - so
/// the margin widens as `ROWS` does rather than narrowing.
///
/// A planner that stopped using the index makes the seek arm a scan, both arms
/// read 33, and the ratio falls to 1. That was measured, by pointing both arms
/// at the scan. The guard asks for 4, which is bounded away from a broken
/// reading rather than away from a slow one - and that is the whole difference
/// between a count and a stopwatch, whose 200x margin was that wide because its
/// noise was.
///
/// The `+email` in the scan arm is what stops the planner using the index for
/// it: it is the same question, asked in a way the index cannot answer. The
/// `explain` assertion below is what keeps the instrument live - without it, a
/// planner that used the index for neither arm would read 33 against 33, fail,
/// and be right to.
#[test]
fn an_index_beats_a_scan() {
    let directory = scratch("index");
    let database = build(&directory.join("b.rdb"));
    let connection = database.session();

    let plan = connection
        .explain("SELECT id FROM t WHERE email = 'person9999@example.com'")
        .expect("the plan is explained")
        .join(" | ");
    assert!(
        plan.contains("t_email"),
        "the index is not being used at all, so the counts below would measure \
         nothing: {plan}"
    );

    let scan_sql = "SELECT id FROM t WHERE +email = 'person9999@example.com'";
    let seek_sql = "SELECT id FROM t WHERE email = 'person9999@example.com'";
    let run = |sql: &str| {
        let rows = connection.query(sql).expect("the query runs");
        assert_eq!(one(&rows), 9_999);
    };
    // Once each first, so neither arm pays for a plan the other already has.
    run(scan_sql);
    run(seek_sql);

    let before = fetches(&database);
    run(scan_sql);
    let between = fetches(&database);
    run(seek_sql);
    let after = fetches(&database);
    let scan = between.saturating_sub(before);
    let seek = after.saturating_sub(between);

    assert!(
        seek > 0,
        "the seek fetched no pages at all, so the comparison below would be \
         against nothing"
    );
    assert!(
        scan >= seek.saturating_mul(4),
        "a scan and a seek over {ROWS} rows fetched {scan} and {seek} page(s); \
         the guard asks the scan for at least four times the seek"
    );
}

/// One transaction around many writes costs far less work than one each.
///
/// **This guard used to be a stopwatch, and it is not one now because that
/// stopwatch flapped under load.** It timed the batched arm, then timed the
/// autocommit arm, and asserted a ratio between the two readings. That ratio
/// measures the machine as much as the engine, and it measures the two arms
/// unequally: the batched arm spends its time on the processor, which a busy
/// scheduler takes away from it, and
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
    let connection = database.session();
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

/// Preparing the same statement again is answered from the plan cache, and does
/// not grow it.
///
/// **This guard is the reason the whole file stopped using a clock.** It was a
/// ratio of two wall-clock readings, and on a box with two other jobs on it it
/// read 6.6x against a bound of 4 and failed - saying, in the failure message,
/// that the plan cache was not being consulted. The plan cache was fine. The
/// arm that reprepares had been descheduled, and the same test passed six times
/// out of six when run on its own.
///
/// It counts compilations instead. `Connection::compiled_statement_count`
/// increments inside the one function that turns SQL text into a plan, so a
/// prepare answered from the cache does not move it and a prepare that
/// recompiled does. Two hundred prepares of one statement move it by **1**, on
/// any machine and under any load.
///
/// **The instrument is proved live in the same run.** A counter that had stopped
/// counting would read 0 and pass, which is the failure §1.5 of the testing
/// standard is about. So a second database runs the identical loop with
/// `Levers::PLAN_CACHE` switched off - the lever exists precisely so a
/// measurement can price the compile - and that arm must read 200. One run
/// therefore shows the cache saving 199 compilations and shows the counter able
/// to report them.
///
/// A second defect leaves the compile count alone: a cache that is consulted but
/// no longer *bounded*, keyed on something that varies per call, grows without
/// limit while every prepare still hits. That is a leak no count of compilations
/// notices, so the cache's size is asserted as well.
#[test]
fn re_preparing_is_cached_rather_than_recompiled() {
    let directory = scratch("prepare");
    let database = build(&directory.join("b.rdb"));
    let connection = database.session();

    let sql = "SELECT id FROM t WHERE email = ?1";
    let rounds = 200;
    let before = connection.cached_plan_count().expect("the engine is free");
    let compiles_before = connection
        .compiled_statement_count()
        .expect("the engine is free");
    for _ in 0..rounds {
        let mut fresh = connection.prepare(sql).expect("the statement prepares");
        fresh
            .bind_text(1, "person12345@example.com")
            .expect("the email binds");
        let mut seen = 0;
        while fresh.step().expect("the query steps") {
            seen += 1;
        }
        assert_eq!(seen, 1);
    }
    let cached_compiles = connection
        .compiled_statement_count()
        .expect("the engine is free")
        .saturating_sub(compiles_before);
    let after = connection.cached_plan_count().expect("the engine is free");

    // The same loop with the cache switched off, so this run contains a reading
    // of what "not cached" costs rather than a claim about it.
    let uncached_directory = scratch("prepare-uncached");
    let uncached_database = build(&uncached_directory.join("b.rdb"));
    let uncached = uncached_database.session();
    uncached
        .disable_optimizations(Levers::without(Levers::PLAN_CACHE))
        .expect("the engine is free");
    let uncached_before = uncached
        .compiled_statement_count()
        .expect("the engine is free");
    for _ in 0..rounds {
        let mut fresh = uncached.prepare(sql).expect("the statement prepares");
        fresh
            .bind_text(1, "person12345@example.com")
            .expect("the email binds");
        while fresh.step().expect("the query steps") {}
    }
    let uncached_compiles = uncached
        .compiled_statement_count()
        .expect("the engine is free")
        .saturating_sub(uncached_before);

    assert_eq!(
        uncached_compiles, rounds as u64,
        "with the plan cache switched off, {rounds} prepares of one statement \
         compiled it {uncached_compiles} time(s) rather than {rounds}; the \
         counter the assertion below reads is not counting compilations"
    );
    assert!(
        cached_compiles <= 1,
        "{rounds} prepares of one statement compiled it {cached_compiles} \
         time(s), against {uncached_compiles} with the plan cache switched off; \
         the guard asks for at most one"
    );
    assert!(
        after <= before + 1,
        "{rounds} prepares of one statement grew the plan cache from {before} \
         to {after}; the guard asks for at most one new entry"
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
        let connection = database.session();
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

/// Reading every row of a large table stays proportional to the table.
///
/// **The one remaining clock in this file, and it is a ceiling rather than a
/// ratio.** It exists to catch an accidental quadratic, which no ratio here
/// would notice because both arms of a ratio would slow down together.
///
/// An accidental quadratic has two shapes and they need two instruments:
///
/// - one re-reaches for rows it already has - re-descending the tree per row,
///   say - and that shows up as **pages fetched**. Measured here, a scan of
///   20,000 rows fetches **20** pages; the guard asks for at most 200, which
///   is the whole table ten times over.
/// - the other is quadratic *inside* the rows already fetched, does no extra
///   I/O at all, and shows up only as time. Measured here at **2.99 ms**; the
///   guard asks for under ten seconds.
///
/// **Why a clock is acceptable here when it is nowhere else in this file.** The
/// headroom is 3,300x. The worst load factor this repository has measured is
/// about 50x - `inillucent::budget` taking 43 to 55 seconds under a 24-wide
/// `--strict` run against 1 to 3 seconds alone - so the bound has sixty times
/// more room than the worst load anybody has seen here. The ratios that were
/// removed from this file had margins of 4x against noise that reached 6.6x,
/// which is the difference.
#[test]
fn a_full_scan_is_linear_enough_to_finish() {
    let directory = scratch("scan");
    let database = build(&directory.join("b.rdb"));
    let connection = database.session();
    let before = fetches(&database);
    let started = Instant::now();
    let rows = connection
        .query("SELECT count(*), sum(bucket) FROM t")
        .expect("the scan runs");
    let elapsed = started.elapsed();
    let fetched = fetches(&database).saturating_sub(before);
    match rows.first().map(Vec::as_slice) {
        Some([OwnedDatum::Int(count), OwnedDatum::Int(_)]) => assert_eq!(*count, ROWS),
        other => panic!("expected a count and a sum, got {other:?}"),
    }
    assert!(
        fetched <= 200,
        "scanning {ROWS} rows fetched {fetched} page(s); the guard asks for at \
         most 200, which is the whole table ten times over"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "scanning {ROWS} rows took {elapsed:?} and fetched {fetched} page(s); \
         the guard asks for under ten seconds"
    );
}

/// A keyset page costs the same wherever it starts, rather than growing with
/// what is left behind it.
///
/// **What this is a regression test for.** `WHERE key > ?
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
/// **The shape is what is asserted, and it is asserted as a count of pages
/// rather than as a stopwatch.** A page at the start of the table and a page
/// near the end of it do the same amount of work, so they ask the pool for the
/// same number of pages: measured here, **502 fetches at each end**. The guard
/// asks the start arm for at most four times the end arm.
///
/// **The instrument is proved live in the same run.** The pipeline breaker read
/// every row of the range, so a third arm reads exactly that - the same query
/// with the limit taken off - and it costs **20,014 fetches against the page's
/// 502**, which is 39.9x. That number is measured beside the assertion rather
/// than quoted from the bug report, so a fixture that had quietly shrunk, or a
/// counter that had stopped counting, fails here instead of passing quietly.
///
/// This used to be a ratio of two wall-clock readings, at 88.3 ms against 3.7 ms
/// before the fix and 5.2 against 5.3 after it. Those numbers are what the
/// defect looked like; they are not what the guard reads, because on a busy
/// machine two readings 0.1 ms apart are decided by the scheduler.
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
    let connection = database.session();
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
    // The same range with no limit on it, which is what the pipeline breaker
    // this test was written for made the start arm cost. It is read here rather
    // than quoted from the bug report, so the run that asserts the guard also
    // contains the number the guard is bounded against.
    let whole_range = || {
        let rows = connection
            .query("SELECT id, body FROM chunk WHERE id > 'chunk-000000000' ORDER BY id")
            .expect("the whole range reads");
        assert_eq!(rows.len(), ROWS as usize - 1);
    };
    let last = ROWS - page as i64 - 1;
    // Once each first, so no arm pays for a plan another already has.
    read(0);
    read(last);
    whole_range();

    let before = fetches(&database);
    read(0);
    let between = fetches(&database);
    read(last);
    let after = fetches(&database);
    whole_range();
    let materialised = fetches(&database).saturating_sub(after);
    let start = between.saturating_sub(before);
    let end = after.saturating_sub(between);

    assert!(
        end > 0,
        "the page near the end of the table fetched no pages at all, so the \
         comparison below would be against nothing"
    );
    // The instrument is live. Reading the range costs 20,014 fetches here
    // against the page's 502, so the bound below is one a pipeline breaker
    // crosses by forty times rather than one nothing in this fixture can reach.
    assert!(
        materialised >= start.saturating_mul(4),
        "reading the whole range fetched {materialised} page(s) and reading one \
         page of {page} rows from the same key fetched {start}; the two are \
         close enough that the bound below could not tell them apart"
    );
    assert!(
        start <= end.saturating_mul(4),
        "a page of {page} rows at the start of a {ROWS}-row table fetched \
         {start} page(s) and one near the end fetched {end}, against \
         {materialised} for the whole range; the guard asks the start for at \
         most four times the end"
    );
}

/// Compiling a statement again does not re-allocate the scratch the last
/// compile filled, and one compile's allocations are a small fixed number.
///
/// **An allocation count is the one absolute number this file is allowed to
/// assert.** Everything else here is a ratio, because a duration is a reading
/// of the machine. This is not: the same code compiling the same statement
/// makes the same allocations on an idle box and on a saturated one, so the
/// bound below is tight on purpose rather than loose on principle. When it
/// fails, something changed what a compile does - and the new number belongs in
/// this test rather than the bound being widened to admit it.
///
/// What it guards, all of which were paid on every compile before task-2026:
/// the binder re-allocating its scope stack and its alias list instead of
/// taking the connection's `BinderScratch`; `finish_select` cloning the result
/// columns to hand `bind_order_by` a copy of something it only reads;
/// `Shape::operators` rendering the operator chain as text for a caller that
/// drops it; and the alias list being filled for a statement with no `GROUP
/// BY`, `HAVING`, `ORDER BY`, `LIMIT` or `OFFSET` to read one.
///
/// The plan cache is switched off, because a cached prepare is a hash lookup
/// and would report a compile as costing nothing. `prepare.trivial` on the
/// scorecard compiles on every iteration for the same reason.
#[test]
fn compiling_again_reuses_the_scratch_rather_than_allocating_it_afresh() {
    let directory = scratch("compile-allocations");
    let database = build(&directory.join("b.rdb"));
    let connection = database.session();
    connection
        .disable_optimizations(Levers::without(Levers::PLAN_CACHE))
        .expect("the engine is free");

    // The two shapes `open.prepare` is made of: a statement that names nothing,
    // and one that reads a table through a parameter.
    let trivial = "SELECT 1";
    let point = "SELECT id FROM t WHERE email = ?1";

    let compile = |sql: &str| {
        let statement = connection.prepare(sql).expect("the statement prepares");
        drop(statement);
    };

    // The first compile on a connection fills the parse arena and the binder's
    // scratch as well as doing the work, so it is read here rather than
    // described: the run that asserts the guard contains the number the guard
    // is against.
    let cold = allocations(|| compile(trivial));
    for _ in 0..20 {
        compile(trivial);
        compile(point);
    }
    let warm_trivial = allocations(|| compile(trivial));
    let warm_point = allocations(|| compile(point));

    // Asked after the measurements, on a connection of its own: the probe
    // compiles a statement, and doing that first would warm the arena the cold
    // reading above depends on being cold.
    let embedder = match embedder_is_compiled_in(&database) {
        true => EMBEDDER_COMPILE_ALLOCATIONS,
        false => 0,
    };
    let trivial_bound = TRIVIAL_COMPILE_ALLOCATIONS.saturating_add(embedder);
    let point_bound = POINT_COMPILE_ALLOCATIONS.saturating_add(embedder);

    // The runner gives this tier `--show-output`, so the numbers the guard was
    // measured at are in the log of every run rather than only in this comment.
    println!("compile allocations, warm connection, plan cache off:");
    println!("  embedder compiled in: {}", embedder > 0);
    println!("  {trivial:<34} cold {cold:>4}, warm {warm_trivial:>4} (bound {trivial_bound})");
    println!("  {point:<34}             warm {warm_point:>4} (bound {point_bound})");

    assert!(
        warm_trivial < cold,
        "the first compile of `{trivial}` on a fresh connection made {cold} \
         allocation(s) and a later one made {warm_trivial}; the guard asks the \
         later one for fewer, which is what reusing the arena and the binder's \
         scratch means"
    );
    assert!(
        warm_trivial <= trivial_bound,
        "compiling `{trivial}` made {warm_trivial} allocation(s) against a bound \
         of {trivial_bound}"
    );
    assert!(
        warm_point <= point_bound,
        "compiling `{point}` made {warm_point} allocation(s) against a bound of \
         {point_bound}"
    );
}

/// A statement outside a transaction rereads nothing the file has not changed.
///
/// **The guard task-2046 exists to leave behind.** `SELECT 1` through
/// `Connection` cost 132,884 nanoseconds outside a transaction and 1,126 inside
/// one, on the same connection over the same file, with nothing differing
/// between the two but an open `BEGIN` - 118 times, and about 132 microseconds
/// in absolute terms. Every application that uses the shipped API without
/// wrapping its reads in an explicit transaction paid it, per statement, and
/// nothing measured it: `inillucent-fullgate`, which is what the scorecard and
/// the performance contract are graded with, drives the engine's own `plan`,
/// `prepare` and `pipeline` calls and never opens a connection, so the cost was
/// invisible to every number this project publishes.
///
/// Eighty-nine of those microseconds were the meta record being read twice a
/// statement, a whole page at a time, to compare 116 bytes. The rest was the
/// Windows lock release unlocking two byte ranges a handle at SHARED does not
/// hold.
///
/// **It is a count and not a ratio of two clocks**, for the reason this file's
/// own header gives at length: the ratio it would assert reads between 1.18 and
/// 59.22 for one unchanged commit depending on what else is on the box, and a
/// threshold that survives that is a threshold that guards nothing. These two
/// counts read the same on an idle machine and on a saturated one, and each
/// names one thing the fix removed rather than the sum of both.
///
/// **What it does not see:** a cost that is neither of these counts. A future
/// change that reads the slots into a buffer it keeps, or that adds a third
/// syscall to the take, moves neither number. `inillucent-prepareperf`'s
/// breakdown table is where the whole per-statement cost is still visible, and
/// its `+step` and `+step in txn` columns are the measurement this guard is
/// derived from rather than a second copy of.
#[test]
fn a_statement_outside_a_transaction_rereads_nothing() {
    let directory = scratch("implicit-transaction");
    let database = build(&directory.join("b.rdb"));
    let connection = database.session();
    let mut statement = connection
        .prepare("SELECT 1")
        .expect("the statement prepares");

    // The first run of a fresh statement reads the slots in full, because this
    // connection has not yet written down the bytes it is going to compare
    // against. That is once per connection and it is not what the guard is
    // about, so it happens before the counters are read.
    step_once(&mut statement);

    let before = database.cache_stats();
    let outside_allocations = allocations(|| step_once(&mut statement));
    for _ in 0..(RUNS - 1) {
        step_once(&mut statement);
    }
    let after = database.cache_stats();
    let meta_reads = after.meta_reads.saturating_sub(before.meta_reads);
    let meta_probes = after.meta_probes.saturating_sub(before.meta_probes);

    connection
        .execute_batch("BEGIN")
        .expect("the transaction opens");
    let inside_allocations = allocations(|| step_once(&mut statement));
    let inside_before = database.cache_stats();
    for _ in 0..RUNS {
        step_once(&mut statement);
    }
    let inside_after = database.cache_stats();
    connection
        .execute_batch("COMMIT")
        .expect("the transaction commits");
    let inside_reads = inside_after
        .meta_reads
        .saturating_sub(inside_before.meta_reads);
    let inside_probes = inside_after
        .meta_probes
        .saturating_sub(inside_before.meta_probes);

    // The runner gives this tier `--show-output`, so the numbers are in the log
    // of every run and not only in the comment above.
    println!("`SELECT 1` stepped {RUNS} times through a connection:");
    println!(
        "  outside a transaction: {meta_reads} full meta read(s), \
         {meta_probes} record read(s), {outside_allocations} allocation(s) \
         for one step"
    );
    println!(
        "  inside one:            {inside_reads} full meta read(s), \
         {inside_probes} record read(s), {inside_allocations} allocation(s) \
         for one step"
    );

    assert_eq!(
        inside_reads, 0,
        "a statement inside a transaction read both meta slots in full \
         {inside_reads} time(s) over {RUNS} statements; a transaction holds \
         the file from its first statement to its commit, so there is no lock \
         acquisition for a staleness check to belong to"
    );
    assert_eq!(
        inside_probes, 0,
        "a statement inside a transaction read the meta record \
         {inside_probes} time(s) over {RUNS} statements, against none"
    );
    assert!(
        meta_reads <= META_READS_PER_UNCHANGED_STATEMENT.saturating_mul(RUNS),
        "{RUNS} statements outside a transaction read both meta slots in \
         full {meta_reads} time(s), against \
         {META_READS_PER_UNCHANGED_STATEMENT} per statement; nothing wrote the \
         file between them"
    );
    assert!(
        meta_probes <= META_PROBES_PER_UNCHANGED_STATEMENT.saturating_mul(RUNS),
        "{RUNS} statements outside a transaction read the meta record \
         {meta_probes} time(s), against \
         {META_PROBES_PER_UNCHANGED_STATEMENT} per statement"
    );
    assert!(
        outside_allocations <= inside_allocations,
        "one step outside a transaction made {outside_allocations} \
         allocation(s) and one inside a transaction made {inside_allocations}; \
         taking and giving back the file lock is not work that allocates"
    );
    // **A number, because the assertion above is a comparison** (task-2066
    // section 4.3.5). Both sides of it moved together when a `SELECT`'s result
    // column names were built on every execution, so three allocations a step
    // could be added without it noticing. This reads what one step costs.
    assert!(
        inside_allocations <= STEP_ALLOCATIONS,
        "one step of `SELECT 1` made {inside_allocations} allocation(s) against a bound of {STEP_ALLOCATIONS}"
    );
}
