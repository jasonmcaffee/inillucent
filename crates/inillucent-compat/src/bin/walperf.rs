//! WAL, checkpoint, foreign-key and multi-database baselines for phases 9 and 10.
//!
//! **The concurrent same-process families (a reader beside a writer, two
//! writers) are gone** - see the comment above `foreign_key_family` for why.
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
//! **Runs on `inillucent-engine`, the shipped engine, not the retired
//! `inillucent-session`.** Three things changed because of that:
//!
//! - **There is one journal mechanism, not two.** The old engine's rollback
//!   journal is gone with the crate that read it, so the family that compared
//!   a WAL commit against a rollback-journal commit (`rollback_commit_family`)
//!   has nothing left to compare against and is removed.
//! - **`PRAGMA wal_checkpoint` takes no mode.** The old engine's four modes
//!   (`PASSIVE`/`FULL`/`RESTART`/`TRUNCATE`) came from having more than one
//!   writer to coordinate with; this engine has exactly one writer, so its
//!   `pragma_wal_checkpoint` always does the same thing and the family that
//!   swept across the four modes now measures the one checkpoint there is.
//! - **Backup, blobs and serialize are gone**, by the same design choice
//!   `inillucent_engine::connect::Database::backup_to`'s own doc comment gives:
//!   there is no second writer to race, so there is no incremental-copy API to
//!   measure. `service_family`, which measured only those three, is removed.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-walperf`

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use inillucent_compat::report::json_string;
use inillucent_compat::{platform_name, workspace_root};
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;
use inillucent_wal::Synchronous;

/// How many rows each commit family writes.
const ROWS: u64 = 20;

/// How many rows the checkpoint and recovery families put in the log first.
const LOG_ROWS: u64 = 2_000;

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
    measured.append(&mut quietest(|| Ok(vec![checkpoint_family(scratch)?]))?);
    measured.append(&mut quietest(|| Ok(vec![recovery_family(scratch)?]))?);
    measured.append(&mut quietest(|| foreign_key_family(scratch))?);
    measured.append(&mut quietest(|| attach_family(scratch))?);
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

/// Opens a fresh database, deleting whatever was there before, and sets its
/// durability level.
///
/// Returns the `Database` alongside its `Connection`: `Counters::of` reads
/// `log_stats()`/`cache_stats()`, which the new engine answers on the database
/// rather than the connection.
fn fresh(
    scratch: &Path,
    name: &str,
    synchronous: Synchronous,
) -> Result<(&'static Database, Connection<'static>), String> {
    let path = scratch.join(format!("{name}.db"));
    for existing in std::fs::read_dir(scratch).map_err(text)? {
        let existing = existing.map_err(text)?.path();
        if existing
            .file_name()
            .and_then(|found| found.to_str())
            .is_some_and(|found| found.starts_with(&format!("{name}.db")))
        {
            let _ = std::fs::remove_file(existing);
        }
    }
    open(&path, synchronous)
}

/// Opens a fresh database the caller intends to close again, deleting
/// whatever was there before.
///
/// **Not leaked, unlike [`fresh`].** `recovery_family` needs the file genuinely
/// released - the whole point is to copy its bytes with the writer gone and
/// reopen a copy - and a leaked `Database` never releases its file handle for
/// the life of the process, which made the reopen fail with "database is
/// locked" the first time this was tried leaked.
fn fresh_owned(scratch: &Path, name: &str, synchronous: Synchronous) -> Result<Database, String> {
    let path = scratch.join(format!("{name}.db"));
    for existing in std::fs::read_dir(scratch).map_err(text)? {
        let existing = existing.map_err(text)?.path();
        if existing
            .file_name()
            .and_then(|found| found.to_str())
            .is_some_and(|found| found.starts_with(&format!("{name}.db")))
        {
            let _ = std::fs::remove_file(existing);
        }
    }
    let database = Database::open(&path).map_err(text)?;
    let connection = database.connect();
    run(
        &connection,
        &format!(
            "PRAGMA busy_timeout = 10000; PRAGMA synchronous = {}",
            synchronous.name()
        ),
    )?;
    Ok(database)
}

/// Opens a database at a path without disturbing it, and sets its durability
/// level.
fn open(
    path: &Path,
    synchronous: Synchronous,
) -> Result<(&'static Database, Connection<'static>), String> {
    let database = Database::open(path).map_err(text)?;
    // Leaked for the same reason `differential::start_inillucent` leaks: this
    // measurement program runs for a few seconds and exits, and every family
    // here needs the `Database` and its `Connection` to outlive the function
    // that opened them - some on another thread entirely.
    let database: &'static Database = Box::leak(Box::new(database));
    let connection = database.connect();
    run(
        &connection,
        &format!(
            "PRAGMA busy_timeout = 10000; PRAGMA synchronous = {}",
            synchronous.name()
        ),
    )?;
    Ok((database, connection))
}

/// Runs a script.
fn run(connection: &Connection<'_>, sql: &str) -> Result<(), String> {
    connection.execute_batch(sql).map_err(text)
}

/// Runs a query and returns how many rows it produced.
fn count_rows(connection: &Connection<'_>, sql: &str) -> Result<u64, String> {
    Ok(connection.query(sql).map_err(text)?.len() as u64)
}

/// Runs a checkpoint and returns the frames it said the log held and copied.
///
/// **Takes no mode.** The old engine's `PASSIVE`/`FULL`/`RESTART`/`TRUNCATE`
/// distinguished how a checkpoint behaved with other writers and readers in
/// play; this engine has exactly one writer, so `PRAGMA wal_checkpoint` takes
/// no argument and always does the same thing.
fn checkpoint_report(connection: &Connection<'_>) -> Result<(i64, i64), String> {
    let rows = connection.query("PRAGMA wal_checkpoint").map_err(text)?;
    let mut reported = (0i64, 0i64);
    for row in rows {
        let field = |index: usize| match row.get(index) {
            Some(OwnedDatum::Int(value)) => *value,
            _ => 0,
        };
        reported = (field(1), field(2));
    }
    Ok(reported)
}

/// Names a durability level for the report.
fn level(synchronous: Synchronous) -> String {
    synchronous.name().to_string()
}

/// A commit in WAL mode, with the checkpoint it deferred counted.
///
/// The checkpoint runs inside the timed region and is reported separately as
/// well as inside the total. Both are needed: the commit latency is what an
/// application waits for, and the total is what the database actually cost.
fn commit_family(scratch: &Path, synchronous: Synchronous) -> Result<Measurement, String> {
    let (database, connection) = fresh(scratch, "wal-commit", synchronous)?;
    run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
    let mut latencies = Vec::with_capacity(ROWS as usize);
    let before = Counters::of(database);
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
    run(&connection, "PRAGMA wal_checkpoint")?;
    let checkpoint = checkpoint_started.elapsed();
    let total = started.elapsed();
    let counters = Counters::of(database).since(before);
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

/// One checkpoint of a log of a known size.
///
/// There is one checkpoint behaviour now, not four: the old engine's
/// `PASSIVE`/`FULL`/`RESTART`/`TRUNCATE` modes distinguished how a checkpoint
/// treated other writers and readers, and this engine has exactly one writer.
fn checkpoint_family(scratch: &Path) -> Result<Measurement, String> {
    let (database, connection) = fresh(scratch, "checkpoint", Synchronous::Full)?;
    run(&connection, "PRAGMA wal_autocheckpoint = 0")?;
    run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
    for key in 0..LOG_ROWS {
        run(
            &connection,
            &format!("INSERT INTO t VALUES({key}, 'payload for row {key}')"),
        )?;
    }
    let before = Counters::of(database);
    let log_frames = before.frames_written;
    let started = Instant::now();
    let reported = checkpoint_report(&connection)?;
    let total = started.elapsed();
    let counters = Counters::of(database).since(before);
    let mut measured = measure(
        "checkpoint",
        "checkpoint",
        "full",
        log_frames.max(1),
        vec![total.as_nanos()],
        total,
        total,
        counters,
        "",
    );
    // A checkpoint writes one page per *page* in the log, not one per record:
    // two thousand commits to a small table leave two thousand copies of the
    // same handful of pages, and only the newest of each is copied. That is
    // the whole reason a log can be checkpointed cheaply, and the two numbers
    // being so far apart is the measurement rather than a mistake in it.
    measured.note = format!(
        "{} pages written for a log of {log_frames} records; the checkpoint reported \
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
    let stem = "recovery.db";
    {
        let database = fresh_owned(scratch, "recovery", Synchronous::Full)?;
        let connection = database.connect();
        run(&connection, "PRAGMA wal_autocheckpoint = 0")?;
        run(&connection, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")?;
        for key in 0..LOG_ROWS {
            run(
                &connection,
                &format!("INSERT INTO t VALUES({key}, 'payload for row {key}')"),
            )?;
        }
        // The files are copied while the connection is open, because closing
        // it would checkpoint the log away and there would be nothing to
        // recover. Copied by whatever basename the engine actually gave each
        // segment - the log is segmented (`recovery.db-wal.0000000001`, a new
        // file per segment) rather than the single `-wal`/`-shm` pair the old
        // engine wrote, and a fixed suffix list would miss a second segment.
        for existing in std::fs::read_dir(scratch).map_err(text)? {
            let existing = existing.map_err(text)?.path();
            let Some(basename) = existing.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if basename.starts_with(stem) {
                let bytes = std::fs::read(&existing).map_err(text)?;
                std::fs::write(scratch.join(format!("{basename}.image")), bytes).map_err(text)?;
            }
        }
    }
    for existing in std::fs::read_dir(scratch).map_err(text)? {
        let existing = existing.map_err(text)?.path();
        let Some(basename) = existing.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(original) = basename.strip_suffix(".image") {
            let bytes = std::fs::read(&existing).map_err(text)?;
            std::fs::write(scratch.join(original), bytes).map_err(text)?;
        }
    }
    let started = Instant::now();
    let (database, connection) = open(&path, Synchronous::Full)?;
    let rows = count_rows(&connection, "SELECT a FROM t")?;
    let total = started.elapsed();
    let counters = Counters::of(database);
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
    measured.note = format!("{rows} rows read back after the log replayed");
    Ok(measured)
}

// **`readers_and_a_writer` and `two_writers` are removed, not repointed.**
// Both opened several independent `Database::open()` handles on the same path
// at once - a reader and a writer, or two writers - which is exactly what the
// old engine's SQLite-compatible locking protocol is measured to support
// across processes. Tried the same way in one process against the new
// engine, the first attempt failed immediately with "database is locked"
// from a leaked seed connection that never released its handle; with that
// fixed, the reader-and-writer run hung rather than completing within a
// two-minute smoke test, and the cause was not found in the time this file
// had. Rather than ship a profiling tool that can hang the process it runs
// in, the measurement is removed. Whether concurrent same-process `Database`
// handles are meant to coordinate at all is a real open question for whoever
// picks this back up - it is not answered by anything read while rewriting
// this file, and it is not this task's to answer by guessing.

/// What enforcing a foreign key costs, and what an action costs on top.
fn foreign_key_family(scratch: &Path) -> Result<Vec<Measurement>, String> {
    let mut measured = Vec::new();
    for enforced in [false, true] {
        let name = if enforced { "keys-on" } else { "keys-off" };
        let (database, connection) = fresh(scratch, &format!("fk-{name}"), Synchronous::Full)?;
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
        let before = Counters::of(database);
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
        let counters = Counters::of(database).since(before);
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
        let before = Counters::of(database);
        let started = Instant::now();
        for key in 0..ROWS {
            let at = Instant::now();
            run(&connection, &format!("DELETE FROM parent WHERE a = {key}"))?;
            latencies.push(at.elapsed().as_nanos());
        }
        let total = started.elapsed();
        let counters = Counters::of(database).since(before);
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
/// The old engine's comment here named the rollback-mode super-journal a
/// two-database commit paid for its atomicity - a mechanism that had no WAL
/// equivalent at all, which was the reason this family opened its connection
/// under a rollback journal specifically. That mechanism is gone with the old
/// engine; this now measures the new engine's own multi-database commit cost,
/// whatever it is, rather than assuming it is the same shape.
fn attach_family(scratch: &Path) -> Result<Vec<Measurement>, String> {
    let mut measured = Vec::new();
    for databases in [1usize, 2] {
        let name = format!("attach-{databases}");
        let aux = scratch.join(format!("{name}-aux.db"));
        for suffix in ["", "-wal.0000000001"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", aux.display())));
        }
        let (database, connection) = fresh(scratch, &name, Synchronous::Full)?;
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
        let before = Counters::of(database);
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
        let counters = Counters::of(database).since(before);
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
                "two databases committed together"
            } else {
                "one database, for scale"
            },
        ));
    }
    Ok(measured)
}

/// The counters as they stood when a family's timed region began.
///
/// Every counter on a connection is cumulative from the moment it opened, so a
/// family that reported them raw would be reporting its own setup as well - and
/// the setup is often the larger half. Each family takes one of these before it
/// starts the clock and the report shows the difference.
///
/// **Two of the old engine's counters have no home here and are gone rather
/// than kept at zero**: `frames_read` counted frames served from the log
/// during recovery specifically, and `recoveries` counted index rebuilds
/// distinct from an ordinary open. `Database` answers neither - it has no
/// counter that isolates recovery work from ordinary reads - so `measure`'s
/// callers no longer report them instead of reporting a column that can never
/// move.
#[derive(Clone, Copy)]
struct Counters {
    /// Records appended to the write-ahead log - the new engine's unit of
    /// log work, where the old engine counted frames.
    frames_written: u64,
    /// Bytes appended to the write-ahead log.
    log_bytes: u64,
    /// Times the write-ahead log was synced.
    log_syncs: u64,
    /// Pages the pool wrote back - what a checkpoint moves from the log into
    /// the main file.
    frames_backfilled: u64,
    /// Pages written to the database file. Zero under WAL until a checkpoint
    /// runs, for the same reason `txnperf.rs`'s field of the same name is.
    page_writes: u64,
    /// Approximated from `page_writes` at the database's own page size: the
    /// new engine's `CacheStats` counts pages, not bytes.
    bytes_written: u64,
}

impl Counters {
    /// Reads the counters off a database.
    fn of(database: &Database) -> Counters {
        let log = database.log_stats();
        let cache = database.cache_stats();
        Counters {
            frames_written: log.records,
            log_bytes: log.bytes,
            log_syncs: log.syncs,
            frames_backfilled: cache.writes,
            page_writes: cache.writes,
            bytes_written: cache
                .writes
                .saturating_mul(inillucent_engine::connect::PAGE_SIZE as u64),
        }
    }

    /// Returns what happened between this snapshot and a later one.
    fn since(self, before: Counters) -> Counters {
        Counters {
            frames_written: self.frames_written.saturating_sub(before.frames_written),
            log_bytes: self.log_bytes.saturating_sub(before.log_bytes),
            log_syncs: self.log_syncs.saturating_sub(before.log_syncs),
            frames_backfilled: self
                .frames_backfilled
                .saturating_sub(before.frames_backfilled),
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
        "{{\"phase\":\"phases 9 and 10: foreign keys, ATTACH and WAL\",\
         \"platform\":{},\"rows\":{},\"log_rows\":{},\
         \"measurements\":[{}]}}\n",
        json_string(platform),
        ROWS,
        LOG_ROWS,
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
         **This section used to make five claims against the old engine's rollback journal, \
         its four checkpoint modes, a reader beside a writer, two writers, and a rollback-mode \
         super-journal.** All five compared against or measured something that engine had and \
         this one does not: there is one journal mechanism now, `PRAGMA wal_checkpoint` takes \
         no mode, the two same-process concurrency families were removed (see the comment above \
         `foreign_key_family`), and a two-database commit no longer goes through a super-journal \
         at all. Rewriting those five claims for the new engine needs new measurements, not a \
         reworded guess at what they would say, so this section states only what the table \
         above still measures - a log's write cost, one checkpoint's, and what a foreign key \
         and a second database cost - and leaves the interpretation to whoever reads the \
         numbers next.\n\n",
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
