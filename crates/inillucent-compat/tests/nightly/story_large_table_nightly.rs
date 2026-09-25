//! One table an order of magnitude past the buffer pool, graded the way the
//! small ones are, at the engine's own page size and at SQLite's.
//!
//! Invariant: **the answers do not change when the table stops fitting in
//! memory, at either page size.** Every differential case in this repository
//! runs on six rows, and nothing above twenty thousand is graded at all
//! (task-2066 §4.4.5). So every claim the suite makes is a claim about a table
//! the pool holds entirely, and the two things that only happen past that
//! point - a scan that evicts what it read, and an index probe into a tree
//! deep enough to have interior levels the pool cannot keep - are claims
//! nothing checks.
//!
//! ## Sized from the pool rather than from a number
//!
//! The row count is derived from what the database says its own pool holds -
//! `PRAGMA page_size` and `PRAGMA cache_size` - so it scales with the
//! configuration rather than restating it. A fixed "one million rows"
//! would be past the pool today and inside it after somebody raised the cache,
//! and the day it stopped testing anything nothing would say so - which is the
//! failure class `tests/inillucent-testing-tdd.md` rule 1.4 is about.
//!
//! [`the_table_is_past_the_pool`] asserts the ratio it achieved, so a change
//! that made the rows smaller cannot quietly bring the table back inside the
//! pool.
//!
//! ## What it does to the table
//!
//! One story, the same at both arms, on one table built once per arm:
//!
//! 1. **Build** it past the pool, with a secondary index created before the
//!    load, which is the order an application writes.
//! 2. **Scan** it: TLP and NoREC, the same two properties `tlp_differential.rs`
//!    checks. They need no oracle, which is what makes them affordable here: a
//!    table this size cannot be compared row by row against a second engine in
//!    a nightly window.
//! 3. **Sort** it twice, by a column no index covers and by the indexed column
//!    descending, and compare every key in the order it came back against the
//!    order worked out in Rust.
//! 4. **Delete half** of it, and check the count, the sum, an index probe and
//!    TLP again over what is left, then sort what is left.
//! 5. **Reopen** the file and check the page size, the count and
//!    `PRAGMA integrity_check`, whose page leak arm is what catches a delete
//!    that did not hand a page back.
//!
//! ## Not a matrix story
//!
//! **Not a matrix story.** It runs at two arms, `matrix::default_arm()` and
//! `matrix::sqlite_page_arm()`: the engine's own 32,768 byte page and SQLite's
//! 4,096 byte page. The pool is narrowed to 8 MiB at both, so the table
//! reaches ten times it at a size a nightly can build. Both arms aim at the
//! same number of bytes; see [`POOL_KIB`] for why they do not land on it.
//!
//! The page size is checked rather than described: `narrow_the_pool` asserts
//! the page size of the file it opened against the arm's, and the reopen checks
//! it again, so an open that quietly built at the default would say so.
//!
//! **Why these two and not the six.** Each arm carries its own pool,
//! `frames * page_size`, and the other four arms differ from these two only in
//! the journal mode, the busy timeout, or a pool smaller than the 8 MiB this
//! file sets anyway:
//!
//! | arm | pool | what it varies | here |
//! |---|---|---|---|
//! | `default` | 4,096 x 32,768 = 128 MiB | nothing | yes |
//! | `sqlite-page` | 4,096 x 4,096 = 16 MiB | page size | yes (task-2075) |
//! | `small-pool` | 64 x 4,096 = 256 KiB | pool | no: the pool is set here |
//! | `truncate-journal` | 128 MiB | journal | no |
//! | `persist-journal` | 16 MiB | journal | no |
//! | `waiting` | 16 MiB | busy timeout | no |
//!
//! Page size is the one thing on that list that changes what a table past the
//! pool looks like: how many rows fit a leaf, how deep the tree is, how often
//! the delta area of a leaf compacts, and how many frames one scan touches.
//! Those are the things task-2066 §4.3 changed. The journal modes and the busy
//! timeout are graded by the matrix stories on tables of every size where they
//! matter, which is a commit and not a scan.
//!
//! ## What each page size costs
//!
//! Each arm prints one line starting `cost |` with the columns below. The
//! arms take turns rather than running at the same time (see [`ONE_ARM`]), so
//! the two lines are measured on the same box under the same load and can be
//! read side by side. "scan" is the 25 TLP and NoREC cases, "after" is the ten
//! cases and both sorts over what the delete left, and "reopen" includes
//! `PRAGMA integrity_check`. "delete reads" is how many pages the pool read
//! from the file during the delete, which the story also asserts on (see
//! [`DELETE_READS_PER_PAGE`]).
//!
//! **After task-2077**, measured on 2026-09-23 in a quiet window: both other
//! agents in this repository paused, CPU at 5% at the start, the debug build
//! the nightly tier runs, the prebuilt test binary run twice back to back and
//! the second run kept. The two runs agreed to within 1% on every delete.
//!
//! | arm | page | rows | file | build | scan | sort | delete half | delete reads | after | reopen | total |
//! |---|---|---|---|---|---|---|---|---|---|---|---|
//! | `default` | 32,768 | 1,613,193 | 69.4 MiB | 239.8 s | 173.6 s | 8.7 s | 69.8 s | 2,641 | 38.6 s | 17.9 s | 548 s |
//! | `sqlite-page` | 4,096 | 2,046,001 | 91.6 MiB | 154.2 s | 209.9 s | 12.1 s | 116.0 s | 37,407 | 51.6 s | 32.3 s | 576 s |
//!
//! The delete is now 86.5 microseconds a deleted row at 32,768 and 113.4 at
//! 4,096, against 908 and 169 before. The 32 KiB delete went from 5.4 times
//! the 4,096 byte cost per row to 0.76 times it. The cause was the order the
//! delete visited the trees, not work inside a leaf: see `delete_unwatched` in
//! `crates/inillucent-exec/src/dml/delete.rs` and
//! `crates/inillucent-compat/tests/engine/delete_order.rs`.
//!
//! **Before task-2077**, measured on 2026-09-23 in a quiet window (task-2075): every other agent on
//! the box paused, CPU at 17% from browsers and terminals at the start, the
//! debug build the nightly tier runs, one arm after the other:
//!
//! | arm | page | rows | file | build | scan | sort | delete half | after | reopen | total |
//! |---|---|---|---|---|---|---|---|---|---|---|
//! | `default` | 32,768 | 1,613,193 | 69.4 MiB | 354.4 s | 199.2 s | 8.6 s | 732.5 s | 60.1 s | 17.2 s | 1,374 s |
//! | `sqlite-page` | 4,096 | 2,046,001 | 91.6 MiB | 211.6 s | 215.7 s | 11.9 s | 173.3 s | 79.2 s | 34.5 s | 730 s |
//!
//! Per row, the build is 220 microseconds at 32,768 and 103 at 4,096, and the
//! delete is 908 microseconds a deleted row at 32,768 and 169 at 4,096: **5.4
//! times as expensive at the engine's own page size**, on a table with fewer
//! rows. That was task-2077. The sorts cost about the same at both sizes, and
//! the 4,096 byte arm pays in the reopen and the checks after the delete,
//! which have about ten times as many pages to visit (23,450 against 2,221).
//!
//! The first run, on a shared box where task-2068 was running its own copy of
//! the previous version of this story, read 576.3, 546.5, 9.3, 1,391.5, 56.4
//! and 15.0 s at 32,768 and 225.6, 203.1, 10.1, 196.6, 84.5 and 29.9 s at
//! 4,096. The four largest phases of the 4,096 byte arm moved by less than 14%
//! between the two runs and the 32 KiB arm by up to 2.7 times, because it was the one that overlapped
//! the other process. So the 32 KiB seconds above are the quiet ones, and a
//! `cost |` line from a shared box can be read for its ratios at best.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use inillucent_compat::facade::{Connection, Database};
use inillucent_compat::matrix::{default_arm, sqlite_page_arm, Arm};
use inillucent_value::Value;

/// How many times past the pool the table must reach.
///
/// Ten, which is enough that a scan evicts what it read several times over and
/// an index probe cannot hold the interior levels. Asserted rather than
/// assumed: see [`the_table_is_past_the_pool`].
const PAST_THE_POOL: u64 = 10;

/// How many predicates the scan over the whole table generates.
///
/// Fewer than the small generator's four hundred, because each one runs over a
/// table that does not fit in memory. The point here is the size, not the
/// count - the shapes are covered by `tlp_differential.rs`.
const CASES: usize = 25;

/// How many predicates the scan after the delete generates.
///
/// Fewer again: this scan is asking whether the delete left the trees
/// answering consistently, and ten shapes over half the table answer that.
const CASES_AFTER_DELETE: usize = 10;

/// Makes the arms take turns.
///
/// **Held for the whole story, because the costs are the point of having two
/// arms.** libtest runs the tests in one file at the same time, and two
/// builds past the pool at once would each report the other's load as its own cost.
/// The line one arm prints could then not be read beside the other's, which is
/// what task-2075 asked for.
static ONE_ARM: Mutex<()> = Mutex::new(());

/// Returns a scratch path nothing else uses.
///
/// **A directory per arm, not one path both of them open.** Two arms building
/// one `large.db` would be two tables written over each other.
///
/// The directory is removed whole rather than the file alone. A `.rdb` is a
/// file plus log segments named after it, so removing the first and leaving the
/// rest leaves a log from the previous run beside a fresh database, and the two
/// chains are then read as one. That is the shape of the incident in §5 of
/// task-2066: a database replaced under a log that outlived it.
///
/// @param tag - which arm is asking
fn scratch(tag: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("large-table")
        .join(tag);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::create_dir_all(&directory);
    directory.join("large.db")
}

/// Advances the generator.
///
/// @param state - the generator's state
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Returns a value below `bound`.
///
/// @param state - the generator's state
/// @param bound - one past the largest value
fn below(state: &mut u64, bound: u64) -> u64 {
    if bound == 0 {
        0
    } else {
        next(state) % bound
    }
}

/// Returns the `d` column the load writes for a key, `None` for NULL.
///
/// One function for the load and for every expectation, so the rows the test
/// predicts are the rows it wrote.
///
/// @param key - the row's `a`
fn d_of(key: i64) -> Option<i64> {
    if key % 7 == 0 {
        None
    } else {
        Some(key % 1013)
    }
}

/// Returns the `b` column the load writes for a key.
///
/// @param key - the row's `a`
fn b_of(key: i64) -> String {
    format!("row-{key}-padding-padding")
}

/// Runs a statement, returning its rows rendered or the failure's message.
///
/// @param connection - the database
/// @param sql - the statement
fn run(connection: &Connection, sql: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|failure| failure.message().to_string())?;
    let mut rows = Vec::new();
    loop {
        match statement.step() {
            Ok(true) => {}
            Ok(false) => break,
            Err(failure) => return Err(failure.message().to_string()),
        }
        let rendered: Vec<String> = statement
            .row()
            .iter()
            .map(|value| match value {
                Value::Null => "null".to_string(),
                Value::Integer(number) => format!("{number}"),
                Value::Real(number) => format!("{number:?}"),
                Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
                Value::Blob(blob) => format!("blob:{}", blob.raw().len()),
            })
            .collect();
        rows.push(rendered.join("|"));
    }
    Ok(rows)
}

/// Returns the one scalar a query answered.
///
/// @param connection - the database
/// @param sql - the statement
fn scalar(connection: &Connection, sql: &str) -> String {
    run(connection, sql)
        .unwrap_or_else(|failure| panic!("{sql}: {failure}"))
        .first()
        .cloned()
        .unwrap_or_default()
}

/// Returns how many bytes the pool holds, asked of the database itself.
///
/// **Asked rather than restated** (rule 1.4). A constant here would be a second
/// opinion about the engine's configuration, and the day the two disagreed this
/// file would still pass while grading a table that fits. `PRAGMA cache_size`
/// and `PRAGMA page_size` are the engine's own answer, so they cannot drift
/// from it.
///
/// A negative `cache_size` is a kibibyte budget, which is SQLite's convention
/// and this engine's; a positive one is a page count.
///
/// @param connection - the open database
fn pool_bytes(connection: &Connection) -> u64 {
    let page = scalar(connection, "PRAGMA page_size")
        .parse::<i64>()
        .unwrap_or(4096);
    let cache = scalar(connection, "PRAGMA cache_size")
        .parse::<i64>()
        .unwrap_or(-2000);
    let bytes = if cache < 0 {
        cache.saturating_neg().saturating_mul(1024)
    } else {
        cache.saturating_mul(page)
    };
    u64::try_from(bytes).unwrap_or(2 * 1024 * 1024).max(1)
}

/// How large a pool this test runs against, in kibibytes.
///
/// **Set rather than inherited, and this is the whole reason the test is
/// affordable.** The shipped default is `cache_size = -131072`, a 128 MiB
/// budget, so ten times it is 1.25 GiB - a table a nightly cannot build and
/// the first version of this file asked for, at 22,369,621 rows inserted one
/// statement at a time. Two runs were stopped by hand at quarter of an hour
/// with the file still climbing.
///
/// The condition under test is that the table does not fit in the pool. A pool
/// the test sets down reaches that condition in minutes and reaches it for the
/// same reason: a scan evicts what it read, and a probe cannot keep the
/// interior levels. What it does not reproduce is the shipped default's
/// *absolute* size, which no property here depends on.
///
/// **The same number of bytes at both page sizes**, which is 256 frames at
/// 32,768 and 2,048 at 4,096. Holding the bytes fixed rather than the frames
/// gives both arms the same target, ten times 8 MiB. They do not land on the
/// same file size: the row count is sized from a 20,000 row probe batch, and a
/// row's cost on disk after the probe is not what it was during it. The first
/// run built 69.4 MiB and 1,613,193 rows at 32,768 and 91.6 MiB and 2,046,001
/// rows at 4,096, which is why the cost line prints both.
const POOL_KIB: i64 = 8 * 1024;

/// Sets the pool this test runs against, and returns how many bytes it holds.
///
/// The value is written and then read back through [`pool_bytes`] rather than
/// returned from the constant, so everything downstream is still using the
/// engine's own answer. A pragma this engine refused would otherwise leave the
/// file sizing a table against a pool it does not have.
///
/// @param connection - the open database
/// @param arm - the page size the file must have
fn narrow_the_pool(connection: &Connection, arm: &Arm) -> u64 {
    let sql = format!("PRAGMA cache_size = -{POOL_KIB}");
    run(connection, &sql).unwrap_or_else(|failure| panic!("{sql}: {failure}"));
    let held = pool_bytes(connection);
    let wanted = u64::try_from(POOL_KIB).unwrap_or(0).saturating_mul(1024);
    assert_eq!(
        held, wanted,
        "`{sql}` left the pool at {held} bytes rather than {wanted} at `{}`, so every size \
         below would be measured against a pool this test does not have",
        arm.name
    );

    // **The arm this story says it runs at, checked.** `PRAGMA page_size`
    // reports the geometry of the file that was opened, so this is what fails
    // if `open_at` ever builds at the default and ignores the size it was
    // given - which would make the 4,096 byte arm a second copy of the 32 KiB
    // one while both reported green.
    let page = scalar(connection, "PRAGMA page_size");
    assert_eq!(
        page,
        format!("{}", arm.page_size),
        "the `{}` arm has {} byte pages and the file it opened has {page} byte pages",
        arm.name,
        arm.page_size
    );
    held
}

/// Returns how many rows put the table `PAST_THE_POOL` times past the pool.
///
/// **Measured, not estimated.** The estimate was sixty bytes a row - the key,
/// the text and the index entry added up - and the real figure on disk is
/// several hundred once pages, the index tree and the free map are counted, so
/// a table meant to be ten times the pool came out sixty times it and took
/// proportionally longer. The first batch is written, the file is measured, and
/// the rest is sized from what that batch actually cost. A measured width
/// cannot drift from the engine the way a constant can, which is the argument
/// [`pool_bytes`] already makes for asking the pragmas. It is also what keeps
/// the table the same size in bytes at both page sizes, whose rows cost a
/// different number of bytes each.
///
/// @param connection - the open database
/// @param written - how many rows the probe batch put in
/// @param path - the database file, to measure
fn rows_needed(connection: &Connection, written: i64, path: &Path) -> i64 {
    let file = std::fs::metadata(path).map(|held| held.len()).unwrap_or(0);
    let per_row = (file / u64::try_from(written).unwrap_or(1).max(1)).max(1);
    let wanted = pool_bytes(connection).saturating_mul(PAST_THE_POOL);
    i64::try_from((wanted / per_row).max(200_000)).unwrap_or(200_000)
}

/// Builds the table, in batches, and returns how many rows it holds.
///
/// One probe batch first, so the rest can be sized from what a row really
/// costs on disk; then the remainder.
///
/// @param connection - the database
/// @param path - the database file, to measure the probe batch against
/// @param arm - the page size the file must have
fn build(connection: &Connection, path: &Path, arm: &Arm) -> i64 {
    narrow_the_pool(connection, arm);
    for statement in [
        "CREATE TABLE big(a INTEGER PRIMARY KEY, b TEXT, d INTEGER)",
        // Before the load, which is the order an application writes and the
        // order that leaves the leaves holding delta rows (task-2066 §4.3.4).
        "CREATE INDEX big_d ON big(d)",
    ] {
        run(connection, statement).unwrap_or_else(|failure| panic!("{statement}: {failure}"));
    }
    let probe = 20_000i64;
    let mut at = load(connection, 1, probe);
    let rows = rows_needed(connection, probe, path).max(probe);
    while at <= rows {
        at = load(connection, at, (at + 20_000 - 1).min(rows));
    }
    rows
}

/// Writes one run of rows and returns the next unused key.
///
/// **Five hundred rows a statement, not one.** The cost of this load is the
/// statement and not the row: the text is formatted, parsed, planned and
/// executed per statement, and the rows inside one `VALUES` list share all of
/// it. The rows written are the same rows either way.
///
/// @param connection - the database
/// @param first - the first key to write
/// @param last - the last key to write
fn load(connection: &Connection, first: i64, last: i64) -> i64 {
    let per_statement = 500i64;
    let mut at = first;
    run(connection, "BEGIN").expect("a batch begins");
    while at <= last {
        let end = (at + per_statement - 1).min(last);
        let mut values = String::from("INSERT INTO big VALUES ");
        while at <= end {
            if !values.ends_with("VALUES ") {
                values.push_str(", ");
            }
            let d = d_of(at).map_or_else(|| "NULL".to_string(), |d| format!("{d}"));
            values.push_str(&format!("({at}, '{}', {d})", b_of(at)));
            at += 1;
        }
        run(connection, &values)
            .unwrap_or_else(|failure| panic!("a run of rows ending at {end}: {failure}"));
    }
    run(connection, "COMMIT").expect("a batch commits");
    at
}

/// The table really is past the pool, and says by how much.
///
/// **Without this the whole file could pass on a table that fits** (rule 1.4).
/// A change that made the rows narrower, or raised the default cache, would
/// leave both arms green while grading exactly what the small generator
/// already grades. Returns the file's size, for the cost line.
///
/// @param connection - the database
/// @param path - the database file
/// @param rows - how many rows the build wrote
/// @param arm - the arm, for the message
fn the_table_is_past_the_pool(connection: &Connection, path: &Path, rows: i64, arm: &Arm) -> u64 {
    let held = pool_bytes(connection);
    let file_bytes = std::fs::metadata(path)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    let ratio = file_bytes / held.max(1);
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM big"),
        format!("{rows}"),
        "`{}`: the load did not put every row in",
        arm.name
    );
    assert!(
        ratio >= PAST_THE_POOL / 2,
        "`{}`: the table is {file_bytes} bytes against a {held} byte pool, which is {ratio} \
         times past it and not the {PAST_THE_POOL} this file is for. Either the rows got \
         narrower or the pool got bigger; raise the row count rather than lowering the bar.",
        arm.name
    );
    file_bytes
}

/// Returns one generated predicate over the table.
///
/// @param state - the generator's state
/// @param rows - the largest key the table was built with
fn predicate(state: &mut u64, rows: i64) -> String {
    match below(state, 4) {
        0 => format!("d = {}", below(state, 1013)),
        1 => {
            let low = below(state, 1013);
            format!("d BETWEEN {low} AND {}", low + below(state, 50))
        }
        2 => format!("a > {}", below(state, u64::try_from(rows).unwrap_or(0))),
        _ => format!("b LIKE 'row-{}%'", below(state, 9)),
    }
}

/// TLP and NoREC hold over the table, for `cases` generated predicates.
///
/// What a table past the pool changes is the route: a probe evicts what it
/// read on the way down, so a page it needs again is fetched again rather than
/// found - and a defect in that path is invisible to every other case in this
/// repository, all of which run on tables the pool holds whole.
///
/// @param connection - the database
/// @param live - how many rows the table holds now
/// @param rows - the largest key the table was built with
/// @param seed - the generator's starting state
/// @param cases - how many predicates to check
/// @param arm - the arm, for the message
fn the_partitions_hold(
    connection: &Connection,
    live: i64,
    rows: i64,
    seed: u64,
    cases: usize,
    arm: &Arm,
) {
    let mut state = seed;
    for case in 0..cases {
        let condition = predicate(&mut state, rows);
        let mut counted = 0i64;
        for part in [
            format!("SELECT count(*) FROM big WHERE {condition}"),
            format!("SELECT count(*) FROM big WHERE NOT ({condition})"),
            format!("SELECT count(*) FROM big WHERE ({condition}) IS NULL"),
        ] {
            counted = counted.saturating_add(scalar(connection, &part).parse().unwrap_or(0));
        }
        assert_eq!(
            counted, live,
            "`{}` case {case}: the partitions of `{condition}` count {counted} rows over a \
             table of {live}",
            arm.name
        );

        let through_index = scalar(
            connection,
            &format!("SELECT count(*) FROM big WHERE {condition}"),
        );
        let through_scan = scalar(
            connection,
            &format!("SELECT sum(CASE WHEN ({condition}) THEN 1 ELSE 0 END) FROM big"),
        );
        let scanned = if through_scan == "null" {
            "0".to_string()
        } else {
            through_scan
        };
        assert_eq!(
            through_index, scanned,
            "`{}` case {case}: `{condition}` counts differently through an index and through a \
             scan over a table past the pool",
            arm.name
        );
    }
}

/// Compares a query's keys, in the order they came back, with the order
/// expected.
///
/// **Every key, in order, not a count and a spot check.** A sort that dropped
/// a run when it merged, or repeated one, or put two runs back in the wrong
/// order, keeps the count right about as often as not. The first position that
/// differs is what the message names.
///
/// @param connection - the database
/// @param sql - a query whose first column is `a`
/// @param expected - the keys in the order the query must return them
/// @param arm - the arm, for the message
fn the_order_is(connection: &Connection, sql: &str, expected: &[i64], arm: &Arm) {
    let got = run(connection, sql).unwrap_or_else(|failure| panic!("{sql}: {failure}"));
    let wanted: Vec<String> = expected.iter().map(|key| format!("{key}")).collect();
    if got != wanted {
        let at = got
            .iter()
            .zip(&wanted)
            .position(|(one, other)| one != other)
            .unwrap_or(got.len().min(wanted.len()));
        panic!(
            "`{}`: `{sql}` returned {} rows where {} were expected, and the first difference is \
             at position {at}: {:?} where {:?} was expected",
            arm.name,
            got.len(),
            wanted.len(),
            got.get(at),
            wanted.get(at)
        );
    }
}

/// The table sorts correctly by an unindexed column and by the indexed one.
///
/// `ORDER BY b` has no index to read in order, so it is a real sort over a
/// table the pool cannot hold. `ORDER BY d DESC, a` has one, and NULLs last,
/// which is where SQLite puts them in a descending order.
///
/// @param connection - the database
/// @param live - the keys the table holds
/// @param arm - the arm, for the message
fn the_sorts_hold(connection: &Connection, live: &[i64], arm: &Arm) {
    let mut by_text = live.to_vec();
    by_text.sort_by_key(|key| b_of(*key));
    the_order_is(connection, "SELECT a FROM big ORDER BY b", &by_text, arm);

    let mut by_index = live.to_vec();
    by_index.sort_by(|one, other| d_of(*other).cmp(&d_of(*one)).then(one.cmp(other)));
    the_order_is(
        connection,
        "SELECT a FROM big ORDER BY d DESC, a",
        &by_index,
        arm,
    );
}

/// Deletes every even key and checks what is left, returning the keys left.
///
/// Half the table, spread across every leaf, which is the delete that leaves
/// every page half full rather than emptying some and leaving the rest alone.
///
/// @param connection - the database
/// @param rows - how many rows the table holds before the delete
/// @param arm - the arm, for the message
fn delete_half(connection: &Connection, rows: i64, arm: &Arm) -> Vec<i64> {
    run(connection, "DELETE FROM big WHERE a % 2 = 0")
        .unwrap_or_else(|failure| panic!("`{}`: the delete: {failure}", arm.name));
    assert_eq!(
        connection.changes(),
        rows / 2,
        "`{}`: the delete reported the wrong number of rows",
        arm.name
    );
    let live: Vec<i64> = (1..=rows).filter(|key| key % 2 == 1).collect();
    let sum: i64 = live.iter().sum();
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM big"),
        format!("{}", live.len()),
        "`{}`: the count after the delete",
        arm.name
    );
    assert_eq!(
        scalar(connection, "SELECT sum(a) FROM big"),
        format!("{sum}"),
        "`{}`: the sum of the keys after the delete",
        arm.name
    );
    // Through the index, which the delete had to change as well as the table.
    let probe = 505;
    let through_index = live.iter().filter(|key| d_of(**key) == Some(probe)).count();
    assert_eq!(
        scalar(
            connection,
            &format!("SELECT count(*) FROM big WHERE d = {probe}")
        ),
        format!("{through_index}"),
        "`{}`: the index still answers for rows the delete removed",
        arm.name
    );
    live
}

/// How many pages the delete may read, per page of the file.
///
/// **The delete reads each leaf about once, or it reads about one page per
/// row, and nothing in between** (task-2077). The keys come from `SCAN big
/// USING COVERING INDEX big_d`, in `(d, a)` order, and at 32,768 bytes a leaf
/// holds fewer rows than `d` takes to repeat, so looked up in that order every
/// key is in a different leaf. Removing the rows in the table's order with
/// each row's entry in `big_d` beside it visits the index out of order
/// instead: this story read 285,098 pages of a 2,221 page file that way, and
/// 2,641 once the rows went in the table's order and the index entries after
/// them in the index's order (release build). The bound is wide on purpose:
/// it is a count, so it catches that change of kind on any machine, and it
/// does not notice anything smaller.
const DELETE_READS_PER_PAGE: u64 = 4;

/// The delete read each leaf about once rather than once per deleted row.
///
/// @param connection - the database, to ask its page count
/// @param reads - how many pages the pool read during the delete
/// @param arm - the arm, for the message
fn the_delete_read_each_leaf_about_once(connection: &Connection, reads: u64, arm: &Arm) {
    let pages = scalar(connection, "PRAGMA page_count")
        .parse::<u64>()
        .unwrap_or(0);
    assert!(
        pages > 0 && reads <= pages.saturating_mul(DELETE_READS_PER_PAGE),
        "`{}`: deleting half the table read {reads} pages of a {pages} page file, more than \
         {DELETE_READS_PER_PAGE} a page, so the delete is visiting the table or `big_d` in an \
         order other than the one the tree holds (task-2077)",
        arm.name
    );
}

/// The file is sound and says what it held, after it is closed and reopened.
///
/// @param path - the database file
/// @param arm - the arm the file was built at
/// @param live - how many rows it must hold
fn the_file_reopens(path: &Path, arm: &Arm, live: usize) {
    let database = Database::open_at(path, arm.page_size as usize, arm.frames as usize)
        .expect("the database reopens");
    let connection = database.session().expect("the connection reopens");
    assert_eq!(
        scalar(&connection, "PRAGMA page_size"),
        format!("{}", arm.page_size),
        "`{}`: the file came back at a different page size",
        arm.name
    );
    assert_eq!(
        scalar(&connection, "SELECT count(*) FROM big"),
        format!("{live}"),
        "`{}`: the count after reopening",
        arm.name
    );
    let checked = scalar(&connection, "PRAGMA integrity_check");
    assert_eq!(
        checked, "ok",
        "`{}`: the table is not sound after the run: {checked}",
        arm.name
    );
}

/// How long each part of the story took at one arm.
struct Costs {
    /// The load, from an empty file to a table past the pool.
    build: Duration,
    /// TLP and NoREC over the whole table.
    scan: Duration,
    /// Both sorts over the whole table.
    sort: Duration,
    /// The delete of every even key and the checks on what it left.
    delete: Duration,
    /// How many pages the pool read from the file during that delete.
    delete_reads: u64,
    /// TLP, NoREC and both sorts over the half that is left.
    after: Duration,
    /// Closing, reopening and `PRAGMA integrity_check`.
    reopen: Duration,
}

/// Prints the arm's cost line, in the columns the module documentation uses.
///
/// @param arm - the arm
/// @param rows - how many rows the build wrote
/// @param file_bytes - the file's size after the build
/// @param costs - how long each part took
fn print_costs(arm: &Arm, rows: i64, file_bytes: u64, costs: &Costs) {
    let seconds = |spent: Duration| format!("{:.1} s", spent.as_secs_f64());
    println!(
        "cost | {} | {} | {rows} | {:.1} MiB | {} | {} | {} | {} | {} | {} | {} |",
        arm.name,
        arm.page_size,
        file_bytes as f64 / (1024.0 * 1024.0),
        seconds(costs.build),
        seconds(costs.scan),
        seconds(costs.sort),
        seconds(costs.delete),
        costs.delete_reads,
        seconds(costs.after),
        seconds(costs.reopen),
    );
}

/// Runs the whole story at one arm: build, scan, sort, delete half, reopen.
///
/// @param arm - the page size and pool to open the file at
fn the_story_at(arm: Arm) {
    let _turn = ONE_ARM.lock().unwrap_or_else(|held| held.into_inner());
    let path = scratch(arm.name);
    let database = Database::open_at(&path, arm.page_size as usize, arm.frames as usize)
        .expect("the database opens");
    let connection = database.session().expect("the connection opens");

    let started = Instant::now();
    let rows = build(&connection, &path, &arm);
    let build_cost = started.elapsed();
    let file_bytes = the_table_is_past_the_pool(&connection, &path, rows, &arm);
    let everything: Vec<i64> = (1..=rows).collect();

    let started = Instant::now();
    the_partitions_hold(&connection, rows, rows, 0x2068_4405_u64, CASES, &arm);
    let scan = started.elapsed();
    let started = Instant::now();
    the_sorts_hold(&connection, &everything, &arm);
    let sort = started.elapsed();
    let started = Instant::now();
    let reads_before = database.cache_stats().reads;
    let live = delete_half(&connection, rows, &arm);
    let delete_reads = database.cache_stats().reads.saturating_sub(reads_before);
    let delete = started.elapsed();
    the_delete_read_each_leaf_about_once(&connection, delete_reads, &arm);
    let started = Instant::now();
    let left = i64::try_from(live.len()).unwrap_or(0);
    the_partitions_hold(&connection, left, rows, 0x2075, CASES_AFTER_DELETE, &arm);
    the_sorts_hold(&connection, &live, &arm);
    let after = started.elapsed();

    drop(connection);
    drop(database);
    let started = Instant::now();
    the_file_reopens(&path, &arm, live.len());
    let reopen = started.elapsed();

    let costs = Costs {
        build: build_cost,
        scan,
        sort,
        delete,
        delete_reads,
        after,
        reopen,
    };
    print_costs(&arm, rows, file_bytes, &costs);
}

/// The story at the engine's own page, 32,768 bytes.
#[test]
fn a_table_past_the_pool_at_the_default_page() {
    the_story_at(default_arm());
}

/// The story at SQLite's page, 4,096 bytes (task-2075).
///
/// The file an application migrating off SQLite is most likely to have: small
/// pages, more rows than the pool holds, a secondary index. A page holds an
/// eighth of what a default page holds, so the same rows need more leaves, a
/// deeper tree, and more frames touched by one scan.
#[test]
fn a_table_past_the_pool_at_the_sqlite_page() {
    the_story_at(sqlite_page_arm());
}
