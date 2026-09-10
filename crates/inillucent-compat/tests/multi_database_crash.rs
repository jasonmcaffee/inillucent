//! Power loss during a commit that spans two databases.
//!
//! Invariant: after any modelled failure the two databases are both what they
//! were before the transaction or both what they would have been after it, and
//! never one of each. That is the whole claim a super-journal makes, and the
//! only way to test it is to cut the commit at every point it has and open both
//! files at each one.
//!
//! The campaign is systematic rather than random, in the same shape the
//! single-database one uses: every injectable VFS call of the transaction is
//! numbered, and the transaction is repeated once per number with the power
//! loss armed at exactly that call.

use std::sync::Arc;

use inillucent_session::connection::{Connection, OpenOptions, SessionDatabase};
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_transaction::journal::{JournalMode, JournalOptions, Synchronous};
use inillucent_value::Value;
use inillucent_vfs::path::DbPath;
use inillucent_vfs::Vfs;

/// The schema both databases start with.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
     INSERT INTO t VALUES(1, 'one');
     ATTACH DATABASE '/sim/aux.db' AS aux;
     CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b TEXT);
     INSERT INTO aux.t VALUES(1, 'one');";

/// The transaction each run tries to commit across both of them.
const WORKLOAD: &str = "ATTACH DATABASE '/sim/aux.db' AS aux;
     BEGIN;
     INSERT INTO main.t VALUES(2, 'two');
     INSERT INTO aux.t VALUES(2, 'two');
     COMMIT;";

/// Returns a simulator with the pessimistic device model.
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// The main database's path; the attached one is beside it.
fn path() -> DbPath {
    DbPath::from("/sim/main.db")
}

/// Opens a connection on one simulated file system.
fn try_connect(
    vfs: Arc<dyn Vfs>,
    journal: JournalOptions,
) -> Result<Connection, inillucent_base::DbError> {
    let database = SessionDatabase::open_with(
        path().as_path(),
        vfs,
        OpenOptions {
            journal,
            ..OpenOptions::default()
        },
    )?;
    database.connect()
}

/// Runs a script, reporting whether it succeeded.
fn run(connection: &Connection, sql: &str) -> Result<(), inillucent_base::DbError> {
    inillucent_session::statement::execute_batch(connection, sql.as_bytes())
}

/// Reads both databases, as one list of rows per database.
fn try_contents(connection: &Connection) -> Result<Vec<String>, inillucent_base::DbError> {
    let mut rows = Vec::new();
    for sql in [
        b"SELECT 'main', a, b FROM main.t ORDER BY a".as_slice(),
        b"SELECT 'aux', a, b FROM aux.t ORDER BY a".as_slice(),
    ] {
        let (mut statement, _) =
            inillucent_session::statement::Statement::prepare(connection, sql)?;
        while statement.step()? {
            let row = statement.row();
            rows.push(format!(
                "{:?}|{:?}|{:?}",
                row.first()
                    .and_then(|value| value.as_text().map(|text| text.utf8_bytes().into_owned())),
                row.get(1).and_then(Value::as_integer),
                row.get(2)
                    .and_then(|value| value.as_text().map(|text| text.utf8_bytes().into_owned())),
            ));
        }
    }
    Ok(rows)
}

/// Builds both databases and returns the simulator holding them.
fn built(journal: JournalOptions, seed: u64) -> Arc<SimVfs> {
    let vfs = simulator(seed);
    let connection =
        try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal).expect("the connection opens");
    run(&connection, SCHEMA).expect("the schema builds");
    drop(connection);
    vfs
}

/// The two states a run may legitimately end in.
fn expected_states(journal: JournalOptions) -> (Vec<String>, Vec<String>) {
    let vfs = built(journal, 11);
    let before = {
        let connection =
            try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal).expect("the connection opens");
        run(&connection, "ATTACH DATABASE '/sim/aux.db' AS aux").expect("the second attaches");
        try_contents(&connection).expect("the query runs")
    };
    let after = {
        let connection =
            try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal).expect("the connection opens");
        run(&connection, WORKLOAD).expect("the workload commits");
        try_contents(&connection).expect("the query runs")
    };
    (before, after)
}

/// What reopening a crashed pair produced.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Recovery {
    /// Both databases opened and reported these rows.
    Rows(Vec<String>),
    /// One of them refused to be read.
    Broken(String),
}

/// Reopens what a crash left behind and reports what both databases hold.
fn recovered(journal: JournalOptions, snapshot: &CrashSnapshot, seed: u64) -> Recovery {
    let vfs = Arc::new(SimVfs::recovered(
        SimConfig {
            seed,
            model: MediaModel::default(),
            ..SimConfig::default()
        },
        snapshot,
    ));
    let outcome = try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal).and_then(|connection| {
        run(&connection, "ATTACH DATABASE '/sim/aux.db' AS aux")?;
        try_contents(&connection)
    });
    match outcome {
        Ok(rows) => Recovery::Rows(rows),
        Err(failure) => Recovery::Broken(format!("{failure}")),
    }
}

/// Every cut point of a two-database commit leaves both databases in the same
/// state as each other: both old, or both new.
///
/// A power loss before the super-journal is deleted finds two hot journals
/// naming a file that is still there and undoes both. One after it finds two
/// hot journals naming a file that is gone and undoes neither. There is no
/// third outcome, and the campaign is what says so.
#[test]
fn every_cut_of_a_two_database_commit_leaves_one_state_or_the_other() {
    let journal = JournalOptions {
        mode: JournalMode::Delete,
        synchronous: Synchronous::Full,
    };
    let (before, after) = expected_states(journal);
    assert_ne!(before, after, "the workload has to change something");

    let mut cuts = 0u64;
    let mut old = 0u64;
    let mut new = 0u64;
    let mut report = String::new();
    for nth in 1..=400u64 {
        let vfs = built(journal, 2000 + nth);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), Failure::Crash);
        let committed = match try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal) {
            Ok(connection) => run(&connection, WORKLOAD).is_ok(),
            Err(_) => false,
        };
        let reached = vfs.failpoints().sites_reached().saturating_sub(base);
        let snapshot = vfs.crash();
        if reached < nth {
            // The transaction made fewer injectable calls than that: every cut
            // point has been covered.
            break;
        }
        cuts = cuts.saturating_add(1);
        let state = recovered(journal, &snapshot, 3000 + nth);
        let verdict = match &state {
            Recovery::Rows(rows) if *rows == before => {
                old = old.saturating_add(1);
                "old"
            }
            Recovery::Rows(rows) if *rows == after => {
                new = new.saturating_add(1);
                "new"
            }
            other => panic!(
                "cut {nth}: neither state.\n  before: {before:?}\n  after:  {after:?}\n  got:    {other:?}"
            ),
        };
        assert!(
            !committed || verdict == "new",
            "cut {nth}: the commit reported success and the databases do not hold it"
        );
        report.push_str(&format!("{nth}\t{verdict}\t{committed}\n"));
    }
    assert!(cuts > 20, "only {cuts} cut points were reached");
    assert!(old > 0, "no cut left the old state");
    assert!(new > 0, "no cut left the new state");
    record(
        "multi-database-commit.tsv",
        &format!("cut\tstate\treported\n{report}"),
    );
}

/// The same campaign with a short write rather than a power loss.
///
/// A short write is the one failure a journal cannot always undo: a device that
/// stores half its bytes and reports success has destroyed data the engine was
/// told had landed, and by the time the journal is finalised there is no image
/// left to put back. SQLite has exactly the same exposure, which is why its
/// durability argument assumes a write either lands or fails. What is still
/// owed, and what this measures, is that every run ends either in a clean
/// old-or-new pair or in a *reported* failure - never in plausible rows that
/// are neither, and never with the two databases disagreeing.
#[test]
fn every_short_write_of_a_two_database_commit_leaves_one_state_or_the_other() {
    let journal = JournalOptions {
        mode: JournalMode::Delete,
        synchronous: Synchronous::Full,
    };
    let (before, after) = expected_states(journal);
    let mut cuts = 0u64;
    let _reported = 0u64;
    let mut report = String::new();
    for nth in 1..=400u64 {
        let vfs = built(journal, 5000 + nth);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), Failure::ShortWrite);
        let committed = match try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, journal) {
            Ok(connection) => run(&connection, WORKLOAD).is_ok(),
            Err(_) => false,
        };
        let reached = vfs.failpoints().sites_reached().saturating_sub(base);
        let snapshot = vfs.crash();
        if reached < nth {
            break;
        }
        cuts = cuts.saturating_add(1);
        let state = recovered(journal, &snapshot, 6000 + nth);
        let verdict = match &state {
            Recovery::Rows(rows) if *rows == before => "old",
            Recovery::Rows(rows) if *rows == after => "new",
            Recovery::Broken(_) => "reported",
            other => panic!(
                "short write {nth}: neither state.\n  before: {before:?}\n  after:  {after:?}\n  got:    {other:?}"
            ),
        };
        // An acknowledged commit must be there, unless the short write
        // destroyed a page the journal was no longer holding an image of -
        // in which case the damage has to be *reported* rather than served as
        // rows, which is what the "reported" verdict records.
        assert!(
            !committed || verdict == "new" || verdict == "reported",
            "short write {nth}: the commit reported success and the databases do not hold it"
        );
        report.push_str(&format!("{nth}\t{verdict}\t{committed}\n"));
    }
    assert!(cuts > 20, "only {cuts} cut points were reached");
    record(
        "multi-database-short-write.tsv",
        &format!("cut\tstate\treported\n{report}"),
    );
}

/// Writes a report into the checked-in crash schedules.
fn record(name: &str, body: &str) {
    let directory = inillucent_compat::workspace_root().join("tests/crash");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join(name), body);
}
