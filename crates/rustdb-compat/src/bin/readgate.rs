//! The Phase 2 gate: the four read families on the new engine, against SQLite.
//!
//! Invariant: a timing is read only after the two engines agree on the answer.
//! Each round digests every value of every row on both sides with the same
//! function the scorecard uses, and a workload whose digests differ is reported
//! as a correctness failure and is not timed at all. That is the existing
//! contract and this binary does not relax it because the file is no longer
//! shared - if anything the import makes it more load-bearing, since a bug in
//! the import now shows up here rather than being impossible.
//!
//! ## Four fairness questions, all answered in the output
//!
//! **Same SQL.** The workloads come from `rustdb_compat::perf::plan_for`, which
//! is the function the scorecard itself calls. Phase 1's harness kept its own
//! copy of the four analytical query strings; this one cannot drift from the
//! scorecard because there is nothing to drift from.
//!
//! **Same structure.** `EXPLAIN QUERY PLAN` is printed for SQLite and the
//! physical operator list for rust-db, per workload. SQLite answers three of
//! the four analytical shapes from a covering index, and a ratio measured
//! against a table scan on our side would be a ratio between two different
//! amounts of work rather than between two engines.
//!
//! **Same repeat count.** From `repeats_for`, the scorecard's own table.
//!
//! **Same cache size.** This is the question Phase 1 could not answer and this
//! phase exists to close. The new engine's data now lives in a file behind a
//! buffer pool of a stated size, and `--frames` sets it. SQLite's `cache_size`
//! is set to the *same number of bytes* in the plan file both arms read, and
//! both numbers are printed. Phase 1's report said "the new engine's trees are
//! fully resident and SQLite runs at the plan's 2 MB `cache_size`" and named
//! `scan.sort` at 7.58x as the number that would move. `--frames` is how a
//! reader moves it.
//!
//! Usage:
//!   rustdb-readgate <sqlite fixture> [--rounds N] [--page-size N] [--scale S]
//!                   [--frames N] [--families a,b] [--repeat N]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::Instant;

use rustdb_compat::newengine::ImportedDatabase;
use rustdb_compat::perf::{plan_for, Bind, Digest, Paired, Sample, Workload};
use rustdb_compat::workspace_root;
use rustdb_exec::physical::{Params, Prepared};
use rustdb_sql::plan::PhysicalPlan;
use rustdb_tree::datum::OwnedDatum;

/// The seed the bootstrap uses, fixed so a report is reproducible.
const SEED: u64 = 17_900_001;

/// The families this gate covers, with the TDD's bar for each.
///
/// The bars are the TDD's Phase 2 acceptance, quoted: "`read.point` at least
/// 2.0x, `read.range` at least 3.0x, `read.join` at least 3.0x,
/// `read.analytical` still at least 5.0x, all lower bounds at medium".
const FAMILIES: [(&str, f64); 4] = [
    ("read.point", 2.0),
    ("read.range", 3.0),
    ("read.join", 3.0),
    ("read.analytical", 5.0),
];

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = arguments.first().filter(|value| !value.starts_with("--")) else {
        eprintln!(
            "usage: rustdb-readgate <sqlite fixture> [--rounds N] [--page-size N] \
             [--scale S] [--frames N] [--families a,b] [--repeat N] \
             [--unfair-sqlite-cache-kib N]"
        );
        return ExitCode::from(2);
    };
    let settings = Settings::from(&arguments);
    match run(&PathBuf::from(fixture), &settings) {
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

/// What the command line asked for.
struct Settings {
    rounds: u32,
    page_size: usize,
    scale: String,
    frames: usize,
    families: Vec<String>,
    repeat_override: Option<u32>,
    /// SQLite's cache in KiB, when the caller wants it *not* matched.
    ///
    /// The gate exists to stop this being possible by accident, so the only
    /// way to get an unmatched comparison is to ask for one by name - and the
    /// output then says, in the fairness line, that the run is not fair. It is
    /// here because "the fairness fix mattered" is a claim, and a claim about a
    /// measurement is worth what the measurement of it is worth.
    unfair_cache_kib: Option<i32>,
}

impl Settings {
    /// Reads the settings off the command line.
    ///
    /// @param arguments - the command line, without the program name
    fn from(arguments: &[String]) -> Settings {
        let scale = flag(arguments, "--scale").unwrap_or_else(|| "medium".to_string());
        let page_size = flag(arguments, "--page-size")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(32_768);
        // The default pool is large enough to hold every fixture, which is the
        // configuration Phase 1 measured under without being able to say so.
        // It is a default rather than a policy: the number is printed, and a
        // reader who wants the cache-limited comparison passes `--frames`.
        let frames = flag(arguments, "--frames")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(4_096);
        let families = flag(arguments, "--families")
            .map(|value| {
                value
                    .split(',')
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_else(|| FAMILIES.iter().map(|(name, _)| name.to_string()).collect());
        Settings {
            rounds: flag(arguments, "--rounds")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(30),
            page_size,
            scale,
            frames,
            families,
            repeat_override: flag(arguments, "--repeat").and_then(|value| value.parse().ok()),
            unfair_cache_kib: flag(arguments, "--unfair-sqlite-cache-kib")
                .and_then(|value| value.parse::<i32>().ok()),
        }
    }
}

/// Returns the reason an engine error carries.
///
/// `DbError::message` answers with the *code's* message when no caller set one
/// - "bad parameter or other API misuse" - and the physical pass puts what it
/// actually refused in the detail. A harness that printed only the message made
/// its first failure unreadable, so this prints the detail when there is one.
///
/// @param error - the error to explain
fn why(error: &rustdb_base::DbError) -> String {
    match error.detail() {
        Some(detail) => detail.to_string(),
        None => why(&error),
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

/// Runs the gate and reports whether every family's bar was met.
///
/// @param fixture - the SQLite database both engines read from
/// @param settings - what the command line asked for
fn run(fixture: &Path, settings: &Settings) -> Result<bool, String> {
    let bench = sqlite_bench().ok_or_else(|| {
        "sqlite-bench is not built; run tools/sqlite-reference.ps1 first".to_string()
    })?;

    // The plan both arms read, from the scorecard's own table.
    let mut plan = plan_for(&settings.scale);
    plan.setup.clear();
    plan.workloads.retain(|workload| {
        settings
            .families
            .iter()
            .any(|name| *name == workload.family)
    });
    if let Some(repeat) = settings.repeat_override {
        for workload in &mut plan.workloads {
            workload.repeat = repeat;
        }
    }
    if plan.workloads.is_empty() {
        return Err(format!(
            "no workload is in the families {:?}",
            settings.families
        ));
    }
    // The fairness lever. SQLite's `cache_size` is negative KiB; the pool is
    // frames of `page_size` bytes. Setting one from the other is what lets the
    // report state one cache configuration instead of describing an asymmetry.
    let pool_bytes = settings.frames.saturating_mul(settings.page_size);
    plan.cache_size = match settings.unfair_cache_kib {
        Some(kib) => -kib,
        None => -((pool_bytes / 1024) as i32),
    };

    let imported_at = Instant::now();
    let database =
        ImportedDatabase::import_with(fixture.to_path_buf(), settings.page_size, settings.frames)
            .map_err(|error| format!("import failed: {}", why(&error)))?;
    database
        .warm()
        .map_err(|error| format!("warming failed: {}", why(&error)))?;
    println!(
        "imported into {} trees at page size {} in {:.1}s",
        database.roots().len(),
        settings.page_size,
        imported_at.elapsed().as_secs_f64()
    );
    println!(
        "  file        : {} ({} pages)",
        database.file().display(),
        database.page_count()
    );
    println!(
        "  pool        : {} frames = {:.1} MiB",
        settings.frames,
        database.pool_bytes() as f64 / (1024.0 * 1024.0)
    );
    let sqlite_mib = -(plan.cache_size as f64) / 1024.0;
    println!(
        "  sqlite cache: {} = {:.1} MiB",
        plan.cache_size, sqlite_mib
    );
    match settings.unfair_cache_kib {
        None => println!("  fairness    : matched - one memory budget, both engines"),
        Some(_) => println!(
            "  fairness    : NOT MATCHED - rust-db has {:.1} MiB and SQLite has {:.1} MiB. \
             Every ratio below is inflated by whatever that difference is worth, and this run \
             exists only to measure how much.",
            pool_bytes as f64 / (1024.0 * 1024.0),
            sqlite_mib
        ),
    }

    let plan_path =
        std::env::temp_dir().join(format!("rustdb-readgate-{}.plan", std::process::id()));
    std::fs::write(&plan_path, plan.render())
        .map_err(|error| format!("could not write the plan: {error}"))?;

    // Plan once, run many: `prepare once` in a scorecard plan means the prepare
    // is outside the timed region on both sides.
    let mut prepared: Vec<Prepared_> = Vec::with_capacity(plan.workloads.len());
    let mut refused: Vec<(String, String)> = Vec::new();
    for workload in &plan.workloads {
        match prepare_one(&database, workload) {
            Ok(entry) => prepared.push(entry),
            Err(reason) => refused.push((workload.name.clone(), reason)),
        }
    }

    println!();
    println!("## plans, both engines");
    for entry in &prepared {
        let theirs = explain(fixture, &entry.sql)?;
        println!("  {}", entry.name);
        // The operator chain the builder actually made, not the plan it was
        // asked for. Building it needs a sink, so a throwaway one is handed in
        // and never pushed into.
        let chain = database
            .pipeline(
                &entry.plan,
                &entry.choice,
                &entry.params_for(1, plan.rows),
                Box::new(CountRows { rows: 0 }),
            )
            .map(|(_, shape)| shape.operators.join(" -> "))
            .unwrap_or_else(|error| format!("<{}>", why(&error)));
        println!("    rust-db : {chain}");
        println!("    sqlite  : {}", theirs.join(" | "));
    }
    if !refused.is_empty() {
        println!();
        println!("## refused by the physical pass");
        for (name, reason) in &refused {
            println!("  {name}: {reason}");
        }
    }

    // Where an execution's time goes, before any of it is compared to
    // anything. This breakdown exists because two rounds of optimising
    // `scan.distinct` in Phase 1 were spent on hypotheses a measurement would
    // have refused in a minute.
    println!();
    println!("## where one execution goes, microseconds");
    println!(
        "  {:<18} {:>10} {:>10} {:>10} {:>8}",
        "workload", "build", "+produce", "+digest", "rows"
    );
    for entry in &prepared {
        let iterations = 64u32;
        let params = entry.params_for(1, plan.rows);
        let build = time_stage(iterations, || {
            let sink = Box::new(DigestRows {
                folded: Rc::new(RefCell::new(Folded::default())),
            });
            let built = database
                .pipeline(&entry.plan, &entry.choice, &params, sink)
                .map_err(|error| format!("{}: {}", entry.name, why(&error)))?;
            drop(built);
            Ok(())
        })?;
        let produce = time_stage(iterations, || {
            let counter = Box::new(CountRows { rows: 0 });
            let (mut pipeline, _) = database
                .pipeline(&entry.plan, &entry.choice, &params, counter)
                .map_err(|error| format!("{}: {}", entry.name, why(&error)))?;
            pipeline
                .run()
                .map_err(|error| format!("{}: {}", entry.name, why(&error)))?;
            Ok(())
        })?;
        let folded = Rc::new(RefCell::new(Folded::default()));
        let whole = time_stage(iterations, || {
            let sink = Box::new(DigestRows {
                folded: Rc::clone(&folded),
            });
            let (mut pipeline, _) = database
                .pipeline(&entry.plan, &entry.choice, &params, sink)
                .map_err(|error| format!("{}: {}", entry.name, why(&error)))?;
            pipeline
                .run()
                .map_err(|error| format!("{}: {}", entry.name, why(&error)))?;
            Ok(())
        })?;
        let rows = folded.borrow().rows / u64::from(iterations);
        println!(
            "  {:<18} {:>10.2} {:>10.2} {:>10.2} {rows:>8}",
            entry.name,
            build / 1000.0,
            produce / 1000.0,
            whole / 1000.0
        );
    }

    // `PointProbe` measured on its own, which is a TDD acceptance item in its
    // own right: "under 500 ns warm on the medium fixture". It is timed here
    // rather than inferred from `point.rowid`'s ratio, because that workload
    // includes the pipeline around the probe and the acceptance is about the
    // probe.
    let probe_nanos = measure_point_probe(&database, plan.rows)?;
    if let Some(nanos) = probe_nanos {
        println!();
        println!("## PointProbe, warm");
        println!("  {nanos:.1} ns per probe   (TDD acceptance: under 500 ns)");
        // Where a probe's time goes, in the same shape as the pipeline
        // breakdown above and for the same reason: the descent is the cost of
        // three of the four read families, and a guess about which part of it
        // is expensive is a guess that has been wrong four times on this
        // project.
        let (fetch, descend, searched) = measure_descent(&database, plan.rows)?;
        println!("  {fetch:.1} ns per resident fetch");
        println!("  {descend:.1} ns per descent to a leaf");
        println!(
            "  {searched:.1} ns per descent + leaf search   (the search alone: {:.1} ns)",
            searched - descend
        );
    }

    let mut measured: Vec<Paired> = prepared
        .iter()
        .map(|entry| Paired {
            workload: entry.name.clone(),
            family: entry.family.clone(),
            pairs: Vec::with_capacity(settings.rounds as usize),
            agreed: true,
            disagreement: String::new(),
        })
        .collect();

    println!();
    println!("## {} paired rounds, interleaved", settings.rounds);
    for round in 0..settings.rounds {
        // The engine order alternates by round so a warm cache or a busy
        // machine does not systematically favour whichever went first.
        let ours_first = round % 2 == 0;
        let (ours, theirs) = if ours_first {
            let ours = time_new_engine(&database, &prepared, plan.rows)?;
            let theirs = run_sqlite(&bench, &plan_path, fixture)?;
            (ours, theirs)
        } else {
            let theirs = run_sqlite(&bench, &plan_path, fixture)?;
            let ours = time_new_engine(&database, &prepared, plan.rows)?;
            (ours, theirs)
        };
        for (index, entry) in prepared.iter().enumerate() {
            let Some(slot) = measured.get_mut(index) else {
                continue;
            };
            let Some(mine) = ours.iter().find(|sample| sample.workload == entry.name) else {
                return Err(format!("{}: the new engine produced no sample", entry.name));
            };
            let Some(reference) = theirs.iter().find(|sample| sample.workload == entry.name) else {
                return Err(format!("{}: sqlite produced no sample", entry.name));
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
        "  {:<18} {:>14} {:>14} {:>9} {:>9} {:>9}  {}",
        "workload", "rust-db ns", "sqlite ns", "ratio", "low", "high", "agreed"
    );
    let mut passed = refused.is_empty();
    if !refused.is_empty() {
        println!(
            "  (the physical pass refused {} workload(s); the gate cannot pass without them)",
            refused.len()
        );
    }
    for entry in &measured {
        if !entry.agreed {
            println!(
                "  {:<18} {:>14} {:>14} {:>9} {:>9} {:>9}  NO: {}",
                entry.workload, "-", "-", "-", "-", "-", entry.disagreement
            );
            passed = false;
            continue;
        }
        let (ours, theirs) = entry.medians();
        let (low, high) = entry.interval(SEED);
        println!(
            "  {:<18} {:>14.0} {:>14.0} {:>8.2}x {:>8.2}x {:>8.2}x  yes",
            entry.workload,
            ours,
            theirs,
            entry.ratio(),
            low,
            high
        );
    }

    println!();
    println!("## families");
    println!(
        "  {:<18} {:>9} {:>9} {:>9} {:>8}  {}",
        "family", "ratio", "low", "high", "bar", "verdict"
    );
    for (family, bar) in FAMILIES {
        if !settings.families.iter().any(|name| name == family) {
            continue;
        }
        // The family figure is the arithmetic mean of the paired log ratios,
        // exponentiated - the geometric mean - pooled over every workload in
        // the family. That is `family_interval` in `scorecard.rs`, character
        // for character, and it is written that way here rather than more
        // conveniently because a gate measured by a different statistic than
        // the scorecard reports is a gate on a different number. Phase 1's
        // first harness took the median and read 5.21x where the scorecard's
        // statistic said 3.88x on the same samples.
        let ratios: Vec<f64> = measured
            .iter()
            .filter(|entry| entry.family == family && entry.agreed)
            .flat_map(|entry| entry.log_ratios())
            .collect();
        if ratios.is_empty() {
            println!(
                "  {family:<18} {:>9} {:>9} {:>9} {bar:>7.2}x  NO DATA",
                "-", "-", "-"
            );
            passed = false;
            continue;
        }
        let point = (ratios.iter().sum::<f64>() / ratios.len() as f64).exp();
        let (low, high) = rustdb_compat::perf::bootstrap(&ratios, SEED);
        let (low, high) = (low.exp(), high.exp());
        let met = low >= bar;
        println!(
            "  {family:<18} {point:>8.2}x {low:>8.2}x {high:>8.2}x {bar:>7.2}x  {}",
            if met { "MET" } else { "MISSED" }
        );
        passed = passed && met;
    }

    if let Some(nanos) = probe_nanos {
        let met = nanos < 500.0;
        println!(
            "  {:<18} {nanos:>8.1}ns {:>9} {:>9} {:>7}ns  {}",
            "PointProbe",
            "-",
            "-",
            500,
            if met { "MET" } else { "MISSED" }
        );
        passed = passed && met;
    }

    println!();
    println!("  VERDICT: {}", if passed { "MET" } else { "MISSED" });
    Ok(passed)
}

/// One workload, planned and prepared.
struct Prepared_ {
    name: String,
    family: String,
    sql: String,
    binds: Vec<Bind>,
    repeat: u32,
    plan: PhysicalPlan,
    choice: Prepared,
}

impl Prepared_ {
    /// Returns the parameters for one iteration, generated the way
    /// `sqlite_bench.c` generates them.
    ///
    /// The formulas are that file's, value for value, and they have to agree
    /// exactly: a benchmark whose two arms read different rows is not a
    /// comparison.
    ///
    /// @param iteration - which iteration, from zero
    /// @param rows - how many rows the base table holds
    fn params_for(&self, iteration: u32, rows: u32) -> Params {
        Params::from_values(
            self.binds
                .iter()
                .map(|bind| bind_value(*bind, iteration, rows))
                .collect(),
        )
    }
}

/// Returns the value one bind kind produces for one iteration.
///
/// The formulas are `compat/oracle/sqlite_bench.c`'s `bind_one`, transcribed.
///
/// @param bind - the bind kind
/// @param iteration - which iteration, from zero
/// @param rows - how many rows the base table holds
fn bind_value(bind: Bind, iteration: u32, rows: u32) -> OwnedDatum {
    let iteration = u64::from(iteration);
    let rows64 = u64::from(rows);
    match bind {
        Bind::Rowid => OwnedDatum::Int(if rows > 0 {
            1 + (iteration % rows64) as i64
        } else {
            1
        }),
        Bind::Scatter => OwnedDatum::Int(if rows > 0 {
            1 + (iteration.wrapping_mul(2_654_435_761) % rows64) as i64
        } else {
            1
        }),
        Bind::Counter => OwnedDatum::Int((rows64 + 1 + iteration) as i64),
        Bind::Int => OwnedDatum::Int(
            (iteration.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7fff_ffff) as i64,
        ),
        Bind::Text => OwnedDatum::Text(
            format!("row {iteration} lorem ipsum dolor sit amet consectetur").into_bytes(),
        ),
        Bind::Blob => {
            OwnedDatum::Blob((0..64u64).map(|j| ((iteration + j) & 0xff) as u8).collect())
        }
    }
}

/// Plans and prepares one workload, or says why the physical pass refused it.
///
/// @param database - the imported trees
/// @param workload - the workload from the plan
fn prepare_one(database: &ImportedDatabase, workload: &Workload) -> Result<Prepared_, String> {
    let plan = database.plan(&workload.sql).map_err(|error| why(&error))?;
    let choice = database.prepare(&plan).map_err(|error| why(&error))?;
    Ok(Prepared_ {
        name: workload.name.clone(),
        family: workload.family.clone(),
        sql: workload.sql.clone(),
        binds: workload.binds.clone(),
        repeat: workload.repeat,
        plan,
        choice,
    })
}

/// What a digesting run accumulated, shared with the caller.
#[derive(Default)]
struct Folded {
    digest: Digest,
    rows: u64,
}

/// A sink that digests rows as they are produced and keeps none of them.
///
/// The SQLite arm digests inside its `sqlite3_step` loop and keeps nothing, so
/// a rust-db arm that built a `Vec<Vec<OwnedDatum>>` first would be timing a
/// result set neither engine's caller asked for.
struct DigestRows {
    folded: Rc<RefCell<Folded>>,
}

impl rustdb_exec::Sink for DigestRows {
    fn push(&mut self, batch: &rustdb_exec::Batch<'_>) -> rustdb_base::DbResult<rustdb_exec::Flow> {
        let mut held = self.folded.borrow_mut();
        for nth in 0..batch.live() {
            for column in 0..batch.columns.len() {
                let value = batch.value(nth, column)?;
                eat_borrowed(&mut held.digest, &value);
            }
            held.rows = held.rows.saturating_add(1);
        }
        Ok(rustdb_exec::Flow::Continue)
    }

    fn finish(&mut self) -> rustdb_base::DbResult<()> {
        Ok(())
    }
}

/// A sink that counts rows and reads no values, for the breakdown.
struct CountRows {
    rows: u64,
}

impl rustdb_exec::Sink for CountRows {
    fn push(&mut self, batch: &rustdb_exec::Batch<'_>) -> rustdb_base::DbResult<rustdb_exec::Flow> {
        self.rows = self.rows.saturating_add(batch.live() as u64);
        Ok(rustdb_exec::Flow::Continue)
    }

    fn finish(&mut self) -> rustdb_base::DbResult<()> {
        Ok(())
    }
}

/// Adds one borrowed value to the digest, tagged the way the reference tags it.
///
/// @param digest - the running digest
/// @param value - the value to fold in
fn eat_borrowed(digest: &mut Digest, value: &rustdb_tree::datum::Datum<'_>) {
    use rustdb_tree::datum::Datum;
    match value {
        Datum::Null => digest.tag(0),
        Datum::Int(number) => {
            digest.tag(1);
            digest.word(*number as u64);
        }
        Datum::Real(number) => {
            digest.tag(2);
            digest.word(number.to_bits());
        }
        Datum::Text(bytes) => {
            digest.tag(3);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
        Datum::Blob(bytes) => {
            digest.tag(4);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
    }
}

/// Runs every workload through the new engine and returns timed samples.
///
/// @param database - the imported trees
/// @param prepared - the planned workloads
/// @param rows - how many rows the base table holds, for the bind formulas
fn time_new_engine(
    database: &ImportedDatabase,
    prepared: &[Prepared_],
    rows: u32,
) -> Result<Vec<Sample>, String> {
    let mut samples = Vec::with_capacity(prepared.len());
    for entry in prepared {
        let folded = Rc::new(RefCell::new(Folded::default()));
        let started = Instant::now();
        for iteration in 0..entry.repeat {
            let params = Params::from_values(
                entry
                    .binds
                    .iter()
                    .map(|bind| bind_value(*bind, iteration, rows))
                    .collect(),
            );
            let sink = Box::new(DigestRows {
                folded: Rc::clone(&folded),
            });
            let (mut pipeline, _) = database
                .pipeline(&entry.plan, &entry.choice, &params, sink)
                .map_err(|error| format!("{}: {}", entry.name, why(&error)))?;
            pipeline
                .run()
                .map_err(|error| format!("{}: {}", entry.name, why(&error)))?;
        }
        let elapsed = started.elapsed();
        let held = folded.replace(Folded::default());
        samples.push(Sample {
            workload: entry.name.clone(),
            nanos: elapsed.as_secs_f64() * 1e9,
            rows: held.rows,
            digest: held.digest.finish(),
        });
    }
    Ok(samples)
}

/// Measures a bare `PointProbe` on the table tree, warm.
///
/// The TDD's acceptance is about the probe itself: "descends the tree with
/// swizzled pointers, finds the row, evaluates the predicate on the row's
/// values in place, and writes the projected values into the statement's result
/// slots. No batch, no vector, no selection... under 500 ns per probe for a
/// warm three-level tree". So this times exactly that: the probe object, a
/// scattered key per call, and a caller-owned buffer.
///
/// @param database - the imported trees
/// @param rows - how many rows the base table holds
fn measure_point_probe(database: &ImportedDatabase, rows: u32) -> Result<Option<f64>, String> {
    let Some(root) = database.table_root("main_table") else {
        return Ok(None);
    };
    let Some(tree) = rustdb_exec::physical::TreeCatalog::tree(database, root) else {
        return Ok(None);
    };
    let width = tree.columns().len();
    let probe = rustdb_exec::PointProbe::new(tree, rustdb_exec::Projection::all(width));
    let pool = rustdb_exec::physical::TreeCatalog::pool(database);
    let mut out: Vec<OwnedDatum> = Vec::with_capacity(width);
    let keys: Vec<i64> = (0..4_096u64)
        .map(|iteration| {
            1 + (iteration.wrapping_mul(2_654_435_761) % u64::from(rows.max(1))) as i64
        })
        .collect();
    // Warm the path, then measure it.
    for key in &keys {
        probe
            .lookup(pool, &[rustdb_tree::datum::Datum::Int(*key)], &mut out)
            .map_err(|error| why(&error))?;
    }
    let started = Instant::now();
    let passes = 8u32;
    for _ in 0..passes {
        for key in &keys {
            probe
                .lookup(pool, &[rustdb_tree::datum::Datum::Int(*key)], &mut out)
                .map_err(|error| why(&error))?;
        }
    }
    let elapsed = started.elapsed().as_secs_f64() * 1e9;
    Ok(Some(elapsed / (f64::from(passes) * keys.len() as f64)))
}

/// Measures a resident page fetch and a bare descent, separately.
///
/// Returns nanoseconds per fetch and nanoseconds per descent. The descent is
/// `PointProbe` minus the leaf search and the projection, so the difference
/// between the two numbers above and the probe's own is the leaf work.
///
/// @param database - the imported trees
/// @param rows - how many rows the base table holds
fn measure_descent(database: &ImportedDatabase, rows: u32) -> Result<(f64, f64, f64), String> {
    let Some(root) = database.table_root("main_table") else {
        return Ok((0.0, 0.0, 0.0));
    };
    let Some(tree) = rustdb_exec::physical::TreeCatalog::tree(database, root) else {
        return Ok((0.0, 0.0, 0.0));
    };
    let pool = rustdb_exec::physical::TreeCatalog::pool(database);
    let keys: Vec<i64> = (0..4_096u64)
        .map(|iteration| {
            1 + (iteration.wrapping_mul(2_654_435_761) % u64::from(rows.max(1))) as i64
        })
        .collect();
    let encoded: Vec<Vec<u8>> = keys
        .iter()
        .map(|key| tree.encode_key(&[rustdb_tree::datum::Datum::Int(*key)]))
        .collect();

    // One resident fetch of the root, which is the cheapest fetch there is.
    let page = tree.root();
    for _ in 0..1_000 {
        let _ = pool.fetch(page).map_err(|error| why(&error))?;
    }
    let started = Instant::now();
    let passes = 100_000u32;
    for _ in 0..passes {
        let guard = pool.fetch(page).map_err(|error| why(&error))?;
        std::hint::black_box(guard.bytes().len());
    }
    let fetch = started.elapsed().as_secs_f64() * 1e9 / f64::from(passes);

    for key in &encoded {
        let _ = tree.descend_guard(pool, key).map_err(|error| why(&error))?;
    }
    let started = Instant::now();
    let rounds = 8u32;
    for _ in 0..rounds {
        for key in &encoded {
            let (guard, _) = tree.descend_guard(pool, key).map_err(|error| why(&error))?;
            std::hint::black_box(guard.bytes().len());
        }
    }
    let descend =
        started.elapsed().as_secs_f64() * 1e9 / (f64::from(rounds) * encoded.len() as f64);

    // The same descent plus the leaf's own binary search, so the difference
    // says what the search costs on its own.
    let started = Instant::now();
    for _ in 0..rounds {
        for (index, key) in encoded.iter().enumerate() {
            let (guard, _) = tree.descend_guard(pool, key).map_err(|error| why(&error))?;
            let leaf =
                rustdb_tree::leaf::LeafRef::parse(guard.bytes()).map_err(|error| why(&error))?;
            let found = leaf
                .search(&[rustdb_tree::datum::Datum::Int(keys[index])])
                .map_err(|error| why(&error))?;
            std::hint::black_box(found.is_ok());
        }
    }
    let searched =
        started.elapsed().as_secs_f64() * 1e9 / (f64::from(rounds) * encoded.len() as f64);
    Ok((fetch, descend, searched))
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
