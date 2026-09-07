//! Where a `CREATE INDEX` spends its time, stage by stage.
//!
//! Invariant: the four stages add up to the statement. `schema.index` is one
//! statement in the full gate and 0.58x against SQLite, so "make it faster" has
//! to start with which of the four - scanning the table, sorting the entries,
//! checking uniqueness, packing the leaves - is actually the cost. The engine
//! already times them; nothing printed them.
//!
//! Usage:
//!   inillucent-indexprofile <sqlite fixture> [--iterations N] [--sql "CREATE INDEX ..."]

/// The engine's own allocator, installed for this program.
///
/// **Part of the build, not of a workload.** SQLite ships its own memory
/// subsystem and is compiled as one translation unit; a Rust workspace measured
/// on the platform allocator is being measured on a build configuration rather
/// than on an engine, which is the same reasoning that fixed fat LTO and one
/// codegen unit in the release profile. task-1838 §5 measured the Windows CRT
/// heap at 59% of a trivial compile and this size-classed free list at 17%
/// overall, which is why Phase 3's Part E names it the cheapest first move.
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
    let mut last = String::new();
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
        totals.push(started.elapsed().as_nanos() as f64 / 1e6);
        last = database.build_stages();
    }
    let _ = database.execute_any(DROP, &Params::new());
    totals.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let middle = totals.get(totals.len() / 2).copied().unwrap_or(0.0);
    println!("  median total: {middle:.1} ms");
    println!("  last round  : {last}");
    ExitCode::SUCCESS
}

/// Returns a flag's value, when it was given.
///
/// @param arguments - the command line
/// @param name - the flag, with its dashes
fn flag(arguments: &[String], name: &str) -> Option<String> {
    let at = arguments.iter().position(|value| value == name)?;
    arguments.get(at.saturating_add(1)).cloned()
}
