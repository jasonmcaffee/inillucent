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
