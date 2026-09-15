//! The release performance scorecard: both engines, one plan, paired samples.
//!
//! Invariant: nothing here decides what a workload is. The plan is generated
//! once, written to a file, and read by both arms - this program for inillucent,
//! and `sqlite-bench`, compiled from the pinned amalgamation, for SQLite. The
//! two arms therefore run the same SQL with the same parameters in the same
//! transactions against databases built by the same statements, and the
//! fairness contract is a property of the file rather than of anybody's care.
//!
//! What a round is: clone both pristine databases, run every workload on both
//! engines in plan order, record a timing, a row count and a digest for each.
//! The engine order alternates by round so a warm cache or a busy machine does
//! not systematically favour whichever went first. A workload whose two digests
//! differ is not timed; it is reported as a correctness failure, and the family
//! it belongs to cannot pass.
//!
//! Usage:
//!
//! ```text
//! cargo run --release -p inillucent-compat --bin inillucent-scorecard -- \
//!     [--scale small|medium|large|all] [--rounds N] [--out <dir>] [--label <text>]
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use inillucent_compat::history;
use inillucent_compat::perf::{
    plan_for, Bind, Contract, Digest, Grouping, Paired, Plan, Sample, Verdict, Workload,
};
use inillucent_compat::report::json_string;
use inillucent_compat::{platform_name, workspace_root};
use inillucent_engine::connect::{Connection, Database, Statement};
use inillucent_sql::plan::Levers;
use inillucent_tree::datum::OwnedDatum;

/// How many paired rounds a scale is measured over.
///
/// Thirty is the floor the TDD sets for an end-to-end family, and it is the
/// number below which a bootstrap interval starts describing the resampling
/// rather than the data.
const DEFAULT_ROUNDS: u32 = 30;

/// The seed every bootstrap uses, declared here so a report is reproducible.
const SEED: u64 = 17_900_001;

/// Runs the scorecard.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let out = flag(&arguments, "--out")
        .map(PathBuf::from)
        // Ticket-neutral. The default used to name one ticket's output folder,
        // so a later ticket that forgot `--out` regenerated that ticket's
        // scorecard and dashboard in place and appended to its history. The
        // published copies live in `compat/release/` and were never at risk,
        // but a run should not write into another run's evidence by default.
        .unwrap_or_else(|| workspace_root().join("_agent_output/scorecard"));
    let rounds = flag(&arguments, "--rounds")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(DEFAULT_ROUNDS);
    let scales = match flag(&arguments, "--scale").as_deref() {
        Some("all") | None => vec!["small", "medium", "large"],
        Some(one) => vec![leak(one)],
    };
    let label = flag(&arguments, "--label").unwrap_or_else(|| "baseline".to_string());
    // The arm this run measures, as a mask of optimizations to switch *off*.
    // Named levers rather than a raw number, because a run recorded as
    // `--disable 3` is a number nobody can read back in six months.
    let disabled = match flag(&arguments, "--disable") {
        None => 0,
        Some(names) => match parse_levers(&names) {
            Ok(mask) => mask,
            Err(reason) => {
                eprintln!("{reason}");
                return ExitCode::FAILURE;
            }
        },
    };
    match run(&out, &scales, rounds, &label, disabled) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the lever mask named by a comma-separated list.
///
/// The names are the ones the report prints, so a command line and a report row
/// say the same thing. An unknown name is refused rather than ignored: a typo
/// that silently measured the shipped engine and labelled it as an arm would be
/// worse than no arm at all.
/// @param names - a comma-separated list, or `all`
fn parse_levers(names: &str) -> Result<u32, String> {
    let mut mask = 0;
    for name in names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        mask |= match name {
            "all" => Levers::EVERY,
            "covering-index" => Levers::COVERING_INDEX,
            "indexed-write" => Levers::INDEXED_WRITE,
            "ordered-walk" => Levers::ORDERED_WALK,
            "streaming-group" => Levers::STREAMING_GROUP,
            "fused-bytecode" => Levers::FUSED_BYTECODE,
            other => {
                return Err(format!(
                    "unknown lever `{other}`; the levers are covering-index, indexed-write, \n                     ordered-walk, streaming-group, fused-bytecode, all"
                ))
            }
        };
    }
    Ok(mask)
}

/// Returns the value of a `--flag value` argument.
fn flag(arguments: &[String], name: &str) -> Option<String> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).cloned()
}

/// Leaks one scale name, which lives for the whole run.
fn leak(name: &str) -> &'static str {
    Box::leak(name.to_string().into_boxed_str())
}

/// Returns the pinned benchmark driver, if it has been built.
fn sqlite_bench() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_BENCH") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-bench{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Runs every scale and writes the report.
fn run(
    out: &Path,
    scales: &[&str],
    rounds: u32,
    label: &str,
    disabled: u32,
) -> Result<String, String> {
    let contract = Contract::parse(
        &std::fs::read_to_string(workspace_root().join("compat/perf/contract.toml"))
            .map_err(|error| format!("cannot read the performance contract: {error}"))?,
    )?;
    let Some(bench) = sqlite_bench() else {
        return Err(
            "the pinned benchmark driver is not built; run tools/sqlite-reference.{ps1,sh}"
                .to_string(),
        );
    };
    std::fs::create_dir_all(out).map_err(|error| format!("cannot create {out:?}: {error}"))?;

    let mut sections = Vec::new();
    for scale in scales {
        let plan = plan_for(scale);
        let measured = measure(&plan, &bench, out, rounds, disabled)?;
        sections.push((plan, measured));
    }

    // The arm travels with the label into the history, so two runs of the same
    // ticket under different arms are two series rather than one series that
    // silently changed meaning halfway through.
    let names = Levers::without(disabled).names_disabled();
    let label = &if names.is_empty() {
        label.to_string()
    } else {
        format!("{label} (no {})", names.join(", "))
    };
    let markdown = render_markdown(&sections, &contract, label, rounds, disabled);
    let json = render_json(&sections, &contract, label, rounds);
    std::fs::write(out.join("scorecard.md"), &markdown)
        .map_err(|error| format!("cannot write the scorecard: {error}"))?;
    std::fs::write(out.join("scorecard.json"), &json)
        .map_err(|error| format!("cannot write the scorecard: {error}"))?;
    append_history(out, &sections, &contract, label, disabled)?;

    let mut summary = String::new();
    for (plan, measured) in &sections {
        let (centre, low, high) = headline(measured, &contract);
        summary.push_str(&format!(
            "{}: geomean {centre:.3}x [{low:.3}, {high:.3}]\n",
            plan.scale
        ));
    }
    summary.push_str(&format!("written to {}", out.display()));
    Ok(summary)
}

/// Returns the weighted headline for one scale.
fn headline(measured: &[Paired], contract: &Contract) -> (f64, f64, f64) {
    let rounds = inillucent_compat::perf::qualified_rounds(measured);
    inillucent_compat::perf::weighted_headline(&rounds, contract, SEED)
}

/// Runs one plan over both engines for the requested number of rounds.
fn measure(
    plan: &Plan,
    bench: &Path,
    out: &Path,
    rounds: u32,
    disabled: u32,
) -> Result<Vec<Paired>, String> {
    let area = out.join(&plan.scale);
    std::fs::create_dir_all(&area).map_err(|error| format!("cannot create {area:?}: {error}"))?;
    let plan_path = area.join("plan.txt");
    std::fs::write(&plan_path, plan.render())
        .map_err(|error| format!("cannot write the plan: {error}"))?;

    // The pristine images, built once and cloned per round so that every round
    // starts from the same state and the build is never inside a timed window.
    let pristine_ours = area.join("pristine-inillucent.db");
    let pristine_theirs = area.join("pristine-sqlite.db");
    remove(&pristine_ours);
    remove(&pristine_theirs);
    build_inillucent(plan, &pristine_ours, disabled)?;
    let built = Command::new(bench)
        .arg("build")
        .arg(&plan_path)
        .arg(&pristine_theirs)
        .output()
        .map_err(|error| format!("cannot run {bench:?}: {error}"))?;
    if !built.status.success() {
        return Err(format!(
            "the reference could not build its database: {}",
            String::from_utf8_lossy(&built.stderr)
        ));
    }

    let mut paired: Vec<Paired> = plan
        .workloads
        .iter()
        .map(|workload| Paired {
            workload: workload.name.clone(),
            family: workload.family.clone(),
            pairs: Vec::new(),
            agreed: true,
            disagreement: String::new(),
        })
        .collect();

    let working_ours = area.join("work-inillucent.db");
    let working_theirs = area.join("work-sqlite.db");
    for round in 0..rounds {
        clone(&pristine_ours, &working_ours)?;
        clone(&pristine_theirs, &working_theirs)?;
        // The order alternates, so neither engine is systematically the one
        // that ran while the file system cache was cold.
        let (ours, theirs) = if round % 2 == 0 {
            let ours = run_inillucent(plan, &working_ours, disabled)?;
            let theirs = run_sqlite(bench, &plan_path, &working_theirs)?;
            (ours, theirs)
        } else {
            let theirs = run_sqlite(bench, &plan_path, &working_theirs)?;
            let ours = run_inillucent(plan, &working_ours, disabled)?;
            (ours, theirs)
        };
        for entry in paired.iter_mut() {
            let mine = ours.iter().find(|sample| sample.workload == entry.workload);
            let yours = theirs
                .iter()
                .find(|sample| sample.workload == entry.workload);
            let (Some(mine), Some(yours)) = (mine, yours) else {
                entry.agreed = false;
                entry.disagreement = "one engine did not report this workload".to_string();
                continue;
            };
            if mine.digest != yours.digest || mine.rows != yours.rows {
                entry.agreed = false;
                entry.disagreement = format!(
                    "inillucent returned {} rows digest {:016x}, the reference {} rows digest {:016x}",
                    mine.rows, mine.digest, yours.rows, yours.digest
                );
                continue;
            }
            entry.pairs.push((mine.nanos, yours.nanos));
        }
    }
    remove(&working_ours);
    remove(&working_theirs);
    Ok(paired)
}

/// Removes a database and whatever it left beside it.
fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut side = path.as_os_str().to_os_string();
        side.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(side));
    }
}

/// Copies a pristine database into place for one round.
fn clone(from: &Path, to: &Path) -> Result<(), String> {
    remove(to);
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|error| format!("cannot clone {from:?}: {error}"))
}

/// Runs the reference arm and reads its samples back.
fn run_sqlite(bench: &Path, plan: &Path, database: &Path) -> Result<Vec<Sample>, String> {
    let output = Command::new(bench)
        .arg("run")
        .arg(plan)
        .arg(database)
        .output()
        .map_err(|error| format!("cannot run {bench:?}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "the reference arm failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(Sample::parse)
        .collect())
}

/// Opens a database, applies the plan's settings, and hands the one
/// connection it makes to a callback - then checkpoints and closes.
///
/// **Not leaked, and not returned.** `Connection<'d>` borrows the `Database`
/// it came from, so a helper that tried to hand one back across a function
/// boundary would need the database to outlive the call; the old code before
/// this rearchitecture never had that problem because its `Connection` did
/// not borrow anything. Rather than leak the database for the rest of the
/// process (which was tried and produced "database disk image is malformed" -
/// the pristine build's file handle was still open when the round loop copied
/// it as a template), the database and its connection now live and die inside
/// this one function: the callback does the real work, the checkpoint runs
/// while the connection is still valid, and the database closes when this
/// function returns.
///
/// @param body - runs with the open, configured connection
fn with_inillucent<T>(
    plan: &Plan,
    path: &Path,
    disabled: u32,
    body: impl FnOnce(&Connection<'_>) -> Result<T, String>,
) -> Result<T, String> {
    let database = Database::open(path)
        .map_err(|error| format!("cannot open {path:?}: {}", error.message()))?;
    let connection = database.session();
    let _ = connection.disable_optimizations(Levers::without(disabled));
    for pragma in [
        format!("PRAGMA page_size={};", plan.page_size),
        format!("PRAGMA journal_mode={};", plan.journal),
        format!("PRAGMA synchronous={};", plan.synchronous),
        format!("PRAGMA cache_size={};", plan.cache_size),
    ] {
        connection
            .execute_batch(&pragma)
            .map_err(|error| format!("{pragma}: {}", error.message()))?;
    }
    let result = body(&connection)?;
    // Checkpointed on every call, not only the pristine build: it is cheap
    // next to a workload round, it runs after every timed sample is already
    // collected so it cannot skew a measurement, and a working copy that gets
    // inspected after a failure is as entitled to a consistent file as the
    // template it was cloned from.
    database
        .checkpoint()
        .map_err(|error| format!("checkpoint: {}", error.message()))?;
    Ok(result)
}

/// Builds the inillucent pristine image from the plan's setup statements.
fn build_inillucent(plan: &Plan, path: &Path, disabled: u32) -> Result<(), String> {
    with_inillucent(plan, path, disabled, |connection| {
        for statement in &plan.setup {
            connection
                .execute_batch(statement)
                .map_err(|error| format!("{statement}: {}", error.message()))?;
        }
        Ok(())
    })
}

/// Runs every workload of the plan on inillucent, returning one sample each.
fn run_inillucent(plan: &Plan, path: &Path, disabled: u32) -> Result<Vec<Sample>, String> {
    with_inillucent(plan, path, disabled, |connection| {
        let mut samples = Vec::with_capacity(plan.workloads.len());
        for workload in &plan.workloads {
            samples.push(run_one(connection, workload, plan.rows)?);
        }
        Ok(samples)
    })
}

/// Runs one workload, timing exactly what the reference times.
fn run_one(connection: &Connection<'_>, workload: &Workload, rows: u32) -> Result<Sample, String> {
    if let Some(pre) = &workload.pre {
        connection
            .execute_batch(pre)
            .map_err(|error| format!("{pre}: {}", error.message()))?;
    }
    let mut digest = Digest::new();
    let mut produced = 0u64;
    let mut prepared = if workload.prepare_each {
        None
    } else {
        Some(
            connection
                .prepare(&workload.sql)
                .map_err(|error| format!("{}: {}", workload.sql, error.message()))?,
        )
    };
    let started = Instant::now();
    if workload.grouping != Grouping::Autocommit {
        begin(connection)?;
    }
    for iteration in 0..workload.repeat {
        let mut fresh;
        let statement = match prepared.as_mut() {
            Some(held) => held,
            None => {
                fresh = connection
                    .prepare(&workload.sql)
                    .map_err(|error| format!("{}: {}", workload.sql, error.message()))?;
                &mut fresh
            }
        };
        for (position, bind) in workload.binds.iter().enumerate() {
            bind_one(statement, position as u32 + 1, *bind, iteration, rows)?;
        }
        while statement
            .step()
            .map_err(|error| format!("{}: {}", workload.sql, error.message()))?
        {
            for value in statement.row() {
                eat(&mut digest, value);
            }
            produced = produced.saturating_add(1);
        }
        if !workload.prepare_each {
            statement.reset();
            statement.clear_bindings();
        }
        if let Grouping::Every(size) = workload.grouping {
            if size > 0 && (iteration.saturating_add(1)) % size == 0 {
                commit(connection)?;
                if iteration.saturating_add(1) < workload.repeat {
                    begin(connection)?;
                }
            }
        }
    }
    match workload.grouping {
        Grouping::Single => commit(connection)?,
        Grouping::Every(size) if size > 0 && !workload.repeat.is_multiple_of(size) => {
            commit(connection)?
        }
        _ => {}
    }
    let elapsed = started.elapsed();
    drop(prepared);
    if let Some(post) = &workload.post {
        connection
            .execute_batch(post)
            .map_err(|error| format!("{post}: {}", error.message()))?;
    }
    Ok(Sample {
        workload: workload.name.clone(),
        nanos: elapsed.as_secs_f64() * 1e9,
        rows: produced,
        digest: digest.finish(),
    })
}

/// Opens a transaction.
fn begin(connection: &Connection<'_>) -> Result<(), String> {
    connection
        .execute_batch("BEGIN")
        .map_err(|error| format!("BEGIN: {}", error.message()))
}

/// Closes a transaction.
fn commit(connection: &Connection<'_>) -> Result<(), String> {
    connection
        .execute_batch("COMMIT")
        .map_err(|error| format!("COMMIT: {}", error.message()))
}

/// Adds one produced value to the digest, tagged the way the reference tags it.
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

/// Binds one parameter, by the same formula the reference uses.
fn bind_one(
    statement: &mut Statement<'_>,
    position: u32,
    bind: Bind,
    iteration: u32,
    rows: u32,
) -> Result<(), String> {
    let iteration = i64::from(iteration);
    let span = i64::from(rows).max(1);
    let outcome = match bind {
        Bind::Rowid => statement.bind_integer(position, 1 + iteration % span),
        Bind::Scatter => {
            let scattered = (iteration as u64).wrapping_mul(2_654_435_761) % span as u64;
            statement.bind_integer(position, 1 + scattered as i64)
        }
        Bind::Counter => statement.bind_integer(position, span + 1 + iteration),
        Bind::Int => {
            let value = (iteration as u64)
                .wrapping_mul(1_103_515_245)
                .wrapping_add(12_345)
                & 0x7fff_ffff;
            statement.bind_integer(position, value as i64)
        }
        Bind::Text => statement.bind_text(
            position,
            &format!("row {iteration} lorem ipsum dolor sit amet consectetur"),
        ),
        Bind::Blob => {
            let bytes: Vec<u8> = (0..inillucent_compat::perf::BLOB_BYTES)
                .map(|offset| ((iteration as usize + offset) & 0xff) as u8)
                .collect();
            statement.bind_blob(position, &bytes)
        }
    };
    outcome.map_err(|error| error.message().to_string())
}

// ---------------------------------------------------------------------------
// The plans.
//
// They live in `inillucent_compat::perf` rather than here, because the Phase 2
// read-family gate measures the same workloads and a second copy of the SQL is
// the exact class of instrument bug this project has been bitten by four times.
// One table, two readers.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Reporting.
// ---------------------------------------------------------------------------

/// Returns one family's aggregate ratio, its interval, and how many pairs it
/// came from.
///
/// The contract's floor is written per *family*, so the number judged against
/// it has to be the family's, not the worst workload inside it - a family of
/// five workloads would otherwise fail on its slowest member no matter what the
/// other four did. This is the one place that aggregate is computed, so the
/// report and the history cannot disagree about it.
///
/// The geometric mean rather than the median, so the point estimate is the same
/// statistic the interval brackets. A median beside a bootstrapped mean can sit
/// outside its own interval, which reads like an arithmetic error and is one.
/// @param measured - every workload of one scale
/// @param family - the family to aggregate
fn family_interval(measured: &[Paired], family: &str) -> Option<(f64, f64, f64, usize)> {
    let members: Vec<&Paired> = measured
        .iter()
        .filter(|paired| paired.family == family)
        .collect();
    if members.is_empty() {
        return None;
    }
    let logs: Vec<f64> = members
        .iter()
        .filter(|paired| paired.agreed)
        .flat_map(|paired| paired.log_ratios())
        .collect();
    let mean = if logs.is_empty() {
        0.0
    } else {
        logs.iter().sum::<f64>() / logs.len() as f64
    };
    let (low, high) = inillucent_compat::perf::bootstrap(&logs, SEED);
    Some((mean.exp(), low.exp(), high.exp(), logs.len()))
}

/// Renders the scorecard a person reads.
fn render_markdown(
    sections: &[(Plan, Vec<Paired>)],
    contract: &Contract,
    label: &str,
    rounds: u32,
    disabled: u32,
) -> String {
    let mut out = String::new();
    out.push_str("# inillucent performance scorecard\n\n");
    // The arm is named in the first paragraph rather than in a footnote,
    // because a scorecard read out of context and mistaken for the shipped
    // engine is worse than no scorecard.
    let names = Levers::without(disabled).names_disabled();
    let arm = if names.is_empty() {
        "Every optimization is on, which is the shipped engine.".to_string()
    } else {
        format!(
            "**Arm: `{}` switched off.** This is one side of an A/B pair and not the shipped \
             engine; compare it with the run whose arm is empty.",
            names.join("`, `")
        )
    };
    out.push_str(&format!(
        "Label `{label}`, platform `{}`, {rounds} paired rounds per scale, bootstrap seed \
         {SEED}. {arm}\n\n",
        platform_name()
    ));
    out.push_str(
        "Both engines read the same plan file. The ratio is SQLite over inillucent, so **above one \
         means inillucent is faster**. A workload whose two engines returned different answers is \
         reported as a correctness failure and is not timed.\n\n",
    );
    out.push_str("## Fair configuration\n\n");
    if let Some((plan, _)) = sections.first() {
        out.push_str(&format!(
            "| setting | value |\n|---|---|\n| journal mode | `{}` |\n| synchronous | `{}` |\n| \
             page size | {} |\n| cache | {} in SQLite's units: positive is pages, negative is KiB |\n| statement reuse | \
             prepared once except the `open.prepare` family |\n| database | on disk, cloned from \
             one pristine image per round |\n\n",
            plan.journal, plan.synchronous, plan.page_size, plan.cache_size
        ));
    }
    out.push_str(
        "**On memory, which is the setting most easily got wrong.** This scorecard measures the \
         bytecode engine, whose page cache and SQLite's are both governed by the `cache_size` \
         above, so the two arms are given the same memory by construction.\n\n",
    );
    out.push_str(
        "That is *not* automatic for the vectorised engine, and the Phase 1 numbers were \
         inflated because it was not: the prototype's trees were fully resident while SQLite ran \
         at the plan's 2 MB cache. Phase 2's gate (`inillucent-readgate`) closes it by deriving \
         SQLite's `cache_size` from the byte size of inillucent's own buffer pool, so `--frames` \
         moves both sides together and neither engine can be given memory the other is not. The \
         gate prints both figures before it times anything. A ratio measured without that is a \
         ratio between two different machines.\n\n",
    );
    for (plan, measured) in sections {
        let (centre, low, high) = headline(measured, contract);
        out.push_str(&format!(
            "## Scale `{}` - {} rows\n\n",
            plan.scale, plan.rows
        ));
        out.push_str(&format!(
            "Weighted geometric mean **{centre:.3}x**, 95% interval [{low:.3}, {high:.3}]. \
             The release bound is a lower bound of at least {:.2}x.\n\n",
            contract.headline
        ));
        out.push_str("### By family\n\n");
        out.push_str(
            "| family | weight | ratio | 95% interval | verdict | required floor |\n\
             |---|---:|---:|---|---|---|\n",
        );
        for family in &contract.families {
            let Some((ratio, low, high, _)) = family_interval(measured, &family.id) else {
                continue;
            };
            let verdict = Verdict::of(low, high);
            let floor = if family.required {
                if low >= contract.floor {
                    "met".to_string()
                } else {
                    format!("**below {:.2}x**", contract.floor)
                }
            } else {
                "-".to_string()
            };
            out.push_str(&format!(
                "| `{}` | {:.2} | {ratio:.3}x | [{low:.3}, {high:.3}] | {} | {floor} |\n",
                family.id,
                family.weight,
                verdict.name()
            ));
        }
        out.push_str("\n### By workload\n\n");
        out.push_str(
            "| workload | family | inillucent median | SQLite median | ratio | 95% interval | \
             samples |\n|---|---|---:|---:|---:|---|---:|\n",
        );
        for paired in measured {
            if !paired.agreed {
                out.push_str(&format!(
                    "| `{}` | `{}` | - | - | **answers differ** | {} | 0 |\n",
                    paired.workload, paired.family, paired.disagreement
                ));
                continue;
            }
            let (ours, theirs) = paired.medians();
            let (low, high) = paired.interval(SEED);
            out.push_str(&format!(
                "| `{}` | `{}` | {} | {} | {:.3}x | [{low:.3}, {high:.3}] | {} |\n",
                paired.workload,
                paired.family,
                duration(ours),
                duration(theirs),
                paired.ratio(),
                paired.pairs.len()
            ));
        }
        out.push('\n');
    }
    out
}

/// Renders a nanosecond count in units a reader can compare.
fn duration(nanos: f64) -> String {
    if nanos >= 1.0e9 {
        return format!("{:.2} s", nanos / 1.0e9);
    }
    if nanos >= 1.0e6 {
        return format!("{:.2} ms", nanos / 1.0e6);
    }
    if nanos >= 1.0e3 {
        return format!("{:.2} us", nanos / 1.0e3);
    }
    format!("{nanos:.0} ns")
}

/// Renders the scorecard as the machine-readable record.
fn render_json(
    sections: &[(Plan, Vec<Paired>)],
    contract: &Contract,
    label: &str,
    rounds: u32,
) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"label\": {},\n", json_string(label)));
    out.push_str(&format!(
        "  \"platform\": {},\n",
        json_string(&platform_name())
    ));
    out.push_str(&format!("  \"rounds\": {rounds},\n"));
    out.push_str(&format!("  \"seed\": {SEED},\n"));
    out.push_str("  \"scales\": [\n");
    for (index, (plan, measured)) in sections.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        let (centre, low, high) = headline(measured, contract);
        out.push_str("    {\n");
        out.push_str(&format!("      \"scale\": {},\n", json_string(&plan.scale)));
        out.push_str(&format!("      \"rows\": {},\n", plan.rows));
        out.push_str(&format!(
            "      \"headline\": {{\"ratio\": {centre:.6}, \"low\": {low:.6}, \"high\": {high:.6}}},\n"
        ));
        out.push_str("      \"workloads\": [\n");
        for (position, paired) in measured.iter().enumerate() {
            if position > 0 {
                out.push_str(",\n");
            }
            let (ours, theirs) = paired.medians();
            let (low, high) = paired.interval(SEED);
            out.push_str(&format!(
                "        {{\"workload\": {}, \"family\": {}, \"agreed\": {}, \"inillucent_nanos\": \
                 {ours:.1}, \"sqlite_nanos\": {theirs:.1}, \"ratio\": {:.6}, \"low\": {low:.6}, \
                 \"high\": {high:.6}, \"samples\": {}, \"detail\": {}}}",
                json_string(&paired.workload),
                json_string(&paired.family),
                paired.agreed,
                paired.ratio(),
                paired.pairs.len(),
                json_string(&paired.disagreement)
            ));
        }
        out.push_str("\n      ]\n    }");
    }
    out.push_str("\n  ]\n}\n");
    out
}

/// The line separator the history file uses.
const NEWLINE: char = '\n';

/// Appends this run to the versioned performance history and redraws the
/// dashboard.
///
/// One line per workload per run, plus one for the weighted headline, each
/// carrying the label, the platform and the scale it belongs to. A regression
/// is then a comparison against the file rather than against somebody's memory
/// of the last number.
fn append_history(
    out: &Path,
    sections: &[(Plan, Vec<Paired>)],
    contract: &Contract,
    label: &str,
    disabled: u32,
) -> Result<(), String> {
    let path = out.join("history.jsonl");
    let platform = platform_name();
    let arm = Levers::without(disabled).names_disabled().join(",");
    let mut lines = Vec::new();
    for (plan, measured) in sections {
        let (centre, low, high) = headline(measured, contract);
        lines.push(
            history::Entry {
                label: label.to_string(),
                platform: platform.clone(),
                scale: plan.scale.clone(),
                workload: "*headline*".to_string(),
                family: "*weighted*".to_string(),
                ratio: centre,
                low,
                high,
                samples: measured.first().map(|entry| entry.pairs.len()).unwrap_or(0),
                arm: arm.clone(),
            }
            .render(),
        );
        for family in &contract.families {
            let Some((ratio, low, high, samples)) = family_interval(measured, &family.id) else {
                continue;
            };
            lines.push(
                history::Entry {
                    label: label.to_string(),
                    platform: platform.clone(),
                    scale: plan.scale.clone(),
                    workload: format!("*family* {}", family.id),
                    family: family.id.clone(),
                    ratio,
                    low,
                    high,
                    samples,
                    arm: arm.clone(),
                }
                .render(),
            );
        }
        for paired in measured {
            let (low, high) = paired.interval(SEED);
            lines.push(
                history::Entry {
                    label: label.to_string(),
                    platform: platform.clone(),
                    scale: plan.scale.clone(),
                    workload: paired.workload.clone(),
                    family: paired.family.clone(),
                    ratio: paired.ratio(),
                    low,
                    high,
                    samples: paired.pairs.len(),
                    arm: arm.clone(),
                }
                .render(),
            );
        }
    }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    for line in lines {
        text.push_str(&line);
        text.push(NEWLINE);
    }
    std::fs::write(&path, text)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;

    let recorded = history::History::load(&path);
    std::fs::write(
        out.join("dashboard.md"),
        history::dashboard(&recorded, &platform),
    )
    .map_err(|error| format!("cannot write the dashboard: {error}"))?;
    Ok(())
}
