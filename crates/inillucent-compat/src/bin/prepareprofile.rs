//! Where `prepare.trivial` spends its nanoseconds, on the new engine.
//!
//! Invariant: every stage is timed by *running that stage and everything before
//! it*, so a column is a cumulative cost and the difference between two columns
//! is what the later stage added. Timing a stage in isolation would need each
//! one's input built outside the clock, and building it is most of what the
//! stage costs - which is how a profile ends up attributing a statement's whole
//! compile to whichever stage was measured last.
//!
//! ## Why this exists beside `probeprofile`
//!
//! `probeprofile` prints the same breakdown for the scorecard's workloads, over
//! an imported fixture, and takes about a minute. The floor work in the Phase 2
//! TDD is an iteration loop on `prepare.trivial` - `SELECT 1`, no table, no
//! parameters - and a minute per attempt is the difference between trying six
//! ideas and trying one. This runs in under a second against a database it
//! creates itself, and prints the same columns so the two are comparable.
//!
//! It measures the engine and never SQLite: the ratio is `fullgate`'s job, and
//! a profile that also had to be fair would have to build the reference's arm.
//!
//! Usage:
//!   inillucent-prepareprofile [--iterations N] [--sql "SELECT 1"]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;

/// How many times each stage runs before its total is divided.
const DEFAULT_ITERATIONS: u32 = 20_000;

/// How many allocations the process has made.
///
/// **Because the nanoseconds alone do not say what to fix.** A `SELECT 1`
/// compile measured 3.03 ms per four thousand on the Windows
/// CRT heap and 1.23 ms on a pooled allocator, and 1.31 / 1.11 on Linux - so
/// nearly a microsecond of a Windows compile is `malloc` and nothing else. The
/// count is what turns that into a list of things to stop allocating.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// An allocator that counts, and otherwise delegates to the system one.
struct CountingAllocator;

// SAFETY: every method forwards to the system allocator with the same
// arguments; the counter is the only addition and it touches no memory the
// allocator owns.
unsafe impl GlobalAlloc for CountingAllocator {
    // SAFETY: the layout is the caller's, forwarded unchanged.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    // SAFETY: the pointer and layout are the ones this allocator handed out.
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.dealloc(pointer, layout) }
    }

    // SAFETY: the pointer and layout are the ones this allocator handed out.
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Returns how many allocations one call of a closure made.
///
/// @param iterations - how many calls to average over
/// @param body - the work to count
fn allocations(
    iterations: u32,
    mut body: impl FnMut() -> Result<(), String>,
) -> Result<f64, String> {
    body()?;
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..iterations {
        body()?;
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    Ok((after.saturating_sub(before)) as f64 / f64::from(iterations.max(1)))
}

/// Returns the average nanoseconds one call of a closure took.
///
/// @param iterations - how many calls to average over
/// @param body - the work to time
fn per(iterations: u32, mut body: impl FnMut() -> Result<(), String>) -> Result<f64, String> {
    // A warm pass first, so the first call's page faults and lazy statics are
    // not charged to the stage they happen to be in.
    for _ in 0..iterations.min(200) {
        body()?;
    }
    let start = Instant::now();
    for _ in 0..iterations {
        body()?;
    }
    Ok(start.elapsed().as_nanos() as f64 / f64::from(iterations.max(1)))
}

/// Builds an empty database to compile against, in a scratch directory.
///
/// A table is created so the profile can be pointed at a statement that reads
/// one; `SELECT 1` never touches it.
fn fixture() -> Result<ImportedDatabase, String> {
    let root = inillucent_compat::workspace_root().join("_agent_output/prepareprofile");
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let path: PathBuf = root.join(format!("{}.rdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut database = ImportedDatabase::create(path, 32_768, 4_096)
        .map_err(|error| format!("the fixture opens: {error:?}"))?;
    database
        .execute_any(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
            &Params::new(),
        )
        .map_err(|error| format!("the fixture is made: {error:?}"))?;
    // **The plan cache is off.** This profile is about what a compile costs,
    // and a cache turns the second call into a hash lookup - which is a true
    // number about a different question and would report the compile as thirty
    // nanoseconds. `prepare.trivial` is `prepare_each`, so the gate pays the
    // compile every iteration too.
    database.disable_optimizations(inillucent_sql::plan::Levers::without(
        inillucent_sql::plan::Levers::PLAN_CACHE,
    ));
    Ok(database)
}

fn main() -> ExitCode {
    let mut iterations = DEFAULT_ITERATIONS;
    let mut statements: Vec<String> = Vec::new();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--iterations" => {
                iterations = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_ITERATIONS)
            }
            "--sql" => {
                if let Some(sql) = arguments.next() {
                    statements.push(sql);
                }
            }
            other => statements.push(other.to_string()),
        }
    }
    if statements.is_empty() {
        statements.push("SELECT 1".to_string());
        statements.push("SELECT id, a FROM t WHERE id = 1".to_string());
    }
    let mut database = match fixture() {
        Ok(database) => database,
        Err(why) => {
            eprintln!("{why}");
            return ExitCode::FAILURE;
        }
    };
    println!("## compiling a statement, stage by stage   (nanoseconds)");
    println!(
        "  {:<34} {:>8} {:>8} {:>8} {:>10} {:>9} {:>9} {:>7} {:>7} {:>7}",
        "statement",
        "parse",
        "+bind",
        "+plan",
        "+physical",
        "+pipeline",
        "compile",
        "a/parse",
        "a/plan",
        "a/pipe"
    );
    for sql in &statements {
        match profile(&mut database, sql, iterations) {
            Ok(()) => {}
            Err(why) => {
                eprintln!("{sql}: {why}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

/// Times one statement's compile, stage by cumulative stage.
///
/// @param database - the database to compile against
/// @param sql - the statement
/// @param iterations - how many calls to average over
fn profile(database: &mut ImportedDatabase, sql: &str, iterations: u32) -> Result<(), String> {
    let limits = inillucent_base::limits::Limits::default();
    let parse = per(iterations, || {
        inillucent_sql::parser::parse_next_statement(sql.as_bytes(), 0, &limits)
            .map(|_| ())
            .map_err(|error| format!("{error:?}"))
    })?;
    let bind = per(iterations, || {
        let parsed = inillucent_sql::parser::parse_next_statement(sql.as_bytes(), 0, &limits)
            .map_err(|error| format!("{error:?}"))?;
        let authorizer = inillucent_sql::bind::AllowAll;
        let mut binder =
            inillucent_sql::bind::Binder::new(database.catalog_view(), &parsed.ast, &authorizer)
                .with_source(sql.as_bytes());
        binder
            .bind_statement(&parsed.statement)
            .map(|_| ())
            .map_err(|error| format!("{error:?}"))
    })?;
    let plan = per(iterations, || {
        database.plan(sql).map(|_| ()).map_err(|e| format!("{e:?}"))
    })?;
    let physical = per(iterations, || {
        let plan = database.plan(sql).map_err(|e| format!("{e:?}"))?;
        database
            .prepare(&plan)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    })?;
    // **The gate's own path**, which is not `prepare_statement`: `fullgate`
    // times `plan` then `prepare` then `pipeline` for a `prepare: each`
    // workload, so the operator chain's construction is inside the clock and
    // the plan cache is not on it at all.
    let params = Params::new();
    let built = per(iterations, || {
        let plan = database.plan(sql).map_err(|e| format!("{e:?}"))?;
        let choice = database.prepare(&plan).map_err(|e| format!("{e:?}"))?;
        let sink = Box::new(inillucent_exec::ops::Collect::default());
        database
            .pipeline(&plan, &choice, &params, sink)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    })?;
    // The whole compile the engine performs when a caller asks it by text,
    // which is a different path and is what `execute_any` pays.
    let prepared = per(iterations, || {
        database
            .prepare_statement(sql)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    })?;
    let counted = iterations.min(2_000);
    let parse_allocs = allocations(counted, || {
        inillucent_sql::parser::parse_next_statement(sql.as_bytes(), 0, &limits)
            .map(|_| ())
            .map_err(|error| format!("{error:?}"))
    })?;
    let plan_allocs = allocations(counted, || {
        database.plan(sql).map(|_| ()).map_err(|e| format!("{e:?}"))
    })?;
    let built_allocs = allocations(counted, || {
        let plan = database.plan(sql).map_err(|e| format!("{e:?}"))?;
        let choice = database.prepare(&plan).map_err(|e| format!("{e:?}"))?;
        let sink = Box::new(inillucent_exec::ops::Collect::default());
        database
            .pipeline(&plan, &choice, &params, sink)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    })?;
    let shown: String = sql.chars().take(34).collect();
    println!(
        "  {shown:<34} {parse:>8.1} {bind:>8.1} {plan:>8.1} {physical:>10.1} {built:>9.1} {prepared:>9.1} {parse_allocs:>7.1} {plan_allocs:>7.1} {built_allocs:>7.1}"
    );
    Ok(())
}
