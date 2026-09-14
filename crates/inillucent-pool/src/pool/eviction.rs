//! Choosing a frame to reuse, and what has to happen before it can be.
//!
//! Invariant: **a frame is only taken from a page that can be read back.** A
//! clean page can go at once; a dirty one is written first; an uncommitted one
//! is written only when something can undo the steal, and otherwise keeps its
//! frame.
//!
//! Split out of `pool.rs` in task-1946 (M12), which held this, the frame table,
//! the journal's sync gating and the swip logic in one 2,728 line file. Nothing
//! moved but the text: these are still `impl Pool`, reading the same private
//! state, because a child module can see its parent's private items.

use super::*;

impl Pool {
    /// Returns a frame holding nothing, cooling and evicting to get one.
    ///
    /// The one place a frame is handed out for a page it does not yet hold, and
    /// therefore the one place its buffer has to exist by. Every caller - the
    /// read path through `fill_frame`, and `install` for a page built in memory
    /// - comes through here.
    pub(super) fn claim_frame(&self) -> DbResult<u32> {
        let frame = self.take_frame()?;
        self.give_the_frame_a_buffer(frame)?;
        Ok(frame)
    }

    /// Returns a free frame's index, cooling and evicting to get one.
    ///
    /// **The budget is consulted before the free list.** `PRAGMA cache_size` is
    /// a ceiling on how many database pages are held in memory - that is
    /// SQLite's own definition of it - so a pool asked for a smaller cache
    /// stops taking fresh frames once that many are resident and re-uses one
    /// instead. The allocated buffers stay allocated, which is what SQLite's
    /// page cache does too; what the setting bounds is the pages, and that is
    /// what this bounds.
    ///
    /// With no `cache_size` set the budget is the whole pool, so the first
    /// branch is the only one taken until the pool is full - the same path, and
    /// the same cost, as before the budget existed.
    pub(super) fn take_frame(&self) -> DbResult<u32> {
        let budget = self.budget.get();
        {
            let mut state = self.state.borrow_mut();
            let resident = self.buffers.len().saturating_sub(state.free.len());
            if resident < budget {
                if let Some(frame) = state.free.pop() {
                    return Ok(frame);
                }
            }
        }
        self.cool()?;
        if let Some(frame) = self.evict_one()? {
            return Ok(frame);
        }
        // Over budget with nothing evictable - every resident page is pinned.
        // The budget is a ceiling on caching, not a wall the statement runs
        // into, so a frame the pool owns and is not using is better than a
        // refusal.
        if let Some(frame) = self.state.borrow_mut().free.pop() {
            return Ok(frame);
        }
        // Nothing is evictable, and the two reasons for that are different
        // enough to the caller that they are told apart. A pool whose frames
        // are all pinned is a caller holding too many guards at once. A pool
        // whose frames all hold pages an open transaction has changed, with no
        // rollback journal to undo an eviction from, is the documented limit of
        // a no-steal policy: such a page may not reach the file before its
        // commit, and it may not be dropped either, so the transaction cannot
        // dirty more pages than the pool holds. Selecting a journal mode that
        // keeps pre-images, or a larger `PRAGMA cache_size`, is what lifts it.
        let held = self.frames_no_steal_is_holding();
        if held > 0 {
            return Err(no_mem(format!(
                "the open transaction has changed {held} of the buffer pool's {} pages, and no \
                 rollback journal is in force to undo an eviction from, so none of them may \
                 be written before it commits: a transaction cannot dirty more pages than \
                 the pool holds",
                self.buffers.len()
            )));
        }
        Err(no_mem(
            "every frame in the buffer pool is pinned; nothing can be evicted",
        ))
    }

    /// Returns how many resident frames hold a page no-steal will not let go.
    ///
    /// Only asked when the pool has nothing to give, so that the refusal names
    /// the reason it is refusing rather than the first reason anybody wrote a
    /// message for.
    pub(super) fn frames_no_steal_is_holding(&self) -> usize {
        if self.can_undo_a_steal() {
            return 0;
        }
        let uncommitted = self.uncommitted_lsn.load(Ordering::SeqCst);
        if uncommitted == u64::MAX {
            return 0;
        }
        let dirty: Vec<u32> = {
            let state = self.state.borrow();
            state
                .frames
                .iter()
                .enumerate()
                .filter(|(_, meta)| meta.dirty && meta.state != FrameState::Free)
                .map(|(index, _)| index as u32)
                .collect()
        };
        dirty
            .into_iter()
            .filter(|frame| self.lsn_of(*frame).is_ok_and(|lsn| lsn >= uncommitted))
            .count()
    }

    /// Makes sure a claimed frame has a page-sized buffer.
    ///
    /// Costs a length check on every claim and an allocation on the first one
    /// per frame; a pool that has been full once never allocates again, because
    /// an evicted frame keeps its buffer.
    ///
    /// @param frame - the frame just claimed
    pub(super) fn give_the_frame_a_buffer(&self, frame: u32) -> DbResult<()> {
        let cell = self
            .buffers
            .get(frame as usize)
            .ok_or_else(|| misuse("frame index out of range"))?;
        let mut bytes = cell
            .try_borrow_mut()
            .map_err(|_| misuse("a frame chosen for a new page was still borrowed"))?;
        if bytes.len() == self.page_size {
            return Ok(());
        }
        // `try_reserve` rather than `resize`, so a pool that cannot grow says
        // so as an error instead of aborting the process.
        let wanted = self.page_size.saturating_sub(bytes.len());
        bytes
            .try_reserve_exact(wanted)
            .map_err(|_| no_mem(format!("a frame of {} bytes", self.page_size)))?;
        bytes.resize(self.page_size, 0);
        Ok(())
    }

    /// Moves a share of the pool into the cooling FIFO.
    ///
    /// Sampling random hot frames rather than scanning is LeanStore's clock:
    /// the sweep costs what it cools rather than what the pool holds, and a
    /// frame that keeps being used keeps being rewarmed out of the queue before
    /// it reaches the front.
    ///
    /// Returns how many frames it moved.
    pub fn cool(&self) -> DbResult<usize> {
        let total = self.buffers.len();
        let want = ((total as f64) * COOL_FRACTION).ceil() as usize;
        let want = want.max(1);
        let mut moved = 0usize;
        let mut attempts = 0usize;
        let budget = want.saturating_mul(SAMPLE_FACTOR).max(total.min(64));
        while moved < want && attempts < budget {
            attempts = attempts.saturating_add(1);
            let candidate = {
                let mut state = self.state.borrow_mut();
                if state.cooling.len() >= want {
                    break;
                }
                let pick = (state.clock.next_u64() as usize) % total.max(1);
                match state.frames.get(pick) {
                    Some(meta)
                        if meta.state == FrameState::Hot
                            && self.pins_of(pick as u32) == 0
                            && !self.parent_is_pinned(meta.parent) =>
                    {
                        Some(pick as u32)
                    }
                    _ => None,
                }
            };
            let Some(frame) = candidate else {
                continue;
            };
            if self.unswizzle_from_parent(frame)? {
                let mut state = self.state.borrow_mut();
                if let Some(meta) = state.frames.get_mut(frame as usize) {
                    meta.state = FrameState::Cooling;
                    meta.parent = None;
                }
                state.cooling.push_back(frame);
                moved = moved.saturating_add(1);
            }
        }
        if moved == 0 {
            // Sampling is a policy, not a guarantee. In a pool of a few frames
            // a random walk can miss the one evictable frame there is, and the
            // caller's next step is to report that the pool is full - which is
            // wrong when it is not. So a sweep that found nothing falls back to
            // a scan, and the pool reports "everything is pinned" only when
            // everything really is.
            //
            // The eight-frame campaign is what found this: a descent pinned two
            // frames, six were coolable, and the clock sampled its budget away
            // without touching one of them.
            for frame in 0..total {
                let candidate = {
                    let state = self.state.borrow();
                    match state.frames.get(frame) {
                        Some(meta)
                            if meta.state == FrameState::Hot
                                && self.pins_of(frame as u32) == 0
                                && !self.parent_is_pinned(meta.parent) =>
                        {
                            Some(frame as u32)
                        }
                        _ => None,
                    }
                };
                let Some(frame) = candidate else {
                    continue;
                };
                if self.unswizzle_from_parent(frame)? {
                    let mut state = self.state.borrow_mut();
                    if let Some(meta) = state.frames.get_mut(frame as usize) {
                        meta.state = FrameState::Cooling;
                        meta.parent = None;
                    }
                    state.cooling.push_back(frame);
                    moved = moved.saturating_add(1);
                    break;
                }
            }
        }
        Counters::add(&self.counters.cooled, moved as u64);
        Ok(moved)
    }

    /// Evicts the coldest frame in the FIFO, writing it back if it is dirty.
    ///
    /// Returns the freed frame, or `None` when nothing in the queue could go.
    pub fn evict_one(&self) -> DbResult<Option<u32>> {
        loop {
            let frame = {
                let mut state = self.state.borrow_mut();
                match state.cooling.pop_front() {
                    Some(frame) => frame,
                    None => return Ok(None),
                }
            };
            let (page, dirty, pins) = {
                let state = self.state.borrow();
                match state.frames.get(frame as usize) {
                    Some(meta) => (meta.page, meta.dirty, self.pins_of(frame)),
                    None => continue,
                }
            };
            if pins > 0 {
                // Somebody pinned it after it was queued. Put it back to hot
                // rather than evicting under a live reader.
                let mut state = self.state.borrow_mut();
                if let Some(meta) = state.frames.get_mut(frame as usize) {
                    meta.state = FrameState::Hot;
                }
                continue;
            }
            if dirty && !self.writeback(frame, page, Writing::Eviction)? {
                // No-steal held the page back and there is no durable rollback
                // journal to undo a steal with, so this frame cannot be freed:
                // the frame holds the only copy of the page, and emptying it
                // would lose the change rather than defer it. Back to hot, and
                // on to the next candidate. A pool with nothing else to give
                // then fails the statement in `take_frame`, which is the
                // documented limit of a no-steal policy - a transaction can
                // dirty at most the pool - and is an error rather than a file
                // with a hole in it.
                let mut state = self.state.borrow_mut();
                if let Some(meta) = state.frames.get_mut(frame as usize) {
                    meta.state = FrameState::Hot;
                }
                continue;
            }
            // TDD invariant 5: a frame is never reused while a swizzled swip
            // still names it. The only thing that can name it is the parent
            // recorded on the way in, and cooling put a page id back there.
            debug_assert!(
                self.state
                    .borrow()
                    .frames
                    .get(frame as usize)
                    .map(|meta| meta.parent.is_none())
                    .unwrap_or(true),
                "a frame was evicted with a parent still pointing at it"
            );
            let mut state = self.state.borrow_mut();
            state.table.remove(&page);
            if let Some(meta) = state.frames.get_mut(frame as usize) {
                *meta = FrameMeta::empty();
            }
            drop(state);
            // The frame no longer holds the page a descent may have observed.
            if let Some(latch) = self.latch(frame) {
                if latch.try_exclusive() {
                    latch.release_exclusive();
                }
            }
            Counters::add(&self.counters.evicted, 1);
            return Ok(Some(frame));
        }
    }

    /// Reports whether a frame is queued for eviction.
    ///
    /// @param frame - the frame's index
    pub(super) fn frame_is_cooling(&self, frame: u32) -> bool {
        self.state
            .borrow()
            .frames
            .get(frame as usize)
            .map(|meta| meta.state == FrameState::Cooling)
            .unwrap_or(false)
    }

    /// Reports whether a frame's parent is pinned, so its swip cannot be
    /// rewritten.
    ///
    /// @param parent - the parent reference, if any
    pub(super) fn parent_is_pinned(&self, parent: Option<(u32, PageId, usize)>) -> bool {
        match parent {
            Some((frame, _, _)) => self.pins_of(frame) > 0,
            None => false,
        }
    }

    /// Drops every cached page, so the next read comes from the file.
    ///
    /// **What a connection does when another process has committed.** Every
    /// frame is written back if it is dirty and then released, and every
    /// swizzled pointer into it is put back to a page id on the way - which is
    /// `evict_one`'s job and the reason this is written in terms of it rather
    /// than by clearing the tables. Clearing them directly would leave a parent
    /// page holding a pointer to a frame that now holds something else, which
    /// is the single worst thing this engine can do.
    ///
    /// Returns how many frames went.
    pub fn discard_all(&self) -> DbResult<usize> {
        let mut gone = 0usize;
        // Bounded by the frame count: a pinned frame cannot be evicted, and a
        // caller that still holds a guard gets fewer frames dropped rather than
        // an endless sweep.
        for _ in 0..self.frames().saturating_mul(2) {
            if self.resident() == 0 {
                break;
            }
            self.cool()?;
            match self.evict_one()? {
                Some(_) => gone = gone.saturating_add(1),
                None => break,
            }
        }
        Ok(gone)
    }
}
