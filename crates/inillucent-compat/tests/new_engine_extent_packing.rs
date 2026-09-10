//! What a value costs on disk when it is just past the spill threshold.
//!
//! Invariant: **a value that fits inside one page does not cost a whole page.**
//! An extent used to be a run of whole pages and nothing else, so a value one
//! byte over the threshold took an entire 32 KiB page. Measured before the fix,
//! 1,000 rows per case, file size after a checkpoint:
//!
//! | value | bytes on disk per row | ratio |
//! |---:|---:|---:|
//! | 3,000 B (inline) | 3,801 | 1.27x |
//! | 4,096 B (inline) | 5,636 | 1.38x |
//! | **4,200 B** | **33,980** | **8.09x** |
//! | 9,513 B | 33,980 | 3.57x |
//! | 20,000 B | 33,980 | 1.70x |
//! | 40,000 B | 66,748 | 1.67x |
//!
//! That band is where extracted document text, JSON payloads and rendered
//! vectors live, and it is why migrating Nikaya's 5,852 MB PostgreSQL database
//! produced a 25.66 GB staged file: 601,862 rendered `halfvec` values at 9,513
//! bytes each were 20.5 GB of it.
//!
//! ## Why the bound is a ratio and not a number
//!
//! A file holds the rows, the trees over them, the free map and two meta pages,
//! and every one of those moves when something unrelated changes. What must not
//! move is the *order* of the cost: a value of four kilobytes costing four
//! kilobytes and a bit, rather than eight times itself. So the assertions are
//! ratios wide enough that ordinary overhead never trips them and narrow enough
//! that a return to a page per value always does - 8.09x against a bound of 2x.

use std::path::PathBuf;

use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// The page size `Database::open` builds at, which fixes the spill threshold.
const PAGE_SIZE: usize = 32_768;

/// How many rows each measurement writes.
const ROWS: usize = 1_000;

/// Returns a clean path for one test's database.
///
/// @param name - the test's name
fn scratch(name: &str) -> PathBuf {
    let area = workspace_root().join("target/scratch/packing");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    remove(&path);
    path
}

/// Removes a database and every log segment beside it.
///
/// @param path - the database file
fn remove(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let Some(directory) = path.parent() else {
        return;
    };
    let Some(stem) = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&format!("{stem}-")) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Writes `ROWS` rows of one width and returns the file's size afterwards.
///
/// Reads one back and counts them before measuring, so a file that is small
/// because nothing was stored fails here rather than reporting a good number.
///
/// @param path - where to build the database
/// @param width - how many bytes each value holds
fn cost_per_row(path: &PathBuf, width: usize) -> f64 {
    {
        let database = Database::open(path).expect("a fresh database opens");
        let connection = database.connect();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)")
            .expect("the schema is created");
        fill(&connection, width);
        database.checkpoint().expect("the file is checkpointed");
        assert_eq!(read_width(&connection, 500), width);
        assert_eq!(count(&connection), ROWS as i64);
    }
    let bytes = std::fs::metadata(path).expect("the file is there").len();
    bytes as f64 / ROWS as f64
}

/// Inserts `ROWS` rows of one width in a single transaction.
///
/// @param connection - the database
/// @param width - how many bytes each value holds
fn fill(connection: &Connection<'_>, width: usize) {
    let body = "x".repeat(width);
    connection.execute("BEGIN").expect("begin");
    let mut statement = connection
        .prepare("INSERT INTO t (id, body) VALUES (?1, ?2)")
        .expect("the insert prepares");
    for nth in 1..=ROWS {
        statement.reset();
        statement.bind_integer(1, nth as i64).expect("bind");
        statement.bind_text(2, &body).expect("bind");
        while statement.step().expect("step") {}
    }
    drop(statement);
    connection.execute("COMMIT").expect("commit");
}

/// Returns the length of one row's value.
///
/// @param connection - the database
/// @param id - which row
fn read_width(connection: &Connection<'_>, id: i64) -> usize {
    let rows = connection
        .query(&format!("SELECT body FROM t WHERE id = {id}"))
        .expect("the row reads");
    match rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Text(bytes)) => bytes.len(),
        other => panic!("expected text, got {other:?}"),
    }
}

/// Returns how many rows the table holds.
///
/// @param connection - the database
fn count(connection: &Connection<'_>) -> i64 {
    let rows = connection
        .query("SELECT count(*) FROM t")
        .expect("the count reads");
    match rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// A value one byte past the spill threshold costs about what it holds.
///
/// The threshold is `page_size / 8`, so 4,096 at the default page size, and
/// 4,200 is the first case the old measurement showed at 8.09x.
#[test]
fn a_value_just_past_the_spill_threshold_does_not_cost_a_whole_page() {
    let width = PAGE_SIZE / 8 + 104;
    let path = scratch("just-past-the-threshold");
    let per = cost_per_row(&path, width);
    assert!(
        per < width as f64 * 2.0,
        "a {width}-byte value costs {per:.0} bytes on disk, which is {:.2}x itself - a whole \\
         page is {PAGE_SIZE}",
        per / width as f64
    );
    remove(&path);
}

/// And so does one in the middle of the band, which is Nikaya's own case.
///
/// 9,513 bytes is the width of a rendered `halfvec` in that corpus: 601,862 of
/// them, 20.5 GB of a 25.66 GB staged file, at 33,980 bytes each.
#[test]
fn a_value_in_the_middle_of_the_band_does_not_cost_a_whole_page() {
    let path = scratch("middle-of-the-band");
    let per = cost_per_row(&path, 9_513);
    assert!(
        per < 9_513.0 * 2.0,
        "a 9,513-byte value costs {per:.0} bytes on disk, which is {:.2}x itself",
        per / 9_513.0
    );
    remove(&path);
}

/// A value larger than a page still gets a contiguous run.
///
/// The other half of the change, and the one a packing fix could quietly break:
/// a value that needs more than one page is not packed, because it needs every
/// byte of the pages it takes and a run is one seek and one sequential read.
/// Two pages of payload for a 40,000-byte value is 65,536 bytes, so the bound is
/// the third page it must not be spending.
#[test]
fn a_value_larger_than_a_page_still_costs_its_pages_and_no_more() {
    let path = scratch("larger-than-a-page");
    let per = cost_per_row(&path, 40_000);
    assert!(
        per < (PAGE_SIZE * 3) as f64,
        "a 40,000-byte value costs {per:.0} bytes on disk, which is more than the two pages it \\
         needs plus its row"
    );
    assert!(
        per > 40_000.0,
        "a 40,000-byte value cannot cost {per:.0} bytes; something was not stored"
    );
    remove(&path);
}

/// Values that share a page read back byte for byte after a reopen.
///
/// Written in one process and read in another handle, because what is being
/// checked is the bytes on disk rather than what the pool happened to be
/// holding - and because the shared page a writer was filling is a hint it
/// keeps in memory and does not write down.
#[test]
fn values_sharing_a_page_read_back_after_a_reopen() {
    let path = scratch("reopen");
    let widths = [4_200usize, 5_000, 4_500, 9_513, 4_097];
    {
        let database = Database::open(&path).expect("a fresh database opens");
        let connection = database.connect();
        connection
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)")
            .expect("the schema is created");
        connection.execute("BEGIN").expect("begin");
        let mut statement = connection
            .prepare("INSERT INTO t (id, body) VALUES (?1, ?2)")
            .expect("the insert prepares");
        for (nth, width) in widths.iter().enumerate() {
            statement.reset();
            statement.bind_integer(1, nth as i64 + 1).expect("bind");
            statement.bind_text(2, &marked(*width, nth)).expect("bind");
            while statement.step().expect("step") {}
        }
        drop(statement);
        connection.execute("COMMIT").expect("commit");
        database.checkpoint().expect("the file is checkpointed");
    }
    let database = Database::open(&path).expect("the database reopens");
    let connection = database.connect();
    for (nth, width) in widths.iter().enumerate() {
        let rows = connection
            .query(&format!("SELECT body FROM t WHERE id = {}", nth + 1))
            .expect("the row reads");
        let held = match rows.first().and_then(|row| row.first()) {
            Some(OwnedDatum::Text(bytes)) => bytes.clone(),
            other => panic!("expected text for row {nth}, got {other:?}"),
        };
        assert_eq!(
            held,
            marked(*width, nth).into_bytes(),
            "row {nth} of {width} bytes did not read back byte for byte"
        );
    }
    let _ = connection;
    drop(database);
    remove(&path);
}

/// Deleting every value on a shared page gives the page back to the free map.
///
/// A slot's bytes are never reused while the page lives, so the page is the unit
/// that comes back - and a page that never came back would be a leak that grows
/// with every rewrite of a table of mid-sized values.
#[test]
fn deleting_every_packed_value_returns_the_pages() {
    let path = scratch("free");
    let database = Database::open(&path).expect("a fresh database opens");
    let connection = database.connect();
    connection
        .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)")
        .expect("the schema is created");
    fill(&connection, 4_200);
    database.checkpoint().expect("the file is checkpointed");
    let before = free_pages(&connection);
    connection
        .execute_batch("DELETE FROM t")
        .expect("every row is deleted");
    database
        .checkpoint()
        .expect("the file is checkpointed again");
    let after = free_pages(&connection);
    assert_eq!(count(&connection), 0, "the table is empty");
    // A thousand values of 4,200 bytes share about seven to a 32 KiB page, so
    // the pages that come back are in the hundreds. The bound is loose because
    // what it is pinning is that they come back at all.
    assert!(
        after > before + 100,
        "the free map went from {before} to {after} pages after deleting a thousand packed \\
         values, so the pages they were on did not come back"
    );
    let _ = connection;
    drop(database);
    remove(&path);
}

/// Returns how many pages the free map holds.
///
/// @param connection - the database
fn free_pages(connection: &Connection<'_>) -> i64 {
    let rows = connection
        .query("PRAGMA freelist_count")
        .expect("the pragma reads");
    match rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// Returns a value of one width whose bytes say which row it belongs to.
///
/// A value of one repeated character reads back correctly even when a slot's
/// offset is wrong by a few bytes, which is exactly the failure this is looking
/// for; a marked one does not.
///
/// @param width - how many bytes
/// @param nth - which row
fn marked(width: usize, nth: usize) -> String {
    let head = format!("row-{nth}-of-{width}:");
    let mut out = String::with_capacity(width);
    out.push_str(&head);
    while out.len() < width {
        out.push(char::from(b'a' + (nth as u8 % 26)));
    }
    out.truncate(width);
    out
}

/// Churning a table of packed values leaves every tree intact.
///
/// **The check a cost measurement cannot make.** Packing several values into one
/// page means a page is shared, freed only when the last slot on it goes, and
/// handed back to the free map to become anything - a leaf, an interior page,
/// another shared page. A reference left pointing into a page that has become
/// something else is a corruption that reads fine until the moment it does not,
/// and it would not show up in a test that only writes.
///
/// So this writes, rewrites, deletes and re-inserts values across the whole
/// band - inline, packed, and a run of several pages - and asks
/// `PRAGMA integrity_check` after every round. The check walks every tree in key
/// order and compares each index against the table it is on, which is what
/// would catch a slot the free map has since given away.
#[test]
fn churning_packed_values_leaves_every_tree_intact() {
    let path = scratch("churn");
    let database = Database::open(&path).expect("a fresh database opens");
    let connection = database.connect();
    connection
        .execute_batch(
            "CREATE TABLE t (id TEXT PRIMARY KEY, tag INTEGER, body TEXT);\
             CREATE INDEX t_tag ON t (tag);",
        )
        .expect("the schema is created");

    // Every width the change touches: inline, one page over the threshold, the
    // middle of the band, and a value that needs a run of two pages.
    let widths = [1_000usize, 4_200, 9_513, 40_000];
    for round in 0..6usize {
        connection.execute("BEGIN").expect("begin");
        let mut insert = connection
            .prepare("INSERT OR REPLACE INTO t (id, tag, body) VALUES (?1, ?2, ?3)")
            .expect("the insert prepares");
        for nth in 0..200usize {
            let width = widths[(nth + round) % widths.len()];
            insert.reset();
            insert.bind_text(1, &format!("row-{nth:05}")).expect("bind");
            insert.bind_integer(2, (nth % 17) as i64).expect("bind");
            insert.bind_text(3, &marked(width, nth)).expect("bind");
            while insert.step().expect("step") {}
        }
        drop(insert);
        connection.execute("COMMIT").expect("commit");

        // Every third row goes, so pages lose some slots and keep others -
        // which is the state a page that is freed too early would be found in.
        connection
            .execute_batch(&format!(
                "DELETE FROM t WHERE CAST(substr(id, 5) AS INTEGER) % 3 = {}",
                round % 3
            ))
            .expect("a third of the rows are deleted");
        database.checkpoint().expect("the file is checkpointed");

        let held = connection
            .query("PRAGMA integrity_check")
            .expect("the check runs");
        let said = match held.first().and_then(|row| row.first()) {
            Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).to_string(),
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(said, "ok", "round {round} left the database damaged");

        // And every value still reads back byte for byte, which the check
        // cannot say: it walks structure, not content.
        let rows = connection
            .query("SELECT id, body FROM t ORDER BY id")
            .expect("the rows read");
        for row in &rows {
            let (id, body) = match row.as_slice() {
                [OwnedDatum::Text(id), OwnedDatum::Text(body)] => (id.clone(), body.clone()),
                other => panic!("expected two text values, got {other:?}"),
            };
            let nth: usize = String::from_utf8_lossy(&id)
                .trim_start_matches("row-")
                .parse()
                .expect("the id carries its number");
            let width = widths[(nth + round) % widths.len()];
            assert_eq!(
                body,
                marked(width, nth).into_bytes(),
                "row {nth} of {width} bytes did not read back after round {round}"
            );
        }
    }
    let _ = connection;
    drop(database);
    remove(&path);
}
