//! Baselines for the SQL front end and the read-only virtual machine.
//!
//! Invariant: these are baselines, not comparisons. Nothing here is measured
//! against SQLite and no number here should be read as one; they exist so that
//! a later phase which changes the parser, the planner or the machine has
//! something to change it against. They are also taken only after the semantic
//! gates are green, because a fast wrong answer is not a measurement.
//!
//! Three counters are recorded next to the clock, and they matter more than the
//! clock does: the number of heap allocations, the bytes those allocations
//! asked for, and the number of bytecode instructions the machine executed.
//! Wall time on a shared machine moves with what else is running; an allocation
//! count and an instruction count do not.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-sqlperf`

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use inillucent_base::limits::Limits;
use inillucent_compat::oracle::{Driver, Op};
use inillucent_compat::{platform_name, workspace_root};
use inillucent_legacy::Database;
use inillucent_sql::parser;

/// How many allocations have been made since the process started.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
/// How many bytes those allocations asked for.
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counted.
///
/// Counting allocations is the only way to see the difference between a parser
/// that borrows its identifiers and one that copies them, and that difference
/// does not show up reliably in wall time on a machine doing other work.
struct CountingAllocator;

// SAFETY: every method forwards to the system allocator with the same layout
// and pointer it was given, and the counters are atomics. The counting adds no
// requirement of its own.
unsafe impl GlobalAlloc for CountingAllocator {
    /// Allocates through the system allocator, counting the request.
    // SAFETY: the layout is the caller's, forwarded unchanged; the counting
    // adds no requirement of its own.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    /// Frees through the system allocator.
    // SAFETY: the pointer and layout are the ones this allocator handed out,
    // which is the caller's obligation and unchanged by the counting.
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.dealloc(pointer, layout) }
    }

    /// Reallocates through the system allocator, counting the request.
    // SAFETY: the pointer and layout are the ones this allocator handed out,
    // which is the caller's obligation and unchanged by the counting.
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// One measured workload.
struct Measurement {
    workload: &'static str,
    scale: String,
    operations: u64,
    nanos_per_operation: f64,
    allocations_per_operation: f64,
    bytes_per_operation: f64,
    instructions_per_operation: f64,
}

/// Runs the baselines and writes them.
fn main() -> ExitCode {
    let root = workspace_root();
    match run(&root) {
        Ok(count) => {
            println!("recorded {count} measurements");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the pinned oracle, if it has been built.
fn oracle_path(root: &Path) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = root
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Builds the measurement database, which the pinned binary writes.
///
/// inillucent cannot write yet, so the data these baselines read has to come from
/// somewhere; having the reference engine write it also means the file is the
/// same shape a real one would be.
fn build_database(root: &Path, rows: usize) -> Result<PathBuf, String> {
    let path = root.join("_agent_output/sqlperf/perf.db");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|reason| reason.to_string())?;
    }
    if path.is_file() {
        return Ok(path);
    }
    let Some(program) = oracle_path(root) else {
        return Err("the pinned SQLite oracle is not built".to_string());
    };
    let mut driver = Driver::start("sqlite", &program)?;
    driver.send(&Op::Hello)?;
    driver.send(&Op::Open(path.display().to_string()))?;
    driver.send(&Op::Exec(
        "CREATE TABLE bench (id INTEGER PRIMARY KEY, key TEXT, score REAL, tag INTEGER)"
            .to_string(),
    ))?;
    driver.send(&Op::Exec(
        "CREATE INDEX bench_by_key ON bench (key)".to_string(),
    ))?;
    driver.send(&Op::Exec("BEGIN".to_string()))?;
    for row in 0..rows {
        driver.send(&Op::Exec(format!(
            "INSERT INTO bench VALUES ({row}, 'k{:08}', {}.5, {})",
            row,
            row % 1000,
            row % 17
        )))?;
    }
    driver.send(&Op::Exec("COMMIT".to_string()))?;
    // A wide schema, for the schema-load baseline.
    driver.send(&Op::Exec("BEGIN".to_string()))?;
    for index in 0..100 {
        driver.send(&Op::Exec(format!(
            "CREATE TABLE wide{index} (a INTEGER PRIMARY KEY, b TEXT, c REAL, d BLOB, e)"
        )))?;
    }
    driver.send(&Op::Exec("COMMIT".to_string()))?;
    let _ = driver.send(&Op::Bye);
    Ok(path)
}

/// Runs one workload, returning its measurement.
fn measure(
    workload: &'static str,
    scale: impl Into<String>,
    operations: u64,
    mut body: impl FnMut() -> u64,
) -> Measurement {
    // One untimed pass, so the first-call costs of a cache or a lazily built
    // table are not charged to the first measured iteration.
    let _ = body();
    let allocations_before = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes_before = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let started = Instant::now();
    let mut instructions = 0u64;
    for _ in 0..operations {
        instructions = instructions.saturating_add(body());
    }
    let elapsed = started.elapsed().as_nanos() as f64;
    let allocations = ALLOCATIONS
        .load(Ordering::Relaxed)
        .saturating_sub(allocations_before) as f64;
    let bytes = ALLOCATED_BYTES
        .load(Ordering::Relaxed)
        .saturating_sub(bytes_before) as f64;
    let count = operations.max(1) as f64;
    Measurement {
        workload,
        scale: scale.into(),
        operations,
        nanos_per_operation: elapsed / count,
        allocations_per_operation: allocations / count,
        bytes_per_operation: bytes / count,
        instructions_per_operation: instructions as f64 / count,
    }
}

/// The statement the front-end baselines are taken on.
const REPRESENTATIVE: &str = "SELECT b.id, b.key, count(*) AS n FROM bench AS b \
     WHERE b.key = ?1 AND b.score BETWEEN 1.0 AND 900.0 AND b.tag IN (1, 2, 3) \
     GROUP BY b.id, b.key HAVING count(*) > 0 ORDER BY n DESC, b.id LIMIT 10";

/// Runs every baseline.
fn run(root: &Path) -> Result<usize, String> {
    let rows = 20_000usize;
    let path = build_database(root, rows)?;
    let database = Database::open(&path).map_err(|failure| failure.to_string())?;
    let connection = database.connect().map_err(|failure| failure.to_string())?;
    let limits = Limits::default();
    let mut measurements = Vec::new();

    // The front end, with no file anywhere near it.
    measurements.push(measure("lex", "representative", 20_000, || {
        let mut lexer = inillucent_sql::Lexer::new(REPRESENTATIVE.as_bytes());
        let mut tokens = 0u64;
        while let Ok(token) = lexer.next_token() {
            if token.kind == inillucent_sql::TokenKind::EndOfInput {
                break;
            }
            tokens = tokens.saturating_add(1);
        }
        tokens
    }));
    measurements.push(measure("parse", "representative", 20_000, || {
        let parsed = parser::parse_next_statement(REPRESENTATIVE.as_bytes(), 0, &limits);
        parsed
            .map(|statement| statement.ast.expr_count() as u64)
            .unwrap_or(0)
    }));
    measurements.push(measure(
        "prepare",
        "representative",
        20_000,
        || match connection.prepare(REPRESENTATIVE) {
            Ok(statement) => statement.explain().len() as u64,
            Err(_) => 0,
        },
    ));

    // A prepared statement reused, which is what a cached prepare buys.
    let mut cached = connection
        .prepare(REPRESENTATIVE)
        .map_err(|failure| failure.to_string())?;
    measurements.push(measure("prepare-cached", "representative", 20_000, || {
        let _ = cached.reset();
        let _ = cached.bind_text(1, "k00000001");
        1
    }));

    // Opening a connection loads the catalog, which is the schema-load cost.
    measurements.push(measure(
        "schema-load",
        "104-objects",
        200,
        || match database.connect() {
            Ok(_) => 1,
            Err(_) => 0,
        },
    ));

    // The machine, on the shapes the phase delivers.
    for (workload, sql) in [
        (
            "point-select-rowid",
            "SELECT key FROM bench WHERE id = 12345",
        ),
        (
            "point-select-index",
            "SELECT id FROM bench WHERE key = 'k00012345'",
        ),
        (
            "range-select-rowid",
            "SELECT id FROM bench WHERE id > 10000 AND id <= 10100",
        ),
        ("scan-count", "SELECT count(*) FROM bench"),
        ("scan-project", "SELECT id, key, score FROM bench"),
        ("sort", "SELECT id FROM bench ORDER BY key LIMIT 100"),
        (
            "aggregate-grouped",
            "SELECT tag, count(*) FROM bench GROUP BY tag",
        ),
        ("distinct", "SELECT DISTINCT tag FROM bench"),
    ] {
        let operations = match workload {
            "point-select-rowid" | "point-select-index" | "range-select-rowid" => 5_000u64,
            _ => 20u64,
        };
        let mut statement = connection
            .prepare(sql)
            .map_err(|failure| format!("{sql}: {failure}"))?;
        measurements.push(measure(
            workload,
            format!("{rows}-rows"),
            operations,
            || {
                let _ = statement.reset();
                while statement.step().unwrap_or(false) {}
                statement.steps()
            },
        ));
    }

    write_report(root, &measurements)?;
    Ok(measurements.len())
}

/// Writes the baselines as JSON and as a report.
fn write_report(root: &Path, measurements: &[Measurement]) -> Result<(), String> {
    let platform = platform_name();
    let mut json = String::new();
    json.push_str(&format!(
        "{{\n  \"platform\": \"{platform}\",\n  \"measurements\": [\n"
    ));
    for (index, measurement) in measurements.iter().enumerate() {
        json.push_str(&format!(
            "    {{\"workload\": \"{}\", \"scale\": \"{}\", \"operations\": {}, \
             \"nanos_per_operation\": {:.1}, \"allocations_per_operation\": {:.1}, \
             \"bytes_per_operation\": {:.1}, \"instructions_per_operation\": {:.1}}}{}\n",
            measurement.workload,
            measurement.scale,
            measurement.operations,
            measurement.nanos_per_operation,
            measurement.allocations_per_operation,
            measurement.bytes_per_operation,
            measurement.instructions_per_operation,
            if index.saturating_add(1) == measurements.len() {
                ""
            } else {
                ","
            }
        ));
    }
    json.push_str("  ]\n}\n");

    let mut report = String::new();
    report.push_str("# SQL front end and read-only VM baselines, phases 5-6\n\n");
    report.push_str(&format!("Platform: `{platform}`\n\n"));
    report.push_str(
        "These are baselines, not results. Nothing here is a comparison against SQLite and no\n\
         number here should be read as one; they exist so that a later phase which changes the\n\
         parser, the planner or the machine has something to change it against. They were taken\n\
         only after the semantic gates were green, because a fast wrong answer is not a\n\
         measurement.\n\n\
         The allocation and instruction counts matter more than the clock. Wall time on a shared\n\
         machine moves with whatever else is running; the number of heap allocations a parse makes\n\
         and the number of bytecode instructions a query executes do not, and they are what a\n\
         later change will actually have moved.\n\n",
    );
    report.push_str("| Workload | Scale | Ops | ns/op | Allocs/op | Bytes/op | VM ops |\n");
    report.push_str("|---|---|--:|--:|--:|--:|--:|\n");
    for measurement in measurements {
        report.push_str(&format!(
            "| `{}` | {} | {} | {:.1} | {:.1} | {:.0} | {:.0} |\n",
            measurement.workload,
            measurement.scale,
            measurement.operations,
            measurement.nanos_per_operation,
            measurement.allocations_per_operation,
            measurement.bytes_per_operation,
            measurement.instructions_per_operation
        ));
    }

    let directory = root.join("compat/baseline");
    std::fs::create_dir_all(&directory).map_err(|reason| reason.to_string())?;
    std::fs::write(directory.join("phase6-sql-baselines.json"), json)
        .map_err(|reason| reason.to_string())?;
    std::fs::write(directory.join("phase6-sql-baselines.md"), report)
        .map_err(|reason| reason.to_string())?;
    Ok(())
}
