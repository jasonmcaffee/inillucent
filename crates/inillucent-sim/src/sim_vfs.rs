//! The deterministic simulator VFS.
//!
//! Invariant: a run is a pure function of its seed, its schedule, and its
//! failpoint policy. Nothing here reads the wall clock, the real random
//! generator, or the real file system, so a failure is replayable from the
//! artifacts the run wrote.
//!
//! `SimVfs` satisfies the same contract as a disk and passes the same
//! conformance suite, then adds what a disk will not do on request: torn
//! writes, dropped unsynced sectors, short reads, ENOSPC, and power loss.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use inillucent_base::rng::Rng;
use inillucent_vfs::contract::{
    AccessMode, DeviceCharacteristics, FileIdentity, FileLock, OpenOptions, SharedMemory,
    ShmLockRequest, ShmRegion, SyncMode, Vfs, VfsFile, SHM_LOCK_COUNT,
};
use inillucent_vfs::error::{self, VfsError, VfsOperation, VfsResult};
use inillucent_vfs::locks::{next_step, HandleId, LockConflict, LockTable};
use inillucent_vfs::path::DbPath;
use inillucent_vfs::shm_locks::ShmLockTable;

use crate::failpoint::{Failpoints, Failure, Site};
use crate::media::{MediaModel, SimFileImage};
use crate::schedule::{ActorId, Scheduler};
use crate::trace::Trace;

thread_local! {
    /// Which actor the calling thread is, for the trace and the scheduler.
    static CURRENT_ACTOR: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Declares which actor the calling thread is.
pub fn set_current_actor(actor: ActorId) {
    CURRENT_ACTOR.with(|cell| cell.set(actor.0));
}

/// Returns the calling thread's actor.
fn current_actor() -> ActorId {
    ActorId(CURRENT_ACTOR.with(|cell| cell.get()))
}

/// How to build a simulated file system.
#[derive(Clone, Debug)]
pub struct SimConfig {
    /// The seed every random decision in the run comes from.
    pub seed: u64,
    /// What the simulated device promises.
    pub model: MediaModel,
    /// The wall-clock time the simulated clock starts at, in microseconds
    /// since the epoch.
    pub start_time_micros: u64,
    /// How much the simulated clock advances at each observation.
    pub clock_tick_micros: u64,
}

impl Default for SimConfig {
    /// The default configuration: a pessimistic device and a fixed start time,
    /// so that a run's timestamps are part of the deterministic output.
    fn default() -> SimConfig {
        SimConfig {
            seed: 0,
            model: MediaModel::default(),
            start_time_micros: 1_700_000_000_000_000,
            clock_tick_micros: 1_000,
        }
    }
}

/// One simulated file.
#[derive(Debug)]
struct SimInode {
    id: u64,
    image: Mutex<SimFileImage>,
    locks: Mutex<LockTable>,
    shm: Mutex<Option<Arc<SimShm>>>,
}

/// The counters that hand out identities.
#[derive(Debug, Default)]
struct Counters {
    inode: u64,
    handle: u64,
    temp: u64,
}

/// The state every handle shares.
#[derive(Debug)]
struct SimState {
    config: SimConfig,
    files: Mutex<BTreeMap<PathBuf, Arc<SimInode>>>,
    /// Files whose deletion has not been made durable.
    ///
    /// A directory entry is data like any other: removing it writes to the
    /// directory, and until that write is synced a power loss may put the
    /// entry back. That is the whole reason `synchronous=FULL` syncs the
    /// directory after deleting a rollback journal - in DELETE mode the
    /// deletion *is* the commit point, and a commit that was reported and then
    /// un-deleted would be a commit recovery undoes.
    pending_deletes: Mutex<BTreeMap<PathBuf, Arc<SimInode>>>,
    counters: Mutex<Counters>,
    failpoints: Failpoints,
    trace: Trace,
    clock: AtomicU64,
    rng: Mutex<Rng>,
    crash_rng: Mutex<Rng>,
    scheduler: Mutex<Option<Arc<Scheduler>>>,
    powered_off: AtomicBool,
}

/// A deterministic simulated file system.
#[derive(Debug)]
pub struct SimVfs {
    name: String,
    state: Arc<SimState>,
}

/// What a simulated power loss left on the media.
#[derive(Clone, Debug, Default)]
pub struct CrashSnapshot {
    /// Each file's recovered bytes, by path.
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    /// What happened to each unsynced sector, for the run's report.
    pub outcomes: Vec<String>,
}

impl CrashSnapshot {
    /// Writes the snapshot to a directory as replay evidence: one file per
    /// simulated file, plus a manifest naming what happened to each sector.
    pub fn write_artifacts(&self, directory: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(directory)?;
        let mut manifest = String::new();
        for (index, (path, bytes)) in self.files.iter().enumerate() {
            let name = format!("file-{index:03}.bin");
            std::fs::write(directory.join(&name), bytes)?;
            manifest.push_str(&format!(
                "{name}\t{}\t{} bytes\n",
                path.display(),
                bytes.len()
            ));
        }
        for outcome in &self.outcomes {
            manifest.push_str(outcome);
            manifest.push('\n');
        }
        std::fs::write(directory.join("crash-manifest.txt"), manifest)?;
        Ok(())
    }
}

impl SimVfs {
    /// Creates an empty simulated file system.
    pub fn new(config: SimConfig) -> SimVfs {
        let seed = config.seed;
        let start = config.start_time_micros;
        SimVfs {
            name: "simulator".to_string(),
            state: Arc::new(SimState {
                config,
                files: Mutex::new(BTreeMap::new()),
                counters: Mutex::new(Counters::default()),
                failpoints: Failpoints::new(seed ^ 0x9e37_79b9),
                trace: Trace::new(),
                clock: AtomicU64::new(start),
                rng: Mutex::new(Rng::new(seed)),
                crash_rng: Mutex::new(Rng::new(seed ^ 0xc0ff_ee00)),
                scheduler: Mutex::new(None),
                powered_off: AtomicBool::new(false),
                pending_deletes: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Creates a simulated file system holding what a crash left behind.
    pub fn recovered(config: SimConfig, snapshot: &CrashSnapshot) -> SimVfs {
        let vfs = SimVfs::new(config);
        for (path, bytes) in &snapshot.files {
            let inode = vfs.inode_for(path);
            let mut image = guard(&inode.image);
            image.write(vfs.state.config.model, 0, bytes);
            image.sync(vfs.state.config.model);
        }
        vfs
    }

    /// Returns the failpoint table, so a campaign can arm it.
    pub fn failpoints(&self) -> &Failpoints {
        &self.state.failpoints
    }

    /// Returns the event trace.
    pub fn trace(&self) -> &Trace {
        &self.state.trace
    }

    /// Attaches a scheduler, so that every operation is a yield point.
    pub fn attach_scheduler(&self, scheduler: Arc<Scheduler>) {
        *guard(&self.state.scheduler) = Some(scheduler);
    }

    /// Simulates a power loss and returns what the media would hold afterwards.
    ///
    /// Every handle opened before the crash stops working, because in a real
    /// power loss the process holding it is gone too. A recovery run starts
    /// from `SimVfs::recovered`.
    pub fn crash(&self) -> CrashSnapshot {
        self.state.powered_off.store(true, Ordering::SeqCst);
        let files = guard(&self.state.files);
        let mut snapshot = CrashSnapshot::default();
        let mut rng = guard(&self.state.crash_rng);
        // A deletion that was never synced to the directory may not have
        // happened. Each one is resolved independently, so a run that deleted
        // a journal without syncing the directory sees both outcomes across a
        // campaign rather than only the convenient one.
        let pending = guard(&self.state.pending_deletes);
        for (path, inode) in pending.iter() {
            if rng.chance(1, 2) {
                snapshot
                    .outcomes
                    .push(format!("{}\tdirectory entry\tRestored", path.display()));
                let image = guard(&inode.image);
                let (recovered, _) = image.crash(self.state.config.model, &mut rng);
                snapshot
                    .files
                    .insert(path.clone(), recovered.durable_bytes().to_vec());
            } else {
                snapshot
                    .outcomes
                    .push(format!("{}\tdirectory entry\tRemoved", path.display()));
            }
        }
        drop(pending);
        for (path, inode) in files.iter() {
            let image = guard(&inode.image);
            let (recovered, outcomes) = image.crash(self.state.config.model, &mut rng);
            for (sector, outcome) in outcomes {
                snapshot
                    .outcomes
                    .push(format!("{}\tsector {sector}\t{outcome:?}", path.display()));
            }
            snapshot
                .files
                .insert(path.clone(), recovered.durable_bytes().to_vec());
        }
        self.state
            .trace
            .record(current_actor().0, "crash", "", 0, 0, "power-loss");
        snapshot
    }

    /// Returns a snapshot of what one file currently holds, cached writes
    /// included, for a test that wants to compare against an oracle.
    pub fn visible_bytes(&self, path: &DbPath) -> Option<Vec<u8>> {
        let files = guard(&self.state.files);
        let inode = files.get(path.as_path())?;
        let image = guard(&inode.image);
        let mut bytes = vec![0u8; image.len() as usize];
        image.read(self.state.config.model, 0, &mut bytes);
        Some(bytes)
    }

    /// Returns or creates the inode for a path.
    fn inode_for(&self, path: &std::path::Path) -> Arc<SimInode> {
        let mut files = guard(&self.state.files);
        if let Some(existing) = files.get(path) {
            return Arc::clone(existing);
        }
        let mut counters = guard(&self.state.counters);
        counters.inode = counters.inode.saturating_add(1);
        let inode = Arc::new(SimInode {
            id: counters.inode,
            image: Mutex::new(SimFileImage::new()),
            locks: Mutex::new(LockTable::new()),
            shm: Mutex::new(None),
        });
        files.insert(path.to_path_buf(), Arc::clone(&inode));
        inode
    }
}

impl SimState {
    /// Runs the scheduler's yield point, if a scheduler is attached.
    fn yield_point(&self) {
        let scheduler = guard(&self.scheduler).clone();
        if let Some(scheduler) = scheduler {
            scheduler.yield_point(current_actor());
        }
    }

    /// Refuses every operation once the simulated machine has lost power.
    fn require_power(&self, operation: VfsOperation) -> VfsResult<()> {
        if self.powered_off.load(Ordering::SeqCst) {
            return Err(VfsError::new(
                operation.extended_code(),
                "the simulated machine has lost power",
            ));
        }
        Ok(())
    }

    /// Reaches a failpoint site, returning the failure to inject.
    fn failpoint(&self, site: Site) -> Option<Failure> {
        self.failpoints.check(site)
    }

    /// Cuts the power, whichever operation was running.
    ///
    /// Every site can lose power, not only the ones that write. A crash during
    /// a read or a lock changes nothing on the media, but it is still a cut
    /// point a systematic campaign has to be able to land on - and a campaign
    /// that silently did nothing at those calls would report a coverage number
    /// several times larger than the number of crashes it actually caused.
    fn power_off(&self, operation: VfsOperation) -> VfsError {
        self.powered_off.store(true, Ordering::SeqCst);
        VfsError::new(
            operation.extended_code(),
            "the simulated machine lost power",
        )
    }

    /// Advances the simulated clock and returns the new value.
    fn tick(&self) -> u64 {
        self.clock
            .fetch_add(self.config.clock_tick_micros, Ordering::Relaxed)
            .saturating_add(self.config.clock_tick_micros)
    }
}

impl Vfs for SimVfs {
    /// Returns the registered name of this VFS.
    fn name(&self) -> &str {
        &self.name
    }

    /// Opens a file, honouring the open failpoint.
    fn open(&self, path: &DbPath, options: OpenOptions) -> VfsResult<Box<dyn VfsFile>> {
        self.state.yield_point();
        self.state.require_power(VfsOperation::Open)?;
        if let Some(failure) = self.state.failpoint(Site::Open) {
            if failure == Failure::Crash {
                return Err(self.state.power_off(VfsOperation::Open));
            }
            if let Some(error) = failure.to_error(Site::Open) {
                self.state.trace.record(
                    current_actor().0,
                    "open",
                    &path.display(),
                    0,
                    0,
                    "injected",
                );
                return Err(error);
            }
        }
        let exists = guard(&self.state.files).contains_key(path.as_path());
        if exists && options.exclusive {
            return Err(VfsError::new(
                VfsOperation::Open.extended_code(),
                format!("{} already exists", path.display()),
            ));
        }
        if !exists && !options.create {
            return Err(VfsError::new(
                VfsOperation::Open.extended_code(),
                format!("{} does not exist", path.display()),
            ));
        }
        let inode = self.inode_for(path.as_path());
        let mut counters = guard(&self.state.counters);
        counters.handle = counters.handle.saturating_add(1);
        let handle = HandleId(counters.handle);
        drop(counters);
        self.state
            .trace
            .record(current_actor().0, "open", &path.display(), 0, 0, "ok");
        Ok(Box::new(SimFile {
            state: Arc::clone(&self.state),
            inode,
            handle,
            level: Mutex::new(FileLock::None),
            options,
            path: path.clone(),
        }))
    }

    /// Replaces `to` with `from`, and can be cut in the middle.
    ///
    /// **The cut is before the move, which is the only place it can be.** The
    /// simulator's directory is one map behind one lock, so the move itself is
    /// a single assignment nothing can observe half of - the same thing a real
    /// file system's directory update is, and the reason `VACUUM` ends with a
    /// rename rather than a copy. What a campaign is really asking is whether
    /// the database survives a machine that stopped with the rebuilt file
    /// written, the original still in place, and the rename not yet made; that
    /// is this failpoint, and recovery has to find the original.
    fn rename(&self, from: &DbPath, to: &DbPath) -> VfsResult<()> {
        self.state.yield_point();
        self.state.require_power(VfsOperation::Rename)?;
        if let Some(failure) = self.state.failpoint(Site::Rename) {
            if failure == Failure::Crash {
                self.state.powered_off.store(true, Ordering::SeqCst);
                return Err(VfsError::new(
                    VfsOperation::Rename.extended_code(),
                    "the simulated machine lost power during a rename",
                ));
            }
            if let Some(error) = failure.to_error(Site::Rename) {
                return Err(error);
            }
        }
        {
            let mut files = guard(&self.state.files);
            let Some(inode) = files.remove(from.as_path()) else {
                return Err(VfsError::new(
                    VfsOperation::Rename.extended_code(),
                    format!("rename: {} is not there", from.as_path().display()),
                ));
            };
            files.insert(to.as_path().to_path_buf(), inode);
        }
        // The destination's old entry is gone for good, so it is no longer a
        // removal waiting for a directory flush to make it durable.
        guard(&self.state.pending_deletes).remove(to.as_path());
        self.state
            .trace
            .record(current_actor().0, "rename", &from.display(), 0, 0, "ok");
        Ok(())
    }

    /// Removes a file.
    fn delete(&self, path: &DbPath, sync_dir: bool) -> VfsResult<()> {
        self.state.yield_point();
        self.state.require_power(VfsOperation::Delete)?;
        if let Some(failure) = self.state.failpoint(Site::Delete) {
            if failure == Failure::Crash {
                self.state.powered_off.store(true, Ordering::SeqCst);
                return Err(VfsError::new(
                    VfsOperation::Delete.extended_code(),
                    "the simulated machine lost power during a delete",
                ));
            }
            if let Some(error) = failure.to_error(Site::Delete) {
                return Err(error);
            }
        }
        let removed = guard(&self.state.files).remove(path.as_path());
        if sync_dir {
            // A synced directory makes every pending removal durable, not just
            // this one: the entries share the directory this call flushed.
            guard(&self.state.pending_deletes).clear();
        } else if let Some(inode) = removed {
            guard(&self.state.pending_deletes).insert(path.as_path().to_path_buf(), inode);
        }
        self.state.trace.record(
            current_actor().0,
            "delete",
            &path.display(),
            0,
            u64::from(sync_dir),
            "ok",
        );
        Ok(())
    }

    /// Reports whether a path exists.
    fn access(&self, path: &DbPath, _mode: AccessMode) -> VfsResult<bool> {
        self.state.require_power(VfsOperation::Access)?;
        Ok(guard(&self.state.files).contains_key(path.as_path()))
    }

    /// Returns the path unchanged; simulated names are already canonical.
    fn full_pathname(&self, path: &DbPath) -> VfsResult<DbPath> {
        Ok(path.clone())
    }

    /// Fills `output` from the run's seeded generator, so that randomness is
    /// part of the deterministic output rather than an escape from it.
    fn randomness(&self, output: &mut [u8]) -> VfsResult<()> {
        guard(&self.state.rng).fill(output);
        Ok(())
    }

    /// Returns the simulated clock, which advances by a fixed tick.
    fn current_time(&self) -> VfsResult<SystemTime> {
        let micros = self.state.tick();
        Ok(SystemTime::UNIX_EPOCH + Duration::from_micros(micros))
    }

    /// Returns a temporary name that is not currently in use.
    fn temp_path(&self, prefix: &str) -> VfsResult<DbPath> {
        let mut counters = guard(&self.state.counters);
        counters.temp = counters.temp.saturating_add(1);
        Ok(DbPath::new(format!(
            "/sim/tmp/{prefix}{:016x}",
            counters.temp
        )))
    }

    /// Advances the simulated clock instead of waiting.
    fn sleep(&self, micros: u64) -> VfsResult<()> {
        self.state.clock.fetch_add(micros, Ordering::Relaxed);
        self.state.yield_point();
        Ok(())
    }
}

/// One open handle on a simulated file.
#[derive(Debug)]
struct SimFile {
    state: Arc<SimState>,
    inode: Arc<SimInode>,
    handle: HandleId,
    level: Mutex<FileLock>,
    options: OpenOptions,
    path: DbPath,
}

impl SimFile {
    /// Refuses a mutating operation on a read-only handle.
    fn require_writable(&self, operation: VfsOperation) -> VfsResult<()> {
        if self.options.read_only {
            return Err(error::read_only(format!(
                "{operation:?} on a read-only handle"
            )));
        }
        Ok(())
    }
}

impl VfsFile for SimFile {
    /// Reads at `offset`, honouring the read failpoint and the short-read
    /// contract.
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> VfsResult<()> {
        self.state.yield_point();
        self.state.require_power(VfsOperation::Read)?;
        let injected = self.state.failpoint(Site::Read);
        if let Some(failure) = injected {
            if failure == Failure::Crash {
                return Err(self.state.power_off(VfsOperation::Read));
            }
            if let Some(error) = failure.to_error(Site::Read) {
                for slot in output.iter_mut() {
                    *slot = 0;
                }
                self.state.trace.record(
                    current_actor().0,
                    "read",
                    &self.path.display(),
                    offset,
                    output.len() as u64,
                    "injected",
                );
                return Err(error);
            }
        }
        let image = guard(&self.inode.image);
        let read = image.read(self.state.config.model, offset, output);
        drop(image);
        let outcome = if read < output.len() { "short" } else { "ok" };
        self.state.trace.record(
            current_actor().0,
            "read",
            &self.path.display(),
            offset,
            output.len() as u64,
            outcome,
        );
        if read < output.len() {
            return Err(error::short_read(format!(
                "read {read} of {} bytes at {offset}",
                output.len()
            )));
        }
        Ok(())
    }

    /// Writes at `offset`, honouring the write failpoint. A short write stores
    /// part of the data and reports success, which is the failure mode that
    /// finds bugs nothing else does.
    fn write_all_at(&self, offset: u64, input: &[u8]) -> VfsResult<()> {
        self.state.yield_point();
        self.state.require_power(VfsOperation::Write)?;
        self.require_writable(VfsOperation::Write)?;
        let injected = self.state.failpoint(Site::Write);
        let mut payload = input;
        if let Some(failure) = injected {
            if let Some(error) = failure.to_error(Site::Write) {
                self.state.trace.record(
                    current_actor().0,
                    "write",
                    &self.path.display(),
                    offset,
                    input.len() as u64,
                    "injected",
                );
                return Err(error);
            }
            if failure == Failure::ShortWrite {
                let keep = input.len() / 2;
                payload = input.get(..keep).unwrap_or(&[]);
            }
            if failure == Failure::Crash {
                self.state.powered_off.store(true, Ordering::SeqCst);
                return Err(VfsError::new(
                    VfsOperation::Write.extended_code(),
                    "the simulated machine lost power during a write",
                ));
            }
        }
        // **A write past the end of the file is where a buffer is allocated,
        // and where `Site::Allocate` fires (task-1932).** The site was declared
        // and never injected, so `fault_campaign.rs` could only assert that
        // three of nine sites were reached. Growing a file is what free map
        // growth does - `FreeMap::ensure` takes each new map page from the end
        // of the file - so a failure here is the one a real allocator raises
        // when the map cannot grow, reported as the read error `Site::Allocate`
        // names.
        let grows = offset.saturating_add(payload.len() as u64) > guard(&self.inode.image).len();
        if grows {
            if let Some(failure) = self.state.failpoint(Site::Allocate) {
                if let Some(error) = failure.to_error(Site::Allocate) {
                    self.state.trace.record(
                        current_actor().0,
                        "allocate",
                        &self.path.display(),
                        offset,
                        input.len() as u64,
                        "injected",
                    );
                    return Err(error);
                }
                if failure == Failure::Crash {
                    self.state.powered_off.store(true, Ordering::SeqCst);
                    return Err(VfsError::new(
                        VfsOperation::Write.extended_code(),
                        "the simulated machine lost power while the file grew",
                    ));
                }
            }
        }
        guard(&self.inode.image).write(self.state.config.model, offset, payload);
        let outcome = if payload.len() == input.len() {
            "ok"
        } else {
            "short"
        };
        self.state.trace.record(
            current_actor().0,
            "write",
            &self.path.display(),
            offset,
            input.len() as u64,
            outcome,
        );
        Ok(())
    }

    /// Returns the file's length.
    fn file_size(&self) -> VfsResult<u64> {
        self.state.require_power(VfsOperation::FileSize)?;
        Ok(guard(&self.inode.image).len())
    }

    /// Sets the file's length.
    fn truncate(&self, size: u64) -> VfsResult<()> {
        self.state.yield_point();
        self.state.require_power(VfsOperation::Truncate)?;
        self.require_writable(VfsOperation::Truncate)?;
        if let Some(failure) = self.state.failpoint(Site::Truncate) {
            if failure == Failure::Crash {
                self.state.powered_off.store(true, Ordering::SeqCst);
                return Err(VfsError::new(
                    VfsOperation::Truncate.extended_code(),
                    "the simulated machine lost power during a truncate",
                ));
            }
            if let Some(error) = failure.to_error(Site::Truncate) {
                return Err(error);
            }
        }
        guard(&self.inode.image).truncate(self.state.config.model, size);
        self.state.trace.record(
            current_actor().0,
            "truncate",
            &self.path.display(),
            size,
            0,
            "ok",
        );
        Ok(())
    }

    /// Makes every cached byte durable, honouring the sync failpoint. A failed
    /// sync leaves the cached bytes cached, which is what makes a crash after
    /// it lose them.
    fn sync(&self, mode: SyncMode) -> VfsResult<()> {
        self.state.yield_point();
        self.state.require_power(VfsOperation::Sync)?;
        self.require_writable(VfsOperation::Sync)?;
        if let Some(failure) = self.state.failpoint(Site::Sync) {
            if failure == Failure::Crash {
                self.state.powered_off.store(true, Ordering::SeqCst);
                return Err(VfsError::new(
                    VfsOperation::Sync.extended_code(),
                    "the simulated machine lost power during a sync",
                ));
            }
            if let Some(error) = failure.to_error(Site::Sync) {
                self.state.trace.record(
                    current_actor().0,
                    "sync",
                    &self.path.display(),
                    0,
                    0,
                    "injected",
                );
                return Err(error);
            }
        }
        let _ = mode;
        guard(&self.inode.image).sync(self.state.config.model);
        self.state
            .trace
            .record(current_actor().0, "sync", &self.path.display(), 0, 0, "ok");
        Ok(())
    }

    /// Raises the lock level, one protocol step at a time.
    fn lock(&self, level: FileLock) -> VfsResult<()> {
        self.state.yield_point();
        self.state.require_power(VfsOperation::Lock)?;
        if let Some(failure) = self.state.failpoint(Site::Lock) {
            if failure == Failure::Crash {
                return Err(self.state.power_off(VfsOperation::Lock));
            }
            if let Some(error) = failure.to_error(Site::Lock) {
                return Err(error);
            }
        }
        let mut current = guard(&self.level);
        if level <= *current {
            return Ok(());
        }
        let mut table = guard(&self.inode.locks);
        let mut step = *current;
        let mut refusal = None;
        while step < level {
            let next = next_step(step, level);
            match table.acquire(self.handle, step, next) {
                Ok(()) => step = next,
                Err(LockConflict::Busy) => {
                    refusal = Some(error::busy(format!("cannot raise to {next:?}")));
                    break;
                }
                Err(LockConflict::Protocol) => {
                    refusal = Some(error::misuse(format!(
                        "illegal transition {step:?} -> {next:?}"
                    )));
                    break;
                }
            }
        }
        *current = step;
        let outcome = match refusal.is_some() {
            true => format!("busy:{step:?}"),
            false => format!("ok:{step:?}"),
        };
        self.state.trace.record(
            current_actor().0,
            "lock",
            &self.path.display(),
            0,
            0,
            &outcome,
        );
        match refusal {
            Some(failure) => Err(failure),
            None => Ok(()),
        }
    }

    /// Lowers the lock level.
    fn unlock(&self, level: FileLock) -> VfsResult<()> {
        self.state.yield_point();
        self.state.require_power(VfsOperation::Unlock)?;
        let mut current = guard(&self.level);
        if level >= *current {
            return Ok(());
        }
        guard(&self.inode.locks)
            .release(self.handle, *current, level)
            .map_err(|_| error::misuse(format!("illegal release {:?} -> {level:?}", *current)))?;
        *current = level;
        self.state.trace.record(
            current_actor().0,
            "unlock",
            &self.path.display(),
            0,
            0,
            "ok",
        );
        Ok(())
    }

    /// Returns the lock level this handle holds.
    fn lock_level(&self) -> FileLock {
        *guard(&self.level)
    }

    /// Reports whether another handle holds RESERVED or stronger.
    fn check_reserved_lock(&self) -> VfsResult<bool> {
        self.state.require_power(VfsOperation::CheckReservedLock)?;
        Ok(guard(&self.inode.locks).has_reserved_or_stronger(self.handle))
    }

    /// Returns what the simulated device promises.
    fn device_characteristics(&self) -> DeviceCharacteristics {
        let model = self.state.config.model;
        DeviceCharacteristics {
            atomic_write_size: model.atomic_write_size,
            safe_append: false,
            sequential: model.sequential,
            undeletable_when_open: false,
            sector_size: model.sector_size,
            powersafe_overwrite: model.powersafe_overwrite,
            immutable: false,
            supports_mmap: false,
        }
    }

    /// Returns the shared-memory file for this database, creating it on first
    /// use.
    fn shared_memory(&self) -> VfsResult<Option<Arc<dyn SharedMemory>>> {
        if !self.options.kind.is_locked() {
            return Ok(None);
        }
        if let Some(failure) = self.state.failpoint(Site::Shm) {
            if failure == Failure::Crash {
                return Err(self.state.power_off(Site::Shm.operation()));
            }
            if let Some(error) = failure.to_error(Site::Shm) {
                return Err(error);
            }
        }
        let mut slot = guard(&self.inode.shm);
        let shm = match slot.as_ref() {
            Some(shm) => Arc::clone(shm),
            None => {
                let created = Arc::new(SimShm::default());
                *slot = Some(Arc::clone(&created));
                created
            }
        };
        Ok(Some(shm))
    }

    /// Returns the simulated inode number.
    fn file_identity(&self) -> VfsResult<FileIdentity> {
        Ok(FileIdentity {
            volume: 1,
            file: u128::from(self.inode.id),
        })
    }
}

impl Drop for SimFile {
    /// Releases the handle's locks and removes a delete-on-close file.
    fn drop(&mut self) {
        guard(&self.inode.locks).release_all(self.handle);
        if self.options.delete_on_close {
            guard(&self.state.files).remove(self.path.as_path());
        }
    }
}

/// A simulated shared-memory file.
#[derive(Debug, Default)]
struct SimShm {
    regions: Mutex<Vec<Arc<SimShmRegion>>>,
    locks: Mutex<ShmLockTable>,
}

/// One simulated shared-memory region.
#[derive(Debug)]
struct SimShmRegion {
    bytes: Mutex<Vec<u8>>,
}

impl ShmRegion for SimShmRegion {
    /// Returns the region's length.
    fn len(&self) -> usize {
        guard(&self.bytes).len()
    }

    /// Copies bytes out of the region.
    fn read(&self, offset: usize, output: &mut [u8]) -> VfsResult<()> {
        let bytes = guard(&self.bytes);
        let end = offset
            .checked_add(output.len())
            .ok_or_else(|| error::misuse("shm read overflowed"))?;
        let window = bytes
            .get(offset..end)
            .ok_or_else(|| error::misuse("shm read out of range"))?;
        for (slot, byte) in output.iter_mut().zip(window.iter()) {
            *slot = *byte;
        }
        Ok(())
    }

    /// Copies bytes into the region.
    fn write(&self, offset: usize, input: &[u8]) -> VfsResult<()> {
        let mut bytes = guard(&self.bytes);
        let end = offset
            .checked_add(input.len())
            .ok_or_else(|| error::misuse("shm write overflowed"))?;
        let window = bytes
            .get_mut(offset..end)
            .ok_or_else(|| error::misuse("shm write out of range"))?;
        for (slot, byte) in window.iter_mut().zip(input.iter()) {
            *slot = *byte;
        }
        Ok(())
    }
}

impl SharedMemory for SimShm {
    /// Maps a region, growing the file when asked to.
    fn map(
        &self,
        index: u32,
        region_size: usize,
        extend: bool,
    ) -> VfsResult<Option<Arc<dyn ShmRegion>>> {
        let mut regions = guard(&self.regions);
        let wanted = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or_else(|| error::misuse("region index overflowed"))?;
        if regions.len() < wanted {
            if !extend {
                return Ok(None);
            }
            while regions.len() < wanted {
                regions.push(Arc::new(SimShmRegion {
                    bytes: Mutex::new(vec![0u8; region_size]),
                }));
            }
        }
        match regions.get(wanted.saturating_sub(1)) {
            Some(region) => Ok(Some(Arc::clone(region) as Arc<dyn ShmRegion>)),
            None => Ok(None),
        }
    }

    /// Takes or releases shared-memory lock slots.
    fn lock(&self, request: ShmLockRequest) -> VfsResult<()> {
        if request.offset >= SHM_LOCK_COUNT || request.count == 0 {
            return Err(error::misuse("shared-memory lock slot out of range"));
        }
        guard(&self.locks).apply(request)
    }

    /// Nothing to order in a single simulated process.
    fn barrier(&self) {}

    /// Drops the mapping.
    fn unmap(&self, _delete: bool) -> VfsResult<()> {
        Ok(())
    }
}

/// Locks a mutex, recovering from poisoning rather than propagating a panic.
fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(inner) => inner,
        Err(poisoned) => poisoned.into_inner(),
    }
}
