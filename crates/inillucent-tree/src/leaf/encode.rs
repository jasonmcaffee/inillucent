//! Building a leaf page out of rows.
//!
//! Invariant: **a page is written whole or not at all.** The builder sizes
//! every region before it writes a byte, so a row that does not fit is
//! refused while the page is still empty rather than discovered half way
//! through.
use inillucent_base::DbResult;
use inillucent_pool::extent::ExtentRef;

use crate::datum::Datum;
use crate::types::{ColumnSpec, PhysicalType, ValueClass};

use super::layout::*;
use super::*;

/// Builds one leaf page from a set of rows.
///
/// This is the only writer of the layout above: bulk build, compaction and
/// split all go through it, so there is one encoder to prove correct rather
/// than three. It fills the sorted region and leaves the delta area empty, so
/// every leaf it produces is [`LeafRef::is_clean`].
pub struct LeafBuilder {
    page_size: usize,
    tree: u64,
    columns: Vec<ColumnSpec>,
    key_columns: usize,
}
/// The out-of-line values one leaf holds, read into memory.
///
/// **This is what lets a leaf answer for a value that is not in it.** A
/// `LeafRef` hands back `Datum<'p>` borrowed from the page, and an extent's
/// bytes are on other pages - so the only way the ordinary accessor can return
/// one is for the bytes to already be somewhere that outlives the borrow. That
/// somewhere is this: the caller reads the extents once through the pool, hands
/// the result to [`LeafRef::with_extents`], and every accessor then works
/// exactly as it does for an inline value.
///
/// One read per leaf rather than one per access, which matters because a leaf
/// with extents is scanned column by column and a naive resolver would re-read
/// the same value once per column pass.
#[derive(Debug, Default)]
pub struct Extents {
    /// `(row, column, bytes)`, in the order the leaf holds them.
    values: Vec<(usize, usize, Vec<u8>)>,
    /// The same, for rows in the delta area, keyed by delta index.
    ///
    /// **A delta index is not a row number**, so the two cannot share a table:
    /// delta row 0 and sorted row 0 are different rows, and a lookup that
    /// confused them would answer one row's value for another's.
    delta: Vec<(usize, usize, Vec<u8>)>,
}
impl Extents {
    /// Records one resolved value.
    ///
    /// @param row - the row's position in the sorted region
    /// @param column - which column
    /// @param bytes - the value
    pub fn push(&mut self, row: usize, column: usize, bytes: Vec<u8>) {
        self.values.push((row, column, bytes));
    }

    /// Returns one resolved value, when it was read.
    ///
    /// @param row - the row's position in the sorted region
    /// @param column - which column
    pub fn get(&self, row: usize, column: usize) -> Option<&[u8]> {
        self.values
            .iter()
            .find(|(held_row, held_column, _)| *held_row == row && *held_column == column)
            .map(|(_, _, bytes)| bytes.as_slice())
    }

    /// Records one resolved value of a delta row.
    ///
    /// @param index - the row's position in the delta area
    /// @param column - which column
    /// @param bytes - the value
    pub fn push_delta(&mut self, index: usize, column: usize, bytes: Vec<u8>) {
        self.delta.push((index, column, bytes));
    }

    /// Returns one resolved delta value, when it was read.
    ///
    /// @param index - the row's position in the delta area
    /// @param column - which column
    pub fn get_delta(&self, index: usize, column: usize) -> Option<&[u8]> {
        self.delta
            .iter()
            .find(|(held, held_column, _)| *held == index && *held_column == column)
            .map(|(_, _, bytes)| bytes.as_slice())
    }

    /// Reports whether anything was read.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.delta.is_empty()
    }
}
/// Writes the tagged form a delta row holds for an out-of-line value.
///
/// @param out - the buffer the row is being encoded into
/// @param reference - where the value was written
pub fn encode_extent_tagged(out: &mut Vec<u8>, reference: ExtentRef) {
    out.push(crate::datum::tag::EXTENT);
    out.extend_from_slice(&reference.encode());
}
/// Writes one value of its column's own type, and returns where the heap now
/// starts.
///
/// **The `Typed` arm of [`LeafBuilder::encode_rows_with`]'s per-value write,
/// held apart so the loop reads as the four classes it is choosing between.**
/// It is four different writes rather than one: an integer into a slot as
/// narrow as the column's range allows, a double's bit pattern, a heap pair for
/// a text or a blob, and a tagged value for a column with no useful type. The
/// loop that calls it already decided the class, and what it does with the
/// other three is short.
///
/// @param page - the page being built
/// @param slot - where this row's slot sits in the value array
/// @param width - how wide that slot is
/// @param frame - the column's frame of reference, or zero for none
/// @param physical - the column's layout
/// @param value - the value to write
/// @param heap_end - where the heap currently starts
pub(super) fn write_typed_value(
    page: &mut [u8],
    slot: usize,
    width: usize,
    frame: i64,
    physical: PhysicalType,
    value: &Datum<'_>,
    heap_end: usize,
) -> DbResult<usize> {
    match physical {
        PhysicalType::Int64 => {
            let target = page
                .get_mut(slot..slot.saturating_add(width))
                .ok_or_else(|| misuse("an integer slot runs past the page"))?;
            write_frame(frame, target, value.as_int().unwrap_or(0));
            Ok(heap_end)
        }
        PhysicalType::Float64 => {
            page::write_u64(
                page,
                slot,
                match value {
                    Datum::Real(number) => number.to_bits(),
                    // REAL affinity converts, which is why this is a typed
                    // value rather than an exception.
                    Datum::Int(number) => (*number as f64).to_bits(),
                    // Unreachable: `classify_at` returns `Typed` for a Float64
                    // column only for these two classes.
                    _ => return Err(unreachable_branch("a typed Float64 slot")),
                },
            )?;
            Ok(heap_end)
        }
        PhysicalType::Text | PhysicalType::Blob => {
            let bytes = value.as_bytes().unwrap_or(&[]);
            let start = heap_end
                .checked_sub(bytes.len())
                .ok_or_else(|| misuse("the heap overflowed the page"))?;
            let target = page
                .get_mut(start..start.saturating_add(bytes.len()))
                .ok_or_else(|| misuse("the heap overflowed the page"))?;
            target.copy_from_slice(bytes);
            let pair = page
                .get_mut(slot..slot.saturating_add(width))
                .ok_or_else(|| misuse("a heap slot runs past the page"))?;
            write_heap_slot(pair, start, bytes.len());
            Ok(start)
        }
        PhysicalType::Any => {
            let start = write_tagged(page, heap_end, value)?;
            page::write_u32(page, slot, start as u32)?;
            Ok(start)
        }
    }
}
/// The rows a leaf builder packs, read one value at a time.
///
/// **A value accessor, not a row iterator, and not a slice.** The builder is
/// column-major - it writes every value of one mini-column, then the next - so
/// a source that handed back whole rows would have to rebuild each row once per
/// column. And a slice is what this exists to avoid: `CREATE INDEX` holds its
/// entries in an arena, and it used to materialise a flat
/// `Vec<Datum>` in key order plus a `Vec<&[Datum]>` of slices into it purely so
/// that a `&[R]` could be passed - 6.4 MiB of copies at a hundred thousand rows,
/// of a statement whose whole resident cost was 28.9 MiB.
///
/// A source is indexed in **its own** order, which for an index build is key
/// order and not scan order. Out-of-range indices answer `Datum::Null` rather
/// than panicking, exactly as the slice form did.
pub trait Rows<'d> {
    /// How many rows there are.
    fn len(&self) -> usize;

    /// Reports whether there are none, which clippy asks for beside `len`.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns one value.
    ///
    /// @param row - which row, in this source's own order
    /// @param column - which column of it
    fn value(&self, row: usize, column: usize) -> Datum<'d>;
}
/// A slice of already-materialised rows, as a [`Rows`].
///
/// The trivial implementation, so every caller that already holds owned rows -
/// the compaction path, the fixture import, the tests - is unchanged.
pub struct RowSlice<'r, R>(pub &'r [R]);
/// Where a value too large for a leaf is written.
///
/// The builder decides *that* a value goes out of line - it is the only thing
/// that knows the page size and the layout - and this decides *where*. The two
/// are separate because the builder has no file: it is handed rows and hands
/// back a page image, and allocating a run of pages and describing it in the log
/// is the caller's business.
pub trait Spill {
    /// Writes a value out of line and returns the reference the leaf stores.
    ///
    /// **The position is passed because a repack usually has nothing to write.**
    /// A compaction, a split or a merge repacks rows that are already in the
    /// tree, and a value that was out of line before is out of line in the same
    /// run afterwards - so the spiller answers with the reference it already has
    /// and no bytes move. Without the position it could not tell that case from
    /// a value arriving for the first time, and every repack of a leaf holding
    /// three hundred out-of-line values would read and rewrite all of them to
    /// change one.
    ///
    /// @param row - the row's position among the rows being packed
    /// @param column - which column
    /// @param value - the bytes to store
    fn spill(&mut self, row: usize, column: usize, value: &[u8]) -> DbResult<ExtentRef>;
}
/// What a build produced, or why the rows would not fit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Packed {
    /// The page, and how many of the offered rows it holds.
    Filled {
        /// The encoded page.
        page: Vec<u8>,
        /// How many of the offered rows it holds.
        rows: usize,
    },
    /// Not even one row fits.
    ///
    /// With a spiller this means a row whose *inline* part alone is larger than
    /// the page - every oversized text and blob has already gone out of line, so
    /// what is left is keys, fixed-width slots and sixteen bytes per reference.
    /// Without one it is the older answer: a value too large to keep in a leaf
    /// and nowhere to put it.
    RowTooLarge,
}
/// What each half of packing one leaf image cost.
///
/// **Two numbers rather than one, because they answer different questions.**
/// Pricing the page is work any packer has to do to know the slot widths its
/// rows force; writing the page is the work a caller holding the rows in
/// column-major form already could copy instead of repeat. A single figure for
/// "pack and encode" cannot say how much of it a splice could remove, and the
/// share of it that is the encode moves with the page size.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImageTiming {
    /// Nanoseconds in the sizing pass, [`LeafBuilder::fit_widths`].
    pub sizing_nanos: u128,
    /// Nanoseconds writing the page, [`LeafBuilder::encode_rows_with`].
    pub encode_nanos: u128,
}

impl LeafBuilder {
    /// Returns a builder for one tree's leaves.
    ///
    /// @param page_size - the database's page size in bytes
    /// @param tree - the tree the leaves belong to
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    pub fn new(
        page_size: usize,
        tree: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
    ) -> DbResult<LeafBuilder> {
        if columns.is_empty() {
            return Err(misuse("a leaf needs at least one column"));
        }
        if key_columns == 0 || key_columns > columns.len() {
            return Err(misuse("key_columns must name a prefix of the columns"));
        }
        if page_size < leaf_header::DIRECTORY {
            return Err(misuse("page size is smaller than a leaf header"));
        }
        Ok(LeafBuilder {
            page_size,
            tree,
            columns,
            key_columns,
        })
    }

    /// Packs as many of `rows` as fit into one page, up to `fill` of it.
    ///
    /// Rows must already be sorted by the key columns; the builder does not
    /// sort, because every caller either has sorted input (bulk build) or has
    /// just sorted it (compaction), and sorting twice is the kind of cost that
    /// does not show up in a profile as one line.
    ///
    /// @param rows - the rows to pack, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    pub fn pack<'d, R: AsRef<[Datum<'d>]>>(&self, rows: &[R], fill: f64) -> DbResult<Packed> {
        self.pack_with(rows, fill, None)
    }

    /// Packs **every** row into one page at `fill`, or answers `None`.
    ///
    /// The difference from [`LeafBuilder::pack`] is what happens when they do
    /// not all fit: this encodes nothing. A caller walking a ladder of fills
    /// asks this at each rung, so a rung that fails costs one sizing pass
    /// rather than a whole page image that is then thrown away - which is what
    /// `make_room` was doing on every compaction of a leaf packed above
    /// `COMPACT_FILL`.
    ///
    /// @param rows - the rows to pack, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    pub fn pack_all<'d, R: AsRef<[Datum<'d>]>>(
        &self,
        rows: &[R],
        fill: f64,
    ) -> DbResult<Option<Vec<u8>>> {
        self.pack_all_rows(&RowSlice(rows), fill)
    }

    /// The row-source form of [`LeafBuilder::pack_all`].
    ///
    /// A compaction reads its rows straight out of the page it is repacking, so
    /// it has a [`Rows`] rather than a slice and never builds one.
    ///
    /// @param rows - the rows to pack, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    pub fn pack_all_rows<'d>(&self, rows: &dyn Rows<'d>, fill: f64) -> DbResult<Option<Vec<u8>>> {
        let mut timing = ImageTiming::default();
        self.pack_all_rows_timed(rows, fill, &mut timing)
    }

    /// [`LeafBuilder::pack_all_rows`], saying what each of its two passes cost.
    ///
    /// **One body, so the timed form and the plain one cannot disagree about the
    /// bytes.** A compaction's page is re-derived by recovery from the same rows
    /// rather than copied out of the log, so a second implementation of the same
    /// sizing-then-encode sequence would be a second chance for the two to drift -
    /// and such a divergence is silent, because the page checksum is computed over
    /// whatever was produced.
    ///
    /// The two `Instant::now` calls are per leaf, not per row, against a pass that
    /// reads every value of the leaf twice.
    ///
    /// @param rows - the rows to pack, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param timing - where the cost of the two passes is added
    pub fn pack_all_rows_timed<'d>(
        &self,
        rows: &dyn Rows<'d>,
        fill: f64,
        timing: &mut ImageTiming,
    ) -> DbResult<Option<Vec<u8>>> {
        let sizing = std::time::Instant::now();
        let fitted = self.fit_all_widths(rows, fill, false);
        timing.sizing_nanos = timing
            .sizing_nanos
            .saturating_add(sizing.elapsed().as_nanos());
        let Some(layout) = fitted else {
            return Ok(None);
        };
        let encoding = std::time::Instant::now();
        let image = self.encode_rows_with(rows, 0, rows.len(), None, Some(&layout))?;
        timing.encode_nanos = timing
            .encode_nanos
            .saturating_add(encoding.elapsed().as_nanos());
        Ok(Some(image))
    }

    /// Returns the layout every row resolves to, or `None` when they do not all
    /// fit one page.
    ///
    /// **The whole-leaf question, asked once instead of once per row.**
    /// [`LeafBuilder::fit_widths`] prices a leaf incrementally because its other
    /// caller, [`LeafBuilder::pack_rows`], has to stop at the first row that does
    /// not fit - so it resolves the candidate layout and recomputes the page size
    /// on **every row**, and copies the column shapes twice per row to be able to
    /// undo the last one. A compaction never needs the running answer: it either
    /// keeps every live row or it is a split.
    ///
    /// The two agree, and the reason is that the price of a run of rows never
    /// falls as rows are added. A column's shape only widens, the heap only grows
    /// and the fixed area only grows with the row count - so if the whole leaf
    /// fits the budget, so did every prefix of it, and the layout the incremental
    /// loop ends on is `resolve` over the shapes of all the rows, which is what
    /// this computes directly. When they do not all fit, both answer "no" and the
    /// layout is discarded either way. `fit_all_widths_agrees_with_fit_widths`
    /// checks it over the value classes that make the widths move.
    ///
    /// Measured on `inillucent-writelogattrib`, 2,000 inserts into a table carrying
    /// two secondary indexes at a 32 KiB page: the sizing pass inside a compaction
    /// was 4.00 ms of a 33.91 ms transaction, which was more than the encode it
    /// sizes for (2.27 ms).
    ///
    /// @param rows - the rows, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param spilling - whether the real pass will have a spiller
    fn fit_all_widths<'d>(&self, rows: &dyn Rows<'d>, fill: f64, spilling: bool) -> Option<Layout> {
        let budget = ((self.page_size as f64) * fill.clamp(0.05, 1.0)) as usize;
        let total = rows.len();
        let mut shapes: Vec<Shape> = vec![Shape::new(); self.columns.len()];
        let mut heap = 0usize;
        for row in 0..total {
            for (index, column) in self.columns.iter().enumerate() {
                let value = rows.value(row, index);
                // One classification per value, as in `fit_widths`: the heap cost
                // and the column's shape are both functions of the class.
                let class = classify_at(column.physical, &value, self.threshold(index, spilling));
                heap = heap.saturating_add(heap_cost_of(column.physical, &value, class));
                if let Some(shape) = shapes.get_mut(index) {
                    shape.observe(column.physical, &value, class, self.page_size);
                }
            }
        }
        let layout = self.resolve(&shapes);
        let size = self
            .fixed_size_with(total, &layout.widths, layout.has_bases())
            .saturating_add(heap);
        match size > budget {
            true => None,
            false => Some(layout),
        }
    }

    /// Returns how many rows from `at` would fit in one page, without encoding.
    ///
    /// **The sizing half of [`LeafBuilder::pack_rows`], on its own.** A bulk
    /// build has to allocate its leaves as one contiguous run, so it has to know
    /// how many leaves there will be before it writes the first one - and the
    /// only way to find out used to be to pack every leaf into a
    /// `Vec<Vec<u8>>` and count them, which is a whole copy of the tree held in
    /// memory for the sake of one integer. A `CREATE INDEX` over a hundred
    /// thousand rows spent 6.2 MiB that way.
    ///
    /// The count is exact rather than an estimate: it is the same forward pass
    /// [`LeafBuilder::pack_rows`] runs, over the same values, with the same
    /// spill threshold. It does not spill - it only *prices* a spill, which is
    /// what makes running it twice safe.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to place
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param spilling - whether the real pass will have a spiller
    pub fn fit<'d>(&self, rows: &dyn Rows<'d>, at: usize, fill: f64, spilling: bool) -> usize {
        self.fit_widths(rows, at, fill, spilling).0
    }

    /// Returns both halves of the sizing pass: how many rows fit, and the slot
    /// widths those rows force.
    ///
    /// **The widths are handed back rather than recomputed** because
    /// [`LeafBuilder::encode_rows`] would otherwise derive them a second time,
    /// over the same values, by the same rule - a whole extra pass over the
    /// rows of every leaf a compaction or a bulk build writes.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to place
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param spilling - whether the real pass will have a spiller
    pub fn fit_widths<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        fill: f64,
        spilling: bool,
    ) -> (usize, Layout) {
        let budget = ((self.page_size as f64) * fill.clamp(0.05, 1.0)) as usize;
        let mut heap = 0usize;
        let mut placed = 0usize;
        let total = rows.len();
        let mut row = at;
        // **A column's shape widens as rows are added, and never narrows.** A
        // value outside the span makes the whole column wider, which raises the
        // price of the rows already placed - so the size is recomputed against
        // the widened column rather than accumulated. The loop is still one
        // forward pass and still exact.
        let mut shapes: Vec<Shape> = vec![Shape::new(); self.columns.len()];
        let mut wanted: Vec<Shape> = shapes.clone();
        let mut layout = self.resolve(&shapes);
        // **The candidate layout is one buffer, written over, and not a new one a
        // row.** `resolve` returns a `Layout`, and a `Layout` owns two `Vec`s - so
        // asking it for the price of each row in turn allocated and freed two
        // vectors per row of every leaf a compaction repacks, to read four numbers
        // out of them and usually throw them away. The widths and bases a row
        // forces are written into these two buffers instead, and accepting a row
        // copies them into `layout` in place.
        let mut candidate = layout.clone();
        while row < total {
            let mut row_heap = 0usize;
            wanted.copy_from_slice(&shapes);
            for (index, column) in self.columns.iter().enumerate() {
                let value = rows.value(row, index);
                // **One classification per value, not two.** The heap cost and
                // the column's shape are both functions of the class, and
                // asking for them separately classified every value of every
                // column twice - on the path a compaction and a bulk build
                // both take.
                let class = classify_at(column.physical, &value, self.threshold(index, spilling));
                row_heap = row_heap.saturating_add(heap_cost_of(column.physical, &value, class));
                if let Some(shape) = wanted.get_mut(index) {
                    shape.observe(column.physical, &value, class, self.page_size);
                }
            }
            let next = placed.saturating_add(1);
            self.resolve_into(&mut candidate, &wanted);
            let size = self
                .fixed_size_with(next, &candidate.widths, candidate.has_bases())
                .saturating_add(heap.saturating_add(row_heap));
            if size > budget {
                break;
            }
            shapes.copy_from_slice(&wanted);
            layout.widths.copy_from_slice(&candidate.widths);
            layout.bases.copy_from_slice(&candidate.bases);
            heap = heap.saturating_add(row_heap);
            placed = next;
            row = row.saturating_add(1);
        }
        (placed, layout)
    }

    /// Packs as many rows from `at` as fit, sending oversized values out of line.
    ///
    /// The row-source form of [`LeafBuilder::pack_with`]: identical arithmetic,
    /// reading its values through [`Rows`] instead of out of a slice, so a
    /// caller whose rows are in an arena never has to build the slice.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to place
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param spill - where an oversized value goes, when there is somewhere
    pub fn pack_rows<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        fill: f64,
        spill: Option<&mut dyn Spill>,
    ) -> DbResult<Packed> {
        let (placed, layout) = self.fit_widths(rows, at, fill, spill.is_some());
        if placed == 0 {
            return Ok(Packed::RowTooLarge);
        }
        let page = self.encode_rows_with(rows, at, placed, spill, Some(&layout))?;
        Ok(Packed::Filled { page, rows: placed })
    }

    /// Packs as many of `rows` as fit, sending oversized values out of line.
    ///
    /// With `None` for the spiller nothing goes out of line and this is
    /// [`LeafBuilder::pack`] exactly - which is what the import wants, because
    /// it builds into a file nothing has read and measures the same bytes twice.
    ///
    /// @param rows - the rows to pack, sorted by key
    /// @param fill - the fraction of the page to fill, 0.0..=1.0
    /// @param spill - where an oversized value goes, when there is somewhere
    ///
    /// **Generic over the row's container, and that is the whole point.** A
    /// `Vec<Datum>` already implements `AsRef<[Datum]>`, so every caller that
    /// holds owned rows (the compaction path in `write.rs`, the fixture
    /// import) compiles unchanged; and a caller that has its rows in an arena passes
    /// `&[&[Datum]]` and copies nothing. `CREATE INDEX` used to materialise a
    /// `Vec<Vec<Datum>>` of the whole input purely to call this, which was
    /// 4.2 ms of a 48 ms statement at a hundred thousand rows.
    pub fn pack_with<'d, R: AsRef<[Datum<'d>]>>(
        &self,
        rows: &[R],
        fill: f64,
        spill: Option<&mut dyn Spill>,
    ) -> DbResult<Packed> {
        // **One forward pass over the rows it places, not a binary search over
        // the rows it does not.** The pass itself is [`LeafBuilder::fit`] now;
        // the reasoning it was written with is kept here because this is the
        // signature everything but the bulk builder still calls.
        //
        // The size of a prefix is a closed form in the count plus a prefix sum
        // over the rows' heap costs, so a running total answers "does the next
        // row still fit" in the cost of that one row. The version this replaces
        // bisected `0..rows.len()` and re-measured a whole prefix per probe -
        // correct, and quadratic in the wrong argument: a bulk build hands the
        // *entire remaining input* to every pack, so filling the first leaf of
        // a hundred thousand rows measured about fifty thousand rows seventeen
        // times to place a hundred and forty-five.
        //
        // It cost 96 ms of a 155 ms `CREATE INDEX` and it was not the first
        // guess. The first guess was the write-ahead log, which a measurement
        // with the log switched off showed costs nothing at all.
        self.pack_rows(&RowSlice(rows), 0, fill, spill)
    }

    /// Returns the longest value one column keeps in the leaf.
    ///
    /// **A key column never spills, whatever its length.** Every comparison the
    /// tree makes - the binary search inside a leaf, the separator an interior
    /// page holds, the order a bulk build relies on - reads key columns out of
    /// the page, and a key whose bytes were on another page would turn each of
    /// those into a page fetch. A long key is a slow tree; a long key out of
    /// line would be a tree that cannot be searched without the pool.
    ///
    /// @param column - which column
    /// @param spilling - whether the caller gave a spiller
    fn threshold(&self, column: usize, spilling: bool) -> usize {
        if !spilling || column < self.key_columns {
            return usize::MAX;
        }
        self.page_size / EXTENT_DIVISOR
    }

    /// Returns the bytes a leaf of `count` rows spends before its heap, at the
    /// slot widths given.
    ///
    /// The widths are a parameter rather than a property of the column
    /// directory because they are a property of the *values*: see
    /// [`NARROW_INT_SLOTS`]. Every caller that prices a page and the one that
    /// writes it derive them from the same rows by the same rule, which is what
    /// keeps `fit` and `encode_rows` in agreement.
    ///
    /// @param count - how many rows
    /// @param widths - the slot width of each column, in directory order
    /// @param wide_directory - whether the entries carry a base each
    fn fixed_size_with(&self, count: usize, widths: &[usize], wide_directory: bool) -> usize {
        let entry = if wide_directory {
            DIRECTORY_ENTRY_WIDE
        } else {
            DIRECTORY_ENTRY
        };
        let mut fixed =
            leaf_header::DIRECTORY.saturating_add(self.columns.len().saturating_mul(entry));
        for (index, column) in self.columns.iter().enumerate() {
            let width = widths
                .get(index)
                .copied()
                .unwrap_or_else(|| column.physical.slot_width());
            fixed = fixed
                .saturating_add(class_bytes(count))
                .saturating_add(count.saturating_mul(width));
            // Every mini-column starts 8-byte aligned; the class array is
            // already a multiple of eight and so is an inline value array, but
            // an `Any` array of an odd row count is not.
            fixed = align8(fixed);
        }
        fixed
    }

    /// Returns the layout a run of rows forces on this builder's columns.
    ///
    /// One pass over the values, widening each column's shape. It is the same
    /// rule [`LeafBuilder::fit_widths`] applies incrementally, run over a row
    /// count that has already been decided - so the page `encode_rows` lays out
    /// is the page `fit` priced.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row
    /// @param count - how many rows
    /// @param spilling - whether the real pass will have a spiller
    fn layout_over<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        count: usize,
        spilling: bool,
    ) -> Layout {
        let mut shapes: Vec<Shape> = vec![Shape::new(); self.columns.len()];
        for row in 0..count {
            for (index, column) in self.columns.iter().enumerate() {
                let Some(shape) = shapes.get_mut(index) else {
                    continue;
                };
                let value = rows.value(at.saturating_add(row), index);
                let class = classify_at(column.physical, &value, self.threshold(index, spilling));
                shape.observe(column.physical, &value, class, self.page_size);
            }
        }
        self.resolve(&shapes)
    }

    /// Turns a set of column shapes into the layout they resolve to.
    ///
    /// Allocates. [`LeafBuilder::fit_widths`] asks for a layout once per row and
    /// uses [`LeafBuilder::resolve_into`] instead.
    ///
    /// @param shapes - one shape per column
    fn resolve(&self, shapes: &[Shape]) -> Layout {
        let mut layout = Layout {
            widths: vec![0; self.columns.len()],
            bases: vec![0; self.columns.len()],
        };
        self.resolve_into(&mut layout, shapes);
        layout
    }

    /// Writes the layout a set of column shapes resolves to into a buffer that
    /// already has room for it.
    ///
    /// Same rule as [`LeafBuilder::resolve`] and the same numbers; it writes them
    /// into `layout` rather than into two fresh vectors. A layout whose vectors are
    /// shorter than the column count keeps whatever it held past the end, which is
    /// why the only caller sizes its buffer from `resolve` before the loop.
    ///
    /// @param layout - the buffer to write into, one slot per column
    /// @param shapes - one shape per column
    fn resolve_into(&self, layout: &mut Layout, shapes: &[Shape]) {
        for (index, column) in self.columns.iter().enumerate() {
            let shape = shapes.get(index).copied().unwrap_or_else(Shape::new);
            let (width, base) = shape.resolve(column.physical);
            if let Some(slot) = layout.widths.get_mut(index) {
                *slot = width;
            }
            if let Some(slot) = layout.bases.get_mut(index) {
                *slot = base;
            }
        }
    }

    /// Encodes the rows into a page.
    ///
    /// @param rows - the rows to encode, sorted by key
    pub fn encode<'d, R: AsRef<[Datum<'d>]>>(&self, rows: &[R]) -> DbResult<Vec<u8>> {
        self.encode_with(rows, None)
    }

    /// Encodes a leaf holding no rows.
    ///
    /// Named rather than written as `encode(&[])`, because a generic `encode`
    /// cannot infer the row container from an empty slice and the turbofish it
    /// would otherwise need at each call site says nothing to a reader.
    pub fn encode_empty(&self) -> DbResult<Vec<u8>> {
        self.encode_with::<&[Datum<'_>]>(&[], None)
    }

    /// Encodes the rows into a page, sending oversized values out of line.
    ///
    /// @param rows - the rows to encode, sorted by key
    /// @param spill - where an oversized value goes, when there is somewhere
    pub fn encode_with<'d, R: AsRef<[Datum<'d>]>>(
        &self,
        rows: &[R],
        spill: Option<&mut dyn Spill>,
    ) -> DbResult<Vec<u8>> {
        self.encode_rows(&RowSlice(rows), 0, rows.len(), spill)
    }

    /// Encodes `count` rows from `at` into one leaf page.
    ///
    /// The row-source form of [`LeafBuilder::encode_with`]. It reads
    /// column-major - every value of one column, then the next - which is why
    /// [`Rows`] is a value accessor rather than a row iterator: a row-at-a-time
    /// source would have to rebuild each row once per column.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to encode
    /// @param count - how many to encode
    /// @param spill - where an oversized value goes, when there is somewhere
    pub fn encode_rows<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        count: usize,
        spill: Option<&mut dyn Spill>,
    ) -> DbResult<Vec<u8>> {
        self.encode_rows_with(rows, at, count, spill, None)
    }

    /// Encodes `count` rows from `at`, at slot widths the caller may already
    /// have.
    ///
    /// [`LeafBuilder::fit_widths`] derives the widths as it prices the page, so
    /// a caller that has just called it passes them here rather than paying for
    /// a second pass over the same values.
    ///
    /// @param rows - the rows, sorted by key
    /// @param at - the first row to encode
    /// @param count - how many to encode
    /// @param spill - where an oversized value goes, when there is somewhere
    /// @param layout - the widths and bases, when the caller already derived them
    pub fn encode_rows_with<'d>(
        &self,
        rows: &dyn Rows<'d>,
        at: usize,
        count: usize,
        mut spill: Option<&mut dyn Spill>,
        layout: Option<&Layout>,
    ) -> DbResult<Vec<u8>> {
        if count > u16::MAX as usize {
            return Err(misuse("a leaf cannot hold more than 65535 rows"));
        }
        let mut page = vec![0u8; self.page_size];
        page::write_common(&mut page, PageKind::Leaf, 0, self.tree)?;
        // **One pass for the widths, then the layout** - unless the caller
        // already made that pass. `fit_widths` widened its columns over exactly
        // these rows by exactly this rule, so the page it priced is the page
        // laid out here either way.
        let derived;
        let layout: &Layout = match layout {
            Some(held) => held,
            None => {
                derived = self.layout_over(rows, at, count, spill.is_some());
                &derived
            }
        };
        let wide_directory = layout.has_bases();

        // Lay the mini-columns out first so the directory can name them.
        //
        // `layout` rather than `at`: `at` is this function's first-row argument,
        // and a layout cursor called the same thing shadowed it silently - which
        // would have read every value out of the wrong row.
        let mut offsets = Vec::with_capacity(self.columns.len());
        let entry_size = if wide_directory {
            DIRECTORY_ENTRY_WIDE
        } else {
            DIRECTORY_ENTRY
        };
        let mut cursor = align8(
            leaf_header::DIRECTORY.saturating_add(self.columns.len().saturating_mul(entry_size)),
        );
        for (index, column) in self.columns.iter().enumerate() {
            offsets.push(cursor);
            cursor = cursor
                .saturating_add(class_bytes(count))
                .saturating_add(count.saturating_mul(layout.width(index, column)));
            cursor = align8(cursor);
        }
        if cursor > self.page_size {
            return Err(misuse("the mini-columns do not fit in one page"));
        }

        // The heap grows down from the page end. Every variable-width value and
        // every exception is appended to it as the columns are written.
        let mut heap_end = self.page_size;
        let mut has_exceptions = false;
        let mut has_extents = false;
        // Which columns turned out to hold nothing but present, correctly typed
        // values. The builder is walking every value anyway, so recording the
        // answer costs a branch and saves every later reader the walk.
        let mut all_typed: Vec<bool> = vec![true; self.columns.len()];

        for (index, column) in self.columns.iter().enumerate() {
            let base = offsets.get(index).copied().unwrap_or(0);
            let values_at = base.saturating_add(class_bytes(count));
            let threshold = self.threshold(index, spill.is_some());
            let width = layout.width(index, column);
            let frame = layout.base(index);
            for row in 0..count {
                let value = rows.value(at.saturating_add(row), index);
                let class = classify_at(column.physical, &value, threshold);
                if class == ValueClass::Exception {
                    has_exceptions = true;
                }
                if class == ValueClass::Extent {
                    has_extents = true;
                }
                if class != ValueClass::Typed {
                    if let Some(slot) = all_typed.get_mut(index) {
                        *slot = false;
                    }
                }
                set_class(&mut page, base, row, class)?;
                let slot = values_at.saturating_add(row.saturating_mul(width));
                match class {
                    ValueClass::Null => {}
                    ValueClass::Typed => {
                        heap_end = write_typed_value(
                            &mut page,
                            slot,
                            width,
                            frame,
                            column.physical,
                            &value,
                            heap_end,
                        )?;
                    }
                    ValueClass::Exception => {
                        heap_end = write_tagged(&mut page, heap_end, &value)?;
                        let narrow = narrow_pair_at(column.physical, width);
                        let target = page
                            .get_mut(slot..slot.saturating_add(width))
                            .ok_or_else(|| misuse("a slot runs past the page"))?;
                        write_slot_offset(target, heap_end, narrow);
                    }
                    ValueClass::Extent => {
                        // Unreachable without a spiller: `classify_at` returns
                        // this class only when `threshold` is finite, and
                        // `threshold` is `usize::MAX` when there is none.
                        let spiller = spill
                            .as_deref_mut()
                            .ok_or_else(|| unreachable_branch("an extent with no spiller"))?;
                        // The reference says what the bytes are whenever the
                        // column would say something else - which is what lets
                        // a column declared `BLOB`, or declared nothing at all,
                        // hold a text out of line (task-1986). A repack states
                        // it over a reference it carried unchanged: the bytes do
                        // not move, and a carried reference for a column that
                        // answers correctly is stated as `Unstated` and so is
                        // encoded exactly as it was.
                        let reference = spiller
                            .spill(row, index, value.as_bytes().unwrap_or(&[]))?
                            .stating(super::extent_class_for(column.physical, &value));
                        heap_end = heap_end
                            .checked_sub(EXTENT_REF_BYTES)
                            .ok_or_else(|| misuse("the heap overflowed the page"))?;
                        let target = page
                            .get_mut(heap_end..heap_end.saturating_add(EXTENT_REF_BYTES))
                            .ok_or_else(|| misuse("the heap overflowed the page"))?;
                        target.copy_from_slice(&reference.encode());
                        let narrow = narrow_pair_at(column.physical, width);
                        let held = page
                            .get_mut(slot..slot.saturating_add(width))
                            .ok_or_else(|| misuse("a slot runs past the page"))?;
                        write_slot_offset(held, heap_end, narrow);
                    }
                }
            }
        }

        if heap_end < cursor {
            return Err(misuse("the heap collided with the mini-columns"));
        }

        // The delta area is empty and sits where the heap begins, so a later
        // insert has the whole free gap to grow into.
        let delta_start = heap_end;
        page::write_u16(&mut page, leaf_header::ROW_COUNT, count as u16)?;
        page::write_u16(&mut page, leaf_header::DELTA_COUNT, 0)?;
        page::write_u16(
            &mut page,
            leaf_header::COLUMN_COUNT,
            self.columns.len() as u16,
        )?;
        page::write_u16(&mut page, leaf_header::KEY_COLUMNS, self.key_columns as u16)?;
        page::write_u32(&mut page, leaf_header::HEAP_START, heap_end as u32)?;
        page::write_u32(&mut page, leaf_header::DELTA_START, delta_start as u32)?;
        page::write_u64(&mut page, leaf_header::MAX_CTS, 0)?;
        let low_fence = if count == 0 {
            0
        } else {
            rows.value(at, 0).as_int().unwrap_or(0)
        };
        page::write_u64(&mut page, leaf_header::LOW_FENCE, low_fence as u64)?;
        for (index, column) in self.columns.iter().enumerate() {
            let entry = leaf_header::DIRECTORY.saturating_add(index.saturating_mul(entry_size));
            let type_slot = page
                .get_mut(entry)
                .ok_or_else(|| misuse("the directory does not fit"))?;
            *type_slot = column.physical.code();
            let flag_slot = page
                .get_mut(entry.saturating_add(1))
                .ok_or_else(|| misuse("the directory does not fit"))?;
            let key_bit = if index < self.key_columns {
                column.flags | COLUMN_KEY
            } else {
                column.flags & !COLUMN_KEY
            };
            *flag_slot = if all_typed.get(index).copied().unwrap_or(false) {
                key_bit | COLUMN_ALL_TYPED
            } else {
                key_bit & !COLUMN_ALL_TYPED
            };
            page::write_u16(
                &mut page,
                entry.saturating_add(2),
                layout.width(index, column) as u16,
            )?;
            page::write_u32(
                &mut page,
                entry.saturating_add(4),
                offsets.get(index).copied().unwrap_or(0) as u32,
            )?;
            if wide_directory {
                page::write_u64(
                    &mut page,
                    entry.saturating_add(8),
                    layout.base(index) as u64,
                )?;
            }
        }
        // Every leaf this build writes is in format 2's layout. See
        // `LEAF_DELTA_DIRECTORY`.
        {
            let flags = page
                .get_mut(header::FLAGS)
                .ok_or_else(|| misuse("the page has no flag byte"))?;
            *flags |= LEAF_DELTA_DIRECTORY;
            if wide_directory {
                *flags |= LEAF_WIDE_DIRECTORY;
            }
        }
        if has_exceptions || has_extents {
            let flags = page
                .get_mut(header::FLAGS)
                .ok_or_else(|| misuse("the page has no flag byte"))?;
            if has_exceptions {
                *flags |= LEAF_HAS_EXCEPTIONS;
            }
            if has_extents {
                *flags |= LEAF_HAS_EXTENTS;
            }
        }
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One leaf's worth of rows, built to move the things a layout depends on.
    ///
    /// @param count - how many rows
    /// @param labels - the text values, which outlive the rows that borrow them
    /// @param widen - whether the last row carries a value that widens a column
    /// @param ragged - whether some rows carry a null and a wrongly typed value
    fn rows_of<'t>(
        count: usize,
        labels: &'t [String],
        widen: bool,
        ragged: bool,
    ) -> Vec<Vec<Datum<'t>>> {
        (0..count)
            .map(|n| {
                let last = n.saturating_add(1) == count;
                let third = match widen && last {
                    true => Datum::Int(i64::MAX),
                    false => Datum::Int(n as i64),
                };
                let second = match ragged && n % 7 == 3 {
                    // An integer in a text column is an exception, which is
                    // stored on the heap and narrows nothing.
                    true => Datum::Int(n as i64),
                    false => match ragged && n % 11 == 5 {
                        true => Datum::Null,
                        false => labels
                            .get(n)
                            .map(|held| Datum::Text(held.as_bytes()))
                            .unwrap_or(Datum::Null),
                    },
                };
                vec![Datum::Int(n as i64), second, third]
            })
            .collect()
    }

    /// The one-pass sizing pass answers exactly what the incremental one does.
    ///
    /// **Because a compaction's page is re-derived by recovery rather than copied
    /// out of the log.** `pack_all_rows` sizes a leaf with `fit_all_widths` and
    /// `pack_rows` sizes it with `fit_widths`, and if the two ever chose different
    /// slot widths for the same rows, a leaf compacted by a live write and the same
    /// leaf compacted by a replay would differ in their bytes - and nothing would
    /// say so, because the page checksum is computed over whatever was produced.
    ///
    /// So this asserts the page, not just the verdict: for every row set and every
    /// fill, the two agree on whether the rows fit, on the widths and bases they
    /// force, and on the 4,096 bytes that come out.
    #[test]
    fn fit_all_widths_agrees_with_fit_widths() {
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
            ColumnSpec::new(PhysicalType::Int64),
        ];
        let builder = LeafBuilder::new(4_096, 1, columns, 1).expect("a builder");
        let labels: Vec<String> = (0..400).map(|n| format!("label-{n:06}")).collect();
        let mut fitted = 0usize;
        let mut refused = 0usize;
        for count in [0usize, 1, 2, 17, 64, 120, 260, 400] {
            for widen in [false, true] {
                for ragged in [false, true] {
                    for fill in [0.25f64, 0.75, 0.95, 1.0] {
                        let rows = rows_of(count, &labels, widen, ragged);
                        let source = RowSlice(&rows);
                        let (placed, incremental) = builder.fit_widths(&source, 0, fill, false);
                        let whole = builder.fit_all_widths(&source, fill, false);
                        let all = placed == rows.len();
                        assert_eq!(
                            all,
                            whole.is_some(),
                            "count {count}, widen {widen}, ragged {ragged}, fill {fill}: \
                             incremental placed {placed} of {}, one-pass said {}",
                            rows.len(),
                            whole.is_some()
                        );
                        let Some(whole) = whole else {
                            refused = refused.saturating_add(1);
                            continue;
                        };
                        fitted = fitted.saturating_add(1);
                        assert_eq!(
                            incremental.widths, whole.widths,
                            "count {count}, widen {widen}, ragged {ragged}, fill {fill}: widths"
                        );
                        assert_eq!(
                            incremental.bases, whole.bases,
                            "count {count}, widen {widen}, ragged {ragged}, fill {fill}: bases"
                        );
                        let by_hand = builder
                            .encode_rows_with(&source, 0, placed, None, Some(&incremental))
                            .expect("the incremental layout encodes");
                        let packed = builder
                            .pack_all_rows(&source, fill)
                            .expect("packing does not fail")
                            .expect("the rows fit");
                        assert_eq!(
                            by_hand, packed,
                            "count {count}, widen {widen}, ragged {ragged}, fill {fill}: \
                             the two sizing passes produced different pages"
                        );
                    }
                }
            }
        }
        // **The case counts are asserted so the test cannot pass by testing
        // nothing.** A row set that never fits would make every comparison above
        // vacuous, and a fill that always fits would never exercise the refusal.
        assert!(fitted >= 40, "only {fitted} row sets fitted a page");
        assert!(refused >= 10, "only {refused} row sets were refused");
    }
}
