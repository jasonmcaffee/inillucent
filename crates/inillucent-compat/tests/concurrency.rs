//! Two connections at once: schedules, snapshots, and the three ways out of a
//! statement that is not going to finish.
//!
//! Invariant: whatever order two connections' operations arrive in, the
//! database they leave behind is one that could have been produced by running
//! their transactions one after the other, and every transaction that reported
//! success is in it. Nothing here asserts on timing. The interleavings are
//! chosen by a deterministic scheduler and recorded, so a failure names the
//! schedule that produced it and replaying that schedule reproduces it.
//!
//! The liveness half is the other side of the same promise. A connection that
//! cannot get the lock has to be told so rather than wait forever; a statement
//! that is taking too long has to be stoppable from outside; and a reader must
//! be able to hold a snapshot without the writer having to wait for it. Those
//! are what make concurrency usable rather than merely correct.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use inillucent_compat::workspace_root;
use inillucent_legacy::{Database, Value};
use inillucent_session::connection::{Connection, OpenOptions, SessionDatabase};
use inillucent_sim::media::MediaModel;
use inillucent_sim::schedule::{explore_two_actors, ActorId, Decisions, Scheduler};
use inillucent_sim::sim_vfs::{set_current_actor, SimConfig, SimVfs};
use inillucent_transaction::journal::{JournalMode, JournalOptions, Synchronous};
use inillucent_vfs::path::DbPath;
use inillucent_vfs::Vfs;

/// How many transactions each actor tries to commit in a scheduled run.
const ROUNDS: i64 = 3;

/// Returns a fresh scratch path, with every companion file removed.
fn scratch(name: &str) -> std::path::PathBuf {
    let directory = workspace_root().join("_agent_output/concurrency");
    let _ = std::fs::create_dir_all(&directory);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
    directory.join(format!("{name}.db"))
}

/// Opens a connection on a real file.
fn connect(path: &std::path::Path) -> inillucent_legacy::Connection {
    let database = Database::open(path).expect("the database opens");
    database.connect().expect("the connection opens")
}

/// Returns the single integer a query reports.
fn integer(connection: &inillucent_legacy::Connection, sql: &str) -> i64 {
    let rows = connection.query(sql).expect("the query runs");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => *value,
        other => panic!("{sql} reported {other:?}"),
    }
}

/// Fills a one-column table with `rows` integers.
///
/// A recursive CTE would say this in one statement, which `INSERT` does not
/// take yet; doubling the table is the next shortest thing and is what makes
/// the cross joins below long enough to be worth stopping.
fn fill(connection: &inillucent_legacy::Connection, rows: i64) {
    connection
        .execute_batch("INSERT INTO t VALUES(1)")
        .expect("the first row");
    while integer(connection, "SELECT count(*) FROM t") < rows {
        connection
            .execute_batch("INSERT INTO t SELECT a FROM t")
            .expect("the table doubles");
    }
}

/// Returns the text a query reports.
fn text(connection: &inillucent_legacy::Connection, sql: &str) -> String {
    let rows = connection.query(sql).expect("the query runs");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Text(value)) => String::from_utf8_lossy(&value.utf8_bytes()).to_string(),
        other => panic!("{sql} reported {other:?}"),
    }
}

/// Opens a simulated database in WAL mode.
fn simulated(vfs: &Arc<SimVfs>) -> Result<Connection, inillucent_base::DbError> {
    let database = SessionDatabase::open_with(
        DbPath::from("/sim/busy.db").as_path(),
        Arc::clone(vfs) as Arc<dyn Vfs>,
        OpenOptions {
            journal: JournalOptions {
                mode: JournalMode::Wal,
                synchronous: Synchronous::Full,
            },
            // Zero, deliberately. A connection that waits is a connection the
            // scheduler has to wait for, and every wait would be a decision
            // taken on the strength of how long a sleep happened to be. A
            // refusal is a decision the schedule can hold still.
            busy_timeout: std::time::Duration::ZERO,
            ..OpenOptions::default()
        },
    )?;
    database.connect()
}

/// Runs one script, reporting whether it succeeded.
fn run(connection: &Connection, sql: &str) -> Result<(), inillucent_base::DbError> {
    inillucent_session::statement::execute_batch(connection, sql.as_bytes())
}

/// What one scheduled run produced.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Outcome {
    /// The keys whose transactions reported success, in the order they landed.
    ///
    /// This is what says the schedule was a schedule: two actors committing in
    /// a fixed order every time would mean the decisions were not reaching the
    /// engine, and the exploration would be one run repeated sixty-four times.
    order: Vec<i64>,
    /// The same keys, sorted, which is what the database is judged against.
    committed: Vec<i64>,
    /// The keys the database held afterwards.
    present: Vec<i64>,
    /// How many times a transaction was refused and tried again.
    refusals: u64,
    /// What the integrity check said.
    integrity: String,
}

/// Runs two connections against one simulated database under `decisions`.
///
/// Each actor commits `ROUNDS` single-row transactions, retrying its own
/// transaction when the other holds the writer. The keys are disjoint, so the
/// two orders are distinguishable but neither is wrong: what is being tested is
/// that both connections' committed rows survive, whichever order they land in.
fn scheduled_run(decisions: Decisions) -> (Outcome, Vec<usize>) {
    let vfs = Arc::new(SimVfs::new(SimConfig {
        seed: 4242,
        model: MediaModel::default(),
        ..SimConfig::default()
    }));
    {
        let setup = simulated(&vfs).expect("the connection opens");
        run(
            &setup,
            "PRAGMA journal_mode=wal; CREATE TABLE t(a INTEGER PRIMARY KEY, who INTEGER);",
        )
        .expect("the schema builds");
    }
    let scheduler = Scheduler::new(2, decisions);
    vfs.attach_scheduler(Arc::clone(&scheduler));
    let committed = Arc::new(Mutex::new(Vec::new()));
    let refusals = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for id in 0..2u32 {
        let vfs = Arc::clone(&vfs);
        let scheduler = Arc::clone(&scheduler);
        let committed = Arc::clone(&committed);
        let refusals = Arc::clone(&refusals);
        handles.push(std::thread::spawn(move || {
            let actor = ActorId(id);
            set_current_actor(actor);
            if let Ok(connection) = simulated(&vfs) {
                for round in 0..ROUNDS {
                    let key = i64::from(id) * 100 + round;
                    let sql = format!("BEGIN; INSERT INTO t VALUES({key}, {id}); COMMIT;");
                    // A refusal is not a failure: the other connection holds
                    // the writer, and the transaction is tried again. The bound
                    // is what turns a livelock into a failing test rather than
                    // a hanging one.
                    for _ in 0..64 {
                        match run(&connection, &sql) {
                            Ok(()) => {
                                lock(&committed).push(key);
                                break;
                            }
                            Err(_) => {
                                refusals.fetch_add(1, Ordering::Relaxed);
                                let _ = run(&connection, "ROLLBACK");
                            }
                        }
                    }
                }
            }
            scheduler.finish(actor);
        }));
    }
    for handle in handles {
        handle.join().expect("the actor finished");
    }
    let schedule = scheduler.recorded_schedule();
    let order = lock(&committed).clone();
    let mut committed = order.clone();
    committed.sort_unstable();
    let reader = simulated(&vfs).expect("the connection reopens");
    let mut present = Vec::new();
    let (mut statement, _) =
        inillucent_session::statement::Statement::prepare(&reader, b"SELECT a FROM t ORDER BY a")
            .expect("the query prepares");
    while statement.step().expect("the query runs") {
        if let Some(value) = statement.row().first().and_then(Value::as_integer) {
            present.push(value);
        }
    }
    drop(statement);
    // The check runs against the pager rather than through SQL, because
    // `PRAGMA integrity_check` is not wired into the front end yet; what it
    // reports is the same walk, and it is the walk that matters here.
    let integrity = reader
        .with_database(inillucent_storage::MAIN_DATABASE, |pager| {
            pager.begin_read()?;
            let report = inillucent_storage::check::check_database(
                pager,
                inillucent_storage::check::CheckLevel::Integrity,
            );
            let released = pager.end_read();
            let report = report?;
            released?;
            Ok(report.as_pragma_output().join("; "))
        })
        .and_then(|inner| inner)
        .unwrap_or_else(|failure| format!("{failure}"));
    (
        Outcome {
            order,
            committed,
            present,
            refusals: refusals.load(Ordering::Relaxed),
            integrity,
        },
        schedule,
    )
}

/// Locks a mutex, recovering from poisoning rather than propagating a panic.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Judges one run, returning what went wrong or nothing.
fn judge(outcome: &Outcome) -> Option<String> {
    if outcome.integrity != "ok" {
        return Some(format!("integrity_check said {}", outcome.integrity));
    }
    for key in &outcome.committed {
        if !outcome.present.contains(key) {
            return Some(format!("row {key} committed and is not in the database"));
        }
    }
    for key in &outcome.present {
        if !outcome.committed.contains(key) {
            return Some(format!("row {key} is in the database and never committed"));
        }
    }
    if outcome.committed.len() as i64 != ROUNDS * 2 {
        return Some(format!(
            "only {} of {} transactions got through",
            outcome.committed.len(),
            ROUNDS * 2
        ));
    }
    None
}

/// Every interleaving of two committing connections leaves every committed row
/// in the database and nothing else, and none of them deadlocks.
///
/// The bounded exploration is exhaustive over the first decisions of the run,
/// which is where the interesting races are: both connections are trying for
/// the writer at once, and the loser has to notice and try again.
#[test]
fn every_explored_schedule_of_two_writers_is_serialisable() {
    let mut report = String::from("schedule\torder\trefusals\tintegrity\n");
    let explored = explore_two_actors(6, |schedule| {
        scheduled_run(Decisions::Replay(schedule.clone(), 7))
    });
    let mut orders = std::collections::BTreeSet::new();
    let mut refused = 0u64;
    for (schedule, (outcome, recorded)) in &explored {
        if let Some(complaint) = judge(outcome) {
            panic!("schedule {schedule:?} ({recorded:?}): {complaint}");
        }
        orders.insert(outcome.order.clone());
        refused = refused.saturating_add(outcome.refusals);
        report.push_str(&format!(
            "{schedule:?}\t{:?}\t{}\t{}\n",
            outcome.order, outcome.refusals, outcome.integrity
        ));
    }
    assert_eq!(explored.len(), 64, "the exploration was cut short");
    assert!(
        orders.len() > 1,
        "every schedule committed in the same order, so the decisions never reached the engine"
    );
    assert!(
        refused > 0,
        "no transaction was ever refused, so the writers never contended"
    );
    record("schedules-two-writers.tsv", &report);
}

/// A schedule that is recorded reproduces the run it came from.
///
/// Without this the exploration above would be noise: a failing schedule that
/// could not be replayed would name a bug nobody could look at twice.
#[test]
fn a_recorded_schedule_reproduces_its_run() {
    let (first, schedule) = scheduled_run(Decisions::Seeded(31337));
    for _ in 0..3 {
        let (again, _) = scheduled_run(Decisions::Replay(schedule.clone(), 0));
        assert_eq!(again, first, "replaying schedule {schedule:?} diverged");
    }
    record("schedule-replay.txt", &format!("{schedule:?}\n{first:?}\n"));
}

/// A reader holds its snapshot while a writer commits over the top of it, and
/// sees the writer's rows only once it has let go.
///
/// This is the promise WAL mode exists to make. The writer never waits for the
/// reader and the reader never sees a row that arrived after it started, which
/// together are why a log is worth the second file.
#[test]
fn a_reader_keeps_its_snapshot_while_a_writer_commits() {
    let path = scratch("snapshot");
    let writer = connect(&path);
    writer
        .execute_batch("PRAGMA journal_mode=wal")
        .expect("the mode changes");
    writer
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
        .expect("the table is made");
    writer
        .execute_batch("INSERT INTO t VALUES(1)")
        .expect("a row");

    let reader = connect(&path);
    reader.execute_batch("BEGIN").expect("the read opens");
    assert_eq!(integer(&reader, "SELECT count(*) FROM t"), 1);

    // The writer commits twice while the reader is holding its snapshot. It
    // must not have to wait for the reader to do it.
    writer
        .execute_batch("INSERT INTO t VALUES(2)")
        .expect("a row");
    writer
        .execute_batch("INSERT INTO t VALUES(3)")
        .expect("a row");
    assert_eq!(integer(&writer, "SELECT count(*) FROM t"), 3);

    // The reader is still looking at the database it started with.
    assert_eq!(
        integer(&reader, "SELECT count(*) FROM t"),
        1,
        "a reader saw rows that were committed after it began"
    );
    reader.execute_batch("COMMIT").expect("the read closes");
    assert_eq!(
        integer(&reader, "SELECT count(*) FROM t"),
        3,
        "a reader did not pick up the new rows after letting go"
    );
}

/// A checkpoint may not copy back a frame a reader is still reading, and says
/// so rather than doing it anyway.
#[test]
fn a_checkpoint_does_not_pass_a_reader() {
    let path = scratch("protected");
    let writer = connect(&path);
    writer
        .execute_batch("PRAGMA journal_mode=wal")
        .expect("the mode changes");
    writer
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
        .expect("the table is made");
    writer
        .execute_batch("INSERT INTO t VALUES(1)")
        .expect("a row");

    let reader = connect(&path);
    reader.execute_batch("BEGIN").expect("the read opens");
    assert_eq!(integer(&reader, "SELECT count(*) FROM t"), 1);
    writer
        .execute_batch("INSERT INTO t VALUES(2)")
        .expect("a row");

    // A passive checkpoint reports how many frames the log holds and how many
    // it managed to copy. The reader's mark is in the way of the last commit,
    // so the two numbers cannot agree.
    let rows = writer
        .query("PRAGMA wal_checkpoint(PASSIVE)")
        .expect("the checkpoint runs");
    let row = rows.first().expect("the checkpoint reports a row");
    let frames = row.get(1).and_then(Value::as_integer).unwrap_or(-1);
    let copied = row.get(2).and_then(Value::as_integer).unwrap_or(-1);
    assert!(
        copied < frames,
        "a checkpoint copied {copied} of {frames} frames past a live reader"
    );
    reader.execute_batch("COMMIT").expect("the read closes");

    // With the reader gone the same checkpoint finishes the job.
    let rows = writer
        .query("PRAGMA wal_checkpoint(PASSIVE)")
        .expect("the checkpoint runs");
    let row = rows.first().expect("the checkpoint reports a row");
    let frames = row.get(1).and_then(Value::as_integer).unwrap_or(-1);
    let copied = row.get(2).and_then(Value::as_integer).unwrap_or(-1);
    assert_eq!(copied, frames, "the log was not fully copied back");
}

/// One writer at a time, and the loser is told rather than left waiting.
///
/// Both connections are in this process, which is the case a kernel lock
/// cannot decide on POSIX: byte-range locks are held per process, so two
/// connections here would each be told they hold the writer unless something
/// in the process arbitrates first.
#[test]
fn a_second_writer_in_this_process_is_refused() {
    let path = scratch("one-writer");
    let first = connect(&path);
    first
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
        .expect("the table is made");
    let second = connect(&path);

    first
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the writer opens");
    let refused = second.execute_batch("BEGIN IMMEDIATE");
    assert_eq!(
        refused.map_err(|error| error.code()),
        Err(inillucent_base::PrimaryCode::Busy),
        "two connections held the writer at once"
    );

    // An autocommit write has to be refused too, and by its own route: it takes
    // the reservation inside the statement rather than at a `BEGIN`, and that
    // route once had a POSIX-only hole in it that let both connections write at
    // the same time and lost one of the two transactions.
    let refused = second.execute_batch("INSERT INTO t VALUES(99)");
    assert_eq!(
        refused.map_err(|error| error.code()),
        Err(inillucent_base::PrimaryCode::Busy),
        "an autocommit write got in while another connection held the writer"
    );

    // And once the first one commits, the second gets in.
    first
        .execute_batch("INSERT INTO t VALUES(1)")
        .expect("a row");
    first.execute_batch("COMMIT").expect("the writer closes");
    second
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the writer is free now");
    second
        .execute_batch("INSERT INTO t VALUES(2)")
        .expect("a row");
    second.execute_batch("COMMIT").expect("the writer closes");
    assert_eq!(integer(&first, "SELECT count(*) FROM t"), 2);
}

/// A connection that has already read a page sees another connection's commit
/// to it, in both journal modes.
///
/// Each connection has its own page cache, so a page read once is served from
/// memory the next time - and a second connection committing over it leaves
/// the first with bytes that no longer describe the file. What stops that is
/// the change counter in the header: it moves on every commit, so a reader
/// entering a read transaction can tell in a hundred bytes whether anything it
/// holds is still true.
///
/// The first version of this test found the bug rather than confirming the
/// fix, and it found it on Linux only: on Windows the cache happened to have
/// dropped the page. A test that reads the rows first, so the page is
/// certainly cached, catches it on either.
#[test]
fn a_connection_sees_another_connections_commit() {
    for mode in ["delete", "wal"] {
        let path = scratch(&format!("stale-cache-{mode}"));
        let first = connect(&path);
        first
            .execute_batch(&format!("PRAGMA journal_mode={mode}"))
            .expect("the mode changes");
        first
            .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .expect("the table is made");
        first
            .execute_batch("INSERT INTO t VALUES(1, 'first')")
            .expect("a row");

        let second = connect(&path);
        // Both connections read the row, so both have the leaf page cached and
        // a stale answer is available to be given.
        assert_eq!(integer(&first, "SELECT count(*) FROM t"), 1);
        assert_eq!(integer(&second, "SELECT count(*) FROM t"), 1);

        second
            .execute_batch("INSERT INTO t VALUES(2, 'second')")
            .expect("a row");
        assert_eq!(
            integer(&first, "SELECT count(*) FROM t"),
            2,
            "in {mode} mode a connection served a page from before another connection's commit"
        );
        assert_eq!(
            text(&first, "SELECT b FROM t WHERE a = 2"),
            "second",
            "in {mode} mode the row that came back was not the one committed"
        );

        // And back the other way, so neither connection is special.
        first
            .execute_batch("INSERT INTO t VALUES(3, 'third')")
            .expect("a row");
        assert_eq!(integer(&second, "SELECT count(*) FROM t"), 3);

        // The same again with an explicit transaction on the connection that
        // is about to be overtaken. This is the shape that actually caught the
        // bug: a transaction the caller opened and closed itself leaves pages
        // in the cache that an autocommit statement had already let go of.
        first
            .execute_batch("BEGIN IMMEDIATE")
            .expect("the writer opens");
        first
            .execute_batch("INSERT INTO t VALUES(4, 'fourth')")
            .expect("a row");
        first.execute_batch("COMMIT").expect("the writer closes");
        second
            .execute_batch("INSERT INTO t VALUES(5, 'fifth')")
            .expect("a row");
        assert_eq!(
            integer(&first, "SELECT count(*) FROM t"),
            5,
            "in {mode} mode a connection that had run its own transaction served a stale page"
        );
    }
}

/// A busy timeout turns a refusal into a wait that succeeds when the other
/// connection lets go.
#[test]
fn a_busy_timeout_waits_for_the_writer() {
    let path = scratch("busy-timeout");
    {
        let setup = connect(&path);
        setup
            .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
            .expect("the table is made");
    }
    // Both connections have a timeout, and both need one. The waiter waits for
    // the writer's reservation; the holder waits, at its commit, for the
    // waiter's read to go away. A commit is the one lock the caller cannot
    // retry for - the journal is already written and synced by then - so that
    // wait happens inside the pager, and a connection that had not asked for
    // one gets SQLite's default of a `SQLITE_BUSY` on `COMMIT`.
    let holder = Database::open_with_busy_timeout(&path, std::time::Duration::from_secs(10))
        .expect("the database opens")
        .connect()
        .expect("the connection opens");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the writer opens");
    holder
        .execute_batch("INSERT INTO t VALUES(1)")
        .expect("a row");

    let path_for_thread = path.clone();
    let waiter = std::thread::spawn(move || {
        let database =
            Database::open_with_busy_timeout(&path_for_thread, std::time::Duration::from_secs(10))
                .expect("the database opens");
        let connection = database.connect().expect("the connection opens");
        connection.execute_batch("INSERT INTO t VALUES(2)")
    });

    // The holder finishes while the other connection is waiting for it.
    std::thread::sleep(std::time::Duration::from_millis(50));
    holder.execute_batch("COMMIT").expect("the writer closes");
    waiter
        .join()
        .expect("the waiting thread finished")
        .expect("the wait ended in a write, not a refusal");
    let fresh = connect(&path);
    eprintln!(
        "PROBE fresh={} rows={:?} holder={} size={:?}",
        integer(&fresh, "SELECT count(*) FROM t"),
        fresh.query("SELECT a FROM t ORDER BY a"),
        integer(&holder, "SELECT count(*) FROM t"),
        std::fs::metadata(&path).map(|m| m.len()),
    );
    assert_eq!(integer(&holder, "SELECT count(*) FROM t"), 2);
}

/// A progress callback stops a statement that is taking too long, and the
/// connection is usable straight afterwards.
///
/// It is the single-threaded way out: there is no other thread to call
/// `interrupt` from, so the machine asks on the way past instead.
#[test]
fn a_progress_handler_stops_a_long_statement() {
    let path = scratch("progress");
    let connection = connect(&path);
    connection
        .execute_batch("CREATE TABLE t(a INTEGER)")
        .expect("the table is made");
    fill(&connection, 512);

    let calls = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&calls);
    connection.set_progress_handler(
        16,
        Some(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed) >= 4
        })),
    );
    let stopped = connection.query("SELECT count(*) FROM t AS x, t AS y");
    assert_eq!(
        stopped.map(|rows| rows.len()).map_err(|error| error.code()),
        Err(inillucent_base::PrimaryCode::Interrupt),
        "the progress handler did not stop the statement"
    );
    assert!(
        calls.load(Ordering::Relaxed) >= 5,
        "the handler was asked {} times",
        calls.load(Ordering::Relaxed)
    );

    // With the handler gone the connection works exactly as before.
    connection.set_progress_handler(16, None);
    assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 512);
}

/// An interrupt from another thread stops a statement, and the connection
/// recovers once the flag is cleared.
#[test]
fn an_interrupt_from_another_thread_stops_a_statement() {
    let path = scratch("interrupt");
    let connection = connect(&path);
    connection
        .execute_batch("CREATE TABLE t(a INTEGER)")
        .expect("the table is made");
    fill(&connection, 512);

    // The flag is what crosses the thread boundary; the connection itself does
    // not have to, which is the same shape `sqlite3_interrupt` has.
    let flag = connection.interrupt_flag();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        flag.store(true, Ordering::Relaxed);
    });
    let stopped = connection.query("SELECT count(*) FROM t AS x, t AS y, t AS z");
    stopper.join().expect("the stopping thread finished");
    assert_eq!(
        stopped.map(|rows| rows.len()).map_err(|error| error.code()),
        Err(inillucent_base::PrimaryCode::Interrupt),
        "the interrupt did not stop the statement"
    );
    connection.clear_interrupt();
    assert_eq!(integer(&connection, "SELECT count(*) FROM t"), 512);
}

/// A wal-index left behind by a process that died is rebuilt from the log
/// rather than believed, so the rows a crashed writer committed are all there.
///
/// The files are copied while a connection has them open, which is what a
/// power loss leaves on the disk: a database, a log, and a shared-memory index
/// nobody can vouch for.
#[test]
fn a_wal_index_left_by_a_crash_is_rebuilt() {
    let path = scratch("orphan-index");
    let mut saved: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::new();
    {
        let writer = connect(&path);
        writer
            .execute_batch("PRAGMA journal_mode=wal")
            .expect("the mode changes");
        writer
            .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
            .expect("the table is made");
        for key in 1..=8 {
            writer
                .execute_batch(&format!("INSERT INTO t VALUES({key})"))
                .expect("a row");
        }
        // A second connection is what makes the index exist and stay: the
        // files are read while both are open, exactly as a crash would find
        // them.
        let reader = connect(&path);
        assert_eq!(integer(&reader, "SELECT count(*) FROM t"), 8);
        for suffix in ["", "-wal", "-shm"] {
            let companion = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
            if let Ok(bytes) = std::fs::read(&companion) {
                saved.push((companion, bytes));
            }
        }
    }
    assert_eq!(saved.len(), 3, "the crash image is missing a file");

    // Put the image back, which undoes the tidy close the connections made.
    for (companion, bytes) in &saved {
        std::fs::write(companion, bytes).expect("the crash image is restored");
    }
    let survivor = connect(&path);
    assert_eq!(
        integer(&survivor, "SELECT count(*) FROM t"),
        8,
        "a database recovered from a crash image lost committed rows"
    );
    assert_eq!(
        integer(&survivor, "SELECT sum(a) FROM t"),
        36,
        "the rows that came back were not the ones that were committed"
    );
    survivor
        .execute_batch("INSERT INTO t VALUES(9)")
        .expect("the recovered database is writable");
    assert_eq!(integer(&survivor, "SELECT count(*) FROM t"), 9);
}

/// Writes a report into the checked-in schedules.
fn record(name: &str, body: &str) {
    let directory = workspace_root().join("tests/schedules");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join(name), body);
}
