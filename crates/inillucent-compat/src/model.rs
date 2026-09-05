//! `ModelBTree`: an independent answer to "what should be in the tree".
//!
//! Invariant: nothing in this file knows how a B-tree works. It is a sorted map
//! and a list of operations, and it imports no production algorithm - not the
//! page codec, not the comparator, not the allocator. That is the entire point:
//! a model that shared code with the thing it checks would agree with it about
//! exactly the cases the shared code got wrong.
//!
//! What it models is the *logical* contents of a tree: which keys are present,
//! what payload each one has, and what order they come out in. It deliberately
//! does not model pages, splits, or the freelist. Those are checked by the raw
//! integrity checker, which is a different independent reader, and asking the
//! model to predict a page layout would tie it to one implementation of
//! balancing - which is the thing most likely to be rewritten.
//!
//! Savepoints are modelled by keeping a stack of whole snapshots. That is
//! hopelessly inefficient and completely obvious, which is the right trade for
//! a model: an undo log in the model would be the same design as the undo log
//! being tested, and a shared design is a shared bug.

use std::collections::BTreeMap;

use inillucent_base::rng::Rng;

/// Which kind of tree is being modelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelKind {
    /// Keyed by rowid, with the payload stored against it.
    Table,
    /// Keyed by the record itself.
    Index,
}

/// A key in the model.
///
/// The two forms sort separately and are never mixed, because a tree is one
/// kind or the other for its whole life.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ModelKey {
    /// A rowid, which sorts as a signed integer.
    Rowid(i64),
    /// A record, which sorts as bytes.
    ///
    /// Byte order is the right comparator here only because the harness builds
    /// keys that sort the same way under BINARY collation and under memcmp -
    /// integers encoded big-endian into a blob. A model that tried to
    /// reimplement record comparison would be reimplementing the code it
    /// checks.
    Bytes(Vec<u8>),
}

/// The logical contents of one B-tree.
#[derive(Clone, Debug)]
pub struct ModelBTree {
    kind: ModelKind,
    entries: BTreeMap<ModelKey, Vec<u8>>,
    savepoints: Vec<BTreeMap<ModelKey, Vec<u8>>>,
    committed: BTreeMap<ModelKey, Vec<u8>>,
}

impl ModelBTree {
    /// Builds an empty model of a tree of the given kind.
    pub fn new(kind: ModelKind) -> ModelBTree {
        ModelBTree {
            kind,
            entries: BTreeMap::new(),
            savepoints: Vec::new(),
            committed: BTreeMap::new(),
        }
    }

    /// Returns which kind of tree this models.
    pub fn kind(&self) -> ModelKind {
        self.kind
    }

    /// Inserts or replaces an entry.
    pub fn insert(&mut self, key: ModelKey, payload: Vec<u8>) {
        self.entries.insert(key, payload);
    }

    /// Deletes an entry, reporting whether it was there.
    pub fn delete(&mut self, key: &ModelKey) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Returns every entry in key order.
    pub fn entries(&self) -> Vec<(ModelKey, Vec<u8>)> {
        self.entries
            .iter()
            .map(|(key, payload)| (key.clone(), payload.clone()))
            .collect()
    }

    /// Returns the entries between two rowids, inclusive.
    pub fn rowid_range(&self, low: i64, high: i64) -> Vec<(ModelKey, Vec<u8>)> {
        self.entries
            .iter()
            .filter(|(key, _)| match key {
                ModelKey::Rowid(rowid) => *rowid >= low && *rowid <= high,
                ModelKey::Bytes(_) => false,
            })
            .map(|(key, payload)| (key.clone(), payload.clone()))
            .collect()
    }

    /// Returns how many entries the model holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Reports whether the model is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Reports whether a key is present.
    pub fn contains(&self, key: &ModelKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Opens a savepoint by taking a snapshot.
    pub fn begin_savepoint(&mut self) {
        self.savepoints.push(self.entries.clone());
    }

    /// Closes the innermost savepoint, keeping its changes.
    pub fn release_savepoint(&mut self) {
        self.savepoints.pop();
    }

    /// Undoes everything the innermost savepoint did.
    pub fn rollback_savepoint(&mut self) {
        if let Some(snapshot) = self.savepoints.pop() {
            self.entries = snapshot;
        }
    }

    /// Returns how many savepoints are open.
    pub fn savepoint_depth(&self) -> usize {
        self.savepoints.len()
    }

    /// Commits, which makes the current contents the ones a reopen would see.
    pub fn commit(&mut self) {
        self.savepoints.clear();
        self.committed = self.entries.clone();
    }

    /// Rolls the whole transaction back to the last commit.
    pub fn rollback(&mut self) {
        self.savepoints.clear();
        self.entries = self.committed.clone();
    }
}

/// One operation in a generated sequence.
///
/// The set is chosen so that every structural change a B-tree can make is
/// reachable: payloads that straddle the local-payload threshold force overflow
/// chains in and out of existence, deletes force merges and root collapses,
/// commits and reopens force the file to be re-read rather than trusted, and
/// vacuum forces pages to move.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Op {
    /// Insert or replace an entry with a payload of the given length.
    Insert {
        /// The key to write.
        key: i64,
        /// How many bytes of payload the entry carries.
        payload_len: usize,
    },
    /// Delete an entry.
    Delete {
        /// The key to remove.
        key: i64,
    },
    /// Scan the whole tree forwards and compare it with the model.
    Scan,
    /// Scan a range of rowids.
    Range {
        /// The first key of the range, inclusive.
        low: i64,
        /// The last key of the range, inclusive.
        high: i64,
    },
    /// Position a cursor, make a change, and restore it.
    CursorRestore {
        /// The key to leave the cursor on before the tree changes.
        key: i64,
    },
    /// Open a savepoint.
    Savepoint,
    /// Undo the innermost savepoint.
    RollbackSavepoint,
    /// Close the innermost savepoint, keeping its changes.
    ReleaseSavepoint,
    /// Commit the transaction and open a new one.
    Commit,
    /// Roll the whole transaction back.
    Rollback,
    /// Close the database and open it again.
    Reopen,
    /// Run some incremental vacuum steps.
    Vacuum {
        /// How many pages to try to reclaim.
        steps: u32,
    },
}

impl Op {
    /// Returns a short name, for a corpus file and a failure message.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Insert { .. } => "insert",
            Op::Delete { .. } => "delete",
            Op::Scan => "scan",
            Op::Range { .. } => "range",
            Op::CursorRestore { .. } => "cursor-restore",
            Op::Savepoint => "savepoint",
            Op::RollbackSavepoint => "rollback-savepoint",
            Op::ReleaseSavepoint => "release-savepoint",
            Op::Commit => "commit",
            Op::Rollback => "rollback",
            Op::Reopen => "reopen",
            Op::Vacuum { .. } => "vacuum",
        }
    }

    /// Renders the operation as one line of JSON.
    pub fn to_json(&self) -> String {
        match self {
            Op::Insert { key, payload_len } => {
                format!("{{\"op\":\"insert\",\"key\":{key},\"payload_len\":{payload_len}}}")
            }
            Op::Delete { key } => format!("{{\"op\":\"delete\",\"key\":{key}}}"),
            Op::Scan => "{\"op\":\"scan\"}".to_string(),
            Op::Range { low, high } => {
                format!("{{\"op\":\"range\",\"low\":{low},\"high\":{high}}}")
            }
            Op::CursorRestore { key } => {
                format!("{{\"op\":\"cursor-restore\",\"key\":{key}}}")
            }
            Op::Savepoint => "{\"op\":\"savepoint\"}".to_string(),
            Op::RollbackSavepoint => "{\"op\":\"rollback-savepoint\"}".to_string(),
            Op::ReleaseSavepoint => "{\"op\":\"release-savepoint\"}".to_string(),
            Op::Commit => "{\"op\":\"commit\"}".to_string(),
            Op::Rollback => "{\"op\":\"rollback\"}".to_string(),
            Op::Reopen => "{\"op\":\"reopen\"}".to_string(),
            Op::Vacuum { steps } => format!("{{\"op\":\"vacuum\",\"steps\":{steps}}}"),
        }
    }

    /// Parses one line written by [`Op::to_json`].
    pub fn parse(line: &str) -> Option<Op> {
        let name = field(line, "op")?;
        Some(match name.as_str() {
            "insert" => Op::Insert {
                key: number(line, "key")?,
                payload_len: usize::try_from(number(line, "payload_len")?).ok()?,
            },
            "delete" => Op::Delete {
                key: number(line, "key")?,
            },
            "scan" => Op::Scan,
            "range" => Op::Range {
                low: number(line, "low")?,
                high: number(line, "high")?,
            },
            "cursor-restore" => Op::CursorRestore {
                key: number(line, "key")?,
            },
            "savepoint" => Op::Savepoint,
            "rollback-savepoint" => Op::RollbackSavepoint,
            "release-savepoint" => Op::ReleaseSavepoint,
            "commit" => Op::Commit,
            "rollback" => Op::Rollback,
            "reopen" => Op::Reopen,
            "vacuum" => Op::Vacuum {
                steps: u32::try_from(number(line, "steps")?).ok()?,
            },
            _ => return None,
        })
    }
}

/// Returns a string field from a one-line JSON object.
fn field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = line.find(&needle)?.checked_add(needle.len())?;
    let rest = line.get(start..)?;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_string)
}

/// Returns a numeric field from a one-line JSON object.
fn number(line: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)?.checked_add(needle.len())?;
    let rest = line.get(start..)?;
    let end = rest
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(rest.len());
    rest.get(..end)?.parse().ok()
}

/// How a generated sequence is shaped.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    /// The page size the database is built at.
    pub page_size: u32,
    /// How many operations the sequence holds.
    pub length: usize,
    /// The largest key the generator will use.
    ///
    /// A small range makes the same key be inserted, replaced and deleted many
    /// times, which is where replace and merge live; a large one makes a deep
    /// tree. Both are generated.
    pub key_range: i64,
    /// Whether the tree is a table or an index.
    pub kind: ModelKind,
    /// Whether the database keeps pointer maps.
    pub vacuum: bool,
}

/// Builds a sequence of operations from a seed.
///
/// The payload lengths are not uniform: they are drawn from the boundaries of
/// the file format's own arithmetic - one byte either side of the local-payload
/// maximum, one either side of a varint width, exactly one overflow page, and a
/// few pages' worth. A uniform draw would spend almost all its time in the
/// middle of the range, where nothing interesting happens.
pub fn generate(seed: u64, shape: Shape) -> Vec<Op> {
    let mut rng = Rng::new(seed);
    let usable = shape.page_size as usize;
    let max_local = usable.saturating_sub(35);
    let per_overflow = usable.saturating_sub(4);
    let interesting: Vec<usize> = vec![
        1,
        2,
        11,
        12,
        13,
        126,
        127,
        128,
        max_local.saturating_sub(1),
        max_local,
        max_local.saturating_add(1),
        max_local.saturating_add(per_overflow.saturating_sub(1)),
        max_local.saturating_add(per_overflow),
        max_local.saturating_add(per_overflow).saturating_add(1),
        max_local.saturating_add(per_overflow.saturating_mul(3)),
    ];

    let mut ops = Vec::with_capacity(shape.length);
    let mut depth = 0usize;
    for _ in 0..shape.length {
        let roll = rng.below(100);
        let key = rng.below(shape.key_range.unsigned_abs()) as i64;
        let op = if roll < 40 {
            let payload_len = interesting
                .get(rng.below(interesting.len() as u64) as usize)
                .copied()
                .unwrap_or(16);
            Op::Insert { key, payload_len }
        } else if roll < 62 {
            Op::Delete { key }
        } else if roll < 70 {
            Op::Scan
        } else if roll < 76 {
            let low = key;
            let high = low.saturating_add(rng.below(64) as i64);
            Op::Range { low, high }
        } else if roll < 82 {
            Op::CursorRestore { key }
        } else if roll < 86 {
            depth = depth.saturating_add(1);
            Op::Savepoint
        } else if roll < 89 && depth > 0 {
            depth = depth.saturating_sub(1);
            Op::RollbackSavepoint
        } else if roll < 92 && depth > 0 {
            depth = depth.saturating_sub(1);
            Op::ReleaseSavepoint
        } else if roll < 96 {
            depth = 0;
            Op::Commit
        } else if roll < 97 {
            depth = 0;
            Op::Rollback
        } else if roll < 99 {
            depth = 0;
            Op::Reopen
        } else if shape.vacuum {
            Op::Vacuum {
                steps: rng.below(8).saturating_add(1) as u32,
            }
        } else {
            Op::Scan
        };
        ops.push(op);
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model is a sorted map and behaves like one, including replacing.
    #[test]
    fn the_model_keeps_its_entries_sorted_and_unique() {
        let mut model = ModelBTree::new(ModelKind::Table);
        for key in [5i64, 1, 9, 1, 3] {
            model.insert(ModelKey::Rowid(key), vec![key as u8]);
        }
        let keys: Vec<ModelKey> = model.entries().into_iter().map(|(key, _)| key).collect();
        assert_eq!(
            keys,
            vec![
                ModelKey::Rowid(1),
                ModelKey::Rowid(3),
                ModelKey::Rowid(5),
                ModelKey::Rowid(9)
            ]
        );
        assert!(model.delete(&ModelKey::Rowid(3)));
        assert!(!model.delete(&ModelKey::Rowid(3)));
    }

    /// A savepoint rolls back to the snapshot it took, and a release keeps the
    /// changes made inside it.
    #[test]
    fn savepoints_snapshot_and_restore() {
        let mut model = ModelBTree::new(ModelKind::Table);
        model.insert(ModelKey::Rowid(1), vec![1]);
        model.begin_savepoint();
        model.insert(ModelKey::Rowid(2), vec![2]);
        model.rollback_savepoint();
        assert_eq!(model.len(), 1);

        model.begin_savepoint();
        model.insert(ModelKey::Rowid(3), vec![3]);
        model.release_savepoint();
        assert_eq!(model.len(), 2);
        assert_eq!(model.savepoint_depth(), 0);
    }

    /// A rollback returns to the last commit, and a reopen sees the same.
    #[test]
    fn a_rollback_returns_to_the_last_commit() {
        let mut model = ModelBTree::new(ModelKind::Table);
        model.insert(ModelKey::Rowid(1), vec![1]);
        model.commit();
        model.insert(ModelKey::Rowid(2), vec![2]);
        model.rollback();
        assert_eq!(model.len(), 1);
    }

    /// Every operation survives a round trip through its JSON form, which is
    /// what makes a retained corpus replayable.
    #[test]
    fn every_operation_round_trips_through_json() {
        let ops = vec![
            Op::Insert {
                key: -7,
                payload_len: 4096,
            },
            Op::Delete { key: 12 },
            Op::Scan,
            Op::Range { low: -3, high: 9 },
            Op::CursorRestore { key: 5 },
            Op::Savepoint,
            Op::RollbackSavepoint,
            Op::ReleaseSavepoint,
            Op::Commit,
            Op::Rollback,
            Op::Reopen,
            Op::Vacuum { steps: 3 },
        ];
        for op in ops {
            let line = op.to_json();
            assert_eq!(Op::parse(&line), Some(op.clone()), "{line}");
        }
    }

    /// A seed produces the same sequence every time, which is what makes a
    /// failure replayable from its seed alone.
    #[test]
    fn generation_is_deterministic() {
        let shape = Shape {
            page_size: 1024,
            length: 200,
            key_range: 500,
            kind: ModelKind::Table,
            vacuum: true,
        };
        assert_eq!(generate(7, shape), generate(7, shape));
        assert_ne!(generate(7, shape), generate(8, shape));
    }
}
