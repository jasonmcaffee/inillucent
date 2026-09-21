//! `PRAGMA busy_timeout` waits, and then succeeds.
//!
//! Invariant: **a writer that already has the file open and meets it busy waits
//! for it when it was given a timeout, and is refused at once when it was not,
//! and the difference is visible as a row that is there or is not.**
//!
//! ## The half of `busy_timeout` nothing tested
//!
//! `process_concurrency::a_refusal_names_the_holder_and_the_operation` covers
//! the immediate refusal. The *waiting* half had a test once - in process, in
//! `concurrency.rs` near line 471 - and it was deleted when the connection
//! stopped being `Send`, because a second writer inside one process cannot be
//! made to contend. Nothing replaced it. So the pragma that decides whether a
//! busy application blocks or fails had half its behaviour under test.
//!
//! ## The pragma does not govern the open, and the first version of this suite
//! ## was measuring the open
//!
//! `inillucent_pool`'s bootstrap retries with `DEFAULT_BUSY_MILLIS` written in,
//! because there is no connection yet to have a pragma on - so a process that
//! opens a file another process is holding waits five seconds whatever it was
//! going to set. That is the same shape SQLite has and it is not a defect; what
//! it is, is a way to write a test that passes without exercising anything.
//!
//! The first version of this file started a second `inillucent-shell` with
//! `PRAGMA busy_timeout = 5000` at the top of its script while the file was
//! already held. It passed - and the refusal text of the third case gave it
//! away: *waited 5013 ms of the 5000 ms PRAGMA busy_timeout*, against a budget
//! of 50 that the script had set. The waiting was the **open** retrying on the
//! default, and the pragma had not been read yet.
//!
//! So every case here opens the second writer **first**, on a file nobody is
//! holding, sets its timeout, and only then lets the first writer take the
//! lock. Under the default `locking_mode = normal` a connection releases the
//! file between statements, so an open shell holds nothing until it writes.
//!
//! ## What is asserted, and what the design asked for instead
//!
//! The design asked for the slot's `waited` counter to have moved and its
//! `timed_out` not to have. **Those counters do not exist.** `inillucent stats`
//! reports the page cache and the file's geometry, and `inillucent-pool`'s
//! `waited` is a local in a retry loop that reaches the outside only as a
//! number inside a refusal message. Adding them is an engine change and this
//! ticket does not make one.
//!
//! What is asserted instead is a value rather than a clock, which is what rule
//! 1.7 is about: with a timeout the second writer's row **is in the table**,
//! and without one it **is not** and the refusal names the holder and the
//! budget. The sleeps decide when the contention ends; nothing reads a duration
//! and compares it to a bound.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use inillucent_compat::cliproc::{program, run};
use inillucent_compat::workspace_root;

/// How long the first writer holds the file.
///
/// It decides when the contention ends and is not compared to anything.
const HELD: Duration = Duration::from_millis(800);

/// How long to give a child to reach the statement that takes the lock.
const SETTLING: Duration = Duration::from_millis(400);

/// A scratch directory of one case's own, emptied first.
///
/// @param case - what to name it after
fn area(case: &str) -> PathBuf {
    let directory = workspace_root()
        .join("_agent_output/busy-timeout")
        .join(case);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// Builds the database both writers write to.
///
/// @param binary - the built `inillucent`
/// @param directory - where to put it
fn prepared(binary: &Path, directory: &Path) -> PathBuf {
    let database = directory.join("shared.rdb");
    let path = database.to_string_lossy().to_string();
    let made = run(binary, &["create", path.as_str()]);
    assert_eq!(made.code, 0, "`create` failed:\n{}", made.said());
    let built = run(
        binary,
        &[
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE note (id INTEGER PRIMARY KEY, who TEXT NOT NULL)",
        ],
    );
    assert_eq!(built.code, 0, "the schema failed:\n{}", built.said());
    database
}

/// A shell this test is feeding one line at a time.
struct Fed {
    /// The process.
    child: Child,
    /// Its output, read a line at a time.
    said: BufReader<ChildStdout>,
    /// Everything it wrote on standard error, collected by a thread of its own.
    ///
    /// **A refusal goes to standard error, and the first version of this suite
    /// read only standard output.** With `PRAGMA busy_timeout = 0` the shell
    /// printed `another process holds the file` on the error stream and the
    /// case saw nothing but its own marker, so it reported that a writer with
    /// no timeout had not been refused. A thread rather than a read at the end,
    /// because a full error pipe blocks the child.
    complained: Arc<Mutex<String>>,
}

impl Fed {
    /// Starts a shell on a database and reads nothing yet.
    ///
    /// @param shell - the built `inillucent-shell`
    /// @param database - the file to open
    fn start(shell: &Path, database: &Path) -> Fed {
        let mut child = Command::new(shell)
            .arg(database.to_string_lossy().replace('\\', "/"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("the shell did not start: {error}"));
        let said = BufReader::new(
            child
                .stdout
                .take()
                .unwrap_or_else(|| panic!("the shell has no standard output")),
        );
        let complained = Arc::new(Mutex::new(String::new()));
        if let Some(errors) = child.stderr.take() {
            let collected = Arc::clone(&complained);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(errors);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(mut held) = collected.lock() {
                                held.push_str(&line);
                            }
                        }
                    }
                }
            });
        }
        Fed {
            child,
            said,
            complained,
        }
    }

    /// Everything the shell has written on standard error so far.
    fn errors(&self) -> String {
        self.complained
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    /// Writes one line and does not wait for anything.
    ///
    /// @param line - the statement, without its newline
    fn send(&mut self, line: &str) {
        let Some(pipe) = self.child.stdin.as_mut() else {
            panic!("the shell's input is already closed");
        };
        let _ = writeln!(pipe, "{line}");
        let _ = pipe.flush();
    }

    /// Reads lines until one carries a marker.
    ///
    /// **A marker rather than a sleep.** What has to be true before the next
    /// step is that the child has *finished* a statement, and the only thing
    /// that says so is the child.
    ///
    /// @param marker - the text to wait for
    fn until(&mut self, marker: &str) -> String {
        let mut seen = String::new();
        loop {
            let mut line = String::new();
            match self.said.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    panic!("the shell ended before it printed `{marker}`; it had said:\n{seen}")
                }
                Ok(_) => {
                    seen.push_str(&line);
                    if line.contains(marker) {
                        return seen;
                    }
                }
            }
        }
    }

    /// Closes the input and waits for the shell to finish.
    fn finish(mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

/// How many rows one writer put in the table.
///
/// @param binary - the built `inillucent`
/// @param database - the file
/// @param who - the writer's name
fn rows_by(binary: &Path, database: &Path, who: &str) -> usize {
    let asked = run(
        binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "query",
            &format!("SELECT count(*) AS n FROM note WHERE who = '{who}'"),
            "--output",
            "json",
        ],
    );
    assert_eq!(
        asked.code,
        0,
        "counting {who}'s rows failed:\n{}",
        asked.said()
    );
    inillucent_compat::cliproc::rows(&asked.stdout)
        .first()
        .and_then(|row| row.first())
        .and_then(|cell| cell.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("the count did not come back as a number:\n{}", asked.stdout))
}

/// Runs one arm: a second writer with `timeout_ms`, contending with a holder.
///
/// @param shell - the built `inillucent-shell`
/// @param database - the file
/// @param timeout_ms - what the second writer sets `PRAGMA busy_timeout` to
/// @returns everything the second writer printed while it contended
fn contend(shell: &Path, database: &Path, timeout_ms: u64) -> String {
    // The second writer opens first, on a file nobody holds, so the open never
    // contends and its own default budget never applies.
    let mut second = Fed::start(shell, database);
    second.send(&format!("PRAGMA busy_timeout = {timeout_ms};"));
    second.send("SELECT 'second-is-open';");
    second.until("second-is-open");

    // Now the holder takes the write lock and keeps it.
    let mut holder = Fed::start(shell, database);
    holder.send("BEGIN;");
    holder.send("INSERT INTO note (who) VALUES ('holder');");
    holder.send("SELECT 'holder-has-it';");
    holder.until("holder-has-it");
    std::thread::sleep(SETTLING);

    // The second writer meets it busy. Its answer is read after the holder has
    // let go, because a timeout long enough to wait is still waiting now.
    second.send("INSERT INTO note (who) VALUES ('second');");
    second.send("SELECT 'second-is-done';");
    std::thread::sleep(HELD);

    holder.send("COMMIT;");
    holder.finish();

    let printed = second.until("second-is-done");
    let complained = second.errors();
    second.finish();
    format!("{printed}{complained}")
}

/// A writer given a timeout waits for a busy file and then writes.
#[test]
fn a_writer_with_a_timeout_waits_and_then_succeeds() {
    let (Some(binary), Some(shell)) = (program("inillucent"), program("inillucent-shell")) else {
        return;
    };
    let directory = area("waits");
    let database = prepared(&binary, &directory);

    let printed = contend(&shell, &database, 30_000);
    assert!(
        !printed.to_ascii_lowercase().contains("another process"),
        "a writer with a thirty second timeout was refused:\n{printed}"
    );
    assert_eq!(
        rows_by(&binary, &database, "second"),
        1,
        "the second writer waited and its row is not in the table:\n{printed}"
    );
    assert_eq!(
        rows_by(&binary, &database, "holder"),
        1,
        "the holder's row is not in the table, so nothing was holding the file and this case \
         was not about contention at all"
    );
}

/// A writer given no timeout is refused at once, and the refusal names the
/// holder and the budget.
///
/// The counting version of the deleted in-process test: no duration is read,
/// and what separates this from the case above is a row that is not there.
#[test]
fn a_writer_with_no_timeout_is_refused_and_writes_nothing() {
    let (Some(binary), Some(shell)) = (program("inillucent"), program("inillucent-shell")) else {
        return;
    };
    let directory = area("refused");
    let database = prepared(&binary, &directory);

    let printed = contend(&shell, &database, 0);
    let refusal = printed.to_ascii_lowercase();
    assert!(
        refusal.contains("another process holds the file"),
        "a writer with `PRAGMA busy_timeout = 0` was not refused, or the refusal does not name \
         the holder:\n{printed}"
    );
    assert!(
        refusal.contains("writing"),
        "the refusal does not say what the holder is doing with the file:\n{printed}"
    );
    assert!(
        refusal.contains("0 ms pragma busy_timeout"),
        "the refusal does not name the budget it was given, so a caller cannot tell a timeout \
         of zero from one that elapsed:\n{printed}"
    );
    assert_eq!(
        rows_by(&binary, &database, "second"),
        0,
        "the second writer was refused and its row is in the table anyway"
    );
    assert_eq!(
        rows_by(&binary, &database, "holder"),
        1,
        "the holder's row is not in the table, so nothing was holding the file"
    );
}

/// A timeout shorter than the hold gives up, and says which budget elapsed.
///
/// The third arm, and the one that separates "waits" from "waits long enough":
/// the budget is a real number rather than a switch between blocking for ever
/// and not blocking at all. The 50 is what the refusal has to name; nothing
/// compares it to how long anything took.
#[test]
fn a_timeout_shorter_than_the_hold_gives_up_and_names_its_budget() {
    let (Some(binary), Some(shell)) = (program("inillucent"), program("inillucent-shell")) else {
        return;
    };
    let directory = area("short");
    let database = prepared(&binary, &directory);

    let printed = contend(&shell, &database, 50);
    let refusal = printed.to_ascii_lowercase();
    assert!(
        refusal.contains("another process holds the file"),
        "a writer whose budget elapsed was not refused:\n{printed}"
    );
    assert!(
        refusal.contains("50 ms pragma busy_timeout"),
        "the refusal does not name the 50 ms budget, so a caller cannot tell which timeout \
         elapsed - and if it names 5000, the waiting was the open retrying on its own default \
         rather than this pragma:\n{printed}"
    );
    assert_eq!(
        rows_by(&binary, &database, "second"),
        0,
        "the writer whose budget elapsed wrote its row anyway"
    );
}
