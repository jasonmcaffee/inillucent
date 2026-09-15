//! Where a `CREATE INDEX` spends its time, stage by stage.
//!
//! Invariant: the four stages add up to the statement. `schema.index` is one
//! statement in the full gate and 0.58x against SQLite, so "make it faster" has
//! to start with which of the four - scanning the table, sorting the entries,
//! checking uniqueness, packing the leaves - is actually the cost. The engine
//! already times them; nothing printed them.
//!
//! Usage:
//!   inillucent-indexprofile `<sqlite fixture>` [--iterations N] [--sql "CREATE INDEX ..."]

/// The engine's own allocator, installed for this program.
///
/// **Part of the build, not of a workload.** SQLite ships its own memory
/// subsystem and is compiled as one translation unit; a Rust workspace measured
/// on the platform allocator is being measured on a build configuration rather
/// than on an engine, which is the same reasoning that fixed fat LTO and one
/// codegen unit in the release profile. The Windows C runtime heap was
/// measured at 59% of a trivial compile and this size-classed free list at
/// 17% overall, which is why Phase 3's Part E names it the cheapest first move.
#[global_allocator]
static ALLOCATOR: inillucent_alloc::Pooled = inillucent_alloc::Pooled;

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;

/// The index the full gate's `schema.index` workload builds.
const SQL: &str = "CREATE INDEX main_label ON main_table(label)";

/// The statement that undoes it between rounds.
const DROP: &str = "DROP INDEX IF EXISTS main_label";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = arguments.first().filter(|first| !first.starts_with("--")) else {
        eprintln!("usage: inillucent-indexprofile <sqlite fixture> [--iterations N] [--sql SQL]");
        return ExitCode::from(2);
    };
    let iterations: usize = flag(&arguments, "--iterations")
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    let sql = flag(&arguments, "--sql").unwrap_or_else(|| SQL.to_string());
    let mut database =
        match ImportedDatabase::import_with(Path::new(fixture).to_path_buf(), 32_768, 4_096) {
            Ok(database) => database,
            Err(error) => {
                eprintln!("import failed: {}", error.message());
                return ExitCode::FAILURE;
            }
        };
    if let Err(error) = database.warm() {
        eprintln!("warming failed: {}", error.message());
        return ExitCode::FAILURE;
    }
    println!("## {sql}");
    println!("  iterations: {iterations}");
    let mut totals: Vec<f64> = Vec::with_capacity(iterations);
    // One column per stage, so each is a median over the rounds rather than
    // whatever the last one happened to cost. The prologue is the sixth: the
    // part of the statement before the scan - parsing, binding, allocating the
    // root, re-parsing the canonical SQL - which no stage times and which
    // subtraction is therefore the only way to see.
    let mut stages: Vec<Vec<f64>> = (0..8).map(|_| Vec::with_capacity(iterations)).collect();
    for round in 0..iterations {
        if let Err(error) = database.execute_any(DROP, &Params::new()) {
            eprintln!("drop failed on round {round}: {}", error.message());
            return ExitCode::FAILURE;
        }
        let started = Instant::now();
        if let Err(error) = database.execute_any(&sql, &Params::new()) {
            eprintln!("create failed on round {round}: {}", error.message());
            return ExitCode::FAILURE;
        }
        let total = started.elapsed().as_nanos() as f64 / 1e6;
        totals.push(total);
        let stage = database.build_stage_nanos();
        let measured = [
            stage.scan,
            stage.sort,
            stage.unique,
            stage.flatten,
            stage.pack,
            stage.catalog,
            stage.seal,
        ]
        .map(|value| value as f64 / 1e6);
        let named = measured.iter().sum::<f64>();
        for (column, value) in measured.iter().enumerate() {
            if let Some(held) = stages.get_mut(column) {
                held.push(*value);
            }
        }
        if let Some(held) = stages.get_mut(7) {
            held.push((total - named).max(0.0));
        }
    }
    let _ = database.execute_any(DROP, &Params::new());
    println!("  median total: {:.1} ms", median(&mut totals));
    for (column, name) in [
        "scan", "sort", "unique", "flatten", "pack", "catalog", "seal", "prologue",
    ]
    .iter()
    .enumerate()
    {
        let Some(held) = stages.get_mut(column) else {
            continue;
        };
        println!("  {name:<9}: {:.1} ms", median(held));
    }
    ExitCode::SUCCESS
}

/// Returns the median of a set of measurements, sorting it in the process.
///
/// @param values - the measurements
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

/// Returns a flag's value, when it was given.
///
/// @param arguments - the command line
/// @param name - the flag, with its dashes
fn flag(arguments: &[String], name: &str) -> Option<String> {
    let at = arguments.iter().position(|value| value == name)?;
    arguments.get(at.saturating_add(1)).cloned()
}
