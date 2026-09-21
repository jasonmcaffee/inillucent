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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// How many size buckets the histogram keeps, each eight bytes wide.
///
/// **Because a count of 24 says to stop allocating and not what to stop allocating.**
/// A `SELECT 1` compile makes two dozen allocations and the only cheap fingerprint of
/// one is its size: a `Vec<usize>` of four is 32 bytes and a `String` of a table name is
/// not, so a histogram of sizes turns the count into a list of candidates to go and read
/// the code for. The last bucket holds everything above it.
const BUCKETS: usize = 96;

/// How many allocations of each size the recorded region made.
///
/// A fixed array of counters rather than a list of sizes, because recording inside the
/// allocator must not allocate: pushing to a `Vec` here would call this function from
/// inside itself.
static SIZES: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];

/// Whether the histogram is being recorded. Off for every timed pass.
static RECORDING: AtomicBool = AtomicBool::new(false);

/// The largest allocation a backtrace is captured for, or zero for none.
///
/// **Because a size is a candidate and a call site is an answer.** Eight allocations of
/// under eight bytes each are 30% of what a `SELECT 1` compile costs on this machine, and
/// nothing about "under eight bytes" says which `Vec` asked for them. Capturing where they
/// came from is the difference between reading the whole compiler and reading four
/// functions.
static TRACE_UPTO: AtomicU64 = AtomicU64::new(0);

/// Where each traced allocation came from, in the order they happened.
///
/// Capturing and formatting a backtrace allocates, so `RECORDING` is turned off around
/// the capture: what the backtrace machinery allocates is not part of the compile being
/// measured, and counting it would make the instrument its own subject.
static TRACES: std::sync::OnceLock<std::sync::Mutex<Vec<(usize, String)>>> =
    std::sync::OnceLock::new();

/// Captures where one allocation was made, when tracing asks for that size.
///
/// @param size - how many bytes were asked for
fn trace(size: usize) {
    let upto = TRACE_UPTO.load(Ordering::Relaxed) as usize;
    if upto == 0 || size > upto || !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    RECORDING.store(false, Ordering::Relaxed);
    let captured = std::backtrace::Backtrace::force_capture().to_string();
    let held = TRACES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if let Ok(mut traces) = held.lock() {
        traces.push((size, captured));
    }
    RECORDING.store(true, Ordering::Relaxed);
}

/// Prints the frames of each traced allocation that name this workspace.
///
/// The standard library and the backtrace machinery are dropped: every frame that
/// survives is a line of this repository, which is the only part a fix can touch.
fn print_traces() {
    let held = TRACES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let Ok(mut traces) = held.lock() else {
        return;
    };
    for (nth, (size, captured)) in traces.iter().enumerate() {
        println!("    [{nth}] {size} bytes");
        let mut shown = 0usize;
        for line in captured.lines() {
            let trimmed = line.trim();
            if !trimmed.contains("inillucent") || trimmed.contains("prepareprofile") {
                continue;
            }
            if trimmed.starts_with("at ") {
                continue;
            }
            println!("         {trimmed}");
            shown = shown.saturating_add(1);
            if shown >= 6 {
                break;
            }
        }
    }
    traces.clear();
}

/// Records one allocation's size in the histogram, when recording is on.
///
/// @param size - how many bytes were asked for
fn record(size: usize) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    let bucket = size.div_euclid(8).min(BUCKETS.saturating_sub(1));
    if let Some(counter) = SIZES.get(bucket) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// An allocator that counts, and otherwise delegates to the system one.
struct CountingAllocator;

// SAFETY: every method forwards to the system allocator with the same
// arguments; the counter is the only addition and it touches no memory the
// allocator owns.
unsafe impl GlobalAlloc for CountingAllocator {
    // SAFETY: the layout is the caller's, forwarded unchanged.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        record(layout.size());
        trace(layout.size());
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
        record(size);
        trace(size);
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

/// Prints the size of every allocation one call of a closure makes.
///
/// One call and not an average: the point is the list, and a size seen twice in one
/// compile is two rows of it rather than a fraction.
///
/// @param label - what the region is, for the heading
/// @param body - the work to record
fn sizes(label: &str, mut body: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    // A warm pass outside the recording, so a lazy static allocated once is not counted
    // as part of a compile.
    body()?;
    for counter in SIZES.iter() {
        counter.store(0, Ordering::Relaxed);
    }
    RECORDING.store(true, Ordering::Relaxed);
    let outcome = body();
    RECORDING.store(false, Ordering::Relaxed);
    outcome?;
    println!("  {label}");
    let mut total = 0u64;
    for (bucket, counter) in SIZES.iter().enumerate() {
        let count = counter.load(Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        total = total.saturating_add(count);
        let low = bucket.saturating_mul(8);
        match bucket == BUCKETS.saturating_sub(1) {
            true => println!("    {count:>4} x  {low} bytes or more"),
            false => println!("    {count:>4} x  {low}..{} bytes", low.saturating_add(7)),
        }
    }
    println!("    {total:>4} allocations in total");
    // Per stage rather than once at the end, because the stages are cumulative - the plan
    // stage parses too - and a single list cannot be attributed to the stage that made it.
    if TRACE_UPTO.load(Ordering::Relaxed) > 0 {
        print_traces();
    }
    Ok(())
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

/// Returns the nanoseconds one call took in the quietest batch of calls.
///
/// **Because a mean is a reading of the machine and a minimum is not
/// (task-2026).** `prepare.trivial` is a compile of under a microsecond, and on
/// a box running three other agents' test suites the mean of twenty thousand of
/// them moved between 1,389 and 2,435 ns across three runs of one binary. Load
/// can only ever *add* time to a sample, so the quietest batch is close to what
/// the work costs with nothing in the way, and it is the same number on a busy
/// box and an idle one.
///
/// Batches rather than single calls because `Instant::now` costs tens of
/// nanoseconds, which is a tenth of what is being measured; a batch of a
/// hundred amortises the clock to a rounding error while still being short
/// enough that many batches land in a gap between other processes' work.
///
/// It is reported beside the mean rather than instead of it. The mean is what
/// an application waiting on a loaded machine actually experiences; the minimum
/// is what the code costs, and a change to the code should move the second.
///
/// @param iterations - how many calls in total
/// @param batch - how many calls in each batch
/// @param body - the work to time
fn per_quietest(
    iterations: u32,
    batch: u32,
    mut body: impl FnMut() -> Result<(), String>,
) -> Result<f64, String> {
    for _ in 0..batch.min(200) {
        body()?;
    }
    let batch = batch.max(1);
    let mut quietest = f64::MAX;
    for _ in 0..iterations.div_euclid(batch).max(1) {
        let start = Instant::now();
        for _ in 0..batch {
            body()?;
        }
        let each = start.elapsed().as_nanos() as f64 / f64::from(batch);
        if each < quietest {
            quietest = each;
        }
    }
    Ok(quietest)
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
    let mut histogram = false;
    let mut trace_upto = 0u64;
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
            "--sizes" => histogram = true,
            "--trace" => {
                histogram = true;
                trace_upto = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(8);
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
    TRACE_UPTO.store(trace_upto, Ordering::Relaxed);
    println!("## compiling a statement, stage by stage   (nanoseconds)");
    println!(
        "  {:<34} {:>8} {:>8} {:>8} {:>10} {:>9} {:>9} {:>9} {:>7} {:>7} {:>7}",
        "statement",
        "parse",
        "+bind",
        "+plan",
        "+physical",
        "+pipeline",
        "quietest",
        "compile",
        "a/parse",
        "a/plan",
        "a/pipe"
    );
    for sql in &statements {
        match profile(&mut database, sql, iterations, histogram) {
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
/// @param histogram - whether to also print the size of every allocation
fn profile(
    database: &mut ImportedDatabase,
    sql: &str,
    iterations: u32,
    histogram: bool,
) -> Result<(), String> {
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
    // The same path again, reported as the quietest batch rather than the mean.
    // See `per_quietest`: on a shared box the mean of this moved by 75% between
    // two runs of one binary, and the minimum is what the code costs.
    let built_quietest = per_quietest(iterations, 100, || {
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
    if histogram {
        println!("## every allocation one compile makes, by size   ({sql})");
        sizes("parsing", || {
            inillucent_sql::parser::parse_next_statement(sql.as_bytes(), 0, &limits)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        })?;
        sizes("parse, bind and plan", || {
            database.plan(sql).map(|_| ()).map_err(|e| format!("{e:?}"))
        })?;
        sizes("the whole pipeline the gate builds", || {
            let plan = database.plan(sql).map_err(|e| format!("{e:?}"))?;
            let choice = database.prepare(&plan).map_err(|e| format!("{e:?}"))?;
            let sink = Box::new(inillucent_exec::ops::Collect::default());
            database
                .pipeline(&plan, &choice, &params, sink)
                .map(|_| ())
                .map_err(|e| format!("{e:?}"))
        })?;
    }
    let shown: String = sql.chars().take(34).collect();
    println!(
        "  {shown:<34} {parse:>8.1} {bind:>8.1} {plan:>8.1} {physical:>10.1} {built:>9.1} {built_quietest:>9.1} {prepared:>9.1} {parse_allocs:>7.1} {plan_allocs:>7.1} {built_allocs:>7.1}"
    );
    Ok(())
}
