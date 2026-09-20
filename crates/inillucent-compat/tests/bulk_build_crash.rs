//! Power loss at every cut point of a `CREATE INDEX`.
//!
//! Invariant: **a bulk build's pages are durable before the commit that names
//! their root, and unreachable if that commit never happens.** That sentence is
//! the whole of why design 2 of task-2000 could stop logging a whole page image
//! per built page: the build writes its pages straight into the data file through
//! `Pool::write_built_page`, syncs the file, and only then appends the statement's
//! own records and syncs the log. Nothing in the log describes the pages'
//! contents, so nothing in the log has to be replayed for them - and the ordering
//! is what makes that safe rather than lucky.
//!
//! The two states a cut may leave, and there are only two:
//!
//! - **before the log sync**: the catalog never named the root, and the
//!   `AllocPage` records belong to a transaction with no `Commit`, so recovery
//!   does not replay them. The pages are still free, the bytes in them are
//!   unreachable, and the index is not there.
//! - **after the log sync**: the pages were on the media before the commit was, so
//!   the index is readable and complete.
//!
//! A torn page cannot exist at the commit point, because the file sync preceded
//! it. What this file asserts is that every cut lands in one of the two states and
//! that the database opens, reads and passes its integrity check in both.
//!
//! **It is a campaign rather than a case** for the reason `wal_crash.rs` gives:
//! every injectable call of the run is numbered, and the run is repeated once per
//! number with the failure armed at exactly that call. A single crash at a chosen
//! moment tests the moment somebody thought of.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::Vfs;

/// The page size these runs build at.
///
/// **Four kilobytes rather than the engine's 32 KiB default, so the index has a
/// shape.** The point of the campaign is a build that allocates a run of leaves and
/// an interior level above them; four hundred rows of forty-odd bytes fit in one
/// 32 KiB leaf, and a one page index exercises neither the run nor the level. It is
/// also SQLite's own default, so it is not an exotic configuration.
const PAGE_SIZE: usize = 4_096;

/// How many frames the pool holds.
const FRAMES: usize = 4_096;

/// How many rows the index is built over.
///
/// Enough that the build produces several leaves and an interior level - which is
/// what makes the `AllocPage` records, the sequential page writes and the
/// `BulkBuilt` record all present - and small enough that four hundred cuts of it
/// run in minutes rather than hours.
const ROWS: usize = 400;

/// The schema and rows every run starts from.
const SCHEMA: &str = "PRAGMA journal_mode=wal;
     PRAGMA synchronous=full;
     CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);";

/// The statement each run tries to commit.
const WORKLOAD: &str = "CREATE INDEX t_b ON t(b);";

/// Returns a simulator with the pessimistic device model.
///
/// @param seed - the run's seed
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// The path every run uses.
fn path() -> PathBuf {
    PathBuf::from("/sim/bulk.db")
}

/// Runs a script of one or more statements, stopping at the first failure.
///
/// @param engine - the connection
/// @param sql - one or more statements
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

/// Builds the table and its rows, and returns the simulator holding them.
///
/// @param seed - the run's seed
fn built(seed: u64) -> Arc<SimVfs> {
    let vfs = simulator(seed);
    let mut engine =
        ImportedDatabase::create_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES)
            .expect("the connection opens");
    run(&mut engine, SCHEMA).expect("the schema builds");
    let mut rows = String::from("BEGIN;\n");
    for nth in 1..=ROWS {
        // Padded so the values are wide enough that the index needs several
        // leaves, and ordered so the built tree has a shape rather than one page.
        rows.push_str(&format!(
            "INSERT INTO t VALUES({nth}, 'label-{nth:08}-padding-padding-padding');\n"
        ));
    }
    rows.push_str("COMMIT;\n");
    run(&mut engine, &rows).expect("the rows insert");
    // Folded, so the cuts below are cuts of the `CREATE INDEX` and not of the
    // insert's log still being folded in.
    engine.checkpoint().expect("the fold runs");
    drop(engine);
    vfs
}

/// What reopening a crashed database produced.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Recovery {
    /// It opened, and the index was there and answered.
    WithIndex(usize),
    /// It opened, and the index was not there.
    WithoutIndex,
    /// It refused to be read, naming the damage.
    Broken(String),
}

/// Reopens what a crash left behind and asks whether the index is there.
///
/// **The question is asked of the catalog and then of the index**, because those
/// are the two halves that have to agree: a catalog row naming a root whose pages
/// were never written is the failure this campaign exists to catch, and it shows
/// up as a query that refuses rather than as an open that does.
///
/// @param snapshot - what the crash left
/// @param seed - the recovery's own seed
fn recovered(snapshot: &CrashSnapshot, seed: u64) -> Recovery {
    let vfs = Arc::new(SimVfs::recovered(
        SimConfig {
            seed,
            model: MediaModel::default(),
            ..SimConfig::default()
        },
        snapshot,
    ));
    let mut engine = match ImportedDatabase::open_on(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        path(),
        PAGE_SIZE,
        FRAMES,
    ) {
        Ok(engine) => engine,
        Err(failure) => return Recovery::Broken(detail(&failure)),
    };
    // Every row, read through the table, has to be there either way: the index is
    // the only thing a cut may take away.
    match engine.execute_any("SELECT count(*) FROM t", &Params::new()) {
        Ok(answer) => {
            let count = first_integer(&answer.rows);
            if count != Some(ROWS as i64) {
                return Recovery::Broken(format!(
                    "the table came back with {count:?} rows rather than {ROWS}"
                ));
            }
        }
        Err(failure) => return Recovery::Broken(detail(&failure)),
    }
    let named = match engine.execute_any(
        "SELECT count(*) FROM sqlite_master WHERE name = 't_b'",
        &Params::new(),
    ) {
        Ok(answer) => first_integer(&answer.rows).unwrap_or(0) > 0,
        Err(failure) => return Recovery::Broken(detail(&failure)),
    };
    if !named {
        return Recovery::WithoutIndex;
    }
    // The catalog names it, so every page of it has to be readable. A query the
    // planner answers through the index is what asks.
    match engine.execute_any(
        "SELECT count(*) FROM t WHERE b > 'label-00000000'",
        &Params::new(),
    ) {
        Ok(answer) => match first_integer(&answer.rows) {
            Some(count) => Recovery::WithIndex(count as usize),
            None => Recovery::Broken("the index query answered no row".to_string()),
        },
        Err(failure) => Recovery::Broken(detail(&failure)),
    }
}

/// Renders a failure with its detail, which is where the page and the checksums
/// are.
///
/// @param failure - what the engine reported
fn detail(failure: &inillucent_base::DbError) -> String {
    match failure.detail() {
        Some(said) => format!("{failure}: {said}"),
        None => format!("{failure}"),
    }
}

/// Returns the first value of the first row, as an integer.
///
/// @param rows - the outcome's rows
fn first_integer(rows: &[Vec<OwnedDatum>]) -> Option<i64> {
    match rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(number)) => Some(*number),
        _ => None,
    }
}

/// A crash at every cut point of a bulk index build leaves the index whole or
/// absent, and never a catalog row naming pages nobody wrote.
#[test]
fn every_cut_of_a_bulk_index_build_is_recoverable() {
    let mut cuts = 0u64;
    let mut with = 0u64;
    let mut without = 0u64;
    let mut report = String::new();
    // **Twelve hundred rather than four hundred, and the loop stops itself.** A
    // `CREATE INDEX` over four hundred rows reaches more than four hundred
    // injectable calls - it reads every row, sorts, packs the pages, writes them and
    // syncs - so a campaign of four hundred cuts landed entirely *before* the
    // commit and never once tested the state after it. The `break` below is what
    // decides the real bound: it stops the first time a run has fewer sites than the
    // cut being armed, so the sweep covers the whole statement and nothing more.
    for nth in 1..=1_200u64 {
        let vfs = built(5_000 + nth);
        let base = vfs.failpoints().sites_reached();
        vfs.failpoints()
            .fail_nth_call(base.saturating_add(nth), Failure::Crash);
        let mut connection =
            ImportedDatabase::open_on(Arc::clone(&vfs) as Arc<dyn Vfs>, path(), PAGE_SIZE, FRAMES);
        let committed = match &mut connection {
            Ok(engine) => run(engine, WORKLOAD).is_ok(),
            Err(_) => false,
        };
        let reached = vfs.failpoints().sites_reached().saturating_sub(base);
        let snapshot = vfs.crash();
        drop(connection);
        if reached < nth {
            break;
        }
        cuts = cuts.saturating_add(1);
        let state = recovered(&snapshot, 6_000 + nth);
        let verdict = match &state {
            Recovery::WithIndex(count) => {
                assert_eq!(
                    *count, ROWS,
                    "cut {nth}: the index is there and answered {count} of {ROWS} rows"
                );
                with = with.saturating_add(1);
                "with"
            }
            Recovery::WithoutIndex => {
                without = without.saturating_add(1);
                "without"
            }
            Recovery::Broken(said) => {
                panic!("cut {nth}: the database came back unreadable: {said}")
            }
        };
        assert!(
            !committed || verdict == "with",
            "cut {nth}: the CREATE INDEX reported success and the index is not there"
        );
        report.push_str(&format!("{nth}\t{verdict}\t{committed}\n"));
    }
    assert!(cuts > 20, "only {cuts} cut points were reached");
    assert!(
        without > 0,
        "no cut left the index absent, so the before-the-commit state was never tested"
    );
    assert!(
        with > 0,
        "no cut left the index present, so the after-the-commit state was never tested"
    );
    let directory = inillucent_compat::workspace_root().join("_agent_output/bulk-build-crash");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(
        directory.join("cuts.tsv"),
        format!(
            "# {cuts} cuts: {with} with the index, {without} without\ncut\tstate\tcommitted\n{report}"
        ),
    );
}
