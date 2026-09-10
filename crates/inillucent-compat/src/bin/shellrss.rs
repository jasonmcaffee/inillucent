//! Peak resident set of each shell, opening the same data and reading it.
//!
//! Invariant: both arms are **separate processes measured the same way**, which
//! is the one comparison of memory this workspace can make honestly. The gate's
//! own table cannot: this engine runs inside it, alongside the harness and the
//! plan, so what the gate reports for its arm is a delta over a region rather
//! than a process's peak. Two shells are two processes, and the operating
//! system's accounting for each is the whole answer.
//!
//! The two open different files, and that is the measurement rather than a
//! flaw: each shell builds its own copy from the same SQL, then opens it.
//! File-format compatibility was dropped by the rearchitecture, so there is no one file
//! both can read, and the question - what does it cost to hold this data and
//! read it - is about the data rather than the file.
//!
//! Usage:
//!   inillucent-shellrss

use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use inillucent_compat::procstat::{child_cost, mebibytes, ProcessCost};
use inillucent_compat::workspace_root;

/// The rows each shell is asked to hold, and how they are made.
///
/// **Each engine builds its own file from the same SQL.** There is no one file
/// both can read - the rearchitecture dropped the file format - so a comparison of what
/// it costs to hold this data has to give each engine the data rather than a
/// file. Written through a recursive CTE so the build is one statement and the
/// two arms are given identical text.
const BUILD: &str = concat!(
    "CREATE TABLE main_table(id INTEGER PRIMARY KEY, key INTEGER NOT NULL, ",
    "category INTEGER NOT NULL, label TEXT NOT NULL);\n",
    "CREATE INDEX main_key ON main_table(key);\n",
    "CREATE INDEX main_category ON main_table(category, key);\n",
    "INSERT INTO main_table(id, key, category, label) ",
    "SELECT i, (i * 7919) % 100000, i % 64, 'label-' || i FROM ",
    "(WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n ",
    "WHERE i < 200000) SELECT i FROM n);\n",
    // **Checkpointed, or the reading process is measured replaying a log
    // rather than holding data.** This engine writes about 940 bytes of WAL
    // per row and does not checkpoint when a shell exits, so an
    // uncheckpointed build of this table leaves 940 MB of segments behind
    // and the next open replays every byte of them: 1,026 MiB peak and
    // 2.9 s of processor time, which measures recovery rather than
    // residency. `sqlite3` answers the same pragma harmlessly on a
    // rollback-journal database.
    "PRAGMA wal_checkpoint;\n",
);

/// The statements each shell runs while its memory is being watched.
///
/// The read plan's own shapes: a count, a point lookup, a grouped aggregate and
/// a sum over every row - which is what makes the peak a peak.
const SCRIPT: &str = "SELECT count(*) FROM main_table;\n\
                      SELECT id, label FROM main_table WHERE id = 500;\n\
                      SELECT category, count(*) FROM main_table GROUP BY category ORDER BY category;\n\
                      SELECT sum(key) FROM main_table;\n";

/// Reports how many rows a shell says the table holds.
///
/// **So that a peak is a peak over the same data.** A build that silently did
/// nothing would leave both shells reading an empty table and reporting a
/// pleasingly small resident set, which is the one failure a memory comparison
/// cannot notice on its own.
///
/// @param exe - the shell to ask
/// @param database - the file it built
fn rows_in(exe: &Path, database: &Path) -> Result<String, String> {
    let output = Command::new(exe)
        .arg(database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut input) = child.stdin.take() {
                let _ = input.write_all(
                    b"SELECT count(*) FROM main_table;
",
                );
            }
            child.wait_with_output()
        })
        .map_err(|error| format!("{} did not answer: {error}", exe.display()))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Runs one shell over one database with a script, and returns what it cost.
///
/// @param exe - the shell to run
/// @param database - the file to open
/// @param script - the statements to feed it
fn run_script(exe: &Path, database: &Path, script: &str) -> Result<ProcessCost, String> {
    let mut child = Command::new(exe)
        .arg(database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{} did not start: {error}", exe.display()))?;
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(script.as_bytes());
    }
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut out);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut err);
    }
    let status = child
        .wait()
        .map_err(|error| format!("{} did not finish: {error}", exe.display()))?;
    let cost = child_cost(&child);
    if !status.success() {
        return Err(format!("{} failed: {err}", exe.display()));
    }
    Ok(cost)
}

/// Builds one engine's own copy of the data, and returns the file.
///
/// Outside everything measured: what is watched is a shell opening a finished
/// file and reading it.
///
/// @param exe - the shell that builds it
/// @param name - what to call the file
fn build(exe: &Path, name: &str) -> Result<std::path::PathBuf, String> {
    let root = workspace_root().join("_agent_output/shellrss");
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let target = root.join(name);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(root.join(format!("{name}{suffix}")));
    }
    run_script(exe, &target, BUILD)?;
    Ok(target)
}

fn main() -> ExitCode {
    // An optional statement list, so the four reads can be measured one at a
    // time when a number needs explaining. Default: all four.
    let script = std::env::args()
        .skip(1)
        .find(|value| !value.starts_with("--"))
        .unwrap_or_else(|| SCRIPT.to_string());
    let sqlite = workspace_root().join(format!(
        ".sqlite-ref/3.53.4/shell/sqlite3{}",
        std::env::consts::EXE_SUFFIX
    ));
    let shell = workspace_root().join(format!(
        "target/release/inillucent-shell{}",
        std::env::consts::EXE_SUFFIX
    ));
    let measure = |exe: &Path, name: &str| -> Result<(ProcessCost, String), String> {
        let database = build(exe, name)?;
        let cost = run_script(exe, &database, &script)?;
        Ok((cost, rows_in(exe, &database)?))
    };
    let (their_cost, their_rows) = match measure(&sqlite, "sqlite.db") {
        Ok(cost) => cost,
        Err(why) => {
            eprintln!("{why}");
            return ExitCode::FAILURE;
        }
    };
    let (our_cost, our_rows) = match measure(&shell, "inillucent.rdb") {
        Ok(cost) => cost,
        Err(why) => {
            eprintln!("{why}");
            return ExitCode::FAILURE;
        }
    };
    println!("## peak resident set, one shell each, reading the same data");
    println!("  rows: sqlite3 {their_rows}, inillucent-shell {our_rows}");
    println!(
        "  {:<20} {:>10} {:>10} {:>10}",
        "shell", "peak MiB", "user ms", "kernel ms"
    );
    for (name, cost) in [("sqlite3", their_cost), ("inillucent-shell", our_cost)] {
        println!(
            "  {:<20} {:>10.2} {:>10.2} {:>10.2}",
            name,
            mebibytes(cost.peak_working_set),
            inillucent_compat::procstat::millis(cost.user_nanos),
            inillucent_compat::procstat::millis(cost.kernel_nanos)
        );
    }
    let ratio = our_cost.peak_working_set as f64 / their_cost.peak_working_set.max(1) as f64;
    println!("  inillucent holds {ratio:.2}x what sqlite3 holds");
    ExitCode::SUCCESS
}
