//! What a test target actually did, read from its transcript and its exit code.
//!
//! Invariant: **this module never turns a failure into a pass, and it never
//! reports a pass or a failure it cannot justify from what the target printed.**
//! Where it cannot tell, it says so, and saying so is a third answer rather than
//! a rounding of one of the other two.
//!
//! ## The run this exists because of
//!
//! `inillucent-testrun --strict` reported `inillucent-bench` as FAILED. Run on
//! its own immediately afterwards it exited 0 with 156 tests passing, so the
//! FAILED was not about a test. The runner had decided it from one fact - the
//! process's exit status - and `inillucent-bench` loads the ONNX runtime and its
//! CUDA provider, which sometimes takes the process down *after* libtest has
//! printed its summary. Reproduced here directly: three consecutive runs of that
//! binary alone printed `test result: ok. 156 passed; 0 failed` every time, and
//! the first of them exited **127**.
//!
//! One word, FAILED, for that event is wrong in a way that matters more than the
//! red build it caused. A runner whose verdict does not come from the target's
//! test results can be wrong in the other direction too, and nothing in that run
//! said it could not be. So the verdict is derived from the transcript *and* the
//! exit status together, and the three cases they can disagree in are named:
//!
//! | transcript | exit | verdict |
//! |---|---|---|
//! | a summary with no failures | zero | [`Verdict::Passed`] |
//! | a summary naming failures | anything | [`Verdict::Failed`] |
//! | a summary with no failures | non-zero | [`Undetermined::DiedAfterTestsPassed`] |
//! | no summary at all | non-zero | [`Undetermined::NoSummary`] |
//!
//! ## Its retry, and why only here
//!
//! A target the runner could not read is run once more, on its own, at the end of
//! the pass, and the second answer stands. That is a determination rather than a
//! rounding: a process that dies at teardown one time in nine passes its retry,
//! and one that dies every time keeps failing the run and is named as having died
//! *after* its tests passed - a different sentence from "a test failed", pointing
//! at a different bug.
//!
//! The retry fires only on an absence of information. A target that failed a test
//! is never re-run, because a runner that re-runs failures until they pass has
//! stopped being a gate.
//!
//! ## Why not match on the exit code
//!
//! The obvious shortcut is to recognise the teardown crash by its status -
//! `0xC0000409`, or 127. That is a rule about one library on one operating
//! system, and the next library to die at teardown will use a different number.
//! The general fact is the one that is read: the harness said every test passed,
//! and the process then did not exit cleanly.

/// What one test binary's own harness reported.
///
/// libtest prints exactly one `test result:` line per run, and it carries both
/// counts. Reading `failed` as well as `passed` is what separates "a test
/// failed" from "the process died after the tests".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    /// Tests the harness reported as passing.
    pub passed: usize,
    /// Tests the harness reported as failing.
    pub failed: usize,
}

/// Why the runner cannot say what a target did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Undetermined {
    /// The harness reported every test passing and the process then exited
    /// non-zero.
    ///
    /// The tests are a fact and so is the death; neither cancels the other. What
    /// is unknown is whether the death is reproducible, which is what the
    /// runner's retry answers.
    DiedAfterTestsPassed,
    /// The process exited non-zero without printing a `test result:` line.
    ///
    /// The runner does not know which tests ran, so it cannot report a failure
    /// against any of them. This is what a binary that dies before its summary
    /// looks like, and it is also what a lost line under load would look like -
    /// which is exactly why it is not called a failure.
    NoSummary,
    /// The executable could not be started at all.
    NeverStarted,
    /// The runner stopped waiting for it and killed it.
    ///
    /// Not a failure: nothing was graded, and the tests it had run before it was
    /// stopped are not evidence that the rest would have passed. Not a pass
    /// either, for the same reason. It is the third answer, which is what this
    /// enum is for - and it is the one reason here that must never be retried,
    /// because a second attempt costs the same budget again and cannot produce
    /// information the first one withheld.
    TimedOut,
}

impl Undetermined {
    /// Returns the one sentence a report prints for this reason.
    pub fn reason(self) -> &'static str {
        match self {
            Undetermined::DiedAfterTestsPassed => {
                "the harness reported every test passing and the process then exited non-zero"
            }
            Undetermined::NoSummary => {
                "the process exited non-zero and printed no `test result:` line, so which tests ran is unknown"
            }
            Undetermined::NeverStarted => "the executable could not be started",
            Undetermined::TimedOut => {
                "it ran past its budget while printing nothing, so the runner stopped waiting and killed it"
            }
        }
    }
}

/// What a target did, as far as the runner can establish it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The harness reported a summary with no failures and the process exited
    /// zero.
    Passed,
    /// The harness reported failing tests.
    Failed,
    /// The runner cannot say, for the named reason.
    Undetermined(Undetermined),
}

impl Verdict {
    /// Returns the word a report prints in the per-target line.
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Passed => "ok",
            Verdict::Failed => "FAILED",
            Verdict::Undetermined(_) => "UNKNOWN",
        }
    }

    /// Reports whether this verdict lets the run exit zero.
    pub fn is_green(self) -> bool {
        matches!(self, Verdict::Passed)
    }

    /// Returns the reason, when the verdict is that there is no verdict.
    pub fn undetermined(self) -> Option<Undetermined> {
        match self {
            Verdict::Undetermined(reason) => Some(reason),
            _ => None,
        }
    }
}

/// Reads the harness's own summary line out of a transcript.
///
/// The line looks like `test result: ok. 156 passed; 0 failed; 0 ignored; 0
/// measured; 0 filtered out; finished in 137.03s`. Both counts are read from the
/// same line so that a transcript which somehow carries one and not the other is
/// treated as no summary rather than as a half-read one.
///
/// @param text - everything the binary printed, standard output then standard error
pub fn read_summary(text: &str) -> Option<Summary> {
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("test result:") else {
            continue;
        };
        let Some(passed) = count_before(rest, " passed;") else {
            continue;
        };
        let Some(failed) = count_before(rest, " failed;") else {
            continue;
        };
        return Some(Summary { passed, failed });
    }
    None
}

/// Returns the number immediately before a label in the summary line.
///
/// @param rest - the summary line after its `test result:` prefix
/// @param label - the label to read the count in front of, such as ` passed;`
fn count_before(rest: &str, label: &str) -> Option<usize> {
    let (head, _) = rest.split_once(label)?;
    head.rsplit(' ')
        .find(|word| !word.is_empty())
        .and_then(|number| number.parse::<usize>().ok())
}

/// Decides what a finished target did.
///
/// @param text - everything the binary printed
/// @param exited_zero - whether the process exited with status zero
pub fn classify(text: &str, exited_zero: bool) -> Verdict {
    match read_summary(text) {
        Some(summary) if summary.failed > 0 => Verdict::Failed,
        Some(_) if exited_zero => Verdict::Passed,
        Some(_) => Verdict::Undetermined(Undetermined::DiedAfterTestsPassed),
        // No summary and a clean exit is a target the harness filtered to
        // nothing, which `--strict`'s own hollowness check is the right place to
        // notice. It is not a failure and there is nothing undetermined about
        // it: the process said it was done and left with status zero.
        None if exited_zero => Verdict::Passed,
        None => Verdict::Undetermined(Undetermined::NoSummary),
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, read_summary, Summary, Undetermined, Verdict};

    /// A real transcript tail, as libtest writes it.
    const OK: &str = "running 156 tests\n\
                      test result: ok. 156 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 137.03s\n";

    /// The same, with a failure in it.
    const BAD: &str = "running 6 tests\ntest one_transaction_beats_many ... FAILED\n\n\
                       test result: FAILED. 5 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.9s\n";

    /// Both counts come off the one summary line.
    #[test]
    fn the_summary_carries_both_counts() {
        assert_eq!(
            read_summary(OK),
            Some(Summary {
                passed: 156,
                failed: 0
            })
        );
        assert_eq!(
            read_summary(BAD),
            Some(Summary {
                passed: 5,
                failed: 1
            })
        );
        assert_eq!(read_summary("nothing useful here\n"), None);
    }

    /// A clean run is a pass and a reported failure is a failure, whatever the
    /// exit status says.
    #[test]
    fn a_reported_failure_is_a_failure() {
        assert_eq!(classify(OK, true), Verdict::Passed);
        assert_eq!(classify(BAD, false), Verdict::Failed);
        // Exit zero with a failing summary should not happen, and if it does the
        // harness's own count is what is believed.
        assert_eq!(classify(BAD, true), Verdict::Failed);
    }

    /// **The case this module was written for.** 156 tests passed and the
    /// process exited 127, which is neither a pass nor a test failure.
    #[test]
    fn dying_after_the_tests_passed_is_not_a_test_failure() {
        assert_eq!(
            classify(OK, false),
            Verdict::Undetermined(Undetermined::DiedAfterTestsPassed)
        );
        assert!(!classify(OK, false).is_green());
        assert_eq!(classify(OK, false).word(), "UNKNOWN");
    }

    /// A non-zero exit with no summary is not attributed to any test.
    #[test]
    fn a_missing_summary_is_not_attributed_to_a_test() {
        assert_eq!(
            classify("", false),
            Verdict::Undetermined(Undetermined::NoSummary)
        );
        assert_eq!(
            classify("thread 'main' panicked at src/main.rs:1:1\n", false),
            Verdict::Undetermined(Undetermined::NoSummary)
        );
        // A clean exit with no summary is a filtered run, not a mystery.
        assert_eq!(classify("", true), Verdict::Passed);
    }

    /// Every reason prints a sentence, so a report can never show an empty one.
    #[test]
    fn every_reason_has_a_sentence() {
        for reason in [
            Undetermined::DiedAfterTestsPassed,
            Undetermined::NoSummary,
            Undetermined::NeverStarted,
        ] {
            assert!(!reason.reason().is_empty());
            assert_eq!(Verdict::Undetermined(reason).undetermined(), Some(reason));
        }
        assert_eq!(Verdict::Passed.undetermined(), None);
        assert_eq!(Verdict::Failed.word(), "FAILED");
    }
}
