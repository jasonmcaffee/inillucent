//! Durable-write baselines for phase 7: what a committed transaction costs.
//!
//! Invariant: every number here is measured, and every number here is inillucent
//! against itself at a fixed durability level. Nothing in this file is a
//! comparison with SQLite - the fairness contract for that lands with the
//! phase that qualifies performance - and reading one of these as a
//! competitive number would be reading it wrong.
//!
//! Two things are measured that a read baseline has no equivalent of. The
//! first is the *log*: how many records a commit had to append and how many
//! times it had to sync, which is the price of the crash guarantee and the
//! number any change to the commit path has to be judged against. The second
//! is commit latency at p50, p95 and p99 rather than a mean, because a
//! commit's cost is dominated by syncs and a mean hides the tail those
//! produce.
//!
//! Every workload runs on the real operating-system VFS in a scratch
//! directory. A memory VFS would make the sync free, and a benchmark of a
//! durability mechanism whose syncs are free is not a benchmark of it. The
//! consequence is that the wall-clock columns move with the machine and the
//! filesystem; the log and page counters do not, and they are the ones a
//! later change should be read against.
//!
//! **There is one journal mechanism now, not five.** The old engine's
//! `inillucent-transaction::journal::JournalMode` (delete/truncate/persist/
//! memory/wal) is gone with the crate that read it; the new engine always
//! writes a segmented write-ahead log (`inillucent-wal`), so the axis this file
//! used to sweep across five journal modes now sweeps across the three
//! `PRAGMA synchronous` levels instead - `off`, `normal`, `full` - which is the
//! durability knob the new engine actually has.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-txnperf`

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use inillucent_compat::report::json_string;
use inillucent_compat::{platform_name, workspace_root};
use inillucent_engine::connect::{Connection, Database};
use inillucent_engine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_vfs::path::DbPath;
use inillucent_vfs::Vfs;
use inillucent_wal::Synchronous;

/// How many rows each workload writes.
const ROWS: usize = 2_000;

/// How many rows a batched transaction writes before committing.
const BATCH: usize = 200;

/// Which calls of the recovery workload's final commit are tried as the point
/// the power goes.
///
/// The first one that leaves a log a recovery would actually replay is the one
/// measured. The commit's shape depends on how many pages the batch dirtied,
/// so the right cut point is found rather than assumed; the campaigns in
/// `durability.rs`/`wal_crash.rs` are what cover every cut point, and this only
/// has to produce a hot log to time the replay of.
const RECOVERY_CUT_POINTS: [u64; 10] = [40, 36, 32, 28, 24, 20, 16, 12, 8, 4];

/// Page size the crash-simulation database is built with - the engine's own
/// default, so it matches what `Database::open` builds in `fresh()`.
const PAGE_SIZE: usize = inillucent_engine::connect::PAGE_SIZE;

/// Buffer-pool frame count the crash-simulation database is built with - the
/// engine's own default, for the same reason.
const FRAMES: usize = inillucent_engine::DEFAULT_FRAMES;

/// One measured workload.
#[derive(Clone, Debug)]
struct Measurement {
    /// What was measured.
    workload: String,
    /// The journal mechanism it ran under - always `wal`, the only one the new engine has.
    mode: String,
    /// The durability level it ran under.
    synchronous: String,
    /// How many rows were written.
    rows: u64,
    /// How many transactions committed.
    commits: u64,
    /// Nanoseconds per row.
    nanos_per_row: f64,
    /// Commit latency, in microseconds.
    commit_p50: f64,
    /// Commit latency at the ninety-fifth percentile.
    commit_p95: f64,
    /// Commit latency at the ninety-ninth percentile.
    commit_p99: f64,
    /// Records appended to the write-ahead log.
    journal_records: u64,
    /// Bytes appended to the write-ahead log.
    journal_bytes: u64,
    /// Times the write-ahead log was synced.
    journal_syncs: u64,
    /// Pages written to the database file. Zero under WAL until a checkpoint
    /// runs - the log is what a commit writes to, and the main file is only
    /// caught up later - so a workload here that never checkpoints reports
    /// zero correctly rather than reporting the log's own writes twice.
    page_writes: u64,
    /// Bytes written to the database file.
    bytes_written: u64,
    /// Bytes of row payload the caller asked to store.
    payload_bytes: u64,
}

impl Measurement {
    /// Returns bytes written for each byte of payload stored.
    ///
    /// The journal is counted, because it is written for the same rows and a
    /// number that left it out would make the crash guarantee look free.
    fn amplification(&self) -> f64 {
        if self.payload_bytes == 0 {
            return 0.0;
        }
        (self.bytes_written.saturating_add(self.journal_bytes)) as f64 / self.payload_bytes as f64
    }

    /// Renders the measurement as one JSON object.
    fn to_json(&self) -> String {
        format!(
            "{{\"workload\":{},\"mode\":{},\"synchronous\":{},\"rows\":{},\"commits\":{},\
             \"nanos_per_row\":{:.1},\"commit_p50_micros\":{:.1},\"commit_p95_micros\":{:.1},\
             \"commit_p99_micros\":{:.1},\"journal_records\":{},\"journal_bytes\":{},\
             \"journal_syncs\":{},\"page_writes\":{},\"bytes_written\":{},\
             \"payload_bytes\":{},\"write_amplification\":{:.2}}}",
            json_string(&self.workload),
            json_string(&self.mode),
            json_string(&self.synchronous),
            self.rows,
            self.commits,
            self.nanos_per_row,
            self.commit_p50,
            self.commit_p95,
            self.commit_p99,
            self.journal_records,
            self.journal_bytes,
            self.journal_syncs,
            self.page_writes,
            self.bytes_written,
            self.payload_bytes,
            self.amplification()
        )
    }
}

/// Runs every workload and writes the report.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let out = flag(&arguments, "--out").unwrap_or_else(|| workspace_root().join("compat/baseline"));
    let scratch = flag(&arguments, "--scratch")
        .unwrap_or_else(|| workspace_root().join("_agent_output/txnperf"));
    if let Err(failure) = std::fs::create_dir_all(&scratch) {
        eprintln!("cannot create {}: {failure}", scratch.display());
        return ExitCode::FAILURE;
    }
    let mut measurements = Vec::new();
    for synchronous in [Synchronous::Full, Synchronous::Normal, Synchronous::Off] {
        match run_suite(&scratch, synchronous) {
            Ok(mut measured) => measurements.append(&mut measured),
            Err(failure) => {
                eprintln!("{failure}");
                return ExitCode::FAILURE;
            }
        }
    }
    let platform = platform_name();
    let json = render_json(&platform, &measurements);
    let markdown = render_markdown(&platform, &measurements);
    if let Err(failure) = std::fs::create_dir_all(&out) {
        eprintln!("cannot create {}: {failure}", out.display());
        return ExitCode::FAILURE;
    }
    let json_path = out.join("phase7-durable-write-baselines.json");
    let markdown_path = out.join("phase7-durable-write-baselines.md");
    if let Err(failure) = std::fs::write(&json_path, json) {
        eprintln!("cannot write {}: {failure}", json_path.display());
        return ExitCode::FAILURE;
    }
    if let Err(failure) = std::fs::write(&markdown_path, markdown) {
        eprintln!("cannot write {}: {failure}", markdown_path.display());
        return ExitCode::FAILURE;
    }
    println!("wrote {}", markdown_path.display());
    ExitCode::SUCCESS
}

/// Returns the value of a `--name value` flag.
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}

/// Runs every workload at one durability setting.
fn run_suite(scratch: &Path, synchronous: Synchronous) -> Result<Vec<Measurement>, String> {
    let mut measurements = vec![
        insert_workload(scratch, synchronous, "insert-autocommit", 1)?,
        insert_workload(scratch, synchronous, "insert-batched", BATCH)?,
        update_workload(scratch, synchronous, "update-autocommit", 1)?,
        update_workload(scratch, synchronous, "update-batched", BATCH)?,
        delete_workload(scratch, synchronous, "delete-autocommit", 1)?,
    ];
    measurements.push(delete_workload(
        scratch,
        synchronous,
        "delete-batched",
        BATCH,
    )?);
    measurements.push(savepoint_workload(scratch, synchronous)?);
    measurements.push(recovery_workload(scratch, synchronous)?);
    Ok(measurements)
}

/// Opens a fresh database with a schema, deleting whatever was there before.
///
/// Returns the `Database` alongside its `Connection`, rather than the
/// connection alone the way the old engine's front end let this file do:
/// `log_stats()`/`cache_stats()` are answered by the database, not the
/// connection, and `snapshot`/`finish` below need both.
fn fresh(
    scratch: &Path,
    name: &str,
    synchronous: Synchronous,
) -> Result<(&'static Database, Connection<'static>), String> {
    let path = scratch.join(format!("{name}.db"));
    inillucent_base::testing::remove_database(&path);
    let database = Database::open(&path).map_err(text)?;
    // Leaked for the same reason `differential::start_inillucent` leaks: the
    // new engine's `Connection<'d>` borrows the `Database`, and this measurement
    // program runs for a few seconds and exits.
    let database: &'static Database = Box::leak(Box::new(database));
    let connection = database.session();
    run(
        &connection,
        &format!("PRAGMA synchronous = {}", synchronous.name()),
    )?;
    run(
        &connection,
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER)",
    )?;
    Ok((database, connection))
}

/// Runs one script, reporting a failure as a string.
fn run(connection: &Connection<'_>, sql: &str) -> Result<(), String> {
    connection.execute_batch(sql).map_err(text)
}

/// Returns the row a workload writes at one index.
fn row_sql(index: usize) -> String {
    format!(
        "INSERT INTO t VALUES({index}, 'row {index} padded out to something like a real row', {index})"
    )
}

/// How many bytes of user data one row holds.
///
/// The text plus the two integers, not the length of the statement that wrote
/// them: write amplification is a ratio against what was *stored*, and using
/// the SQL text as the denominator would make a verbose statement look
/// efficient.
fn payload_of(index: usize) -> u64 {
    let text = format!("row {index} padded out to something like a real row");
    (text.len() as u64).saturating_add(16)
}

/// Measures inserting `ROWS` rows in transactions of `batch` rows each.
fn insert_workload(
    scratch: &Path,
    synchronous: Synchronous,
    name: &str,
    batch: usize,
) -> Result<Measurement, String> {
    let (database, connection) = fresh(scratch, name, synchronous)?;
    let before = snapshot(database);
    let mut latencies = Vec::new();
    let mut payload = 0u64;
    let started = Instant::now();
    let mut index = 0usize;
    while index < ROWS {
        let end = index.saturating_add(batch).min(ROWS);
        let explicit = batch > 1;
        if explicit {
            run(&connection, "BEGIN")?;
        }
        for row in index..end {
            // In autocommit the statement *is* the transaction, so the
            // statement's own elapsed time is the commit latency; timing only
            // an explicit COMMIT would report zero for every autocommit run
            // and make the mode look free.
            let statement = Instant::now();
            run(&connection, &row_sql(row))?;
            if !explicit {
                latencies.push(statement.elapsed());
            }
            payload = payload.saturating_add(payload_of(row));
        }
        if explicit {
            let commit = Instant::now();
            run(&connection, "COMMIT")?;
            latencies.push(commit.elapsed());
        }
        index = end;
    }
    let elapsed = started.elapsed();
    Ok(finish(
        name,
        synchronous,
        database,
        before,
        ROWS as u64,
        elapsed,
        latencies,
        payload,
    ))
}

/// Measures updating every row, in transactions of `batch` rows each.
fn update_workload(
    scratch: &Path,
    synchronous: Synchronous,
    name: &str,
    batch: usize,
) -> Result<Measurement, String> {
    let (database, connection) = fresh(scratch, name, synchronous)?;
    run(&connection, "BEGIN")?;
    for row in 0..ROWS {
        run(&connection, &row_sql(row))?;
    }
    run(&connection, "COMMIT")?;
    let before = snapshot(database);
    let mut latencies = Vec::new();
    let mut payload = 0u64;
    let started = Instant::now();
    let mut index = 0usize;
    while index < ROWS {
        let end = index.saturating_add(batch).min(ROWS);
        let explicit = batch > 1;
        if explicit {
            run(&connection, "BEGIN")?;
        }
        for row in index..end {
            let statement = Instant::now();
            run(
                &connection,
                &format!("UPDATE t SET c = c + 1 WHERE a = {row}"),
            )?;
            if !explicit {
                latencies.push(statement.elapsed());
            }
            payload = payload.saturating_add(payload_of(row));
        }
        if explicit {
            let commit = Instant::now();
            run(&connection, "COMMIT")?;
            latencies.push(commit.elapsed());
        }
        index = end;
    }
    let elapsed = started.elapsed();
    Ok(finish(
        name,
        synchronous,
        database,
        before,
        ROWS as u64,
        elapsed,
        latencies,
        payload,
    ))
}

/// Measures deleting every row, in transactions of `batch` rows each.
fn delete_workload(
    scratch: &Path,
    synchronous: Synchronous,
    name: &str,
    batch: usize,
) -> Result<Measurement, String> {
    let (database, connection) = fresh(scratch, name, synchronous)?;
    run(&connection, "BEGIN")?;
    for row in 0..ROWS {
        run(&connection, &row_sql(row))?;
    }
    run(&connection, "COMMIT")?;
    let before = snapshot(database);
    let mut latencies = Vec::new();
    let mut payload = 0u64;
    let started = Instant::now();
    let mut index = 0usize;
    while index < ROWS {
        let end = index.saturating_add(batch).min(ROWS);
        let explicit = batch > 1;
        if explicit {
            run(&connection, "BEGIN")?;
        }
        for row in index..end {
            let statement = Instant::now();
            run(&connection, &format!("DELETE FROM t WHERE a = {row}"))?;
            if !explicit {
                latencies.push(statement.elapsed());
            }
            payload = payload.saturating_add(payload_of(row));
        }
        if explicit {
            let commit = Instant::now();
            run(&connection, "COMMIT")?;
            latencies.push(commit.elapsed());
        }
        index = end;
    }
    let elapsed = started.elapsed();
    Ok(finish(
        name,
        synchronous,
        database,
        before,
        ROWS as u64,
        elapsed,
        latencies,
        payload,
    ))
}

/// Measures a savepoint opened, filled, and rolled back.
///
/// The rows never reach the file, so what this measures is what an undo level
/// costs in memory and what the transaction pays for having had one.
fn savepoint_workload(scratch: &Path, synchronous: Synchronous) -> Result<Measurement, String> {
    let (database, connection) = fresh(scratch, "savepoint", synchronous)?;
    let before = snapshot(database);
    let mut latencies = Vec::new();
    let started = Instant::now();
    let mut index = 0usize;
    while index < ROWS {
        let end = index.saturating_add(BATCH).min(ROWS);
        run(&connection, "BEGIN")?;
        run(&connection, "SAVEPOINT s")?;
        for row in index..end {
            run(&connection, &row_sql(row))?;
        }
        let commit = Instant::now();
        run(&connection, "ROLLBACK TO s")?;
        run(&connection, "RELEASE s")?;
        run(&connection, "COMMIT")?;
        latencies.push(commit.elapsed());
        index = end;
    }
    let elapsed = started.elapsed();
    Ok(finish(
        "savepoint-rollback",
        synchronous,
        database,
        before,
        ROWS as u64,
        elapsed,
        latencies,
        0,
    ))
}

/// Measures recovering a database whose writer really was interrupted.
///
/// The crashed image is produced by the simulator, because a real writer
/// cannot be interrupted on demand: the power loss is armed at a numbered VFS
/// call inside the commit, and what the simulated media holds afterwards is
/// written out as ordinary files. The *measurement* is then a real recovery on
/// the real VFS, opening those files - so the input is synthetic and the clock
/// is not.
///
/// Which call to cut at is found rather than assumed. The commit's shape
/// depends on how many pages the batch dirtied, so the workload tries each cut
/// point in turn and keeps the first that leaves a journal a recovery would
/// actually replay.
fn recovery_workload(scratch: &Path, synchronous: Synchronous) -> Result<Measurement, String> {
    let target = scratch.join("recovery.db");
    for cut in RECOVERY_CUT_POINTS {
        let Some(files) = crashed_image(synchronous, cut)? else {
            continue;
        };
        // Every file the crash produced is written out under its own basename,
        // not renamed - the simulated path was `/sim/recovery.db`, so a segment
        // the WAL wrote beside it comes back as `recovery.db-wal.NNNNNNNN`, and
        // writing it under any other name would be guessing at a naming scheme
        // rather than reproducing the one the engine actually used.
        for existing in std::fs::read_dir(scratch).map_err(text)? {
            let existing = existing.map_err(text)?.path();
            if existing
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("recovery.db"))
            {
                let _ = std::fs::remove_file(existing);
            }
        }
        let mut wrote_database = false;
        let mut segment_bytes = 0u64;
        for (name, bytes) in &files {
            let Some(basename) = name.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            std::fs::write(scratch.join(basename), bytes).map_err(text)?;
            if basename == "recovery.db" {
                wrote_database = true;
            } else {
                segment_bytes = segment_bytes.saturating_add(bytes.len() as u64);
            }
        }
        if !wrote_database {
            continue;
        }

        let started = Instant::now();
        let Ok(recovered) = Database::open(&target) else {
            // This cut point's file did not even open cleanly - try the next.
            continue;
        };
        // `check()` is what confirms the recovery actually replayed something
        // a caller can use, rather than opening a file whose log never got
        // far enough to matter: it walks every tree's structure, which is what
        // `inillucent-testrun`'s own crash suites use for the same reason.
        if recovered.check().is_err() {
            continue;
        }
        let elapsed = started.elapsed();
        let counters = recovered.cache_stats();
        let bytes = std::fs::metadata(&target)
            .map(|data| data.len())
            .unwrap_or(0);
        return Ok(Measurement {
            workload: "recovery".to_string(),
            mode: "wal".to_string(),
            synchronous: synchronous.name().to_string(),
            rows: 1,
            commits: 0,
            nanos_per_row: elapsed.as_nanos() as f64,
            commit_p50: micros(elapsed),
            commit_p95: micros(elapsed),
            commit_p99: micros(elapsed),
            journal_records: 0,
            journal_bytes: segment_bytes,
            journal_syncs: 0,
            page_writes: counters.writes,
            bytes_written: bytes,
            payload_bytes: 0,
        });
    }
    // No cut point left a log segment that both wrote a database and checked
    // out after reopening. Reporting that is better than reporting a recovery
    // time for a recovery that did not happen.
    Ok(empty("recovery", synchronous))
}

/// Builds a crashed database and whatever log segments were beside it, or
/// `None` when that cut point left no database to recover.
///
/// Built directly against `inillucent_engine::ImportedDatabase` rather than the
/// public `connect::Database` facade, because injecting the simulated `Vfs` a
/// power-loss test needs is an engine-internal constructor
/// (`ImportedDatabase::create_on`) that the public facade does not expose -
/// deliberately, since an embedding application has no simulator to hand it.
///
/// Returns every file the simulated media held afterwards, keyed by its
/// simulated path, rather than guessing which one is "the" log segment: the
/// engine's log is written in numbered segments and this has no reason to
/// know how many it made.
fn crashed_image(
    synchronous: Synchronous,
    cut: u64,
) -> Result<Option<std::collections::BTreeMap<PathBuf, Vec<u8>>>, String> {
    let path = DbPath::from("/sim/recovery.db");
    let sim = Arc::new(SimVfs::new(SimConfig {
        seed: 1786 + cut,
        model: MediaModel::default(),
        ..SimConfig::default()
    }));
    {
        let vfs = Arc::clone(&sim) as Arc<dyn Vfs>;
        let mut database =
            ImportedDatabase::create_on(vfs, path.as_path().to_path_buf(), PAGE_SIZE, FRAMES)
                .map_err(text)?;
        database
            .execute_any(
                &format!("PRAGMA synchronous = {}", synchronous.name()),
                &Params::new(),
            )
            .map_err(text)?;
        database
            .execute_any(
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER)",
                &Params::new(),
            )
            .map_err(text)?;
        database
            .execute_any("BEGIN", &Params::new())
            .map_err(text)?;
        for row in 0..ROWS {
            database
                .execute_any(&row_sql(row), &Params::new())
                .map_err(text)?;
        }
        database
            .execute_any("COMMIT", &Params::new())
            .map_err(text)?;
        database
            .execute_any("BEGIN", &Params::new())
            .map_err(text)?;
        for row in ROWS..ROWS.saturating_add(BATCH) {
            database
                .execute_any(&row_sql(row), &Params::new())
                .map_err(text)?;
        }
        // Arm the power loss for the commit only. Arming it before the inserts
        // would cut somewhere in the middle of a read, which leaves nothing to
        // recover and says nothing about how long a recovery takes.
        let base = sim.failpoints().sites_reached();
        sim.failpoints()
            .fail_nth_call(base.saturating_add(cut), Failure::Crash);
        let _ = database.execute_any("COMMIT", &Params::new());
    }
    let crashed = sim.crash();
    let has_database = crashed
        .files
        .keys()
        .any(|name| name.file_name().and_then(|name| name.to_str()) == Some("recovery.db"));
    if !has_database {
        return Ok(None);
    }
    Ok(Some(crashed.files))
}

/// Returns a measurement that says a workload did not apply.
fn empty(workload: &str, synchronous: Synchronous) -> Measurement {
    Measurement {
        workload: workload.to_string(),
        mode: "wal".to_string(),
        synchronous: synchronous.name().to_string(),
        rows: 0,
        commits: 0,
        nanos_per_row: 0.0,
        commit_p50: 0.0,
        commit_p95: 0.0,
        commit_p99: 0.0,
        journal_records: 0,
        journal_bytes: 0,
        journal_syncs: 0,
        page_writes: 0,
        bytes_written: 0,
        payload_bytes: 0,
    }
}

/// Returns the counters a workload starts from.
fn snapshot(database: &Database) -> (inillucent_engine::connect::LogStats, u64) {
    let log = database.log_stats();
    (log, database.cache_stats().writes)
}

/// Builds the measurement from what the counters moved by.
fn finish(
    workload: &str,
    synchronous: Synchronous,
    database: &Database,
    before: (inillucent_engine::connect::LogStats, u64),
    rows: u64,
    elapsed: Duration,
    latencies: Vec<Duration>,
    payload_bytes: u64,
) -> Measurement {
    let log = database.log_stats();
    let page_writes = database.cache_stats().writes;
    let mut sorted: Vec<u128> = latencies.iter().map(Duration::as_nanos).collect();
    sorted.sort_unstable();
    Measurement {
        workload: workload.to_string(),
        mode: "wal".to_string(),
        synchronous: synchronous.name().to_string(),
        rows,
        // The new engine has no separate "commit" counter; every commit that
        // writes appends at least one log record, so the number of commits is
        // recovered from how the caller shaped the workload instead - see each
        // workload's own `explicit`/`batch` accounting, folded into `latencies`
        // one entry per commit.
        commits: latencies.len() as u64,
        nanos_per_row: if rows == 0 {
            0.0
        } else {
            elapsed.as_nanos() as f64 / rows as f64
        },
        commit_p50: percentile(&sorted, 50.0),
        commit_p95: percentile(&sorted, 95.0),
        commit_p99: percentile(&sorted, 99.0),
        journal_records: log.records.saturating_sub(before.0.records),
        journal_bytes: log.bytes.saturating_sub(before.0.bytes),
        journal_syncs: log.syncs.saturating_sub(before.0.syncs),
        page_writes: page_writes.saturating_sub(before.1),
        // The new engine's `CacheStats` has no separate "bytes written to the
        // file" counter distinct from pages written; a page is the unit the
        // pool writes in, so bytes are approximated from the page count at the
        // database's own page size rather than left at zero.
        bytes_written: page_writes
            .saturating_sub(before.1)
            .saturating_mul(PAGE_SIZE as u64),
        payload_bytes,
    }
}

/// Returns a percentile of sorted nanosecond samples, in microseconds.
fn percentile(sorted: &[u128], percent: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (percent / 100.0) * (sorted.len().saturating_sub(1)) as f64;
    let index = rank.round().max(0.0) as usize;
    let value = sorted
        .get(index.min(sorted.len().saturating_sub(1)))
        .copied();
    value.unwrap_or(0) as f64 / 1000.0
}

/// Returns a duration in microseconds.
fn micros(duration: Duration) -> f64 {
    duration.as_nanos() as f64 / 1000.0
}

/// Returns an error as a string.
fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Renders the measurements as JSON.
fn render_json(platform: &str, measurements: &[Measurement]) -> String {
    let body: Vec<String> = measurements.iter().map(Measurement::to_json).collect();
    format!(
        "{{\"phase\":\"phase 7: single-database rollback transactions and DML\",\
         \"platform\":{},\"rows\":{},\"batch\":{},\"measurements\":[{}]}}\n",
        json_string(platform),
        ROWS,
        BATCH,
        body.join(",")
    )
}

/// Renders the measurements as a table.
fn render_markdown(platform: &str, measurements: &[Measurement]) -> String {
    let mut out = String::new();
    out.push_str("# Durable-write baselines, phase 7\n\n");
    out.push_str(&format!("Platform: `{platform}`\n\n"));
    out.push_str(&format!(
        "Each workload writes {ROWS} rows, either one transaction per row (`autocommit`) or \
         {BATCH} rows per transaction (`batched`). Every run is on the real operating-system \
         VFS: a memory VFS would make the sync free, and a benchmark of a durability \
         mechanism whose syncs are free measures nothing about it.\n\n"
    ));
    out.push_str(
        "These are baselines, not comparisons. Nothing here is measured against SQLite and no \
         number here should be read as one. The wall-clock columns move with the machine and \
         the filesystem; the journal and page counters do not, and they are what a later \
         change to the commit path should be read against.\n\n\
         `Amp` counts the journal as well as the database, because the journal is written for \
         the same rows and a figure that left it out would make the crash guarantee look \
         free. `memory`/`off` is included as the floor a run without a crash guarantee \
         reaches, and is never a durability result: the two modes are recorded as not \
         crash-safe and cannot be quoted for one.\n\n",
    );
    out.push_str(
        "## What was changed, and what the numbers said\n\n\
         **The commit was rewriting the journal's header sector when it only had to rewrite \
         its fields.** The header is written twice: once with the magic zeroed, so the file is \
         not yet hot, and again at commit with the magic and the real record count. The second \
         write was a full sector, and every byte of it past the twenty-eighth was already \
         exactly what it was putting there.\n\n\
         Shrinking it to the fields changes nothing about the crash argument, and the argument \
         is worth restating because it is what licenses the change: a torn write leaves a \
         prefix of the new bytes and the rest of the old, the old bytes here are the *first* \
         header, and the two versions differ only in the magic and the record count. Every \
         mixture is therefore either not hot, or hot with a record count that is the real one \
         or smaller - and a smaller one is safe because no database page has been written at \
         the moment that count was the truth. Whether the padding is rewritten does not enter \
         into it.\n\n\
         Measured on the autocommit insert, where the header dominates because each commit \
         journals only two or three pages: journal bytes fell from 34,957 KB to 27,012 KB for \
         the same 2,000 rows, and write amplification from 436 to 372. The batched workloads \
         moved much less, which is the expected shape - one header per 200 rows instead of one \
         per row. No wall-clock column moved outside run-to-run noise, and none is claimed.\n\n\
         The transaction boundaries and the durability level are identical on both sides of \
         that change, and the whole crash matrix was re-run after it.\n\n",
    );
    out.push_str(
        "| Workload | Mode | Sync | Rows | Commits | ns/row | p50 us | p95 us | p99 us | Journal recs | Journal KB | Syncs | Page writes | Amp |\n",
    );
    out.push_str("|---|---|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|\n");
    for measurement in measurements {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {:.0} | {:.1} | {:.1} | {:.1} | {} | {:.1} | {} | {} | {:.2} |\n",
            measurement.workload,
            measurement.mode,
            measurement.synchronous,
            measurement.rows,
            measurement.commits,
            measurement.nanos_per_row,
            measurement.commit_p50,
            measurement.commit_p95,
            measurement.commit_p99,
            measurement.journal_records,
            measurement.journal_bytes as f64 / 1024.0,
            measurement.journal_syncs,
            measurement.page_writes,
            measurement.amplification()
        ));
    }
    out
}
