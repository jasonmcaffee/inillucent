//! Where one row's write into a leaf goes, and the switch that keeps the
//! measuring from moving the measurement.
//!
//! Invariant: **the counters are off unless a harness asked for them, and when
//! they are on they nest the way the calls do.** `PagedTree::write_row` is the
//! path an ordinary `INSERT` and a virtual table's shadow row both reach, so a
//! default that records puts fifteen clock reads into the hottest loop the
//! engine has - and the binary that decides whether a family cleared its bar
//! would be measuring instrumented code. `module_stages.rs` asserts the same
//! thing one layer up, for the same reason and after the same measurement
//! (task-2025).
//!
//! ## Why the sums are asserted rather than the durations
//!
//! A nanosecond count is a statement about the machine and the load on it. What
//! is true on any machine is the nesting: `plan` and `place` happen inside the
//! `modify` that writes the page, which happens inside `apply_row`, which
//! happens inside one `write_row`. Those are the assertions here.
//!
//! The one number that is not a nesting is `rows`, and it is asserted against
//! the writes the statements must have made rather than against zero - §1.2 of
//! the testing standard, because a case whose loop wrote nothing asserts
//! nothing.

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_tree::stages::{record_put_stages, taken, PutStages};

/// How many rows each case writes.
///
/// Enough that a clock with a coarse tick reads something on every stage, and
/// few enough that the case is milliseconds.
const ROWS: usize = 200;

/// Returns a fresh database with an ordinary table and an FTS5 table in it.
///
/// @param tag - what to name the file
fn database(tag: &str) -> ImportedDatabase {
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("put-stages")
        .join(format!("{tag}.rdb"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&path);
    let mut database = ImportedDatabase::create(path, 32_768, 256).expect("a fresh database");
    for statement in [
        "CREATE TABLE note (id INTEGER PRIMARY KEY, label TEXT, payload BLOB)",
        "CREATE VIRTUAL TABLE documents USING fts5(title, body)",
    ] {
        database
            .execute_any(statement, &Params::new())
            .expect("the schema is created");
    }
    database
}

/// Writes `ROWS` rows into the ordinary table, one statement each.
///
/// @param database - the database to write to
fn fill_table(database: &mut ImportedDatabase) {
    for at in 0..ROWS {
        let sql =
            format!("INSERT INTO note (id, label, payload) VALUES ({at}, 'label {at}', NULL)");
        database
            .execute_any(&sql, &Params::new())
            .expect("the row is written");
    }
}

/// Writes `ROWS` documents into the FTS5 table, one statement each.
///
/// @param database - the database to write to
fn fill_documents(database: &mut ImportedDatabase) {
    for at in 0..ROWS {
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
/// that records is not a wrong number in a report; it is the write path of
/// every application paying for a measurement nobody asked for.
#[test]
fn no_stage_is_recorded_until_a_harness_asks() {
    let mut database = database("off-by-default");
    fill_table(&mut database);
    fill_documents(&mut database);
    assert_eq!(
        taken(),
        PutStages::default(),
        "the leaf write counters recorded {ROWS} ordinary rows and {ROWS} documents nobody \
         asked to be timed. Recording is switched on by `record_put_stages(true)` and by \
         nothing else: a default that records makes every gate run measure instrumented code."
    );
}

/// With the timing on, the stages nest the way the calls do.
#[test]
fn the_recorded_stages_nest_the_way_the_calls_do() {
    let mut database = database("nesting");
    record_put_stages(true);
    fill_table(&mut database);
    let stages = taken();
    record_put_stages(false);

    // One row into the table itself for each statement, and nothing else: the
    // table has no secondary index, so a row that wrote fewer leaves than it
    // had statements means writes stopped being counted.
    assert!(
        stages.rows >= ROWS as u64,
        "{ROWS} inserts wrote {} rows into leaves; each one writes at least the table's own \
         row, so anything under {ROWS} means the write path stopped being counted",
        stages.rows,
    );
    assert!(
        stages.whole > 0 && stages.apply > 0 && stages.modify > 0,
        "the write took {} ns, applying the row {} ns and writing the page {} ns; a zero here \
         is a counter that looks taken and is not",
        stages.whole,
        stages.apply,
        stages.modify,
    );
    assert!(
        stages.apply >= stages.modify,
        "`apply_row` took {} ns and the page write inside it took {} ns, which cannot be more \
         than its caller",
        stages.apply,
        stages.modify,
    );
    assert!(
        stages.modify >= stages.plan.saturating_add(stages.delta),
        "the page write took {} ns and the two stages inside it took {} ns and {} ns, which \
         together cannot be more than the `modify` they run in",
        stages.modify,
        stages.plan,
        stages.delta,
    );
    assert!(
        stages.whole
            >= stages
                .encode
                .saturating_add(stages.find)
                .saturating_add(stages.locate)
                .saturating_add(stages.room)
                .saturating_add(stages.undo)
                .saturating_add(stages.apply)
                .saturating_add(stages.making),
        "the stages named inside `write_row` add up to more than `write_row` itself took \
         ({} ns), so one of them is being counted twice",
        stages.whole,
    );
}

/// A write that made no room is counted apart from one that did.
///
/// **This is the number the ticket is about, so a report that could not
/// separate the two classes would be answering a different question.**
/// task-2025 measured 46 of 1,508 shadow row writes compacting or splitting and
/// those 46 carrying the whole of the making of room; what a write with no
/// compaction at all costs is only visible once they are taken out.
#[test]
fn the_writes_that_made_room_are_counted_apart_from_the_rest() {
    let mut database = database("making-room");
    record_put_stages(true);
    fill_documents(&mut database);
    let stages = taken();
    record_put_stages(false);

    assert!(
        stages.remade < stages.rows,
        "{} of {} writes made room, so there is no write left that did not - a 32 KiB leaf \
         holds far more than {ROWS} documents' shadow rows and the common case is supposed to \
         be the one that compacts nothing",
        stages.remade,
        stages.rows,
    );
    assert!(
        stages.remade_whole <= stages.whole,
        "the writes that made room took {} ns of a total of {} ns, which is more than the \
         total",
        stages.remade_whole,
        stages.whole,
    );
    // `making` is inside the `remade_whole` calls by construction: only a write
    // that called `make_room` has either.
    assert_eq!(
        stages.making == 0,
        stages.remade == 0,
        "{} writes made room taking {} ns; a count with no time or a time with no count means \
         the two are recorded in different places",
        stages.remade,
        stages.making,
    );
}

/// Turning the timing off stops it, so a harness can measure one workload.
#[test]
fn switching_the_timing_off_stops_the_counters() {
    let mut database = database("switch-off");
    record_put_stages(true);
    fill_table(&mut database);
    let timed = taken();
    assert!(timed.rows > 0, "the first pass was not timed");

    record_put_stages(false);
    fill_documents(&mut database);
    assert_eq!(
        taken(),
        PutStages::default(),
        "writes after `record_put_stages(false)` were still counted, so a gate that timed one \
         workload would be charging the next one for it too"
    );
}

/// A shadow row write and an ordinary row write are counted by the same counters.
///
/// **Because the claim the split is used to make is about the write path, not
/// about virtual tables.** task-2025 left two microseconds a row inside this
/// function for FTS5's shadow rows and could not say whether an ordinary
/// `INSERT` pays the same, and a counter that only moved for one of them could
/// not answer that either.
#[test]
fn an_ordinary_insert_and_a_shadow_row_are_counted_by_the_same_counters() {
    let mut database = database("both-paths");

    record_put_stages(true);
    fill_table(&mut database);
    let ordinary = taken();
    record_put_stages(false);

    record_put_stages(true);
    fill_documents(&mut database);
    let shadow = taken();
    record_put_stages(false);

    assert!(
        ordinary.rows >= ROWS as u64 && ordinary.apply > 0,
        "{ROWS} ordinary inserts were counted as {} writes taking {} ns in `apply_row`",
        ordinary.rows,
        ordinary.apply,
    );
    // `%_content` and `%_docsize`, one row each per document, plus the `%_idx`
    // rows the dictionary flush writes and the corpus totals row.
    assert!(
        shadow.rows >= 2 * ROWS as u64 && shadow.apply > 0,
        "{ROWS} documents wrote {} shadow rows taking {} ns in `apply_row`; FTS5 writes at \
         least a %_content and a %_docsize row for each one, so anything under {} means the \
         shadow write path is not reaching these counters",
        shadow.rows,
        shadow.apply,
        2 * ROWS,
    );
}
