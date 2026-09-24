//! Baselines for the planner, the join enumerator, and the schema statements.
//!
//! Invariant: these are baselines, not comparisons. Nothing here is measured
//! against SQLite and no number here should be read as one; they exist so that
//! a later phase which changes the cost model, the enumerator or the write path
//! has something to change it against. They are taken only after the semantic
//! gates are green, because a fast wrong answer is not a measurement.
//!
//! The join numbers are the ones worth watching. Join-order enumeration is
//! exhaustive up to eight terms and greedy past it, so `plan-join-12` and
//! `plan-join-32` are measuring a different algorithm from `plan-join-2` and
//! `plan-join-5` - and the whole point of recording all four is that a change
//! which moves the cut-over shows up as a step rather than as a slope.
//!
//! Unlike the phase-6 baselines, the database here is written by inillucent. It
//! can write now, and a file it produced is the file its own reads will meet.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-planperf`

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use inillucent_compat::{platform_name, workspace_root};
use inillucent_engine::connect::{Connection, Database};

/// How many allocations have been made since the process started.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
/// How many bytes those allocations asked for.
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counted.
///
/// An allocation count is the one number here that does not move with whatever
/// else the machine is doing, which is what makes it the one worth comparing
/// across runs.
struct CountingAllocator;

// SAFETY: every method forwards to the system allocator with the same layout
// it was given; the counters are the only addition and they touch no memory the
// allocator owns.
unsafe impl GlobalAlloc for CountingAllocator {
    // SAFETY: the layout is the caller's, forwarded unchanged; the counting
    // happens before the allocation and reads nothing the allocator returns.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
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
        // SAFETY: forwarded unchanged to the allocator that made the pointer.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// One workload's numbers.
struct Measurement {
    workload: String,
    scale: String,
    operations: u64,
    nanos_per_operation: f64,
    allocations_per_operation: f64,
    bytes_per_operation: f64,
    rows_per_operation: f64,
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

/// Runs one workload, returning its measurement.
fn measure(
    workload: impl Into<String>,
    scale: impl Into<String>,
    operations: u64,
    mut body: impl FnMut() -> u64,
) -> Measurement {
    // One untimed pass, so the first-call costs of a cache or a lazily built
    // structure are not charged to the first measured iteration.
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
        workload: workload.into(),
        scale: scale.into(),
        operations,
        nanos_per_operation: elapsed / count,
        allocations_per_operation: allocations / count,
        bytes_per_operation: bytes / count,
        rows_per_operation: instructions as f64 / count,
    }
}

/// Builds the database the baselines run against.
///
/// One wide fact table with an index, and thirty-two narrow dimension tables
/// for the join-width workloads. The dimensions are small on purpose: a
/// thirty-two-way join exists here to measure *planning*, and rows in the
/// dimensions would turn it into a measurement of the cartesian product.
fn build_database(root: &Path) -> Result<PathBuf, String> {
    let directory = root.join("_agent_output/planperf");
    std::fs::create_dir_all(&directory).map_err(|reason| reason.to_string())?;
    let path = directory.join("plan.db");
    inillucent_base::testing::remove_database(&path);
    let database = Database::open(&path).map_err(|failure| failure.to_string())?;
    let connection = database.session();
    let mut script = vec![
        "CREATE TABLE fact (id INTEGER PRIMARY KEY, key TEXT, score REAL, tag INTEGER)".to_string(),
        "CREATE INDEX fact_by_key ON fact (key)".to_string(),
        "CREATE INDEX fact_by_tag ON fact (tag, score)".to_string(),
        "BEGIN".to_string(),
    ];
    for row in 0..20_000usize {
        script.push(format!(
            "INSERT INTO fact VALUES ({row}, 'k{row:08}', {}.5, {})",
            row % 1000,
            row % 17
        ));
    }
    script.push("COMMIT".to_string());
    script.push("BEGIN".to_string());
    for term in 0..32usize {
        script.push(format!(
            "CREATE TABLE dim{term} (id INTEGER PRIMARY KEY, label TEXT)"
        ));
        // One row per value `fact.tag` takes, so every join term matches
        // exactly one row and the width of the join does not change the size of
        // its answer.
        for row in 0..17usize {
            script.push(format!("INSERT INTO dim{term} VALUES ({row}, 'd{row}')"));
        }
    }
    script.push("COMMIT".to_string());
    for statement in &script {
        connection
            .execute_batch(statement)
            .map_err(|failure| format!("{statement}: {failure}"))?;
    }
    Ok(path)
}

/// Returns a join over `terms` dimension tables, all equated to the fact table.
///
/// Every term is joined on the same column, so the enumerator has a genuine
/// choice of order rather than one forced by which predicates are usable, and
/// each dimension holds exactly one row per key so the result stays the size of
/// the fact selection however many terms are added. That is deliberate: these
/// rows measure *planning*, and a join whose result grows with its width would
/// measure the cartesian product instead. The first version of this file did
/// exactly that and took a five-way run past twenty minutes.
fn join_query(terms: usize) -> String {
    let mut sql = String::from("SELECT count(*) FROM fact");
    for term in 0..terms {
        sql.push_str(&format!(
            " JOIN dim{term} AS d{term} ON d{term}.id = fact.tag"
        ));
    }
    sql.push_str(" WHERE fact.id < 50");
    sql
}

/// Measures preparing one statement, which is where planning happens.
fn plan_workload(
    connection: &Connection,
    workload: String,
    scale: String,
    operations: u64,
    sql: String,
) -> Result<Measurement, String> {
    // Prepared once outside the loop first, so a statement that does not
    // compile is a reported error rather than a measurement of nothing.
    connection
        .prepare(&sql)
        .map_err(|failure| format!("{sql}: {failure}"))?;
    Ok(measure(workload, scale, operations, || {
        match connection.prepare(&sql) {
            Ok(_) => 1,
            Err(_) => 0,
        }
    }))
}

/// Measures running one statement to completion.
///
/// The work counter is the number of rows the statement produced, not a
/// bytecode-instruction count: the new engine compiles to an operator tree
/// rather than to bytecode, so there is no instruction to count any more. A
/// row is still a count rather than a clock reading, which is what the
/// counters-not-clocks rule actually asks for - it is a different count, not a
/// weaker one, for a DML statement that returns no rows either (the counter is
/// then `changes()`, which is the row count a write always has).
fn run_workload(
    connection: &Connection<'_>,
    workload: &str,
    scale: String,
    operations: u64,
    sql: &str,
) -> Result<Measurement, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|failure| format!("{sql}: {failure}"))?;
    Ok(measure(workload.to_string(), scale, operations, || {
        statement.reset();
        let mut rows = 0u64;
        while statement.step().unwrap_or(false) {
            rows = rows.saturating_add(1);
        }
        if rows == 0 {
            statement.changes() as u64
        } else {
            rows
        }
    }))
}

/// Runs every baseline.
fn run(root: &Path) -> Result<usize, String> {
    let path = build_database(root)?;
    let database = Database::open(&path).map_err(|failure| failure.to_string())?;
    let connection = database.session();
    let mut measurements = Vec::new();

    // Planning, at four join widths. Two and five are inside the exhaustive
    // range; twelve and thirty-two are past it.
    for terms in [2usize, 5, 12, 32] {
        let operations = if terms > 8 { 200u64 } else { 2_000 };
        measurements.push(plan_workload(
            &connection,
            format!("plan-join-{terms}"),
            format!("{terms}-terms"),
            operations,
            join_query(terms),
        )?);
    }

    // Running them. The join is a rowid seek per term, so the work is the fifty
    // fact rows times the width rather than anything exponential - but a wider
    // join is still more work per row, which is what these two show.
    for terms in [2usize, 5, 12] {
        measurements.push(run_workload(
            &connection,
            &format!("run-join-{terms}"),
            format!("{terms}-terms"),
            50,
            &join_query(terms),
        )?);
    }

    // The query shapes phase 8 delivered.
    for (workload, operations, sql) in [
        (
            "correlated-exists",
            20u64,
            "SELECT count(*) FROM fact AS f WHERE EXISTS \
             (SELECT 1 FROM dim0 AS d WHERE d.id = f.tag)",
        ),
        (
            // Re-evaluated per outer row against an indexed inner scan, which is
            // the shape a correlated aggregate always has. Kept to the first
            // thousand rows because the cost is the product of the two.
            "correlated-scalar",
            5,
            "SELECT count(*) FROM fact AS f WHERE f.id < 1000 AND f.score > \
             (SELECT avg(score) FROM fact AS g WHERE g.tag = f.tag)",
        ),
        (
            "in-subquery",
            50,
            "SELECT count(*) FROM fact WHERE tag IN (SELECT id FROM dim0)",
        ),
        (
            "aggregate-grouped",
            50,
            "SELECT tag, count(*), sum(score), avg(score) FROM fact GROUP BY tag",
        ),
        (
            "aggregate-having",
            50,
            "SELECT tag, count(*) AS n FROM fact GROUP BY tag HAVING n > 100 ORDER BY n DESC",
        ),
        (
            "window-row-number",
            20,
            "SELECT id, row_number() OVER (PARTITION BY tag ORDER BY score) FROM fact",
        ),
        (
            "window-frame",
            20,
            "SELECT id, sum(score) OVER (ORDER BY id ROWS BETWEEN 5 PRECEDING AND 5 FOLLOWING) \
             FROM fact",
        ),
        (
            "sort-indexed",
            50,
            "SELECT id FROM fact ORDER BY key LIMIT 100",
        ),
        ("sort-full", 20, "SELECT id FROM fact ORDER BY score, id"),
        (
            "distinct",
            50,
            "SELECT DISTINCT tag, cast(score AS INTEGER) FROM fact",
        ),
        (
            "compound-union",
            20,
            "SELECT id FROM fact WHERE tag = 1 UNION SELECT id FROM fact WHERE tag = 2",
        ),
        (
            "recursive-cte",
            200,
            "WITH RECURSIVE counter(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM counter \
             WHERE n < 500) SELECT count(*) FROM counter",
        ),
    ] {
        measurements.push(run_workload(
            &connection,
            workload,
            "20000-rows".to_string(),
            operations,
            sql,
        )?);
    }

    // The schema statements, which are measured by running them rather than by
    // preparing them: what they cost is the write, not the compile.
    measurements.push(measure("analyze", "35-objects", 3, || {
        match connection.execute_batch("ANALYZE") {
            Ok(_) => 1,
            Err(_) => 0,
        }
    }));

    let mut round = 0u64;
    measurements.push(measure("ddl-create-drop", "one-table", 200, || {
        round = round.saturating_add(1);
        let create = format!("CREATE TABLE scratch{round} (a INTEGER PRIMARY KEY, b TEXT)");
        let drop = format!("DROP TABLE scratch{round}");
        let made = connection.execute_batch(&create).is_ok();
        let gone = connection.execute_batch(&drop).is_ok();
        u64::from(made && gone)
    }));

    measurements.push(measure("vacuum", "20000-rows", 2, || {
        match connection.execute_batch("VACUUM") {
            Ok(_) => 1,
            Err(_) => 0,
        }
    }));

    // Triggers, measured as the cost a write pays for having them.
    connection
        .execute_batch("CREATE TABLE audit (seq INTEGER PRIMARY KEY, tag INTEGER)")
        .map_err(|failure| failure.to_string())?;
    connection
        .execute_batch("CREATE TABLE plain (id INTEGER PRIMARY KEY, tag INTEGER)")
        .map_err(|failure| failure.to_string())?;
    connection
        .execute_batch("CREATE TABLE fired (id INTEGER PRIMARY KEY, tag INTEGER)")
        .map_err(|failure| failure.to_string())?;
    connection
        .execute_batch(
            "CREATE TRIGGER fired_ai AFTER INSERT ON fired BEGIN \
             INSERT INTO audit (tag) VALUES (new.tag); END",
        )
        .map_err(|failure| failure.to_string())?;
    for (workload, table) in [("insert-plain", "plain"), ("insert-triggered", "fired")] {
        let mut statement = connection
            .prepare(&format!("INSERT INTO {table} (tag) VALUES (1)"))
            .map_err(|failure| failure.to_string())?;
        measurements.push(measure(workload.to_string(), "one-row", 2_000, || {
            statement.reset();
            while statement.step().unwrap_or(false) {}
            statement.changes() as u64
        }));
    }

    write_report(root, &measurements)?;
    Ok(measurements.len())
}

/// What the first run of these baselines showed, and what was done about it.
///
/// Kept next to the numbers rather than in a commit message, because a baseline
/// is only useful to somebody who knows which of its rows are already known to
/// be wrong.
///
/// **This finding describes the old, now-deleted bytecode engine
/// (`inillucent-vm`), not the engine this binary measures today.** It is left
/// here as the historical record it always was rather than rewritten to look
/// like a finding about the current engine, which it is not. The `rows/op`
/// column this binary now reports is a row count, not a bytecode-instruction
/// count - there is no bytecode any more - so a fresh run cannot reproduce or
/// contradict the "identical instruction and allocation counts" comparison
/// below; it is describing a different engine's numbers.
const FINDINGS: &str = "\n\
## What the first run showed, against the engine this baseline used to measure\n\
\n\
**`DISTINCT` was quadratic, and is not any more.** It measured 447 ms against 18 ms for\n\
the `GROUP BY` of the same shape, on *fewer* bytecode instructions - which is the tell:\n\
time far out of line with the instruction count is time being spent somewhere the VM is\n\
not looking. The distinct set was a linear scan of every row it had already kept, so a\n\
`DISTINCT` over twenty thousand rows with seventeen thousand distinct values cost about\n\
three hundred million comparisons. It now keeps an ordered index of what it has seen and\n\
binary-searches it: 447 ms became 18.5 ms, on identical instruction and allocation\n\
counts, which is how you can tell the change was the search structure and not the plan.\n\
The ordering it searches by is proved in `inillucent-vm` to say `Equal` exactly where the\n\
equality it replaced said `true`.\n\
\n\
## What it still shows, and has not been changed\n\
\n\
These are recorded rather than fixed. The phase's brief is correctness, and each of them\n\
is a cost that is *explained* by the design rather than a defect in it - but each is also\n\
where a later phase should look first.\n\
\n\
- **`plan-join-5` costs more than `plan-join-12` and `plan-join-32` put together.** Six\n\
  reorderable terms is 720 permutations, each costed; thirteen and thirty-three are past\n\
  the eight-term cut-off and keep their written order for nothing. The step is the\n\
  intended behaviour and these four rows exist to show where it falls, but 15,361\n\
  allocations to plan one six-way join is a lot of allocation for a plan.\n\
- **`correlated-scalar` allocates about thirteen thousand times per outer row.** A\n\
  correlated aggregate is re-evaluated per row by definition, so the *shape* is right;\n\
  what the number says is that re-evaluating it rebuilds rather than rewinds.\n\
- **`vacuum` asks for 1.27 GB to rebuild a 1.7 MB database.** The copy back reads every\n\
  page of the rebuilt file into memory before writing any of it, which is what makes it a\n\
  single journalled transaction; streaming it a page at a time would need the journal to\n\
  be held open across the read, and that trade has not been made.\n";

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
             \"bytes_per_operation\": {:.1}, \"rows_per_operation\": {:.1}}}{}\n",
            measurement.workload,
            measurement.scale,
            measurement.operations,
            measurement.nanos_per_operation,
            measurement.allocations_per_operation,
            measurement.bytes_per_operation,
            measurement.rows_per_operation,
            if index.saturating_add(1) == measurements.len() {
                ""
            } else {
                ","
            }
        ));
    }
    json.push_str("  ]\n}\n");

    let mut report = String::new();
    report.push_str("# Planner, join, and schema baselines, phase 8\n\n");
    report.push_str(&format!("Platform: `{platform}`\n\n"));
    report.push_str(
        "These are baselines, not results. Nothing here is a comparison against SQLite and no\n\
         number here should be read as one; they exist so that a later phase which changes the\n\
         cost model, the join enumerator or the write path has something to change it against.\n\
         They were taken only after the semantic gates were green, because a fast wrong answer\n\
         is not a measurement.\n\n\
         The allocation and row counts matter more than the clock. Wall time on a shared\n\
         machine moves with whatever else is running; the number of heap allocations a plan makes\n\
         and the number of rows a query produces do not.\n\n\
         The four `plan-join-N` rows are the ones worth watching. Join-order enumeration is\n\
         exhaustive up to eight terms and greedy past it, so twelve and thirty-two are measuring\n\
         a different algorithm from two and five - and recording all four means a change that\n\
         moves the cut-over shows up as a step rather than as a slope.\n\n",
    );
    report.push_str("| Workload | Scale | Ops | ns/op | Allocs/op | Bytes/op | rows/op |\n");
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
            measurement.rows_per_operation
        ));
    }
    report.push_str(FINDINGS);

    let directory = root.join("compat/baseline");
    std::fs::create_dir_all(&directory).map_err(|reason| reason.to_string())?;
    std::fs::write(directory.join("phase8-planner-baselines.json"), json)
        .map_err(|reason| reason.to_string())?;
    std::fs::write(directory.join("phase8-planner-baselines.md"), report)
        .map_err(|reason| reason.to_string())?;
    Ok(())
}
