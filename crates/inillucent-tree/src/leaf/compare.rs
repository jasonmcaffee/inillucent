//! Ordering two rows, and searching a page with that order.
//!
//! Invariant: **one comparison function, used by the search and by the
//! builder.** A page whose rows were ordered by one rule and searched by
//! another answers *no row* rather than an error, which is the failure this
//! file is arranged to make impossible.

use inillucent_base::error::corrupt;
use inillucent_base::DbResult;

use crate::datum::Datum;
use crate::types::PhysicalType;

use super::layout::*;
use super::read::*;
use super::*;

impl<'p> LeafRef<'p> {
    /// Returns a reusable view of the leaf's key columns.
    ///
    /// A binary search over a leaf makes about eleven comparisons, and each one
    /// re-derived the key columns' directory entries and slice bounds from the
    /// page. That is a dozen bounds-checked reads per comparison to rediscover
    /// something that does not change, and on a skip scan - which searches a
    /// leaf per seek per distinct value - it was the whole cost of the query.
    /// Deriving it once per search and passing it down makes the comparison
    /// two slice reads.
    pub fn key_view(&self) -> DbResult<KeyView<'p>> {
        let mut columns: [Option<MiniColumn<'p>>; KEY_VIEW_INLINE] = Default::default();
        let width = self.key_columns.min(KEY_VIEW_INLINE);
        for index in 0..width {
            if let Some(slot) = columns.get_mut(index) {
                *slot = Some(self.column(index)?);
            }
        }
        Ok(KeyView {
            columns,
            width: self.key_columns,
        })
    }
    /// Compares one sorted-region row's key against a probe key.
    ///
    /// @param row - the row's position in the sorted region
    /// @param probe - the key to compare against, one value per key column
    pub fn compare_key(&self, row: usize, probe: &[Datum<'_>]) -> DbResult<std::cmp::Ordering> {
        self.compare_key_with(&self.key_view()?, row, probe)
    }
    /// Compares one row's key against a probe, through a prepared key view.
    ///
    /// @param view - the leaf's key columns, from [`LeafRef::key_view`]
    /// @param row - the row's position in the sorted region
    /// @param probe - the key to compare against, one value per key column
    pub fn compare_key_with(
        &self,
        view: &KeyView<'p>,
        row: usize,
        probe: &[Datum<'_>],
    ) -> DbResult<std::cmp::Ordering> {
        for (index, wanted) in probe.iter().enumerate().take(view.width) {
            let held = match view.columns.get(index).and_then(|held| held.as_ref()) {
                Some(column) => column.value(row)?,
                // Past the inline capacity: fall back to the general path,
                // which is correct and only slower.
                None => self.value(row, index)?,
            };
            let order = self.directed(
                crate::types::compare_under(&held, wanted, self.collation_of(index)),
                index,
            );
            if order != std::cmp::Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(std::cmp::Ordering::Equal)
    }
    /// Returns the run of sorted rows whose key begins with a prefix.
    ///
    /// The half-open range `begin..end`, empty when the prefix is not present.
    ///
    /// An index nested loop asks this once per outer row, so the shape matters:
    /// the lower bound is a search, and the *upper* bound is a short forward
    /// walk rather than a second search, because the run a join probe finds is
    /// usually one entry long. Past `scan_cap` matching rows it stops walking
    /// and bisects, so a prefix that matches a whole leaf still costs a search.
    ///
    /// The integer fast path reads the leading key column's raw values, which
    /// is what [`LeafRef::lower_bound`] already does for the search; doing it
    /// for the walk as well is what removes the key view and the two
    /// `Datum` comparisons that a probe finding one entry was paying.
    ///
    /// @param prefix - the prefix to match, one value per compared column
    /// @param scan_cap - how far the run is walked before the end is bisected
    pub fn equal_run(&self, prefix: &[Datum<'_>], scan_cap: usize) -> DbResult<(usize, usize)> {
        // The guide is built once and used for both ends. Asking for it again
        // is not free: `all_typed` walks the column's class array, which on a
        // leaf holding 1,667 index entries is 417 bytes, and a probe that
        // called `lower_bound` and then re-derived the guide walked it twice.
        if let [Datum::Int(target)] = prefix {
            if let Some(guide) = self.integer_guide(prefix)? {
                let begin = self.partition_integer(&guide, *target, false)?;
                if begin >= self.row_count || guide.read(begin)? != *target {
                    return Ok((begin, begin));
                }
                let mut end = begin.saturating_add(1);
                while end < self.row_count
                    && end.saturating_sub(begin) < scan_cap
                    && guide.read(end)? == *target
                {
                    end = end.saturating_add(1);
                }
                if end.saturating_sub(begin) >= scan_cap {
                    end = self
                        .partition_integer(&guide, *target, true)?
                        .min(self.row_count);
                }
                return Ok((begin, end));
            }
        }
        let begin = self.lower_bound(prefix)?;
        if begin >= self.row_count {
            return Ok((begin, begin));
        }
        let view = self.key_view()?;
        if self.compare_key_with(&view, begin, prefix)? != std::cmp::Ordering::Equal {
            return Ok((begin, begin));
        }
        let mut end = begin.saturating_add(1);
        while end < self.row_count
            && end.saturating_sub(begin) < scan_cap
            && self.compare_key_with(&view, end, prefix)? == std::cmp::Ordering::Equal
        {
            end = end.saturating_add(1);
        }
        if end.saturating_sub(begin) >= scan_cap {
            end = self.upper_bound(prefix)?.min(self.row_count);
        }
        Ok((begin, end))
    }
    /// Finds the position of a key in the sorted region.
    ///
    /// Returns `Ok(row)` when the key is present and `Err(insertion point)` when
    /// it is not, which is `slice::binary_search`'s contract and the shape both
    /// the point probe and the insert path want.
    ///
    /// @param probe - the key to look for, one value per key column
    pub fn search(&self, probe: &[Datum<'_>]) -> DbResult<Result<usize, usize>> {
        if self.key_columns == 1 {
            // **The column comes back with the target.** Both halves need the
            // key mini-column - one to decide the probe is an integer one, the
            // other to interpolate over its values - and parsing the directory
            // entry twice to hand the same four fields back twice is a
            // measurable part of a 40 ns leaf search on a probe that does two
            // or three value reads in total.
            if let Some((target, column)) = self.integer_key_probe(probe)? {
                return self.search_integer_key(target, &column);
            }
        }
        let view = self.key_view()?;
        self.search_between(&view, probe, 0, self.row_count)
    }
    /// Binary-searches a window of the sorted region.
    ///
    /// @param view - the leaf's key columns
    /// @param probe - the key to look for
    /// @param from - the first row of the window
    /// @param to - one past the last row of the window
    pub(super) fn search_between(
        &self,
        view: &KeyView<'p>,
        probe: &[Datum<'_>],
        from: usize,
        to: usize,
    ) -> DbResult<Result<usize, usize>> {
        let mut low = from;
        let mut high = to;
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            match self.compare_key_with(view, middle, probe)? {
                std::cmp::Ordering::Less => low = middle.saturating_add(1),
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(Ok(middle)),
            }
        }
        Ok(Err(low))
    }
    /// Returns the integer a probe is looking for, when this leaf is one a
    /// rowid tree would have.
    ///
    /// The conditions are narrow on purpose: one key column, physically
    /// `Int64`, every row typed, and an integer probe. Anything else - a
    /// compound key, a NULL, an exception row, a text probe against an integer
    /// column - falls through to the general comparison, which is correct for
    /// all of them.
    ///
    /// @param probe - the key being looked for
    fn integer_key_probe(&self, probe: &[Datum<'_>]) -> DbResult<Option<(i64, MiniColumn<'p>)>> {
        if probe.len() != 1 || self.row_count < 8 {
            return Ok(None);
        }
        let Some(Datum::Int(target)) = probe.first() else {
            return Ok(None);
        };
        let column = self.column(0)?;
        if column.physical != PhysicalType::Int64 || !column.all_typed() {
            return Ok(None);
        }
        Ok(Some((*target, column)))
    }
    /// Finds an integer key by interpolation, falling back to a binary search.
    ///
    /// **This is a measurement, not a preference.** A 32 KiB leaf holds a few
    /// hundred rows, and a binary search over them touches a scattered cache
    /// line per step: measured at **120 ns** against a 100 ns descent and a
    /// 423 ns point probe, so the search was the largest single cost in three
    /// of the four read families - `read.point`, `read.range` through a rowid
    /// lookup, and `read.analytical` through the skip scan's per-seek search.
    ///
    /// Interpolation converges in one or two steps on keys that are anywhere
    /// near uniform, which a rowid is by construction: `INTEGER PRIMARY KEY`
    /// values are handed out in order. It is *not* a promise about arbitrary
    /// data, so the step count is capped and what is left of the window is
    /// binary searched. The worst case is therefore a binary search plus four
    /// probes, and the ordinary case is two.
    ///
    /// @param target - the integer being looked for
    fn search_integer_key(
        &self,
        target: i64,
        column: &MiniColumn<'p>,
    ) -> DbResult<Result<usize, usize>> {
        /// How many interpolation steps before giving up and bisecting.
        ///
        /// Four, because a distribution that has not converged in four steps is
        /// not one interpolation is going to help with, and because bounding it
        /// is what makes the worst case no worse than the search it replaces.
        const STEPS: usize = 4;

        let values = column.inline_bytes();
        let width = column.width;
        let base = column.base;
        let read = |row: usize| -> DbResult<i64> {
            let at = row.saturating_mul(width);
            let slice = values
                .get(at..at.saturating_add(width))
                .ok_or_else(|| corrupt(format!("row {row} is past the key column")))?;
            Ok(from_frame(base, width, slice))
        };

        let mut low = 0usize;
        let mut high = self.row_count.saturating_sub(1);
        let mut low_value = read(low)?;
        let mut high_value = read(high)?;
        if target < low_value {
            return Ok(Err(0));
        }
        if target > high_value {
            return Ok(Err(self.row_count));
        }
        for _ in 0..STEPS {
            if low > high {
                break;
            }
            if low_value == high_value {
                return Ok(if low_value == target {
                    Ok(low)
                } else {
                    Err(low)
                });
            }
            // The guess, in i128 so a span of nearly the whole integer range
            // cannot overflow the multiply.
            let span = i128::from(high_value).saturating_sub(i128::from(low_value));
            let into = i128::from(target).saturating_sub(i128::from(low_value));
            let width = (high.saturating_sub(low)) as i128;
            let offset = if span == 0 { 0 } else { into * width / span };
            let guess = low.saturating_add(offset.max(0).min(width) as usize);
            let seen = read(guess)?;
            match seen.cmp(&target) {
                std::cmp::Ordering::Equal => return Ok(Ok(guess)),
                std::cmp::Ordering::Less => {
                    low = guess.saturating_add(1);
                    if low > high {
                        return Ok(Err(low));
                    }
                    low_value = read(low)?;
                    if target < low_value {
                        return Ok(Err(low));
                    }
                }
                std::cmp::Ordering::Greater => {
                    if guess == 0 {
                        return Ok(Err(0));
                    }
                    high = guess.saturating_sub(1);
                    if low > high {
                        return Ok(Err(low));
                    }
                    high_value = read(high)?;
                    if target > high_value {
                        return Ok(Err(high.saturating_add(1)));
                    }
                }
            }
        }
        // Whatever window is left, bisected. The general comparison is used so
        // that this path and the one above cannot disagree about ordering.
        let view = self.key_view()?;
        self.search_between(&view, &[Datum::Int(target)], low, high.saturating_add(1))
    }
    /// Returns the first row whose key is at or above a probe.
    ///
    /// The bound an index range and an index nested loop each need once per
    /// leaf. It is an ordinary bound search with an **interpolated midpoint**
    /// for its first few steps: when the leading key column is an all-typed
    /// `Int64` - which every index on an integer column is - a guess placed by
    /// proportion lands far closer than the middle, and the loop's invariant is
    /// unchanged by where the midpoint came from.
    ///
    /// That last sentence is the whole correctness argument, and it is why the
    /// interpolation is here rather than in a separate narrowing pass: a pass
    /// that returned a *window* would have to be right about the window, and a
    /// run longer than it would put the answer outside. A midpoint cannot be
    /// wrong; it can only be a poor guess, and after a few of those the loop
    /// falls back to bisection.
    ///
    /// @param probe - the bound, one value per compared column
    pub fn lower_bound(&self, probe: &[Datum<'_>]) -> DbResult<usize> {
        self.bounded(probe, false)
    }
    /// Returns the first row whose key is above a probe.
    ///
    /// @param probe - the bound, one value per compared column
    pub fn upper_bound(&self, probe: &[Datum<'_>]) -> DbResult<usize> {
        self.bounded(probe, true)
    }
    /// The shared bound search.
    ///
    /// @param probe - the bound
    /// @param past_equal - whether a row equal to the probe is below the bound
    fn bounded(&self, probe: &[Datum<'_>], past_equal: bool) -> DbResult<usize> {
        /// How many interpolated midpoints before falling back to bisection.
        const GUESSES: u32 = 4;

        // The integer fast path, and it is a measurement rather than a
        // preference. A bound over one integer column is what a skip scan seeks
        // with and what an index nested loop probes with, and the generic
        // search reads a `Datum` out of the mini-column and compares it under a
        // collation at every step. `inillucent-probeprofile` measured a prefix
        // probe into `side_owner` - two integer key columns, 25,000 entries -
        // at 209 ns for the descent plus this search, of which the descent was
        // 62 ns. `join.range` pays it 201 times per execution.
        //
        // When the probe is one integer and the leading key column is a fully
        // typed `Int64` mini-column, the same partition point is found over a
        // contiguous run of eight-byte values, with the interpolation guide
        // still choosing the midpoints. Comparing only column zero is exactly
        // what the generic path does for a one-column probe, so this is the
        // same answer by a shorter route rather than a different one.
        if let [Datum::Int(target)] = probe {
            if let Some(guide) = self.integer_guide(probe)? {
                return self.partition_integer(&guide, *target, past_equal);
            }
        }

        let view = self.key_view()?;
        let guide = self.integer_guide(probe)?;
        let mut low = 0usize;
        let mut high = self.row_count;
        let mut guesses = 0u32;
        while low < high {
            let middle = match &guide {
                Some(guide) if guesses < GUESSES => {
                    guesses = guesses.saturating_add(1);
                    guide.between(low, high)?
                }
                _ => low.saturating_add(high.saturating_sub(low) / 2),
            };
            let order = self.compare_key_with(&view, middle, probe)?;
            let below = match order {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Equal => past_equal,
                std::cmp::Ordering::Greater => false,
            };
            if below {
                low = middle.saturating_add(1);
            } else {
                high = middle;
            }
        }
        Ok(low)
    }
    /// Returns the partition point of an integer bound over column zero.
    ///
    /// @param guide - the interpolation guide over the leading key column
    /// @param target - the integer being bounded
    /// @param past_equal - whether a row equal to the target is below the bound
    fn partition_integer(
        &self,
        guide: &IntegerGuide<'p>,
        target: i64,
        past_equal: bool,
    ) -> DbResult<usize> {
        /// How many interpolated midpoints before falling back to bisection.
        const GUESSES: u32 = 4;

        let mut low = 0usize;
        let mut high = self.row_count;
        let mut guesses = 0u32;
        while low < high {
            let middle = if guesses < GUESSES {
                guesses = guesses.saturating_add(1);
                guide.between(low, high)?
            } else {
                low.saturating_add(high.saturating_sub(low) / 2)
            };
            let held = guide.read(middle)?;
            let below = if past_equal {
                held <= target
            } else {
                held < target
            };
            if below {
                low = middle.saturating_add(1);
            } else {
                high = middle;
            }
        }
        Ok(low)
    }
    /// Returns what is needed to interpolate on the leading key column.
    ///
    /// `None` whenever interpolation does not apply, which leaves the bound
    /// search an ordinary bisection.
    ///
    /// @param probe - the bound
    fn integer_guide(&self, probe: &[Datum<'_>]) -> DbResult<Option<IntegerGuide<'p>>> {
        if probe.is_empty() || self.row_count < 8 {
            return Ok(None);
        }
        let Some(Datum::Int(target)) = probe.first() else {
            return Ok(None);
        };
        let column = self.column(0)?;
        if column.physical != PhysicalType::Int64 || !column.all_typed() {
            return Ok(None);
        }
        // A collation is an order over text and never changes where an integer
        // sits, so interpolation on an integer column is valid under any of
        // them. The guard is here so that a collation that *did* reorder
        // numbers would turn it off rather than silently mis-guess.
        if self.collation_of(0) != inillucent_value::collation::Collation::Binary {
            return Ok(None);
        }
        // **And off entirely for a descending column.** Interpolation assumes
        // the values rise across the leaf; in a descending tree they fall, and
        // a guess made on the wrong slope is a guess that lands past the row it
        // was looking for.
        if self.descending_at(0) {
            return Ok(None);
        }
        Ok(Some(IntegerGuide {
            values: column.inline_bytes(),
            width: column.width,
            base: column.base,
            target: *target,
        }))
    }
    /// Compares two materialised rows on their key columns, under the leaf's
    /// collations.
    ///
    /// @param left - one row
    /// @param right - the other row
    pub(crate) fn compare_keys(
        &self,
        left: &[Datum<'_>],
        right: &[Datum<'_>],
    ) -> std::cmp::Ordering {
        for index in 0..self.key_columns {
            let (Some(a), Some(b)) = (left.get(index), right.get(index)) else {
                return std::cmp::Ordering::Equal;
            };
            let order = self.directed(
                crate::types::compare_under(a, b, self.collation_of(index)),
                index,
            );
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    }
    /// Compares a row against a probe that may be shorter than the key.
    ///
    /// A probe of two columns against a three-column key matches a *run*, so
    /// only the columns the probe names are compared - which is the same rule
    /// `lower_bound` and `upper_bound` follow over the sorted region, and the
    /// reason a prefix bound returns three rows rather than one.
    ///
    /// @param row - the row
    /// @param probe - the bound, one value per column it names
    pub(crate) fn compare_prefix(
        &self,
        row: &[Datum<'_>],
        probe: &[Datum<'_>],
    ) -> std::cmp::Ordering {
        for (index, wanted) in probe.iter().enumerate() {
            let Some(held) = row.get(index) else {
                return std::cmp::Ordering::Less;
            };
            let order = self.directed(
                crate::types::compare_under(held, wanted, self.collation_of(index)),
                index,
            );
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    }
}

/// A leaf's key columns, derived once and reused across a binary search.
#[derive(Clone, Copy, Debug)]
pub struct KeyView<'p> {
    columns: [Option<MiniColumn<'p>>; KEY_VIEW_INLINE],
    width: usize,
}
/// A validated read view over one leaf page.
///
/// Holds no allocation: everything is derived from the page bytes on demand,
/// because a scan visits hundreds of leaves and asks each for two or three of
/// its columns.
/// Where to place a bound search's next probe, by proportion.
///
/// It reads the leading key column's raw bytes, which for an all-typed `Int64`
/// mini-column is a contiguous run of eight-byte little-endian values.
pub(crate) struct IntegerGuide<'p> {
    /// The leading key column's value array.
    values: &'p [u8],
    /// How many bytes one of its slots occupies.
    width: usize,
    /// What those slots are measured from.
    base: i64,
    /// The value being looked for.
    target: i64,
}
impl IntegerGuide<'_> {
    /// Returns a row in `low..high` to probe next.
    ///
    /// Always inside the window, so the loop that calls it terminates whatever
    /// the data looks like: a degenerate guess is a slow search, never a wrong
    /// one or a hanging one.
    ///
    /// @param low - the first row still in the window
    /// @param high - one past the last row still in the window
    fn between(&self, low: usize, high: usize) -> DbResult<usize> {
        let last = high.saturating_sub(1);
        let low_value = self.read(low)?;
        let high_value = self.read(last)?;
        if high_value <= low_value {
            return Ok(low.saturating_add(high.saturating_sub(low) / 2));
        }
        if self.target <= low_value {
            return Ok(low);
        }
        if self.target >= high_value {
            return Ok(last);
        }
        let span = i128::from(high_value).saturating_sub(i128::from(low_value));
        let into = i128::from(self.target).saturating_sub(i128::from(low_value));
        let width = last.saturating_sub(low) as i128;
        let offset = if span == 0 { 0 } else { into * width / span };
        Ok(low.saturating_add(offset.max(0).min(width) as usize))
    }

    /// Reads one row's value from the leading key column.
    ///
    /// @param row - the row to read
    fn read(&self, row: usize) -> DbResult<i64> {
        let at = row.saturating_mul(self.width);
        let slice = self
            .values
            .get(at..at.saturating_add(self.width))
            .ok_or_else(|| corrupt(format!("row {row} is past the key column")))?;
        Ok(from_frame(self.base, self.width, slice))
    }
}
/// Where a row a probe found actually lives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hit {
    /// In the sorted region, at this row.
    Sorted(usize),
    /// In the delta area, at this index.
    Delta(usize),
}
/// Compares two materialised rows on their leading key columns.
///
/// @param left - one row
/// @param right - the other row
/// @param key_columns - how many leading columns form the key
pub fn compare_rows(
    left: &[Datum<'_>],
    right: &[Datum<'_>],
    key_columns: usize,
) -> std::cmp::Ordering {
    compare_rows_under(left, right, key_columns, &[], &[])
}
/// Compares two rows' keys the way the tree they came from is ordered.
///
/// **The same comparison the tree's own searches make, which is the point.** A
/// caller that checks a tree's ordering with `BINARY` while the tree is ordered
/// by `NOCASE` is not checking the tree - it is checking a different tree. The
/// integrity check did exactly that, and `REINDEX` over
/// `CREATE INDEX ix ON t(team COLLATE NOCASE)` holding `'Blue'` and `'BLUE'`
/// reported `a key does not increase across the leaf chain` about a tree whose
/// every read was correct.
///
/// Short slices default the rest: no collation is `BINARY` and no direction is
/// ascending, which is what every rowid tree is.
///
/// @param left - one row
/// @param right - the other
/// @param key_columns - how many leading columns form the key
/// @param collations - the collation of each key column
/// @param directions - whether each key column is stored descending
pub fn compare_rows_under(
    left: &[Datum<'_>],
    right: &[Datum<'_>],
    key_columns: usize,
    collations: &[inillucent_value::collation::Collation],
    directions: &[bool],
) -> std::cmp::Ordering {
    for index in 0..key_columns {
        let (Some(a), Some(b)) = (left.get(index), right.get(index)) else {
            return std::cmp::Ordering::Equal;
        };
        let collation = collations
            .get(index)
            .copied()
            .unwrap_or(inillucent_value::collation::Collation::Binary);
        let order = crate::types::compare_under(a, b, collation);
        let order = if directions.get(index).copied().unwrap_or(false) {
            order.reverse()
        } else {
            order
        };
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    std::cmp::Ordering::Equal
}
