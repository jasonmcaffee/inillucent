//! The gate programs fail when their prerequisite is absent, and measure when it is not.
//!
//! Invariant: **a gate that checked nothing exits non-zero and says what was
//! missing.** The programs under `crates/inillucent-compat/src/bin/` are 19,000
//! lines that decide pass or fail, and until this file none of them had a test
//! of its own. `tests/inillucent-testing-tdd.md` section 9's "; skipping"
//! convention exists because the differential suites once reported green with a
//! prerequisite absent; the `bin/` programs were never covered by it, because
//! they are the harness rather than `#[test]` functions.
//!
//! It is not a hypothetical class. Two shipped in one week:
//!
//! - `b467293` (task-1951) fixed a packaging gate that "said every check passed
//!   having checked nothing" when `rcodesign` was not installed.
//! - `c5e1471` (task-1913) fixed an oracle whose counters silently read zero.
//!
//! And this file found a third while it was being written: `searchgate` on a
//! corpus of no documents answered every query plausibly - `count(*)` reads `0`,
//! which is exactly the number of documents there are - timed an empty index and
//! exited zero. Its refusal, and the assertion that catches it, are below.
//!
//! ## Two cases per gate
//!
//! **Missing prerequisite.** The gate is run with the thing it measures against
//! absent - a fixture that is not there, a corpus of nothing, a scratch
//! directory it cannot create, a tier that does not exist. It has to exit
//! non-zero *and* name what was missing, because an exit code with no sentence
//! is a gate a person has to read the source of.
//!
//! **A real, smallest fixture.** The gate is run on
//! `_agent_output/fixtures/small.db` - the 1.2 MB fixture
//! `tools/build-gate-fixtures.sh` builds and `tools/validate` builds before the
//! suite - and its report has to carry a measurement greater than zero.
//!
//! **Why the second case does not assert exit code zero**, where the finding
//! that asked for this file said it should. A performance gate exits zero only
//! when it *meets its bars*, and the bars in `compat/perf/contract.toml` are
//! production numbers measured on a release build against the medium fixture.
//! A debug build over a 1.2 MB fixture misses them, correctly: `read.range`
//! reads 0.35x against a 3.00x bar. Asserting zero here would mean either
//! running the real gate - minutes per case, on a machine the suite does not own
//! - or lowering the bars to whatever a debug build happens to reach, which is
//! the "test that cannot fail" the testing standard's rule 1.5 bans.
//!
//! What separates "measured and missed" from "measured nothing" is the exit
//! code the gates already distinguish - **2 means it could not start, 0 and 1
//! mean it ran** - plus the report itself. So the second case asserts the exit
//! code is not 2 and that the report names a positive measurement. A gate that
//! printed a pass having measured nothing fails on the second half.
//!
//! ## A third case, for the paired gates: the two arms hold the same data
//!
//! Added by task-2029, which found a gate that started, ran, refused every
//! round and measured nothing - and did all of it correctly. The write gate
//! compares the state of the database at the end of each round, and its own arm
//! was skipping the `pre` statements `sqlite_bench.c` runs, so the two arms
//! ended every round holding different text and the gate rightly declined to
//! time them. Nothing above catches it: the gate exits 1 rather than 2, and the
//! `write` family it measures has no workload with a `pre`.
//!
//! So a paired gate also gets a case that runs a family whose workloads *do*
//! carry setup, and asserts the report says the arms agreed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use inillucent_compat::workspace_root;

/// The test runner, which a plain `cargo test` does not build.
const TESTRUN: &str = env!("CARGO_BIN_EXE_inillucent-testrun");

/// What builds it, for the message a skip prints.
const BUILDS_TESTRUN: &str =
    "cargo build -p inillucent-compat --bin inillucent-testrun --features testrun";

/// The fixture the gates read, built by `tools/build-gate-fixtures.sh`.
///
/// Not checked in - it is 1.2 MB and is built by the pinned SQLite shell so that
/// both arms of a comparison start from a file SQLite itself wrote.
fn small_fixture() -> Option<PathBuf> {
    let path = workspace_root().join("_agent_output/fixtures/small.db");
    path.is_file().then_some(path)
}

/// Returns the pinned benchmark driver, which the read and write gates need.
fn sqlite_bench_built() -> bool {
    let root = workspace_root().join(".sqlite-ref/3.53.4");
    ["sqlite-bench.exe", "sqlite-bench"]
        .iter()
        .any(|name| root.join(name).is_file())
}

/// Returns a private copy of the small fixture.
///
/// **A copy per run, because a gate leaves an index behind.** The
/// `schema.index` workload creates `main_label` on the SQLite arm, so a second
/// run against the same file stops on "index main_label already exists" -
/// which would be a gate failing for a reason that is not about the engine.
///
/// @param tag - what to name the copy after
fn fixture_copy(tag: &str) -> Option<PathBuf> {
    let source = small_fixture()?;
    let directory = std::env::temp_dir().join(format!(
        "inillucent-gates-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).ok()?;
    let target = directory.join("small.db");
    std::fs::copy(&source, &target).ok()?;
    Some(target)
}

/// Returns a gate's path, or nothing when this build did not produce it.
///
/// **`inillucent-testrun` is behind `required-features = ["testrun"]`, so a
/// plain `cargo test` does not build it.** `CARGO_BIN_EXE_inillucent-testrun`
/// still resolves - to a path in the target directory that may hold nothing -
/// and the three cases below passed on this machine only because an earlier
/// explicit build had left the debug binary there. Under
/// `cargo llvm-cov --release`, which builds into a target directory of its own,
/// they failed with "the system cannot find the file specified", which is what
/// found this.
///
/// A test that passes because of an artifact another command left behind is the
/// defect class this whole file is about, so it skips with the marker
/// `tests/inillucent-testing-tdd.md` section 9 requires and `--strict` counts.
///
/// The path is `&'static str` rather than generic on purpose: `policy.rs`'s
/// `every_early_return_in_a_test_says_why` reads a helper's name off its
/// signature to decide whether the helper announces for its caller, and a
/// lifetime parameter between the name and the arguments hides it.
///
/// @param path - what `CARGO_BIN_EXE_*` resolved to
/// @param how - the command that builds it, for the message
fn built(path: &'static str, how: &str) -> Option<&'static str> {
    if Path::new(path).is_file() {
        return Some(path);
    }
    inillucent_compat::differential::skipping(&format!("{path} is not built; run `{how}`"));
    None
}

/// Runs a gate and returns what it printed and how it exited.
///
/// @param gate - the built binary
/// @param arguments - the command line
fn run(gate: &str, arguments: &[&str]) -> Output {
    Command::new(gate)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("{gate} did not start: {error}"))
}

/// Runs a nested `inillucent-testrun` over the outer run's artifact list, with
/// no cargo it could start.
///
/// **The nested runner builds nothing (task-2114 C2).** It used to call
/// `locate`, which runs `cargo test --no-run`, and cargo then relinks whatever
/// is stale - including `inillucent-testrun.exe`, the outer runner's own
/// running image, and `inillucent-scorecard.exe`, which a sibling case here
/// executes. Windows refuses that, so the nested runs built a whole second
/// workspace under `CARGO_TARGET_TMPDIR`, which took minutes, and the target
/// ran alone at the end of every run for 36 minutes of 37.
///
/// The outer runner now writes the executables it located to an artifact list
/// and names it in `INILLUCENT_TESTRUN_ARTIFACTS`. The nested runner is given
/// that list with `--artifacts`, and `CARGO` is pointed at the runner itself,
/// which exits 2 for any cargo command line. So a nested case that started a
/// cargo would fail on it, and every case here proves on each run that none
/// did.
///
/// Under a plain `cargo test` the variable is absent and the case skips with
/// the marker, because there is no outer run to have located anything.
///
/// @param gate - the built runner
/// @param arguments - the command line
fn run_nested(gate: &str, arguments: &[&str]) -> Option<Output> {
    let Some(list) = std::env::var_os(inillucent_compat::testplan::ARTIFACTS_VARIABLE) else {
        inillucent_compat::differential::skipping(&no_artifact_list());
        return None;
    };
    Some(nested(gate, &list, arguments, None))
}

/// Runs a nested `inillucent-testrun` as [`run_nested`] does, reading a
/// declaration of absent prerequisites from a file of the case's own.
///
/// @param gate - the built runner
/// @param arguments - the command line
/// @param declaration - the `tests/prerequisites.local.toml` stand in
fn run_nested_declaring(gate: &str, arguments: &[&str], declaration: &Path) -> Option<Output> {
    let Some(list) = std::env::var_os(inillucent_compat::testplan::ARTIFACTS_VARIABLE) else {
        inillucent_compat::differential::skipping(&no_artifact_list());
        return None;
    };
    Some(nested(gate, &list, arguments, Some(declaration)))
}

/// The sentence a nested case skips with when no outer run is there.
fn no_artifact_list() -> String {
    format!(
        "{} is not set, so no outer inillucent-testrun located the executables a nested run \
         starts; run this through `inillucent-testrun`",
        inillucent_compat::testplan::ARTIFACTS_VARIABLE
    )
}

/// Starts the nested runner over an artifact list.
///
/// @param gate - the built runner
/// @param list - the artifact list the outer run wrote
/// @param arguments - the command line
/// @param declaration - the `tests/prerequisites.local.toml` stand in, if any
fn nested(
    gate: &str,
    list: &std::ffi::OsStr,
    arguments: &[&str],
    declaration: Option<&Path>,
) -> Output {
    let mut command = Command::new(gate);
    command
        .arg("--artifacts")
        .arg(list)
        .arg("--no-build")
        .args(arguments)
        .env("CARGO", gate)
        // No server URLs, so the two live database suites go without their
        // prerequisite on every machine, including a CI runner that starts
        // both servers. The red run and the declared absence case rely on
        // that; the other cases never select those suites.
        .env_remove("INILLUCENT_TEST_POSTGRES_URL")
        .env_remove("INILLUCENT_TEST_MYSQL_URL");
    // Set either way, so a declaration file in the checkout never changes
    // what a case here measures: with no declaration of its own, a case reads
    // a file that is not there, which declares nothing.
    let nothing = std::env::temp_dir().join("inillucent-declares-nothing.toml");
    command.env(
        inillucent_compat::testplan::DECLARED_ABSENCES_VARIABLE,
        declaration.unwrap_or(&nothing),
    );
    command
        .output()
        .unwrap_or_else(|error| panic!("{gate} did not start: {error}"))
}

/// Returns everything a run printed, both streams together.
///
/// A gate says what is missing on standard error and reports on standard
/// output, and which one a particular message went to is not the thing under
/// test.
///
/// @param output - what the run produced
fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Returns a run's exit code, or 130 for a run a signal ended.
///
/// @param output - what the run produced
fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(130)
}

/// Asserts a gate refused, and named what was missing.
///
/// @param gate - the gate's name, for the message
/// @param output - what the run produced
/// @param naming - a word the message has to carry
fn refused(gate: &str, output: &Output, naming: &[&str]) {
    let text = said(output);
    assert_ne!(
        code(output),
        0,
        "{gate} exited zero with its prerequisite absent. It printed:\n{text}"
    );
    let folded = text.to_lowercase();
    assert!(
        naming.iter().any(|word| folded.contains(word)),
        "{gate} refused without naming what was missing - none of {naming:?} is in its \
         message:\n{text}"
    );
}

/// Asserts a gate ran and measured something.
///
/// @param gate - the gate's name, for the message
/// @param output - what the run produced
/// @param counted - a line the report has to carry a positive number on
fn measured(gate: &str, output: &Output, counted: &dyn Fn(&str) -> bool) {
    let text = said(output);
    assert_ne!(
        code(output),
        2,
        "{gate} could not start on the smallest real fixture. It printed:\n{text}"
    );
    assert!(
        counted(&text),
        "{gate} reported no measurement greater than zero, so it graded nothing. It \
         printed:\n{text}"
    );
}

/// Reports whether any line of a report carries a number above zero.
///
/// Deliberately crude: what it is asking is "did this program put a measurement
/// on the page", and a gate that measured nothing prints zeros or prints
/// nothing.
///
/// @param text - the report
/// @param marker - a word the measured lines carry
fn a_positive_number_on_a_line_with(text: &str, marker: &str) -> bool {
    text.lines()
        .filter(|line| line.contains(marker))
        .any(|line| {
            line.split(|c: char| !c.is_ascii_digit() && c != '.')
                .filter_map(|word| word.parse::<f64>().ok())
                .any(|number| number > 0.0)
        })
}

// --- readgate ---------------------------------------------------------------

/// The read gate refuses a fixture that is not there.
#[test]
fn readgate_refuses_a_fixture_that_is_not_there() {
    if !sqlite_bench_built() {
        inillucent_compat::differential::skipping(
            "sqlite-bench is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    }
    let missing = std::env::temp_dir().join("inillucent-gates-no-such-fixture.db");
    let _ = std::fs::remove_file(&missing);
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-readgate"),
        &[
            &missing.to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "1",
        ],
    );
    refused("readgate", &output, &["import", "fixture", "no such file"]);
}

/// The read gate refuses a family selection that names no workload.
///
/// The second way it can end up grading nothing: every workload filtered out,
/// a plan of no rows, and a table of no measurements to print a verdict from.
#[test]
fn readgate_refuses_a_family_that_selects_no_workload() {
    let Some(fixture) = fixture_copy("readgate-families") else {
        inillucent_compat::differential::skipping(
            "the small gate fixture is not built; run tools/build-gate-fixtures.sh",
        );
        return;
    };
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-readgate"),
        &[
            &fixture.to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "1",
            "--families",
            "read.nothing",
        ],
    );
    refused("readgate", &output, &["workload", "famil"]);
}

/// The read gate measures the smallest real fixture.
#[test]
fn readgate_measures_the_small_fixture() {
    if !sqlite_bench_built() {
        inillucent_compat::differential::skipping(
            "sqlite-bench is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    }
    let Some(fixture) = fixture_copy("readgate-small") else {
        inillucent_compat::differential::skipping(
            "the small gate fixture is not built; run tools/build-gate-fixtures.sh",
        );
        return;
    };
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-readgate"),
        &[
            &fixture.to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "1",
            "--families",
            "read.point",
        ],
    );
    measured("readgate", &output, &|text| {
        a_positive_number_on_a_line_with(text, "point.rowid")
    });
}

// --- writegate --------------------------------------------------------------

/// The write gate refuses a fixture that is not there.
#[test]
fn writegate_refuses_a_fixture_that_is_not_there() {
    let missing = std::env::temp_dir().join("inillucent-gates-no-such-write-fixture.db");
    let _ = std::fs::remove_file(&missing);
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-writegate"),
        &[
            &missing.to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "1",
        ],
    );
    refused(
        "writegate",
        &output,
        &["import", "fixture", "no such file", "sqlite-bench"],
    );
}

/// The write gate's two arms end a round holding the same data.
///
/// **The case task-2029 needed and the one above does not provide.** A gate
/// that starts, runs, refuses every round and measures nothing is what this
/// one had become: `txn.batched` and `txn.large` carry a `pre` of
/// `UPDATE side_table SET note = 'note ' || id`, `sqlite_bench.c` runs a
/// workload's `pre` before it starts its clock, and `writegate.rs` did not run
/// it at all. Every round then ended with `sum(length(note))` reading 261865 on
/// our arm against 258445 on SQLite's, the agreement check refused the round,
/// and the `write` and `transaction` families had no ratio from this gate at
/// all - on any fixture, on any machine, since `aaa0d0c` put the `pre` in
/// `perf.rs` on 2026-09-09.
///
/// The case above runs `--families write`, where no workload has a `pre`, so it
/// passed throughout. This one runs the whole plan, and **both families are
/// needed to see the defect**: the `transaction` workloads alone cannot show
/// it, because all three bind `Bind::Scatter` and `Bind::Text` over the same
/// iteration range, so `txn.autocommit`'s hundred rows are a subset of
/// `txn.batched`'s two thousand carrying identical text, and resetting them or
/// not ends in the same place. What diverges is the hundred rows
/// `write.insert.autocommit` adds with `row {i} lorem ipsum ...` in `note`:
/// SQLite's `pre` rewrites those to `'note ' || id` and no later workload binds
/// their ids, so the arm that skipped the `pre` keeps the longer text.
///
/// The assertion is about what the report says of the data rather than of the
/// clock: a round that disagreed prints `NO:` and the disagreement, and a round
/// that agreed prints `yes`. It says nothing about the ratios - a debug build
/// over a 1.2 MB fixture misses the bars, correctly, for the reason this file's
/// header gives, so asserting the exit code or the verdict here would be
/// asserting the build profile.
#[test]
fn writegate_arms_agree_on_a_round() {
    if !sqlite_bench_built() {
        inillucent_compat::differential::skipping(
            "sqlite-bench is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    }
    let Some(fixture) = fixture_copy("writegate-agreement") else {
        inillucent_compat::differential::skipping(
            "the small gate fixture is not built; run tools/build-gate-fixtures.sh",
        );
        return;
    };
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-writegate"),
        &[
            &fixture.to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "1",
        ],
    );
    let text = said(&output);
    assert_ne!(
        code(&output),
        2,
        "writegate could not start on the smallest real fixture. It printed:\n{text}"
    );
    assert!(
        !text.contains("NO:"),
        "writegate's two arms ended a round holding different data, so it timed nothing. \
         Check that both arms run each workload's `pre`. It printed:\n{text}"
    );
    assert!(
        text.contains("txn.large") && text.contains("  yes"),
        "writegate's report named no workload whose arms agreed, so the check above passed \
         by finding nothing to disagree. It printed:\n{text}"
    );
}

/// The write gate measures the smallest real fixture.
#[test]
fn writegate_measures_the_small_fixture() {
    if !sqlite_bench_built() {
        inillucent_compat::differential::skipping(
            "sqlite-bench is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    }
    let Some(fixture) = fixture_copy("writegate-small") else {
        inillucent_compat::differential::skipping(
            "the small gate fixture is not built; run tools/build-gate-fixtures.sh",
        );
        return;
    };
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-writegate"),
        &[
            &fixture.to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "1",
            "--families",
            "write",
        ],
    );
    measured("writegate", &output, &|text| {
        a_positive_number_on_a_line_with(text, "write.")
    });
}

// --- fullgate ---------------------------------------------------------------

/// The full gate refuses a fixture that is not there.
#[test]
fn fullgate_refuses_a_fixture_that_is_not_there() {
    let missing = std::env::temp_dir().join("inillucent-gates-no-such-full-fixture.db");
    let _ = std::fs::remove_file(&missing);
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-fullgate"),
        &[
            &missing.to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "1",
        ],
    );
    refused(
        "fullgate",
        &output,
        &["import", "fixture", "no such file", "sqlite-bench"],
    );
}

/// The full gate measures the smallest real fixture.
#[test]
fn fullgate_measures_the_small_fixture() {
    if !sqlite_bench_built() {
        inillucent_compat::differential::skipping(
            "sqlite-bench is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    }
    let Some(fixture) = fixture_copy("fullgate-small") else {
        inillucent_compat::differential::skipping(
            "the small gate fixture is not built; run tools/build-gate-fixtures.sh",
        );
        return;
    };
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-fullgate"),
        &[
            &fixture.to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "1",
            "--families",
            "read.point",
        ],
    );
    measured("fullgate", &output, &|text| {
        a_positive_number_on_a_line_with(text, "point.rowid")
    });
}

/// The full gate's arm runs in a fresh child each round and writes every round's
/// raw times, when asked (task-2095).
///
/// **Both options exist to take numbers, so the test reads the numbers.** A child
/// round that ran nothing would still let the gate exit, with an empty sample list
/// and every workload unpaired. What this checks is that each of the two rounds
/// wrote a positive time for `point.rowid` on both arms, and that the child's own
/// process accounting was read: a round of the plan faults in pages, so a fault
/// count of zero means the cost was never taken from the child.
#[test]
fn fullgate_times_its_arm_in_a_fresh_child_and_writes_the_samples() {
    if !sqlite_bench_built() {
        inillucent_compat::differential::skipping(
            "sqlite-bench is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    }
    let Some(fixture) = fixture_copy("fullgate-child") else {
        inillucent_compat::differential::skipping(
            "the small gate fixture is not built; run tools/build-gate-fixtures.sh",
        );
        return;
    };
    let samples = fixture.with_file_name("samples.tsv");
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-fullgate"),
        &[
            &fixture.to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "2",
            "--families",
            "read.point",
            "--engine-child",
            "--samples",
            &samples.to_string_lossy(),
        ],
    );
    let text = said(&output);
    assert_ne!(
        code(&output),
        2,
        "the gate did not run. It printed:\n{text}"
    );
    assert!(
        text.contains("a fresh child process per round"),
        "the configuration does not say the arm ran in a child. It printed:\n{text}"
    );
    let written = std::fs::read_to_string(&samples).unwrap_or_default();
    for round in ["0", "1"] {
        for arm in ["ours", "theirs"] {
            let nanos: Vec<f64> = written
                .lines()
                .map(|line| line.split('\t').collect::<Vec<&str>>())
                .filter(|fields| {
                    fields.len() == 5
                        && fields.first() == Some(&round)
                        && fields.get(2) == Some(&arm)
                        && fields.get(3) == Some(&"point.rowid")
                })
                .filter_map(|fields| fields.get(4).and_then(|value| value.parse().ok()))
                .collect();
            assert!(
                nanos.len() == 1 && nanos.first().is_some_and(|value| *value > 0.0),
                "round {round}, {arm}: expected one positive point.rowid time, found {nanos:?} \
                 in:\n{written}"
            );
        }
    }
    let faults: Vec<u64> = written
        .lines()
        .filter(|line| line.contains("\tours\t(cost)\t"))
        .filter_map(|line| line.split("faults ").nth(1))
        .filter_map(|rest| rest.split(' ').next())
        .filter_map(|count| count.parse().ok())
        .collect();
    assert!(
        faults.len() == 2 && faults.iter().all(|count| *count > 0),
        "expected a page fault count above zero for each child round, found {faults:?} \
         in:\n{written}"
    );
}

// --- prepareperf ------------------------------------------------------------

/// The prepare report refuses a fixture that is not there.
#[test]
fn prepareperf_refuses_a_fixture_that_is_not_there() {
    let missing = std::env::temp_dir().join("inillucent-gates-no-such-prepare-fixture.db");
    let _ = std::fs::remove_file(&missing);
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-prepareperf"),
        &[&missing.to_string_lossy(), "1", "--repeat", "8"],
    );
    refused(
        "prepareperf",
        &output,
        &["fixture", "sqlite-bench", "no such file"],
    );
}

/// The prepare report runs to completion on the smallest real fixture.
///
/// **It could not run on any file at all until task-2041.** It handed one path
/// to both arms: `Database::open` refuses a SQLite `.db`, because this engine
/// writes its own format, and `sqlite-bench` cannot read a `.rdb`. So a `.db`
/// failed the native arm and a `.rdb` failed the reference, no argument
/// satisfied both, and nothing in `tests/selection.toml` ran the binary - which
/// is how it stayed that way. It now imports the fixture for the native arms
/// and gives the original to `sqlite-bench`, and this case is what says so.
///
/// The measurement asserted on is `prepare.point`, because that is the workload
/// that reads a row: `prepare.trivial` is `SELECT 1` and would time on a fixture
/// with nothing in it.
#[test]
fn prepareperf_measures_the_small_fixture() {
    if !sqlite_bench_built() {
        inillucent_compat::differential::skipping(
            "sqlite-bench is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    }
    let Some(fixture) = fixture_copy("prepareperf-small") else {
        inillucent_compat::differential::skipping(
            "the small gate fixture is not built; run tools/build-gate-fixtures.sh",
        );
        return;
    };
    // One round of fifty prepares rather than the thirty rounds of four
    // thousand the published number is taken over: this case is asking whether
    // the program runs to completion and reports, and a debug build of the
    // engine misses the bars either way.
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-prepareperf"),
        &[&fixture.to_string_lossy(), "1", "--repeat", "50"],
    );
    measured("prepareperf", &output, &|text| {
        a_positive_number_on_a_line_with(text, "prepare.point")
    });
    // The three arms agree on what the query answered, or `record` stops the
    // run. A report that reached the table has already passed that check, so
    // the assertion here is that the table is the thing that was printed.
    let text = said(&output);
    assert!(
        text.contains("open.prepare with the cache:"),
        "prepareperf measured prepare.point and then did not report a family ratio, so it \
         stopped between the two. It printed:\n{text}"
    );
}

// --- searchgate -------------------------------------------------------------

/// The search gate refuses a corpus of no documents.
///
/// **This is the defect this file found (task-1961, T1).** With `--documents 0`
/// every query was plausible - `count(*)` answered `0`, which is exactly the
/// number of documents there are, and every other query answered no rows, which
/// is not more rows than the corpus holds - so the gate timed an index with
/// nothing in it and exited zero. A report of "one pass of every query: 0.004
/// ms" over an empty corpus is the fastest engine anybody has ever measured.
#[test]
fn searchgate_refuses_an_empty_corpus() {
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-searchgate"),
        &["--documents", "0", "--rounds", "1"],
    );
    refused("searchgate", &output, &["document", "corpus", "empty"]);
}

/// The search gate measures a corpus it built.
#[test]
fn searchgate_measures_a_small_corpus() {
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-searchgate"),
        &["--documents", "40", "--rounds", "2"],
    );
    measured("searchgate", &output, &|text| {
        a_positive_number_on_a_line_with(text, "one pass of every query")
    });
}

// --- walperf ----------------------------------------------------------------

/// The log gate refuses a scratch directory it cannot create.
///
/// A path *inside a file* rather than a made-up name, because a made-up name is
/// one the gate would happily create.
#[test]
fn walperf_refuses_a_scratch_it_cannot_create() {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-gates-walperf-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let blocker = directory.join("not-a-directory");
    std::fs::write(&blocker, b"this is a file").expect("the blocking file is written");
    let inside = blocker.join("scratch");
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-walperf"),
        &[
            "--scratch",
            &inside.to_string_lossy(),
            "--out",
            &directory.to_string_lossy(),
        ],
    );
    refused("walperf", &output, &["cannot create", "scratch"]);
}

/// The log gate measures a scratch directory it can create.
///
/// **The gate that had one case (task-1969, 4.3).** `walperf` had a refusal and
/// no measuring twin, so a build where every timing read zero would have passed
/// the only test it had. The assertion is on the report's own commit timings
/// rather than the exit code, for the reason the header gives: a debug build
/// misses production bars correctly, and what separates "measured and missed"
/// from "measured nothing" is whether a number reached the page.
#[test]
fn walperf_measures_the_small_fixture() {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-gates-walperf-measures-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let scratch = directory.join("scratch");
    let out = directory.join("out");
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-walperf"),
        &[
            "--scratch",
            &scratch.to_string_lossy(),
            "--out",
            &out.to_string_lossy(),
        ],
    );
    // The gate prints one line - the path it wrote - and puts every number in
    // the report, so the report is what the measurement has to be read out of.
    // Asserting on the one stdout line would pass on a run that wrote a table
    // of zeros, which is the shape this file exists to refuse.
    let report = out.join("phase10-concurrency-baselines.md");
    let written = std::fs::read_to_string(&report).unwrap_or_default();
    measured("walperf", &output, &|text| {
        a_positive_number_on_a_line_with(&format!("{text}\n{written}"), "commit")
    });
    let _ = std::fs::remove_dir_all(&directory);
}

// --- scorecard --------------------------------------------------------------

/// The scorecard refuses a lever name it does not know.
///
/// The gate's own argument says why this is a refusal and not an ignored word:
/// a typo that silently measured the shipped engine and labelled the result an
/// arm is worse than no arm at all.
#[test]
fn scorecard_refuses_a_lever_it_does_not_know() {
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-scorecard"),
        &[
            "--disable",
            "no-such-lever",
            "--scale",
            "small",
            "--rounds",
            "1",
        ],
    );
    refused("scorecard", &output, &["unknown lever", "lever"]);
}

/// The scorecard refuses to report when the benchmark driver is not built.
///
/// Its whole output is a comparison, so a run with nothing to compare against
/// has to say so rather than publish one arm.
#[test]
fn scorecard_says_when_it_cannot_measure() {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-gates-scorecard-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    let blocker = directory.join("not-a-directory");
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    std::fs::write(&blocker, b"this is a file").expect("the blocking file is written");
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-scorecard"),
        &[
            "--out",
            &blocker.join("out").to_string_lossy(),
            "--scale",
            "small",
            "--rounds",
            "1",
        ],
    );
    refused(
        "scorecard",
        &output,
        &["cannot", "could not", "not built", "failed"],
    );
}

/// The scorecard measures a lever it knows.
///
/// **The gate whose only two cases were both refusals (task-1969, 4.3).** A
/// scorecard that produced a table of zeros satisfied everything this file
/// asked of it. The measurement is read out of `scorecard.md` rather than out
/// of the summary line, because the summary quotes a headline ratio that a run
/// measuring nothing would still print.
#[test]
fn scorecard_measures_a_lever_it_knows() {
    if !sqlite_bench_built() {
        inillucent_compat::differential::skipping(
            "sqlite-bench is not built; run tools/sqlite-reference.{ps1,sh}",
        );
        return;
    }
    let directory = std::env::temp_dir().join(format!(
        "inillucent-gates-scorecard-measures-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-scorecard"),
        &[
            "--out",
            &directory.to_string_lossy(),
            "--disable",
            "covering-index",
            "--scale",
            "small",
            "--rounds",
            "1",
        ],
    );
    let card = std::fs::read_to_string(directory.join("scorecard.md")).unwrap_or_default();
    measured("scorecard", &output, &|text| {
        a_positive_number_on_a_line_with(&format!("{text}\n{card}"), "ms")
    });
    let _ = std::fs::remove_dir_all(&directory);
}

// --- foldgate ---------------------------------------------------------------

/// The fold gate refuses an arm it does not have.
///
/// `foldgate` is one of the two programs under `src/bin/` that decide pass or
/// fail and had no test at all (task-1969, 4.3); it is the roadmap's M8
/// measurement, so a run of it that graded nothing would be published as the
/// evidence for a closed roadmap item.
#[test]
fn foldgate_refuses_an_arm_it_does_not_have() {
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-foldgate"),
        &["--arm", "no-such-arm", "--documents", "8", "--dims", "4"],
    );
    refused("foldgate", &output, &["no such arm", "arm"]);
}

/// The fold gate measures one arm on a corpus it builds.
///
/// One arm rather than the comparison, because the comparison spawns both arms
/// as child processes and each builds its own corpus; what is under test here
/// is that an arm reports a count it actually wrote, which is the half a report
/// of zeros would fail.
///
/// `inserted_total` rather than the commit timings: a run that wrote the corpus
/// and folded nothing prints a hundred and twenty commit times and zero
/// insertions, which is exactly the shape of a gate that measured the wrong
/// thing and said a number.
#[test]
fn foldgate_measures_a_small_corpus() {
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-foldgate"),
        &[
            "--arm",
            "fold",
            "--documents",
            "120",
            "--queries",
            "4",
            "--dims",
            "8",
            // **`--compact` is what makes 120 documents enough.** The default
            // delta log length is `max(1024, rows / 8)`, so a corpus this small
            // never reaches a fold and the arm reports `inserted_total = 0` -
            // correctly, and with nothing for this case to measure. Pinning the
            // log at 16 puts seven folds inside a run that takes six seconds.
            "--compact",
            "16",
        ],
    );
    measured("foldgate", &output, &|text| {
        a_positive_number_on_a_line_with(text, "inserted_total")
    });
}

// --- release ----------------------------------------------------------------

/// The release gate refuses a candidate directory it cannot create.
///
/// The other of the two deciding programs with no test (task-1969, 4.3). It
/// returns `ExitCode::FAILURE` from two places and nothing exercised either.
#[test]
fn release_refuses_a_candidate_directory_it_cannot_create() {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-gates-release-refuses-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let blocker = directory.join("not-a-directory");
    std::fs::write(&blocker, b"this is a file").expect("the blocking file is written");
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-release"),
        &["--out", &blocker.join("candidate").to_string_lossy()],
    );
    refused("release", &output, &["cannot create", "cannot"]);
    let _ = std::fs::remove_dir_all(&directory);
}

/// The release gate digests the measurement files it was pointed at.
///
/// A planted scorecard rather than the repository's own, so the case measures
/// something whose bytes it chose: the report has to carry that file's name, a
/// SHA-256 of the right length, and its byte count. A gate that wrote a
/// candidate naming no artifact at all is the failure this catches, and it is
/// the one `packaging/sign-sums.ps1` shipped in task-1951.
#[test]
fn release_measures_the_artifacts_it_was_given() {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-gates-release-measures-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    let measurements = directory.join("measurements");
    let out = directory.join("candidate");
    std::fs::create_dir_all(&measurements).expect("a measurements directory");
    let planted = b"# scorecard\n\nthis file exists so the gate has something to digest\n";
    std::fs::write(measurements.join("scorecard.md"), planted).expect("the scorecard is planted");
    let output = run(
        env!("CARGO_BIN_EXE_inillucent-release"),
        &[
            "--out",
            &out.to_string_lossy(),
            "--measurements",
            &measurements.to_string_lossy(),
        ],
    );
    let report = std::fs::read_to_string(out.join("release.md")).unwrap_or_default();
    measured("release", &output, &|text| {
        let both = format!("{text}\n{report}");
        both.contains("scorecard.md")
            && a_positive_number_on_a_line_with(&both, "scorecard.md")
            // The digest is a table cell and the page writes it in backticks,
            // so the whitespace-split word is 66 characters rather than 64.
            && report.split_whitespace().any(|word| {
                let bare = word.trim_matches('`');
                bare.len() == 64 && bare.chars().all(|c| c.is_ascii_hexdigit())
            })
    });
    let _ = std::fs::remove_dir_all(&directory);
}

// --- testrun ----------------------------------------------------------------

/// The runner refuses a tier that is not in the map.
///
/// The runner is the one gate whose failure hides every other test, so a tier
/// name it does not recognise selecting nothing and reporting success is the
/// worst shape of this defect in the tree.
#[test]
fn testrun_refuses_a_tier_that_does_not_exist() {
    let Some(gate) = built(TESTRUN, BUILDS_TESTRUN) else {
        return;
    };
    let output = run(gate, &["--tier", "no-such-tier", "--list"]);
    refused("testrun", &output, &["tier", "no-such-tier"]);
}

/// The runner lists the smoke tier, and the tier holds targets.
///
/// Named `_measures_` because what it measures is the size of the selection.
/// The runner's "nothing was checked" failure is a tier that resolves to an
/// empty set: every suite in it then reports ok, because there were none.
///
/// `--list` rather than a run, because the run is what every other suite in
/// this file is already inside: what is asserted is that the selection is not
/// empty, which is the thing that would make a green run meaningless.
#[test]
fn testrun_measures_the_smoke_tier_selection() {
    let Some(gate) = built(TESTRUN, BUILDS_TESTRUN) else {
        return;
    };
    let output = run(gate, &["--tier", "smoke", "--list"]);
    let text = said(&output);
    assert_eq!(
        code(&output),
        0,
        "the runner could not list the smoke tier:\n{text}"
    );
    let selected = text
        .lines()
        .filter(|line| line.trim_start().starts_with("smoke"))
        .count();
    assert!(
        selected > 0,
        "the smoke tier selected no target, so a green smoke run grades nothing:\n{text}"
    );
}

// --- testrun's three exit codes (task-2047) ----------------------------------
//
// The runner answers 0, 1 or 2, and the five cases below are one contract in
// five parts rather than five tests that happen to be next to each other.
//
// - `0` - every selected target ran and passed.
// - `1` - the run happened and was red.
// - `2` - the run did not happen, so nothing was graded.
//
// **Why it takes five cases and not one.** The obvious test - "a broken build
// exits non-zero" - passes against a runner that exits non-zero for everything,
// which is a worse program than the one being fixed. Each case here asserts an
// *exact* code, and the set is arranged so that every degenerate runner fails at
// least one of them: one that always answers 0 fails the three refusals, one
// that always answers 1 fails all five, one that always answers 2 fails the
// green run, and one that answers 2 for a failing test fails the red run. That
// last pair is the distinction the whole change is for, and neither case can
// hold it alone.
//
// **Four of them start a real nested run, and a nested run starts no cargo.**
// `locate` asks cargo what it built and cargo answers by building, so on a
// stale workspace it relinks, and Windows will not replace an image that is
// running. `testrun_passes_a_real_run_with_code_zero` failed exactly that way
// while `scorecard_measures_a_lever_it_knows`, a case in this same binary, was
// executing `inillucent-scorecard.exe`:
//
//     error: failed to remove file `.../debug/inillucent-scorecard.exe`
//     Caused by: Access is denied. (os error 5)
//     inillucent-testrun: cargo could not list the built targets
//
// The nested runs then built in a directory of their own, which cost a whole
// second workspace build. They now read the outer run's artifact list with
// `--artifacts` and have `CARGO` pointed at a program that refuses every cargo
// command line, so none of them can reach cargo at all. `run_nested` says more.
//
// `testrun_measures_the_smoke_tier_selection` above asserts `code == 0` on a
// `--list`, which needs no cargo at all - so even under a plain `cargo test`,
// where the nested cases skip, "the runner refuses everything" is still
// falsified.
//
// **Why `1` is asserted on a run that went red rather than on a failing test.**
// This repository has no test that fails, so a case wanting one would have to
// add a test that exists to fail - a test that cannot pass is the mirror image
// of `tests/inillucent-testing-tdd.md` rule 1.5, and it would then fail every
// ordinary run of the suite it lived in. `--strict` over a suite whose
// prerequisite is absent is a real red run already in the tree: the targets
// start, their tests execute, and the run ends red. What it holds is that a run
// which *happened* does not answer 2. What it does not hold is the failing-test
// path specifically, and that is said here rather than left for a reader to
// work out.

/// A build that does not complete exits 2, not 1 and not 0.
///
/// **This is the ticket (task-2047).** `the build failed` used to exit 1, the
/// same code a failing test exits, so nothing reading the status could tell a
/// broken toolchain from a real defect - and an agent read one as the other and
/// had to go back through 60 KB of log.
///
/// The build is failed through `CARGO`, which `testrun::cargo` reads: any
/// program that exits non-zero for cargo's arguments is a cargo that cannot
/// build, and the runner itself is one - given `test --workspace ...` it
/// refuses on `unknown option`. That makes the case instant, independent of
/// which compiler is installed, and it mutates nothing, where the real failure
/// needs an absent MSVC environment or a source file that does not compile.
#[test]
fn testrun_exits_two_when_the_build_does_not_complete() {
    let Some(gate) = built(TESTRUN, BUILDS_TESTRUN) else {
        return;
    };
    let output = run_with_cargo(gate, &["--tier", "smoke"], gate);
    let text = said(&output);
    assert_eq!(
        code(&output),
        2,
        "a build that did not complete has to exit 2, so a caller can tell it from a test \
         that failed. It printed:\n{text}"
    );
    assert!(
        text.contains("the build failed"),
        "the runner refused without saying the build failed, so the code is the only \
         evidence:\n{text}"
    );
}

/// A selection whose names do not overlap is refused, not reported green.
///
/// `--target inillucent-compat::engine::dml --tier smoke` names a real target and a
/// real tier, so neither of the two guards above fires - they catch a name the
/// map does not hold at all - and the intersection is empty. The runner printed
/// `nothing selected` and exited 0, which is rule 1.5's shape exactly: a gate
/// that graded nothing reading as a pass. Two real names are easier to type
/// than one wrong one, so this was the reachable half of the defect.
///
/// The two names are read off the map rather than typed here, because a case
/// that hard-codes them starts passing for the wrong reason the day one is
/// renamed - it would be asserting that a name is unknown.
#[test]
fn testrun_refuses_a_selection_whose_names_do_not_overlap() {
    let Some(gate) = built(TESTRUN, BUILDS_TESTRUN) else {
        return;
    };
    let listed = run(gate, &["--list"]);
    let Some(outside) = a_target_outside_the_smoke_tier(&said(&listed)) else {
        inillucent_compat::differential::skipping(
            "`--list` named no target outside the smoke tier, so there is no pair of real \
             names that cannot overlap",
        );
        return;
    };
    // Nested too, although this one refuses its empty selection before it
    // builds anything: a runner spawned from inside a run should never be able
    // to reach the target directory the outer one is executing from, and a rule
    // with an exception is one somebody has to re-derive.
    let Some(output) = run_nested(gate, &["--target", &outside, "--tier", "smoke"]) else {
        return;
    };
    let text = said(&output);
    assert_eq!(
        code(&output),
        2,
        "`--target {outside} --tier smoke` selected nothing and did not refuse, so a request \
         that could not be honoured reads as a pass. It printed:\n{text}"
    );
    assert!(
        text.contains(&outside) && text.contains("smoke"),
        "the runner refused without naming what was asked for, so a reader cannot tell which \
         of the two was wrong:\n{text}"
    );
}

/// A filter that matches no test is refused, and the report agrees with it.
///
/// `--filter` takes free text, so a typo in a test name reaches it on the first
/// try. Every selected binary really ran, libtest matched nothing, and the
/// summary read `0 test(s), 0 failed, 0 undetermined` under the word `ok`.
///
/// The second assertion is not decoration. `report`'s own comment - written
/// when `--strict` was printing `ok` over a run about to exit 1 - says the
/// report and the exit status have to agree or one of the two stops being read,
/// so a refusal printed under `ok` would be half the defect still shipping.
#[test]
fn testrun_refuses_a_filter_that_matches_no_test() {
    let Some(gate) = built(TESTRUN, BUILDS_TESTRUN) else {
        return;
    };
    // **Nested, like every other case here that runs targets (task-2106).**
    // `--no-build` alone still ran `locate`, which is `cargo test --no-run`, in
    // the target directory the outer run executes from, and a relink there
    // failed with `Access is denied`. `run_nested` starts no cargo at all.
    let arguments = [
        "--tier",
        "smoke",
        "--filter",
        "no_test_is_called_this_task_2047",
    ];
    let Some(output) = run_nested(gate, &arguments) else {
        return;
    };
    let text = said(&output);
    refuse_a_contended_run("a filter that matches no test", &text);
    assert_eq!(
        code(&output),
        2,
        "a run that graded no test has to refuse: every binary started and nothing was \
         measured. It printed:\n{text}"
    );
    assert!(
        !text.lines().any(|line| line.trim() == "ok"),
        "the report said `ok` over a run that exited 2, so the report and the exit status \
         disagree:\n{text}"
    );
}

/// A run that really ran and really passed exits 0.
///
/// The falsifier for the three refusals above: without it they all pass against
/// a runner that refuses everything, which is the failure this change could
/// plausibly introduce and the one a reviewer would not see.
///
/// `--tier smoke` is the tier that exists to be the cheap real answer: one
/// target, a file opened, written, reopened and read.
///
/// `--no-build` because this test is itself inside a run that has already built
/// the workspace, and the build step is nearly all of the cost - 38 seconds
/// against 4.9 measured on a warm tree. It is not a way of skipping the
/// compiler: `locate` asks cargo what it built and cargo answers by building,
/// so a test binary that is missing is still compiled before it runs.
#[test]
fn testrun_passes_a_real_run_with_code_zero() {
    let Some(gate) = built(TESTRUN, BUILDS_TESTRUN) else {
        return;
    };
    let Some(output) = run_nested(gate, &["--tier", "smoke"]) else {
        return;
    };
    let text = said(&output);
    refuse_a_contended_run("a real run that passes", &text);
    assert_eq!(
        code(&output),
        0,
        "the smoke tier did not pass, so every refusal asserted above could be a runner that \
         refuses everything. It printed:\n{text}"
    );
    assert!(
        graded_tests(&text).is_some_and(|ran| ran > 0),
        "the smoke tier exited 0 having graded no test, which is the pass this file exists to \
         disbelieve:\n{text}"
    );
}

/// A run that happened and went red exits 1, so it is not a run that did not happen.
///
/// The falsifier for the other direction: a runner that answered 2 for
/// everything red would satisfy every case above, and it would be the same
/// defect pointing the other way - a real failing suite reported as a build
/// that never started.
///
/// `--strict` over the two suites that need a database server is a red run
/// already in the tree. They run, their tests execute and report nothing, and
/// `--strict` turns that into a failure. A machine with both servers running
/// gets a green and the case skips, because a prerequisite that is present is
/// not a reason to assert the opposite.
#[test]
fn testrun_exits_one_when_the_run_went_red() {
    let Some(gate) = built(TESTRUN, BUILDS_TESTRUN) else {
        return;
    };
    let arguments = [
        "--strict",
        "--target",
        "inillucent-remote::live_postgres",
        "--target",
        "inillucent-remote::live_mysql",
    ];
    let Some(output) = run_nested(gate, &arguments) else {
        return;
    };
    let text = said(&output);
    refuse_a_contended_run("a real run that goes red", &text);
    assert!(
        text.contains("ran without a prerequisite"),
        "the nested run was given no server URL and still did not report the two suites as \
         missing their prerequisite. It printed:\n{text}"
    );
    assert_eq!(
        code(&output),
        1,
        "a run whose suites started and evidenced nothing has to exit 1: 2 is reserved for a \
         run that did not happen. It printed:\n{text}"
    );
}

/// A strict run passes when every missing prerequisite is declared absent, and
/// fails when one is not.
///
/// The same two suites as the red run above, which have no database server
/// here. Declaring both makes the run green and lists them under the
/// declaration heading. Declaring one of the two leaves the other a hollow
/// suite, and the run exits 1 as before. The nested runs get no server URL,
/// so this holds on a machine with both servers running too.
#[test]
fn testrun_excuses_a_declared_absence_and_nothing_else() {
    let Some(gate) = built(TESTRUN, BUILDS_TESTRUN) else {
        return;
    };
    let directory = std::env::temp_dir().join(format!(
        "inillucent-declared-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::create_dir_all(&directory);
    let both = directory.join("both.toml");
    let one = directory.join("one.toml");
    std::fs::write(&both, "absent = [\"postgres\", \"mysql\"]\n").expect("a scratch file");
    std::fs::write(&one, "absent = [\"postgres\"]\n").expect("a scratch file");
    let arguments = [
        "--strict",
        "--target",
        "inillucent-remote::live_postgres",
        "--target",
        "inillucent-remote::live_mysql",
    ];

    let Some(excused) = run_nested_declaring(gate, &arguments, &both) else {
        return;
    };
    let text = said(&excused);
    refuse_a_contended_run("a run whose absences are declared", &text);
    assert!(
        text.contains("by declaration"),
        "the nested run was given no server URL and did not list the two suites under the \
         declaration heading. It printed:\n{text}"
    );
    assert_eq!(
        code(&excused),
        0,
        "every missing prerequisite was declared absent and the strict run still failed. It \
         printed:\n{text}"
    );

    let Some(partly) = run_nested_declaring(gate, &arguments, &one) else {
        return;
    };
    let text = said(&partly);
    let _ = std::fs::remove_dir_all(&directory);
    assert_eq!(
        code(&partly),
        1,
        "`mysql` was not declared absent, so its suite is hollow and a strict run has to exit \
         1. It printed:\n{text}"
    );
    assert!(
        text.contains("ran without a prerequisite"),
        "the undeclared absence was not reported as a suite that evidenced nothing:\n{text}"
    );
}

/// The contention guard matches the failure it was written for, and nothing else.
///
/// **A guard nobody has ever seen fire is the defect this file is about**, one
/// level down: three cases fail on it with a sentence naming the collision, so
/// if it matched nothing a collision would be reported as whatever exit code
/// the nested run happened to return, and if it matched everything they would
/// fail on every run. Both texts below are verbatim from runs
/// on 2026-09-21 - the first from the collision that found this, the second
/// from the deliberate compile error used to check the exit code.
#[test]
fn the_contention_guard_tells_a_held_binary_from_a_build_that_failed() {
    let held = "error: failed to remove file \
                `D:/agent-worktrees/cargo-target/inillucent-task-2047\\debug\\\
                inillucent-scorecard.exe`\n\nCaused by:\n  Access is denied. (os error 5)\n\
                inillucent-testrun: cargo could not list the built targets\n";
    assert!(
        cargo_could_not_replace_a_running_binary(held),
        "the guard did not recognise the collision it was written for, so the three cases \
         that fail on it would assert an exit code from a run that never started"
    );

    let did_not_compile = "error: could not compile `inillucent-compat` (test \"escapes\") \
                           due to 1 previous error\n\
                           inillucent-testrun: the build failed\n";
    assert!(
        !cargo_could_not_replace_a_running_binary(did_not_compile),
        "the guard swallowed a build that failed to compile, which is the case this ticket \
         exists for - `testrun_exits_two_when_the_build_does_not_complete` would report a \
         collision instead of asserting"
    );
}

/// Returns whether a nested run was stopped by cargo, rather than by the runner.
///
/// **The signature of a sibling in this same binary holding an artifact open
/// (task-2047).** cargo relinks a stale target, Windows refuses to replace a
/// running image, and the runner reports that it could not be told what was
/// built. Every string here comes from the observed failure, quoted in the
/// section comment above.
///
/// This is deliberately narrow. It matches cargo failing to *replace a file*,
/// not a build that failed to compile - `the build failed` is the sentence
/// `testrun_exits_two_when_the_build_does_not_complete` asserts on, and a guard
/// that swallowed it would turn this ticket's own case into a skip.
///
/// @param text - everything the nested run printed
fn cargo_could_not_replace_a_running_binary(text: &str) -> bool {
    text.contains("cargo could not list the built targets")
        || text.contains("failed to remove file")
        || text.contains("Access is denied")
}

/// Fails a case whose nested run cargo stopped, with everything the run printed.
///
/// **A failure, not a skip (task-2106).** This was a skip, and under `--strict`
/// a skip is a missing prerequisite, so a build that could not replace a file
/// was reported as a machine that lacked something. It said "through
/// `inillucent-testrun` this cannot happen", and task-2101's run through
/// `inillucent-testrun` hit it. Every nested run now reads the outer run's
/// artifact list and has `CARGO` pointed at a program that refuses, so a nested
/// run that reached cargo at all is a defect in the runner's `--artifacts`
/// path, and what the run printed is what a reader needs to find it.
///
/// @param case - what the case was trying to measure
/// @param text - everything the nested run printed
fn refuse_a_contended_run(case: &str, text: &str) {
    assert!(
        !cargo_could_not_replace_a_running_binary(text) && !text.contains("the build failed"),
        "{case}: the nested run reached cargo, which a run given `--artifacts` never starts. \
         This is a failure and not a missing prerequisite. It printed:\n{text}"
    );
}

/// A nested run given an artifact list that is not there exits 2 and names it.
///
/// The list is the only thing a nested run has to go on, so a missing one is a
/// run that cannot happen, which is what 2 means. Reading it as an empty list
/// would be a run that selects its targets and then finds none of them.
#[test]
fn testrun_exits_two_when_the_artifact_list_is_missing() {
    let Some(gate) = built(TESTRUN, BUILDS_TESTRUN) else {
        return;
    };
    let missing = std::env::temp_dir().join(format!(
        "inillucent-no-artifacts-{}.json",
        std::process::id()
    ));
    let output = Command::new(gate)
        .arg("--artifacts")
        .arg(&missing)
        .args(["--tier", "smoke"])
        .env("CARGO", gate)
        .output()
        .unwrap_or_else(|error| panic!("{gate} did not start: {error}"));
    let text = said(&output);
    assert_eq!(
        code(&output),
        2,
        "a run with no artifact list to read has to exit 2. It printed:\n{text}"
    );
    assert!(
        text.contains(&missing.display().to_string()),
        "the refusal did not name the list it could not read:\n{text}"
    );
}

/// Runs a gate with `CARGO` pointed somewhere else.
///
/// @param gate - the built binary
/// @param arguments - the command line
/// @param cargo - what the gate should invoke as cargo
fn run_with_cargo(gate: &str, arguments: &[&str], cargo: &str) -> Output {
    Command::new(gate)
        .args(arguments)
        .env("CARGO", cargo)
        .output()
        .unwrap_or_else(|error| panic!("{gate} did not start: {error}"))
}

/// Returns a target `--list` named in some tier other than smoke.
///
/// @param listed - what `--list` printed
fn a_target_outside_the_smoke_tier(listed: &str) -> Option<String> {
    listed.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let tier = fields.next()?;
        let label = fields.next()?;
        (tier != "smoke" && label.contains("::")).then(|| label.to_string())
    })
}

/// Returns how many tests a run's summary line says were graded.
///
/// @param text - what the run printed
fn graded_tests(text: &str) -> Option<usize> {
    let line = text.lines().find(|line| line.contains(" test(s), "))?;
    let (before, _) = line.split_once(" test(s), ")?;
    before.rsplit(", ").next()?.trim().parse().ok()
}

/// Every program `docs/repository.md` tells a reader to run is one that exists.
///
/// **Forty-one bin targets sit outside `tests/selection.toml` by design, and
/// the design had no floor (task-1969, 4.10).** `selection.rs`'s
/// `every_target_has_a_row` excludes a `Kind::Bin` target holding no `#[test]`,
/// which is safe for a *program's tests* and says nothing about the program:
/// `inillucent-compat` has 45 bins against four rows, so any of them could be
/// renamed or deleted and no test would change. `docs/repository.md`'s
/// reproduction block is the page that tells a reader which ones to run to get
/// the published numbers back, so it is the list with a reason to be right.
///
/// The names are read off that page rather than typed here as well, because two
/// lists of the same thing is how one of them goes stale - which is the defect
/// §4.14 of the same review found in the three published target counts.
///
/// `inillucent-testrun` is not reachable this way: it is behind
/// `required-features = ["testrun"]`, so its absence is a fact about the build
/// rather than a rename, and the two cases that drive it skip by name instead.
#[test]
fn every_gate_under_test_is_a_binary_that_exists() {
    let page = workspace_root().join("docs/repository.md");
    let text = std::fs::read_to_string(&page)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", page.display()));
    let named = programs_named_in_the_reproduction_block(&text);
    assert!(
        named.len() >= 5,
        "read {} program names out of the reproduction block in {}, which means this is \
         reading the wrong block rather than that the page names no programs",
        named.len(),
        page.display()
    );

    // The gates this file drives, which have to be in the list as well as on
    // the page: a program with a test here and no line on the page is one a
    // reader cannot reproduce, and a program on the page with no test here is
    // what §4.10 is about.
    let mut wanted: Vec<String> = named;
    for driven in [
        "inillucent-readgate",
        "inillucent-writegate",
        "inillucent-fullgate",
        "inillucent-prepareperf",
        "inillucent-searchgate",
        "inillucent-walperf",
        "inillucent-scorecard",
        "inillucent-foldgate",
        "inillucent-release",
    ] {
        wanted.push(driven.to_string());
    }
    wanted.sort();
    wanted.dedup();

    // Every bin of this package lands beside every other one, so one known
    // `CARGO_BIN_EXE_*` gives the directory the rest are in. Reading it from a
    // resolved path rather than assuming `target/debug` is what makes this work
    // under `cargo llvm-cov`, which builds into a target directory of its own.
    let known = Path::new(env!("CARGO_BIN_EXE_inillucent-readgate"));
    let directory = known
        .parent()
        .unwrap_or_else(|| panic!("{} has no parent directory", known.display()));
    let mut absent: Vec<String> = Vec::new();
    for program in &wanted {
        let path = directory.join(format!("{program}{}", std::env::consts::EXE_SUFFIX));
        if !path.is_file() {
            absent.push(path.to_string_lossy().to_string());
        }
    }
    assert!(
        absent.is_empty(),
        "these programs are named by docs/repository.md or driven by this suite and this \
         build produced none of them, so either the name is stale or the target is gone:\n  {}",
        absent.join("\n  ")
    );
}

/// Returns every `inillucent-*` program named in the reproduction block.
///
/// The block is the fenced `sh` listing under `## Reproducing the
/// measurements`. A name counts when it is the last path component of a token,
/// so `target/release/inillucent-fullgate` and a bare `inillucent-manifest`
/// both read as the same program, and a prose mention elsewhere on the page
/// does not.
///
/// @param page - the text of `docs/repository.md`
fn programs_named_in_the_reproduction_block(page: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut inside_section = false;
    let mut inside_fence = false;
    for line in page.lines() {
        if line.starts_with("## ") {
            inside_section = line.trim() == "## Reproducing the measurements";
            continue;
        }
        if !inside_section {
            continue;
        }
        if line.trim_start().starts_with("```") {
            inside_fence = !inside_fence;
            continue;
        }
        if !inside_fence || line.trim_start().starts_with('#') {
            continue;
        }
        // `-p inillucent-compat` names the *package* a `cargo run` builds from,
        // not a program. The block carries three of those, and reading them as
        // programs asked for `target/debug/inillucent-compat.exe`, which cargo
        // has no reason to produce.
        let mut previous = "";
        for token in line.split_whitespace() {
            let names_a_package = previous == "-p" || previous == "--package";
            previous = token;
            if names_a_package {
                continue;
            }
            let candidate = token.rsplit('/').next().unwrap_or(token);
            let candidate =
                candidate.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
            if candidate.starts_with("inillucent-") && !candidate.contains('.') {
                names.push(candidate.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}
