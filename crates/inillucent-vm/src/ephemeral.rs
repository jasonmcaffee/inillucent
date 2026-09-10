//! Ephemeral row stores: the temporary tables everything nested is built on.
//!
//! Invariant: an ephemeral store answers with the same comparison rules the
//! rest of the engine uses. Its ordered index compares two rows column by
//! column with each column's own collation and with NULLs equal to each other,
//! because every consumer of this structure - `UNION`, `INTERSECT`, `EXCEPT`,
//! `DISTINCT` and `IN` - is a set operation, and a set operation is the one
//! place in SQL where two NULLs are the same value.
//!
//! A materialised subquery, a compound arm, a recursive CTE's queue and an
//! `IN` set are all this one structure. They differ only in whether the store
//! keeps an index, which is what makes a probe possible; a store with no index
//! is an ordered list that is only ever scanned.

use inillucent_value::Value;

use crate::program::{SortColumn, SortKey};
use crate::sorter::{compare_named, compare_rows_by_key};

/// Whether a store keeps an index, and what it is keyed by.
#[derive(Clone, Debug)]
pub struct Ephemeral {
    /// How many columns a row holds.
    columns: usize,
    /// The rows, in insertion order.
    rows: Vec<Vec<Value<'static>>>,
    /// Whether each row is still present, so a delete does not renumber.
    live: Vec<bool>,
    /// Row numbers ordered by the key, when the store keeps an index.
    ordered: Option<Vec<usize>>,
    /// The comparison rules the index uses.
    key: SortKey,
    /// Where a scan is, as an index into `rows`.
    position: Option<usize>,
    /// Whether any row inserted through the index held a NULL.
    ///
    /// `x IN (SELECT ...)` is NULL rather than false when `x` does not match
    /// and the set holds a NULL, and re-deriving that at probe time would mean
    /// scanning the whole set on every miss.
    saw_null: bool,
    /// How many rows are live, so `count` is not a scan.
    live_count: usize,
}

impl Ephemeral {
    /// Returns an empty store of a given width.
    ///
    /// A store with a key keeps an ordered index and can be probed; one
    /// without is append-and-scan only.
    pub fn new(columns: usize, key: Option<SortKey>) -> Ephemeral {
        let indexed = key.is_some();
        Ephemeral {
            columns,
            rows: Vec::new(),
            live: Vec::new(),
            ordered: indexed.then(Vec::new),
            key: key.unwrap_or(SortKey {
                columns: Vec::new(),
            }),
            position: None,
            saw_null: false,
            live_count: 0,
        }
    }

    /// Returns how many columns a row holds.
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Returns how many live rows the store holds.
    pub fn len(&self) -> usize {
        self.live_count
    }

    /// Returns whether the store holds no live row.
    pub fn is_empty(&self) -> bool {
        self.live_count == 0
    }

    /// Returns whether any row inserted through the index held a NULL.
    pub fn saw_null(&self) -> bool {
        self.saw_null
    }

    /// Empties the store, keeping its shape.
    ///
    /// A correlated subquery is re-run per outer row, and clearing rather than
    /// re-opening is what lets the compiler emit the open once.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.live.clear();
        if let Some(ordered) = self.ordered.as_mut() {
            ordered.clear();
        }
        self.position = None;
        self.saw_null = false;
        self.live_count = 0;
    }

    /// Appends a row, whether or not an equal one is already present.
    pub fn insert(&mut self, row: Vec<Value<'static>>) {
        let number = self.rows.len();
        self.note_nulls(&row);
        self.rows.push(row);
        self.live.push(true);
        self.live_count = self.live_count.saturating_add(1);
        if self.ordered.is_some() {
            let at = self.locate(number);
            if let Some(ordered) = self.ordered.as_mut() {
                ordered.insert(at, number);
            }
        }
    }

    /// Inserts a row only when no equal row is present, returning whether it
    /// was inserted.
    pub fn insert_unique(&mut self, row: Vec<Value<'static>>) -> bool {
        if self.contains(&row) {
            self.note_nulls(&row);
            return false;
        }
        self.insert(row);
        true
    }

    /// Returns whether an equal row is present.
    pub fn contains(&self, row: &[Value<'static>]) -> bool {
        self.find(row).is_some()
    }

    /// Removes one row equal to the given one, returning whether it removed
    /// anything.
    ///
    /// `INTERSECT` needs the removal: a duplicate on the right of the operator
    /// must not emit the left row twice, and deleting the entry as it matches
    /// is what makes the second probe miss.
    pub fn remove(&mut self, row: &[Value<'static>]) -> bool {
        let Some(at) = self.find(row) else {
            return false;
        };
        let Some(ordered) = self.ordered.as_mut() else {
            return false;
        };
        let Some(number) = ordered.get(at).copied() else {
            return false;
        };
        ordered.remove(at);
        if let Some(slot) = self.live.get_mut(number) {
            *slot = false;
        }
        self.live_count = self.live_count.saturating_sub(1);
        true
    }

    /// Keeps the first of every group of equal rows.
    ///
    /// A compound applies its operator to everything to its left, so
    /// `a UNION ALL b UNION c` de-duplicates `a UNION ALL b` before it unions
    /// `c` - which means a store built without de-duplication has to be able to
    /// acquire it after the fact.
    pub fn dedup(&mut self) {
        if self.ordered.is_none() {
            return;
        }
        let rows = core::mem::take(&mut self.rows);
        let live = core::mem::take(&mut self.live);
        self.clear();
        for (number, row) in rows.into_iter().enumerate() {
            if !live.get(number).copied().unwrap_or(false) {
                continue;
            }
            self.insert_unique(row);
        }
    }

    /// Takes every live row out of the store, leaving it empty.
    ///
    /// The window operator rewrites every row, so it takes them rather than
    /// reading and then clearing: the rows are moved once instead of copied
    /// twice, and a store half-rewritten by a failure is not a state anything
    /// can observe.
    pub fn take_rows(&mut self) -> Vec<Vec<Value<'static>>> {
        let rows = core::mem::take(&mut self.rows);
        let live = core::mem::take(&mut self.live);
        self.clear();
        rows.into_iter()
            .enumerate()
            .filter(|(number, _)| live.get(*number).copied().unwrap_or(false))
            .map(|(_, row)| row)
            .collect()
    }

    /// Orders the store's rows in place, by columns the caller names.
    pub fn sort_on(&mut self, key: &[(usize, SortColumn)]) {
        let mut rows = self.take_rows();
        rows.sort_by(|left, right| compare_named(left, right, key));
        for row in rows {
            self.insert(row);
        }
    }

    /// Positions the scan on the first live row, returning whether there is one.
    pub fn rewind(&mut self) -> bool {
        self.position = None;
        self.advance_from(0)
    }

    /// Moves the scan to the next live row, returning whether there is one.
    ///
    /// Named `next` and not an `Iterator`: a cursor is positioned rather than
    /// consumed, and the row it is on is read through the cursor afterwards -
    /// which is the opposite of what `Iterator::next` hands back.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> bool {
        let Some(position) = self.position else {
            return false;
        };
        self.advance_from(position.saturating_add(1))
    }

    /// Returns one column of the row the scan is on.
    pub fn column(&self, index: usize) -> Value<'static> {
        let Some(position) = self.position else {
            return Value::Null;
        };
        self.rows
            .get(position)
            .and_then(|row| row.get(index))
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Returns the whole row the scan is on.
    pub fn row(&self) -> Option<&[Value<'static>]> {
        let position = self.position?;
        self.rows.get(position).map(|row| row.as_slice())
    }

    /// Moves the scan to the first live row at or after a position.
    fn advance_from(&mut self, start: usize) -> bool {
        let mut position = start;
        while position < self.rows.len() {
            if self.live.get(position).copied().unwrap_or(false) {
                self.position = Some(position);
                return true;
            }
            position = position.saturating_add(1);
        }
        self.position = None;
        false
    }

    /// Records whether a row being indexed carried a NULL.
    fn note_nulls(&mut self, row: &[Value<'static>]) {
        if self.ordered.is_some() && row.iter().any(|value| matches!(value, Value::Null)) {
            self.saw_null = true;
        }
    }

    /// Returns where a row number belongs in the ordered index.
    fn locate(&self, number: usize) -> usize {
        let Some(row) = self.rows.get(number) else {
            return 0;
        };
        self.lower_bound(row)
    }

    /// Returns the first ordered position whose row is not less than a row.
    fn lower_bound(&self, row: &[Value<'static>]) -> usize {
        let Some(ordered) = self.ordered.as_ref() else {
            return 0;
        };
        let mut low = 0usize;
        let mut high = ordered.len();
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            let Some(number) = ordered.get(middle).copied() else {
                break;
            };
            let Some(candidate) = self.rows.get(number) else {
                break;
            };
            if compare_rows_by_key(candidate, row, &self.key).is_lt() {
                low = middle.saturating_add(1);
            } else {
                high = middle;
            }
        }
        low
    }

    /// Returns the ordered position of a row equal to the given one.
    fn find(&self, row: &[Value<'static>]) -> Option<usize> {
        let ordered = self.ordered.as_ref()?;
        let at = self.lower_bound(row);
        let number = ordered.get(at).copied()?;
        let candidate = self.rows.get(number)?;
        compare_rows_by_key(candidate, row, &self.key)
            .is_eq()
            .then_some(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::SortColumn;
    use inillucent_value::Collation;

    /// Returns a key over a given number of binary-collated columns.
    fn key(columns: usize) -> SortKey {
        SortKey {
            columns: (0..columns)
                .map(|_| SortColumn {
                    descending: false,
                    nulls_first: true,
                    collation: Collation::Binary,
                })
                .collect(),
        }
    }

    /// Returns a one-column row holding an integer.
    fn row(value: i64) -> Vec<Value<'static>> {
        vec![Value::Integer(value)]
    }

    #[test]
    fn an_unindexed_store_keeps_duplicates_in_order() {
        let mut store = Ephemeral::new(1, None);
        store.insert(row(2));
        store.insert(row(1));
        store.insert(row(2));
        let mut seen = Vec::new();
        if store.rewind() {
            loop {
                seen.push(store.column(0));
                if !store.next() {
                    break;
                }
            }
        }
        assert_eq!(seen.len(), 3);
        assert!(seen[0].identical(&Value::Integer(2)));
        assert!(seen[1].identical(&Value::Integer(1)));
        assert!(seen[2].identical(&Value::Integer(2)));
    }

    #[test]
    fn a_unique_insert_refuses_an_equal_row() {
        let mut store = Ephemeral::new(1, Some(key(1)));
        assert!(store.insert_unique(row(1)));
        assert!(!store.insert_unique(row(1)));
        assert!(store.insert_unique(row(2)));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn two_nulls_are_the_same_row_to_a_set() {
        let mut store = Ephemeral::new(1, Some(key(1)));
        assert!(store.insert_unique(vec![Value::Null]));
        assert!(!store.insert_unique(vec![Value::Null]));
        assert!(store.saw_null());
    }

    #[test]
    fn a_removal_makes_the_next_probe_miss() {
        let mut store = Ephemeral::new(1, Some(key(1)));
        store.insert(row(7));
        store.insert(row(7));
        assert!(store.remove(&row(7)));
        assert!(store.contains(&row(7)));
        assert!(store.remove(&row(7)));
        assert!(!store.contains(&row(7)));
        assert!(store.is_empty());
    }

    #[test]
    fn a_scan_skips_a_removed_row() {
        let mut store = Ephemeral::new(1, Some(key(1)));
        store.insert(row(1));
        store.insert(row(2));
        store.insert(row(3));
        assert!(store.remove(&row(2)));
        let mut seen = Vec::new();
        if store.rewind() {
            loop {
                seen.push(store.column(0));
                if !store.next() {
                    break;
                }
            }
        }
        assert_eq!(seen.len(), 2);
        assert!(seen[0].identical(&Value::Integer(1)));
        assert!(seen[1].identical(&Value::Integer(3)));
    }

    #[test]
    fn clearing_forgets_the_null_flag() {
        let mut store = Ephemeral::new(1, Some(key(1)));
        store.insert(vec![Value::Null]);
        assert!(store.saw_null());
        store.clear();
        assert!(!store.saw_null());
        assert!(store.is_empty());
    }
}
