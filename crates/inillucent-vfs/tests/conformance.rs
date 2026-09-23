//! Runs the VFS conformance suite against both shipped implementations, and
//! proves cross-process locking with a second process.
//!
//! Invariant: the same suite runs against memory and the real file system, and
//! a failure names the case identifier the parity manifest cites.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use inillucent_vfs::conformance;
use inillucent_vfs::contract::{FileLock, OpenOptions, Vfs};
use inillucent_vfs::memory::MemoryVfs;
use inillucent_vfs::os::OsVfs;
use inillucent_vfs::path::DbPath;

/// Creates a fresh directory for one test and returns its path.
fn workspace(name: &str) -> DbPath {
    let mut root = std::env::temp_dir();
    root.push(format!(
        "inillucent-vfs-tests/{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the temporary directory is writable");
    DbPath::new(root)
}

/// Writes a report next to the other evidence artifacts so a run leaves proof.
fn record(report: &conformance::ConformanceReport, name: &str) {
    let Ok(root) = std::env::var("INILLUCENT_TEST_ARTIFACTS") else {
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
        Err(inillucent_base::error::PrimaryCode::Busy),
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
        Err(inillucent_base::error::PrimaryCode::Busy),
        "two processes held RESERVED at once"
    );

    // Once we leave, the other process can finish its write.
    local
        .unlock(FileLock::None)
        .expect("dropping our lock succeeds");
    assert_eq!(probe.send("lock exclusive"), "ok");
    assert_eq!(
        local.lock(FileLock::Shared).map_err(|error| error.code()),
        Err(inillucent_base::error::PrimaryCode::Busy),
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

/// A wal-index left behind by a process that died must be thrown away, and one
/// a live process is still using must not be.
///
/// The index is a cache of the log and nothing else, so a copy whose owner is
/// gone cannot be vouched for: the frames it names may never have reached the
/// disk. SQLite settles this with a dead-man switch - a byte every connection
/// holds a shared lock on, which the first arrival can only take exclusively
/// when nobody else has the file mapped - and this is that byte doing its job.
/// Both directions matter. Discarding too eagerly would throw away the index a
/// running connection is reading, which is why the second half of this test
/// exists at all.
#[test]
fn an_abandoned_wal_index_is_discarded() {
    let root = workspace("abandoned-index");
    let path = DbPath::new(root.as_path().join("index.db"));
    let vfs = OsVfs::new();
    let owner = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    owner.write_all_at(0, b"x").expect("the write lands");
    drop(owner);

    // A second process maps the shared memory and leaves a marker in it.
    let mut probe = Probe::start(&path);
    assert_eq!(probe.send("shm-open"), "ok");
    assert_eq!(probe.send("shm-map"), "ok");
    assert_eq!(probe.send("shm-write 64 6d61726b6572"), "ok");

    // While that process is alive the marker is what everyone else sees.
    let live = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    let live_shm = live
        .shared_memory()
        .expect("shared memory is available")
        .expect("this VFS has shared memory");
    let live_region = live_shm
        .map(0, 32_768, false)
        .expect("the region maps")
        .expect("the region the other process made exists");
    let mut seen = [0u8; 6];
    live_region.read(64, &mut seen).expect("the read succeeds");
    assert_eq!(
        &seen, b"marker",
        "a second connection did not see the live index"
    );

    // The process dies, and this one lets go too, so nobody has it mapped.
    probe.child.kill().expect("the probe can be stopped");
    let _ = probe.child.wait();
    drop(probe);
    drop(live_region);
    drop(live_shm);
    drop(live);

    // The next arrival is alone, so what it finds is a leftover and goes.
    let survivor = vfs
        .open(&path, OpenOptions::main_db())
        .expect("the database opens");
    let survivor_shm = survivor
        .shared_memory()
        .expect("shared memory is available")
        .expect("this VFS has shared memory");
    let mut after = [0u8; 6];
    match survivor_shm
        .map(0, 32_768, false)
        .expect("the map succeeds")
    {
        None => {}
        Some(region) => {
            region.read(64, &mut after).expect("the read succeeds");
            assert_eq!(
                after, [0u8; 6],
                "an abandoned wal-index was believed rather than rebuilt"
            );
        }
    }
    drop(survivor_shm);
    drop(survivor);
    let _ = std::fs::remove_dir_all(root.as_path());
}

/// **Creating a file that survives a restart forces its directory entry.**
///
/// `OsVfs::open` created a file and never synced the parent, while `delete`
/// and `rename` had done it since phase 1 (task-2066 section 4.2, item 19). On
/// ext4 or XFS a power loss can then leave a log segment whose header was
/// written and synced with no directory entry naming it - and `read_chain`
/// reads a missing segment as the ordinary end of the chain, so every commit
/// inside it is lost and recovery reports success over the loss.
///
/// The count is the observable, because the durability of a directory entry is
/// not: nothing a later read does can tell a forced entry from an unforced
/// one, and no test on a real file system can arrange the power loss that
/// would. Three things are asserted and none of them alone is the behaviour: a
/// create forces the entry exactly once, reopening the file it just made
/// forces nothing, and a transient file - which is deleted when it is closed
/// and has nothing to lose - pays no directory sync for it.
#[test]
fn creating_a_durable_file_forces_its_directory_entry() {
    use inillucent_vfs::contract::FileKind;

    let root = workspace("directory-sync");
    let vfs = OsVfs::new();
    let path = DbPath::new(root.as_path().join("made.rdb"));
    assert_eq!(vfs.directory_syncs(), 0, "a fresh VFS has forced nothing");

    let made = vfs
        .open(&path, OpenOptions::of_kind(FileKind::MainDb))
        .expect("the file is created");
    assert_eq!(
        vfs.directory_syncs(),
        1,
        "creating a database file did not force the directory entry naming it"
    );
    drop(made);

    // Reopening is not creating, and there is no new entry to force.
    let again = vfs
        .open(&path, OpenOptions::of_kind(FileKind::MainDb))
        .expect("the file reopens");
    assert_eq!(
        vfs.directory_syncs(),
        1,
        "reopening an existing file forced a directory entry it did not create"
    );
    drop(again);

    // A log segment is the case the defect actually lost commits through.
    let segment = DbPath::new(root.as_path().join("made.rdb-wal-1"));
    let log = vfs
        .open(&segment, OpenOptions::of_kind(FileKind::Wal))
        .expect("the segment is created");
    assert_eq!(
        vfs.directory_syncs(),
        2,
        "creating a log segment did not force the directory entry naming it"
    );
    drop(log);

    // And a file that is deleted when it is closed pays nothing for a name
    // nobody will ever look for.
    let scratch = DbPath::new(root.as_path().join("sort-run"));
    let transient = vfs
        .open(&scratch, OpenOptions::of_kind(FileKind::Transient))
        .expect("the scratch file is created");
    assert_eq!(
        vfs.directory_syncs(),
        2,
        "a transient file forced a directory entry it has no use for"
    );
    drop(transient);

    let _ = std::fs::remove_dir_all(root.as_path());
}
