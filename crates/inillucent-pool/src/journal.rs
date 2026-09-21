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
//! **And since task-2000 the fold no longer runs on every commit**, in this mode
//! or any other, which makes the paragraph below true of the whole of a
//! connection's life rather than of the gap between an eviction and the next
//! statement. A commit is an append to the log and a sync of it; the fold happens
//! when the log passes `RECLAIM_BYTES`, when somebody asks for a checkpoint, or
//! when the connection is dropped, and this journal protects it whenever it runs.
//! That is the same protection as before and it covers the same moment - what
//! changed is how often that moment arrives.
//!
//! One thing did go with the per-commit fold, and it is named here because a test
//! used to assert it. An eager fold left every acknowledged commit in the log
//! *and* in the data file, so a device that acknowledges a write and stores half
//! of it could lose one copy and not both. That was redundancy - the second write
//! and the second sync - rather than anything this journal did, and
//! `durability.rs`'s short write campaign is where the argument and the
//! measurement now live.
//!
//! **In `delete` mode that is true after a checkpoint, not after a commit**,
//! and the difference is visible to the same application. A journal is created
//! by the first page this connection writes back, which is a checkpoint or an
//! eviction, and it is removed by [`Journal::finish`], which only a checkpoint
//! reaches. So a transaction whose dirty pages outgrow the buffer pool evicts,
//! which creates the journal, and the file then sits beside the database until
//! the next checkpoint - under `PRAGMA locking_mode = exclusive`, which is the
//! default, a connection that never checkpoints again never removes it. The
//! file is harmless: it holds pre-images of pages the commit has since made
//! current, and `replay_hot_journal` puts them back over a database its meta
//! record still describes, which is the state that commit left. It is named
//! here because "one file" is a claim this makes about itself.
//!
//! `PRAGMA journal_mode` used to accept either name without changing what
//! recovery did; the choice was made real, so the modes now differ in practice
//! as well as on paper.
//!
//! **`delete` is the default, not `wal`**, which is what SQLite does and what
//! review 5 measured as costing nothing: 3.78x weighted with `wal` as the
//! default against 3.70x with `delete`, lower bounds 3.45x and 3.44x. A
//! sentence here used to say the write-ahead log was the default because it is
//! faster, and it was describing an earlier decision. The write-ahead log is
//! still there and `PRAGMA journal_mode = wal` still selects it.
//!
//! # What this protects, which is not what SQLite's rollback journal protects
//!
//! **An application's `ROLLBACK` never comes through here.** In SQLite the
//! rollback journal is how an abandoned transaction is undone; in this engine
//! that is `ImportedDatabase::undo_to`, reading the log, and it works the same
//! under every value of `PRAGMA journal_mode` including `off`. What this file
//! protects is the **checkpoint**: the moment pages move out of the log and
//! into the data file, in place, which is the one moment the log stops being
//! able to rebuild them.
//!
//! That is worth being exact about, because the obvious reading of the mode
//! names is the SQLite one and it is wrong here.
//!
//! # The five modes
//!
//! | mode | after a checkpoint | after a crash mid-checkpoint |
//! |---|---|---|
//! | `delete` | the journal file is removed | replayed, then removed |
//! | `truncate` | it is truncated to nothing | replayed, then truncated |
//! | `persist` | its header is zeroed | replayed, then zeroed |
//! | `memory` | the pre-images are dropped | nothing to replay: a half-written checkpoint stands |
//! | `off` | nothing was written at all | nothing to replay: the same |
//!
//! `wal` takes a `delete` journal rather than none, because a write-ahead log
//! does not make a checkpoint undoable when the log is logical - see
//! `journal_for` in `crates/inillucent-engine/src/lib.rs`.
//!
//! **`memory` and `off` are the same thing in this engine**, and the table says
//! so rather than repeating SQLite's distinction. `memory` collects pre-images
//! into `held`, nothing reads them, and the only caller that could - a
//! checkpoint undoing itself after an I/O error without a crash - does not
//! exist. An application that wants a checkpoint it can lose should choose
//! either; one that wants a checkpoint it cannot should not choose either.

use std::collections::BTreeSet;

use inillucent_base::{error::misuse, DbResult};
use inillucent_vfs::{AccessMode, DbPath, FileKind, FileLock, OpenOptions, SyncMode, Vfs, VfsFile};

use crate::page::PageId;

/// The eight bytes at the head of a journal that say it is one.
///
/// A file that does not start with these is not this engine's journal, and a
/// journal whose magic has been zeroed is one that has been *finished* - which
/// is what `persist` leaves behind and what stops it being replayed twice.
///
/// It reads `2` because the record layout gained a checksum; see
/// [`record_checksum`]. A journal an older build left behind is not recognised
/// and is removed rather than replayed, which is the same thing this function
/// already did with any other file sitting under the journal's name.
const MAGIC: [u8; 8] = *b"RDBJRNL2";

/// How many bytes the header takes.
///
/// The magic, the page size, the page count, the nonce, and a checksum over
/// the five of them.
const HEADER: usize = 40;

/// How many bytes a record spends on its own bookkeeping, before the image.
///
/// The page id, the record's checksum, and four bytes that keep the image
/// aligned to sixteen.
const RECORD_PREFIX: usize = 16;

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
    /// What this transaction's records are checksummed against.
    ///
    /// **It has to differ from every nonce any other transaction has ever used
    /// on this file, including one a different process used**, and that is the
    /// only thing asked of it. `persist` mode leaves the previous
    /// transaction's records in the file and writes the new ones over the
    /// front of them, so a record the crash dropped can leave an older record
    /// sitting at the same offset, complete and readable. Checksumming against
    /// a nonce that older record cannot have carried is what stops recovery
    /// putting a *previous* transaction's pages back over the database.
    ///
    /// **It comes from the VFS rather than from a sequence**, which is a
    /// correction: it used to be a multiplicative step from a seed derived
    /// from the database's path, which repeats. Every reopen started the
    /// sequence again, so the first transaction of one process used the same
    /// nonce as the first transaction of the process before it - and under
    /// `persist`, whose body survives a reopen, that is exactly the pair that
    /// has to differ. This is the same source `Database::create` takes the
    /// file's uuid from, so a simulator still reproduces a run from its seed.
    nonce: u64,
    /// Whether a pre-image has been written since the last sync.
    ///
    /// **This is what makes `seal` safe to call per page without costing a
    /// sync per page.** The ordering the journal exists to enforce is that no
    /// page reaches the database before its own pre-image reaches the disk, so
    /// every writeback has to seal - but a checkpoint saves every pre-image it
    /// needs before it writes the first page, so all but the first of those
    /// calls find nothing outstanding and return without touching the file. A
    /// checkpoint of a thousand pages still syncs the journal once.
    unsealed: std::cell::Cell<bool>,
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
            // Zero until the first transaction asks for one. A journal that
            // never writes a record never needs a nonce, and `new` is on the
            // path of every connection whether it writes or not.
            nonce: 0,
            unsealed: std::cell::Cell::new(false),
        }
    }

    /// Takes a nonce for the transaction about to write pre-images.
    ///
    /// From the VFS, because it has to differ from one a *different process*
    /// used on the same file and nothing derived from the file itself can
    /// promise that. A VFS that cannot produce randomness is not a reason to
    /// refuse a write, so the fallback mixes the path with the count of
    /// journals this process has opened - which is worse and is still never
    /// the same twice within a run.
    fn take_nonce(&mut self) {
        let mut bytes = [0u8; 8];
        if self.vfs.randomness(&mut bytes).is_ok() {
            self.nonce = u64::from_le_bytes(bytes);
            return;
        }
        static OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nth = OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            inillucent_base::checksum::crc32(self.path.as_path().to_string_lossy().as_bytes());
        self.nonce = u64::from(path) ^ nth.rotate_left(32);
    }

    /// Returns where the journal file is, or would be.
    pub fn path(&self) -> &DbPath {
        &self.path
    }

    /// Returns the mode the journal was opened in.
    ///
    /// The pool asks so that it can tell a journal whose pre-images reach the
    /// disk from one whose do not: an eviction may write a page an open
    /// transaction has changed only when a crash could put that page back, and
    /// `memory` cannot. See `Pool::can_undo_a_steal`.
    pub fn mode(&self) -> JournalMode {
        self.mode
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
        // **A nonce per transaction, taken at its first pre-image.** Every
        // record this transaction writes is checksummed against it, and a
        // record an earlier transaction left in the file carries an earlier
        // one - which is what makes the earlier record fail its check rather
        // than be put back over the database. `finish` clears `saved`, so the
        // next transaction's first save comes through here and takes another.
        if self.saved.len() == 1 {
            self.take_nonce();
        }
        // The header is rewritten with the running count each time, so a
        // journal a crash caught mid-write has a count naming only the pages
        // that are actually there - and each record carries a checksum, so
        // recovery stops at the first one whose bytes are not the bytes that
        // were written rather than at the first one it cannot read at all.
        let count = self.saved.len();
        let record_size = self.page_size.saturating_add(RECORD_PREFIX);
        let offset =
            HEADER.saturating_add(count.saturating_sub(1).saturating_mul(record_size)) as u64;
        let page_size = self.page_size;
        let nonce = self.nonce;
        let mut record = vec![0u8; record_size];
        if let Some(slot) = record.get_mut(..8) {
            slot.copy_from_slice(&page.0.to_le_bytes());
        }
        if let Some(slot) = record.get_mut(RECORD_PREFIX..) {
            let width = slot.len().min(before.len());
            if let (Some(target), Some(source)) = (slot.get_mut(..width), before.get(..width)) {
                target.copy_from_slice(source);
            }
        }
        let checksum = record_checksum(nonce, page.0, record.get(RECORD_PREFIX..).unwrap_or(&[]));
        if let Some(slot) = record.get_mut(8..12) {
            slot.copy_from_slice(&checksum.to_le_bytes());
        }
        let file = self.opened()?;
        file.write_all_at(offset, &record)
            .map_err(|error| error.into_db_error())?;
        write_header(file, page_size, count, nonce)?;
        self.unsealed.set(true);
        Ok(())
    }

    /// Makes the journal safe to rely on, before any page is overwritten.
    ///
    /// **The sync that makes the whole thing work.** Everything above writes;
    /// this is what puts it on the disk, and it must happen before the first
    /// new page image does.
    ///
    /// It used to be called once at the head of a checkpoint, which synced an
    /// empty journal: nothing has been saved at that point, because the pages
    /// are journaled by the writeback loop that runs next. The pre-images a
    /// checkpoint saved therefore sat in the file's buffers while that same
    /// loop overwrote the pages they belonged to, and a power loss in the
    /// middle left a torn database page whose only copy of the old bytes was
    /// in a cache that the power loss emptied. `crates/inillucent-compat/tests/durability.rs`
    /// caught it in TRUNCATE and PERSIST mode as `page 3 checksum ... is not
    /// the computed ...` after a failure the engine had itself reported.
    ///
    /// So it is called per page now, and `unsealed` is what stops that costing
    /// a sync per page: the checkpoint saves every pre-image it needs before it
    /// writes anything, so only the first call finds work to do.
    pub fn seal(&self) -> DbResult<()> {
        if !self.mode.is_durable() || !self.unsealed.get() {
            return Ok(());
        }
        let Some(file) = &self.file else {
            self.unsealed.set(false);
            return Ok(());
        };
        file.sync(SyncMode::Full)
            .map_err(|error| error.into_db_error())?;
        self.unsealed.set(false);
        Ok(())
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
        // **Nothing is forgotten until the disposal below has succeeded.**
        // These three lines used to run first, so a disposal that failed left
        // a journal the next open would find hot, a `saved` set that no longer
        // named the pages it holds, and an `unsealed` flag saying its bytes
        // were on the disk. The clear is at the end now, after the one `?`
        // that can leave the file in place.
        match self.mode {
            JournalMode::Delete => {
                // **The pre-images this checkpoint just saved have to be
                // durable before the file that holds them goes away, and the
                // removal itself has to be durable too.** Neither used to
                // happen: `seal` only syncs whatever the journal held when
                // *this* checkpoint began, so a page saved by *this*
                // checkpoint's own flush sat in the device's write-behind
                // cache, unsynced, right up to this unlink - and the unlink
                // itself was `sync_dir: false`, an unsynced directory entry
                // removal. A power loss right there can leave the directory
                // still naming the file (the delete never reached disk) with
                // its last-written bytes only partially landed - a journal
                // that looks perfectly hot, whose pre-images are torn or
                // garbled, on a database whose commit already completed
                // cleanly. The next open's `replay_hot_journal` then puts that
                // garbled pre-image back over a good page, which is a crash
                // *after* a clean checkpoint corrupting a database, not a
                // recovery. Syncing this file's own bytes first, and syncing
                // the directory on the way out, is what `DELETE` mode's name
                // promises: gone, not "gone unless the power goes now".
                if let Some(file) = &self.file {
                    file.sync(SyncMode::Full)
                        .map_err(|error| error.into_db_error())?;
                }
                // The handle goes before the unlink because a file cannot be
                // removed on Windows while it is open. If the unlink below then
                // fails, this journal is left with no handle and its `saved`
                // set intact, and the next `save` reopens the same path through
                // `opened` - which is correct rather than lucky, but only
                // because `OpenOptions::of_kind(FileKind::MainJournal)` does
                // not truncate. It reopens onto the records that are already
                // there, and `saved` still names them, so the pre-images this
                // transaction is relying on are still the ones in the file.
                // A truncating open here would silently discard them.
                self.file = None;
                // **And the removal is reported.** It used to be discarded,
                // so a checkpoint whose journal would not delete returned
                // success with the journal still sitting beside the database -
                // and the next open replayed it over a commit that had already
                // been acknowledged. A journal that will not go is a failed
                // checkpoint, because the commit it is about to make durable
                // is exactly what it would undo.
                self.vfs
                    .delete(&self.path, true)
                    .map_err(|error| error.into_db_error())?;
            }
            // `Full` rather than `Normal` in both arms below, for the same
            // reason the `Delete` arm syncs before it unlinks: what makes the
            // journal stop being hot is a *change to this file*, and until that
            // change is on the media the next open still finds a journal full
            // of pre-images naming a database whose commit already completed.
            // Replaying it puts the previous transaction back over a good
            // database. `Normal` is the level that is allowed not to reach the
            // media here, so it was the wrong one to finish a commit with.
            JournalMode::Truncate => {
                if let Some(file) = &self.file {
                    file.truncate(0).map_err(|error| error.into_db_error())?;
                    file.sync(SyncMode::Full)
                        .map_err(|error| error.into_db_error())?;
                }
            }
            JournalMode::Persist => {
                if let Some(file) = &self.file {
                    file.write_all_at(0, &[0u8; HEADER])
                        .map_err(|error| error.into_db_error())?;
                    file.sync(SyncMode::Full)
                        .map_err(|error| error.into_db_error())?;
                }
            }
            JournalMode::Wal | JournalMode::Memory | JournalMode::Off => {}
        }
        self.saved.clear();
        self.held.clear();
        self.unsealed.set(false);
        Ok(())
    }

    /// Reports whether this page's pre-image is still wanted.
    ///
    /// The caller reads the page off the disk to hand to `save`, and that read
    /// is wasted for a page already saved - which is every page, on the second
    /// of `flush`'s two passes.
    ///
    /// @param page - the page about to be overwritten
    pub fn wants(&self, page: PageId) -> bool {
        self.mode.is_rollback() && !self.saved.contains(&page.0)
    }

    /// Reports whether this journal could put this page back.
    ///
    /// **The question a bulk build has to ask before it writes a page straight
    /// into the data file** (task-2055). Such a page's contents are in the file
    /// and in no log record, so a replay of this journal restores the page's
    /// previous life and nothing anywhere can rebuild what was written over it.
    /// See `inillucent_tree`'s `write_built_page`, which logs the page instead
    /// when the answer is yes.
    ///
    /// The exact complement of [`Journal::wants`] within a rollback mode: a page
    /// this journal wants a pre-image of is one it has not got, and a page it
    /// does not want is one it already holds.
    ///
    /// @param page - the page about to be written
    pub fn holds(&self, page: PageId) -> bool {
        self.mode.is_rollback() && self.saved.contains(&page.0)
    }

    /// Reports whether this journal would be hot to the next open.
    ///
    /// **A durable mode with pre-images in it, and nothing else.** `memory`
    /// keeps its pre-images in this process and writes none, so it is never on
    /// the disk for an open to find; `wal` and `off` are not rollback journals
    /// at all.
    ///
    /// Asked by the fold a connection does on its way out - see
    /// `ImportedDatabase::fold_on_close` - because a journal is disposed of by
    /// [`Journal::finish`] and only a checkpoint reaches it (task-2055).
    pub fn is_hot(&self) -> bool {
        self.mode.is_durable() && !self.saved.is_empty()
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
            write_header(file.as_ref(), self.page_size, 0, self.nonce)?;
            self.file = Some(file);
        }
        self.file
            .as_deref()
            .ok_or_else(|| misuse("the journal was not opened"))
    }
}

/// Returns the checksum one record carries.
///
/// Over the nonce, the page id and the image, so a record cannot pass its check
/// under a different transaction's nonce, cannot pass it while naming a
/// different page, and cannot pass it with a torn image.
///
/// @param nonce - what this transaction's records are checksummed against
/// @param page - which page the record restores
/// @param image - the page's contents as they were before the transaction
fn record_checksum(nonce: u64, page: u64, image: &[u8]) -> u32 {
    let mut seed = Vec::with_capacity(16);
    seed.extend_from_slice(&nonce.to_le_bytes());
    seed.extend_from_slice(&page.to_le_bytes());
    let over = inillucent_base::checksum::crc32(&seed);
    inillucent_base::checksum::crc32_continue(over, image)
}

/// Writes the journal's header: the magic, the page size, the page count, the
/// nonce, and a checksum over the four.
///
/// **The header is checksummed for the same reason the records are.** It is
/// rewritten on every `save`, so a crash can catch it mid-write and leave a
/// page size or a count that was never written by anything. Recovery reads
/// those two numbers to decide how many bytes to put back where, so believing
/// a torn one is how a recovery writes over pages that the transaction never
/// touched.
///
/// @param file - the journal file
/// @param page_size - how big a page is
/// @param count - how many pre-images are in the file
/// @param nonce - what this transaction's records are checksummed against
fn write_header(file: &dyn VfsFile, page_size: usize, count: usize, nonce: u64) -> DbResult<()> {
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
    if let Some(slot) = header.get_mut(24..32) {
        slot.copy_from_slice(&nonce.to_le_bytes());
    }
    let checksum = inillucent_base::checksum::crc32(header.get(..32).unwrap_or(&[]));
    if let Some(slot) = header.get_mut(32..36) {
        slot.copy_from_slice(&checksum.to_le_bytes());
    }
    file.write_all_at(0, &header)
        .map_err(|error| error.into_db_error())
}

/// Takes the whole lock chain on a database, reporting whether it got it.
///
/// **The one thing that makes a journal beside a file interpretable.** A journal
/// is written, and disposed of, by a process holding EXCLUSIVE on the database
/// it belongs to, so a journal that is still there while this process holds
/// EXCLUSIVE belongs to nobody: the process that wrote it is gone. Without the
/// lock, a journal on the disk means only that *somebody* has one open, and
/// putting its pre-images back then undoes a checkpoint that is still running or
/// has already finished (task-1987).
///
/// SHARED and RESERVED are asked for with no wait, because a concurrent holder
/// refuses them at once and there is nothing here worth waiting for - whoever
/// holds the file will dispose of its own journal. EXCLUSIVE is the one step
/// allowed to wait: RESERVED already stops new readers arriving, so all it waits
/// for is the readers already inside to leave, and it waits the same bounded,
/// backing-off way [`crate::pool::Pool::lock_within`] does rather than a second,
/// differently-tuned retry.
///
/// Every failing path leaves the handle holding nothing.
///
/// @param target - the database file
fn hold_the_database(target: &dyn VfsFile) -> bool {
    if target.lock(FileLock::Shared).is_err() {
        let _ = target.unlock(FileLock::None);
        return false;
    }
    if target.lock(FileLock::Reserved).is_err() {
        let _ = target.unlock(FileLock::None);
        return false;
    }
    if crate::pool::lock_with_wait(
        target,
        FileLock::Exclusive,
        crate::pool::DEFAULT_BUSY_MILLIS,
    )
    .is_err()
    {
        let _ = target.unlock(FileLock::None);
        return false;
    }
    true
}

/// Puts a hot journal's pages back, then removes it.
///
/// **Called before a database is opened, never after.** A journal that survived
/// a crash describes a file that is halfway through a transaction, and every
/// page it names has to go back before anything reads one of them. A journal
/// whose header is absent, short, zeroed or unchecksummed describes nothing,
/// which is what a finished one looks like.
///
/// **Every record is verified before it is written, and the first one that
/// fails ends the replay.** It used to write whatever it read. A journal's
/// records reach the media unsynced right up to the seal that precedes the
/// first page write, so a power loss leaves the ones written since that seal
/// torn, dropped or garbled - and a replay that trusts them puts garbage over
/// a database that the same power loss left perfectly intact. That is a
/// recovery corrupting a good file, and `crates/inillucent-compat/tests/durability.rs`
/// caught it in `truncate` and `persist` mode as `page 3 checksum ... is not
/// the computed ...` on a database whose page 3 was, on the media, exactly
/// right.
///
/// Stopping at the first bad record is not a partial repair. A record is only
/// unverifiable if it was written after the last seal, and a page is only
/// overwritten after the seal that covers its own pre-image - so a record that
/// fails its check names a page the crash never reached, as does every record
/// appended after it. Records are appended in order and a whole batch is
/// sealed together, so a record that fails is followed only by records from
/// the same unsealed batch.
///
/// The one thing that argument does *not* cover is the two meta pages, which
/// `Pool::checkpoint` saves after `flush` has already written the batch: a
/// record failing there leaves this checkpoint's pages on the disk with no
/// pre-image put back. That is still correct, because the meta record is the
/// last thing a checkpoint writes, so the file's `checkpoint_lsn` is still the
/// previous one and redo re-applies the batch from the log. The journal is
/// only strictly needed to undo the meta record itself.
///
/// **The database is locked before this journal is opened, let alone read**
/// (task-1987). Every mode now carries a journal, so this can no longer assume
/// it runs before anyone else touches the file - another process can be inside
/// its own checkpoint, with this very journal open and about to be disposed of.
/// Taking the lock first is what makes the journal's own presence mean
/// something: with EXCLUSIVE held nobody else can create one, write one or
/// delete one, so a journal that is still on the disk at that moment really is
/// a crashed process's and really is this one's to put back.
///
/// Reading it first is what task-1980 left behind, and it lost acknowledged
/// commits. The lock escalation was already here and already had the right
/// argument written beside it - a concurrent holder makes this a no-op - but it
/// ran *after* the header and the records had been read, and nothing asked
/// again once the lock was in hand. Measured on two writer processes each
/// running single statement inserts through the command line: one process
/// opened the database, found the journal a live checkpoint was in the middle
/// of, was refused SHARED, read the journal anyway, waited for the lock, and
/// then wrote that journal's pre-images over the checkpoint that had already
/// finished and deleted it - so the meta record went from generation 28 back to
/// 27 and the row the caller had been told about was gone. The Windows and
/// POSIX file handles both survive an unlink, so deleting the journal does not
/// take it away from a reader that already has it open.
///
/// Returns whether anything was restored, which the caller reports.
///
/// @param vfs - where the files live
/// @param database - the database file's path
pub fn replay_hot_journal(vfs: &dyn Vfs, database: &DbPath) -> DbResult<bool> {
    let path = database.journal();
    // **A cheap look before any lock is taken**, because this runs on every
    // open and the overwhelmingly common answer is "there is no journal". It
    // decides nothing: the answer is asked again below with the lock held, and
    // that is the one that counts.
    if !vfs.access(&path, AccessMode::Exists).unwrap_or(false) {
        return Ok(false);
    }
    let Ok(target) = vfs.open(database, OpenOptions::main_db()) else {
        return Ok(false);
    };
    if !hold_the_database(target.as_ref()) {
        return Ok(false);
    }
    // **Asked again, now that the lock is held.** Between the first look and
    // this one, the process that owned that journal can have finished its
    // checkpoint and disposed of it, so the file having been there a moment ago
    // says nothing about whether there is anything to put back.
    if !vfs.access(&path, AccessMode::Exists).unwrap_or(false) {
        let _ = target.unlock(FileLock::None);
        return Ok(false);
    }
    let Ok(journal) = vfs.open(&path, OpenOptions::of_kind(FileKind::MainJournal)) else {
        let _ = target.unlock(FileLock::None);
        return Ok(false);
    };
    let mut header = [0u8; HEADER];
    if journal.read_exact_at(0, &mut header).is_err() {
        let _ = target.unlock(FileLock::None);
        drop(journal);
        let _ = vfs.delete(&path, false);
        return Ok(false);
    }
    if header.get(..8) != Some(&MAGIC[..]) {
        // Not a journal, one an older build wrote, or one that was finished.
        // In every case there is nothing here that can be put back safely, and
        // deleting it is what `delete` mode would have done.
        let _ = target.unlock(FileLock::None);
        drop(journal);
        let _ = vfs.delete(&path, false);
        return Ok(false);
    }
    let stored = read_u32(&header, 32);
    if stored != inillucent_base::checksum::crc32(header.get(..32).unwrap_or(&[])) {
        // The crash caught the header itself, so nothing here can be trusted
        // and the whole journal is discarded rather than read.
        //
        // **Discarding it does not leave a half-applied checkpoint behind, and
        // the reason is not the one it looks like.** The tempting argument is
        // that a header in flight means no page had been overwritten yet, and
        // that is false: the header is rewritten by every `save`, and
        // `Pool::checkpoint` saves the two meta pages *after* `flush` has
        // already written the batch. A crash there catches a header rewrite
        // with this checkpoint's pages on the disk.
        //
        // What makes that safe is that the meta record is the last thing a
        // checkpoint writes. Until it lands, the file's `checkpoint_lsn` is
        // still the previous one, so redo re-applies every record the
        // half-written batch came from and the database ends up where the
        // checkpoint was taking it. The journal is only strictly needed to
        // undo the *meta* record, and a torn header means the meta record was
        // never reached.
        let _ = target.unlock(FileLock::None);
        drop(journal);
        let _ = vfs.delete(&path, false);
        return Ok(false);
    }
    let page_size = read_u64(&header, 8) as usize;
    let count = read_u64(&header, 16) as usize;
    let nonce = read_u64(&header, 24);
    if page_size == 0 || page_size > 1 << 20 {
        let _ = target.unlock(FileLock::None);
        drop(journal);
        let _ = vfs.delete(&path, false);
        return Ok(false);
    }
    let mut record = vec![0u8; page_size.saturating_add(RECORD_PREFIX)];
    let mut restored = 0usize;
    for index in 0..count {
        let offset = HEADER.saturating_add(index.saturating_mul(record.len())) as u64;
        if journal.read_exact_at(offset, &mut record).is_err() {
            // The journal is shorter than its header claims, which means the
            // crash caught it mid-write - so the pages after this one were
            // never overwritten and there is nothing to put back for them.
            break;
        }
        let page = read_u64(&record, 0);
        let Some(image) = record.get(RECORD_PREFIX..) else {
            break;
        };
        if read_u32(&record, 8) != record_checksum(nonce, page, image) {
            break;
        }
        target
            .write_all_at(page.saturating_mul(page_size as u64), image)
            .map_err(|error| error.into_db_error())?;
        restored = restored.saturating_add(1);
    }
    target
        .sync(SyncMode::Full)
        .map_err(|error| error.into_db_error())?;
    // Released before the journal goes, not after: a process waiting on this
    // same EXCLUSIVE request is waiting to do its own work, not to see this
    // journal deleted, and holding the lock one statement longer than
    // necessary is exactly the contention `lock_with_wait`'s backoff exists to
    // shorten.
    let _ = target.unlock(FileLock::None);
    drop(journal);
    let _ = vfs.delete(&path, false);
    Ok(restored > 0)
}

/// Reads a little-endian `u32`, answering zero for a slice that is too short.
///
/// @param bytes - the buffer
/// @param at - where the number starts
fn read_u32(bytes: &[u8], at: usize) -> u32 {
    let Some(slice) = bytes.get(at..at.saturating_add(4)) else {
        return 0;
    };
    let mut raw = [0u8; 4];
    raw.copy_from_slice(slice);
    u32::from_le_bytes(raw)
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

    /// A hot journal beside a file another connection still holds locked is
    /// left alone rather than replayed.
    ///
    /// **The scenario this guards against.** Before every mode carried a
    /// journal, `replay_hot_journal` only ever ran ahead of any lock a second
    /// connection could take - a `wal` connection had no journal to be hot in
    /// the first place. Now it can run in the middle of another connection's
    /// own live checkpoint: that connection has flushed pages and is still
    /// holding the file (under `locking_mode = exclusive`, the default, for
    /// as long as it stays open), and a second connection merely *opening*
    /// the same file - `ImportedDatabase::open_on` calls this before
    /// anything else - would otherwise write the first connection's own
    /// pre-images back over pages that connection has not finished writing,
    /// then delete the journal the first connection still needs.
    ///
    /// `SimVfs` is what makes this a genuine two-holder scenario rather than
    /// one process talking to itself: `file` below and the handle
    /// `replay_hot_journal` opens internally are two different handles on the
    /// same simulated inode, with independent lock state exactly as two
    /// processes on a real file would have, which is what lets the file's own
    /// `LockTable` refuse the second one.
    #[test]
    fn a_hot_journal_beside_a_locked_file_is_left_alone() {
        use inillucent_sim::media::MediaModel;
        use inillucent_sim::sim_vfs::{SimConfig, SimVfs};

        let sim = std::sync::Arc::new(SimVfs::new(SimConfig {
            seed: 4_242,
            model: MediaModel::default(),
            ..SimConfig::default()
        }));
        let vfs: std::sync::Arc<dyn Vfs> = sim.clone();
        let path = DbPath::new("/locked.db");
        let page_size = 64usize;
        let file = vfs
            .open(&path, OpenOptions::main_db())
            .expect("the file opens");
        for page in 0..3u64 {
            let image = vec![page as u8 + 1; page_size];
            file.write_all_at(page * page_size as u64, &image)
                .expect("the page is written");
        }
        // This handle stands in for the connection whose checkpoint is still
        // in progress - EXCLUSIVE, and never released until this test
        // releases it, matching `locking_mode = exclusive`.
        file.lock(FileLock::Exclusive)
            .expect("the first connection takes the lock");

        // A transaction saves a pre-image and seals it, then the crash:
        // `finish` is never called, so the journal is hot beside a file this
        // test's `file` handle is still holding open and locked - exactly the
        // state a real crash mid-checkpoint leaves while the checkpointing
        // connection has not yet closed.
        let mut journal = Journal::new(
            std::sync::Arc::clone(&vfs),
            &path,
            JournalMode::Delete,
            page_size,
        );
        journal
            .save(PageId(0), &[9u8; 64])
            .expect("the pre-image is saved");
        journal.seal().expect("the journal syncs");
        for page in 0..3u64 {
            file.write_all_at(page * page_size as u64, &vec![0xffu8; page_size])
                .expect("the page is overwritten");
        }
        drop(journal);

        let before = sim.trace().len();
        let restored = replay_hot_journal(vfs.as_ref(), &path)
            .expect("a locked file is a refusal, not an error");
        assert!(
            !restored,
            "a hot journal was replayed while another connection held the file"
        );
        let wrote_the_database = sim.trace().events()[before..].iter().any(|event| {
            event.kind == "write" && event.path == path.as_path().display().to_string()
        });
        assert!(
            !wrote_the_database,
            "the database file was written to while another connection held it locked"
        );
        // Not this test's journal to dispose of either - the connection that
        // is still using it has not finished.
        assert!(
            vfs.access(&path.journal(), AccessMode::Exists)
                .unwrap_or(false),
            "a journal that was refused, not replayed, was deleted anyway"
        );

        // Once the first connection lets go, the same journal replays and
        // restores what it names - the lock was the only thing standing in
        // the way.
        file.unlock(FileLock::None)
            .expect("the first connection releases the lock");
        assert!(
            replay_hot_journal(vfs.as_ref(), &path).expect("the replay runs"),
            "the journal should now be free to replay"
        );
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
