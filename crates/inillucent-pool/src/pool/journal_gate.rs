//! What the pool may write, and when, given what the log and the journal know.
//!
//! Invariant: **a page never reaches the file before whatever can undo it
//! does.** Two different rules meet here: a page whose change is only in the log
//! may not be written before the log is durable, and a page whose change is not
//! committed may not be written at all unless a rollback journal holds its
//! pre-image.
//!
//! Split out of `pool.rs` in task-1946 (M12). The journal itself is
//! `crate::journal`; this is the gating that decides when to ask it.

use super::*;

impl Pool {
    /// Reports whether a page written before its transaction committed could be
    /// put back after a crash.
    ///
    /// Which is to say: whether a rollback journal whose pre-images reach the
    /// disk is in force. `wal` takes a `delete` journal rather than none, so
    /// this is true of every mode the engine ships with except `memory` and
    /// `off` - see `journal_for` in `crates/inillucent-engine/src/lib.rs`.
    pub(super) fn can_undo_a_steal(&self) -> bool {
        self.journal
            .borrow()
            .as_ref()
            .is_some_and(|journal| journal.mode().is_durable())
    }

    /// Refuses a writeback the log has not caught up with.
    ///
    /// Reads the LSN out of the page's own header rather than out of any
    /// bookkeeping beside it, because the header is what the file will hold and
    /// bookkeeping is what can drift from it. A page whose LSN is at or above
    /// the durable watermark describes a change whose log record is not on the
    /// media, and writing it would mean a crash could leave the data file ahead
    /// of the log with no way back.
    ///
    /// Reports whether a page holds a change no transaction has committed.
    ///
    /// **This is no-steal, and it is a condition rather than a convention.** The
    /// checkpointer's whole correctness argument is that an open transaction's
    /// pages are not in the file: recovery is redo-only, so a page written
    /// before its transaction committed can never be taken back out - replaying
    /// from an earlier point does not *undo* anything, it only re-applies.
    ///
    /// Until this existed the argument was written down and nothing enforced
    /// it. A checkpoint taken while a transaction was open wrote that
    /// transaction's dirty pages, the crash that followed rolled it back
    /// everywhere except the data file, and the row was still there afterwards.
    /// The model campaign found it on its second seed: "(0, 12) is there and
    /// should not be".
    ///
    /// A page above the watermark is **skipped**, not refused. A checkpoint
    /// with a writer open is an ordinary thing to do and has to succeed; what
    /// it must not do is advance the recovery point past the pages it skipped,
    /// which is why the caller sets `recovery_from` no higher than the oldest
    /// open transaction's first record.
    ///
    /// @param frame - the frame about to be written
    /// @param page - the page it holds
    pub(super) fn holds_uncommitted(&self, frame: u32, page: PageId) -> DbResult<bool> {
        let uncommitted = self.uncommitted_lsn.load(Ordering::SeqCst);
        if uncommitted == u64::MAX {
            return Ok(false);
        }
        let _ = page;
        let lsn = self.lsn_of(frame)?;
        Ok(lsn >= uncommitted)
    }

    /// Returns the LSN stamped on a frame's page.
    ///
    /// @param frame - the frame
    pub(super) fn lsn_of(&self, frame: u32) -> DbResult<u64> {
        let bytes = self
            .buffers
            .get(frame as usize)
            .ok_or_else(|| misuse("frame index out of range"))?
            .try_borrow()
            .map_err(|_| misuse("a frame chosen for writeback was mutably borrowed"))?;
        page::read_u64(&bytes, page::header::LSN)
    }

    /// Sets the LSN at or above which a page's change is uncommitted.
    ///
    /// `u64::MAX` means nothing is uncommitted, which is the state between
    /// transactions and the state of a database with no log at all.
    ///
    /// @param lsn - the open writer's first record, or `u64::MAX` for none
    pub fn set_uncommitted_lsn(&self, lsn: u64) {
        self.uncommitted_lsn.store(lsn, Ordering::SeqCst);
    }

    /// Returns how many writebacks no-steal has held back.
    pub fn held_back(&self) -> u64 {
        self.counters.held_back.get()
    }

    /// Returns the LSN at or above which a page's change is uncommitted.
    pub fn uncommitted_lsn(&self) -> u64 {
        self.uncommitted_lsn.load(Ordering::SeqCst)
    }

    /// Returns a handle to the watermark, for a caller that has to move it
    /// while the file is borrowed.
    ///
    /// The transaction manager takes one at assembly and keeps it. A
    /// transaction's first log record is written from inside the tree mutation
    /// that holds the file mutably, so reaching the pool through the file at
    /// that moment is not possible - and setting the watermark afterwards would
    /// leave a window in which an eviction could steal the page.
    pub fn uncommitted_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.uncommitted_lsn)
    }

    /// @param frame - the frame about to be written
    /// @param page - the page it holds, for the message
    pub(super) fn refuse_if_ahead_of_the_log(&self, frame: u32, page: PageId) -> DbResult<()> {
        let durable = self.durable_lsn.get();
        if durable == u64::MAX {
            return Ok(());
        }
        let lsn = {
            let bytes = self
                .buffers
                .get(frame as usize)
                .ok_or_else(|| misuse("frame index out of range"))?
                .try_borrow()
                .map_err(|_| misuse("a frame chosen for writeback was mutably borrowed"))?;
            page::read_u64(&bytes, page::header::LSN)?
        };
        if lsn <= durable {
            return Ok(());
        }
        // The log is behind. Ask it to catch up before refusing: a statement
        // that dirties more pages than the pool holds has to evict, and every
        // candidate it has carries an LSN the log has not reached, so refusing
        // outright fails a statement that has done nothing wrong.
        //
        // The handle is cloned out and the borrow dropped before the call,
        // so an `advance` that reaches back into the pool - to register a
        // different one, or to write a page of its own - does not find this
        // cell already borrowed. The comment here used to say that while the
        // borrow was held straight through the call (task-1932, M9).
        let advance = self.advance_log.borrow().as_ref().map(std::rc::Rc::clone);
        let Some(advance) = advance else {
            return Err(misuse(format!(
                "page {} carries lsn {lsn} and the log is durable to {durable}: writing it would put the data file ahead of the log",
                page.0
            )));
        };
        let reached = advance()?;
        self.durable_lsn.set(reached);
        if lsn <= reached {
            return Ok(());
        }
        Err(misuse(format!(
            "page {} carries lsn {lsn}, the log was asked to catch up and reached {reached}: writing it would put the data file ahead of the log",
            page.0
        )))
    }

    /// Saves one page's current contents to the rollback journal, reporting
    /// whether it saved anything.
    ///
    /// The answer is what tells `writeback` whether it owes a sync: a page
    /// already saved by this checkpoint's first pass needs neither the read
    /// below nor a second sync, and in write-ahead-log mode there is no
    /// journal and the answer is always no.
    ///
    /// Reads the page **from the file**, not from the pool: the pre-image the
    /// journal needs is what is durably there, and the frame holds the new
    /// version. A page beyond the end of the file has no pre-image, which is
    /// the right answer - restoring it would mean writing zeros over a page the
    /// transaction created.
    ///
    /// @param page - the page about to be overwritten
    pub(super) fn journal_page(&self, page: PageId) -> DbResult<bool> {
        let mut journal = self.journal.borrow_mut();
        let Some(journal) = journal.as_mut() else {
            return Ok(false);
        };
        if page.0 >= self.page_count.get() || !journal.wants(page) {
            return Ok(false);
        }
        // **A read that fails is a refusal, not an absence** - unless the page
        // is genuinely not in the file yet. This used to answer "no pre-image
        // needed" for *any* read error, and both callers then carried on and
        // overwrote the page, so a transient read error followed by a crash
        // left a modified page with nothing to put back. That is the one thing
        // the invariant at the top of `crate::journal` forbids.
        //
        // The page count above is not the test for "not in the file yet", and
        // using it as one is what made the first attempt at this refuse every
        // growing transaction: `page_count` is the pool's logical count, and it
        // runs ahead of the file whenever pages have been allocated but not yet
        // written. The file's own length is the answer. A page at or past it
        // has no pre-image because it has no image, and restoring it would mean
        // writing zeros over a page the transaction created.
        let offset = page.0.saturating_mul(self.page_size as u64);
        let length = self
            .file
            .file_size()
            .map_err(|error| error.into_db_error())?;
        if offset.saturating_add(self.page_size as u64) > length {
            return Ok(false);
        }
        let mut before = vec![0u8; self.page_size];
        self.file
            .read_exact_at(offset, &mut before)
            .map_err(|error| error.into_db_error())?;
        journal.save(page, &before)?;
        Ok(true)
    }

    /// Puts a rollback journal in force, or takes it out of force.
    ///
    /// @param journal - the journal, or nothing for the write-ahead log
    pub fn set_journal(&self, journal: Option<crate::journal::Journal>) {
        *self.journal.borrow_mut() = journal;
    }

    /// Syncs the journal, which must happen before the first page is written.
    pub fn seal_journal(&self) -> DbResult<()> {
        match self.journal.borrow().as_ref() {
            Some(journal) => journal.seal(),
            None => Ok(()),
        }
    }

    /// Disposes of the journal once the commit is durable.
    ///
    /// Clears the record of what evictions have stolen along with it: the
    /// pre-images are gone, so the next steal is the first one this journal
    /// has to outlive.
    pub fn finish_journal(&self) -> DbResult<()> {
        let outcome = match self.journal.borrow_mut().as_mut() {
            Some(journal) => journal.finish(),
            None => Ok(()),
        };
        if outcome.is_ok() {
            self.stolen.set(false);
        }
        outcome
    }
}
