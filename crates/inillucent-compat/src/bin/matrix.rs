//! `inillucent-matrix`: converts the existing corpora into matrix case files,
//! and runs matrix cases outside the test harness to measure them.
//!
//! Invariant: **this program writes case files and reports numbers; it never
//! decides whether the engine is right.** The grading is the test suites'.
//! A measurement here uses the same `Runner` the suites use, so the numbers
//! in section 8.1 of the design are numbers about the code that runs.
//!
//! ```text
//! inillucent-matrix convert-part8                  the differential part 8 corpus
//! inillucent-matrix convert-probe <cases.json>     the feature probe's cases
//! inillucent-matrix convert-syntax                 compat/syntax.toml
//! inillucent-matrix run <family> [--limit N] [--arm NAME] [--threads N] [--cadence C]
//! inillucent-matrix inventory                      writes _agent_output/matrix/inventory.md
//! inillucent-matrix counts                         prints the counts.toml numbers
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::oracle::{Driver, Op};
use inillucent_compat::statement_matrix::case::Case;
use inillucent_compat::statement_matrix::convert::{self, Source};
use inillucent_compat::statement_matrix::grade::Verdict;
use inillucent_compat::statement_matrix::group::{self, Cadence};
use inillucent_compat::statement_matrix::known::{corpus_root, judge, Judged};
use inillucent_compat::statement_matrix::run::{placement_key, Runner, Stats};
use inillucent_compat::workspace_root;

/// Reads the command line and runs one command.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let result = match arguments.first().map(String::as_str) {
        Some("convert-part8") => convert_part8(),
        Some("convert-probe") => convert_probe(arguments.get(1).map(PathBuf::from)),
        Some("convert-syntax") => convert_syntax(),
        Some("run") => run(&arguments[1..]),
        Some("inventory") => inventory(),
        Some("counts") => counts(),
        Some("probe-sessions") => probe_sessions(),
        Some("shrink") => shrink(&arguments[1..]),
        Some("show") => show(arguments.get(1).map(String::as_str).unwrap_or("")),
        _ => Err(
            "usage: inillucent-matrix convert-part8 | convert-probe <json> | convert-syntax | \
                  run <family> [--limit N] [--arm NAME] [--threads N] [--cadence C] | inventory \
                  | counts"
                .to_string(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(problem) => {
            eprintln!("inillucent-matrix: {problem}");
            ExitCode::FAILURE
        }
    }
}

/// Starts the pinned oracle, or says it is missing.
fn oracle() -> Result<Driver, String> {
    let program = inillucent_compat::differential::sqlite_oracle()
        .ok_or("the pinned SQLite oracle is not built; run tools/sqlite-reference.ps1")?;
    let mut driver = Driver::start("sqlite", &program)?;
    let hello = driver.send(&Op::Hello)?;
    if !hello.ok {
        return Err("the oracle did not answer hello".to_string());
    }
    Ok(driver)
}

/// A scratch directory for conversion.
fn scratch() -> Result<PathBuf, String> {
    let directory = workspace_root().join("_agent_output/matrix/convert");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot make {}: {error}", directory.display()))?;
    Ok(directory)
}

/// Converts every case of the part 8 corpus into matrix files.
fn convert_part8() -> Result<(), String> {
    let source = workspace_root().join("crates/inillucent-compat/tests/corpora/differential-part8");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&source)
        .map_err(|error| format!("cannot read {}: {error}", source.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "cases"))
        .collect();
    files.sort();
    let mut driver = oracle()?;
    let directory = scratch()?;
    let mut by_file: BTreeMap<(String, String), Vec<Case>> = BTreeMap::new();
    let mut total = 0usize;
    for file in files {
        let category = file
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = std::fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let (id, sql) = line
                .split_once('\t')
                .ok_or_else(|| format!("{}: a line has no tab", file.display()))?;
            let script = sql.replace("\\n", "\n").replace("\\\\", "\\");
            let family = convert::family_of_category(&category)
                .unwrap_or_else(|| convert::family_by_content(&script));
            let mut case = convert::convert(
                &mut driver,
                &directory,
                &Source {
                    id: id.to_string(),
                    family: family.to_string(),
                    capabilities: Vec::new(),
                    script,
                },
            )?;
            convert::hoist(&mut case);
            total += 1;
            by_file
                .entry((family.to_string(), category.clone()))
                .or_default()
                .push(case);
        }
    }
    for ((family, category), cases) in &by_file {
        let path = corpus_root()
            .join(family)
            .join(format!("part8-{category}.slt"));
        let header = [
            &format!(
                "Moved from crates/inillucent-compat/tests/corpora/differential-part8/{category}.cases,"
            )[..],
            "where each case was one line run through both shells with -bail. Each case now runs",
            "on files through the persistent oracle, and stops where the script stopped.",
        ];
        write(&path, &convert::render_file(&header, cases))?;
    }
    println!(
        "converted {total} part 8 cases into {} files",
        by_file.len()
    );
    Ok(())
}

/// Converts the feature probe's cases, exported to JSON, into matrix files.
///
/// @param path - the export: a list of `{id, area, family, sql}` objects
fn convert_probe(path: Option<PathBuf>) -> Result<(), String> {
    let path = path.ok_or("convert-probe needs the path of the exported cases")?;
    let sources = convert::read_probe_export(&path)?;
    let mut driver = oracle()?;
    let directory = scratch()?;
    let mut by_family: BTreeMap<String, Vec<Case>> = BTreeMap::new();
    for source in &sources {
        let mut case = convert::convert(&mut driver, &directory, source)?;
        convert::hoist(&mut case);
        by_family
            .entry(source.family.clone())
            .or_default()
            .push(case);
    }
    for (family, cases) in &by_family {
        let path = corpus_root().join(family).join("feature-probe.slt");
        let header = [
            "Copied from tools/feature-probe/cases.js and cases-extra.js, which stay as the",
            "published comparison. Dot commands and shell settings are left out: the matrix",
            "compares typed values through the engine, not what the shell prints.",
        ];
        write(&path, &convert::render_file(&header, cases))?;
    }
    println!("converted {} feature probe cases", sources.len());
    Ok(())
}

/// Converts `compat/syntax.toml` into cases that run every example.
fn convert_syntax() -> Result<(), String> {
    let register = inillucent_compat::syntax::SyntaxRegister::load(
        &workspace_root().join("compat/syntax.toml"),
    )?;
    let mut driver = oracle()?;
    let directory = scratch()?;
    let files = convert::syntax_cases(&mut driver, &directory, &register)?;
    let mut total = 0usize;
    for (family, text, count) in files {
        total += count;
        write(&corpus_root().join(&family).join("syntax.slt"), &text)?;
    }
    println!("converted {total} syntax register examples");
    Ok(())
}

/// Runs a family's cases in this process and prints what they cost.
///
/// @param arguments - the family, then options
fn run(arguments: &[String]) -> Result<(), String> {
    let family = arguments.first().ok_or("run needs a family")?.clone();
    let option = |name: &str| -> Option<String> {
        arguments
            .iter()
            .position(|argument| argument == name)
            .and_then(|at| arguments.get(at + 1).cloned())
    };
    let limit: usize = option("--limit")
        .map(|text| text.parse().map_err(|_| "--limit takes a number"))
        .transpose()?
        .unwrap_or(usize::MAX);
    let threads: usize = option("--threads")
        .map(|text| text.parse().map_err(|_| "--threads takes a number"))
        .transpose()?
        .unwrap_or(1);
    let arm_name = option("--arm").unwrap_or_else(|| "default".to_string());
    let arm = group::every_arm()
        .into_iter()
        .find(|arm| arm.name == arm_name)
        .ok_or_else(|| format!("no arm named {arm_name}"))?;
    let cadence = match option("--cadence").as_deref() {
        None | Some("change") => Cadence::Change,
        Some("merge") => Cadence::Merge,
        Some("nightly") => Cadence::Nightly,
        Some(other) => return Err(format!("no cadence named {other}")),
    };
    if family.ends_with(".slt") {
        let text = std::fs::read_to_string(&family)
            .map_err(|error| format!("cannot read {family}: {error}"))?;
        let mut cases =
            inillucent_compat::statement_matrix::case::parse(&text, "file", &family)?.cases;
        let repeat: usize = option("--repeat")
            .map(|text| text.parse().map_err(|_| "--repeat takes a number"))
            .transpose()?
            .unwrap_or(1);
        let single = cases.clone();
        for round in 1..repeat {
            cases.extend(single.iter().cloned().map(|mut case| {
                case.id = format!("{}-{round}", case.id);
                case
            }));
        }
        cases.truncate(limit);
        return measure(&cases, &arm, threads);
    }
    let families: Vec<String> = if family == "all" {
        inillucent_compat::statement_matrix::inventory::FAMILIES
            .iter()
            .map(|name| name.to_string())
            .collect()
    } else {
        vec![family]
    };
    let mut cases: Vec<Case> = Vec::new();
    for family in &families {
        for (case, _) in group::work(family, cadence)?.runs {
            cases.push(case);
        }
    }
    cases.truncate(limit);
    measure(&cases, &arm, threads)
}

/// Runs cases on `threads` runners and prints the totals.
fn measure(
    cases: &[Case],
    arm: &inillucent_compat::matrix::Arm,
    threads: usize,
) -> Result<(), String> {
    // `INILLUCENT_MATRIX_ROOT` puts the scratch files where the test suites
    // put theirs, under the target directory, which may be another disk.
    let root = std::env::var("INILLUCENT_MATRIX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root().join("_agent_output/matrix/run"));
    let started = Instant::now();
    let cpu_before = process_cpu_seconds();
    let chunks: Vec<Vec<Case>> = (0..threads.max(1))
        .map(|thread| {
            cases
                .iter()
                .filter(|case| {
                    group::id_hash(&placement_key(case)) % threads.max(1) as u64 == thread as u64
                })
                .cloned()
                .collect()
        })
        .collect();
    let results: Vec<(Stats, Vec<String>, usize)> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .enumerate()
            .map(|(thread, chunk)| {
                let root = root.join(format!("t{thread}"));
                let arm = *arm;
                scope.spawn(move || {
                    let mut runner = Runner::new(arm, &root);
                    let mut failing = Vec::new();
                    let mut skipped = 0usize;
                    let borrowed: Vec<&Case> = chunk.iter().collect();
                    let (known_list, deliberate) = lists();
                    for (case, verdict) in borrowed.iter().zip(runner.run_all(&borrowed)) {
                        let (failures, ran) = match verdict {
                            Verdict::Passed => (Vec::new(), true),
                            Verdict::Failed(failures) => (failures, true),
                            Verdict::Skipped(_) => {
                                skipped += 1;
                                (Vec::new(), false)
                            }
                        };
                        match judge(&case.id, failures, ran, &known_list, &deliberate) {
                            Judged::Pass | Judged::Expected => {}
                            Judged::Fail(failures) => failing
                                .extend(failures.iter().take(3).map(|failure| failure.render())),
                            Judged::Stale(listed) => failing.push(format!(
                                "{} is listed as bug {} and now agrees",
                                case.id, listed.bug
                            )),
                        }
                    }
                    runner.finish();
                    (runner.stats.clone(), failing, skipped)
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });
    let wall = started.elapsed();
    let cpu = process_cpu_seconds() - cpu_before;
    let mut total = Stats::default();
    let mut failing = Vec::new();
    let mut skipped = 0usize;
    for (stats, failures, skips) in results {
        total.add(&stats);
        failing.extend(failures);
        skipped += skips;
    }
    for failure in &failing {
        println!("FAIL {failure}");
    }
    let per_case = |seconds: f64| seconds * 1000.0 / total.cases.max(1) as f64;
    println!(
        "{} case(s), {} skipped, {} failing, {} statement(s) per engine, {} thread(s), arm {}",
        total.cases,
        skipped,
        failing.len(),
        total.statements,
        threads,
        arm.name
    );
    let oracle_cpu = total.oracle_cpu.as_secs_f64();
    println!(
        "wall {:.2}s; processor time: inillucent side {:.2}s = {:.2} ms per case, oracle \
         processes {:.2}s = {:.2} ms per case, together {:.2} ms per case",
        wall.as_secs_f64(),
        cpu,
        per_case(cpu),
        oracle_cpu,
        per_case(oracle_cpu),
        per_case(cpu + oracle_cpu)
    );
    println!(
        "summed case time {:.2}s = {:.2} ms per case; {} case(s) shared a fixture copy; \
         fixtures: {} built in {:.2}s, {} copies in {:.3}s = {:.3} ms per copy",
        total.case_time.as_secs_f64(),
        per_case(total.case_time.as_secs_f64()),
        total.batched,
        total.fixtures_built,
        total.fixture_time.as_secs_f64(),
        total.fixture_copies,
        total.copy_time.as_secs_f64(),
        total.copy_time.as_secs_f64() * 1000.0 / total.fixture_copies.max(1) as f64
    );
    for (time, id) in &total.slowest {
        println!("  slow: {:8.1} ms  {id}", time.as_secs_f64() * 1000.0);
    }
    Ok(())
}

/// Reads `known.list` and `deliberate.toml`, or empty lists when they do not
/// parse; the test suites report a list that does not parse, this does not.
fn lists() -> (
    std::collections::BTreeMap<String, inillucent_compat::statement_matrix::known::Known>,
    Vec<inillucent_compat::statement_matrix::known::Deliberate>,
) {
    use inillucent_compat::statement_matrix::known;
    let root = corpus_root();
    (
        known::read_known(&root.join("known.list")).unwrap_or_default(),
        known::read_deliberate(&root.join("deliberate.toml")).unwrap_or_else(|problem| {
            eprintln!("deliberate.toml: {problem}");
            Vec::new()
        }),
    )
}

/// The processor time this process has used, in seconds.
fn process_cpu_seconds() -> f64 {
    inillucent_compat::procstat::ProcessCost::now().cpu_nanos() as f64 / 1e9
}

/// Writes the coverage report of section 9.1.
fn inventory() -> Result<(), String> {
    let report = inillucent_compat::statement_matrix::inventory::report()?;
    let path = workspace_root().join("_agent_output/matrix/inventory.md");
    write(&path, &report.markdown)?;
    println!("{}", report.summary);
    Ok(())
}

/// Asks whether one session's `changes()` moves when another session of the
/// same database writes, which decides whether the property checks may share
/// a database with the case they check.
fn probe_sessions() -> Result<(), String> {
    let directory = workspace_root().join("_agent_output/matrix/probe-sessions");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let database = inillucent_engine::connect::Database::open(directory.join("p.rdb"))
        .map_err(|error| error.to_string())?;
    let first = database.session();
    let second = database.session();
    first
        .execute("CREATE TABLE t(a)")
        .map_err(|error| error.to_string())?;
    first
        .execute("INSERT INTO t VALUES (1), (2), (3)")
        .map_err(|error| error.to_string())?;
    println!("first after its insert: changes {:?}", first.changes());
    second
        .execute("INSERT INTO t VALUES (4)")
        .map_err(|error| error.to_string())?;
    println!(
        "first after the second session's insert: changes {:?}, total {:?}",
        first.changes(),
        first.total_changes()
    );
    println!("second: changes {:?}", second.changes());
    second
        .execute("SAVEPOINT p")
        .map_err(|error| error.to_string())?;
    second
        .execute("CREATE TEMP TABLE x AS SELECT a FROM t")
        .map_err(|error| error.to_string())?;
    second
        .execute("ROLLBACK TO p")
        .map_err(|error| error.to_string())?;
    second
        .execute("RELEASE p")
        .map_err(|error| error.to_string())?;
    println!(
        "first after the second session's CREATE TABLE AS: changes {:?}",
        first.changes()
    );
    Ok(())
}

/// Finds a case by id at any cadence.
///
/// @param id - the case id
fn find(id: &str) -> Result<Case, String> {
    for family in inillucent_compat::statement_matrix::inventory::FAMILIES {
        for cadence in [Cadence::Change, Cadence::Merge, Cadence::Nightly] {
            for (case, _) in group::work(family, cadence)?.runs {
                if case.id == id {
                    return Ok(case);
                }
            }
        }
    }
    Err(format!("no case has the id {id}"))
}

/// Prints one case, found by id at any cadence, in the file format.
///
/// @param id - the case id
fn show(id: &str) -> Result<(), String> {
    println!("{}", find(id)?.render());
    Ok(())
}

/// Shrinks a failing case and prints the smallest case that fails the same
/// way; with `--save`, writes it into the retained corpus.
///
/// @param arguments - the case id, then `--arm NAME` and `--save`
fn shrink(arguments: &[String]) -> Result<(), String> {
    let id = arguments.first().ok_or("shrink needs a case id")?;
    let arm_name = arguments
        .iter()
        .position(|argument| argument == "--arm")
        .and_then(|at| arguments.get(at + 1))
        .cloned()
        .unwrap_or_else(|| "default".to_string());
    let arm = group::every_arm()
        .into_iter()
        .find(|arm| arm.name == arm_name)
        .ok_or_else(|| format!("no arm named {arm_name}"))?;
    let case = find(id)?;
    let root = workspace_root().join("_agent_output/matrix/shrink");
    let mut runner = Runner::new(arm, &root);
    let Some((small, runs)) =
        inillucent_compat::statement_matrix::shrink::shrink(&mut runner, &case)
    else {
        return Err(format!("{id} does not fail at the {arm_name} arm"));
    };
    runner.finish();
    let mut small = small;
    let saving = arguments.iter().any(|argument| argument == "--save");
    if saving {
        // The retained copy is a case of its own, run at every arm on every
        // change, so it has an id of its own beside the case it came from.
        small.id = format!("retained-{}", small.id);
    }
    let mut text = small.render();
    text = text.replacen(
        &format!("# {} from {}", small.id, small.origin),
        &format!("# {} shrunk in {runs} runs from {}", small.id, small.origin),
        1,
    );
    println!("{text}");
    if saving {
        let path = corpus_root()
            .join("retained")
            .join(format!("{}.slt", small.id));
        write(&path, &text)?;
        println!("saved {}", path.display());
    }
    Ok(())
}

/// Prints the case counts `counts.toml` records.
fn counts() -> Result<(), String> {
    print!(
        "{}",
        inillucent_compat::statement_matrix::templates::counts_toml()?
    );
    Ok(())
}

/// Writes a file, making its directory.
fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot make {}: {error}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}
