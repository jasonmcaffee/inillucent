//! The Phase 1 gate: `read.analytical` on the new engine, against SQLite.
//!
//! Invariant: a timing is read only after the two engines agree on the answer.
//! Each round digests every value of every row on both sides with the same
//! function the scorecard uses, and a workload whose digests differ is reported
//! as a correctness failure and is not timed at all. That is the existing
//! contract and this binary does not relax it because the file is no longer
//! shared - if anything the import makes it more load-bearing, since a bug in
//! the import now shows up here rather than being impossible.
//!
//! ## What it measures, and the two fairness questions it answers out loud
//!
//! **Same SQL.** The four `read.analytical` workloads are read from the same
//! generator the scorecard uses, so the text both engines run is one string.
//!
//! **Same structure.** `EXPLAIN QUERY PLAN` is printed for both engines on
//! every workload. SQLite answers `count(*), sum(key), max(category)` from the
//! covering index `main_category`, not from the table, and a ratio measured
//! against a table scan on our side would be a ratio between two different
//! amounts of work rather than between two engines. The new engine's physical
//! pass honours the planner's access-path choice, so it picks the same
//! structure; when it does not, the two plan descriptions sit side by side in
//! the output and the reader can see it.
//!
//! **Same cache state.** Not yet, and it is stated rather than hidden. The new
//! engine's trees are fully resident and SQLite runs at the plan's
//! `cache_size`. For the covering-index shapes both fit; for a full table scan
//! SQLite's 11 MB table does not fit its 2 MB cache. Phase 2's buffer pool is
//! what makes this configurable on both sides.
//!
//! Usage: rustdb-analytical <sqlite fixture> [--rounds N] [--page-size N]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use rustdb_compat::newengine::ImportedDatabase;
use rustdb_compat::perf::{Digest, Paired, Sample};
use rustdb_compat::workspace_root;
use rustdb_tree::datum::OwnedDatum;

/// The seed the bootstrap uses, fixed so a report is reproducible.
const SEED: u64 = 17_900_001;

/// The four `read.analytical` workloads, byte-for-byte as the scorecard
/// generates them (`scorecard.rs`, the `read.analytical` family).
const WORKLOADS: [(&str, &str); 4] = [
    (
        "scan.aggregate",
        "SELECT count(*), sum(key), max(category) FROM main_table",
    ),
    (
        "scan.group",
        "SELECT category, count(*) FROM main_table GROUP BY category ORDER BY category",
    ),
    (
        "scan.sort",
        "SELECT id FROM main_table ORDER BY label LIMIT 100",
    ),
    (
        "scan.distinct",
        "SELECT DISTINCT category FROM main_table ORDER BY category",
    ),
];

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = arguments.first().filter(|value| !value.starts_with("--")) else {
        eprintln!("usage: rustdb-analytical <sqlite fixture> [--rounds N] [--page-size N]");
        return ExitCode::from(2);
    };
    let rounds = flag(&arguments, "--rounds")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(30);
    let page_size = flag(&arguments, "--page-size")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(32_768);
    let repeat = flag(&arguments, "--repeat")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(20);
    match run(&PathBuf::from(fixture), rounds, page_size, repeat) {
        Ok(passed) => {
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--name value` flag.
///
/// @param arguments - the command line
/// @param name - the flag to look for
fn flag(arguments: &[String], name: &str) -> Option<String> {
    arguments
        .iter()
        .position(|value| value == name)
        .and_then(|at| arguments.get(at.saturating_add(1)))
        .cloned()
}

/// Runs the gate and reports whether it passed.
///
/// @param fixture - the SQLite database both engines read from
/// @param rounds - how many paired rounds
/// @param page_size - the page size the new engine's trees are built at
/// @param repeat - how many times each workload runs inside one timed round
fn run(fixture: &Path, rounds: u32, page_size: usize, repeat: u32) -> Result<bool, String> {
    let bench = sqlite_bench().ok_or_else(|| {
        "sqlite-bench is not built; run tools/sqlite-reference.ps1 first".to_string()
    })?;
    let imported_at = Instant::now();
    let database = ImportedDatabase::import(fixture.to_path_buf(), page_size)
        .map_err(|error| format!("import failed: {}", error.message()))?;
    println!(
        "imported into {} trees at page size {page_size} in {:.1}s",
        database.roots().len(),
        imported_at.elapsed().as_secs_f64()
    );

    // The plan file both arms read. Written once, so there is one copy of the
    // SQL and neither side can drift from it.
    let plan_path = std::env::temp_dir().join(format!(
        "rustdb-analytical-{}.plan",
        std::process::id()
    ));
    std::fs::write(&plan_path, plan_file(repeat))
        .map_err(|error| format!("could not write the plan: {error}"))?;

    println!();
    println!("## plans, both engines");
    for (name, sql) in WORKLOADS {
        let ours = database
            .describe(sql)
            .map_err(|error| format!("{name}: {}", error.message()))?;
        let theirs = explain(fixture, sql)?;
        println!("  {name}");
        println!("    rust-db : {}", ours.join(" | "));
        println!("    sqlite  : {}", theirs.join(" | "));
    }

    // Plan once, run many: `prepare_each: false` in the scorecard plan means
    // the prepare is outside the timed region on both sides.
    let mut prepared = Vec::with_capacity(WORKLOADS.len());
    for (name, sql) in WORKLOADS {
        let plan = database
            .plan(sql)
            .map_err(|error| format!("{name}: planning failed: {}", error.message()))?;
        prepared.push((name, sql, plan));
    }

    let mut measured: Vec<Paired> = WORKLOADS
        .iter()
        .map(|(name, _)| Paired {
            workload: (*name).to_string(),
            family: "read.analytical".to_string(),
            pairs: Vec::with_capacity(rounds as usize),
            agreed: true,
            disagreement: String::new(),
        })
        .collect();

    println!();
    println!("## {rounds} paired rounds, interleaved");
    for round in 0..rounds {
        // The engine order alternates by round so a warm cache or a busy
        // machine does not systematically favour whichever went first.
        let ours_first = round % 2 == 0;
        let ours = time_new_engine(&database, &prepared, repeat)?;
        let theirs = run_sqlite(&bench, &plan_path, fixture)?;
        let (first, second) = if ours_first {
            (ours, theirs)
        } else {
            // Re-run in the other order rather than reordering the results,
            // because the point of alternating is the order the machine saw.
            let theirs_again = run_sqlite(&bench, &plan_path, fixture)?;
            let ours_again = time_new_engine(&database, &prepared, repeat)?;
            let _ = (ours, theirs);
            (ours_again, theirs_again)
        };
        for (index, (name, _, _)) in prepared.iter().enumerate() {
            let Some(slot) = measured.get_mut(index) else {
                continue;
            };
            let Some(mine) = first.iter().find(|sample| sample.workload == *name) else {
                return Err(format!("{name}: the new engine produced no sample"));
            };
            let Some(reference) = second.iter().find(|sample| sample.workload == *name) else {
                return Err(format!("{name}: sqlite produced no sample"));
            };
            if mine.digest != reference.digest || mine.rows != reference.rows {
                slot.agreed = false;
                slot.disagreement = format!(
                    "rust-db {} rows digest {:016x} against sqlite {} rows digest {:016x}",
                    mine.rows, mine.digest, reference.rows, reference.digest
                );
                continue;
            }
            slot.pairs.push((mine.nanos, reference.nanos));
        }
    }

    println!();
    println!("## result");
    println!(
        "  {:<16} {:>12} {:>12} {:>9} {:>9} {:>9}  {}",
        "workload", "rust-db ns", "sqlite ns", "ratio", "low", "high", "agreed"
    );
    let mut all_ratios: Vec<f64> = Vec::new();
    let mut passed = true;
    for entry in &measured {
        if !entry.agreed {
            println!(
                "  {:<16} {:>12} {:>12} {:>9} {:>9} {:>9}  NO: {}",
                entry.workload, "-", "-", "-", "-", "-", entry.disagreement
            );
            passed = false;
            continue;
        }
        let (ours, theirs) = entry.medians();
        let (low, high) = entry.interval(SEED);
        println!(
            "  {:<16} {:>12.0} {:>12.0} {:>8.2}x {:>8.2}x {:>8.2}x  yes",
            entry.workload,
            ours,
            theirs,
            entry.ratio(),
            low,
            high
        );
        all_ratios.extend(entry.log_ratios());
    }

    if !all_ratios.is_empty() {
        let family = rustdb_compat::perf::median(&all_ratios).exp();
        let (low, high) = rustdb_compat::perf::bootstrap(&all_ratios, SEED);
        println!();
        println!(
            "  read.analytical: {family:.2}x  (95% interval {:.2}x .. {:.2}x over {} paired samples)",
            low.exp(),
            high.exp(),
            all_ratios.len()
        );
        println!("  Phase 1 gate: lower bound at least 5.00x");
        if low.exp() < 5.0 {
            println!("  VERDICT: MISSED");
            passed = false;
        } else {
            println!("  VERDICT: MET");
        }
    } else {
        passed = false;
    }
    Ok(passed)
}

/// Runs every workload once through the new engine and returns timed samples.
///
/// The timed region is the same one the scorecard times on the old engine: the
/// prepare is outside it, the row production and the digesting of every value
/// are inside it. Digesting inside the timed region costs both engines the same
/// work - `sqlite-bench` digests inside its own timer too - which is why it is
/// there rather than being subtracted afterwards.
///
/// @param database - the imported trees
/// @param prepared - the planned workloads
/// @param repeat - how many times each workload runs inside one sample
fn time_new_engine(
    database: &ImportedDatabase,
    prepared: &[(&str, &str, rustdb_sql::plan::PhysicalPlan)],
    repeat: u32,
) -> Result<Vec<Sample>, String> {
    let mut samples = Vec::with_capacity(prepared.len());
    for (name, _, plan) in prepared {
        let mut digest = Digest::new();
        let mut produced = 0u64;
        let started = Instant::now();
        for _ in 0..repeat {
            let (rows, _) = database
                .execute(plan)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            for row in &rows {
                for value in row {
                    eat(&mut digest, value);
                }
                produced = produced.saturating_add(1);
            }
        }
        let elapsed = started.elapsed();
        samples.push(Sample {
            workload: (*name).to_string(),
            nanos: elapsed.as_secs_f64() * 1e9,
            rows: produced,
            digest: digest.finish(),
        });
    }
    Ok(samples)
}

/// Adds one produced value to the digest, tagged the way the reference tags it.
///
/// A byte-for-byte copy of `scorecard.rs`'s `eat`, over the new engine's value
/// type. It has to be: a digest is only a correctness gate if both engines
/// compute it the same way, and the reference implementation is
/// `compat/oracle/sqlite_bench.c`.
///
/// @param digest - the running digest
/// @param value - the value to fold in
fn eat(digest: &mut Digest, value: &OwnedDatum) {
    match value {
        OwnedDatum::Null => digest.tag(0),
        OwnedDatum::Int(number) => {
            digest.tag(1);
            digest.word(*number as u64);
        }
        OwnedDatum::Real(number) => {
            digest.tag(2);
            digest.word(number.to_bits());
        }
        OwnedDatum::Text(bytes) => {
            digest.tag(3);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
        OwnedDatum::Blob(bytes) => {
            digest.tag(4);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
    }
}

/// Runs the SQLite arm and parses its samples.
///
/// @param bench - the sqlite-bench executable
/// @param plan - the plan file both engines read
/// @param database - the fixture
fn run_sqlite(bench: &Path, plan: &Path, database: &Path) -> Result<Vec<Sample>, String> {
    let output = Command::new(bench)
        .arg("run")
        .arg(plan)
        .arg(database)
        .output()
        .map_err(|error| format!("sqlite-bench did not start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sqlite-bench failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(Sample::parse)
        .collect())
}

/// Returns SQLite's own plan for a query, so the two can be compared.
///
/// @param database - the fixture
/// @param sql - the statement
fn explain(database: &Path, sql: &str) -> Result<Vec<String>, String> {
    let shell = workspace_root().join(".sqlite-ref/3.53.4/shell/sqlite3.exe");
    let shell = if shell.exists() {
        shell
    } else {
        workspace_root().join(".sqlite-ref/3.53.4/shell/sqlite3")
    };
    let output = Command::new(shell)
        .arg(database)
        .arg(format!("EXPLAIN QUERY PLAN {sql};"))
        .output()
        .map_err(|error| format!("the sqlite shell did not start: {error}"))?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

/// Renders the plan file `sqlite-bench` reads.
///
/// The setup section is empty: the fixture already exists and `run` does not
/// build it. The workloads are the four the new engine ran, in the same order.
///
/// @param repeat - how many times each workload runs inside one timed round
fn plan_file(repeat: u32) -> String {
    let mut out = String::new();
    out.push_str("# read.analytical, Phase 1 gate. Both engines read this file.\n");
    out.push_str("version\t1\n");
    out.push_str("scale\tmedium\n");
    out.push_str("rows\t100000\n");
    out.push_str("journal\tdelete\n");
    out.push_str("synchronous\tfull\n");
    out.push_str("page_size\t4096\n");
    out.push_str("cache_size\t-2000\n");
    for (name, sql) in WORKLOADS {
        out.push_str(&format!("workload\t{name}\n"));
        out.push_str("family\tread.analytical\n");
        out.push_str(&format!("repeat\t{repeat}\n"));
        out.push_str("txn\tnone\n");
        out.push_str("prepare\tonce\n");
        out.push_str(&format!("sql\t{sql}\n"));
    }
    out
}

/// Returns the `sqlite-bench` executable, if it has been built.
fn sqlite_bench() -> Option<PathBuf> {
    let root = workspace_root().join(".sqlite-ref/3.53.4");
    for name in ["sqlite-bench.exe", "sqlite-bench"] {
        let candidate = root.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}
