//! Hot-journal detection, replay, and the opener that runs them first.
//!
//! Invariant: no page of a database with a hot journal beside it is ever
//! exposed to anything above this module. Recovery happens inside `open`,
//! under an EXCLUSIVE lock, before the pager that will serve pages has read
//! its header - so there is no window in which a caller can see the mixture a
//! crashed writer left behind.
//!
//! The second invariant is idempotence. Recovery writes the same bytes to the
//! same offsets whatever happened last time, syncs the database before it
//! removes the journal, and removes the journal last. A crash at any point
//! during recovery therefore leaves either another hot journal - which the
//! next open replays to the same result - or a complete old database.

use std::sync::Arc;

use rustdb_base::error::misuse;
use rustdb_base::DbResult;
use rustdb_storage::pager::{Pager, PagerOptions};
use rustdb_vfs::{AccessMode, DbPath, FileKind, FileLock, OpenOptions, Vfs};

use crate::journal::{
    decode_journal, recover_hot_journal, JournalMode, JournalOptions, RollbackJournal, Synchronous,
};
use crate::wal::{Wal, WalOptions};

/// How a database file is opened by the transaction layer.
#[derive(Clone, Copy, Debug)]
pub struct DatabaseOptions {
    /// The pager's own options: cache size and database identity.
    pub pager: PagerOptions,
    /// The journal mode and durability level the connection starts with.
    pub journal: JournalOptions,
    /// Whether the database is opened for writing.
    pub writable: bool,
}

impl Default for DatabaseOptions {
    /// A writable database with SQLite's journal defaults.
    fn default() -> DatabaseOptions {
        DatabaseOptions {
            pager: PagerOptions::default(),
            journal: JournalOptions::default(),
            writable: true,
        }
    }
}

/// Opens a database, recovering a hot journal first, and attaches a journal.
///
/// The order is the point. A hot journal is replayed before the pager reads
/// the header, because the header is one of the pages the journal may be about
/// to put back; a pager that had already read the crashed writer's page one
/// would be describing a database that is about to stop existing.
pub fn open_database(
    vfs: Arc<dyn Vfs>,
    path: &DbPath,
    options: DatabaseOptions,
) -> DbResult<Pager> {
    if options.writable {
        recover_if_hot(vfs.as_ref(), path, options.journal.synchronous)?;
    } else if journal_would_be_replayed(vfs.as_ref(), path)? {
        return Err(misuse(
            "the database has a hot journal and cannot be opened read-only until it is recovered",
        ));
    }
    let missing = !vfs.access(path, AccessMode::Exists)?
        || vfs
            .open(path, OpenOptions::of_kind(FileKind::MainDb).read_only())
            .and_then(|file| file.file_size())
            .map(|size| size == 0)
            .unwrap_or(false);
    let mut pager = if !options.writable {
        Pager::open_read_only(vfs.as_ref(), path, options.pager)?
    } else if missing {
        // A database that does not exist yet is created empty, which is what
        // SQLite does: one page holding the header and an empty
        // `sqlite_schema`. Doing it here rather than in the pager keeps the
        // "recover before exposing a page" rule in one place - a file that
        // does not exist has nothing to recover, and one that does has already
        // been recovered above.
        Pager::create(
            vfs.as_ref(),
            path,
            options.pager,
            rustdb_storage::pager::NewDatabase::default(),
        )?
    } else {
        Pager::open_read_write(vfs.as_ref(), path, options.pager)?
    };
    // A file whose format versions say WAL is opened in WAL mode whatever the
    // connection asked for. The alternative - honouring the request - would
    // have this connection writing undo images into a database another
    // connection is appending frames for, which is not a mode, it is a race.
    let wal = options.journal.mode.is_wal() || pager.header().is_wal();
    if wal {
        attach_wal(&mut pager, &vfs, path, options)?;
    } else if options.writable {
        let mut journal = RollbackJournal::new(Arc::clone(&vfs), path, options.journal);
        journal.set_sector_size(sector_size_of(vfs.as_ref(), path));
        pager.attach_journal(Box::new(journal));
    }
    Ok(pager)
}

/// Opens the write-ahead log beside a database and attaches it to the pager.
///
/// The log takes its own handle on the database file, for the shared memory
/// rather than for the pages: the shared-memory file is keyed by the identity
/// of the database it belongs to, so a second handle finds the same one, and
/// the pager keeps its handle to itself.
pub fn attach_wal(
    pager: &mut Pager,
    vfs: &Arc<dyn Vfs>,
    path: &DbPath,
    options: DatabaseOptions,
) -> DbResult<()> {
    let file = vfs.open(path, OpenOptions::of_kind(FileKind::MainDb))?;
    let wal = Wal::open(
        Arc::clone(vfs),
        path,
        file.as_ref(),
        pager.page_size(),
        WalOptions {
            synchronous: options.journal.synchronous,
            auto_checkpoint: WalOptions::default().auto_checkpoint,
            writable: options.writable,
        },
    )?;
    pager.attach_wal(Box::new(wal));
    Ok(())
}

/// Creates a database file and opens it the same way.
pub fn create_database(
    vfs: Arc<dyn Vfs>,
    path: &DbPath,
    options: DatabaseOptions,
    spec: rustdb_storage::pager::NewDatabase,
) -> DbResult<Pager> {
    let mut pager = Pager::create(vfs.as_ref(), path, options.pager, spec)?;
    let mut journal = RollbackJournal::new(Arc::clone(&vfs), path, options.journal);
    journal.set_sector_size(sector_size_of(vfs.as_ref(), path));
    pager.attach_journal(Box::new(journal));
    Ok(pager)
}

/// Replays a hot journal, if there is one, and returns whether it did.
///
/// The lock ladder is SQLite's. SHARED first, because a journal beside a
/// database another connection is actively committing is *not* hot - that
/// connection holds RESERVED and is going to clean up after itself. Only when
/// nobody claims it does this take PENDING and EXCLUSIVE and replay.
pub fn recover_if_hot(vfs: &dyn Vfs, path: &DbPath, synchronous: Synchronous) -> DbResult<bool> {
    if !vfs.access(&path.journal(), AccessMode::Exists)? {
        return Ok(false);
    }
    if !vfs.access(path, AccessMode::Exists)? {
        // A journal with no database is nothing to replay onto. Leaving it
        // would make every later open pay for the same check.
        vfs.delete(&path.journal(), false)?;
        return Ok(false);
    }
    let database = vfs.open(path, OpenOptions::of_kind(FileKind::MainDb))?;
    database.lock(FileLock::Shared)?;
    if database.check_reserved_lock()? {
        database.unlock(FileLock::None)?;
        return Ok(false);
    }
    database.lock(FileLock::Reserved)?;
    database.lock(FileLock::Pending)?;
    database.lock(FileLock::Exclusive)?;
    let outcome = recover_hot_journal(vfs, path, database.as_ref(), synchronous);
    // The lock is released whatever happened: holding it after a failed
    // recovery would turn one unreadable database into a wedged process.
    let unlocked = database.unlock(FileLock::None);
    let recovered = outcome?;
    unlocked?;
    Ok(recovered.is_some())
}

/// Reports whether a journal beside the database would be replayed.
pub fn journal_would_be_replayed(vfs: &dyn Vfs, path: &DbPath) -> DbResult<bool> {
    let journal_path = path.journal();
    if !vfs.access(&journal_path, AccessMode::Exists)? {
        return Ok(false);
    }
    let file = vfs.open(
        &journal_path,
        OpenOptions::of_kind(FileKind::MainJournal).read_only(),
    )?;
    let size = file
        .file_size()?
        .min(crate::journal::JOURNAL_HEADER_SIZE as u64);
    let mut raw = vec![0u8; size as usize];
    if !raw.is_empty() {
        file.read_exact_at(0, &mut raw)?;
    }
    Ok(decode_journal(&raw)?.is_some())
}

/// Returns the sector size a journal for this database should align to.
fn sector_size_of(vfs: &dyn Vfs, path: &DbPath) -> u32 {
    let Ok(file) = vfs.open(path, OpenOptions::of_kind(FileKind::MainDb).read_only()) else {
        return crate::journal::MIN_SECTOR_SIZE;
    };
    file.device_characteristics().sector_size
}

/// Returns the journal mode a pager was opened with, for `PRAGMA journal_mode`.
///
/// It is reported from the connection rather than from the file because a
/// journal that has been finalised leaves nothing on disk to read it from, and
/// answering "delete" by observing that there is no journal would answer
/// "delete" for a database in WAL mode too.
pub fn describe_journal_mode(mode: JournalMode) -> &'static str {
    mode.as_str()
}
