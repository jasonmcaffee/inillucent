//! Layer 4 of the SQL statement matrix: random statements over random small
//! schemas, from a seed that is the date, graded against the pinned SQLite.
//!
//! Invariant: **every random case runs on files on both engines, and a group
//! fails for every difference that no rule in `deliberate.toml` or
//! `random-known.toml` covers.** Each such failure is shrunk and written to
//! `_agent_output/matrix/random/<id>.slt` for a person to retain. The seed and
//! the case count are written to `_agent_output/nightly/matrix-random.txt`,
//! which `packaging/nightly.ps1` copies into the note column of
//! `tests/nightly-history.tsv`. `INILLUCENT_MATRIX_SEED` replays another night.
//! Section 5.4 of `tasks/task-2135-sql-statement-matrix-tdd.md` is the design.

use inillucent_compat::statement_matrix::random;

/// How many random cases a night runs, across all its groups and shards.
///
/// About 135 ms each at the default arm on the development machine, measured
/// on 2,000 cases, so 4,000 cost about nine minutes of one core.
const COUNT: u64 = 4_000;

/// The seed: `INILLUCENT_MATRIX_SEED`, or today's date as `YYYYMMDD`.
fn seed() -> u64 {
    std::env::var("INILLUCENT_MATRIX_SEED")
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or_else(|| random::seed_of_date(&random::today()))
}

/// Runs one group and fails once, naming every uncovered difference.
///
/// @param group - this test function's index
fn run(group: usize) {
    let seed = seed();
    let note = inillucent_compat::workspace_root().join("_agent_output/nightly/matrix-random.txt");
    if let Some(parent) = note.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&note, format!("seed={seed} cases={COUNT}\n"));
    let scratch = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("matrix-random");
    let report = random::run_group(seed, COUNT, group, 4, &scratch)
        .unwrap_or_else(|problem| panic!("{problem}"));
    if report.oracle_missing {
        inillucent_compat::differential::announce_skip();
    }
    println!(
        "random seed {seed} group {group}/4: {} case(s), {} covered by a rule, {} not covered",
        report.cases,
        report.expected,
        report.problems.len()
    );
    assert!(
        report.problems.is_empty(),
        "{}",
        report.problems.join("\n\n")
    );
}

#[test]
fn r0() {
    run(0);
}

#[test]
fn r1() {
    run(1);
}

#[test]
fn r2() {
    run(2);
}

#[test]
fn r3() {
    run(3);
}
