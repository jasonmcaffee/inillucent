//! The release performance scorecard: both engines, one plan, paired samples.
//!
//! Invariant: nothing here decides what a workload is. The plan is generated
//! once, written to a file, and read by both arms - this program for rust-db,
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
//! cargo run --release -p rustdb-compat --bin rustdb-scorecard -- \
//!     [--scale small|medium|large|all] [--rounds N] [--out <dir>] [--label <text>]
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use rustdb::{Connection, Database, Value};
use rustdb_compat::perf::{
    Bind, Contract, Digest, Grouping, Paired, Plan, Sample, Verdict, Workload,
};
use rustdb_compat::report::json_string;
use rustdb_compat::{platform_name, workspace_root};

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
        .unwrap_or_else(|| workspace_root().join("_agent_output/task-1790/scorecard"));
    let rounds = flag(&arguments, "--rounds")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(DEFAULT_ROUNDS);
    let scales = match flag(&arguments, "--scale").as_deref() {
        Some("all") | None => vec!["small", "medium", "large"],
        Some(one) => vec![leak(one)],
    };
    let label = flag(&arguments, "--label").unwrap_or_else(|| "baseline".to_string());
    match run(&out, &scales, rounds, &label) {
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
    if let Ok(explicit) = std::env::var("RUSTDB_SQLITE_BENCH") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-bench{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Runs every scale and writes the report.
fn run(out: &Path, scales: &[&str], rounds: u32, label: &str) -> Result<String, String> {
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
        let measured = measure(&plan, &bench, out, rounds)?;
        sections.push((plan, measured));
    }

    let markdown = render_markdown(&sections, &contract, label, rounds);
    let json = render_json(&sections, &contract, label, rounds);
    std::fs::write(out.join("scorecard.md"), &markdown)
        .map_err(|error| format!("cannot write the scorecard: {error}"))?;
    std::fs::write(out.join("scorecard.json"), &json)
        .map_err(|error| format!("cannot write the scorecard: {error}"))?;
    append_history(out, &sections, &contract, label)?;

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
    let rounds = round_shaped(measured);
    rustdb_compat::perf::weighted_headline(&rounds, contract, SEED)
}

/// Returns the log ratios shaped one vector per round.
fn round_shaped(measured: &[Paired]) -> Vec<Vec<(String, f64)>> {
    // Over the workloads that agreed, because one that did not has no pairs at
    // all - and taking the minimum across everything made a single correctness
    // failure silently report a headline of exactly 1.000x, which is the most
    // misleading number the whole report could have produced.
    let depth = measured
        .iter()
        .filter(|paired| paired.agreed)
        .map(|paired| paired.pairs.len())
        .min()
        .unwrap_or(0);
    (0..depth)
        .map(|round| {
            measured
                .iter()
                .filter(|paired| paired.agreed)
                .filter_map(|paired| {
                    let (ours, theirs) = paired.pairs.get(round).copied()?;
                    if ours <= 0.0 || theirs <= 0.0 {
                        return None;
                    }
                    Some((paired.family.clone(), (theirs / ours).ln()))
                })
                .collect()
        })
        .collect()
}

/// Runs one plan over both engines for the requested number of rounds.
fn measure(plan: &Plan, bench: &Path, out: &Path, rounds: u32) -> Result<Vec<Paired>, String> {
    let area = out.join(&plan.scale);
    std::fs::create_dir_all(&area).map_err(|error| format!("cannot create {area:?}: {error}"))?;
    let plan_path = area.join("plan.txt");
    std::fs::write(&plan_path, plan.render())
        .map_err(|error| format!("cannot write the plan: {error}"))?;

    // The pristine images, built once and cloned per round so that every round
    // starts from the same state and the build is never inside a timed window.
    let pristine_ours = area.join("pristine-rustdb.db");
    let pristine_theirs = area.join("pristine-sqlite.db");
    remove(&pristine_ours);
    remove(&pristine_theirs);
    build_rustdb(plan, &pristine_ours)?;
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

    let working_ours = area.join("work-rustdb.db");
    let working_theirs = area.join("work-sqlite.db");
    for round in 0..rounds {
        clone(&pristine_ours, &working_ours)?;
        clone(&pristine_theirs, &working_theirs)?;
        // The order alternates, so neither engine is systematically the one
        // that ran while the file system cache was cold.
        let (ours, theirs) = if round % 2 == 0 {
            let ours = run_rustdb(plan, &working_ours)?;
            let theirs = run_sqlite(bench, &plan_path, &working_theirs)?;
            (ours, theirs)
        } else {
            let theirs = run_sqlite(bench, &plan_path, &working_theirs)?;
            let ours = run_rustdb(plan, &working_ours)?;
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
                    "rust-db returned {} rows digest {:016x}, the reference {} rows digest {:016x}",
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

/// Opens a database with the plan's settings applied.
fn open(plan: &Plan, path: &Path) -> Result<(Database, Connection), String> {
    let database = Database::open(path)
        .map_err(|error| format!("cannot open {path:?}: {}", error.message()))?;
    let connection = database
        .connect()
        .map_err(|error| format!("cannot connect: {}", error.message()))?;
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
    Ok((database, connection))
}

/// Builds the rust-db pristine image from the plan's setup statements.
fn build_rustdb(plan: &Plan, path: &Path) -> Result<(), String> {
    let (_database, connection) = open(plan, path)?;
    for statement in &plan.setup {
        connection
            .execute_batch(statement)
            .map_err(|error| format!("{statement}: {}", error.message()))?;
    }
    Ok(())
}

/// Runs every workload of the plan on rust-db, returning one sample each.
fn run_rustdb(plan: &Plan, path: &Path) -> Result<Vec<Sample>, String> {
    let (_database, connection) = open(plan, path)?;
    let mut samples = Vec::with_capacity(plan.workloads.len());
    for workload in &plan.workloads {
        samples.push(run_one(&connection, workload, plan.rows)?);
    }
    Ok(samples)
}

/// Runs one workload, timing exactly what the reference times.
fn run_one(connection: &Connection, workload: &Workload, rows: u32) -> Result<Sample, String> {
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
            statement
                .reset()
                .map_err(|error| format!("{}: {}", workload.sql, error.message()))?;
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
        Grouping::Every(size) if size > 0 && workload.repeat % size != 0 => commit(connection)?,
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
fn begin(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch("BEGIN")
        .map_err(|error| format!("BEGIN: {}", error.message()))
}

/// Closes a transaction.
fn commit(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch("COMMIT")
        .map_err(|error| format!("COMMIT: {}", error.message()))
}

/// Adds one produced value to the digest, tagged the way the reference tags it.
fn eat(digest: &mut Digest, value: &Value<'static>) {
    match value {
        Value::Null => digest.tag(0),
        Value::Integer(number) => {
            digest.tag(1);
            digest.word(*number as u64);
        }
        Value::Real(number) => {
            digest.tag(2);
            digest.word(number.to_bits());
        }
        Value::Text(text) => {
            let bytes = text.utf8_bytes();
            digest.tag(3);
            digest.word(bytes.len() as u64);
            digest.bytes(&bytes);
        }
        Value::Blob(blob) => {
            digest.tag(4);
            digest.word(blob.raw().len() as u64);
            digest.bytes(blob.raw());
        }
    }
}

/// Binds one parameter, by the same formula the reference uses.
fn bind_one(
    statement: &mut rustdb::Statement<'_>,
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
            let bytes: Vec<u8> = (0..rustdb_compat::perf::BLOB_BYTES)
                .map(|offset| ((iteration as usize + offset) & 0xff) as u8)
                .collect();
            statement.bind_blob(position, &bytes)
        }
    };
    outcome.map_err(|error| error.message().to_string())
}

// ---------------------------------------------------------------------------
// The plans.
// ---------------------------------------------------------------------------

/// Returns the row count one scale uses.
///
/// Small fits in the cache budget; medium exceeds it and fits in memory; large
/// exceeds the budget by enough that the page cache cannot hold the working set
/// and the file system has to answer.
fn rows_for(scale: &str) -> u32 {
    match scale {
        "small" => 5_000,
        "medium" => 100_000,
        _ => 600_000,
    }
}

/// Returns how many times a workload repeats at one scale.
///
/// The repeat counts are chosen so that one round takes a comparable amount of
/// time at every scale: a point read is cheap and runs many times, a full scan
/// of six hundred thousand rows is not and runs once.
fn repeats_for(scale: &str) -> (u32, u32, u32) {
    match scale {
        "small" => (4_000, 400, 2_000),
        "medium" => (4_000, 40, 2_000),
        _ => (2_000, 4, 1_000),
    }
}

/// Returns a subquery producing `seq` from 1 upwards, far enough for `wanted`.
///
/// One digit table joined to itself as many times as the count needs. Six
/// copies reach a million, which is above every scale here; fewer are used when
/// fewer will do, because a cross join nobody needs is a million rows nobody
/// reads.
fn counter_sql(wanted: u32) -> String {
    let mut digits = 1usize;
    while 10u64.pow(digits as u32) < u64::from(wanted) && digits < 7 {
        digits = digits.saturating_add(1);
    }
    let names: Vec<String> = (0..digits).map(|index| format!("d{index}")).collect();
    let mut expression = String::new();
    for (position, name) in names.iter().enumerate() {
        if position == 0 {
            expression.push_str(&format!("{name}.n"));
        } else {
            expression = format!("({expression} * 10 + {name}.n)");
        }
    }
    let from: Vec<String> = names.iter().map(|name| format!("digits {name}")).collect();
    format!("SELECT {expression} + 1 AS seq FROM {}", from.join(", "))
}

/// Returns the plan for one scale.
fn plan_for(scale: &str) -> Plan {
    let rows = rows_for(scale);
    let (point, scan, write) = repeats_for(scale);
    let mut setup = vec![
        "CREATE TABLE main_table(id INTEGER PRIMARY KEY, key INTEGER NOT NULL, \
         category INTEGER NOT NULL, label TEXT NOT NULL, payload BLOB)"
            .to_string(),
        "CREATE INDEX main_key ON main_table(key)".to_string(),
        "CREATE INDEX main_category ON main_table(category, key)".to_string(),
        "CREATE TABLE side_table(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL, note TEXT)"
            .to_string(),
        "CREATE INDEX side_owner ON side_table(owner)".to_string(),
        "CREATE TABLE wide(id INTEGER PRIMARY KEY, body TEXT)".to_string(),
    ];
    // The rows are generated by SQL rather than by a loop of inserts, so the
    // plan file stays small and both engines build the same rows from the same
    // expression. A cross join of a ten-row digit table is the portable way to
    // count: a recursive CTE reads better and is not accepted on the left of an
    // INSERT by both engines, and a plan that had to be written twice would be
    // the one thing this file exists to avoid.
    setup.push("CREATE TABLE digits(n INTEGER PRIMARY KEY)".to_string());
    setup.push("INSERT INTO digits(n) VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)".to_string());
    setup.push(format!(
        "INSERT INTO main_table(id, key, category, label, payload) \
         SELECT seq, (seq * 2654435761) % {rows}, seq % 64, \
                'row ' || seq || ' lorem ipsum dolor sit amet consectetur', zeroblob(48) \
         FROM ({counter}) WHERE seq <= {rows}",
        counter = counter_sql(rows)
    ));
    setup.push(format!(
        "INSERT INTO side_table(id, owner, note) \
         SELECT seq, ((seq * 7) % {rows}) + 1, 'note ' || seq \
         FROM ({counter}) WHERE seq <= {side}",
        side = rows / 4,
        counter = counter_sql(rows / 4)
    ));
    setup.push(format!(
        "INSERT INTO wide(id, body) \
         SELECT seq, replace(hex(zeroblob(2048)), '0', 'x') FROM ({counter}) WHERE seq <= 400",
        counter = counter_sql(400)
    ));
    setup.push("ANALYZE".to_string());

    let workloads = vec![
        Workload {
            name: "prepare.trivial".to_string(),
            family: "open.prepare".to_string(),
            sql: "SELECT 1".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: true,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "prepare.point".to_string(),
            family: "open.prepare".to_string(),
            sql: "SELECT label FROM main_table WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: true,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "point.rowid".to_string(),
            family: "read.point".to_string(),
            sql: "SELECT label FROM main_table WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "point.index".to_string(),
            family: "read.point".to_string(),
            sql: "SELECT id, label FROM main_table WHERE key = ?1".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "point.miss".to_string(),
            family: "read.point".to_string(),
            sql: "SELECT label FROM main_table WHERE id = ?1 + 100000000".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "range.covering".to_string(),
            family: "read.range".to_string(),
            sql: "SELECT count(key) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200".to_string(),
            pre: None,
            post: None,
            repeat: point / 4,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "range.lookaside".to_string(),
            family: "read.range".to_string(),
            sql: "SELECT sum(length(label)) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200"
                .to_string(),
            pre: None,
            post: None,
            repeat: point / 8,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "range.reverse".to_string(),
            family: "read.range".to_string(),
            sql: "SELECT id FROM main_table WHERE id <= ?1 ORDER BY id DESC LIMIT 50".to_string(),
            pre: None,
            post: None,
            repeat: point / 4,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "scan.aggregate".to_string(),
            family: "read.analytical".to_string(),
            sql: "SELECT count(*), sum(key), max(category) FROM main_table".to_string(),
            pre: None,
            post: None,
            repeat: scan,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "scan.group".to_string(),
            family: "read.analytical".to_string(),
            sql: "SELECT category, count(*) FROM main_table GROUP BY category ORDER BY category"
                .to_string(),
            pre: None,
            post: None,
            repeat: scan,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "scan.sort".to_string(),
            family: "read.analytical".to_string(),
            sql: "SELECT id FROM main_table ORDER BY label LIMIT 100".to_string(),
            pre: None,
            post: None,
            repeat: scan,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "scan.distinct".to_string(),
            family: "read.analytical".to_string(),
            sql: "SELECT DISTINCT category FROM main_table ORDER BY category".to_string(),
            pre: None,
            post: None,
            repeat: scan,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "join.selective".to_string(),
            family: "read.join".to_string(),
            sql: "SELECT count(*) FROM main_table JOIN side_table ON side_table.owner = \
                  main_table.id WHERE main_table.id = ?1"
                .to_string(),
            pre: None,
            post: None,
            repeat: point / 2,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "join.range".to_string(),
            family: "read.join".to_string(),
            sql: "SELECT count(side_table.note) FROM main_table JOIN side_table ON \
                  side_table.owner = main_table.id WHERE main_table.key BETWEEN ?1 AND ?1 + 200"
                .to_string(),
            pre: None,
            post: None,
            repeat: point / 8,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "write.insert.batch".to_string(),
            family: "write".to_string(),
            sql: "INSERT INTO main_table(id, key, category, label, payload) VALUES (?1, ?2, ?3, ?4, ?5)"
                .to_string(),
            pre: None,
            post: None,
            repeat: write,
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Counter, Bind::Int, Bind::Int, Bind::Text, Bind::Blob],
            mutates: true,
        },
        Workload {
            name: "write.insert.autocommit".to_string(),
            family: "write".to_string(),
            sql: "INSERT INTO side_table(owner, note) VALUES (?1, ?2)".to_string(),
            pre: None,
            post: None,
            repeat: (write / 20).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Int, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "write.update.indexed".to_string(),
            family: "write".to_string(),
            sql: "UPDATE main_table SET key = key + 1 WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: write,
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: true,
        },
        Workload {
            name: "write.delete".to_string(),
            family: "write".to_string(),
            sql: "DELETE FROM main_table WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: write,
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: true,
        },
        Workload {
            name: "write.upsert".to_string(),
            family: "write".to_string(),
            sql: "INSERT INTO wide(id, body) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET \
                  body = excluded.body"
                .to_string(),
            pre: None,
            post: None,
            repeat: (write / 4).max(20),
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Rowid, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "txn.autocommit".to_string(),
            family: "transaction".to_string(),
            sql: "UPDATE side_table SET note = ?2 WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: (write / 20).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "txn.batched".to_string(),
            family: "transaction".to_string(),
            sql: "UPDATE side_table SET note = ?2 WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: write,
            grouping: Grouping::Every(10),
            prepare_each: false,
            binds: vec![Bind::Scatter, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "txn.large".to_string(),
            family: "transaction".to_string(),
            sql: "UPDATE side_table SET note = ?2 WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: write,
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Scatter, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "schema.index".to_string(),
            family: "schema".to_string(),
            sql: "CREATE INDEX main_label ON main_table(label)".to_string(),
            pre: Some("DROP INDEX IF EXISTS main_label".to_string()),
            post: Some("DROP INDEX IF EXISTS main_label".to_string()),
            repeat: 1,
            grouping: Grouping::Autocommit,
            prepare_each: true,
            binds: Vec::new(),
            mutates: true,
        },
        Workload {
            name: "extension.json".to_string(),
            family: "extension".to_string(),
            sql: "SELECT json_extract('{\"a\":[1,2,3],\"b\":{\"c\":\"d\"}}', '$.b.c')".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "extension.fts.build".to_string(),
            family: "extension".to_string(),
            sql: "INSERT INTO documents(title, body) VALUES (?1, ?2)".to_string(),
            pre: Some(
                "DROP TABLE IF EXISTS documents; \
                 CREATE VIRTUAL TABLE documents USING fts5(title, body)"
                    .to_string(),
            ),
            post: None,
            repeat: (write / 4).max(50),
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Text, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "extension.fts.query".to_string(),
            family: "extension".to_string(),
            sql: "SELECT count(*) FROM documents WHERE documents MATCH 'lorem'".to_string(),
            pre: None,
            post: Some("DROP TABLE IF EXISTS documents".to_string()),
            repeat: (point / 8).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "extension.rtree.insert".to_string(),
            family: "extension".to_string(),
            sql: "INSERT INTO boxes(id, minX, maxX, minY, maxY) VALUES (?1, ?2, ?2 + 10, ?3, ?3 + 10)"
                .to_string(),
            pre: Some(
                "DROP TABLE IF EXISTS boxes; \
                 CREATE VIRTUAL TABLE boxes USING rtree(id, minX, maxX, minY, maxY)"
                    .to_string(),
            ),
            post: None,
            repeat: (write / 4).max(50),
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Counter, Bind::Int, Bind::Int],
            mutates: true,
        },
        Workload {
            name: "extension.rtree.query".to_string(),
            family: "extension".to_string(),
            sql: "SELECT count(*) FROM boxes WHERE minX > ?1 AND maxX < ?1 + 100000".to_string(),
            pre: None,
            post: Some("DROP TABLE IF EXISTS boxes".to_string()),
            repeat: (point / 8).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Int],
            mutates: false,
        },
        Workload {
            name: "large.read".to_string(),
            family: "large.values".to_string(),
            sql: "SELECT length(body) FROM wide WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: (point / 2).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Rowid],
            mutates: false,
        },
        Workload {
            name: "large.write".to_string(),
            family: "large.values".to_string(),
            sql: "UPDATE wide SET body = ?2 || body WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: (write / 20).max(20),
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Rowid, Bind::Text],
            mutates: true,
        },
    ];
    Plan {
        scale: scale.to_string(),
        rows,
        journal: "delete".to_string(),
        synchronous: "full".to_string(),
        page_size: 4096,
        cache_size: -2000,
        setup,
        workloads,
    }
}

// ---------------------------------------------------------------------------
// Reporting.
// ---------------------------------------------------------------------------

/// Renders the scorecard a person reads.
fn render_markdown(
    sections: &[(Plan, Vec<Paired>)],
    contract: &Contract,
    label: &str,
    rounds: u32,
) -> String {
    let mut out = String::new();
    out.push_str("# rust-db performance scorecard\n\n");
    out.push_str(&format!(
        "Label `{label}`, platform `{}`, {rounds} paired rounds per scale, bootstrap seed {SEED}.\n\n",
        platform_name()
    ));
    out.push_str(
        "Both engines read the same plan file. The ratio is SQLite over rust-db, so **above one \
         means rust-db is faster**. A workload whose two engines returned different answers is \
         reported as a correctness failure and is not timed.\n\n",
    );
    out.push_str("## Fair configuration\n\n");
    if let Some((plan, _)) = sections.first() {
        out.push_str(&format!(
            "| setting | value |\n|---|---|\n| journal mode | `{}` |\n| synchronous | `{}` |\n| \
             page size | {} |\n| cache | {} pages-or-KiB (SQLite units) |\n| statement reuse | \
             prepared once except the `open.prepare` family |\n| database | on disk, cloned from \
             one pristine image per round |\n\n",
            plan.journal, plan.synchronous, plan.page_size, plan.cache_size
        ));
    }
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
            let members: Vec<&Paired> = measured
                .iter()
                .filter(|paired| paired.family == family.id)
                .collect();
            if members.is_empty() {
                continue;
            }
            let logs: Vec<f64> = members
                .iter()
                .filter(|paired| paired.agreed)
                .flat_map(|paired| paired.log_ratios())
                .collect();
            // The geometric mean rather than the median, so the point estimate
            // is the same statistic the interval brackets. A median beside a
            // bootstrapped mean can sit outside its own interval, which reads
            // like an arithmetic error and is one.
            let mean = if logs.is_empty() {
                0.0
            } else {
                logs.iter().sum::<f64>() / logs.len() as f64
            };
            let ratio = mean.exp();
            let (low, high) = rustdb_compat::perf::bootstrap(&logs, SEED);
            let (low, high) = (low.exp(), high.exp());
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
            "| workload | family | rust-db median | SQLite median | ratio | 95% interval | \
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
                "        {{\"workload\": {}, \"family\": {}, \"agreed\": {}, \"rustdb_nanos\": \
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

/// Appends this run to the versioned performance history.
///
/// One line per workload per run, with the label and the platform, so a
/// regression is a comparison against the file rather than against somebody's
/// memory of the last number.
fn append_history(
    out: &Path,
    sections: &[(Plan, Vec<Paired>)],
    contract: &Contract,
    label: &str,
) -> Result<(), String> {
    let path = out.join("history.jsonl");
    let mut text = String::new();
    for (plan, measured) in sections {
        let (centre, low, high) = headline(measured, contract);
        text.push_str(&format!(
            "{{\"label\": {}, \"platform\": {}, \"scale\": {}, \"workload\": \"*headline*\", \
             \"ratio\": {centre:.6}, \"low\": {low:.6}, \"high\": {high:.6}, \"samples\": {}}}\n",
            json_string(label),
            json_string(&platform_name()),
            json_string(&plan.scale),
            measured.first().map(|entry| entry.pairs.len()).unwrap_or(0)
        ));
        for paired in measured {
            let (low, high) = paired.interval(SEED);
            let (ours, theirs) = paired.medians();
            text.push_str(&format!(
                "{{\"label\": {}, \"platform\": {}, \"scale\": {}, \"workload\": {}, \"family\": \
                 {}, \"agreed\": {}, \"rustdb_nanos\": {ours:.1}, \"sqlite_nanos\": {theirs:.1}, \
                 \"ratio\": {:.6}, \"low\": {low:.6}, \"high\": {high:.6}, \"samples\": {}}}\n",
                json_string(label),
                json_string(&platform_name()),
                json_string(&plan.scale),
                json_string(&paired.workload),
                json_string(&paired.family),
                paired.agreed,
                paired.ratio(),
                paired.pairs.len()
            ));
        }
    }
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    existing.push_str(&text);
    std::fs::write(&path, existing)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}
