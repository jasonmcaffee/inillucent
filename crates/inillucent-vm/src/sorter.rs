//! The sorter and the distinct set.
//!
//! Invariant: the sort is *stable*, and the key comparison is SQL's, not
//! Rust's. Stability is what makes a query with a tie return the same rows in
//! the same order on every run, which a differential test against another
//! engine depends on; SQL's comparison is what makes NULL sort where the query
//! asked rather than where a derived `Ord` would put it.
//!
//! The distinct set answers "have I emitted this row before" in first-seen
//! order, which is the order an ephemeral index gives and therefore the order
//! the reference engine emits.

use inillucent_value::{compare, Value};

use crate::program::{SortColumn, SortKey};

/// A sorter: rows in, sorted rows out.
#[derive(Clone, Debug)]
pub struct Sorter {
    key: SortKey,
    rows: Vec<Vec<Value<'static>>>,
    position: Option<usize>,
    sorted: bool,
}

impl Sorter {
    /// Returns an empty sorter with a key description.
    pub fn new(key: SortKey) -> Sorter {
        Sorter {
            key,
            rows: Vec::new(),
            position: None,
            sorted: false,
        }
    }

    /// Adds one row.
    pub fn insert(&mut self, row: Vec<Value<'static>>) {
        self.rows.push(row);
        self.sorted = false;
    }

    /// Returns how many rows the sorter holds.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Returns whether the sorter is empty.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Sorts and positions on the first row, reporting whether there is one.
    pub fn sort(&mut self) -> bool {
        if !self.sorted {
            let key = self.key.clone();
            // `sort_by` is stable, which is the property that makes a tie
            // resolve the same way every run.
            self.rows
                .sort_by(|left, right| compare_rows(&key, left, right));
            self.sorted = true;
        }
        self.position = if self.rows.is_empty() { None } else { Some(0) };
        self.position.is_some()
    }

    /// Advances to the next row, reporting whether there is one.
    ///
    /// Named `next` and not an `Iterator`, for the reason
    /// `ephemeral::Cursor::next` gives: the row is read through the cursor
    /// after it moves, rather than handed back by the move.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> bool {
        let Some(position) = self.position else {
            return false;
        };
        let next = position.saturating_add(1);
        if next >= self.rows.len() {
            self.position = None;
            return false;
        }
        self.position = Some(next);
        true
    }

    /// Returns one column of the row the sorter is on.
    pub fn column(&self, index: usize) -> Value<'static> {
        self.position
            .and_then(|position| self.rows.get(position))
            .and_then(|row| row.get(index))
            .cloned()
            .unwrap_or(Value::Null)
    }
}

/// Compares two rows by a sort key, for callers outside this module.
///
/// The ephemeral store's index needs exactly the sorter's comparison - each
/// column's own collation, and two NULLs equal - and a second implementation
/// of it would be a second place for a set operation to disagree with an
/// `ORDER BY` about what "the same row" means.
pub fn compare_rows_by_key(
    left: &[Value<'static>],
    right: &[Value<'static>],
    key: &SortKey,
) -> std::cmp::Ordering {
    compare_rows(key, left, right)
}

/// Compares two rows by columns the caller names.
///
/// The window record is built once and sorted several times, by a different
/// set of its columns each time, so its comparator addresses columns rather
/// than assuming the key and the row line up position for position.
pub fn compare_named(
    left: &[Value<'static>],
    right: &[Value<'static>],
    key: &[(usize, SortColumn)],
) -> std::cmp::Ordering {
    for (column, rules) in key {
        let a = left.get(*column).cloned().unwrap_or(Value::Null);
        let b = right.get(*column).cloned().unwrap_or(Value::Null);
        let ordering = compare_one(rules, &a, &b);
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

/// Compares two rows by a sort key.
fn compare_rows(
    key: &SortKey,
    left: &[Value<'static>],
    right: &[Value<'static>],
) -> std::cmp::Ordering {
    for (index, column) in key.columns.iter().enumerate() {
        let a = left.get(index).cloned().unwrap_or(Value::Null);
        let b = right.get(index).cloned().unwrap_or(Value::Null);
        let ordering = compare_one(column, &a, &b);
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

/// Compares one key column, honouring its direction and null ordering.
fn compare_one(column: &SortColumn, left: &Value<'_>, right: &Value<'_>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // Null ordering is decided before the comparison, because it is not a
    // property of the values' order - `NULLS LAST` on a descending sort is not
    // the reverse of `NULLS FIRST` on an ascending one.
    match (left.is_null(), right.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => {
            return if column.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            return if column.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {}
    }
    let ordering = compare::compare_values(left, right, column.collation);
    if column.descending {
        return ordering.reverse();
    }
    ordering
}

/// The set that answers "have I already emitted this row".
#[derive(Clone, Debug, Default)]
pub struct DistinctSet {
    seen: Vec<Vec<Value<'static>>>,
    /// Positions into `seen`, ordered by [`rows_ordering`].
    ///
    /// Membership used to be a linear scan of `seen`, which made `DISTINCT`
    /// cost one comparison per row already kept - quadratic in the number of
    /// distinct values, and measured at 447 ms for a two-column `DISTINCT` over
    /// twenty thousand rows against 18 ms for the `GROUP BY` of the same shape.
    /// The order here is only a search structure: it is never read back, so it
    /// does not decide what order rows come out in, and `seen` still keeps them
    /// in the order they arrived.
    order: Vec<u32>,
    key: SortKey,
}

impl DistinctSet {
    /// Returns an empty set that compares every column with BINARY.
    pub fn new() -> DistinctSet {
        DistinctSet::default()
    }

    /// Returns an empty set with a collation per column.
    ///
    /// The collations matter: a NOCASE column makes `blue` and `Blue` one value
    /// for `DISTINCT`, and comparing them with BINARY returns a row SQLite
    /// does not.
    pub fn with_key(key: SortKey) -> DistinctSet {
        DistinctSet {
            seen: Vec::new(),
            order: Vec::new(),
            key,
        }
    }

    /// Records a row, returning whether it had been seen before.
    pub fn check(&mut self, row: Vec<Value<'static>>) -> bool {
        let key = self.key.clone();
        let found = self.order.binary_search_by(|position| {
            let candidate = self
                .seen
                .get(*position as usize)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            rows_ordering(candidate, &row, &key)
        });
        match found {
            Ok(_) => true,
            Err(at) => {
                let position = self.seen.len() as u32;
                self.seen.push(row);
                self.order.insert(at, position);
                false
            }
        }
    }

    /// Returns how many distinct rows have been seen.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Returns whether nothing has been seen.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Returns whether two rows are the same for DISTINCT purposes.
///
/// Two NULLs are the same row here, which is what `SELECT DISTINCT` does even
/// though `NULL = NULL` is not true.
/// Orders two rows the way [`rows_identical`] compares them.
///
/// A total order whose `Equal` is exactly that function's `true`, which is the
/// only property the distinct set needs of it: NULLs are equal to each other
/// and sort before everything, and every other value compares under the key's
/// collation for its column. Direction is deliberately ignored - the set never
/// reads its order back, so a descending key would only be a way for the
/// ordering and the equality to disagree.
fn rows_ordering(
    left: &[Value<'static>],
    right: &[Value<'static>],
    key: &SortKey,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let width = left.len().max(right.len());
    for index in 0..width {
        let a = left.get(index);
        let b = right.get(index);
        let (Some(a), Some(b)) = (a, b) else {
            // A row that ran out of columns is the shorter one, which is what
            // `rows_identical` calls unequal.
            return left.len().cmp(&right.len());
        };
        let ordering = match (a.is_null(), b.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => {
                let collation = key
                    .columns
                    .get(index)
                    .map_or(inillucent_value::Collation::Binary, |column| {
                        column.collation
                    });
                compare::compare_values(a, b, collation)
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

/// Returns whether two rows are the same value to `DISTINCT`.
///
/// The definition of equality the distinct set is built on. It is only called
/// from the test that proves [`rows_ordering`] agrees with it, because the set
/// itself searches by that ordering - and the whole reason the ordering is safe
/// to search by is that its `Equal` is this.
#[cfg(test)]
fn rows_identical(left: &[Value<'static>], right: &[Value<'static>], key: &SortKey) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .enumerate()
        .all(|(index, (a, b))| {
            if a.is_null() || b.is_null() {
                return a.is_null() && b.is_null();
            }
            let collation = key
                .columns
                .get(index)
                .map_or(inillucent_value::Collation::Binary, |column| {
                    column.collation
                });
            compare::compare_values(a, b, collation) == std::cmp::Ordering::Equal
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_value::Collation;

    /// Every row the distinct set could be asked about, for the property below.
    fn spread() -> Vec<Vec<Value<'static>>> {
        let text = |bytes: &[u8]| Value::owned_text(bytes).expect("text");
        vec![
            vec![Value::Null],
            vec![Value::Integer(1)],
            vec![Value::Real(1.0)],
            vec![Value::Real(1.5)],
            vec![Value::Integer(-1)],
            vec![text(b"a")],
            vec![text(b"A")],
            vec![text(b"b")],
            vec![Value::owned_blob(&[1, 2]).expect("blob")],
            vec![Value::Null, Value::Integer(1)],
            vec![Value::Integer(1), Value::Null],
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Integer(1), text(b"a")],
            vec![Value::Integer(1)],
        ]
    }

    /// The ordering the distinct set searches by says `Equal` exactly when the
    /// equality it replaced says `true`.
    ///
    /// This is the whole safety argument for searching instead of scanning: a
    /// binary search finds a row only if the ordering puts it where the
    /// equality would have. It is checked under both collations, because
    /// `NOCASE` is where the two could most easily part company.
    #[test]
    fn the_distinct_ordering_agrees_with_the_equality() {
        for collation in [Collation::Binary, Collation::NoCase] {
            let key = SortKey {
                columns: vec![
                    SortColumn {
                        descending: false,
                        nulls_first: true,
                        collation,
                    },
                    SortColumn {
                        descending: true,
                        nulls_first: false,
                        collation,
                    },
                ],
            };
            let rows = spread();
            for left in &rows {
                for right in &rows {
                    let ordered = rows_ordering(left, right, &key) == std::cmp::Ordering::Equal;
                    let identical = rows_identical(left, right, &key);
                    assert_eq!(ordered, identical, "{collation:?}: {left:?} vs {right:?}");
                }
            }
        }
    }

    /// And it is a total order, or a binary search over it would be nonsense.
    #[test]
    fn the_distinct_ordering_is_a_total_order() {
        let key = SortKey {
            columns: vec![SortColumn {
                descending: false,
                nulls_first: true,
                collation: Collation::NoCase,
            }],
        };
        let rows = spread();
        for left in &rows {
            for right in &rows {
                let forward = rows_ordering(left, right, &key);
                let backward = rows_ordering(right, left, &key);
                assert_eq!(forward, backward.reverse(), "{left:?} vs {right:?}");
                for middle in &rows {
                    let a = rows_ordering(left, middle, &key);
                    let b = rows_ordering(middle, right, &key);
                    if a == std::cmp::Ordering::Less && b == std::cmp::Ordering::Less {
                        assert_eq!(forward, std::cmp::Ordering::Less);
                    }
                }
            }
        }
    }

    /// The set answers what it answered before, on the shapes that matter.
    #[test]
    fn the_distinct_set_reports_repeats() {
        let key = SortKey {
            columns: vec![SortColumn {
                descending: false,
                nulls_first: true,
                collation: Collation::NoCase,
            }],
        };
        let mut set = DistinctSet::with_key(key);
        let text = |bytes: &[u8]| Value::owned_text(bytes).expect("text");
        assert!(!set.check(vec![text(b"a")]));
        assert!(set.check(vec![text(b"A")]), "NOCASE makes these one value");
        assert!(!set.check(vec![text(b"b")]));
        assert!(!set.check(vec![Value::Null]));
        assert!(set.check(vec![Value::Null]), "NULLs are one value here");
        assert!(!set.check(vec![Value::Integer(1)]));
        assert!(set.check(vec![Value::Real(1.0)]), "1 and 1.0 are one value");
        assert_eq!(set.len(), 4);
    }

    /// Builds a one-column key.
    fn key(descending: bool, nulls_first: bool) -> SortKey {
        SortKey {
            columns: vec![SortColumn {
                descending,
                nulls_first,
                collation: Collation::Binary,
            }],
        }
    }

    /// Asserts a list of values matches, bit for bit.
    fn assert_values(actual: &[Value<'static>], expected: &[Value<'static>]) {
        assert_eq!(actual.len(), expected.len(), "{actual:?} vs {expected:?}");
        for (left, right) in actual.iter().zip(expected.iter()) {
            assert!(left.identical(right), "{actual:?} vs {expected:?}");
        }
    }

    /// Collects a sorter's first column, in order.
    fn drain(sorter: &mut Sorter) -> Vec<Value<'static>> {
        let mut out = Vec::new();
        if !sorter.sort() {
            return out;
        }
        loop {
            out.push(sorter.column(0));
            if !sorter.next() {
                return out;
            }
        }
    }

    /// Null ordering is independent of direction: `NULLS LAST` descending is
    /// not the reverse of `NULLS FIRST` ascending.
    #[test]
    fn null_ordering_is_independent_of_direction() {
        let mut ascending = Sorter::new(key(false, true));
        for value in [Value::Integer(2), Value::Null, Value::Integer(1)] {
            ascending.insert(vec![value]);
        }
        assert_values(
            &drain(&mut ascending),
            &[Value::Null, Value::Integer(1), Value::Integer(2)],
        );

        let mut descending = Sorter::new(key(true, false));
        for value in [Value::Integer(2), Value::Null, Value::Integer(1)] {
            descending.insert(vec![value]);
        }
        assert_values(
            &drain(&mut descending),
            &[Value::Integer(2), Value::Integer(1), Value::Null],
        );
    }

    /// The sort is stable, so rows that tie keep their insertion order.
    #[test]
    fn the_sort_is_stable() {
        let mut sorter = Sorter::new(key(false, true));
        for tag in 0..5i64 {
            sorter.insert(vec![Value::Integer(1), Value::Integer(tag)]);
        }
        assert!(sorter.sort());
        let mut tags = Vec::new();
        loop {
            tags.push(sorter.column(1));
            if !sorter.next() {
                break;
            }
        }
        assert_values(
            &tags,
            &(0..5).map(Value::Integer).collect::<Vec<Value<'static>>>(),
        );
    }

    /// An empty sorter reports empty rather than positioning on nothing.
    #[test]
    fn an_empty_sorter_reports_empty() {
        let mut sorter = Sorter::new(key(false, true));
        assert!(!sorter.sort());
        assert_same!(sorter.column(0), Value::Null);
        assert!(sorter.is_empty());
    }

    /// A column's collation decides what counts as a duplicate.
    #[test]
    fn the_distinct_set_uses_each_columns_collation() {
        let key = SortKey {
            columns: vec![SortColumn {
                descending: false,
                nulls_first: true,
                collation: Collation::NoCase,
            }],
        };
        let mut set = DistinctSet::with_key(key);
        assert!(!set.check(vec![Value::owned_text(b"blue").expect("owned")]));
        assert!(set.check(vec![Value::owned_text(b"BLUE").expect("owned")]));
        assert_eq!(set.len(), 1);

        let mut binary = DistinctSet::new();
        assert!(!binary.check(vec![Value::owned_text(b"blue").expect("owned")]));
        assert!(!binary.check(vec![Value::owned_text(b"BLUE").expect("owned")]));
    }

    /// The distinct set treats two NULL rows as one.
    #[test]
    fn the_distinct_set_folds_nulls_together() {
        let mut set = DistinctSet::new();
        assert!(!set.check(vec![Value::Null]));
        assert!(set.check(vec![Value::Null]));
        assert!(!set.check(vec![Value::Integer(1)]));
        assert!(set.check(vec![Value::Integer(1)]));
        assert_eq!(set.len(), 2);
    }
}
