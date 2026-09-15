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
//! ## Superseded, and kept anyway
//!
//! Phase 2's gate is `inillucent-readgate`, which measures all four read families
//! and reads its workloads out of `inillucent_compat::perf::plan_for` rather than
//! keeping its own copy. This binary stays because the Phase 1 report quotes
//! its output and a number nobody can reproduce is not evidence. It now runs on
//! the Phase 2 engine - a real file behind a buffer pool - so its numbers are
//! comparable to, and not identical with, the ones the Phase 1 report
//! captured.
//!
//! Usage: inillucent-analytical `<sqlite fixture>` [--rounds N] [--page-size N]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::perf::{eat_borrowed, run_sqlite};
use inillucent_compat::perf::{Digest, Paired, Sample};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;

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
        eprintln!("usage: inillucent-analytical <sqlite fixture> [--rounds N] [--page-size N]");
        return ExitCode::from(2);
    };
    let rounds = flag(&arguments, "--rounds")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(30);
    let page_size = flag(&arguments, "--page-size")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(32_768);
    // The scale decides the row count and the repeat count, and both are read
    // from `scorecard.rs` rather than chosen here: `rows_for` gives 5,000 /
    // 100,000 / 600,000 and `repeats_for(...).1` gives 400 / 40 / 4 for the
    // scan family. A harness that used a different repeat would be measuring a
    // different amount of amortisation of each engine's per-statement setup,
    // and SQLite's is large enough for that to move `scan.distinct` by 5x.
    let scale = flag(&arguments, "--scale").unwrap_or_else(|| "medium".to_string());
    let (rows, default_repeat) = match scale.as_str() {
        "small" => (5_000u32, 400u32),
        "large" => (600_000, 4),
        _ => (100_000, 40),
    };
    let repeat = flag(&arguments, "--repeat")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(default_repeat);
    match run(
        &PathBuf::from(fixture),
        rounds,
        page_size,
        repeat,
        &scale,
        rows,
    ) {
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
/// @param scale - the scale name, which fixes the repeat count
/// @param rows - how many rows the base table holds
fn run(
    fixture: &Path,
    rounds: u32,
    page_size: usize,
    repeat: u32,
    scale: &str,
    rows: u32,
) -> Result<bool, String> {
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
    let plan_path =
        std::env::temp_dir().join(format!("inillucent-analytical-{}.plan", std::process::id()));
    std::fs::write(&plan_path, plan_file(repeat, scale, rows))
        .map_err(|error| format!("could not write the plan: {error}"))?;

    println!();
    println!("## plans, both engines");
    for (name, sql) in WORKLOADS {
        let ours = database
            .describe(sql)
            .map_err(|error| format!("{name}: {}", error.message()))?;
        let theirs = explain(fixture, sql)?;
        println!("  {name}");
        println!("    inillucent : {}", ours.join(" | "));
        println!("    sqlite  : {}", theirs.join(" | "));
    }

    // Plan once, run many: `prepare_each: false` in the scorecard plan means
    // the prepare is outside the timed region on both sides.
    let mut prepared = Vec::with_capacity(WORKLOADS.len());
    for (name, sql) in WORKLOADS {
        let plan = database
            .plan(sql)
            .map_err(|error| format!("{name}: planning failed: {}", error.message()))?;
        let choice = database
            .prepare(&plan)
            .map_err(|error| format!("{name}: preparing failed: {}", error.message()))?;
        prepared.push((name, sql, plan, choice));
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

    // Where an execution's time goes, before any of it is compared to
    // anything. Three nested measurements over the same prepared statement:
    // building the operator chain and throwing it away; building it and
    // driving the source into a sink that counts and nothing else; and the
    // real thing, which adds digesting every value. Each is the one before it
    // plus one stage, so the differences are the stages.
    //
    // This exists because two rounds of optimising `scan.distinct` were spent
    // on hypotheses - key comparison, then leaf parsing during the descent -
    // that a measurement would have refused in a minute.
    println!();
    println!("## where one execution goes, microseconds");
    println!(
        "  {:<16} {:>10} {:>10} {:>10} {:>8}",
        "workload", "build", "+produce", "+digest", "rows"
    );
    for (name, _, plan, choice) in &prepared {
        let build = time_stage(64, || {
            let sink = Box::new(DigestRows {
                folded: Rc::new(RefCell::new(Folded::default())),
            });
            let built = database
                .pipeline(plan, choice, &Params::new(), sink)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            drop(built);
            Ok(())
        })?;
        let produce = time_stage(64, || {
            let counter = Box::new(CountRows { rows: 0 });
            let (mut pipeline, _) = database
                .pipeline(plan, choice, &Params::new(), counter)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            pipeline
                .run()
                .map_err(|error| format!("{name}: {}", error.message()))?;
            Ok(())
        })?;
        let folded = Rc::new(RefCell::new(Folded::default()));
        let whole = time_stage(64, || {
            let sink = Box::new(DigestRows {
                folded: Rc::clone(&folded),
            });
            let (mut pipeline, _) = database
                .pipeline(plan, choice, &Params::new(), sink)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            pipeline
                .run()
                .map_err(|error| format!("{name}: {}", error.message()))?;
            Ok(())
        })?;
        let rows = folded.borrow().rows / 64;
        println!(
            "  {name:<16} {:>10.1} {:>10.1} {:>10.1} {rows:>8}",
            build / 1000.0,
            produce / 1000.0,
            whole / 1000.0
        );
    }

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
        for (index, (name, _, _, _)) in prepared.iter().enumerate() {
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
                    "inillucent {} rows digest {:016x} against sqlite {} rows digest {:016x}",
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
        "  {:<16} {:>12} {:>12} {:>9} {:>9} {:>9}  agreed",
        "workload", "inillucent ns", "sqlite ns", "ratio", "low", "high"
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
        // The family figure is the arithmetic mean of the paired log ratios,
        // exponentiated - the geometric mean - pooled over every workload in
        // the family. That is `family_interval` in `scorecard.rs`, character
        // for character, and it is written that way here rather than more
        // conveniently because a gate measured by a different statistic than
        // the scorecard reports is a gate on a different number. The first
        // version of this harness took the median and read 5.21x where the
        // scorecard's statistic said 3.88x on the same samples.
        let family = (all_ratios.iter().sum::<f64>() / all_ratios.len() as f64).exp();
        let (low, high) = inillucent_compat::perf::bootstrap(&all_ratios, SEED);
        println!();
        println!(
            "  read.analytical: {family:.2}x  (95% interval {:.2}x .. {:.2}x over {} paired samples)",
            low.exp(),
            high.exp(),
            all_ratios.len()
        );
        println!("  Phase 1 gate: lower bound at least 5.00x at medium scale");
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

/// What a digesting run accumulated, shared with the caller.
///
/// The pipeline owns its sink, so the digest it builds has to live somewhere
/// the caller can still reach. One `Rc` is the whole mechanism.
#[derive(Default)]
struct Folded {
    digest: Digest,
    rows: u64,
}

/// A sink that digests rows as they are produced and keeps none of them.
///
/// The SQLite arm digests inside its `sqlite3_step` loop and keeps nothing, so
/// a inillucent arm that built a `Vec<Vec<OwnedDatum>>` first would be timing a
/// result set neither engine's caller asked for - one heap allocation per row,
/// which on a sixty-four-row answer was most of the measured cost. This is the
/// row shape both arms actually use.
struct DigestRows {
    folded: Rc<RefCell<Folded>>,
}

impl inillucent_exec::Sink for DigestRows {
    fn push(
        &mut self,
        batch: &inillucent_exec::Batch<'_>,
    ) -> inillucent_base::DbResult<inillucent_exec::Flow> {
        let mut held = self.folded.borrow_mut();
        for nth in 0..batch.live() {
            for column in 0..batch.columns.len() {
                let value = batch.value(nth, column)?;
                eat_borrowed(&mut held.digest, &value);
            }
            held.rows = held.rows.saturating_add(1);
        }
        Ok(inillucent_exec::Flow::Continue)
    }

    fn finish(&mut self) -> inillucent_base::DbResult<()> {
        Ok(())
    }
    /// Returns the sink to its pre-input state; it keeps no rows to forget.
    fn reset(&mut self) -> inillucent_base::DbResult<()> {
        Ok(())
    }
}

/// Runs every workload through the new engine and returns timed samples.
///
/// The timed region is the one the scorecard times on the old engine: the
/// prepare is outside it, and building the operator chain, producing the rows
/// and digesting every value are inside it. Digesting inside the timer costs
/// both engines the same work, because `sqlite-bench` digests inside its own.
///
/// @param database - the imported trees
/// @param prepared - the planned workloads
/// @param repeat - how many times each workload runs inside one sample
fn time_new_engine(
    database: &ImportedDatabase,
    prepared: &[(
        &str,
        &str,
        inillucent_sql::plan::PhysicalPlan,
        inillucent_exec::physical::Prepared,
    )],
    repeat: u32,
) -> Result<Vec<Sample>, String> {
    let mut samples = Vec::with_capacity(prepared.len());
    for (name, _, plan, choice) in prepared {
        let folded = Rc::new(RefCell::new(Folded::default()));
        let started = Instant::now();
        for _ in 0..repeat {
            let sink = Box::new(DigestRows {
                folded: Rc::clone(&folded),
            });
            let (mut pipeline, _) = database
                .pipeline(plan, choice, &Params::new(), sink)
                .map_err(|error| format!("{name}: {}", error.message()))?;
            pipeline
                .run()
                .map_err(|error| format!("{name}: {}", error.message()))?;
        }
        let elapsed = started.elapsed();
        let held = folded.replace(Folded::default());
        samples.push(Sample {
            workload: (*name).to_string(),
            nanos: elapsed.as_secs_f64() * 1e9,
            rows: held.rows,
            digest: held.digest.finish(),
        });
    }
    Ok(samples)
}

/// A sink that counts rows and reads no values, for the breakdown.
struct CountRows {
    rows: u64,
}

impl inillucent_exec::Sink for CountRows {
    fn push(
        &mut self,
        batch: &inillucent_exec::Batch<'_>,
    ) -> inillucent_base::DbResult<inillucent_exec::Flow> {
        self.rows = self.rows.saturating_add(batch.live() as u64);
        Ok(inillucent_exec::Flow::Continue)
    }

    fn finish(&mut self) -> inillucent_base::DbResult<()> {
        Ok(())
    }
    /// Returns the sink to its pre-input state; it keeps no rows to forget.
    fn reset(&mut self) -> inillucent_base::DbResult<()> {
        Ok(())
    }
}

/// Times one stage over several iterations and returns nanoseconds per one.
///
/// @param iterations - how many times to run the body
/// @param body - the stage to time
fn time_stage(
    iterations: u32,
    mut body: impl FnMut() -> Result<(), String>,
) -> Result<f64, String> {
    body()?;
    let started = Instant::now();
    for _ in 0..iterations {
        body()?;
    }
    Ok(started.elapsed().as_secs_f64() * 1e9 / f64::from(iterations))
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
/// @param scale - the scale name the plan records
/// @param rows - how many rows the base table holds
fn plan_file(repeat: u32, scale: &str, rows: u32) -> String {
    let mut out = String::new();
    out.push_str("# read.analytical, Phase 1 gate. Both engines read this file.\n");
    out.push_str("version\t1\n");
    out.push_str(&format!(
        "scale	{scale}
"
    ));
    out.push_str(&format!(
        "rows	{rows}
"
    ));
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
