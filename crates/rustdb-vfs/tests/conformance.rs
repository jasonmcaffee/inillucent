//! Runs the VFS conformance suite against both shipped implementations, and
//! proves cross-process locking with a second process.
//!
//! Invariant: the same suite runs against memory and the real file system, and
//! a failure names the case identifier the parity manifest cites.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use rustdb_vfs::conformance;
use rustdb_vfs::contract::{FileLock, OpenOptions, Vfs};
use rustdb_vfs::memory::MemoryVfs;
use rustdb_vfs::os::OsVfs;
use rustdb_vfs::path::DbPath;

/// Creates a fresh directory for one test and returns its path.
fn workspace(name: &str) -> DbPath {
    let mut root = std::env::temp_dir();
    root.push(format!("rustdb-vfs-tests/{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the temporary directory is writable");
    DbPath::new(root)
}

/// Writes a report next to the other evidence artifacts so a run leaves proof.
fn record(report: &conformance::ConformanceReport, name: &str) {
    let Ok(root) = std::env::var("RUSTDB_TEST_ARTIFACTS") else {
        return;
    };
    let directory = PathBuf::from(root);
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let _ = std::fs::write(directory.join(format!("{name}.txt")), report.to_text());
}

/// The in-memory VFS must pass every case; it is a real implementation, not a
/// stub, because `:memory:` databases run on it.
#[test]
fn memory_vfs_passes_the_conformance_suite() {
    let vfs = MemoryVfs::new();
    let report = conformance::run(&vfs, &DbPath::from("/memory"));
    record(&report, "conformance-memory");
    assert!(report.is_clean(), "{}", report.to_text());
    assert!(report.passed() >= 24, "{}", report.to_text());
}

/// The real file system must pass every case on this platform.
#[test]
fn os_vfs_passes_the_conformance_suite() {
    let root = workspace("conformance");
    let vfs = OsVfs::new();
    let report = conformance::run(&vfs, &root);
    record(&report, "conformance-os");
    assert!(report.is_clean(), "{}", report.to_text());
    assert!(report.passed() >= 24, "{}", report.to_text());
    let _ = std::fs::remove_dir_all(root.as_path());
}

/// A second process holding the probe's locks.
struct Probe {
    child: Child,
    input: ChildStdin,
    output: BufReader<std::process::ChildStdout>,
}

impl Probe {
    /// Starts the probe and opens `path` in it.
    fn start(path: &DbPath) -> Probe {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rustdb-lock-probe"))
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

/// Two processes must exclude each other exactly as the protocol says, which is
/// the property no same-process test can demonstrate on POSIX.
#[test]
fn locks_conflict_across_processes() {
    let root = workspace("cross-process");
    let path = DbPath::new(root.as_path().join("shared.db"));
    let vfs = OsVfs::new();
    let local = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    local
        .write_all_at(0, b"contended")
        .expect("the write lands");

    let mut probe = Probe::start(&path);

    // The other process takes a read lock; we may still read, but not write.
    // A refused promotion leaves us at PENDING, exactly as SQLite's own lock
    // routine does, so we drop back to nothing before the next step.
    assert_eq!(probe.send("lock shared"), "ok");
    local
        .lock(FileLock::Shared)
        .expect("a second reader is allowed");
    assert_eq!(
        local
            .lock(FileLock::Exclusive)
            .map_err(|error| error.code()),
        Err(rustdb_base::error::PrimaryCode::Busy),
        "EXCLUSIVE was granted while another process was reading"
    );
    assert_eq!(local.lock_level(), FileLock::Pending);
    local
        .unlock(FileLock::None)
        .expect("dropping our lock succeeds");

    // The other process declares its intention to write; we must see it, and we
    // must still be able to read underneath it.
    assert_eq!(probe.send("lock reserved"), "ok");
    local
        .lock(FileLock::Shared)
        .expect("RESERVED does not stop a reader");
    assert!(
        local.check_reserved_lock().expect("the check succeeds"),
        "another process's RESERVED lock was invisible"
    );
    assert_eq!(
        local.lock(FileLock::Reserved).map_err(|error| error.code()),
        Err(rustdb_base::error::PrimaryCode::Busy),
        "two processes held RESERVED at once"
    );

    // Once we leave, the other process can finish its write.
    local
        .unlock(FileLock::None)
        .expect("dropping our lock succeeds");
    assert_eq!(probe.send("lock exclusive"), "ok");
    assert_eq!(
        local.lock(FileLock::Shared).map_err(|error| error.code()),
        Err(rustdb_base::error::PrimaryCode::Busy),
        "a reader arrived while another process held EXCLUSIVE"
    );
    assert_eq!(probe.send("write 0 6465616462656566"), "ok");
    assert_eq!(probe.send("unlock none"), "ok");

    // And the write it made is visible to us afterwards.
    local
        .lock(FileLock::Shared)
        .expect("the file is free again");
    let mut buffer = [0u8; 8];
    local
        .read_exact_at(0, &mut buffer)
        .expect("the read succeeds");
    assert_eq!(&buffer, b"deadbeef");

    drop(probe);
    drop(local);
    let _ = std::fs::remove_dir_all(root.as_path());
}

/// A process that dies while holding a lock must not leave the file locked, or
/// a crashed connection would wedge the database until a reboot.
#[test]
fn a_dead_process_releases_its_locks() {
    let root = workspace("dead-process");
    let path = DbPath::new(root.as_path().join("orphan.db"));
    let vfs = OsVfs::new();
    let owner = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    owner.write_all_at(0, b"x").expect("the write lands");
    drop(owner);

    let mut probe = Probe::start(&path);
    assert_eq!(probe.send("lock shared"), "ok");
    assert_eq!(probe.send("lock exclusive"), "ok");
    probe.child.kill().expect("the probe can be stopped");
    let _ = probe.child.wait();
    drop(probe);

    let survivor = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    let mut attempts = 0;
    loop {
        match survivor.lock(FileLock::Exclusive) {
            Ok(()) => break,
            Err(_) if attempts < 100 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("the lock was never released: {error}"),
        }
    }
    drop(survivor);
    let _ = std::fs::remove_dir_all(root.as_path());
}
