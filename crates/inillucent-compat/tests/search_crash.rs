//! Power loss while a transaction is changing rows *and* a search index.
//!
//! Invariant: after a modelled power loss the relational rows and the search
//! entries are both from before the transaction or both from after it, and
//! never a mixture. That is the acceptance criterion of phase 13. A database
//! that came back holding the new row but ranking the old corpus would be the
//! worst kind of failure - nothing about it looks wrong, and it is only visible
//! to somebody who runs the right query.
//!
//! The state a run compares is deliberately *joint*: the ordinary table's rows,
//! the search table's rows, and the ranking a query returns, all in one string.
//! A mixture is therefore neither the before state nor the after state and
//! fails the run rather than being quietly classified as one of them.
//!
//! The method is the one `wal_crash.rs` established: every injectable call is
//! numbered, and the run is repeated once per number with the failure armed at
//! exactly that call.

use std::sync::Arc;

use inillucent_session::connection::{Connection, OpenOptions, SessionDatabase};
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_transaction::journal::{JournalMode, JournalOptions, Synchronous};
use inillucent_value::Value;
use inillucent_vfs::path::DbPath;
use inillucent_vfs::Vfs;

/// The schema every run starts from.
///
/// `compact = 0` keeps the automatic fold out of the ordinary campaigns, so
/// they measure the commit rather than the compaction. Compaction gets its own
/// campaign below, where it is the thing being cut.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
     CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 0);
     INSERT INTO t VALUES(1, 'one'), (2, 'two');
     INSERT INTO docs(rowid, title, body) VALUES (1, 'Offer', 'who qualifies for the discount');
     INSERT INTO docs(rowid, title, body) VALUES (2, 'Rules', 'the discount applies to accounts');
     INSERT INTO docs(rowid, title, body) VALUES (3, 'Weather', 'rain and wind tomorrow');";

/// The transaction each run tries to commit on top of it.
///
/// It touches both worlds in one transaction, which is the whole point: an
/// ordinary row, a new search row, an edited search row and a deleted one.
const WORKLOAD: &str = "BEGIN;
     INSERT INTO t VALUES(3, 'three');
     INSERT INTO docs(rowid, title, body) VALUES (4, 'Trial', 'tirzepatide dosing schedule');
     UPDATE docs SET body = 'the forecast is sunshine' WHERE rowid = 3;
     DELETE FROM docs WHERE rowid = 1;
     COMMIT;";

/// Returns a simulator with the pessimistic device model.
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// The path every run uses.
fn path() -> DbPath {
    DbPath::from("/sim/search.db")
}

/// Opens a connection in one journal mode, reporting failure rather than panicking.
fn try_connect(
    vfs: Arc<dyn Vfs>,
    mode: JournalMode,
) -> Result<Connection, inillucent_base::DbError> {
    let database = SessionDatabase::open_with(
        path().as_path(),
        vfs,
        OpenOptions {
            journal: JournalOptions {
                mode,
                synchronous: Synchronous::Full,
            },
            ..OpenOptions::default()
        },
    )?;
    database.connect()
}

/// Runs a script, reporting whether it succeeded.
fn run(connection: &Connection, sql: &str) -> Result<(), inillucent_base::DbError> {
    inillucent_session::statement::execute_batch(connection, sql.as_bytes())
}

/// Returns one query's rows, each rendered as text.
fn query(connection: &Connection, sql: &str) -> Result<Vec<String>, inillucent_base::DbError> {
    let (mut statement, _) =
        inillucent_session::statement::Statement::prepare(connection, sql.as_bytes())?;
    let mut rows = Vec::new();
    while statement.step()? {
        let width = statement.column_count();
        let mut parts = Vec::with_capacity(width);
        for index in 0..width {
            parts.push(match statement.value(index) {
                Value::Null => "NULL".to_string(),
                Value::Integer(number) => number.to_string(),
                Value::Real(number) => format!("{number:.4}"),
                Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
                Value::Blob(blob) => format!("blob:{}", blob.raw().len()),
            });
        }
        rows.push(parts.join("|"));
    }
    Ok(rows)
}

/// The joint state: ordinary rows, search rows, and what a search returns.
///
/// One string, on purpose. Splitting them would let a mixture match one half of
/// a legitimate state and be classified as it.
fn try_state(connection: &Connection) -> Result<Vec<String>, inillucent_base::DbError> {
    let mut state = Vec::new();
    for row in query(connection, "SELECT a, b FROM t ORDER BY a")? {
        state.push(format!("t:{row}"));
    }
    for row in query(
        connection,
        "SELECT rowid, title, body FROM docs ORDER BY rowid",
    )? {
        state.push(format!("row:{row}"));
    }
    for probe in ["discount", "tirzepatide", "sunshine", "rain"] {
        let found = query(
            connection,
            &format!("SELECT rowid FROM docs WHERE docs MATCH '{probe}' AND k = 10 ORDER BY rowid"),
        )?;
        state.push(format!("find {probe}:{}", found.join(",")));
    }
    Ok(state)
}

/// Builds the database and returns the simulator holding it.
fn built(seed: u64, mode: JournalMode) -> Arc<SimVfs> {
    let vfs = simulator(seed);
    let connection =
        try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
    if mode == JournalMode::Wal {
        run(&connection, "PRAGMA journal_mode=wal;").expect("wal mode is entered");
    }
    run(&connection, SCHEMA).expect("the schema builds");
    drop(connection);
    vfs
}

/// What reopening a crashed database produced.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Recovery {
    /// It opened and reported this state.
    Rows(Vec<String>),
    /// It refused to be read, naming the damage.
    Broken(String),
}

/// Reopens what a crash left behind.
fn recovered(snapshot: &CrashSnapshot, seed: u64, mode: JournalMode) -> Recovery {
    let vfs = Arc::new(SimVfs::recovered(
        SimConfig {
            seed,
            model: MediaModel::default(),
            ..SimConfig::default()
        },
        snapshot,
    ));
    match try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, mode)
        .and_then(|connection| try_state(&connection))
    {
        Ok(rows) => Recovery::Rows(rows),
        Err(failure) => Recovery::Broken(format!("{failure}")),
    }
}

/// The two states a run may legitimately end in.
fn expected_states(mode: JournalMode) -> (Vec<String>, Vec<String>) {
    let vfs = built(4242, mode);
    let before = {
        let connection =
            try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
        try_state(&connection).expect("the query runs")
    };
    let after = {
        let connection =
            try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
        run(&connection, WORKLOAD).expect("the workload commits");
        try_state(&connection).expect("the query runs")
    };
    (before, after)
}

/// Runs one campaign and returns its report.
/// The statements a run executes after its transaction.
///
/// Without them there is no injectable call *after* the commit marker, so no
/// cut can land there and the campaign would never observe the committed state
/// - which would make it a test that only ever proves the transaction can be
/// abandoned. `wal_crash.rs` uses the same device for the same reason.
const TAIL: &str = "SELECT count(*) FROM t; SELECT count(*) FROM docs_content;";

fn campaign(name: &str, mode: JournalMode, failure: Failure, cuts_wanted: u64) -> String {
    let (before, after) = expected_states(mode);
    assert_ne!(before, after, "the workload has to change something");
    let corruption_allowed = !matches!(failure, Failure::Crash);
    let silent = matches!(failure, Failure::ShortWrite);
    let mut report = String::new();
    let mut cuts = 0u64;
    let mut old = 0u64;
    let mut new = 0u64;
    let mut detected = 0u64;
    for nth in 1..=cuts_wanted {
        let vfs = built(7000 + nth, mode);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), failure);
        let connection = try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, mode);
        let committed = match &connection {
            Ok(connection) => run(connection, WORKLOAD).is_ok() && run(connection, TAIL).is_ok(),
            Err(_) => false,
        };
        let reached = vfs.failpoints().sites_reached().saturating_sub(base);
        let snapshot = vfs.crash();
        drop(connection);
        if reached < nth {
            break;
        }
        cuts = cuts.saturating_add(1);
        let state = recovered(&snapshot, 8000 + nth, mode);
        let verdict = match &state {
            Recovery::Rows(rows) if *rows == before => {
                old = old.saturating_add(1);
                "old"
            }
            Recovery::Rows(rows) if *rows == after => {
                new = new.saturating_add(1);
                "new"
            }
            Recovery::Broken(detail) => {
                assert!(
                    corruption_allowed,
                    "{name} cut {nth}: the database came back unreadable: {detail}"
                );
                detected = detected.saturating_add(1);
                "reported"
            }
            Recovery::Rows(rows) => {
                let mixture: Vec<&String> = rows
                    .iter()
                    .filter(|line| !before.contains(line) || !after.contains(line))
                    .collect();
                panic!(
                    "{name} cut {nth}: the rows and the index disagree.\n  before: {before:?}\n  after:  {after:?}\n  got:    {rows:?}\n  differs: {mixture:?}"
                )
            }
        };
        assert!(
            !committed
                || verdict == "new"
                || silent
                || (corruption_allowed && verdict == "reported"),
            "{name} cut {nth}: the commit reported success and the database does not hold it"
        );
        report.push_str(&format!("{nth}\t{verdict}\t{committed}\n"));
    }
    assert!(cuts > 20, "{name}: only {cuts} cut points were reached");
    assert!(old > 0, "{name}: no cut left the old state");
    assert!(new > 0, "{name}: no cut left the new state");
    format!(
        "# {name}: {cuts} cuts, {old} old, {new} new, {detected} reported\ncut\tstate\treported\n{report}"
    )
}

/// Writes one campaign's report beside the others.
fn record(name: &str, report: &str) {
    let directory = inillucent_compat::workspace_root().join("_agent_output/search-crash");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join(format!("{name}.tsv")), report);
}

/// Power loss anywhere in a rollback-journal commit leaves one state or the
/// other, in the rows *and* in the ranking.
#[test]
fn a_rollback_journal_commit_is_atomic_across_both() {
    let report = campaign("journal-crash", JournalMode::Delete, Failure::Crash, 4000);
    record("journal-crash", &report);
}

/// The same, through a write-ahead log.
#[test]
fn a_wal_commit_is_atomic_across_both() {
    let report = campaign("wal-crash", JournalMode::Wal, Failure::Crash, 4000);
    record("wal-crash", &report);
}

/// A device that fails a write and says so never produces a mixture either.
#[test]
fn a_reported_write_failure_never_produces_a_mixture() {
    let report = campaign("journal-io", JournalMode::Delete, Failure::IoError, 4000);
    record("journal-io", &report);
}

/// Power loss during a compaction leaves the index the compaction started from.
///
/// Compaction writes a whole new generation, moves the state rows to name it,
/// and removes the folded log entries - all inside the caller's transaction. A
/// crash part way through has to leave the *old* generation named and the log
/// intact, which is the same index and answers the same queries.
#[test]
fn a_crash_during_compaction_leaves_the_index_it_started_from() {
    let mode = JournalMode::Delete;
    // A separate workload, because the thing being cut is the compaction rather
    // than the ordinary commit.
    let schema = "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 0);
         INSERT INTO docs(rowid, title, body) VALUES (1, 'Offer', 'who qualifies for the discount');
         INSERT INTO docs(rowid, title, body) VALUES (2, 'Rules', 'the discount applies here');
         INSERT INTO docs(rowid, title, body) VALUES (3, 'Weather', 'rain and wind tomorrow');";
    let compaction = "INSERT INTO docs(docs) VALUES ('compact')";

    let build = |seed: u64| -> Arc<SimVfs> {
        let vfs = simulator(seed);
        let connection =
            try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
        run(&connection, schema).expect("the schema builds");
        drop(connection);
        vfs
    };
    let probe = |connection: &Connection| -> Result<Vec<String>, inillucent_base::DbError> {
        let mut state = query(
            connection,
            "SELECT rowid FROM docs WHERE docs MATCH 'discount' AND k = 10 ORDER BY rowid",
        )?;
        state.extend(query(
            connection,
            "SELECT rowid, body FROM docs ORDER BY rowid",
        )?);
        Ok(state)
    };

    let reference = {
        let vfs = build(1234);
        let connection =
            try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
        let before = probe(&connection).expect("the query runs");
        run(&connection, compaction).expect("the compaction runs");
        let after = probe(&connection).expect("the query runs");
        assert_eq!(before, after, "compaction changes no answer");
        before
    };

    let mut cuts = 0u64;
    let mut report = String::new();
    for nth in 1..=200u64 {
        let vfs = build(9000 + nth);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), Failure::Crash);
        let connection = try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, mode);
        if let Ok(connection) = &connection {
            let _ = run(connection, compaction);
        }
        let reached = vfs.failpoints().sites_reached().saturating_sub(base);
        let snapshot = vfs.crash();
        drop(connection);
        if reached < nth {
            break;
        }
        cuts = cuts.saturating_add(1);
        let vfs = Arc::new(SimVfs::recovered(
            SimConfig {
                seed: 9500 + nth,
                model: MediaModel::default(),
                ..SimConfig::default()
            },
            &snapshot,
        ));
        let state = try_connect(Arc::clone(&vfs) as Arc<dyn Vfs>, mode)
            .and_then(|connection| probe(&connection));
        match state {
            Ok(rows) => assert_eq!(
                rows, reference,
                "compaction cut {nth}: the index answers differently"
            ),
            Err(failure) => panic!("compaction cut {nth}: the database will not open: {failure}"),
        }
        report.push_str(&format!("{nth}\tsame\n"));
    }
    assert!(cuts > 20, "only {cuts} cut points were reached");
    record(
        "compaction-crash",
        &format!(
            "# compaction: {cuts} cuts, every one answering identically\ncut\tstate\n{report}"
        ),
    );
}
