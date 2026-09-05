//! The write-ahead log, from the pager's side.
//!
//! Invariant: storage declares *when* the log matters and knows nothing about
//! what one looks like. This is the same split the rollback [`Journal`] trait
//! makes, and it is made for the same reason: the pager owns the page cache,
//! the dirty set and the state machine, and the transaction layer owns file
//! formats. What is different about a WAL is that it changes reading as well
//! as writing - a page may live in the log rather than in the database - so the
//! trait has to answer questions on the read path too.
//!
//! The three questions the pager asks are: which snapshot am I reading, does
//! this page have a frame in it, and what are that frame's bytes. Everything
//! else - the shared-memory index, the read marks, the single writer, the
//! checkpoint - is behind the trait, because the pager has no business knowing
//! how a snapshot is arbitrated between processes.
//!
//! [`Journal`]: crate::journal::Journal

use inillucent_base::DbResult;
use inillucent_vfs::VfsFile;

/// The snapshot a read transaction is pinned to.
///
/// A snapshot is a pair: how far into the log the reader may look, and how
/// many pages the database had at that point. Both are needed, because a
/// transaction that shrank the database left frames beyond the new end that a
/// reader on the older snapshot must still see, and a reader on the newer one
/// must not.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalSnapshot {
    /// The last log frame this reader may see. Zero means the reader sees the
    /// database file alone.
    pub max_frame: u32,
    /// How many pages the database has, as of this snapshot.
    pub page_count: u32,
    /// How many times the log has been reset, which distinguishes two
    /// snapshots that happen to have the same frame count.
    pub generation: u32,
}

impl WalSnapshot {
    /// Reports whether the reader sees the database file and nothing else.
    pub fn is_database_only(self) -> bool {
        self.max_frame == 0
    }
}

/// Which checkpoint a caller asked for.
///
/// The four differ only in how long they are prepared to wait and how much
/// they insist on finishing. Every one of them copies as many frames as it
/// safely can; the modes above PASSIVE add the willingness to wait for readers
/// so that the copy can be complete, and the two above that add the reset that
/// makes the next writer start the log again from its beginning.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum CheckpointMode {
    /// Copy what can be copied without waiting for anybody.
    Passive,
    /// Wait for readers so that every frame can be copied.
    Full,
    /// As FULL, and then wait until the log can be restarted.
    Restart,
    /// As RESTART, and then shorten the log file to nothing.
    Truncate,
}

impl CheckpointMode {
    /// Parses the spelling `PRAGMA wal_checkpoint` accepts.
    pub fn parse(text: &str) -> Option<CheckpointMode> {
        match text.to_ascii_lowercase().as_str() {
            "passive" => Some(CheckpointMode::Passive),
            "full" => Some(CheckpointMode::Full),
            "restart" => Some(CheckpointMode::Restart),
            "truncate" => Some(CheckpointMode::Truncate),
            _ => None,
        }
    }

    /// Returns the spelling `PRAGMA wal_checkpoint` uses.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckpointMode::Passive => "passive",
            CheckpointMode::Full => "full",
            CheckpointMode::Restart => "restart",
            CheckpointMode::Truncate => "truncate",
        }
    }

    /// Reports whether the mode waits for readers rather than giving up.
    pub fn waits_for_readers(self) -> bool {
        self > CheckpointMode::Passive
    }

    /// Reports whether the mode restarts the log when it has copied it all.
    pub fn restarts_the_log(self) -> bool {
        self >= CheckpointMode::Restart
    }
}

/// What a checkpoint did.
///
/// The three numbers are the ones `PRAGMA wal_checkpoint` reports, and the
/// distinction between the second and the third is the whole answer to "did it
/// work": a log of 40 frames of which 40 were copied is a checkpoint that
/// finished, and one of which 12 were copied is a checkpoint that ran into a
/// reader and stopped where it was still safe.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CheckpointOutcome {
    /// Whether the checkpoint was blocked before it could start.
    pub busy: bool,
    /// How many frames the log held.
    pub log_frames: u32,
    /// How many of them are now in the database file.
    pub checkpointed_frames: u32,
    /// Whether the log was restarted from its beginning.
    pub restarted: bool,
    /// Whether the log file was shortened to nothing.
    pub truncated: bool,
    /// Whether a budget stopped the copy before it reached what was safe.
    ///
    /// Not a failure and not busy: the frames it did not take are the next
    /// commit's to take, which is the whole point of spreading the work.
    pub bounded: bool,
}

/// What the log has cost, for the write baselines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalStats {
    /// Frames appended to the log.
    pub frames_written: u64,
    /// Bytes appended to the log.
    pub bytes_written: u64,
    /// Frames read back out of the log.
    pub frames_read: u64,
    /// Times the log file was synced.
    pub syncs: u64,
    /// Checkpoints that ran, whatever they managed to copy.
    pub checkpoints: u64,
    /// Frames copied into the database by a checkpoint.
    pub frames_backfilled: u64,
    /// Times the log was recovered by reading it from its first byte.
    pub recoveries: u64,
    /// Times the log was restarted from its beginning.
    pub restarts: u64,
}

impl WalStats {
    /// Adds another log's numbers to these.
    pub fn add(&mut self, other: WalStats) {
        self.frames_written = self.frames_written.saturating_add(other.frames_written);
        self.bytes_written = self.bytes_written.saturating_add(other.bytes_written);
        self.frames_read = self.frames_read.saturating_add(other.frames_read);
        self.syncs = self.syncs.saturating_add(other.syncs);
        self.checkpoints = self.checkpoints.saturating_add(other.checkpoints);
        self.frames_backfilled = self
            .frames_backfilled
            .saturating_add(other.frames_backfilled);
        self.recoveries = self.recoveries.saturating_add(other.recoveries);
        self.restarts = self.restarts.saturating_add(other.restarts);
    }
}

/// The write-ahead log protocol, from the pager's side.
///
/// The methods are called from exactly one named point of the pager's read and
/// commit paths, and the order is the order the TDD's WAL sequence lists.
pub trait WriteAheadLog: std::fmt::Debug + Send {
    /// Opens a read transaction and returns the snapshot it is pinned to.
    ///
    /// This is where a reader takes its read mark, and where a log another
    /// process left behind is recovered, so it is the point at which the
    /// answer to "how big is the database" can change under a caller that has
    /// no transaction open.
    fn begin_read(&mut self) -> DbResult<WalSnapshot>;

    /// Ends the read transaction, releasing the read mark.
    fn end_read(&mut self) -> DbResult<()>;

    /// Returns the snapshot the open read transaction is pinned to.
    fn snapshot(&self) -> Option<WalSnapshot>;

    /// Returns the frame holding this page within the current snapshot.
    ///
    /// `None` means the page is not in the log and must be read from the
    /// database file, which is the ordinary case for a page nobody has
    /// written since the last checkpoint.
    fn frame_for(&mut self, page: u32) -> DbResult<Option<u32>>;

    /// Copies the contents of a frame into `output`.
    fn read_frame(&mut self, frame: u32, output: &mut [u8]) -> DbResult<()>;

    /// Takes the single writer lock and confirms the snapshot is still current.
    ///
    /// Fails with `SQLITE_BUSY_SNAPSHOT` when another connection has committed
    /// since this one began reading: the writer would otherwise build its
    /// changes on a database state that no longer exists.
    fn begin_write(&mut self) -> DbResult<()>;

    /// Releases the writer lock without publishing anything.
    fn end_write(&mut self) -> DbResult<()>;

    /// Appends one page image to the log.
    ///
    /// `commit_page_count` is zero for every frame but the last of a
    /// transaction, where it is the size the database has once the
    /// transaction is applied. A non-zero value is what makes a frame a commit
    /// record, so it is the caller's statement that everything before it is
    /// now durable enough to be replayed.
    fn append(&mut self, page: u32, image: &[u8], commit_page_count: u32) -> DbResult<()>;

    /// Publishes the frames appended so far, which is the commit point.
    fn publish_commit(&mut self, page_count: u32) -> DbResult<()>;

    /// Discards the frames appended since the write transaction began.
    fn undo(&mut self) -> DbResult<()>;

    /// Returns how many frames the log holds.
    fn frame_count(&self) -> u32;

    /// Copies frames into the database file.
    ///
    /// `budget` caps how many frames one call copies, which is what lets the
    /// automatic checkpoint be spread across commits instead of landing on
    /// one. `None` is "as many as are safe", which is what an explicit
    /// `PRAGMA wal_checkpoint` asks for.
    fn checkpoint(
        &mut self,
        mode: CheckpointMode,
        database: &dyn VfsFile,
        budget: Option<u32>,
    ) -> DbResult<CheckpointOutcome>;

    /// Closes the log, checkpointing and removing it when this is the last
    /// connection using it.
    fn close(&mut self, database: &dyn VfsFile) -> DbResult<()>;

    /// Returns the running totals.
    fn stats(&self) -> WalStats;

    /// Returns the frame count that triggers an automatic checkpoint, or zero
    /// when automatic checkpoints are off.
    fn auto_checkpoint(&self) -> u32;

    /// Sets the frame count that triggers an automatic checkpoint.
    fn set_auto_checkpoint(&mut self, frames: u32);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The modes order by how much they insist on, which is what the
    /// comparisons in the checkpoint path rely on.
    #[test]
    fn checkpoint_modes_order_by_insistence() {
        assert!(CheckpointMode::Passive < CheckpointMode::Full);
        assert!(CheckpointMode::Full < CheckpointMode::Restart);
        assert!(CheckpointMode::Restart < CheckpointMode::Truncate);
        assert!(!CheckpointMode::Passive.waits_for_readers());
        assert!(CheckpointMode::Full.waits_for_readers());
        assert!(!CheckpointMode::Full.restarts_the_log());
        assert!(CheckpointMode::Truncate.restarts_the_log());
    }

    /// Every spelling round-trips, because `PRAGMA wal_checkpoint` reports the
    /// mode back and a mode that parsed to something it does not print would
    /// make the report a lie.
    #[test]
    fn checkpoint_mode_spellings_round_trip() {
        for mode in [
            CheckpointMode::Passive,
            CheckpointMode::Full,
            CheckpointMode::Restart,
            CheckpointMode::Truncate,
        ] {
            assert_eq!(CheckpointMode::parse(mode.as_str()), Some(mode));
            assert_eq!(
                CheckpointMode::parse(&mode.as_str().to_ascii_uppercase()),
                Some(mode)
            );
        }
        assert_eq!(CheckpointMode::parse("sometimes"), None);
    }

    /// A snapshot with no frames is the database file on its own.
    #[test]
    fn an_empty_snapshot_is_the_database_file() {
        assert!(WalSnapshot::default().is_database_only());
        assert!(!WalSnapshot {
            max_frame: 1,
            page_count: 3,
            generation: 0,
        }
        .is_database_only());
    }
}
