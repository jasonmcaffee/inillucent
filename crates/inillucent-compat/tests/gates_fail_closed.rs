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
    println!("{path} is not built; run `{how}`; skipping");
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
        println!("sqlite-bench is not built; skipping");
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
        println!("the small gate fixture is not built; skipping");
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
        println!("sqlite-bench is not built; skipping");
        return;
    }
    let Some(fixture) = fixture_copy("readgate-small") else {
        println!("the small gate fixture is not built; skipping");
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

/// The write gate measures the smallest real fixture.
#[test]
fn writegate_measures_the_small_fixture() {
    if !sqlite_bench_built() {
        println!("sqlite-bench is not built; skipping");
        return;
    }
    let Some(fixture) = fixture_copy("writegate-small") else {
        println!("the small gate fixture is not built; skipping");
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
        println!("sqlite-bench is not built; skipping");
        return;
    }
    let Some(fixture) = fixture_copy("fullgate-small") else {
        println!("the small gate fixture is not built; skipping");
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
/// `--list` rather than a run, because the run is what every other suite in
/// this file is already inside: what is asserted is that the selection is not
/// empty, which is the thing that would make a green run meaningless.
#[test]
fn testrun_selects_targets_for_the_smoke_tier() {
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

/// Every gate this file covers is a binary the workspace still builds.
///
/// A rename that left a test naming a program nobody builds would make this
/// whole file a check of nothing, which is the defect it exists to prevent.
///
/// `inillucent-testrun` is not in the list: it is behind a feature, so its
/// absence is a fact about the build rather than a rename, and the two cases
/// that drive it skip by name instead. Everything else here is an ordinary
/// binary of this package and a `cargo test` builds all of them.
#[test]
fn every_gate_under_test_is_a_binary_that_exists() {
    for gate in [
        env!("CARGO_BIN_EXE_inillucent-readgate"),
        env!("CARGO_BIN_EXE_inillucent-writegate"),
        env!("CARGO_BIN_EXE_inillucent-fullgate"),
        env!("CARGO_BIN_EXE_inillucent-searchgate"),
        env!("CARGO_BIN_EXE_inillucent-walperf"),
        env!("CARGO_BIN_EXE_inillucent-scorecard"),
    ] {
        assert!(
            Path::new(gate).is_file(),
            "{gate} is named by this suite and is not built"
        );
    }
}
