//! Large values through blob extents, graded against pinned SQLite 3.53.4.
//!
//! Invariant: **a value stored out of line reads back byte for byte, and every
//! path that touches it agrees with SQLite about what it holds.** The extent is
//! a storage decision and nothing above the leaf is supposed to be able to tell
//! - so the test is not "the extent works", it is "the answers are the same",
//! asked of a scan, a point read, an update, a delete and a reopen.
//!
//! ## Why the page size is stated
//!
//! A value goes out of line when it is longer than `page_size / 8`. At the
//! 4 KiB page size these tests use that is 512 bytes, so the `body` column's
//! two-kilobyte values are out of line and its short ones are not - which is the
//! case worth testing, because a leaf holding both is a leaf whose class array
//! carries two different classes for one column.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// The page size these tests build at, which fixes the spill threshold at 512.
const PAGE_SIZE: usize = 4_096;

/// Returns the pinned SQLite oracle binary, if it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns a fresh directory for one test's files.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-extents-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// A fixture both engines have opened.
struct Pair {
    engine: ImportedDatabase,
    oracle: Driver,
    /// Kept so the directory outlives the test.
    _directory: PathBuf,
}

/// Builds a `wide`-shaped fixture with the oracle and imports it.
///
/// @param tag - what to name the scratch directory after
/// @param rows - how many rows to seed
fn pair(tag: &str, rows: usize) -> Option<Pair> {
    let program = oracle_path()?;
    let directory = scratch(tag);
    let path = directory.join("wide.db");
    let mut oracle = Driver::start("sqlite", &program).expect("the oracle starts");
    oracle
        .send(&Op::Open(path.to_string_lossy().into_owned()))
        .expect("the oracle opens the fixture");
    let schema = [
        "CREATE TABLE wide (id INTEGER PRIMARY KEY, tag TEXT, body TEXT)",
        "CREATE INDEX wide_tag ON wide (tag)",
    ];
    for sql in schema {
        let observed = oracle
            .send(&Op::Exec(sql.to_string()))
            .expect("the exec runs");
        assert!(observed.ok, "the fixture did not build: {sql}");
    }
    for nth in 1..=rows {
        // Every fourth row is *short*, so one leaf's `body` column carries both
        // classes and the reader has to consult the class array rather than the
        // leaf's flag alone.
        let body = if nth % 4 == 0 {
            format!("short {nth}")
        } else {
            format!("{nth}:{}", "x".repeat(2_048))
        };
        let sql = format!(
            "INSERT INTO wide VALUES ({nth}, 'tag{}', '{body}')",
            nth % 7
        );
        let observed = oracle.send(&Op::Exec(sql)).expect("the insert runs");
        assert!(observed.ok, "the seed did not insert: {}", observed.message);
    }
    let engine = ImportedDatabase::import_with(path, PAGE_SIZE, 4_096)
        .unwrap_or_else(|error| panic!("the fixture did not import: {:?}", error.detail()));
    Some(Pair {
        engine,
        oracle,
        _directory: directory,
    })
}

impl Pair {
    /// Runs one statement on both engines and asserts they agreed about it.
    ///
    /// @param sql - the statement
    fn both(&mut self, sql: &str) {
        let theirs = self
            .oracle
            .send(&Op::Exec(sql.to_string()))
            .expect("the oracle runs the statement");
        let ours = self.engine.execute_any(sql, &Params::new());
        assert_eq!(
            theirs.ok,
            ours.is_ok(),
            "{sql}: sqlite said {} ({}), inillucent said {} ({:?})",
            theirs.ok,
            theirs.message,
            ours.is_ok(),
            ours.as_ref().err().and_then(|error| error.detail())
        );
    }

    /// Asserts both engines answer one query identically.
    ///
    /// @param sql - the query
    fn answers_agree(&mut self, sql: &str) {
        let theirs = self
            .oracle
            .send(&Op::Query(sql.to_string()))
            .expect("the oracle runs the query");
        assert!(theirs.ok, "{sql}: the oracle refused: {}", theirs.message);
        let ours = self
            .engine
            .execute_any(sql, &Params::new())
            .unwrap_or_else(|error| panic!("{sql}: the new engine refused: {:?}", error.detail()));
        let mine: Vec<Vec<TaggedValue>> = ours
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        OwnedDatum::Null => TaggedValue::Null,
                        OwnedDatum::Int(number) => TaggedValue::Integer(*number),
                        OwnedDatum::Real(number) => TaggedValue::Real(*number),
                        OwnedDatum::Text(bytes) => TaggedValue::Text(bytes.clone()),
                        OwnedDatum::Blob(bytes) => TaggedValue::Blob(bytes.clone()),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(mine, theirs.rows, "{sql} disagreed");
    }

    /// Walks every tree and fails on the first broken invariant.
    ///
    /// @param after - what was just run, for the message
    fn is_intact(&self, after: &str) {
        self.engine
            .check_trees()
            .unwrap_or_else(|error| panic!("the file is not intact after `{after}`: {error:?}"));
    }
}

/// The questions every campaign is graded by.
///
/// A whole-value read, a length, a point read, a scan in index order and an
/// aggregate - so a value that came back short, long, or from the wrong row
/// fails at least one of them.
const PROBES: &[&str] = &[
    "SELECT id, body FROM wide ORDER BY id",
    "SELECT id, length(body) FROM wide ORDER BY id",
    "SELECT body FROM wide WHERE id = 3",
    "SELECT body FROM wide WHERE id = 4",
    "SELECT count(*), sum(length(body)) FROM wide",
    "SELECT id, tag FROM wide ORDER BY tag, id",
    "SELECT id FROM wide WHERE tag = 'tag3' ORDER BY id",
    "SELECT substr(body, 1, 12) FROM wide ORDER BY id",
];

/// Says the suite could not run, rather than passing quietly.
fn no_oracle() {
    eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.ps1");
}

#[test]
fn a_value_larger_than_the_threshold_reads_back_whole() {
    let Some(mut pair) = pair("read", 40) else {
        return no_oracle();
    };
    pair.is_intact("the import");
    for probe in PROBES {
        pair.answers_agree(probe);
    }
}

#[test]
fn writing_over_a_large_value_keeps_every_other_row() {
    let Some(mut pair) = pair("write", 40) else {
        return no_oracle();
    };
    for sql in [
        // Longer than it was, so the run it was in cannot be reused.
        "UPDATE wide SET body = body || body WHERE id = 5",
        // Shorter, and now under the threshold - the value comes back inline.
        "UPDATE wide SET body = 'tiny' WHERE id = 6",
        // A short value becomes a large one.
        "UPDATE wide SET body = replace(hex(zeroblob(1500)), '0', 'y') WHERE id = 8",
        // A fresh row whose value is out of line from the start.
        "INSERT INTO wide VALUES (100, 'tag1', 'z')",
        "UPDATE wide SET body = replace(hex(zeroblob(2000)), '0', 'w') WHERE id = 100",
        // And one that goes away.
        "DELETE FROM wide WHERE id = 7",
    ] {
        pair.both(sql);
        pair.is_intact(sql);
    }
    for probe in PROBES {
        pair.answers_agree(probe);
    }
    pair.answers_agree("SELECT id, length(body) FROM wide WHERE id IN (5, 6, 8, 100)");
}

#[test]
fn a_campaign_of_large_writes_leaves_the_file_intact() {
    let Some(mut pair) = pair("campaign", 60) else {
        return no_oracle();
    };
    // Enough writes to force compactions, splits and at least one merge, with
    // out-of-line values moving through all three.
    for nth in 1..=30 {
        pair.both(&format!(
            "UPDATE wide SET body = body || 'appended {nth}' WHERE id = {}",
            nth * 2
        ));
    }
    for nth in 1..=15 {
        pair.both(&format!("DELETE FROM wide WHERE id = {}", nth * 4));
    }
    for nth in 1..=15 {
        pair.both(&format!(
            "INSERT INTO wide VALUES ({}, 'tag2', '{}:{}')",
            200 + nth,
            nth,
            "q".repeat(1_800)
        ));
    }
    pair.is_intact("the campaign");
    for probe in PROBES {
        pair.answers_agree(probe);
    }
}

#[test]
fn out_of_line_values_survive_closing_and_reopening_the_file() {
    let Some(mut pair) = pair("reopen", 40) else {
        return no_oracle();
    };
    pair.both("UPDATE wide SET body = body || ' tail' WHERE id = 9");
    pair.both("INSERT INTO wide VALUES (101, 'tag5', 'a')");
    let before = pair
        .engine
        .execute_any(
            "SELECT id, length(body) FROM wide ORDER BY id",
            &Params::new(),
        )
        .expect("the lengths read")
        .rows;

    pair.engine.reopen().expect("the file reopens");

    pair.is_intact("a reopen");
    let after = pair
        .engine
        .execute_any(
            "SELECT id, length(body) FROM wide ORDER BY id",
            &Params::new(),
        )
        .expect("the lengths read again")
        .rows;
    assert_eq!(before, after, "the lengths changed across a reopen");
    for probe in PROBES {
        pair.answers_agree(probe);
    }
}

#[test]
fn a_reference_in_the_delta_area_is_replaced_and_given_back() {
    let Some(mut pair) = pair("delta", 40) else {
        return no_oracle();
    };
    // **The row is written twice, and the second write is the one under test.**
    // A large value is spilled and its *reference* goes into the delta area; the
    // second write finds the key there rather than in the sorted region, and has
    // to remove that row, give its pages back, and stop saying the leaf holds an
    // out-of-line value if that was the last one. Nothing else in this suite
    // reaches that path: every other write finds its key in the sorted region.
    for sql in [
        "UPDATE wide SET body = replace(hex(zeroblob(1200)), '0', 'a') WHERE id = 11",
        "UPDATE wide SET body = replace(hex(zeroblob(1600)), '0', 'b') WHERE id = 11",
        "UPDATE wide SET body = 'small again' WHERE id = 11",
        "UPDATE wide SET body = replace(hex(zeroblob(1400)), '0', 'c') WHERE id = 11",
        // And the same shape ending in a delete, which frees the reference the
        // delta area holds rather than leaving it for a repack that never comes.
        "UPDATE wide SET body = replace(hex(zeroblob(1300)), '0', 'd') WHERE id = 13",
        "DELETE FROM wide WHERE id = 13",
        // A row that is new, spilled, and then replaced while still in the delta.
        "INSERT INTO wide VALUES (300, 'tag4', 'seed')",
        "UPDATE wide SET body = replace(hex(zeroblob(1100)), '0', 'e') WHERE id = 300",
        "UPDATE wide SET body = 'short' WHERE id = 300",
    ] {
        pair.both(sql);
        pair.is_intact(sql);
    }
    for probe in PROBES {
        pair.answers_agree(probe);
    }
    // The same answers after a reopen, because a delta row's reference is
    // replayed from the log rather than rebuilt from values.
    pair.engine.reopen().expect("the file reopens");
    pair.is_intact("a reopen after delta references");
    for probe in PROBES {
        pair.answers_agree(probe);
    }
}

#[test]
fn a_created_index_over_a_table_of_large_values_is_correct() {
    let Some(mut pair) = pair("index", 40) else {
        return no_oracle();
    };
    // The index's own key is short, so nothing in it goes out of line - but the
    // build reads the table, whose values do.
    pair.both("CREATE INDEX wide_body_head ON wide (tag, id)");
    pair.is_intact("a CREATE INDEX over out-of-line values");
    pair.answers_agree("SELECT id FROM wide WHERE tag = 'tag3' ORDER BY id");
    pair.answers_agree("SELECT tag, count(*) FROM wide GROUP BY tag ORDER BY tag");
    for probe in PROBES {
        pair.answers_agree(probe);
    }
}

/// Many out-of-line rows written through one prepared statement inside one
/// transaction, which is what a bulk load is.
///
/// **This is the shape every other test in this file misses, and it is the one a
/// migration takes.** The suite above writes out-of-line values one statement at
/// a time - and a statement that commits leaves a leaf with nothing in its delta
/// area to trip over. `inillucent migrate` binds and steps *one* prepared
/// `INSERT` thousands of times inside a single transaction, so the delta area
/// fills with extent references and the leaf has to compact while they are still
/// there. Reading a delta row to compact it is what refused, with `this value is
/// stored out of line; read the leaf's extents through the tree first` - which
/// reached the operator as `public.attachment could not be copied` on the first
/// table of a real 5.8 GB database.
///
/// It needs no oracle: the claim is that the write succeeds and reads back byte
/// for byte, and this engine is the only thing that has to be asked.
#[test]
fn a_bulk_load_of_out_of_line_values_in_one_transaction_succeeds() {
    let directory = scratch("bulk");
    let path = directory.join("bulk.rdb");
    let database = inillucent::Database::open(&path).expect("a database opens");
    let connection = database.connect();
    // **A TEXT key, arriving in no order**, because that is what a migration
    // does: `attachment.id` is a UUID and a server hands its rows back in heap
    // order, so a key lands in the middle of a leaf that is already there and
    // the leaf has to split. An INTEGER key written 1, 2, 3 only ever appends,
    // and an append never reads a neighbouring row back.
    connection
        .execute_batch("CREATE TABLE wide (id TEXT PRIMARY KEY, body TEXT)")
        .expect("the table is created");

    // Longer than an eighth of the default 32 KiB page, so every value spills,
    // and distinct per row so a value that came back from the wrong row is a
    // failure rather than a coincidence.
    // **The sizes matter as much as the count.** A value a little past the spill
    // threshold occupies one extent page; a value of a megabyte or two occupies
    // many, and the real corpus that found this holds attachment text up to
    // 1,918,953 bytes. Every eighty-third row here is that shape, so the leaf
    // carries short values, spilled values and multi-page spilled values at once.
    let rows = 2_000usize;
    let body = |id: usize| {
        let units = if id.is_multiple_of(83) {
            250_000
        } else {
            1_000
        };
        format!("{id:08}").repeat(units)
    };
    let total: usize = (1..=rows).map(|id| body(id).len()).sum();
    // A hash of the row number, hyphenated like a UUID, so the keys arrive in an
    // order unrelated to the order they are written in.
    let key = |id: usize| {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in id.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}-{id:08}")
    };

    connection
        .execute_batch("BEGIN")
        .expect("a transaction opens");
    let mut statement = connection
        .prepare("INSERT INTO wide (id, body) VALUES (?1, ?2)")
        .expect("the insert prepares");
    for id in 1..=rows {
        statement.clear_bindings();
        statement
            .bind(1, OwnedDatum::Text(key(id).into_bytes()))
            .expect("the key binds");
        statement
            .bind(2, OwnedDatum::Text(body(id).into_bytes()))
            .expect("the value binds");
        statement.step().unwrap_or_else(|error| {
            panic!(
                "row {id} could not be written: {}",
                error.detail().unwrap_or_else(|| error.message())
            )
        });
        statement.reset();
    }
    drop(statement);
    connection
        .execute_batch("COMMIT")
        .expect("the transaction commits");

    // Read back from a fresh open, because a value answered out of the pool that
    // wrote it is not evidence that it reached the file.
    let _ = connection;
    database.checkpoint().expect("the log folds into the file");
    drop(database);
    let reopened = inillucent::Database::open(&path).expect("the file reopens");
    reopened.check().expect("every tree walks in key order");
    let connection = reopened.connect();
    let counted = connection
        .query("SELECT count(*), sum(length(body)) FROM wide")
        .expect("the aggregate answers");
    assert_eq!(
        counted.first().and_then(|row| row.first()),
        Some(&OwnedDatum::Int(rows as i64)),
        "every row is there"
    );
    assert_eq!(
        counted.first().and_then(|row| row.get(1)),
        Some(&OwnedDatum::Int(total as i64)),
        "every value is its whole length"
    );
    for id in [1usize, 2, 83, 166, 999, 1_000, 1_660, 1_999, rows] {
        let read = connection
            .query(&format!("SELECT body FROM wide WHERE id = '{}'", key(id)))
            .expect("the point read answers");
        assert_eq!(
            read.first().and_then(|row| row.first()),
            Some(&OwnedDatum::Text(body(id).into_bytes())),
            "row {id} reads back byte for byte"
        );
    }
}

/// Narrows the bulk load failure to the position a key lands in, not its type.
///
/// Three inserts, each of a value past the spill threshold, into a table with an
/// INTEGER key. The first goes in, the second is written with a *lower* key, so
/// it lands before a row that is already in the leaf. If only the second one
/// fails, the fault is in reading a neighbouring row back while making room, and
/// nothing to do with TEXT keys or with how many rows there are.
#[test]
fn an_out_of_line_row_written_before_an_existing_one() {
    let directory = scratch("before");
    let path = directory.join("before.rdb");
    let database = inillucent::Database::open(&path).expect("a database opens");
    let connection = database.connect();
    connection
        .execute_batch("CREATE TABLE wide (id INTEGER PRIMARY KEY, body TEXT)")
        .expect("the table is created");
    let body = |id: i64| format!("{id:08}").repeat(1_000);
    let mut statement = connection
        .prepare("INSERT INTO wide (id, body) VALUES (?1, ?2)")
        .expect("the insert prepares");
    for id in [100i64, 50, 150, 25] {
        statement.clear_bindings();
        statement
            .bind(1, OwnedDatum::Int(id))
            .expect("the key binds");
        statement
            .bind(2, OwnedDatum::Text(body(id).into_bytes()))
            .expect("the value binds");
        statement.step().unwrap_or_else(|error| {
            panic!(
                "key {id} could not be written: {}",
                error.detail().unwrap_or_else(|| error.message())
            )
        });
        statement.reset();
    }
    drop(statement);
    let read = connection
        .query("SELECT id, length(body) FROM wide ORDER BY id")
        .expect("the scan answers");
    assert_eq!(read.len(), 4, "all four rows are there");
}

/// The smallest case: two rows, a TEXT primary key, one value past the threshold.
///
/// The bulk load fails on its second row and the same load with an INTEGER key
/// succeeds, so this asks the question with everything else removed. Four
/// variants, so the answer says which part matters: the key type, and whether
/// the spilled value is in the row being written or in the row already there.
#[test]
fn two_rows_with_a_text_key_and_a_spilled_value() {
    let cases: [(&str, &str, bool, bool); 4] = [
        ("text key, both spilled", "TEXT", true, true),
        ("text key, only the first spilled", "TEXT", true, false),
        ("text key, only the second spilled", "TEXT", false, true),
        ("integer key, both spilled", "INTEGER", true, true),
    ];
    let mut failures = Vec::new();
    for (name, key_type, first_big, second_big) in cases {
        let directory = scratch(&format!("two-{}", name.replace(' ', "-").replace(',', "")));
        let path = directory.join("two.rdb");
        let database = inillucent::Database::open(&path).expect("a database opens");
        let connection = database.connect();
        connection
            .execute_batch(&format!(
                "CREATE TABLE wide (id {key_type} PRIMARY KEY, body TEXT)"
            ))
            .expect("the table is created");
        let mut statement = connection
            .prepare("INSERT INTO wide (id, body) VALUES (?1, ?2)")
            .expect("the insert prepares");
        for (position, big) in [(1i64, first_big), (2i64, second_big)] {
            statement.clear_bindings();
            let key = if key_type == "TEXT" {
                OwnedDatum::Text(format!("key-{position:04}").into_bytes())
            } else {
                OwnedDatum::Int(position)
            };
            statement.bind(1, key).expect("the key binds");
            let value = if big {
                "v".repeat(8_000)
            } else {
                String::from("short")
            };
            statement
                .bind(2, OwnedDatum::Text(value.into_bytes()))
                .expect("the value binds");
            if let Err(error) = statement.step() {
                failures.push(format!(
                    "{name}: row {position} refused with {}",
                    error.detail().unwrap_or_else(|| error.message())
                ));
                break;
            }
            statement.reset();
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
