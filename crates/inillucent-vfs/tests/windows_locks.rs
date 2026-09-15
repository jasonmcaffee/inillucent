//! What Windows does with a byte-range lock that the conformance suite cannot
//! ask about.
//!
//! Invariant: **the two handles in these tests are in one process, and Windows
//! enforces a lock between them anyway.** That is the platform difference the
//! generic suite cannot see. `conformance.rs` drives the same twenty-odd cases
//! against memory and the real file system and proves cross-process locking
//! with a second process; what it never asks is what one process's *second*
//! handle sees, because on POSIX the answer is "nothing" - byte-range locks
//! there belong to the process, so `os/unix.rs` arbitrates in a table of its
//! own first. On Windows a lock belongs to the handle, the kernel is the
//! arbiter, and `os/windows.rs::LockState` keeps no table at all. Two code
//! paths, one contract; this file is the Windows half and `unix_locks.rs` is
//! the other.
//!
//! task-1913 found a Windows-only silent no-op in `link_directory`, which is
//! why T4 asks for these: a generic suite over three backends can pass while
//! one backend does nothing.

#![cfg(windows)]

use std::time::Instant;

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
        "inillucent-vfs-windows-locks/{name}-{}",
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

/// A second handle in the same process is refused a read lock while the first
/// holds EXCLUSIVE.
///
/// **This is the assertion POSIX would fail**, and it is why there are two
/// implementations. A `LockFileEx` range belongs to the handle that took it and
/// is enforced against every other handle, this process's included; an `fcntl`
/// range belongs to the process and its own second handle sees nothing. The
/// refusal is `SQLITE_BUSY` either way, and that is the contract both files
/// have to meet.
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

/// A reader retries the PENDING byte three times before it reports BUSY.
///
/// **Windows has no shared mode two readers can take on the same byte at once**,
/// so two connections acquiring SHARED at the same moment collide on the
/// serialising PENDING byte even though neither is a writer. `os/windows.rs`
/// makes `PENDING_ATTEMPTS` attempts a millisecond apart for that reason -
/// SQLite's own Windows VFS makes the same three - and reporting BUSY on the
/// first collision would turn ordinary read concurrency into a spurious
/// failure.
///
/// The retries are asserted by their cost, because that is the only thing they
/// leave behind: three attempts with a millisecond between them cannot return
/// in under two milliseconds. A version that gave up immediately took about
/// twenty microseconds.
#[test]
fn a_reader_retries_the_pending_byte_before_reporting_busy() {
    let path = scratch("pending");
    let vfs = OsVfs::new();
    let writer = handle(&vfs, &path);
    let reader = handle(&vfs, &path);

    writer
        .lock(FileLock::Pending)
        .expect("nothing else holds the file");
    let started = Instant::now();
    let refused = reader
        .lock(FileLock::Shared)
        .expect_err("a writer holds PENDING");
    let waited = started.elapsed();

    assert_eq!(refused.code(), PrimaryCode::Busy);
    assert!(
        refused.detail().contains("PENDING"),
        "the refusal should name the byte it could not take; it said {:?}",
        refused.detail()
    );
    assert!(
        waited >= std::time::Duration::from_millis(2),
        "three attempts a millisecond apart cannot report in {waited:?}; \\
         that is the retry loop not running"
    );
}

/// Two readers in one process both hold SHARED at once.
///
/// This is the case the retry loop above exists for: they collide on the
/// PENDING byte on the way in, and both come out holding the read range. A
/// build that serialised readers would pass every other test in this file and
/// make two connections to one database take turns.
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
    assert_eq!(
        first.lock_level(),
        FileLock::Pending,
        "the failed promotion leaves PENDING held, which is what stops new \\
         readers arriving while the writer waits"
    );
}

/// RESERVED is visible to another handle in this process.
///
/// `check_reserved_lock` is how the pager decides whether somebody else has
/// declared an intention to write. On Windows it is answered by probing the
/// byte rather than by consulting a table, so a second handle in the same
/// process is exactly the case that distinguishes the two implementations.
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
        "the reader should see the other handle's RESERVED, in this process \\
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
