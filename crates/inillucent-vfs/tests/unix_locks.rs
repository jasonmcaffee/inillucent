//! What POSIX does with a byte-range lock that the conformance suite cannot
//! ask about.
//!
//! Invariant: **an `fcntl` lock belongs to the process, so the table in
//! `os/unix.rs` is what makes two handles in one process exclude each other -
//! and the kernel lock has to survive one of them closing.** Those are the two
//! things POSIX gets wrong on its own, and neither is visible to a suite that
//! drives one handle at a time.
//!
//! `conformance.rs` runs the same twenty-odd cases against memory and the real
//! file system and proves cross-process locking with `inillucent-lock-probe`.
//! What it never asks is what one process's *second* handle sees. On Windows
//! the kernel answers, which is what `windows_locks.rs` asserts; here the
//! kernel answers "no conflict" to its own process and `LockState` has to
//! answer instead. Two code paths, one contract.
//!
//! The closing trap is the sharper one. `close()` on **any** descriptor for a
//! file releases every `fcntl` lock this process holds on it, whichever
//! descriptor took them - so a second connection dropping its handle would have
//! silently unlocked the first connection's database, and nothing in this
//! process could tell. `os/unix.rs` keeps one shared descriptor per file
//! identity for that reason, and the last test here is a second process
//! checking that the lock is still there.
//!
//! task-1913 found a Windows-only silent no-op in `link_directory`, which is
//! why T4 asks for these: a generic suite over three backends can pass while
//! one backend does nothing.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use inillucent_base::error::PrimaryCode;
use inillucent_vfs::contract::{FileLock, OpenOptions, Vfs, VfsFile};
use inillucent_vfs::os::OsVfs;
use inillucent_vfs::path::DbPath;

/// Creates a fresh directory for one test and returns the database path in it.
///
/// @param name - the test's name
fn scratch(name: &str) -> DbPath {
    let mut root = std::env::temp_dir();
    root.push(format!(
        "inillucent-vfs-unix-locks/{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the temporary directory is writable");
    root.push("db.rdb");
    DbPath::new(root)
}

/// Opens one handle on the database, creating it when it is not there.
///
/// @param vfs - the file system
/// @param path - the database
fn handle(vfs: &OsVfs, path: &DbPath) -> Box<dyn VfsFile> {
    vfs.open(path, OpenOptions::main_db())
        .expect("the database opens")
}

/// A second process holding the probe's locks.
struct Probe {
    child: Child,
    input: ChildStdin,
    output: BufReader<std::process::ChildStdout>,
}

impl Probe {
    /// Starts the probe and opens `path` in it.
    ///
    /// @param path - the database both processes are holding
    fn start(path: &DbPath) -> Probe {
        let mut child = Command::new(env!("CARGO_BIN_EXE_inillucent-lock-probe"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the probe binary was built by cargo");
        let input = child.stdin.take().expect("stdin was piped");
        let output = BufReader::new(child.stdout.take().expect("stdout was piped"));
        let mut probe = Probe {
            child,
            input,
            output,
        };
        assert_eq!(probe.send(&format!("open {}", path.display())), "ok");
        probe
    }

    /// Sends one command and returns the reply line.
    ///
    /// @param command - the line to send
    fn send(&mut self, command: &str) -> String {
        writeln!(self.input, "{command}").expect("the probe is still running");
        self.input.flush().expect("the probe is still running");
        let mut reply = String::new();
        self.output
            .read_line(&mut reply)
            .expect("the probe replied");
        reply.trim().to_string()
    }
}

impl Drop for Probe {
    /// Stops the probe so a failed assertion cannot leave a process behind
    /// holding a lock on a file the next test wants.
    fn drop(&mut self) {
        let _ = writeln!(self.input, "exit");
        let _ = self.input.flush();
        let _ = self.child.wait();
    }
}

/// A second handle in the same process is refused a read lock while the first
/// holds EXCLUSIVE.
///
/// **The kernel would allow this**, which is the whole reason `LockState` keeps
/// a table. An `fcntl` range belongs to the process, so the second handle's
/// request does not conflict with the first's as far as POSIX is concerned;
/// without the table both connections would believe they held the file. The
/// refusal is `SQLITE_BUSY`, which is the same answer `windows_locks.rs`
/// asserts for the same case through a different mechanism.
#[test]
fn a_second_handle_in_this_process_is_refused_the_read_lock() {
    let path = scratch("exclusive");
    let vfs = OsVfs::new();
    let writer = handle(&vfs, &path);
    let reader = handle(&vfs, &path);

    writer
        .lock(FileLock::Exclusive)
        .expect("nothing else holds the file");
    let refused = reader
        .lock(FileLock::Shared)
        .expect_err("a reader cannot join a writer");
    assert_eq!(
        refused.code(),
        PrimaryCode::Busy,
        "the refusal is BUSY, which is what a caller retries on; it said {:?}",
        refused.detail()
    );
    assert_eq!(
        reader.lock_level(),
        FileLock::None,
        "a refused acquisition leaves the handle at the level it had"
    );

    writer.unlock(FileLock::None).expect("the writer lets go");
    reader
        .lock(FileLock::Shared)
        .expect("and then the reader can take its lock");
}

/// Two readers in one process both hold SHARED at once, and the writer that
/// wants the file waits for both.
///
/// The PENDING byte is taken *shared* here rather than exclusively, which is
/// why there is no retry loop on this platform and why two readers arriving
/// together do not collide. `windows_locks.rs` asserts the other half: there
/// the same byte can only be taken exclusively, so the reader retries.
#[test]
fn two_readers_in_this_process_share_the_file() {
    let path = scratch("two-readers");
    let vfs = OsVfs::new();
    let first = handle(&vfs, &path);
    let second = handle(&vfs, &path);

    first.lock(FileLock::Shared).expect("the first reader");
    second.lock(FileLock::Shared).expect("the second reader");
    assert_eq!(first.lock_level(), FileLock::Shared);
    assert_eq!(second.lock_level(), FileLock::Shared);

    let refused = first
        .lock(FileLock::Exclusive)
        .expect_err("a reader is still present");
    assert_eq!(
        refused.code(),
        PrimaryCode::Busy,
        "a writer waits for the readers to leave, and says so with BUSY"
    );
}

/// RESERVED is visible to another handle in this process.
///
/// Answered from the table rather than by probing the byte, because probing it
/// would succeed: this process already holds whatever it holds. A build that
/// probed here would tell every connection that nobody was writing.
#[test]
fn a_reserved_lock_is_visible_to_another_handle_here() {
    let path = scratch("reserved");
    let vfs = OsVfs::new();
    let writer = handle(&vfs, &path);
    let reader = handle(&vfs, &path);

    reader.lock(FileLock::Shared).expect("the reader");
    assert!(
        !reader
            .check_reserved_lock()
            .expect("the probe is not an error"),
        "nothing has declared an intention to write yet"
    );

    writer
        .lock(FileLock::Shared)
        .expect("the writer reads first");
    writer
        .lock(FileLock::Reserved)
        .expect("and then declares its intention");
    assert!(
        reader
            .check_reserved_lock()
            .expect("the probe is not an error"),
        "the reader should see the other handle's RESERVED, in this process \
         as well as in another one"
    );

    writer
        .unlock(FileLock::Shared)
        .expect("the writer stands down");
    assert!(
        !reader
            .check_reserved_lock()
            .expect("the probe is not an error"),
        "and stops being visible when it does"
    );
}

/// Closing one handle does not release another handle's kernel lock.
///
/// **The POSIX trap, asserted from outside this process because there is no
/// other way to see it.** `close()` on any descriptor for a file releases every
/// `fcntl` lock the process holds on it, whichever descriptor took them. A
/// second connection being dropped would therefore have unlocked the first
/// connection's database, and every check inside this process would still have
/// said the lock was held - the table would answer from its own state. The
/// probe is another process, so it asks the kernel.
///
/// `os/unix.rs` keeps one shared descriptor per file identity for this reason,
/// so the descriptor outlives every handle that shares it.
#[test]
fn closing_one_handle_keeps_the_other_s_kernel_lock() {
    let path = scratch("close");
    let vfs = OsVfs::new();
    let keeper = handle(&vfs, &path);
    let closing = handle(&vfs, &path);

    keeper.lock(FileLock::Shared).expect("the keeper reads");
    closing
        .lock(FileLock::Shared)
        .expect("and so does the handle about to go");

    let mut probe = Probe::start(&path);
    assert_eq!(
        probe.send("lock exclusive"),
        "busy",
        "two readers hold the file, so another process cannot write it"
    );

    drop(closing);

    assert_eq!(
        probe.send("lock exclusive"),
        "busy",
        "the keeper still holds SHARED; another process seeing the file free \
         here is the close having dropped a lock it did not take"
    );
    assert_eq!(
        keeper.lock_level(),
        FileLock::Shared,
        "and this process still believes it holds it, which is the half that \
         was always true"
    );

    keeper.unlock(FileLock::None).expect("the keeper lets go");
    assert_eq!(
        probe.send("lock exclusive"),
        "ok",
        "with the last reader gone the other process can take the file"
    );
}
