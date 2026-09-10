//! The rollback journal: what `PRAGMA journal_mode = DELETE` selects.
//!
//! Invariant: **a page's old image is on disk and synced before its new image
//! is written.** That single ordering is the whole of what makes a rollback
//! journal work, and everything else here is bookkeeping around it. A crash at
//! any moment then leaves the file recoverable: either the journal is
//! incomplete, in which case no page was overwritten and the file is already
//! whole, or it is complete, in which case every page it names can be put back.
//!
//! # Why the engine has two of these
//!
//! The write-ahead log records what a transaction *did*, so recovery replays
//! forwards and the data file may lag behind indefinitely. A rollback journal
//! records what the pages *were*, so recovery replays backwards and the data
//! file is always current at a commit. They are opposite answers to one
//! question, and SQLite exposes the choice under `PRAGMA journal_mode` because
//! the difference is visible to an application: after a clean commit a rollback
//! journal leaves nothing on disk beside the database, which is what an
//! application shipping a database as one file needs.
//!
//! `PRAGMA journal_mode` used to accept either name without changing what
//! recovery did; the choice was made real, so the two modes now differ in
//! practice as well as on paper. The write-ahead log stays the default
//! because it is faster - one sync per commit
//! against two - and the rollback journal is what an application selects when it
//! wants the file to stand alone.
//!
//! # The five modes
//!
//! | mode | after a commit | after a crash |
//! |---|---|---|
//! | `delete` | the journal file is removed | replayed, then removed |
//! | `truncate` | it is truncated to nothing | replayed, then truncated |
//! | `persist` | its header is zeroed | replayed, then zeroed |
//! | `memory` | nothing was ever written | nothing to replay: the pages are lost |
//! | `off` | nothing is written at all | nothing to replay |
//!
//! `memory` and `off` are both "no durability", and they differ in what they
//! protect: `memory` still rolls back a transaction the *application* abandons,
//! and `off` does not roll back at all.

use std::collections::BTreeSet;

use inillucent_base::{error::misuse, DbResult};
use inillucent_vfs::{AccessMode, DbPath, FileKind, OpenOptions, SyncMode, Vfs, VfsFile};

use crate::page::PageId;

/// The eight bytes at the head of a journal that say it is one.
///
/// A file that does not start with these is not this engine's journal, and a
/// journal whose magic has been zeroed is one that has been *finished* - which
/// is what `persist` leaves behind and what stops it being replayed twice.
const MAGIC: [u8; 8] = *b"RDBJRNL1";

/// How many bytes the header takes: the magic, the page size, the page count.
const HEADER: usize = 24;

/// How the pre-commit state is protected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JournalMode {
    /// The write-ahead log: what a transaction did, replayed forwards.
    #[default]
    Wal,
    /// A rollback journal removed at each commit.
    Delete,
    /// A rollback journal truncated at each commit.
    Truncate,
    /// A rollback journal whose header is zeroed at each commit.
    Persist,
    /// Pre-images held in memory only.
    Memory,
    /// No protection at all.
    Off,
}

impl JournalMode {
    /// Returns the mode a `PRAGMA journal_mode = X` word names.
    ///
    /// @param word - the argument, folded
    pub fn named(word: &str) -> Option<JournalMode> {
        match word.to_ascii_lowercase().as_str() {
            "wal" => Some(JournalMode::Wal),
            "delete" => Some(JournalMode::Delete),
            "truncate" => Some(JournalMode::Truncate),
            "persist" => Some(JournalMode::Persist),
            "memory" => Some(JournalMode::Memory),
            "off" | "none" => Some(JournalMode::Off),
            _ => None,
        }
    }

    /// Returns the word `PRAGMA journal_mode` reports.
    pub fn word(self) -> &'static str {
        match self {
            JournalMode::Wal => "wal",
            JournalMode::Delete => "delete",
            JournalMode::Truncate => "truncate",
            JournalMode::Persist => "persist",
            JournalMode::Memory => "memory",
            JournalMode::Off => "off",
        }
    }

    /// Reports whether the mode keeps page pre-images rather than a log.
    pub fn is_rollback(self) -> bool {
        !matches!(self, JournalMode::Wal | JournalMode::Off)
    }

    /// Reports whether the pre-images reach the disk.
    ///
    /// `memory` keeps them and never writes them, which is what makes it fast
    /// and what makes a crash under it unrecoverable.
    pub fn is_durable(self) -> bool {
        matches!(
            self,
            JournalMode::Delete | JournalMode::Truncate | JournalMode::Persist
        )
    }
}

/// The pre-images one transaction has saved.
pub struct Journal {
    /// Where files are opened and deleted.
    ///
    /// The journal holds it rather than being handed it per call, because the
    /// pool - which is what calls `save` - has a file handle and nothing else,
    /// and threading a VFS through the pool for one caller would widen its
    /// reach on purpose.
    vfs: std::sync::Arc<dyn Vfs>,
    /// Where the file is, or would be.
    path: DbPath,
    /// The open file, once anything has been written to it.
    file: Option<Box<dyn VfsFile>>,
    /// How the journal is disposed of at a commit.
    mode: JournalMode,
    /// How big a page is, which the header records so a replay can size itself.
    page_size: usize,
    /// The pages already saved, so a page changed twice is saved once.
    ///
    /// **Once, and it has to be the first time.** The journal restores the
    /// state at the *start* of the transaction; saving a page again halfway
    /// through would restore it to the middle of one.
    saved: BTreeSet<u64>,
    /// The pre-images, for `memory` mode.
    held: Vec<(PageId, Vec<u8>)>,
}

impl Journal {
    /// Returns an empty journal for one database.
    ///
    /// @param vfs - where files are opened and deleted
    /// @param database - the database file's path
    /// @param mode - how the pre-commit state is protected
    /// @param page_size - how big a page is
    pub fn new(
        vfs: std::sync::Arc<dyn Vfs>,
        database: &DbPath,
        mode: JournalMode,
        page_size: usize,
    ) -> Journal {
        Journal {
            vfs,
            // `DbPath` already knows the name a journal takes beside a
            // database, and it is the same name the VFS conformance suite
            // uses - so a journal is recognisable to everything that looks.
            path: database.journal(),
            file: None,
            mode,
            page_size,
            saved: BTreeSet::new(),
            held: Vec::new(),
        }
    }

    /// Returns where the journal file is, or would be.
    pub fn path(&self) -> &DbPath {
        &self.path
    }

    /// Saves one page's image as it was before the transaction touched it.
    ///
    /// Called from the one place a page is written, so a page cannot reach the
    /// file by a route that skipped its pre-image.
    ///
    /// @param page - which page
    /// @param before - its current contents on disk
    pub fn save(&mut self, page: PageId, before: &[u8]) -> DbResult<()> {
        if !self.mode.is_rollback() || !self.saved.insert(page.0) {
            return Ok(());
        }
        if !self.mode.is_durable() {
            self.held.push((page, before.to_vec()));
            return Ok(());
        }
        // The header is rewritten with the running count each time, so a
        // journal a crash caught mid-write has a count naming only the pages
        // that are actually there - which is what lets recovery stop at the
        // first record it cannot read.
        let count = self.saved.len();
        let record_size = self.page_size.saturating_add(8);
        let offset =
            HEADER.saturating_add(count.saturating_sub(1).saturating_mul(record_size)) as u64;
        let page_size = self.page_size;
        let mut record = page.0.to_le_bytes().to_vec();
        record.extend_from_slice(before);
        record.resize(record_size, 0);
        let file = self.opened()?;
        file.write_all_at(offset, &record)
            .map_err(|error| error.into_db_error())?;
        write_header(file, page_size, count)?;
        Ok(())
    }

    /// Makes the journal safe to rely on, before any page is overwritten.
    ///
    /// **The sync that makes the whole thing work.** Everything above writes;
    /// this is what puts it on the disk, and it must happen before the first
    /// new page image does. Called at the head of a flush rather than per page,
    /// so a transaction touching a thousand pages syncs the journal once.
    pub fn seal(&self) -> DbResult<()> {
        if !self.mode.is_durable() {
            return Ok(());
        }
        let Some(file) = &self.file else {
            return Ok(());
        };
        file.sync(SyncMode::Full)
            .map_err(|error| error.into_db_error())
    }

    /// Disposes of the journal once the commit is on the disk.
    ///
    /// The three durable modes differ only here, and only in how much work is
    /// saved for the next transaction: `delete` unlinks, `truncate` keeps the
    /// inode, `persist` keeps the file's blocks and zeroes the magic. All three
    /// leave a journal that recovery will not replay, which is the part that
    /// matters.
    ///
    pub fn finish(&mut self) -> DbResult<()> {
        self.saved.clear();
        self.held.clear();
        match self.mode {
            JournalMode::Delete => {
                self.file = None;
                let _ = self.vfs.delete(&self.path, false);
            }
            JournalMode::Truncate => {
                if let Some(file) = &self.file {
                    file.truncate(0).map_err(|error| error.into_db_error())?;
                    file.sync(SyncMode::Normal)
                        .map_err(|error| error.into_db_error())?;
                }
            }
            JournalMode::Persist => {
                if let Some(file) = &self.file {
                    file.write_all_at(0, &[0u8; HEADER])
                        .map_err(|error| error.into_db_error())?;
                    file.sync(SyncMode::Normal)
                        .map_err(|error| error.into_db_error())?;
                }
            }
            JournalMode::Wal | JournalMode::Memory | JournalMode::Off => {}
        }
        Ok(())
    }

    /// Returns the pre-images a `memory` journal is holding.
    ///
    /// The rollback path reads them; nothing else does.
    pub fn held(&self) -> &[(PageId, Vec<u8>)] {
        &self.held
    }

    /// Opens the journal file, creating it the first time anything is saved.
    ///
    /// @param vfs - where the file is opened
    fn opened(&mut self) -> DbResult<&dyn VfsFile> {
        if self.file.is_none() {
            let file = self
                .vfs
                .open(&self.path, OpenOptions::of_kind(FileKind::MainJournal))
                .map_err(|error| error.into_db_error())?;
            write_header(file.as_ref(), self.page_size, 0)?;
            self.file = Some(file);
        }
        self.file
            .as_deref()
            .ok_or_else(|| misuse("the journal was not opened"))
    }
}

/// Writes the journal's header: the magic, the page size and the page count.
///
/// @param file - the journal file
/// @param page_size - how big a page is
/// @param count - how many pre-images are in the file
fn write_header(file: &dyn VfsFile, page_size: usize, count: usize) -> DbResult<()> {
    let mut header = [0u8; HEADER];
    if let Some(slot) = header.get_mut(..8) {
        slot.copy_from_slice(&MAGIC);
    }
    if let Some(slot) = header.get_mut(8..16) {
        slot.copy_from_slice(&(page_size as u64).to_le_bytes());
    }
    if let Some(slot) = header.get_mut(16..24) {
        slot.copy_from_slice(&(count as u64).to_le_bytes());
    }
    file.write_all_at(0, &header)
        .map_err(|error| error.into_db_error())
}

/// Puts a hot journal's pages back, then removes it.
///
/// **Called before a database is opened, never after.** A journal that survived
/// a crash describes a file that is halfway through a transaction, and every
/// page it names has to go back before anything reads one of them. A journal
/// whose header is absent, short or zeroed describes nothing, which is what a
/// finished one looks like.
///
/// Returns whether anything was restored, which the caller reports.
///
/// @param vfs - where the files live
/// @param database - the database file's path
pub fn replay_hot_journal(vfs: &dyn Vfs, database: &DbPath) -> DbResult<bool> {
    let path = database.journal();
    if !vfs.access(&path, AccessMode::Exists).unwrap_or(false) {
        return Ok(false);
    }
    let Ok(journal) = vfs.open(&path, OpenOptions::of_kind(FileKind::MainJournal)) else {
        return Ok(false);
    };
    let mut header = [0u8; HEADER];
    if journal.read_exact_at(0, &mut header).is_err() {
        let _ = vfs.delete(&path, false);
        return Ok(false);
    }
    if header.get(..8) != Some(&MAGIC[..]) {
        // Not a journal, or one that was finished. Either way there is nothing
        // to put back, and deleting it is what `delete` mode would have done.
        let _ = vfs.delete(&path, false);
        return Ok(false);
    }
    let page_size = read_u64(&header, 8) as usize;
    let count = read_u64(&header, 16) as usize;
    if page_size == 0 || page_size > 1 << 20 {
        let _ = vfs.delete(&path, false);
        return Ok(false);
    }
    let Ok(target) = vfs.open(database, OpenOptions::main_db()) else {
        return Ok(false);
    };
    let mut record = vec![0u8; page_size.saturating_add(8)];
    for index in 0..count {
        let offset = HEADER.saturating_add(index.saturating_mul(record.len())) as u64;
        if journal.read_exact_at(offset, &mut record).is_err() {
            // The journal is shorter than its header claims, which means the
            // crash caught it mid-write - so the pages after this one were
            // never overwritten and there is nothing to put back for them.
            break;
        }
        let page = read_u64(&record, 0);
        let Some(image) = record.get(8..) else {
            break;
        };
        target
            .write_all_at(page.saturating_mul(page_size as u64), image)
            .map_err(|error| error.into_db_error())?;
    }
    target
        .sync(SyncMode::Full)
        .map_err(|error| error.into_db_error())?;
    drop(journal);
    let _ = vfs.delete(&path, false);
    Ok(count > 0)
}

/// Reads a little-endian `u64`, answering zero for a slice that is too short.
fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let Some(slice) = bytes.get(at..at.saturating_add(8)) else {
        return 0;
    };
    let mut raw = [0u8; 8];
    raw.copy_from_slice(slice);
    u64::from_le_bytes(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mode round-trips through its word, which is what the pragma reads.
    #[test]
    fn a_mode_round_trips_through_its_word() {
        for mode in [
            JournalMode::Wal,
            JournalMode::Delete,
            JournalMode::Truncate,
            JournalMode::Persist,
            JournalMode::Memory,
            JournalMode::Off,
        ] {
            assert_eq!(JournalMode::named(mode.word()), Some(mode));
        }
        assert_eq!(JournalMode::named("DELETE"), Some(JournalMode::Delete));
        assert_eq!(JournalMode::named("none"), Some(JournalMode::Off));
        assert_eq!(JournalMode::named("nonsense"), None);
    }

    /// A journal a crash left behind puts the pages back.
    ///
    /// **The test the whole file exists for.** The sequence is the one a crash
    /// produces: pre-images saved and synced, new page images written, and then
    /// nothing - no `finish`. Reopening must find the journal, restore every
    /// page it names, and remove it.
    #[test]
    fn a_hot_journal_restores_every_page_it_names() {
        use inillucent_vfs::{MemoryVfs, OpenOptions};
        let vfs: std::sync::Arc<dyn Vfs> = std::sync::Arc::new(MemoryVfs::new());
        let path = DbPath::new("/hot.db");
        let page_size = 64usize;
        // A database of three pages, each filled with its own page number.
        let file = vfs
            .open(&path, OpenOptions::main_db())
            .expect("the file opens");
        for page in 0..3u64 {
            let image = vec![page as u8 + 1; page_size];
            file.write_all_at(page * page_size as u64, &image)
                .expect("the page is written");
        }
        drop(file);

        // A transaction saves two of them and then overwrites all three.
        let mut journal = Journal::new(
            std::sync::Arc::clone(&vfs),
            &path,
            JournalMode::Delete,
            page_size,
        );
        for page in [PageId(0), PageId(2)] {
            let before = vec![page.0 as u8 + 1; page_size];
            journal.save(page, &before).expect("the pre-image is saved");
        }
        journal.seal().expect("the journal syncs");
        let file = vfs
            .open(&path, OpenOptions::main_db())
            .expect("the file opens");
        for page in 0..3u64 {
            file.write_all_at(page * page_size as u64, &vec![0xff; page_size])
                .expect("the page is overwritten");
        }
        drop(file);
        // And then the process dies: `finish` is never called.
        drop(journal);

        assert!(
            replay_hot_journal(vfs.as_ref(), &path).expect("the replay runs"),
            "a journal holding two pages is hot"
        );
        let file = vfs
            .open(&path, OpenOptions::main_db())
            .expect("the file opens");
        let mut image = vec![0u8; page_size];
        for (page, expected) in [(0u64, 1u8), (2, 3)] {
            file.read_exact_at(page * page_size as u64, &mut image)
                .expect("the page reads");
            assert!(
                image.iter().all(|byte| *byte == expected),
                "page {page} was not put back"
            );
        }
        // Page 1 was never saved, so it keeps what the transaction wrote - which
        // is correct: a page with no pre-image had none to restore.
        file.read_exact_at(page_size as u64, &mut image)
            .expect("the page reads");
        assert!(image.iter().all(|byte| *byte == 0xff));
        // And the journal is gone, so a second open does not replay it again.
        assert!(!replay_hot_journal(vfs.as_ref(), &path).expect("the replay runs"));
    }

    /// A journal that was finished is not replayed, whatever is still in it.
    #[test]
    fn a_finished_journal_is_not_hot() {
        use inillucent_vfs::{MemoryVfs, OpenOptions};
        let vfs: std::sync::Arc<dyn Vfs> = std::sync::Arc::new(MemoryVfs::new());
        let path = DbPath::new("/done.db");
        let file = vfs
            .open(&path, OpenOptions::main_db())
            .expect("the file opens");
        file.write_all_at(0, &[7u8; 64])
            .expect("the page is written");
        drop(file);
        let mut journal =
            Journal::new(std::sync::Arc::clone(&vfs), &path, JournalMode::Persist, 64);
        journal.save(PageId(0), &[1u8; 64]).expect("saved");
        journal.seal().expect("sealed");
        journal.finish().expect("finished");
        drop(journal);
        assert!(
            !replay_hot_journal(vfs.as_ref(), &path).expect("the replay runs"),
            "a zeroed header means the commit completed"
        );
        let file = vfs
            .open(&path, OpenOptions::main_db())
            .expect("the file opens");
        let mut image = [0u8; 64];
        file.read_exact_at(0, &mut image).expect("the page reads");
        assert!(image.iter().all(|byte| *byte == 7), "the commit stood");
    }

    /// The three durable modes keep pre-images on disk; the other two do not.
    #[test]
    fn only_the_three_file_modes_are_durable() {
        assert!(!JournalMode::Wal.is_rollback());
        assert!(!JournalMode::Off.is_rollback());
        assert!(JournalMode::Memory.is_rollback());
        assert!(!JournalMode::Memory.is_durable());
        for mode in [
            JournalMode::Delete,
            JournalMode::Truncate,
            JournalMode::Persist,
        ] {
            assert!(mode.is_rollback());
            assert!(mode.is_durable());
        }
    }
}
