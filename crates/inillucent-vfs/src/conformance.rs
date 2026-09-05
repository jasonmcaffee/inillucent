//! One conformance suite that every VFS implementation must pass.
//!
//! Invariant: there is exactly one description of what a VFS has to do, and the
//! real file system, the in-memory file system, and the deterministic simulator
//! are all held to it. A simulator that quietly disagrees with a disk produces
//! crash evidence about a database nobody ships, which is worse than no
//! evidence at all.
//!
//! The suite is a library rather than a set of `#[test]` functions so that
//! `inillucent-sim` and the compatibility harness can run it against a VFS they
//! construct themselves, and so a run can be recorded as an artifact.

use std::collections::BTreeSet;

use crate::contract::{
    AccessMode, FileKind, FileLock, OpenOptions, ShmLockRequest, SyncMode, Vfs, VfsFile,
};
use crate::error::VfsResult;
use crate::path::DbPath;

/// What one case did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The case held.
    Passed,
    /// The case did not hold, with the reason.
    Failed(String),
    /// The case does not apply to this VFS, with the reason.
    Skipped(String),
}

/// One case's result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaseResult {
    /// The stable case identifier, used as the test id in the parity manifest.
    pub name: &'static str,
    /// What the case did.
    pub outcome: Outcome,
}

/// Every case's result for one VFS.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConformanceReport {
    /// The VFS that was exercised.
    pub vfs: String,
    /// One entry per case, in the order they ran.
    pub cases: Vec<CaseResult>,
}

impl ConformanceReport {
    /// Returns the cases that failed.
    pub fn failures(&self) -> Vec<&CaseResult> {
        self.cases
            .iter()
            .filter(|case| matches!(case.outcome, Outcome::Failed(_)))
            .collect()
    }

    /// Reports whether every case passed or was skipped.
    pub fn is_clean(&self) -> bool {
        self.failures().is_empty()
    }

    /// Returns how many cases passed.
    pub fn passed(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| case.outcome == Outcome::Passed)
            .count()
    }

    /// Renders the report as one line per case, for an evidence artifact.
    pub fn to_text(&self) -> String {
        let mut text = format!("vfs: {}\n", self.vfs);
        for case in &self.cases {
            let status = match &case.outcome {
                Outcome::Passed => "pass".to_string(),
                Outcome::Skipped(reason) => format!("skip ({reason})"),
                Outcome::Failed(reason) => format!("FAIL ({reason})"),
            };
            text.push_str(&format!("{:<44} {status}\n", case.name));
        }
        text
    }
}

/// The type every case has: it is handed the VFS and a directory to work in.
type Case = fn(&dyn Vfs, &DbPath) -> Result<(), String>;

/// Every case, paired with the identifier the parity manifest cites.
const CASES: &[(&str, Case)] = &[
    ("vfs.open.create-and-reopen", open_creates_and_reopens),
    (
        "vfs.open.missing-without-create",
        open_missing_without_create,
    ),
    (
        "vfs.open.exclusive-refuses-existing",
        open_exclusive_refuses_existing,
    ),
    ("vfs.read.short-read-zero-fills", short_read_zero_fills),
    ("vfs.write.extends-with-zeroes", write_extends_with_zeroes),
    ("vfs.truncate.shrinks-and-grows", truncate_shrinks_and_grows),
    (
        "vfs.sync.succeeds-in-every-mode",
        sync_succeeds_in_every_mode,
    ),
    ("vfs.readonly.refuses-writes", read_only_refuses_writes),
    ("vfs.delete.is-idempotent", delete_is_idempotent),
    ("vfs.access.reports-existence", access_reports_existence),
    (
        "vfs.fullpath.is-absolute-and-stable",
        full_pathname_is_absolute_and_stable,
    ),
    (
        "vfs.identity.is-stable-across-handles",
        identity_is_stable_across_handles,
    ),
    ("vfs.lock.shared-is-shared", shared_locks_coexist),
    (
        "vfs.lock.reserved-is-exclusive",
        reserved_excludes_a_second_writer,
    ),
    (
        "vfs.lock.reserved-is-visible",
        reserved_is_visible_to_another_handle,
    ),
    (
        "vfs.lock.exclusive-waits-for-readers",
        exclusive_waits_for_readers,
    ),
    (
        "vfs.lock.pending-blocks-new-readers",
        pending_blocks_new_readers,
    ),
    (
        "vfs.lock.downgrade-restores-reader",
        downgrade_restores_the_reader,
    ),
    ("vfs.lock.close-releases", closing_releases_locks),
    ("vfs.temp.path-is-fresh", temp_path_is_fresh),
    ("vfs.temp.delete-on-close", delete_on_close_removes_the_file),
    ("vfs.random.is-not-constant", randomness_is_not_constant),
    (
        "vfs.clock.is-monotonic-enough",
        clock_returns_a_plausible_time,
    ),
    ("vfs.device.claims-are-true", device_claims_are_true),
    ("vfs.shm.round-trips", shared_memory_round_trips),
    ("vfs.shm.locks-exclude", shared_memory_locks_exclude),
];

/// Runs every case against `vfs`, working inside `root`.
///
/// `root` must be a directory that already exists for a file-backed VFS; the
/// in-memory VFS ignores it beyond using it as a name prefix.
pub fn run(vfs: &dyn Vfs, root: &DbPath) -> ConformanceReport {
    let mut cases = Vec::new();
    for (name, case) in CASES {
        let outcome = match case(vfs, root) {
            Ok(()) => Outcome::Passed,
            Err(reason) if reason.starts_with("skip:") => {
                Outcome::Skipped(reason.trim_start_matches("skip:").trim().to_string())
            }
            Err(reason) => Outcome::Failed(reason),
        };
        cases.push(CaseResult { name, outcome });
    }
    ConformanceReport {
        vfs: vfs.name().to_string(),
        cases,
    }
}

/// Returns a fresh path inside the working directory for one case.
fn scratch(root: &DbPath, name: &str) -> DbPath {
    DbPath::new(root.as_path().join(format!("{name}.db")))
}

/// Turns a VFS error into the string a case reports.
fn describe<T>(what: &str, result: VfsResult<T>) -> Result<T, String> {
    result.map_err(|error| format!("{what}: {} ({})", error.extended().value(), error.detail()))
}

/// Opens a scratch file for a case, deleting anything left by a previous run.
fn fresh(vfs: &dyn Vfs, root: &DbPath, name: &str) -> Result<(DbPath, Box<dyn VfsFile>), String> {
    let path = scratch(root, name);
    describe("delete", vfs.delete(&path, false))?;
    let file = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    Ok((path, file))
}

/// A file created by one handle must be visible to the next.
fn open_creates_and_reopens(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, file) = fresh(vfs, root, "open-create")?;
    describe("write", file.write_all_at(0, b"hello"))?;
    drop(file);
    let reopened = describe("reopen", vfs.open(&path, OpenOptions::main_db()))?;
    let mut buffer = [0u8; 5];
    describe("read", reopened.read_exact_at(0, &mut buffer))?;
    if &buffer != b"hello" {
        return Err(format!("reopened file held {buffer:?}"));
    }
    Ok(())
}

/// Opening a missing file without `create` must fail rather than create it.
fn open_missing_without_create(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let path = scratch(root, "open-missing");
    describe("delete", vfs.delete(&path, false))?;
    let mut options = OpenOptions::main_db();
    options.create = false;
    if vfs.open(&path, options).is_ok() {
        return Err("opening a missing file without create succeeded".to_string());
    }
    if describe("access", vfs.access(&path, AccessMode::Exists))? {
        return Err("a failed open created the file anyway".to_string());
    }
    Ok(())
}

/// An exclusive create must refuse a file that already exists.
fn open_exclusive_refuses_existing(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, file) = fresh(vfs, root, "open-exclusive")?;
    drop(file);
    let mut options = OpenOptions::main_db();
    options.exclusive = true;
    if vfs.open(&path, options).is_ok() {
        return Err("an exclusive create succeeded on an existing file".to_string());
    }
    Ok(())
}

/// Reading past the end must zero-fill and report a short read, because that is
/// how the pager tells a truncated database from an unreadable one.
fn short_read_zero_fills(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (_, file) = fresh(vfs, root, "short-read")?;
    describe("write", file.write_all_at(0, &[0xab; 4]))?;
    let mut buffer = [0xffu8; 16];
    match file.read_exact_at(0, &mut buffer) {
        Ok(()) => Err("reading past the end succeeded".to_string()),
        Err(error) => {
            if error.extended() != inillucent_base::error::ExtendedCode::IO_ERR_SHORT_READ {
                return Err(format!("wrong code {}", error.extended().value()));
            }
            if buffer.get(0..4) != Some(&[0xab; 4]) {
                return Err("the readable prefix was lost".to_string());
            }
            if buffer.iter().skip(4).any(|byte| *byte != 0) {
                return Err("the tail was not zero-filled".to_string());
            }
            Ok(())
        }
    }
}

/// Writing past the end must extend the file with zeroes, not leave a hole of
/// unspecified bytes.
fn write_extends_with_zeroes(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (_, file) = fresh(vfs, root, "write-extend")?;
    describe("write", file.write_all_at(8, b"tail"))?;
    let size = describe("size", file.file_size())?;
    if size != 12 {
        return Err(format!("file is {size} bytes, expected 12"));
    }
    let mut buffer = [0xffu8; 12];
    describe("read", file.read_exact_at(0, &mut buffer))?;
    if buffer.iter().take(8).any(|byte| *byte != 0) {
        return Err("the gap was not zero-filled".to_string());
    }
    Ok(())
}

/// Truncation must both shrink and grow, and a grown region must read as zero.
fn truncate_shrinks_and_grows(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (_, file) = fresh(vfs, root, "truncate")?;
    describe("write", file.write_all_at(0, &[0x5a; 64]))?;
    describe("shrink", file.truncate(16))?;
    if describe("size", file.file_size())? != 16 {
        return Err("shrink did not take effect".to_string());
    }
    describe("grow", file.truncate(32))?;
    let mut buffer = [0xffu8; 32];
    describe("read", file.read_exact_at(0, &mut buffer))?;
    if buffer.iter().skip(16).any(|byte| *byte != 0) {
        return Err("the grown region was not zero".to_string());
    }
    Ok(())
}

/// Every sync mode must be accepted; a VFS that cannot flush says so by
/// failing, not by silently ignoring the request.
fn sync_succeeds_in_every_mode(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (_, file) = fresh(vfs, root, "sync")?;
    describe("write", file.write_all_at(0, b"durable"))?;
    for mode in [SyncMode::Normal, SyncMode::Full, SyncMode::DataOnly] {
        describe("sync", file.sync(mode))?;
    }
    Ok(())
}

/// A read-only handle must refuse every mutating operation.
fn read_only_refuses_writes(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, file) = fresh(vfs, root, "read-only")?;
    describe("write", file.write_all_at(0, b"fixed"))?;
    drop(file);
    let reader = describe("open", vfs.open(&path, OpenOptions::main_db().read_only()))?;
    if reader.write_all_at(0, b"nope").is_ok() {
        return Err("a read-only handle accepted a write".to_string());
    }
    if reader.truncate(0).is_ok() {
        return Err("a read-only handle accepted a truncate".to_string());
    }
    let mut buffer = [0u8; 5];
    describe("read", reader.read_exact_at(0, &mut buffer))?;
    Ok(())
}

/// Deleting a file that is not there must succeed; the pager tidies up a
/// journal it may already have removed.
fn delete_is_idempotent(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, file) = fresh(vfs, root, "delete")?;
    drop(file);
    describe("delete", vfs.delete(&path, false))?;
    describe("delete again", vfs.delete(&path, false))?;
    if describe("access", vfs.access(&path, AccessMode::Exists))? {
        return Err("the file survived deletion".to_string());
    }
    Ok(())
}

/// `access` must answer for a file that exists and one that does not.
fn access_reports_existence(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, file) = fresh(vfs, root, "access")?;
    drop(file);
    for mode in [
        AccessMode::Exists,
        AccessMode::ReadOnly,
        AccessMode::ReadWrite,
    ] {
        if !describe("access", vfs.access(&path, mode))? {
            return Err(format!("{mode:?} said no for a file that exists"));
        }
    }
    let missing = scratch(root, "access-missing");
    describe("delete", vfs.delete(&missing, false))?;
    if describe("access", vfs.access(&missing, AccessMode::Exists))? {
        return Err("a missing file reported as present".to_string());
    }
    Ok(())
}

/// Resolving a path twice must give the same answer, and it must be absolute.
fn full_pathname_is_absolute_and_stable(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, file) = fresh(vfs, root, "fullpath")?;
    drop(file);
    let first = describe("fullpath", vfs.full_pathname(&path))?;
    let second = describe("fullpath", vfs.full_pathname(&path))?;
    if first != second {
        return Err("resolving twice gave two answers".to_string());
    }
    if !first.as_path().is_absolute() && !first.as_path().starts_with("/") {
        return Err(format!("{} is not absolute", first.display()));
    }
    Ok(())
}

/// Two handles on one file must report the same identity, and a different file
/// must report a different one.
fn identity_is_stable_across_handles(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, first) = fresh(vfs, root, "identity")?;
    let second = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    let (_, other) = fresh(vfs, root, "identity-other")?;
    let a = describe("identity", first.file_identity())?;
    let b = describe("identity", second.file_identity())?;
    let c = describe("identity", other.file_identity())?;
    if a != b {
        return Err("two handles on one file reported different identities".to_string());
    }
    if a == c {
        return Err("two different files reported the same identity".to_string());
    }
    Ok(())
}

/// Any number of readers may hold SHARED at once.
fn shared_locks_coexist(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, first) = fresh(vfs, root, "lock-shared")?;
    let second = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    describe("lock a", first.lock(FileLock::Shared))?;
    describe("lock b", second.lock(FileLock::Shared))?;
    if first.lock_level() != FileLock::Shared || second.lock_level() != FileLock::Shared {
        return Err("a reader did not record its level".to_string());
    }
    Ok(())
}

/// One RESERVED at a time, with readers continuing underneath.
fn reserved_excludes_a_second_writer(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, first) = fresh(vfs, root, "lock-reserved")?;
    let second = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    let third = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    describe("lock a", first.lock(FileLock::Shared))?;
    describe("lock b", second.lock(FileLock::Shared))?;
    describe("reserve a", first.lock(FileLock::Reserved))?;
    if second.lock(FileLock::Reserved).is_ok() {
        return Err("two connections held RESERVED at once".to_string());
    }
    describe("lock c", third.lock(FileLock::Shared))?;
    Ok(())
}

/// A RESERVED lock must be visible to another handle, which is how a writer
/// learns that it should not start.
fn reserved_is_visible_to_another_handle(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, first) = fresh(vfs, root, "lock-visible")?;
    let second = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    if describe("check", second.check_reserved_lock())? {
        return Err("RESERVED was reported before anyone took it".to_string());
    }
    describe("lock", first.lock(FileLock::Shared))?;
    describe("reserve", first.lock(FileLock::Reserved))?;
    if !describe("check", second.check_reserved_lock())? {
        return Err("RESERVED was invisible to another handle".to_string());
    }
    describe("unlock", first.unlock(FileLock::Shared))?;
    if describe("check", second.check_reserved_lock())? {
        return Err("RESERVED stayed visible after it was released".to_string());
    }
    Ok(())
}

/// EXCLUSIVE must wait for every reader to leave.
fn exclusive_waits_for_readers(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, writer) = fresh(vfs, root, "lock-exclusive")?;
    let reader = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    describe("lock w", writer.lock(FileLock::Shared))?;
    describe("lock r", reader.lock(FileLock::Shared))?;
    if writer.lock(FileLock::Exclusive).is_ok() {
        return Err("EXCLUSIVE was granted while a reader was present".to_string());
    }
    describe("unlock r", reader.unlock(FileLock::None))?;
    describe("lock w", writer.lock(FileLock::Exclusive))?;
    Ok(())
}

/// PENDING must stop a new reader arriving, or a writer could starve.
fn pending_blocks_new_readers(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, writer) = fresh(vfs, root, "lock-pending")?;
    let reader = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    let latecomer = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    describe("lock w", writer.lock(FileLock::Shared))?;
    describe("lock r", reader.lock(FileLock::Shared))?;
    if writer.lock(FileLock::Exclusive).is_ok() {
        return Err("EXCLUSIVE was granted while a reader was present".to_string());
    }
    if latecomer.lock(FileLock::Shared).is_ok() {
        return Err("a new reader arrived while a writer was waiting".to_string());
    }
    Ok(())
}

/// Dropping from EXCLUSIVE to SHARED must leave a working read lock.
fn downgrade_restores_the_reader(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, writer) = fresh(vfs, root, "lock-downgrade")?;
    describe("lock", writer.lock(FileLock::Shared))?;
    describe("lock", writer.lock(FileLock::Exclusive))?;
    describe("write", writer.write_all_at(0, b"committed"))?;
    describe("downgrade", writer.unlock(FileLock::Shared))?;
    if writer.lock_level() != FileLock::Shared {
        return Err("the level did not drop to SHARED".to_string());
    }
    let reader = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    describe("lock", reader.lock(FileLock::Shared))?;
    Ok(())
}

/// Closing a handle must release everything it held.
fn closing_releases_locks(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, writer) = fresh(vfs, root, "lock-close")?;
    describe("lock", writer.lock(FileLock::Shared))?;
    describe("lock", writer.lock(FileLock::Exclusive))?;
    drop(writer);
    let next = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    describe("lock", next.lock(FileLock::Shared))?;
    describe("lock", next.lock(FileLock::Exclusive))?;
    Ok(())
}

/// A temporary path must be fresh and must not already exist.
fn temp_path_is_fresh(vfs: &dyn Vfs, _root: &DbPath) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for _ in 0..8 {
        let path = describe("temp", vfs.temp_path("inillucent-conformance-"))?;
        if describe("access", vfs.access(&path, AccessMode::Exists))? {
            return Err(format!("{} already exists", path.display()));
        }
        if !seen.insert(path.clone()) {
            return Err("temp_path repeated a name".to_string());
        }
    }
    Ok(())
}

/// A file opened to be deleted on close must be gone afterwards.
fn delete_on_close_removes_the_file(vfs: &dyn Vfs, _root: &DbPath) -> Result<(), String> {
    let path = describe("temp", vfs.temp_path("inillucent-transient-"))?;
    let file = describe(
        "open",
        vfs.open(&path, OpenOptions::of_kind(FileKind::Transient)),
    )?;
    describe("write", file.write_all_at(0, b"scratch"))?;
    drop(file);
    if describe("access", vfs.access(&path, AccessMode::Exists))? {
        return Err("a delete-on-close file survived".to_string());
    }
    Ok(())
}

/// Randomness must vary; a VFS that returns zeroes would make every WAL salt
/// and temporary name collide.
fn randomness_is_not_constant(vfs: &dyn Vfs, _root: &DbPath) -> Result<(), String> {
    let mut first = [0u8; 32];
    let mut second = [0u8; 32];
    describe("random", vfs.randomness(&mut first))?;
    describe("random", vfs.randomness(&mut second))?;
    if first == second {
        return Err("two randomness calls returned the same bytes".to_string());
    }
    if first.iter().all(|byte| *byte == 0) {
        return Err("randomness returned zeroes".to_string());
    }
    Ok(())
}

/// The clock must return a time after the epoch and must not go backwards
/// between two immediately consecutive calls.
fn clock_returns_a_plausible_time(vfs: &dyn Vfs, _root: &DbPath) -> Result<(), String> {
    let first = describe("clock", vfs.current_time())?;
    let second = describe("clock", vfs.current_time())?;
    if second < first {
        return Err("the clock went backwards".to_string());
    }
    if first.duration_since(std::time::UNIX_EPOCH).is_err() {
        return Err("the clock is before the epoch".to_string());
    }
    Ok(())
}

/// A capability a VFS declares must actually hold. Only one of them can be
/// checked without cutting the power, and this is it.
fn device_claims_are_true(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, file) = fresh(vfs, root, "device-claims")?;
    let device = file.device_characteristics();
    let deleted_while_open = vfs.delete(&path, false).is_ok();
    if device.undeletable_when_open && deleted_while_open {
        return Err("the VFS claims files cannot be deleted while open, but one was".to_string());
    }
    if !device.undeletable_when_open && !deleted_while_open {
        return Err(
            "the VFS claims files can be deleted while open, but one could not".to_string(),
        );
    }
    drop(file);
    describe("cleanup", vfs.delete(&path, false))?;
    Ok(())
}

/// Shared memory must round-trip bytes between two handles on one database.
fn shared_memory_round_trips(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, writer) = fresh(vfs, root, "shm-round-trip")?;
    let Some(write_shm) = describe("shm", writer.shared_memory())? else {
        return Err("skip: this VFS has no shared memory".to_string());
    };
    let reader = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    let Some(read_shm) = describe("shm", reader.shared_memory())? else {
        return Err("skip: this VFS has no shared memory".to_string());
    };
    let Some(write_region) = describe("map", write_shm.map(0, 32_768, true))? else {
        return Err("mapping region 0 with extend returned nothing".to_string());
    };
    let Some(read_region) = describe("map", read_shm.map(0, 32_768, false))? else {
        return Err("region 0 was invisible to the second handle".to_string());
    };
    describe("write", write_region.write(64, b"wal-index"))?;
    write_shm.barrier();
    read_shm.barrier();
    let mut buffer = [0u8; 9];
    describe("read", read_region.read(64, &mut buffer))?;
    if &buffer != b"wal-index" {
        return Err(format!("shared memory held {buffer:?}"));
    }
    describe("unmap", write_shm.unmap(false))?;
    Ok(())
}

/// Shared-memory lock slots must exclude across handles the way the WAL
/// protocol needs them to.
fn shared_memory_locks_exclude(vfs: &dyn Vfs, root: &DbPath) -> Result<(), String> {
    let (path, first) = fresh(vfs, root, "shm-locks")?;
    let Some(first_shm) = describe("shm", first.shared_memory())? else {
        return Err("skip: this VFS has no shared memory".to_string());
    };
    let second = describe("open", vfs.open(&path, OpenOptions::main_db()))?;
    let Some(second_shm) = describe("shm", second.shared_memory())? else {
        return Err("skip: this VFS has no shared memory".to_string());
    };
    let take = |exclusive: bool| ShmLockRequest {
        offset: 1,
        count: 1,
        acquire: true,
        exclusive,
    };
    describe("write lock", first_shm.lock(take(true)))?;
    if second_shm.lock(take(true)).is_ok() {
        return Err("two handles held the same slot exclusively".to_string());
    }
    if second_shm.lock(take(false)).is_ok() {
        return Err("a reader took a slot another handle holds exclusively".to_string());
    }
    describe(
        "release",
        first_shm.lock(ShmLockRequest {
            offset: 1,
            count: 1,
            acquire: false,
            exclusive: true,
        }),
    )?;
    describe("read lock", second_shm.lock(take(false)))?;
    Ok(())
}
