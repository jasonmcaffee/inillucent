//! The Layer 1 cases of every family through the surfaces an application uses
//! besides one statement on one connection, at the cadence of every merge.
//!
//! Invariant: **every Layer 1 case that gives the same answers twice runs
//! through `SharedDatabase`, one prepared statement run more than once, and
//! one `execute_batch` script, and each answer equals the driver's
//! `Connection`'s.** A difference `known.list` does not name fails its group,
//! and so does a `known.list` line for a surface that now agrees. The cases
//! and the comparison are in `inillucent_compat::statement_matrix::surfaces`;
//! section 5.5 of `tasks/task-2135-sql-statement-matrix-tdd.md` is the design.

/// Runs one group and fails once, naming every problem.
///
/// @param group - this test function's index
fn run(group: usize) {
    let scratch = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("matrix-surfaces")
        .join(format!("g{group}"));
    let (cases, problems) =
        inillucent_compat::statement_matrix::surfaces::run_group(group, 8, &scratch)
            .unwrap_or_else(|problem| panic!("{problem}"));
    println!(
        "surfaces group {group}/8: {cases} case(s), {} problem(s)",
        problems.len()
    );
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}

#[test]
fn g0() {
    run(0);
}

#[test]
fn g1() {
    run(1);
}

#[test]
fn g2() {
    run(2);
}

#[test]
fn g3() {
    run(3);
}

#[test]
fn g4() {
    run(4);
}

#[test]
fn g5() {
    run(5);
}

#[test]
fn g6() {
    run(6);
}

#[test]
fn g7() {
    run(7);
}
