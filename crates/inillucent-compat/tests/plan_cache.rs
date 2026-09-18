//! The plan cache returns the same answers as no plan cache, and lets go of a
//! program whenever something it was compiled against changes.
//!
//! Invariant: a cache hit is indistinguishable from a compile. That
//! is not a performance property, it is a correctness one, and it is the only
//! interesting thing about a cache - the speed is easy and the staleness is
//! where the bugs are. Every case here is a way the compiled program *should*
//! change: the schema changed, a function of that name now exists, a collation
//! of that name now exists, or the planner's levers moved.
//!
//! Each case runs the statement, changes something, runs the identical SQL
//! again, and requires the second answer to be the one a fresh connection
//! gives. A cache that held on would return the first answer and pass every
//! other test in this repository.

use inillucent_compat::facade::Database;
use inillucent_value::Value;

/// A database file of this test's own, under the gitignored agent-output root.
///
/// Each test gets its own directory, and a serial keeps two tests running in
/// parallel off one another's file.
///
/// **The file is removed before the path is handed out.** It used to be handed
/// out as it was, on the argument that a process id in the name makes every run
/// name a file no earlier run had written. Windows hands process ids back out,
/// so the same name does come round again, and nothing here ever deleted one:
/// when task-1978 counted them, `_agent_output/plan-cache/` held 1,125
/// databases written by 143 process ids, the oldest dated 2026-09-09 and none
/// of them removed. When a run drew a name one of them was already
/// sitting at, `Database::open_with_busy_timeout` opened that database with its
/// schema, and the `CREATE TABLE t` in every fixture below failed on the table
/// the earlier run had created. Six of the seven tests in this file failed that
/// way in the run of 2026-09-17 21:56 and passed on their own afterwards.
///
/// The WAL segment beside the database is left where it is. A segment records
/// the uuid of the database that wrote it, a uuid a new file draws at random,
/// and `inillucent_wal::recover::read_chain` ends the chain at a segment
/// belonging to another database rather than replaying it - so the schema
/// cannot come back that way, and the new database truncates the segment and
/// writes its own header over it. `a_leftover_database_does_not_reach_the_next_run`
/// is the case for both halves of that.
///
/// @param name - the test's name, which names its directory
fn scratch(name: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    scratch_at(
        name,
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    )
}

/// The same thing for a serial the caller chose.
///
/// Split out so `a_leftover_database_does_not_reach_the_next_run` can ask for
/// one path twice. The counter above hands out a different serial every call,
/// which is what every other test here wants and is exactly what a case about
/// one path being reused cannot use.
///
/// @param name - the test's name, which names its directory
/// @param serial - which of that directory's files to name
fn scratch_at(name: &str, serial: u32) -> std::path::PathBuf {
    let root = inillucent_compat::workspace_root()
        .join("_agent_output/plan-cache")
        .join(name);
    std::fs::create_dir_all(&root).expect("the scratch directory");
    let path = root.join(format!("{}-{serial}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Opens a fresh database with the given schema and rows.
///
/// @param name - the test's name, which names its scratch directory
/// @param setup - the statements to run before the test
fn open(name: &str, setup: &[&str]) -> Database {
    let path = scratch(name);
    // Nothing is at this path: `scratch` deleted whatever an earlier run left
    // there, so this open creates the database and `setup` is its whole schema.
    let database =
        Database::open_with_busy_timeout(&path, std::time::Duration::from_secs(5)).expect("open");
    let connection = database.session().expect("connect");
    // **No `journal_mode = delete`.** The old facade accepted it and this
    // engine is WAL-only by design - `PRAGMA journal_mode` refuses anything
    // else rather than reporting a mode it does not have. Nothing in this file
    // asserts anything about the journal; the pragma was here so the fixture
    // left one file behind rather than three, and a scratch directory does not
    // care.
    for statement in setup {
        connection.execute_batch(statement).expect(statement);
    }
    // **Reopened, so the cache starts empty.** On this engine the compiled
    // statements live on the *database* rather than on the connection - one
    // file is one pool and one plan cache, and a `Connection` is a handle on
    // it - so a second `connect()` would inherit the fixture's own `INSERT`.
    // Every assertion below is about how the cache grows and what invalidates
    // it, and each needs a known starting point rather than a particular place
    // for the cache to live.
    drop(connection);
    drop(database);
    Database::open_with_busy_timeout(&path, std::time::Duration::from_secs(5)).expect("reopen")
}

/// Runs a query and returns its rows as strings.
///
/// @param connection - the connection to run on
/// @param sql - the statement
fn rows(connection: &inillucent_compat::facade::Connection, sql: &str) -> Vec<String> {
    let mut statement = connection.prepare(sql).expect(sql);
    let mut out = Vec::new();
    while statement.step().expect(sql) {
        let rendered: Vec<String> = statement
            .row()
            .iter()
            .map(|value| match value {
                Value::Null => "NULL".to_string(),
                Value::Integer(number) => number.to_string(),
                Value::Real(number) => format!("{number}"),
                Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
                Value::Blob(blob) => format!("blob:{}", blob.raw().len()),
            })
            .collect();
        out.push(rendered.join("|"));
    }
    out
}

/// The cache actually holds something, so the tests below are testing a cache
/// rather than an empty one.
#[test]
fn preparing_the_same_statement_twice_keeps_one_program() {
    let database = open(
        "preparing_twice",
        &[
            "CREATE TABLE t(a INTEGER, b TEXT)",
            "INSERT INTO t VALUES (1, 'one'), (2, 'two')",
        ],
    );
    let connection = database.session().expect("connect");
    assert_eq!(connection.cached_plan_count(), 0);
    let first = rows(&connection, "SELECT a, b FROM t ORDER BY a");
    let held = connection.cached_plan_count();
    assert!(held > 0, "nothing was cached");
    let second = rows(&connection, "SELECT a, b FROM t ORDER BY a");
    assert_eq!(first, second);
    assert_eq!(
        connection.cached_plan_count(),
        held,
        "the same statement should not add a second entry"
    );
}

/// A schema change makes the next prepare of identical SQL compile again.
///
/// The catalog generation is in the cache key, so this is the case the key
/// handles rather than the invalidation - and it is the case that matters most,
/// because `ALTER TABLE` changing a column's position would otherwise make a
/// cached program read the wrong slot.
#[test]
fn a_schema_change_is_not_answered_from_the_cache() {
    let database = open(
        "schema_change",
        &[
            "CREATE TABLE t(a INTEGER, b TEXT)",
            "INSERT INTO t VALUES (1, 'one')",
        ],
    );
    let connection = database.session().expect("connect");
    assert_eq!(rows(&connection, "SELECT * FROM t"), vec!["1|one"]);
    connection
        .execute_batch("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 9")
        .expect("alter");
    // `SELECT *` expands at bind time, so the cached program would return two
    // columns where the schema now has three.
    assert_eq!(rows(&connection, "SELECT * FROM t"), vec!["1|one|9"]);
}

/// A dropped and recreated table with different columns is not answered from
/// the cache either.
#[test]
fn a_recreated_table_is_not_answered_from_the_cache() {
    let database = open(
        "recreated_table",
        &["CREATE TABLE t(a INTEGER)", "INSERT INTO t VALUES (1)"],
    );
    let connection = database.session().expect("connect");
    assert_eq!(rows(&connection, "SELECT * FROM t"), vec!["1"]);
    connection
        .execute_batch(
            "DROP TABLE t; CREATE TABLE t(x TEXT, y TEXT); INSERT INTO t VALUES ('p','q')",
        )
        .expect("recreate");
    assert_eq!(rows(&connection, "SELECT * FROM t"), vec!["p|q"]);
}

/// Registering a function after a statement was prepared makes the next prepare
/// of identical SQL see the function.
///
/// The registered functions are deliberately *not* in the cache key - comparing
/// them per prepare would cost more than the cache saves - so this is the case
/// the outright invalidation exists for. Without it, `SELECT twice(2)` would
/// keep failing to bind after `twice` had been registered.
#[test]
fn registering_a_function_drops_the_cached_programs() {
    let database = open(
        "function_registration",
        &["CREATE TABLE t(a INTEGER)", "INSERT INTO t VALUES (21)"],
    );
    let connection = database.session().expect("connect");
    // Warm the cache with something that binds today.
    assert_eq!(rows(&connection, "SELECT a FROM t"), vec!["21"]);
    assert!(connection.cached_plan_count() > 0);
    // A statement that cannot bind yet.
    assert!(connection.prepare("SELECT twice(a) FROM t").is_err());
    connection
        .create_scalar_function(
            "twice",
            1,
            inillucent_engine::extensions::FunctionFlags::default(),
            std::sync::Arc::new(|arguments: &[Value<'_>]| {
                let value = arguments.first().and_then(|value| match value {
                    Value::Integer(number) => Some(*number),
                    _ => None,
                });
                Ok(Value::Integer(value.unwrap_or(0) * 2))
            }),
        )
        .expect("register");
    assert_eq!(
        connection.cached_plan_count(),
        0,
        "registering a function must drop the cached programs"
    );
    assert_eq!(rows(&connection, "SELECT twice(a) FROM t"), vec!["42"]);
}

/// Defining a collation drops the cached programs, so a comparison compiled
/// under BINARY does not keep comparing under BINARY.
#[test]
fn defining_a_collation_drops_the_cached_programs() {
    let database = open(
        "collation",
        &[
            "CREATE TABLE t(a TEXT)",
            "INSERT INTO t VALUES ('b'), ('A'), ('a')",
        ],
    );
    let connection = database.session().expect("connect");
    assert_eq!(
        rows(&connection, "SELECT a FROM t ORDER BY a"),
        vec!["A", "a", "b"]
    );
    assert!(connection.cached_plan_count() > 0);
    connection
        .create_collation(
            "REVERSED",
            std::sync::Arc::new(|left: &[u8], right: &[u8]| right.cmp(left)),
        )
        .expect("collation");
    assert_eq!(
        connection.cached_plan_count(),
        0,
        "defining a collation must drop the cached programs"
    );
    assert_eq!(
        rows(&connection, "SELECT a FROM t ORDER BY a COLLATE REVERSED"),
        vec!["b", "a", "A"]
    );
}

/// A statement prepared under different levers is cached under different keys.
///
/// The levers are in the cache key, and this asserts the key rather than a
/// planner behaviour: preparing identical SQL after moving a lever adds an
/// entry instead of hitting the first one. Asserting on a *plan difference*
/// would make the test depend on whichever lever happens to change the plan for
/// whichever table the test builds, which is a different thing to check and a
/// fragile way to check this one.
///
/// It is what makes an A/B measurement of any lever trustworthy on a connection
/// that has already run the other arm.
#[test]
fn a_lever_change_is_cached_separately() {
    let database = open(
        "lever_change",
        &[
            "CREATE TABLE t(a INTEGER, b INTEGER)",
            "CREATE INDEX t_ab ON t(a, b)",
            "INSERT INTO t VALUES (1, 10), (2, 20)",
        ],
    );
    let connection = database.session().expect("connect");
    let with = rows(&connection, "SELECT a, b FROM t ORDER BY a");
    assert_eq!(connection.cached_plan_count(), 1);
    connection.disable_optimizations(inillucent_sql::plan::Levers::without(
        inillucent_sql::plan::Levers::COVERING_INDEX,
    ));
    let without = rows(&connection, "SELECT a, b FROM t ORDER BY a");
    assert_eq!(
        connection.cached_plan_count(),
        2,
        "the same SQL under different levers must not hit the same entry"
    );
    assert_eq!(with, without, "the levers change the plan, not the answer");
    connection.disable_optimizations(inillucent_sql::plan::Levers::all());
    let again = rows(&connection, "SELECT a, b FROM t ORDER BY a");
    assert_eq!(
        connection.cached_plan_count(),
        2,
        "going back to the first lever mask should hit the first entry"
    );
    assert_eq!(with, again);
}

/// With the cache switched off, every answer is the same.
///
/// The arm that proves the cache is a speed change and not a behaviour change:
/// the same statements, the same order, the lever disabled.
#[test]
fn the_cache_changes_no_answer() {
    let statements = [
        "SELECT a, b FROM t ORDER BY a",
        "SELECT count(*), sum(a) FROM t",
        "SELECT b FROM t WHERE a = 2",
        "SELECT DISTINCT b FROM t ORDER BY b",
        "SELECT a FROM t ORDER BY b DESC LIMIT 1",
    ];
    let mut answers = Vec::new();
    for disabled in [0u32, inillucent_sql::plan::Levers::PLAN_CACHE] {
        let database = open(
            "cache_off",
            &[
                "CREATE TABLE t(a INTEGER, b TEXT)",
                "INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'two')",
            ],
        );
        let connection = database.session().expect("connect");
        connection.disable_optimizations(inillucent_sql::plan::Levers::without(disabled));
        let mut held = Vec::new();
        // Twice through, so the second pass is the cached one where the cache
        // is on and is a fresh compile where it is off.
        for _ in 0..2 {
            for statement in statements {
                held.push(rows(&connection, statement));
            }
        }
        if disabled == inillucent_sql::plan::Levers::PLAN_CACHE {
            assert_eq!(
                connection.cached_plan_count(),
                0,
                "the lever should switch the cache off"
            );
        } else {
            assert!(connection.cached_plan_count() > 0);
        }
        answers.push(held);
    }
    assert_eq!(answers[0], answers[1]);
}

/// A database an earlier run left at the path does not reach the next run.
///
/// The fixture's own case, and it is here because the fixture was wrong. Every
/// other test in this file opens a database and asserts what the plan cache did
/// with a known schema, so every one of them depends on `scratch` returning a
/// path nothing is at - and across the 143 process ids task-1978 counted in
/// that directory, it did not.
/// The case writes a database at one of those paths and then asks the fixture for
/// that same path, which is what a run that drew a process id Windows had
/// handed out before was doing.
///
/// It fails against the fixture as it was, on the assertion that the path the
/// fixture handed back holds nothing. That is one step earlier than what the
/// other tests saw - they reported `table t already exists` from the fixture's
/// own `CREATE TABLE t`, six of the seven of them in the run of 2026-09-17
/// 21:56 - and the last statement here is that same `CREATE TABLE t`, so the
/// symptom they had is covered as well as the cause.
///
/// The log segment is asserted as well as the database, because the fixture
/// deletes one of the two. A segment records the uuid of the database that
/// wrote it and `inillucent_wal::recover::read_chain` ends the chain at a
/// segment belonging to a different database, so a segment left beside a
/// deleted file cannot replay the old schema into the new one - and the
/// assertion keeps this case from quietly stopping to cover that half.
#[test]
fn a_leftover_database_does_not_reach_the_next_run() {
    let path = scratch_at("leftover", 0);
    {
        let database = Database::open_with_busy_timeout(&path, std::time::Duration::from_secs(5))
            .expect("open");
        let connection = database.session().expect("connect");
        connection
            .execute_batch("CREATE TABLE t(a INTEGER)")
            .expect("the leftover schema is written");
    }
    assert!(path.exists(), "the run left no database to be inherited");
    // **Any segment, not segment one.** A checkpoint rolls the log to the next
    // sequence and retires the ones below it, and `locking_mode = normal` -
    // the default since task-1980 - checkpoints after every statement that
    // wrote. So the segment a finished run leaves behind is whichever sequence
    // it had reached, and naming 1 made this assertion fail on a case that was
    // still testing exactly what it says it is.
    let stem = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let left_behind = std::fs::read_dir(path.parent().unwrap_or(std::path::Path::new(".")))
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{stem}-wal."))
        });
    assert!(
        left_behind,
        "no log segment beside {}, so this case is not checking what it says it is",
        path.display()
    );

    let again = scratch_at("leftover", 0);
    assert_eq!(again, path, "the fixture named a different file");
    assert!(
        !again.exists(),
        "the fixture handed back the database the first run left at {}",
        again.display()
    );
    let database = Database::open_with_busy_timeout(&again, std::time::Duration::from_secs(5))
        .expect("reopen");
    let connection = database.session().expect("connect");
    connection
        .execute_batch("CREATE TABLE t(a INTEGER)")
        .expect("the earlier run's schema must not have come back");
}
