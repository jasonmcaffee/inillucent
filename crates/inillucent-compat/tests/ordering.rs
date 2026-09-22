//! `ORDER BY`, `GROUP BY` and `DISTINCT` answered by the walk instead of by a
//! sorter or a set, graded against the pinned oracle.
//!
//! Invariant: skipping the sorter is only ever a speed decision, never an
//! answer one. Every statement here is one where the planner may decide the access
//! path already produces the requested order - and every one of them is graded
//! on the rows *in order* against SQLite 3.53.4's own answer, because an
//! ordering optimisation that gets it wrong returns rows in the wrong order and
//! nothing about the result looks wrong.
//!
//! The fixture is built to make that failure visible rather than lucky: NULLs
//! in every indexed column so their placement matters, a `NOCASE` index so a
//! collation mismatch shows, a descending index so a direction mismatch shows,
//! ties on every key so the tie-break shows, and a two-column index so a
//! partial prefix match shows.

use std::path::{Path, PathBuf};

use inillucent_compat::facade::Database;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::rendering::tagged as render;
use inillucent_compat::workspace_root;

/// Returns the pinned oracle binary, when it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// The schema every statement here is graded against.
const SCHEMA: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, name TEXT, grp INTEGER, note TEXT)",
    "CREATE INDEX t_k ON t (k)",
    "CREATE INDEX t_name_nocase ON t (name COLLATE NOCASE)",
    "CREATE INDEX t_desc ON t (k DESC)",
    "CREATE INDEX t_grp_k ON t (grp, k)",
    "CREATE TABLE side (id INTEGER PRIMARY KEY, owner INTEGER, tag TEXT)",
    "CREATE INDEX side_owner ON side (owner)",
    "INSERT INTO t VALUES (1, 30, 'Ada', 1, 'x')",
    "INSERT INTO t VALUES (2, 10, 'bob', 1, NULL)",
    "INSERT INTO t VALUES (3, NULL, 'CAI', 2, 'y')",
    "INSERT INTO t VALUES (4, 10, NULL, 2, 'z')",
    "INSERT INTO t VALUES (5, 20, 'dee', 1, NULL)",
    "INSERT INTO t VALUES (6, NULL, 'Eve', 3, 'w')",
    "INSERT INTO t VALUES (7, 30, 'fay', 2, 'v')",
    "INSERT INTO t VALUES (9, 40, 'gus', 3, NULL)",
    "INSERT INTO side VALUES (1, 1, 'p')",
    "INSERT INTO side VALUES (2, 2, 'q')",
    "INSERT INTO side VALUES (3, 2, 'r')",
    "INSERT INTO side VALUES (4, 9, 's')",
];

/// The statements. Every one of them names an order, so every one is compared
/// in order.
const STATEMENTS: &[&str] = &[
    // The rowid, both directions and both spellings, with and without a range.
    "SELECT id FROM t ORDER BY id",
    "SELECT id FROM t ORDER BY id DESC",
    "SELECT id FROM t ORDER BY rowid DESC",
    "SELECT id FROM t WHERE id <= 5 ORDER BY id DESC",
    "SELECT id FROM t WHERE id < 5 ORDER BY id DESC",
    "SELECT id FROM t WHERE id >= 3 ORDER BY id DESC",
    "SELECT id FROM t WHERE id BETWEEN 2 AND 6 ORDER BY id DESC",
    "SELECT id FROM t WHERE id BETWEEN 2 AND 6 ORDER BY id",
    "SELECT id FROM t WHERE id <= 5 ORDER BY id DESC LIMIT 2",
    "SELECT id FROM t WHERE id <= 5 ORDER BY id DESC LIMIT 2 OFFSET 1",
    "SELECT id FROM t ORDER BY id DESC LIMIT 3",
    "SELECT id FROM t WHERE id = 4 ORDER BY id DESC",
    // An indexed column, where the NULLs and the ties decide the answer.
    "SELECT k, id FROM t ORDER BY k",
    "SELECT k, id FROM t ORDER BY k DESC",
    "SELECT k, id FROM t ORDER BY k, id",
    "SELECT k, id FROM t ORDER BY k DESC, id DESC",
    "SELECT k, id FROM t WHERE k >= 20 ORDER BY k DESC",
    "SELECT k, id FROM t WHERE k BETWEEN 10 AND 30 ORDER BY k DESC",
    "SELECT k, id FROM t WHERE k IS NOT NULL ORDER BY k",
    "SELECT k, id FROM t ORDER BY k NULLS FIRST, id",
    "SELECT k, id FROM t ORDER BY k NULLS LAST, id",
    "SELECT k, id FROM t ORDER BY k DESC NULLS FIRST, id",
    "SELECT k, id FROM t ORDER BY k DESC NULLS LAST, id",
    // A descending index: the same column, held the other way round.
    "SELECT k, id FROM t WHERE k > 5 ORDER BY k DESC",
    "SELECT k, id FROM t WHERE k > 5 ORDER BY k",
    // A collation the index does not hold the column in.
    "SELECT name, id FROM t ORDER BY name",
    "SELECT name, id FROM t ORDER BY name COLLATE NOCASE",
    "SELECT name, id FROM t ORDER BY name COLLATE NOCASE DESC",
    "SELECT name, id FROM t ORDER BY name COLLATE BINARY",
    // A two-column index: a prefix, the whole key, and a mixed direction that
    // no single walk can produce.
    "SELECT grp, k, id FROM t ORDER BY grp, k",
    "SELECT grp, k, id FROM t ORDER BY grp DESC, k DESC",
    "SELECT grp, k, id FROM t ORDER BY grp, k DESC",
    "SELECT grp, k, id FROM t ORDER BY grp",
    "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY k",
    "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY k DESC",
    "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY grp, k DESC",
    "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY k DESC LIMIT 1",
    // Grouping and de-duplicating that the walk can deliver, where the NULLs,
    // the ties and the empty groups decide the answer.
    "SELECT k, count(*) FROM t GROUP BY k ORDER BY k",
    "SELECT k, count(*), sum(id), max(name) FROM t GROUP BY k ORDER BY k",
    "SELECT k, count(*) FROM t GROUP BY k ORDER BY k DESC",
    "SELECT k, count(*) FROM t WHERE k IS NOT NULL GROUP BY k ORDER BY k",
    "SELECT k, count(*) FROM t WHERE k > 100 GROUP BY k ORDER BY k",
    "SELECT k, count(*) FROM t GROUP BY k HAVING count(*) > 1 ORDER BY k",
    "SELECT grp, k, count(*) FROM t GROUP BY grp, k ORDER BY grp, k",
    "SELECT k, grp, count(*) FROM t GROUP BY k, grp ORDER BY grp, k",
    "SELECT grp, count(*) FROM t GROUP BY grp ORDER BY grp",
    "SELECT k FROM t WHERE k = 10 GROUP BY k",
    "SELECT k, count(*) FROM t WHERE k = 999 GROUP BY k",
    "SELECT DISTINCT k FROM t ORDER BY k",
    "SELECT DISTINCT k FROM t ORDER BY k DESC",
    "SELECT DISTINCT grp, k FROM t ORDER BY grp, k",
    "SELECT DISTINCT k FROM t WHERE k IS NOT NULL ORDER BY k",
    "SELECT DISTINCT name FROM t ORDER BY name COLLATE NOCASE",
    "SELECT DISTINCT id FROM t ORDER BY id DESC",
    // The collation trap: a NOCASE index puts `Ada` and `ADA` together, and a
    // BINARY grouping over it would then treat them as one row.
    "SELECT DISTINCT name FROM t ORDER BY name",
    "SELECT name, count(*) FROM t GROUP BY name ORDER BY name",
    "SELECT name COLLATE NOCASE, count(*) FROM t GROUP BY name COLLATE NOCASE",
    // Shapes where the order is not the walk's, and the sort has to stay.
    "SELECT k, id FROM t ORDER BY note",
    "SELECT k, id FROM t ORDER BY k + 1",
    "SELECT k, count(*) FROM t GROUP BY k ORDER BY k DESC",
    "SELECT DISTINCT k FROM t ORDER BY k DESC",
    "SELECT k FROM t UNION ALL SELECT k FROM t ORDER BY k DESC",
    "SELECT t.id, side.tag FROM t JOIN side ON side.owner = t.id ORDER BY t.id DESC",
    "SELECT t.id, side.tag FROM t LEFT JOIN side ON side.owner = t.id ORDER BY t.id DESC",
    // A window reorders the rows after the walk, so the outer `ORDER BY`
    // still has to be answered by a sort. This statement was retired on the
    // reading that "the shipping engine refuses every `OVER (...)` clause
    // outright"; what refused it was `compiled::try_compile` failing to bail
    // out on `plan.select.windows` the way `prepare_any` does (task-1932, H1),
    // and the evaluator behind it answers all forty-one forms
    // `windows_match_the_oracle` grades.
    "SELECT id, row_number() OVER (ORDER BY k, id) FROM t ORDER BY id",
    "SELECT id FROM (SELECT id FROM t ORDER BY id DESC) ORDER BY id",
    // An empty range, and one whose bounds cross.
    "SELECT id FROM t WHERE id BETWEEN 6 AND 2 ORDER BY id DESC",
    "SELECT k FROM t WHERE k > 1000 ORDER BY k DESC",
    "SELECT k FROM t WHERE k < -1000 ORDER BY k",
];

/// Renders one of the oracle's tagged values the same way.
fn render_tagged(value: &TaggedValue) -> String {
    match value {
        TaggedValue::Null => "null".to_string(),
        TaggedValue::Integer(integer) => format!("int:{integer}"),
        TaggedValue::Real(real) => format!("real:{real:?}"),
        TaggedValue::Text(bytes) => format!("text:{}", String::from_utf8_lossy(bytes)),
        TaggedValue::Blob(bytes) => format!(
            "blob:{}",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

/// Runs a statement through inillucent, returning its rows or its failure.
fn inillucent_rows(
    connection: &inillucent_compat::facade::Connection,
    sql: &str,
) -> Result<Vec<String>, String> {
    let mut statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(reason) => return Err(format!("{reason:?}")),
    };
    let mut rows = Vec::new();
    loop {
        match statement.step() {
            Ok(true) => rows.push(
                statement
                    .row()
                    .iter()
                    .map(render)
                    .collect::<Vec<String>>()
                    .join("|"),
            ),
            Ok(false) => break,
            Err(reason) => return Err(format!("{reason:?}")),
        }
    }
    Ok(rows)
}

/// Builds the graded database with the oracle and returns a driver on it.
fn build(directory: &Path, tag: &str) -> Option<(Driver, PathBuf)> {
    let program = oracle_path()?;
    let database = directory.join(format!("ordering-{tag}.db"));
    let _ = std::fs::remove_file(&database);
    let mut driver = Driver::start("sqlite", &program).ok()?;
    driver.send(&Op::Hello).ok()?;
    driver
        .send(&Op::Open(database.display().to_string()))
        .ok()?;
    for statement in SCHEMA {
        let observation = driver.send(&Op::Exec((*statement).to_string())).ok()?;
        assert!(
            observation.ok,
            "the oracle refused the fixture schema: {statement}: {}",
            observation.message
        );
    }
    Some((driver, database))
}

/// Every ordered statement returns the reference's rows, in the reference's
/// order.
#[test]
fn ordered_statements_match_the_oracle() {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ordering");
    let _ = std::fs::create_dir_all(&directory);
    let Some((mut driver, database)) = build(&directory, "rows") else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let handle = Database::import_with_busy_timeout(&database, std::time::Duration::from_secs(5))
        .expect("the fixture opens");
    let connection = handle.session().expect("the connection opens");
    let mut failures = Vec::new();
    for sql in STATEMENTS {
        let observation = driver
            .send(&Op::Query((*sql).to_string()))
            .expect("the oracle answers");
        let ours = inillucent_rows(&connection, sql);
        if !observation.ok {
            if ours.is_ok() {
                failures.push(format!(
                    "{sql}\n  sqlite refused: {}\n  inillucent accepted it",
                    observation.message
                ));
            }
            continue;
        }
        let expected: Vec<String> = observation
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(render_tagged)
                    .collect::<Vec<String>>()
                    .join("|")
            })
            .collect();
        match ours {
            Err(reason) => failures.push(format!("{sql}\n  inillucent failed: {reason}")),
            Ok(actual) if actual != expected => {
                failures.push(format!(
                    "{sql}\n  sqlite:  {expected:?}\n  inillucent: {actual:?}"
                ));
            }
            Ok(_) => {}
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} statements diverged:\n{}",
        failures.len(),
        STATEMENTS.len(),
        failures.join("\n")
    );
}

/// The optimisation actually happens, and only where it should.
///
/// The rows being right is necessary and not sufficient: a planner that quietly
/// sorted everything would pass the test above and be exactly the thing this
/// work was meant to remove. So the plans are read too - `USE TEMP B-TREE FOR
/// ORDER BY` is what a sort looks like in `EXPLAIN QUERY PLAN` - and both
/// halves are asserted: gone where the walk can answer the order, still there
/// where it cannot.
#[test]
fn the_sort_is_skipped_exactly_where_the_walk_answers_the_order() {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ordering");
    let _ = std::fs::create_dir_all(&directory);
    let Some((_driver, database)) = build(&directory, "plans") else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let handle = Database::import_with_busy_timeout(&database, std::time::Duration::from_secs(5))
        .expect("the fixture opens");
    let connection = handle.session().expect("the connection opens");

    let walked = [
        "SELECT id FROM t ORDER BY id",
        "SELECT id FROM t ORDER BY id DESC",
        "SELECT id FROM t WHERE id <= 5 ORDER BY id DESC",
        "SELECT k, id FROM t ORDER BY k",
        "SELECT k, id FROM t ORDER BY k DESC",
        "SELECT grp, k, id FROM t WHERE grp = 2 ORDER BY k DESC",
        "SELECT name, id FROM t ORDER BY name COLLATE NOCASE",
    ];
    // Grouping and de-duplicating the walk delivers, so no temp b-tree is
    // built for either.
    let streamed = [
        "SELECT k, count(*) FROM t GROUP BY k",
        "SELECT k, count(*) FROM t GROUP BY k ORDER BY k",
        "SELECT grp, k, count(*) FROM t GROUP BY grp, k",
        // The group *set*, not the group order: a walk ordered by `(grp, k)`
        // makes every `(grp, k)` pair adjacent, so it groups them written
        // either way round.
        "SELECT k, grp, count(*) FROM t GROUP BY k, grp",
        "SELECT DISTINCT k FROM t",
        "SELECT DISTINCT grp, k FROM t ORDER BY grp, k",
        // Backwards too. The walk runs in reverse, the rows of a key are still
        // adjacent, and the keys still arrive in order - the other order. The
        // pinned SQLite answers both of these off the same covering index with
        // no sort, which is what these two are here to keep true.
        "SELECT k, count(*) FROM t GROUP BY k ORDER BY k DESC",
        "SELECT DISTINCT k FROM t ORDER BY k DESC",
    ];
    // Grouping and de-duplicating it cannot, so one is.
    let collected = [
        // No index holds `note`.
        "SELECT note, count(*) FROM t GROUP BY note",
        "SELECT DISTINCT note FROM t",
        // A BINARY grouping over a NOCASE index would merge `Ada` and `ADA`.
        "SELECT name, count(*) FROM t GROUP BY name",
        "SELECT DISTINCT name FROM t",
        // Distinct over an aggregate is distinct over values no walk produced.
        "SELECT DISTINCT count(*) FROM t GROUP BY k",
    ];
    let sorted = [
        // The order is over an expression, not a column.
        "SELECT k, id FROM t ORDER BY k + 1",
        // A column no index holds.
        "SELECT k, id FROM t ORDER BY note",
        // Two columns of one index, in opposite directions: one walk cannot
        // produce both.
        "SELECT grp, k, id FROM t ORDER BY grp, k DESC",
        // A window reorders the rows after the walk. Grouping and DISTINCT do
        // not, when they stream: they emit one row per key, in key order, so
        // the walk answers the ORDER BY and those cases are in `streamed`
        // above. A statement that is both grouped and DISTINCT still sorts,
        // because the de-duplication then runs on the aggregate output rather
        // than on the walk.
        "SELECT DISTINCT count(*) FROM t GROUP BY k ORDER BY count(*)",
        "SELECT id, row_number() OVER (ORDER BY k, id) FROM t ORDER BY id",
        // A collation the index does not hold the column in.
        "SELECT name, id FROM t ORDER BY name COLLATE BINARY",
    ];

    let sorts = |sql: &str| -> bool {
        let plan = inillucent_rows(&connection, &format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("the plan renders");
        plan.iter().any(|row| row.contains("ORDER BY"))
    };
    for sql in walked {
        assert!(!sorts(sql), "this should be answered by the walk: {sql}");
    }
    for sql in sorted {
        assert!(sorts(sql), "this should still sort: {sql}");
    }

    let builds = |sql: &str, what: &str| -> bool {
        let plan = inillucent_rows(&connection, &format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("the plan renders");
        plan.iter().any(|row| row.contains(what))
    };
    for sql in streamed {
        let what = if sql.contains("DISTINCT") {
            "DISTINCT"
        } else {
            "GROUP BY"
        };
        assert!(
            !builds(sql, what),
            "this should be answered as the rows arrive: {sql}"
        );
    }
    for sql in collected {
        let what = if sql.starts_with("SELECT DISTINCT") {
            "DISTINCT"
        } else {
            "GROUP BY"
        };
        assert!(builds(sql, what), "this should still collect: {sql}");
    }
}

/// The pieces the generated strings are built out of.
///
/// **Chosen for the four ways a string comparison goes wrong**, not for
/// coverage of the alphabet. Case, so a `NOCASE` index and a `BINARY` index
/// disagree about the same pair. A combining mark, so two strings that look
/// identical are different bytes and must stay in byte order rather than in
/// reading order. A codepoint outside the basic plane, which is a surrogate
/// pair to anything comparing UTF-16 and four bytes to anything comparing
/// UTF-8. And a NUL, which no SQL string literal can carry - it is why every
/// value here is inserted as a blob cast to text.
const PIECES: &[&str] = &[
    "a",
    "A",
    "b",
    "B",
    "y",
    "Y",
    "z",
    "Z",
    // é written as one codepoint, and as `e` followed by a combining acute.
    // They read the same and sort far apart.
    "\u{e9}",
    "e\u{301}",
    // Outside the basic multilingual plane: a surrogate pair in UTF-16, four
    // bytes in UTF-8, and the last codepoint there is.
    "\u{1f600}",
    "\u{10ffff}",
    // A NUL in the middle of the text.
    "\0",
    " ",
    "0",
    "9",
    "_",
];

/// Builds `count` distinct strings from a seed.
///
/// Distinct by bytes, because the `BINARY` arms order by `s` alone: a
/// duplicate would make the order partial, and two engines are then free to
/// answer different row orders while both being right.
///
/// @param seed - the seed, which a failure prints so the run can be repeated
/// @param count - how many distinct strings to return
fn generated_strings(seed: u64, count: usize) -> Vec<String> {
    let mut rng = inillucent_base::rng::Rng::new(seed);
    let mut made: Vec<String> = Vec::new();
    // Bounded, because the alphabet is finite and a caller asking for more
    // distinct strings than it can build must not spin.
    for _ in 0..count.saturating_mul(50) {
        if made.len() >= count {
            break;
        }
        let pieces = 1 + rng.below(4) as usize;
        let mut built = String::new();
        for _ in 0..pieces {
            let at = rng.below(PIECES.len() as u64) as usize;
            built.push_str(PIECES.get(at).copied().unwrap_or("a"));
        }
        if !made.contains(&built) {
            made.push(built);
        }
    }
    made
}

/// Renders a string as a SQL expression that carries every byte of it.
///
/// A blob literal cast to text, rather than a quoted string: a quoted string
/// cannot hold a NUL, because the statement is parsed as text that ends at one.
///
/// @param value - the string to render
fn as_text_literal(value: &str) -> String {
    let hex: String = value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect();
    if hex.is_empty() {
        return "''".to_string();
    }
    format!("CAST(x'{hex}' AS TEXT)")
}

/// One generated statement: what to run, which table it reads, and whether the
/// walk is expected to answer the order without a sort.
struct Generated {
    /// The statement.
    sql: &'static str,
    /// Whether it reads `g`, which holds every generated string, or `n`, which
    /// holds only the ones with no NUL in them.
    every_string: bool,
    /// Whether the access path is expected to produce the order.
    walked: bool,
}

/// The statements the generated arm compares, and what each one is for.
///
/// The five together are the three-way section 4.4.14 asks for: index order,
/// scan order, and the oracle's order.
///
/// **The NOCASE arms read a table with no NULs in it**, and that is a recorded
/// difference rather than a convenience -
/// `nocase_stops_at_an_embedded_nul_in_sqlite_and_not_here` below is the case
/// that asserts it.
const GENERATED: &[Generated] = &[
    // BINARY, answered by walking `g_s`. The `s >= ''` is what lets the index
    // be the access path: it is true of every text value, so it selects the
    // whole table through a seek rather than through a scan.
    Generated {
        sql: "SELECT hex(s) FROM g WHERE s >= '' ORDER BY s",
        every_string: true,
        walked: true,
    },
    // BINARY, the same rows with every index taken away.
    Generated {
        sql: "SELECT hex(s) FROM g NOT INDEXED ORDER BY s",
        every_string: true,
        walked: false,
    },
    // NOCASE, answered by walking `n_s`, whose key is
    // `(s COLLATE NOCASE, rowid)` - so the `id` tie-break comes out of the
    // index too. The tie-break is needed because NOCASE makes `a` and `A`
    // equal, and an order with ties is not an order two engines must agree on.
    Generated {
        sql: "SELECT hex(s) FROM n ORDER BY s COLLATE NOCASE, id",
        every_string: false,
        walked: true,
    },
    // NOCASE, scanned and sorted.
    Generated {
        sql: "SELECT hex(s) FROM n NOT INDEXED ORDER BY s COLLATE NOCASE, id",
        every_string: false,
        walked: false,
    },
    // And backwards, which is a different walk of the same index.
    Generated {
        sql: "SELECT hex(s) FROM g WHERE s >= '' ORDER BY s DESC",
        every_string: true,
        walked: true,
    },
];

/// Builds the generated fixture with the oracle and returns a driver on it.
///
/// The rows are written by the oracle, so both engines hold the same bytes:
/// writing them twice would test two writers as well as two readers, and a
/// difference could then be in either.
///
/// Two tables. `g` holds every generated string under a `BINARY` index. `n`
/// holds the ones with no NUL in them under a `NOCASE` index, because the two
/// engines are known to disagree about NOCASE over a string containing a NUL
/// and that difference has its own case.
///
/// @param directory - where to put the database
/// @param strings - the values to insert
fn build_generated(directory: &Path, strings: &[String]) -> Option<(Driver, PathBuf)> {
    let program = oracle_path()?;
    let database = directory.join("ordering-generated.db");
    let _ = std::fs::remove_file(&database);
    let mut driver = Driver::start("sqlite", &program).ok()?;
    driver.send(&Op::Hello).ok()?;
    driver
        .send(&Op::Open(database.display().to_string()))
        .ok()?;
    let mut script: Vec<String> = vec![
        "CREATE TABLE g (id INTEGER PRIMARY KEY, s TEXT)".to_string(),
        "CREATE INDEX g_s ON g (s)".to_string(),
        "CREATE TABLE n (id INTEGER PRIMARY KEY, s TEXT)".to_string(),
        "CREATE INDEX n_s ON n (s COLLATE NOCASE)".to_string(),
    ];
    for (at, value) in strings.iter().enumerate() {
        let id = at.saturating_add(1);
        let literal = as_text_literal(value);
        script.push(format!("INSERT INTO g VALUES ({id}, {literal})"));
        if !value.as_bytes().contains(&0) {
            script.push(format!("INSERT INTO n VALUES ({id}, {literal})"));
        }
    }
    for statement in &script {
        let observation = driver.send(&Op::Exec(statement.clone())).ok()?;
        assert!(
            observation.ok,
            "the oracle refused the generated fixture: {statement}: {}",
            observation.message
        );
    }
    Some((driver, database))
}

/// **Generated strings order the same by index, by scan, and by the oracle.**
///
/// The fixture at the top of this file is eight hand-written names, which is
/// enough to show a NULL placement or a direction mismatch and not enough to
/// show a collation one: every one of them is ASCII, and ASCII is where a
/// wrong comparison is right by accident. This arm generates from an alphabet
/// built out of the four cases that break a comparison - case variants, a
/// combining mark, a codepoint outside the basic plane, and a NUL inside the
/// text (task-2066 section 4.4.14).
///
/// **Three answers, not two.** The same rows are read through the index and
/// with every index taken away, and both are compared to SQLite's. A test that
/// compared only one of them would pass while the index and the scan disagreed
/// with each other, which is the failure this is for: an index whose key order
/// is not the order its collation defines returns the right rows in the wrong
/// order, and nothing about the result looks wrong.
///
/// The plans are asserted too. Without that, both arms could be a sort over a
/// scan and the comparison would be one route against itself. `NOT INDEXED` is
/// what makes the scan arm a scan, and it only started working in task-2068 -
/// before that both arms could have been the same route.
#[test]
fn generated_strings_order_the_same_by_index_by_scan_and_by_the_oracle() {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ordering");
    let _ = std::fs::create_dir_all(&directory);
    // A fixed seed, so a failure is reproducible from the message. The number
    // is arbitrary; what matters is that it never changes silently.
    const SEED: u64 = 20_662_014;
    let strings = generated_strings(SEED, 120);
    assert!(
        strings.len() >= 100,
        "the generator produced {} distinct strings from seed {SEED}, which is too few \
         to order",
        strings.len()
    );
    let without_nul = strings
        .iter()
        .filter(|value| !value.as_bytes().contains(&0))
        .count();
    assert!(
        without_nul < strings.len(),
        "not one of the {} generated strings holds a NUL, so the alphabet is not \
         producing the case it was chosen for",
        strings.len()
    );

    let Some((mut driver, database)) = build_generated(&directory, &strings) else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let handle = Database::import_with_busy_timeout(&database, std::time::Duration::from_secs(5))
        .expect("the generated fixture opens");
    let connection = handle.session().expect("the connection opens");

    let mut failures: Vec<String> = Vec::new();
    let mut answers: Vec<Vec<String>> = Vec::new();
    for case in GENERATED {
        let sql = case.sql;
        let expected_rows = if case.every_string {
            strings.len()
        } else {
            without_nul
        };
        let observation = driver
            .send(&Op::Query(sql.to_string()))
            .expect("the oracle answers");
        assert!(
            observation.ok,
            "the oracle refused {sql}: {}",
            observation.message
        );
        let expected: Vec<String> = observation
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(render_tagged)
                    .collect::<Vec<String>>()
                    .join("|")
            })
            .collect();
        assert_eq!(
            expected.len(),
            expected_rows,
            "{sql}: the oracle returned {} rows and the table holds {expected_rows}",
            expected.len()
        );

        match inillucent_rows(&connection, sql) {
            Err(reason) => failures.push(format!("{sql}\n  inillucent failed: {reason}")),
            Ok(actual) => {
                if actual != expected {
                    failures.push(disagreement(sql, &expected, &actual));
                }
                answers.push(actual);
            }
        }

        // The plan, so that "index order" and "scan order" are two routes and
        // not one route run twice.
        let explained = format!("EXPLAIN QUERY PLAN {sql}");
        let plan = inillucent_rows(&connection, &explained)
            .unwrap_or_else(|reason| panic!("{explained}: {reason}"))
            .join("\n");
        let sorted = plan.contains("USE TEMP B-TREE");
        if case.walked && sorted {
            failures.push(format!(
                "{sql}\n  was expected to be answered by the index walk and was sorted:\n{plan}"
            ));
        }
        if !case.walked && !sorted {
            failures.push(format!(
                "{sql}\n  was expected to be scanned and sorted and was not:\n{plan}"
            ));
        }
    }

    // And the two arms of each collation against each other, which is the
    // comparison the oracle cannot make: it says what the order is, and this
    // says the engine gives the same one whichever way it got there.
    for (walk, scan, what) in [(0usize, 1usize, "BINARY"), (2, 3, "NOCASE")] {
        if let (Some(walked), Some(scanned)) = (answers.get(walk), answers.get(scan)) {
            if walked != scanned {
                failures.push(format!(
                    "the {what} index walk and the {what} scan answered different orders"
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "seed {SEED}, {} strings:\n{}",
        strings.len(),
        failures.join("\n")
    );
}

/// **NOCASE stops at a NUL in SQLite and does not stop here.**
///
/// A known difference, recorded as §1.3 of the testing standard requires, and
/// found by the generated arm above on its first run.
///
/// SQLite's `NOCASE` is `sqlite3StrNICmp`, a C string walk whose loop condition
/// includes `*a != 0`: it stops at the first NUL in the *left* operand and
/// compares whatever the two bytes are at that position, which for two NULs is
/// a tie that falls through to the lengths. So for
/// `x'0061'` (`"\0a"`) against `x'000079'` (`"\0\0y"`), SQLite compares the
/// leading NULs, stops, ties, and answers by length - putting the two-byte
/// string first. This engine compares the second byte and puts the three-byte
/// string first.
///
/// **Why it is recorded rather than matched.** `Collation::NoCase` is declared
/// order preserving in keys, which is what lets an index on
/// `s COLLATE NOCASE` answer an `ORDER BY` by walking rather than by sorting:
/// the key encoder lowercases the bytes and their natural order is the
/// collation's order. SQLite's rule is not reachable that way - under it
/// `"\0a"` sorts before `"\0\0y"` while their lowercased bytes sort the other
/// way - so matching it means either dropping that optimisation for every
/// `NOCASE` index or keeping two orders that disagree. Both are larger than a
/// test, and the second is the thing this file exists to prevent.
///
/// Every string in this workspace that is not built from a blob literal is
/// free of NULs, so the difference is reachable only through
/// `CAST(x'..' AS TEXT)` or a bound parameter holding one.
#[test]
fn nocase_stops_at_an_embedded_nul_in_sqlite_and_not_here() {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ordering");
    let _ = std::fs::create_dir_all(&directory);
    let database = directory.join("ordering-nocase-nul.db");
    let _ = std::fs::remove_file(&database);

    let Some(program) = oracle_path() else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("the oracle answers");
    driver
        .send(&Op::Open(database.display().to_string()))
        .expect("the oracle opens the file");
    for statement in [
        "CREATE TABLE p (id INTEGER PRIMARY KEY, s TEXT)",
        "INSERT INTO p VALUES (1, CAST(x'0061' AS TEXT))",
        "INSERT INTO p VALUES (2, CAST(x'000079' AS TEXT))",
    ] {
        let observation = driver
            .send(&Op::Exec(statement.to_string()))
            .expect("the oracle answers");
        assert!(observation.ok, "{statement}: {}", observation.message);
    }

    let sql = "SELECT hex(s) FROM p ORDER BY s COLLATE NOCASE";
    let observation = driver
        .send(&Op::Query(sql.to_string()))
        .expect("the oracle answers");
    let theirs: Vec<String> = observation
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(render_tagged)
                .collect::<Vec<String>>()
                .join("|")
        })
        .collect();
    assert_eq!(
        theirs,
        vec!["text:0061".to_string(), "text:000079".to_string()],
        "SQLite no longer stops NOCASE at a NUL, so this difference has closed from \
         its side"
    );

    let handle = Database::import_with_busy_timeout(&database, std::time::Duration::from_secs(5))
        .expect("the fixture opens");
    let connection = handle.session().expect("the connection opens");
    let ours = inillucent_rows(&connection, sql).expect("the statement runs");
    assert_eq!(
        ours,
        vec!["text:000079".to_string(), "text:0061".to_string()],
        "NOCASE over a string holding a NUL now answers something other than the byte \
         order this engine has always given it. If it answers SQLite's order, the \
         difference is closed - move the NOCASE arms of \
         `generated_strings_order_the_same_by_index_by_scan_and_by_the_oracle` back \
         onto the table that holds every string, and take the row out of \
         docs/feature-comparison.md."
    );
}

/// Describes where two orders first differ, rather than printing both.
///
/// A hundred and twenty rows of hex on each side is a message nobody reads.
/// This names the first position that differs and the two values at it, which
/// is what a collation failure looks like: everything agrees until one pair.
///
/// @param sql - the statement
/// @param expected - the oracle's rows, in order
/// @param actual - inillucent's rows, in order
fn disagreement(sql: &str, expected: &[String], actual: &[String]) -> String {
    let at = expected
        .iter()
        .zip(actual.iter())
        .position(|(theirs, ours)| theirs != ours);
    match at {
        Some(at) => format!(
            "{sql}\n  rows agree to position {at}, and then\n    sqlite:     {:?}\n    \
             inillucent: {:?}",
            expected.get(at),
            actual.get(at)
        ),
        None => format!(
            "{sql}\n  the orders agree as far as they both go, and there are {} rows \
             against {}",
            expected.len(),
            actual.len()
        ),
    }
}
