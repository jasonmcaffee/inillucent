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
//! A leaf refuses an insert when the row would collide with the mini-columns:
//! the delta area is as large as the free gap, and nothing else limits it. The
//! caller **compacts**: the live rows are merged into key order and packed into
//! a fresh page with the delta area empty again - or, when they still fit the
//! page's own slot widths, spliced into the page it has (`leaf/splice.rs`). If they do not all fit, the leaf **splits**. So an
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
use crate::leaf::{ImageTiming, LeafBuilder, LeafRef, Packed, Rows};
use crate::mutate::{DeltaOffsets, DeltaPlan, LeafMut};
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

/// Returns how many of a split's rows go to the left half.
///
/// **`fill` is how full to *prefer* the left half, not what a row has to fit
/// inside (task-2033).** The left half takes at least one row whatever the
/// measure said, so a fill that holds none is not the question being asked;
/// the question is whether a whole page holds one. Measuring only against
/// [`SPLIT_FILL`] refused every row whose fixed part is over half a page, and
/// on a small page that is an ordinary row: a ten-column leaf spends 384 bytes
/// on its directory and mini-columns before any value, so at a 512-byte page
/// the schema catalog's own rows are "larger than half a page" and the third
/// `CREATE TABLE` in such a database could not be written.
///
/// The refusal that is left is a row no page can hold, which is a statement
/// this engine will not run rather than a damaged file - see the sentence
/// [`PagedTree::split_carrying`] answers a single row with.
///
/// @param builder - the packer, at this tree's page size and columns
/// @param rows - the rows being split, sorted by key
/// @param fill - how full to prefer the left half
fn rows_for_the_left_half(
    builder: &LeafBuilder,
    rows: &[Vec<Datum<'_>>],
    fill: f64,
) -> DbResult<usize> {
    let most = rows.len().saturating_sub(1);
    match builder.pack_with(rows, fill, Some(&mut Measuring))? {
        Packed::Filled { rows: packed, .. } => Ok(packed.max(1).min(most)),
        Packed::RowTooLarge => match builder.pack_with(rows, 1.0, Some(&mut Measuring))? {
            Packed::Filled { .. } => Ok(1.min(most)),
            Packed::RowTooLarge => Err(misuse(
                "a row's keys and fixed-width columns alone are larger than a page",
            )),
        },
    }
}

/// What a leaf that has run out of room turns out to need.
///
/// Decided while the page is still borrowed, and acted on after the borrow ends
/// - a compaction and a split both write the page they are reading from.
enum Fit {
    /// The leaf holds out-of-line values and has to be rebuilt with the file in
    /// hand, because moving one needs a run of pages allocated.
    Repack(bool),
    /// The live rows fit one page: this image, the two header fields the old
    /// page carried that a fresh pack does not know about, and whether the log
    /// record can leave the image out.
    ///
    /// **The flag is false in two cases.** A leaf a splice was allowed on,
    /// whose spliced page had no room for the arriving row, repacked instead:
    /// recovery re-runs a compaction from the page and does not know the row, so
    /// it would splice, which is a different page from the one the write
    /// produced. And any leaf format 1 wrote: recovery re-runs a compaction of
    /// such a leaf by format 1's rule, which is what a format 1 log needs, and
    /// this build's compaction of it is not that. In both the record carries
    /// the page and recovery copies it.
    Compact(Vec<u8>, PageId, u64, bool),
    /// They do not, so the leaf splits and the rows have to outlive the borrow.
    /// The flag says whether the rows are arriving in key order.
    Split(Vec<Vec<OwnedDatum>>, bool),
    /// Neither will help, because the leaf holds fewer than two live rows: a
    /// compaction of one row produces the page that is already there, and a
    /// split needs two rows to have something to put on each side. The caller
    /// packs the arriving row into the leaf itself rather than making room for
    /// it in the delta area - see [`Tree::pack_row_into_leaf`].
    Stuck,
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
/// **One rung, not two, and the reason is that the second one produced the same page
/// as the first** (task-2006). This tried `COMPACT_FILL` and then `TIGHT_FILL`, and
/// `pack_all_rows` is a sizing pass followed by an encode - so a rung that fails costs
/// a whole pass over every value of the leaf for nothing. Its own comment recorded that
/// "a leaf that arrived from a bulk build fails the first rung every time", and a bulk
/// build packs to `BULK_FILL` of 0.9, so every compaction of such a leaf paid for three
/// passes where two would do.
///
/// The two rungs cannot differ in what they produce. A fill is a cap on how many rows
/// are placed, and `pack_all_rows` returns `None` unless *all* of them are - so when a
/// rung succeeds, the rows placed are the same rows, the slot widths those rows force
/// are the same widths, and the encode is handed the same layout. The page is
/// byte-identical. What the looser rung decided was not the bytes but whether to
/// compact at all, and the tight rung decides that in strictly more cases. So
/// `COMPACT_FILL`'s role here was to answer "no" to a leaf that `TIGHT_FILL` then said
/// "yes" to, one pass later, with the same answer.
///
/// `COMPACT_FILL` still means what it says everywhere else: `pack_rows` uses it to
/// leave a leaf room for the delta rows that follow, which is a decision about how many
/// rows to *place* and is exactly the decision a compaction does not get to make.
///
/// Measured on `inillucent-writelogattrib`, 2,000 inserts into a table carrying two
/// secondary indexes at a 32 KiB page: `compact_image` was 16.28 ms of a 46.04 ms
/// transaction, which was 35% of it and 53% of what the two indexes cost. It is
/// **3.45 ms of 29.60** now - 1.16 ms pricing the page and 2.22 ms writing it - after
/// the sizing pass stopped re-pricing the whole leaf on every row (task-2024), which
/// is `LeafBuilder::fit_all_widths`.
///
/// @param builder - the leaf builder for this tree
/// @param rows - the leaf's live rows, sorted
pub fn compact_image<'d>(
    builder: &LeafBuilder,
    rows: &dyn crate::leaf::Rows<'d>,
) -> DbResult<Option<Vec<u8>>> {
    let mut timing = ImageTiming::default();
    compact_image_timed(builder, rows, &mut timing)
}

/// [`compact_image`], saying what its sizing pass and its encode each cost.
///
/// **So that what a splice could remove is a measured number rather than a share
/// of a stage.** The sizing pass prices the page and settles the slot widths, and
/// a caller that moved slots instead of re-encoding values would still have to run
/// it; the encode is the half such a caller would replace. They were timed together
/// as `image_nanos`, and the split between them moves with the page size - which is
/// why a figure taken at one page size does not answer the question at another.
///
/// @param builder - the leaf builder for this tree
/// @param rows - the leaf's live rows, sorted
/// @param timing - where the cost of the two passes is added
pub fn compact_image_timed<'d>(
    builder: &LeafBuilder,
    rows: &dyn crate::leaf::Rows<'d>,
    timing: &mut ImageTiming,
) -> DbResult<Option<Vec<u8>>> {
    // `pack_all_rows` rather than `pack`: a rung that cannot hold every row costs one
    // sizing pass here, where `pack` would encode a whole page image and then have it
    // discarded.
    builder.pack_all_rows_timed(rows, TIGHT_FILL, timing)
}

/// Compacts a leaf the way a replayed `CompactLeaf` record without an image does.
///
/// **The rule recovery follows, in one place so the write path cannot drift
/// from it.** A splice when the page allows one and a repack otherwise, both
/// decided from the page alone. The write path makes the same choice first and
/// logs the image only when it had to depart from it - see `Fit::Compact`.
///
/// `None` when the live rows do not fit one page at all, which a replay reports
/// as corruption: the write that logged the record had them fit.
///
/// **A leaf format 1 wrote is compacted by format 1's rule**: a repack, and the
/// result left in format 1's layout. The only compaction of such a leaf that is
/// logged without its image is one a format 1 build logged - this build logs
/// the image whenever it compacts one - so a replay here is replaying a format
/// 1 log, and every later record in that log was written against a page in
/// format 1's layout.
///
/// @param builder - the leaf builder for this tree
/// @param leaf - the leaf, parsed with its tree's collations and directions
/// @param page_size - the database's page size
pub fn replay_compaction(
    builder: &LeafBuilder,
    leaf: &LeafRef<'_>,
    page_size: usize,
) -> DbResult<Option<Vec<u8>>> {
    let order = leaf.live_order()?;
    if !leaf.has_delta_directory() {
        let Some(mut image) = compact_image(builder, &order.materialise()?)? else {
            return Ok(None);
        };
        // The repack is the same bytes format 1's builder wrote, save the flag
        // that says which layout the delta area is in; there is no delta area
        // yet, so clearing it is all that makes this format 1's page.
        let flags = image
            .get_mut(page::header::FLAGS)
            .ok_or_else(|| corrupt("a repacked leaf has no flag byte"))?;
        *flags &= !crate::leaf::LEAF_DELTA_DIRECTORY;
        return Ok(Some(image));
    }
    if let Some(image) = crate::leaf::splice_image(leaf, &order, page_size, TIGHT_FILL)? {
        return Ok(Some(image));
    }
    compact_image(builder, &order.materialise()?)
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

/// Where a write's key sits in its leaf, and where its row will go.
///
/// The two answers one search of the leaf gives, carried together from the
/// search to the write: what the write displaces, and the delta directory
/// position its row takes once that is gone.
#[derive(Clone, Copy, Debug)]
struct Landing {
    /// Where the key sits now.
    located: Located,
    /// The delta directory position the row takes.
    slot: usize,
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
    /// Nanoseconds spent compacting a leaf, which is the cheaper of the two and the
    /// one that happens sixty times more often.
    pub compaction_nanos: u128,
    /// Nanoseconds spent splitting a leaf, which rewrites three pages and copies every
    /// row out of the old one first.
    pub split_nanos: u128,
    /// Nanoseconds spent in `LeafRef::live_source`: the tombstone scan, the delta
    /// decode and the merge that puts the delta rows in key order among the sorted ones.
    pub source_nanos: u128,
    /// Nanoseconds spent in `compact_image`: one or two sizing passes and one encode of
    /// every live row.
    pub image_nanos: u128,
    /// Nanoseconds of `source_nanos` spent deciding *which* rows are live, rather than
    /// reading them: the tombstone scan, the delta decode and the merge.
    ///
    /// **Split out because a compaction that spliced its delta rows in would still pay
    /// this and would not pay the rest.** `source_nanos` on its own cannot say how much
    /// of it such a compaction would keep.
    pub merge_nanos: u128,
    /// Nanoseconds of `image_nanos` spent pricing the page, in `fit_widths`.
    pub sizing_nanos: u128,
    /// Nanoseconds of `image_nanos` spent writing it, in `encode_rows_with`.
    ///
    /// **The one number that bounds what a splice is worth.** A compaction that moved
    /// slots rather than re-encoding values would still size the page, so this - and
    /// the materialisation half of `source_nanos` - is the whole of what it could
    /// remove. Measured separately because the split between sizing and encoding moves
    /// with the page size, and a figure taken at one page size does not answer the
    /// question at another.
    pub encode_nanos: u128,
    /// Nanoseconds spent deciding what will fit and building the image, which for a
    /// compaction is reading every live row of the leaf and encoding it again.
    pub choose_nanos: u128,
    /// Compactions that spliced their delta rows into the page rather than
    /// repacking it (task-2074).
    pub splices: u64,
    /// Nanoseconds spent building spliced images, including the ones a leaf
    /// then could not use because the arriving row did not fit.
    pub splice_nanos: u128,
    /// Writes whose leaf the hint named, costing two key comparisons.
    pub hinted: u64,
    /// Writes that descended the tree from its root to find their leaf.
    pub descended: u64,
    /// Nanoseconds spent making room: compacting a leaf, or splitting one.
    ///
    /// **Because `write.insert.batch`'s cost has been attributed to three different
    /// things and measured to be none of them** (task-2006). The delta walk was
    /// counted at under eight per cent; the split record is 55% of the log's bytes and
    /// about 2% of its time; and `inillucent-writelogattrib` then put the two secondary
    /// indexes at 69% of the transaction and 123 of its 181 leaf compactions. Whether
    /// those compactions are the 69% is the next question, and a number answers it
    /// where arithmetic over four other numbers does not.
    pub room_nanos: u128,
}

impl std::ops::Add for WriteStats {
    type Output = WriteStats;

    /// Adds two trees' counters together, saturating.
    ///
    /// **A struct literal rather than a field at a time, so a counter added later
    /// cannot be left out of a total silently.** The engine adds every tree's
    /// counters up to answer `write_stats`, and it did so by assigning thirteen of
    /// the sixteen fields by name - so the three added for this ticket read zero in
    /// every total that was printed, which is a measurement that looks taken and is
    /// not. This form does not compile until a new field is named here as well.
    ///
    /// @param other - the counters to add to these
    fn add(self, other: WriteStats) -> WriteStats {
        WriteStats {
            inserted: self.inserted.saturating_add(other.inserted),
            deleted: self.deleted.saturating_add(other.deleted),
            updated_in_place: self.updated_in_place.saturating_add(other.updated_in_place),
            compactions: self.compactions.saturating_add(other.compactions),
            splits: self.splits.saturating_add(other.splits),
            merges: self.merges.saturating_add(other.merges),
            compaction_nanos: self.compaction_nanos.saturating_add(other.compaction_nanos),
            split_nanos: self.split_nanos.saturating_add(other.split_nanos),
            source_nanos: self.source_nanos.saturating_add(other.source_nanos),
            image_nanos: self.image_nanos.saturating_add(other.image_nanos),
            merge_nanos: self.merge_nanos.saturating_add(other.merge_nanos),
            sizing_nanos: self.sizing_nanos.saturating_add(other.sizing_nanos),
            encode_nanos: self.encode_nanos.saturating_add(other.encode_nanos),
            choose_nanos: self.choose_nanos.saturating_add(other.choose_nanos),
            splices: self.splices.saturating_add(other.splices),
            splice_nanos: self.splice_nanos.saturating_add(other.splice_nanos),
            hinted: self.hinted.saturating_add(other.hinted),
            descended: self.descended.saturating_add(other.descended),
            room_nanos: self.room_nanos.saturating_add(other.room_nanos),
        }
    }
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
    /// Encodes one row, writing any value too wide for a leaf out of line first.
    ///
    /// **A value too large for a leaf is written out of line before the row is
    /// placed, not instead of placing it.**
    ///
    /// The first version repacked the whole leaf around such a row, because the
    /// builder is where the spiller is. That made a two-kilobyte value cost a
    /// page rewrite and a full-page log record: on the gate's
    /// `extension.fts.build`, three thousand of FTS5's segment blocks fall
    /// between the threshold and what a leaf holds, and the repacks were 111 ms
    /// of a 250 ms workload - more than half of it.
    ///
    /// Spilling first turns the row into one a delta area can hold: the
    /// seventeen tagged bytes of a reference in place of the value. From there
    /// it is an ordinary write, with an ordinary short log record, and the leaf
    /// is repacked when it fills rather than once per wide row.
    ///
    /// Called once by [`Tree::write_row`], outside its retry loop, because a
    /// retry after a compaction must reuse the run rather than write a second
    /// and leak the first.
    ///
    /// The run each value went into is reported alongside the bytes, because
    /// [`Tree::pack_row_into_leaf`] hands the row to the builder a second time
    /// and the builder's spiller has to be told to reuse those runs rather than
    /// write them again. The two agree about which values are out of line
    /// because they use the same threshold and both leave a key column alone:
    /// see [`Tree::out_of_line`] and `LeafBuilder::threshold`.
    ///
    /// @param database - the open database
    /// @param log - where the extent's own record goes
    /// @param row - the values, in tree-column order
    /// @returns the encoded row, whether anything was spilled, and the run each
    /// spilled value is in, one entry per column
    fn encode_row_spilling_wide_values(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        row: &[Datum<'_>],
    ) -> DbResult<(Vec<u8>, bool, Vec<Option<ExtentRef>>)> {
        let mut spilled = false;
        let mut encoded_row = Vec::new();
        let mut runs: Vec<Option<ExtentRef>> = vec![None; row.len()];
        for (column, value) in row.iter().enumerate() {
            match self.out_of_line(column, value) {
                Some(bytes) => {
                    let reference =
                        crate::paged::write_extent(database, log, self.tree_id(), bytes)?;
                    if let Some(slot) = runs.get_mut(column) {
                        *slot = Some(reference);
                    }
                    // The reference says what the bytes are whenever the column
                    // would say something else, which is what lets a column
                    // declared `BLOB` - or declared nothing - hold a text out
                    // of line at all (task-1986).
                    let physical = self
                        .columns()
                        .get(column)
                        .map(|spec| spec.physical)
                        .unwrap_or(crate::types::PhysicalType::Any);
                    crate::leaf::encode_extent_tagged(
                        &mut encoded_row,
                        reference.stating(crate::leaf::extent_class_for(physical, value)),
                    );
                    spilled = true;
                }
                None => value.encode_tagged(&mut encoded_row),
            }
        }
        Ok((encoded_row, spilled, runs))
    }

    /// Finds the key in a leaf and reads the row that was there.
    ///
    /// **The key is found once.** Where it sits decides three things - whether
    /// it was there, what the caller gets back, and what the mutation has to
    /// displace - and the first version asked the page all three times. A locate
    /// is a page parse and two binary searches, one of the delta directory and
    /// one of the sorted region (the delta area was a walk of up to thirty-two
    /// rows before task-2074); on the
    /// gate's `write.insert.batch` the two indexes cost 8.4 us of a 19 us insert,
    /// and half of that was asking twice.
    ///
    /// Nothing between this and the mutation changes the page: the room check
    /// only reads, and the log append does not touch pages at all. A `make_room`
    /// restarts the attempt, which re-locates.
    ///
    /// The guard is dropped before this returns, because the write that follows
    /// needs the page mutably.
    ///
    /// @param database - the open database
    /// @param page - the leaf the key belongs on
    /// @param key - the key columns of the row being written
    /// @param want_previous - whether the row that was there has to be read
    /// @param timing - where this write's nanoseconds are collected, all zero unless a harness asked
    /// @returns where the key sits, the delta directory position a row for it
    ///   takes, and the row that was there
    fn locate_and_read_previous(
        &self,
        database: &mut Database,
        page: PageId,
        key: &[Datum<'_>],
        want_previous: bool,
        timing: &mut crate::stages::PutStages,
    ) -> DbResult<(Located, usize, Option<Vec<OwnedDatum>>)> {
        let fetching = crate::stages::clock();
        let guard = database.pool().fetch(page)?;
        let leaf = LeafRef::parse(&guard)?
            .with_collations(self.collations())
            .with_directions(self.directions());
        timing.fetch = timing
            .fetch
            .saturating_add(crate::stages::elapsed(fetching));
        let (located, slot) = leaf.locate_slot(key, self.key_columns())?;
        // One row's out-of-line values, and only when the caller wants
        // the row it is replacing. Locating reads key columns, which are
        // never out of line, so this comes after.
        let held = match (want_previous, located) {
            (true, Located::Sorted(row)) => self.read_extents_row(database.pool(), &leaf, row)?,
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
        Ok((located, slot, previous))
    }

    /// Returns the out-of-line pages a delta row about to be removed owns.
    ///
    /// **Nothing else names them.** A tombstoned *sorted* row's reference is
    /// still on the page for the next repack to free; a removed delta row's is
    /// not, so it has to be read before the write and freed after it.
    ///
    /// @param database - the open database
    /// @param page - the leaf
    /// @param located - where the key sits
    fn orphaned_extents(
        &self,
        database: &mut Database,
        page: PageId,
        located: Located,
    ) -> DbResult<Vec<inillucent_pool::extent::ExtentRef>> {
        let Located::Delta(index) = located else {
            return Ok(Vec::new());
        };
        let guard = database.pool().fetch(page)?;
        let leaf = LeafRef::parse(&guard)?;
        let mut refs = Vec::new();
        for column in 0..leaf.column_count() {
            if let Some(reference) = leaf.delta_extent_at(index, column)? {
                refs.push(reference);
            }
        }
        Ok(refs)
    }

    /// Records the row that was there, for a log that can undo.
    ///
    /// Called after the room check rather than before it, because a retry
    /// re-locates and would otherwise record the same row twice.
    ///
    /// Given rather than cloned when the caller did not ask for the row: `put` and
    /// `put_absent` report presence and throw the row away, so cloning it for them
    /// was a whole row copied per write for nothing.
    ///
    /// @param log - where the before-image goes
    /// @param key - the key being written
    /// @param previous - the row that was there, taken when nobody else wants it
    /// @param caller_wants_previous - whether the caller asked for the row
    fn record_undo(
        &self,
        log: &mut dyn TreeLog,
        key: &[Datum<'_>],
        previous: &mut Option<Vec<OwnedDatum>>,
        caller_wants_previous: bool,
    ) -> DbResult<()> {
        if !log.wants_undo() {
            return Ok(());
        }
        let recorded = match caller_wants_previous {
            true => previous.clone(),
            false => previous.take(),
        };
        log.undo(self.tree_id(), key, recorded)
    }

    /// Writes one row into the leaf that has been found and has room.
    ///
    /// The log record goes first and the page carries its LSN, which is what makes
    /// redo idempotent: a page already stamped at or above the record's LSN has the
    /// change and is left alone.
    ///
    /// Any extent the displaced row owned is freed after the page is written, not
    /// before, so a failure in between leaves a run nothing points at rather than a
    /// row pointing at a freed run.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf
    /// @param landing - where the key sits, which decides what is displaced, and
    ///   the delta directory position the row takes once that is done
    /// @param encoded_row - the row's bytes, taken by the write
    /// @param spilled - whether any of its values went out of line
    /// @param costed - where the room check found the row would land
    /// @param timing - where this write's nanoseconds are collected, all zero unless a harness asked
    fn apply_row(
        &self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        landing: Landing,
        encoded_row: &mut Vec<u8>,
        spilled: bool,
        costed: DeltaOffsets,
        timing: &mut crate::stages::PutStages,
    ) -> DbResult<()> {
        let Landing { located, slot } = landing;
        let asking = crate::stages::clock();
        let orphaned = self.orphaned_extents(database, page, located)?;
        timing.orphans = crate::stages::elapsed(asking);
        let logging = crate::stages::clock();
        let lsn = log.log(Body::InsertRow {
            tree: self.tree_id(),
            page: page.0,
            row: encoded_row,
        })?;
        timing.logging = crate::stages::elapsed(logging);
        let writing = crate::stages::clock();
        database.pool().modify(page, |bytes| {
            let mut leaf = LeafMut::new(bytes)?;
            match located {
                Located::Sorted(row_index) => {
                    leaf.set_tombstone(row_index)?;
                }
                Located::Delta(index) => leaf.remove_delta(index)?,
                Located::Absent => {}
            }
            let planning = crate::stages::clock();
            // Taken rather than cloned: the loop only retries above the call to
            // this function, and the attempt that reaches it returns.
            let encoded = std::mem::take(encoded_row);
            let plan = match located {
                // **The room check's own answer, carried here rather than
                // computed again** (task-2034). Nothing above displaced a row,
                // so the delta area starts where the check found it and the
                // bitmap is the size it was. This is the ordinary write: 1,505
                // of task-2025's 1,508 shadow row writes were an append over a
                // key that was not there, and recomputing cost 0.40 of the 3.3
                // microseconds such a write takes.
                Located::Absent => DeltaPlan::at(costed, encoded, slot),
                // A tombstone may have created the bitmap and a delta removal
                // rebuilt the area, and either moves the offsets the check
                // found. So these are the two cases the check cannot answer
                // for, and the arithmetic runs again on the page as it now
                // stands.
                //
                // **`None` here is unreachable from a full leaf, and that took
                // a fix to be true (task-2033).**
                // `LeafMut::room_for_row_and_tombstone` costs the row and the
                // tombstone bitmap together, so the bitmap `set_tombstone`
                // creates two lines above is already paid for by the check that
                // let this write through. It used to cost them separately - the
                // row against a page with no bitmap, and the bitmap against a
                // page with no row - and a leaf with room for either one alone
                // answered yes and then had room for neither. Two hundred FTS5
                // documents refused on row 42 at a 4 KiB page for that reason,
                // and at every page size under the default. What is left here
                // is a page whose header says something the page does not hold.
                Located::Sorted(_) | Located::Delta(_) => leaf
                    .plan_encoded(encoded, slot)?
                    .ok_or_else(|| corrupt("a leaf that had room lost it before the write"))?,
            };
            timing.plan = crate::stages::elapsed(planning);
            let placing = crate::stages::clock();
            leaf.apply_delta(&plan)?;
            if spilled {
                leaf.mark_extents()?;
            }
            let outcome = leaf.set_lsn(lsn);
            timing.delta = crate::stages::elapsed(placing);
            outcome
        })?;
        timing.modify = crate::stages::elapsed(writing);
        for reference in orphaned {
            crate::paged::free_extent(database, log, reference)?;
        }
        Ok(())
    }

    /// Returns the leaf a write should try, and whether the hint answered.
    ///
    /// **The hint on the first attempt, a descent on the second.** See
    /// [`PagedTree::leaf_hint`]: an append at the right edge - a rowid insert,
    /// FTS5's ordered dictionary flush - lands in the leaf the last one did, and
    /// the descent answers a question it has already answered. The path comes
    /// back empty when the hint answered, because a path is only wanted when the
    /// leaf turns out to be full, and the caller descends again for one then.
    ///
    /// @param pool - the buffer pool
    /// @param encoded_key - the key in its comparison encoding
    /// @param attempt - which pass of the write's loop this is
    /// @param timing - where the nanoseconds go, all zero unless a harness asked
    /// @returns the leaf, the path to it when one was built, and whether the hint answered
    fn leaf_for_attempt(
        &self,
        pool: &Pool,
        encoded_key: &[u8],
        attempt: usize,
        timing: &mut crate::stages::PutStages,
    ) -> DbResult<(PageId, Vec<PageId>, bool)> {
        let finding = crate::stages::clock();
        let found = match attempt {
            0 => match self.leaf_for_hinted(pool, encoded_key) {
                Some(page) => (page, Vec::new(), true),
                None => {
                    let (page, path) = self.leaf_for(pool, encoded_key)?;
                    (page, path, false)
                }
            },
            _ => {
                let (page, path) = self.leaf_for(pool, encoded_key)?;
                (page, path, false)
            }
        };
        timing.find = timing.find.saturating_add(crate::stages::elapsed(finding));
        Ok(found)
    }

    /// Returns where a row of this size would land in a leaf, or nothing when it would not.
    ///
    /// **The offsets rather than a yes or no, because the write needs them and
    /// was computing them again** (task-2034). See
    /// [`crate::mutate::LeafMut::room_for_row_and_tombstone`], which carries the
    /// measurement.
    ///
    /// @param database - the open database
    /// @param page - the leaf
    /// @param encoded_len - how many bytes the row's tagged form occupies
    /// @param timing - where the nanoseconds go, all zero unless a harness asked
    fn room_for_write(
        &self,
        database: &mut Database,
        page: PageId,
        encoded_len: usize,
        timing: &mut crate::stages::PutStages,
    ) -> DbResult<Option<crate::mutate::DeltaOffsets>> {
        let asking = crate::stages::clock();
        let mut working = 0u128;
        let costed = database.pool().modify(page, |bytes| {
            let started = crate::stages::clock();
            let costed = LeafMut::new(bytes)?.room_for_row_and_tombstone(encoded_len);
            working = crate::stages::elapsed(started);
            costed
        })?;
        timing.roomwork = timing.roomwork.saturating_add(working);
        timing.room = timing.room.saturating_add(crate::stages::elapsed(asking));
        Ok(costed)
    }

    /// Compacts or splits a full leaf so the write's next attempt will fit.
    ///
    /// The path is descended for here when the hint produced none, because a
    /// split has to rewrite the parent. That is the one point a hinted write
    /// pays for a descent, and it is once per split rather than once per row.
    ///
    /// `false` means neither a compaction nor a split can help this leaf, and
    /// the caller packs the row into the sorted region instead - see
    /// `PagedTree::place_row_outside_the_delta_area` (task-2033).
    ///
    /// @param database - the open database
    /// @param log - where the records go
    /// @param page - the full leaf
    /// @param path - the path to it, filled in here when the hint left it empty
    /// @param key - the key arriving, which decides where a split divides
    /// @param encoded_len - how many bytes the row's tagged form occupies
    /// @param timing - where the nanoseconds go, all zero unless a harness asked
    /// @returns whether room was made, which is `false` for a leaf no compaction or split can help
    fn make_room_for_row(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &mut Vec<PageId>,
        key: &[Datum<'_>],
        encoded_len: usize,
        timing: &mut crate::stages::PutStages,
    ) -> DbResult<bool> {
        if path.is_empty() {
            let encoded_key = self.encode_key(key);
            *path = self.leaf_for(database.pool(), &encoded_key)?.1;
        }
        let remaking = crate::stages::clock();
        let made = self.make_room(database, log, page, path, Some(key), encoded_len);
        timing.making = timing
            .making
            .saturating_add(crate::stages::elapsed(remaking));
        timing.remade = 1;
        made
    }

    /// Counts one write that reached the page.
    ///
    /// One read and one write of [`WriteStats`], because a `Cell` of it copies
    /// the whole struct each way and it is nineteen counters wide.
    ///
    /// @param from_hint - whether the leaf came from the hint rather than a descent
    /// @param is_new_key - whether the key was absent, so the tree holds one row more
    fn note_one_write(&mut self, from_hint: bool, is_new_key: bool) {
        let mut stats = self.stats.get();
        stats.inserted = stats.inserted.saturating_add(1);
        match from_hint {
            true => stats.hinted = stats.hinted.saturating_add(1),
            false => stats.descended = stats.descended.saturating_add(1),
        }
        self.stats.set(stats);
        if is_new_key {
            self.note_rows(1);
        }
    }

    fn write_row(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        row: &[Datum<'_>],
        want_previous: bool,
        replace: bool,
    ) -> DbResult<Option<Vec<OwnedDatum>>> {
        // **Where one write's time goes, behind a switch** (task-2034). The
        // stage above this one has been measured twice and each time the
        // remainder landed here: task-2025 put the engine's virtual table arm
        // at 0.44 to 0.55 us a document and left 1.8 to 2.2 us inside this
        // function for a write that descends nothing, compacts nothing and
        // splits nothing. `crate::stages` reads no clock unless a harness asked
        // for the split, and the numbers are added up in locals and handed over
        // once at the end - a `Cell` of them copies seventeen counters each
        // way, which is the cost that took the timers that were always on out
        // of this path in task-2006.
        let entered = crate::stages::clock();
        let mut timing = crate::stages::PutStages::default();
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
        // Outside the loop, because a retry after a compaction must reuse the
        // run rather than write a second and leak the first. Why a wide value
        // is spilled before the row is placed rather than instead of placing it
        // is on `encode_row_spilling_wide_values`, with what it was measured to
        // be worth.
        let encoding = crate::stages::clock();
        let (mut encoded_row, spilled, arriving_runs) =
            self.encode_row_spilling_wide_values(database, log, row)?;
        timing.encode = crate::stages::elapsed(encoding);
        // **Where this transaction's time goes, measured once rather than timed on every
        // write** (task-2006). Timers here read the clock and copy the whole `WriteStats`
        // through its `Cell` twice a block, which is why they are not in this path any
        // more; what they said, over the 6,000 writes `inillucent-writelogattrib` makes
        // into a table carrying two secondary indexes, was a 29.66 ms transaction split
        // as: making room 8.89 ms, of which building a compacted image 5.46; placing the
        // row, its log record and its undo record 4.14; locating the key 3.97; encoding
        // the row 1.46. The rest is the descent, the room check and the commit.
        let mut from_hint = false;

        // Two attempts at most: the first may find the leaf full, and the
        // compaction or split that follows leaves a page with room for a row.
        // When it cannot - a leaf with fewer than two live rows, or a row the
        // delta area will not hold at any fill - the write is packed instead.
        for attempt in 0..2 {
            let (page, mut path, hinted) =
                self.leaf_for_attempt(database.pool(), &encoded_key, attempt, &mut timing)?;
            from_hint = from_hint || hinted;
            // **The key is found once.** Where it sits decides four things -
            // whether it was there, what the caller gets back, what the
            // mutation below has to displace, and where in the delta directory
            // the row goes - and the first version asked the page three times.
            // A locate is a page parse and two binary searches, one of the
            // delta directory and one of the sorted region; on the gate's
            // `write.insert.batch` the two indexes cost 8.4 us of a 19 us
            // insert, and half of that was asking twice.
            //
            // Nothing between here and the mutation changes the page: the
            // room check only reads, and the log append does not touch pages
            // at all. A `make_room` restarts the attempt, which re-locates.
            let locating = crate::stages::clock();
            let (located, slot, mut previous) =
                self.locate_and_read_previous(database, page, &key, want_previous, &mut timing)?;
            timing.locate = timing
                .locate
                .saturating_add(crate::stages::elapsed(locating));
            // A caller that refuses a duplicate is told so before anything is
            // written, which is the whole point of asking.
            if !replace && previous.is_some() {
                return Ok(previous);
            }
            let costed = self.room_for_write(database, page, encoded_row.len(), &mut timing)?;
            let Some(costed) = costed else {
                // Attempt 1 has already had room made for it and a `Stuck`
                // leaf never will: the delta area is not how this row gets in.
                let stuck = attempt == 1
                    || !self.make_room_for_row(
                        database,
                        log,
                        page,
                        &mut path,
                        &key,
                        encoded_row.len(),
                        &mut timing,
                    )?;
                if stuck {
                    return self.place_row_outside_the_delta_area(
                        database,
                        log,
                        row,
                        &arriving_runs,
                        previous,
                        caller_wants_previous,
                    );
                }
                continue;
            };
            let undoing = crate::stages::clock();
            self.record_undo(log, &key, &mut previous, caller_wants_previous)?;
            timing.undo = crate::stages::elapsed(undoing);
            let applying = crate::stages::clock();
            self.apply_row(
                database,
                log,
                page,
                Landing { located, slot },
                &mut encoded_row,
                spilled,
                costed,
                &mut timing,
            )?;
            timing.apply = crate::stages::elapsed(applying);
            self.note_one_write(from_hint, previous.is_none());
            timing.rows = 1;
            timing.whole = crate::stages::elapsed(entered);
            crate::stages::record(timing);
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
        // **Any text or blob over the threshold, whatever the column is
        // declared.** It used to be only a text in a column declared `TEXT` or
        // bytes in one declared `BLOB`, because an extent reference said
        // nothing about which of the two it held and the column's declaration
        // was the only thing that could answer on the way back out. A column
        // declared `BLOB` - which is what a column declared nothing at all is -
        // therefore kept a text inline however long it was, and a text larger
        // than a page could not be stored at all (task-1980, task-1986). The
        // reference states the class when the column would answer wrongly; see
        // `leaf::extent_class_for`, and `leaf::layout::classify_at`, which has
        // to agree with this because the builder classifies again from the
        // rows.
        value.as_bytes().filter(|bytes| bytes.len() > threshold)
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
                // A leaf too full for a tombstone that no compaction or split
                // can help is a leaf whose header disagrees with its own
                // contents: the bitmap is `row_count / 8` bytes and a
                // compaction hands back the whole delta area. Said here rather
                // than recursed on, because `make_room` answering `false`
                // twice is what an infinite recursion looks like from outside.
                if !self.make_room(database, log, page, &path, None, 0)? {
                    return Err(corrupt(
                        "a leaf has no room for a tombstone and cannot be split",
                    ));
                }
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

    /// Decides which rows of a leaf are live and in what order, timed.
    ///
    /// **The half of a compaction a splice pays for as well.** `live_order`
    /// merges the delta directory into the sorted region by position;
    /// `materialise`, which only a repack needs, then reads every value of every
    /// live row. `merge_nanos` is this and `source_nanos` is both.
    ///
    /// @param leaf - the leaf being compacted, already parsed
    fn timed_live_order<'p>(&self, leaf: &LeafRef<'p>) -> DbResult<crate::leaf::LiveOrder<'p>> {
        let merging = std::time::Instant::now();
        let order = leaf.live_order()?;
        let merged = merging.elapsed().as_nanos();
        let mut stats = self.stats.get();
        stats.source_nanos = stats.source_nanos.saturating_add(merged);
        stats.merge_nanos = stats.merge_nanos.saturating_add(merged);
        self.stats.set(stats);
        Ok(order)
    }

    /// Reads every value of every live row into a flat vector, timed.
    ///
    /// @param order - the live rows, from `timed_live_order`
    fn timed_materialise<'p>(
        &self,
        order: crate::leaf::LiveOrder<'p>,
    ) -> DbResult<crate::leaf::LiveSource<'p>> {
        let reading = std::time::Instant::now();
        let source = order.materialise()?;
        let mut stats = self.stats.get();
        stats.source_nanos = stats
            .source_nanos
            .saturating_add(reading.elapsed().as_nanos());
        self.stats.set(stats);
        Ok(source)
    }

    /// Splices a leaf's delta rows into its packed region, timed.
    ///
    /// `None` when the leaf does not allow a splice; see
    /// [`crate::leaf::splice_image`].
    ///
    /// @param leaf - the leaf being compacted, already parsed
    /// @param order - its live rows in key order
    fn timed_splice(
        &self,
        leaf: &LeafRef<'_>,
        order: &crate::leaf::LiveOrder<'_>,
    ) -> DbResult<Option<Vec<u8>>> {
        let splicing = std::time::Instant::now();
        let image = crate::leaf::splice_image(leaf, order, self.page_size(), TIGHT_FILL)?;
        let mut stats = self.stats.get();
        stats.splice_nanos = stats
            .splice_nanos
            .saturating_add(splicing.elapsed().as_nanos());
        if image.is_some() {
            stats.splices = stats.splices.saturating_add(1);
        }
        self.stats.set(stats);
        Ok(image)
    }

    /// Packs a leaf's live rows into one page, timing the sizing apart from the encode.
    ///
    /// `None` when they do not fit one page, which is the caller's signal to split.
    ///
    /// @param builder - the leaf builder for this tree
    /// @param source - the leaf's live rows, sorted
    fn timed_compact_image<'d>(
        &self,
        builder: &LeafBuilder,
        source: &dyn Rows<'d>,
    ) -> DbResult<Option<Vec<u8>>> {
        let imaging = std::time::Instant::now();
        let mut timing = ImageTiming::default();
        let compacted = compact_image_timed(builder, source, &mut timing)?;
        let mut stats = self.stats.get();
        stats.image_nanos = stats
            .image_nanos
            .saturating_add(imaging.elapsed().as_nanos());
        stats.sizing_nanos = stats.sizing_nanos.saturating_add(timing.sizing_nanos);
        stats.encode_nanos = stats.encode_nanos.saturating_add(timing.encode_nanos);
        self.stats.set(stats);
        Ok(compacted)
    }

    /// Compacts a leaf, or splits it when its live rows no longer fit one page.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf
    /// @param arriving - the key about to be written, when there is one
    /// Chooses between compacting, splitting and repacking a full leaf.
    ///
    /// **The decision half of [`Tree::make_room`], which is the half with the
    /// argument in it.** Everything here reads the page through one guard and
    /// answers a `Fit`; the caller drops the guard and does what it says. They
    /// were one function of 166 lines until task-1946's M12, and the two halves
    /// were already separate paragraphs.
    ///
    /// @param database - the open database
    /// @param page - the full leaf
    /// @param arriving - the key of the row that needs room, when there is one
    /// @param needed - how many bytes it needs
    fn choose_fit(
        &self,
        database: &mut Database,
        page: PageId,
        arriving: Option<&[Datum<'_>]>,
        needed: usize,
    ) -> DbResult<Fit> {
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
        // a row, and a splice needs nothing more than that.
        let order = self.timed_live_order(&leaf)?;
        // **A leaf with out-of-line values always takes the owned route.**
        // The fast path below packs straight out of the page, which needs
        // the guard held - and repacking an extent needs the *file*, to
        // allocate the run the value moves into. The two cannot be held at
        // once, and a leaf with extents holds few rows, so the copy costs
        // little where it costs anything at all.
        let spilled = leaf.has_extents();
        let appending = self.is_appending(&leaf, &order, arriving)?;
        // **Fewer than two live rows is stuck, whichever route it would take
        // (task-2033).** Every way of making room here moves rows between
        // pages, and a leaf holding one row has nothing to move: a compaction
        // rebuilds the page it already is, and a split has no second row to
        // give the right half. Saying so is what lets the caller take the one
        // route that does work, which is packing the arriving row into the
        // leaf beside the row already there.
        if order.len() < 2 {
            return Ok(Fit::Stuck);
        }
        if spilled {
            // The rows are *not* copied out here. Reading them through the
            // guard would resolve every out-of-line value, which is the read
            // the repack exists to avoid; `rows_to_repack` reads them again
            // without one.
            return Ok(Fit::Repack(appending));
        }
        // **A splice first, when the page allows one** (task-2074). It keeps
        // the page's widths and heap and writes only the delta rows' values,
        // so it skips the sizing pass, the materialising pass and most of the
        // encode. Whether it is allowed is a question about the page alone -
        // see `leaf/splice.rs` - which is what lets recovery make the same
        // choice from the same page.
        let spliced = self.timed_splice(&leaf, &order)?;
        let splice_allowed = spliced.is_some();
        if let Some(mut image) = spliced {
            if LeafMut::new(&mut image)?.room_for(needed)? {
                return Ok(Fit::Compact(
                    image,
                    leaf.right_sibling(),
                    leaf.max_cts(),
                    leaf.has_delta_directory(),
                ));
            }
        }
        self.pack_or_split(&leaf, order, needed, splice_allowed, appending)
    }

    /// Reports whether the row a full leaf is making room for arrives in key
    /// order past everything the leaf holds.
    ///
    /// @param leaf - the full leaf
    /// @param order - its live rows in key order
    /// @param arriving - the key of the row that needs room, when there is one
    fn is_appending(
        &self,
        leaf: &LeafRef<'_>,
        order: &crate::leaf::LiveOrder<'_>,
        arriving: Option<&[Datum<'_>]>,
    ) -> DbResult<bool> {
        // **An append splits *lopsidedly*; it does not split *early*.**
        //
        // This flag used to force a split instead of a compaction, on the
        // argument that a leaf filled by rows arriving in key order is
        // compacted, filled again by the next thirty-two, and compacted
        // again - and that a compaction repacks every live row and writes
        // the whole page to the log.
        //
        // The argument does not survive being measured on a page rather
        // than on a workload. A leaf could only hold 32 delta
        // rows before it was full, so forcing a split gave the
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
        let last_row: Option<Vec<Datum<'_>>> = match order.len().checked_sub(1) {
            Some(last) => Some(
                (0..self.key_columns())
                    .map(|column| order.value(last, column))
                    .collect::<DbResult<Vec<Datum<'_>>>>()?,
            ),
            None => None,
        };
        Ok(leaf.right_sibling().is_none()
            && order.len() >= 2
            && match (arriving, last_row.as_deref()) {
                (Some(key), Some(last)) => {
                    is_above(key, last, self.collations(), self.key_columns())
                }
                _ => false,
            })
    }

    /// Repacks a full leaf from scratch, or splits it when a repack leaves no
    /// room for the arriving row.
    ///
    /// The half of [`Tree::choose_fit`] a splice did not settle.
    ///
    /// @param leaf - the full leaf
    /// @param order - its live rows in key order
    /// @param needed - how many bytes the arriving row needs
    /// @param splice_allowed - whether the page allowed a splice, which decides
    ///   whether the repack has to be logged with its image
    /// @param appending - whether the rows arrive in key order
    fn pack_or_split(
        &self,
        leaf: &LeafRef<'_>,
        order: crate::leaf::LiveOrder<'_>,
        needed: usize,
        splice_allowed: bool,
        appending: bool,
    ) -> DbResult<Fit> {
        let source = self.timed_materialise(order)?;
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
        let mut compacted = self.timed_compact_image(&builder, &source)?;
        if let Some(image) = compacted.as_mut() {
            if !LeafMut::new(image)?.room_for(needed)? {
                compacted = None;
            }
        }
        Ok(match compacted {
            // A repack after a splice that was allowed is logged with its
            // page, because recovery would splice. See `Fit::Compact`.
            Some(image) => Fit::Compact(
                image,
                leaf.right_sibling(),
                leaf.max_cts(),
                leaf.has_delta_directory() && !splice_allowed,
            ),
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
        })
    }

    /// Compacts a leaf, or splits it when its live rows no longer fit one page.
    ///
    /// **The action half.** `choose_fit` reads the page and answers which
    /// of the three this is; this drops the guard and does it. They were one
    /// function of 166 lines until task-1946's M12.
    ///
    /// **`false` means nothing was done and nothing can be**, which is a leaf
    /// holding fewer than two live rows: see [`Fit::Stuck`]. A caller that
    /// retries after this has to have another way in, or it is a loop that
    /// cannot end - `write_row` packs the row into the leaf and `delete`
    /// refuses.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf
    /// @param path - the interior pages above it, for a split
    /// @param arriving - the key about to be written, when there is one
    /// @param needed - how many bytes the arriving row needs
    /// @returns whether room was made
    pub fn make_room(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
        arriving: Option<&[Datum<'_>]>,
        needed: usize,
    ) -> DbResult<bool> {
        let before = self.stats.get();
        let started = std::time::Instant::now();
        let outcome = self.make_room_timed(database, log, page, path, arriving, needed);
        let elapsed = started.elapsed().as_nanos();
        let mut stats = self.stats.get();
        stats.room_nanos = stats.room_nanos.saturating_add(elapsed);
        // **Which of the two it was, told by what it did rather than by a flag.** A
        // split raises `splits`, a compaction raises `compactions`, and one call does
        // one of them - so the counters that are already kept say which bucket the time
        // belongs in, and a split that also compacted is charged to the split, which is
        // the more expensive half.
        match stats.splits > before.splits {
            true => stats.split_nanos = stats.split_nanos.saturating_add(elapsed),
            false => stats.compaction_nanos = stats.compaction_nanos.saturating_add(elapsed),
        }
        self.stats.set(stats);
        outcome
    }

    /// The whole of making room, timed by the wrapper above.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param page - the leaf
    /// @param path - the interior pages above it, for a split
    /// @param arriving - the key about to be written, when there is one
    /// @param needed - how many bytes the arriving row needs
    /// @returns whether room was made
    fn make_room_timed(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
        arriving: Option<&[Datum<'_>]>,
        needed: usize,
    ) -> DbResult<bool> {
        let choosing = std::time::Instant::now();
        let fit = self.choose_fit(database, page, arriving, needed)?;
        let mut stats = self.stats.get();
        stats.choose_nanos = stats
            .choose_nanos
            .saturating_add(choosing.elapsed().as_nanos());
        self.stats.set(stats);
        match fit {
            Fit::Stuck => Ok(false),
            Fit::Compact(image, right, max_cts, logical) => {
                self.compact_into(database, log, page, image, right, max_cts, logical)?;
                Ok(true)
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
                self.split(database, log, page, path, &borrowed, fill)?;
                Ok(true)
            }
            Fit::Repack(appending) => {
                let (rows, carried) = self.rows_to_repack(database.pool(), page)?;
                // The same reasoning as `Fit::Stuck`, asked again because this
                // route reads the leaf's rows a second way and a leaf whose
                // only live rows were shadowed can turn out to hold fewer than
                // the borrowed read counted.
                if rows.len() < 2 {
                    return Ok(false);
                }
                let borrowed: Vec<Vec<Datum<'_>>> = rows
                    .iter()
                    .map(|row| row.iter().map(OwnedDatum::borrow).collect())
                    .collect();
                self.repack(database, log, page, path, &borrowed, &carried, appending)?;
                Ok(true)
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
            // **The sentence a caller reads, because the one that was here was
            // not one (task-1979, section 10, D2).** `misuse` keeps its words
            // inside the process, so what reached every front end was the
            // primary code's own text, `bad parameter or other API misuse`, for
            // a statement that is written correctly.
            //
            // **What is left here is a narrow case, and it used to be a wide
            // one.** Until task-1986 the reason a large value stayed inline was
            // almost always the column's declaration - only a column declared
            // `TEXT` holding a text or `BLOB` holding bytes could have an
            // extent - so `CREATE TABLE t (a)` refused 32,680 bytes and this
            // was the sentence that said why. A value of any class now goes out
            // of line whatever the column says, so what reaches this is a row
            // that is too large for a page with every one of its values already
            // outside it: a key column, which is never spilled because a
            // descent compares keys, or enough columns just under the spill
            // threshold to fill a page between them.
            return Err(inillucent_base::error::statement_refusal(
                "this row is larger than a page even with its large values stored outside it; \
                 a key column is never stored outside the page, so a key this long has to be \
                 shortened, and a row of many values just under the page's eighth has to be \
                 split across tables",
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
        let taken = rows_for_the_left_half(&builder, rows, fill)?;
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

        // `true`: the `Structural` record logged a few lines below reads this
        // exact page back and carries it whole as `parent_image`, so neither
        // branch needs to log it a second time here.
        let parent = if path.is_empty() {
            self.build_root(database, log, left_page, &separator, right_page, true)?
        } else {
            self.insert_separator(database, log, page, path, &separator, right_page, true)?
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
    /// **`folded_by_caller` skips this function's own log record.** A leaf
    /// split's direct call (`split_carrying`, the common case at every scale
    /// this tree has been measured at) re-reads the root it just built and logs
    /// it again, whole, as the `Structural` record's `parent_image` a few lines
    /// later - so writing it here too logged the same bytes twice. Measured on
    /// the write gate's `write.insert.batch` (2,000 inserts into `main_table`
    /// and its two secondary indexes, one transaction): 40 splits, 40 of these
    /// `WritePage` records, one every time, at 8,240 bytes each - 321.9 KiB of
    /// the workload's 1,985.8 KiB, gone once the caller stopped asking for both.
    /// The one caller that does *not* immediately fold this into a `Structural`
    /// record - `insert_separator`'s own recursion, propagating a separator
    /// insertion up past a full interior page - passes `false` and keeps
    /// logging here, because nothing else ever will.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param left - the page holding what the root used to hold
    /// @param separator - the right half's first key
    /// @param right - the right half's page
    /// @param folded_by_caller - whether the caller logs this page's image itself
    fn build_root(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        left: PageId,
        separator: &[u8],
        right: PageId,
        folded_by_caller: bool,
    ) -> DbResult<PageId> {
        let level = self.height().saturating_add(1);
        let builder = InteriorBuilder::new(self.page_size(), self.tree_id(), level)?;
        let mut image = builder.build(
            &[separator],
            &[Swip::unswizzled(left), Swip::unswizzled(right)],
        )?;
        let root = self.root();
        if folded_by_caller {
            // The caller reads this page back and logs it whole a few lines
            // after this returns; the LSN it carries until then is never read,
            // because nothing evicts a page this function is still building.
            database.install(root, &image)?;
        } else {
            let lsn = log.log(Body::WritePage {
                page: root.0,
                image: &image,
            })?;
            page::write_u64(&mut image, page::header::LSN, lsn)?;
            database.install(root, &image)?;
        }
        self.note_height(level);
        Ok(root)
    }

    /// Adds a separator and a right child above `left`.
    ///
    /// Returns the page the separator landed in, which is what the caller
    /// stamps with the split's LSN.
    ///
    /// **`folded_by_caller` is for the common case, where the parent has room.**
    /// A leaf split's direct call passes `true`: `split_carrying` is about to
    /// re-read this exact page and log it whole inside the `Structural` record
    /// it writes next, so a `WritePage` here would be the same bytes logged
    /// twice for one split. See [`PagedTree::build_root`]'s comment for the
    /// measurement. It is always `false` one level up: propagating a separator
    /// past a full interior page recurses into this function again, and that
    /// recursive call's result is never folded into anything - it is the only
    /// record its page gets, so it always logs.
    ///
    /// @param database - the file
    /// @param log - where the record goes
    /// @param left - the page that was split
    /// @param path - the interior pages above `left`, root first
    /// @param separator - the right half's first key, encoded
    /// @param right - the right half's page
    /// @param folded_by_caller - whether the immediate caller logs this page's image itself
    fn insert_separator(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        left: PageId,
        path: &[PageId],
        separator: &[u8],
        right: PageId,
        folded_by_caller: bool,
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
            if folded_by_caller {
                // The caller reads this page back and logs it whole inside its
                // own `Structural` record a few lines after this returns.
                database.install(parent, &image)?;
            } else {
                let lsn = log.log(Body::WritePage {
                    page: parent.0,
                    image: &image,
                })?;
                page::write_u64(&mut image, page::header::LSN, lsn)?;
                database.install(parent, &image)?;
            }
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
            // `false`: this call's result is not about to be folded into a
            // `Structural` record - it is the interior level's own standalone
            // page, and this is the only record that will ever describe it.
            self.build_root(database, log, moved, &promoted, sibling, false)?;
            return Ok(if landed == parent { moved } else { sibling });
        }
        // Same reasoning as above: propagating a separator past a full
        // interior page has no enclosing `Structural` record to fold into.
        self.insert_separator(database, log, parent, ancestors, &promoted, sibling, false)?;
        Ok(landed)
    }

    /// Returns the parent's contents and this leaf's place in them, when the
    /// two leaves are adjacent children of one parent.
    ///
    /// `None` when there is no parent, when the leaf is not among its
    /// children, or when the right sibling is under a *different* parent -
    /// merging across that boundary rewrites two interior pages and the
    /// separator between them, and a half-empty leaf is cheaper than the code
    /// that would.
    ///
    /// Split out of [`PagedTree::merge_if_small`], which reached 160 lines
    /// against the 157 it is recorded at. It is one question and it reads one
    /// page.
    ///
    /// @param database - the file and its pool
    /// @param path - the descent that reached the leaf, parent last
    /// @param page - the leaf the delete emptied
    /// @param right - its right sibling
    ///
    /// @returns the separators, the children, the level, the parent's page and
    ///   the leaf's position among the children
    #[allow(clippy::type_complexity)]
    fn adjacent_under_one_parent(
        &mut self,
        database: &mut Database,
        path: &[PageId],
        page: PageId,
        right: PageId,
    ) -> DbResult<Option<(Vec<Vec<u8>>, Vec<PageId>, u16, PageId, usize)>> {
        let Some(parent) = path.last().copied() else {
            return Ok(None);
        };
        let (separators, children, level) = self.read_interior(database.pool(), parent)?;
        let Some(position) = children.iter().position(|held| *held == page) else {
            return Ok(None);
        };
        if children.get(position.saturating_add(1)).copied() != Some(right) {
            return Ok(None);
        }
        Ok(Some((separators, children, level, parent, position)))
    }

    /// Whether the right sibling is too full to take anything, asked from its
    /// header.
    ///
    /// **The second cheap question, and it is the one that was costing**
    /// (task-2066 §4.3.7). The check above stops a merge attempt on a leaf
    /// that has not emptied; nothing stopped the attempt being made again
    /// on every later delete, because `underflows` stays true once it is
    /// true. So a table deleted in key order materialised both leaves,
    /// borrowed them into a second vector, packed them to measure and threw
    /// it away, once per row - work that grows with the rows per leaf. It is
    /// why the delete was linear at a 4,096 byte page and not at 32,768:
    /// 8,000 rows took 252 ms at the first and 1,301 at the second, with the
    /// page count and the cache hits per row flat in both.
    ///
    /// The merged image has to hold every live row of the right sibling, so
    /// a sibling that already fills more than `COMPACT_FILL` of a page
    /// cannot take anything else. `a_bulk_built_leaf_compacts_rather_than_
    /// splitting` states the same fact from the other side: a leaf packed at
    /// `BULK_FILL` can never be repacked into `COMPACT_FILL` of a page.
    ///
    /// Asked only of a sibling with no tombstones and no delta rows, because
    /// then its bytes are exactly its live payload. With either of those the
    /// bytes overstate what a repack would need and the question is left to
    /// the pack.
    ///
    /// @param database - the file and its pool
    /// @param page - the leaf the delete emptied
    /// @param right - the sibling a merge would fold into
    fn sibling_is_already_full(
        &self,
        database: &Database,
        page: PageId,
        right: PageId,
    ) -> DbResult<bool> {
        // **Neither leaf may hold an out-of-line value** (task-2066 §4.3.7,
        // found by `new_engine_extent_packing`). A merge frees the extents of
        // both leaves it consumes - `doomed` below is `extents_of(page)` plus
        // `extents_of(right)` - so a merge this skips is extent pages that
        // never come back. `deleting_every_packed_value_returns_the_pages`
        // deletes a thousand 4,200 byte values, which are past
        // `page_size / EXTENT_DIVISOR` and therefore out of line, and it saw 3
        // pages return where it expects hundreds.
        //
        // This shortcut is about *cost* and must not change what the tree
        // reclaims, so a leaf with extents goes the long way.
        let ceiling = (self.page_size() as f64 * COMPACT_FILL) as usize;
        for held in [page, right] {
            let guard = database.pool().fetch(held)?;
            if LeafRef::parse(&guard)?.has_extents() {
                return Ok(false);
            }
        }
        let guard = database.pool().fetch(right)?;
        let leaf = LeafRef::parse(&guard)?;
        Ok(!leaf.has_writes() && leaf.used_bytes(self.page_size()) > ceiling)
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
        // The second question is `sibling_is_already_full`, on the same line as
        // the first because both answer "do not attempt this" and neither
        // materialises anything.
        if !underflowed || right.is_none() || self.sibling_is_already_full(database, page, right)? {
            return Ok(());
        }
        let Some((mut separators, mut children, level, parent, position)) =
            self.adjacent_under_one_parent(database, path, page, right)?
        else {
            return Ok(());
        };

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
        self.note_leaf_hint(pool, descent.leaf, encoded_key);
        Ok((descent.leaf, path))
    }

    /// Returns the leaf a key belongs in without descending, when the hint says so.
    ///
    /// **The leaf hint** - see [`PagedTree::leaf_hint`] for the whole argument and
    /// for why a stale hint cannot give a wrong answer. `None` means "descend", which
    /// is every key that is not an append at the right edge and every first write to
    /// a tree.
    ///
    /// No path comes back with it. A path is only wanted when the leaf turns out to
    /// be full, and the caller re-descends for one then - which is once per split
    /// rather than once per row.
    ///
    /// @param pool - the buffer pool
    /// @param encoded_key - the key's comparable bytes
    fn leaf_for_hinted(&self, pool: &Pool, encoded_key: &[u8]) -> Option<PageId> {
        // **The comparisons first, and they are the whole of the miss path.** Two
        // `memcmp`s against bytes the caller has already built. Outside the hinted
        // leaf's window means the descent has to place this key, and that answer costs
        // nothing beyond the comparisons. See `PagedTree::leaf_hint` for why a
        // rightmost leaf is tested at the low end only.
        let (at, page, right) = {
            let hints = self.leaf_hints.try_borrow().ok()?;
            let at = hints.iter().position(|hint| {
                encoded_key >= hint.low.as_slice()
                    && (hint.right.is_none() || encoded_key <= hint.high.as_slice())
            })?;
            let hint = hints.get(at)?;
            (at, hint.page, hint.right)
        };
        // The header on a hit, for the reasons `PagedTree::leaf_hint` gives: a page
        // handed to another tree, and a fence moved by something other than this write
        // path, are both visible here and neither is visible in the comparisons above.
        // The sibling is what catches the second: a split or a merge of this leaf
        // changes it. This fetches a page the insert is about to fetch regardless, and
        // it parses nothing.
        let Ok(guard) = pool.fetch(page) else {
            self.forget_leaf_hint(at);
            return None;
        };
        if page::kind_of(&guard).ok() != Some(page::PageKind::Leaf)
            || page::tree_of(&guard).ok() != Some(self.tree_id())
            || page::right_of(&guard).ok() != Some(right)
        {
            self.forget_leaf_hint(at);
            return None;
        }
        Some(page)
    }

    /// Records the leaf a descent reached and widens the window that proves it.
    ///
    /// The window is the lowest and highest probes seen to land in this leaf, so a hit
    /// is every key between them. A descent into the leaf already hinted widens the
    /// window by one end at most; a descent into a different leaf starts a new window
    /// at that probe.
    ///
    /// One header read off a page the descent has just fetched, and a key copy only
    /// when an end of the window moves. See [`PagedTree::leaf_hint`].
    ///
    /// @param pool - the buffer pool
    /// @param leaf - the leaf the descent reached
    /// @param encoded_key - the probe the descent used, in comparable bytes
    fn note_leaf_hint(&self, pool: &Pool, leaf: PageId, encoded_key: &[u8]) {
        let Some(right) = pool
            .fetch(leaf)
            .ok()
            .and_then(|guard| page::right_of(&guard).ok())
        else {
            return;
        };
        let Ok(mut hints) = self.leaf_hints.try_borrow_mut() else {
            return;
        };
        if let Some(held) = hints.iter_mut().find(|hint| hint.page == leaf) {
            held.right = right;
            if encoded_key < held.low.as_slice() {
                held.low.clear();
                held.low.extend_from_slice(encoded_key);
            }
            if encoded_key > held.high.as_slice() {
                held.high.clear();
                held.high.extend_from_slice(encoded_key);
            }
            return;
        }
        if hints.len() < crate::paged::LEAF_HINTS {
            hints.push(crate::paged::LeafHint {
                page: leaf,
                low: encoded_key.to_vec(),
                high: encoded_key.to_vec(),
                right,
            });
            return;
        }
        // The victim's two key buffers are written over rather than freed and allocated
        // again, which is why this replaces an entry in place instead of removing one and
        // pushing another.
        let at = self.hint_victim.get() % crate::paged::LEAF_HINTS;
        self.hint_victim
            .set(at.saturating_add(1) % crate::paged::LEAF_HINTS);
        if let Some(slot) = hints.get_mut(at) {
            slot.page = leaf;
            slot.right = right;
            slot.low.clear();
            slot.low.extend_from_slice(encoded_key);
            slot.high.clear();
            slot.high.extend_from_slice(encoded_key);
        }
    }

    /// Drops one entry of the leaf hint set, because the page it names is not a leaf of
    /// this tree any more.
    ///
    /// One entry and not the set: the entries name different leaves, and the proof that
    /// failed was this one's. See [`PagedTree::leaf_hints`]. A split or a merge clears
    /// the whole set through [`PagedTree::note_leaves`] instead, which holds `&mut self`.
    ///
    /// @param at - which entry failed its proof
    fn forget_leaf_hint(&self, at: usize) {
        if let Ok(mut hints) = self.leaf_hints.try_borrow_mut() {
            if at < hints.len() {
                hints.remove(at);
            }
        }
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

    /// Returns the stand-in an out-of-line value takes during a repack.
    ///
    /// Zeroes of the value's own length, so the builder's sizing is right, and
    /// **of the class the reference says**, so the builder states the same class
    /// back. Reading the column's declaration here instead is what used to be
    /// done, and it would now rewrite a text in a column declared `BLOB` as a
    /// blob the first time the leaf was repacked (task-1986).
    ///
    /// @param column - which column the value is in
    /// @param reference - the reference the leaf holds for it
    fn placeholder_for(&self, column: usize, reference: ExtentRef) -> OwnedDatum {
        let physical = self
            .columns()
            .get(column)
            .map(|spec| spec.physical)
            .unwrap_or(crate::types::PhysicalType::Any);
        let blob = matches!(
            crate::leaf::extent_datum(reference.class, physical, &[]),
            Datum::Blob(_)
        );
        let filler = vec![0u8; reference.length as usize];
        if blob {
            OwnedDatum::Blob(filler)
        } else {
            OwnedDatum::Text(filler)
        }
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
                    values.push(self.placeholder_for(column, reference));
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
                    values.push(self.placeholder_for(column, reference));
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

    /// Writes a row the delta area cannot take, and reports the row it replaced.
    ///
    /// **The route a write takes when making room cannot help it (task-2033).**
    /// An ordinary write appends the row's tagged bytes to the delta area, and
    /// on a small page that area is a fraction of what the page holds: a leaf
    /// spends `64 + 16 * columns` on its directory and eight bytes plus a slot
    /// per row on each column's mini-column before it holds anything, so the
    /// ten-column schema catalog has spent 384 of a 512-byte page once it holds
    /// a single row, against its own rows of 107 to 153 bytes tagged. Making
    /// room does not help, because a split leaves both halves holding a row and
    /// the gap is the same size in each - so the third `CREATE TABLE` in a
    /// 512-byte database answered `SQLITE_CORRUPT`.
    ///
    /// Packed into the sorted region the same row is one slot per column and
    /// its text in the heap rather than a tagged copy of every value, which is
    /// what makes it fit. This is the expensive route - it rewrites the page
    /// and logs the image - and nothing reaches it that had a cheaper way
    /// through: the caller has already had an ordinary compaction or split
    /// decline to make room.
    ///
    /// **The page and the path come from one descent.** A split rewrites the
    /// parent, so the path has to be the one above the leaf being packed;
    /// pairing a hinted page with a descended path would hold only while the
    /// hint's window and the descent agree, which is not an invariant this
    /// path has any reason to rest on.
    ///
    /// @param database - the file
    /// @param log - where the records go
    /// @param row - the arriving row's values, in tree-column order
    /// @param arriving - the run each of its spilled values is already in
    /// @param previous - the row that was under the key, for the undo record
    /// @param caller_wants_previous - whether the caller asked for that row
    /// @returns the row that was under the key, when the caller asked for it
    fn place_row_outside_the_delta_area(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        row: &[Datum<'_>],
        arriving: &[Option<ExtentRef>],
        mut previous: Option<Vec<OwnedDatum>>,
        caller_wants_previous: bool,
    ) -> DbResult<Option<Vec<OwnedDatum>>> {
        let key: Vec<Datum<'_>> = row.iter().copied().take(self.key_columns()).collect();
        self.record_undo(log, &key, &mut previous, caller_wants_previous)?;
        // Encoded again rather than carried in: this is a page rewrite and a
        // descent, and one key encoding beside those is not worth a parameter.
        let encoded_key = self.encode_key(&key);
        let (page, path) = self.leaf_for(database.pool(), &encoded_key)?;
        self.pack_row_into_leaf(database, log, page, &path, row, &key, arriving)?;
        let mut stats = self.stats.get();
        stats.inserted = stats.inserted.saturating_add(1);
        stats.descended = stats.descended.saturating_add(1);
        self.stats.set(stats);
        if previous.is_none() {
            self.note_rows(1);
        }
        Ok(previous)
    }

    /// Rebuilds a leaf with the arriving row packed into its sorted region.
    ///
    /// **The one way into a leaf that is not the delta area.** An ordinary
    /// write appends the row's tagged bytes below `delta_start`, which is the
    /// cheap placement and the only one the write path had. It needs the whole
    /// row to fit the gap between the mini-columns and the heap, and that gap
    /// is small on a small page: a ten-column leaf holding one row has spent
    /// 384 of a 512-byte page before the gap starts. Packed into the sorted
    /// region the same row is one slot per column plus its text in the heap,
    /// and the builder is what packs it - so the route to placing such a row is
    /// to hand the leaf's own rows and the new one to the builder together.
    ///
    /// The row under this key goes out before the new one goes in. Two rows
    /// under one key in a packed leaf is not a duplicate a later write would
    /// clean up: the sorted region is searched by comparison, so a descent
    /// finds whichever of the two the search lands on and the other is
    /// unreachable and never freed.
    ///
    /// **`repack` carries its image into the log rather than re-running.** A
    /// compaction is normally replayed by packing the page's own live rows
    /// again, which is deterministic; this one packs a row the page did not
    /// hold, so it is not derivable from the page and `repack` logs the bytes.
    ///
    /// @param database - the file
    /// @param log - where the records go
    /// @param page - the leaf
    /// @param path - the interior pages above it, root first, for a split
    /// @param row - the arriving row's values, in tree-column order
    /// @param key - its key columns, for finding the row it replaces
    /// @param arriving - the run each of its spilled values is already in
    fn pack_row_into_leaf(
        &mut self,
        database: &mut Database,
        log: &mut dyn TreeLog,
        page: PageId,
        path: &[PageId],
        row: &[Datum<'_>],
        key: &[Datum<'_>],
        arriving: &[Option<ExtentRef>],
    ) -> DbResult<()> {
        let (mut rows, mut carried) = self.rows_to_repack(database.pool(), page)?;
        // **Under the tree's own ordering, because `previous` was decided under
        // it.** `locate` compares with the tree's collations, so on a `NOCASE`
        // index it can answer that `'BLUE'` is already there when a `BINARY`
        // comparison says it is not. Asking the two questions differently
        // would pack both rows into the leaf, and a descent then finds
        // whichever of them its search lands on while the other is unreachable
        // and never freed.
        let replaced = rows.iter().position(|held| {
            let other: Vec<Datum<'_>> = held
                .iter()
                .take(self.key_columns())
                .map(OwnedDatum::borrow)
                .collect();
            crate::leaf::compare_rows_under(
                &other,
                key,
                self.key_columns(),
                self.collations(),
                self.directions(),
            ) == std::cmp::Ordering::Equal
        });
        if let Some(at) = replaced {
            if at < rows.len() {
                rows.remove(at);
            }
            if at < carried.len() {
                carried.remove(at);
            }
        }
        rows.push(row.iter().map(OwnedDatum::from_datum).collect());
        carried.push(arriving.to_vec());
        // The builder packs and does not sort, and the arriving row was pushed
        // onto the end whatever its key is.
        self.in_key_order(&mut rows, &mut carried);
        let borrowed: Vec<Vec<Datum<'_>>> = rows
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        // Not `appending`: that fill exists to leave the room on the right for
        // rows still arriving in key order, and this call is here because a
        // row did not fit, so the fill that packs both halves evenly is the one
        // that gives it somewhere to go.
        self.repack(database, log, page, path, &borrowed, &carried, false)
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
