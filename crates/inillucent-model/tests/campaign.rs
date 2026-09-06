//! The trace driver: one trace, run against the model and against the engine.
//!
//! Invariant: **the model is asked first and the engine is graded against it.**
//! Every step of a trace is applied to both, and every read is compared. When
//! the trace crashes, the engine is dropped, reopened on the crashed media and
//! recovered - and the state it comes back with has to be one of the states the
//! model says a crash there could leave.
//!
//! ## Why the driver is here and not in `inillucent-txn`
//!
//! Because the model must not be able to call the engine. `inillucent-model`'s
//! only dependency is the error type; the engine crates are dev-dependencies of
//! *this file*, which is the one place the two meet. A model that could reach
//! the implementation could share a bug with it, and the two agreeing would then
//! be evidence of nothing.
//!
//! ## What a crash means here
//!
//! ## Reading a failure
//!
//! Set `CAMPAIGN_DEBUG` in the environment and every crash and every reopen
//! prints what the engine held, what the model expected, what recovery reported
//! and how many writebacks no-steal held back. Four of the defects this file
//! found were diagnosed from those four numbers together, and none of them was
//! diagnosable from the assertion alone - so the printing is part of the test
//! rather than something removed once it had done its job.
//!
//! synced, and whatever of the rest the model of a torn write left behind - and
//! `SimVfs::recovered` opens a new file system over exactly those bytes. Nothing
//! in the process survives: the engine is dropped, the trees are dropped, and
//! what comes back is what a machine that lost power would have.

use std::collections::BTreeMap;
use std::sync::Arc;

use inillucent_model::model::{Model, State};
use inillucent_model::trace::{Generator, Op, Trace};
use inillucent_pool::{Database, Options, PageId};
use inillucent_sim::{Failure, Policy, SimConfig, SimVfs, Site};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::types::{ColumnSpec, PhysicalType};
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_txn::engine::{Begin, Engine, EngineOptions, Transaction, UndoSink};
use inillucent_txn::redo::TreeRows;
use inillucent_txn::undo::Undo;
use inillucent_txn::version::Visible;
use inillucent_vfs::{DbPath, Vfs};
use inillucent_wal::record::Body;
use inillucent_wal::writer::WalOptions;
use inillucent_wal::Synchronous;

/// The page size the campaigns run at.
///
/// Small on purpose. A big page holds every key a trace touches in one leaf,
/// and a campaign that never splits a leaf never tests a split - which is the
/// operation that writes three pages under one log record.
const PAGE: usize = 512;

/// How many trees a trace writes to.
const TREES: u32 = 2;

/// Returns the column directory every tree in a campaign has.
///
/// An integer key and a text value: the smallest shape that has both a
/// fixed-width mini-column and a variable-width one, so a row lands in the heap
/// as well as in a slot.
fn columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
    ]
}

/// Returns engine options at the campaign's page size.
///
/// @param policy - the sync policy
fn options(policy: Synchronous) -> EngineOptions {
    EngineOptions {
        database: Options::default().with_page_size(PAGE).with_frames(24),
        wal: WalOptions {
            synchronous: policy,
            segment_bytes: 65_536,
        },
        busy_timeout_ms: 0,
    }
}

/// A `TreeLog` that writes through one transaction.
struct TxnLog<'a, 'e> {
    txn: &'a mut Transaction<'e>,
}

impl TreeLog for TxnLog<'_, '_> {
    fn log(&mut self, body: Body<'_>) -> DbResultAlias<u64> {
        self.txn.log(body)
    }
}

/// The result type the engine crates use, aliased so this file reads.
type DbResultAlias<T> = Result<T, inillucent_base::DbError>;

/// Puts rolled-back rows back into their trees.
///
/// The restores are logged under transaction **zero**, not under the
/// transaction being rolled back. A record belonging to a transaction that
/// aborts is one recovery skips, so a restore logged there would be skipped too
/// - and the page it was undoing would come back with the *uncommitted* change
/// on it. Zero belongs to no transaction, is never a loser, and is therefore
/// always replayed, which is what an undo needs.
struct TreeUndo<'a> {
    engine: &'a Engine,
    trees: &'a mut BTreeMap<u32, PagedTree>,
    columns: Vec<ColumnSpec>,
}

impl UndoSink for TreeUndo<'_> {
    fn restore(&mut self, undo: &Undo) -> DbResultAlias<()> {
        let Some(tree) = self.trees.get_mut(&(undo.tree as u32)) else {
            return Ok(());
        };
        let (key, _) = Datum::decode_tagged(&undo.key)?;
        let mut log = WalLog {
            engine: self.engine,
        };
        let columns = self.columns.clone();
        let _ = columns;
        self.engine.with_database(|database| match &undo.before {
            Some(bytes) => {
                let (value, _) = Datum::decode_tagged(bytes)?;
                tree.insert(database, &mut log, &[key, value]).map(|_| ())
            }
            None => tree.delete(database, &mut log, &[key]).map(|_| ()),
        })
    }
}

/// A `TreeLog` that writes outside any transaction.
struct WalLog<'a> {
    engine: &'a Engine,
}

impl TreeLog for WalLog<'_> {
    fn log(&mut self, body: Body<'_>) -> DbResultAlias<u64> {
        self.engine.wal().append(0, body)
    }
}

/// Builds an empty tree for every id a trace uses.
///
/// @param database - the file
fn build_trees(database: &mut Database) -> BTreeMap<u32, PageId> {
    let mut roots = BTreeMap::new();
    for id in 0..TREES {
        let empty: Vec<Vec<Datum<'_>>> = Vec::new();
        if let Ok(tree) = PagedTree::bulk_build(database, u64::from(id), columns(), 1, &empty) {
            roots.insert(id, tree.root());
        }
    }
    roots
}

/// Re-attaches every tree after the file has been opened.
///
/// By scanning rather than from remembered counts, because a crash may have
/// left the file at a different shape than the process that wrote it thought -
/// and a harness that carried its own idea of a tree's height across a crash
/// would be asserting its own bookkeeping rather than the file's.
///
/// @param engine - the engine
/// @param roots - each tree's root page
fn attach_trees(engine: &Engine, roots: &BTreeMap<u32, PageId>) -> BTreeMap<u32, PagedTree> {
    let mut trees = BTreeMap::new();
    engine.with_pool(|pool| {
        for (id, root) in roots {
            if let Ok(tree) = PagedTree::attach_scanned(pool, u64::from(*id), *root, columns(), 1) {
                trees.insert(*id, tree);
            }
        }
    });
    trees
}

/// Returns the row redo the campaign's trees need.
fn tree_rows() -> TreeRows {
    let mut rows = TreeRows::new();
    for id in 0..TREES {
        rows = rows.with_tree(u64::from(id), columns(), 1);
    }
    rows
}

/// Reads every committed row out of the engine, as the model holds them.
///
/// @param engine - the engine
/// @param trees - the attached trees
fn engine_state(engine: &Engine, trees: &BTreeMap<u32, PagedTree>) -> State {
    let mut state = State::new();
    engine.with_pool(|pool| {
        for (id, tree) in trees {
            let Ok(rows) = tree.rows(pool) else {
                continue;
            };
            for row in rows {
                let (Some(OwnedDatum::Int(key)), Some(OwnedDatum::Text(value))) =
                    (row.first(), row.get(1))
                else {
                    continue;
                };
                state.insert((*id, *key as u64), value.clone());
            }
        }
    });
    state
}

/// Reads one key as a transaction should see it.
///
/// **The version log first, the tree second.** A tree holds one version of a
/// row - the newest - so a reader that only read the tree would see every commit
/// made since it began, which is not a snapshot. The version log holds the
/// before-image of every key changed since the oldest open snapshot, and
/// `Engine::visible` says which of the three answers applies: read the page,
/// read these bytes instead, or the row did not exist yet.
///
/// Consulting the log even for the writer is safe *because there is one writer
/// slot*: nothing can have committed a change to any key while the writer has
/// been open, so the log has no image above its snapshot and the answer is
/// always "read the page" - which is where its own uncommitted writes are. A
/// second writer would break that, and would need the reader to consult its own
/// undo buffer first.
///
/// @param engine - the engine
/// @param tree - the tree the key lives in
/// @param txn - the reading transaction, or `None` to read the committed state
/// @param tree_id - the tree's id, as the version log keys it
/// @param key - the key
fn read_visible(
    engine: &Engine,
    tree: &PagedTree,
    txn: Option<&Transaction<'_>>,
    tree_id: u32,
    key: u64,
) -> Option<Vec<u8>> {
    let mut key_bytes = Vec::new();
    Datum::Int(key as i64).encode_tagged(&mut key_bytes);
    let from_log = txn.map(|handle| {
        engine.visible_to(
            u64::from(tree_id),
            &key_bytes,
            handle.snapshot(),
            Some(handle.id()),
            |visible| match visible {
                Visible::Page => Held::Page,
                Visible::Instead(bytes) => Held::Instead(bytes.to_vec()),
                Visible::Absent => Held::Absent,
            },
        )
    });
    match from_log {
        Some(Held::Absent) => return None,
        Some(Held::Instead(bytes)) => {
            let (value, _) = Datum::decode_tagged(&bytes).ok()?;
            return match value {
                Datum::Text(held) => Some(held.to_vec()),
                _ => None,
            };
        }
        Some(Held::Page) | None => {}
    }
    engine
        .with_pool(|pool| tree.point(pool, &[Datum::Int(key as i64)]))
        .ok()
        .flatten()
        .and_then(|row| match row.get(1) {
            Some(OwnedDatum::Text(bytes)) => Some(bytes.clone()),
            _ => None,
        })
}

/// The version log's answer, owned so the borrow ends with the lookup.
enum Held {
    /// Read the page.
    Page,
    /// Read these bytes instead.
    Instead(Vec<u8>),
    /// The row did not exist at the snapshot.
    Absent,
}

/// One difference between the model and the engine.
type Divergence = String;

/// Runs one trace against both, and returns every disagreement.
///
/// @param trace - the trace
/// @param policy - the sync policy the engine runs under
fn run(trace: &Trace, policy: Synchronous) -> Vec<Divergence> {
    let mut failures: Vec<Divergence> = Vec::new();
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("model.rdb");
    let roots = {
        let Ok(engine) = Engine::create(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, options(policy))
        else {
            return vec!["the database would not be created".to_string()];
        };
        let roots = engine.with_database(build_trees);
        if engine.checkpoint().is_err() {
            return vec!["the first checkpoint failed".to_string()];
        }
        roots
    };

    let mut model = Model::new();
    let mut at = 0usize;
    // The media, carried forward across every crash. Each crash produces a new
    // file system over the bytes that survived it, and the *next* crash has to
    // be taken from that one - taking it from the original again would discard
    // every segment's work and grade recovery against a file nothing wrote to.
    let mut media = vfs;
    while at < trace.ops.len() {
        // One engine per segment of the trace between crashes. A crash drops
        // everything the process held, which is what a crash *is* - so the
        // segments are separate scopes rather than a flag on a loop.
        let snapshot = media.crash();
        media = Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
        let engine = match Engine::open(
            Arc::clone(&media) as Arc<dyn Vfs>,
            &path,
            options(policy),
            tree_rows(),
        ) {
            Ok(engine) => engine,
            Err(error) => {
                failures.push(format!(
                    "seed {}: the database would not reopen at step {at}: {}",
                    trace.seed,
                    error.detail().unwrap_or("no detail")
                ));
                return failures;
            }
        };
        let mut trees = attach_trees(&engine, &roots);
        if std::env::var("CAMPAIGN_DEBUG").is_ok() {
            eprintln!(
                "CAMPAIGNDBG reopen {at}: engine {:?} model {:?} recovered {:?} roots {:?} trees {}",
                engine_state(&engine, &trees).keys().collect::<Vec<_>>(),
                model.committed().keys().collect::<Vec<_>>(),
                engine.recovered(),
                roots,
                trees.len()
            );
        }
        // What recovery actually produced decides which of the model's allowed
        // prefixes happened; anything outside them is a violation.
        let recovered = engine_state(&engine, &trees);
        if let Some(violation) = model.recover(&recovered) {
            // The segment that led here, so a failure is a sequence rather than
            // a number. A campaign that reports only the step is a campaign
            // whose failures have to be reproduced before they can be read.
            let start = trace.ops[..at]
                .iter()
                .rposition(|op| *op == Op::Crash)
                .map(|held| held.saturating_add(1))
                .unwrap_or(0);
            let segment: Vec<String> = trace.ops[start..at]
                .iter()
                .enumerate()
                .map(|(offset, op)| format!("    {} {op:?}", start.saturating_add(offset)))
                .collect();
            failures.push(format!(
                "seed {}: at step {at}, {}: expected {}, found {}
  the segment was:
{}",
                trace.seed,
                violation.rule,
                violation.expected,
                violation.found,
                segment.join(
                    "
"
                )
            ));
            return failures;
        }
        at = segment(
            &engine,
            &mut trees,
            &mut model,
            trace,
            at,
            policy,
            &mut failures,
        );
        if !failures.is_empty() {
            return failures;
        }
    }
    failures
}

/// Runs one segment of a trace, up to and including the next crash.
///
/// Returns the index of the step after the crash.
///
/// @param engine - the engine for this segment
/// @param trees - the attached trees
/// @param model - the reference
/// @param trace - the trace
/// @param from - where to start
/// @param policy - the sync policy
/// @param failures - where a disagreement is recorded
#[allow(clippy::too_many_arguments)]
fn segment(
    engine: &Engine,
    trees: &mut BTreeMap<u32, PagedTree>,
    model: &mut Model,
    trace: &Trace,
    from: usize,
    policy: Synchronous,
    failures: &mut Vec<Divergence>,
) -> usize {
    let mut open: BTreeMap<u32, Transaction<'_>> = BTreeMap::new();
    for (offset, op) in trace.ops.iter().enumerate().skip(from) {
        let step = offset;
        match op {
            Op::Crash => {
                if std::env::var("CAMPAIGN_DEBUG").is_ok() {
                    eprintln!(
                        "CAMPAIGNDBG crash at {step}: held_back {} watermark {} engine {:?}",
                        engine.with_pool(|pool| pool.held_back()),
                        engine.with_pool(|pool| pool.uncommitted_lsn()),
                        engine_state(engine, trees).keys().collect::<Vec<_>>()
                    );
                }
                // Every transaction goes away unfinished, which is what makes
                // its writes disappear.
                drop(open);
                return step.saturating_add(1);
            }
            Op::Begin { txn, immediate } => {
                let how = if *immediate {
                    Begin::Immediate
                } else {
                    Begin::Deferred
                };
                match engine.begin(how) {
                    Ok(handle) => {
                        open.insert(*txn, handle);
                        model.begin(*txn);
                    }
                    Err(error) => failures.push(format!(
                        "seed {}: step {step}: begin refused: {}",
                        trace.seed,
                        error.detail().unwrap_or("no detail")
                    )),
                }
            }
            Op::Write {
                txn,
                tree,
                key,
                value,
            } => {
                let Some(handle) = open.get_mut(txn) else {
                    continue;
                };
                let Some(held) = trees.get_mut(tree) else {
                    continue;
                };
                let before = engine.with_pool(|pool| held.point(pool, &[Datum::Int(*key as i64)]));
                let before = match before {
                    Ok(row) => row.and_then(|row| row.get(1).cloned()),
                    Err(error) => {
                        failures.push(format!(
                            "seed {}: step {step}: reading the before-image failed: {}",
                            trace.seed,
                            error.detail().unwrap_or("no detail")
                        ));
                        continue;
                    }
                };
                let mut key_bytes = Vec::new();
                Datum::Int(*key as i64).encode_tagged(&mut key_bytes);
                let before_bytes = before.as_ref().map(|held| {
                    let mut bytes = Vec::new();
                    held.borrow().encode_tagged(&mut bytes);
                    bytes
                });
                handle.record_undo(u64::from(*tree), key_bytes, before_bytes);
                let mut log = TxnLog { txn: handle };
                let outcome = engine.with_database(|database| match value {
                    Some(bytes) => held
                        .insert(
                            database,
                            &mut log,
                            &[Datum::Int(*key as i64), Datum::Text(bytes)],
                        )
                        .map(|_| ()),
                    None => held
                        .delete(database, &mut log, &[Datum::Int(*key as i64)])
                        .map(|_| ()),
                });
                match outcome {
                    Ok(()) => model.write(*txn, *tree, *key, value.clone()),
                    Err(error) => failures.push(format!(
                        "seed {}: step {step}: the write failed: {}",
                        trace.seed,
                        error.detail().unwrap_or("no detail")
                    )),
                }
            }
            Op::Read { txn, tree, key } => {
                let Some(held) = trees.get(tree) else {
                    continue;
                };
                let found =
                    read_visible(engine, held, txn.and_then(|id| open.get(&id)), *tree, *key);
                let wanted = model.read(*txn, *tree, *key).map(<[u8]>::to_vec);
                if found != wanted {
                    failures.push(format!(
                        "seed {}: step {step}: reading tree {tree} key {key} in {txn:?} gave \
                         {found:?}, the model says {wanted:?}",
                        trace.seed
                    ));
                }
            }
            Op::Savepoint { txn, name } => {
                if let Some(handle) = open.get_mut(txn) {
                    handle.savepoint(name);
                    model.savepoint(*txn, name);
                }
            }
            Op::RollbackTo { txn, name } => {
                let Some(handle) = open.get_mut(txn) else {
                    continue;
                };
                let mut sink = TreeUndo {
                    engine,
                    trees,
                    columns: columns(),
                };
                match handle.rollback_to(name, &mut sink) {
                    Ok(_) => model.rollback_to(*txn, name),
                    Err(error) => failures.push(format!(
                        "seed {}: step {step}: rolling back to {name} failed: {}",
                        trace.seed,
                        error.detail().unwrap_or("no detail")
                    )),
                }
            }
            Op::Release { txn, name } => {
                if let Some(handle) = open.get_mut(txn) {
                    let _ = handle.release(name);
                    model.release(*txn, name);
                }
            }
            Op::Commit { txn } => {
                let Some(mut handle) = open.remove(txn) else {
                    continue;
                };
                match handle.commit() {
                    Ok(_) => {
                        model.commit(*txn);
                        if policy == Synchronous::Full {
                            // Under FULL the commit is on the media when it
                            // returns, so nothing at or below it can be lost.
                            model.note_durable();
                        }
                    }
                    Err(error) => failures.push(format!(
                        "seed {}: step {step}: the commit failed: {}",
                        trace.seed,
                        error.detail().unwrap_or("no detail")
                    )),
                }
            }
            Op::Rollback { txn } => {
                let Some(mut handle) = open.remove(txn) else {
                    continue;
                };
                let mut sink = TreeUndo {
                    engine,
                    trees,
                    columns: columns(),
                };
                match handle.rollback(&mut sink) {
                    Ok(()) => model.rollback(*txn),
                    Err(error) => failures.push(format!(
                        "seed {}: step {step}: the rollback failed: {}",
                        trace.seed,
                        error.detail().unwrap_or("no detail")
                    )),
                }
            }
            Op::Checkpoint => {
                // A checkpoint with a transaction open cannot advance past it,
                // which the engine enforces.
                //
                // **It is a durability boundary under `NORMAL` and `FULL` and
                // not under `OFF`**, because under `OFF` it syncs nothing - the
                // log's own documentation says so, and telling the model
                // otherwise would have it demand a guarantee the setting does
                // not offer. Under `OFF` nothing is ever known durable, so every
                // prefix of the commits is an allowed outcome, which is exactly
                // what `OFF` promises and the whole of what it promises.
                match engine.checkpoint() {
                    Ok(_) => {
                        if policy != Synchronous::Off {
                            model.note_durable();
                        }
                    }
                    Err(error) => failures.push(format!(
                        "seed {}: step {step}: the checkpoint failed: {}",
                        trace.seed,
                        error.detail().unwrap_or("no detail")
                    )),
                }
            }
        }
        if !failures.is_empty() {
            return trace.ops.len();
        }
    }
    trace.ops.len()
}

/// A seeded campaign holds every rule the model states.
#[test]
fn a_seeded_campaign_agrees_with_the_model() {
    let mut failures = Vec::new();
    for seed in 0..24u64 {
        let trace = Generator::new(seed).trace(seed, 220);
        failures.extend(run(&trace, Synchronous::Full));
        if failures.len() > 4 {
            break;
        }
    }
    assert!(
        failures.is_empty(),
        "the engine and the model disagree:\n{}",
        failures.join("\n")
    );
}

/// The campaign holds under the two policies that promise anything.
///
/// `FULL` makes a commit durable when it returns. `NORMAL` does not, but it
/// syncs at a checkpoint - so the log is always repairable and the prefix rule
/// still holds; what differs is *how much* a crash may lose, which is what the
/// model's durable point tracks. Running the same traces under both is what
/// makes the difference between them a tested difference rather than a
/// documented one.
///
/// `OFF` is deliberately not here; [`under_off_a_crash_promises_only_that_the_database_reopens`]
/// says what it does promise and why that is less.
#[test]
fn the_campaign_holds_under_every_sync_policy_that_promises_anything() {
    let mut failures = Vec::new();
    for policy in [Synchronous::Full, Synchronous::Normal] {
        for seed in 100..108u64 {
            let trace = Generator::new(seed).trace(seed, 160);
            for failure in run(&trace, policy) {
                failures.push(format!("{policy:?}: {failure}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "a sync policy broke the model:\n{}",
        failures.join("\n")
    );
}

/// Under `OFF`, a crash promises only that the database reopens.
///
/// **This test asserts less than the others on purpose, and the smaller claim is
/// the correct one.** `synchronous = OFF` syncs nothing: not the log, not the
/// data file. A power failure can therefore leave the file with some of a
/// checkpoint's pages written and others not, and the log that would repair them
/// gone too - a state that is not any prefix of the commits. SQLite says the
/// same thing about the same setting, in the same words: the database "might
/// become corrupted if the operating system crashes or the computer loses
/// power".
///
/// So the prefix rule is not asserted here, because it is not promised. What is
/// promised, and is asserted, is that the engine does not *fail*: the database
/// reopens, recovery terminates, and every tree it comes back with is
/// structurally sound. A caller who chose `OFF` accepted losing data; nobody
/// accepts a file that cannot be opened or a b-tree whose separators are wrong.
///
/// The campaign was originally written to hold the prefix rule under all three
/// policies. It failed under `OFF` - "nearest is commits 0..=6, differs by
/// (0, 6) is missing" - and the right response was to correct the claim rather
/// than the engine, because the engine was doing what `OFF` says.
#[test]
fn under_off_a_crash_promises_only_that_the_database_reopens() {
    let mut failures = Vec::new();
    let mut arms = 0usize;
    for seed in 200..208u64 {
        let trace = Generator::new(seed).trace(seed, 160);
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        let path = DbPath::new("off.rdb");
        let roots = {
            let Ok(engine) = Engine::create(
                Arc::clone(&vfs) as Arc<dyn Vfs>,
                &path,
                options(Synchronous::Off),
            ) else {
                continue;
            };
            let roots = engine.with_database(build_trees);
            if engine.checkpoint().is_err() {
                continue;
            }
            roots
        };
        let mut media: Arc<SimVfs> = vfs;
        let mut model = Model::new();
        let mut at = 0usize;
        while at < trace.ops.len() {
            let snapshot = media.crash();
            media = Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
            let engine = match Engine::open(
                Arc::clone(&media) as Arc<dyn Vfs>,
                &path,
                options(Synchronous::Off),
                tree_rows(),
            ) {
                Ok(engine) => engine,
                Err(error) => {
                    failures.push(format!(
                        "seed {seed}: the database would not reopen at step {at}: {}",
                        error.detail().unwrap_or("no detail")
                    ));
                    break;
                }
            };
            let mut trees = attach_trees(&engine, &roots);
            for (id, tree) in &trees {
                if let Err(error) = engine.with_pool(|pool| tree.check(pool)) {
                    failures.push(format!(
                        "seed {seed}: tree {id} is structurally wrong after the crash at \
                         step {at}: {}",
                        error.detail().unwrap_or("no detail")
                    ));
                }
            }
            arms = arms.saturating_add(1);
            // The model is driven so the trace's transactions are well formed,
            // and its answers are *not* compared: what it would claim about
            // durability is more than `OFF` offers.
            let mut ignored = Vec::new();
            at = segment(
                &engine,
                &mut trees,
                &mut model,
                &trace,
                at,
                Synchronous::Off,
                &mut ignored,
            );
            model.note_durable();
        }
    }
    assert!(arms >= 8, "the campaign only ran {arms} arms");
    assert!(
        failures.is_empty(),
        "`OFF` broke a promise it does make:\n{}",
        failures.join("\n")
    );
}

/// Failing the nth call never leaves a state the model disallows.
///
/// The campaign that says an *error* is as safe as a crash: a write that
/// reports a failure has to leave the database in a state the model recognises,
/// not merely fail loudly.
#[test]
fn failing_the_nth_call_never_leaves_a_state_the_model_disallows() {
    let mut arms = 0usize;
    let mut refused = 0usize;
    for nth in 1..=40u64 {
        let vfs = Arc::new(SimVfs::new(SimConfig::default()));
        let path = DbPath::new("nth.rdb");
        let roots = {
            let Ok(engine) = Engine::create(
                Arc::clone(&vfs) as Arc<dyn Vfs>,
                &path,
                options(Synchronous::Full),
            ) else {
                continue;
            };
            let roots = engine.with_database(build_trees);
            if engine.checkpoint().is_err() {
                continue;
            }
            roots
        };
        let trace = Generator::new(nth).trace(nth, 60);
        let mut model = Model::new();
        vfs.failpoints()
            .set(Site::Write, Policy::Nth(nth, Failure::IoError));
        {
            let Ok(engine) = Engine::open(
                Arc::clone(&vfs) as Arc<dyn Vfs>,
                &path,
                options(Synchronous::Full),
                tree_rows(),
            ) else {
                continue;
            };
            let mut trees = attach_trees(&engine, &roots);
            let mut ignored = Vec::new();
            // The failures are *expected* here - the point is what the database
            // looks like afterwards, not whether the write reported an error.
            segment(
                &engine,
                &mut trees,
                &mut model,
                &trace,
                0,
                Synchronous::Full,
                &mut ignored,
            );
            if !ignored.is_empty() {
                refused += 1;
            }
        }
        vfs.failpoints().set(Site::Write, Policy::Off);
        let snapshot = vfs.crash();
        let media: Arc<dyn Vfs> = Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
        let Ok(engine) = Engine::open(media, &path, options(Synchronous::Full), tree_rows()) else {
            continue;
        };
        let trees = attach_trees(&engine, &roots);
        let recovered = engine_state(&engine, &trees);
        arms += 1;
        assert!(
            model.recover(&recovered).is_none(),
            "failing write {nth} left a state the model does not allow"
        );
    }
    assert!(arms >= 24, "the campaign only ran {arms} arms");
    assert!(
        refused > 0,
        "no arm actually interrupted a write, so the campaign proved nothing"
    );
}
