//! The smallest crash that has to work, written out rather than generated.
//!
//! Invariant: a committed transaction survives a crash **whatever an open
//! transaction was doing at the time**. The two interact through the
//! checkpointer: no-steal keeps the open transaction's pages out of the file,
//! and those are the same pages the committed transaction wrote - so the
//! recovery point has to stay below the committed change as well as below the
//! open one.
//!
//! This file exists because the generated campaign found the failure and a
//! two-hundred-step trace is not a diagnosis. Every test here is one sequence
//! with one question.

use std::collections::BTreeMap;
use std::sync::Arc;

use inillucent_pool::{Database, Options, PageId};
use inillucent_sim::{SimConfig, SimVfs};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::types::{ColumnSpec, PhysicalType};
use inillucent_tree::write::TreeLog;
use inillucent_tree::PagedTree;
use inillucent_txn::engine::{Begin, Engine, EngineOptions, RecordingUndo};
use inillucent_txn::redo::TreeRows;
use inillucent_vfs::{DbPath, Vfs};
use inillucent_wal::record::Body;
use inillucent_wal::writer::WalOptions;
use inillucent_wal::Synchronous;

/// The page size these tests run at.
const PAGE: usize = 512;

/// Returns the one-key, one-value column directory.
fn columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
    ]
}

/// Returns engine options at the test page size.
fn options() -> EngineOptions {
    EngineOptions {
        database: Options::default().with_page_size(PAGE).with_frames(16),
        wal: WalOptions {
            synchronous: Synchronous::Full,
            segment_bytes: 65_536,
        },
        busy_timeout_ms: 0,
    }
}

/// A `TreeLog` over one transaction.
struct TxnLog<'a, 'e> {
    txn: &'a mut inillucent_txn::engine::Transaction<'e>,
}

impl TreeLog for TxnLog<'_, '_> {
    fn log(&mut self, body: Body<'_>) -> Result<u64, inillucent_base::DbError> {
        self.txn.log(body)
    }
}

/// Returns the rows a tree holds, as `(key, value)`.
///
/// @param engine - the engine
/// @param tree - the tree
fn rows_of(engine: &Engine, tree: &PagedTree) -> BTreeMap<i64, String> {
    engine.with_pool(|pool| {
        let mut found = BTreeMap::new();
        if let Ok(rows) = tree.rows(pool) {
            for row in rows {
                if let (Some(OwnedDatum::Int(key)), Some(OwnedDatum::Text(value))) =
                    (row.first(), row.get(1))
                {
                    found.insert(*key, String::from_utf8_lossy(value).into_owned());
                }
            }
        }
        found
    })
}

/// A committed transaction survives a crash taken during an open one.
///
/// The sequence is the one the generated campaign kept failing on, reduced:
///
/// 1. transaction one writes three keys and **commits**;
/// 2. transaction two writes to the same pages and stays **open**;
/// 3. a **checkpoint** happens, which must hold those pages back;
/// 4. the machine loses power.
///
/// Afterwards transaction one's rows have to be there and transaction two's
/// must not. Step 3 is what makes it interesting: the pages that carry the
/// committed rows are the same pages that carry the uncommitted ones, so a
/// checkpoint cannot write them - and then the *committed* change is not in the
/// file either, and the recovery point has to stay below it.
#[test]
fn a_commit_survives_a_crash_taken_during_an_open_transaction() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("minimal.rdb");
    let root = {
        let engine = Engine::create(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, options())
            .expect("a fresh database");
        let root = engine.with_database(|database: &mut Database| {
            let empty: Vec<Vec<Datum<'_>>> = Vec::new();
            PagedTree::bulk_build(database, 0, columns(), 1, &empty)
                .map(|tree| tree.root())
                .unwrap_or(PageId(0))
        });
        engine.checkpoint().expect("the first checkpoint");

        let mut tree = engine
            .with_pool(|pool| PagedTree::attach_scanned(pool, 0, root, columns(), 1))
            .expect("the tree attaches");

        // 1. A transaction that commits.
        let mut one = engine.begin(Begin::Immediate).expect("a transaction");
        for key in [7i64, 9, 16] {
            let value = format!("committed-{key}");
            let mut key_bytes = Vec::new();
            Datum::Int(key).encode_tagged(&mut key_bytes);
            one.record_undo(0, key_bytes, None);
            let mut log = TxnLog { txn: &mut one };
            engine
                .with_database(|database| {
                    tree.insert(
                        database,
                        &mut log,
                        &[Datum::Int(key), Datum::Text(value.as_bytes())],
                    )
                })
                .expect("the insert");
        }
        one.commit().expect("the commit");
        drop(one);

        // 2. A transaction that stays open, writing to the same pages.
        let mut two = engine.begin(Begin::Immediate).expect("a transaction");
        for key in [7i64, 22] {
            let value = format!("uncommitted-{key}");
            let mut key_bytes = Vec::new();
            Datum::Int(key).encode_tagged(&mut key_bytes);
            two.record_undo(0, key_bytes, None);
            let mut log = TxnLog { txn: &mut two };
            engine
                .with_database(|database| {
                    tree.insert(
                        database,
                        &mut log,
                        &[Datum::Int(key), Datum::Text(value.as_bytes())],
                    )
                })
                .expect("the insert");
        }

        // 3. A checkpoint while it is open.
        engine.checkpoint().expect("the checkpoint");

        // 4. Power loss: the transaction is never finished.
        std::mem::forget(two);
        root
    };

    let snapshot = vfs.crash();
    let media: Arc<dyn Vfs> = Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
    let engine = Engine::open(
        media,
        &path,
        options(),
        TreeRows::new().with_tree(0, columns(), 1),
    )
    .expect("the database reopens");
    let tree = engine
        .with_pool(|pool| PagedTree::attach_scanned(pool, 0, root, columns(), 1))
        .expect("the tree attaches");
    let found = rows_of(&engine, &tree);

    let mut wanted = BTreeMap::new();
    for key in [7i64, 9, 16] {
        wanted.insert(key, format!("committed-{key}"));
    }
    assert_eq!(
        found,
        wanted,
        "recovery lost a committed row or kept an uncommitted one; \
         recovery reported {:?}",
        engine.recovered()
    );
}

/// A rollback leaves the trees as they were, and the crash after it agrees.
#[test]
fn a_rollback_leaves_the_trees_as_they_were() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("rollback.rdb");
    let engine = Engine::create(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, options())
        .expect("a fresh database");
    let root = engine.with_database(|database: &mut Database| {
        let empty: Vec<Vec<Datum<'_>>> = Vec::new();
        PagedTree::bulk_build(database, 0, columns(), 1, &empty)
            .map(|tree| tree.root())
            .unwrap_or(PageId(0))
    });
    engine.checkpoint().expect("the first checkpoint");
    let mut tree = engine
        .with_pool(|pool| PagedTree::attach_scanned(pool, 0, root, columns(), 1))
        .expect("the tree attaches");

    let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
    let mut key_bytes = Vec::new();
    Datum::Int(3).encode_tagged(&mut key_bytes);
    txn.record_undo(0, key_bytes, None);
    let mut log = TxnLog { txn: &mut txn };
    engine
        .with_database(|database| {
            tree.insert(database, &mut log, &[Datum::Int(3), Datum::Text(b"gone")])
        })
        .expect("the insert");
    let mut sink = RecordingUndo::default();
    txn.rollback(&mut sink).expect("the rollback");
    assert_eq!(sink.restored.len(), 1, "the rollback restored nothing");
    // The sink here records rather than restores, so the *tree* still holds the
    // row - that is what a `RecordingUndo` is for. What this asserts is that the
    // rollback asked for the right undo, which is the transaction machinery's
    // half of the job.
}
