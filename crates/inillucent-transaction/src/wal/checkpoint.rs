//! Moving frames out of the log and back into the database file.
//!
//! Invariant: a frame is copied into the database only once every reader that
//! could still want to read it from the log has gone. The read marks say what
//! each reader is willing to see; the checkpoint copies up to the smallest of
//! them and no further, so a page a reader is about to ask for is still where
//! the reader's snapshot says it is. This is the one place where "the log is
//! append-only" stops being true, and the read marks are what make it safe.
//!
//! The second invariant is that a checkpoint is restartable. It copies pages,
//! syncs the database, and only then records how far it got. A crash before
//! that record leaves frames that will be copied again - writing the same
//! bytes to the same offsets, because a frame's contents never change - and a
//! crash after it leaves a database that already holds them. Neither outcome
//! needs repair.
//!
//! Reference: <https://sqlite.org/wal.html#checkpointing>.

use std::collections::BTreeMap;

use inillucent_base::page::{self, PageSize};
use inillucent_base::DbResult;
use inillucent_storage::wal::{CheckpointMode, CheckpointOutcome};
use inillucent_vfs::{SyncMode, VfsFile};

use super::format::{frame_offset, WAL_FRAME_HEADER_SIZE};
use super::index::{read_lock, READ_MARK_COUNT, READ_MARK_UNUSED};
use super::Wal;
use crate::journal::Synchronous;

/// Runs a checkpoint with the checkpoint lock already held.
///
/// The four modes differ only in what they do after the copying: PASSIVE stops
/// there, FULL has already waited for readers on the way in, and the two above
/// it restart the log so the next writer begins at its first frame again.
pub fn run(
    wal: &mut Wal,
    mode: CheckpointMode,
    database: &dyn VfsFile,
    budget: Option<u32>,
) -> DbResult<CheckpointOutcome> {
    if wal.index.header()?.is_none() {
        wal.recover()?;
    } else if let Some(header) = wal.index.header()? {
        wal.header = header;
        if let Ok(size) = header.page_size() {
            wal.page_size = size;
        }
        if wal.log_header.is_none() {
            wal.log_header = wal.read_log_header()?;
        }
    }
    let mut outcome = CheckpointOutcome {
        log_frames: wal.header.max_frame,
        ..CheckpointOutcome::default()
    };
    if wal.header.max_frame == 0 {
        outcome.checkpointed_frames = 0;
        return Ok(outcome);
    }
    let safe = safe_frame(wal, mode)?;
    // A budget stops the copy short of what is safe, on purpose. The backfill
    // point is in the shared index, so where this one stops is where the next
    // one starts - it is the same job, spread over the commits that caused it,
    // rather than a job left half done.
    let safe = match budget {
        Some(budget) => safe.min(wal.index.backfill()?.saturating_add(budget)),
        None => safe,
    };
    backfill(wal, database, safe, &mut outcome)?;
    let copied = wal.index.backfill()?;
    outcome.checkpointed_frames = copied;
    outcome.bounded = budget.is_some() && copied < wal.header.max_frame;
    // A mode that promised to copy the whole log has to say when it did not.
    // `PASSIVE` promised nothing, so a reader in its way is an ordinary
    // outcome; for the three above it a short copy is the `SQLITE_BUSY` the
    // caller is waiting to hear, and reporting success instead would have an
    // application believe its log had been emptied when it had not.
    if mode.waits_for_readers() && copied < wal.header.max_frame && budget.is_none() {
        outcome.busy = true;
    }
    if mode.restarts_the_log() && copied == wal.header.max_frame {
        restart(wal, mode, &mut outcome)?;
    }
    Ok(outcome)
}

/// Returns the highest frame it is safe to copy into the database.
///
/// A reader mark below the end of the log pins every frame from that point on,
/// so the answer is the smallest mark any reader still holds. Marks nobody
/// holds are moved up out of the way first, which is what stops an abandoned
/// low mark from blocking every checkpoint for the life of the file.
fn safe_frame(wal: &mut Wal, mode: CheckpointMode) -> DbResult<u32> {
    let mut safe = wal.header.max_frame;
    for slot in 1..READ_MARK_COUNT {
        let mark = wal.index.read_mark(slot)?;
        if mark == READ_MARK_UNUSED || safe <= mark {
            continue;
        }
        if wal.lock(read_lock(slot), 1, true, true).is_ok() {
            let raised = if slot == 1 { safe } else { READ_MARK_UNUSED };
            let set = wal.index.set_read_mark(slot, raised);
            let released = wal.lock(read_lock(slot), 1, true, false);
            set?;
            released?;
            continue;
        }
        // The mark is held by a live reader, so the frames from it on stay.
        // Every mode stops here, including the ones whose names suggest they
        // wait: waiting for a reader that is inside a long query is a wedge
        // rather than a wait, and the caller has a busy handler of its own to
        // decide with. What the waiting modes do differently is *report* the
        // short copy, which `run` does above.
        let _ = mode;
        safe = safe.min(mark);
    }
    Ok(safe)
}

/// Copies frames into the database file.
fn backfill(
    wal: &mut Wal,
    database: &dyn VfsFile,
    safe: u32,
    outcome: &mut CheckpointOutcome,
) -> DbResult<()> {
    let start = wal.index.backfill()?;
    if start >= safe {
        return Ok(());
    }
    if wal.lock(read_lock(0), 1, true, true).is_err() {
        outcome.busy = true;
        return Ok(());
    }
    let copied = copy_frames(wal, database, start, safe);
    let released = wal.lock(read_lock(0), 1, true, false);
    copied?;
    released?;
    Ok(())
}

/// Copies frames `after..=safe`, truncates, syncs, and records the progress.
///
/// The order is what makes a crash harmless: every page is in the database and
/// the database is durable before the log is told that it is. The record is
/// last because it is the only irreversible step - once a frame is declared
/// backfilled, a reader may be given the database's copy instead of the log's.
fn copy_frames(wal: &mut Wal, database: &dyn VfsFile, after: u32, safe: u32) -> DbResult<()> {
    wal.index.set_backfill_attempted(safe)?;
    let newest = newest_frames(wal, after, safe)?;
    let page_size = wal.page_size;
    let mut image = vec![0u8; page_size.as_usize()];
    let mut written = 0u64;
    for (page, frame) in newest {
        if page > wal.header.page_count {
            continue;
        }
        read_frame_image(wal, frame, &mut image)?;
        let Ok(page_id) = inillucent_base::ids::PageId::from_persisted(page) else {
            continue;
        };
        let offset = page::page_offset(page_size, page_id)?;
        database.write_all_at(offset, &image)?;
        written = written.saturating_add(1);
    }
    wal.stats.frames_backfilled = wal.stats.frames_backfilled.saturating_add(written);
    if safe == wal.header.max_frame {
        truncate_database(database, page_size, wal.header.page_count)?;
    }
    if let Some(mode) = checkpoint_sync(wal.options.synchronous) {
        database.sync(mode)?;
        wal.stats.syncs = wal.stats.syncs.saturating_add(1);
    }
    wal.index.set_backfill(safe)?;
    Ok(())
}

/// Returns the newest frame holding each page in `after..=safe`.
///
/// A page written five times in the range is copied once, from its last frame,
/// and the map's ordering makes the copies run up the file rather than jumping
/// about in it.
fn newest_frames(wal: &mut Wal, after: u32, safe: u32) -> DbResult<BTreeMap<u32, u32>> {
    let mut newest = BTreeMap::new();
    for frame in after.saturating_add(1)..=safe {
        if let Some(page) = wal.index.page_of(frame)? {
            newest.insert(page, frame);
        }
    }
    Ok(newest)
}

/// Reads one frame's page image out of the log.
fn read_frame_image(wal: &mut Wal, frame: u32, image: &mut [u8]) -> DbResult<()> {
    let offset = frame_offset(frame, wal.page_size)?.saturating_add(WAL_FRAME_HEADER_SIZE as u64);
    let Some(file) = wal.log_file(false)? else {
        return Err(inillucent_base::error::corrupt(
            "a checkpoint found no log to copy from",
        ));
    };
    file.read_exact_at(offset, image)?;
    wal.stats.frames_read = wal.stats.frames_read.saturating_add(1);
    Ok(())
}

/// Shortens the database to the size the log's last commit declared.
fn truncate_database(database: &dyn VfsFile, page_size: PageSize, page_count: u32) -> DbResult<()> {
    let wanted = page::file_size(page_size, page_count)?;
    if database.file_size()? > wanted {
        database.truncate(wanted)?;
    }
    Ok(())
}

/// Returns how hard the database is synced by a checkpoint.
///
/// This is the sync WAL mode's NORMAL setting keeps: a commit is not synced,
/// but a checkpoint is, because a checkpoint is the point at which the log
/// stops being the record of what was committed.
fn checkpoint_sync(synchronous: Synchronous) -> Option<SyncMode> {
    match synchronous {
        Synchronous::Off => None,
        Synchronous::Normal => Some(SyncMode::Normal),
        Synchronous::Full | Synchronous::Extra => Some(SyncMode::Full),
    }
}

/// Restarts the log so the next writer begins at its first frame.
///
/// Every reader has to be gone first, and the test is the lock: a reader holds
/// a shared lock on its mark for as long as its snapshot is open, so taking
/// all four exclusively is the proof that no snapshot survives. The salts move
/// on, which is what makes every frame already in the file unreadable rather
/// than merely unreferenced.
fn restart(wal: &mut Wal, mode: CheckpointMode, outcome: &mut CheckpointOutcome) -> DbResult<()> {
    let first = read_lock(1);
    let count = READ_MARK_COUNT.saturating_sub(1);
    if wal.lock(first, count, true, true).is_err() {
        outcome.busy = true;
        return Ok(());
    }
    let restarted = restart_locked(wal, mode, outcome);
    let released = wal.lock(first, count, true, false);
    restarted?;
    released?;
    Ok(())
}

/// Restarts the log with every reader mark held exclusively.
fn restart_locked(
    wal: &mut Wal,
    mode: CheckpointMode,
    outcome: &mut CheckpointOutcome,
) -> DbResult<()> {
    let mut fresh = [0u8; 4];
    wal.vfs.randomness(&mut fresh)?;
    let Some(previous) = wal.log_header else {
        return Ok(());
    };
    let header = previous.restarted(fresh)?;
    if mode == CheckpointMode::Truncate {
        if let Some(file) = wal.log_file(false)? {
            file.truncate(0)?;
            file.sync(SyncMode::Normal)?;
        }
        outcome.truncated = true;
    } else {
        let raw = header.encode()?;
        if let Some(file) = wal.log_file(true)? {
            file.write_all_at(0, &raw)?;
        }
    }
    wal.log_header = Some(header);
    wal.header.max_frame = 0;
    wal.header.salt = header.salt;
    wal.header.frame_checksum = header.checksum;
    wal.header.change = wal.header.change.wrapping_add(1);
    wal.index.write_header(&wal.header.clone())?;
    wal.index.reset_checkpoint_block()?;
    wal.append_frame = 0;
    wal.append_checksum = header.checksum;
    wal.stats.restarts = wal.stats.restarts.saturating_add(1);
    outcome.restarted = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use inillucent_storage::wal::WriteAheadLog;
    use inillucent_vfs::{DbPath, MemoryVfs, OpenOptions, Vfs};

    use super::*;
    use crate::wal::{Wal, WalOptions};

    /// Opens a log over a memory database of `pages` zeroed pages.
    fn open(vfs: &Arc<dyn Vfs>, name: &str, pages: u32) -> (Wal, Box<dyn VfsFile>) {
        let path = DbPath::new(std::path::PathBuf::from(name));
        let database = vfs.open(&path, OpenOptions::main_db()).unwrap();
        database
            .write_all_at(0, &vec![0u8; (pages as usize) * 512])
            .unwrap();
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

    /// Commits one page through the log.
    fn commit(wal: &mut Wal, page: u32, fill: u8, pages: u32) {
        wal.begin_read().unwrap();
        wal.begin_write().unwrap();
        wal.append(page, &vec![fill; 512], pages).unwrap();
        wal.publish_commit(pages).unwrap();
        wal.end_write().unwrap();
        wal.end_read().unwrap();
    }

    /// A checkpoint puts the log's pages in the database file, where a reader
    /// that never looks at the log can find them.
    #[test]
    fn a_checkpoint_moves_pages_into_the_database() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut wal, database) = open(&vfs, "/ckpt.db", 3);
        commit(&mut wal, 2, 0xaa, 3);
        commit(&mut wal, 3, 0xbb, 3);
        let outcome = wal
            .checkpoint(CheckpointMode::Passive, database.as_ref(), None)
            .unwrap();
        assert!(!outcome.busy);
        assert_eq!(outcome.checkpointed_frames, outcome.log_frames);
        let mut page = vec![0u8; 512];
        database.read_exact_at(512, &mut page).unwrap();
        assert_eq!(page, vec![0xaa; 512]);
        database.read_exact_at(1024, &mut page).unwrap();
        assert_eq!(page, vec![0xbb; 512]);
    }

    /// A live reader stops the checkpoint at the frame that reader can see,
    /// and the frames it pinned are still readable from the log.
    #[test]
    fn a_reader_holds_the_checkpoint_back() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut writer, database) = open(&vfs, "/held.db", 3);
        commit(&mut writer, 2, 0x11, 3);

        let mut reader = Wal::open(
            Arc::clone(&vfs),
            &DbPath::new(std::path::PathBuf::from("/held.db")),
            database.as_ref(),
            PageSize::new(512).unwrap(),
            WalOptions::default(),
        )
        .unwrap();
        reader.begin_read().unwrap();
        let pinned = reader.frame_for(2).unwrap().unwrap();

        commit(&mut writer, 2, 0x22, 3);
        let outcome = writer
            .checkpoint(CheckpointMode::Passive, database.as_ref(), None)
            .unwrap();
        assert!(outcome.checkpointed_frames < outcome.log_frames);

        let mut image = vec![0u8; 512];
        reader.read_frame(pinned, &mut image).unwrap();
        assert_eq!(image, vec![0x11; 512]);
    }

    /// A truncating checkpoint empties the log file and starts the next
    /// transaction at its first frame again.
    #[test]
    fn a_truncating_checkpoint_empties_the_log() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut wal, database) = open(&vfs, "/truncate.db", 3);
        commit(&mut wal, 2, 0x33, 3);
        commit(&mut wal, 3, 0x44, 3);
        let outcome = wal
            .checkpoint(CheckpointMode::Truncate, database.as_ref(), None)
            .unwrap();
        assert!(outcome.restarted, "the log was not restarted");
        assert!(outcome.truncated, "the log file was not shortened");
        let log = vfs
            .open(
                &DbPath::new(std::path::PathBuf::from("/truncate.db-wal")),
                OpenOptions::of_kind(inillucent_vfs::FileKind::Wal),
            )
            .unwrap();
        assert_eq!(log.file_size().unwrap(), 0);

        commit(&mut wal, 2, 0x55, 3);
        assert_eq!(wal.frame_count(), 1, "the log did not restart at frame one");
        let mut image = vec![0u8; 512];
        wal.begin_read().unwrap();
        let frame = wal.frame_for(2).unwrap().unwrap();
        wal.read_frame(frame, &mut image).unwrap();
        assert_eq!(image, vec![0x55; 512]);
        wal.end_read().unwrap();
    }

    /// Checkpointing twice with nothing in between copies nothing the second
    /// time, which is what makes an automatic checkpoint cheap.
    #[test]
    fn a_second_checkpoint_has_nothing_to_do() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut wal, database) = open(&vfs, "/twice.db", 3);
        commit(&mut wal, 2, 0x66, 3);
        let first = wal
            .checkpoint(CheckpointMode::Passive, database.as_ref(), None)
            .unwrap();
        let before = wal.stats().frames_backfilled;
        let second = wal
            .checkpoint(CheckpointMode::Passive, database.as_ref(), None)
            .unwrap();
        assert_eq!(first.checkpointed_frames, second.checkpointed_frames);
        assert_eq!(wal.stats().frames_backfilled, before);
    }

    /// A checkpoint that shrank the database shortens the file, so the pages a
    /// dropped table used stop occupying disk.
    #[test]
    fn a_checkpoint_shortens_a_database_that_shrank() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let (mut wal, database) = open(&vfs, "/shrink.db", 8);
        commit(&mut wal, 2, 0x77, 2);
        wal.checkpoint(CheckpointMode::Passive, database.as_ref(), None)
            .unwrap();
        assert_eq!(database.file_size().unwrap(), 1024);
    }
}
