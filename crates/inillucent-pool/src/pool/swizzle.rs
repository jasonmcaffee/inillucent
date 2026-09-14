//! Swips: a pointer that is either a page number or the frame holding it.
//!
//! Invariant: **a swizzled swip is only ever followed while the frame it names
//! holds the page it was swizzled for.** Everything here is about keeping that
//! true - recording which parent points at a child, unswizzling a parent's
//! pointer before the child's frame is taken, and forgetting a page's children
//! when the page itself goes.
//!
//! Split out of `pool.rs` in task-1946 (M12). See `crate::swip` for the
//! encoding; this is the pool's side of it.

use super::*;

impl Pool {
    /// Returns the page a swip names, whichever form it is in.
    ///
    /// A swizzled swip names a frame, and the frame knows its page, so this is
    /// the one place the two forms are reconciled. It is separate from
    /// [`Pool::fetch`] because a descent needs the page id before it fetches -
    /// it records the child in its path, and a path of frame numbers would stop
    /// meaning anything the moment one was evicted.
    ///
    /// @param swip - the child reference read from an interior page
    pub fn page_of_swip(&self, swip: Swip) -> DbResult<PageId> {
        if let Some(page) = swip.page() {
            if page.is_none() {
                return Err(corrupt("an interior slot names no child"));
            }
            return Ok(page);
        }
        let frame = swip
            .frame()
            .ok_or_else(|| corrupt("a swip is neither a page nor a frame"))?;
        let page = self
            .page_in_frame(frame)
            .ok_or_else(|| corrupt(format!("a swip names frame {frame}, which holds no page")))?;
        if page.is_none() {
            return Err(corrupt(format!("frame {frame} holds no page")));
        }
        Ok(page)
    }

    /// Returns the page a frame currently holds.
    ///
    /// @param frame - the frame's index
    pub fn page_in_frame(&self, frame: u32) -> Option<PageId> {
        self.state
            .borrow()
            .frames
            .get(frame as usize)
            .map(|meta| meta.page)
    }

    /// Records which slot of which frame points at a child.
    ///
    /// This is the back-reference eviction needs: to reuse a frame it must
    /// first put a page id back into whatever swizzled swip names it, and the
    /// child is the only thing that knows where that is.
    ///
    /// @param child - the child's frame
    /// @param parent - the parent's frame
    /// @param parent_page - the page that frame holds
    /// @param at - the swip's byte offset inside the parent page
    pub fn note_parent(&self, child: u32, parent: u32, parent_page: PageId, at: usize) {
        let mut state = self.state.borrow_mut();
        if let Some(meta) = state.frames.get_mut(child as usize) {
            meta.parent = Some((parent, parent_page, at));
        }
    }

    /// Writes a swizzled swip into a parent page, if the parent is still there.
    ///
    /// The page check is what makes this safe to call after the parent's guard
    /// has been dropped: a frame that was reused in between holds somebody
    /// else's page, and writing eight bytes of frame number into the middle of
    /// it would be a corruption with no error attached. A skipped swizzle costs
    /// one page-table lookup on the next descent and nothing else.
    ///
    /// The write does not dirty the frame. A swizzled swip and an unswizzled
    /// one name the same child; the page's *content* is unchanged, and
    /// `Pool::writeback` translates the form back on the way out.
    ///
    /// @param parent - the parent's frame
    /// @param expected - the page the parent frame should still hold
    /// @param at - the swip's byte offset inside the parent page
    /// @param swip - the reference to store
    pub fn swizzle_into(
        &self,
        parent: u32,
        expected: PageId,
        at: usize,
        swip: Swip,
    ) -> DbResult<bool> {
        if self.page_in_frame(parent) != Some(expected) {
            return Ok(false);
        }
        let Some(cell) = self.buffers.get(parent as usize) else {
            return Ok(false);
        };
        let Ok(mut bytes) = cell.try_borrow_mut() else {
            return Ok(false);
        };
        page::write_u64(&mut bytes, at, swip.raw())?;
        Ok(true)
    }

    /// Forgets every back-reference that points into a page being rewritten.
    ///
    /// A child records *where in its parent* its swip lives, so that eviction
    /// can put a page id back there. That offset is only meaningful for the
    /// layout the parent had when the child was swizzled - and a split rewrites
    /// its parent with one more separator and one more child, which moves every
    /// slot after the insertion point.
    ///
    /// The existing guard checks that the parent's *frame* still holds the
    /// parent's *page*, which is true throughout: it is the same page, rewritten
    /// in place. So without this, evicting a child after a split writes eight
    /// bytes of page id into whatever the new layout put at the old offset. The
    /// symptom was an interior page whose seventh key claimed to start at byte
    /// 23, which is inside the header.
    ///
    /// Called only for interior pages, because only an interior page is ever a
    /// parent - so the bulk builder's leaf installs do not pay for the sweep.
    ///
    /// @param page - the page whose layout is about to change
    pub(super) fn forget_children_of(&self, page: PageId) {
        let mut state = self.state.borrow_mut();
        for meta in state.frames.iter_mut() {
            if let Some((_, parent_page, _)) = meta.parent {
                if parent_page == page {
                    meta.parent = None;
                }
            }
        }
    }

    /// Puts a page id back into the parent's swip, so the child can be reused.
    ///
    /// Returns false when the parent could not be written, which leaves the
    /// child hot rather than making it unreachable.
    ///
    /// @param frame - the child frame
    pub(super) fn unswizzle_from_parent(&self, frame: u32) -> DbResult<bool> {
        let (parent, parent_page, at, page) = {
            let state = self.state.borrow();
            let Some(meta) = state.frames.get(frame as usize) else {
                return Ok(false);
            };
            match meta.parent {
                Some((parent, parent_page, at)) => (parent, parent_page, at, meta.page),
                None => return Ok(true),
            }
        };
        // The parent may itself have been evicted since it swizzled this child.
        // If its frame now holds a different page then nothing points at this
        // one any more - the parent's own writeback translated the swip on its
        // way out - so there is nothing to put back, and writing into that
        // frame would corrupt whatever page it holds now.
        //
        // The page check is necessary and, on its own, **not sufficient**: it
        // catches a frame reused for a different page and misses the same page
        // rewritten with a different layout, where `at` now points at some other
        // field. A split does exactly that to a parent, and the symptom was a
        // page id appearing where an interior page's key offset should be -
        // `interior key 7 starts at 23, before the heap`. What closes it is
        // [`Pool::forget_children_of`], called from `install`, which is the one
        // operation that replaces a whole page image.
        if self.page_in_frame(parent) != Some(parent_page) {
            return Ok(true);
        }
        let Some(cell) = self.buffers.get(parent as usize) else {
            return Ok(false);
        };
        let Ok(mut bytes) = cell.try_borrow_mut() else {
            return Ok(false);
        };
        page::write_u64(&mut bytes, at, Swip::unswizzled(page).raw())?;
        Ok(true)
    }
}
