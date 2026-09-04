//! What a plan costs, and which order to visit the FROM terms in.
//!
//! Invariant: cost is measured in the same currency SQLite measures it in - the
//! base-2 logarithm of the number of rows a step touches, scaled - so that a
//! plan comparison means the same thing in both engines. That is not a stylistic
//! choice. The acceptance test for this phase compares which *plan* each engine
//! picks, and two engines using different currencies disagree on ties for
//! reasons that are not defects.
//!
//! Absent statistics the numbers are SQLite's own guesses: a table holds about
//! a million rows, an equality selects a tenth of them, a range a quarter, and a
//! unique index exactly one. Those defaults are what make an unanalysed plan
//! match the reference's, so they are written out here rather than being
//! whatever seemed reasonable.

/// The row count assumed for a table nothing has measured.
///
/// SQLite's `SQLITE_DEFAULT_ROWEST`. It is deliberately large: with a small
/// guess every index looks pointless, and the plan for an unanalysed schema
/// would be a full scan of everything.
pub const DEFAULT_ROWS: f64 = 1_048_576.0;

/// The share of a table an equality on an indexed column is assumed to select.
///
/// Used only where no index is involved. An equality *on an index* is estimated
/// by [`default_equality_rows`] instead, which is an absolute count rather than
/// a share - see the note there for why the difference matters.
pub const EQUALITY_SHARE: f64 = 10.0;

/// How many rows an equality on an unmeasured index is assumed to match.
///
/// SQLite's `sqlite3DefaultRowEst` fills an unanalysed index's estimates with
/// these, as LogEst 33, 32, 30, 28, 26, 23 - about twenty rows for the first
/// equality column, falling to ten and staying there. They are *counts*, not
/// fractions of the table, and that distinction is the whole point: somebody
/// who indexed a column and then compared it for equality was pinning down a
/// row, not selecting a tenth of the table, and the bigger the table the more
/// true that is.
///
/// Getting this wrong is not a rounding error, it reverses join orders. With a
/// tenth-of-the-table estimate the planner priced a seek into a 25,000 row
/// table at 2,500 rows, decided the seek was not worth it, and scanned that
/// table once per outer row instead - which took a benchmark round from seconds
/// to the better part of an hour, and put a `SCAN` where the reference had a
/// `SEARCH ... USING INDEX`.
const DEFAULT_EQUALITY_ROWS: [f64; 6] = [20.0, 18.0, 15.0, 13.0, 11.0, 10.0];

/// Returns how many rows an equality on an unmeasured index is assumed to match.
///
/// Never more than the table holds: a two-row table cannot return twenty.
/// @param equalities - how many leading index columns the search pins down
/// @param rows - how many rows the table is estimated to hold
pub fn default_equality_rows(equalities: usize, rows: f64) -> f64 {
    let at = equalities.max(1).saturating_sub(1);
    let estimate = DEFAULT_EQUALITY_ROWS
        .get(at)
        .copied()
        .unwrap_or_else(|| DEFAULT_EQUALITY_ROWS.last().copied().unwrap_or(10.0));
    estimate.min(rows.max(1.0))
}

/// The share a range on an indexed column is assumed to select.
pub const RANGE_SHARE: f64 = 4.0;

/// What it costs to fetch a table row once an index has found its key.
///
/// A second descent of a second B-tree, so it is charged per matching row and
/// is what makes a covering index worth having.
pub const FETCH_PENALTY: f64 = 3.0;

/// What sorting a row costs relative to visiting one.
pub const SORT_FACTOR: f64 = 3.0;

/// Returns how much of a row's width one index entry is.
///
/// An entry holds the indexed columns and the row's key; a row holds every
/// column. Cost here is bytes touched, so the ratio of the two is what a
/// covering path saves over reading the rows - and it is what makes a covering
/// scan of a narrow index beat a scan of a wide table when neither has a
/// predicate to narrow it.
///
/// The floor stops a one-column index over a fifty-column table from looking
/// fifty times cheaper than it is: pages, not just bytes, are what a scan
/// touches, and a b-tree of any width has a per-entry cost that does not shrink
/// with the entry.
/// @param index_columns - how many columns the index is over
/// @param table_columns - how many the table has
pub fn entry_share(index_columns: usize, table_columns: usize) -> f64 {
    let entry = index_columns.saturating_add(1) as f64;
    let row = table_columns.max(1) as f64;
    (entry / row).clamp(ENTRY_SHARE_FLOOR, 1.0)
}

/// The least a covering entry is allowed to be worth relative to a row.
pub const ENTRY_SHARE_FLOOR: f64 = 0.25;

/// Returns the estimated cost of visiting a number of rows through a scan.
///
/// One visit per row, and no descent per row: a scan walks the leaves in order
/// and each step is to the next entry rather than from the root. Charging it a
/// descent per row - the obvious reading of "reading a row costs a descent" -
/// makes a scan of a thousand rows cost ten thousand, and then an index search
/// that returns *every* row of the table still looks cheaper than reading the
/// table. Nothing would ever choose a scan again.
pub fn scan_cost(rows: f64) -> f64 {
    rows.max(1.0)
}

/// Returns the estimated cost of a search that returns some of the rows.
///
/// One descent to find the first match, then one visit per match, plus a second
/// descent per match when the index does not carry the columns the query wants.
pub fn search_cost(rows: f64, matches: f64, covering: bool) -> f64 {
    let rows = rows.max(1.0);
    let matches = matches.max(1.0);
    let fetch = if covering { 0.0 } else { FETCH_PENALTY };
    log2(rows) + matches * (1.0 + fetch)
}

/// Returns the estimated cost of sorting a number of rows.
///
/// This one *is* `n log n`, because a sort really does compare each row against
/// a logarithmic number of others.
pub fn sort_cost(rows: f64) -> f64 {
    let rows = rows.max(1.0);
    rows * log2(rows) * SORT_FACTOR
}

/// Returns a base-2 logarithm that never goes below one.
///
/// A B-tree of one row still costs a descent, and a cost of zero would make a
/// search over an empty table free - which is how a planner ends up preferring
/// a path over a table it has not measured to one it has.
pub fn log2(rows: f64) -> f64 {
    rows.max(2.0).log2()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A search that returns one row of a million beats a scan, and by a lot.
    #[test]
    fn a_selective_search_beats_a_scan() {
        let rows = 1_000_000.0;
        assert!(search_cost(rows, 1.0, false) < scan_cost(rows) / 1000.0);
    }

    /// A search that returns every row does not - it pays a second descent per
    /// row for the privilege of reading the same table.
    #[test]
    fn an_unselective_search_does_not() {
        let rows = 1_000.0;
        assert!(search_cost(rows, rows, false) > scan_cost(rows));
    }

    /// The crossover is where it should be: a search that returns a fifth of a
    /// table is still worth it, and one that returns half is not.
    #[test]
    fn the_crossover_is_a_fraction_of_the_table() {
        let rows = 10_000.0;
        assert!(search_cost(rows, rows / 5.0, false) < scan_cost(rows));
        assert!(search_cost(rows, rows / 2.0, false) > scan_cost(rows));
    }

    /// A covering index is cheaper than the same search that has to fetch.
    #[test]
    fn covering_is_cheaper_than_fetching() {
        assert!(search_cost(1_000.0, 100.0, true) < search_cost(1_000.0, 100.0, false));
    }

    /// An empty table still costs a descent, so nothing is free.
    #[test]
    fn nothing_costs_nothing() {
        assert!(scan_cost(0.0) > 0.0);
        assert!(search_cost(0.0, 0.0, true) > 0.0);
    }
}
