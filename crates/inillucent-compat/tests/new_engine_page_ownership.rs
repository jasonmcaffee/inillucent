//! `PRAGMA integrity_check` over who holds a page.
//!
//! Invariant: **a detector is only worth having once it has been shown the
//! damage it looks for.** Every check here damages a real file, closes it,
//! opens it again, and asks the pragma - so what is being tested is a state of
//! the file rather than of the process that made it.
//!
//! ## Why these three states and not a torn page
//!
//! `PagedTree::check` reads one tree and `check_indexes_agree` reads a table
//! with its indexes. Both pass over every file below, because every tree in
//! every one of them is a well formed tree and no two of them are an index of
//! each other. The damage is only visible by asking which pages each tree
//! reaches and comparing the answer against the free map:
//!
//! - **two tables reachable from one page** - the state task-2043's defect
//!   produced, where `SELECT count(*) FROM p` answers with `q`'s rows;
//! - **a page a tree reaches that the free map calls free** - which is not yet
//!   damage anybody can read, and becomes the first state at the next
//!   allocation;
//! - **a page the free map calls allocated that no tree reaches** - a leak.
//!
//! **`PRAGMA integrity_check` and `PRAGMA quick_check` both report the first
//! two. The third is reached through `report_leaked_pages` instead**, because
//! the engine leaves that state behind itself from a rolled-back `CREATE` and
//! from a `DROP TABLE` of a table holding out-of-line values;
//! `crate::engine::pages` carries the measurement and the decision. The arm is
//! tested here all the same, because an arm nothing runs is an arm nobody has
//! checked.
//!
//! **The first two states are built by a hook on `ImportedDatabase` that
//! writes past the write path**, for the reason `write_index_entry_unchecked`
//! gives: no SQL statement produces either of them, which is exactly why a
//! checker for them cannot be exercised through SQL. The third needs no hook
//! for the two cases the engine produces - ordinary SQL leaves it behind, and
//! two of the tests below say so - and has one anyway, so the arm can be shown
//! a leak that is not one of those two.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// The page size and frame count every database here is created at.
const PAGE_SIZE: usize = 32_768;
/// How many frames the pool holds.
const FRAMES: usize = 4_096;

/// Returns a fresh scratch directory named after the test.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-pages-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// Returns what `PRAGMA integrity_check` answers, as one string.
///
/// @param engine - the database to ask
fn integrity(engine: &mut ImportedDatabase) -> String {
    let outcome = engine
        .execute_any("PRAGMA integrity_check", &Params::new())
        .expect("the pragma runs");
    render(outcome.rows.first().and_then(|row| row.first()))
}

/// Renders one answered value as text, naming anything that is not text.
///
/// @param value - the value the row held, if there was one
fn render(value: Option<&OwnedDatum>) -> String {
    match value {
        Some(OwnedDatum::Text(text)) => String::from_utf8_lossy(text).into_owned(),
        Some(OwnedDatum::Int(number)) => number.to_string(),
        other => format!("{other:?}"),
    }
}

/// Runs each statement, naming the one that failed.
///
/// @param engine - the database to write into
/// @param statements - the SQL to run in order
fn run(engine: &mut ImportedDatabase, statements: &[&str]) {
    for sql in statements {
        engine
            .execute_any(sql, &Params::new())
            .unwrap_or_else(|error| panic!("{sql}: {:?}", error.detail()));
    }
}

/// Returns what `report_leaked_pages` says, or `ok`.
///
/// @param engine - the database to ask
fn leaks(engine: &ImportedDatabase) -> String {
    match engine.report_leaked_pages() {
        Ok(()) => "ok".to_string(),
        Err(error) => error
            .detail()
            .unwrap_or_else(|| error.message())
            .to_string(),
    }
}

/// Returns the first value of the first row a query answers.
///
/// @param engine - the database to ask
/// @param sql - the query
fn answer(engine: &mut ImportedDatabase, sql: &str) -> String {
    let outcome = engine
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| panic!("{sql}: {:?}", error.detail()));
    render(outcome.rows.first().and_then(|row| row.first()))
}

/// Two catalog rows naming one tree is reported, and is invisible otherwise.
///
/// **The state task-2043's defect left behind.** `p`'s catalog row names a page
/// that holds `q`'s tree, so every read of `p` answers with `q`'s rows and
/// every write to `p` writes into `q`. Nothing else in the checker can see it:
/// there is one tree, it is well formed, and neither table has an index.
///
/// The count is asserted before the pragma is, because a test that only
/// asserted the pragma's text would pass over a build where the damage had
/// stopped being damage.
#[test]
fn integrity_check_reports_a_page_two_tables_both_hold() {
    let directory = scratch("two-owners");
    let path = directory.join("shared.db");
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    run(
        &mut engine,
        &[
            "CREATE TABLE p (a TEXT)",
            "INSERT INTO p VALUES ('one'),('two'),('three')",
            "CREATE TABLE q (a TEXT)",
            "INSERT INTO q VALUES ('x'),('y')",
        ],
    );
    assert_eq!(
        integrity(&mut engine),
        "ok",
        "an undamaged database was reported as damaged"
    );
    engine
        .point_table_at_unchecked("p", "q")
        .expect("the catalog row is moved");
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);

    let mut reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    // The damage, stated as what a reader sees: `p` had three rows and answers
    // with `q`'s two, and every one of its own is gone.
    assert_eq!(
        answer(&mut reopened, "SELECT count(*) FROM p"),
        "2",
        "the damage did not survive the reopen, so the pragma below proves nothing"
    );
    let said = integrity(&mut reopened);
    assert!(
        said.contains("is used by table p and also by table q"),
        "a page two tables both hold was not reported: {said}"
    );
}

/// A page the free map calls allocated that no tree reaches is reported.
///
/// **Through `report_leaked_pages`, not through the pragma.** The engine leaves
/// that state behind itself - the two cases are named in `crate::engine::pages`
/// and measured in `a_drop_leaves_its_out_of_line_values_pages_behind` below -
/// so `PRAGMA integrity_check` would call a sound file damaged. The arm is here
/// because a detector nobody has run against the damage is a detector nobody
/// has tested.
///
/// The wording is the pinned SQLite 3.53.4's, read out of its source: it writes
/// `Page %u: never used`, where the older form this ticket quoted was `Page N
/// is never used`.
#[test]
fn the_leak_report_names_a_page_no_tree_reaches() {
    let directory = scratch("never-used");
    let path = directory.join("leaked.db");
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    run(
        &mut engine,
        &[
            "CREATE TABLE t (a TEXT, b INTEGER)",
            "INSERT INTO t VALUES ('p',1),('q',2),('r',3)",
        ],
    );
    assert_eq!(integrity(&mut engine), "ok");
    assert!(
        engine.report_leaked_pages().is_ok(),
        "a file with nothing leaked was reported as leaking"
    );
    let stranded = engine
        .strand_a_page_unchecked()
        .expect("a page is taken out of the free map");
    assert_eq!(
        leaks(&engine),
        format!("Page {stranded}: never used"),
        "a page nothing reaches was not reported"
    );
    // And the pragma still answers `ok`, which is the decision this ticket
    // recorded rather than an oversight.
    assert_eq!(
        integrity(&mut engine),
        "ok",
        "integrity_check reported a leak, which it is not yet meant to"
    );
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);

    let mut reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(
        leaks(&reopened),
        format!("Page {stranded}: never used"),
        "the leak was not visible after a reopen"
    );
    // And the rows are all still there, which is what makes this a leak rather
    // than a loss - and why nothing but this check can see it.
    assert_eq!(answer(&mut reopened, "SELECT count(*) FROM t"), "3");
}

/// `DROP TABLE` leaves the pages its out-of-line values sat on behind.
///
/// **A leak the engine has, recorded so that closing it fails this test rather
/// than passing silently.** `release_tree` gives back the tree's interior pages
/// and its leaves; `paged::free_extent` is reached only from the tree's own
/// write paths, so a tree released whole never reaches it. `DELETE FROM t`
/// first does give the space back, which is the second half below and is what
/// makes this a gap in one path rather than in the format.
///
/// It is written as an assertion on the leak rather than a `#[ignore]`, because
/// a test that does not run is a test that says nothing about either state.
#[test]
fn a_drop_leaves_its_out_of_line_values_pages_behind() {
    let directory = scratch("drop-extents");
    let body = "y".repeat(PAGE_SIZE / 4);

    let dropped = directory.join("dropped.db");
    let mut engine =
        ImportedDatabase::create(dropped, PAGE_SIZE, FRAMES).expect("a fresh database");
    run(&mut engine, &["CREATE TABLE t (a TEXT)"]);
    run(&mut engine, &[&format!("INSERT INTO t VALUES ('{body}')")]);
    run(&mut engine, &["DROP TABLE t"]);
    assert_eq!(
        integrity(&mut engine),
        "ok",
        "the pragma reported a leak, which it is not yet meant to"
    );
    assert!(
        leaks(&engine).contains(": never used"),
        "DROP TABLE no longer leaks its extents, so task-2065 has landed: wire the leak \n         arm to the pragma and turn this test round: {}",
        leaks(&engine)
    );

    let emptied = directory.join("emptied.db");
    let mut engine =
        ImportedDatabase::create(emptied, PAGE_SIZE, FRAMES).expect("a fresh database");
    run(&mut engine, &["CREATE TABLE t (a TEXT)"]);
    run(&mut engine, &[&format!("INSERT INTO t VALUES ('{body}')")]);
    run(&mut engine, &["DELETE FROM t", "DROP TABLE t"]);
    assert!(
        engine.report_leaked_pages().is_ok(),
        "deleting the rows before the drop did not give the extent pages back: {}",
        leaks(&engine)
    );
}

/// A rolled-back `CREATE TABLE` leaves its tree's root page behind.
///
/// **The other leak, and the one the ticket predicted.** It is invisible on the
/// connection that did it: the rollback leaves the tree's handle in
/// `schema.trees`, so the walk still reaches the page. It appears after a
/// checkpoint and a reopen, when the schema is rebuilt from the catalog and
/// nothing names that tree.
#[test]
fn a_rolled_back_create_table_leaves_its_root_page_behind() {
    let directory = scratch("rollback-leak");
    let path = directory.join("undone.db");
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    run(
        &mut engine,
        &[
            "CREATE TABLE kept (a TEXT)",
            "INSERT INTO kept VALUES ('one')",
            "BEGIN",
            "CREATE TABLE gone (a TEXT)",
            "INSERT INTO gone VALUES ('x'),('y')",
            "ROLLBACK",
        ],
    );
    assert!(
        engine.report_leaked_pages().is_ok(),
        "the leak is visible on the connection that made it, which the reopen below is \n         written on the assumption that it is not: {}",
        leaks(&engine)
    );
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);

    let mut reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(
        integrity(&mut reopened),
        "ok",
        "the pragma reported a leak, which it is not yet meant to"
    );
    assert!(
        leaks(&reopened).contains(": never used"),
        "a rolled-back CREATE TABLE no longer leaks its root page, so task-2065 has \n         landed: wire the leak arm to the pragma and turn this test round: {}",
        leaks(&reopened)
    );
}

/// A page a tree reaches that the free map calls free is reported.
///
/// **The state that becomes a page two tables both hold.** Every read still
/// answers correctly, so there is nothing to see in any tree; the next
/// allocation hands the page to a second owner and the rows on it are gone.
#[test]
fn integrity_check_reports_a_live_page_the_free_map_gave_back() {
    let directory = scratch("handed-back");
    let path = directory.join("loose.db");
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    run(
        &mut engine,
        &[
            "CREATE TABLE t (a TEXT)",
            "INSERT INTO t VALUES ('one'),('two')",
        ],
    );
    assert_eq!(integrity(&mut engine), "ok");
    let loose = engine
        .free_root_page_unchecked("t")
        .expect("the root page is given back");
    assert_eq!(
        integrity(&mut engine),
        format!("page {loose} is used by table t but the free map says it is free"),
        "a live page the map had handed back was not reported"
    );
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);

    let mut reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(
        integrity(&mut reopened),
        format!("page {loose} is used by table t but the free map says it is free"),
        "the disagreement was not visible after a reopen"
    );
    assert_eq!(answer(&mut reopened, "SELECT count(*) FROM t"), "2");
}

/// A database that has had pages moved about answers `ok`.
///
/// **The other half of a detector, and the half that is usually missing.** A
/// check that reported damage over a healthy file would be worse than no check,
/// because every suite in the tree runs `PRAGMA integrity_check`. The workload
/// is chosen to move pages rather than to be large: an index built and dropped,
/// a table dropped, values too large to sit in a leaf, and rows deleted so the
/// free map has handed pages back and taken them again.
#[test]
fn a_database_that_has_moved_its_pages_about_still_answers_ok() {
    let directory = scratch("healthy");
    let path = directory.join("busy.db");
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    run(
        &mut engine,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, body TEXT)",
            "CREATE TABLE spare (a TEXT)",
            "INSERT INTO spare VALUES ('one'),('two')",
        ],
    );
    for id in 1..=40 {
        let body = "x".repeat(id * 400);
        run(
            &mut engine,
            &[&format!(
                "INSERT INTO t VALUES ({id}, 'row {id}', '{body}')"
            )],
        );
    }
    // A whole table of out-of-line values, dropped. Its leaves go back to the
    // free map and so must the pages its values sit on - a page size over the
    // spill threshold is out of line, and 40 rows of them is most of this file.
    run(&mut engine, &["CREATE TABLE bulky (a TEXT)"]);
    for row in 0..40 {
        let body = "y".repeat(PAGE_SIZE / 4 + row);
        run(
            &mut engine,
            &[&format!("INSERT INTO bulky VALUES ('{body}')")],
        );
    }
    run(
        &mut engine,
        &[
            "CREATE INDEX t_name ON t (name)",
            "DELETE FROM t WHERE id % 3 = 0",
            "DROP INDEX t_name",
            "DROP TABLE spare",
            "DROP TABLE bulky",
            "CREATE TABLE after (a TEXT)",
            "INSERT INTO after VALUES ('p'),('q'),('r')",
            "UPDATE t SET body = 'short' WHERE id % 2 = 0",
        ],
    );
    assert_eq!(
        integrity(&mut engine),
        "ok",
        "a healthy file was reported as damaged"
    );
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);

    let mut reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(
        integrity(&mut reopened),
        "ok",
        "a healthy file was reported as damaged after a reopen"
    );
}

/// The page walk is not what `quick_check` is not paying for.
///
/// **The measurement the split between the two pragmas rests on, and it went
/// the other way from the guess.** The obvious reading is that accounting for
/// every page of a file is the expensive half. It is not: the walk reads a
/// tree's interior pages and its leaves, and it takes an out-of-line value's
/// pages from the reference in the leaf it is already holding rather than by
/// reading the value. Over a table of sixty out-of-line values and no index it
/// costs a few fetches on top of the tree walk, so `quick_check` does it.
///
/// The expensive half is the index pass, which walks each index and the table
/// it is on and merges them - so that is what `quick_check` leaves out, which
/// is also where the pinned SQLite draws its line.
///
/// Counted in page fetches off the pool rather than timed, so the number is the
/// same on every machine and the test cannot pass by being run on a fast one.
/// Both bounds are loose: what they defend is which half is the expensive one,
/// and a build where that has changed is one where the split needs rereading.
#[test]
fn the_page_walk_is_not_what_quick_check_is_not_paying_for() {
    let directory = scratch("cost");

    // No index, so `quick_check` and `integrity_check` do the same work and
    // the only thing between them is the page walk.
    let bulky = directory.join("bulky.db");
    let mut engine = ImportedDatabase::create(bulky, PAGE_SIZE, FRAMES).expect("a fresh database");
    run(&mut engine, &["CREATE TABLE t (a TEXT)"]);
    for row in 0..60 {
        let body = "y".repeat(PAGE_SIZE / 4 + row);
        run(&mut engine, &[&format!("INSERT INTO t VALUES ('{body}')")]);
    }
    let without_the_walk = fetches(&mut engine, "PRAGMA quick_check");
    let with_the_walk = fetches(&mut engine, "PRAGMA integrity_check");
    assert!(
        with_the_walk <= without_the_walk.saturating_mul(3) / 2,
        "the page walk fetched {with_the_walk} pages against {without_the_walk} for the          tree walk alone, so it is no longer the cheap half and the split between the          two pragmas needs rereading"
    );

    // And with indexes, where the pass `quick_check` leaves out is the one that
    // reads the file twice more.
    let indexed = directory.join("indexed.db");
    let mut engine =
        ImportedDatabase::create(indexed, PAGE_SIZE, FRAMES).expect("a fresh database");
    run(
        &mut engine,
        &[
            "CREATE TABLE m (id INTEGER PRIMARY KEY, email TEXT, team TEXT)",
            "CREATE UNIQUE INDEX m_email ON m (email)",
            "CREATE INDEX m_team ON m (team)",
        ],
    );
    for row in 0..400 {
        run(
            &mut engine,
            &[&format!(
                "INSERT INTO m VALUES ({row}, 'p{row}@example.test', 'team {}')",
                row % 7
            )],
        );
    }
    let quick = fetches(&mut engine, "PRAGMA quick_check");
    let full = fetches(&mut engine, "PRAGMA integrity_check");
    assert!(
        full > quick,
        "integrity_check fetched {full} pages and quick_check {quick}, so the index pass          is no longer the expensive half and the split needs rereading"
    );
}

/// Returns how many pages the pool was asked for while a statement ran.
///
/// @param engine - the database to run it on
/// @param sql - the statement
fn fetches(engine: &mut ImportedDatabase, sql: &str) -> u64 {
    let before = engine.pool_stats();
    run(engine, &[sql]);
    let after = engine.pool_stats();
    after
        .hits
        .saturating_add(after.misses)
        .saturating_sub(before.hits.saturating_add(before.misses))
}
