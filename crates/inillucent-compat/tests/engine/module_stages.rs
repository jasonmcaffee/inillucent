//! Where a write into a virtual table spends its time, and the switch that
//! keeps the measuring from changing the measurement.
//!
//! Invariant: **the counters are off unless a harness asked for them, and when
//! they are on they add up.** Both halves are load bearing and the first one is
//! the one that will be broken by accident: `ModuleStages` times every shadow
//! row write and every pass through the insert arm, and always-on it took
//! `extension.rtree.insert` from a paired 1.29x to 1.10x over two gate runs each
//! (task-2025). The gate is what decides whether a bar is met, so a default that
//! records is a gate measuring instrumented code and publishing the number as
//! the engine's.
//!
//! ## Why the sums are asserted rather than the durations
//!
//! A nanosecond count is a statement about the machine and the load on it. What
//! is true on any machine is the **nesting**: `update` happens inside `change`,
//! which happens inside one pass of the arm, so `whole >= change >= update`
//! whatever the box is doing. Those are the assertions here, and the one
//! proportion among them - that the module's own `update` is the majority of the
//! arm's time - is safe for the same reason, because `update` is a part of
//! `whole` rather than a rival measurement of it.
//!
//! This is what `extension.fts.build`'s ticket got wrong and what these
//! counters exist to answer: the engine's virtual table write path was assumed
//! to be 8.4 us a document of a 16 us workload, and measured it is 0.45 us.

use inillucent_compat::newengine::{ImportedDatabase, ModuleStages};
use inillucent_exec::physical::Params;

/// How many documents each case writes.
///
/// Enough that a clock with a coarse tick still reads something on every stage,
/// and few enough that the case is milliseconds. §1.2 of the testing standard:
/// a loop that asserts nothing because its input was empty has not tested
/// anything, so `rows` is asserted against this number rather than against zero.
const DOCUMENTS: usize = 200;

/// The page sizes every case here runs at.
///
/// **4,096 is here because it used to be impossible (task-2033).** This suite
/// was written at 4,096 and had to be moved to 32,768 to pass, because two
/// hundred FTS5 documents at a 4,096-byte page answered `SQLITE_CORRUPT` on
/// row 42 - `LeafMut::room_for` costed a row and the tombstone bitmap
/// separately, and 32,768 was simply large enough that two hundred documents
/// never reached a leaf tight enough to show it. Running at both is what says
/// the workaround is no longer needed, and it is the page size an embedder who
/// picks SQLite's own default gets.
const PAGE_SIZES: [usize; 2] = [4_096, 32_768];

/// Returns a fresh database with an FTS5 table and an R-Tree table in it.
///
/// @param tag - what to name the file
/// @param page_size - the page size to create it with
fn database(tag: &str, page_size: usize) -> ImportedDatabase {
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("module-stages")
        .join(format!("{tag}-{page_size}.rdb"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&path);
    let mut database = ImportedDatabase::create(path, page_size, 256).expect("a fresh database");
    for statement in [
        "CREATE VIRTUAL TABLE documents USING fts5(title, body)",
        "CREATE VIRTUAL TABLE boxes USING rtree(id, minX, maxX, minY, maxY)",
    ] {
        database
            .execute_any(statement, &Params::new())
            .expect("the module connects");
    }
    database
}

/// Writes `DOCUMENTS` documents into the FTS5 table, one statement each.
///
/// @param database - the database to write to
fn fill_documents(database: &mut ImportedDatabase) {
    for at in 0..DOCUMENTS {
        let sql = format!(
            "INSERT INTO documents(title, body) VALUES ('note {at}', \
             'lorem ipsum dolor sit amet number {at} consectetur adipiscing elit')"
        );
        database
            .execute_any(&sql, &Params::new())
            .expect("the document is indexed");
    }
}

/// Nothing is timed until a harness turns the timing on.
///
/// **The assertion that protects every ratio the gate publishes.** A default
/// that records is not a wrong number in a report, it is the write path of every
/// application paying for a measurement nobody asked for, in the binary that
/// says whether the `extension` family cleared its bar.
#[test]
fn no_stage_is_recorded_until_a_harness_asks() {
    for page_size in PAGE_SIZES {
        let mut database = database("off-by-default", page_size);
        fill_documents(&mut database);
        let stages = database.module_stage_nanos();
        assert_eq!(
            stages,
            ModuleStages::default(),
            "at a {page_size}-byte page the stage counters recorded {DOCUMENTS} documents nobody asked \
             timed. Recording is switched on by `record_module_stages(true)` and by nothing else: \
             a default that records makes every gate run measure instrumented code."
        );
    }
}

/// With the timing on, the stages nest the way the calls do.
#[test]
fn the_recorded_stages_nest_the_way_the_calls_do() {
    for page_size in PAGE_SIZES {
        let mut database = database("nesting", page_size);
        database.record_module_stages(true);
        fill_documents(&mut database);
        let stages = database.module_stage_nanos();
        database.record_module_stages(false);

        assert_eq!(
            stages.rows, DOCUMENTS as u64,
            "at a {page_size}-byte page the insert arm reported {} rows for {DOCUMENTS} inserts",
            stages.rows
        );
        // `%_content` and `%_docsize`, one row each per document, plus the `%_idx`
        // rows the dictionary flush writes and the corpus totals row. Two a document
        // is the floor and the flush only adds to it.
        assert!(
            stages.shadow_writes >= 2 * DOCUMENTS as u64,
            "{DOCUMENTS} documents wrote {} shadow rows; FTS5 writes at least a %_content and a \
             %_docsize row for each one, so anything under {} means the shadow write path stopped \
             being counted",
            stages.shadow_writes,
            2 * DOCUMENTS
        );
        assert!(
            stages.update > 0 && stages.put > 0,
            "the module's own update took {} ns and the tree writes took {} ns; a zero here is a \
             counter that looks taken and is not",
            stages.update,
            stages.put
        );
        assert!(
            stages.change >= stages.update,
            "change_module took {} ns and the module's update inside it took {} ns, which cannot \
             be more than its caller",
            stages.change,
            stages.update
        );
        assert!(
            stages.whole >= stages.change,
            "one pass of the insert arm took {} ns and the change_module inside it took {} ns, \
             which cannot be more than its caller",
            stages.whole,
            stages.change
        );
        assert!(
            stages.whole >= stages.values,
            "one pass of the insert arm took {} ns and building the row inside it took {} ns",
            stages.whole,
            stages.values
        );
    }
}

/// The module's own work is the majority of what a virtual table insert costs.
///
/// **This is the ticket's finding, written as a test that fails if it stops
/// being true.** The engine's arm above the module - the owned copy of the row,
/// `change_module`'s per-row `WalLog`, `WriteStore` and `Context` - was believed
/// to be half of `extension.fts.build` and measured 0.45 us of 12.6. A change
/// that puts real work back into that arm turns this red and names it, which is
/// what the measurement was for.
#[test]
fn the_module_is_the_majority_of_what_the_insert_arm_costs() {
    for page_size in PAGE_SIZES {
        let mut database = database("majority", page_size);
        database.record_module_stages(true);
        fill_documents(&mut database);
        let stages = database.module_stage_nanos();
        database.record_module_stages(false);

        assert!(
            stages.whole > 0,
            "at a {page_size}-byte page the insert arm was not timed at all"
        );
        assert!(
            stages.update.saturating_mul(2) >= stages.whole,
            "the module's own update was {} ns of the insert arm's {} ns - under half - so the \
             engine's own virtual table path has become the cost. It was 96% the module's when \
             this was written. `values` was {} ns and change_module's plumbing {} ns.",
            stages.update,
            stages.whole,
            stages.values,
            stages.change.saturating_sub(stages.update),
        );
    }
}

/// Turning the timing off stops it, so a harness can measure one workload.
#[test]
fn switching_the_timing_off_stops_the_counters() {
    for page_size in PAGE_SIZES {
        let mut database = database("switch-off", page_size);
        database.record_module_stages(true);
        fill_documents(&mut database);
        let timed = database.module_stage_nanos();
        assert_eq!(
            timed.rows, DOCUMENTS as u64,
            "at a {page_size}-byte page the first pass was not timed"
        );

        database.record_module_stages(false);
        fill_documents(&mut database);
        assert_eq!(
            database.module_stage_nanos(),
            ModuleStages::default(),
            "writes after `record_module_stages(false)` were still counted, so a gate that timed \
             one workload would be charging the next one for it too"
        );
    }
}

/// Every module's writes are counted, not only FTS5's.
///
/// **Because the path is shared and the claim about it is general.** The arm and
/// `change_module` are what every `INSERT` into a virtual table goes through, so
/// a counter that only moved for fts5 would be measuring fts5 rather than the
/// path - and `extension.rtree.insert` is the other workload the numbers were
/// taken for.
#[test]
fn an_rtree_insert_is_counted_by_the_same_counters() {
    for page_size in PAGE_SIZES {
        let mut database = database("rtree", page_size);
        database.record_module_stages(true);
        for at in 0..DOCUMENTS as i64 {
            let sql = format!(
                "INSERT INTO boxes(id, minX, maxX, minY, maxY) \
                 VALUES ({at}, {at}, {at} + 10, {at}, {at} + 10)"
            );
            database
                .execute_any(&sql, &Params::new())
                .expect("the box is indexed");
        }
        let stages = database.module_stage_nanos();
        database.record_module_stages(false);

        assert_eq!(
            stages.rows, DOCUMENTS as u64,
            "at a {page_size}-byte page the insert arm reported {} rows for {DOCUMENTS} R-Tree inserts",
            stages.rows
        );
        assert!(
            stages.shadow_writes > 0 && stages.put > 0,
            "the R-Tree wrote {} shadow rows taking {} ns; it keeps its nodes in shadow tables \
             like every other module, so a zero means only FTS5 is being counted",
            stages.shadow_writes,
            stages.put
        );
    }
}
