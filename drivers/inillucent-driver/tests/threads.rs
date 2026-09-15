//! One database, several threads, one statement at a time.
//!
//! Invariant: **serialized, not parallel.** Any number of threads may use one
//! [`SharedDatabase`]; exactly one statement runs at a time, and a transaction
//! holds the turn for its whole life. These are the three properties that makes
//! true, each asserted as a value rather than as the absence of a crash.
//!
//! The database is never moved between threads: it is opened on a thread of its
//! own and stays there, and the handles send it statements. That is why
//! `inillucent-driver` keeps `#![forbid(unsafe_code)]` while offering this - see
//! the module documentation on `shared`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use inillucent_driver::{SharedDatabase, Value};

/// How many worker threads the fan-out test uses.
const WORKERS: i64 = 8;

/// How many rows each worker inserts.
const PER_WORKER: i64 = 1_000;

/// Returns a clean path for one test's database.
///
/// @param name - the test's name
fn scratch(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir()
        .join("inillucent-driver-threads")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the temporary directory is writable");
    directory.join("app.rdb")
}

/// Returns the one integer a counting query answered.
///
/// @param database - the database
/// @param sql - the query
fn count(database: &SharedDatabase, sql: &str) -> i64 {
    let rows = database.query_all(sql, &[]).expect("the count reads");
    match rows.value(0, 0) {
        Some(Value::Integer(number)) => *number,
        other => panic!("{sql} answered {other:?}"),
    }
}

/// Eight threads inserting a thousand rows each land eight thousand rows.
///
/// **The count is the assertion.** A design that lost writes under contention
/// would still open, still answer, and still look right in a log; the only
/// thing that catches it is asking how many rows are there afterwards. Eight
/// threads and a thousand rows each is enough that any two of them overlap on
/// every run.
#[test]
fn eight_threads_inserting_a_thousand_rows_each_land_eight_thousand() {
    let path = scratch("fan-out");
    let database = SharedDatabase::open(&path).expect("the database opens");
    database
        .execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, worker INTEGER, seq INTEGER)")
        .expect("the table is made");

    let mut threads = Vec::new();
    for worker in 0..WORKERS {
        let held = database.clone();
        threads.push(std::thread::spawn(move || {
            for seq in 0..PER_WORKER {
                held.execute(
                    "INSERT INTO note (worker, seq) VALUES (?1, ?2)",
                    &[Value::Integer(worker), Value::Integer(seq)],
                )
                .expect("the row is written");
            }
        }));
    }
    for thread in threads {
        thread.join().expect("the worker finished");
    }

    assert_eq!(
        count(&database, "SELECT count(*) FROM note"),
        WORKERS * PER_WORKER,
        "every row every worker wrote has to be there"
    );
    assert_eq!(
        count(&database, "SELECT count(DISTINCT worker) FROM note"),
        WORKERS,
        "and every worker has to have written some, or the test ran on fewer threads than it says"
    );
    assert_eq!(
        count(&database, "SELECT count(DISTINCT id) FROM note"),
        WORKERS * PER_WORKER,
        "no two rows share a key, which is what a rowid handed out under contention has to promise"
    );
    drop(database);
    let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
}

/// A reader during a writer's transaction sees the state before it or the state
/// after it, and nothing between.
///
/// **A transaction belongs to the database rather than to the handle that
/// opened it**, which is what makes this worth asserting: another thread's
/// statement between a `BEGIN` and its `COMMIT` would join that transaction and
/// be committed by it. [`SharedTransaction`] holds the turn for its whole life
/// so that cannot happen, and this is the shape that would catch it - a reader
/// sampling as fast as it can while the writer inserts a thousand rows one at a
/// time.
#[test]
fn a_reader_sees_the_state_before_a_transaction_or_the_state_after_it() {
    let path = scratch("transaction");
    let database = SharedDatabase::open(&path).expect("the database opens");
    database
        .execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, seq INTEGER)")
        .expect("the table is made");

    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let held = database.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                seen.push(count(&held, "SELECT count(*) FROM note"));
            }
            seen.push(count(&held, "SELECT count(*) FROM note"));
            seen
        })
    };

    {
        let writing = database.begin().expect("the transaction opens");
        for seq in 0..PER_WORKER {
            writing
                .execute("INSERT INTO note (seq) VALUES (?1)", &[Value::Integer(seq)])
                .expect("the row is written");
        }
        writing.commit().expect("the transaction commits");
    }
    stop.store(true, Ordering::Relaxed);
    let seen = reader.join().expect("the reader finished");

    assert!(
        !seen.is_empty(),
        "the reader never got a turn, so this asserts nothing"
    );
    for held in &seen {
        assert!(
            *held == 0 || *held == PER_WORKER,
            "a reader saw {held} rows part-way through a transaction that writes \
             {PER_WORKER}; it should see nothing or all of them"
        );
    }
    assert_eq!(
        seen.last().copied(),
        Some(PER_WORKER),
        "the last read is after the commit, so it has to see the whole transaction"
    );
    drop(database);
    let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
}

/// A database opened on one thread is used and dropped on another.
///
/// **Which is the whole point of the handle being `Send`.** An application's
/// pool hands work to whichever thread is free, and the last handle to go is
/// whichever one finished last - so the close happens wherever that was. The
/// assertion is that the file is usable again afterwards, which it is only if
/// the owner thread stopped and released its lock.
#[test]
fn a_database_is_used_and_dropped_on_another_thread() {
    let path = scratch("moved");
    let database = SharedDatabase::open(&path).expect("the database opens");
    database
        .execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")
        .expect("the table is made");

    let moved = std::thread::spawn(move || {
        database
            .execute(
                "INSERT INTO note (body) VALUES (?1)",
                &[Value::Text("written elsewhere".to_string())],
            )
            .expect("the row is written");
        let held = count(&database, "SELECT count(*) FROM note");
        drop(database);
        held
    });
    assert_eq!(moved.join().expect("the thread finished"), 1);

    // The file opens again, which it does only if the owner thread stopped and
    // let go of its lock when the last handle was dropped.
    let reopened = SharedDatabase::open(&path).expect("the database reopens");
    assert_eq!(count(&reopened, "SELECT count(*) FROM note"), 1);
    drop(reopened);
    let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
}

/// A transaction dropped without being committed leaves nothing behind.
///
/// The same promise `Transaction` makes on one thread, and it has to survive
/// the turn being given back: a rollback that did not run would leave the
/// transaction open and the next thread's statement inside it.
#[test]
fn a_dropped_transaction_rolls_back_and_gives_the_turn_up() {
    let path = scratch("dropped");
    let database = SharedDatabase::open(&path).expect("the database opens");
    database
        .execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, seq INTEGER)")
        .expect("the table is made");

    {
        let writing = database.begin().expect("the transaction opens");
        writing
            .execute("INSERT INTO note (seq) VALUES (1)", &[])
            .expect("the row is written");
        // No commit: the guard goes out of scope here.
    }
    assert_eq!(
        count(&database, "SELECT count(*) FROM note"),
        0,
        "a transaction nobody committed writes nothing"
    );

    // And the turn came back, so another thread can still work.
    let held = database.clone();
    let after = std::thread::spawn(move || {
        held.execute("INSERT INTO note (seq) VALUES (2)", &[])
            .expect("the row is written");
    });
    after.join().expect("the thread finished");
    assert_eq!(count(&database, "SELECT count(*) FROM note"), 1);
    drop(database);
    let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
}
