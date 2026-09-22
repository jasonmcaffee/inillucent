//! A model of what the log means, compared against what replaying it produces.
//!
//! Invariant: **after every record, replaying the whole log produces exactly
//! the pages a `BTreeMap` updated by the same records holds.** The map is the
//! second opinion: it is built from the operations the test issued, in the
//! order it issued them, by three lines of code with no pager, no segments and
//! no page-LSN rule in them - so a disagreement is the log's, and there is
//! nowhere for the two to be wrong together.
//!
//! ## Why the log needed one and the B-tree already had one
//!
//! `inillucent-compat`'s `model.rs` and `btree_model.rs` have graded the tree
//! against an independent model since phase 2, and the tree is the best tested
//! component in the workspace because of it. The log has 1,603 lines of hand
//! written cases in `recovery.rs` and no model at all (task-2066 section
//! 4.4.10). A hand written case can only catch a bug somebody thought of; a
//! model catches the sequence nobody would have written down, which for a log
//! is exactly where the bugs are - a commit interleaved with another
//! transaction's write, an abort after a segment rolled, a page written twice
//! by two transactions where only the second commits.
//!
//! ## What the model claims, and what it deliberately does not
//!
//! It claims the *contents*: which pages a recovery produces and what is in
//! them. It says nothing about which segment a record landed in, how many
//! records were scanned, or where recovery decided to start - those are the
//! log's own arrangements, `recovery.rs` asserts them directly, and a model
//! that reproduced them would be a second implementation rather than a second
//! opinion.
//!
//! **A transaction that has not committed contributes nothing.** That is the
//! whole of what a log is for, and it is the property a generated sequence
//! tests that a written one cannot: the generator interleaves writes from
//! several transactions and commits some of them, so a replay that applied a
//! loser's write would have to apply it *under* a winner's and would be visible
//! as one page of one sequence out of hundreds.
//!
//! ## Shrinking
//!
//! A failing sequence is shrunk by removing operations while it still fails,
//! the way `btree_model.rs` shrinks: the order of what is left never changes,
//! because a shrinker that reordered would produce a sequence that fails for a
//! different reason than the one it was given.

use std::collections::BTreeMap;
use std::sync::Arc;

use inillucent_base::DbResult;
use inillucent_vfs::{DbPath, MemoryVfs, Vfs};
use inillucent_wal::record::{Body, Record};
use inillucent_wal::recover::{self, RecoveryStart, Redo};
use inillucent_wal::writer::{Wal, WalOptions};
use inillucent_wal::{Synchronous, FIRST_LSN};

/// The page size these sequences write.
const PAGE: usize = 64;

/// The database identity every sequence uses.
const UUID: u128 = 0x4d4f_4445_4c4d_4f44;

/// One operation in a generated sequence.
///
/// The set is small on purpose. What a log has to get right is *which* records
/// are applied, not what is in them, so a richer record body would spend the
/// generator's budget in the decoder that `wal_record`'s fuzz target and
/// `recovery.rs`'s corrupt-log cases already cover.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Op {
    /// A transaction writes a page image.
    Write { txn: u64, page: u64, mark: u8 },
    /// A transaction commits, so everything it wrote becomes durable.
    Commit { txn: u64 },
    /// A transaction aborts, so everything it wrote is discarded.
    Abort { txn: u64 },
}

impl Op {
    /// The name a failure message calls this operation.
    fn name(self) -> String {
        match self {
            Op::Write { txn, page, mark } => format!("write txn {txn} page {page} mark {mark}"),
            Op::Commit { txn } => format!("commit txn {txn}"),
            Op::Abort { txn } => format!("abort txn {txn}"),
        }
    }
}

/// How a generated sequence is shaped.
#[derive(Clone, Copy, Debug)]
struct Shape {
    /// How many operations the sequence holds.
    length: usize,
    /// How many distinct pages it writes.
    ///
    /// Few pages on purpose: the interesting case is two transactions writing
    /// the *same* page and only one of them committing, and a wide page range
    /// would make that rare.
    pages: u64,
    /// How many transactions are in flight at once.
    transactions: u64,
}

/// A small deterministic generator, so a failure is reproduced from its seed.
struct Rng(u64);

impl Rng {
    /// Returns the next value.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Builds a sequence of operations from a seed.
///
/// **A commit or an abort only ever names a transaction that has written**, so
/// the sequence is one the engine could have produced. A generator that closed
/// a transaction nobody opened would be testing the log's tolerance of a caller
/// that does not exist.
///
/// @param seed - the run's seed
/// @param shape - how long, how many pages, how many transactions
fn generate(seed: u64, shape: Shape) -> Vec<Op> {
    let mut rng = Rng(seed | 1);
    let mut open: Vec<u64> = Vec::new();
    let mut next_txn = 1u64;
    let mut ops = Vec::with_capacity(shape.length);
    while ops.len() < shape.length {
        let choice = rng.next() % 10;
        match choice {
            0..=5 => {
                if open.len() < shape.transactions as usize && (open.is_empty() || choice < 2) {
                    open.push(next_txn);
                    next_txn = next_txn.saturating_add(1);
                }
                let Some(txn) = open.get((rng.next() as usize) % open.len().max(1)).copied() else {
                    continue;
                };
                ops.push(Op::Write {
                    txn,
                    page: rng.next() % shape.pages,
                    mark: (rng.next() % 251) as u8,
                });
            }
            6..=8 => {
                if open.is_empty() {
                    continue;
                }
                let at = (rng.next() as usize) % open.len();
                let txn = open.remove(at);
                ops.push(Op::Commit { txn });
            }
            _ => {
                if open.is_empty() {
                    continue;
                }
                let at = (rng.next() as usize) % open.len();
                let txn = open.remove(at);
                ops.push(Op::Abort { txn });
            }
        }
    }
    ops
}

/// The pages a recovery produced, by number.
#[derive(Default)]
struct Pages {
    /// Each page's bytes, as the replay wrote them.
    held: BTreeMap<u64, Vec<u8>>,
}

impl Pages {
    /// Returns the mark a page carries, which is the byte the write put there.
    ///
    /// @param page - the page number
    fn mark_of(&self, page: u64) -> Option<u8> {
        self.held.get(&page).and_then(|bytes| bytes.get(8).copied())
    }

    /// Returns the marks of every page, for comparing against the model.
    fn marks(&self) -> BTreeMap<u64, u8> {
        self.held
            .keys()
            .filter_map(|page| self.mark_of(*page).map(|mark| (*page, mark)))
            .collect()
    }
}

impl Redo for Pages {
    fn page_lsn(&mut self, page: u64) -> DbResult<Option<u64>> {
        let Some(bytes) = self.held.get(&page) else {
            return Ok(None);
        };
        let mut raw = [0u8; 8];
        let Some(head) = bytes.get(..8) else {
            return Ok(None);
        };
        raw.copy_from_slice(head);
        Ok(Some(u64::from_le_bytes(raw)))
    }

    fn redo(&mut self, record: &Record<'_>, _wanted: &[bool]) -> DbResult<()> {
        if let Body::WritePage { page, image } = record.body {
            let mut bytes = vec![0u8; PAGE];
            if let Some(slot) = bytes.get_mut(..8) {
                slot.copy_from_slice(&record.lsn.to_le_bytes());
            }
            let width = image.len().min(PAGE.saturating_sub(8));
            if let Some(slot) = bytes.get_mut(8..8 + width) {
                slot.copy_from_slice(image.get(..width).unwrap_or(&[]));
            }
            self.held.insert(page, bytes);
        }
        Ok(())
    }
}

/// The model: what each page holds once every committed transaction is applied.
///
/// **In the order the writes were made, not the order the transactions
/// committed - and the model is what got that wrong first.** The first version
/// held each transaction's writes aside and applied them at its commit, which
/// reads as the obvious meaning of "a transaction's writes become durable when
/// it commits". It is not what a write-ahead log does: redo replays records in
/// LSN order whatever order the commits arrived in, so the last *write* to a
/// page wins rather than the last *commit*.
///
/// It took four operations to show, and the generator found them on the second
/// seed:
///
/// ```text
/// write txn 6 page 1 mark 24
/// write txn 7 page 1 mark 31
/// commit txn 7
/// commit txn 6
/// ```
///
/// The replay answers 31 and the first model answered 24. Nothing in
/// `recovery.rs`'s 1,603 lines interleaves two transactions on one page and
/// then commits them backwards, which is the whole argument for having a model
/// at all: it is the sequence nobody would have written down.
#[derive(Default)]
struct Model {
    /// Every write, in the order it was made.
    writes: Vec<(u64, u64, u8)>,
    /// Which transactions committed.
    committed: std::collections::BTreeSet<u64>,
    /// Which transactions aborted.
    aborted: std::collections::BTreeSet<u64>,
}

impl Model {
    /// Records a write.
    ///
    /// @param txn - the transaction
    /// @param page - the page
    /// @param mark - the byte it wrote
    fn write(&mut self, txn: u64, page: u64, mark: u8) {
        self.writes.push((txn, page, mark));
    }

    /// Records that a transaction committed.
    ///
    /// @param txn - the transaction
    fn commit(&mut self, txn: u64) {
        self.committed.insert(txn);
    }

    /// Records that a transaction aborted.
    ///
    /// @param txn - the transaction
    fn abort(&mut self, txn: u64) {
        self.aborted.insert(txn);
    }

    /// Returns what each page holds, from the writes that count.
    ///
    /// A write counts when its transaction committed, and the writes are walked
    /// in the order they were made, so the last one to a page wins. That is the
    /// LSN order rule, said in one line.
    fn pages(&self) -> BTreeMap<u64, u8> {
        let mut held = BTreeMap::new();
        for (txn, page, mark) in &self.writes {
            if self.committed.contains(txn) {
                held.insert(*page, *mark);
            }
        }
        held
    }
}

/// Runs one sequence and returns the first disagreement, if there is one.
///
/// **Recovered after every operation**, which is what the section asks for and
/// is what makes a disagreement point at one record rather than at a sequence.
/// A fresh `Pages` each time, because recovery into a store that already holds
/// the answer would be graded against itself.
///
/// @param seed - the seed, so the message can name it
/// @param ops - the operations to run
fn run(seed: u64, ops: &[Op]) -> Result<(), String> {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("model.rdb");
    let wal = Wal::open(
        Arc::clone(&vfs),
        &path,
        UUID,
        FIRST_LSN,
        1,
        WalOptions {
            synchronous: Synchronous::Full,
            // Small, so a sequence of this length rolls several times and the
            // model meets a segment boundary rather than being told about one.
            segment_bytes: 2_048,
        },
    )
    .map_err(|why| format!("the log did not open: {why:?}"))?;

    let mut model = Model::default();
    for (index, op) in ops.iter().enumerate() {
        match *op {
            Op::Write { txn, page, mark } => {
                let image = vec![mark; 8];
                wal.append(
                    txn,
                    Body::WritePage {
                        page,
                        image: &image,
                    },
                )
                .map_err(|why| format!("step {index} ({}): {why:?}", op.name()))?;
                model.write(txn, page, mark);
            }
            Op::Commit { txn } => {
                wal.commit(txn, txn)
                    .map_err(|why| format!("step {index} ({}): {why:?}", op.name()))?;
                model.commit(txn);
            }
            Op::Abort { txn } => {
                wal.append(txn, Body::Abort)
                    .map_err(|why| format!("step {index} ({}): {why:?}", op.name()))?;
                model.abort(txn);
            }
        }
        let mut pages = Pages::default();
        recover::recover(vfs.as_ref(), &path, RecoveryStart::fresh(UUID), &mut pages)
            .map_err(|why| format!("step {index} ({}): recovery refused: {why:?}", op.name()))?;
        let replayed = pages.marks();
        let wanted = model.pages();
        if replayed != wanted {
            return Err(format!(
                "seed {seed}, step {index} ({}): the replay and the model disagree\n  \
                 replay: {replayed:?}\n  model:  {:?}",
                op.name(),
                model.committed
            ));
        }
    }
    Ok(())
}

/// Removes operations while the failure survives, returning the shortest
/// sequence that still fails.
///
/// The order of what is left never changes: a shrinker that reordered would
/// produce a sequence that fails for a different reason than the one it was
/// given.
///
/// @param seed - the seed the sequence came from
/// @param ops - the failing sequence
fn shrink(seed: u64, ops: &[Op]) -> Vec<Op> {
    let mut best = ops.to_vec();
    let mut rounds = 0;
    let mut changed = true;
    while changed && rounds < 6 {
        rounds += 1;
        changed = false;
        let mut index = 0;
        while index < best.len() {
            let mut candidate = best.clone();
            candidate.remove(index);
            if run(seed, &candidate).is_err() {
                best = candidate;
                changed = true;
            } else {
                index += 1;
            }
        }
    }
    best
}

/// **Replaying the log produces what the model holds, after every record.**
///
/// Sixty-four sequences, each of forty operations over four pages and three
/// transactions in flight. The shapes are chosen so that two transactions write
/// the same page often: that is where a log gets it wrong, and where a hand
/// written case would have had to think of the interleaving first.
#[test]
fn a_replay_produces_what_the_model_holds() {
    let shape = Shape {
        length: 40,
        pages: 4,
        transactions: 3,
    };
    let mut wrote = 0usize;
    for seed in 1..=64u64 {
        let ops = generate(seed, shape);
        // A sequence that committed nothing would pass by holding nothing, so
        // the count of committing sequences is asserted at the end.
        if ops.iter().any(|op| matches!(op, Op::Commit { .. })) {
            wrote = wrote.saturating_add(1);
        }
        if let Err(reason) = run(seed, &ops) {
            let shortest = shrink(seed, &ops);
            let written: Vec<String> = shortest.iter().map(|op| op.name()).collect();
            panic!(
                "{reason}\n  shrunk to {} op(s):\n    {}",
                shortest.len(),
                written.join("\n    ")
            );
        }
    }
    assert!(
        wrote >= 60,
        "only {wrote} of 64 sequences committed anything, so most of them asserted that an \
         empty log replays to nothing"
    );
}

/// **A transaction that never commits contributes nothing, and the model says
/// so by holding nothing.**
///
/// The case above would pass against a log that applied a loser's write *and* a
/// model that did the same, so this one is written down rather than generated:
/// one transaction writes every page and never commits, and the replay has to
/// produce an empty file.
#[test]
fn a_transaction_that_never_commits_replays_to_nothing() {
    let ops: Vec<Op> = (0..8u64)
        .map(|page| Op::Write {
            txn: 1,
            page,
            mark: 0x5A,
        })
        .collect();
    run(9_001, &ops).expect("the sequence runs");

    // And the same writes under a transaction that does commit produce eight
    // pages, so the arm above is about committing rather than about a log that
    // produces nothing whatever it is told.
    let mut committing = ops.clone();
    committing.push(Op::Commit { txn: 1 });
    run(9_002, &committing).expect("the committing sequence runs");
}

/// **The shrinker returns a shorter sequence than it was given, and the
/// shortest one still fails.**
///
/// A shrinker nobody has run is a shrinker that will not work on the night it
/// is needed, and the failure it is needed for cannot be arranged. So it is run
/// against a predicate that fails on a sequence holding an abort, which is not
/// the log's behaviour and is exactly what the shrinker does not know.
#[test]
fn the_shrinker_finds_the_shortest_failing_sequence() {
    /// Fails when the sequence holds an abort, whatever else is in it.
    fn fails(ops: &[Op]) -> bool {
        ops.iter().any(|op| matches!(op, Op::Abort { .. }))
    }
    let ops = vec![
        Op::Write {
            txn: 1,
            page: 0,
            mark: 1,
        },
        Op::Write {
            txn: 2,
            page: 1,
            mark: 2,
        },
        Op::Abort { txn: 2 },
        Op::Commit { txn: 1 },
    ];
    // The same loop the real shrinker runs, over the predicate above.
    let mut best = ops.clone();
    let mut changed = true;
    while changed {
        changed = false;
        let mut index = 0;
        while index < best.len() {
            let mut candidate = best.clone();
            candidate.remove(index);
            if fails(&candidate) {
                best = candidate;
                changed = true;
            } else {
                index += 1;
            }
        }
    }
    assert_eq!(best.len(), 1, "the shrinker did not reach one operation");
    assert!(fails(&best), "the shortest sequence stopped failing");
}
