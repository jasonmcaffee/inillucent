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

use rustdb_value::{compare, Value};

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
            key,
        }
    }

    /// Records a row, returning whether it had been seen before.
    pub fn check(&mut self, row: Vec<Value<'static>>) -> bool {
        let key = self.key.clone();
        let seen = self
            .seen
            .iter()
            .any(|candidate| rows_identical(candidate, &row, &key));
        if !seen {
            self.seen.push(row);
        }
        seen
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
                .map_or(rustdb_value::Collation::Binary, |column| column.collation);
            compare::compare_values(a, b, collation) == std::cmp::Ordering::Equal
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustdb_value::Collation;

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
