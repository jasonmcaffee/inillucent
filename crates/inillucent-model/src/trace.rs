//! The vocabulary a trace is written in, and a seeded generator for them.
//!
//! Invariant: a trace is **data**, and the same trace run twice does the same
//! thing. Nothing here reads a clock, a thread id or the environment, and the
//! generator's only input is its seed - so a campaign that fails reports a
//! number, and that number is the whole of what a person needs to see it again.
//!
//! ## Why the generator is written rather than borrowed
//!
//! A property-testing crate would shrink a failing case for us, which is worth
//! something. It would also decide *what* to generate, and what to generate is
//! the interesting part: a uniform random trace over this vocabulary almost
//! never commits a transaction that wrote to a key another transaction is
//! reading, and almost never crashes between a commit and its checkpoint. Those
//! are the cases the whole phase is about, so the shape is chosen here -
//! deliberately, and stated in [`Generator`] - rather than inherited.

/// One step of a trace.
///
/// Deliberately small. Every operation here is one the engine has a primitive
/// for, so a driver is a `match` with no interpretation in it; anything that
/// needed interpreting would be the driver deciding what the engine meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Open a transaction.
    Begin {
        /// The transaction's number.
        txn: u32,
        /// Whether it takes the writer slot immediately.
        immediate: bool,
    },
    /// Write a key.
    Write {
        /// The transaction's number.
        txn: u32,
        /// Which tree.
        tree: u32,
        /// The key.
        key: u64,
        /// The value, or `None` to delete it.
        value: Option<Vec<u8>>,
    },
    /// Read a key and check the answer.
    Read {
        /// The transaction's number, or `None` to read outside one.
        txn: Option<u32>,
        /// Which tree.
        tree: u32,
        /// The key.
        key: u64,
    },
    /// Take a savepoint.
    Savepoint {
        /// The transaction's number.
        txn: u32,
        /// The savepoint's name.
        name: String,
    },
    /// Undo to a savepoint.
    RollbackTo {
        /// The transaction's number.
        txn: u32,
        /// The savepoint's name.
        name: String,
    },
    /// Release a savepoint.
    Release {
        /// The transaction's number.
        txn: u32,
        /// The savepoint's name.
        name: String,
    },
    /// Commit a transaction.
    Commit {
        /// The transaction's number.
        txn: u32,
    },
    /// Abandon a transaction.
    Rollback {
        /// The transaction's number.
        txn: u32,
    },
    /// Write every dirty page out and advance the recovery point.
    Checkpoint,
    /// Discard everything not on the media, then recover and compare.
    Crash,
}

/// A trace: the steps, and the seed that produced them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    /// The seed, so a failure names the one number needed to see it again.
    pub seed: u64,
    /// The steps, in order.
    pub ops: Vec<Op>,
}

/// A seeded random trace generator.
///
/// ## The shape, and why it is not uniform
///
/// A uniformly random trace over this vocabulary is nearly all noise. It opens
/// transactions that write nothing, reads keys nobody has written, and crashes
/// at points where the answer is obvious. The interesting states are the ones
/// where two things are true at once (a transaction is open across a commit, a
/// crash lands between a commit and the checkpoint that would have made it
/// safe) and a uniform draw reaches those about as often as it reaches anything
/// else, which is to say rarely.
///
/// So the generator is skewed on purpose, and every skew is written down:
///
/// - **The key space is small** (`keys`), so two transactions collide often. A
///   wide key space makes conflicts vanishingly rare and the isolation rules
///   untested.
/// - **A transaction is left open across other transactions' commits**, because
///   a snapshot that is never crossed is a snapshot never tested.
/// - **Crashes are frequent and unaligned**, landing between a commit and its
///   checkpoint far more often than a uniform draw would.
/// - **Savepoints nest**, because releasing an outer one while an inner one is
///   open is the case the rules are subtle about.
///
/// ## One writer at a time
///
/// The engine has a single writer slot, so a trace in which two transactions
/// write at once is a trace asking for `BUSY`. That refusal is real behaviour
/// with tests of its own - `busy_timeout` is Part 2's acceptance - but it is not
/// what an ACID campaign grades, and a trace made mostly of refused writes would
/// spend its steps proving the engine can say no. So the generator gives the pen
/// to one open transaction at a time and takes it back when that transaction
/// ends. **Readers stay open across other transactions' commits**, which is the
/// part that matters here: it is the only way a snapshot is ever something other
/// than the current state.
#[derive(Clone, Debug)]
pub struct Generator {
    state: u64,
    /// How many distinct keys a trace touches.
    pub keys: u64,
    /// How many trees it writes to.
    pub trees: u32,
    /// One in this many steps is a crash.
    pub crash_in: u32,
    /// One in this many steps is a checkpoint.
    pub checkpoint_in: u32,
}

impl Generator {
    /// Returns a generator for one seed, with the campaign's usual shape.
    ///
    /// @param seed - the seed
    pub fn new(seed: u64) -> Generator {
        Generator {
            // A zero seed would leave the xorshift at zero forever, producing
            // the same step every time - a trace that runs and tests one thing.
            state: seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1),
            keys: 24,
            trees: 2,
            crash_in: 40,
            checkpoint_in: 25,
        }
    }

    /// Returns the next pseudo-random number.
    ///
    /// xorshift64*, written out. The generator has to be reproducible across
    /// machines and across releases of anything, which rules out the standard
    /// library's hasher and every crate whose algorithm may change.
    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Returns a number below a bound.
    ///
    /// @param bound - the exclusive upper bound, treated as 1 when zero
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    /// Returns a trace of the given length.
    ///
    /// @param steps - how many operations to generate
    pub fn trace(&mut self, seed: u64, steps: usize) -> Trace {
        let mut ops = Vec::with_capacity(steps);
        // The transactions currently open, and the savepoints each one holds.
        let mut open: Vec<(u32, Vec<String>)> = Vec::new();
        let mut next_txn = 1u32;
        // Who currently holds the writer slot, if anybody.
        let mut writer: Option<u32> = None;
        for _ in 0..steps {
            if self.below(u64::from(self.crash_in)) == 0 {
                ops.push(Op::Crash);
                // A crash ends every open transaction, in the trace as in the
                // engine. A trace that went on addressing them afterwards would
                // be testing the driver's error handling, not the engine's.
                open.clear();
                writer = None;
                continue;
            }
            if self.below(u64::from(self.checkpoint_in)) == 0 {
                ops.push(Op::Checkpoint);
                continue;
            }
            // A new transaction, biased so that several are open at once - which
            // is what makes a snapshot something other than the current state.
            if open.len() < 3 && self.below(3) == 0 {
                let txn = next_txn;
                next_txn = next_txn.saturating_add(1);
                // `IMMEDIATE` takes the writer slot at `BEGIN`, so it is only
                // offered when the slot is free - and taking it makes this
                // transaction the writer for as long as it is open.
                let immediate = writer.is_none() && self.below(2) == 0;
                if immediate {
                    writer = Some(txn);
                }
                ops.push(Op::Begin { txn, immediate });
                open.push((txn, Vec::new()));
                continue;
            }
            if open.is_empty() {
                // Nothing is open, so the only meaningful step is a read of the
                // committed state - which is the one that catches a commit that
                // did not become visible.
                let tree = self.below(u64::from(self.trees)) as u32;
                let key = self.below(self.keys);
                ops.push(Op::Read {
                    txn: None,
                    tree,
                    key,
                });
                continue;
            }
            let at = self.below(open.len() as u64) as usize;
            let Some((txn, savepoints)) = open.get(at).map(|held| (held.0, held.1.clone())) else {
                continue;
            };
            let tree = self.below(u64::from(self.trees)) as u32;
            let key = self.below(self.keys);
            // Only the writer may write, and a transaction that writes becomes
            // the writer. Everything else this transaction can do is available
            // whether it holds the pen or not.
            let may_write = writer.is_none() || writer == Some(txn);
            match self.below(16) {
                0..=5 if may_write => {
                    writer = Some(txn);
                    let value = if self.below(5) == 0 {
                        None
                    } else {
                        Some(format!("t{txn}k{key}v{}", self.below(1_000)).into_bytes())
                    };
                    ops.push(Op::Write {
                        txn,
                        tree,
                        key,
                        value,
                    });
                }
                0..=5 => ops.push(Op::Read {
                    txn: Some(txn),
                    tree,
                    key,
                }),
                6..=9 => ops.push(Op::Read {
                    txn: Some(txn),
                    tree,
                    key,
                }),
                10 => {
                    let name = format!("s{}", savepoints.len());
                    ops.push(Op::Savepoint {
                        txn,
                        name: name.clone(),
                    });
                    if let Some(held) = open.get_mut(at) {
                        held.1.push(name);
                    }
                }
                11 => {
                    if let Some(name) =
                        pick(&savepoints, self.below(savepoints.len().max(1) as u64))
                    {
                        ops.push(Op::RollbackTo {
                            txn,
                            name: name.clone(),
                        });
                        if let Some(held) = open.get_mut(at) {
                            if let Some(position) = held.1.iter().rposition(|entry| entry == name) {
                                held.1.truncate(position.saturating_add(1));
                            }
                        }
                    }
                }
                12 => {
                    if let Some(name) =
                        pick(&savepoints, self.below(savepoints.len().max(1) as u64))
                    {
                        ops.push(Op::Release {
                            txn,
                            name: name.clone(),
                        });
                        if let Some(held) = open.get_mut(at) {
                            if let Some(position) = held.1.iter().rposition(|entry| entry == name) {
                                held.1.truncate(position);
                            }
                        }
                    }
                }
                13 => {
                    ops.push(Op::Rollback { txn });
                    open.remove(at);
                    if writer == Some(txn) {
                        writer = None;
                    }
                }
                _ => {
                    ops.push(Op::Commit { txn });
                    open.remove(at);
                    if writer == Some(txn) {
                        writer = None;
                    }
                }
            }
        }
        Trace { seed, ops }
    }
}

/// Returns how many distinct steps a trace holds.
///
/// By formatting rather than by hashing, because `Op` is compared for equality
/// and nothing else - giving it an ordering so a test could count it would be
/// deriving a trait to satisfy a test rather than because the type has one.
///
/// @param ops - the trace's steps
#[cfg(test)]
fn variety(ops: &[Op]) -> usize {
    ops.iter()
        .map(|op| format!("{op:?}"))
        .collect::<std::collections::BTreeSet<String>>()
        .len()
}

/// Returns one savepoint name by position, if there is one.
///
/// @param names - the open savepoints
/// @param at - which one
fn pick(names: &[String], at: u64) -> Option<&String> {
    names.get(at as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same seed produces the same trace.
    #[test]
    fn a_seed_names_one_trace() {
        for seed in 0..8u64 {
            let first = Generator::new(seed).trace(seed, 200);
            let again = Generator::new(seed).trace(seed, 200);
            assert_eq!(first, again, "seed {seed} produced two different traces");
        }
    }

    /// Different seeds produce different traces, including zero.
    ///
    /// Zero is the one that has to be checked: an xorshift left at zero stays
    /// at zero, and a generator seeded that way emits the same step forever -
    /// a trace that runs, passes, and tests one thing.
    #[test]
    fn different_seeds_produce_different_traces() {
        let zero = Generator::new(0).trace(0, 200);
        let one = Generator::new(1).trace(1, 200);
        assert_ne!(zero, one);
        assert!(
            variety(&zero.ops) > 20,
            "the zero seed produced a trace with almost no variety"
        );
    }

    /// A trace reaches every operation the vocabulary has.
    ///
    /// A generator that never emits a savepoint is a generator whose savepoint
    /// rules are untested, and nothing else in the campaign would say so.
    #[test]
    fn a_campaign_reaches_every_operation() {
        let mut seen = [false; 10];
        for seed in 0..40u64 {
            for op in Generator::new(seed).trace(seed, 400).ops {
                let at = match op {
                    Op::Begin { .. } => 0,
                    Op::Write { .. } => 1,
                    Op::Read { .. } => 2,
                    Op::Savepoint { .. } => 3,
                    Op::RollbackTo { .. } => 4,
                    Op::Release { .. } => 5,
                    Op::Commit { .. } => 6,
                    Op::Rollback { .. } => 7,
                    Op::Checkpoint => 8,
                    Op::Crash => 9,
                };
                if let Some(slot) = seen.get_mut(at) {
                    *slot = true;
                }
            }
        }
        assert!(
            seen.iter().all(|held| *held),
            "the generator never emitted every operation: {seen:?}"
        );
    }

    /// A trace never names a transaction that is not open.
    ///
    /// The driver would otherwise be exercising its own error handling rather
    /// than the engine, and a campaign that spends its steps on that is a
    /// campaign that measures less than it looks like it does.
    #[test]
    fn a_trace_never_names_a_transaction_that_is_not_open() {
        for seed in 0..20u64 {
            let mut open = std::collections::BTreeSet::new();
            for op in Generator::new(seed).trace(seed, 400).ops {
                match op {
                    Op::Begin { txn, .. } => {
                        assert!(open.insert(txn), "seed {seed} began {txn} twice");
                    }
                    Op::Crash => open.clear(),
                    Op::Checkpoint => {}
                    Op::Read { txn: None, .. } => {}
                    Op::Commit { txn } | Op::Rollback { txn } => {
                        assert!(open.remove(&txn), "seed {seed} ended {txn} when not open");
                    }
                    Op::Write { txn, .. }
                    | Op::Savepoint { txn, .. }
                    | Op::RollbackTo { txn, .. }
                    | Op::Release { txn, .. }
                    | Op::Read { txn: Some(txn), .. } => {
                        assert!(open.contains(&txn), "seed {seed} used {txn} when not open");
                    }
                }
            }
        }
    }
}
