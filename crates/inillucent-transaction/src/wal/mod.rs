//! The write-ahead log: readers on stable snapshots, one writer, and the
//! checkpoint that moves the log back into the database.
//!
//! Invariant: a reader's snapshot never moves under it. A reader publishes the
//! frame it is willing to see in a shared read mark before it reads anything,
//! and a checkpoint refuses to copy past the smallest mark any reader holds,
//! so a page a reader is about to ask for is still in the log when it asks.
//! That is the whole of why WAL mode lets a writer run while readers read: the
//! writer only ever appends, and appending cannot disturb a page that is
//! already there.
//!
//! The second invariant is that the log file is the truth and the index is a
//! cache of it. Every state the index can be in is either valid - two matching
//! header copies with a good checksum - or rebuilt by reading the log from its
//! first byte. A crash therefore has nothing to repair: the log's own rolling
//! checksum says where the last complete transaction ended, and everything
//! after that is discarded whether or not it was going to be a transaction.
//!
//! The third is that there is exactly one writer. It is not a scheduling
//! choice; frames are numbered, and two writers appending frame 41 would each
//! believe they had committed. The single write lock is what makes the frame
//! number a fact rather than a race.
//!
//! Module map:
//!
//! - `format` - the log file's header, frames, and rolling checksum;
//! - [`index`] - the shared-memory index, its read marks, and its lock slots;
//! - [`checkpoint`] - moving frames back into the database file.

pub mod checkpoint;
pub mod format;
pub mod index;

use std::sync::Arc;

use inillucent_base::error::{corrupt, misuse, DbError};
use inillucent_base::page::PageSize;
use inillucent_base::{DbResult, PrimaryCode};
use inillucent_storage::wal::{
    CheckpointMode, CheckpointOutcome, WalSnapshot, WalStats, WriteAheadLog,
};
use inillucent_vfs::{AccessMode, DbPath, FileKind, OpenOptions, SyncMode, Vfs, VfsFile};

use crate::journal::Synchronous;
use format::{
    frame_offset, frames_in_file, FrameHeader, WalHeader, WAL_FRAME_HEADER_SIZE, WAL_HEADER_SIZE,
};
use index::{
    read_lock, IndexHeader, WalIndex, CHECKPOINT_LOCK, READ_MARK_COUNT, READ_MARK_UNUSED,
    WRITE_LOCK,
};

/// How many times a read transaction retries before it reports BUSY.
///
/// The protocol's retry is not a back-off against contention: it is what a
/// reader does when it observes the index change between two of its own steps,
/// which is a race it loses at most as often as writers commit. A bound exists
/// so that a wedged index cannot spin forever, and it is generous because
/// every retry is cheap and a spurious BUSY is a real failure to a caller.
const READ_ATTEMPTS: u32 = 100;

/// How to open a log.
#[derive(Clone, Copy, Debug)]
pub struct WalOptions {
    /// How hard a commit is pushed toward the media.
    pub synchronous: Synchronous,
    /// How many frames the log may reach before a commit checkpoints it, or
    /// zero to never checkpoint automatically.
    pub auto_checkpoint: u32,
    /// Whether this connection may write.
    pub writable: bool,
}

impl Default for WalOptions {
    /// SQLite's defaults: FULL synchronous and a thousand-frame checkpoint.
    fn default() -> WalOptions {
        WalOptions {
            synchronous: Synchronous::Full,
            auto_checkpoint: 1_000,
            writable: true,
        }
    }
}

/// The write-ahead log of one database.
#[derive(Debug)]
pub struct Wal {
    vfs: Arc<dyn Vfs>,
    path: DbPath,
    file: Option<Box<dyn VfsFile>>,
    index: WalIndex,
    /// The connection's private copy of the index header, which is its
    /// snapshot for as long as a read transaction is open.
    header: IndexHeader,
    /// The log file header, as this connection last read or wrote it.
    log_header: Option<WalHeader>,
    page_size: PageSize,
    read_mark: Option<u16>,
    write_lock: bool,
    checkpoint_lock: bool,
    /// The lowest frame this reader is willing to look at, which is one past
    /// what a checkpoint had already copied when the snapshot was taken.
    min_frame: u32,
    /// The frame the writer has appended up to, which is ahead of the
    /// published header until the commit is published.
    append_frame: u32,
    /// The rolling checksum through `append_frame`.
    append_checksum: inillucent_base::checksum::WalChecksum,
    /// Whether this connection holds every lock slot exclusively.
    ///
    /// Closing the log takes all eight to prove it is the last user, and then
    /// runs a checkpoint - which asks for slots it is already holding. Without
    /// this, that checkpoint refuses itself and the log is never emptied,
    /// which is exactly the bug that left a log beside a database nobody was
    /// using any more.
    exclusive: bool,
    options: WalOptions,
    stats: WalStats,
    sector_size: u32,
    pad_to_sector: bool,
}

impl Wal {
    /// Opens the log beside a database, without reading or creating anything.
    ///
    /// Nothing happens on disk here. A log that exists is found by the first
    /// read transaction, and one that does not is created by the first write,
    /// which is what makes opening a database in WAL mode cost nothing when
    /// nobody has written to it.
    pub fn open(
        vfs: Arc<dyn Vfs>,
        database_path: &DbPath,
        database: &dyn VfsFile,
        page_size: PageSize,
        options: WalOptions,
    ) -> DbResult<Wal> {
        let Some(shm) = database.shared_memory()? else {
            return Err(misuse(
                "a database that cannot have shared memory cannot use a write-ahead log",
            ));
        };
        let characteristics = database.device_characteristics();
        Ok(Wal {
            vfs,
            path: database_path.wal(),
            file: None,
            index: WalIndex::new(shm),
            header: IndexHeader::default(),
            log_header: None,
            page_size,
            read_mark: None,
            write_lock: false,
            checkpoint_lock: false,
            min_frame: 0,
            append_frame: 0,
            append_checksum: inillucent_base::checksum::WalChecksum::default(),
            exclusive: false,
            options,
            stats: WalStats::default(),
            sector_size: characteristics.sector_size.max(512),
            pad_to_sector: !characteristics.powersafe_overwrite,
        })
    }

    /// Returns the path of the log file.
    pub fn path(&self) -> &DbPath {
        &self.path
    }

    /// Takes or releases index lock slots.
    ///
    /// A connection that already holds every slot exclusively asks for nothing
    /// and is refused nothing: it is the only user, and the protocol's
    /// exclusions exist to keep users apart.
    pub(crate) fn lock(
        &mut self,
        offset: u16,
        count: u16,
        exclusive: bool,
        acquire: bool,
    ) -> DbResult<()> {
        if self.exclusive {
            return Ok(());
        }
        self.index.lock(offset, count, exclusive, acquire)
    }

    /// Opens the log file, creating it when `create` is set.
    fn log_file(&mut self, create: bool) -> DbResult<Option<&mut Box<dyn VfsFile>>> {
        if self.file.is_none() {
            let exists = self.vfs.access(&self.path, AccessMode::Exists)?;
            if !exists && !create {
                return Ok(None);
            }
            let mut options = OpenOptions::of_kind(FileKind::Wal);
            options.create = create;
            options.read_only = !self.options.writable;
            self.file = Some(self.vfs.open(&self.path, options)?);
        }
        Ok(self.file.as_mut())
    }

    /// Reads the log file's header, or `None` when there is not a usable one.
    fn read_log_header(&mut self) -> DbResult<Option<WalHeader>> {
        let Some(file) = self.log_file(false)? else {
            return Ok(None);
        };
        if file.file_size()? < WAL_HEADER_SIZE as u64 {
            return Ok(None);
        }
        let mut raw = [0u8; WAL_HEADER_SIZE];
        file.read_exact_at(0, &mut raw)?;
        Ok(WalHeader::decode(&raw).ok())
    }

    /// Rebuilds the index by reading the log from its first byte.
    ///
    /// Every other connection is excluded for the duration: the index is being
    /// replaced, and a reader that saw it half-rebuilt would be reading a
    /// mixture of two logs. Taking every slot rather than the three SQLite
    /// takes costs a retry under contention and removes a class of reasoning
    /// about which slot protects which byte.
    fn recover(&mut self) -> DbResult<()> {
        self.lock(0, 8, true, true)?;
        let outcome = self.recover_locked();
        let released = self.lock(0, 8, true, false);
        outcome?;
        released?;
        self.stats.recoveries = self.stats.recoveries.saturating_add(1);
        Ok(())
    }

    /// Rebuilds the index with every lock slot already held.
    fn recover_locked(&mut self) -> DbResult<()> {
        let Some(log) = self.read_log_header()? else {
            self.header = IndexHeader {
                page_size_encoded: self.page_size.to_encoded(),
                ..IndexHeader::default()
            };
            self.log_header = None;
            self.index.write_header(&self.header)?;
            self.index.reset_checkpoint_block()?;
            return Ok(());
        };
        let scan = self.scan_log(&log)?;
        self.log_header = Some(log);
        self.page_size = log.page_size;
        self.header = IndexHeader {
            version: index::INDEX_VERSION,
            change: self.header.change.wrapping_add(1),
            initialised: true,
            big_endian_checksum: matches!(log.order, inillucent_base::checksum::WalByteOrder::Big),
            page_size_encoded: log.page_size.to_encoded(),
            max_frame: scan.max_frame,
            page_count: scan.page_count,
            frame_checksum: scan.checksum,
            salt: log.salt,
            checksum: inillucent_base::checksum::WalChecksum::default(),
        };
        self.index.truncate_to(scan.max_frame)?;
        self.index.write_header(&self.header)?;
        self.index.reset_checkpoint_block()?;
        Ok(())
    }

    /// Walks the log, entering every valid frame into the index.
    ///
    /// The walk stops at the first frame that does not continue the chain, and
    /// the transaction boundary is the last commit frame before that. Frames
    /// after the last commit are entered into the index and then dropped: a
    /// writer that was interrupted mid-transaction left them, and nothing may
    /// ever read them.
    fn scan_log(&mut self, log: &WalHeader) -> DbResult<LogScan> {
        let Some(file) = self.log_file(false)? else {
            return Ok(LogScan::empty(log.checksum));
        };
        let size = file.file_size()?;
        let frames = frames_in_file(size, log.page_size);
        let mut scan = LogScan::empty(log.checksum);
        let mut running = log.checksum;
        let mut header_raw = [0u8; WAL_FRAME_HEADER_SIZE];
        let mut image = vec![0u8; log.page_size.as_usize()];
        for frame in 1..=frames {
            let offset = frame_offset(frame, log.page_size)?;
            let Some(file) = self.file.as_mut() else {
                break;
            };
            if file.read_exact_at(offset, &mut header_raw).is_err() {
                break;
            }
            let data_offset = offset.saturating_add(WAL_FRAME_HEADER_SIZE as u64);
            if file.read_exact_at(data_offset, &mut image).is_err() {
                break;
            }
            let decoded = FrameHeader::decode(&header_raw)?;
            if !decoded.continues(&header_raw, &image, running, log.salt, log.order)? {
                break;
            }
            running = decoded.checksum;
            self.index.append(frame, decoded.page)?;
            if decoded.is_commit() {
                scan.max_frame = frame;
                scan.page_count = decoded.commit_page_count;
                scan.checksum = decoded.checksum;
            }
        }
        Ok(scan)
    }

    /// Reads the published header, rebuilding the index when there is none.
    ///
    /// Returns whether the snapshot this connection holds changed, which is
    /// what tells the pager its cached pages describe a database that has
    /// moved on.
    fn read_index_header(&mut self) -> DbResult<bool> {
        let previous = self.header;
        match self.index.header()? {
            Some(header) => {
                self.header = header;
                if let Ok(size) = header.page_size() {
                    self.page_size = size;
                }
                if self.log_header.is_none() {
                    self.log_header = self.read_log_header()?;
                }
            }
            None => self.recover()?,
        }
        Ok(previous != self.header)
    }

    /// Chooses a read mark and takes a shared lock on it.
    ///
    /// The mark says how far into the log this reader is willing to look. A
    /// checkpoint reads every mark and refuses to copy past the smallest, so
    /// publishing the mark before reading anything is what makes the snapshot
    /// stable rather than merely likely.
    fn take_read_mark(&mut self) -> DbResult<ReadOutcome> {
        let max_frame = self.header.max_frame;
        let backfill = self.index.backfill()?;
        if backfill == max_frame {
            return self.take_database_only_mark();
        }
        let mut best = 0u32;
        let mut chosen = None;
        for slot in 1..READ_MARK_COUNT {
            let mark = self.index.read_mark(slot)?;
            if mark != READ_MARK_UNUSED && best <= mark && mark <= max_frame {
                best = mark;
                chosen = Some(slot);
            }
        }
        if best < max_frame || chosen.is_none() {
            if let Some(slot) = self.claim_read_mark(max_frame)? {
                best = max_frame;
                chosen = Some(slot);
            }
        }
        let Some(slot) = chosen else {
            return Ok(ReadOutcome::Retry);
        };
        if self.lock(read_lock(slot), 1, false, true).is_err() {
            return Ok(ReadOutcome::Retry);
        }
        self.index.barrier();
        let still_ours =
            self.index.read_mark(slot)? == best && self.index.header()? == Some(self.header);
        if !still_ours {
            self.lock(read_lock(slot), 1, false, false)?;
            return Ok(ReadOutcome::Retry);
        }
        self.read_mark = Some(slot);
        self.min_frame = self.index.backfill()?.saturating_add(1);
        Ok(ReadOutcome::Taken)
    }

    /// Takes read mark zero, which says "the database file alone".
    ///
    /// It is available exactly when a checkpoint has copied the whole log, and
    /// holding it is what stops a checkpoint from restarting the log while
    /// this reader is using the database it produced.
    fn take_database_only_mark(&mut self) -> DbResult<ReadOutcome> {
        if self.lock(read_lock(0), 1, false, true).is_err() {
            return Ok(ReadOutcome::Retry);
        }
        self.index.barrier();
        if self.index.header()? != Some(self.header) {
            self.lock(read_lock(0), 1, false, false)?;
            return Ok(ReadOutcome::Retry);
        }
        self.read_mark = Some(0);
        self.min_frame = self.header.max_frame.saturating_add(1);
        Ok(ReadOutcome::Taken)
    }

    /// Moves an unused read mark up to the end of the log and returns it.
    ///
    /// The mark is raised under an exclusive lock on that one slot, so a
    /// reader cannot be holding it at a lower value while it moves.
    fn claim_read_mark(&mut self, max_frame: u32) -> DbResult<Option<u16>> {
        for slot in 1..READ_MARK_COUNT {
            if self.lock(read_lock(slot), 1, true, true).is_err() {
                continue;
            }
            let set = self.index.set_read_mark(slot, max_frame);
            let released = self.lock(read_lock(slot), 1, true, false);
            set?;
            released?;
            return Ok(Some(slot));
        }
        Ok(None)
    }

    /// Writes a header onto the log file and adopts it.
    fn put_log_header(&mut self, header: WalHeader) -> DbResult<WalHeader> {
        let raw = header.encode()?;
        let Some(file) = self.log_file(true)? else {
            return Err(misuse("the write-ahead log cannot be created"));
        };
        file.write_all_at(0, &raw)?;
        self.stats.bytes_written = self
            .stats
            .bytes_written
            .saturating_add(WAL_HEADER_SIZE as u64);
        self.log_header = Some(header);
        self.header.salt = header.salt;
        self.header.big_endian_checksum =
            matches!(header.order, inillucent_base::checksum::WalByteOrder::Big);
        self.header.page_size_encoded = header.page_size.to_encoded();
        self.header.frame_checksum = header.checksum;
        Ok(header)
    }

    /// Returns the log header a writer should append against.
    ///
    /// The log needs a header written when it is empty, and "empty" is two
    /// facts, not one: the published frame count is zero *and* this
    /// transaction has not appended anything yet. The published count does not
    /// move until the commit is published, so asking it alone says the log is
    /// empty for every frame of the first transaction - and each of them would
    /// then restart the append cursor and overwrite frame one.
    ///
    /// There are two ways to arrive at an empty log. One a checkpoint
    /// restarted already carries the salt the index publishes, so its header
    /// is written back unchanged; deriving another would move the salt a
    /// second time and make the frames the checkpoint just declared safe
    /// unreadable to everybody it had told about them. A log that is genuinely
    /// new, or one whose file was truncated, gets a header derived from the
    /// previous one where there was one, so the checkpoint sequence keeps
    /// counting.
    fn writer_header(&mut self) -> DbResult<WalHeader> {
        if self.header.max_frame != 0 || self.append_frame != 0 {
            return match self.log_header {
                Some(header) => Ok(header),
                None => Err(corrupt("a write-ahead log with frames but no header")),
            };
        }
        let reusable = self
            .log_header
            .filter(|header| header.page_size == self.page_size && header.salt == self.header.salt);
        let header = match reusable {
            Some(header) => header,
            None => {
                let mut fresh = [0u8; 4];
                self.vfs.randomness(&mut fresh)?;
                match self.log_header {
                    Some(previous) if previous.page_size == self.page_size => {
                        previous.restarted(fresh)?
                    }
                    _ => {
                        let mut salt = [0u8; 8];
                        self.vfs.randomness(&mut salt)?;
                        WalHeader::new(self.page_size, 0, salt, format::host_byte_order())?
                    }
                }
            }
        };
        let written = self.put_log_header(header)?;
        self.append_frame = 0;
        self.append_checksum = written.checksum;
        Ok(written)
    }

    /// Writes one frame at `frame`, returning the running checksum after it.
    fn write_frame(
        &mut self,
        frame: u32,
        page: u32,
        image: &[u8],
        commit_page_count: u32,
        log: &WalHeader,
    ) -> DbResult<inillucent_base::checksum::WalChecksum> {
        let raw = FrameHeader::encode(
            page,
            commit_page_count,
            log.salt,
            image,
            self.append_checksum,
            log.order,
        )?;
        let offset = frame_offset(frame, log.page_size)?;
        let Some(file) = self.log_file(true)? else {
            return Err(misuse("the write-ahead log cannot be created"));
        };
        file.write_all_at(offset, &raw)?;
        file.write_all_at(offset.saturating_add(WAL_FRAME_HEADER_SIZE as u64), image)?;
        let written = (WAL_FRAME_HEADER_SIZE as u64).saturating_add(image.len() as u64);
        self.stats.frames_written = self.stats.frames_written.saturating_add(1);
        self.stats.bytes_written = self.stats.bytes_written.saturating_add(written);
        let decoded = FrameHeader::decode(&raw)?;
        self.index.append(frame, page)?;
        Ok(decoded.checksum)
    }

    /// Pads the log past the end of the sector the last frame ends in.
    ///
    /// The padding is copies of the frame just written, so every one of them
    /// is a valid commit of the same page and recovery reaching one produces
    /// the same database. They count as frames: the next transaction starts
    /// after them, so it never rewrites a sector that holds a committed frame,
    /// which is the whole reason for the padding on a device that cannot
    /// overwrite one byte of a sector without risking the rest.
    fn pad_to_sector(
        &mut self,
        page: u32,
        image: &[u8],
        commit_page_count: u32,
        log: &WalHeader,
    ) -> DbResult<()> {
        if !self.pad_to_sector {
            return Ok(());
        }
        let stride = u64::from(log.page_size.bytes()).saturating_add(WAL_FRAME_HEADER_SIZE as u64);
        let sector = u64::from(self.sector_size).max(1);
        let mut end = frame_offset(self.append_frame, log.page_size)?.saturating_add(stride);
        let sync_point = end
            .saturating_add(sector.saturating_sub(1))
            .saturating_div(sector)
            .saturating_mul(sector);
        while end < sync_point {
            let frame = self.append_frame.saturating_add(1);
            let checksum = self.write_frame(frame, page, image, commit_page_count, log)?;
            self.append_frame = frame;
            self.append_checksum = checksum;
            end = end.saturating_add(stride);
        }
        Ok(())
    }

    /// Returns how hard a commit is synced, or `None` when it is not.
    ///
    /// WAL mode's NORMAL is the setting worth naming: it does not sync on
    /// commit at all, only at a checkpoint, which is why it is fast and why a
    /// power loss can lose the last transactions without ever corrupting the
    /// database. That is SQLite's documented trade and inillucent makes the same
    /// one rather than a quieter version of it.
    fn commit_sync(&self) -> Option<SyncMode> {
        match self.options.synchronous {
            Synchronous::Off | Synchronous::Normal => None,
            Synchronous::Full | Synchronous::Extra => Some(SyncMode::Full),
        }
    }

    /// Returns the snapshot's frame count, for diagnostics.
    pub fn max_frame(&self) -> u32 {
        self.header.max_frame
    }

    /// Runs a checkpoint with the checkpoint lock held.
    fn checkpoint_locked(
        &mut self,
        mode: CheckpointMode,
        database: &dyn VfsFile,
        budget: Option<u32>,
    ) -> DbResult<CheckpointOutcome> {
        checkpoint::run(self, mode, database, budget)
    }

    /// Returns the index, so a test can put it in a state a crash would.
    #[cfg(test)]
    fn index_mut(&mut self) -> &mut WalIndex {
        &mut self.index
    }
}

/// What a scan of the log found.
#[derive(Clone, Copy, Debug)]
struct LogScan {
    max_frame: u32,
    page_count: u32,
    checksum: inillucent_base::checksum::WalChecksum,
}

impl LogScan {
    /// Returns the result of scanning a log with no valid frames.
    fn empty(checksum: inillucent_base::checksum::WalChecksum) -> LogScan {
        LogScan {
            max_frame: 0,
            page_count: 0,
            checksum,
        }
    }
}

/// Whether a read attempt took a snapshot or has to start again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadOutcome {
    /// The snapshot is taken and the read mark is held.
    Taken,
    /// Something changed under the attempt; start again.
    Retry,
}

impl WriteAheadLog for Wal {
    /// Opens a read transaction, recovering the index when it has to.
    fn begin_read(&mut self) -> DbResult<WalSnapshot> {
        if self.read_mark.is_some() {
            return Ok(self.snapshot().unwrap_or_default());
        }
        for _ in 0..READ_ATTEMPTS {
            self.read_index_header()?;
            match self.take_read_mark()? {
                ReadOutcome::Taken => {
                    self.append_frame = self.header.max_frame;
                    self.append_checksum = self.header.frame_checksum;
                    return Ok(self.snapshot().unwrap_or_default());
                }
                ReadOutcome::Retry => {
                    self.vfs.sleep(1)?;
                }
            }
        }
        Err(DbError::primary(PrimaryCode::Busy)
            .with_message("database is locked")
            .with_detail("the write-ahead log index kept changing under a reader"))
    }

    /// Ends the read transaction, releasing the read mark.
    fn end_read(&mut self) -> DbResult<()> {
        if let Some(slot) = self.read_mark.take() {
            self.lock(read_lock(slot), 1, false, false)?;
        }
        self.min_frame = 0;
        Ok(())
    }

    /// Returns the snapshot the open read transaction is pinned to.
    fn snapshot(&self) -> Option<WalSnapshot> {
        self.read_mark?;
        Some(WalSnapshot {
            max_frame: self.header.max_frame,
            page_count: self.header.page_count,
            generation: self.header.change,
        })
    }

    /// Returns the frame holding a page within the snapshot.
    fn frame_for(&mut self, page: u32) -> DbResult<Option<u32>> {
        if self.read_mark.is_none() {
            return Ok(None);
        }
        self.index
            .find_frame(page, self.min_frame, self.header.max_frame)
    }

    /// Copies a frame's page image out of the log.
    fn read_frame(&mut self, frame: u32, output: &mut [u8]) -> DbResult<()> {
        let offset =
            frame_offset(frame, self.page_size)?.saturating_add(WAL_FRAME_HEADER_SIZE as u64);
        let Some(file) = self.log_file(false)? else {
            return Err(corrupt(
                "a frame was asked for from a log that is not there",
            ));
        };
        file.read_exact_at(offset, output)?;
        self.stats.frames_read = self.stats.frames_read.saturating_add(1);
        Ok(())
    }

    /// Takes the write lock and confirms the snapshot is still the newest.
    fn begin_write(&mut self) -> DbResult<()> {
        if self.write_lock {
            return Ok(());
        }
        if !self.options.writable {
            return Err(DbError::primary(PrimaryCode::ReadOnly)
                .with_message("attempt to write a readonly database"));
        }
        self.lock(WRITE_LOCK, 1, true, true)?;
        self.write_lock = true;
        let published = self.index.header()?;
        if published != Some(self.header) {
            self.lock(WRITE_LOCK, 1, true, false)?;
            self.write_lock = false;
            return Err(DbError::new(inillucent_base::ExtendedCode(517))
                .with_message("database is locked")
                .with_detail("another connection committed after this snapshot was taken"));
        }
        self.append_frame = self.header.max_frame;
        self.append_checksum = self.header.frame_checksum;
        Ok(())
    }

    /// Releases the write lock.
    fn end_write(&mut self) -> DbResult<()> {
        if self.write_lock {
            self.lock(WRITE_LOCK, 1, true, false)?;
            self.write_lock = false;
        }
        Ok(())
    }

    /// Appends one page image to the log.
    fn append(&mut self, page: u32, image: &[u8], commit_page_count: u32) -> DbResult<()> {
        if !self.write_lock {
            return Err(misuse("a frame was appended without the write lock"));
        }
        if image.len() != self.page_size.as_usize() {
            return Err(misuse("a frame whose image is not one page"));
        }
        let log = self.writer_header()?;
        let frame = self.append_frame.saturating_add(1);
        let checksum = self.write_frame(frame, page, image, commit_page_count, &log)?;
        self.append_frame = frame;
        self.append_checksum = checksum;
        if commit_page_count != 0 {
            self.pad_to_sector(page, image, commit_page_count, &log)?;
        }
        Ok(())
    }

    /// Publishes the appended frames, which is the commit point.
    ///
    /// The order is the argument. The frames are made durable first, so that
    /// a reader that later sees them has something to read; the header is
    /// published second, and it is one write of two copies with a barrier
    /// between them, so a reader either sees the whole new snapshot or the
    /// whole old one.
    fn publish_commit(&mut self, page_count: u32) -> DbResult<()> {
        if !self.write_lock {
            return Err(misuse("a commit was published without the write lock"));
        }
        if let Some(mode) = self.commit_sync() {
            if let Some(file) = self.log_file(true)? {
                file.sync(mode)?;
                self.stats.syncs = self.stats.syncs.saturating_add(1);
            }
        }
        self.header.max_frame = self.append_frame;
        self.header.page_count = page_count;
        self.header.frame_checksum = self.append_checksum;
        self.header.change = self.header.change.wrapping_add(1);
        self.header.page_size_encoded = self.page_size.to_encoded();
        self.index.write_header(&self.header)?;
        Ok(())
    }

    /// Discards the frames appended since the write transaction began.
    ///
    /// Nothing is erased from the log file: the frames are simply not
    /// published, and the next writer overwrites them. What has to be undone
    /// is the index, which the appends did change, because it is shared.
    fn undo(&mut self) -> DbResult<()> {
        if self.append_frame > self.header.max_frame {
            self.index.truncate_to(self.header.max_frame)?;
        }
        self.append_frame = self.header.max_frame;
        self.append_checksum = self.header.frame_checksum;
        Ok(())
    }

    /// Returns how many frames the published log holds.
    fn frame_count(&self) -> u32 {
        self.header.max_frame
    }

    /// Copies frames into the database file.
    fn checkpoint(
        &mut self,
        mode: CheckpointMode,
        database: &dyn VfsFile,
        budget: Option<u32>,
    ) -> DbResult<CheckpointOutcome> {
        if self.lock(CHECKPOINT_LOCK, 1, true, true).is_err() {
            return Ok(CheckpointOutcome {
                busy: true,
                log_frames: self.header.max_frame,
                ..CheckpointOutcome::default()
            });
        }
        self.checkpoint_lock = true;
        let outcome = self.checkpoint_locked(mode, database, budget);
        let released = self.lock(CHECKPOINT_LOCK, 1, true, false);
        self.checkpoint_lock = false;
        let outcome = outcome?;
        released?;
        self.stats.checkpoints = self.stats.checkpoints.saturating_add(1);
        Ok(outcome)
    }

    /// Closes the log, removing it when this is the last connection.
    ///
    /// The test for "last" is the lock itself: a connection that can take
    /// every slot exclusively is the only one using the log, so checkpointing
    /// it and deleting it cannot strand anybody. One that cannot leaves the
    /// files where they are, which is correct and costs the next opener a
    /// recovery at worst.
    fn close(&mut self, database: &dyn VfsFile) -> DbResult<()> {
        self.end_read()?;
        self.end_write()?;
        if !self.options.writable {
            return self.index.unmap(false);
        }
        if self.index.lock(0, 8, true, true).is_err() {
            return self.index.unmap(false);
        }
        self.exclusive = true;
        let checkpointed = checkpoint::run(self, CheckpointMode::Truncate, database, None);
        // The log goes when there is nothing left in it, which is a stronger
        // test than "the checkpoint said it truncated": a log that was already
        // empty is one nobody needs either.
        let empty = checkpointed.is_ok() && self.header.max_frame == 0;
        let removed = if empty {
            self.file = None;
            self.vfs.delete(&self.path, false)
        } else {
            Ok(())
        };
        self.exclusive = false;
        let unlocked = self.index.lock(0, 8, true, false);
        let unmapped = self.index.unmap(empty);
        checkpointed?;
        removed?;
        unlocked?;
        unmapped
    }

    /// Returns the running totals.
    fn stats(&self) -> WalStats {
        self.stats
    }

    /// Returns the automatic checkpoint threshold in frames.
    fn auto_checkpoint(&self) -> u32 {
        self.options.auto_checkpoint
    }

    /// Sets the automatic checkpoint threshold in frames.
    fn set_auto_checkpoint(&mut self, frames: u32) {
        self.options.auto_checkpoint = frames;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_vfs::MemoryVfs;

    /// Opens a log over a memory database file of one page.
    fn open_wal(vfs: &Arc<dyn Vfs>, name: &str) -> (Wal, Box<dyn VfsFile>) {
        let path = DbPath::new(std::path::PathBuf::from(name));
        let database = vfs.open(&path, OpenOptions::main_db()).unwrap();
        database.write_all_at(0, &[0u8; 512]).unwrap();
        let wal = Wal::open(
            Arc::clone(vfs),
            &path,
            database.as_ref(),
            PageSize::new(512).unwrap(),
            WalOptions::default(),
        )
        .unwrap();
        (wal, database)
    }

    /// A page written and committed is read back from the log by a reader that
    /// starts afterwards.
    #[test]
    fn a_committed_frame_is_visible_to_the_next_reader() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut wal, _database) = open_wal(&vfs, "/commit.db");
        wal.begin_read().unwrap();
        wal.begin_write().unwrap();
        let image = vec![7u8; 512];
        wal.append(3, &image, 4).unwrap();
        wal.publish_commit(4).unwrap();
        wal.end_write().unwrap();
        wal.end_read().unwrap();

        let snapshot = wal.begin_read().unwrap();
        assert_eq!(snapshot.page_count, 4);
        assert!(snapshot.max_frame >= 1);
        let frame = wal.frame_for(3).unwrap().unwrap();
        let mut out = vec![0u8; 512];
        wal.read_frame(frame, &mut out).unwrap();
        assert_eq!(out, image);
        assert_eq!(wal.frame_for(2).unwrap(), None);
    }

    /// An unpublished append is invisible, and undoing it leaves the log the
    /// way the last commit left it.
    #[test]
    fn an_unpublished_append_is_not_a_commit() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut wal, _database) = open_wal(&vfs, "/undo.db");
        wal.begin_read().unwrap();
        wal.begin_write().unwrap();
        wal.append(2, &vec![1u8; 512], 2).unwrap();
        wal.publish_commit(2).unwrap();
        let committed = wal.frame_count();
        wal.append(2, &vec![9u8; 512], 0).unwrap();
        assert_eq!(wal.frame_count(), committed);
        wal.undo().unwrap();
        wal.end_write().unwrap();
        let frame = wal.frame_for(2).unwrap().unwrap();
        let mut out = vec![0u8; 512];
        wal.read_frame(frame, &mut out).unwrap();
        assert_eq!(out, vec![1u8; 512]);
    }

    /// A second connection rebuilds the index from the log alone, which is
    /// what a connection that finds no shared memory has to do.
    #[test]
    fn the_index_is_rebuilt_from_the_log() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut wal, database) = open_wal(&vfs, "/recover.db");
        wal.begin_read().unwrap();
        wal.begin_write().unwrap();
        wal.append(2, &vec![5u8; 512], 2).unwrap();
        wal.publish_commit(2).unwrap();
        wal.end_write().unwrap();
        wal.end_read().unwrap();

        // Zeroing the header is what a connection that could not trust the
        // index would leave behind; the next reader must not believe it.
        wal.index_mut().invalidate_header().unwrap();
        let mut second = Wal::open(
            Arc::clone(&vfs),
            &DbPath::new(std::path::PathBuf::from("/recover.db")),
            database.as_ref(),
            PageSize::new(512).unwrap(),
            WalOptions::default(),
        )
        .unwrap();
        let snapshot = second.begin_read().unwrap();
        assert_eq!(snapshot.page_count, 2);
        assert_eq!(second.stats().recoveries, 1);
        let frame = second.frame_for(2).unwrap().unwrap();
        let mut out = vec![0u8; 512];
        second.read_frame(frame, &mut out).unwrap();
        assert_eq!(out, vec![5u8; 512]);
    }

    /// A torn tail is discarded: recovery stops at the first frame whose
    /// checksum does not continue the chain.
    #[test]
    fn a_torn_tail_is_discarded() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut wal, database) = open_wal(&vfs, "/torn.db");
        wal.begin_read().unwrap();
        wal.begin_write().unwrap();
        wal.append(2, &vec![1u8; 512], 2).unwrap();
        wal.publish_commit(2).unwrap();
        let good = wal.frame_count();
        wal.append(3, &vec![2u8; 512], 3).unwrap();
        wal.publish_commit(3).unwrap();
        wal.end_write().unwrap();
        wal.end_read().unwrap();

        // Damage the second transaction's frame, then rebuild.
        let log = vfs
            .open(
                &DbPath::new(std::path::PathBuf::from("/torn.db-wal")),
                OpenOptions::of_kind(FileKind::Wal),
            )
            .unwrap();
        let offset = frame_offset(good.saturating_add(1), PageSize::new(512).unwrap()).unwrap();
        log.write_all_at(offset.saturating_add(30), &[0xff; 8])
            .unwrap();
        wal.index_mut().invalidate_header().unwrap();

        let mut second = Wal::open(
            Arc::clone(&vfs),
            &DbPath::new(std::path::PathBuf::from("/torn.db")),
            database.as_ref(),
            PageSize::new(512).unwrap(),
            WalOptions::default(),
        )
        .unwrap();
        let snapshot = second.begin_read().unwrap();
        assert_eq!(snapshot.page_count, 2, "the torn transaction was kept");
        assert_eq!(second.frame_for(3).unwrap(), None);
    }

    /// Two connections cannot both hold the write lock.
    #[test]
    fn only_one_connection_writes() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut first, database) = open_wal(&vfs, "/writers.db");
        let mut second = Wal::open(
            Arc::clone(&vfs),
            &DbPath::new(std::path::PathBuf::from("/writers.db")),
            database.as_ref(),
            PageSize::new(512).unwrap(),
            WalOptions::default(),
        )
        .unwrap();
        first.begin_read().unwrap();
        second.begin_read().unwrap();
        first.begin_write().unwrap();
        let refused = second.begin_write();
        assert!(refused.is_err());
        assert_eq!(refused.unwrap_err().code(), PrimaryCode::Busy);
        first.end_write().unwrap();
        second.begin_write().unwrap();
    }

    /// A writer whose snapshot is stale is refused with BUSY_SNAPSHOT rather
    /// than being allowed to build on a database state that has moved.
    #[test]
    fn a_stale_writer_is_refused_with_busy_snapshot() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut first, database) = open_wal(&vfs, "/stale.db");
        let mut second = Wal::open(
            Arc::clone(&vfs),
            &DbPath::new(std::path::PathBuf::from("/stale.db")),
            database.as_ref(),
            PageSize::new(512).unwrap(),
            WalOptions::default(),
        )
        .unwrap();
        second.begin_read().unwrap();

        first.begin_read().unwrap();
        first.begin_write().unwrap();
        first.append(2, &vec![3u8; 512], 2).unwrap();
        first.publish_commit(2).unwrap();
        first.end_write().unwrap();
        first.end_read().unwrap();

        let refused = second.begin_write();
        let error = refused.unwrap_err();
        assert_eq!(error.code(), PrimaryCode::Busy);
        assert_eq!(error.extended().value(), 517);
    }

    /// A reader that took its snapshot before a commit keeps seeing the old
    /// page while the writer commits a new one.
    #[test]
    fn a_reader_keeps_its_snapshot_across_a_commit() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut writer, database) = open_wal(&vfs, "/snapshot.db");
        writer.begin_read().unwrap();
        writer.begin_write().unwrap();
        writer.append(2, &vec![1u8; 512], 2).unwrap();
        writer.publish_commit(2).unwrap();
        writer.end_write().unwrap();
        writer.end_read().unwrap();

        let mut reader = Wal::open(
            Arc::clone(&vfs),
            &DbPath::new(std::path::PathBuf::from("/snapshot.db")),
            database.as_ref(),
            PageSize::new(512).unwrap(),
            WalOptions::default(),
        )
        .unwrap();
        reader.begin_read().unwrap();
        let first_frame = reader.frame_for(2).unwrap().unwrap();

        writer.begin_read().unwrap();
        writer.begin_write().unwrap();
        writer.append(2, &vec![2u8; 512], 2).unwrap();
        writer.publish_commit(2).unwrap();
        writer.end_write().unwrap();
        writer.end_read().unwrap();

        assert_eq!(reader.frame_for(2).unwrap(), Some(first_frame));
        let mut out = vec![0u8; 512];
        reader.read_frame(first_frame, &mut out).unwrap();
        assert_eq!(out, vec![1u8; 512]);
    }
}
