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

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::Vfs;

/// The page size these runs build at, matching `inillucent_engine::connect::PAGE_SIZE`.
const PAGE_SIZE: usize = 32_768;

/// How many frames the pool holds.
const FRAMES: usize = 4_096;

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
fn path() -> PathBuf {
    PathBuf::from("/sim/search.db")
}

/// Creates the database on a simulator with nothing written to it yet, in one
/// journal mode.
///
/// `mode` is asserted through `PRAGMA journal_mode`, the one place either
/// engine lets an application choose it - `ImportedDatabase::create_on` takes
/// no journal configuration of its own.
fn create_fresh(
    vfs: Arc<dyn Vfs>,
    mode: &str,
) -> Result<ImportedDatabase, inillucent_base::DbError> {
    let mut engine = ImportedDatabase::create_on(vfs, path(), PAGE_SIZE, FRAMES)?;
    run(&mut engine, &format!("PRAGMA journal_mode={mode}"))?;
    Ok(engine)
}

/// Reopens a database a prior connection already built, in one journal mode,
/// reporting the failure rather than panicking - a crash is exactly the case
/// where this refuses.
fn reopen(vfs: Arc<dyn Vfs>, mode: &str) -> Result<ImportedDatabase, inillucent_base::DbError> {
    let mut engine = ImportedDatabase::open_on(vfs, path(), PAGE_SIZE, FRAMES)?;
    run(&mut engine, &format!("PRAGMA journal_mode={mode}"))?;
    Ok(engine)
}

/// Runs a script of one or more statements, stopping at the first failure.
fn run(engine: &mut ImportedDatabase, sql: &str) -> Result<(), inillucent_base::DbError> {
    let mut rest = sql;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return Ok(());
        }
        let consumed = engine.statement_length(trimmed)?;
        let Some(head) = trimmed.get(..consumed) else {
            return Ok(());
        };
        if head.trim().is_empty() {
            return Ok(());
        }
        engine.execute_any(head, &Params::new())?;
        rest = trimmed.get(consumed..).unwrap_or("");
    }
}

/// Returns one query's rows, each rendered as text.
fn query(
    engine: &mut ImportedDatabase,
    sql: &str,
) -> Result<Vec<String>, inillucent_base::DbError> {
    let outcome = engine.execute_any(sql, &Params::new())?;
    let mut rows = Vec::new();
    for row in outcome.rows {
        let parts: Vec<String> = row
            .iter()
            .map(|value| match value {
                OwnedDatum::Null => "NULL".to_string(),
                OwnedDatum::Int(number) => number.to_string(),
                OwnedDatum::Real(number) => format!("{number:.4}"),
                OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                OwnedDatum::Blob(bytes) => format!("blob:{}", bytes.len()),
            })
            .collect();
        rows.push(parts.join("|"));
    }
    Ok(rows)
}

/// The joint state: ordinary rows, search rows, and what a search returns.
///
/// One string, on purpose. Splitting them would let a mixture match one half of
/// a legitimate state and be classified as it.
fn try_state(engine: &mut ImportedDatabase) -> Result<Vec<String>, inillucent_base::DbError> {
    let mut state = Vec::new();
    for row in query(engine, "SELECT a, b FROM t ORDER BY a")? {
        state.push(format!("t:{row}"));
    }
    for row in query(engine, "SELECT rowid, title, body FROM docs ORDER BY rowid")? {
        state.push(format!("row:{row}"));
    }
    for probe in ["discount", "tirzepatide", "sunshine", "rain"] {
        let found = query(
            engine,
            &format!("SELECT rowid FROM docs WHERE docs MATCH '{probe}' AND k = 10 ORDER BY rowid"),
        )?;
        state.push(format!("find {probe}:{}", found.join(",")));
    }
    Ok(state)
}

/// Builds the database and returns the simulator holding it.
fn built(seed: u64, mode: &str) -> Arc<SimVfs> {
    let vfs = simulator(seed);
    let mut engine =
        create_fresh(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
    run(&mut engine, SCHEMA).expect("the schema builds");
    drop(engine);
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
fn recovered(snapshot: &CrashSnapshot, seed: u64, mode: &str) -> Recovery {
    let vfs = Arc::new(SimVfs::recovered(
        SimConfig {
            seed,
            model: MediaModel::default(),
            ..SimConfig::default()
        },
        snapshot,
    ));
    match reopen(Arc::clone(&vfs) as Arc<dyn Vfs>, mode)
        .and_then(|mut engine| try_state(&mut engine))
    {
        Ok(rows) => Recovery::Rows(rows),
        Err(failure) => Recovery::Broken(format!("{failure}")),
    }
}

/// The two states a run may legitimately end in.
fn expected_states(mode: &str) -> (Vec<String>, Vec<String>) {
    let vfs = built(4242, mode);
    let before = {
        let mut engine =
            reopen(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
        try_state(&mut engine).expect("the query runs")
    };
    let after = {
        let mut engine =
            reopen(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
        run(&mut engine, WORKLOAD).expect("the workload commits");
        try_state(&mut engine).expect("the query runs")
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
///
/// **The checkpoint is what makes this campaign reach the rollback journal at
/// all, and it was not here.** The two counts are served out of the buffer
/// pool, so in `delete` mode they made no VFS call whatever and every cut the
/// campaign covered fell inside the commit's own log writes. A commit that
/// only reaches the log has not touched a rollback journal: the journal holds
/// pre-images while a *checkpoint* moves pages out of the log and into the
/// data file, and nowhere else. So `a_rollback_journal_commit_is_atomic_across_both`
/// was, in fact, covering the log. It went from 42 cut points with 6 reaching
/// the committed state to 22 with none as this ticket's other work changed how
/// many calls a commit makes, and the campaign failed on `new > 0` - which was
/// the first thing that had ever made the gap visible.
///
/// `PRAGMA wal_checkpoint` checkpoints under a rollback journal as well as
/// under a log, answering `0|-1|-1` the way SQLite does when there is no log
/// to count, so it is the statement that puts the journal on the path of every
/// cut after the commit. `durability.rs` had the same gap and closed it the
/// same way.
const TAIL: &str =
    "SELECT count(*) FROM t; SELECT count(*) FROM docs_content; PRAGMA wal_checkpoint;";

fn campaign(name: &str, mode: &str, failure: Failure, cuts_wanted: u64) -> String {
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
        let mut connection = reopen(Arc::clone(&vfs) as Arc<dyn Vfs>, mode);
        let committed = match &mut connection {
            Ok(engine) => run(engine, WORKLOAD).is_ok() && run(engine, TAIL).is_ok(),
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
    // The counts go in the message because what goes wrong here is almost
    // never "the engine answered a third state". It is that the cuts stopped
    // reaching as far into the run as they used to - a campaign that covered
    // the commit now stops before it, and every cut then honestly reports the
    // old database. Without the numbers those two read identically.
    assert!(
        old > 0,
        "{name}: no cut left the old state ({cuts} cuts, {old} old, {new} new, {detected} reported)"
    );
    assert!(
        new > 0,
        "{name}: no cut left the new state ({cuts} cuts, {old} old, {new} new, {detected} reported)"
    );
    format!(
        "# {name}: {cuts} cuts, {old} old, {new} new, {detected} damaged and detected\ncut\tstate\tcommitted\n{report}"
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
    let report = campaign("journal-crash", "delete", Failure::Crash, 4000);
    record("journal-crash", &report);
}

/// The same, through a write-ahead log.
#[test]
fn a_wal_commit_is_atomic_across_both() {
    let report = campaign("wal-crash", "wal", Failure::Crash, 4000);
    record("wal-crash", &report);
}

/// A device that fails a write and says so never produces a mixture either.
#[test]
fn a_reported_write_failure_never_produces_a_mixture() {
    let report = campaign("journal-io", "delete", Failure::IoError, 4000);
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
    let mode = "delete";
    // A separate workload, because the thing being cut is the compaction rather
    // than the ordinary commit.
    let schema = "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 0);
         INSERT INTO docs(rowid, title, body) VALUES (1, 'Offer', 'who qualifies for the discount');
         INSERT INTO docs(rowid, title, body) VALUES (2, 'Rules', 'the discount applies here');
         INSERT INTO docs(rowid, title, body) VALUES (3, 'Weather', 'rain and wind tomorrow');";
    let compaction = "INSERT INTO docs(docs) VALUES ('compact')";

    let build = |seed: u64| -> Arc<SimVfs> {
        let vfs = simulator(seed);
        let mut engine =
            create_fresh(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
        run(&mut engine, schema).expect("the schema builds");
        drop(engine);
        vfs
    };
    let probe = |engine: &mut ImportedDatabase| -> Result<Vec<String>, inillucent_base::DbError> {
        let mut state = query(
            engine,
            "SELECT rowid FROM docs WHERE docs MATCH 'discount' AND k = 10 ORDER BY rowid",
        )?;
        state.extend(query(
            engine,
            "SELECT rowid, body FROM docs ORDER BY rowid",
        )?);
        Ok(state)
    };

    let reference = {
        let vfs = build(1234);
        let mut engine =
            reopen(Arc::clone(&vfs) as Arc<dyn Vfs>, mode).expect("the connection opens");
        let before = probe(&mut engine).expect("the query runs");
        run(&mut engine, compaction).expect("the compaction runs");
        let after = probe(&mut engine).expect("the query runs");
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
        let mut connection = reopen(Arc::clone(&vfs) as Arc<dyn Vfs>, mode);
        if let Ok(engine) = &mut connection {
            let _ = run(engine, compaction);
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
        let state = reopen(Arc::clone(&vfs) as Arc<dyn Vfs>, mode)
            .and_then(|mut engine| probe(&mut engine));
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

/// The schema a fresh build starts from: the search table, empty.
///
/// Empty rather than absent, so that the state before the transaction is a
/// state the probes below can read. A campaign whose "before" is a table that
/// does not exist has nothing to compare against.
const FRESH_SCHEMA: &str =
    "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 0);";

/// A whole corpus and the index over it, in one transaction.
///
/// **The build, not an update.** Every campaign above cuts a transaction that
/// changes an index that is already there; this one cuts the transaction that
/// makes it. The two are different code - the incremental path appends a
/// generation and the `rebuild` packs the whole corpus in one pass - and until
/// task-1932 only the first was ever interrupted.
const FRESH_WORKLOAD: &str = "BEGIN;
     INSERT INTO docs(rowid, title, body) VALUES (1, 'Offer', 'who qualifies for the discount');
     INSERT INTO docs(rowid, title, body) VALUES (2, 'Rules', 'the discount applies to accounts');
     INSERT INTO docs(rowid, title, body) VALUES (3, 'Weather', 'rain and wind tomorrow');
     INSERT INTO docs(rowid, title, body) VALUES (4, 'Trial', 'tirzepatide dosing schedule');
     INSERT INTO docs(rowid, title, body) VALUES (5, 'Notes', 'the forecast is sunshine');
     INSERT INTO docs(docs) VALUES ('rebuild');
     COMMIT;";

/// The statements after the build, so a cut can land past its commit.
const FRESH_TAIL: &str = "SELECT count(*) FROM docs_content; PRAGMA wal_checkpoint;";

/// What a fresh build is graded on: the rows and what each query ranks.
const FRESH_PROBES: &[&str] = &[
    "SELECT rowid, title, body FROM docs ORDER BY rowid",
    "SELECT rowid FROM docs WHERE docs MATCH 'discount' AND k = 10 ORDER BY rowid",
    "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide' AND k = 10 ORDER BY rowid",
    "SELECT rowid FROM docs WHERE docs MATCH 'sunshine' AND k = 10 ORDER BY rowid",
];

/// Power loss during a fresh index build leaves no index and no rows.
///
/// The corpus and the `rebuild` that packs it are one transaction, so the two
/// legitimate states are an empty search table and a fully built one. A cut
/// that left the rows without the index, or the index without the rows, is the
/// mixture this grades - and it is the one a fresh build can produce that an
/// incremental update cannot, because a `rebuild` writes a whole generation
/// and names it in the state rows afterwards.
///
/// It uses `crashcampaign::Campaign` rather than this file's own `campaign`
/// because the shared one takes its schema, workload and probes as arguments
/// and this arm needs different ones. The campaigns above predate it and are
/// left where they are; each of them has an arm the shared harness does not
/// model.
#[test]
fn a_fresh_index_build_cut_short_leaves_an_empty_table_or_a_built_one() {
    let report = inillucent_compat::crashcampaign::Campaign {
        name: "search-fresh-build-crash",
        mode: "delete",
        schema: FRESH_SCHEMA,
        workload: FRESH_WORKLOAD,
        tail: FRESH_TAIL,
        probes: FRESH_PROBES,
        failure: inillucent_sim::failpoint::Failure::Crash,
        cuts: 4_000,
    }
    .run();
    inillucent_compat::crashcampaign::record("search-fresh-build-crash", &report);
}

/// The same, through a write-ahead log.
#[test]
fn a_fresh_index_build_under_a_log_leaves_an_empty_table_or_a_built_one() {
    let report = inillucent_compat::crashcampaign::Campaign {
        name: "search-fresh-build-wal-crash",
        mode: "wal",
        schema: FRESH_SCHEMA,
        workload: FRESH_WORKLOAD,
        tail: FRESH_TAIL,
        probes: FRESH_PROBES,
        failure: inillucent_sim::failpoint::Failure::Crash,
        cuts: 4_000,
    }
    .run();
    inillucent_compat::crashcampaign::record("search-fresh-build-wal-crash", &report);
}
