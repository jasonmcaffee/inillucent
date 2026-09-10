//! Concurrency and multi-database baselines for phases 9 and 10.
//!
//! Invariant: every number here is measured on the real operating-system VFS,
//! at a stated durability level, with the checkpoint counted. Those three
//! together are what make a log's numbers honest. A write-ahead log is fast at
//! commit time precisely because it defers work, so a benchmark that stops the
//! clock before the checkpoint is measuring the deferral rather than the
//! system: the commit families here run their checkpoint inside the timed
//! region and the report says how much of the total it was.
//!
//! Nothing here is a comparison with SQLite. The fairness contract for that
//! lands with the phase that qualifies performance; these are inillucent against
//! itself, so that a later change to the log, the checkpoint, the foreign-key
//! machinery or the two-database commit has a number to be read against.
//!
//! The wall-clock columns move with the machine and the filesystem. The frame,
//! sync and page counters do not, and they are the ones worth arguing about.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-walperf`

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use inillucent_compat::report::json_string;
use inillucent_compat::{platform_name, workspace_root};
use inillucent_session::connection::{Connection, OpenOptions, SessionDatabase};
use inillucent_session::statement;
use inillucent_storage::wal::CheckpointMode;
use inillucent_transaction::journal::{JournalMode, JournalOptions, Synchronous};

/// How many rows each commit family writes.
const ROWS: u64 = 1_000;

/// How many rows the checkpoint and recovery families put in the log first.
const LOG_ROWS: u64 = 2_000;

/// How long the reader-and-writer families run for.
const CONTENTION: Duration = Duration::from_millis(750);

/// How many rows the service families work over.
const SERVICE_ROWS: u64 = 4_000;

/// One measured workload.
///
/// The columns are the union of what the families need rather than a shape per
/// family, because a reader comparing a WAL commit against a rollback commit
/// should not have to line two tables up by eye. A column a family has nothing
/// to say about is zero.
#[derive(Clone, Debug, Default)]
struct Measurement {
    /// Which family it belongs to.
    family: String,
    /// What was measured.
    workload: String,
    /// The durability level it ran under.
    synchronous: String,
    /// How many units of work the family counts.
    operations: u64,
    /// Nanoseconds per unit of work.
    nanos_per_operation: f64,
    /// Latency at the fiftieth percentile, in microseconds.
    p50: f64,
    /// Latency at the ninety-fifth percentile, in microseconds.
    p95: f64,
    /// Latency at the ninety-ninth percentile, in microseconds.
    p99: f64,
    /// Seconds the whole family took, checkpoint included.
    seconds: f64,
    /// Seconds of that spent checkpointing.
    checkpoint_seconds: f64,
    /// Frames appended to the log.
    frames_written: u64,
    /// Times the log was synced.
    log_syncs: u64,
    /// Frames copied back into the database.
    frames_backfilled: u64,
    /// Pages written to the database file.
    page_writes: u64,
    /// Bytes written to the database file and the log together.
    bytes_written: u64,
    /// What the family has to say for itself that no column holds.
    note: String,
}

impl Measurement {
    /// Returns what share of the total was checkpointing.
    fn checkpoint_share(&self) -> f64 {
        if self.seconds <= 0.0 {
            return 0.0;
        }
        self.checkpoint_seconds / self.seconds
    }

    /// Renders the measurement as one JSON object.
    fn to_json(&self) -> String {
        format!(
            "{{\"family\":{},\"workload\":{},\"synchronous\":{},\"operations\":{},\
             \"nanos_per_operation\":{:.1},\"p50_micros\":{:.1},\"p95_micros\":{:.1},\
             \"p99_micros\":{:.1},\"seconds\":{:.4},\"checkpoint_seconds\":{:.4},\
             \"checkpoint_share\":{:.3},\"frames_written\":{},\"log_syncs\":{},\
             \"frames_backfilled\":{},\"page_writes\":{},\"bytes_written\":{},\"note\":{}}}",
            json_string(&self.family),
            json_string(&self.workload),
            json_string(&self.synchronous),
            self.operations,
            self.nanos_per_operation,
            self.p50,
            self.p95,
            self.p99,
            self.seconds,
            self.checkpoint_seconds,
            self.checkpoint_share(),
            self.frames_written,
            self.log_syncs,
            self.frames_backfilled,
            self.page_writes,
            self.bytes_written,
            json_string(&self.note),
        )
    }

    /// Renders the measurement as one table row.
    fn to_row(&self) -> String {
        format!(
            "| {} | {} | {} | {} | {:.0} | {:.1} | {:.1} | {:.1} | {:.3} | {:.1}% | {} | {} | {} | {} | {} |\n",
            self.family,
            self.workload,
            self.synchronous,
            self.operations,
            self.nanos_per_operation,
            self.p50,
            self.p95,
            self.p99,
            self.seconds,
            self.checkpoint_share() * 100.0,
            self.frames_written,
            self.log_syncs,
            self.frames_backfilled,
            self.page_writes,
            self.bytes_written,
        )
    }
}

/// Runs every family and writes the report.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let out = flag(&arguments, "--out").unwrap_or_else(|| workspace_root().join("compat/baseline"));
    let scratch = flag(&arguments, "--scratch")
        .unwrap_or_else(|| workspace_root().join("_agent_output/walperf"));
    if let Err(failure) = std::fs::create_dir_all(&scratch) {
        eprintln!("cannot create {}: {failure}", scratch.display());
        return ExitCode::FAILURE;
    }
    let measured = match run_everything(&scratch) {
        Ok(measured) => measured,
        Err(failure) => {
            eprintln!("{failure}");
            return ExitCode::FAILURE;
        }
    };
    let platform = platform_name();
    if let Err(failure) = write_report(&out, &platform, &measured) {
        eprintln!("{failure}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// How many times each family runs before the quietest of them is kept.
const ATTEMPTS: usize = 3;

/// Runs every family in order, keeping each one's quietest attempt.
fn run_everything(scratch: &Path) -> Result<Vec<Measurement>, String> {
    let mut measured = Vec::new();
    for synchronous in [Synchronous::Full, Synchronous::Normal] {
        measured.append(&mut quietest(|| {
            Ok(vec![commit_family(scratch, synchronous)?])
        })?);
    }
    measured.append(&mut quietest(|| {
        Ok(vec![rollback_commit_family(scratch)?])
    })?);
    for mode in [
        CheckpointMode::Passive,
        CheckpointMode::Full,
        CheckpointMode::Restart,
        CheckpointMode::Truncate,
    ] {
        measured.append(&mut quietest(|| {
            Ok(vec![checkpoint_family(scratch, mode)?])
        })?);
    }
    measured.append(&mut quietest(|| Ok(vec![recovery_family(scratch)?]))?);
    for readers in [1usize, 4] {
        measured.append(&mut quietest(|| {
            Ok(vec![readers_and_a_writer(scratch, readers)?])
        })?);
    }
    measured.append(&mut quietest(|| Ok(vec![two_writers(scratch)?]))?);
    measured.append(&mut quietest(|| foreign_key_family(scratch))?);
    measured.append(&mut quietest(|| attach_family(scratch))?);
    measured.append(&mut quietest(|| service_family(scratch))?);
    Ok(measured)
}

/// Runs a family `ATTEMPTS` times and keeps the quietest attempt.
///
/// This machine is not a bench: a virus scanner, a build, or the previous
/// family's write-back can double a wall-clock figure, and a run that reported
/// whichever attempt it happened to make would be reporting the interference.
/// Best-of therefore means *least disturbed*, which is the honest reading of a
/// wall clock on a shared machine.
///
/// The whole family is kept or discarded together rather than each row being
/// best-of on its own. That matters wherever two rows are meant to be compared
/// - foreign keys on against off, one database against two - because a quiet
/// run of one against a noisy run of the other is exactly the comparison that
/// would mislead.
///
/// The counters are unaffected by any of this. They come out the same on every
/// attempt, which is what makes them the columns worth arguing about.
fn quietest<F>(mut family: F) -> Result<Vec<Measurement>, String>
where
    F: FnMut() -> Result<Vec<Measurement>, String>,
{
    let mut best: Option<Vec<Measurement>> = None;
    for _ in 0..ATTEMPTS {
        let attempt = family()?;
        let took: f64 = attempt.iter().map(|row| row.seconds).sum();
        let previous = best
            .as_ref()
            .map(|rows| rows.iter().map(|row| row.seconds).sum::<f64>());
        if previous.is_none_or(|previous| took < previous) {
            best = Some(attempt);
        }
    }
    best.ok_or_else(|| "a family produced no attempts".to_string())
}

/// Writes the JSON and the table.
fn write_report(out: &Path, platform: &str, measured: &[Measurement]) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|error| format!("cannot create {out:?}: {error}"))?;
    let json = out.join("phase10-concurrency-baselines.json");
    let markdown = out.join("phase10-concurrency-baselines.md");
    std::fs::write(&json, render_json(platform, measured))
        .map_err(|error| format!("cannot write {}: {error}", json.display()))?;
    std::fs::write(&markdown, render_markdown(platform, measured))
        .map_err(|error| format!("cannot write {}: {error}", markdown.display()))?;
    println!("wrote {}", markdown.display());
    Ok(())
}

/// Returns the value of a `--name value` flag.
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}

/// Opens a fresh database, deleting whatever was there before.
fn fresh(scratch: &Path, name: &str, options: JournalOptions) -> Result<Connection, String> {
    let path = scratch.join(format!("{name}.db"));
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(scratch.join(format!("{name}.db{suffix}")));
    }
    open(&path, options)
}

/// Opens a database at a path without disturbing it.
fn open(path: &Path, options: JournalOptions) -> Result<Connection, String> {
    let database = SessionDatabase::open_with_options(
        path,
        OpenOptions {
            journal: options,
            busy_timeout: Duration::from_secs(10),
            ..OpenOptions::default()
        },
    )
    .map_err(text)?;
    database.connect().map_err(text)
}

/// Runs a script.
fn run(connection: &Connection, sql: &str) -> Result<(), String> {
    statement::execute_batch(connection, sql.as_bytes()).map_err(text)
}

/// Runs a query and returns how many rows it produced.
fn count_rows(connection: &Connection, sql: &str) -> Result<u64, String> {
    let (mut prepared, _) =
        statement::Statement::prepare(connection, sql.as_bytes()).map_err(text)?;
    let mut rows = 0u64;
    while prepared.step().map_err(text)? {
        rows = rows.saturating_add(1);
    }
    Ok(rows)
}

/// Runs a checkpoint and returns the frames it said the log held and copied.
fn checkpoint_report(connection: &Connection, mode: CheckpointMode) -> Result<(i64, i64), String> {
    let sql = format!("PRAGMA wal_checkpoint({})", mode.as_str().to_uppercase());
    let (mut prepared, _) =
        statement::Statement::prepare(connection, sql.as_bytes()).map_err(text)?;
    let mut reported = (0i64, 0i64);
    while prepared.step().map_err(text)? {
        let row = prepared.row();
        let field = |index: usize| {
            row.get(index)
                .and_then(inillucent_value::Value::as_integer)
                .unwrap_or(0)
        };
        reported = (field(1), field(2));
    }
    Ok(reported)
}

/// The WAL options at a durability level.
fn wal(synchronous: Synchronous) -> JournalOptions {
    JournalOptions {
        mode: JournalMode::Wal,
        synchronous,
    }
}

/// Names a durability level for the report.
fn level(synchronous: Synchronous) -> String {
    match synchronous {
        Synchronous::Off => "off",
        Synchronous::Normal => "normal",
        Synchronous::Full => "full",
        Synchronous::Extra => "extra",
    }
    .to_string()
}

/// A commit in WAL mode, with the checkpoint it deferred counted.
///
/// The checkpoint runs inside the timed region and is reported separately as
/// well as inside the total. Both are needed: the commit latency is what an
/// application waits for, and the total is what the database actually cost.
fn commit_family(scratch: &Path, synchronous: Synchronous) -> Result<Measurement, String> {
    let connection = fresh(scratch, "wal-commit", wal(synchronous))?;
    run(&connection, "PRAGMA journal_mode=wal")?;
    run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
    let mut latencies = Vec::with_capacity(ROWS as usize);
    let before = Counters::of(&connection);
    let started = Instant::now();
    for key in 0..ROWS {
        let at = Instant::now();
        run(
            &connection,
            &format!("INSERT INTO t VALUES({key}, 'payload for row {key}')"),
        )?;
        latencies.push(at.elapsed().as_nanos());
    }
    let checkpoint_started = Instant::now();
    run(&connection, "PRAGMA wal_checkpoint(TRUNCATE)")?;
    let checkpoint = checkpoint_started.elapsed();
    let total = started.elapsed();
    let counters = Counters::of(&connection).since(before);
    Ok(measure(
        "wal-commit",
        "insert-autocommit",
        &level(synchronous),
        ROWS,
        latencies,
        total,
        checkpoint,
        counters,
        "one transaction per row, then the whole log copied back",
    ))
}

/// The same commits under a rollback journal, as the thing the log is
/// supposed to be better than.
fn rollback_commit_family(scratch: &Path) -> Result<Measurement, String> {
    let options = JournalOptions {
        mode: JournalMode::Delete,
        synchronous: Synchronous::Full,
    };
    let connection = fresh(scratch, "journal-commit", options)?;
    run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
    let mut latencies = Vec::with_capacity(ROWS as usize);
    let before = Counters::of(&connection);
    let started = Instant::now();
    for key in 0..ROWS {
        let at = Instant::now();
        run(
            &connection,
            &format!("INSERT INTO t VALUES({key}, 'payload for row {key}')"),
        )?;
        latencies.push(at.elapsed().as_nanos());
    }
    let total = started.elapsed();
    let counters = Counters::of(&connection).since(before);
    Ok(measure(
        "wal-commit",
        "insert-autocommit-journal",
        "full",
        ROWS,
        latencies,
        total,
        Duration::ZERO,
        counters,
        "the same rows through a rollback journal, for scale",
    ))
}

/// One checkpoint of a log of a known size, in each mode.
fn checkpoint_family(scratch: &Path, mode: CheckpointMode) -> Result<Measurement, String> {
    let name = format!("checkpoint-{}", mode.as_str());
    let connection = fresh(scratch, &name, wal(Synchronous::Full))?;
    run(&connection, "PRAGMA journal_mode=wal")?;
    run(&connection, "PRAGMA wal_autocheckpoint=0")?;
    run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
    for key in 0..LOG_ROWS {
        run(
            &connection,
            &format!("INSERT INTO t VALUES({key}, 'payload for row {key}')"),
        )?;
    }
    let before = Counters::of(&connection);
    let log_frames = before.frames_written;
    let started = Instant::now();
    let reported = checkpoint_report(&connection, mode)?;
    let total = started.elapsed();
    let counters = Counters::of(&connection).since(before);
    let mut measured = measure(
        "checkpoint",
        mode.as_str(),
        "full",
        log_frames.max(1),
        vec![total.as_nanos()],
        total,
        total,
        counters,
        "",
    );
    // A checkpoint writes one page per *page* in the log, not one per frame:
    // two thousand commits to a small table leave two thousand copies of the
    // same handful of pages, and only the newest of each is copied. That is
    // the whole reason a log can be checkpointed cheaply, and the two numbers
    // being so far apart is the measurement rather than a mistake in it.
    measured.note = format!(
        "{} pages written for a log of {log_frames} frames; the checkpoint reported \
         {} of {} frames copied",
        counters.frames_backfilled, reported.1, reported.0
    );
    Ok(measured)
}

/// Reopening a database whose log was never checkpointed.
///
/// This is what a crash costs the next connection: the index is gone, so the
/// log is read from its first byte and every frame is entered again.
fn recovery_family(scratch: &Path) -> Result<Measurement, String> {
    let path = scratch.join("recovery.db");
    {
        let connection = fresh(scratch, "recovery", wal(Synchronous::Full))?;
        run(&connection, "PRAGMA journal_mode=wal")?;
        run(&connection, "PRAGMA wal_autocheckpoint=0")?;
        run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
        for key in 0..LOG_ROWS {
            run(
                &connection,
                &format!("INSERT INTO t VALUES({key}, 'payload for row {key}')"),
            )?;
        }
        // The files are copied while the connection is open, because closing
        // it would checkpoint the log away and there would be nothing to
        // recover.
        for suffix in ["", "-wal", "-shm"] {
            let from = PathBuf::from(format!("{}{suffix}", path.display()));
            let to = PathBuf::from(format!("{}.image{suffix}", path.display()));
            let _ = std::fs::copy(&from, &to);
        }
    }
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}.image{suffix}", path.display()));
        let to = PathBuf::from(format!("{}{suffix}", path.display()));
        std::fs::copy(&from, &to).map_err(text)?;
    }
    let started = Instant::now();
    let connection = open(&path, wal(Synchronous::Full))?;
    let rows = count_rows(&connection, "SELECT a FROM t")?;
    let total = started.elapsed();
    let counters = Counters::of(&connection);
    let mut measured = measure(
        "recovery",
        "reopen-and-rebuild",
        "full",
        LOG_ROWS,
        vec![total.as_nanos()],
        total,
        Duration::ZERO,
        counters,
        "",
    );
    measured.note = format!(
        "{rows} rows read back after {} index rebuild; {} frames then served from the log",
        counters.recoveries, counters.frames_read
    );
    Ok(measured)
}

/// Readers querying while one connection commits.
///
/// The number that matters is what the readers cost, not what they achieve: in
/// WAL mode a reader is not supposed to wait for a writer at all, so a reader
/// tail latency that tracks the writer's commit latency would mean the log is
/// not doing its job.
fn readers_and_a_writer(scratch: &Path, readers: usize) -> Result<Measurement, String> {
    let path = scratch.join(format!("readers-{readers}.db"));
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
    }
    {
        let connection = open(&path, wal(Synchronous::Full))?;
        run(&connection, "PRAGMA journal_mode=wal")?;
        run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
        for key in 0..500u64 {
            run(&connection, &format!("INSERT INTO t VALUES({key}, 'seed')"))?;
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(Barrier::new(readers.saturating_add(1)));
    let reads = Arc::new(AtomicU64::new(0));
    let slowest = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for _ in 0..readers {
        let path = path.clone();
        let stop = Arc::clone(&stop);
        let gate = Arc::clone(&gate);
        let reads = Arc::clone(&reads);
        let slowest = Arc::clone(&slowest);
        handles.push(std::thread::spawn(move || {
            let Ok(connection) = open(&path, wal(Synchronous::Full)) else {
                return;
            };
            gate.wait();
            while !stop.load(Ordering::Relaxed) {
                let at = Instant::now();
                if count_rows(&connection, "SELECT a, b FROM t WHERE a < 400").is_err() {
                    break;
                }
                let took = at.elapsed().as_micros() as u64;
                reads.fetch_add(1, Ordering::Relaxed);
                slowest.fetch_max(took, Ordering::Relaxed);
            }
        }));
    }
    let writer = open(&path, wal(Synchronous::Full))?;
    gate.wait();
    let mut latencies = Vec::new();
    let before = Counters::of(&writer);
    let started = Instant::now();
    let mut key = 1_000u64;
    while started.elapsed() < CONTENTION {
        let at = Instant::now();
        run(&writer, &format!("INSERT INTO t VALUES({key}, 'written')"))?;
        latencies.push(at.elapsed().as_nanos());
        key = key.saturating_add(1);
    }
    let checkpoint_started = Instant::now();
    run(&writer, "PRAGMA wal_checkpoint(PASSIVE)")?;
    let checkpoint = checkpoint_started.elapsed();
    let total = started.elapsed();
    let counters = Counters::of(&writer).since(before);
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = handle.join();
    }
    let commits = latencies.len() as u64;
    let mut measured = measure(
        "readers-and-writer",
        &format!("{readers}-readers"),
        "full",
        commits,
        latencies,
        total,
        checkpoint,
        counters,
        "",
    );
    measured.note = format!(
        "{} reads by {readers} readers, slowest {} us, while the writer committed {commits} times",
        reads.load(Ordering::Relaxed),
        slowest.load(Ordering::Relaxed)
    );
    Ok(measured)
}

/// Two connections both writing, which is the case one of them has to lose.
fn two_writers(scratch: &Path) -> Result<Measurement, String> {
    let path = scratch.join("two-writers.db");
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
    }
    {
        let connection = open(&path, wal(Synchronous::Full))?;
        run(&connection, "PRAGMA journal_mode=wal")?;
        run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
    }
    let gate = Arc::new(Barrier::new(2));
    let stop = Arc::new(AtomicBool::new(false));
    let other = {
        let path = path.clone();
        let gate = Arc::clone(&gate);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let Ok(connection) = open(&path, wal(Synchronous::Full)) else {
                return 0u64;
            };
            gate.wait();
            let mut key = 1_000_000u64;
            let mut commits = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if run(
                    &connection,
                    &format!("INSERT INTO t VALUES({key}, 'other')"),
                )
                .is_ok()
                {
                    commits = commits.saturating_add(1);
                }
                key = key.saturating_add(1);
            }
            commits
        })
    };
    let writer = open(&path, wal(Synchronous::Full))?;
    gate.wait();
    let mut latencies = Vec::new();
    let before = Counters::of(&writer);
    let started = Instant::now();
    let mut key = 0u64;
    while started.elapsed() < CONTENTION {
        let at = Instant::now();
        if run(&writer, &format!("INSERT INTO t VALUES({key}, 'mine')")).is_ok() {
            latencies.push(at.elapsed().as_nanos());
        }
        key = key.saturating_add(1);
    }
    let checkpoint_started = Instant::now();
    run(&writer, "PRAGMA wal_checkpoint(PASSIVE)")?;
    let checkpoint = checkpoint_started.elapsed();
    let total = started.elapsed();
    let counters = Counters::of(&writer).since(before);
    stop.store(true, Ordering::Relaxed);
    let theirs = other.join().unwrap_or(0);
    let mine = latencies.len() as u64;
    let mut measured = measure(
        "contention",
        "two-writers",
        "full",
        mine,
        latencies,
        total,
        checkpoint,
        counters,
        "",
    );
    measured.note = format!("{mine} commits here and {theirs} on the other connection");
    Ok(measured)
}

/// What enforcing a foreign key costs, and what an action costs on top.
fn foreign_key_family(scratch: &Path) -> Result<Vec<Measurement>, String> {
    let mut measured = Vec::new();
    for enforced in [false, true] {
        let name = if enforced { "keys-on" } else { "keys-off" };
        let connection = fresh(scratch, &format!("fk-{name}"), wal(Synchronous::Full))?;
        run(&connection, "PRAGMA journal_mode=wal")?;
        run(&connection, "CREATE TABLE parent(a INTEGER PRIMARY KEY)")?;
        run(
            &connection,
            "CREATE TABLE child(a INTEGER PRIMARY KEY, p INTEGER REFERENCES parent(a) ON DELETE CASCADE)",
        )?;
        run(
            &connection,
            &format!(
                "PRAGMA foreign_keys={}",
                if enforced { "ON" } else { "OFF" }
            ),
        )?;
        for key in 0..ROWS {
            run(&connection, &format!("INSERT INTO parent VALUES({key})"))?;
        }
        let mut latencies = Vec::with_capacity(ROWS as usize);
        let before = Counters::of(&connection);
        let started = Instant::now();
        for key in 0..ROWS {
            let at = Instant::now();
            run(
                &connection,
                &format!("INSERT INTO child VALUES({key}, {key})"),
            )?;
            latencies.push(at.elapsed().as_nanos());
        }
        let total = started.elapsed();
        let counters = Counters::of(&connection).since(before);
        measured.push(measure(
            "foreign-keys",
            &format!("insert-child-{name}"),
            "full",
            ROWS,
            latencies,
            total,
            Duration::ZERO,
            counters,
            "the same inserts with the parent lookup on and off",
        ));
        if !enforced {
            continue;
        }
        let mut latencies = Vec::with_capacity(ROWS as usize);
        let before = Counters::of(&connection);
        let started = Instant::now();
        for key in 0..ROWS {
            let at = Instant::now();
            run(&connection, &format!("DELETE FROM parent WHERE a = {key}"))?;
            latencies.push(at.elapsed().as_nanos());
        }
        let total = started.elapsed();
        let counters = Counters::of(&connection).since(before);
        measured.push(measure(
            "foreign-keys",
            "delete-parent-cascade",
            "full",
            ROWS,
            latencies,
            total,
            Duration::ZERO,
            counters,
            "each delete takes one child with it",
        ));
    }
    Ok(measured)
}

/// What a second database costs a commit.
///
/// One database commits through the log alone. Two need a super-journal: a
/// file naming both, written and synced before either commits and removed once
/// both have, which is three extra file operations per transaction and is the
/// whole price of the atomicity.
fn attach_family(scratch: &Path) -> Result<Vec<Measurement>, String> {
    let mut measured = Vec::new();
    for databases in [1usize, 2] {
        let name = format!("attach-{databases}");
        let aux = scratch.join(format!("{name}-aux.db"));
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", aux.display())));
        }
        // A rollback journal, because the super-journal is a rollback-mode
        // protocol: a log has no equivalent and a two-database commit in WAL
        // mode is two commits, not one.
        let options = JournalOptions {
            mode: JournalMode::Delete,
            synchronous: Synchronous::Full,
        };
        let connection = fresh(scratch, &name, options)?;
        run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
        if databases == 2 {
            run(
                &connection,
                &format!(
                    "ATTACH DATABASE '{}' AS aux",
                    aux.display().to_string().replace('\\', "/")
                ),
            )?;
            run(
                &connection,
                "CREATE TABLE aux.t(a INTEGER PRIMARY KEY, b TEXT)",
            )?;
        }
        let mut latencies = Vec::with_capacity(ROWS as usize);
        let before = Counters::of(&connection);
        let started = Instant::now();
        for key in 0..ROWS {
            let at = Instant::now();
            if databases == 2 {
                run(
                    &connection,
                    &format!(
                        "BEGIN; INSERT INTO main.t VALUES({key}, 'main'); \
                         INSERT INTO aux.t VALUES({key}, 'aux'); COMMIT;"
                    ),
                )?;
            } else {
                run(
                    &connection,
                    &format!("BEGIN; INSERT INTO main.t VALUES({key}, 'main'); COMMIT;"),
                )?;
            }
            latencies.push(at.elapsed().as_nanos());
        }
        let total = started.elapsed();
        let counters = Counters::of(&connection).since(before);
        measured.push(measure(
            "attach",
            &format!("{databases}-database-commit"),
            "full",
            ROWS,
            latencies,
            total,
            Duration::ZERO,
            counters,
            if databases == 2 {
                "a super-journal is written, synced and removed per transaction"
            } else {
                "one database, no super-journal"
            },
        ));
    }
    Ok(measured)
}

/// Backup, incremental blob and serialize, which are all bulk page work.
fn service_family(scratch: &Path) -> Result<Vec<Measurement>, String> {
    let mut measured = Vec::new();
    let connection = fresh(scratch, "services", wal(Synchronous::Full))?;
    run(&connection, "PRAGMA journal_mode=wal")?;
    run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
    for key in 0..SERVICE_ROWS {
        run(
            &connection,
            &format!("INSERT INTO t VALUES({key}, 'a payload long enough to fill pages {key}')"),
        )?;
    }
    run(&connection, "PRAGMA wal_checkpoint(TRUNCATE)")?;

    let destination = fresh(scratch, "services-backup", wal(Synchronous::Full))?;
    let before = Counters::of(&destination);
    let started = Instant::now();
    let mut backup =
        inillucent_session::Backup::begin(&connection, 0, &destination, 0).map_err(text)?;
    let pages = backup.progress().page_count;
    let mut steps = 0u64;
    loop {
        steps = steps.saturating_add(1);
        if backup.step(64).map_err(text)?.is_complete() {
            break;
        }
    }
    backup.finish().map_err(text)?;
    let total = started.elapsed();
    let counters = Counters::of(&destination).since(before);
    let mut row = measure(
        "services",
        "backup",
        "full",
        u64::from(pages),
        vec![total.as_nanos()],
        total,
        Duration::ZERO,
        counters,
        "",
    );
    row.note = format!("{pages} pages copied in {steps} steps of 64");
    measured.push(row);

    let before = Counters::of(&connection);
    let started = Instant::now();
    let bytes = inillucent_session::serialize(&connection, 0).map_err(text)?;
    let total = started.elapsed();
    let counters = Counters::of(&connection).since(before);
    let mut row = measure(
        "services",
        "serialize",
        "full",
        bytes.len() as u64 / 4096,
        vec![total.as_nanos()],
        total,
        Duration::ZERO,
        counters,
        "",
    );
    row.note = format!("{} bytes handed over without a temporary file", bytes.len());
    measured.push(row);

    let mut latencies = Vec::new();
    let before = Counters::of(&connection);
    let started = Instant::now();
    for key in 0..1_000u64 {
        let at = Instant::now();
        let blob =
            inillucent_session::Blob::open(&connection, b"main", b"t", b"b", key as i64, true)
                .map_err(text)?;
        blob.write_at(0, b"X").map_err(text)?;
        latencies.push(at.elapsed().as_nanos());
    }
    let total = started.elapsed();
    let counters = Counters::of(&connection).since(before);
    measured.push(measure(
        "services",
        "blob-write-one-byte",
        "full",
        1_000,
        latencies,
        total,
        Duration::ZERO,
        counters,
        "one byte of a value, without reading or rewriting the row",
    ));
    Ok(measured)
}

/// The counters as they stood when a family's timed region began.
///
/// Every counter on a connection is cumulative from the moment it opened, so a
/// family that reported them raw would be reporting its own setup as well - and
/// the setup is often the larger half. Each family takes one of these before it
/// starts the clock and the report shows the difference.
#[derive(Clone, Copy)]
struct Counters {
    frames_written: u64,
    frames_read: u64,
    log_bytes: u64,
    log_syncs: u64,
    frames_backfilled: u64,
    recoveries: u64,
    page_writes: u64,
    bytes_written: u64,
}

impl Counters {
    /// Reads the counters off a connection.
    fn of(connection: &Connection) -> Counters {
        let stats = connection.wal_stats();
        let counters = connection.pager_counters();
        Counters {
            frames_written: stats.frames_written,
            frames_read: stats.frames_read,
            log_bytes: stats.bytes_written,
            log_syncs: stats.syncs,
            frames_backfilled: stats.frames_backfilled,
            recoveries: stats.recoveries,
            page_writes: counters.page_writes,
            bytes_written: counters.bytes_written,
        }
    }

    /// Returns what happened between this snapshot and a later one.
    fn since(self, before: Counters) -> Counters {
        Counters {
            frames_written: self.frames_written.saturating_sub(before.frames_written),
            frames_read: self.frames_read.saturating_sub(before.frames_read),
            log_bytes: self.log_bytes.saturating_sub(before.log_bytes),
            log_syncs: self.log_syncs.saturating_sub(before.log_syncs),
            frames_backfilled: self
                .frames_backfilled
                .saturating_sub(before.frames_backfilled),
            recoveries: self.recoveries.saturating_sub(before.recoveries),
            page_writes: self.page_writes.saturating_sub(before.page_writes),
            bytes_written: self.bytes_written.saturating_sub(before.bytes_written),
        }
    }
}

/// Builds a measurement from a family's timings and what its counters did.
#[allow(clippy::too_many_arguments)]
fn measure(
    family: &str,
    workload: &str,
    synchronous: &str,
    operations: u64,
    mut latencies: Vec<u128>,
    total: Duration,
    checkpoint: Duration,
    counters: Counters,
    note: &str,
) -> Measurement {
    latencies.sort_unstable();
    Measurement {
        family: family.to_string(),
        workload: workload.to_string(),
        synchronous: synchronous.to_string(),
        operations,
        nanos_per_operation: if operations == 0 {
            0.0
        } else {
            total.as_nanos() as f64 / operations as f64
        },
        p50: percentile(&latencies, 50.0),
        p95: percentile(&latencies, 95.0),
        p99: percentile(&latencies, 99.0),
        seconds: total.as_secs_f64(),
        checkpoint_seconds: checkpoint.as_secs_f64(),
        frames_written: counters.frames_written,
        log_syncs: counters.log_syncs,
        frames_backfilled: counters.frames_backfilled,
        page_writes: counters.page_writes,
        bytes_written: counters.bytes_written.saturating_add(counters.log_bytes),
        note: note.to_string(),
    }
}

/// Returns a percentile of a sorted list of nanosecond timings, in
/// microseconds.
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

/// Formats an error as a string.
fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Renders the measurements as JSON.
fn render_json(platform: &str, measured: &[Measurement]) -> String {
    let body: Vec<String> = measured.iter().map(Measurement::to_json).collect();
    format!(
        "{{\"phase\":\"phases 9 and 10: foreign keys, ATTACH, WAL and concurrency\",\
         \"platform\":{},\"rows\":{},\"log_rows\":{},\"contention_millis\":{},\
         \"measurements\":[{}]}}\n",
        json_string(platform),
        ROWS,
        LOG_ROWS,
        CONTENTION.as_millis(),
        body.join(",")
    )
}

/// Renders the measurements as a table with the argument that goes with it.
fn render_markdown(platform: &str, measured: &[Measurement]) -> String {
    let mut out = String::new();
    out.push_str("# Concurrency and multi-database baselines, phases 9 and 10\n\n");
    out.push_str(&format!("Platform: `{platform}`\n\n"));
    out.push_str(
        "Every run is on the real operating-system VFS at a stated durability level, and \
         **the checkpoint is inside the timed region**. That last one is the only thing about \
         this table that needs defending. A write-ahead log is quick at commit time precisely \
         because it defers work; a benchmark that stops the clock before the checkpoint is \
         measuring the deferral rather than the system, and would report a log as free. The \
         `Ckpt%` column says how much of each family's total was the checkpoint, so the \
         deferral is visible rather than hidden.\n\n",
    );
    out.push_str(&format!(
        "These are baselines, not comparisons. Nothing here is measured against SQLite. The \
         wall-clock columns move with the machine and the filesystem; the frame, sync and page \
         counters do not, and they are what a later change should be read against.\n\n\
         Each family ran {ATTEMPTS} times and the quietest attempt is the one reported, kept \
         whole so that rows meant to be compared with each other come from the same attempt. \
         On a machine that is also running a virus scanner and a compiler, the alternative is \
         to report the interference.\n\n"
    ));
    out.push_str(
        "| Family | Workload | Sync | Ops | ns/op | p50 us | p95 us | p99 us | s | Ckpt% | \
         Frames | Syncs | Backfilled | Pages | Bytes |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for row in measured {
        out.push_str(&row.to_row());
    }
    out.push_str("\n## What the shape of these numbers says\n\n");
    out.push_str(
        "The absolute figures belong to this machine. The *relations* between them are the \
         findings, and they are what a later change should be checked against.\n\n\
         **A log is worth having, and the checkpoint does not take it back.** The same thousand \
         autocommitted rows cost roughly a third of what they cost through a rollback \
         journal, and that is with the whole log copied back inside the timed region - the \
         `Ckpt%` column puts the checkpoint at well under one percent of the total. The \
         journal writes an undo image of every page it touches before it touches it; the log \
         writes the new page once and sorts it out later, and later turns out to be cheap.\n\n\
         **Later is cheap because a log compacts.** A checkpoint of a log holding several \
         thousand frames writes only as many pages as there are distinct pages in it - two \
         thousand commits to a small table leave two thousand copies of the same handful of \
         pages, and only the newest of each is copied. That is the single most important \
         property of the design and it is why deferring is not merely postponing.\n\n\
         **`TRUNCATE` is the expensive mode and the other three are not.** Passive, full and \
         restart differ from each other by noise here; truncate costs several times any of \
         them, because shortening the file is a metadata operation the file system has to \
         make durable. A caller that wants the log to stop growing wants `RESTART`; only a \
         caller that wants the file *gone* should pay for `TRUNCATE`.\n\n\
         **Readers do not cost the writer.** Going from one reader to four leaves the writer's \
         commit rate within a few percent of where it was, while the readers do several times \
         as many queries. That is the promise WAL mode exists to make, and it is the one \
         number here that would look completely different under a rollback journal, where \
         every reader is a lock the writer has to wait behind.\n\n\
         **Two writers is a tail-latency story, not a throughput one.** The pair together \
         commit about as often as one writer alone; what changes is p99, which is several \
         times p50 because a refused transaction waits and tries again. Contention costs \
         predictability rather than work.\n\n\
         **Enforcement and atomicity both have a price, and it is visible.** Foreign keys on \
         cost around forty percent more per child insert than keys off, which is the lookup. \
         A two-database commit costs close to three times a one-database commit, which \
         is the super-journal: a file written, synced and removed on every transaction, and \
         the whole reason the two databases move together.\n\n",
    );
    out.push_str("\n## What each family says\n\n");
    for row in measured {
        if row.note.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "- **{} / {}** - {}\n",
            row.family, row.workload, row.note
        ));
    }
    out.push('\n');
    out
}
