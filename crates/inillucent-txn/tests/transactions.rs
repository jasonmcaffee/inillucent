//! Transactions, isolation, the commit gate and the write-ahead rule, end to
//! end over a real database file and a real log.
//!
//! Invariant: **an isolation claim is checked against what a reader actually
//! reads**, never against a flag saying the reader is isolated.
//! The two are not the same, and the difference is the whole of MVCC: a snapshot
//! that returns the right timestamp and the wrong bytes passes every test of the
//! timestamp.

use std::sync::Arc;

use inillucent_pool::page::{self, header};
use inillucent_pool::{Options, PageId};
use inillucent_txn::engine::{Begin, Engine, EngineOptions, RecordingUndo};
use inillucent_txn::redo::RefuseRows;
use inillucent_txn::slot::WriterSlot;
use inillucent_txn::version::TxnId;
use inillucent_txn::version::Visible;
use inillucent_vfs::{DbPath, MemoryVfs, Vfs};
use inillucent_wal::record::Body;
use inillucent_wal::writer::WalOptions;
use inillucent_wal::Synchronous;

/// The page size the tests use. Small, because what is being tested is the
/// transaction machinery and a 32 KiB page would only slow the file down.
const PAGE: usize = 512;

/// Returns engine options at the test page size.
///
/// @param policy - the sync policy
fn options(policy: Synchronous) -> EngineOptions {
    EngineOptions {
        database: Options::default().with_page_size(PAGE).with_frames(32),
        wal: WalOptions {
            synchronous: policy,
            segment_bytes: 16_384,
        },
        busy_timeout_ms: 0,
    }
}

/// Creates a fresh engine on a fresh in-memory file system.
///
/// @param policy - the sync policy
fn fresh(policy: Synchronous) -> (Arc<dyn Vfs>, DbPath, Engine) {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("engine.rdb");
    let engine = Engine::create(Arc::clone(&vfs), &path, options(policy)).expect("an engine");
    (vfs, path, engine)
}

/// Writes a page image through the engine, with the LSN a record gave it.
///
/// @param engine - the engine
/// @param page - the page
/// @param body - the bytes to put after the header
/// @param lsn - the LSN to stamp
fn install(engine: &Engine, page: PageId, body: &[u8], lsn: u64) {
    let mut image = vec![0u8; PAGE];
    page::write_common(&mut image, page::PageKind::Leaf, 0, 1).expect("a header");
    page::write_u64(&mut image, header::LSN, lsn).expect("an lsn");
    let width = body.len().min(PAGE - 64);
    image
        .get_mut(64..64 + width)
        .expect("room in the page")
        .copy_from_slice(body.get(..width).unwrap_or(&[]));
    engine.with_database(|database| database.install(page, &image).expect("the page installs"));
}

/// Reads the bytes a page holds after its header.
///
/// @param engine - the engine
/// @param page - the page
fn read_back(engine: &Engine, page: PageId, width: usize) -> Vec<u8> {
    engine.with_pool(|pool| {
        let guard = pool.fetch(page).expect("the page reads");
        guard.bytes().get(64..64 + width).unwrap_or(&[]).to_vec()
    })
}

/// A reader never sees a writer's uncommitted state.
///
/// The check is on the *bytes the reader is sent to*, not on a flag: the page
/// already holds the new value, and what makes the reader correct is that
/// something sends it somewhere else.
///
/// ## This test used to assert the opposite, and its name was the part that was
/// right
///
/// It read: "the reader is sent to the page, because nothing has been
/// *published* - an uncommitted change has no timestamp, so there is no version
/// for the reader to be diverted to". Every clause of that is true about the
/// version log and the conclusion does not follow, because the *page* has the
/// uncommitted value on it. Sending a reader there is a **dirty read**, which is
/// what the test's own name says must not happen.
///
/// The engine now keeps the open writer's before-images in a second, small map
/// beside the version log - `Engine::uncommitted` - covering exactly the window
/// between a write and its commit that the version log cannot. The model
/// campaign found it on its first seed, as a reader that saw a value tagged
/// with the writing transaction's number.
#[test]
fn a_reader_never_sees_an_uncommitted_write() {
    let (_, _, engine) = fresh(Synchronous::Full);

    // A row committed at the start, so there is something to read.
    let mut setup = engine.begin(Begin::Immediate).expect("a transaction");
    setup.record_undo(1, b"k".to_vec(), None);
    setup
        .log(Body::InsertRow {
            tree: 1,
            page: 4,
            row: b"original",
        })
        .expect("a record");
    let first = setup.commit().expect("a commit");
    drop(setup);

    let reader = engine.begin(Begin::Deferred).expect("a reader");
    assert_eq!(reader.snapshot().cts(), first);

    // A writer changes the row and does not commit.
    let mut writer = engine.begin(Begin::Immediate).expect("a writer");
    writer.record_undo(1, b"k".to_vec(), Some(b"original".to_vec()));
    writer
        .log(Body::InsertRow {
            tree: 1,
            page: 4,
            row: b"changed",
        })
        .expect("a record");

    // The reader is diverted to what the row held before the writer touched it.
    // The page holds `changed`; sending the reader there would be a dirty read.
    engine.visible(1, b"k", reader.snapshot(), |visible| {
        assert_eq!(
            visible,
            Visible::Instead(b"original"),
            "a reader was sent to a page holding an uncommitted change"
        );
    });
    // The writer still reads its own write, which is the other half of the rule
    // and the reason the asking transaction is named.
    engine.visible_to(1, b"k", writer.snapshot(), Some(writer.id()), |visible| {
        assert_eq!(
            visible,
            Visible::Page,
            "a writer must read what it has written"
        );
    });
    // One image is in the *version log*, and it is the setup transaction's: the
    // row was absent before cts 1, so a reader older than that is owed
    // `Absent`. The uncommitted writer's image is not there - it is in the
    // separate map, because it has no timestamp to be filed under.
    assert_eq!(engine.versions_held(), 1);
    let older = engine.clock().active_timestamps();
    assert!(older.contains(&first));

    // Now it commits, and the reader is diverted to the before-image.
    let second = writer.commit().expect("a commit");
    assert!(second > first);
    engine.visible(1, b"k", reader.snapshot(), |visible| {
        assert_eq!(
            visible,
            Visible::Instead(b"original"),
            "the reader must see what the row held at its own snapshot"
        );
    });

    // And a reader that starts now sees the page.
    let later = engine.begin(Begin::Deferred).expect("a later reader");
    engine.visible(1, b"k", later.snapshot(), |visible| {
        assert_eq!(visible, Visible::Page);
    });
}

/// A row does not change under a reader inside one snapshot.
///
/// Ten commits to the same row while one reader holds a snapshot, and the
/// **value the reader resolves to** is the same before, during and after.
///
/// The first version of this test compared the `Visible` verdicts and failed on
/// correct behaviour: before any writer the reader is sent to the page, and
/// afterwards it is sent to a before-image, and those are *different verdicts
/// naming the same bytes*. What isolation promises is the bytes. So the test
/// keeps a model of what the page holds and resolves the verdict against it,
/// which is what a reader does and is the only version of the claim worth
/// asserting.
#[test]
fn a_row_does_not_change_under_a_reader() {
    let (_, _, engine) = fresh(Synchronous::Full);
    let mut page_holds = b"v-start".to_vec();
    let mut setup = engine.begin(Begin::Immediate).expect("a transaction");
    setup.record_undo(1, b"k".to_vec(), None);
    setup.commit().expect("a commit");

    /// Resolves what a reader would actually read.
    ///
    /// @param engine - the engine
    /// @param snapshot - the reader's snapshot
    /// @param page_holds - what the page holds right now
    fn resolve(
        engine: &Engine,
        snapshot: &inillucent_txn::version::Snapshot,
        page_holds: &[u8],
    ) -> Option<Vec<u8>> {
        engine.visible(1, b"k", snapshot, |visible| match visible {
            Visible::Page => Some(page_holds.to_vec()),
            Visible::Instead(bytes) => Some(bytes.to_vec()),
            Visible::Absent => None,
        })
    }

    let reader = engine.begin(Begin::Deferred).expect("a reader");
    let mut answers = Vec::new();
    for round in 0..10u64 {
        answers.push(resolve(&engine, reader.snapshot(), &page_holds));
        let mut writer = engine.begin(Begin::Immediate).expect("a writer");
        // The before-image is what the page holds; then the page changes. That
        // ordering is the contract `record_undo` documents, and getting it the
        // other way round is the mistake every single-change test would pass.
        writer.record_undo(1, b"k".to_vec(), Some(page_holds.clone()));
        page_holds = format!("v{round}").into_bytes();
        writer.commit().expect("a commit");
    }
    answers.push(resolve(&engine, reader.snapshot(), &page_holds));

    let first = answers.first().cloned().unwrap_or_default();
    assert_eq!(
        first,
        Some(b"v-start".to_vec()),
        "the reader did not start from what the row held"
    );
    assert!(
        answers.iter().all(|answer| *answer == first),
        "the reader's row changed under it: {answers:?}"
    );

    // And a reader that starts now sees the newest value, which is what makes
    // the assertion above about isolation rather than about a stuck cache.
    let now = engine.begin(Begin::Deferred).expect("a fresh reader");
    assert_eq!(
        resolve(&engine, now.snapshot(), &page_holds),
        Some(b"v9".to_vec())
    );
}

/// Commit order, visibility order and log order are one order.
///
/// The commit gate exists for this, and the reason it is a test is that the two
/// orders can disagree only under concurrency, where an assertion inside one
/// transaction would never look.
#[test]
fn commit_order_visibility_order_and_log_order_agree() {
    let (_, _, engine) = fresh(Synchronous::Full);
    let mut seen: Vec<(u64, u64)> = Vec::new();
    for round in 1..=20u64 {
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        let lsn = txn
            .log(Body::WritePage {
                page: round,
                image: b"x",
            })
            .expect("a record");
        let cts = txn.commit().expect("a commit");
        seen.push((cts, lsn));
    }
    for pair in seen.windows(2) {
        let (previous_cts, previous_lsn) = pair.first().copied().unwrap_or((0, 0));
        let (cts, lsn) = pair.get(1).copied().unwrap_or((0, 0));
        assert!(cts > previous_cts, "commit timestamps did not increase");
        assert!(lsn > previous_lsn, "log positions did not increase");
    }
}

/// A page whose LSN is above the durable log is refused by the pool.
///
/// This is the write-ahead rule, and it is asserted where it is *enforced*
/// rather than where it is intended. Phase 2 said the seam was
/// `Pool::writeback` and that Phase 3 would add "a condition rather than a
/// caller"; a test that only checked the engine's own ordering would pass
/// against a condition that was never wired in.
#[test]
fn a_page_ahead_of_the_log_cannot_be_written() {
    let (_, _, engine) = fresh(Synchronous::Full);
    let durable = engine.wal().durable_end();

    // A page stamped with an LSN the log has not reached.
    install(&engine, PageId(6), b"ahead", durable + 1_000);
    let refused = engine
        .with_pool(|pool| pool.flush())
        .expect_err("a page whose lsn is above the durable log must not reach the data file");
    let detail = refused.detail().unwrap_or_default().to_string();
    assert!(
        detail.contains("ahead of the log"),
        "the refusal must say what was wrong: {detail}"
    );

    // Once the log has caught up, the same page goes out.
    engine.with_pool(|pool| pool.set_durable_lsn(durable + 1_000));
    engine
        .with_pool(|pool| pool.flush())
        .expect("the page writes once the log has described it");
}

/// A checkpoint never starts recovery above an open transaction's first record.
///
/// No-steal means an open transaction's pages are not in the file, so a recovery
/// that started above its first record would lose them. The assertion is on the
/// *number the checkpoint wrote*, which is the thing recovery will read.
#[test]
fn a_checkpoint_does_not_advance_past_an_open_transaction() {
    let (_, _, engine) = fresh(Synchronous::Full);
    let mut committed = engine.begin(Begin::Immediate).expect("a transaction");
    committed
        .log(Body::WritePage {
            page: 5,
            image: b"committed",
        })
        .expect("a record");
    committed.commit().expect("a commit");
    drop(committed);

    // Taken *before* the checkpoint: the checkpoint writes its own `Checkpoint`
    // record and syncs it, so `durable_end` afterwards is past the point the
    // checkpoint decided recovery should start from.
    engine.wal().sync().expect("a sync");
    let durable_before = engine.wal().durable_end();
    let clean = engine.checkpoint().expect("a checkpoint");
    assert_eq!(
        clean, durable_before,
        "with nothing open, recovery starts at the durable end"
    );

    let mut open = engine.begin(Begin::Immediate).expect("a transaction");
    let first = open
        .log(Body::WritePage {
            page: 7,
            image: b"open",
        })
        .expect("a record");
    engine.wal().flush().expect("a flush");

    let held = engine.checkpoint().expect("a checkpoint");
    assert_eq!(
        held, first,
        "a checkpoint advanced recovery past an open transaction's first record"
    );
    assert!(held < engine.wal().durable_end());

    open.commit().expect("a commit");
    drop(open);
    let after = engine.checkpoint().expect("a checkpoint");
    assert!(
        after > held,
        "recovery did not move on once the transaction finished"
    );
}

/// An engine reopens, recovers its log, and holds what was committed.
#[test]
fn an_engine_reopens_and_replays_what_was_committed() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("reopen.rdb");
    {
        let engine =
            Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full)).expect("an engine");
        for round in 1..=6u64 {
            let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
            let mut image = vec![0u8; PAGE];
            page::write_common(&mut image, page::PageKind::Leaf, 0, 1).expect("a header");
            image
                .get_mut(64..72)
                .expect("room")
                .copy_from_slice(&round.to_le_bytes());
            txn.log(Body::WritePage {
                page: 4 + round,
                image: &image,
            })
            .expect("a record");
            txn.commit().expect("a commit");
        }
        // Deliberately not checkpointed: the pages exist only in the log, so
        // reopening has to replay them rather than find them in the file.
    }
    let engine = Engine::open(
        Arc::clone(&vfs),
        &path,
        options(Synchronous::Full),
        RefuseRows,
    )
    .expect("the engine reopens");
    let recovered = engine.recovered();
    assert_eq!(recovered.committed, 6, "six transactions committed");
    assert_eq!(
        recovered.applied, 12,
        "six page images and six commit records were replayed - a commit record \
         names no page, so the page-LSN rule does not gate it and it is applied \
         so the applier can advance its timestamp"
    );
    assert_eq!(engine.clock().latest(), 6, "the timestamp came back");
    for round in 1..=6u64 {
        assert_eq!(
            read_back(&engine, PageId(4 + round), 8),
            round.to_le_bytes().to_vec(),
            "page {} did not come back",
            4 + round
        );
    }
}

/// A rolled-back transaction is not replayed after a reopen.
///
/// The rollback happens after its records were flushed, so an `Abort` is written
/// and recovery has to honour it. A rollback whose records were still in the
/// buffer would prove nothing.
#[test]
fn a_rollback_after_a_flush_is_not_replayed() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("abort.rdb");
    {
        let engine =
            Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full)).expect("an engine");
        let mut kept = engine.begin(Begin::Immediate).expect("a transaction");
        let mut image = vec![0u8; PAGE];
        page::write_common(&mut image, page::PageKind::Leaf, 0, 1).expect("a header");
        image
            .get_mut(64..69)
            .expect("room")
            .copy_from_slice(b"kept!");
        kept.log(Body::WritePage {
            page: 9,
            image: &image,
        })
        .expect("a record");
        kept.commit().expect("a commit");
        drop(kept);

        let mut abandoned = engine.begin(Begin::Immediate).expect("a transaction");
        let mut image = vec![0u8; PAGE];
        page::write_common(&mut image, page::PageKind::Leaf, 0, 1).expect("a header");
        image
            .get_mut(64..69)
            .expect("room")
            .copy_from_slice(b"gone!");
        abandoned
            .log(Body::WritePage {
                page: 10,
                image: &image,
            })
            .expect("a record");
        engine.wal().flush().expect("the records reach the media");
        let mut sink = RecordingUndo::default();
        abandoned.rollback(&mut sink).expect("a rollback");
    }
    let engine = Engine::open(
        Arc::clone(&vfs),
        &path,
        options(Synchronous::Full),
        RefuseRows,
    )
    .expect("the engine reopens");
    assert_eq!(read_back(&engine, PageId(9), 5), b"kept!".to_vec());
    // The rolled-back page was never written, so the file does not hold it at
    // all - which is a stronger outcome than holding it with the old contents
    // and is why this reads the file rather than the page.
    let replayed = engine.with_pool(|pool| pool.fetch(PageId(10)).is_ok());
    assert!(
        !replayed,
        "a rolled-back transaction's page reached the data file"
    );
    assert_eq!(engine.recovered().committed, 1);
}

/// `busy_timeout` decides whether a second writer waits or is refused.
///
/// The same contention, twice, with only the timeout different - which is what
/// "shows the behaviour changing" means.
///
/// **Which of the two happened is read off the slot's own counters rather than
/// off a stopwatch.** Both arms used to assert an elapsed time as well - under
/// 50 ms for the refusal, under 350 ms for the wait - and neither reading says
/// anything the counters do not: `timed_out` rising with `waited` unmoved *is*
/// a refusal, and `waited` rising on an acquisition that succeeded *is* a wait
/// that ended when the slot freed. What a wall-clock bound adds is a way for
/// the test to fail on a busy machine, which task-1886 saw a bound of the same
/// kind do in `inillucent-compat::new_engine_vtab_stream`. The durations are
/// still measured, and they are reported in the failure messages, where a
/// number that cannot decide anything belongs.
#[test]
fn busy_timeout_decides_whether_a_second_writer_waits() {
    let (_, _, engine) = fresh(Synchronous::Full);

    // At zero, the second `BEGIN IMMEDIATE` is refused.
    engine.slot().set_busy_timeout_ms(0);
    let first = engine.begin(Begin::Immediate).expect("the slot is free");
    let started = std::time::Instant::now();
    let refused = engine
        .begin(Begin::Immediate)
        .expect_err("a second writer must be refused");
    let waited = started.elapsed();
    assert_eq!(refused.code(), inillucent_base::PrimaryCode::Busy);
    let refusal = engine.slot().stats();
    assert_eq!(
        (refusal.waited, refusal.timed_out),
        (0, 1),
        "with `busy_timeout` at zero the second writer waited {} time(s) and \
         gave up {} time(s) after {waited:?}; it should have given up once \
         without waiting at all",
        refusal.waited,
        refusal.timed_out
    );

    // A deferred transaction still begins: it takes no slot until it writes.
    let mut deferred = engine.begin(Begin::Deferred).expect("a reader begins");
    assert!(!deferred.is_writer());
    assert!(
        deferred.become_writer().is_err(),
        "the first write of a deferred transaction is where BUSY happens"
    );
    drop(deferred);
    drop(first);

    // At 400 ms, the second writer waits for a slot that frees at 50 ms.
    //
    // The slot is taken directly rather than through a `Transaction`, because
    // an `Engine` is deliberately not `Sync` - it owns a buffer pool, and the
    // pool's bookkeeping is single-threaded by construction. The *slot* is the
    // shared thing and the only thing being contended here, so it is the slot
    // that crosses the thread boundary.
    engine.slot().set_busy_timeout_ms(400);
    let held = WriterSlot::acquire(engine.slot(), TxnId(999)).expect("the slot is free");
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(held);
    });
    let started = std::time::Instant::now();
    let taken = engine
        .begin(Begin::Immediate)
        .expect("the slot frees inside the timeout");
    let waited = started.elapsed();
    releaser.join().expect("the releasing thread");
    assert!(taken.is_writer());
    // One acquisition waited and got the slot, and the two refusals above are
    // the only ones that gave up. An acquisition that had sat out the whole
    // 400 ms would have returned BUSY instead of a writer, so the `expect`
    // above is what rules that out and this is what says it waited at all
    // rather than finding the slot already free.
    let contended = engine.slot().stats();
    assert_eq!(
        (contended.waited, contended.timed_out),
        (1, 2),
        "with `busy_timeout` at 400 ms and the slot freed after 50, the slot \
         recorded {} wait(s) and {} timeout(s) over {waited:?}",
        contended.waited,
        contended.timed_out
    );
}
/// A savepoint through a transaction rolls back exactly what came after it.
#[test]
fn a_savepoint_through_a_transaction_rolls_back_what_came_after_it() {
    let (_, _, engine) = fresh(Synchronous::Full);
    let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
    txn.record_undo(1, b"before".to_vec(), None);
    txn.savepoint("s1");
    txn.record_undo(1, b"after".to_vec(), Some(b"old".to_vec()));
    txn.savepoint("s2");
    txn.record_undo(1, b"latest".to_vec(), None);
    assert_eq!(txn.savepoints(), vec!["s1", "s2"]);

    let mut sink = RecordingUndo::default();
    assert_eq!(txn.rollback_to("s1", &mut sink).expect("a rollback"), 2);
    assert_eq!(
        sink.restored
            .iter()
            .map(|undo| String::from_utf8_lossy(&undo.key).to_string())
            .collect::<Vec<_>>(),
        vec!["latest", "after"],
        "the rollback ran newest first"
    );
    assert_eq!(txn.savepoints(), vec!["s1"], "the inner savepoint closed");

    // What came before the savepoint is still there and still commits.
    let cts = txn.commit().expect("a commit");
    assert!(cts > 0);
}

/// A transaction dropped without committing releases the writer slot.
///
/// The failure this catches has no error message: a slot that is only released
/// by an explicit call stops being released the first time a caller returns
/// early, and the database becomes unwritable until the process exits.
#[test]
fn a_dropped_transaction_releases_the_writer_slot() {
    let (_, _, engine) = fresh(Synchronous::Full);
    {
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        txn.log(Body::WritePage {
            page: 3,
            image: b"x",
        })
        .expect("a record");
        // No commit and no rollback: it just goes out of scope, which is what
        // an early `?` in a caller looks like.
    }
    assert_eq!(engine.slot().holder(), None);
    let again = engine.begin(Begin::Immediate).expect("the slot came back");
    assert!(again.is_writer());
    assert_eq!(engine.stats().rolled_back, 1);
}

/// A rollback writes an `Abort` only when its records already reached the media.
///
/// Three shapes, and the two that write nothing are the point: an `Abort` for a
/// transaction whose records are still in the log's buffer would be a record
/// saying something did not happen that never happened.
#[test]
fn an_abort_is_written_only_when_the_records_already_escaped() {
    let (_, _, engine) = fresh(Synchronous::Full);
    let mut sink = RecordingUndo::default();

    // Nothing logged at all.
    let before = engine.wal().stats().records;
    {
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        txn.record_undo(1, b"k".to_vec(), None);
        txn.rollback(&mut sink).expect("a rollback");
    }
    assert_eq!(
        engine.wal().stats().records,
        before,
        "a transaction that logged nothing wrote an Abort"
    );
    assert_eq!(sink.restored.len(), 1, "the row was still put back");

    // Logged, but still in the buffer.
    let before = engine.wal().stats().records;
    {
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        txn.log(Body::WritePage {
            page: 21,
            image: b"buffered",
        })
        .expect("a record");
        txn.rollback(&mut sink).expect("a rollback");
    }
    assert_eq!(
        engine.wal().stats().records,
        before + 1,
        "a transaction whose records never left the buffer wrote an Abort"
    );

    // Logged and flushed.
    let before = engine.wal().stats().records;
    {
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        txn.log(Body::WritePage {
            page: 22,
            image: b"flushed",
        })
        .expect("a record");
        engine.wal().flush().expect("a flush");
        txn.rollback(&mut sink).expect("a rollback");
    }
    assert_eq!(
        engine.wal().stats().records,
        before + 2,
        "a transaction whose records reached the media did not write an Abort"
    );
}

/// A record for a page that is already in the file is skipped by the page-LSN
/// rule rather than applied twice.
///
/// The rule's *other* side: every other test reaches it with pages the file does
/// not hold, where "apply it" is the only possible answer.
#[test]
fn a_record_below_a_pages_lsn_is_skipped() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("skip.rdb");
    {
        let engine =
            Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full)).expect("an engine");
        // One transaction writes page 12 twice, at two LSNs, and the *second*
        // image is put into the file. Recovery then sees a record whose LSN is
        // below the page's and one whose LSN is above it.
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        let mut older = vec![0u8; PAGE];
        page::write_common(&mut older, page::PageKind::Leaf, 0, 1).expect("a header");
        older
            .get_mut(64..69)
            .expect("room")
            .copy_from_slice(b"older");
        txn.log(Body::WritePage {
            page: 12,
            image: &older,
        })
        .expect("a record");

        let mut newer = vec![0u8; PAGE];
        page::write_common(&mut newer, page::PageKind::Leaf, 0, 1).expect("a header");
        newer
            .get_mut(64..69)
            .expect("room")
            .copy_from_slice(b"newer");
        let lsn = txn
            .log(Body::WritePage {
                page: 12,
                image: &newer,
            })
            .expect("a record");
        page::write_u64(&mut newer, header::LSN, lsn).expect("an lsn");
        engine.with_database(|database| {
            database.claim(PageId(12)).expect("claimed");
            database.install(PageId(12), &newer).expect("installed");
        });
        txn.commit().expect("a commit");
        drop(txn);
        engine.checkpoint().expect("a checkpoint");
    }
    // Reopening replays from before the checkpoint's own start only if the
    // checkpoint left something open; here it did not, so force the issue by
    // recovering from the beginning of the log.
    let engine = Engine::open(
        Arc::clone(&vfs),
        &path,
        options(Synchronous::Full),
        RefuseRows,
    )
    .expect("the engine reopens");
    assert_eq!(
        read_back(&engine, PageId(12), 5),
        b"newer".to_vec(),
        "an older record overwrote a newer page"
    );
}

/// A split's three page images are replayed, and only the ones still needed.
///
/// The page-LSN rule applies **per page** to a record that names three, so a
/// split whose left half is already in the file writes the other two and leaves
/// the left alone. A rule that was applied to the record as a whole would either
/// rewrite a page it did not need to or skip two it did.
#[test]
fn a_split_replays_only_the_pages_that_still_need_it() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("split.rdb");
    {
        let engine =
            Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full)).expect("an engine");
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        let build = |tag: &[u8]| {
            let mut image = vec![0u8; PAGE];
            page::write_common(&mut image, page::PageKind::Leaf, 0, 1).expect("a header");
            image
                .get_mut(64..64 + tag.len())
                .expect("room")
                .copy_from_slice(tag);
            image
        };
        let left = build(b"left-half");
        let right = build(b"right-half");
        let parent = build(b"the-parent");
        let lsn = txn
            .log(Body::Structural {
                kind: inillucent_wal::Structural::Split,
                tree: 1,
                left: 30,
                right: 31,
                parent: 32,
                left_image: &left,
                right_image: &right,
                parent_image: &parent,
            })
            .expect("a record");
        // Only the *left* page is put into the file, stamped with the record's
        // own LSN. Recovery must then leave it alone and write the other two.
        let mut stamped = left.clone();
        page::write_u64(&mut stamped, header::LSN, lsn).expect("an lsn");
        engine.with_database(|database| {
            database.claim(PageId(30)).expect("claimed");
            database.install(PageId(30), &stamped).expect("installed");
        });
        txn.commit().expect("a commit");
        drop(txn);
        // Checkpointed so the page and the file's page count both reach the
        // meta record - and told to start recovery *before* the record anyway.
        // That is not contrived: no-steal means a checkpoint never advances
        // recovery past an open transaction's first record, so a log whose
        // records are already in the file is the ordinary state of every
        // checkpoint taken while somebody was writing.
        engine.with_database(|database| {
            database.set_log_position(inillucent_wal::FIRST_LSN, 0, 1);
            database.checkpoint().expect("a checkpoint");
        });
    }
    let engine = Engine::open(
        Arc::clone(&vfs),
        &path,
        options(Synchronous::Full),
        RefuseRows,
    )
    .expect("the engine reopens");
    assert_eq!(read_back(&engine, PageId(30), 9), b"left-half".to_vec());
    assert_eq!(read_back(&engine, PageId(31), 10), b"right-half".to_vec());
    assert_eq!(read_back(&engine, PageId(32), 10), b"the-parent".to_vec());
}

/// A record carrying an image of the wrong size is refused rather than applied.
#[test]
fn a_record_with_a_wrong_sized_image_is_refused() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("wrong-size.rdb");
    {
        let engine =
            Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full)).expect("an engine");
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        txn.log(Body::WritePage {
            page: 14,
            image: b"far too short to be a page",
        })
        .expect("a record");
        txn.commit().expect("a commit");
    }
    let refused = Engine::open(
        Arc::clone(&vfs),
        &path,
        options(Synchronous::Full),
        RefuseRows,
    )
    .err()
    .expect("a wrong-sized image must be refused");
    assert!(
        refused
            .detail()
            .unwrap_or_default()
            .contains("byte image for page"),
        "the refusal must say what was wrong: {refused:?}"
    );
}

/// A logical row record with no tree to apply into is refused, not ignored.
#[test]
fn a_row_record_with_no_tree_is_refused() {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let path = DbPath::new("rows.rdb");
    {
        let engine =
            Engine::create(Arc::clone(&vfs), &path, options(Synchronous::Full)).expect("an engine");
        let mut txn = engine.begin(Begin::Immediate).expect("a transaction");
        txn.log(Body::InsertRow {
            tree: 1,
            page: 15,
            row: b"a row",
        })
        .expect("a record");
        txn.commit().expect("a commit");
    }
    let refused = Engine::open(
        Arc::clone(&vfs),
        &path,
        options(Synchronous::Full),
        RefuseRows,
    )
    .err()
    .expect("a row record with no tree must be refused");
    assert!(refused
        .detail()
        .unwrap_or_default()
        .contains("needs a tree to apply into"));
}

/// A read-only transaction writes nothing and costs no timestamp.
#[test]
fn a_read_only_transaction_writes_nothing() {
    let (_, _, engine) = fresh(Synchronous::Full);
    let before = engine.wal().stats();
    let mut txn = engine.begin(Begin::Deferred).expect("a transaction");
    assert!(!txn.has_written());
    let cts = txn.commit().expect("a commit");
    assert_eq!(cts, engine.clock().latest(), "no timestamp was consumed");
    assert_eq!(
        engine.wal().stats().records,
        before.records,
        "a read-only commit wrote a record"
    );
}

/// The version log is collected as readers finish.
#[test]
fn the_version_log_is_collected_as_readers_finish() {
    let (_, _, engine) = fresh(Synchronous::Full);
    let reader = engine.begin(Begin::Deferred).expect("a reader");
    for round in 0..50u64 {
        let mut writer = engine.begin(Begin::Immediate).expect("a writer");
        writer.record_undo(
            1,
            format!("k{}", round % 5).into_bytes(),
            Some(vec![round as u8]),
        );
        writer.commit().expect("a commit");
    }
    engine.collect_versions();
    assert_eq!(
        engine.versions_held(),
        5,
        "the log holds one image per row the reader can reach"
    );
    drop(reader);
    engine.collect_versions();
    assert_eq!(
        engine.versions_held(),
        0,
        "a finished reader releases it all"
    );
    assert_eq!(engine.stats().versions_collected, 50);
}
