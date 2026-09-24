//! Running one child process and knowing when to stop waiting for it.
//!
//! Invariant: **this module never waits on a pipe after the child has gone.**
//! It waits on the child, and reads the pipes alongside; when the child has
//! exited the pipes get a bounded window to finish and then the answer is
//! returned with whatever they produced. There is no path through it that ends
//! in a process sitting on a file descriptor with nothing on the other end.
//!
//! ## The defect this exists for
//!
//! `Command::output()` does not wait for the child. It reads standard output
//! and standard error to **end of file** and then reaps the child, and the two
//! are not the same event. A pipe reaches end of file when the last handle to
//! its write end is closed, and a child that spawned anything with inherited
//! standard output has given that handle away. The descendant keeps the pipe
//! open after the child is gone, and `output()` waits on it, for as long as the
//! descendant lives.
//!
//! Fourteen lines reproduce it, and did, on 2026-09-22:
//!
//! ```text
//! parent  : Command::new(me).arg("holder").output()
//! holder  : Command::new(me).arg("sleeper").spawn(); println!("holder is exiting now")
//! sleeper : sleep 20s
//!
//! output() returned after 20.0s, status Some(0), stdout "holder is exiting now"
//! ```
//!
//! The child had exited with status zero and printed everything it was going to
//! print. `output()` waited twenty seconds anyway.
//!
//! That is what happened to `inillucent-testrun` on task-2067's run: a target's
//! process was stopped by hand, a descendant of it survived holding the pipe,
//! and the runner sat there with **no child of its own** and 1.625 seconds of
//! processor time, never printing a result for the target and never printing a
//! summary. It had to be stopped by its process id. Nothing was graded and no
//! exit code was produced, which is the one outcome the runner's three exit
//! codes cannot describe - a red run is information and a run that never ends is
//! not.
//!
//! It needs nobody to kill anything, either. `interchange::our_shell` ran cargo
//! through `Command::status()`, which inherits standard output, so every rustc
//! cargo started held the runner's pipe, and a cargo that outlived its test
//! binary by any means hung the run. Since task-2106 no suite the runner starts
//! runs cargo for the programs, and `cliproc::program` captures cargo's output
//! when a plain `cargo test` makes it build, but any child a suite starts with
//! inherited standard output can still do the same thing.
//!
//! ## What counts as progress, and why it is not elapsed time
//!
//! A target that has been quiet for half an hour is not thereby stuck. The
//! nightly ledger story replays a day through the pinned SQLite shell and waits
//! on it: its own processor time is flat, its threads are all in `Wait`, and the
//! `sqlite3.exe` doing the work is a process this one never sees. Three separate
//! readings of that signature were called a hang on this machine in one evening
//! and one of them cost a legitimate thirty minute run.
//!
//! So a duration on its own is not allowed to stop anything here. Two facts are
//! required together before the supervisor will kill a child:
//!
//! 1. the target has run past a budget drawn from **its own** recorded time, and
//! 2. it has produced **no output at all** for a long stretch.
//!
//! A target that is still printing is still doing something, and this will wait
//! on it however long it takes. That is deliberate: the expensive direction of a
//! wrong answer here is killing a suite that was working, and bytes arriving on
//! the pipe are direct evidence rather than an inference from a counter.
//!
//! ## How much of a heartbeat the pipe really is
//!
//! Less than it looks, and the tests below found it out rather than assuming it.
//! libtest **captures** what a test itself prints and holds it until the run
//! ends, so a `println!` inside a test never reaches this pipe while the test is
//! running. What does reach it, as each test finishes, is libtest's own
//! `test <name> ... ok` line.
//!
//! So the heartbeat is one line per finished test, and it says nothing about how
//! a suite that is **one long test** is getting on. `story_ledger_day_nightly`
//! is exactly that: its one test replays a day through the pinned SQLite shell,
//! and it is silent on this pipe from the moment it starts until the moment it
//! ends. Nothing here can tell that apart from a hang.
//!
//! The budget is what covers that, which is why it is drawn from the target's
//! own recorded time rather than from one number for the whole suite, and why it
//! is eight times that time with a thirty minute floor. A suite recorded at half
//! an hour is given four hours before this will even consider stopping it.
//!
//! Reading a grandchild's processor time would close the gap, and is not done
//! here: enumerating the process tree needs an operating-system dependency this
//! workspace's dependency policy does not allow, for a signal that could only
//! ever make a kill happen *sooner*. Waiting longer is the cheap direction to be
//! wrong in.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many times its recorded time a target may take before it may be stopped.
///
/// Eight, because the recorded times are measured under the parallel runner on a
/// loaded machine and already vary by a factor of two or three between runs. A
/// multiple small enough to be tight would be a multiple that fires on an
/// ordinary slow day, and a bound that fires on healthy work stops being read.
pub const BUDGET_SLACK: u32 = 8;

/// The shortest budget any target gets, whatever the ledger says about it.
///
/// **Two hours, and the number was chosen after nearly getting it wrong.** The
/// floor is what a target with no row in `tests/timings.toml` gets, because
/// eight times a time nobody measured is eight times a guess.
///
/// The obvious floor is thirty minutes, and thirty minutes would have killed
/// `inillucent::story_ledger_day_nightly`. That target is in the `nightly` tier,
/// so an ordinary run never selects it, so `--record` has never written a row
/// for it - and it completes at **1800.37s**, having printed nothing on its pipe
/// for the whole of it, because its one test replays a day through the pinned
/// SQLite shell and libtest holds a test's own output back until it ends. It
/// would have been stopped four tenths of a second before it finished, and the
/// run would have called a healthy suite hung.
///
/// So the floor is four times the longest legitimate target this repository has,
/// which also puts it above every recorded target's own budget but the two
/// longest.
///
/// **Four times, because the bound has to survive the machine being busy and not
/// only the suite being slow.** That same target was watched again while two
/// other tickets were running their own suites on this box: it grew its replay
/// database at about 1.2 KB/s and took near an hour rather than the 1800.37s it
/// takes to itself. A bound drawn from a suite's idle time is a bound that fires
/// the first time three people are working.
///
/// The asymmetry is the rest of the argument: waiting two hours to report a
/// target that was never going to finish costs one run's wall clock, and killing
/// a working suite costs the run *and* teaches everyone to stop believing the
/// bound. This repository's history is full of thresholds that had to be raised
/// after they stopped healthy work.
pub const BUDGET_FLOOR: Duration = Duration::from_secs(2 * 60 * 60);

/// How long a target must print nothing before its budget may stop it.
///
/// Ten minutes of complete silence, on top of the budget. Both have to be true,
/// so this is the condition that protects the long single test: it may sit past
/// its budget for as long as it likes provided it is saying something.
pub const BUDGET_SILENCE: Duration = Duration::from_secs(10 * 60);

/// How long to keep reading the pipes after the child has exited.
///
/// Ten seconds, which is the whole of the fix for the observed hang: the child
/// has already answered, so this is a window to collect a transcript that is
/// almost always sitting in the reader threads already, not a wait for anything
/// to happen.
pub const DRAIN: Duration = Duration::from_secs(10);

/// Why the supervisor stopped waiting for the child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stopped {
    /// The child exited and both pipes reached end of file.
    Exited,
    /// The child exited, and something it started still held the pipes open.
    ///
    /// The exit status and the transcript are both real; what is not guaranteed
    /// is that the transcript is complete, because the drain window ended while
    /// a descendant still had the write end.
    ExitedHoldingPipes,
    /// The child was killed: it ran past its budget with nothing to show for it.
    Killed {
        /// How long it had been running.
        ran_for: Duration,
        /// How long it had been since it last printed anything.
        silent_for: Duration,
    },
}

/// What one supervised run produced.
pub struct Supervised {
    /// How the child exited, or `None` when killing it left no readable status.
    pub status: Option<ExitStatus>,
    /// Everything the child wrote to standard output, as far as it was read.
    pub stdout: Vec<u8>,
    /// Everything the child wrote to standard error, as far as it was read.
    pub stderr: Vec<u8>,
    /// Why the supervisor stopped waiting.
    pub stopped: Stopped,
    /// How long the child ran.
    pub elapsed: Duration,
}

/// What the supervisor is allowed to wait for.
pub struct Limits {
    /// How long the child may run before it may be stopped; `None` never stops it.
    pub budget: Option<Duration>,
    /// How long the child must print nothing before `budget` may stop it.
    pub silence: Duration,
    /// How long to keep reading the pipes once the child has exited.
    pub drain: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            budget: None,
            silence: BUDGET_SILENCE,
            drain: DRAIN,
        }
    }
}

/// Returns whether a child that is still running should be stopped.
///
/// **Both conditions, never either.** A target past its budget that is still
/// printing is slow, and a target printing nothing that is inside its budget is
/// most likely blocked on a child doing the work. Only a target that is past its
/// budget *and* has gone quiet is one this can say nothing good about.
///
/// It is a function of four durations and nothing else so that the rule can be
/// tested without a clock. The live tests below drive real processes and so can
/// only ever check the rule to within a scheduling delay; this checks it exactly,
/// and it is what would fail if somebody later wrote `||` here.
///
/// @param ran_for - how long the child has been running
/// @param silent_for - how long since it last printed anything
/// @param limits - what the supervisor is allowed to wait for
pub fn should_stop(ran_for: Duration, silent_for: Duration, limits: &Limits) -> bool {
    match limits.budget {
        Some(budget) => ran_for > budget && silent_for > limits.silence,
        None => false,
    }
}

/// Returns how long a target may run, from what it has been measured taking.
///
/// The floor is applied after the multiple rather than instead of it, so a
/// target that is genuinely slow gets a budget proportional to itself and a
/// target nobody has timed gets the floor.
///
/// @param recorded - what the ledger says this target takes, when it knows
pub fn budget(recorded: Option<Duration>) -> Duration {
    let scaled = recorded
        .and_then(|time| time.checked_mul(BUDGET_SLACK))
        .unwrap_or(BUDGET_FLOOR);
    scaled.max(BUDGET_FLOOR)
}

/// A pipe being read into memory by a thread that owns its read end.
///
/// The bytes are appended under a lock rather than returned at the end, so the
/// supervisor can read a partial transcript from a reader it has given up on,
/// and can use the length as the progress signal.
#[derive(Default)]
struct Collected {
    /// What has arrived so far.
    bytes: Mutex<Vec<u8>>,
    /// Whether the reader reached end of file and stopped.
    finished: AtomicBool,
}

impl Collected {
    /// Returns how many bytes have arrived so far.
    fn len(&self) -> usize {
        match self.bytes.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Takes a copy of what has arrived so far.
    fn snapshot(&self) -> Vec<u8> {
        match self.bytes.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Appends what one read produced.
    ///
    /// @param chunk - the bytes just read
    fn append(&self, chunk: &[u8]) {
        match self.bytes.lock() {
            Ok(mut guard) => guard.extend_from_slice(chunk),
            Err(poisoned) => poisoned.into_inner().extend_from_slice(chunk),
        }
    }
}

/// Reads one pipe to end of file, appending as it goes.
///
/// Runs on its own thread, and is deliberately abandonable: when the supervisor
/// stops waiting, this keeps its read end and its buffer alive rather than being
/// interrupted, because there is no portable way to interrupt a blocking read
/// and forcing one would be a worse trade than leaving a sleeping thread behind
/// in a process that is about to print a summary and exit.
///
/// @param source - the pipe's read end
/// @param into - where the bytes go, shared with the supervisor
fn collect(mut source: impl Read, into: Arc<Collected>) {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match source.read(&mut buffer) {
            // End of file: every handle to the write end has closed, which is
            // the event `Command::output()` mistakes for the child exiting.
            Ok(0) => break,
            Ok(read) => match buffer.get(..read) {
                Some(chunk) => into.append(chunk),
                None => break,
            },
            // A signal arriving mid-read is not the end of the transcript, and
            // treating it as one would truncate a suite's output for a reason
            // that has nothing to do with the suite.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    into.finished.store(true, Ordering::SeqCst);
}

/// Runs a command, waiting on the child rather than on its pipes.
///
/// The command's standard output and standard error are replaced with pipes and
/// its standard input with nothing, which is what `Command::output` does; what
/// differs is everything after the spawn.
///
/// @param command - the command to run, configured except for its stdio
/// @param limits - how long to wait, and for what
pub fn supervise(command: &mut Command, limits: &Limits) -> std::io::Result<Supervised> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();
    let out = reader(child.stdout.take());
    let err = reader(child.stderr.take());
    let (stopped, status) = wait_for(&mut child, &out, &err, limits, started);
    let elapsed = started.elapsed();
    Ok(Supervised {
        status,
        stdout: out.snapshot(),
        stderr: err.snapshot(),
        stopped,
        elapsed,
    })
}

/// Starts a thread reading one of the child's pipes.
///
/// @param pipe - the pipe, when the child was given one
fn reader(pipe: Option<impl Read + Send + 'static>) -> Arc<Collected> {
    let collected = Arc::new(Collected::default());
    match pipe {
        Some(pipe) => {
            let into = Arc::clone(&collected);
            if std::thread::Builder::new()
                .spawn(move || collect(pipe, into))
                .is_err()
            {
                // A thread that could not start reads nothing, and saying so is
                // better than leaving the supervisor watching a length that can
                // never change and calling the target silent for it.
                collected.finished.store(true, Ordering::SeqCst);
            }
        }
        // No pipe is end of file immediately, which is the right answer: there
        // is nothing to wait for.
        None => collected.finished.store(true, Ordering::SeqCst),
    }
    collected
}

/// Waits for the child, killing it only when it is both over budget and silent.
///
/// Returns why it stopped waiting and the child's status, which is read here
/// rather than by the caller so there is exactly one place the child is reaped.
///
/// @param child - the running child
/// @param out - the standard output collector
/// @param err - the standard error collector
/// @param limits - how long to wait, and for what
/// @param started - when the child was spawned
fn wait_for(
    child: &mut Child,
    out: &Arc<Collected>,
    err: &Arc<Collected>,
    limits: &Limits,
    started: Instant,
) -> (Stopped, Option<ExitStatus>) {
    let mut seen = 0usize;
    let mut last_change = Instant::now();
    let mut interval = Duration::from_millis(25);
    loop {
        // **The child first, every time round.** This is the whole defect: once
        // the child has exited the pipes are no longer evidence about it, and
        // anything still holding them is somebody else's process.
        if let Ok(Some(status)) = child.try_wait() {
            return (drain(out, err, limits.drain), Some(status));
        }
        let arrived = out.len() + err.len();
        if arrived > seen {
            seen = arrived;
            last_change = Instant::now();
        }
        {
            let ran_for = started.elapsed();
            let silent_for = last_change.elapsed();
            if should_stop(ran_for, silent_for, limits) {
                let _ = child.kill();
                let status = child.wait().ok();
                return (
                    Stopped::Killed {
                        ran_for,
                        silent_for,
                    },
                    status,
                );
            }
        }
        std::thread::sleep(interval);
        // A smoke suite finishes in under a second and should not pay half of
        // one to be noticed; a four-hour campaign should not be polled forty
        // thousand times an hour. Doubling from 25ms to 500ms is both.
        interval = (interval * 2).min(Duration::from_millis(500));
    }
}

/// Gives the pipes a bounded window to finish, after the child has exited.
///
/// @param out - the standard output collector
/// @param err - the standard error collector
/// @param window - how long to keep reading
fn drain(out: &Arc<Collected>, err: &Arc<Collected>, window: Duration) -> Stopped {
    let until = Instant::now() + window;
    loop {
        if out.finished.load(Ordering::SeqCst) && err.finished.load(Ordering::SeqCst) {
            return Stopped::Exited;
        }
        if Instant::now() >= until {
            return Stopped::ExitedHoldingPipes;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::{budget, should_stop, supervise, Limits, Stopped, BUDGET_FLOOR};
    use std::io::Write;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// The environment variable that turns this test binary into the helper the
    /// supervision tests drive. Unset in an ordinary run, so `the_helper` is a
    /// test that returns immediately and asserts nothing about anything.
    const ROLE: &str = "INILLUCENT_SUPERVISE_ROLE";

    /// The helper's own name, as libtest spells it, so a rename of the test
    /// below cannot leave the filter pointing at nothing - which would make
    /// every test here pass against a child that ran no helper at all.
    const HELPER: &str = "supervise::tests::the_helper";

    /// Plays whichever part `ROLE` asks for, so the tests below have a child and
    /// a grandchild to drive without needing a program that is not this one.
    ///
    /// `grandchild` is the case the module exists for: it starts a long-lived
    /// process with **inherited** standard output - so the grandchild takes the
    /// supervisor's pipe write handle with it - and then returns, which ends the
    /// child. `sleep` is that long-lived process. `noisy` prints on a timer, to
    /// stand for a target that is slow and working.
    #[test]
    fn the_helper() {
        // **No early return, on purpose.** `policy`'s
        // `every_early_return_in_a_test_says_why` reads one as a test that
        // reports success having run nothing because its prerequisite was
        // absent, and it is right to: that is what an unannounced `return` in a
        // test means everywhere else in this workspace. Here there is no
        // prerequisite and nothing is being skipped - with no role asked for,
        // playing no part is the whole of what this does - so the answer is to
        // have no early return rather than to announce one.
        let role = std::env::var(ROLE).unwrap_or_default();
        let me = std::env::current_exe().unwrap();
        match role.as_str() {
            "grandchild" => {
                // Default stdio is inherited, which is the point.
                let mut child = Command::new(&me);
                child.env(ROLE, "sleep").args([HELPER, "--exact"]);
                let _ = child.spawn().unwrap();
                println!("the child is exiting and the grandchild is not");
            }
            "sleep" => std::thread::sleep(Duration::from_secs(30)),
            "silent" => std::thread::sleep(Duration::from_secs(30)),
            "noisy" => {
                // **Written to the handle, not through `println!`.** libtest
                // captures what a test prints and holds it to the end of the
                // run, so a `println!` here would reach the supervisor's pipe
                // only once this process was already finished - which is no
                // heartbeat at all, and is what made the first version of
                // `a_child_that_is_still_printing_is_never_killed_for_being_slow`
                // fail. This stands for the `test <name> ... ok` lines libtest
                // itself writes straight to the handle as each test finishes,
                // which is the real heartbeat a suite has.
                let mut out = std::io::stdout();
                for tick in 0..100 {
                    let _ = writeln!(out, "still working, tick {tick}");
                    let _ = out.flush();
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            _ => {}
        }
    }

    /// Builds a command that runs this test binary as the helper in `role`.
    ///
    /// @param role - which part the helper should play
    fn helper(role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env(ROLE, role)
            // `--show-output`, because libtest holds a passing test's own
            // output back otherwise and these tests read it.
            .args([HELPER, "--exact", "--test-threads", "1", "--show-output"]);
        command
    }

    /// The defect: the child is gone, a grandchild holds the pipe, and the
    /// supervisor has to answer from the child rather than from the pipe.
    ///
    /// `Command::output()` returns from this after thirty seconds. Anything over
    /// the drain window means the fix has been lost.
    #[test]
    fn a_child_that_has_exited_is_reported_without_waiting_for_its_grandchild() {
        let started = Instant::now();
        let run = supervise(
            &mut helper("grandchild"),
            &Limits {
                drain: Duration::from_secs(2),
                ..Limits::default()
            },
        )
        .unwrap();
        let waited = started.elapsed();

        assert_eq!(run.stopped, Stopped::ExitedHoldingPipes);
        assert!(
            waited < Duration::from_secs(20),
            "waited {waited:?} for a child that had already exited"
        );
        assert!(
            run.status.is_some_and(|status| status.success()),
            "the child's own status was lost: {:?}",
            run.status
        );
        let text = String::from_utf8_lossy(&run.stdout).into_owned();
        assert!(
            text.contains("the child is exiting and the grandchild is not"),
            "the transcript the child did write was lost: {text}"
        );
    }

    /// An ordinary child, which must come back saying the pipes closed too -
    /// otherwise the test above would pass on a supervisor that never waits for
    /// a transcript at all.
    #[test]
    fn an_ordinary_child_reaches_end_of_file_on_both_pipes() {
        let run = supervise(&mut helper("none"), &Limits::default()).unwrap();
        assert_eq!(run.stopped, Stopped::Exited);
        let text = String::from_utf8_lossy(&run.stdout).into_owned();
        assert!(
            text.contains("test result:"),
            "the whole transcript should be here: {text}"
        );
    }

    /// A child that is over budget and saying nothing is stopped, and says so
    /// with both numbers the decision was made on.
    #[test]
    fn a_silent_child_past_its_budget_is_killed_and_named() {
        let run = supervise(
            &mut helper("silent"),
            &Limits {
                budget: Some(Duration::from_millis(300)),
                silence: Duration::from_millis(300),
                drain: Duration::from_secs(2),
            },
        )
        .unwrap();
        match run.stopped {
            Stopped::Killed {
                ran_for,
                silent_for,
            } => {
                assert!(ran_for >= Duration::from_millis(300));
                assert!(silent_for >= Duration::from_millis(300));
            }
            other => panic!("a silent child past its budget was not killed: {other:?}"),
        }
        assert!(
            run.elapsed < Duration::from_secs(20),
            "it was killed far too late: {:?}",
            run.elapsed
        );
    }

    /// The case the budget must never fire on: a child past its budget that is
    /// still printing. This is the expensive direction to get wrong, and three
    /// readings of it were got wrong by hand on this machine in one evening.
    #[test]
    fn a_child_that_is_still_printing_is_never_killed_for_being_slow() {
        let run = supervise(
            &mut helper("noisy"),
            &Limits {
                // Long past, and it still must not be stopped.
                budget: Some(Duration::from_millis(100)),
                // **Forty times the tick.** This test drives real processes,
                // so it can only ever be robust to within a scheduling delay,
                // and it has to hold on a box carrying two other tickets' runs.
                // The rule itself is checked exactly by
                // `the_kill_rule_needs_both_conditions`, which has no clock in
                // it; this one checks that the rule is wired to the loop.
                //
                // **It has been seen to fail once, and the load that did it is
                // worth knowing** (task-2066). On 2026-09-23 this box was
                // carrying two other tickets' full suites, a third copy of the
                // nightly stories under `cargo llvm-cov --release`, and this
                // ticket's own gate - four concurrent runs rather than the two
                // the paragraph above is sized for. It passed three times in a
                // row immediately afterwards on the same build. The threshold
                // was deliberately **not** raised: the stated design point was
                // exceeded rather than wrong, and widening a guard to cover a
                // load nobody should create is how a guard stops guarding.
                silence: Duration::from_secs(2),
                drain: Duration::from_secs(2),
            },
        )
        .unwrap();
        assert!(
            !matches!(run.stopped, Stopped::Killed { .. }),
            "a working child was killed for being over a duration: {:?}",
            run.stopped
        );
        let text = String::from_utf8_lossy(&run.stdout).into_owned();
        assert!(
            text.contains("still working, tick 99"),
            "it was cut off before it finished: {text}"
        );
    }

    /// The rule itself, with no processes and no clock: a target is stopped only
    /// when it is past its budget **and** has gone quiet. Three of these four
    /// cases are a working target that must be left alone.
    #[test]
    fn the_kill_rule_needs_both_conditions() {
        let limits = Limits {
            budget: Some(Duration::from_secs(100)),
            silence: Duration::from_secs(10),
            ..Limits::default()
        };
        let long = Duration::from_secs(101);
        let short = Duration::from_secs(99);
        let quiet = Duration::from_secs(11);
        let talking = Duration::from_secs(9);

        assert!(
            should_stop(long, quiet, &limits),
            "past its budget and silent"
        );
        assert!(
            !should_stop(long, talking, &limits),
            "past its budget but still printing: slow, not stuck"
        );
        assert!(
            !should_stop(short, quiet, &limits),
            "silent but inside its budget: most likely blocked on a child doing the work"
        );
        assert!(!should_stop(short, talking, &limits), "working");

        // `--timeout 0`, which leaves only the half with no threshold in it.
        let unbounded = Limits {
            budget: None,
            ..Limits::default()
        };
        assert!(!should_stop(
            Duration::from_secs(86_400),
            Duration::from_secs(86_400),
            &unbounded
        ));
    }

    /// The budget is drawn from the target's own recorded time, and the floor is
    /// what makes that safe when the ledger has never seen it.
    #[test]
    fn the_budget_scales_with_the_target_and_never_goes_under_the_floor() {
        assert_eq!(budget(None), BUDGET_FLOOR);
        assert_eq!(budget(Some(Duration::from_secs(1))), BUDGET_FLOOR);
        // `story_ledger_day_nightly` has no row in the ledger and takes
        // 1800.37s, so the floor is the only thing standing between it and
        // being killed four tenths of a second before it finishes.
        assert!(
            BUDGET_FLOOR > Duration::from_secs(1801),
            "the floor does not clear the longest target that has no recorded time"
        );
        // The nightly ledger story: half an hour recorded, four hours allowed.
        assert_eq!(
            budget(Some(Duration::from_secs(1800))),
            Duration::from_secs(4 * 3600)
        );
    }
}
