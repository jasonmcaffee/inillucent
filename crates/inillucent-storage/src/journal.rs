//! The hook a pager calls to make its commit crash-atomic.
//!
//! Invariant: storage declares *when* a journal is needed and knows nothing
//! about what one looks like. The pager knows the two facts a journal exists
//! to act on - this page is about to change for the first time in this
//! transaction, and the database file is now consistent - and calls them out
//! at exactly those points. The format, the journal modes, the synchronous
//! policy and hot-journal recovery all live in `inillucent-transaction`, which is
//! the crate the TDD's module map puts them in.
//!
//! The alternative was to move the whole commit out of the pager. That would
//! have moved the dirty set, the page cache and the lock ladder with it, and
//! the pager would have been left as a byte-shuffling helper with no state
//! machine - which is the opposite of what the TDD describes, where the pager
//! owns `WriterLocked`, `WriterCacheMod` and `WriterDbMod` and journals the
//! before image inside `get_page_mut`. So the ownership split is by knowledge:
//! the pager knows the moments, the transaction layer knows the file.

use inillucent_base::DbResult;
use inillucent_vfs::{SyncMode, VfsFile};

/// What one transaction cost, in journal terms.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JournalStats {
    /// Page images written to the journal.
    pub records: u64,
    /// Bytes written to the journal.
    pub bytes_written: u64,
    /// Times the journal was synced.
    pub syncs: u64,
    /// Journals created.
    pub created: u64,
    /// Journals finalised, whether by commit or by rollback.
    pub finalized: u64,
    /// Page images replayed onto the database by a rollback or a recovery.
    pub played_back: u64,
}

impl JournalStats {
    /// Adds another transaction's numbers to these.
    pub fn add(&mut self, other: JournalStats) {
        self.records = self.records.saturating_add(other.records);
        self.bytes_written = self.bytes_written.saturating_add(other.bytes_written);
        self.syncs = self.syncs.saturating_add(other.syncs);
        self.created = self.created.saturating_add(other.created);
        self.finalized = self.finalized.saturating_add(other.finalized);
        self.played_back = self.played_back.saturating_add(other.played_back);
    }
}

/// The rollback protocol, from the pager's side.
///
/// Every method is called from one named step of the pager's commit, and the
/// order they are called in is the order the TDD's commit sequence lists. An
/// implementation that does nothing is a legal one - it is what journal mode
/// `OFF` is - and the pager's behaviour with no journal attached at all is the
/// same as with that one, which is why the phase-4 tests still pass unchanged.
pub trait Journal: std::fmt::Debug + Send {
    /// Starts a transaction over a database of `original_page_count` pages.
    fn begin(&mut self, page_size: u32, original_page_count: u32) -> DbResult<()>;

    /// Records a page's pre-transaction image, at most once per transaction.
    ///
    /// Called before the page is modified, so `image` is what a rollback has
    /// to put back. A page the transaction created has no pre-transaction
    /// image and is never passed here; the recorded original page count is
    /// what removes it again.
    fn record(&mut self, page: u32, image: &[u8]) -> DbResult<()>;

    /// Makes the journal durable, which is what licenses the first database
    /// write. Steps 4 and 5 of the commit sequence.
    fn prepare_commit(&mut self) -> DbResult<()>;

    /// Returns how hard the database file must be synced before the commit
    /// point, or `None` when the durability mode asks for no sync at all.
    fn database_sync(&self) -> Option<SyncMode>;

    /// Makes the journal non-hot, which is the atomic commit point. Step 9.
    fn commit_point(&mut self) -> DbResult<()>;

    /// Replays the journal onto the database and returns the page count the
    /// database had before the transaction started.
    ///
    /// Called when a transaction is abandoned after the database file has been
    /// modified, and by hot-journal recovery, which is the same operation
    /// performed by a different connection.
    fn playback(&mut self, database: &dyn VfsFile) -> DbResult<Option<u32>>;

    /// Finalises the journal after a rollback, leaving nothing hot behind.
    fn discard(&mut self) -> DbResult<()>;

    /// Reports whether a journal file with records in it currently exists.
    fn is_active(&self) -> bool;

    /// Returns the path this journal is written to, when it has one.
    ///
    /// A commit across several databases lists every journal in one file, so
    /// it has to be able to name them. A journal that lives in memory has no
    /// name and answers `None`, which is also why such a mode cannot take part
    /// in a multi-database commit.
    fn path(&self) -> Option<inillucent_vfs::DbPath> {
        None
    }

    /// Names the super-journal whose existence decides this transaction.
    ///
    /// A journal that names one is replayed only while that file is there. It
    /// is how several databases commit together: the deletion of that one file
    /// is the moment every one of them has committed, and until it happens
    /// every one of them rolls back.
    ///
    /// The default does nothing, which is right for a journal that cannot take
    /// part - and a caller that needs the guarantee asks for the path first.
    fn set_super_journal(&mut self, path: Option<inillucent_vfs::DbPath>) {
        let _ = path;
    }

    /// Returns the running totals.
    fn stats(&self) -> JournalStats;
}
