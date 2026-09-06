//! Joins, compounds, subqueries and CTEs, graded against the pinned oracle.
//!
//! Invariant: every expectation here is SQLite 3.53.4's own answer, taken from
//! the oracle at run time rather than written out by hand. A hand-written
//! expectation is a second opinion about what SQL means, and the whole point of
//! this suite is that there is only one.
//!
//! When the oracle has not been built the tests skip rather than fail: the
//! conformance suite is where checked-in expectations live, and duplicating
//! them here would mean two places to update when the corpus grows.

use std::path::{Path, PathBuf};

use inillucent_compat::facade::Database;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_value::Value;

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

/// The schema every statement in this file is graded against.
///
/// It is deliberately awkward: nullable columns on both sides of every join,
/// a row in each table that matches nothing in the other, duplicate keys so a
/// set operation has something to de-duplicate, and a `NULL` in the column an
/// `IN` subquery reads so the three-valued case is exercised rather than
/// assumed.
const SCHEMA: &[&str] = &[
    "CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT, team TEXT, score REAL)",
    "CREATE TABLE b (id INTEGER PRIMARY KEY, team TEXT, region TEXT, rank INTEGER)",
    "CREATE TABLE c (k INTEGER, v TEXT)",
    "CREATE INDEX a_team ON a (team)",
    "INSERT INTO a VALUES (1, 'ada', 'blue', 10.5)",
    "INSERT INTO a VALUES (2, 'bob', 'red', -2.0)",
    "INSERT INTO a VALUES (3, 'cai', 'blue', 0.0)",
    "INSERT INTO a VALUES (4, 'dee', NULL, 99.25)",
    "INSERT INTO a VALUES (5, 'eve', 'gone', NULL)",
    "INSERT INTO b VALUES (10, 'blue', 'north', 1)",
    "INSERT INTO b VALUES (11, 'red', 'south', 2)",
    "INSERT INTO b VALUES (12, 'green', 'east', 3)",
    "INSERT INTO b VALUES (13, NULL, 'west', 4)",
    "INSERT INTO c VALUES (1, 'one')",
    "INSERT INTO c VALUES (1, 'one')",
    "INSERT INTO c VALUES (2, 'two')",
    "INSERT INTO c VALUES (NULL, 'null')",
    "INSERT INTO c VALUES (3, NULL)",
    "CREATE VIEW blue AS SELECT id, name, score FROM a WHERE team = 'blue'",
    "CREATE VIEW ranked (who, place) AS SELECT a.name, b.rank FROM a JOIN b ON a.team = b.team",
];

/// Renders one value as a tagged string, so a storage class difference shows.
fn render(value: &Value<'static>) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(integer) => format!("int:{integer}"),
        Value::Real(real) => format!("real:{real:?}"),
        Value::Text(text) => format!("text:{}", String::from_utf8_lossy(&text.utf8_bytes())),
        Value::Blob(blob) => format!(
            "blob:{}",
            blob.raw()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

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
    // One file per test: these run in parallel threads, and a shared file gives
    // whichever test loses the race a "database is locked" that looks like a
    // divergence rather than the collision it is.
    let database = directory.join(format!("advanced-{tag}.db"));
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

/// Grades every statement in a list against the oracle.
///
/// Both engines answer the same statement against the same file, and the
/// comparison is on the rendered rows in order - so a row that came back as an
/// integer where SQLite returned a real is a failure, not a rounding detail.
fn grade(tag: &str, statements: &[&str]) {
    // `CARGO_TARGET_TMPDIR` is set at compile time, not at run time. Reading it
    // with `var_os` returns nothing, and the whole suite then skips silently -
    // which is how these five tests first "passed" in 0.00 seconds.
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("advanced-sql");
    let _ = std::fs::create_dir_all(&directory);
    let Some((mut driver, database)) = build(&directory, tag) else {
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let handle = Database::import_with_busy_timeout(&database, std::time::Duration::from_secs(5))
        .expect("the fixture opens");
    let connection = handle.connect().expect("the connection opens");
    let mut failures = Vec::new();
    for sql in statements {
        let observation = driver
            .send(&Op::Query((*sql).to_string()))
            .expect("the oracle answers");
        let ours = inillucent_rows(&connection, sql);
        if !observation.ok {
            // SQLite refused it, so inillucent must refuse it too. The messages
            // are not compared: they are prose, and this suite grades answers.
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
        // A statement with no `ORDER BY` has no defined row order, so the two
        // answers are compared as multisets. Comparing them in order would
        // grade the access path each engine happened to choose - and SQLite
        // reading a covering index while inillucent scans the table is a
        // difference in speed, not in answer.
        let ordered = sql.to_ascii_uppercase().contains("ORDER BY");
        match ours {
            Err(reason) => failures.push(format!("{sql}\n  inillucent failed: {reason}")),
            Ok(actual) => {
                let (mut left, mut right) = (expected.clone(), actual.clone());
                if !ordered {
                    left.sort();
                    right.sort();
                }
                if left != right {
                    failures.push(format!(
                        "{sql}\n  sqlite:  {left:?}\n  inillucent: {right:?}"
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} statements diverged:\n{}",
        failures.len(),
        statements.len(),
        failures.join("\n")
    );
}

/// Every join form returns exactly what SQLite returns, including the rows an
/// outer join keeps and the NULLs it fills them with.
#[test]
fn joins_match_the_oracle() {
    grade("joins", &[
        "SELECT a.name, b.region FROM a, b WHERE a.team = b.team ORDER BY a.id",
        "SELECT a.name, b.region FROM a JOIN b ON a.team = b.team ORDER BY a.id",
        "SELECT a.name, b.region FROM a CROSS JOIN b ORDER BY a.id, b.id",
        "SELECT a.name, b.region FROM a LEFT JOIN b ON a.team = b.team ORDER BY a.id",
        "SELECT a.name, b.region FROM a LEFT OUTER JOIN b ON a.team = b.team ORDER BY a.id",
        "SELECT count(*) FROM a LEFT JOIN b ON a.team = b.team",
        "SELECT a.name, b.region FROM a LEFT JOIN b ON a.team = b.team WHERE b.region IS NULL ORDER BY a.id",
        "SELECT a.name, b.region FROM a LEFT JOIN b ON a.team = b.team AND b.rank > 1 ORDER BY a.id",
        "SELECT * FROM a JOIN b USING (team) ORDER BY a.id",
        "SELECT * FROM a NATURAL JOIN b ORDER BY a.id",
        "SELECT a.name, b.region, c.v FROM a LEFT JOIN b ON a.team = b.team LEFT JOIN c ON c.k = b.rank ORDER BY a.id, c.v",
        "SELECT a.name FROM a LEFT JOIN b ON a.team = b.team WHERE b.rank IS NULL ORDER BY a.id",
        "SELECT a.name, b.region FROM a RIGHT JOIN b ON a.team = b.team ORDER BY b.id, a.id",
        "SELECT a.name, b.region FROM a RIGHT OUTER JOIN b ON a.team = b.team ORDER BY b.id, a.id",
        "SELECT count(*) FROM a RIGHT JOIN b ON a.team = b.team",
        "SELECT a.name, b.region FROM a FULL JOIN b ON a.team = b.team ORDER BY a.id, b.id",
        "SELECT a.name, b.region FROM a FULL OUTER JOIN b ON a.team = b.team ORDER BY a.id, b.id",
        "SELECT count(*) FROM a FULL JOIN b ON a.team = b.team",
        "SELECT a.name, b.region FROM a RIGHT JOIN b ON a.team = b.team WHERE a.name IS NULL ORDER BY b.id",
        "SELECT a.name, b.region FROM a FULL JOIN b ON a.team = b.team AND b.rank > 1 ORDER BY a.id, b.id",
        "SELECT a.name, b.region, c.v FROM a RIGHT JOIN b ON a.team = b.team JOIN c ON c.k = b.rank ORDER BY b.id, c.v",
    ]);
}

/// Compound selects, including the de-duplication each operator applies to
/// everything to its left.
#[test]
fn compounds_match_the_oracle() {
    grade(
        "compounds",
        &[
            "SELECT team FROM a UNION SELECT team FROM b",
            "SELECT team FROM a UNION ALL SELECT team FROM b",
            "SELECT team FROM a INTERSECT SELECT team FROM b",
            "SELECT team FROM a EXCEPT SELECT team FROM b",
            "SELECT k FROM c UNION SELECT id FROM b ORDER BY 1",
            "SELECT k FROM c UNION ALL SELECT k FROM c ORDER BY 1",
            "SELECT k FROM c UNION ALL SELECT k FROM c UNION SELECT 99 ORDER BY 1",
            "SELECT team FROM a UNION SELECT team FROM b ORDER BY team DESC",
            "SELECT team FROM a UNION SELECT team FROM b ORDER BY 1 LIMIT 2",
            "SELECT team FROM a UNION SELECT team FROM b ORDER BY 1 LIMIT 2 OFFSET 1",
            "SELECT 1, 'x' UNION SELECT 2, 'y' ORDER BY 1",
            "SELECT id, name FROM a UNION ALL SELECT id, region FROM b ORDER BY 1, 2",
            "VALUES (1, 'a'), (2, 'b') UNION SELECT 3, 'c'",
        ],
    );
}

/// Subqueries in every position, correlated and not.
#[test]
fn subqueries_match_the_oracle() {
    grade("subqueries", &[
        "SELECT * FROM (SELECT id, name FROM a WHERE id > 2) ORDER BY id",
        "SELECT t.name FROM (SELECT id, name FROM a) AS t WHERE t.id = 3",
        "SELECT n FROM (SELECT name AS n, score FROM a ORDER BY score DESC LIMIT 2) ORDER BY n",
        "SELECT a.name FROM a WHERE a.team IN (SELECT team FROM b) ORDER BY a.id",
        "SELECT a.name FROM a WHERE a.team NOT IN (SELECT team FROM b) ORDER BY a.id",
        "SELECT a.name FROM a WHERE a.team NOT IN (SELECT region FROM b) ORDER BY a.id",
        "SELECT a.name FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.team = a.team) ORDER BY a.id",
        "SELECT a.name FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.team = a.team) ORDER BY a.id",
        "SELECT a.name, (SELECT b.region FROM b WHERE b.team = a.team) FROM a ORDER BY a.id",
        "SELECT a.name, (SELECT count(*) FROM b) FROM a ORDER BY a.id",
        "SELECT (SELECT max(score) FROM a), (SELECT min(score) FROM a)",
        "SELECT a.name FROM a WHERE a.score > (SELECT avg(score) FROM a) ORDER BY a.id",
        "SELECT x.name, y.region FROM (SELECT * FROM a) AS x LEFT JOIN (SELECT * FROM b) AS y ON x.team = y.team ORDER BY x.id",
        "SELECT k FROM c WHERE k IN (SELECT k FROM c WHERE v IS NULL)",
    ]);
}

/// Views behave as the query they stand for, and refuse to be written.
#[test]
fn views_match_the_oracle() {
    grade("views", &[
        "SELECT * FROM blue ORDER BY id",
        "SELECT name FROM blue WHERE score > 1 ORDER BY id",
        "SELECT * FROM ranked ORDER BY who",
        "SELECT count(*) FROM blue",
        "SELECT b.region FROM blue JOIN a ON a.id = blue.id JOIN b ON b.team = a.team ORDER BY blue.id",
        "INSERT INTO blue VALUES (9, 'x', 1.0)",
        "UPDATE blue SET name = 'x'",
        "DELETE FROM blue",
    ]);
}

/// Common table expressions, ordinary and recursive.
#[test]
fn ctes_match_the_oracle() {
    grade("ctes", &[
        "WITH t AS (SELECT id, name FROM a WHERE id > 2) SELECT * FROM t ORDER BY id",
        "WITH t(x, y) AS (SELECT id, name FROM a) SELECT y FROM t WHERE x = 1",
        "WITH t AS (SELECT team FROM a) SELECT count(*) FROM t",
        "WITH t AS (SELECT id FROM a), u AS (SELECT id FROM b) SELECT * FROM t UNION ALL SELECT * FROM u ORDER BY 1",
        "WITH t AS (SELECT id FROM a) SELECT * FROM t AS x JOIN t AS y ON x.id = y.id ORDER BY x.id",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5) SELECT i FROM n",
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i * 2 FROM n WHERE i < 40) SELECT i FROM n",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION SELECT 1 FROM n) SELECT count(*) FROM n",
    ]);
}

/// Window functions: every frame unit, every bound, every `EXCLUDE`, and the
/// eleven functions that only exist in a window.
#[test]
fn windows_match_the_oracle() {
    grade(
        "windows",
        &[
            "SELECT name, row_number() OVER () FROM a ORDER BY id",
            "SELECT name, row_number() OVER (ORDER BY score) FROM a ORDER BY id",
            "SELECT name, rank() OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, dense_rank() OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, percent_rank() OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, cume_dist() OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, ntile(3) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, ntile(2) OVER (PARTITION BY team ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lag(name) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lag(name, 2) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lag(name, 2, 'none') OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lead(name) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, lead(name, 3, 'none') OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, first_value(name) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, last_value(name) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, nth_value(name, 2) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, count(*) OVER () FROM a ORDER BY id",
            "SELECT name, count(*) OVER (PARTITION BY team) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM a ORDER BY id",
            "SELECT name, sum(id) OVER (ORDER BY id ROWS 2 PRECEDING) FROM a ORDER BY id",
            "SELECT name, avg(score) OVER (PARTITION BY team ORDER BY id) FROM a ORDER BY id",
            "SELECT name, min(id) OVER (ORDER BY team), max(id) OVER (ORDER BY team) FROM a ORDER BY id",
            "SELECT name, group_concat(name, '-') OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) FROM a ORDER BY id",
            "SELECT name, count(*) OVER (ORDER BY team RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES) FROM a ORDER BY id",
            "SELECT name, count(*) FILTER (WHERE score > 0) OVER (PARTITION BY team) FROM a ORDER BY id",
            "SELECT name, sum(id) FILTER (WHERE id > 2) OVER () FROM a ORDER BY id",
            "SELECT name, row_number() OVER w FROM a WINDOW w AS (ORDER BY id) ORDER BY id",
            "SELECT name, row_number() OVER (w ORDER BY id DESC) FROM a WINDOW w AS (PARTITION BY team) ORDER BY id",
            "SELECT name, row_number() OVER (PARTITION BY team ORDER BY id), count(*) OVER (PARTITION BY score) FROM a ORDER BY id",
            "SELECT id + row_number() OVER (ORDER BY id) FROM a ORDER BY id",
            "SELECT name FROM a WHERE id > 1 ORDER BY row_number() OVER (ORDER BY id DESC)",
            "SELECT team, count(*), row_number() OVER (ORDER BY team) FROM a GROUP BY team ORDER BY team",
            "SELECT name, sum(id) OVER (ORDER BY id) FROM a ORDER BY id LIMIT 2 OFFSET 1",
            "SELECT DISTINCT count(*) OVER (PARTITION BY team) FROM a ORDER BY 1",
        ],
    );
}

/// The math built-ins, over a value matrix that includes the awkward cases:
/// a domain error, a non-numeric argument, the integer/real boundary, and the
/// two functions whose meaning changes with their argument count.
#[test]
fn math_functions_match_the_oracle() {
    grade(
        "math",
        &[
            "SELECT pi()",
            "SELECT abs(-3), abs(-3.5), abs('x'), abs(NULL)",
            "SELECT ceil(1.2), ceil(-1.2), ceiling(1.0), floor(1.8), floor(-1.8)",
            "SELECT trunc(1.9), trunc(-1.9), trunc(2)",
            "SELECT sqrt(4), sqrt(2), sqrt(0), sqrt(-1), sqrt('4'), sqrt('four'), sqrt(NULL)",
            "SELECT exp(0), exp(1), ln(1), ln(0), ln(-1)",
            "SELECT log(100), log(10, 1000), log(1, 5), log(0), log10(1000), log2(8)",
            "SELECT sin(0), cos(0), tan(0)",
            "SELECT asin(1), acos(1), atan(1), asin(2), acos(2)",
            "SELECT sinh(0), cosh(0), tanh(0), asinh(0), acosh(1), atanh(0)",
            "SELECT acosh(0), atanh(1), atanh(-1)",
            "SELECT atan2(1, 1), atan2(0, 1)",
            "SELECT degrees(pi()), radians(180)",
            "SELECT mod(7, 3), mod(-7, 3), mod(7, -3), mod(7.5, 2), mod(7, 0)",
            "SELECT pow(2, 10), power(2, 0.5), pow(-8, 2)",
            "SELECT typeof(ceil(1)), typeof(floor(1)), typeof(sqrt(4))",
            "SELECT ln(x'00'), sqrt(x'00')",
            "SELECT round(2.5), round(-2.5), round(2.345, 2), round(1)",
            "SELECT max(1, 2, 3), min(1, 2, 3), max(1, NULL), min(NULL, 1)",
        ],
    );
}

/// The date and time built-ins.
///
/// Nothing here names `'now'`: the two engines read the clock microseconds
/// apart and a test that compared them would fail whenever those microseconds
/// crossed a second. Every value is a fixed timestamp, and the wall clock is
/// covered by the unit test in `datetime.rs` instead.
#[test]
fn date_and_time_functions_match_the_oracle() {
    grade(
        "datetime",
        &[
            "SELECT date('2026-09-03'), time('2026-09-03 14:30:15'), datetime('2026-09-03 14:30:15')",
            "SELECT date('2026-09-03T14:30:15'), datetime('2026-09-03T14:30:15Z')",
            "SELECT julianday('2026-09-03'), julianday('1970-01-01'), julianday('2000-01-01 12:00:00')",
            "SELECT unixepoch('1970-01-01'), unixepoch('2026-09-03'), unixepoch('2026-09-03 14:30:15')",
            "SELECT date(2451545.0), datetime(2451545.0)",
            "SELECT datetime(0, 'unixepoch'), datetime(1000000000, 'unixepoch')",
            "SELECT datetime('2026-09-03', '+1 day'), datetime('2026-09-03', '-1 day')",
            "SELECT datetime('2026-09-03', '+3 hours'), datetime('2026-09-03', '+90 minutes')",
            "SELECT datetime('2026-09-03 00:00:00', '+30 seconds')",
            "SELECT date('2026-01-31', '+1 month'), date('2026-03-31', '-1 month')",
            "SELECT date('2024-02-29', '+1 year'), date('2026-09-03', '-2 years')",
            "SELECT date('2026-09-03', 'start of month'), date('2026-09-03', 'start of year')",
            "SELECT datetime('2026-09-03 14:30:15', 'start of day')",
            "SELECT date('2026-09-03', 'weekday 0'), date('2026-09-03', 'weekday 3'), date('2026-09-03', 'weekday 6')",
            "SELECT date('2026-09-03', 'start of month', '+1 month', '-1 day')",
            "SELECT strftime('%Y-%m-%d', '2026-09-03')",
            "SELECT strftime('%Y/%m/%d %H:%M:%S', '2026-09-03 14:30:15')",
            "SELECT strftime('%d %m %Y %H %M %S %j %w %W', '2026-09-03 14:30:15')",
            "SELECT strftime('%s', '2026-09-03'), strftime('%J', '2026-09-03')",
            "SELECT strftime('%f', '2026-09-03 14:30:15.250')",
            "SELECT strftime('%%|%Y', '2026-09-03')",
            "SELECT date('not a date'), time('nonsense'), julianday('xyz'), unixepoch('xyz')",
            "SELECT date(NULL), datetime(NULL), strftime('%Y', NULL)",
            "SELECT date('2026-09-03', 'bogus modifier')",
            "SELECT timediff('2026-09-03', '2026-09-01')",
            "SELECT timediff('2026-09-01', '2026-09-03')",
            "SELECT timediff('2026-03-01', '2026-01-31')",
            "SELECT date('1582-10-15'), julianday('1582-10-15'), date('1200-06-06')",
            "SELECT datetime('2026-09-03 14:30:15+02:00'), datetime('2026-09-03 14:30:15-05:30')",
            "SELECT date('2026-09-03 25:00:00'), date('2026-13-01'), date('2026-09-32')",
        ],
    );
}

/// The core scalar built-ins, with `printf` the substantial one.
///
/// `random()`, `randomblob()` and `sqlite_source_id()` are deliberately absent:
/// two engines cannot agree on a random number, and the source id names the
/// build rather than the behaviour. Their shape is covered by unit tests.
#[test]
fn core_functions_match_the_oracle() {
    grade(
        "core",
        &[
            "SELECT printf('%d', 42), printf('%d', -42), printf('%i', 7), printf('%u', 3)",
            "SELECT printf('%5d|', 42), printf('%-5d|', 42), printf('%05d', 42), printf('%05d', -42)",
            "SELECT printf('%+d %+d % d', 42, -42, 42)",
            "SELECT printf('%.3d', 7), printf('%8.3d|', 7)",
            "SELECT printf('%x %X %o', 255, 255, 8)",
            "SELECT printf('%#x %#X %#o', 255, 255, 8)",
            "SELECT printf('%f', 3.5), printf('%.2f', 3.14159), printf('%.0f', 2.5)",
            "SELECT printf('%10.2f|', 3.14159), printf('%-10.2f|', 3.14159)",
            "SELECT printf('%e', 1234.5), printf('%E', 1234.5), printf('%.2e', 1234.5)",
            "SELECT printf('%g', 1234.5), printf('%g', 0.00001234), printf('%G', 1e20)",
            "SELECT printf('%s|%s', 'ab', 'cd'), printf('%10s|', 'ab'), printf('%-10s|', 'ab')",
            "SELECT printf('%.2s', 'abcdef')",
            "SELECT printf('%c%c', 65, 66)",
            "SELECT printf('%q', 'it''s'), printf('%Q', 'it''s'), printf('%Q', NULL), printf('%q', NULL)",
            "SELECT printf('%w', 'a\"b')",
            "SELECT printf('%%'), printf('a%%b'), printf('100%%')",
            "SELECT printf('%*d|', 5, 42), printf('%-*d|', 5, 42), printf('%.*f', 2, 3.14159)",
            "SELECT printf('%d-%d', 1), printf('%s!')",
            "SELECT printf('%s', 5), printf('%s', 5.5), printf('%d', '42'), printf('%d', 'abc')",
            "SELECT printf(NULL, 1), printf('no conversions')",
            "SELECT format('%d apples', 3)",
            "SELECT octet_length('abc'), octet_length(x'00ff'), octet_length(123), octet_length(NULL)",
            "SELECT length('abc'), length(x'00ff'), length(12345), length(NULL)",
            "SELECT hex('abc'), hex(x'00ff'), quote('it''s'), quote(NULL), quote(1.5)",
            "SELECT substr('abcdef', 2), substr('abcdef', 2, 3), substr('abcdef', -2), substr('abcdef', -2, 1)",
            "SELECT instr('abcdef', 'cd'), instr('abcdef', 'z'), instr(NULL, 'a')",
            "SELECT replace('abcabc', 'b', 'X'), replace('abc', '', 'X'), replace(NULL, 'a', 'b')",
            "SELECT trim('  ab  '), ltrim('xxabxx', 'x'), rtrim('xxabxx', 'x')",
            "SELECT upper('aBc'), lower('aBc'), unicode('A'), char(65, 66)",
            "SELECT iif(1, 'y', 'n'), iif(0, 'y', 'n'), iif(NULL, 'y', 'n')",
            "SELECT coalesce(NULL, NULL, 3), ifnull(NULL, 2), nullif(1, 1), nullif(1, 2)",
            "SELECT typeof(1), typeof(1.5), typeof('x'), typeof(x'00'), typeof(NULL)",
            "SELECT zeroblob(3), hex(zeroblob(3)), unhex('414243')",
            "SELECT concat('a', 1, NULL, 'b'), concat_ws('-', 'a', NULL, 'b')",
            "SELECT sign(-3), sign(0), sign(3.5), sign('x')",
            "SELECT likelihood(1, 0.5), likely(1), unlikely(1)",
            // `soundex` is not in the pinned build - it needs
            // `SQLITE_SOUNDEX` - so parity means refusing it rather than
            // implementing it.
            "SELECT soundex('Robert')",
        ],
    );
}

/// The statements SQLite refuses, refused the same way.
///
/// A negative test is worth as much as a positive one and is easier to get
/// wrong: an engine that accepts what the reference rejects has a *larger*
/// language, and every statement it accepts is one the reference cannot read.
/// The messages are not compared - they are prose - but the refusal is.
#[test]
fn refusals_match_the_oracle() {
    grade(
        "negative",
        &[
            // Access control is not SQLite's; these are not statements at all.
            "GRANT SELECT ON a TO someone",
            "REVOKE SELECT ON a FROM someone",
            "CREATE USER bob",
            // A view is not writable, however it is written to.
            "INSERT INTO blue VALUES (9, 'x', 1.0)",
            "INSERT INTO blue (id) VALUES (9)",
            "UPDATE blue SET name = 'x'",
            "UPDATE blue SET name = 'x' WHERE id = 1",
            "DELETE FROM blue",
            "DELETE FROM blue WHERE id = 1",
            "DROP TABLE blue",
            "DROP VIEW a",
            "CREATE INDEX blue_name ON blue (name)",
            // A compound's arms must agree on their width, and only the last
            // may carry an ORDER BY or a LIMIT.
            "SELECT id FROM a UNION SELECT id, name FROM a",
            "SELECT id FROM a ORDER BY id UNION SELECT id FROM a",
            "SELECT id FROM a LIMIT 1 UNION SELECT id FROM a",
            "SELECT id FROM a UNION SELECT id FROM a ORDER BY nosuchcolumn",
            // A scalar subquery is one column.
            "SELECT (SELECT id, name FROM a)",
            "SELECT * FROM a WHERE id IN (SELECT id, name FROM a)",
            // Names that do not resolve.
            "SELECT nosuchcolumn FROM a",
            "SELECT * FROM nosuchtable",
            "SELECT * FROM a JOIN b USING (nosuchcolumn)",
            "SELECT * FROM a AS x JOIN b AS y ON x.nosuch = y.team",
            "WITH t AS (SELECT 1) SELECT * FROM nosucht",
            "SELECT row_number() OVER nosuchwindow FROM a",
            "SELECT nosuchfunction(1)",
            "SELECT abs(1, 2)",
            "SELECT count(*, 1) FROM a",
            // Aggregates and windows where they do not belong.
            "SELECT * FROM a WHERE count(*) > 1",
            "SELECT sum(sum(id)) FROM a",
            "SELECT * FROM a WHERE row_number() OVER () = 1",
            // A frame offset is a constant.
            "SELECT sum(id) OVER (ORDER BY id ROWS id PRECEDING) FROM a",
            // `RIGHT` and `FULL` are accepted by the pinned build, so this is
            // the one row here that both engines must *not* refuse.
            "SELECT count(*) FROM a RIGHT JOIN b ON a.team = b.team",
            "SELECT count(*) FROM a FULL JOIN b ON a.team = b.team",
        ],
    );
}
