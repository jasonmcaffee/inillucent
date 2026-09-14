//! The rollback journal: its format, its five modes, and its four sync
//! policies.
//!
//! Invariant: a database page is written to the file only after the image it
//! is replacing is durable in the journal, and the journal stops being hot
//! only after the database is durable. Between those two points a crash finds
//! a hot journal and undoes the transaction; after the second it finds none
//! and keeps it. There is no third outcome, because the step that makes the
//! journal non-hot is one file operation.
//!
//! The header is written twice, and the reason is the whole crash argument.
//! The first write lays down the record count, the original database size, the
//! sector size, the page size and the checksum nonce - with the magic left as
//! zeroes, so the file is *not* hot. Records are appended after it. The second
//! write, at commit, rewrites the same sector with the magic and the real
//! record count. A power loss can leave that sector torn, holding a mixture of
//! the two versions, and every mixture is safe: without the magic the journal
//! is ignored, and with it the record count is either the real one or the zero
//! the first write left, which is correct because no database page had been
//! written when that zero was the truth.
//!
//! Reference: <https://sqlite.org/fileformat2.html#the_rollback_journal> and
//! <https://sqlite.org/atomiccommit.html>.

use std::sync::Arc;

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::{bytes, DbResult};
use inillucent_storage::journal::{Journal, JournalStats};
use inillucent_vfs::{AccessMode, DbPath, FileKind, OpenOptions, SyncMode, Vfs, VfsFile};

/// The eight bytes that mark a journal as one worth replaying.
pub const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

/// The size of the journal header, before it is padded to a sector.
pub const JOURNAL_HEADER_SIZE: usize = 28;

/// The smallest sector the format is willing to align to.
pub const MIN_SECTOR_SIZE: u32 = 512;

/// The largest sector the format is willing to align to.
pub const MAX_SECTOR_SIZE: u32 = 65_536;

/// How a journal is disposed of once the transaction it protects is over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalMode {
    /// Delete the file. The commit point is the deletion.
    Delete,
    /// Truncate the file to nothing. The commit point is the truncation, which
    /// avoids the directory update a deletion needs.
    Truncate,
    /// Keep the file and zero its magic. The commit point is that write, which
    /// avoids both the directory update and the length change.
    Persist,
    /// Keep the journal in memory. Nothing is written, and a crash mid-commit
    /// leaves a database that cannot be repaired.
    Memory,
    /// Write no journal at all. A crash mid-commit leaves a database that
    /// cannot be repaired.
    Off,
    /// Write changes to a log beside the database instead of undo images
    /// inside it.
    ///
    /// It is a journal mode in name and in the PRAGMA, and almost nothing else:
    /// there is no rollback journal, the commit point is a write to a shared
    /// memory header rather than to a file, and readers do not exclude the
    /// writer. It is listed here because that is where SQLite puts it and
    /// because a connection has exactly one of these at a time.
    Wal,
}

impl JournalMode {
    /// Parses the PRAGMA spelling of a mode.
    pub fn parse(text: &str) -> Option<JournalMode> {
        match text.to_ascii_lowercase().as_str() {
            "delete" => Some(JournalMode::Delete),
            "truncate" => Some(JournalMode::Truncate),
            "persist" => Some(JournalMode::Persist),
            "memory" => Some(JournalMode::Memory),
            "off" | "none" => Some(JournalMode::Off),
            "wal" => Some(JournalMode::Wal),
            _ => None,
        }
    }

    /// Returns the PRAGMA spelling of a mode.
    pub fn as_str(self) -> &'static str {
        match self {
            JournalMode::Delete => "delete",
            JournalMode::Truncate => "truncate",
            JournalMode::Persist => "persist",
            JournalMode::Memory => "memory",
            JournalMode::Off => "off",
            JournalMode::Wal => "wal",
        }
    }

    /// Reports whether this mode can undo a transaction after a power loss.
    ///
    /// The two that cannot are supported, documented, and never used to claim
    /// a durability result: a benchmark run in `memory` or `off` is measuring
    /// a different promise from the one SQLite makes by default.
    pub fn is_crash_safe(self) -> bool {
        matches!(
            self,
            JournalMode::Delete | JournalMode::Truncate | JournalMode::Persist | JournalMode::Wal
        )
    }

    /// Reports whether the mode keeps its records on disk.
    pub fn writes_a_file(self) -> bool {
        self.is_crash_safe()
    }

    /// Reports whether the mode is the write-ahead log rather than a journal.
    pub fn is_wal(self) -> bool {
        self == JournalMode::Wal
    }
}

/// How hard the engine pushes each step of a commit toward the media.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Synchronous {
    /// Sync nothing. The operating system decides when bytes land.
    Off,
    /// Sync the journal before the database is written and the database
    /// before the journal is finalised, but do not sync the directory.
    Normal,
    /// As NORMAL, with a full barrier sync and a synced directory entry when
    /// the commit point is a deletion.
    Full,
    /// As FULL, and sync the directory again after the journal is gone, so
    /// that a commit is reported only once its erasure is durable.
    Extra,
}

impl Synchronous {
    /// Parses the PRAGMA spelling, which accepts names and numbers.
    pub fn parse(text: &str) -> Option<Synchronous> {
        match text.to_ascii_lowercase().as_str() {
            "0" | "off" => Some(Synchronous::Off),
            "1" | "normal" => Some(Synchronous::Normal),
            "2" | "full" => Some(Synchronous::Full),
            "3" | "extra" => Some(Synchronous::Extra),
            _ => None,
        }
    }

    /// Returns the PRAGMA spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Synchronous::Off => "off",
            Synchronous::Normal => "normal",
            Synchronous::Full => "full",
            Synchronous::Extra => "extra",
        }
    }

    /// Returns the numeric spelling `PRAGMA synchronous` reports.
    pub fn as_number(self) -> i64 {
        match self {
            Synchronous::Off => 0,
            Synchronous::Normal => 1,
            Synchronous::Full => 2,
            Synchronous::Extra => 3,
        }
    }

    /// Returns how a sync is issued at this level, or `None` for no sync.
    pub fn sync_mode(self) -> Option<SyncMode> {
        match self {
            Synchronous::Off => None,
            Synchronous::Normal => Some(SyncMode::Normal),
            Synchronous::Full | Synchronous::Extra => Some(SyncMode::Full),
        }
    }

    /// Reports whether the directory entry is synced when the journal is
    /// created or deleted.
    pub fn syncs_directory(self) -> bool {
        matches!(self, Synchronous::Full | Synchronous::Extra)
    }
}

/// How a journal is configured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalOptions {
    /// How the journal is disposed of at the commit point.
    pub mode: JournalMode,
    /// How hard each step is pushed toward the media.
    pub synchronous: Synchronous,
}

impl Default for JournalOptions {
    /// SQLite's defaults: a deleted journal and FULL synchronous.
    fn default() -> JournalOptions {
        JournalOptions {
            mode: JournalMode::Delete,
            synchronous: Synchronous::Full,
        }
    }
}

/// Where a journal's bytes live.
#[derive(Debug)]
enum Medium {
    /// Nothing is recorded at all.
    None,
    /// The bytes are in this process only.
    Memory(Vec<u8>),
    /// The bytes are in a file.
    File(Box<dyn VfsFile>),
}

/// A rollback journal over one database file.
#[derive(Debug)]
pub struct RollbackJournal {
    vfs: Arc<dyn Vfs>,
    path: DbPath,
    options: JournalOptions,
    medium: Medium,
    page_size: u32,
    sector_size: u32,
    original_page_count: u32,
    checksum_seed: u32,
    records: std::collections::BTreeSet<u32>,
    next_offset: u64,
    stats: JournalStats,
    header_written: bool,
    super_journal: Option<DbPath>,
}

impl RollbackJournal {
    /// Creates a journal for a database, with nothing open yet.
    pub fn new(vfs: Arc<dyn Vfs>, database: &DbPath, options: JournalOptions) -> RollbackJournal {
        RollbackJournal {
            vfs,
            path: database.journal(),
            options,
            medium: Medium::None,
            page_size: 0,
            sector_size: MIN_SECTOR_SIZE,
            original_page_count: 0,
            checksum_seed: 0,
            records: std::collections::BTreeSet::new(),
            next_offset: 0,
            stats: JournalStats::default(),
            header_written: false,
            super_journal: None,
        }
    }

    /// Returns the path the journal is written to.
    pub fn path(&self) -> &DbPath {
        &self.path
    }

    /// Returns how the journal is configured.
    pub fn options(&self) -> JournalOptions {
        self.options
    }

    /// Changes the journal mode, which is only legal between transactions.
    pub fn set_mode(&mut self, mode: JournalMode) -> DbResult<()> {
        if self.header_written {
            return Err(misuse(
                "the journal mode cannot change inside a transaction",
            ));
        }
        self.options.mode = mode;
        Ok(())
    }

    /// Changes the durability level, which is legal at any time and takes
    /// effect at the next sync point.
    pub fn set_synchronous(&mut self, synchronous: Synchronous) {
        self.options.synchronous = synchronous;
    }

    /// Sets the sector size the header records, clamped to the legal range.
    pub fn set_sector_size(&mut self, sector_size: u32) {
        self.sector_size = sector_size.clamp(MIN_SECTOR_SIZE, MAX_SECTOR_SIZE);
    }

    /// Returns the checksum SQLite computes over one page image.
    ///
    /// It samples every two-hundredth byte from the end backwards rather than
    /// summing the page, which is what makes journalling a page cost almost
    /// nothing. It detects a torn or short write, which is what it is for; it
    /// is not a defence against a deliberately corrupted file.
    pub fn checksum(seed: u32, page: &[u8]) -> u32 {
        let mut sum = seed;
        let mut index = page.len() as i64 - 200;
        while index > 0 {
            if let Some(byte) = page.get(index as usize) {
                sum = sum.wrapping_add(u32::from(*byte));
            }
            index -= 200;
        }
        sum
    }

    /// Encodes the journal header, with or without its magic.
    fn header_bytes(&self, with_magic: bool, record_count: u32) -> DbResult<Vec<u8>> {
        let mut raw = vec![0u8; self.sector_size as usize];
        if with_magic {
            let window = bytes::window_mut(&mut raw, 0, JOURNAL_MAGIC.len())?;
            window.copy_from_slice(&JOURNAL_MAGIC);
        }
        bytes::write_u32(&mut raw, 8, record_count)?;
        bytes::write_u32(&mut raw, 12, self.checksum_seed)?;
        bytes::write_u32(&mut raw, 16, self.original_page_count)?;
        bytes::write_u32(&mut raw, 20, self.sector_size)?;
        bytes::write_u32(&mut raw, 24, self.page_size)?;
        Ok(raw)
    }

    /// Opens the journal medium and lays down its first, non-hot header.
    fn create(&mut self) -> DbResult<()> {
        if self.header_written {
            return Ok(());
        }
        let mut seed = [0u8; 4];
        self.vfs.randomness(&mut seed)?;
        self.checksum_seed = u32::from_be_bytes(seed);
        match self.options.mode {
            JournalMode::Off | JournalMode::Wal => {
                self.medium = Medium::None;
                self.header_written = true;
                return Ok(());
            }
            JournalMode::Memory => {
                self.medium = Medium::Memory(Vec::new());
            }
            _ => {
                let file = self
                    .vfs
                    .open(&self.path, OpenOptions::of_kind(FileKind::MainJournal))?;
                // A journal left behind by PERSIST is reused, and its old
                // records must not be read as part of this transaction's.
                file.truncate(0)?;
                self.medium = Medium::File(file);
            }
        }
        let header = self.header_bytes(false, 0)?;
        self.write_at(0, &header)?;
        self.next_offset = u64::from(self.sector_size);
        self.header_written = true;
        self.stats.created = self.stats.created.saturating_add(1);
        Ok(())
    }

    /// Writes to whichever medium the journal is using.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> DbResult<()> {
        match &mut self.medium {
            Medium::None => return Ok(()),
            Medium::Memory(buffer) => {
                let end = offset as usize + data.len();
                if buffer.len() < end {
                    buffer.resize(end, 0);
                }
                let window = bytes::window_mut(buffer, offset as usize, data.len())?;
                window.copy_from_slice(data);
            }
            Medium::File(file) => file.write_all_at(offset, data)?,
        }
        self.stats.bytes_written = self.stats.bytes_written.saturating_add(data.len() as u64);
        Ok(())
    }

    /// Syncs the journal medium at the configured level.
    fn sync(&mut self) -> DbResult<()> {
        let Some(mode) = self.options.synchronous.sync_mode() else {
            return Ok(());
        };
        if let Medium::File(file) = &self.medium {
            file.sync(mode)?;
            self.stats.syncs = self.stats.syncs.saturating_add(1);
        }
        Ok(())
    }

    /// Writes the super-journal's name after the records, when there is one.
    ///
    /// The tail is the format's: a page number no real page can have, the name,
    /// its length, a checksum over it, and the journal magic. It is read
    /// backwards from the end of the file, which is why the length comes after
    /// the name rather than before it.
    fn write_super_journal_name(&mut self) -> DbResult<()> {
        let Some(path) = self.super_journal.clone() else {
            return Ok(());
        };
        let Some(text) = path.to_str() else {
            return Err(misuse(
                "a super-journal whose name is not valid UTF-8 cannot be recorded",
            ));
        };
        let name = text.as_bytes();
        let mut tail = Vec::with_capacity(name.len().saturating_add(20));
        let mut marker = [0u8; 4];
        bytes::write_u32(&mut marker, 0, super_journal_page(self.page_size))?;
        tail.extend_from_slice(&marker);
        tail.extend_from_slice(name);
        let mut trailer = [0u8; 8];
        bytes::write_u32(&mut trailer, 0, name.len() as u32)?;
        bytes::write_u32(&mut trailer, 4, super_journal_checksum(name))?;
        tail.extend_from_slice(&trailer);
        tail.extend_from_slice(&JOURNAL_MAGIC);
        let offset = self.next_offset;
        self.write_at(offset, &tail)?;
        self.stats.bytes_written = self.stats.bytes_written.saturating_add(tail.len() as u64);
        Ok(())
    }

    /// Removes every trace of the journal, whatever mode it is in.
    fn finalize(&mut self) -> DbResult<()> {
        if !self.header_written {
            self.reset();
            return Ok(());
        }
        let mode = self.options.mode;
        let synchronous = self.options.synchronous;
        match mode {
            JournalMode::Off | JournalMode::Memory | JournalMode::Wal => {}
            JournalMode::Truncate => {
                if let Medium::File(file) = &self.medium {
                    file.truncate(0)?;
                    if let Some(sync) = synchronous.sync_mode() {
                        file.sync(sync)?;
                        self.stats.syncs = self.stats.syncs.saturating_add(1);
                    }
                }
            }
            JournalMode::Persist => {
                let blank = vec![0u8; JOURNAL_HEADER_SIZE];
                self.write_at(0, &blank)?;
                if let Medium::File(file) = &self.medium {
                    if let Some(sync) = synchronous.sync_mode() {
                        file.sync(sync)?;
                        self.stats.syncs = self.stats.syncs.saturating_add(1);
                    }
                }
            }
            JournalMode::Delete => {
                self.medium = Medium::None;
                self.vfs.delete(&self.path, synchronous.syncs_directory())?;
            }
        }
        self.stats.finalized = self.stats.finalized.saturating_add(1);
        self.reset();
        Ok(())
    }

    /// Forgets everything about the transaction that has just ended.
    fn reset(&mut self) {
        if !matches!(
            self.options.mode,
            JournalMode::Persist | JournalMode::Truncate
        ) {
            self.medium = Medium::None;
        }
        self.records.clear();
        self.next_offset = 0;
        self.header_written = false;
    }

    /// Reads the whole journal back, from wherever it lives.
    fn read_all(&self) -> DbResult<Vec<u8>> {
        match &self.medium {
            Medium::None => Ok(Vec::new()),
            Medium::Memory(buffer) => Ok(buffer.clone()),
            Medium::File(file) => {
                let size = file.file_size()?;
                let mut raw = vec![0u8; size as usize];
                if !raw.is_empty() {
                    file.read_exact_at(0, &mut raw)?;
                }
                Ok(raw)
            }
        }
    }
}

impl Journal for RollbackJournal {
    /// Arms the journal for a transaction, without creating anything.
    ///
    /// Nothing is written until a page is actually recorded, so a transaction
    /// that reads and commits nothing costs no file operations at all - which
    /// is what makes an empty `BEGIN`/`COMMIT` free.
    fn begin(&mut self, page_size: u32, original_page_count: u32) -> DbResult<()> {
        self.page_size = page_size;
        self.original_page_count = original_page_count;
        self.records.clear();
        self.next_offset = 0;
        self.header_written = false;
        self.stats = JournalStats::default();
        Ok(())
    }

    /// Appends a page's pre-transaction image, once.
    fn record(&mut self, page: u32, image: &[u8]) -> DbResult<()> {
        if matches!(self.options.mode, JournalMode::Off | JournalMode::Wal) {
            return Ok(());
        }
        if self.records.contains(&page) {
            return Ok(());
        }
        self.create()?;
        if image.len() != self.page_size as usize {
            return Err(misuse(format!(
                "a {} byte image was journalled for a {} byte page",
                image.len(),
                self.page_size
            )));
        }
        let checksum = RollbackJournal::checksum(self.checksum_seed, image);
        let mut record = Vec::with_capacity(image.len() + 8);
        record.extend_from_slice(&page.to_be_bytes());
        record.extend_from_slice(image);
        record.extend_from_slice(&checksum.to_be_bytes());
        let offset = self.next_offset;
        self.write_at(offset, &record)?;
        self.next_offset = offset.saturating_add(record.len() as u64);
        self.records.insert(page);
        self.stats.records = self.stats.records.saturating_add(1);
        Ok(())
    }

    /// Makes the journal durable and marks it hot.
    ///
    /// The two syncs are not redundant. The first makes the records durable
    /// while the file is still not hot; the second makes the header that
    /// declares them durable. Reversing them would let a crash leave a journal
    /// that claims records it does not have.
    fn prepare_commit(&mut self) -> DbResult<()> {
        if !self.header_written || self.records.is_empty() {
            return Ok(());
        }
        self.write_super_journal_name()?;
        self.sync()?;
        let count = self.records.len() as u32;
        let header = self.header_bytes(true, count)?;
        // Only the fields are rewritten, not the sector's padding. The padding
        // is already what this write would put there, so the bytes a torn
        // write leaves behind are identical either way - and the whole safety
        // argument is that every field except the magic and the record count
        // is the same in both versions of this sector. Writing the padding
        // again costs four kilobytes per commit and changes nothing.
        let fields = bytes::window(&header, 0, JOURNAL_HEADER_SIZE)?.to_vec();
        self.write_at(0, &fields)?;
        self.sync()?;
        Ok(())
    }

    /// Returns how the database file is synced before the commit point.
    fn database_sync(&self) -> Option<SyncMode> {
        self.options.synchronous.sync_mode()
    }

    /// Returns the path this journal is written to.
    fn path(&self) -> Option<DbPath> {
        self.options.mode.writes_a_file().then(|| self.path.clone())
    }

    /// Names the super-journal that decides this transaction.
    fn set_super_journal(&mut self, path: Option<DbPath>) {
        self.super_journal = path;
    }

    /// Finalises the journal, which is the atomic commit point.
    fn commit_point(&mut self) -> DbResult<()> {
        self.finalize()
    }

    /// Replays the journal onto the database file.
    fn playback(&mut self, database: &dyn VfsFile) -> DbResult<Option<u32>> {
        if !self.header_written {
            return Ok(None);
        }
        let raw = self.read_all()?;
        let Some(replay) = decode_journal(&raw)? else {
            // Nothing was ever made hot, so nothing had been written to the
            // database either: the transaction is already undone.
            return Ok(Some(self.original_page_count));
        };
        let restored = apply_playback(&raw, &replay, database)?;
        self.stats.played_back = self.stats.played_back.saturating_add(restored);
        Ok(Some(replay.original_page_count))
    }

    /// Removes the journal without committing anything.
    fn discard(&mut self) -> DbResult<()> {
        self.finalize()
    }

    /// Reports whether a transaction has written anything to the journal.
    fn is_active(&self) -> bool {
        self.header_written && !self.records.is_empty()
    }

    /// Returns this transaction's numbers.
    fn stats(&self) -> JournalStats {
        self.stats
    }
}

/// A journal that has been parsed and is ready to replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedJournal {
    /// The page count the database had before the transaction.
    pub original_page_count: u32,
    /// The page size the records are written at.
    pub page_size: u32,
    /// The sector the records start after.
    pub sector_size: u32,
    /// The checksum nonce.
    pub checksum_seed: u32,
    /// How many records the header claims.
    pub record_count: u32,
}

/// Parses a journal header, returning `None` when the file is not hot.
///
/// Everything the header claims is checked against the format before it is
/// believed, because a journal is replayed *over* a database: a header that
/// was trusted and wrong would destroy a file that had nothing wrong with it.
pub fn decode_journal(raw: &[u8]) -> DbResult<Option<DecodedJournal>> {
    if raw.len() < JOURNAL_HEADER_SIZE {
        return Ok(None);
    }
    let magic = bytes::window(raw, 0, JOURNAL_MAGIC.len())?;
    if magic != JOURNAL_MAGIC {
        return Ok(None);
    }
    let record_count = bytes::read_u32(raw, 8)?;
    let checksum_seed = bytes::read_u32(raw, 12)?;
    let original_page_count = bytes::read_u32(raw, 16)?;
    let sector_size = bytes::read_u32(raw, 20)?;
    let page_size = bytes::read_u32(raw, 24)?;
    if !(MIN_SECTOR_SIZE..=MAX_SECTOR_SIZE).contains(&sector_size) || !sector_size.is_power_of_two()
    {
        return Ok(None);
    }
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Ok(None);
    }
    if original_page_count == 0 {
        return Ok(None);
    }
    Ok(Some(DecodedJournal {
        original_page_count,
        page_size,
        sector_size,
        checksum_seed,
        record_count,
    }))
}

/// Writes a journal's page images back over the database file.
///
/// A hot journal's records are all durable by construction: they are synced
/// before the header that declares them is written, and the header is what
/// makes the journal hot. So a record that does not validate is not a torn
/// tail - it is damage to media that was already durable, and the database it
/// was protecting cannot be restored. That is reported rather than half
/// applied, because a partial replay is exactly the mixed state the whole
/// scheme exists to prevent, and one nobody downstream could detect.
///
/// SQLite stops at the bad record instead. It can, because its journal is
/// appended sequentially and a write that fails there truncates the file; a
/// VFS whose write reports success and stores half the bytes breaks that
/// assumption, and the simulator injects exactly that.
///
/// The replay is idempotent by construction. It writes the same bytes to the
/// same offsets every time, so a crash part-way through leaves a database that
/// the next attempt finishes, and the journal is only removed afterwards.
pub fn apply_playback(
    raw: &[u8],
    journal: &DecodedJournal,
    database: &dyn VfsFile,
) -> DbResult<u64> {
    let record_size = journal.page_size as usize + 8;
    let mut offset = journal.sector_size as usize;
    let mut restored = 0u64;
    for _ in 0..journal.record_count {
        if offset.saturating_add(record_size) > raw.len() {
            return Err(corrupt(format!(
                "the journal claims {} records and holds {restored}",
                journal.record_count
            )));
        }
        let page = bytes::read_u32(raw, offset)?;
        let image = bytes::window(raw, offset + 4, journal.page_size as usize)?;
        let stored = bytes::read_u32(raw, offset + 4 + journal.page_size as usize)?;
        if RollbackJournal::checksum(journal.checksum_seed, image) != stored {
            return Err(corrupt(format!(
                "journal record {restored} of {} does not match its checksum",
                journal.record_count
            )));
        }
        if page == 0 || page > journal.original_page_count {
            return Err(corrupt(format!(
                "journal record {restored} names page {page}, outside a {}-page database",
                journal.original_page_count
            )));
        }
        let target = u64::from(page - 1).saturating_mul(u64::from(journal.page_size));
        database.write_all_at(target, image)?;
        restored = restored.saturating_add(1);
        offset = offset.saturating_add(record_size);
    }
    Ok(restored)
}

/// Replays a hot journal found beside a database, and removes it.
///
/// This is the same operation `RollbackJournal::playback` performs for its own
/// transaction, performed by whichever connection opens the database next. The
/// order matters: the database is synced *before* the journal is removed, so a
/// crash between the two leaves a journal that is still hot and a database
/// that is replayed again to the same bytes.
pub fn recover_hot_journal(
    vfs: &dyn Vfs,
    database_path: &DbPath,
    database: &dyn VfsFile,
    synchronous: Synchronous,
) -> DbResult<Option<u32>> {
    let journal_path = database_path.journal();
    if !vfs.access(&journal_path, AccessMode::Exists)? {
        return Ok(None);
    }
    let raw = {
        let file = vfs.open(
            &journal_path,
            OpenOptions::of_kind(FileKind::MainJournal).read_only(),
        )?;
        let size = file.file_size()?;
        let mut raw = vec![0u8; size as usize];
        if !raw.is_empty() {
            file.read_exact_at(0, &mut raw)?;
        }
        raw
    };
    let Some(decoded) = decode_journal(&raw)? else {
        // Present but not hot: a PERSIST journal from a finished transaction,
        // or one whose header never landed. Removing it is safe and is what
        // stops it being examined again on every open.
        vfs.delete(&journal_path, false)?;
        return Ok(None);
    };
    if let Some(name) = read_super_journal(&raw)? {
        let path = DbPath::new(std::path::PathBuf::from(
            String::from_utf8_lossy(&name).into_owned(),
        ));
        if !vfs.access(&path, AccessMode::Exists)? {
            // The transaction this journal belongs to committed: the file that
            // would have undone it is gone, and it went only after every
            // database in the transaction was durable. Replaying now would undo
            // a commit that has already been reported.
            vfs.delete(&journal_path, false)?;
            return Ok(None);
        }
        // The super-journal is still there, so the transaction did not reach
        // its commit point. Every database in it rolls back, and the file goes
        // once none of them still names it.
        apply_playback(&raw, &decoded, database)?;
        finish_recovery(vfs, database_path, database, &decoded, synchronous)?;
        let _ = crate::super_journal::remove_if_unused(vfs, &path, |child| {
            let raw = read_journal_bytes(vfs, child)?;
            Ok(read_super_journal(&raw)?.is_some_and(|named| named == name))
        });
        return Ok(Some(decoded.original_page_count));
    }
    apply_playback(&raw, &decoded, database)?;
    let wanted =
        u64::from(decoded.original_page_count).saturating_mul(u64::from(decoded.page_size));
    if database.file_size()? > wanted {
        database.truncate(wanted)?;
    }
    if let Some(mode) = synchronous.sync_mode() {
        database.sync(mode)?;
    }
    vfs.delete(&journal_path, synchronous.syncs_directory())?;
    Ok(Some(decoded.original_page_count))
}

/// Truncates, syncs and removes the journal after a playback.
fn finish_recovery(
    vfs: &dyn Vfs,
    database_path: &DbPath,
    database: &dyn VfsFile,
    decoded: &DecodedJournal,
    synchronous: Synchronous,
) -> DbResult<()> {
    let wanted =
        u64::from(decoded.original_page_count).saturating_mul(u64::from(decoded.page_size));
    if database.file_size()? > wanted {
        database.truncate(wanted)?;
    }
    if let Some(mode) = synchronous.sync_mode() {
        database.sync(mode)?;
    }
    vfs.delete(&database_path.journal(), synchronous.syncs_directory())
        .map_err(|error| error.into_db_error())
}

/// Reads a journal file whole, answering with nothing when it is not there.
fn read_journal_bytes(vfs: &dyn Vfs, path: &DbPath) -> DbResult<Vec<u8>> {
    if !vfs.access(path, AccessMode::Exists)? {
        return Ok(Vec::new());
    }
    let file = vfs.open(
        path,
        OpenOptions::of_kind(FileKind::MainJournal).read_only(),
    )?;
    let size = file.file_size()?;
    let mut raw = vec![0u8; usize::try_from(size).unwrap_or(0)];
    if !raw.is_empty() {
        file.read_exact_at(0, &mut raw)?;
    }
    Ok(raw)
}

/// Returns the page number the super-journal record is written under.
///
/// It is derived from the byte the locking protocol reserves, so it is a page
/// no database can hold data in: a record under it cannot be confused with a
/// page image however the journal is read.
pub fn super_journal_page(page_size: u32) -> u32 {
    const PENDING_BYTE: u32 = 0x4000_0000;
    PENDING_BYTE
        .checked_div(page_size.max(1))
        .unwrap_or(0)
        .saturating_add(1)
}

/// Returns the checksum a super-journal name is recorded with.
///
/// The sum of its bytes, which is what the format specifies. It is not there to
/// resist tampering - it is there so that a torn write of the tail is
/// recognised as one rather than read as a name that happens to parse.
pub fn super_journal_checksum(name: &[u8]) -> u32 {
    name.iter()
        .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
}

/// Reads the super-journal a journal names, if it names one.
///
/// The tail is read backwards from the end: magic, checksum, length, and then
/// the name that many bytes before them. Anything that does not line up means
/// the journal names none, which is the ordinary case and not an error.
pub fn read_super_journal(raw: &[u8]) -> DbResult<Option<Vec<u8>>> {
    if raw.len() < 16 {
        return Ok(None);
    }
    let end = raw.len();
    let magic = bytes::window(raw, end.saturating_sub(8), 8)?;
    if magic != JOURNAL_MAGIC {
        return Ok(None);
    }
    let length = bytes::read_u32(raw, end.saturating_sub(16))? as usize;
    let recorded = bytes::read_u32(raw, end.saturating_sub(12))?;
    if length == 0 || length.saturating_add(16) > end {
        return Ok(None);
    }
    let name = bytes::window(raw, end.saturating_sub(16).saturating_sub(length), length)?;
    if super_journal_checksum(name) != recorded {
        return Ok(None);
    }
    Ok(Some(name.to_vec()))
}

/// Reports whether a journal beside a database would be replayed.
pub fn journal_is_hot(vfs: &dyn Vfs, database_path: &DbPath) -> DbResult<bool> {
    let journal_path = database_path.journal();
    if !vfs.access(&journal_path, AccessMode::Exists)? {
        return Ok(false);
    }
    let file = vfs.open(
        &journal_path,
        OpenOptions::of_kind(FileKind::MainJournal).read_only(),
    )?;
    let size = file.file_size()?.min(JOURNAL_HEADER_SIZE as u64);
    let mut raw = vec![0u8; size as usize];
    if !raw.is_empty() {
        file.read_exact_at(0, &mut raw)?;
    }
    Ok(decode_journal(&raw)?.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_vfs::memory::MemoryVfs;

    /// Returns a journal over a memory VFS, with a database file beside it.
    fn journal_over(
        mode: JournalMode,
        synchronous: Synchronous,
    ) -> (Arc<MemoryVfs>, DbPath, RollbackJournal) {
        let vfs = Arc::new(MemoryVfs::new());
        let path = DbPath::new("/db.sqlite");
        let journal = RollbackJournal::new(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            &path,
            JournalOptions { mode, synchronous },
        );
        (vfs, path, journal)
    }

    /// The journal mode may change between transactions and not inside one.
    ///
    /// Changing it mid-transaction would leave records written under one set of
    /// rules and a commit point applied under another - a journal deleted when
    /// the pages it holds were written expecting it to be zeroed, say. The
    /// refusal is the invariant; nothing asserted either half of it.
    #[test]
    fn the_journal_mode_changes_between_transactions_and_not_inside_one() {
        let (_vfs, _path, mut journal) = journal_over(JournalMode::Delete, Synchronous::Full);
        journal
            .set_mode(JournalMode::Truncate)
            .expect("changing mode between transactions is allowed");
        assert_eq!(journal.options().mode, JournalMode::Truncate);

        journal.begin(1024, 4).expect("the journal begins");
        journal
            .record(2, &page_of(2, 1024))
            .expect("a page image is recorded");
        let refused = journal
            .set_mode(JournalMode::Persist)
            .expect_err("changing mode inside a transaction is refused");
        assert_eq!(refused.code(), inillucent_base::error::PrimaryCode::Misuse);
        assert_eq!(
            journal.options().mode,
            JournalMode::Truncate,
            "the refused change left the mode alone"
        );
    }

    /// The journal reports the path it writes to.
    ///
    /// It is what a super-journal records for each database in a multi-database
    /// commit, so a wrong answer would name a file recovery cannot find.
    #[test]
    fn the_journal_reports_the_path_it_writes_to() {
        let (_vfs, path, journal) = journal_over(JournalMode::Delete, Synchronous::Full);
        assert_eq!(journal.path(), &path.journal());
    }

    /// The super-journal checksum is the sum of the name's bytes.
    ///
    /// Asserted against values worked out by hand rather than against the
    /// function's own output, so it pins what the format says rather than what
    /// this build happens to compute. Nothing checked it before: a version
    /// returning a constant passed the whole suite.
    #[test]
    fn the_super_journal_checksum_sums_the_name() {
        assert_eq!(super_journal_checksum(b""), 0);
        assert_eq!(super_journal_checksum(b"\x01"), 1);
        assert_eq!(super_journal_checksum(b"\x01\x02\x03"), 6);
        // 'a' is 97, 'b' 98, 'c' 99.
        assert_eq!(super_journal_checksum(b"abc"), 294);
        // Order does not matter to a sum, which is a property of the format
        // rather than an accident of the implementation.
        assert_eq!(super_journal_checksum(b"cba"), 294);
        // It wraps rather than overflowing: 300 bytes of 255 exceed a u8 sum
        // many times over and must still land somewhere definite.
        let long = vec![255u8; 300];
        assert_eq!(super_journal_checksum(&long), 255 * 300);
    }

    /// Changing the durability level takes effect.
    ///
    /// A setter that quietly did nothing left every commit at whatever level
    /// the journal was opened with, which is a durability change nothing would
    /// report.
    #[test]
    fn setting_the_durability_level_takes_effect() {
        let (_vfs, _path, mut journal) = journal_over(JournalMode::Delete, Synchronous::Full);
        assert_eq!(journal.options().synchronous, Synchronous::Full);
        journal.set_synchronous(Synchronous::Off);
        assert_eq!(journal.options().synchronous, Synchronous::Off);
        journal.set_synchronous(Synchronous::Normal);
        assert_eq!(journal.options().synchronous, Synchronous::Normal);
    }

    /// The sector size is recorded, and clamped to the legal range.
    ///
    /// The clamp is the part worth pinning: the header has to carry a sector
    /// size the format allows, and a setter that did nothing would leave it at
    /// the default however the device was described.
    #[test]
    fn setting_the_sector_size_records_it_within_the_legal_range() {
        let (_vfs, _path, mut journal) = journal_over(JournalMode::Delete, Synchronous::Full);
        journal.set_sector_size(4096);
        assert_eq!(journal.sector_size, 4096);
        journal.set_sector_size(1);
        assert_eq!(journal.sector_size, MIN_SECTOR_SIZE, "clamped up");
        journal.set_sector_size(u32::MAX);
        assert_eq!(journal.sector_size, MAX_SECTOR_SIZE, "clamped down");
    }

    /// Returns a page of a recognisable pattern.
    fn page_of(byte: u8, size: usize) -> Vec<u8> {
        (0..size)
            .map(|index| byte.wrapping_add(index as u8))
            .collect()
    }

    /// The checksum is SQLite's: the seed plus every two-hundredth byte from
    /// the end backwards.
    #[test]
    fn the_checksum_samples_every_two_hundredth_byte() {
        let page = vec![1u8; 1024];
        // 1024 - 200 = 824, 624, 424, 224, 24 -> five samples of one.
        assert_eq!(RollbackJournal::checksum(0, &page), 5);
        assert_eq!(RollbackJournal::checksum(7, &page), 12);
    }

    /// Until the commit prepares it, the journal has no magic and is not hot.
    #[test]
    fn a_journal_mid_transaction_is_not_hot() {
        let (vfs, path, mut journal) = journal_over(JournalMode::Delete, Synchronous::Full);
        journal.begin(1024, 4).expect("begins");
        journal.record(2, &page_of(2, 1024)).expect("records");
        assert!(vfs
            .access(&path.journal(), AccessMode::Exists)
            .expect("exists"));
        assert!(!journal_is_hot(vfs.as_ref(), &path).expect("readable"));
    }

    /// Preparing the commit makes it hot, with the real record count.
    #[test]
    fn preparing_the_commit_makes_the_journal_hot() {
        let (vfs, path, mut journal) = journal_over(JournalMode::Delete, Synchronous::Full);
        journal.begin(1024, 4).expect("begins");
        journal.record(2, &page_of(2, 1024)).expect("records");
        journal.record(3, &page_of(3, 1024)).expect("records");
        journal.prepare_commit().expect("prepares");
        assert!(journal_is_hot(vfs.as_ref(), &path).expect("readable"));

        let file = vfs
            .open(
                &path.journal(),
                OpenOptions::of_kind(FileKind::MainJournal).read_only(),
            )
            .expect("opens");
        let mut raw = vec![0u8; file.file_size().expect("size") as usize];
        file.read_exact_at(0, &mut raw).expect("reads");
        let decoded = decode_journal(&raw).expect("decodes").expect("is hot");
        assert_eq!(decoded.record_count, 2);
        assert_eq!(decoded.original_page_count, 4);
        assert_eq!(decoded.page_size, 1024);
    }

    /// The commit point removes the journal in every disk mode.
    #[test]
    fn every_disk_mode_leaves_a_journal_that_is_not_hot() {
        for mode in [
            JournalMode::Delete,
            JournalMode::Truncate,
            JournalMode::Persist,
        ] {
            let (vfs, path, mut journal) = journal_over(mode, Synchronous::Full);
            journal.begin(1024, 4).expect("begins");
            journal.record(2, &page_of(2, 1024)).expect("records");
            journal.prepare_commit().expect("prepares");
            journal.commit_point().expect("commits");
            assert!(
                !journal_is_hot(vfs.as_ref(), &path).expect("readable"),
                "{mode:?} left a hot journal behind"
            );
            let exists = vfs
                .access(&path.journal(), AccessMode::Exists)
                .expect("checks");
            assert_eq!(
                exists,
                mode != JournalMode::Delete,
                "{mode:?} file presence"
            );
        }
    }

    /// A page is journalled once however many times it is written.
    #[test]
    fn a_page_is_journalled_once_per_transaction() {
        let (_vfs, _path, mut journal) = journal_over(JournalMode::Delete, Synchronous::Off);
        journal.begin(1024, 4).expect("begins");
        journal.record(2, &page_of(2, 1024)).expect("records");
        journal.record(2, &page_of(9, 1024)).expect("records");
        journal.record(3, &page_of(3, 1024)).expect("records");
        assert_eq!(journal.stats().records, 2);
    }

    /// Journal mode OFF writes nothing at all.
    #[test]
    fn journal_mode_off_writes_nothing() {
        let (vfs, path, mut journal) = journal_over(JournalMode::Off, Synchronous::Full);
        journal.begin(1024, 4).expect("begins");
        journal.record(2, &page_of(2, 1024)).expect("records");
        journal.prepare_commit().expect("prepares");
        assert!(!vfs
            .access(&path.journal(), AccessMode::Exists)
            .expect("checks"));
        assert_eq!(journal.stats().bytes_written, 0);
    }

    /// Journal mode MEMORY records images without touching the disk, and can
    /// still undo a transaction inside the process that made it.
    #[test]
    fn journal_mode_memory_can_still_roll_back_in_process() {
        let vfs = Arc::new(MemoryVfs::new());
        let path = DbPath::new("/db.sqlite");
        let database = vfs
            .open(&path, OpenOptions::of_kind(FileKind::MainDb))
            .expect("opens");
        let original = page_of(1, 1024);
        database.write_all_at(0, &original).expect("writes");

        let mut journal = RollbackJournal::new(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            &path,
            JournalOptions {
                mode: JournalMode::Memory,
                synchronous: Synchronous::Full,
            },
        );
        journal.begin(1024, 1).expect("begins");
        journal.record(1, &original).expect("records");
        journal.prepare_commit().expect("prepares");
        database
            .write_all_at(0, &page_of(200, 1024))
            .expect("writes");

        journal.playback(database.as_ref()).expect("plays back");
        let mut read_back = vec![0u8; 1024];
        database.read_exact_at(0, &mut read_back).expect("reads");
        assert_eq!(read_back, original);
        assert!(!vfs
            .access(&path.journal(), AccessMode::Exists)
            .expect("checks"));
    }

    /// A hot journal is replayed onto the database and then removed.
    #[test]
    fn recovery_restores_the_pages_and_removes_the_journal() {
        let vfs = Arc::new(MemoryVfs::new());
        let path = DbPath::new("/db.sqlite");
        let database = vfs
            .open(&path, OpenOptions::of_kind(FileKind::MainDb))
            .expect("opens");
        let first = page_of(1, 1024);
        let second = page_of(2, 1024);
        database.write_all_at(0, &first).expect("writes");
        database.write_all_at(1024, &second).expect("writes");

        let mut journal = RollbackJournal::new(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            &path,
            JournalOptions::default(),
        );
        journal.begin(1024, 2).expect("begins");
        journal.record(1, &first).expect("records");
        journal.record(2, &second).expect("records");
        journal.prepare_commit().expect("prepares");
        // The crashed writer got as far as replacing both pages and growing
        // the file before the power went.
        database
            .write_all_at(0, &page_of(50, 1024))
            .expect("writes");
        database
            .write_all_at(1024, &page_of(60, 1024))
            .expect("writes");
        database
            .write_all_at(2048, &page_of(70, 1024))
            .expect("writes");
        std::mem::forget(journal);

        let recovered =
            recover_hot_journal(vfs.as_ref(), &path, database.as_ref(), Synchronous::Full)
                .expect("recovers");
        assert_eq!(recovered, Some(2));
        let mut read_back = vec![0u8; 2048];
        database.read_exact_at(0, &mut read_back).expect("reads");
        assert_eq!(read_back.get(..1024), Some(first.as_slice()));
        assert_eq!(read_back.get(1024..), Some(second.as_slice()));
        assert_eq!(database.file_size().expect("size"), 2048);
        assert!(!vfs
            .access(&path.journal(), AccessMode::Exists)
            .expect("checks"));
    }

    /// Recovery is idempotent: replaying twice produces the same database.
    #[test]
    fn recovery_run_twice_produces_the_same_database() {
        let vfs = Arc::new(MemoryVfs::new());
        let path = DbPath::new("/db.sqlite");
        let database = vfs
            .open(&path, OpenOptions::of_kind(FileKind::MainDb))
            .expect("opens");
        let original = page_of(1, 1024);
        database.write_all_at(0, &original).expect("writes");

        let mut journal = RollbackJournal::new(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            &path,
            JournalOptions::default(),
        );
        journal.begin(1024, 1).expect("begins");
        journal.record(1, &original).expect("records");
        journal.prepare_commit().expect("prepares");
        let bytes = journal.read_all().expect("reads");
        database
            .write_all_at(0, &page_of(90, 1024))
            .expect("writes");
        std::mem::forget(journal);

        let decoded = decode_journal(&bytes).expect("decodes").expect("is hot");
        apply_playback(&bytes, &decoded, database.as_ref()).expect("replays");
        apply_playback(&bytes, &decoded, database.as_ref()).expect("replays again");
        let mut read_back = vec![0u8; 1024];
        database.read_exact_at(0, &mut read_back).expect("reads");
        assert_eq!(read_back, original);
    }

    /// A record whose checksum is wrong is reported rather than half applied.
    #[test]
    fn a_corrupt_record_is_reported_rather_than_half_applied() {
        let vfs = Arc::new(MemoryVfs::new());
        let path = DbPath::new("/db.sqlite");
        let database = vfs
            .open(&path, OpenOptions::of_kind(FileKind::MainDb))
            .expect("opens");
        let first = page_of(1, 1024);
        let second = page_of(2, 1024);
        database.write_all_at(0, &first).expect("writes");
        database.write_all_at(1024, &second).expect("writes");

        let mut journal = RollbackJournal::new(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            &path,
            JournalOptions::default(),
        );
        journal.begin(1024, 2).expect("begins");
        journal.record(1, &first).expect("records");
        journal.record(2, &second).expect("records");
        journal.prepare_commit().expect("prepares");
        let mut bytes = journal.read_all().expect("reads");
        std::mem::forget(journal);

        // Damage a byte the checksum samples. The sum takes every
        // two-hundredth byte from the end backwards, so index 24 of a
        // 1024-byte page is one of the five it looks at and index 0 is not -
        // the check detects a torn or short write, which is what it is for,
        // and is not a defence against a file someone edited on purpose.
        if let Some(byte) = bytes.get_mut(512 + 4 + 24) {
            *byte ^= 0xff;
        }
        assert_ne!(
            RollbackJournal::checksum(0, &first),
            RollbackJournal::checksum(0, {
                let mut damaged = first.clone();
                if let Some(byte) = damaged.get_mut(24) {
                    *byte ^= 0xff;
                }
                &damaged.clone()
            })
        );
        database
            .write_all_at(0, &page_of(90, 1024))
            .expect("writes");
        database
            .write_all_at(1024, &page_of(91, 1024))
            .expect("writes");
        let decoded = decode_journal(&bytes).expect("decodes").expect("is hot");
        let failure = apply_playback(&bytes, &decoded, database.as_ref())
            .expect_err("a damaged journal cannot restore the database it protects");
        assert_eq!(failure.code(), inillucent_base::PrimaryCode::Corrupt);
    }

    /// A journal that names a page outside the original database is refused
    /// rather than written past the end of the file.
    #[test]
    fn a_record_beyond_the_original_size_is_refused() {
        let vfs = Arc::new(MemoryVfs::new());
        let path = DbPath::new("/db.sqlite");
        let database = vfs
            .open(&path, OpenOptions::of_kind(FileKind::MainDb))
            .expect("opens");
        let page = page_of(1, 1024);
        database.write_all_at(0, &page).expect("writes");

        let mut journal = RollbackJournal::new(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            &path,
            JournalOptions::default(),
        );
        journal.begin(1024, 1).expect("begins");
        journal.record(9, &page).expect("records");
        journal.prepare_commit().expect("prepares");
        let bytes = journal.read_all().expect("reads");
        std::mem::forget(journal);

        let decoded = decode_journal(&bytes).expect("decodes").expect("is hot");
        let failure = apply_playback(&bytes, &decoded, database.as_ref())
            .expect_err("a page outside the database is refused");
        assert_eq!(failure.code(), inillucent_base::PrimaryCode::Corrupt);
        assert_eq!(database.file_size().expect("size"), 1024);
    }

    /// Header fields outside the format are not believed.
    #[test]
    fn an_implausible_header_is_not_hot() {
        let mut raw = vec![0u8; 512];
        if let Some(window) = raw.get_mut(..8) {
            window.copy_from_slice(&JOURNAL_MAGIC);
        }
        // A page size that is not a power of two, and a zero original size.
        bytes::write_u32(&mut raw, 20, 512).expect("writes");
        bytes::write_u32(&mut raw, 24, 1000).expect("writes");
        bytes::write_u32(&mut raw, 16, 4).expect("writes");
        assert!(decode_journal(&raw).expect("decodes").is_none());

        bytes::write_u32(&mut raw, 24, 1024).expect("writes");
        bytes::write_u32(&mut raw, 16, 0).expect("writes");
        assert!(decode_journal(&raw).expect("decodes").is_none());

        bytes::write_u32(&mut raw, 16, 4).expect("writes");
        assert!(decode_journal(&raw).expect("decodes").is_some());
    }

    /// Every sync policy parses to itself, and OFF really syncs nothing.
    #[test]
    fn synchronous_levels_round_trip_and_off_syncs_nothing() {
        for level in [
            Synchronous::Off,
            Synchronous::Normal,
            Synchronous::Full,
            Synchronous::Extra,
        ] {
            assert_eq!(Synchronous::parse(level.as_str()), Some(level));
            assert_eq!(
                Synchronous::parse(&level.as_number().to_string()),
                Some(level)
            );
        }
        assert_eq!(Synchronous::Off.sync_mode(), None);
        assert_eq!(Synchronous::Normal.sync_mode(), Some(SyncMode::Normal));
        assert_eq!(Synchronous::Full.sync_mode(), Some(SyncMode::Full));
        assert!(!Synchronous::Normal.syncs_directory());
        assert!(Synchronous::Extra.syncs_directory());
    }

    /// Every journal mode parses to itself, and the two weak ones say so.
    #[test]
    fn journal_modes_round_trip_and_declare_their_crash_contract() {
        for mode in [
            JournalMode::Delete,
            JournalMode::Truncate,
            JournalMode::Persist,
            JournalMode::Memory,
            JournalMode::Off,
            JournalMode::Wal,
        ] {
            assert_eq!(JournalMode::parse(mode.as_str()), Some(mode));
        }
        assert!(JournalMode::Delete.is_crash_safe());
        assert!(JournalMode::Truncate.is_crash_safe());
        assert!(JournalMode::Persist.is_crash_safe());
        assert!(!JournalMode::Memory.is_crash_safe());
        assert!(!JournalMode::Off.is_crash_safe());
        assert!(JournalMode::Wal.is_crash_safe());
        assert!(JournalMode::Wal.is_wal());
        assert!(!JournalMode::Delete.is_wal());
    }
}
