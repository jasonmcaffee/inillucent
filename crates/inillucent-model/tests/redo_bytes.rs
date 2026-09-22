//! A replayed log lands on the page bytes the write path produced.
//!
//! Invariant: **recovery reproduces every page the write path left, byte for
//! byte, not only the rows on it.** A compaction and a delta insert are logged
//! without the page they produce - the record says what to do, and redo does it
//! again over the page LSN ordering has put back into the state the write saw.
//! That is only correct if doing it again is deterministic in the page alone,
//! and a replay that lands on different bytes is not caught by anything else:
//! the page it builds passes its checksum, because the checksum is computed
//! over whatever was built, and it may even hold the same rows - until a later
//! record in the same log, written against the page the write path had, meets
//! the page the replay has instead.
//!
//! task-2074 made three such things depend on more than they did: a delta row
//! goes to its place in a directory kept in key order, which is a search under
//! the tree's collations and directions; a compaction may splice rather than
//! repack, which is a choice the replay has to make the same way from the page
//! alone; and the page's checksum covers its LSN. So this test drives both
//! trees through enough writes to fill delta areas, splice, repack and split -
//! and asserts the campaign did each of those, because a test whose workload
//! never reached the code is a test of nothing - then crashes with the whole
//! campaign in the log, recovers, and compares every page.

use std::collections::BTreeMap;
use std::sync::Arc;

use inillucent_pool::page::{self, PageKind};
use inillucent_pool::{Options, PageId};
use inillucent_sim::{SimConfig, SimVfs};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::leaf::LeafRef;
use inillucent_tree::types::{ColumnSpec, PhysicalType};
use inillucent_tree::write::{TreeLog, WriteStats};
use inillucent_tree::PagedTree;
use inillucent_txn::engine::{Begin, Engine, EngineOptions};
use inillucent_txn::redo::TreeRows;
use inillucent_value::collation::Collation;
use inillucent_vfs::{DbPath, Vfs};
use inillucent_wal::record::Body;
use inillucent_wal::writer::WalOptions;
use inillucent_wal::Synchronous;

/// The page size the campaign runs at: small, so leaves fill, splice and split
/// within a few thousand writes.
const PAGE: usize = 4_096;

/// The table tree's id.
const TABLE: u64 = 1;

/// The index tree's id.
const INDEX: u64 = 2;

/// How many writes the campaign makes to each tree.
const STEPS: i64 = 3_000;

/// Returns engine options with a pool large enough that nothing is evicted.
///
/// **Nothing may reach the data file after the first checkpoint**, or the
/// recovered pages would start from something other than the pages the campaign
/// started from, and a difference would be a question about eviction rather than
/// about redo.
fn options() -> EngineOptions {
    EngineOptions {
        database: Options::default().with_page_size(PAGE).with_frames(4_096),
        wal: WalOptions {
            synchronous: Synchronous::Full,
            segment_bytes: 1 << 24,
        },
        busy_timeout_ms: 0,
    }
}

/// A rowid table: an integer key, a text and an integer.
fn table_columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::key(PhysicalType::Int64),
        ColumnSpec::new(PhysicalType::Text),
        ColumnSpec::new(PhysicalType::Int64),
    ]
}

/// An index ordered by a `NOCASE` text and then an integer descending.
///
/// Both properties change where a row sorts, so both change where a delta row
/// goes in the directory - and a replay that searched under `BINARY` or in
/// ascending order would put it somewhere else.
fn index_columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::key(PhysicalType::Text).with_collation(Collation::NoCase),
        ColumnSpec::key(PhysicalType::Int64).with_descending(true),
        ColumnSpec::new(PhysicalType::Int64),
    ]
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

/// Returns the name the index files a key under, in mixed case.
///
/// @param key - the row's key
fn name_of(key: i64) -> String {
    match key % 3 {
        0 => format!("NAME-{:03}", key % 211),
        1 => format!("name-{:03}", key % 211),
        _ => format!("Name-{:03}", key % 211),
    }
}

/// Returns the table row for a key at one step of the campaign.
///
/// @param key - the row's key
/// @param step - the step, which the text and the value carry
/// @param label - where the text is built
fn table_row(key: i64, step: i64, label: &str) -> Vec<Datum<'_>> {
    vec![
        Datum::Int(key),
        Datum::Text(label.as_bytes()),
        Datum::Int(step),
    ]
}

/// Returns the index row for a key at one step of the campaign.
///
/// @param key - the row's key
/// @param step - the step, which the value carries
/// @param name - the key's name, from `name_of`
fn index_row(key: i64, step: i64, name: &str) -> Vec<Datum<'_>> {
    vec![
        Datum::Text(name.as_bytes()),
        Datum::Int(key),
        Datum::Int(step),
    ]
}

/// Creates the two trees, each bulk built from a starting set of rows.
///
/// Bulk built so the first writes land in leaves packed the way an imported
/// table's are, which is what a splice mostly meets.
///
/// @param engine - the engine
/// @returns the two trees' roots
fn create(engine: &Engine) -> (PageId, PageId) {
    let labels: Vec<String> = (0..600).map(|key| format!("seed {key}")).collect();
    let names: Vec<String> = (0..600).map(|key| name_of(key * 17)).collect();
    let table: Vec<Vec<Datum<'_>>> = (0..600)
        .map(|key| table_row(key * 17, 0, &labels[key as usize]))
        .collect();
    let mut index: Vec<Vec<Datum<'_>>> = (0..600)
        .map(|key| index_row(key * 17, 0, &names[key as usize]))
        .collect();
    index.sort_by(|left, right| {
        inillucent_tree::leaf::compare_rows_under(
            left,
            right,
            2,
            &[Collation::NoCase, Collation::Binary],
            &[false, true],
        )
    });
    engine.with_database(|database| {
        let table = PagedTree::bulk_build(database, TABLE, table_columns(), 1, &table)
            .expect("the table builds");
        let index = PagedTree::bulk_build(database, INDEX, index_columns(), 2, &index)
            .expect("the index builds");
        (table.root(), index.root())
    })
}

/// Attaches the two trees.
///
/// @param engine - the engine
/// @param roots - the two roots
fn attach(engine: &Engine, roots: (PageId, PageId)) -> (PagedTree, PagedTree) {
    engine.with_pool(|pool| {
        let table = PagedTree::attach_scanned(pool, TABLE, roots.0, table_columns(), 1)
            .expect("the table attaches");
        let index = PagedTree::attach_scanned(pool, INDEX, roots.1, index_columns(), 2)
            .expect("the index attaches");
        (table, index)
    })
}

/// Runs the campaign: inserts in a scrambled order, replacements that grow a
/// row, and deletes, committed in batches.
///
/// @param engine - the engine
/// @param table - the table tree
/// @param index - the index tree
fn campaign(engine: &Engine, table: &mut PagedTree, index: &mut PagedTree) {
    let mut step = 0i64;
    while step < STEPS {
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        for _ in 0..250 {
            step += 1;
            // 10,007 is prime, so the keys are distinct and scrambled.
            let key = (step * 7_919) % 10_007;
            let label = format!("row {key} written at step {step}");
            let name = name_of(key);
            let mut log = TxnLog { txn: &mut txn };
            engine
                .with_database(|database| {
                    table.put(database, &mut log, &table_row(key, step, &label))?;
                    index.put(database, &mut log, &index_row(key, step, &name))
                })
                .expect("the writes");
            if step % 7 == 0 {
                // A replacement of a row written a few steps ago, with a longer
                // text: a delta row removed and put back, or a tombstone.
                let earlier = ((step - 3) * 7_919) % 10_007;
                let longer = format!("row {earlier} rewritten at step {step}, and longer");
                engine
                    .with_database(|database| {
                        table.put(database, &mut log, &table_row(earlier, step, &longer))
                    })
                    .expect("the replacement");
            }
            if step % 5 == 0 {
                let gone = ((step - 2) * 7_919) % 10_007;
                let gone_name = name_of(gone);
                engine
                    .with_database(|database| {
                        table.delete(database, &mut log, &[Datum::Int(gone)])?;
                        index.delete(
                            database,
                            &mut log,
                            &[Datum::Text(gone_name.as_bytes()), Datum::Int(gone)],
                        )
                    })
                    .expect("the deletes");
            }
        }
        txn.commit().expect("the commit");
    }
}

/// Returns every page the pool can read, with the fields a frame does not keep
/// the file's way cleared.
///
/// Two of them. The checksum is written when a page reaches the file and is not
/// kept in a frame, so it is not part of what a replay has to reproduce in
/// memory. And an interior page's child pointers are **swizzled** in a frame -
/// they hold the frame the child was loaded into, which depends on the order
/// pages were read in and not on the page - and are translated back only when
/// the page is written. Every other byte of an interior page is compared, and
/// every byte of a leaf, which is where the delta directory and the splice are.
///
/// @param engine - the engine
fn pages(engine: &Engine) -> BTreeMap<u64, Vec<u8>> {
    engine.with_pool(|pool| {
        let mut found = BTreeMap::new();
        for page in 2..pool.page_count() {
            let Ok(guard) = pool.fetch(PageId(page)) else {
                continue;
            };
            let mut bytes = guard.bytes().to_vec();
            if let Some(field) = bytes.get_mut(page::header::CHECKSUM..page::header::CHECKSUM + 4) {
                field.fill(0);
            }
            if page::kind_of(&bytes).ok() == Some(PageKind::Interior) {
                let swips = inillucent_pool::interior::swip_offsets_of(&bytes)
                    .expect("an interior page lists its child pointers");
                for at in swips {
                    if let Some(swip) = bytes.get_mut(at..at + 8) {
                        swip.fill(0);
                    }
                }
            }
            found.insert(page, bytes);
        }
        found
    })
}

/// Returns the rows a tree holds.
///
/// @param engine - the engine
/// @param tree - the tree
fn rows(engine: &Engine, tree: &PagedTree) -> Vec<Vec<OwnedDatum>> {
    engine.with_pool(|pool| tree.rows(pool).expect("the rows read"))
}

/// Counts the leaves whose delta directory holds at least two rows.
///
/// @param snapshot - the pages
fn leaves_with_a_directory(snapshot: &BTreeMap<u64, Vec<u8>>) -> usize {
    snapshot
        .values()
        .filter(|bytes| page::kind_of(bytes).ok() == Some(PageKind::Leaf))
        .filter_map(|bytes| LeafRef::parse(bytes).ok())
        .filter(|leaf| leaf.has_delta_directory() && leaf.delta_count() >= 2)
        .count()
}

/// Recovery lands on the page bytes the write path produced, for every page.
#[test]
fn a_replayed_log_lands_on_the_same_page_bytes() {
    let vfs = Arc::new(SimVfs::new(SimConfig::default()));
    let path = DbPath::new("redo-bytes.rdb");
    let (roots, before, table_rows, index_rows, stats) = {
        let engine = Engine::create(Arc::clone(&vfs) as Arc<dyn Vfs>, &path, options())
            .expect("a fresh database");
        let roots = create(&engine);
        engine.checkpoint().expect("the only checkpoint");
        let (mut table, mut index) = attach(&engine, roots);
        campaign(&engine, &mut table, &mut index);
        let stats: WriteStats = table.write_stats() + index.write_stats();
        (
            roots,
            pages(&engine),
            rows(&engine, &table),
            rows(&engine, &index),
            stats,
        )
    };
    // The campaign reached every path the replay has to reproduce.
    assert!(stats.splices > 0, "no compaction spliced: {stats:?}");
    assert!(
        stats.compactions > stats.splices,
        "every compaction spliced, so no repack was compared: {stats:?}"
    );
    assert!(stats.splits > 0, "nothing split: {stats:?}");
    assert!(stats.deleted > 0, "nothing was deleted: {stats:?}");
    assert!(
        leaves_with_a_directory(&before) > 0,
        "no leaf ended with a delta directory of two rows or more"
    );

    let snapshot = vfs.crash();
    let media: Arc<dyn Vfs> = Arc::new(SimVfs::recovered(SimConfig::default(), &snapshot));
    let engine = Engine::open(
        media,
        &path,
        options(),
        TreeRows::new()
            .with_tree(TABLE, table_columns(), 1)
            .with_tree(INDEX, index_columns(), 2),
    )
    .expect("the database reopens");
    let after = pages(&engine);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "recovery left a different set of readable pages"
    );
    for (page, bytes) in &before {
        let Some(replayed) = after.get(page) else {
            continue;
        };
        if bytes == replayed {
            continue;
        }
        let at = bytes
            .iter()
            .zip(replayed.iter())
            .position(|(left, right)| left != right)
            .unwrap_or(0);
        panic!(
            "page {page} ({:?}) differs after recovery, first at byte {at}: the write path had \
             {:?} and the replay has {:?}",
            page::kind_of(bytes),
            bytes.get(at..at + 8),
            replayed.get(at..at + 8),
        );
    }
    let (table, index) = attach(&engine, roots);
    assert_eq!(rows(&engine, &table), table_rows);
    assert_eq!(rows(&engine, &index), index_rows);
}
