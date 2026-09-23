//! What each phase 11 and 12 feature family costs, measured against SQLite.
//!
//! Invariant: this measures, it does not judge. Both engines run the identical
//! workload through their own front door - inillucent through `inillucent-engine`
//! directly (the shipped engine, since the old `inillucent-session` this file
//! used to open was retired with the rest of that engine), SQLite through the
//! pinned shell or amalgamation - and what is recorded is what each took. A
//! ratio is printed because that is the number a reader wants, and nothing here
//! decides whether a ratio is acceptable.
//!
//! FTS5 and R-Tree need no explicit registration on the new engine: both are
//! part of `inillucent_ext::registry::Registry::with_builtins()`, which every
//! `Database::open` builds its module registry from.
//!
//! Six families, chosen because each exercises a different thing the phase
//! added and would show a different regression:
//!
//! - **API overhead**: prepare, bind, step, finalize on a trivial statement.
//!   It is almost entirely the cost of the boundary, which is what makes it
//!   the one to watch after a change to the handle machinery.
//! - **JSON/JSONB**: parsing, path extraction and re-rendering, which is the
//!   only family where the work is all in a value rather than in a b-tree.
//! - **Virtual scans**: a table-valued function driven a row at a time, which
//!   is `xFilter`/`xNext`/`xColumn` and nothing else.
//! - **FTS5**: building an index and then querying it, the two halves being
//!   worth separating because they regress independently.
//! - **R-Tree**: the same, for a spatial index.
//! - **CLI import and dump**: the one family measured as a whole program,
//!   because that is how it is used.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-featureperf -- [--out <dir>]`

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use inillucent_compat::report::json_string;
use inillucent_compat::{platform_name, workspace_root};
use inillucent_engine::connect::{Connection, Database};

/// One measured family.
struct Measurement {
    /// Which family.
    family: String,
    /// What was done.
    operation: String,
    /// How many times.
    iterations: u32,
    /// What inillucent took, in nanoseconds per iteration.
    inillucent_nanos: f64,
    /// What the reference took, or `None` when it was not available.
    sqlite_nanos: Option<f64>,
}

impl Measurement {
    /// Returns the ratio of this engine to the reference, when there is one.
    fn ratio(&self) -> Option<f64> {
        let sqlite = self.sqlite_nanos?;
        if sqlite <= 0.0 {
            return None;
        }
        Some(self.inillucent_nanos / sqlite)
    }
}

/// Runs every family and writes the artifact.
fn main() -> ExitCode {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    // **Pinned before anything is timed, and the mask printed (task-2085).**
    // Unpinned, a hybrid processor can run this program and the arm it compares
    // against on different core classes, and nothing else in the output says so.
    if let Err(reason) = inillucent_compat::affinity::pin_from_arguments(&mut arguments) {
        eprintln!("{reason}");
        return ExitCode::from(2);
    }
    let out = flag(&arguments, "--out")
        .unwrap_or_else(|| workspace_root().join("_agent_output/featureperf"));
    match run(&out) {
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
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}

/// Measures every family and writes what it found.
fn run(out: &Path) -> Result<String, String> {
    let mut measurements = Vec::new();
    measurements.push(api_overhead()?);
    measurements.extend(json_family()?);
    measurements.push(virtual_scan()?);
    measurements.extend(fts5_family()?);
    measurements.extend(rtree_family()?);
    measurements.extend(cli_family()?);
    std::fs::create_dir_all(out).map_err(|error| format!("cannot create {out:?}: {error}"))?;
    let path = out.join("feature-perf.json");
    std::fs::write(&path, artifact(&measurements))
        .map_err(|error| format!("cannot write {path:?}: {error}"))?;
    let readable = out.join("feature-perf.md");
    std::fs::write(&readable, table(&measurements))
        .map_err(|error| format!("cannot write {readable:?}: {error}"))?;
    Ok(format!(
        "wrote {} measurements to {}",
        measurements.len(),
        path.display()
    ))
}

/// Returns a fresh in-memory connection.
fn open() -> Result<Connection<'static>, String> {
    let database = Database::open(":memory:").map_err(|error| error.message().to_string())?;
    // The database is leaked so the connection can be returned alone. This is a
    // measurement program that runs for a second and exits.
    let database: &'static Database = Box::leak(Box::new(database));
    Ok(database.session())
}

/// Runs a statement for its effect.
fn exec(connection: &Connection<'_>, sql: &str) -> Result<(), String> {
    connection
        .execute_batch(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))
}

/// Steps a statement to completion, returning how many rows it produced.
fn drain(connection: &Connection<'_>, sql: &str) -> Result<u32, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    let mut rows = 0;
    while statement
        .step()
        .map_err(|error| format!("{sql}: {}", error.message()))?
    {
        rows += 1;
    }
    Ok(rows)
}

/// Returns the nanoseconds one iteration of a body took, over `iterations`.
fn time(iterations: u32, mut body: impl FnMut() -> Result<(), String>) -> Result<f64, String> {
    // One warm run first: the first statement of a kind compiles a program and
    // fills a cache, and measuring that once as though it were typical is the
    // easiest way to produce a number nobody can reproduce.
    body()?;
    let started = Instant::now();
    for _ in 0..iterations {
        body()?;
    }
    Ok(per_iteration(started.elapsed(), iterations))
}

/// Returns the nanoseconds per iteration of a measured run.
fn per_iteration(elapsed: Duration, iterations: u32) -> f64 {
    if iterations == 0 {
        return 0.0;
    }
    elapsed.as_secs_f64() * 1e9 / f64::from(iterations)
}

/// Measures the cost of the statement boundary itself.
fn api_overhead() -> Result<Measurement, String> {
    let connection = open()?;
    let iterations = 20_000;
    let inillucent = time(iterations, || drain(&connection, "SELECT 1").map(|_| ()))?;
    Ok(Measurement {
        family: "api".to_string(),
        operation: "prepare, step and finalize `SELECT 1`".to_string(),
        iterations,
        inillucent_nanos: inillucent,
        // Enough statements that the statements dominate the process this
        // is measured through; see `shell_nanos`.
        sqlite_nanos: shell_nanos(iterations, "SELECT 1;"),
    })
}

/// Measures the JSON family: parsing, extraction and rendering.
fn json_family() -> Result<Vec<Measurement>, String> {
    let connection = open()?;
    let document = "'{\"a\":[1,2,3],\"b\":{\"c\":\"d\"},\"e\":null,\"f\":1.5}'";
    let iterations = 5_000;
    let extract = time(iterations, || {
        drain(
            &connection,
            &format!("SELECT json_extract({document}, '$.b.c')"),
        )
        .map(|_| ())
    })?;
    let binary = time(iterations, || {
        drain(&connection, &format!("SELECT jsonb({document})")).map(|_| ())
    })?;
    Ok(vec![
        Measurement {
            family: "json".to_string(),
            operation: "parse and extract one path".to_string(),
            iterations,
            inillucent_nanos: extract,
            sqlite_nanos: shell_nanos(
                iterations,
                &format!("SELECT json_extract({document}, '$.b.c');"),
            ),
        },
        Measurement {
            family: "json".to_string(),
            operation: "parse and encode as JSONB".to_string(),
            iterations,
            inillucent_nanos: binary,
            sqlite_nanos: shell_nanos(iterations, &format!("SELECT jsonb({document});")),
        },
    ])
}

/// Measures a table-valued function driven a row at a time.
fn virtual_scan() -> Result<Measurement, String> {
    let connection = open()?;
    let iterations = 500;
    let sql = "SELECT count(*) FROM generate_series(1, 2000)";
    let inillucent = time(iterations, || drain(&connection, sql).map(|_| ()))?;
    Ok(Measurement {
        family: "vtab".to_string(),
        operation: "scan two thousand rows of a table-valued function".to_string(),
        iterations,
        inillucent_nanos: inillucent,
        sqlite_nanos: shell_nanos(iterations, &format!("{sql};")),
    })
}

/// Measures building and querying a full-text index.
fn fts5_family() -> Result<Vec<Measurement>, String> {
    let rows = 400;
    let build_sql = fts5_script(rows);
    let started = Instant::now();
    {
        let connection = open()?;
        exec(&connection, &build_sql)?;
    }
    let build = per_iteration(started.elapsed(), rows);

    let connection = open()?;
    exec(&connection, &build_sql)?;
    let iterations = 2_000;
    let query = time(iterations, || {
        drain(
            &connection,
            "SELECT count(*) FROM docs WHERE docs MATCH 'alpha'",
        )
        .map(|_| ())
    })?;
    let ranked = time(iterations / 2, || {
        drain(
            &connection,
            "SELECT rowid FROM docs WHERE docs MATCH 'alpha OR beta' ORDER BY rank",
        )
        .map(|_| ())
    })?;
    Ok(vec![
        Measurement {
            family: "fts5".to_string(),
            operation: "index one document".to_string(),
            iterations: rows,
            inillucent_nanos: build,
            sqlite_nanos: shell_build_nanos(&build_sql, rows),
        },
        Measurement {
            family: "fts5".to_string(),
            operation: "match one term".to_string(),
            iterations,
            inillucent_nanos: query,
            sqlite_nanos: None,
        },
        Measurement {
            family: "fts5".to_string(),
            operation: "match two terms and rank".to_string(),
            iterations: iterations / 2,
            inillucent_nanos: ranked,
            sqlite_nanos: None,
        },
    ])
}

/// Returns a script that builds a full-text index of `rows` documents.
fn fts5_script(rows: u32) -> String {
    let mut sql = String::from("CREATE VIRTUAL TABLE docs USING fts5(title, body);\n");
    for index in 0..rows {
        let word = ["alpha", "beta", "gamma", "delta"]
            .get((index % 4) as usize)
            .copied()
            .unwrap_or("alpha");
        sql.push_str(&format!(
            "INSERT INTO docs VALUES ('{word} document {index}', 'body {index} with {word} words in it');\n"
        ));
    }
    sql
}

/// Measures building and querying a spatial index.
fn rtree_family() -> Result<Vec<Measurement>, String> {
    let rows = 400;
    let build_sql = rtree_script(rows);
    let started = Instant::now();
    {
        let connection = open()?;
        exec(&connection, &build_sql)?;
    }
    let build = per_iteration(started.elapsed(), rows);

    let connection = open()?;
    exec(&connection, &build_sql)?;
    let iterations = 2_000;
    let query = time(iterations, || {
        drain(
            &connection,
            "SELECT count(*) FROM spots WHERE minX > 100 AND maxX < 200",
        )
        .map(|_| ())
    })?;
    Ok(vec![
        Measurement {
            family: "rtree".to_string(),
            operation: "insert one entry".to_string(),
            iterations: rows,
            inillucent_nanos: build,
            sqlite_nanos: shell_build_nanos(&build_sql, rows),
        },
        Measurement {
            family: "rtree".to_string(),
            operation: "query a bounding box".to_string(),
            iterations,
            inillucent_nanos: query,
            sqlite_nanos: None,
        },
    ])
}

/// Returns a script that builds a spatial index of `rows` entries.
fn rtree_script(rows: u32) -> String {
    let mut sql =
        String::from("CREATE VIRTUAL TABLE spots USING rtree(id, minX, maxX, minY, maxY);\n");
    for index in 0..rows {
        let base = f64::from(index) * 1.5;
        sql.push_str(&format!(
            "INSERT INTO spots VALUES ({index}, {base}, {}, {base}, {});\n",
            base + 1.0,
            base + 1.0
        ));
    }
    sql
}

/// Measures the shell importing a file and dumping a database.
fn cli_family() -> Result<Vec<Measurement>, String> {
    let Some(ours) = shell_binary("inillucent-shell") else {
        return Ok(Vec::new());
    };
    let area = workspace_root().join("_agent_output/featureperf");
    std::fs::create_dir_all(&area).map_err(|error| error.to_string())?;
    let rows = 5_000u32;
    let csv = area.join("import.csv");
    let mut body = String::new();
    for index in 0..rows {
        body.push_str(&format!("{index},name{index},{}\n", f64::from(index) * 0.5));
    }
    std::fs::write(&csv, body).map_err(|error| error.to_string())?;

    let mut measurements = Vec::new();
    let script = format!(
        "CREATE TABLE loaded(n, label, amount);\n.mode csv\n.import {} loaded\n",
        csv.to_string_lossy().replace('\\', "/")
    );
    let ours_import = run_shell(&ours, &area.join("perf-inillucent.db"), &script);
    let reference = shell_binary_reference();
    let sqlite_import = reference
        .as_ref()
        .and_then(|shell| run_shell(shell, &area.join("perf-sqlite.db"), &script));
    if let Some(elapsed) = ours_import {
        measurements.push(Measurement {
            family: "cli".to_string(),
            operation: "import one CSV row".to_string(),
            iterations: rows,
            inillucent_nanos: per_iteration(elapsed, rows),
            sqlite_nanos: sqlite_import.map(|taken| per_iteration(taken, rows)),
        });
    }
    let dump = ".dump\n";
    let ours_dump = run_shell(&ours, &area.join("perf-inillucent.db"), dump);
    let sqlite_dump = reference
        .as_ref()
        .and_then(|shell| run_shell(shell, &area.join("perf-sqlite.db"), dump));
    if let Some(elapsed) = ours_dump {
        measurements.push(Measurement {
            family: "cli".to_string(),
            operation: "dump one row".to_string(),
            iterations: rows,
            inillucent_nanos: per_iteration(elapsed, rows),
            sqlite_nanos: sqlite_dump.map(|taken| per_iteration(taken, rows)),
        });
    }
    Ok(measurements)
}

/// Returns one of this workspace's binaries, if it has been built.
fn shell_binary(name: &str) -> Option<PathBuf> {
    for profile in ["release", "debug"] {
        let path = workspace_root()
            .join("target")
            .join(profile)
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Returns the pinned SQLite shell, if it has been downloaded.
fn shell_binary_reference() -> Option<PathBuf> {
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4/shell")
        .join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Runs a script through a shell on a fresh database and times it.
fn run_shell(program: &Path, database: &Path, script: &str) -> Option<Duration> {
    use std::io::Write;
    use std::process::Stdio;
    let started = Instant::now();
    // Started through the affinity check (task-2085), so a shell running on
    // other processors from this program is not timed.
    let mut child = inillucent_compat::affinity::spawn_on_same_cores(
        Command::new(program)
            .arg(database)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        "a shell",
    )
    .map_err(|reason| eprintln!("{reason}"))
    .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
    }
    child.wait().ok()?;
    Some(started.elapsed())
}

/// Times a statement in the pinned shell, per iteration.
///
/// The shell is a whole process, so this is only comparable at a coarse grain:
/// what it measures is a run of `iterations` statements, minus nothing. It is
/// reported anyway because the ratio it gives is the right order of magnitude
/// and a missing number is worse than a coarse one.
fn shell_nanos(iterations: u32, statement: &str) -> Option<f64> {
    if iterations == 0 {
        return None;
    }
    let shell = shell_binary_reference()?;
    let script: String = std::iter::repeat_n(statement, iterations as usize)
        .collect::<Vec<&str>>()
        .join("\n");
    // The baseline is one statement in the same shell, so what is subtracted is
    // process startup and the parse of one statement.
    let baseline = run_shell(&shell, Path::new(":memory:"), statement)?;
    let elapsed = run_shell(&shell, Path::new(":memory:"), &script)?;
    let net = elapsed.checked_sub(baseline)?;
    // Anything under a tenth of the baseline is startup jitter rather than
    // work, and reporting it would produce a ratio that means nothing.
    if net.as_secs_f64() * 10.0 < baseline.as_secs_f64() {
        return None;
    }
    Some(per_iteration(net, iterations))
}

/// Times a build script in the pinned shell, per row.
///
/// In memory on both sides. Building on disk in the reference while building in
/// memory here would compare fsync rather than index-building, and every
/// unwrapped `INSERT` in a script is its own commit.
fn shell_build_nanos(script: &str, rows: u32) -> Option<f64> {
    let shell = shell_binary_reference()?;
    let baseline = run_shell(&shell, Path::new(":memory:"), "SELECT 1;")?;
    let elapsed = run_shell(&shell, Path::new(":memory:"), script)?;
    Some(per_iteration(elapsed.checked_sub(baseline)?, rows))
}

/// Returns the JSON artifact.
fn artifact(measurements: &[Measurement]) -> String {
    let mut out = String::from("{\n");
    out.push_str(&format!(
        "  \"platform\": {},\n",
        json_string(&platform_name())
    ));
    out.push_str("  \"measurements\": [\n");
    for (index, measurement) in measurements.iter().enumerate() {
        out.push_str("    {");
        out.push_str(&format!(
            "\"family\": {}, ",
            json_string(&measurement.family)
        ));
        out.push_str(&format!(
            "\"operation\": {}, ",
            json_string(&measurement.operation)
        ));
        out.push_str(&format!("\"iterations\": {}, ", measurement.iterations));
        out.push_str(&format!(
            "\"inillucent_nanos\": {:.1}, ",
            measurement.inillucent_nanos
        ));
        match measurement.sqlite_nanos {
            Some(nanos) => out.push_str(&format!("\"sqlite_nanos\": {nanos:.1}")),
            None => out.push_str("\"sqlite_nanos\": null"),
        }
        out.push('}');
        if index + 1 < measurements.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ]\n}\n");
    out
}

/// Returns the readable table.
fn table(measurements: &[Measurement]) -> String {
    let mut out = String::from("# Feature-family performance\n\n");
    out.push_str(&format!("Platform: `{}`\n\n", platform_name()));
    out.push_str(
        "Both engines run the same workload through their own front door. The\n\
         ratio is inillucent over the reference, so below one is faster. A missing\n\
         reference number means the workload has no comparable single-process\n\
         form in the shell, not that it was skipped.\n\n",
    );
    out.push_str("| family | operation | iterations | inillucent ns | SQLite ns | ratio |\n");
    out.push_str("|---|---|---:|---:|---:|---:|\n");
    for measurement in measurements {
        let sqlite = match measurement.sqlite_nanos {
            Some(nanos) => format!("{nanos:.0}"),
            None => "-".to_string(),
        };
        let ratio = match measurement.ratio() {
            Some(ratio) => format!("{ratio:.2}x"),
            None => "-".to_string(),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {:.0} | {sqlite} | {ratio} |\n",
            measurement.family,
            measurement.operation,
            measurement.iterations,
            measurement.inillucent_nanos
        ));
    }
    out
}
