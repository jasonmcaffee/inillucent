//! The undo buffer and savepoints.
//!
//! Invariant: **rollback restores every modified page to a byte-for-byte equal
//! live-row set as before the transaction.** That is the TDD's ninth. Equal *rows*,
//! not equal bytes: a leaf that was compacted on the way through holds the same
//! rows in a different layout afterwards, and requiring the layout back would
//! mean keeping the whole page rather than the rows that changed.
//!
//! ## Why there are no undo records in the log
//!
//! The policy is **no-steal**: a page dirtied by an uncommitted transaction is
//! never written to the data file. So rollback is done in memory, from this
//! buffer, and the log needs no undo records and recovery needs no undo pass.
//! The consequence is the limit the TDD names: a transaction can dirty at most
//! the pool, and beyond that it fails with the dialect's `SQLITE_FULL`
//! equivalent rather than stealing a frame.
//!
//! ## A savepoint is a position
//!
//! `SAVEPOINT` records the buffer's length; `ROLLBACK TO` applies entries in
//! reverse down to that length and truncates; `RELEASE` drops the position.
//! Nested savepoints are nested positions, which is why the structure is a
//! stack of `usize` and not a tree - the SQL nesting is already a stack.
//!
//! The subtle part is `ROLLBACK TO` on a savepoint with others inside it: the
//! inner ones have to go too, because their positions are past the point being
//! rolled back to and a position past the end of the buffer is not a savepoint,
//! it is a bug waiting for the next `ROLLBACK TO`.

use inillucent_base::error::misuse;
use inillucent_base::DbResult;

/// One row's before-image: what it held before this transaction touched it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Undo {
    /// The tree the row is in.
    pub tree: u64,
    /// The row's key, encoded.
    pub key: Vec<u8>,
    /// The row's bytes before the change, or `None` when it did not exist.
    pub before: Option<Vec<u8>>,
}

/// A named savepoint and where it sits in the buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Mark {
    name: String,
    at: usize,
}

/// One transaction's before-images, in the order they were made.
#[derive(Debug, Default)]
pub struct UndoBuffer {
    entries: Vec<Undo>,
    marks: Vec<Mark>,
    /// The largest the buffer has been, for the report and for the limit.
    high_water: usize,
}

impl UndoBuffer {
    /// Returns an empty buffer.
    pub fn new() -> UndoBuffer {
        UndoBuffer::default()
    }

    /// Returns how many before-images are held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Reports whether the transaction has changed anything.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the largest the buffer has been.
    pub fn high_water(&self) -> usize {
        self.high_water
    }

    /// Returns the open savepoints, outermost first.
    pub fn savepoints(&self) -> Vec<&str> {
        self.marks.iter().map(|mark| mark.name.as_str()).collect()
    }

    /// Records that a row is about to change.
    ///
    /// Called **before** the change, which is what makes `before` the value the
    /// row actually held. A caller that recorded afterwards would record the new
    /// value as the old one, and every test of a single change would pass.
    ///
    /// @param tree - the tree the row is in
    /// @param key - the row's key, encoded
    /// @param before - the row's bytes, or `None` when it did not exist
    pub fn record(&mut self, tree: u64, key: Vec<u8>, before: Option<Vec<u8>>) {
        self.entries.push(Undo { tree, key, before });
        self.high_water = self.high_water.max(self.entries.len());
    }

    /// Opens a savepoint.
    ///
    /// @param name - the savepoint's name
    pub fn savepoint(&mut self, name: &str) {
        self.marks.push(Mark {
            name: name.to_string(),
            at: self.entries.len(),
        });
    }

    /// Returns the entries to undo for `ROLLBACK TO`, newest first, and
    /// truncates the buffer to the savepoint.
    ///
    /// The savepoint itself stays open, which is SQL's rule: `ROLLBACK TO x`
    /// may be followed by more work and another `ROLLBACK TO x`. Savepoints
    /// opened *inside* it are closed, because their positions are past the new
    /// end of the buffer.
    ///
    /// @param name - the savepoint to roll back to
    pub fn rollback_to(&mut self, name: &str) -> DbResult<Vec<Undo>> {
        let index = self
            .marks
            .iter()
            .rposition(|mark| mark.name == name)
            .ok_or_else(|| misuse(format!("no such savepoint: {name}")))?;
        let at = self.marks.get(index).map(|mark| mark.at).unwrap_or(0);
        self.marks.truncate(index.saturating_add(1));
        let mut undone: Vec<Undo> = self.entries.split_off(at.min(self.entries.len()));
        undone.reverse();
        Ok(undone)
    }

    /// Closes a savepoint, keeping its work.
    ///
    /// Releasing a savepoint releases every savepoint inside it too, which is
    /// SQL's rule and falls out of the stack: a name is found from the top, and
    /// everything above it goes with it.
    ///
    /// @param name - the savepoint to release
    pub fn release(&mut self, name: &str) -> DbResult<()> {
        let index = self
            .marks
            .iter()
            .rposition(|mark| mark.name == name)
            .ok_or_else(|| misuse(format!("no such savepoint: {name}")))?;
        self.marks.truncate(index);
        Ok(())
    }

    /// Returns every entry, newest first, and empties the buffer.
    ///
    /// This is `ROLLBACK`: the whole transaction, applied in reverse.
    pub fn take_all(&mut self) -> Vec<Undo> {
        self.marks.clear();
        let mut undone = std::mem::take(&mut self.entries);
        undone.reverse();
        undone
    }

    /// Returns the images to publish to the version log, oldest first, and
    /// empties the buffer.
    ///
    /// This is `COMMIT`: the same entries in the other order, because the
    /// version log wants "what the row held before this transaction" and the
    /// first entry for a key is the one that says it.
    pub fn take_for_publication(&mut self) -> Vec<(u64, Vec<u8>, Option<Vec<u8>>)> {
        self.marks.clear();
        std::mem::take(&mut self.entries)
            .into_iter()
            .map(|undo| (undo.tree, undo.key, undo.before))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the three changes the tests share.
    fn buffer_with_three() -> UndoBuffer {
        let mut buffer = UndoBuffer::new();
        buffer.record(1, b"a".to_vec(), None);
        buffer.record(1, b"b".to_vec(), Some(b"old-b".to_vec()));
        buffer.record(1, b"c".to_vec(), Some(b"old-c".to_vec()));
        buffer
    }

    /// A rollback undoes everything, newest first.
    ///
    /// The order matters and is the reason this is a test rather than a
    /// comment: two changes to the same key have to be undone in reverse, or
    /// the row ends up holding the intermediate value.
    #[test]
    fn a_rollback_undoes_everything_newest_first() {
        let mut buffer = buffer_with_three();
        buffer.record(1, b"b".to_vec(), Some(b"middle-b".to_vec()));
        let undone = buffer.take_all();
        assert_eq!(undone.len(), 4);
        assert_eq!(
            undone.first().map(|undo| undo.key.clone()),
            Some(b"b".to_vec())
        );
        assert_eq!(
            undone.first().and_then(|undo| undo.before.clone()),
            Some(b"middle-b".to_vec()),
            "the newest image is undone first"
        );
        assert_eq!(
            undone.last().map(|undo| undo.key.clone()),
            Some(b"a".to_vec())
        );
        assert!(buffer.is_empty());
    }

    /// A savepoint rolls back exactly what came after it.
    #[test]
    fn a_savepoint_rolls_back_exactly_what_came_after_it() {
        let mut buffer = UndoBuffer::new();
        buffer.record(1, b"before".to_vec(), None);
        buffer.savepoint("one");
        buffer.record(1, b"after".to_vec(), Some(b"x".to_vec()));
        buffer.record(1, b"later".to_vec(), None);

        let undone = buffer.rollback_to("one").unwrap();
        assert_eq!(undone.len(), 2);
        assert_eq!(
            undone.first().map(|undo| undo.key.clone()),
            Some(b"later".to_vec())
        );
        assert_eq!(
            undone.last().map(|undo| undo.key.clone()),
            Some(b"after".to_vec())
        );
        assert_eq!(buffer.len(), 1, "what came before the savepoint stayed");
        assert_eq!(
            buffer.savepoints(),
            vec!["one"],
            "the savepoint stays open after a rollback to it"
        );

        // And it can be rolled back to again, which is what "stays open" is for.
        buffer.record(1, b"again".to_vec(), None);
        assert_eq!(buffer.rollback_to("one").unwrap().len(), 1);
        assert_eq!(buffer.len(), 1);
    }

    /// Rolling back to an outer savepoint closes the inner ones.
    ///
    /// An inner savepoint's position is past the new end of the buffer, and a
    /// position past the end is not a savepoint - it is a bug waiting for the
    /// next `ROLLBACK TO`.
    #[test]
    fn rolling_back_to_an_outer_savepoint_closes_the_inner_ones() {
        let mut buffer = UndoBuffer::new();
        buffer.savepoint("outer");
        buffer.record(1, b"a".to_vec(), None);
        buffer.savepoint("inner");
        buffer.record(1, b"b".to_vec(), None);
        assert_eq!(buffer.savepoints(), vec!["outer", "inner"]);

        assert_eq!(buffer.rollback_to("outer").unwrap().len(), 2);
        assert_eq!(buffer.savepoints(), vec!["outer"]);
        assert!(
            buffer.rollback_to("inner").is_err(),
            "an inner savepoint survived a rollback past it"
        );
    }

    /// Releasing a savepoint keeps its work and closes the ones inside it.
    #[test]
    fn releasing_keeps_the_work_and_closes_what_is_inside() {
        let mut buffer = UndoBuffer::new();
        buffer.savepoint("outer");
        buffer.record(1, b"a".to_vec(), None);
        buffer.savepoint("inner");
        buffer.record(1, b"b".to_vec(), None);

        buffer.release("outer").unwrap();
        assert_eq!(buffer.len(), 2, "releasing keeps the work");
        assert!(buffer.savepoints().is_empty());
        assert!(buffer.release("outer").is_err());
        assert!(buffer.rollback_to("inner").is_err());
    }

    /// The same name twice nests, and the inner one is found first.
    ///
    /// SQLite allows `SAVEPOINT x` twice; the inner one shadows the outer until
    /// it is released or rolled back past.
    #[test]
    fn the_same_savepoint_name_twice_nests() {
        let mut buffer = UndoBuffer::new();
        buffer.savepoint("x");
        buffer.record(1, b"outer-work".to_vec(), None);
        buffer.savepoint("x");
        buffer.record(1, b"inner-work".to_vec(), None);

        // The inner `x` is the one found, so only the inner work is undone.
        let undone = buffer.rollback_to("x").unwrap();
        assert_eq!(undone.len(), 1);
        assert_eq!(
            undone.first().map(|undo| undo.key.clone()),
            Some(b"inner-work".to_vec())
        );
        assert_eq!(buffer.savepoints(), vec!["x", "x"]);
    }

    /// Publication is oldest first; rollback is newest first.
    #[test]
    fn publication_and_rollback_run_in_opposite_orders() {
        let mut buffer = buffer_with_three();
        let published = buffer.take_for_publication();
        assert_eq!(published.len(), 3);
        assert_eq!(
            published.first().map(|entry| entry.1.clone()),
            Some(b"a".to_vec())
        );
        assert_eq!(
            published.last().map(|entry| entry.1.clone()),
            Some(b"c".to_vec())
        );
        assert!(buffer.is_empty());
        assert_eq!(buffer.high_water(), 3, "the high water mark survives");
    }
}
