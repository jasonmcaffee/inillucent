//! Two real writer processes share one database file, and nothing acknowledged
//! is lost.
//!
//! Invariant: **the number of rows in the file equals the number of commits the
//! engine acknowledged.** A child that was refused with a busy error and exited
//! non-zero is a correct outcome and is not counted; a child that exited zero
//! and whose row is missing is the failure this file exists to catch.
//!
//! **Nothing in the tree could see this before (task-1979, section 4).**
//! `concurrency.rs` opens two sessions inside one process, which share a buffer
//! pool, a log handle and a lock. The defect is entirely between processes:
//! each one read the meta record and the log's tail at `open`, before it held
//! the file lock, and then trusted both afterwards - so two processes computed
//! the same append position and each wrote over the other's records. Measured
//! on the build at `3073058`: 120 acknowledged inserts, 60 rows present,
//! `integrity-check ok`, every process exit 0.
//!
//! **Both locking modes, because they failed differently.** Under `exclusive`
//! the lock is never released between statements, so a second process read
//! state from before the first process's whole life; under `normal` the loss
//! was the rows written before another process's checkpoint moved the
//! generation, which the cache discard then threw away.
//!
//! **One process per statement is the shape that matters most.** It is what
//! `inillucent exec` does and what the MCP server does for every tool call, so
//! it is the deployment this engine is most often in rather than a corner.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use inillucent_compat::cliproc::{program, rows, run, Ran};
use inillucent_compat::workspace_root;
use inillucent_pool::journal::{replay_hot_journal, Journal, JournalMode};
use inillucent_pool::page::PageId;
use inillucent_vfs::{AccessMode, DbPath, OpenOptions, OsVfs, Vfs, VfsFile, VfsResult};

/// How many inserts each writer sends in the one-statement-per-process shape.
///
/// Sixty per writer rather than more: the defect this catches lost rows on
/// every round at sixty, and each insert is a process start, so the number is
/// the smallest one that failed reliably rather than the largest one that would
/// have.
const ONE_SHOT_INSERTS: usize = 60;

/// How many inserts each long-lived writer sends.
const LONG_LIVED_INSERTS: usize = 300;

/// Returns a directory of this case's own, emptied first.
///
/// @param name - the case's name, which is also the directory's
fn area(name: &str) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/process-concurrency")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Builds an empty database holding the table both writers insert into.
///
/// **No `INTEGER PRIMARY KEY`, and `UNIQUE(who, n)`.** A missing row in a table
/// with a rowid could be two writers colliding on one rowid, which is a
/// different defect with a different fix; with this shape a row from `a` can
/// never be a row from `b`, so a row that is not there was discarded rather
/// than replaced, and a writer that wrote one twice would be refused rather
/// than silently deduplicated.
///
/// @param binary - the built `inillucent`
/// @param directory - where to put the file
fn prepared(binary: &Path, directory: &Path) -> PathBuf {
    let database = directory.join("shared.rdb");
    let path = database.to_string_lossy().to_string();
    for arguments in [
        vec!["create", path.as_str()],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE note (who TEXT NOT NULL, n INTEGER NOT NULL, UNIQUE(who, n))",
        ],
    ] {
        let ran = run(binary, &arguments);
        assert_eq!(
            ran.code,
            0,
            "preparing the database failed at {arguments:?}:\n{}",
            ran.said()
        );
    }
    database
}

/// Returns how many rows the file holds, read by a process that wrote none of
/// them.
///
/// @param binary - the built `inillucent`
/// @param database - the file to read
fn present(binary: &Path, database: &Path) -> usize {
    let ran = run(
        binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "query",
            "SELECT count(*) FROM note",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        ran.code,
        0,
        "counting the rows afterwards failed:\n{}",
        ran.said()
    );
    rows(&ran.stdout)
        .first()
        .and_then(|row| row.first())
        .and_then(|cell| cell.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("the count is not a number:\n{}", ran.stdout))
}

/// Runs one writer's whole sequence of single-statement processes, and returns
/// how many of them exited zero.
///
/// `batch` rather than `exec` because the locking mode has to be set in the
/// same process as the insert, and `exec` runs one statement. The transaction
/// `batch` opens around the two is the same transaction `exec` opens around its
/// one, so the shape under test is unchanged.
///
/// @param binary - the built `inillucent`
/// @param database - the file to write to
/// @param who - which writer this is
/// @param mode - the `locking_mode` each process sets
/// @param inserts - how many inserts to send
fn one_shot_writer(binary: &Path, database: &Path, who: &str, mode: &str, inserts: usize) -> usize {
    let path = database.to_string_lossy().to_string();
    let mut acknowledged = 0usize;
    for n in 1..=inserts {
        let sql = format!(
            "PRAGMA locking_mode = {mode}; INSERT INTO note (who, n) VALUES ('{who}', {n})"
        );
        let ran = run(binary, &["--db", path.as_str(), "batch", sql.as_str()]);
        if ran.code == 0 {
            acknowledged = acknowledged.saturating_add(1);
        }
    }
    acknowledged
}

/// Runs a script through `inillucent-shell` and returns what it produced.
///
/// @param shell - the built `inillucent-shell`
/// @param database - the file to open
/// @param script - the statements, newline separated
fn shell_script(shell: &Path, database: &Path, script: &str) -> Ran {
    let mut child = Command::new(shell)
        .arg(database.to_string_lossy().replace('\\', "/"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("the shell did not start: {error}"));
    if let Some(pipe) = child.stdin.as_mut() {
        let _ = pipe.write_all(script.as_bytes());
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("the shell did not finish: {error}"));
    Ran {
        code: output.status.code().unwrap_or(130),
        stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
    }
}

/// Two writers, one process per statement, lose nothing, under both modes.
#[test]
fn two_writer_processes_lose_nothing_one_statement_each() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    for mode in ["normal", "exclusive"] {
        let directory = area(&format!("one-shot-{mode}"));
        let database = prepared(&binary, &directory);
        let acknowledged = std::thread::scope(|scope| {
            let left = {
                let binary = binary.clone();
                let database = database.clone();
                scope
                    .spawn(move || one_shot_writer(&binary, &database, "a", mode, ONE_SHOT_INSERTS))
            };
            let right = {
                let binary = binary.clone();
                let database = database.clone();
                scope
                    .spawn(move || one_shot_writer(&binary, &database, "b", mode, ONE_SHOT_INSERTS))
            };
            let a = left.join().expect("writer a finished");
            let b = right.join().expect("writer b finished");
            a.saturating_add(b)
        });
        assert!(
            acknowledged > 0,
            "mode {mode}: no insert was acknowledged, so this round tested nothing"
        );
        assert_eq!(
            present(&binary, &database),
            acknowledged,
            "mode {mode}: the file does not hold every acknowledged insert"
        );
    }
}

/// Two long-lived writers, autocommitting, lose nothing, under both modes.
#[test]
fn two_writer_processes_lose_nothing_long_lived() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    for mode in ["normal", "exclusive"] {
        let directory = area(&format!("long-lived-{mode}"));
        let database = prepared(&binary, &directory);
        let script = |who: &str| {
            let mut text = format!("PRAGMA locking_mode = {mode};\n");
            for n in 1..=LONG_LIVED_INSERTS {
                text.push_str(&format!(
                    "INSERT INTO note (who, n) VALUES ('{who}', {n});\n"
                ));
            }
            // The sentinel, for the reason `acknowledged_by` gives: the shell
            // stops at the first statement that fails, so a writer refused part
            // way never ran the statements after it and cannot be credited with
            // them. Line one here is the `PRAGMA`, so the arithmetic is the
            // same one the attached case uses.
            text.push_str(&format!("SELECT 'finished-{who}';\n"));
            text
        };
        let acknowledged = std::thread::scope(|scope| {
            let left = {
                let shell = shell.clone();
                let database = database.clone();
                let text = script("a");
                scope.spawn(move || shell_script(&shell, &database, &text))
            };
            let right = {
                let shell = shell.clone();
                let database = database.clone();
                let text = script("b");
                scope.spawn(move || shell_script(&shell, &database, &text))
            };
            let a = left.join().expect("writer a finished");
            let b = right.join().expect("writer b finished");
            acknowledged_by(&a.said(), "a").saturating_add(acknowledged_by(&b.said(), "b"))
        });
        assert!(
            acknowledged > 0,
            "mode {mode}: no insert was acknowledged, so this round tested nothing"
        );
        assert_eq!(
            present(&binary, &database),
            acknowledged,
            "mode {mode}: the file does not hold every acknowledged insert"
        );
    }
}

/// Two processes with different main databases attaching one shared file lose
/// nothing (task-1979, C2).
#[test]
fn two_processes_attaching_one_file_lose_nothing() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    let directory = area("attached");
    let shared = prepared(&binary, &directory);
    let own = |who: &str| {
        let path = directory.join(format!("own-{who}.rdb"));
        let ran = run(&binary, &["create", &path.to_string_lossy()]);
        assert_eq!(
            ran.code,
            0,
            "creating {who}'s own database:\n{}",
            ran.said()
        );
        path
    };
    let script = |who: &str| {
        let mut text = format!(
            "ATTACH '{}' AS shared;\n",
            shared.to_string_lossy().replace('\\', "/")
        );
        for n in 1..=LONG_LIVED_INSERTS {
            text.push_str(&format!(
                "INSERT INTO shared.note (who, n) VALUES ('{who}', {n});\n"
            ));
        }
        // **The sentinel, which is how many inserts ran is decided from.** The
        // shell stops at the first statement that fails, so a writer refused at
        // line 116 never ran the 184 statements after it - and counting
        // "three hundred minus the error lines" then claimed 299 acknowledged
        // inserts where 114 had happened, and read the difference as a lost
        // write. A busy timeout under load is an ordinary outcome of two
        // processes sharing a file; losing an insert that reported success is
        // not, and that is the one this case is about.
        text.push_str(&format!("SELECT 'finished-{who}';\n"));
        text
    };
    let left_own = own("a");
    let right_own = own("b");
    let acknowledged = std::thread::scope(|scope| {
        let left = {
            let shell = shell.clone();
            let text = script("a");
            let path = left_own.clone();
            scope.spawn(move || shell_script(&shell, &path, &text))
        };
        let right = {
            let shell = shell.clone();
            let text = script("b");
            let path = right_own.clone();
            scope.spawn(move || shell_script(&shell, &path, &text))
        };
        let a = left.join().expect("writer a finished");
        let b = right.join().expect("writer b finished");
        (
            acknowledged_by(&a.said(), "a").saturating_add(acknowledged_by(&b.said(), "b")),
            a.said(),
            b.said(),
        )
    });
    let (acknowledged, said_a, said_b) = acknowledged;
    assert!(
        acknowledged > 0,
        "no attached insert was acknowledged, so this case tested nothing"
    );
    // **What each writer said, on a failure only.** A count that does not add
    // up is the beginning of the question rather than the end of it: the two
    // interesting shapes are a writer that was refused in a way `refusals` does
    // not recognise, and a writer that reported nothing and wrote nothing.
    assert_eq!(
        present(&binary, &shared),
        acknowledged,
        "the attached file does not hold every acknowledged insert.\n\
         writer a said:\n{}\nwriter b said:\n{}",
        first_lines(&said_a),
        first_lines(&said_b)
    );
}

/// Returns how many inserts one writer reported success for.
///
/// **The shell stops at the first statement that fails**, so the count is not
/// "how many were written minus how many errors were printed". Two shapes:
///
/// - the sentinel is in the output, so every statement ran, and the count is
///   the inserts minus the ones that reported an error of their own;
/// - the sentinel is absent, so the shell stopped, and the count is the number
///   of insert lines before the one it stopped at. Line one is the `ATTACH`, so
///   an error at line N means N minus two inserts got through.
///
/// An error on line one is the `ATTACH` itself reporting something - a stale
/// journal it could not remove, for instance - and is not an insert, which is
/// why the line number is part of the question rather than just the word.
///
/// @param said - both of the writer's streams
/// @param who - the writer's name, which its sentinel carries
fn acknowledged_by(said: &str, who: &str) -> usize {
    let finished = said.contains(&format!("finished-{who}"));
    let failures: Vec<usize> = said
        .lines()
        .filter_map(error_line)
        .filter(|at| *at >= 2)
        .collect();
    if finished {
        return LONG_LIVED_INSERTS.saturating_sub(failures.len());
    }
    match failures.first() {
        Some(at) => at.saturating_sub(2),
        None => 0,
    }
}

/// Returns the line number an error line names, when it names one.
///
/// @param line - one line of a writer's output
fn error_line(line: &str) -> Option<usize> {
    let folded = line.to_ascii_lowercase();
    if !folded.contains("error") {
        return None;
    }
    let (_, rest) = folded.split_once("near line ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Returns the first few lines of what a writer said, for a failure message.
///
/// @param said - both of the writer's streams
fn first_lines(said: &str) -> String {
    let held: Vec<&str> = said.lines().take(12).collect();
    match held.is_empty() {
        true => "(nothing at all)".to_string(),
        false => held.join("\n"),
    }
}

/// A read only process reads a file a writer has open, rather than waiting out
/// a budget and reporting the writer's lock (task-1979, C5).
#[test]
fn a_readonly_process_reads_while_a_writer_holds_the_file() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    let directory = area("readonly-reader");
    let database = prepared(&binary, &directory);
    let ran = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "exec",
            "INSERT INTO note (who, n) VALUES ('a', 1)",
        ],
    );
    assert_eq!(ran.code, 0, "seeding the file:\n{}", ran.said());

    // A writer that opens the file, writes, and then sits on its own standard
    // input with the connection open. The parent reads while it is there.
    let mut writer = Command::new(&shell)
        .arg(database.to_string_lossy().replace('\\', "/"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("the writer did not start: {error}"));
    {
        let pipe = writer.stdin.as_mut().expect("the writer takes input");
        let _ = pipe
            .write_all(b"INSERT INTO note (who, n) VALUES ('a', 2);\nSELECT count(*) FROM note;\n");
        let _ = pipe.flush();
    }
    // Long enough for the child to have opened the file and run its statement.
    std::thread::sleep(std::time::Duration::from_millis(1_500));

    let started = std::time::Instant::now();
    let read = run(
        &binary,
        &[
            "--readonly",
            "--db",
            &database.to_string_lossy(),
            "query",
            "SELECT count(*) FROM note",
            "--output",
            "json",
        ],
    );
    let waited = started.elapsed();
    drop(writer.stdin.take());
    let _ = writer.wait();

    assert_eq!(
        read.code,
        0,
        "a read only process could not read a file a writer has open, after {waited:?}:\n{}",
        read.said()
    );
    let counted = rows(&read.stdout)
        .first()
        .and_then(|row| row.first())
        .and_then(|cell| cell.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("the read only count is not a number:\n{}", read.stdout));
    assert!(
        counted >= 1,
        "the read only process read {counted} rows from a file holding at least one"
    );
}

/// A refusal under contention names who holds the file and what they are doing
/// with it (task-1979, C6).
#[test]
fn a_refusal_names_the_holder_and_the_operation() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    let directory = area("refusal-text");
    let database = prepared(&binary, &directory);

    // A writer inside an open transaction, which is the one state that holds
    // the file against another writer for longer than a statement.
    let mut writer = Command::new(&shell)
        .arg(database.to_string_lossy().replace('\\', "/"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("the writer did not start: {error}"));
    {
        let pipe = writer.stdin.as_mut().expect("the writer takes input");
        let _ = pipe.write_all(b"BEGIN;\nINSERT INTO note (who, n) VALUES ('a', 1);\n");
        let _ = pipe.flush();
    }
    std::thread::sleep(std::time::Duration::from_millis(1_500));

    let refused = run(
        &binary,
        &[
            "--db",
            &database.to_string_lossy(),
            "exec",
            "INSERT INTO note (who, n) VALUES ('b', 1)",
        ],
    );
    drop(writer.stdin.take());
    let _ = writer.wait();

    assert_ne!(
        refused.code,
        0,
        "a second writer was let in while a transaction was open:\n{}",
        refused.said()
    );
    let said = refused.said().to_ascii_lowercase();
    assert!(
        said.contains("another process"),
        "the refusal does not say another process holds the file:\n{}",
        refused.said()
    );
    assert!(
        said.contains("writing"),
        "the refusal does not say what the holder is doing:\n{}",
        refused.said()
    );
    assert!(
        !said.contains("pending"),
        "the refusal still names an internal lock level:\n{}",
        refused.said()
    );
}

/// A VFS that disposes of one journal the moment its database is opened.
///
/// **What it stands in for is a checkpoint finishing, and it stands in for it
/// without a race.** The defect below is an ordering, and the ordering is
/// between one process reading a journal and another process disposing of it;
/// reproducing that with two threads and a sleep gets a case that passes on a
/// broken build whenever the machine is busy, which is the shape
/// `tests/inillucent-testing-tdd.md` calls worse than no test. So the disposal
/// is hung off the one call both builds make at the same point in their own
/// control flow - opening the database to take its lock - and fires exactly
/// once. Neither build is told anything the other is not; they differ only in
/// whether they had already read the journal by the time it fired.
#[derive(Debug)]
struct DisposingVfs {
    /// The real VFS every call is delegated to.
    inner: OsVfs,
    /// The database whose opening is the moment the journal goes.
    database: PathBuf,
    /// The journal to dispose of.
    journal: DbPath,
    /// Whether it has been disposed of, so this happens once.
    disposed: std::sync::atomic::AtomicBool,
}

impl Vfs for DisposingVfs {
    /// Names the VFS underneath.
    fn name(&self) -> &str {
        self.inner.name()
    }

    /// Opens a file, disposing of the journal first when this is the database.
    ///
    /// @param path - the file to open
    /// @param options - how to open it
    fn open(&self, path: &DbPath, options: OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        if path.as_path() == self.database
            && !self
                .disposed
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let _ = self.inner.delete(&self.journal, false);
        }
        self.inner.open(path, options)
    }

    /// @param path - the file to remove
    /// @param sync_dir - whether to flush the directory entry
    fn delete(&self, path: &DbPath, sync_dir: bool) -> VfsResult<()> {
        self.inner.delete(path, sync_dir)
    }

    /// @param from - the existing name
    /// @param to - the new name
    fn rename(&self, from: &DbPath, to: &DbPath) -> VfsResult<()> {
        self.inner.rename(from, to)
    }

    /// @param path - the file to ask about
    /// @param mode - what is being asked
    fn access(&self, path: &DbPath, mode: AccessMode) -> VfsResult<bool> {
        self.inner.access(path, mode)
    }

    /// @param path - the path to resolve
    fn full_pathname(&self, path: &DbPath) -> VfsResult<DbPath> {
        self.inner.full_pathname(path)
    }

    /// @param output - the buffer to fill
    fn randomness(&self, output: &mut [u8]) -> VfsResult<()> {
        self.inner.randomness(output)
    }

    /// Returns the wall clock the VFS underneath reports.
    fn current_time(&self) -> VfsResult<std::time::SystemTime> {
        self.inner.current_time()
    }

    /// @param prefix - what to name the temporary file after
    fn temp_path(&self, prefix: &str) -> VfsResult<DbPath> {
        self.inner.temp_path(prefix)
    }

    /// @param micros - how long to wait
    fn sleep(&self, micros: u64) -> VfsResult<()> {
        self.inner.sleep(micros)
    }
}

/// Returns the one byte a page is filled with, straight out of the file.
///
/// The case below writes each version of the page as a single repeated byte,
/// so which version the file holds is one number rather than five hundred and
/// twelve of them - and a failure prints that number instead of two pages of
/// them.
///
/// @param path - the file
/// @param page - which page
/// @param page_size - how big a page is
fn page_marker(path: &Path, page: usize, page_size: usize) -> u8 {
    let whole =
        std::fs::read(path).unwrap_or_else(|error| panic!("the file did not read: {error}"));
    let at = page.saturating_mul(page_size);
    let image = whole
        .get(at..at.saturating_add(page_size))
        .unwrap_or_else(|| panic!("the file is shorter than page {page}"));
    let first = image
        .first()
        .copied()
        .unwrap_or_else(|| panic!("page {page} is empty"));
    assert!(
        image.iter().all(|byte| *byte == first),
        "page {page} is not one repeated byte, so this case cannot name which version it holds"
    );
    first
}

/// A journal is not put back over the checkpoint that owned it (task-1987).
///
/// **The defect, stated as the value of one page.** `replay_hot_journal` opened
/// the journal beside a database and read its header *before* it took any lock
/// on that database, and then waited for the lock. The lock chain was there and
/// the argument written beside it was right - a concurrent holder means the
/// journal is not this process's - but it ran after the read, and nothing asked
/// again once the lock was in hand. So a journal belonging to a checkpoint that
/// finished while this process waited was still written back, over a commit
/// that had already been acknowledged. Deleting a journal does not take it away
/// from a process that already has it open, on Windows or on POSIX.
///
/// Measured through the command line on the build this case was written
/// against, with a microsecond clock on the lock, meta and journal decisions:
/// two writer processes running single statement inserts, one of them writing
/// generation 28 and reading it back through its own handle, and the file
/// reporting generation 27 again eleven milliseconds later because a third
/// process had put that checkpoint's own journal back. `integrity-check` said
/// `ok`, and between two and six of every two hundred and forty acknowledged
/// inserts were not in the file.
///
/// The file here holds the image the checkpoint wrote, and the journal holds
/// the image from before it. Putting the journal back is the whole failure, so
/// the assertion is on the page's bytes.
#[test]
fn a_finished_checkpoints_journal_is_not_put_back_over_it() {
    const PAGE_SIZE: usize = 512;
    const BEFORE: u8 = 0xA5;
    const AFTER: u8 = 0x5C;
    let directory = area("hot-journal-after-checkpoint");
    let database = directory.join("shared.rdb");
    let before_the_checkpoint = vec![BEFORE; PAGE_SIZE];
    let after_the_checkpoint = vec![AFTER; PAGE_SIZE];
    std::fs::write(&database, &before_the_checkpoint)
        .unwrap_or_else(|error| panic!("the database did not write: {error}"));

    let path = DbPath::new(database.to_string_lossy().as_ref());
    let journal_path = path.journal();

    // The pre-image a checkpoint saves before it overwrites the page, written
    // by the same code a checkpoint writes it with.
    let real: std::sync::Arc<dyn Vfs> = std::sync::Arc::new(OsVfs::new());
    let mut journal = Journal::new(
        std::sync::Arc::clone(&real),
        &path,
        JournalMode::Delete,
        PAGE_SIZE,
    );
    journal
        .save(PageId(0), &before_the_checkpoint)
        .unwrap_or_else(|error| panic!("the pre-image did not save: {error}"));
    journal
        .seal()
        .unwrap_or_else(|error| panic!("the journal did not seal: {error}"));
    assert!(
        real.access(&journal_path, AccessMode::Exists)
            .unwrap_or(false),
        "the journal this case is about was never written, so it tests nothing"
    );

    // The rest of that checkpoint: the new image reaches the file. Its own
    // disposal of the journal is what `DisposingVfs` performs, at the moment
    // the replayer reaches for the lock.
    std::fs::write(&database, &after_the_checkpoint)
        .unwrap_or_else(|error| panic!("the new image did not write: {error}"));

    let disposing = DisposingVfs {
        inner: OsVfs::new(),
        database: database.clone(),
        journal: journal_path.clone(),
        disposed: std::sync::atomic::AtomicBool::new(false),
    };
    let restored = replay_hot_journal(&disposing, &path)
        .unwrap_or_else(|error| panic!("the replay reported an error: {error}"));

    assert_eq!(
        page_marker(&database, 0, PAGE_SIZE),
        AFTER,
        "page 0 holds {BEFORE:#x}, from before the checkpoint, not the {AFTER:#x} the checkpoint wrote: a finished checkpoint's journal was put back over it, undoing an acknowledged commit"
    );
    assert!(
        !restored,
        "nothing was there to put back, yet the replay reported that it had"
    );
}
