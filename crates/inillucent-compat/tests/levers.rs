//! The optimization arms: they change the plan, and they do not change the
//! answer.
//!
//! Invariant: an optimization is only measurable if it can be switched off,
//! and only trustworthy if switching it off changes nothing a caller can see.
//! Both halves are checked here on every lever, because either one alone is
//! worthless - an arm that changes no plan measures nothing, and an arm that
//! changes an answer is not an optimization, it is a bug with a benchmark
//! attached.
//!
//! This is the correctness shard the release scorecard's arms are run against.
//! Every statement it uses is one the scorecard measures.

use inillucent_legacy::{Connection, Database, Levers, Value};

/// The rows the fixture holds.
const ROWS: i64 = 2_000;

/// Builds the fixture: the scorecard's own table shape, at a size a test can
/// afford.
fn fixture(path: &std::path::Path) -> Database {
    let database = Database::open(path).expect("the database opens");
    let connection = database.connect().expect("it connects");
    connection
        .execute_batch(
            "CREATE TABLE main_table(id INTEGER PRIMARY KEY, key INTEGER NOT NULL, \
             category INTEGER NOT NULL, label TEXT NOT NULL, payload BLOB);\
             CREATE INDEX main_key ON main_table(key);\
             CREATE INDEX main_category ON main_table(category, key);\
             CREATE TABLE digits(n INTEGER PRIMARY KEY);\
             INSERT INTO digits(n) VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);",
        )
        .expect("the schema is created");
    connection
        .execute_batch(&format!(
            "INSERT INTO main_table(id, key, category, label, payload) \
             SELECT seq, (seq * 2654435761) % {ROWS}, seq % 64, \
                    'row ' || seq || ' lorem ipsum dolor sit amet', zeroblob(48) \
             FROM (SELECT d0.n + d1.n * 10 + d2.n * 100 + d3.n * 1000 AS seq \
                   FROM digits d0, digits d1, digits d2, digits d3) \
             WHERE seq <= {ROWS}"
        ))
        .expect("the rows are inserted");
    database
}

/// Returns every row a query produces, rendered so two runs can be compared.
fn answer(connection: &Connection, sql: &str) -> String {
    let mut statement = connection.prepare(sql).expect("it prepares");
    let mut out = String::new();
    while statement.step().expect("it steps") {
        for value in statement.row() {
            match value {
                Value::Null => out.push_str("|NULL"),
                Value::Integer(number) => out.push_str(&format!("|{number}")),
                Value::Real(number) => out.push_str(&format!("|{number:.6}")),
                Value::Text(text) => {
                    out.push('|');
                    out.push_str(&String::from_utf8_lossy(&text.utf8_bytes()));
                }
                Value::Blob(bytes) => out.push_str(&format!("|blob:{}", bytes.len())),
            }
        }
        out.push('\n');
    }
    out
}

/// Returns which levers one statement's plan used.
fn used(connection: &Connection, sql: &str) -> u32 {
    connection
        .prepare(sql)
        .expect("it prepares")
        .optimizations_used()
}

/// The reads the covering-index lever is about.
const COVERING_READS: [&str; 3] = [
    "SELECT count(key) FROM main_table WHERE key BETWEEN 100 AND 300",
    "SELECT key FROM main_table WHERE key BETWEEN 400 AND 900 ORDER BY key",
    "SELECT category, count(*) FROM main_table WHERE category BETWEEN 2 AND 8 GROUP BY category",
];

/// The reads the ordered-walk lever is about.
const ORDERED_READS: [&str; 3] = [
    "SELECT id FROM main_table ORDER BY id DESC LIMIT 50",
    "SELECT id FROM main_table WHERE id <= 900 ORDER BY id DESC LIMIT 20",
    "SELECT key FROM main_table WHERE key BETWEEN 100 AND 300 ORDER BY key",
];

/// The reads the streaming-group lever is about.
const STREAMED_GROUPS: [&str; 3] = [
    "SELECT category, count(*) FROM main_table GROUP BY category",
    "SELECT category, count(*), sum(key) FROM main_table GROUP BY category ORDER BY category",
    "SELECT DISTINCT category FROM main_table ORDER BY category",
];

/// The reads the fused-bytecode lever is about.
///
/// Every one of these computes a value into a register and copies it somewhere
/// - a result row, an aggregate's argument - which is the shape the fold is
/// for.
const FUSED_READS: [&str; 3] = [
    "SELECT id, key, category FROM main_table WHERE id = 40",
    "SELECT count(*), sum(key), max(category) FROM main_table",
    "SELECT key + 1, label FROM main_table WHERE id BETWEEN 10 AND 20",
];

/// The writes the indexed-write lever is about.
const INDEXED_WRITES: [&str; 3] = [
    "UPDATE main_table SET category = category + 1 WHERE key BETWEEN 10 AND 40",
    "UPDATE main_table SET label = 'changed' WHERE id = 77",
    "DELETE FROM main_table WHERE key BETWEEN 1500 AND 1520",
];

/// Turning the covering-index lever off changes the plan and not the answer.
#[test]
fn the_covering_index_arm_changes_the_plan_and_not_the_answer() {
    let directory = std::env::temp_dir().join("inillucent-levers-covering");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is made");
    let database = fixture(&directory.join("arm.db"));
    let connection = database.connect().expect("it connects");

    for sql in COVERING_READS {
        connection.disable_optimizations(0);
        let on = used(&connection, sql);
        let with = answer(&connection, sql);

        connection.disable_optimizations(Levers::COVERING_INDEX);
        let off = used(&connection, sql);
        let without = answer(&connection, sql);

        assert_eq!(
            on & Levers::COVERING_INDEX,
            Levers::COVERING_INDEX,
            "the lever should be used with it on: {sql}"
        );
        assert_eq!(
            off & Levers::COVERING_INDEX,
            0,
            "the lever should be gone with it off: {sql}"
        );
        assert_eq!(
            with, without,
            "the two arms disagree about the answer: {sql}"
        );
    }
    let _ = std::fs::remove_dir_all(&directory);
}

/// Turning the ordered-walk lever off changes the plan and not the answer.
///
/// This one is the most dangerous of the three to get wrong, because its
/// failure is silent: rows in the wrong order look exactly like rows.
#[test]
fn the_ordered_walk_arm_changes_the_plan_and_not_the_answer() {
    let directory = std::env::temp_dir().join("inillucent-levers-ordered");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is made");
    let database = fixture(&directory.join("arm.db"));
    let connection = database.connect().expect("it connects");

    for sql in ORDERED_READS {
        connection.disable_optimizations(0);
        let on = used(&connection, sql);
        let with = answer(&connection, sql);

        connection.disable_optimizations(Levers::ORDERED_WALK);
        let off = used(&connection, sql);
        let without = answer(&connection, sql);

        assert_eq!(
            on & Levers::ORDERED_WALK,
            Levers::ORDERED_WALK,
            "the lever should be used with it on: {sql}"
        );
        assert_eq!(
            off & Levers::ORDERED_WALK,
            0,
            "the lever should be gone with it off: {sql}"
        );
        assert_eq!(
            with, without,
            "the two arms disagree about the answer: {sql}"
        );
    }
    let _ = std::fs::remove_dir_all(&directory);
}

/// Turning the streaming-group lever off changes the plan and not the answer.
#[test]
fn the_streaming_group_arm_changes_the_plan_and_not_the_answer() {
    let directory = std::env::temp_dir().join("inillucent-levers-grouped");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is made");
    let database = fixture(&directory.join("arm.db"));
    let connection = database.connect().expect("it connects");

    for sql in STREAMED_GROUPS {
        connection.disable_optimizations(0);
        let on = used(&connection, sql);
        let with = answer(&connection, sql);

        connection.disable_optimizations(Levers::STREAMING_GROUP);
        let off = used(&connection, sql);
        let without = answer(&connection, sql);

        assert_eq!(
            on & Levers::STREAMING_GROUP,
            Levers::STREAMING_GROUP,
            "the lever should be used with it on: {sql}"
        );
        assert_eq!(
            off & Levers::STREAMING_GROUP,
            0,
            "the lever should be gone with it off: {sql}"
        );
        assert_eq!(
            with, without,
            "the two arms disagree about the answer: {sql}"
        );
    }
    let _ = std::fs::remove_dir_all(&directory);
}

/// Turning the fused-bytecode lever off changes the program and not the answer.
///
/// The other arms change which structures a plan builds; this one changes the
/// instructions themselves, so it is the one where a mistake is a wrong value
/// rather than a slow query.
#[test]
fn the_fused_bytecode_arm_changes_the_program_and_not_the_answer() {
    let directory = std::env::temp_dir().join("inillucent-levers-fused");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is made");
    let database = fixture(&directory.join("arm.db"));
    let connection = database.connect().expect("it connects");

    for sql in FUSED_READS {
        connection.disable_optimizations(0);
        let on = used(&connection, sql);
        let short = connection
            .prepare(sql)
            .expect("it prepares")
            .instruction_count();
        let with = answer(&connection, sql);

        connection.disable_optimizations(Levers::FUSED_BYTECODE);
        let off = used(&connection, sql);
        let long = connection
            .prepare(sql)
            .expect("it prepares")
            .instruction_count();
        let without = answer(&connection, sql);

        assert_eq!(
            on & Levers::FUSED_BYTECODE,
            Levers::FUSED_BYTECODE,
            "the lever should be used with it on: {sql}"
        );
        assert_eq!(
            off & Levers::FUSED_BYTECODE,
            0,
            "the lever should be gone with it off: {sql}"
        );
        assert!(
            short < long,
            "the folded program should be shorter: {sql} ({short} against {long})"
        );
        assert_eq!(
            with, without,
            "the two arms disagree about the answer: {sql}"
        );
    }
    let _ = std::fs::remove_dir_all(&directory);
}

/// Turning the indexed-write lever off changes the plan and not the outcome.
///
/// The two arms run against separate databases built the same way, because a
/// write cannot be run twice against one: the second run would see the first
/// one's rows. The comparison is of the whole table afterwards, which is what
/// "the write did the same thing" actually means.
#[test]
fn the_indexed_write_arm_changes_the_plan_and_not_the_outcome() {
    let directory = std::env::temp_dir().join("inillucent-levers-writes");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is made");
    let survey = "SELECT id, key, category, label FROM main_table ORDER BY id";

    let mut outcomes = Vec::new();
    let mut masks = Vec::new();
    for (name, mask) in [("on", 0), ("off", Levers::INDEXED_WRITE)] {
        let database = fixture(&directory.join(format!("{name}.db")));
        let connection = database.connect().expect("it connects");
        connection.disable_optimizations(mask);
        let mut seen = 0;
        for sql in INDEXED_WRITES {
            seen |= used(&connection, sql);
            connection.execute_batch(sql).expect("the write runs");
        }
        masks.push(seen);
        outcomes.push(answer(&connection, survey));
    }

    assert_eq!(
        masks.first().copied().unwrap_or(0) & Levers::INDEXED_WRITE,
        Levers::INDEXED_WRITE,
        "the lever should be used with it on"
    );
    assert_eq!(
        masks.get(1).copied().unwrap_or(0) & Levers::INDEXED_WRITE,
        0,
        "the lever should be gone with it off"
    );
    assert_eq!(
        outcomes.first(),
        outcomes.get(1),
        "the two arms left different tables behind"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// A statement prepared before the arm moved keeps the arm it was compiled
/// under, and a statement prepared after it gets the new one.
///
/// This is why the mask lives in the program's dependencies: a cache that
/// compared only the schema cookie would hand back a plan built under the other
/// arm, and the measurement would silently be of a mixture.
#[test]
fn a_prepared_statement_keeps_the_arm_it_was_compiled_under() {
    let directory = std::env::temp_dir().join("inillucent-levers-prepared");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is made");
    let database = fixture(&directory.join("arm.db"));
    let connection = database.connect().expect("it connects");
    let sql = COVERING_READS.first().copied().unwrap_or("SELECT 1");

    connection.disable_optimizations(0);
    let early = connection.prepare(sql).expect("it prepares");
    connection.disable_optimizations(Levers::COVERING_INDEX);
    let late = connection.prepare(sql).expect("it prepares");

    assert_eq!(
        early.optimizations_used() & Levers::COVERING_INDEX,
        Levers::COVERING_INDEX
    );
    assert_eq!(late.optimizations_used() & Levers::COVERING_INDEX, 0);
    let _ = std::fs::remove_dir_all(&directory);
}

/// Bounding the checkpoint moves the same pages, and loses none of them.
///
/// The bound changes when work happens rather than what it is, so the assertion
/// is equality: the two arms must leave databases that hold the same rows and
/// pass the same integrity check. A bounded copy that dropped a frame would
/// look exactly like a faster checkpoint until something read the page.
///
/// The measurement says the bound buys nothing, so it is off by default. This
/// is what keeps the mechanism honest anyway: a tunable nothing exercises is
/// a tunable that quietly stops working.
#[test]
fn the_checkpoint_arm_moves_the_same_pages() {
    let directory = std::env::temp_dir().join("inillucent-levers-checkpoint");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is made");

    let mut contents = Vec::new();
    for (name, budget) in [("spread", Some(100u32)), ("at-once", None)] {
        let path = directory.join(format!("{name}.db"));
        let database = Database::open(&path).expect("the database opens");
        let connection = database.connect().expect("it connects");
        connection
            .set_checkpoint_budget(budget)
            .expect("the budget is set");
        connection
            .execute_batch(
                "PRAGMA journal_mode=wal;                 PRAGMA synchronous=normal;                 CREATE TABLE t(id INTEGER PRIMARY KEY, label TEXT, payload BLOB);",
            )
            .expect("the schema is created");
        // Enough single-row transactions to cross the thousand-frame threshold
        // several times over, so the bounded copy runs and resumes repeatedly.
        for row in 0..4_000 {
            connection
                .execute_batch(&format!(
                    "INSERT INTO t(id, label, payload) VALUES ({row}, 'row {row}', zeroblob(256))"
                ))
                .expect("the row is inserted");
        }
        drop(connection);
        drop(database);

        // Reopened, because what matters is what reached the file rather than
        // what a live connection can still see in its own log.
        let database = Database::open(&path).expect("the database reopens");
        let connection = database.connect().expect("it reconnects");
        let integrity = answer(&connection, "PRAGMA integrity_check");
        assert_eq!(integrity.trim(), "|ok", "{name}: {integrity}");
        contents.push(answer(
            &connection,
            "SELECT count(*), sum(id), sum(length(label)) FROM t",
        ));
    }
    assert_eq!(
        contents.first(),
        contents.get(1),
        "the two arms left different databases"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// The mask only ever names levers this build has.
#[test]
fn an_unknown_lever_is_ignored_rather_than_stored() {
    let levers = Levers::without(u32::MAX);
    assert_eq!(levers.disabled(), Levers::EVERY);
    assert_eq!(
        levers.names_disabled(),
        vec![
            "covering-index",
            "indexed-write",
            "ordered-walk",
            "streaming-group",
            "fused-bytecode"
        ]
    );
    assert!(Levers::all().names_disabled().is_empty());
    assert!(Levers::all().has(Levers::COVERING_INDEX));
}
