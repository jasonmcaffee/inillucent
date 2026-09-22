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
//! **`PRAGMA integrity_check` and `PRAGMA quick_check` report all three
//! (task-2065).** The third was held back while the engine produced that state
//! itself, from a rolled-back `CREATE` and from a `DROP TABLE` of a table
//! holding out-of-line values; both are closed, and the two tests that recorded
//! them now assert the pages come back. `report_leaked_pages` is still here as
//! a way to ask for that one state on its own.
//!
//! **Every one of the three is built by a hook on `ImportedDatabase` that
//! writes past the write path**, for the reason `write_index_entry_unchecked`
//! gives: no SQL statement produces any of them, which is exactly why a checker
//! for them cannot be exercised through SQL. The third needed no hook while the
//! two leaks were open, and needs one now that they are closed - which is the
//! sense in which closing them made this file's job harder and the engine's
//! answers better.

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
/// **Through the pragma and through `report_leaked_pages`, which now answer the
/// same thing (task-2065).** While the engine produced this state itself the
/// pragma had to stay quiet about it or it would have called a sound file
/// damaged; the two statements that did so are fixed, and the two tests below
/// assert as much. The state still has to be built by hand, because no
/// statement produces it any more.
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
    // And the pragma says the same thing, which is what task-2065 wired up.
    assert_eq!(
        integrity(&mut engine),
        format!("Page {stranded}: never used"),
        "integrity_check did not report the leak the arm beside it reports"
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

/// `DROP TABLE` gives back the pages its out-of-line values sat on.
///
/// **The leak task-2052 measured and task-2065 closed, now asserted the other
/// way round.** `release_tree` used to record the tree's interior pages and its
/// leaves and nothing else, and `paged::free_extent` is reached only from the
/// tree's own write paths, so a tree released whole never reached it - the
/// space came back only from `DELETE FROM t` first, or from a `VACUUM`. The
/// walk now answers both halves and the commit frees the values through
/// `free_extent`.
///
/// Three values, each on the other side of a threshold the format cares about:
/// one packed into a shared page, one a run of whole pages, and one big enough
/// to need several. A fix that handled only runs would pass with the middle one
/// alone, which is why the small one is here.
///
/// The `DELETE FROM t` half is kept because it was the path that always worked,
/// so it is the one that says the fix did not get there by breaking it.
#[test]
fn a_drop_gives_back_its_out_of_line_values_pages() {
    let directory = scratch("drop-extents");
    // Under the spill threshold's page-sized values but over the packing one,
    // so this lands in a slot of a shared page rather than on pages of its own.
    let packed = "s".repeat(PAGE_SIZE / 8);
    let body = "y".repeat(PAGE_SIZE / 4);
    let huge = "z".repeat(PAGE_SIZE * 3);

    let dropped = directory.join("dropped.db");
    let mut engine =
        ImportedDatabase::create(dropped.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    run(&mut engine, &["CREATE TABLE t (a TEXT)"]);
    for value in [&packed, &body, &huge] {
        run(&mut engine, &[&format!("INSERT INTO t VALUES ('{value}')")]);
    }
    run(&mut engine, &["DROP TABLE t"]);
    assert_eq!(
        integrity(&mut engine),
        "ok",
        "DROP TABLE left a page the free map holds and no tree reaches"
    );
    assert!(
        engine.report_leaked_pages().is_ok(),
        "DROP TABLE kept its out-of-line values' pages: {}",
        leaks(&engine)
    );
    // **And it survives the reopen**, which is where the state is a fact about
    // the file rather than about this connection's free map.
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);
    let mut reopened = ImportedDatabase::open(dropped, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(
        integrity(&mut reopened),
        "ok",
        "the pages came back in memory and not in the file"
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

/// A dropped table's packed value does not take another table's value with it.
///
/// **The case that makes the fix more than a one-line change.** A value small
/// enough to be packed sits in a slot of a page that holds small values from
/// whichever trees wrote them, because the hint that finds such a page is on
/// the `Database` and not on the tree. Giving that page back because one of the
/// trees using it was dropped would free a page another table's value is still
/// on - and the next allocation would hand it out and write over a live value.
///
/// So the assertion is on the surviving value's bytes, not only on the pragma:
/// a page freed too early is still readable until something else takes it, and
/// a test that only asked the pragma would pass over exactly that.
#[test]
fn dropping_one_table_leaves_another_tables_packed_value_alone() {
    let directory = scratch("shared-slot");
    let path = directory.join("shared-extent.db");
    let packed = "s".repeat(PAGE_SIZE / 8);
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    // Interleaved, so both tables' values land on the same shared page: the
    // hint moves to the page the last small value went on, so writing one each
    // in turn is what puts them together.
    run(
        &mut engine,
        &[
            "CREATE TABLE keeper (a TEXT)",
            "CREATE TABLE goner (a TEXT)",
        ],
    );
    run(
        &mut engine,
        &[
            &format!("INSERT INTO goner VALUES ('{packed}')"),
            &format!("INSERT INTO keeper VALUES ('{packed}')"),
        ],
    );
    run(&mut engine, &["DROP TABLE goner"]);
    assert_eq!(
        integrity(&mut engine),
        "ok",
        "dropping one of two tables sharing an extent page damaged the file"
    );
    // The survivor still reads back, and reads back whole.
    assert_eq!(
        answer(&mut engine, "SELECT length(a) FROM keeper"),
        packed.len().to_string(),
        "the dropped table took the surviving table's value with it"
    );
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);

    let mut reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(integrity(&mut reopened), "ok");
    assert_eq!(
        answer(&mut reopened, "SELECT a FROM keeper"),
        packed,
        "the surviving value did not come back byte for byte after a reopen"
    );
}

/// A rolled-back `CREATE` gives its tree's pages back.
///
/// **The other leak, closed by task-2065, and the reopen is the whole test.**
/// It was always invisible on the connection that did it: the rollback leaves
/// the tree's handle in `schema.trees`, so the walk still reaches the page. It
/// appeared after a checkpoint and a reopen, when the schema is rebuilt from
/// the catalog and nothing names that tree - so a version of this that only
/// asked the live connection passed while the leak was open, and proves
/// nothing now.
///
/// `CREATE INDEX` is here beside `CREATE TABLE` because it is the other
/// statement that builds a tree, and a bulk build allocates more than a root
/// page - so it is the one that says the fix gives back every page of the tree
/// rather than the one the catalog row names.
#[test]
fn a_rolled_back_create_gives_its_pages_back() {
    let directory = scratch("rollback-leak");
    let path = directory.join("undone.db");
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    run(
        &mut engine,
        &[
            "CREATE TABLE kept (id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO kept VALUES (1, 'one')",
        ],
    );
    for row in 2..=200 {
        run(
            &mut engine,
            &[&format!("INSERT INTO kept VALUES ({row}, 'row {row}')")],
        );
    }
    run(
        &mut engine,
        &[
            "BEGIN",
            "CREATE TABLE gone (a TEXT)",
            "INSERT INTO gone VALUES ('x'),('y')",
            "CREATE INDEX kept_name ON kept (name)",
            "ROLLBACK",
        ],
    );
    assert_eq!(
        integrity(&mut engine),
        "ok",
        "the rollback left the connection holding a page nothing reaches"
    );
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);

    let mut reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(
        integrity(&mut reopened),
        "ok",
        "a rolled-back CREATE kept pages the reopened file cannot reach: {}",
        leaks(&reopened)
    );
    assert!(
        reopened.report_leaked_pages().is_ok(),
        "a rolled-back CREATE kept its tree's pages: {}",
        leaks(&reopened)
    );
    // The transaction was abandoned, so neither object exists - and the table
    // the transaction did not touch still reads.
    assert_eq!(answer(&mut reopened, "SELECT count(*) FROM kept"), "200");
}

/// A `CREATE` and a `DROP` in one abandoned transaction both come back.
///
/// **The case the two lists have to be told apart for.** After the undo, both
/// leave a handle the schema no longer names, and they want opposite things:
/// `built`'s pages were allocated by a transaction that is not going to commit,
/// so they are freed, and `dropped`'s pages belong to a commit that never
/// happens, so they are not. Getting that backwards is task-2043 again - it
/// would free the pages of a table the rollback has just put back.
///
/// So this asserts the rolled-back `DROP`'s table still reads its rows, which
/// is what "not freed" means from outside, as well as the pragma.
#[test]
fn an_abandoned_transaction_tells_what_it_built_from_what_it_dropped() {
    let directory = scratch("built-and-dropped");
    let path = directory.join("both.db");
    let body = "y".repeat(PAGE_SIZE / 4);
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    run(&mut engine, &["CREATE TABLE standing (a TEXT)"]);
    run(
        &mut engine,
        &[&format!("INSERT INTO standing VALUES ('{body}')")],
    );
    run(
        &mut engine,
        &[
            "BEGIN",
            // Dropped: its pages go on the list the commit drains, and the
            // rollback has to leave them exactly where they are.
            "DROP TABLE standing",
            // Built: its pages were never committed, so the rollback frees
            // them.
            "CREATE TABLE fresh (a TEXT)",
        ],
    );
    run(
        &mut engine,
        &[&format!("INSERT INTO fresh VALUES ('{body}')")],
    );
    run(&mut engine, &["ROLLBACK"]);
    assert_eq!(
        integrity(&mut engine),
        "ok",
        "the rollback got one of the two lists wrong"
    );
    // The dropped table is back, with the value that was out of line in it -
    // which is what says its extent pages were not handed to the free map.
    assert_eq!(
        answer(&mut engine, "SELECT length(a) FROM standing"),
        body.len().to_string(),
        "the rolled-back DROP lost the table's out-of-line value"
    );
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);

    let mut reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(integrity(&mut reopened), "ok");
    assert_eq!(
        answer(&mut reopened, "SELECT a FROM standing"),
        body,
        "the rolled-back DROP's value did not survive the reopen"
    );
}

/// `REINDEX` gives back the tree it replaced, and is then read from.
///
/// **The third leak, and this ticket did not go looking for it - wiring the
/// leak arm to the pragma is what produced it.** `rebuild_index` allocates a
/// new handle, builds the replacement into it and rewrites the catalog row to
/// name it; nothing released the tree it replaced, so every `REINDEX` left one
/// tree's worth of pages behind.
///
/// **The second assertion is the one that matters more.** Releasing the old
/// tree turned up why nobody had noticed: `rewrite` leaves `Recorded::root`
/// alone, so the recorded *handle* went on naming the tree the statement had
/// just replaced, and the connection went on reading it. The rows matched, so
/// the only symptom was the leak. A test that checked the pragma alone would
/// pass over a build where the pages came back and the reads still went to the
/// tree that is no longer there - which is a crash, not a wrong answer, but it
/// is not what this fixed either.
///
/// Ten rebuilds rather than one, because a leak of one page per `REINDEX` is
/// what this was: a single rebuild would leave a file whose growth could be
/// argued about.
#[test]
fn a_reindex_gives_back_the_tree_it_replaced() {
    let directory = scratch("reindex");
    let path = directory.join("rebuilt.db");
    let mut engine =
        ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
    run(
        &mut engine,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL, w INTEGER)",
            "CREATE INDEX t_by_w ON t (w)",
        ],
    );
    for row in 1..=300 {
        run(
            &mut engine,
            &[&format!(
                "INSERT INTO t VALUES ({row}, 'row {row}', {})",
                row % 17
            )],
        );
    }
    assert_eq!(integrity(&mut engine), "ok");
    // What the index answers before any rebuild, which every rebuild below has
    // to keep answering.
    let expected = answer(&mut engine, "SELECT count(*) FROM t WHERE w = 3");
    assert_ne!(
        expected, "0",
        "the fixture indexes nothing, so it proves nothing"
    );

    for round in 1..=10 {
        run(&mut engine, &["REINDEX"]);
        assert_eq!(
            integrity(&mut engine),
            "ok",
            "REINDEX left a page the free map holds and no tree reaches, at round {round}"
        );
        // Read through the index it has just rebuilt. Before this ticket the
        // recorded handle still named the *old* tree, so this read went to the
        // tree REINDEX was supposed to have replaced.
        assert_eq!(
            answer(&mut engine, "SELECT count(*) FROM t WHERE w = 3"),
            expected,
            "the rebuilt index answers differently at round {round}"
        );
    }
    engine.checkpoint().expect("the database checkpoints");
    drop(engine);

    let mut reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(
        integrity(&mut reopened),
        "ok",
        "ten rebuilds left the reopened file holding pages nothing reaches"
    );
    assert_eq!(
        answer(&mut reopened, "SELECT count(*) FROM t WHERE w = 3"),
        expected,
        "the rebuilt index does not survive a reopen"
    );
    assert_eq!(answer(&mut reopened, "SELECT count(*) FROM t"), "300");
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
    // **This workload is the one that used to leak**, and since task-2065 the
    // `ok` above covers that too: it drops a table of forty out-of-line values
    // and an index, so before the fix the pragma would have named one of their
    // pages the moment the leak arm was wired on.
    assert!(
        reopened.report_leaked_pages().is_ok(),
        "the workload left a page the free map holds and no tree reaches: {}",
        leaks(&reopened)
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
