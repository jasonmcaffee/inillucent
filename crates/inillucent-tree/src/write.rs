//! The write path over the tree: insert, delete, compaction, split and merge.
//!
//! Invariant: **the log record is written before the page is, and the mutation
//! that follows it cannot fail.** Every operation here has the same shape -
//! decide what the page will become, log it, then perform a change that has
//! already been proved to fit. A change that could still fail after its record
//! was written would leave a log saying something that did not happen, which is
//! worse than either half on its own: recovery would replay it.
//!
//! ## The root page id never changes
//!
//! When the root splits, its *contents* move into two new pages and the root is
//! rewritten in place as an interior. That costs one extra allocation on the one
//! occasion a tree gets taller, and it buys three things: the catalog never has
//! to be updated by a split, recovery never has to learn a new root, and a
//! concurrent reader holding the old root id is holding a page that is still the
//! root. The alternative - a new root page and a catalog write - makes every
//! split a two-object transaction.
//!
//! ## What a `NoRoom` means
//!
//! A leaf refuses an insert when its delta area is at [`crate::leaf::DELTA_LIMIT`]
//! or the row would collide with the mini-columns. The caller **compacts**:
//! every live row is read, sorted, and re-packed into a fresh page with the
//! delta area empty again. If they do not all fit, the leaf **splits**. So an
//! insert can do three things and the third is bounded: a page that has just
//! been split is at most half full, and half a page always has room for one row
//! that was small enough to be in the tree at all.
//!
//! ## Merging
//!
//! A delete that empties a leaf enough for it and its right sibling to fit in
//! one page merges them, rewriting the parent without the separator and freeing
//! the right page. Only siblings **under the same parent** are merged, because
//! merging across a parent boundary means rewriting two interior pages and the
//! separator between them, and the case is rare enough that refusing it costs a
//! half-empty leaf rather than a rebalance.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;
use inillucent_pool::extent::ExtentRef;
use inillucent_pool::interior::{InteriorBuilder, InteriorRef};
use inillucent_pool::page;
use inillucent_pool::{Database, PageId, Pool, Swip};
use inillucent_wal::record::{Body, Structural};

use crate::datum::{Datum, OwnedDatum};
use crate::leaf::{LeafBuilder, LeafRef, Packed, Rows};
use crate::mutate::LeafMut;
use crate::paged::PagedTree;

/// Reports whether a key sorts after a row's key.
///
/// @param key - the key arriving
/// @param row - the row to compare against
/// @param collations - the key columns' collations
/// @param key_columns - how many leading columns form the key
fn is_above(
    key: &[Datum<'_>],
    row: &[Datum<'_>],
    collations: &[inillucent_value::collation::Collation],
    key_columns: usize,
) -> bool {
    for column in 0..key_columns {
        let Some(wanted) = key.get(column) else {
            return false;
        };
        let Some(held) = row.get(column) else {
            return false;
        };
        let collation = collations
            .get(column)
            .copied()
            .unwrap_or(inillucent_value::collation::Collation::Binary);
        match crate::types::compare_under(wanted, held, collation) {
            core::cmp::Ordering::Greater => return true,
            core::cmp::Ordering::Less => return false,
            core::cmp::Ordering::Equal => {}
        }
    }
    false
}

/// What a leaf that has run out of room turns out to need.
///
/// Decided while the page is still borrowed, and acted on after the borrow ends
/// - a compaction and a split both write the page they are reading from.
enum Fit {
    /// The leaf holds out-of-line values and has to be rebuilt with the file in
    /// hand, because moving one needs a run of pages allocated.
    Repack(bool),
    /// The live rows fit one page: this image, and the two header fields the
    /// old page carried that a fresh pack does not know about.
    Compact(Vec<u8>, PageId, u64),
    /// They do not, so the leaf splits and the rows have to outlive the borrow.
    /// The flag says whether the rows are arriving in key order.
    Split(Vec<Vec<OwnedDatum>>, bool),
}

/// Reports whether a leaf holds fewer than half the rows it was packed with.
///
/// The B-tree underflow condition, and the gate on whether a merge is worth
/// *considering*. It is a row count and a popcount over the tombstone bitmap -
/// no allocation, no page pack - and it is what keeps the expensive test off the
/// path of a delete that has emptied nothing.
///
/// A leaf with no sorted rows at all has underflowed by definition: everything
/// it holds is in the delta area, which is small.
///
/// @param leaf - the leaf
fn underflows(leaf: &LeafRef<'_>) -> DbResult<bool> {
    let packed = leaf.row_count();
    if packed == 0 {
        return Ok(true);
    }
    Ok(leaf.live_rows()?.saturating_mul(2) < packed)
}

/// How full a compaction packs a page it is not splitting.
///
/// Not a hundred percent, so that a leaf which has just been compacted has room
/// for more delta rows before it has to be compacted again - a hundred would
/// make every insert after a compaction into another compaction.
///
/// **Seventy-five rather than ninety, and the difference is measured.** The room
/// left over *is* the delta area, so the fill decides how many rows a leaf can
/// take before it repacks: ninety percent of an eight-kilobyte page leaves about
/// eight hundred bytes, which is six rows of the gate's `main_table` - so an
/// insert-heavy workload repacked every six rows and wrote the whole page to the
/// log each time. Seventy-five leaves two kilobytes, which is closer to
/// seventeen.
///
/// It is a trade and the other side of it is size: a tree packed at
/// seventy-five percent is a fifth larger than one packed at ninety, and a scan
/// reads a fifth more pages. Both gates were re-run - the read families are the
/// ones that would pay for it.
pub const COMPACT_FILL: f64 = 0.75;

/// TEMPORARY counters for the extension profile: wide placements, room-makings,
/// and the encoded size of every row an insert offered.
/// How full each half of a split is packed.
///
/// A half-full page is what makes the "compact, then split, then insert" path
/// terminate: the row that would not fit is one row, and half a page has room
/// for any row small enough to have been in the tree at all.
const SPLIT_FILL: f64 = 0.50;

/// How full the left half of an *append's* split is packed.
///
/// Rows arriving in key order never come back to the page they left behind, so
/// the room a split creates belongs on the right. Half and half would leave
/// every page but the last permanently half empty - twice the pages, twice the
/// descents, and twice the file.
const APPEND_FILL: f64 = 0.95;

/// The fill a compaction falls back to before it gives up and splits.
///
/// **Without it, every bulk-built leaf split on its first write, whatever the
/// write was.** [`crate::tree::BULK_FILL`] is 0.9 and [`COMPACT_FILL`] is 0.75,
/// and a compaction was refused unless every live row fitted in 0.75 of a page -
/// which a leaf packed at 0.9 never does. So an import's leaves, and a
/// `CREATE INDEX`'s, split at [`SPLIT_FILL`] the moment anything touched them,
/// and the space was never recovered.
///
/// It was measured on the gate's own fixture, at 32 KiB pages. One
/// `UPDATE main_table SET key = key + 1 WHERE id % 20 = 0` - the gate's
/// `write.update.indexed`, five thousand of a hundred thousand rows, **no rows
/// added or removed** - took `main_key` from 57 pages to 113 and
/// `main_category` from 85 to 117. SQLite, the same statement on the same
/// fixture, went from 307 pages to 307.
///
/// Ninety-five rather than a hundred so that a leaf which has just been
/// compacted tight still has room for the delta rows that follow; the caller
/// also checks the packed image against [`LeafMut::room_for`], so a compaction
/// is only taken when the write that asked for it will actually land.
const TIGHT_FILL: f64 = 0.95;

/// Packs every live row of a leaf into one page, at the loosest fill that holds
/// them all.
///
/// `None` when they do not fit one page at any of the fills, which is the
/// caller's signal to split.
///
/// **Recovery calls this too, and that is what makes it a function rather than a
/// loop inside `make_room`.** A `CompactLeaf` record carries no page image when
/// the compaction moved nothing out of line; redo re-runs the pack over the same
/// rows and must land on the same bytes, so the fill has to be chosen from the
/// rows and nothing else.
///
/// @param builder - the leaf builder for this tree
/// @param rows - the leaf's live rows, sorted
pub fn compact_image<'d>(
    builder: &LeafBuilder,
    rows: &dyn crate::leaf::Rows<'d>,
) -> DbResult<Option<Vec<u8>>> {
    for fill in [COMPACT_FILL, TIGHT_FILL] {
        // `pack_all_rows` rather than `pack`: a rung that cannot hold every row
        // costs one sizing pass here, where `pack` would encode a whole page
        // image and then have it discarded. A leaf that arrived from a bulk
        // build fails the first rung every time.
        if let Some(page) = builder.pack_all_rows(rows, fill)? {
            return Ok(Some(page));
        }
    }
    Ok(None)
}

/// Where a mutation writes its log records.
///
/// A trait rather than a direct dependency on the transaction manager, because
/// the tree sits below it: a tree knows about pages, keys and bytes, and what a
/// transaction is is somebody else's question. `inillucent-txn`'s `Transaction`
/// implements this.
pub trait TreeLog {
    /// Appends one record and returns its LSN.
    ///
    /// @param body - what is about to happen
    fn log(&mut self, body: Body<'_>) -> DbResult<u64>;

    /// Whether this log is collecting before-images.
    ///
    /// **The log is asked rather than told.** A write path that had to be
    /// given a flag would have one more argument on every entry point and one
    /// more thing a caller can pass wrongly; the log already knows whether a
    /// transaction is open, because it is the transaction's log.
    ///
    /// The default is false, which is what an autocommit statement wants: it
    /// cannot be abandoned, so nothing has to be remembered, and it pays
    /// nothing for the possibility.
    fn wants_undo(&self) -> bool {
        false
    }

    /// Records what one row looked like before a write changed it.
    ///
    /// `before` is `None` when the key was not there, which is what a rollback
    /// restores by deleting it again.
    ///
    /// **The key is borrowed, not given.** A restore that has a row to write
    /// back does not need it - the row carries its own key columns and `put`
    /// finds them - so only the delete case has to copy it, and that decision
    /// belongs to the log rather than to every write path that calls this.
    /// Handing over an owned key allocated one vector per write and threw most
    /// of them away.
    ///
    /// @param tree - the tree the row is in
    /// @param key - the row's key columns
    /// @param before - the whole row as it was, or `None`
    fn undo(
        &mut self,
        tree: u64,
        key: &[Datum<'_>],
        before: Option<Vec<OwnedDatum>>,
    ) -> DbResult<()> {
        let _ = (tree, key, before);
        Ok(())
    }
}

/// A [`Spill`] that answers with a reference to nowhere.
///
/// For the *measuring* half of a split or a merge, which asks only how many
/// rows fit. A spiller that allocated there would write a run for every value
/// the encode is about to write again, and the run it wrote would be
/// unreferenced by anything. The size is what the measure needs and the size of
/// an extent reference does not depend on where it points.
///
/// The page it names is one rather than zero because
/// [`inillucent_pool::extent::ExtentRef::decode`] refuses page zero, and a
/// reference that could not be decoded would be one this could not be swapped
/// for a real spiller against in a test.
struct Measuring;

/// A leaf's live rows and, per row, the extent each oversized value lives in.
///
/// The two are parallel rather than one list of pairs because the rows are
/// re-encoded as a block and the extents are followed one at a time.
pub type Repacked = (Vec<Vec<OwnedDatum>>, Vec<Vec<Option<ExtentRef>>>);

impl crate::leaf::Spill for Measuring {
    fn spill(
        &mut self,
        _row: usize,
        _column: usize,
        value: &[u8],
    ) -> DbResult<inillucent_pool::extent::ExtentRef> {
        Ok(inillucent_pool::extent::ExtentRef::run(
            PageId(1),
            value.len() as u64,
        ))
    }
}

/// A `TreeLog` that logs nothing and hands out increasing LSNs.
///
/// For the bulk builder, which writes a tree nobody has read yet, and for tests
/// of the tree's own shape. **Not** for anything that has to survive a crash: a
/// page stamped by this carries an LSN no log record explains.
#[derive(Debug, Default)]
pub struct NoLog {
    next: u64,
}

impl TreeLog for NoLog {
    fn log(&mut self, _body: Body<'_>) -> DbResult<u64> {
        self.next = self.next.saturating_add(8);
        Ok(self.next)
    }
}

/// Where a key was found in a leaf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Located {
    /// In the sorted region, at this row, and live.
    Sorted(usize),
    /// In the delta area, at this index.
    Delta(usize),
    /// Not in this leaf.
    Absent,
}

/// What a write did, for the report and for the tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WriteStats {
    /// Rows inserted, including the insert half of an update.
    pub inserted: u64,
    /// Rows deleted.
    pub deleted: u64,
    /// Slots overwritten in place.
    pub updated_in_place: u64,
    /// Leaves compacted.
    pub compactions: u64,
    /// Leaves split.
    pub splits: u64,
    /// Leaves merged away.
    pub merges: u64,
}

impl PagedTree {
    /// Returns what the write path has done to this tree.
    pub fn write_stats(&self) -> WriteStats {
        self.stats.get()
    }

    /// Inserts or replaces one row.
    ///
    /// Returns the row that was there before, when there was one. A key already
    /// present is *replaced*, which is what makes this the one entry point an
    /// `INSERT OR REPLACE` and an `UPDATE` both go through - a second code path
    /// for "the key is already there" is the kind of duplicate that agrees with
    /// the first one until the day it does not.
    ///
    /// @param database - the file, for allocating pages a split needs
    /// @param log - where the record goes
    /// @param row - the row, one value per column, key columns first
    pub fn insert(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        row: &[Datum<'_>],
    ) -> DbResult<Option<Vec<OwnedDatum>>> {
        self.write_row(database, log, row, true, true)
    }

    /// Inserts or replaces one row without copying out what was there.
    ///
    /// **The same write, minus a full row materialisation the caller did not
    /// ask for.** `insert` returns the previous row, so it reads and copies
    /// every column of it - allocating per text and per blob - before it writes.
    /// The executor's write path throws that away: it already knows whether the
    /// key was there, from the uniqueness check it had to do anyway.
    ///
    /// Returns whether the key was already present, which is the only thing the
    /// row count needs. On `main_table` - five columns, a text and a blob - the
    /// copy was a measurable share of the gate's `write.insert.batch`.
    ///
    /// @param database - the file, for allocating pages a split needs
    /// @param log - where the record goes
    /// @param row - the row, one value per column, key columns first
    pub fn put(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        row: &[Datum<'_>],
    ) -> DbResult<bool> {
        Ok(self.write_row(database, log, row, false, true)?.is_some())
    }

    /// Inserts one row only if its key is not already there.
    ///
    /// Returns false, having written nothing, when the key is present.
    ///
    /// **This is one descent where the caller would otherwise make two.** An
    /// insert that must refuse a duplicate has to know whether the key is there
    /// *before* it writes, and the obvious way is to probe and then insert - two
    /// descents, two page parses and two locates of the same key. Finding it
    /// once and stopping is the same guarantee for half the work, and it is the
    /// shape SQLite's insert has: seek, and write where the seek landed.
    ///
    /// The caller still builds its constraint message from a second probe, and
    /// that is the right place for it: the message is only needed when the
    /// insert is about to fail.
    ///
    /// @param database - the file, for allocating pages a split needs
    /// @param log - where the record goes
    /// @param row - the row, one value per column, key columns first
    pub fn put_absent(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        row: &[Datum<'_>],
    ) -> DbResult<bool> {
        Ok(self.write_row(database, log, row, false, false)?.is_none())
    }

    /// The body of `insert` and `put`.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param row - the row
    /// @param want_previous - whether to copy out the row that was there
    /// @param replace - whether a key already there is overwritten or left alone
    fn write_row(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        row: &[Datum<'_>],
        want_previous: bool,
        replace: bool,
    ) -> DbResult<Option<Vec<OwnedDatum>>> {
        // A log collecting before-images needs the row that was there, so the
        // read the caller did not ask for happens anyway. That is the whole
        // cost of being able to abandon a transaction, and it is paid only
        // inside one.
        let caller_wants_previous = want_previous;
        let want_previous = want_previous || log.wants_undo();
        if row.len() != self.columns().len() {
            return Err(misuse(format!(
                "a row of {} values does not fit a tree of {} columns",
                row.len(),
                self.columns().len()
            )));
        }
        let key: Vec<Datum<'_>> = row.iter().copied().take(self.key_columns()).collect();
        let encoded_key = self.encode_key(&key);
        // **A value too large for a leaf is written out of line before the row
        // is placed, not instead of placing it.**
        //
        // The first version repacked the whole leaf around such a row, because
        // the builder is where the spiller is. That made a two-kilobyte value
        // cost a page rewrite and a full-page log record: on the gate's
        // `extension.fts.build`, three thousand of FTS5's segment blocks fall
        // between the threshold and what a leaf holds, and the repacks were
        // 111 ms of a 250 ms workload - more than half of it.
        //
        // Spilling first turns the row into one a delta area can hold: the
        // seventeen tagged bytes of a reference in place of the value. From
        // there it is an ordinary write, with an ordinary short log record, and
        // the leaf is repacked when it fills rather than once per wide row.
        //
        // Spilled once, outside the loop, because a retry after a compaction
        // must reuse the run rather than write a second and leak the first.
        let mut spilled = false;
        let mut encoded_row = Vec::new();
        for (column, value) in row.iter().enumerate() {
            match self.out_of_line(column, value) {
                Some(bytes) => {
                    let reference =
                        crate::paged::write_extent(database, log, self.tree_id(), bytes)?;
                    crate::leaf::encode_extent_tagged(&mut encoded_row, reference);
                    spilled = true;
                }
                None => value.encode_tagged(&mut encoded_row),
            }
        }

        // Two attempts at most: the first may find the leaf full, and the
        // compaction or split that follows leaves a page that has room for one
        // row by construction. A third attempt would mean the second did not,
        // which is a bug rather than a case to loop on.
        for attempt in 0..2 {
            let (page, path) = self.leaf_for(database.pool(), &encoded_key)?;
            // **The key is found once.** Where it sits decides three things -
            // whether it was there, what the caller gets back, and what the
            // mutation below has to displace - and the first version asked the
            // page all three times. A locate is a page parse, a binary search
            // and a walk of the delta area, and the delta area is up to
            // thirty-two rows compared column by column; on the gate's
            // `write.insert.batch` the two indexes cost 8.4 us of a 19 us
            // insert, and half of that was asking twice.
            //
            // Nothing between here and the mutation changes the page: the
            // room check only reads, and the log append does not touch pages
            // at all. A `make_room` restarts the attempt, which re-locates.
            let (located, mut previous) = {
                let guard = database.pool().fetch(page)?;
                let leaf = LeafRef::parse(&guard)?
                    .with_collations(self.collations())
                    .with_directions(self.directions());
                let located = leaf.locate(&key, self.key_columns())?;
                // One row's out-of-line values, and only when the caller wants
                // the row it is replacing. Locating reads key columns, which are
                // never out of line, so this comes after.
                let held = match (want_previous, located) {
                    (true, Located::Sorted(row)) => {
                        self.read_extents_row(database.pool(), &leaf, row)?
                    }
                    (true, Located::Delta(index)) => {
                        self.read_extents_delta(database.pool(), &leaf, index)?
                    }
                    _ => crate::leaf::Extents::default(),
                };
                let leaf = leaf.with_extents(&held);
                let previous = match (want_previous, located) {
                    (_, Located::Absent) => None,
                    (false, _) => {
                        // A marker, not the row: `put` reports presence and this
                        // value never leaves `write_row`.
                        Some(Vec::new())
                    }
                    (true, Located::Sorted(row)) => {
                        let mut values = Vec::with_capacity(leaf.column_count());
                        for column in 0..leaf.column_count() {
                            values.push(OwnedDatum::from_datum(&leaf.value(row, column)?));
                        }
                        Some(values)
                    }
                    (true, Located::Delta(index)) => {
                        let mut values = Vec::with_capacity(leaf.column_count());
                        for column in 0..leaf.column_count() {
                            values.push(OwnedDatum::from_datum(&leaf.delta_value(index, column)?));
                        }
                        Some(values)
                    }
                };
                (located, previous)
            };
            // A caller that refuses a duplicate is told so before anything is
            // written, which is the whole point of asking.
            if !replace && previous.is_some() {
                return Ok(previous);
            }
            let planned = {
                let pool = database.pool();
                pool.modify(page, |bytes| {
                    LeafMut::new(bytes)?.room_for(encoded_row.len())
                })?
            };
            if !planned {
                if attempt == 1 {
                    return Err(corrupt(
                        "a leaf had no room for one row after being compacted and split",
                    ));
                }
                self.make_room(database, log, page, &path, Some(&key), encoded_row.len())?;
                continue;
            }
            // Recorded here rather than above the room check, because a retry
            // re-locates and would record the same row twice.
            if log.wants_undo() {
                // Given rather than cloned when the caller did not ask for the
                // row: `put` and `put_absent` report presence and throw the
                // row away, so cloning it for them was a whole row copied per
                // write for nothing.
                let recorded = match caller_wants_previous {
                    true => previous.clone(),
                    false => previous.take(),
                };
                log.undo(self.tree_id(), &key, recorded)?;
            }

            // A delta row that is about to be removed may own out-of-line
            // pages, and nothing else names them: a tombstoned *sorted* row's
            // reference is still on the page for the next repack to free, but a
            // removed delta row's is not. Read before the write, freed after.
            let orphaned = match located {
                Located::Delta(index) => {
                    let guard = database.pool().fetch(page)?;
                    let leaf = LeafRef::parse(&guard)?;
                    let mut refs = Vec::new();
                    for column in 0..leaf.column_count() {
                        if let Some(reference) = leaf.delta_extent_at(index, column)? {
                            refs.push(reference);
                        }
                    }
                    refs
                }
                _ => Vec::new(),
            };
            let lsn = log.log(Body::InsertRow {
                tree: self.tree_id(),
                page: page.0,
                row: &encoded_row,
            })?;
            database.pool().modify(page, |bytes| {
                let mut leaf = LeafMut::new(bytes)?;
                match located {
                    Located::Sorted(row_index) => {
                        leaf.set_tombstone(row_index)?;
                    }
                    Located::Delta(index) => leaf.remove_delta(index)?,
                    Located::Absent => {}
                }
                let plan = leaf
                    // Taken rather than cloned: the loop only retries
                    // above this point, and the attempt that reaches here
                    // returns.
                    .plan_encoded(std::mem::take(&mut encoded_row))?
                    .ok_or_else(|| corrupt("a leaf that had room lost it before the write"))?;
                leaf.apply_delta(&plan)?;
                if spilled {
                    leaf.mark_extents()?;
                }
                leaf.set_lsn(lsn)
            })?;
            for reference in orphaned {
                crate::paged::free_extent(database, log, reference)?;
            }
            let mut stats = self.stats.get();
            stats.inserted = stats.inserted.saturating_add(1);
            self.stats.set(stats);
            if previous.is_none() {
                self.note_rows(1);
            }
            return Ok(previous);
        }
        Err(corrupt("an insert did not converge"))
    }

    /// Returns the bytes of a value that has to be written out of line.
    ///
    /// **A key column is never spilled**, because a descent compares keys and a
    /// comparison that had to read other pages would turn every search into a
    /// chain of them. The threshold is the same one the builder packs to, so a
    /// value spilled here is one a repack would have spilled anyway.
    ///
    /// @param column - which column the value is in
    /// @param value - the value
    fn out_of_line<'v>(&self, column: usize, value: &Datum<'v>) -> Option<&'v [u8]> {
        if column < self.key_columns() {
            return None;
        }
        let threshold = self.page_size() / crate::leaf::EXTENT_DIVISOR;
        let physical = self
            .columns()
            .get(column)
            .map(|spec| spec.physical)
            .unwrap_or(crate::types::PhysicalType::Any);
        match (physical, value) {
            (crate::types::PhysicalType::Text, Datum::Text(bytes))
            | (crate::types::PhysicalType::Blob, Datum::Blob(bytes))
                if bytes.len() > threshold =>
            {
                Some(bytes)
            }
            _ => None,
        }
    }

    /// Deletes one row by key, returning what was there.
    ///
    /// @param database - the file, for freeing a page a merge empties
    /// @param log - where the record goes
    /// @param key - the key, one value per key column
    pub fn delete(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        key: &[Datum<'_>],
    ) -> DbResult<Option<Vec<OwnedDatum>>> {
        let encoded_key = self.encode_key(key);
        let (page, path) = self.leaf_for(database.pool(), &encoded_key)?;
        let previous = self.row_at(database.pool(), page, key)?;
        if previous.is_none() {
            return Ok(None);
        }
        let located = self.locate(database.pool(), page, key)?;
        if let Located::Sorted(_) = located {
            let room = database.pool().modify(page, |bytes| {
                LeafMut::new(bytes)?.has_room_for_a_tombstone()
            })?;
            if !room {
                self.make_room(database, log, page, &path, None, 0)?;
                return self.delete(database, log, key);
            }
        }
        // The key goes into the record as **tagged values**, not as the
        // comparison encoding the descent uses. The comparison encoding is one
        // way: it orders correctly and it cannot be read back, so a recovery
        // holding one could not find the row it names. The row records already
        // carried tagged values for the same reason, and having the two records
        // disagree about how a key is written was the difference between a
        // database that recovers and one that only recovers if a checkpoint
        // happened to have written the leaf.
        // Recorded after the room handling, because the retry above deletes
        // again and would record the same row twice.
        if log.wants_undo() {
            log.undo(self.tree_id(), key, previous.clone())?;
        }
        let mut tagged_key = Vec::new();
        for value in key.iter().take(self.key_columns()) {
            value.encode_tagged(&mut tagged_key);
        }
        let lsn = log.log(Body::DeleteRow {
            tree: self.tree_id(),
            page: page.0,
            key: &tagged_key,
        })?;
        let located = self.locate(database.pool(), page, key)?;
        // A delta row's out-of-line pages are nobody's once the row is gone: a
        // tombstoned sorted row still names its extent for the next repack to
        // free, and a removed delta row names nothing at all.
        let orphaned = match located {
            Located::Delta(index) => {
                let guard = database.pool().fetch(page)?;
                let leaf = LeafRef::parse(&guard)?;
                let mut refs = Vec::new();
                for column in 0..leaf.column_count() {
                    if let Some(reference) = leaf.delta_extent_at(index, column)? {
                        refs.push(reference);
                    }
                }
                refs
            }
            _ => Vec::new(),
        };
        database.pool().modify(page, |bytes| {
            let mut leaf = LeafMut::new(bytes)?;
            match located {
                Located::Sorted(row_index) => {
                    leaf.set_tombstone(row_index)?;
                }
                Located::Delta(index) => leaf.remove_delta(index)?,
                // Unreachable: `previous` was `Some`, which is exactly the
                // condition under which `locate` finds the key. Reported rather
                // than ignored, because the two disagreeing would mean a row
                // vanished between two reads of the same page.
                Located::Absent => {
                    return Err(corrupt("a row that was read could not be found to delete"))
                }
            }
            leaf.set_lsn(lsn)
        })?;
        for reference in orphaned {
            crate::paged::free_extent(database, log, reference)?;
        }
        let mut stats = self.stats.get();
        stats.deleted = stats.deleted.saturating_add(1);
        self.stats.set(stats);
        self.note_rows(-1);
        self.merge_if_small(database, log, page, &path)?;
        Ok(previous)
    }

    /// Overwrites one fixed-width column of one row, in place.
    ///
    /// The fast path an `UPDATE` of a numeric column takes, and the only write
    /// that leaves a leaf on the vectorised fast path: no delta row, no
    /// tombstone, no compaction. Returns false when the change does not fit that
    /// shape - the column is not fixed-width, the value is not of its class, or
    /// the row's current value is a NULL - and the caller falls back to
    /// [`PagedTree::insert`].
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param key - the row's key
    /// @param column - which column to write
    /// @param value - the new value
    pub fn update_in_place(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        key: &[Datum<'_>],
        column: usize,
        value: &Datum<'_>,
        before: Option<&[OwnedDatum]>,
    ) -> DbResult<bool> {
        if column < self.key_columns() {
            // Changing a key in place would move the row, which is an insert
            // and a delete rather than an update.
            return Ok(false);
        }
        let encoded_key = self.encode_key(key);
        let (page, _) = self.leaf_for(database.pool(), &encoded_key)?;
        let Located::Sorted(row_index) = self.locate(database.pool(), page, key)? else {
            return Ok(false);
        };
        // **Costed by asking, not by writing to a copy of the page.** The page
        // may not change before its record is in the log, and the way this used
        // to find out whether the slot write applied was to run it against
        // `guard.bytes().to_vec()`: a 32 KiB allocation and a 32 KiB copy per
        // update, and `txn.large` is two thousand updates in one transaction.
        // `would_update_slot` asks the same questions in the same order and
        // touches nothing.
        {
            let guard = database.pool().fetch(page)?;
            if !crate::mutate::would_update_slot(guard.bytes(), column, row_index, value)? {
                return Ok(false);
            }
        }
        // The whole row, not the one slot. A rollback restores a row, and a
        // record that named only the column changed would restore a row that
        // never existed if two updates touched two columns of it.
        //
        // **Taken from the caller, which already read it.** The executor reads
        // the row before it decides what to write - that is where the "before"
        // image of an `UPDATE` comes from - and this went and read it again:
        // a third descent of the same tree, a second page parse, a second walk
        // of the delta area and a second materialisation of every column, per
        // statement. `row_at` is still the answer when a caller has no image to
        // give, which is what a trigger body's write is.
        if log.wants_undo() {
            let held = match before {
                Some(row) => Some(row.to_vec()),
                None => self.row_at(database.pool(), page, key)?,
            };
            log.undo(self.tree_id(), key, held)?;
        }
        let mut slot = Vec::new();
        value.encode_tagged(&mut slot);
        let mut tagged_key = Vec::new();
        for value in key.iter().take(self.key_columns()) {
            value.encode_tagged(&mut tagged_key);
        }
        let lsn = log.log(Body::UpdateInPlace {
            tree: self.tree_id(),
            page: page.0,
            key: &tagged_key,
            column: column as u32,
            value: &slot,
        })?;
        database.pool().modify(page, |bytes| {
            let mut leaf = LeafMut::new(bytes)?;
            leaf.update_slot(column, row_index, value)?;
            leaf.set_lsn(lsn)
        })?;
        let mut stats = self.stats.get();
        stats.updated_in_place = stats.updated_in_place.saturating_add(1);
        self.stats.set(stats);
        Ok(true)
    }

    /// Records a commit timestamp on every leaf a transaction touched.
    ///
    /// @param pool - the buffer pool
    /// @param pages - the leaves
    /// @param cts - the commit timestamp
    pub fn stamp_commit(&self, pool: &Pool, pages: &[PageId], cts: u64) -> DbResult<()> {
        for page in pages {
            pool.modify(*page, |bytes| LeafMut::new(bytes)?.set_max_cts(cts))?;
        }
        Ok(())
    }

    /// Compacts a leaf, or splits it when its live rows no longer fit one page.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf
    /// @param arriving - the key about to be written, when there is one
    pub fn make_room(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
        arriving: Option<&[Datum<'_>]>,
        needed: usize,
    ) -> DbResult<()> {
        // **Packed straight out of the page.** The rows a compaction repacks are
        // already in a leaf, where a text or a blob is a slice; copying them to
        // owned values and borrowing them straight back was two allocations per
        // value, per compaction, to produce what the page already held.
        //
        // It matters because a compaction is not rare and it is not cheap: the
        // write gate measured `write.insert.batch` at 7.8 us in the steady state
        // and 17.8 us on average, and the difference was the one insert in
        // twenty that repacks a leaf of eighty rows.
        //
        // The guard is dropped before anything is written, because the write
        // needs the page mutably and this only needs to read it.
        let fit = 'fit: {
            let guard = database.pool().fetch(page)?;
            let leaf = LeafRef::parse(&guard)?
                .with_collations(self.collations())
                .with_directions(self.directions());
            let held = self.read_extents(database.pool(), &leaf)?;
            let leaf = leaf.with_extents(&held);
            // **Positions, not values.** `live()` allocates a `Vec<Datum>` per
            // row and one more for the outer vector; on an index leaf holding
            // three and a half thousand entries that is three and a half
            // thousand allocations per compaction, to produce values that are
            // already on the page. `live_order` is one allocation of four bytes
            // a row and the builder reads through it.
            let source = leaf.live_source()?;
            // **A leaf with out-of-line values always takes the owned route.**
            // The fast path below packs straight out of the page, which needs
            // the guard held - and repacking an extent needs the *file*, to
            // allocate the run the value moves into. The two cannot be held at
            // once, and a leaf with extents holds few rows, so the copy costs
            // little where it costs anything at all.
            let spilled = leaf.has_extents();
            // **An append splits *lopsidedly*; it does not split *early*.**
            //
            // This flag used to force a split instead of a compaction, on the
            // argument that a leaf filled by rows arriving in key order is
            // compacted, filled again by the next thirty-two, and compacted
            // again - and that a compaction repacks every live row and writes
            // the whole page to the log.
            //
            // The argument does not survive being measured on a page rather
            // than on a workload. A leaf can only hold
            // `DELTA_LIMIT` rows before it is full, so forcing a split gave the
            // left page **thirty-two rows** and the right page none - and the
            // next thirty-two filled the new page and split it again. Every
            // page in an appended tree held thirty-two rows where the same
            // table built out of key order held nine hundred and sixty-two:
            // `INSERT INTO w SELECT id, v FROM u` over a hundred thousand rows
            // wrote 3,130 pages for 104 pages of data, and a 200,000-row table
            // was 221 MB against SQLite's 10.7.
            //
            // The log went the same way, which is what settles it: the same
            // insert checkpoints **72** pages now against **3,134** before, so
            // splitting three pages per thirty-two rows always cost more log
            // than compacting one. On the gate: `write.insert.batch` 0.35x to
            // 0.50x, `fts.build` 0.15x to 0.25x, `rtree.insert` 0.26x to 0.33x,
            // the `transaction` family over the 1.00x floor for the first time,
            // the headline 3.01x to 3.11x, and every read family inside the
            // run-to-run spread.
            //
            // What the flag still does is choose the fill when a split really
            // is needed - the page is genuinely full - because rows arriving in
            // order never come back to the page they left behind. See
            // `APPEND_FILL`.
            let last_row: Option<Vec<Datum<'_>>> = (!source.is_empty()).then(|| {
                (0..self.key_columns())
                    .map(|column| source.value(source.len().saturating_sub(1), column))
                    .collect()
            });
            let appending = leaf.right_sibling().is_none()
                && source.len() >= 2
                && match (arriving, last_row.as_deref()) {
                    (Some(key), Some(last)) => {
                        is_above(key, last, self.collations(), self.key_columns())
                    }
                    _ => false,
                };
            if spilled {
                // The rows are *not* copied out here. Reading them through the
                // guard would resolve every out-of-line value, which is the read
                // the repack exists to avoid; `rows_to_repack` reads them again
                // without one.
                break 'fit Fit::Repack(appending);
            }
            let builder = LeafBuilder::new(
                self.page_size(),
                self.tree_id(),
                self.columns().to_vec(),
                self.key_columns(),
            )?;
            // **Two fills, and a split only when neither of them holds the
            // rows.** The preferred fill leaves the delta room `COMPACT_FILL`
            // exists to leave; the tight one is what stands between a leaf that
            // arrived at 0.9 from a bulk build and two leaves at 0.5. See
            // `TIGHT_FILL`.
            //
            // **The room check is on the chosen image, not part of the
            // choosing.** Recovery replays a compaction by running
            // `compact_image` again over the same rows, and it has no idea what
            // row the write was making room for - so the fill has to be a
            // function of the rows alone, or the replayed page would differ from
            // the logged one. A compaction whose page has no room for the
            // arriving row is therefore not a tighter compaction, it is a split.
            let mut compacted = compact_image(&builder, &source)?;
            if let Some(image) = compacted.as_mut() {
                if !LeafMut::new(image)?.room_for(needed)? {
                    compacted = None;
                }
            }
            match compacted {
                Some(image) => Fit::Compact(image, leaf.right_sibling(), leaf.max_cts()),
                // A split rewrites three pages and needs the rows to outlive the
                // guard, so this is where they are copied - and a split is the
                // rarer half by a wide margin.
                None => Fit::Split(
                    (0..source.len())
                        .map(|row| {
                            (0..source.width())
                                .map(|column| OwnedDatum::from_datum(&source.value(row, column)))
                                .collect()
                        })
                        .collect(),
                    appending,
                ),
            }
        };
        match fit {
            Fit::Compact(image, right, max_cts) => {
                self.compact_into(database, log, page, image, right, max_cts, true)
            }
            Fit::Split(rows, appending) => {
                let borrowed: Vec<Vec<Datum<'_>>> = rows
                    .iter()
                    .map(|row| row.iter().map(OwnedDatum::borrow).collect())
                    .collect();
                // **An append splits lopsidedly.** Half and half is right when
                // rows arrive from everywhere: both pages then have room for
                // the next one wherever it lands. When they arrive in order,
                // the left page is finished the moment it is written and every
                // subsequent row goes to the right, so an even split leaves a
                // permanently half-empty page behind and halves how many rows a
                // page ends up holding. Filling the left and leaving the room
                // on the right is what the rows are actually going to need.
                let fill = if appending { APPEND_FILL } else { SPLIT_FILL };
                self.split(database, log, page, path, &borrowed, fill)
            }
            Fit::Repack(appending) => {
                let (rows, carried) = self.rows_to_repack(database.pool(), page)?;
                let borrowed: Vec<Vec<Datum<'_>>> = rows
                    .iter()
                    .map(|row| row.iter().map(OwnedDatum::borrow).collect())
                    .collect();
                self.repack(database, log, page, path, &borrowed, &carried, appending)
            }
        }
    }

    /// Rebuilds a leaf around rows that may hold out-of-line values.
    ///
    /// Compacts when the rows fit one page and splits when they do not, in both
    /// cases through a spiller that hands back the run an already-out-of-line
    /// value is in. Only the runs the repack did *not* keep are freed, which is
    /// what stops a write to one row of a leaf full of large values from
    /// rewriting every one of them.
    ///
    /// @param database - the file
    /// @param log - where the records go
    /// @param page - the leaf
    /// @param path - the interior pages above it, root first
    /// @param rows - the rows to pack, sorted
    /// @param carried - the reference each already-out-of-line value is in
    /// @param appending - whether the rows are arriving in key order
    #[allow(clippy::too_many_arguments)]
    fn repack(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
        rows: &[Vec<Datum<'_>>],
        carried: &[Vec<Option<ExtentRef>>],
        appending: bool,
    ) -> DbResult<()> {
        let held = self.extents_of(database.pool(), page)?;
        let (old_right, max_cts) = {
            let guard = database.pool().fetch(page)?;
            let leaf = LeafRef::parse(&guard)?;
            (leaf.right_sibling(), leaf.max_cts())
        };
        let builder = LeafBuilder::new(
            self.page_size(),
            self.tree_id(),
            self.columns().to_vec(),
            self.key_columns(),
        )?;
        let fits = !appending
            && matches!(
                builder.pack_with(rows, COMPACT_FILL, Some(&mut Measuring))?,
                Packed::Filled { rows: packed, .. } if packed == rows.len()
            );
        let kept = if fits {
            let (image, kept) = {
                let mut spiller = crate::paged::Carrying {
                    inner: crate::paged::Extender {
                        database,
                        log,
                        tree_id: self.tree_id(),
                        written: Vec::new(),
                    },
                    carried: carried.to_vec(),
                    used: Vec::new(),
                };
                let image = builder.encode_with(rows, Some(&mut spiller))?;
                (image, spiller.used)
            };
            self.compact_into(database, log, page, image, old_right, max_cts, false)?;
            kept
        } else {
            let fill = if appending { APPEND_FILL } else { SPLIT_FILL };
            self.split_carrying(database, log, page, path, rows, carried, fill)?
        };
        for reference in held {
            if kept.contains(&reference) {
                continue;
            }
            crate::paged::free_extent(database, log, reference)?;
        }
        Ok(())
    }

    /// Replaces a leaf with a freshly packed image of the same rows.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf
    /// @param image - the packed page, without its sibling pointer
    /// @param right - the sibling the old page pointed at
    /// @param max_cts - the commit watermark the old page carried
    fn compact_into(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        mut image: Vec<u8>,
        right: PageId,
        max_cts: u64,
        logical: bool,
    ) -> DbResult<()> {
        page::set_right(&mut image, right)?;
        crate::page::write_u64(&mut image, crate::leaf::leaf_header::MAX_CTS, max_cts)?;
        // **The record says what happened, not what the page became.**
        //
        // A compaction is the leaf's own live rows repacked, and it is
        // deterministic: the same rows, the same order, the same fill. Redo
        // replays in LSN order, so when recovery reaches this record the page is
        // in exactly the state it was in when the compaction ran - which means
        // re-running it produces the same bytes as copying them would have.
        //
        // Copying them costs a whole page in the log. On the gate's
        // `write.insert.batch` that was ninety compactions and splits per two
        // thousand inserts - one to two megabytes of log for two hundred and
        // forty kilobytes of rows, while SQLite's rollback journal writes each
        // original page once per transaction and amortises it away.
        //
        // An empty image is what says "re-run it". A record carrying one is
        // still applied by copying, so a log written by an older build still
        // replays.
        //
        // **A compaction that moved an out-of-line value is not deterministic**,
        // because the run it moved into came from the free map and recovery
        // would allocate somewhere else. Such a compaction carries its image, so
        // redo copies rather than re-runs - the `AllocPage` and `WritePage`
        // records for the new run are already in the log ahead of it.
        // **What the page was stamped with before this.** A logical record has
        // to be re-derived from the page it started from, and the record used
        // to carry nothing saying which page that was - so a replay that
        // reached it holding a different one could only report that the rows did
        // not fit. `redo::compact_leaf` names both stamps when that happens.
        let from_lsn = {
            let guard = database.pool().fetch(page)?;
            crate::page::read_u64(&guard, page::header::LSN)?
        };
        let lsn = log.log(Body::CompactLeaf {
            tree: self.tree_id(),
            page: page.0,
            image: if logical { &[] } else { &image },
            from_lsn,
        })?;
        page::write_u64(&mut image, page::header::LSN, lsn)?;
        database.install(page, &image)?;
        let mut stats = self.stats.get();
        stats.compactions = stats.compactions.saturating_add(1);
        self.stats.set(stats);
        Ok(())
    }

    /// Splits a leaf in two, giving the right half a new page.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf being split
    /// @param path - the interior pages above it, root first
    /// @param rows - its live rows, sorted
    /// @param fill - how full to pack the left half
    fn split(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
        rows: &[Vec<Datum<'_>>],
        fill: f64,
    ) -> DbResult<()> {
        // A leaf with no out-of-line values carries none, and the split then
        // spills nothing because nothing is over the threshold.
        let carried: Vec<Vec<Option<ExtentRef>>> =
            rows.iter().map(|row| vec![None; row.len()]).collect();
        self.split_carrying(database, log, page, path, rows, &carried, fill)?;
        Ok(())
    }

    /// Splits a leaf, keeping the runs its out-of-line values are already in.
    ///
    /// Returns the carried references the two halves kept, so the caller frees
    /// exactly the ones they did not.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf being split
    /// @param path - the interior pages above it, root first
    /// @param rows - its live rows, sorted
    /// @param carried - the reference each already-out-of-line value is in
    /// @param fill - how full to pack the left half
    #[allow(clippy::too_many_arguments)]
    fn split_carrying(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
        rows: &[Vec<Datum<'_>>],
        carried: &[Vec<Option<ExtentRef>>],
        fill: f64,
    ) -> DbResult<Vec<ExtentRef>> {
        if rows.len() < 2 {
            return Err(misuse(
                "a leaf holding fewer than two rows cannot be split; its one row's keys \
                 and fixed-width columns alone are larger than a page",
            ));
        }
        let builder = LeafBuilder::new(
            self.page_size(),
            self.tree_id(),
            self.columns().to_vec(),
            self.key_columns(),
        )?;
        // **The measuring pack does not spill and the encoding ones do.** The
        // measure only asks how many rows fit, and a spiller there would write
        // runs for values the encode is about to write again. The two agree
        // about the count because they use the same threshold; what differs is
        // only whether the bytes are moved.
        let taken = match builder.pack_with(rows, fill, Some(&mut Measuring))? {
            Packed::Filled { rows: packed, .. } => packed.max(1).min(rows.len().saturating_sub(1)),
            Packed::RowTooLarge => {
                return Err(misuse(
                    "a row's keys and fixed-width columns alone are larger than half a page",
                ))
            }
        };
        let left_rows = rows.get(..taken).unwrap_or(&[]);
        let right_rows = rows.get(taken..).unwrap_or(&[]);
        // The two halves are encoded with their own slices of the carried table,
        // because the spiller is asked by *position among the rows it is
        // packing* and the right half's first row is row zero to it.
        let (mut left_image, mut right_image, kept) = {
            let mut spiller = crate::paged::Carrying {
                inner: crate::paged::Extender {
                    database,
                    log,
                    tree_id: self.tree_id(),
                    written: Vec::new(),
                },
                carried: carried.get(..taken).unwrap_or(&[]).to_vec(),
                used: Vec::new(),
            };
            let left = builder.encode_with(left_rows, Some(&mut spiller))?;
            spiller.carried = carried.get(taken..).unwrap_or(&[]).to_vec();
            let right = builder.encode_with(right_rows, Some(&mut spiller))?;
            (left, right, spiller.used)
        };
        let separator = {
            let head: Vec<Datum<'_>> = right_rows
                .first()
                .map(|row| row.iter().copied().take(self.key_columns()).collect())
                .unwrap_or_default();
            self.encode_key(&head)
        };
        let (old_right, max_cts) = {
            let guard = database.pool().fetch(page)?;
            let leaf = LeafRef::parse(&guard)?;
            (leaf.right_sibling(), leaf.max_cts())
        };
        crate::page::write_u64(&mut left_image, crate::leaf::leaf_header::MAX_CTS, max_cts)?;
        crate::page::write_u64(&mut right_image, crate::leaf::leaf_header::MAX_CTS, max_cts)?;

        let right_page = database.allocate(1)?;
        log.log(Body::AllocPage { page: right_page.0 })?;
        page::set_right(&mut right_image, old_right)?;

        // Splitting the root is the one case where the left half moves. The
        // root's page id has to stay what it is - the catalog names it - so the
        // root becomes an interior page and both halves get fresh pages.
        let left_page = if path.is_empty() {
            let moved = database.allocate(1)?;
            log.log(Body::AllocPage { page: moved.0 })?;
            moved
        } else {
            page
        };
        page::set_right(&mut left_image, right_page)?;

        let parent = if path.is_empty() {
            self.build_root(database, log, left_page, &separator, right_page)?
        } else {
            self.insert_separator(database, log, page, path, &separator, right_page)?
        };
        let parent_image = {
            let guard = database.pool().fetch(parent)?;
            guard.bytes().to_vec()
        };
        let lsn = log.log(Body::Structural {
            kind: Structural::Split,
            tree: self.tree_id(),
            left: left_page.0,
            right: right_page.0,
            parent: parent.0,
            left_image: &left_image,
            right_image: &right_image,
            parent_image: &parent_image,
        })?;
        page::write_u64(&mut left_image, page::header::LSN, lsn)?;
        page::write_u64(&mut right_image, page::header::LSN, lsn)?;
        database.install(left_page, &left_image)?;
        database.install(right_page, &right_image)?;
        database.pool().modify(parent, |bytes| {
            page::write_u64(bytes, page::header::LSN, lsn)
        })?;

        if page == self.first_leaf() {
            self.note_first_leaf(left_page);
        }
        self.note_leaves(1);
        let mut stats = self.stats.get();
        stats.splits = stats.splits.saturating_add(1);
        self.stats.set(stats);
        Ok(kept)
    }

    /// Rewrites the root page as an interior with two children.
    ///
    /// The root's own contents have already been moved into `left` by the
    /// caller, so this only has to write the new root and record that the tree
    /// is a level taller.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param left - the page holding what the root used to hold
    /// @param separator - the right half's first key
    /// @param right - the right half's page
    fn build_root(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        left: PageId,
        separator: &[u8],
        right: PageId,
    ) -> DbResult<PageId> {
        let level = self.height().saturating_add(1);
        let builder = InteriorBuilder::new(self.page_size(), self.tree_id(), level)?;
        let mut image = builder.build(
            &[separator],
            &[Swip::unswizzled(left), Swip::unswizzled(right)],
        )?;
        let root = self.root();
        let lsn = log.log(Body::WritePage {
            page: root.0,
            image: &image,
        })?;
        page::write_u64(&mut image, page::header::LSN, lsn)?;
        database.install(root, &image)?;
        self.note_height(level);
        Ok(root)
    }

    /// Adds a separator and a right child above `left`.
    ///
    /// Returns the page the separator landed in, which is what the caller
    /// stamps with the split's LSN.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param left - the page that was split
    /// @param path - the interior pages above `left`, root first
    /// @param separator - the right half's first key, encoded
    /// @param right - the right half's page
    fn insert_separator(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        left: PageId,
        path: &[PageId],
        separator: &[u8],
        right: PageId,
    ) -> DbResult<PageId> {
        let parent = path
            .last()
            .copied()
            .ok_or_else(|| corrupt("a split with no parent should have grown the root"))?;
        let ancestors = path.get(..path.len().saturating_sub(1)).unwrap_or(&[]);
        let (mut separators, mut children, level) = self.read_interior(database.pool(), parent)?;
        let position = children
            .iter()
            .position(|page| *page == left)
            .ok_or_else(|| corrupt("a child is not in the parent that routes to it"))?;
        separators.insert(position, separator.to_vec());
        children.insert(position.saturating_add(1), right);

        let builder = InteriorBuilder::new(self.page_size(), self.tree_id(), level)?;
        let keys: Vec<&[u8]> = separators.iter().map(Vec::as_slice).collect();
        if builder.fits(&keys) {
            let swips: Vec<Swip> = children
                .iter()
                .map(|page| Swip::unswizzled(*page))
                .collect();
            let mut image = builder.build(&keys, &swips)?;
            let lsn = log.log(Body::WritePage {
                page: parent.0,
                image: &image,
            })?;
            page::write_u64(&mut image, page::header::LSN, lsn)?;
            database.install(parent, &image)?;
            return Ok(parent);
        }

        // The parent is full, so it splits too: the same shape one level up. The
        // middle separator is *promoted* rather than copied, which is what keeps
        // an interior page's separators strictly between its children's ranges.
        let middle = children.len() / 2;
        let promoted = separators
            .get(middle.saturating_sub(1))
            .cloned()
            .ok_or_else(|| corrupt("an interior page with no separator to promote"))?;
        let left_keys: Vec<&[u8]> = separators
            .get(..middle.saturating_sub(1))
            .unwrap_or(&[])
            .iter()
            .map(Vec::as_slice)
            .collect();
        let right_keys: Vec<&[u8]> = separators
            .get(middle..)
            .unwrap_or(&[])
            .iter()
            .map(Vec::as_slice)
            .collect();
        let left_children: Vec<PageId> = children.get(..middle).unwrap_or(&[]).to_vec();
        let right_children: Vec<PageId> = children.get(middle..).unwrap_or(&[]).to_vec();
        let left_swips: Vec<Swip> = left_children
            .iter()
            .map(|page| Swip::unswizzled(*page))
            .collect();
        let right_swips: Vec<Swip> = right_children
            .iter()
            .map(|page| Swip::unswizzled(*page))
            .collect();
        let mut right_image = builder.build(&right_keys, &right_swips)?;
        let sibling = database.allocate(1)?;
        log.log(Body::AllocPage { page: sibling.0 })?;

        // Which half the caller's child ended up in decides which page it has to
        // stamp, and it is decided here rather than rediscovered afterwards.
        let landed = if left_children.contains(&right) {
            parent
        } else {
            sibling
        };

        // The right half is written first, because promoting the separator may
        // rewrite this same parent again one level up and the promotion has to
        // see a page whose children are already the left half's.
        let mut left_image = builder.build(&left_keys, &left_swips)?;
        let left_lsn = log.log(Body::WritePage {
            page: parent.0,
            image: &left_image,
        })?;
        page::write_u64(&mut left_image, page::header::LSN, left_lsn)?;
        database.install(parent, &left_image)?;
        let right_lsn = log.log(Body::WritePage {
            page: sibling.0,
            image: &right_image,
        })?;
        page::write_u64(&mut right_image, page::header::LSN, right_lsn)?;
        database.install(sibling, &right_image)?;

        if ancestors.is_empty() {
            // The root split. Its contents are already in `parent`, and the
            // root itself becomes an interior above the two halves - so the
            // page that was the root has to be moved out of the way first.
            let moved = database.allocate(1)?;
            log.log(Body::AllocPage { page: moved.0 })?;
            let mut moved_image = left_image.clone();
            let moved_lsn = log.log(Body::WritePage {
                page: moved.0,
                image: &moved_image,
            })?;
            page::write_u64(&mut moved_image, page::header::LSN, moved_lsn)?;
            database.install(moved, &moved_image)?;
            self.build_root(database, log, moved, &promoted, sibling)?;
            return Ok(if landed == parent { moved } else { sibling });
        }
        self.insert_separator(database, log, parent, ancestors, &promoted, sibling)?;
        Ok(landed)
    }

    /// Merges a leaf with its right sibling when the two fit in one page.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf that just lost a row
    /// @param path - the interior pages above it, root first
    fn merge_if_small(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
    ) -> DbResult<()> {
        // **The cheap question first.** A merge is only possible when a leaf has
        // actually emptied, and that is a row count and a popcount; deciding it
        // by packing both leaves is one `Vec` per row plus one heap allocation
        // per text and blob in *both* of them - about five hundred allocations
        // and a full page pack, thrown away.
        //
        // It answered "no" on every one of the gate's two thousand deletes, and
        // it was the whole of the cost: `write.delete` spent 290 us per
        // statement on three tree deletes that should cost a descent each.
        //
        // The condition is the classic one - a leaf underflows when it holds
        // fewer than half the rows it was packed with - and it is asked of the
        // leaf the delete emptied, not of its sibling. Asking it of both blocked
        // every merge in a tree emptied in key order, where the left leaf goes
        // first and its sibling is still full; the sibling's size is the pack's
        // question and the pack still asks it.
        let (right, underflowed) = {
            let guard = database.pool().fetch(page)?;
            let leaf = LeafRef::parse(&guard)?;
            (leaf.right_sibling(), underflows(&leaf)?)
        };
        if !underflowed || right.is_none() {
            return Ok(());
        }
        let Some(parent) = path.last().copied() else {
            return Ok(());
        };
        let (mut separators, mut children, level) = self.read_interior(database.pool(), parent)?;
        let Some(position) = children.iter().position(|held| *held == page) else {
            return Ok(());
        };
        if children.get(position.saturating_add(1)).copied() != Some(right) {
            // The right sibling is under another parent. Merging across that
            // boundary rewrites two interior pages and the separator between
            // them, and a half-empty leaf is cheaper than the code that would.
            return Ok(());
        }

        let (mut rows, mut carried) = self.rows_to_repack(database.pool(), page)?;
        let (right_rows, right_carried) = self.rows_to_repack(database.pool(), right)?;
        rows.extend(right_rows);
        carried.extend(right_carried);
        let mut doomed = self.extents_of(database.pool(), page)?;
        doomed.extend(self.extents_of(database.pool(), right)?);
        let builder = LeafBuilder::new(
            self.page_size(),
            self.tree_id(),
            self.columns().to_vec(),
            self.key_columns(),
        )?;
        let borrowed: Vec<Vec<Datum<'_>>> = rows
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        // Measured without spilling and then encoded with it, for the reason
        // `split` gives: a spiller in the measure would write runs the encode
        // then writes again.
        let fits = matches!(
            builder.pack_with(&borrowed, COMPACT_FILL, Some(&mut Measuring))?,
            Packed::Filled { rows: packed, .. } if packed == borrowed.len()
        );
        if !fits {
            // They do not fit, which is the ordinary answer for two leaves that
            // are merely a bit empty. Nothing to do.
            return Ok(());
        }
        let (mut merged, kept) = {
            let mut spiller = crate::paged::Carrying {
                inner: crate::paged::Extender {
                    database,
                    log,
                    tree_id: self.tree_id(),
                    written: Vec::new(),
                },
                carried: carried.clone(),
                used: Vec::new(),
            };
            let image = builder.encode_with(&borrowed, Some(&mut spiller))?;
            (image, spiller.used)
        };
        // A merge that emptied the parent of every separator would leave an
        // interior page with one child, which is legal but pointless, and an
        // interior page with *no* children, which is not. Refusing keeps the
        // tree's shape simple at the cost of one under-filled leaf.
        if separators.len() < 2 && !path.len().eq(&1) {
            return Ok(());
        }
        if separators.is_empty() {
            return Ok(());
        }

        let (far_right, max_cts) = {
            let guard = database.pool().fetch(right)?;
            let leaf = LeafRef::parse(&guard)?;
            (leaf.right_sibling(), leaf.max_cts())
        };
        let mine = {
            let guard = database.pool().fetch(page)?;
            LeafRef::parse(&guard)?.max_cts()
        };
        page::set_right(&mut merged, far_right)?;
        crate::page::write_u64(
            &mut merged,
            crate::leaf::leaf_header::MAX_CTS,
            max_cts.max(mine),
        )?;

        separators.remove(position);
        children.remove(position.saturating_add(1));
        let interior = InteriorBuilder::new(self.page_size(), self.tree_id(), level)?;
        let keys: Vec<&[u8]> = separators.iter().map(Vec::as_slice).collect();
        let swips: Vec<Swip> = children
            .iter()
            .map(|held| Swip::unswizzled(*held))
            .collect();
        let mut parent_image = interior.build(&keys, &swips)?;
        let empty = builder.encode_empty()?;

        let lsn = log.log(Body::Structural {
            kind: Structural::Merge,
            tree: self.tree_id(),
            left: page.0,
            right: right.0,
            parent: parent.0,
            left_image: &merged,
            right_image: &empty,
            parent_image: &parent_image,
        })?;
        page::write_u64(&mut merged, page::header::LSN, lsn)?;
        page::write_u64(&mut parent_image, page::header::LSN, lsn)?;
        database.install(page, &merged)?;
        database.install(parent, &parent_image)?;
        log.log(Body::FreePage { page: right.0 })?;
        database.release(right, 1)?;
        // The merged page's out-of-line values went into new runs, so both
        // leaves' old runs are dead. Freed after the install rather than before,
        // so a failure between the two leaks pages rather than leaving the new
        // page pointing at pages the free map has handed out again.
        for reference in doomed {
            if kept.contains(&reference) {
                continue;
            }
            crate::paged::free_extent(database, log, reference)?;
        }
        self.note_leaves(-1);
        let mut stats = self.stats.get();
        stats.merges = stats.merges.saturating_add(1);
        self.stats.set(stats);
        Ok(())
    }

    /// Returns the leaf a key belongs in.
    ///
    /// @param pool - the buffer pool
    /// @param encoded_key - the key's comparable bytes
    fn leaf_for(&self, pool: &Pool, encoded_key: &[u8]) -> DbResult<(PageId, Vec<PageId>)> {
        let descent = self.descend(pool, encoded_key)?;
        let path: Vec<PageId> = descent.steps.iter().map(|step| step.0).collect();
        Ok((descent.leaf, path))
    }

    /// Returns where a key sits in a leaf.
    ///
    /// @param pool - the buffer pool
    /// @param page - the leaf
    /// @param key - the key, one value per key column
    pub fn locate(&self, pool: &Pool, page: PageId, key: &[Datum<'_>]) -> DbResult<Located> {
        let guard = pool.fetch(page)?;
        let leaf = LeafRef::parse(&guard)?
            .with_collations(self.collations())
            .with_directions(self.directions());
        leaf.locate(key, self.key_columns())
    }

    /// Returns the row a key names in a leaf, copied out.
    ///
    /// @param pool - the buffer pool
    /// @param page - the leaf
    /// @param key - the key
    fn row_at(
        &self,
        pool: &Pool,
        page: PageId,
        key: &[Datum<'_>],
    ) -> DbResult<Option<Vec<OwnedDatum>>> {
        let guard = pool.fetch(page)?;
        let leaf = LeafRef::parse(&guard)?
            .with_collations(self.collations())
            .with_directions(self.directions());
        match self.locate_in(&leaf, key)? {
            Located::Sorted(row) => {
                // The row's own out-of-line values, not the leaf's: a delete
                // reads one row out of a leaf that may hold hundreds.
                let held = self.read_extents_row(pool, &leaf, row)?;
                let leaf = leaf.with_extents(&held);
                let mut values = Vec::with_capacity(leaf.column_count());
                for column in 0..leaf.column_count() {
                    values.push(OwnedDatum::from_datum(&leaf.value(row, column)?));
                }
                Ok(Some(values))
            }
            Located::Delta(index) => {
                let held = self.read_extents_delta(pool, &leaf, index)?;
                let leaf = leaf.with_extents(&held);
                let mut values = Vec::with_capacity(leaf.column_count());
                for column in 0..leaf.column_count() {
                    values.push(OwnedDatum::from_datum(&leaf.delta_value(index, column)?));
                }
                Ok(Some(values))
            }
            Located::Absent => Ok(None),
        }
    }

    /// Returns where a key sits in a leaf already parsed.
    ///
    /// @param leaf - the leaf
    /// @param key - the key
    fn locate_in(&self, leaf: &LeafRef<'_>, key: &[Datum<'_>]) -> DbResult<Located> {
        leaf.locate(key, self.key_columns())
    }

    /// Returns a leaf's live rows without reading a single out-of-line value.
    ///
    /// **This is the reader a repack uses, and the difference from
    /// `live_rows_of` is the whole of what makes a repack affordable.** A leaf
    /// whose values are out of line holds a great many rows - the leaf is
    /// sixteen bytes per value rather than four kilobytes - so materialising
    /// them all to move one would read and rewrite the lot. Instead each
    /// out-of-line value comes back as a placeholder of its own length, which is
    /// all the builder's sizing needs, and its reference travels beside it so
    /// the spiller can hand the same run back.
    ///
    /// The placeholder never leaves the repack: the builder classifies it as
    /// out-of-line - which it is, by length - and asks the spiller for it, and
    /// the spiller answers with the reference rather than writing the
    /// placeholder anywhere.
    ///
    /// @param pool - the buffer pool
    /// @param page - the leaf
    fn rows_to_repack(&self, pool: &Pool, page: PageId) -> DbResult<Repacked> {
        let guard = pool.fetch(page)?;
        let leaf = LeafRef::parse(&guard)?
            .with_collations(self.collations())
            .with_directions(self.directions());
        if !leaf.has_extents() {
            let rows: Vec<Vec<OwnedDatum>> = leaf
                .live()?
                .iter()
                .map(|row| row.iter().map(OwnedDatum::from_datum).collect())
                .collect();
            let carried = rows.iter().map(|row| vec![None; row.len()]).collect();
            return Ok((rows, carried));
        }
        // A leaf with extents is read row by row rather than through `live`,
        // because `live` would resolve them - which is the read this exists to
        // avoid. Both regions can hold one: the sorted region says so in its
        // class array, a delta row says so with a tag.
        let mut rows = Vec::with_capacity(leaf.row_count());
        let mut carried = Vec::with_capacity(leaf.row_count());
        for row in 0..leaf.row_count() {
            if leaf.is_tombstoned(row)? {
                continue;
            }
            let mut values = Vec::with_capacity(leaf.column_count());
            let mut refs = Vec::with_capacity(leaf.column_count());
            for column in 0..leaf.column_count() {
                if leaf.column(column)?.class_at(row)? == crate::types::ValueClass::Extent {
                    let reference = leaf.extent_at(row, column)?;
                    let blob = matches!(
                        self.columns().get(column).map(|spec| spec.physical),
                        Some(crate::types::PhysicalType::Blob)
                    );
                    let filler = vec![0u8; reference.length as usize];
                    values.push(if blob {
                        OwnedDatum::Blob(filler)
                    } else {
                        OwnedDatum::Text(filler)
                    });
                    refs.push(Some(reference));
                    continue;
                }
                values.push(OwnedDatum::from_datum(&leaf.value(row, column)?));
                refs.push(None);
            }
            rows.push(values);
            carried.push(refs);
        }
        // The delta area, which cannot hold an out-of-line value but can hold
        // ordinary ones written since the leaf was packed. A delta row whose key
        // is already here shadows the sorted one, which is what `live` does and
        // is the belt on top of the write path's braces: the write removes the
        // entry it shadows, and a page recovery replayed rather than this
        // process built may not have.
        for index in 0..leaf.delta_count() {
            let mut values = Vec::with_capacity(leaf.column_count());
            let mut refs = Vec::with_capacity(leaf.column_count());
            for column in 0..leaf.column_count() {
                if let Some(reference) = leaf.delta_extent_at(index, column)? {
                    let blob = matches!(
                        self.columns().get(column).map(|spec| spec.physical),
                        Some(crate::types::PhysicalType::Blob)
                    );
                    let filler = vec![0u8; reference.length as usize];
                    values.push(if blob {
                        OwnedDatum::Blob(filler)
                    } else {
                        OwnedDatum::Text(filler)
                    });
                    refs.push(Some(reference));
                    continue;
                }
                values.push(OwnedDatum::from_datum(&leaf.delta_value(index, column)?));
                refs.push(None);
            }
            let head: Vec<Datum<'_>> = values
                .iter()
                .take(self.key_columns())
                .map(OwnedDatum::borrow)
                .collect();
            let shadowed = rows.iter().position(|held| {
                let other: Vec<Datum<'_>> = held
                    .iter()
                    .take(self.key_columns())
                    .map(OwnedDatum::borrow)
                    .collect();
                crate::leaf::compare_rows(&other, &head, self.key_columns())
                    == std::cmp::Ordering::Equal
            });
            match shadowed {
                Some(at) => {
                    if let Some(slot) = rows.get_mut(at) {
                        *slot = values;
                    }
                    if let Some(slot) = carried.get_mut(at) {
                        // The delta row's own references, which are not the
                        // sorted row's: whatever extent the row it shadows was
                        // in is no longer named by anything, and the caller
                        // frees exactly what the repack did not keep.
                        *slot = refs;
                    }
                }
                None => {
                    carried.push(refs);
                    rows.push(values);
                }
            }
        }
        // **Sorted, because the builder packs and does not sort.** `live` sorts
        // its merge and the fast path relies on it; this reader is the merge for
        // a leaf with extents and has to do the same. A repack that handed the
        // builder a delta row after the sorted rows it sorts before produced a
        // leaf whose keys did not increase - which every later descent then
        // missed, so an upsert inserted a duplicate rather than replacing, and
        // `wide` ended a write campaign with 791 rows where SQLite had 500.
        self.in_key_order(&mut rows, &mut carried);
        Ok((rows, carried))
    }

    /// Sorts rows and their carried references together, by key.
    ///
    /// @param rows - the rows
    /// @param carried - the reference each already-out-of-line value is in
    fn in_key_order(
        &self,
        rows: &mut Vec<Vec<OwnedDatum>>,
        carried: &mut Vec<Vec<Option<ExtentRef>>>,
    ) {
        let key_columns = self.key_columns();
        let mut paired: Vec<(Vec<OwnedDatum>, Vec<Option<ExtentRef>>)> = std::mem::take(rows)
            .into_iter()
            .zip(std::mem::take(carried))
            .collect();
        paired.sort_by(|left, right| {
            let one: Vec<Datum<'_>> = left
                .0
                .iter()
                .take(key_columns)
                .map(OwnedDatum::borrow)
                .collect();
            let two: Vec<Datum<'_>> = right
                .0
                .iter()
                .take(key_columns)
                .map(OwnedDatum::borrow)
                .collect();
            crate::leaf::compare_rows(&one, &two, key_columns)
        });
        for (row, refs) in paired {
            rows.push(row);
            carried.push(refs);
        }
    }

    /// Returns the references every out-of-line value in a leaf names.
    ///
    /// @param pool - the buffer pool
    /// @param page - the leaf
    fn extents_of(&self, pool: &Pool, page: PageId) -> DbResult<Vec<ExtentRef>> {
        let guard = pool.fetch(page)?;
        let leaf = LeafRef::parse(&guard)?;
        PagedTree::extent_refs(&leaf)
    }

    /// Returns an interior page's separators, children and level.
    ///
    /// @param pool - the buffer pool
    /// @param page - the interior page
    fn read_interior(
        &self,
        pool: &Pool,
        page: PageId,
    ) -> DbResult<(Vec<Vec<u8>>, Vec<PageId>, u16)> {
        let guard = pool.fetch(page)?;
        let interior = InteriorRef::parse(&guard)?;
        let mut separators = Vec::with_capacity(interior.count());
        for slot in 0..interior.count() {
            separators.push(interior.key(slot)?.to_vec());
        }
        let mut children = Vec::with_capacity(interior.children());
        for child in 0..interior.children() {
            children.push(pool.page_of_swip(interior.swip(child)?)?);
        }
        Ok((separators, children, interior.level()))
    }
}
