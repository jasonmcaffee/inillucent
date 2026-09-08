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

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_storage::pager::{Pager, PagerOptions};
use inillucent_vfs::{AccessMode, DbPath, FileKind, FileLock, OpenOptions, Vfs};

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
    let log = log_page_size(vfs.as_ref(), path)?;
    let mut pager = match open_from_file(vfs.as_ref(), path, options) {
        Ok(pager) => pager,
        // A database file whose own header cannot be read is not lost while a
        // log stands beside it: an interrupted checkpoint leaves page one half
        // copied, and the log still holds every frame that checkpoint was
        // copying. The log is the authority in that case, so the pager is
        // opened against it rather than against the file.
        Err(reason) => match log {
            Some(page_size) => return open_from_log(vfs, path, options, page_size, reason),
            None => return Err(reason),
        },
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

/// Opens the pager against the database file, whose header it expects to hold.
fn open_from_file(vfs: &dyn Vfs, path: &DbPath, options: DatabaseOptions) -> DbResult<Pager> {
    let missing = !vfs.access(path, AccessMode::Exists)?
        || vfs
            .open(path, OpenOptions::of_kind(FileKind::MainDb).read_only())
            .and_then(|file| file.file_size())
            .map(|size| size == 0)
            .unwrap_or(false);
    if !options.writable {
        Pager::open_read_only(vfs, path, options.pager)
    } else if missing {
        // A database that does not exist yet is created empty, which is what
        // SQLite does: one page holding the header and an empty
        // `sqlite_schema`. Doing it here rather than in the pager keeps the
        // "recover before exposing a page" rule in one place - a file that
        // does not exist has nothing to recover, and one that does has already
        // been recovered above.
        Pager::create(
            vfs,
            path,
            options.pager,
            inillucent_storage::pager::NewDatabase::default(),
        )
    } else {
        Pager::open_read_write(vfs, path, options.pager)
    }
}

/// Opens a database whose header only the log can supply.
///
/// The log is attached first and then a read is taken and immediately
/// released, because taking a read is what runs recovery, rebuilds the index
/// and reads page one through the snapshot - and page one is the header. Doing
/// it here rather than leaving it to the first statement means an unreadable
/// database is still reported by `open`, which is where every caller expects
/// to find out.
///
/// `reason` is why the file's own header could not be used, and it is what
/// comes back when the log cannot supply one either. That is the honest error:
/// the database is unreadable, and the log's failure to rescue it says nothing
/// the caller needs beyond that.
fn open_from_log(
    vfs: Arc<dyn Vfs>,
    path: &DbPath,
    options: DatabaseOptions,
    page_size: inillucent_base::page::PageSize,
    reason: inillucent_base::DbError,
) -> DbResult<Pager> {
    let mut pager = Pager::open_with_header_from_log(
        vfs.as_ref(),
        path,
        options.pager,
        page_size,
        options.writable,
    )?;
    attach_wal(&mut pager, &vfs, path, options)?;
    let began = pager.begin_read();
    // The read is released whatever happened, so a database that cannot be
    // recovered does not also leak a read mark for the life of the process.
    let ended = pager.end_read();
    if began.is_err() {
        return Err(reason);
    }
    ended?;
    Ok(pager)
}

/// Returns the page size a log beside the database declares, if it has one.
///
/// A log too short for a header, or one whose header does not verify, is not a
/// log: SQLite starts again from an unreadable header rather than reporting
/// it, so there is nothing here that could rescue a database either.
fn log_page_size(
    vfs: &dyn Vfs,
    path: &DbPath,
) -> DbResult<Option<inillucent_base::page::PageSize>> {
    let log = path.wal();
    if !vfs.access(&log, AccessMode::Exists)? {
        return Ok(None);
    }
    let Ok(file) = vfs.open(&log, OpenOptions::of_kind(FileKind::Wal).read_only()) else {
        return Ok(None);
    };
    let mut raw = [0u8; crate::wal::format::WAL_HEADER_SIZE];
    if file.file_size()? < raw.len() as u64 || file.read_exact_at(0, &mut raw).is_err() {
        return Ok(None);
    }
    Ok(crate::wal::format::WalHeader::decode(&raw)
        .ok()
        .map(|header| header.page_size))
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
    spec: inillucent_storage::pager::NewDatabase,
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

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_storage::journal::Journal;
    use inillucent_vfs::memory::MemoryVfs;

    /// Returns an empty memory file system with a database file in it.
    fn database_at(path: &DbPath) -> Arc<MemoryVfs> {
        let vfs = Arc::new(MemoryVfs::new());
        let file = vfs
            .open(
                path,
                OpenOptions {
                    create: true,
                    ..OpenOptions::of_kind(FileKind::MainDb)
                },
            )
            .expect("the database file is created");
        drop(file);
        vfs
    }

    /// Every journal mode reports the name `PRAGMA journal_mode` answers with.
    ///
    /// Asserted against the literal strings the pragma is specified to return,
    /// not against `mode.as_str()`, so this pins the answer rather than
    /// restating the implementation. Nothing checked it before: a version
    /// returning a constant - or an empty string - passed the whole suite.
    #[test]
    fn every_journal_mode_reports_its_pragma_name() {
        assert_eq!(describe_journal_mode(JournalMode::Delete), "delete");
        assert_eq!(describe_journal_mode(JournalMode::Truncate), "truncate");
        assert_eq!(describe_journal_mode(JournalMode::Persist), "persist");
        assert_eq!(describe_journal_mode(JournalMode::Memory), "memory");
        assert_eq!(describe_journal_mode(JournalMode::Off), "off");
        assert_eq!(describe_journal_mode(JournalMode::Wal), "wal");
    }

    /// The sector size comes from the device, and is legal even without one.
    ///
    /// A database that cannot be opened still has to yield a sector size a
    /// journal header can carry, because the header is written before anything
    /// has established that the file is readable.
    #[test]
    fn the_sector_size_is_always_one_the_format_allows() {
        let path = DbPath::new("/db.sqlite");
        let vfs = database_at(&path);
        let present = sector_size_of(vfs.as_ref(), &path);
        assert!(
            (crate::journal::MIN_SECTOR_SIZE..=crate::journal::MAX_SECTOR_SIZE).contains(&present),
            "a sector size of {present} is outside the range the format allows"
        );
        assert!(present.is_power_of_two(), "{present} is not a power of two");

        // A file that is not there cannot be interrogated, and the answer is
        // still a legal sector size rather than zero.
        let missing = DbPath::new("/nothing.sqlite");
        let absent = sector_size_of(vfs.as_ref(), &missing);
        assert_eq!(absent, crate::journal::MIN_SECTOR_SIZE);
    }

    /// A log holding exactly a header and no frames still declares its page
    /// size.
    ///
    /// This is what a freshly created write-ahead log is, and the length check
    /// guarding the read has to admit it: a log of exactly `WAL_HEADER_SIZE`
    /// bytes is complete, not short. Requiring *more* than a header - or
    /// rejecting a file of exactly that length - would make every fresh log
    /// look like a file that is not a log at all, and the database would open
    /// without it.
    #[test]
    fn a_log_that_is_exactly_a_header_declares_its_page_size() {
        let path = DbPath::new("/db.sqlite");
        let vfs = database_at(&path);
        let size = inillucent_base::page::PageSize::new(4096).expect("4096 is a page size");
        let header = crate::wal::format::WalHeader::new(
            size,
            0,
            [1, 2, 3, 4, 5, 6, 7, 8],
            inillucent_base::checksum::WalByteOrder::Big,
        )
        .expect("the header is built");
        let raw = header.encode().expect("the header encodes");
        assert_eq!(
            raw.len(),
            crate::wal::format::WAL_HEADER_SIZE,
            "the fixture has to be exactly a header for this to test anything"
        );

        let log = vfs
            .open(
                &path.wal(),
                OpenOptions {
                    create: true,
                    ..OpenOptions::of_kind(FileKind::Wal)
                },
            )
            .expect("the log file is created");
        log.write_all_at(0, &raw).expect("the header is written");
        drop(log);

        let declared = log_page_size(vfs.as_ref(), &path).expect("the log is readable");
        assert_eq!(
            declared,
            Some(size),
            "a log of exactly one header is a log, and it declares its page size"
        );
    }

    /// Leaves a hot journal beside a database: begun, with records, unfinished.
    ///
    /// This is what a writer that lost power partway through a transaction
    /// leaves on disk, and it is what recovery exists to find.
    fn leave_a_hot_journal(vfs: &Arc<MemoryVfs>, path: &DbPath) {
        let mut journal = RollbackJournal::new(
            Arc::clone(vfs) as Arc<dyn Vfs>,
            path,
            JournalOptions {
                mode: JournalMode::Delete,
                synchronous: Synchronous::Full,
            },
        );
        journal.begin(1024, 4).expect("the journal begins");
        journal
            .record(2, &vec![7u8; 1024])
            .expect("a page image is recorded");
        // The commit header is what makes a journal hot: it declares the
        // records that are on disk. A writer that lost power after this point
        // and before the database was rewritten leaves exactly this.
        journal
            .prepare_commit()
            .expect("the commit header is written");
        drop(journal);
    }

    /// A database with a hot journal cannot be opened read-only.
    ///
    /// Recovery is a write - it replays page images back into the database -
    /// so a read-only caller cannot perform it, and it must not be handed a
    /// database still holding a half-finished transaction either. Refusing is
    /// the only correct answer, and the branch that does it was never taken.
    #[test]
    fn a_hot_journal_cannot_be_opened_read_only() {
        let path = DbPath::new("/db.sqlite");
        let vfs = database_at(&path);
        leave_a_hot_journal(&vfs, &path);
        assert!(
            journal_would_be_replayed(vfs.as_ref(), &path).expect("the question is answerable"),
            "the fixture has to leave a hot journal for this to test anything"
        );

        let options = DatabaseOptions {
            writable: false,
            ..DatabaseOptions::default()
        };
        let failure = open_database(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, options)
            .expect_err("a hot journal cannot be opened read-only");
        assert_eq!(failure.code(), inillucent_base::error::PrimaryCode::Misuse);
    }

    /// The default options open a writable database with SQLite's own journal
    /// defaults.
    ///
    /// Every caller that does not say otherwise gets these, so they are worth
    /// pinning: a default that quietly became read-only, or that changed
    /// durability level, would change what every unconfigured open means.
    #[test]
    fn the_default_options_are_writable_at_sqlites_defaults() {
        let options = DatabaseOptions::default();
        assert!(options.writable, "an unconfigured open is a writable one");
        assert_eq!(options.journal.mode, JournalMode::Delete);
        assert_eq!(options.journal.synchronous, Synchronous::Full);
    }

    /// A database with no journal beside it has nothing to replay.
    #[test]
    fn a_database_with_no_journal_would_replay_nothing() {
        let path = DbPath::new("/db.sqlite");
        let vfs = database_at(&path);
        assert!(
            !journal_would_be_replayed(vfs.as_ref(), &path).expect("the question is answerable"),
            "there is no journal, so there is nothing to replay"
        );
    }

    /// A journal whose header is not a journal header is not replayed.
    ///
    /// This is the branch that separates "a journal is present" from "a hot
    /// journal is present": a finalised or truncated journal is a file that
    /// exists and says nothing, and replaying it would undo a transaction that
    /// committed.
    #[test]
    fn a_journal_without_a_valid_header_would_replay_nothing() {
        let path = DbPath::new("/db.sqlite");
        let vfs = database_at(&path);
        let journal = vfs
            .open(
                &path.journal(),
                OpenOptions {
                    create: true,
                    ..OpenOptions::of_kind(FileKind::MainJournal)
                },
            )
            .expect("the journal file is created");
        journal
            .write_all_at(0, &[0u8; 64])
            .expect("the zeroed header is written");
        drop(journal);
        assert!(
            !journal_would_be_replayed(vfs.as_ref(), &path).expect("the question is answerable"),
            "a zeroed header is not a hot journal"
        );
    }

    /// An empty journal file is not a hot journal either.
    #[test]
    fn an_empty_journal_would_replay_nothing() {
        let path = DbPath::new("/db.sqlite");
        let vfs = database_at(&path);
        let journal = vfs
            .open(
                &path.journal(),
                OpenOptions {
                    create: true,
                    ..OpenOptions::of_kind(FileKind::MainJournal)
                },
            )
            .expect("the journal file is created");
        drop(journal);
        assert!(
            !journal_would_be_replayed(vfs.as_ref(), &path).expect("the question is answerable"),
            "a journal with no header at all is not hot"
        );
    }
}
