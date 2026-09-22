//! Getting the pool's dirty pages into the file, and the meta record to describe
//! them.
//!
//! Invariant: **the file never describes pages it does not hold.** A fold writes
//! every dirty page, syncs, and only then writes a meta record naming the log
//! position those pages account for - and it writes that record twice, into the
//! shadow slot and then the primary, each with its own sync, so one of the two is
//! always intact.
//!
//! Split out of `pool.rs` in task-2006, which had grown 295 lines past its recorded
//! ceiling while design 1 of task-2000 turned a fold from something every commit
//! did into something that happens when the log fills up. The ratchet in
//! `policy.rs` asks for an extraction rather than a raised number, and this is a
//! coherent unit - how a page leaves the pool and how the file is made to account
//! for it - rather than a slice taken to make a number fit. Nothing changed in the
//! move: these are still `impl Pool`, reading the same private state, because a
//! child module can see its parent's private items.
//!
//! `translate_swips` stayed behind, because `writeback` on the eviction path wants
//! it too and it belongs to the write path rather than to the fold; it is
//! `pub(super)` now for this module's sake.

use super::*;

impl Pool {
    /// Hands every dirty page this pool's next flush will write to `each`, in
    /// the exact bytes that will reach the file.
    ///
    /// **The fold's after images, for the caller that owns the log** (task-2000,
    /// design 1a). The pool cannot append to the log - `inillucent-pool` is
    /// below `inillucent-wal` and depends on neither it nor anything above it,
    /// which is what the write-ahead rule's direction rests on - so the caller
    /// that holds both, `inillucent_txn::engine::log_dirty_page_images`, does
    /// the appending and this says what to append.
    ///
    /// **The bytes are the file's, not the frame's.** A resident frame holds
    /// swizzled swips, which are this process's own pointers; a recovery that
    /// installed those would install page numbers that mean nothing. The image
    /// handed over is therefore translated exactly as [`Pool::writeback`]
    /// translates it, through the same function, so the image in the log and the
    /// bytes in the file differ in nothing but the checksum - which recovery
    /// recomputes, because `put_image` restamps the LSN before it installs.
    ///
    /// **Only the pages the flush will actually write.** A page no-steal will
    /// hold back belongs to a transaction that has not committed, and an after
    /// image of one in the log would be replayed unconditionally by recovery -
    /// which is exactly the uncommitted page reaching the file that no-steal
    /// exists to prevent. The condition asked here is the one
    /// [`Pool::writeback`] asks.
    ///
    /// Returns how many images were handed over.
    ///
    /// @param each - called once per page, with its number and its file bytes
    pub fn each_fold_image(
        &self,
        mut each: impl FnMut(PageId, &[u8]) -> DbResult<()>,
    ) -> DbResult<usize> {
        let mut dirty: Vec<(PageId, u32)> = {
            let state = self.state.borrow();
            state
                .frames
                .iter()
                .enumerate()
                .filter(|(_, meta)| meta.dirty && meta.state != FrameState::Free)
                .map(|(index, meta)| (meta.page, index as u32))
                .collect()
        };
        dirty.sort_unstable();
        let mut handed = 0usize;
        // Its own buffer rather than `Pool::scratch`, because a fold's images
        // are appended before the flush runs and the flush is what uses the
        // scratch - one buffer for both would be one buffer two loops wanted at
        // once. One allocation for the whole pass, not one a page.
        let mut image = vec![0u8; self.page_size];
        for (page, frame) in dirty {
            if self.holds_uncommitted(frame, page)? {
                continue;
            }
            {
                let bytes = self
                    .buffers
                    .get(frame as usize)
                    .ok_or_else(|| misuse("frame index out of range"))?
                    .try_borrow()
                    .map_err(|_| misuse("a frame chosen for an after image was borrowed"))?;
                if image.len() != bytes.len() {
                    image.resize(bytes.len(), 0);
                }
                image.copy_from_slice(&bytes);
            }
            self.translate_swips(&mut image)?;
            each(page, &image)?;
            handed = handed.saturating_add(1);
        }
        Ok(handed)
    }

    /// Writes every dirty frame to the file.
    ///
    /// Pages go out in page-id order so the write pattern is sequential, which
    /// is the checkpointer's rule and costs nothing to honour here.
    ///
    /// Under a rollback journal it takes two passes over the same list: every
    /// pre-image first, then one sync, then the pages. The pre-images have to
    /// be on the media before the first page is overwritten, and doing it in
    /// two passes is what lets a batch of a thousand pages pay for one sync
    /// instead of a thousand. The writeback loop still asks for the sync per
    /// page, because the evictor reaches it without a flush around it; after
    /// this pass there is nothing left for it to sync.
    ///
    /// **One pass when the redo log carries the after images** (task-2000, design
    /// 1a). See [`Pool::fold_protected_by_log`]: the caller has already appended a
    /// whole page image of everything this pass will write and synced the log behind
    /// them, so there is nothing a pre-image would add and the read before write goes
    /// away with it.
    pub fn flush(&self) -> DbResult<usize> {
        let mut dirty: Vec<(PageId, u32)> = {
            let state = self.state.borrow();
            state
                .frames
                .iter()
                .enumerate()
                .filter(|(_, meta)| meta.dirty && meta.state != FrameState::Free)
                .map(|(index, meta)| (meta.page, index as u32))
                .collect()
        };
        dirty.sort_unstable();
        // **No pre images when the redo log carries the after images**
        // (task-2000, design 1a). See `Pool::fold_protected_by_log`.
        if self.journal.borrow().is_some() && !self.fold_protected_by_log.get() {
            for (page, _) in &dirty {
                self.journal_page(*page)?;
            }
            self.seal_journal()?;
        }
        for (page, frame) in &dirty {
            self.writeback(*frame, *page, Writing::Checkpoint)?;
        }
        Ok(dirty.len())
    }

    /// Writes every dirty frame, then the meta page and its shadow, then syncs.
    ///
    /// The order is the durability order: data first, then the record that says
    /// the data is there. A crash between them leaves the previous meta page
    /// describing a file whose pages are a superset of what it claims, which is
    /// exactly what a checkpoint is allowed to leave behind.
    ///
    /// The record is taken mutably because one of its fields is only knowable
    /// **after** the flush: the high water is the highest stamp any page in the
    /// file carries, and the pages this checkpoint is about to write are part
    /// of the file it describes. Setting it before the flush would leave the
    /// meta page one checkpoint behind the stamps it is meant to bound, which
    /// is the state the next open resumes the log above.
    ///
    /// @param meta - the record to write, with its generation already bumped
    pub fn checkpoint(&self, meta: &mut Meta) -> DbResult<()> {
        // **The journal is sealed before the first page moves**, and `flush`
        // is where that happens: it saves every pre-image the batch needs and
        // syncs once before it writes anything. This call used to be the only
        // one, and it ran here - before `flush` had saved a single pre-image -
        // so it synced an empty file and the ordering a rollback journal exists
        // to forbid held anyway. It is kept because anything a caller saved
        // before reaching a checkpoint is still owed a sync, and it costs
        // nothing when there is none.
        self.seal_journal()?;
        self.flush()?;
        // Every page this checkpoint wrote has now raised the high water, so
        // the number recorded here bounds the stamps the file actually holds
        // rather than the ones it held a checkpoint ago. It never goes
        // backwards: a run that writes no stamped page keeps what it read.
        meta.high_water_lsn = meta.high_water_lsn.max(self.high_water_lsn.get());
        let mut image = vec![0u8; self.page_size];
        meta.encode(&mut image)?;
        // **The meta pages are journaled too, and they were the last pages that
        // were not.** A rollback journal has to hold a pre-image of every page
        // the checkpoint overwrites, and these two are pages the checkpoint
        // overwrites. Leaving them out left a crash here able to produce a file
        // whose data pages the journal put back to before the checkpoint and
        // whose meta record says the checkpoint finished: the recorded
        // `checkpoint_lsn` then tells redo that everything up to it is already
        // in the file, so the records that would have re-applied the pages the
        // journal just undid are skipped, and the database comes back as
        // neither its old self nor its new one. It came back with no tables at
        // all, because the catalog's own page is one of the pages the journal
        // put back.
        //
        // The shadow page does not cover this **under a journal**. Both slots take
        // the same image, so the second one is not an older copy to fall back on -
        // it is a second chance for the *new* record to survive, and `Meta::choose`
        // believing either of them is the failure. What makes the checkpoint
        // undoable there is the previous record being on the disk in the journal,
        // which is the same thing that makes every other page undoable.
        //
        // Without a journal the same two slots are what protects the record, and the
        // sync between them is what makes them two states rather than one - which is
        // the next paragraph.
        //
        // **The shadow slot goes first and takes the pages' own sync with it; the
        // primary follows and takes the second.** Two syncs of the data file, and
        // one complete meta record on the disk at every instant.
        //
        // **Why the order is what it is** (task-2000, design 1a). Without a
        // rollback journal, nothing else can put the old meta record back, so the
        // two slots may not be written into one unsynced batch: a crash there can
        // tear both, and then neither decodes and the file does not open at all.
        // Splitting the sync between them closes it:
        //
        // - a crash before the first sync leaves the primary holding the previous
        //   record, whole, whatever the shadow's bytes look like. Recovery starts
        //   from that record's own `checkpoint_lsn` and the after images the fold
        //   appended before it wrote anything repair every page it had reached.
        // - a crash between the syncs leaves the shadow holding this record,
        //   whole, and `Meta::choose` takes it because its generation is higher.
        //   The first sync covered the pages **and** the shadow together, so a
        //   shadow that is durable cannot describe a page that is not.
        // - a crash after the second leaves both, identical.
        //
        // **Writing to one slot a fold, alternating by generation, was tried first
        // and is wrong with two processes.** It looks like the arrangement the two
        // slots were shaped for, and it saves a write: the new record goes to the
        // slot not holding the current one, so a torn write always leaves the other
        // intact. What it misses is that each connection bumps *its own* copy of
        // the generation. Two processes sharing a file can therefore write records
        // of the same generation to the same slot, or leave one process's newer
        // record in the slot `Meta::choose` does not pick - and then that process's
        // fold is invisible: `checkpoint_lsn`, `page_count` and `free_map` all stay
        // at the other process's older record, and `the_meta_moved` answers no
        // because `choose` returns what the asking connection already had. Measured
        // on two processes each inserting sixty rows into one `ATTACH`ed file: a
        // hundred and twenty acknowledged, **one** in the file, `ANALYZE`
        // afterwards reporting the table empty, and every process exit zero. Both
        // slots taking the same image is what makes a meta record a fact about the
        // file rather than about the connection that wrote it.
        //
        // Under a rollback journal the pre-images are what makes the write
        // undoable, so both slots are journaled first and written together, exactly
        // as before - that is the path `journal_mode = delete` is measured on.
        match self.fold_protected_by_log.get() {
            // The redo log protects the fold, so the split sync above is what
            // keeps one record whole. Three writes and two syncs of the data
            // file: the pages, the shadow slot, one sync; the primary slot, one
            // sync.
            true => {
                self.write_slot_and_sync(SHADOW_PAGE, &image)?;
                self.write_slot_and_sync(META_PAGE, &image)?;
            }
            // Under a rollback journal the pre-images are what makes the write
            // undoable, so the order `journal_mode = delete` was measured on is
            // kept exactly: the pages synced on their own, both slots journaled
            // and sealed, both written, one sync behind them.
            false => {
                self.file
                    .sync(SyncMode::Normal)
                    .map_err(|error| error.into_db_error())?;
                Counters::add(&self.counters.file_syncs, 1);
                self.journal_page(META_PAGE)?;
                self.journal_page(SHADOW_PAGE)?;
                self.seal_journal()?;
                for slot in [META_PAGE, SHADOW_PAGE] {
                    self.file
                        .write_all_at(slot.0.saturating_mul(self.page_size as u64), &image)
                        .map_err(|error| error.into_db_error())?;
                }
                self.file
                    .sync(SyncMode::Normal)
                    .map_err(|error| error.into_db_error())?;
                Counters::add(&self.counters.file_syncs, 1);
            }
        }
        Counters::add(&self.counters.folds, 1);
        Counters::add(&self.counters.writes, 2);
        // **And disposed of after the meta record is durable**, which is the
        // moment the commit exists. A journal removed a line earlier would
        // leave a crash with a file it could neither trust nor repair.
        //
        // **Unless an eviction has already put an open transaction's page in
        // the file**, in which case the pre-images in this journal are the only
        // way back from that page and the journal outlives the checkpoint. The
        // journal restores the meta record too, so keeping it does not leave a
        // half-undone file: a crash puts the data pages, the meta page and its
        // shadow all back to what they were before this checkpoint, and the log
        // replays forward from the recovery point that older meta record names.
        // The next checkpoint with no writer open disposes of it.
        if self.stolen.get() && self.uncommitted_lsn.load(Ordering::SeqCst) != u64::MAX {
            return Ok(());
        }
        self.finish_journal()?;
        Ok(())
    }

    /// Writes one meta slot and syncs the data file behind it.
    ///
    /// The half of [`Pool::checkpoint`]'s meta write that happens twice - see that
    /// function for why the two slots take a sync each rather than sharing one, and
    /// for why writing to only one of them is wrong.
    ///
    /// @param slot - `META_PAGE` or `SHADOW_PAGE`
    /// @param image - the encoded record, one page long
    fn write_slot_and_sync(&self, slot: PageId, image: &[u8]) -> DbResult<()> {
        self.file
            .write_all_at(slot.0.saturating_mul(self.page_size as u64), image)
            .map_err(|error| error.into_db_error())?;
        self.file
            .sync(SyncMode::Normal)
            .map_err(|error| error.into_db_error())?;
        Counters::add(&self.counters.file_syncs, 1);
        Ok(())
    }

    /// Reads the two meta slots straight from the file.
    ///
    /// **Past the pool, deliberately.** A connection asking whether another
    /// process has committed cannot ask its own cache: the whole question is
    /// whether the cache is stale.
    ///
    /// @param page_size - how big a page is
    pub fn read_meta_slots(&self, page_size: usize) -> DbResult<(Vec<u8>, Vec<u8>)> {
        let mut primary = vec![0u8; page_size];
        let mut shadow = vec![0u8; page_size];
        self.file
            .read_exact_at(0, &mut primary)
            .map_err(|error| error.into_db_error())?;
        self.file
            .read_exact_at(page_size as u64, &mut shadow)
            .map_err(|error| error.into_db_error())?;
        Counters::add(&self.counters.meta_reads, 1);
        Ok((primary, shadow))
    }

    /// Reads the bytes a meta record occupies from both slots, without the page
    /// around them.
    ///
    /// **The cheap half of the staleness check the multi-process protocol makes
    /// on every statement** (task-2046). A record ends at
    /// [`crate::meta::META_RECORD_BYTES`] and [`crate::meta::Meta::encode`]
    /// zeroes the rest of the page, so two slots whose record bytes agree
    /// describe the same database - and a connection asking whether another
    /// process has folded can answer from 232 bytes rather than from two whole
    /// pages. At the 32 KiB default page size [`Pool::read_meta_slots`] costs
    /// a buffer one page long allocated and zeroed for each slot, a whole page
    /// read into each and, through
    /// `Meta::choose`, two crc32 passes over a whole page; this costs two
    /// reads of 116 bytes into the stack.
    ///
    /// It decides nothing on its own. A caller that finds the bytes changed
    /// reads the slots in full and checksums them, which is the only path that
    /// says what the record now is.
    ///
    /// @param page_size - the page size, which is where the second slot starts
    pub fn read_meta_records(
        &self,
        page_size: usize,
    ) -> DbResult<[[u8; crate::meta::META_RECORD_BYTES]; 2]> {
        let mut records = [[0u8; crate::meta::META_RECORD_BYTES]; 2];
        let (primary, rest) = records.split_at_mut(1);
        let Some(primary) = primary.first_mut() else {
            return Err(misuse("the meta record buffer has no primary slot"));
        };
        let Some(shadow) = rest.first_mut() else {
            return Err(misuse("the meta record buffer has no shadow slot"));
        };
        self.file
            .read_exact_at(0, primary)
            .map_err(|error| error.into_db_error())?;
        self.file
            .read_exact_at(page_size as u64, shadow)
            .map_err(|error| error.into_db_error())?;
        Counters::add(&self.counters.meta_probes, 1);
        Ok(records)
    }

    /// Writes one of the two meta pages straight to the file.
    ///
    /// The meta pages are not pool pages: they carry no common header, they are
    /// written twice, and one of them has to be readable before the pool's own
    /// page size is known. Keeping them off the page table is what stops an
    /// eviction from ever choosing one.
    ///
    /// @param page - [`META_PAGE`] or [`SHADOW_PAGE`]
    /// @param image - the encoded meta record, one page long
    pub fn write_meta_slot(&self, page: PageId, image: &[u8]) -> DbResult<()> {
        if page != META_PAGE && page != SHADOW_PAGE {
            return Err(misuse(format!("page {} is not a meta page", page.0)));
        }
        self.file
            .write_all_at(page.0.saturating_mul(self.page_size as u64), image)
            .map_err(|error| error.into_db_error())?;
        Counters::add(&self.counters.writes, 1);
        Ok(())
    }

    /// Writes one page the bulk loader built straight into the data file, with
    /// no pool frame behind it.
    ///
    /// **Design 2 of task-2000: a bulk built page is written once.**
    /// `PagedTree::bulk_build_rows` used to log a whole page `WritePage` image per
    /// page and hand the page to [`Pool::install`], which marks the frame dirty -
    /// so the next fold wrote every page a second time, and the frames stayed
    /// resident until it did. For `CREATE INDEX` over 100,000 text values that was
    /// 6.2 MB logged, 6.2 MB written, then 6.2 MB written again, with `pack` at
    /// 11.4 ms of a 26 ms statement and 190 frames of the pool holding the index
    /// while it happened.
    ///
    /// **What replaces the log record is the commit order**, and it is the
    /// caller's to keep: every built page written, then [`Pool::sync_data_file`],
    /// then the statement's own records appended and synced. A crash before the
    /// log sync leaves a catalog that never named the root and `AllocPage` records
    /// that are not in the log, so the pages are still free; a crash after leaves
    /// pages that were durable first. A torn page cannot exist at the commit
    /// point, because the file sync preceded it. See
    /// `inillucent_wal::record::Body::BulkBuilt`.
    ///
    /// **The checksum is computed here** because nothing else will: a page that
    /// goes through [`Pool::writeback`] is checksummed on the way out, and this
    /// page does not go through it.
    ///
    /// **And so is the stamp, which used to be left at zero and corrupted a
    /// database (task-2055).** The argument for leaving it was that no record
    /// names a built page, so the page-LSN rule has nothing to order it against.
    /// That is true of records this build writes and false of records the page's
    /// *previous life* wrote: a page the free map hands back was a leaf of
    /// another tree until the statement that dropped it, and every `WritePage`
    /// and `InsertRow` that filled it stays in the log until a checkpoint moves
    /// past them. Recovery
    /// replays a record only onto a page whose stamp is below the record's, and a
    /// stamp of zero is below every record there is - so redo put the old tree's
    /// bytes back over the index that had been built on those pages, and the
    /// reopened file answered `a key below separator 0 is in the child above it`.
    ///
    /// The stamp is the `AllocPage` record's own LSN, which is the same thing
    /// `PagedTree`'s ordinary allocation path writes from its `WritePage`
    /// record's LSN: it is above every record describing what the page used to
    /// hold, because the allocation is what ended that life, and it is a real
    /// position in the log rather than a number invented here.
    ///
    /// It is noted as the pool's high water as well, because nothing else can:
    /// [`Pool::writeback`] raises the high water for every page *it* writes, and
    /// this page never goes through it. `resume_above_every_stamp` reads that
    /// number to decide where a recovered log has to restart, and a file holding
    /// a stamp no part of the pool had seen is exactly the case it exists for.
    ///
    /// **A resident frame for the same page number is kept in step.** A freshly
    /// allocated page has no frame, which is the ordinary case and the one the
    /// memory win comes from - the built pages never enter the pool at all, so a
    /// `CREATE INDEX` no longer holds its own index in a hundred and ninety frames
    /// while it waits for a fold. A page that was freed by a `DROP` and handed out
    /// again can still have a frame, holding the old tree's bytes, and a later fetch
    /// would read that instead of what was just written. Such a frame is overwritten
    /// with the new image and marked **clean**, because the file now holds exactly
    /// those bytes.
    ///
    /// **Evicting it instead was tried and is wrong.** A frame can be pinned by the
    /// build's own descent or queued behind frames that will not go, and an eviction
    /// that could not complete had to refuse the statement - `CREATE TABLE` failed
    /// with "page 4's frame could not be released before a bulk build wrote it" on
    /// `prepared_schema.rs`. Overwriting the frame cannot fail and cannot disagree
    /// with the file.
    ///
    /// @param page - the page, already allocated
    /// @param image - the page bytes, stamped and checksummed in place
    /// @param lsn - the log position the page's contents are as of, which is its
    ///   `AllocPage` record's LSN; zero for an unlogged build, where there is no
    ///   log to replay and the bytes stay what the unlogged builder always wrote
    pub fn write_built_page(&self, page: PageId, image: &mut [u8], lsn: u64) -> DbResult<()> {
        if image.len() != self.page_size {
            return Err(misuse(format!(
                "a built page image is {} bytes, not {}",
                image.len(),
                self.page_size
            )));
        }
        if lsn > 0 {
            page::set_lsn(image, lsn)?;
            self.note_high_water_lsn(lsn);
        }
        page::checksum_page(image)?;
        self.file
            .write_all_at(page.0.saturating_mul(self.page_size as u64), image)
            .map_err(|error| error.into_db_error())?;
        Counters::add(&self.counters.writes, 1);
        if page.0 >= self.page_count.get() {
            self.page_count.set(page.0.saturating_add(1));
        }
        // Only when a frame already holds this page number. `install` would claim
        // one otherwise, which is the residency this design exists to avoid.
        if self.lookup(page).is_some() {
            self.install(page, image)?;
            self.mark_clean(page);
        }
        Ok(())
    }

    /// Clears one resident page's dirty flag, because the file now holds it.
    ///
    /// Only [`Pool::write_built_page`] may say this, and only about a page it has
    /// just written itself. Anything else clearing a dirty flag is a change dropped
    /// on the floor.
    ///
    /// @param page - the page whose frame is now in step with the file
    fn mark_clean(&self, page: PageId) {
        let Some(frame) = self.lookup(page) else {
            return;
        };
        let mut state = self.state.borrow_mut();
        state.amend(frame, |meta| {
            meta.dirty = false;
            meta.rec_lsn = u64::MAX;
        });
    }

    /// Syncs the data file, and nothing else.
    ///
    /// **The half of a fold a bulk build needs on its own** (task-2000, design 2).
    /// The build's pages have to be on the media before the statement's own log
    /// records are, which is the reverse of the write-ahead rule and is sound for
    /// exactly the reason [`Pool::write_built_page`] gives: the log never
    /// describes those pages at all, so there is no record for the file to get
    /// ahead of.
    pub fn sync_data_file(&self) -> DbResult<()> {
        self.file
            .sync(SyncMode::Normal)
            .map_err(|error| error.into_db_error())?;
        Counters::add(&self.counters.file_syncs, 1);
        Ok(())
    }
}
