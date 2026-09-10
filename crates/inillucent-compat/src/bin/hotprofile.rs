//! Where a slow workload's time and allocations actually go.
//!
//! Invariant: this measures, it does not optimise, and it is not a comparison
//! against SQLite. The scorecard says *which* families lose; this says *why* one
//! query is slow, in the two currencies that turned out to matter - how many
//! heap allocations a row costs, and how many bytecode instructions it runs.
//!
//! The scorecard's own small-scale schema is rebuilt here so the numbers line up
//! with the ones the release candidate reports, rather than describing a
//! different table that happens to have the same name.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-hotprofile`

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use inillucent_legacy::{Connection, Database};

/// How many allocations the process has made.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
/// How many bytes those allocations asked for.
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

/// An allocator that counts, and otherwise delegates to the system one.
struct CountingAllocator;

// SAFETY: every method forwards to the system allocator with the same
// arguments; the counters are the only addition and they touch no memory the
// allocator owns.
unsafe impl GlobalAlloc for CountingAllocator {
    // SAFETY: the layout is the caller's, forwarded unchanged; the counting
    // happens before the allocation and reads nothing the allocator returns.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        inillucent_base::probe::ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    // SAFETY: the pointer and layout are the ones this allocator handed out,
    // which is the caller's obligation and is unchanged by the forwarding.
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.dealloc(pointer, layout) }
    }

    // SAFETY: the pointer and layout are the ones this allocator handed out,
    // and the new size is the caller's, all forwarded unchanged.
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        inillucent_base::probe::ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// One workload's profile.
struct Profile {
    /// The workload's name, as the scorecard names it.
    name: &'static str,
    /// The SQL it runs.
    sql: &'static str,
    /// How many times it ran.
    repeats: u64,
    /// How long every repeat took together.
    elapsed_nanos: u128,
    /// How many rows every repeat returned together.
    rows: u64,
    /// How many bytecode instructions every repeat ran together.
    steps: u64,
    /// How many heap allocations they cost.
    allocations: u64,
    /// How many bytes those allocations asked for.
    bytes: u64,
    /// What each opcode cost: its name, runs, nanoseconds and allocations.
    opcodes: Vec<(String, u64, u64, u64)>,
}

impl Profile {
    /// Returns the per-returned-row cost in nanoseconds.
    fn nanos_per_row(&self) -> f64 {
        self.elapsed_nanos as f64 / self.rows.max(1) as f64
    }

    /// Returns how many allocations one returned row cost.
    fn allocations_per_row(&self) -> f64 {
        self.allocations as f64 / self.rows.max(1) as f64
    }

    /// Returns how many allocations one bytecode instruction cost.
    fn allocations_per_step(&self) -> f64 {
        self.allocations as f64 / self.steps.max(1) as f64
    }

    /// Returns how many nanoseconds one bytecode instruction cost.
    fn nanos_per_step(&self) -> f64 {
        self.elapsed_nanos as f64 / self.steps.max(1) as f64
    }
}

fn main() -> ExitCode {
    let root = PathBuf::from("_agent_output/hotprofile");
    match run(&root) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("hotprofile: {failure}");
            ExitCode::FAILURE
        }
    }
}

/// Builds the scorecard's small database and profiles the slow workloads.
fn run(root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|failure| failure.to_string())?;
    let path = build_database(root)?;
    let database = Database::open(&path).map_err(|failure| failure.to_string())?;
    let connection = database.connect().map_err(|failure| failure.to_string())?;

    let workloads: Vec<(&'static str, &'static str, u64)> = vec![
        (
            "scan.aggregate",
            "SELECT count(*), sum(key), max(category) FROM main_table",
            40,
        ),
        (
            "scan.group",
            "SELECT category, count(*) FROM main_table GROUP BY category ORDER BY category",
            40,
        ),
        (
            "scan.sort",
            "SELECT id FROM main_table ORDER BY label LIMIT 100",
            40,
        ),
        (
            "scan.distinct",
            "SELECT DISTINCT category FROM main_table ORDER BY category",
            40,
        ),
        ("scan.rowid", "SELECT id FROM main_table", 40),
        ("scan.oneint", "SELECT key FROM main_table", 40),
        ("scan.count", "SELECT count(*) FROM main_table", 40),
        ("scan.text", "SELECT label FROM main_table", 40),
        (
            "scan.filter",
            "SELECT id FROM main_table WHERE key > 2500",
            40,
        ),
    ];

    let mut profiles = Vec::new();
    for (name, sql, repeats) in workloads {
        profiles.push(profile_workload(&connection, name, sql, repeats)?);
    }

    report(&profiles);
    report_prepares(&connection)?;
    report_prepare_stages();
    Ok(())
}

/// Prints what each bracketed stage of preparing a statement cost.
fn report_prepare_stages() {
    const NAMES: [(usize, &str); 6] = [
        (10, "parse"),
        (11, "catalog snapshot"),
        (12, "functions+collations"),
        (13, "bind"),
        (14, "compile"),
        (15, "verify"),
    ];
    let mut any = false;
    for (slot, name) in NAMES {
        let runs = match inillucent_base::probe::STAGE_RUNS.get(slot) {
            Some(counter) => counter.load(Ordering::Relaxed),
            None => 0,
        };
        if runs == 0 {
            continue;
        }
        if !any {
            println!();
            println!("--- preparing a statement, by stage ---");
            println!(
                "  {:<24} {:>10} {:>12} {:>12} {:>12}",
                "stage", "runs", "ns/run", "allocs", "alloc/run"
            );
            any = true;
        }
        let nanos = match inillucent_base::probe::STAGE_NANOS.get(slot) {
            Some(counter) => counter.load(Ordering::Relaxed),
            None => 0,
        };
        let allocations = match inillucent_base::probe::STAGE_ALLOCATIONS.get(slot) {
            Some(counter) => counter.load(Ordering::Relaxed),
            None => 0,
        };
        println!(
            "  {:<24} {:>10} {:>12.1} {:>12} {:>12.2}",
            name,
            runs,
            nanos as f64 / runs.max(1) as f64,
            allocations,
            allocations as f64 / runs.max(1) as f64
        );
    }
}

/// Measures preparing a statement, which is where parsing and planning happen.
///
/// Reported apart from the running workloads because it is a different
/// question: `SELECT 1` touches no table, so whatever it costs is what a
/// prepare costs before any row is read.
fn report_prepares(connection: &Connection) -> Result<(), String> {
    const ROUNDS: u64 = 4_000;
    println!();
    println!(
        "{:<52} {:>12} {:>12} {:>12}",
        "prepared statement", "ns each", "allocs", "alloc each"
    );
    for sql in [
        "SELECT 1",
        "SELECT label FROM main_table WHERE id = ?1",
        "SELECT count(*), sum(key), max(category) FROM main_table",
    ] {
        connection
            .prepare(sql)
            .map_err(|failure| format!("{sql}: {failure}"))?;
        inillucent_base::probe::reset_stages();
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        for _ in 0..ROUNDS {
            let statement = connection
                .prepare(sql)
                .map_err(|failure| format!("{sql}: {failure}"))?;
            drop(statement);
        }
        let elapsed = started.elapsed().as_nanos() as f64;
        let allocations = ALLOCATIONS.load(Ordering::Relaxed).saturating_sub(before);
        println!(
            "{:<52} {:>12.1} {:>12} {:>12.1}",
            sql,
            elapsed / ROUNDS as f64,
            allocations,
            allocations as f64 / ROUNDS as f64
        );
    }
    Ok(())
}

/// Runs one workload and records what it cost.
fn profile_workload(
    connection: &Connection,
    name: &'static str,
    sql: &'static str,
    repeats: u64,
) -> Result<Profile, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|failure| format!("{sql}: {failure}"))?;
    // One untimed pass, so the page cache and every lazily built structure is
    // warm and the measurement is of the query rather than of the first one.
    statement.reset().map_err(|failure| failure.to_string())?;
    while statement.step().map_err(|failure| failure.to_string())? {}

    inillucent_base::probe::reset_opcodes();
    let allocations_before = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes_before = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let started = Instant::now();
    let mut rows = 0u64;
    let mut steps = 0u64;
    for _ in 0..repeats {
        statement.reset().map_err(|failure| failure.to_string())?;
        while statement.step().map_err(|failure| failure.to_string())? {
            rows = rows.saturating_add(1);
        }
        steps = steps.saturating_add(statement.steps());
    }
    let elapsed_nanos = started.elapsed().as_nanos();
    let allocations = ALLOCATIONS
        .load(Ordering::Relaxed)
        .saturating_sub(allocations_before);
    let bytes = ALLOCATED_BYTES
        .load(Ordering::Relaxed)
        .saturating_sub(bytes_before);

    Ok(Profile {
        name,
        sql,
        repeats,
        elapsed_nanos,
        rows,
        steps,
        allocations,
        bytes,
        opcodes: opcode_costs(),
    })
}

/// Reads the per-opcode tables the profiling build fills in.
///
/// An ordinary build leaves them empty, and the report says so rather than
/// printing a table of zeroes that looks like a measurement.
fn opcode_costs() -> Vec<(String, u64, u64, u64)> {
    let mut rows = Vec::new();
    for slot in 0..inillucent_base::probe::OPCODE_SLOTS {
        let runs = match inillucent_base::probe::OPCODE_RUNS.get(slot) {
            Some(counter) => counter.load(Ordering::Relaxed),
            None => 0,
        };
        if runs == 0 {
            continue;
        }
        let nanos = match inillucent_base::probe::OPCODE_NANOS.get(slot) {
            Some(counter) => counter.load(Ordering::Relaxed),
            None => 0,
        };
        let allocations = match inillucent_base::probe::OPCODE_ALLOCATIONS.get(slot) {
            Some(counter) => counter.load(Ordering::Relaxed),
            None => 0,
        };
        rows.push((opcode_name(slot), runs, nanos, allocations));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.2));
    rows
}

/// Names an opcode by its discriminant.
fn opcode_name(slot: usize) -> String {
    inillucent_vm::Opcode::from_index(slot)
        .map(|opcode| opcode.name().to_string())
        .unwrap_or_else(|| format!("opcode-{slot}"))
}

/// Prints the table.
fn report(profiles: &[Profile]) {
    println!(
        "{:<16} {:>8} {:>10} {:>12} {:>11} {:>9} {:>12} {:>10}",
        "workload", "repeats", "rows", "ns/row", "steps", "ns/step", "allocs", "alloc/step"
    );
    for profile in profiles {
        println!(
            "{:<16} {:>8} {:>10} {:>12.1} {:>11} {:>9.2} {:>12} {:>10.3}",
            profile.name,
            profile.repeats,
            profile.rows,
            profile.nanos_per_row(),
            profile.steps,
            profile.nanos_per_step(),
            profile.allocations,
            profile.allocations_per_step(),
        );
    }
    println!();
    for profile in profiles {
        println!(
            "{:<16} alloc/row {:>9.3}  bytes {:>12}  sql {}",
            profile.name,
            profile.allocations_per_row(),
            profile.bytes,
            profile.sql
        );
    }
    println!();
    for profile in profiles {
        if profile.opcodes.is_empty() {
            continue;
        }
        println!("--- {} ---", profile.name);
        println!(
            "  {:<18} {:>12} {:>14} {:>10} {:>12} {:>10}",
            "opcode", "runs", "nanos", "ns/run", "allocs", "alloc/run"
        );
        for (name, runs, nanos, allocations) in profile.opcodes.iter().take(12) {
            println!(
                "  {:<18} {:>12} {:>14} {:>10.1} {:>12} {:>10.3}",
                name,
                runs,
                nanos,
                *nanos as f64 / (*runs).max(1) as f64,
                allocations,
                *allocations as f64 / (*runs).max(1) as f64,
            );
        }
        println!();
    }
}

/// Builds the scorecard's small-scale database.
fn build_database(root: &Path) -> Result<PathBuf, String> {
    let path = root.join("hotprofile.db");
    if path.exists() {
        std::fs::remove_file(&path).map_err(|failure| failure.to_string())?;
    }
    let database = Database::open(&path).map_err(|failure| failure.to_string())?;
    let connection = database.connect().map_err(|failure| failure.to_string())?;
    let setup = [
        "CREATE TABLE main_table(id INTEGER PRIMARY KEY, key INTEGER NOT NULL, \
         category INTEGER NOT NULL, label TEXT NOT NULL, payload BLOB)",
        "CREATE INDEX main_key ON main_table(key)",
        "CREATE INDEX main_category ON main_table(category, key)",
        "CREATE TABLE digits(n INTEGER PRIMARY KEY)",
    ];
    for statement in setup {
        connection
            .execute_batch(statement)
            .map_err(|failure| format!("{statement}: {failure}"))?;
    }
    connection
        .execute_batch("INSERT INTO digits(n) VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)")
        .map_err(|failure| failure.to_string())?;
    let insert = "INSERT INTO main_table(id, key, category, label, payload) \
                  SELECT seq, (seq * 7919) % 5000, seq % 32, 'label-' || seq, NULL \
                  FROM (SELECT ((d0.n * 10 + d1.n) * 10 + d2.n) * 10 + d3.n + 1 AS seq \
                  FROM digits d0, digits d1, digits d2, digits d3) WHERE seq <= 5000";
    connection
        .execute_batch(insert)
        .map_err(|failure| format!("insert: {failure}"))?;
    Ok(path)
}
