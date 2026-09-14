//! The reference: what the engine is supposed to answer, kept in `BTreeMap`s.
//!
//! Invariant: every rule here is stated once, in the place a reader would look
//! for it. A transaction reads its snapshot plus its own writes; a commit makes
//! its writes visible to transactions that begin after it and to nobody else; a
//! rollback makes them visible to nobody; a crash keeps a prefix of the commits.
//! There is no optimisation anywhere in this file, and there is not meant to be.

use std::collections::BTreeMap;

/// The committed contents of every tree.
///
/// The key is `(tree, key)` rather than a map of maps, because every question
/// the model is asked is about one key and the nesting bought nothing but a
/// second lookup and a second chance to get the empty case wrong.
pub type State = BTreeMap<(u32, u64), Vec<u8>>;

/// What one committed transaction changed.
///
/// The *whole* change, as absolute values rather than as a delta, because a
/// crash replays a prefix and a prefix of deltas is only meaningful if every
/// earlier delta was applied - which is the assumption a torn commit breaks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Commit {
    /// The transaction's number, for the diagnostic.
    pub txn: u32,
    /// The keys it wrote, and what it wrote. `None` is a delete.
    pub writes: Vec<(Key, Written)>,
}

/// Which tree and which row: the whole of what this model calls a key.
pub type Key = (u32, u64);

/// What a write left at a key. `None` is a delete.
pub type Written = Option<Vec<u8>>;

/// One open transaction.
#[derive(Clone, Debug)]
struct Open {
    /// The committed state as it was when the transaction began.
    ///
    /// A whole copy. It is the obviously-correct implementation of a snapshot,
    /// and obvious is what this file is for; the traces are hundreds of keys,
    /// not millions.
    snapshot: State,
    /// This transaction's own writes, newest wins.
    writes: Writes,
    /// The open savepoints, outermost first, each with the writes as they stood
    /// when it was taken.
    savepoints: Vec<(String, Writes)>,
}

/// What a transaction has written, newest wins.
type Writes = BTreeMap<Key, Written>;

/// Something the engine did that the model says it should not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// Which rule was broken, in words a reader can act on.
    pub rule: String,
    /// What the model expected.
    pub expected: String,
    /// What the engine did.
    pub found: String,
}

/// The model.
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// The committed state.
    committed: State,
    /// The transactions that are open, by number.
    open: BTreeMap<u32, Open>,
    /// Every commit, in the order it happened.
    history: Vec<Commit>,
    /// How many of those the engine has said are on the media.
    durable: usize,
}

impl Model {
    /// Returns an empty database with nothing committed.
    pub fn new() -> Model {
        Model::default()
    }

    /// Returns the committed state.
    pub fn committed(&self) -> &State {
        &self.committed
    }

    /// Returns how many transactions have committed.
    pub fn commits(&self) -> usize {
        self.history.len()
    }

    /// Returns how many commits the engine has reported durable.
    pub fn durable(&self) -> usize {
        self.durable
    }

    /// Begins a transaction at the current committed state.
    ///
    /// @param txn - the transaction's number
    pub fn begin(&mut self, txn: u32) {
        self.open.insert(
            txn,
            Open {
                snapshot: self.committed.clone(),
                writes: BTreeMap::new(),
                savepoints: Vec::new(),
            },
        );
    }

    /// Writes one key inside a transaction.
    ///
    /// @param txn - the transaction's number
    /// @param tree - which tree
    /// @param key - the key
    /// @param value - the value, or `None` to delete it
    pub fn write(&mut self, txn: u32, tree: u32, key: u64, value: Option<Vec<u8>>) {
        if let Some(open) = self.open.get_mut(&txn) {
            open.writes.insert((tree, key), value);
        }
    }

    /// Returns what a transaction sees at one key.
    ///
    /// Its own writes first, because a transaction reads what it has written;
    /// then its snapshot, because it reads the database as it was when it
    /// began and not as it is now.
    ///
    /// @param txn - the transaction's number, or `None` to read the committed state
    /// @param tree - which tree
    /// @param key - the key
    pub fn read(&self, txn: Option<u32>, tree: u32, key: u64) -> Option<&[u8]> {
        if let Some(open) = txn.and_then(|number| self.open.get(&number)) {
            if let Some(held) = open.writes.get(&(tree, key)) {
                return held.as_deref();
            }
            return open.snapshot.get(&(tree, key)).map(Vec::as_slice);
        }
        self.committed.get(&(tree, key)).map(Vec::as_slice)
    }

    /// Takes a savepoint.
    ///
    /// @param txn - the transaction's number
    /// @param name - the savepoint's name
    pub fn savepoint(&mut self, txn: u32, name: &str) {
        if let Some(open) = self.open.get_mut(&txn) {
            let held = open.writes.clone();
            open.savepoints.push((name.to_string(), held));
        }
    }

    /// Undoes everything written since a savepoint, keeping the savepoint.
    ///
    /// @param txn - the transaction's number
    /// @param name - the savepoint's name
    pub fn rollback_to(&mut self, txn: u32, name: &str) {
        let Some(open) = self.open.get_mut(&txn) else {
            return;
        };
        let Some(at) = open.savepoints.iter().rposition(|(held, _)| held == name) else {
            return;
        };
        if let Some((_, writes)) = open.savepoints.get(at) {
            open.writes = writes.clone();
        }
        // Rolling back to a savepoint releases every savepoint *inside* it and
        // keeps the one named, which is SQL's rule and the reason this
        // truncates to `at + 1` rather than to `at`.
        open.savepoints.truncate(at.saturating_add(1));
    }

    /// Releases a savepoint without undoing anything.
    ///
    /// @param txn - the transaction's number
    /// @param name - the savepoint's name
    pub fn release(&mut self, txn: u32, name: &str) {
        if let Some(open) = self.open.get_mut(&txn) {
            if let Some(at) = open.savepoints.iter().rposition(|(held, _)| held == name) {
                open.savepoints.truncate(at);
            }
        }
    }

    /// Commits a transaction, making its writes the committed state.
    ///
    /// @param txn - the transaction's number
    pub fn commit(&mut self, txn: u32) {
        let Some(open) = self.open.remove(&txn) else {
            return;
        };
        let mut writes = Vec::with_capacity(open.writes.len());
        for (at, value) in open.writes {
            match &value {
                Some(held) => {
                    self.committed.insert(at, held.clone());
                }
                None => {
                    self.committed.remove(&at);
                }
            }
            writes.push((at, value));
        }
        self.history.push(Commit { txn, writes });
    }

    /// Abandons a transaction, discarding everything it wrote.
    ///
    /// @param txn - the transaction's number
    pub fn rollback(&mut self, txn: u32) {
        self.open.remove(&txn);
    }

    /// Records that every commit so far is on the media.
    ///
    /// The engine says this - after a `FULL` commit, or after a checkpoint -
    /// and the model believes it. It is what narrows the set of states a crash
    /// may leave: everything below this point *must* be there afterwards.
    pub fn note_durable(&mut self) {
        self.durable = self.history.len();
    }

    /// Returns every state a crash here may leave behind.
    ///
    /// One per prefix of the commit history from the durable point to the end:
    /// a crash may lose the commits that were not yet on the media, and it may
    /// lose none of them, and it may not lose one from the middle. **Open
    /// transactions contribute nothing to any of them** - that is atomicity,
    /// and it is why an uncommitted write never appears in this list.
    pub fn states_after_crash(&self) -> Vec<State> {
        (self.durable..=self.history.len())
            .map(|through| self.state_through(through))
            .collect()
    }

    /// Returns the state after the first `through` commits.
    ///
    /// @param through - how many commits to apply
    pub fn state_through(&self, through: usize) -> State {
        let mut state = State::new();
        for commit in self.history.iter().take(through) {
            for (at, value) in &commit.writes {
                match value {
                    Some(held) => {
                        state.insert(*at, held.clone());
                    }
                    None => {
                        state.remove(at);
                    }
                }
            }
        }
        state
    }

    /// Restarts the model after a crash, at whichever state recovery reached.
    ///
    /// The engine's recovered state decides which of the allowed prefixes
    /// actually happened, and the model adopts it - so the *next* crash is
    /// judged against what really survived rather than against what might have.
    /// Returns the violation when the recovered state is not an allowed one.
    ///
    /// @param recovered - the state the engine came back with
    pub fn recover(&mut self, recovered: &State) -> Option<Violation> {
        let allowed = self.states_after_crash();
        let Some(through) = allowed.iter().position(|state| state == recovered) else {
            // The *closest* allowed state, and the keys that differ from it.
            // "eight keys that match no prefix" is a true sentence nobody can
            // act on; "key (1, 7) is 'b' and every allowed state has 'a'" names
            // the row to go and look at.
            let closest = allowed
                .iter()
                .enumerate()
                .min_by_key(|(_, state)| difference(state, recovered).len())
                .map(|(at, state)| (self.durable.saturating_add(at), state));
            let detail = match closest {
                Some((through, state)) => {
                    let mut lines = difference(state, recovered);
                    lines.truncate(6);
                    format!(
                        "nearest is commits 0..={through} applied, which differs by: {}",
                        lines.join("; ")
                    )
                }
                None => "no allowed state at all".to_string(),
            };
            return Some(Violation {
                rule: "a crash leaves a prefix of the commits, whole".to_string(),
                expected: format!(
                    "one of {} states: commits {}..={} applied",
                    allowed.len(),
                    self.durable,
                    self.history.len()
                ),
                found: detail,
            });
        };
        let through = self.durable.saturating_add(through);
        self.history.truncate(through);
        self.durable = through;
        self.committed = self.state_through(through);
        self.open.clear();
        None
    }
}

/// Returns how two states differ, one line per key.
///
/// @param expected - the state the model allows
/// @param found - the state the engine came back with
fn difference(expected: &State, found: &State) -> Vec<String> {
    let mut lines = Vec::new();
    for (at, value) in expected {
        match found.get(at) {
            None => lines.push(format!("{at:?} is missing")),
            Some(held) if held != value => lines.push(format!(
                "{at:?} is {:?}, expected {:?}",
                String::from_utf8_lossy(held),
                String::from_utf8_lossy(value)
            )),
            Some(_) => {}
        }
    }
    for at in found.keys() {
        if !expected.contains_key(at) {
            lines.push(format!("{at:?} is there and should not be"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transaction reads what it has written and nobody else does.
    #[test]
    fn a_transaction_reads_its_own_writes_and_no_one_elses() {
        let mut model = Model::new();
        model.begin(1);
        model.write(1, 7, 42, Some(b"one".to_vec()));
        assert_eq!(model.read(Some(1), 7, 42), Some(b"one".as_slice()));
        assert_eq!(model.read(None, 7, 42), None);

        model.begin(2);
        assert_eq!(model.read(Some(2), 7, 42), None);
        model.commit(1);
        // Transaction 2 began before the commit, so it still must not see it.
        assert_eq!(model.read(Some(2), 7, 42), None);
        assert_eq!(model.read(None, 7, 42), Some(b"one".as_slice()));

        model.begin(3);
        assert_eq!(model.read(Some(3), 7, 42), Some(b"one".as_slice()));
    }

    /// A rollback leaves nothing behind, including in the history.
    #[test]
    fn a_rollback_leaves_nothing_behind() {
        let mut model = Model::new();
        model.begin(1);
        model.write(1, 7, 1, Some(b"x".to_vec()));
        model.rollback(1);
        assert_eq!(model.read(None, 7, 1), None);
        assert_eq!(model.commits(), 0);
        assert_eq!(model.states_after_crash(), vec![State::new()]);
    }

    /// A savepoint undoes what came after it and keeps what came before.
    #[test]
    fn a_savepoint_undoes_only_what_came_after_it() {
        let mut model = Model::new();
        model.begin(1);
        model.write(1, 7, 1, Some(b"before".to_vec()));
        model.savepoint(1, "s");
        model.write(1, 7, 2, Some(b"after".to_vec()));
        model.write(1, 7, 1, Some(b"changed".to_vec()));
        model.rollback_to(1, "s");
        assert_eq!(model.read(Some(1), 7, 1), Some(b"before".as_slice()));
        assert_eq!(model.read(Some(1), 7, 2), None);
        // The savepoint survives its own rollback and can be used again.
        model.write(1, 7, 3, Some(b"again".to_vec()));
        model.rollback_to(1, "s");
        assert_eq!(model.read(Some(1), 7, 3), None);
    }

    /// Rolling back to an outer savepoint releases the inner ones.
    #[test]
    fn rolling_back_to_an_outer_savepoint_releases_the_inner_ones() {
        let mut model = Model::new();
        model.begin(1);
        model.savepoint(1, "outer");
        model.write(1, 7, 1, Some(b"a".to_vec()));
        model.savepoint(1, "inner");
        model.write(1, 7, 2, Some(b"b".to_vec()));
        model.rollback_to(1, "outer");
        assert_eq!(model.read(Some(1), 7, 1), None);
        // `inner` is gone, so rolling back to it changes nothing rather than
        // resurrecting the write that was made after it.
        model.write(1, 7, 3, Some(b"c".to_vec()));
        model.rollback_to(1, "inner");
        assert_eq!(model.read(Some(1), 7, 3), Some(b"c".as_slice()));
    }

    /// A release keeps the writes and forgets the mark.
    #[test]
    fn a_release_keeps_the_writes() {
        let mut model = Model::new();
        model.begin(1);
        model.savepoint(1, "s");
        model.write(1, 7, 1, Some(b"a".to_vec()));
        model.release(1, "s");
        assert_eq!(model.read(Some(1), 7, 1), Some(b"a".as_slice()));
        model.rollback_to(1, "s");
        assert_eq!(model.read(Some(1), 7, 1), Some(b"a".as_slice()));
    }

    /// A crash may lose the commits that were not durable, and no others.
    #[test]
    fn a_crash_may_lose_only_what_was_not_durable() {
        let mut model = Model::new();
        model.begin(1);
        model.write(1, 7, 1, Some(b"a".to_vec()));
        model.commit(1);
        model.note_durable();
        model.begin(2);
        model.write(2, 7, 2, Some(b"b".to_vec()));
        model.commit(2);

        let allowed = model.states_after_crash();
        assert_eq!(allowed.len(), 2, "one commit is durable and one is not");
        assert!(allowed.iter().all(|state| state.contains_key(&(7, 1))));
        assert_eq!(
            allowed
                .iter()
                .filter(|state| state.contains_key(&(7, 2)))
                .count(),
            1
        );
    }

    /// A state that matches no prefix is a violation, and it says so.
    #[test]
    fn a_state_matching_no_prefix_is_a_violation() {
        let mut model = Model::new();
        model.begin(1);
        model.write(1, 7, 1, Some(b"a".to_vec()));
        model.write(1, 7, 2, Some(b"b".to_vec()));
        model.commit(1);
        // Half the commit: exactly what a torn write would leave.
        let mut torn = State::new();
        torn.insert((7, 1), b"a".to_vec());
        let violation = model.recover(&torn).expect("a half commit is a violation");
        assert!(violation.rule.contains("prefix"));
    }

    /// Recovery adopts the prefix that actually survived.
    #[test]
    fn recovery_adopts_the_prefix_that_survived() {
        let mut model = Model::new();
        model.begin(1);
        model.write(1, 7, 1, Some(b"a".to_vec()));
        model.commit(1);
        model.begin(2);
        model.write(2, 7, 2, Some(b"b".to_vec()));
        model.commit(2);
        // The second commit was lost.
        let survived = model.state_through(1);
        assert!(model.recover(&survived).is_none());
        assert_eq!(model.commits(), 1);
        assert_eq!(model.durable(), 1);
        // And a later crash cannot bring it back.
        assert_eq!(model.states_after_crash(), vec![survived]);
    }

    /// An uncommitted write is in no state a crash can leave.
    #[test]
    fn an_uncommitted_write_survives_no_crash() {
        let mut model = Model::new();
        model.begin(1);
        model.write(1, 7, 1, Some(b"a".to_vec()));
        for state in model.states_after_crash() {
            assert!(state.is_empty(), "an open transaction reached the media");
        }
    }
}
