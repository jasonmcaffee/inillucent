//! B-tree cursors: point seeks, range scans, and walking in both directions.
//!
//! Invariant: the stack is always a valid root-to-current path, and the pin on
//! every page in it is held for as long as the cursor is on that page. A
//! cursor that has been dropped or reset has released every pin it took, which
//! is the property `PageCache::pinned_frames` lets a test assert directly.
//!
//! Table and index B-trees are walked by the same code with one difference,
//! and it is worth stating because getting it wrong produces a scan that
//! silently skips rows. In a *table* B-tree an interior cell holds only a
//! child pointer and the largest rowid below it - the row itself lives on a
//! leaf - so a traversal visits leaves only. In an *index* B-tree an interior
//! cell holds a real entry, so the order is child, cell, child, cell, ...,
//! right-most child, and a traversal that visited only leaves would drop one
//! entry per interior cell. [`TreeKind`] carries that difference, and the two
//! stepping functions branch on it in exactly one place each.

use std::cmp::Ordering;
use std::sync::Arc;

use inillucent_base::error::corrupt;
use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_value::record::{self, KeyInfo, RecordRef};
use inillucent_value::{TextEncoding, Value};

use crate::btree::{BTreePage, PageKind, PageLayout};
use crate::cache::{PageKey, PagePin, PageVersion};
use crate::overflow;
use crate::pager::Pager;

/// The deepest path a cursor will follow before calling the file corrupt.
///
/// A B-tree over a 32-bit page space with at least two children per interior
/// page cannot be deeper than this, so a longer path means the child pointers
/// form a cycle.
const MAX_DEPTH: usize = 64;

/// Which kind of B-tree a cursor is walking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeKind {
    /// A table: keyed by rowid, with rows on the leaves only.
    Table,
    /// An index: keyed by a record, with entries on every page.
    Index,
}

/// Where a cursor is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorState {
    /// The cursor has not been positioned.
    Invalid,
    /// The cursor is on an entry.
    OnEntry,
    /// The cursor has walked off one end of the tree.
    Exhausted,
}

/// How a seek should behave when the key is not present.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekBias {
    /// Position on the first entry at or after the key.
    AtOrAfter,
    /// Position on the last entry at or before the key.
    AtOrBefore,
}

/// One page on a cursor's path.
#[derive(Debug)]
struct Frame {
    /// The pin that keeps the page resident.
    pin: PagePin,
    /// The page's validated structure, shared with its cache frame.
    layout: Arc<PageLayout>,
    /// The page's number.
    page: PageId,
    /// The frame's version when it was pushed, so a reload is detectable.
    version: PageVersion,
    /// A cell index when `at_cell`, and otherwise the child index the cursor
    /// descended through.
    slot: usize,
    /// Whether the cursor is positioned on this page's cell rather than
    /// somewhere below it.
    at_cell: bool,
}

impl Frame {
    /// Returns a view of the page this frame holds.
    fn page(&self) -> BTreePage<'_> {
        BTreePage::new(self.pin.bytes(), &self.layout)
    }
}

/// A cursor over one B-tree.
#[derive(Debug)]
pub struct BTreeCursor {
    root: PageId,
    kind: TreeKind,
    stack: Vec<Frame>,
    state: CursorState,
    key: KeyInfo,
    pages_visited: u64,
}

impl BTreeCursor {
    /// Builds a cursor over a table B-tree rooted at `root`.
    pub fn table(root: PageId) -> BTreeCursor {
        BTreeCursor {
            root,
            kind: TreeKind::Table,
            stack: Vec::new(),
            state: CursorState::Invalid,
            key: KeyInfo::default(),
            pages_visited: 0,
        }
    }

    /// Builds a cursor over an index B-tree with the given key ordering.
    pub fn index(root: PageId, key: KeyInfo) -> BTreeCursor {
        BTreeCursor {
            root,
            kind: TreeKind::Index,
            stack: Vec::new(),
            state: CursorState::Invalid,
            key,
            pages_visited: 0,
        }
    }

    /// Returns the tree's root page.
    pub fn root(&self) -> PageId {
        self.root
    }

    /// Returns which kind of tree this is.
    pub fn kind(&self) -> TreeKind {
        self.kind
    }

    /// Returns where the cursor is.
    pub fn state(&self) -> CursorState {
        self.state
    }

    /// Returns how deep the cursor's path currently is.
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Returns how many pages the cursor has read, for the baselines.
    pub fn pages_visited(&self) -> u64 {
        self.pages_visited
    }

    /// Releases every pin the cursor holds.
    pub fn reset(&mut self) {
        self.stack.clear();
        self.state = CursorState::Invalid;
    }

    /// Reports whether the cursor is on an entry.
    pub fn is_positioned(&self) -> bool {
        self.state == CursorState::OnEntry
    }

    /// Returns the page the cursor is currently on, for diagnostics.
    pub fn current_page(&self) -> Option<PageId> {
        self.stack.last().map(|frame| frame.page)
    }

    /// Reports whether every page on the path is still the page the cursor
    /// descended through.
    ///
    /// The comparison is against the frame the cache holds *now*, not against
    /// the one the cursor pinned. That distinction is the whole test: a writer
    /// publishes a new frame rather than mutating the old one, so the pinned
    /// frame keeps its version for ever and a cursor that compared a pin
    /// against itself would answer "current" no matter what had happened to the
    /// tree. What has changed is which frame is resident.
    pub fn path_is_current(&self, pager: &Pager) -> bool {
        let database = pager.database_id();
        self.stack.iter().all(|frame| {
            pager
                .cache()
                .version_of(PageKey {
                    database,
                    page: frame.page,
                })
                .is_some_and(|version| version == frame.version)
        })
    }

    /// Positions on the first entry, returning whether the tree has one.
    pub fn first(&mut self, pager: &mut Pager) -> DbResult<bool> {
        self.reset();
        let root = self.root;
        self.descend_leftmost(pager, root)?;
        if self.top_is_on_a_cell() {
            self.state = CursorState::OnEntry;
            return Ok(true);
        }
        self.step_forward(pager)
    }

    /// Positions on the last entry, returning whether the tree has one.
    pub fn last(&mut self, pager: &mut Pager) -> DbResult<bool> {
        self.reset();
        let root = self.root;
        self.descend_rightmost(pager, root)?;
        if self.top_is_on_a_cell() {
            self.state = CursorState::OnEntry;
            return Ok(true);
        }
        self.step_backward(pager)
    }

    /// Moves to the next entry.
    pub fn next(&mut self, pager: &mut Pager) -> DbResult<bool> {
        if self.state != CursorState::OnEntry {
            return Ok(false);
        }
        self.step_forward(pager)
    }

    /// Moves to the previous entry.
    pub fn previous(&mut self, pager: &mut Pager) -> DbResult<bool> {
        if self.state != CursorState::OnEntry {
            return Ok(false);
        }
        self.step_backward(pager)
    }

    /// Returns the rowid the cursor is on, for a table cursor.
    pub fn rowid(&self) -> DbResult<i64> {
        let frame = self.positioned_frame()?;
        frame.page().cell_rowid(frame.slot)
    }

    /// Returns the payload of the entry the cursor is on.
    ///
    /// The bytes are copied out of the page and its overflow chain, because a
    /// payload that overflows is not contiguous anywhere and a caller that
    /// wanted to borrow would have to know that.
    pub fn payload(&self, pager: &mut Pager, limits: &Limits) -> DbResult<Vec<u8>> {
        let frame = self.positioned_frame()?;
        let cell = frame.page().cell(frame.slot)?;
        overflow::read_payload(
            pager,
            cell.local_payload,
            cell.split.total,
            cell.overflow,
            limits,
        )
    }

    /// Reads the entry's payload into a buffer the caller keeps.
    ///
    /// The same bytes as `payload`, without the allocation: a scan reads one
    /// row per step, and a fresh `Vec` per row was a measurable share of what a
    /// step cost.
    /// @param pager - the pager holding the pages
    /// @param limits - the run-time limits
    /// @param into - the buffer to fill
    pub fn payload_into(
        &self,
        pager: &mut Pager,
        limits: &Limits,
        into: &mut Vec<u8>,
    ) -> DbResult<()> {
        let frame = self.positioned_frame()?;
        let cell = frame.page().cell(frame.slot)?;
        overflow::read_payload_into(
            pager,
            cell.local_payload,
            cell.split.total,
            cell.overflow,
            limits,
            into,
        )
    }

    /// Returns where the entry's payload lives, without reading it.
    ///
    /// A blob handle wants a range of one value and not the row it is in, so
    /// it asks where the payload is and reads the pages that hold the range.
    pub fn payload_place(&self) -> DbResult<crate::overflow::PayloadPlace> {
        let frame = self.positioned_frame()?;
        let cell = frame.page().cell(frame.slot)?;
        Ok(crate::overflow::PayloadPlace {
            page: frame.page,
            local_offset: cell.local_offset,
            local_len: cell.local_payload.len(),
            total: cell.split.total,
            overflow: cell.overflow,
        })
    }

    /// Returns whether the entry the cursor is on has an overflow chain.
    pub fn payload_overflows(&self) -> DbResult<bool> {
        let frame = self.positioned_frame()?;
        Ok(frame.page().cell(frame.slot)?.split.overflows)
    }

    /// Returns the frame the cursor is positioned on.
    fn positioned_frame(&self) -> DbResult<&Frame> {
        if self.state != CursorState::OnEntry {
            return Err(corrupt("an unpositioned cursor was read"));
        }
        let frame = self
            .stack
            .last()
            .ok_or_else(|| corrupt("a positioned cursor with no path"))?;
        if frame.slot >= frame.layout.cell_count {
            return Err(corrupt("a positioned cursor pointing past its page"));
        }
        Ok(frame)
    }

    /// Seeks to a rowid in a table B-tree.
    ///
    /// Returns true when the exact rowid is present. When it is not, the
    /// cursor is left on the neighbouring entry the bias asks for, which is
    /// what a range scan starts from.
    pub fn seek_rowid(&mut self, pager: &mut Pager, rowid: i64, bias: SeekBias) -> DbResult<bool> {
        if self.kind != TreeKind::Table {
            return Err(corrupt("a rowid seek on an index B-tree"));
        }
        self.reset();
        let mut page = self.root;
        loop {
            let mut frame = self.load(pager, page)?;
            let kind = frame.layout.kind;
            let cell_count = frame.layout.cell_count;
            if !kind.is_table() {
                return Err(corrupt("a table cursor reached an index page"));
            }

            // The first cell whose key is at least the target owns the subtree
            // the target would be in; an interior cell's rowid is the largest
            // below its left child, which makes the same search work on both.
            let mut low = 0usize;
            let mut high = cell_count;
            while low < high {
                let middle = low.saturating_add(high.saturating_sub(low) / 2);
                let key = frame.page().cell_rowid(middle)?;
                if key < rowid {
                    low = middle.saturating_add(1);
                } else {
                    high = middle;
                }
            }

            if kind == PageKind::LeafTable {
                let exact = low < cell_count && frame.page().cell_rowid(low)? == rowid;
                frame.at_cell = true;
                frame.slot = low;
                self.stack.push(frame);
                self.state = CursorState::OnEntry;
                if exact {
                    return Ok(true);
                }
                match bias {
                    SeekBias::AtOrAfter => {
                        if low >= cell_count {
                            self.step_forward(pager)?;
                        }
                    }
                    SeekBias::AtOrBefore => {
                        self.step_backward(pager)?;
                    }
                }
                return Ok(false);
            }

            let child = frame.page().child_at(low)?;
            frame.at_cell = false;
            frame.slot = low;
            self.stack.push(frame);
            page = child;
            self.guard_depth()?;
        }
    }

    /// Seeks to a key in an index B-tree.
    ///
    /// The probe may be shorter than the index's key, in which case it matches
    /// any entry with that prefix - which is what a range scan over the first
    /// column of a two-column index needs.
    pub fn seek_index(
        &mut self,
        pager: &mut Pager,
        probe: &[Value<'_>],
        bias: SeekBias,
    ) -> DbResult<bool> {
        if self.kind != TreeKind::Index {
            return Err(corrupt("an index seek on a table B-tree"));
        }
        self.reset();
        let encoding = pager.text_encoding();
        let limits = Limits::default();
        let mut page = self.root;
        let mut exact = false;
        loop {
            let mut frame = self.load(pager, page)?;
            let kind = frame.layout.kind;
            let cell_count = frame.layout.cell_count;
            if kind.is_table() {
                return Err(corrupt("an index cursor reached a table page"));
            }

            // Which side of the equal entries to stop on. `AtOrAfter` wants
            // the *first* entry at or after the probe, so it searches for the
            // lower bound; `AtOrBefore` wants the *last* entry at or before it,
            // which is the upper bound stepped back one. Landing on the first
            // equal entry either way looks right until the caller is walking
            // backwards over an equality prefix - it then starts at the front of
            // the run it meant to start at the back of, steps away, and returns
            // exactly one row.
            let upper = bias == SeekBias::AtOrBefore;
            let mut low = 0usize;
            let mut high = cell_count;
            while low < high {
                let middle = low.saturating_add(high.saturating_sub(low) / 2);
                let ordering =
                    self.compare_probe(pager, &frame, middle, probe, encoding, &limits)?;
                let after = if upper {
                    ordering != Ordering::Less
                } else {
                    ordering == Ordering::Greater
                };
                if after {
                    low = middle.saturating_add(1);
                } else {
                    high = middle;
                }
            }
            // The equal run sits at `low` for a lower bound and just before it
            // for an upper one, so that is where each looks for it.
            let at_equal = if upper { low.checked_sub(1) } else { Some(low) };
            if let Some(at) = at_equal {
                if at < cell_count
                    && self.compare_probe(pager, &frame, at, probe, encoding, &limits)?
                        == Ordering::Equal
                {
                    exact = true;
                }
            }

            if kind == PageKind::LeafIndex {
                frame.at_cell = true;
                frame.slot = low;
                self.stack.push(frame);
                self.state = CursorState::OnEntry;
                match bias {
                    SeekBias::AtOrAfter => {
                        if low >= cell_count {
                            self.step_forward(pager)?;
                        }
                    }
                    // `low` is the first entry strictly after the probe, so the
                    // one before it is the last entry at or before it.
                    SeekBias::AtOrBefore => {
                        self.step_backward(pager)?;
                    }
                }
                return Ok(exact);
            }

            let child = frame.page().child_at(low)?;
            frame.at_cell = false;
            frame.slot = low;
            self.stack.push(frame);
            page = child;
            self.guard_depth()?;
        }
    }

    /// Returns the record the cursor is on, decoded.
    pub fn record_values(
        &self,
        pager: &mut Pager,
        limits: &Limits,
    ) -> DbResult<Vec<inillucent_value::Value<'static>>> {
        let payload = self.payload(pager, limits)?;
        let encoding = pager.text_encoding();
        let record = RecordRef::parse_with_limits(&payload, encoding, limits)?;
        record
            .values()?
            .into_iter()
            .map(|value| value.into_owned())
            .collect()
    }

    /// Compares the probe against one cell's key.
    ///
    /// `Greater` means the probe sorts after the cell, which is the direction
    /// the binary search advances on.
    fn compare_probe(
        &self,
        pager: &mut Pager,
        frame: &Frame,
        index: usize,
        probe: &[Value<'_>],
        encoding: TextEncoding,
        limits: &Limits,
    ) -> DbResult<Ordering> {
        let cell = frame.page().cell(index)?;
        let payload = if cell.is_complete() {
            cell.local_payload.to_vec()
        } else {
            overflow::read_payload(
                pager,
                cell.local_payload,
                cell.split.total,
                cell.overflow,
                limits,
            )?
        };
        let record = RecordRef::parse_with_limits(&payload, encoding, limits)?;
        record::compare_values_to_record(probe, &record, &self.key)
    }

    /// Reads a page and builds a frame for it.
    fn load(&mut self, pager: &mut Pager, page: PageId) -> DbResult<Frame> {
        let pin = pager.get_page(page)?;
        let usable = pager.usable_size()?;
        let layout = pin.layout(usable)?;
        self.pages_visited = self.pages_visited.saturating_add(1);
        Ok(Frame {
            version: pin.version(),
            pin,
            layout,
            page,
            slot: 0,
            at_cell: false,
        })
    }

    /// Descends to the leftmost leaf below `page`, pushing every frame.
    fn descend_leftmost(&mut self, pager: &mut Pager, page: PageId) -> DbResult<()> {
        let mut current = page;
        loop {
            let mut frame = self.load(pager, current)?;
            if frame.layout.kind.is_leaf() {
                frame.at_cell = true;
                frame.slot = 0;
                self.stack.push(frame);
                return Ok(());
            }
            let child = frame.page().child_at(0)?;
            frame.at_cell = false;
            frame.slot = 0;
            self.stack.push(frame);
            current = child;
            self.guard_depth()?;
        }
    }

    /// Descends to the rightmost leaf below `page`, pushing every frame.
    fn descend_rightmost(&mut self, pager: &mut Pager, page: PageId) -> DbResult<()> {
        let mut current = page;
        loop {
            let mut frame = self.load(pager, current)?;
            if frame.layout.kind.is_leaf() {
                frame.at_cell = true;
                frame.slot = frame.layout.cell_count.saturating_sub(1);
                self.stack.push(frame);
                return Ok(());
            }
            let slot = frame.layout.cell_count;
            let child = frame.page().child_at(slot)?;
            frame.at_cell = false;
            frame.slot = slot;
            self.stack.push(frame);
            current = child;
            self.guard_depth()?;
        }
    }

    /// Refuses a path deeper than any legal B-tree, which is how a file whose
    /// child pointers form a cycle is stopped.
    fn guard_depth(&self) -> DbResult<()> {
        if self.stack.len() > MAX_DEPTH {
            return Err(corrupt("a B-tree path deeper than any legal tree"));
        }
        Ok(())
    }

    /// Reports whether the top frame is sitting on a real cell.
    fn top_is_on_a_cell(&self) -> bool {
        self.stack
            .last()
            .is_some_and(|frame| frame.at_cell && frame.slot < frame.layout.cell_count)
    }

    /// Reads the top frame's shape without holding a borrow on the stack.
    fn top_shape(&self) -> Option<(usize, bool, bool, usize)> {
        let frame = self.stack.last()?;
        Some((
            frame.layout.cell_count,
            frame.layout.kind.is_leaf(),
            frame.at_cell,
            frame.slot,
        ))
    }

    /// Returns the child at `slot` on the top frame.
    fn top_child(&self, slot: usize) -> DbResult<PageId> {
        let frame = self
            .stack
            .last()
            .ok_or_else(|| corrupt("a child was asked for with no path"))?;
        frame.page().child_at(slot)
    }

    /// Sets the top frame's position.
    fn set_top(&mut self, slot: usize, at_cell: bool) {
        if let Some(frame) = self.stack.last_mut() {
            frame.slot = slot;
            frame.at_cell = at_cell;
        }
    }

    /// Advances one entry, walking up and down the stack as needed.
    fn step_forward(&mut self, pager: &mut Pager) -> DbResult<bool> {
        loop {
            let Some((cell_count, is_leaf, at_cell, slot)) = self.top_shape() else {
                self.state = CursorState::Exhausted;
                return Ok(false);
            };

            if at_cell {
                if is_leaf {
                    let next = slot.saturating_add(1);
                    if next < cell_count {
                        self.set_top(next, true);
                        self.state = CursorState::OnEntry;
                        return Ok(true);
                    }
                    self.stack.pop();
                    continue;
                }
                // An index interior cell: the next entry is in the subtree to
                // its right.
                let next = slot.saturating_add(1);
                let child = self.top_child(next)?;
                self.set_top(next, false);
                self.descend_leftmost(pager, child)?;
                if self.top_is_on_a_cell() {
                    self.state = CursorState::OnEntry;
                    return Ok(true);
                }
                continue;
            }

            // A frame the cursor descended through at child index `slot`. In an
            // index tree the divider cell at that slot is itself an entry and
            // comes next; in a table tree it is only a separator.
            if self.kind == TreeKind::Index && slot < cell_count {
                self.set_top(slot, true);
                self.state = CursorState::OnEntry;
                return Ok(true);
            }
            if slot < cell_count {
                let next = slot.saturating_add(1);
                let child = self.top_child(next)?;
                self.set_top(next, false);
                self.descend_leftmost(pager, child)?;
                if self.top_is_on_a_cell() {
                    self.state = CursorState::OnEntry;
                    return Ok(true);
                }
                continue;
            }
            self.stack.pop();
        }
    }

    /// Retreats one entry, the mirror image of `step_forward`.
    fn step_backward(&mut self, pager: &mut Pager) -> DbResult<bool> {
        loop {
            let Some((cell_count, is_leaf, at_cell, slot)) = self.top_shape() else {
                self.state = CursorState::Exhausted;
                return Ok(false);
            };

            if at_cell {
                if is_leaf {
                    if slot > 0 && cell_count > 0 {
                        let previous = slot.saturating_sub(1).min(cell_count.saturating_sub(1));
                        self.set_top(previous, true);
                        self.state = CursorState::OnEntry;
                        return Ok(true);
                    }
                    self.stack.pop();
                    continue;
                }
                // An index interior cell: the previous entry is in the subtree
                // to its left.
                let child = self.top_child(slot)?;
                self.set_top(slot, false);
                self.descend_rightmost(pager, child)?;
                if self.top_is_on_a_cell() {
                    self.state = CursorState::OnEntry;
                    return Ok(true);
                }
                continue;
            }

            if self.kind == TreeKind::Index && slot > 0 {
                self.set_top(slot.saturating_sub(1), true);
                self.state = CursorState::OnEntry;
                return Ok(true);
            }
            if slot > 0 {
                let previous = slot.saturating_sub(1);
                let child = self.top_child(previous)?;
                self.set_top(previous, false);
                self.descend_rightmost(pager, child)?;
                if self.top_is_on_a_cell() {
                    self.state = CursorState::OnEntry;
                    return Ok(true);
                }
                continue;
            }
            self.stack.pop();
        }
    }
}

/// Where a cursor was, in terms that survive a write.
///
/// A cursor's stack is page numbers and slot indices, and a write invalidates
/// both: a balance moves cells between pages, so slot four of page nine is a
/// different entry afterwards, or no entry at all. What does survive is the
/// *key* the cursor was on, so that is what a saved position holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SavedPosition {
    /// The cursor was not on an entry.
    Unpositioned,
    /// The cursor was on this rowid, in a table B-tree.
    Rowid(i64),
    /// The cursor was on this key, in an index B-tree.
    Key(Vec<u8>),
}

impl BTreeCursor {
    /// Records where the cursor is, so it can be put back after a write.
    pub fn save_position(&self, pager: &mut Pager, limits: &Limits) -> DbResult<SavedPosition> {
        if self.state != CursorState::OnEntry {
            return Ok(SavedPosition::Unpositioned);
        }
        match self.kind {
            TreeKind::Table => Ok(SavedPosition::Rowid(self.rowid()?)),
            TreeKind::Index => Ok(SavedPosition::Key(self.payload(pager, limits)?)),
        }
    }

    /// Puts the cursor back where it was, reporting whether the entry is still
    /// there.
    ///
    /// When the entry has been deleted the cursor lands on the next one, which
    /// is what a scan that was interrupted by a delete needs: it carries on
    /// from where the deleted row was rather than from the beginning.
    pub fn restore(
        &mut self,
        pager: &mut Pager,
        saved: &SavedPosition,
        limits: &Limits,
    ) -> DbResult<bool> {
        match saved {
            SavedPosition::Unpositioned => {
                self.reset();
                Ok(false)
            }
            SavedPosition::Rowid(rowid) => self.seek_rowid(pager, *rowid, SeekBias::AtOrAfter),
            SavedPosition::Key(key) => {
                let encoding = pager.text_encoding();
                let record = RecordRef::parse_with_limits(key, encoding, limits)?;
                let values: Vec<Value<'static>> = record
                    .values()?
                    .into_iter()
                    .map(|value| value.into_owned())
                    .collect::<DbResult<Vec<_>>>()?;
                self.seek_index(pager, &values, SeekBias::AtOrAfter)
            }
        }
    }

    /// Reports whether the cursor has to be restored before it is read again.
    ///
    /// A cursor whose pages all still hold the version they did when it
    /// descended through them is looking at the same bytes it validated. One
    /// whose pages do not is looking at a page the writer has replaced, and its
    /// slot numbers mean nothing.
    pub fn needs_restore(&self, pager: &Pager) -> bool {
        self.state == CursorState::OnEntry && !self.path_is_current(pager)
    }
}
