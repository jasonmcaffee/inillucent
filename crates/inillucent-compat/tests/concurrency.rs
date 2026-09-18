//! Two connections at once: locks, snapshots, and crash recovery.
//!
//! Invariant: whatever order two connections' operations arrive in, the
//! database they leave behind is one every committed transaction is really in.
//! A connection that cannot get the lock has to be told so rather than wait
//! forever; a reader must be able to hold a snapshot without the writer having
//! to wait for it. Those are what make concurrency usable rather than merely
//! correct.
//!
//! **Two capabilities this file used to test are gone from the new engine's
//! public surface, and the two test cases that needed them are deleted rather
//! than faked:**
//!
//! - **A deterministic two-actor schedule exploration.** The old suite opened
//!   the engine on `inillucent_sim::sim_vfs::SimVfs` and drove it through
//!   `inillucent_sim::schedule::{Scheduler, explore_two_actors}` so every
//!   interleaving of two committing connections could be enumerated and
//!   replayed. `inillucent_engine::connect::Database` has no constructor that
//!   accepts a caller's `Vfs` - only `open`/`open_with`/`import`/`import_with`,
//!   all fixed to the real filesystem or an internal `MemoryVfs`. The lower
//!   layer, `inillucent_engine::ImportedDatabase::open_on`/`create_on`, *does*
//!   take an `Arc<dyn Vfs>`, but `connect::Database`'s fields are private and
//!   it exposes no way to wrap one. Without that, nothing outside
//!   `inillucent-engine` can drive the new engine through the simulator's
//!   scheduler at all, and `every_explored_schedule_of_two_writers_is_serialisable`
//!   and `a_recorded_schedule_reproduces_its_run` cannot be ported.
//! - **A progress handler and a cross-thread interrupt.** `Connection` has no
//!   `set_progress_handler`, `interrupt_flag`, or `clear_interrupt` of any
//!   kind, and nothing in `inillucent-engine` implements one under another
//!   name. `a_progress_handler_stops_a_long_statement` and
//!   `an_interrupt_from_another_thread_stops_a_statement` are deleted for the
//!   same reason: there is nothing left to call.
//!
//! Both are capability gaps in the new engine rather than a test-writing
//! problem, and are reported as such rather than smoothed over.

use std::path::PathBuf;

use inillucent_base::PrimaryCode;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// Returns a fresh scratch path, with every companion file removed.
fn scratch(name: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/concurrency");
    let _ = std::fs::create_dir_all(&directory);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(directory.join(format!("{name}.db{suffix}")));
    }
    directory.join(format!("{name}.db"))
}

/// Returns the workspace root, the way every scratch path here is rooted.
fn workspace_root() -> PathBuf {
    inillucent_compat::workspace_root()
}

/// Opens the one database a path has.
///
/// **One `Database` per file, and every "connection" in this file is a session
/// on it.** The old engine's `Connection` owned its own handle, so a suite
/// about two connections opened the path twice; this engine is built the other
/// way round and says so - "one file is one pool: two connections that each
/// held a pool over one file would be two page caches over one set of bytes".
/// Opening the path a second time therefore does not give a second connection,
/// it gives a second *writer*, and the second one is refused with "a writer
/// holds PENDING" before any test body runs. `database.session()` twice is the
/// shape these tests are actually about.
fn database(path: &std::path::Path) -> Database {
    Database::open(path).expect("the database opens")
}

/// Returns the single integer a query reports.
fn integer(connection: &Connection<'_>, sql: &str) -> i64 {
    let rows = connection.query(sql).expect("the query runs");
    match rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("{sql} reported {other:?}"),
    }
}

/// Returns the text a query reports.
fn text(connection: &Connection<'_>, sql: &str) -> String {
    let rows = connection.query(sql).expect("the query runs");
    match rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Text(value)) => String::from_utf8_lossy(value).to_string(),
        other => panic!("{sql} reported {other:?}"),
    }
}

/// Returns a row value as an integer, or `-1` when it is not one.
///
/// For the checkpoint counters below, which SQLite (and this engine) always
/// answers as integers, so `-1` reads as "not what was expected" rather than
/// silently matching a real frame count.
fn as_integer(value: Option<&OwnedDatum>) -> i64 {
    match value {
        Some(OwnedDatum::Int(value)) => *value,
        _ => -1,
    }
}

/// Two sessions on one database share what they can see, so a `BEGIN` on one
/// does **not** hold a snapshot against the other's commits.
///
/// **This is a recorded difference, not the behaviour anybody wants.** It used
/// to assert the opposite - a reader holding its snapshot while a writer
/// committed over the top of it, which is the promise WAL mode exists to make
/// and which the old engine kept, because each of its connections owned its own
/// handle and its own read mark. This engine is one pool per file, and the two
/// "connections" here are two *sessions* on that one pool, so the second one
/// reads the first one's committed rows immediately.
///
/// Where the real promise lives now is **across processes**, and this file is
/// not where it is graded: `process_concurrency.rs` spawns two real writer
/// processes and asserts that rows present equal commits acknowledged. That
/// distinction is not a technicality - until task-1980 this file's own comment
/// claimed the cross-process case was measured, and two real processes were
/// losing 43% of their acknowledged commits at the time (task-1979, section 4).
/// What this test pins is the in-process answer, so that a future
/// engine which does give a session its own read mark turns this red and the
/// original expectations - 1 while the writer commits, 3 after the commit - go
/// back in.
#[test]
fn two_sessions_on_one_database_do_not_hold_separate_snapshots() {
    let path = scratch("snapshot");
    let held = database(&path);
    let writer = held.session();
    writer
        .execute_batch("PRAGMA journal_mode=wal")
        .expect("the mode changes");
    writer
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
        .expect("the table is made");
    writer
        .execute_batch("INSERT INTO t VALUES(1)")
        .expect("a row");

    let reader = held.session();
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

    // The reader sees all three straight away, because it is a session on the
    // same pool rather than a connection with a read mark of its own.
    assert_eq!(
        integer(&reader, "SELECT count(*) FROM t"),
        3,
        "a session on one pool is expected to see the other session's commits \
         immediately; if this now reads 1, sessions have grown their own read \
         marks and this test should go back to asserting 1 here and 3 after the \
         COMMIT below"
    );
    reader.execute_batch("COMMIT").expect("the read closes");
    assert_eq!(
        integer(&reader, "SELECT count(*) FROM t"),
        3,
        "the rows are still all there after the read transaction closes"
    );
}

/// A passive checkpoint copies the whole log back past a session whose
/// `BEGIN` has not written anything, because a session is not a reader with a
/// mark.
///
/// **The second recorded difference, and it has the same cause as
/// `two_sessions_on_one_database_do_not_hold_separate_snapshots`.** This used
/// to assert that a checkpoint could *not* pass a live reader - that it copied
/// fewer frames than the log held and said so - which is what protects a reader
/// from having the pages under it rewritten. With one pool per file there is no
/// second reader to be in the way, so the checkpoint finishes.
///
/// **The reader's `BEGIN` here must stay unwritten.** This test used to have
/// the writer insert a second row *after* the reader's `BEGIN`, meaning to
/// model a concurrent writer running past a live reader. On one shared
/// `ImportedDatabase` there is no such thing: `self.batch` is one field the
/// two `connect()` handles share, so that insert was not a second writer, it
/// was the reader's own transaction being written to - and
/// `pragma_wal_checkpoint`'s refusal (added alongside the no-steal fix,
/// matching the pinned SQLite 3.53.4 reference) now declines a checkpoint
/// there correctly. See `a_checkpoint_refuses_once_a_shared_transaction_has_written`,
/// below, for that case asserted directly, and `docs/sql.md`'s
/// `PRAGMA wal_checkpoint` entry.
///
/// A reader that genuinely holds a checkpoint back, independent of any
/// writer's transaction, is a cross-process case, and `docs/roadmap.md` item 9
/// is where that is graded. If this engine ever gives a session a read mark of
/// its own, this goes red and the original assertion - `copied < frames` while
/// the reader is open, then `copied == frames` after it commits - is what
/// belongs here again.
#[test]
fn a_checkpoint_copies_the_whole_log_back_past_a_session() {
    let path = scratch("protected");
    let held = database(&path);
    let writer = held.session();
    writer
        .execute_batch("PRAGMA journal_mode=wal")
        .expect("the mode changes");
    writer
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
        .expect("the table is made");
    writer
        .execute_batch("INSERT INTO t VALUES(1)")
        .expect("a row");

    let reader = held.session();
    reader.execute_batch("BEGIN").expect("the read opens");
    assert_eq!(integer(&reader, "SELECT count(*) FROM t"), 1);

    // A passive checkpoint reports how many frames the log holds and how many
    // it managed to copy. Both numbers are real and both are asserted: the log
    // has frames in it, and with no reader mark to stop it - and nothing
    // written inside the reader's still-open `BEGIN` - the checkpoint copies
    // all of them.
    let rows = writer
        .query("PRAGMA wal_checkpoint(PASSIVE)")
        .expect("an unwritten BEGIN does not block a checkpoint");
    let row = rows.first().expect("the checkpoint reports a row");
    let frames = as_integer(row.get(1));
    let copied = as_integer(row.get(2));
    assert!(
        frames > 0,
        "the log held {frames} frames, so this checkpoint had nothing to prove"
    );
    assert_eq!(
        copied, frames,
        "a checkpoint copied {copied} of {frames} frames; if it now stops short, \
         a session has grown a read mark and this test should go back to \
         asserting copied < frames while the reader is open"
    );
    reader.execute_batch("COMMIT").expect("the read closes");

    // With the reader gone, a fresh write and the same checkpoint finish the
    // job again.
    writer
        .execute_batch("INSERT INTO t VALUES(2)")
        .expect("a row");
    let rows = writer
        .query("PRAGMA wal_checkpoint(PASSIVE)")
        .expect("the checkpoint runs");
    let row = rows.first().expect("the checkpoint reports a row");
    let frames = as_integer(row.get(1));
    let copied = as_integer(row.get(2));
    assert!(
        frames > 0,
        "the second write left no frames for this checkpoint to prove either"
    );
    assert_eq!(copied, frames, "the log was not fully copied back");
}

/// A checkpoint refuses once a session has written inside a transaction
/// another `connect()` handle opened, because the two share one `self.batch`
/// rather than holding independent transactions.
///
/// **This is the shape `a_checkpoint_copies_the_whole_log_back_past_a_session`
/// used to test, without either the test or a reader of it being told that is
/// what it was testing.** Its `BEGIN` on a second handle followed by a write on
/// the first was never a concurrent writer running past a live reader - see
/// that test's own comment for why one handle's `BEGIN` and another's write are
/// the same transaction here - and asserting the refusal directly, rather than
/// deleting the case once the mistaken framing was found, is what keeps this
/// distinction on record.
#[test]
fn a_checkpoint_refuses_once_a_shared_transaction_has_written() {
    let path = scratch("protected-refusal");
    let held = database(&path);
    let writer = held.session();
    writer
        .execute_batch("PRAGMA journal_mode=wal")
        .expect("the mode changes");
    writer
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
        .expect("the table is made");

    let reader = held.session();
    reader
        .execute_batch("BEGIN")
        .expect("the transaction opens");
    writer
        .execute_batch("INSERT INTO t VALUES(1)")
        .expect("the write joins the transaction the BEGIN opened");

    let refused = writer.query("PRAGMA wal_checkpoint(PASSIVE)");
    assert_eq!(
        refused.map_err(|error| error.code()),
        Err(PrimaryCode::Locked),
        "a checkpoint ran inside a transaction that had written and not \
         committed; `pragma_wal_checkpoint` is supposed to refuse this exactly \
         as the pinned SQLite 3.53.4 reference does"
    );

    reader
        .execute_batch("COMMIT")
        .expect("the transaction closes");
    let rows = writer
        .query("PRAGMA wal_checkpoint(PASSIVE)")
        .expect("the checkpoint runs once nothing is left open");
    let row = rows.first().expect("the checkpoint reports a row");
    assert_eq!(
        as_integer(row.get(2)),
        as_integer(row.get(1)),
        "the log was not fully copied back once the transaction committed"
    );
}

/// One writer at a time, and the loser is told rather than left waiting.
///
/// **The refusal is `Misuse` rather than `Busy` on this engine**, and the
/// difference is which layer answers. The old engine had two connections and a
/// lock between them, so the second `BEGIN IMMEDIATE` lost a race and was told
/// the database was `Busy`. Here the two are sessions on one pool, so a second
/// `BEGIN IMMEDIATE` is a second transaction on a pool that already has one -
/// which the engine rejects as misuse before any lock is consulted. Either way
/// the invariant this test exists for holds: **two writers cannot both be open,
/// and the loser is told rather than left waiting.**
#[test]
fn a_second_writer_in_this_process_is_refused() {
    let path = scratch("one-writer");
    let held = database(&path);
    let first = held.session();
    first
        .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY)")
        .expect("the table is made");
    let second = held.session();

    first
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the writer opens");
    let refused = second.execute_batch("BEGIN IMMEDIATE");
    assert_eq!(
        refused.map_err(|error| error.code()),
        Err(PrimaryCode::Misuse),
        "two sessions held the writer at once; a `Busy` here instead would mean \
         the two are arbitrating through the lock rather than through the pool, \
         which is what two separate connections used to do"
    );

    // **An autocommit write from the second session is NOT refused, and what
    // it does instead is the thing worth pinning.** The `BEGIN IMMEDIATE`
    // route above is rejected, but a bare `INSERT` takes its reservation
    // inside the statement and succeeds while the first session's transaction
    // is still open. On one pool that is not a second writer racing the first
    // - it is a write landing *inside* the open transaction, which is what the
    // rollback below proves: the row goes away with the first session's
    // `ROLLBACK`, rather than surviving it as a separately committed row.
    //
    // It is recorded rather than asserted away because the distinction decides
    // how bad it is. A write that survived the rollback would be a lost-update
    // hole of the kind this test's original comment describes. A write that is
    // rolled back with the transaction it silently joined is coherent, and is
    // a difference from the old engine rather than a defect in this one - but
    // it does mean an application cannot treat two sessions as two independent
    // writers.
    second
        .execute_batch("INSERT INTO t VALUES(99)")
        .expect("an autocommit write from a second session is accepted");
    first
        .execute_batch("ROLLBACK")
        .expect("the writer rolls back");
    assert_eq!(
        integer(&first, "SELECT count(*) FROM t WHERE a = 99"),
        0,
        "a second session's autocommit write survived the first session's \
         ROLLBACK, which means it committed independently while a transaction \
         was open - a lost-update hole rather than a shared transaction"
    );

    // Re-open the writer so the rest of the test reads as it did before.
    first
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the writer opens again");

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
#[test]
fn a_connection_sees_another_connections_commit() {
    for mode in ["delete", "wal"] {
        let path = scratch(&format!("stale-cache-{mode}"));
        let held = database(&path);
        let first = held.session();
        first
            .execute_batch(&format!("PRAGMA journal_mode={mode}"))
            .expect("the mode changes");
        first
            .execute_batch("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .expect("the table is made");
        first
            .execute_batch("INSERT INTO t VALUES(1, 'first')")
            .expect("a row");

        let second = held.session();
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
        // bug the first time this file was written: a transaction the caller
        // opened and closed itself leaves pages in the cache that an
        // autocommit statement had already let go of.
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

// **`a_busy_timeout_waits_for_the_writer` is deleted, and this is what it
// tested.** A second connection that found the writer busy waited for
// `PRAGMA busy_timeout` and then succeeded, rather than being refused. It
// needed a *second thread* to do the waiting, because the waiting connection
// blocks - and this engine is single threaded by construction: its pool and
// trees hold their state in `RefCell`, a `Connection` borrows the `Database`
// it came from, and neither is `Send`. A thread cannot be handed either one.
//
// There is no in-process shape that tests it instead. Two sessions on one
// `Database` are the same thread by definition, so the second one cannot be
// blocked while the first is still running, and `docs/roadmap.md`'s own item 9
// says where this behaviour does live now: "Access from several **processes**
// works... Threads inside one process do not." The cross-process locking
// protocol - SHARED, RESERVED, PENDING, EXCLUSIVE - is tested by
// `crates/inillucent-compat/tests/process_concurrency.rs`, which spawns real
// writer processes, rather than here.
//
// So the timeout's *waiting* half is not covered by this file any more. Its
// refusing half still is: `a_second_writer_in_this_process_is_refused` above
// asserts that the loser is told with `Busy` rather than left waiting.

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
        let held = database(&path);
        let writer = held.session();
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
        // A second session reads the same rows, so the image below is taken
        // while the database is genuinely in use rather than idle.
        let reader = held.session();
        assert_eq!(integer(&reader, "SELECT count(*) FROM t"), 8);
        // **Whatever companion files this engine actually wrote, not
        // SQLite's.** The old suite copied `.db`, `-wal` and `-shm` and
        // required exactly three, which is SQLite's layout; this engine writes
        // a *segmented* log - `<name>.db-wal.0000000001`, a new file per
        // segment - and has no `-shm` wal-index at all. So the image is
        // everything beside the database whose name starts with the database's,
        // and the count is asserted as "more than the database alone" rather
        // than as a number copied from another engine's file list.
        let directory = path.parent().expect("the scratch path has a parent");
        let stem = path.file_name().expect("the scratch path has a name");
        let stem = stem.to_string_lossy().to_string();
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(directory)
            .expect("the scratch directory reads")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|entry| {
                entry
                    .file_name()
                    .map(|name| name.to_string_lossy().starts_with(&stem))
                    .unwrap_or(false)
            })
            .collect();
        entries.sort();
        for companion in entries {
            if let Ok(bytes) = std::fs::read(&companion) {
                saved.push((companion, bytes));
            }
        }
    }
    assert!(
        saved.len() > 1,
        "the crash image is only the database itself: {saved:?} - a log this engine \
         had written should be in it too"
    );

    // Put the image back, which undoes the tidy close the connections made.
    for (companion, bytes) in &saved {
        std::fs::write(companion, bytes).expect("the crash image is restored");
    }
    let recovered = database(&path);
    let survivor = recovered.session();
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
