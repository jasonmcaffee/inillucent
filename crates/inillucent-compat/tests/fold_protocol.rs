//! What a statement pays, and what a fold pays, counted at the file system.
//!
//! Invariant: **an autocommit statement is one append and one sync of the log,
//! and nothing touches the data file until a fold is due.** That is design 1 of
//! task-2000, and it is the sentence the `transaction`, `write` and `schema`
//! families of the performance contract turn on: before it, a statement's commit
//! was a checkpoint with a rollback journal protecting the checkpoint's in place
//! page writes, and that cost six to eight fsync class calls a statement -
//! `txn.autocommit` at 8.7 ms against SQLite's 1.17.
//!
//! ## Why the file system and not the engine's own counters
//!
//! `ImportedDatabase::pool_stats` reports `file_syncs` and `folds`, and the gate
//! prints both. They are the right instrument for a benchmark and the wrong one
//! for this file: a counter can only count the calls whoever wrote it thought of,
//! and the claim here is about *every* call. `SimVfs` records one trace event per
//! read, write and sync with the path it acted on, so counting its events counts
//! what the operating system would have seen. A sync taken by a path nobody
//! remembered is in the trace and would not be in a counter.
//!
//! ## What the three files are
//!
//! A run touches up to three paths and they have to be told apart, because the
//! whole claim is about which one is synced. The database is `/sim/fold.db`, its
//! log segments are `/sim/fold.db-wal.<sequence>`, and a rollback journal, if one
//! is ever created, is `/sim/fold.db-journal`. `is_log` and `is_database` below
//! are that split; the journal is asserted absent rather than counted.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_vfs::Vfs;

/// The page size these runs build at, matching `inillucent_engine::connect::PAGE_SIZE`.
const PAGE_SIZE: usize = 32_768;

/// How many frames the pool holds.
///
/// Large enough that nothing evicts during a run this small, because an eviction
/// writes a page to the data file and would be counted as a fold's write.
const FRAMES: usize = 4_096;

/// The schema every run starts from.
///
/// `journal_mode = wal` and `synchronous = full` are the shipped defaults and are
/// set explicitly, because this file's whole subject is what those two mean.
const SCHEMA: &str = "PRAGMA journal_mode=wal;
     PRAGMA synchronous=full;
     CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER);
     CREATE INDEX t_b ON t(b);
     INSERT INTO t VALUES(1, 'one', 10);
     INSERT INTO t VALUES(2, 'two', 20);
     INSERT INTO t VALUES(3, 'three', 30);";

/// The path every run uses.
fn path() -> PathBuf {
    PathBuf::from("/sim/fold.db")
}

/// Returns a simulator with the pessimistic device model.
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// Runs a script of one or more statements, stopping at the first failure.
///
/// @param engine - the connection
/// @param sql - one or more statements
fn run(engine: &mut ImportedDatabase, sql: &str) {
    let mut rest = sql;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return;
        }
        let consumed = engine
            .statement_length(trimmed)
            .expect("the statement is well formed");
        let Some(head) = trimmed.get(..consumed) else {
            return;
        };
        if head.trim().is_empty() {
            return;
        }
        engine
            .execute_any(head, &Params::new())
            .unwrap_or_else(|why| panic!("{head}: {why}"));
        rest = trimmed.get(consumed..).unwrap_or("");
    }
}

/// Reports whether a traced path is one of the log's segments.
///
/// @param traced - the path a trace event named
fn is_log(traced: &str) -> bool {
    traced.contains("-wal.")
}

/// Reports whether a traced path is the database file itself.
///
/// @param traced - the path a trace event named
fn is_database(traced: &str) -> bool {
    traced.contains("fold.db") && !is_log(traced) && !traced.contains("-journal")
}

/// What one span of a run asked of the file system.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Asked {
    /// Syncs of the database file.
    file_syncs: u64,
    /// Syncs of a log segment.
    log_syncs: u64,
    /// Writes to the database file.
    file_writes: u64,
    /// Bytes written to a log segment.
    ///
    /// **How an after image per page is counted without reading the log.** A
    /// `WritePage` record carries a whole page, so a fold of N dirty pages cannot
    /// append less than N page sizes. Reading the records themselves would mean
    /// opening a second connection to learn the log's uuid, and opening one runs
    /// a recovery - which is a second thing happening inside the measurement.
    log_bytes: u64,
    /// Whether any path holding `-journal` was touched at all.
    journal_touched: bool,
    /// Every distinct offset written in the database file.
    ///
    /// **What "writes each dirty page once" is asserted against.** Counting writes
    /// alone cannot say it: a fold of two hundred pages and a fold of a hundred
    /// pages written twice are both two hundred writes. Comparing the write count
    /// with the number of distinct offsets is the claim itself, and it is robust to
    /// the free map's own pages, which `log_free_map_pages` dirties inside the fold
    /// and which a count taken before it cannot know about.
    offsets: std::collections::BTreeSet<u64>,
}

/// Counts what the trace recorded from `from` onwards.
///
/// The trace only grows, so a span is the events past a remembered length -
/// which is how a test measures one statement inside a run that had to build a
/// schema first.
///
/// @param vfs - the simulator holding the trace
/// @param from - how many events the trace held when the span began
fn asked_since(vfs: &SimVfs, from: usize) -> Asked {
    let mut asked = Asked::default();
    for event in vfs.trace().events().into_iter().skip(from) {
        // **A write or a sync, not any mention of the path.** `Pool::checkpoint`
        // seals and disposes of the journal unconditionally, and both are no-ops
        // when there is none - but the disposal still asks the file system to
        // delete a file that was never created, which is a trace event on the
        // journal's path. What the claim is about is bytes: a rollback journal that
        // is never written and never synced is a rollback journal the fold did not
        // use, whatever the disposal asked about.
        if event.path.contains("-journal") && matches!(event.kind, "write" | "sync") {
            asked.journal_touched = true;
        }
        match (event.kind, is_database(&event.path), is_log(&event.path)) {
            ("sync", true, _) => asked.file_syncs = asked.file_syncs.saturating_add(1),
            ("sync", _, true) => asked.log_syncs = asked.log_syncs.saturating_add(1),
            ("write", true, _) => {
                asked.file_writes = asked.file_writes.saturating_add(1);
                asked.offsets.insert(event.offset);
            }
            ("write", _, true) => asked.log_bytes = asked.log_bytes.saturating_add(event.length),
            _ => {}
        }
    }
    asked
}

/// An autocommit statement syncs the log once and does not touch the data file.
///
/// The first of task-2000's testing strategy. The schema is built and folded
/// first, so the span measured is one statement on a settled database with far
/// less than `RECLAIM_BYTES` of log behind it - which is the state an application
/// is in for a hundred and nineteen statements out of every hundred and twenty.
#[test]
fn an_autocommit_statement_syncs_once() {
    let vfs = simulator(9_001);
    let mut engine =
        ImportedDatabase::create_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .expect("the connection opens");
    run(&mut engine, SCHEMA);
    engine.checkpoint().expect("the fold runs");
    let from = vfs.trace().events().len();
    run(&mut engine, "INSERT INTO t VALUES(4, 'four', 40);");
    let asked = asked_since(&vfs, from);
    assert_eq!(
        asked.log_syncs, 1,
        "an autocommit statement is one sync of the log, and this one took {}",
        asked.log_syncs
    );
    assert_eq!(
        asked.file_syncs, 0,
        "nothing syncs the data file until a fold is due, and this took {}",
        asked.file_syncs
    );
    assert_eq!(
        asked.file_writes, 0,
        "nothing writes the data file until a fold is due, and this wrote {} page(s)",
        asked.file_writes
    );
    assert!(
        !asked.journal_touched,
        "a rollback journal is not created in wal mode, and one was touched"
    );
    assert!(
        engine.dirty_pages() > 0,
        "the statement's pages stay in the pool until a fold, and none were dirty"
    );
}

/// A fold syncs the data file twice, writes each dirty page once, and has an
/// after image of every one of them in the log first.
///
/// The second of task-2000's testing strategy, and the three halves of design 1a
/// in one assertion: the images are what make an interrupted in place write
/// repairable, the two syncs are the durability order - pages, then the record
/// that says the pages are there - and one write a page is the change design 2
/// depends on not having to undo.
#[test]
fn a_fold_syncs_twice_and_writes_each_page_once() {
    let vfs = simulator(9_002);
    let mut engine =
        ImportedDatabase::create_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .expect("the connection opens");
    run(&mut engine, SCHEMA);
    engine.checkpoint().expect("the fold runs");
    run(&mut engine, "INSERT INTO t VALUES(4, 'four', 40);");
    run(&mut engine, "UPDATE t SET c = c + 1 WHERE a <= 2;");
    let dirty = engine.dirty_pages();
    assert!(dirty > 0, "two statements dirtied nothing");
    let from = vfs.trace().events().len();
    let first_data_write = {
        engine.checkpoint().expect("the fold runs");
        vfs.trace()
            .events()
            .into_iter()
            .skip(from)
            .position(|event| event.kind == "write" && is_database(&event.path))
    };
    let asked = asked_since(&vfs, from);
    assert_eq!(
        asked.file_syncs, 2,
        "a fold syncs the data file twice - once behind the pages, once behind \
         the meta record - and this took {}",
        asked.file_syncs
    );
    assert!(
        !asked.journal_touched,
        "a fold in wal mode takes no rollback journal, and one was touched"
    );
    // **Every page once.** See `Asked::offsets` for why this is the comparison
    // rather than a count against `dirty`: the fold also writes the free map's own
    // pages, which `log_free_map_pages` dirties inside it, so the number of pages a
    // fold writes is not knowable from outside it - but that it writes none of them
    // twice is, and that is the claim design 2 rests on.
    assert_eq!(
        asked.file_writes,
        asked.offsets.len() as u64,
        "a fold wrote {} page(s) to {} distinct offset(s), so it wrote one twice",
        asked.file_writes,
        asked.offsets.len()
    );
    // The dirty pages and the two meta slots at least. Both slots take the same
    // image - see `Pool::checkpoint` for why writing to only one of them is wrong
    // with two processes - and the second is what the second sync covers.
    assert!(
        asked.file_writes >= dirty as u64 + 2,
        "a fold of {dirty} dirty page(s) owes at least {} writes and made {}",
        dirty + 2,
        asked.file_writes
    );
    // **Every image is in the log before the first page moves.** The position of
    // the first write to the data file inside the span is what says so: every log
    // write in the span before it is the images' own append and its sync.
    let first = first_data_write.expect("the fold wrote the data file");
    let log_writes_first = vfs
        .trace()
        .events()
        .into_iter()
        .skip(from)
        .take(first)
        .filter(|event| event.kind == "write" && is_log(&event.path))
        .count();
    assert!(
        log_writes_first > 0,
        "the fold wrote the data file before it appended anything to the log"
    );
    assert_eq!(engine.dirty_pages(), 0, "the fold left dirty pages behind");
    // **An after image per dirty page, counted in bytes.** A `WritePage` record
    // carries a whole page, so a fold that appended one per dirty page cannot have
    // written less than that many page sizes into the log. See `Asked::log_bytes`.
    let owed = (dirty as u64).saturating_mul(PAGE_SIZE as u64);
    assert!(
        asked.log_bytes >= owed,
        "a fold of {dirty} dirty page(s) owes the log at least {owed} bytes of          after images and appended {}",
        asked.log_bytes
    );
}

/// A closed file is self contained: its log holds nothing a reopen has to apply.
///
/// The sixth of task-2000's testing strategy. With the fold lazy, this is the
/// property `inillucent backup` and anybody copying an `.rdb` rely on, and it is
/// `Drop for ImportedDatabase` that keeps it - see that impl for why it is best
/// effort and why that is honest.
#[test]
fn a_closed_file_is_self_contained() {
    let vfs = simulator(9_003);
    {
        let mut engine = ImportedDatabase::create_on(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            path(),
            PAGE_SIZE,
            FRAMES,
        )
        .expect("the connection opens");
        run(&mut engine, SCHEMA);
        run(&mut engine, "INSERT INTO t VALUES(4, 'four', 40);");
        assert!(
            engine.dirty_pages() > 0,
            "the statement folded on its way out, so this test measures nothing"
        );
    }
    let mut reopened =
        ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .expect("the connection opens");
    assert_eq!(
        reopened.dirty_pages(),
        0,
        "the reopen had to replay records into pages, so the close did not fold"
    );
    let rows = reopened
        .execute_any("SELECT count(*) FROM t", &Params::new())
        .expect("the query runs");
    assert_eq!(
        format!("{:?}", rows.rows),
        format!(
            "{:?}",
            vec![vec![inillucent_tree::datum::OwnedDatum::Int(4)]]
        ),
        "the row the last statement inserted is not in the reopened file"
    );
}
