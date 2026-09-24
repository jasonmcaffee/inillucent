//! Binding by name, and the bound on the plan cache.
//!
//! Invariant: **a caller does not have to count its own parameters, and a
//! long-lived connection does not grow a plan per statement it ever issued.**
//! Both were true of the engine and neither was reachable through the driver.
//!
//! ## The two defects (task-1932, M1)
//!
//! `connect::parameter_names` has answered every `:name`, `@name` and `$name`
//! with its index since the parser had parameters, and the driver bound by
//! position only - so an application with nine named parameters counted them
//! itself and kept the count right through every edit of the SQL. Getting it
//! wrong is silent: the values land in the wrong columns and the statement
//! succeeds.
//!
//! The plan cache held one compiled plan per statement text and was emptied
//! only by a schema change or a function registration. An application issuing
//! generated SQL - a query builder, a reporting tool, anything that puts a
//! literal in the statement - kept one plan per distinct string for the life of
//! the process, with no accessor to ask about it and no bound to stop it.

use std::path::PathBuf;

use inillucent_driver::{Database, OpenOptions, Status, Value};

/// Where this suite's scratch databases live.
fn scratch(name: &str) -> PathBuf {
    let mut directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    directory.pop();
    directory.pop();
    let directory = directory.join("_agent_output/driver-statement");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join(format!("{name}.rdb"));
    inillucent_base::testing::remove_database(&path);
    path
}

/// Opens a database holding three people.
///
/// @param name - the file's name
fn peopled(name: &str) -> Database {
    let database = Database::open(scratch(name)).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
             INSERT INTO people VALUES (1, 'Ada', 36), (2, 'Grace', 45), (3, 'Alan', 41)",
        )
        .expect("the table is created");
    database
}

/// `query_all` hands back every row, and `query(.., 0)` hands back none.
///
/// **What `limit` means, written down as a test (task-1947, found by
/// task-1913).** The first application built on this engine called
/// `query(sql, params, 0)` because every other embedded database reads `0` as
/// no limit, and got an empty `rows` beside a `total` reporting the true
/// count and a status saying success. Five of its eight storage tests failed
/// at once and the symptom was "the board is empty" rather than "the query is
/// wrong".
///
/// `0` cannot be made to mean "all of them": `Connection::execute` passes it
/// to run a statement for its effect, so the sentinel space is taken. What was
/// missing was a call that says what it wants, and the consumer wrote one for
/// itself - an `ALL_ROWS: usize = usize::MAX` constant with a comment
/// explaining why it had to exist. Both halves are asserted here: the `0` that
/// surprised somebody, so the surprise is recorded rather than rediscovered,
/// and the `query_all` that answers it.
#[test]
fn query_all_returns_every_row_and_a_limit_of_zero_returns_none() {
    let database = peopled("limits");
    let connection = database.session();

    let none = connection
        .query("SELECT id FROM people ORDER BY id", &[], 0)
        .expect("the query runs");
    assert!(
        none.rows.is_empty(),
        "a limit of 0 handed back {} row(s)",
        none.rows.len()
    );
    assert_eq!(
        none.total, 3,
        "the count is the rows produced, not the rows handed back"
    );

    let some = connection
        .query("SELECT id FROM people ORDER BY id", &[], 2)
        .expect("the query runs");
    assert_eq!(some.rows.len(), 2, "a limit of 2 hands back two rows");
    assert_eq!(some.total, 3);
    assert!(some.more, "a row was left behind");

    let all = connection
        .query_all("SELECT id FROM people ORDER BY id", &[])
        .expect("the query runs");
    assert_eq!(all.rows.len(), 3, "query_all hands back every row");
    assert_eq!(all.total, 3);
    assert!(!all.more, "nothing was left behind");
}

/// A named bind reaches the right column.
///
/// **The case the defect was about**: two parameters of the same type, in an
/// order the caller could get wrong by position and cannot get wrong by name.
#[test]
fn a_named_bind_returns_the_right_row() {
    let database = peopled("named");
    let connection = database.session();
    let rows = connection
        .query_named(
            "SELECT name FROM people WHERE age > :least AND age < :most",
            &[
                (":most", Value::Integer(44)),
                (":least", Value::Integer(38)),
            ],
            10,
        )
        .expect("the query runs");
    assert_eq!(
        rows.rows,
        vec![vec![Value::Text("Alan".to_string())]],
        "the named bind answered {:?}",
        rows.rows
    );
}

/// The same statement bound by position answers the same thing.
///
/// The other half: a named bind that silently reversed its arguments would pass
/// the case above if the expectation had been written from its own output.
#[test]
fn a_named_bind_agrees_with_the_positional_one() {
    let database = peopled("agree");
    let connection = database.session();
    let sql = "SELECT name FROM people WHERE age > :least AND age < :most";
    let named = connection
        .query_named(
            sql,
            &[
                (":least", Value::Integer(38)),
                (":most", Value::Integer(44)),
            ],
            10,
        )
        .expect("the named query runs");
    let positional = connection
        .query(
            "SELECT name FROM people WHERE age > ?1 AND age < ?2",
            &[Value::Integer(38), Value::Integer(44)],
            10,
        )
        .expect("the positional query runs");
    assert_eq!(named.rows, positional.rows);
}

/// The sigil is optional, because a caller writing a map does not want it.
#[test]
fn a_name_can_be_written_with_or_without_its_sigil() {
    let database = peopled("sigil");
    let connection = database.session();
    let sql = "SELECT name FROM people WHERE id = :who";
    for spelling in [":who", "who", "@who", "$who"] {
        let rows = connection
            .query_named(sql, &[(spelling, Value::Integer(2))], 10)
            .unwrap_or_else(|error| panic!("`{spelling}`: {}", error.message));
        assert_eq!(
            rows.rows,
            vec![vec![Value::Text("Grace".to_string())]],
            "`{spelling}` answered {:?}",
            rows.rows
        );
    }
}

/// A name the statement does not use is refused, and the refusal lists the ones
/// it does.
///
/// **Refused rather than ignored.** A typo in a parameter name is the mistake
/// this method exists to prevent, and silently dropping the value would leave
/// the statement's own parameter unbound - which is a different and worse
/// failure two steps later.
#[test]
fn a_name_the_statement_does_not_use_is_refused() {
    let database = peopled("unknown-name");
    let connection = database.session();
    let refused = connection
        .query_named(
            "SELECT name FROM people WHERE id = :who",
            &[(":whom", Value::Integer(2))],
            10,
        )
        .expect_err("an unknown name is refused");
    assert_eq!(refused.status, Status::InvalidState);
    assert!(
        refused.message.contains("whom") && refused.message.contains("who"),
        "the refusal names neither the mistake nor the alternative: {}",
        refused.message
    );
}

/// A parameter with no value given is refused by name.
#[test]
fn a_parameter_with_no_value_is_refused() {
    let database = peopled("missing-value");
    let connection = database.session();
    let refused = connection
        .query_named(
            "SELECT name FROM people WHERE age > :least AND age < :most",
            &[(":least", Value::Integer(38))],
            10,
        )
        .expect_err("a missing value is refused");
    assert!(
        refused.message.contains("most"),
        "the refusal does not say which parameter is missing: {}",
        refused.message
    );
}

/// A named write reports what it changed.
#[test]
fn a_named_write_reports_what_it_changed() {
    let database = peopled("named-write");
    let connection = database.session();
    let changed = connection
        .execute_named(
            "UPDATE people SET age = :age WHERE name = :name",
            &[
                (":age", Value::Integer(37)),
                (":name", Value::Text("Ada".to_string())),
            ],
        )
        .expect("the update runs");
    assert_eq!(changed, 1, "the update reported {changed} changed rows");
    let rows = connection
        .query("SELECT age FROM people WHERE name = 'Ada'", &[], 1)
        .expect("the read runs");
    assert_eq!(
        rows.rows.first().and_then(|row| row.first()),
        Some(&Value::Integer(37))
    );
}

/// The plan cache stops growing at its ceiling.
///
/// **The bound, driven by the shape it exists for**: distinct statement text on
/// every call, which is what generated SQL produces. Before this ticket the
/// count below grew to the number of statements issued and stayed there.
#[test]
fn the_plan_cache_stops_at_its_ceiling() {
    let options = OpenOptions {
        statement_cache: 8,
        ..OpenOptions::default()
    };
    let database =
        Database::open_with(scratch("cache-ceiling"), options).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)")
        .expect("the table is created");

    assert_eq!(
        database.statement_cache_limit(),
        8,
        "the ceiling the caller asked for did not reach the engine"
    );

    for nth in 0..200usize {
        // A distinct statement each time, the way a query builder writes them.
        let sql = format!("SELECT id FROM people WHERE id = {nth}");
        connection.query(&sql, &[], 1).expect("the query runs");
        assert!(
            database.cached_statements() <= 8,
            "the cache holds {} compiled statements after {} distinct queries, past its \
             ceiling of 8",
            database.cached_statements(),
            nth.saturating_add(1)
        );
    }
}

/// A ceiling of zero compiles every statement fresh.
#[test]
fn a_ceiling_of_zero_caches_nothing() {
    let options = OpenOptions {
        statement_cache: 0,
        ..OpenOptions::default()
    };
    let database = Database::open_with(scratch("cache-zero"), options).expect("the database opens");
    let connection = database.session();
    connection
        .execute_batch("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)")
        .expect("the table is created");
    for _ in 0..10 {
        connection
            .query("SELECT id FROM people", &[], 1)
            .expect("the query runs");
    }
    assert_eq!(
        database.cached_statements(),
        0,
        "a ceiling of zero still cached something"
    );
}

/// The cache can be emptied on request, and the count says so.
///
/// The accessor and the clear are a pair: a count nobody can move is a number
/// with nothing to compare against.
#[test]
fn the_cache_can_be_emptied() {
    let database = peopled("cache-clear");
    let connection = database.session();
    for nth in 0..5 {
        let sql = format!("SELECT name FROM people WHERE id = {nth}");
        connection.query(&sql, &[], 1).expect("the query runs");
    }
    assert!(
        database.cached_statements() > 0,
        "nothing was cached, so this case is not about the clear"
    );
    database.clear_statement_cache();
    assert_eq!(database.cached_statements(), 0, "the cache was not emptied");
}

/// The default ceiling is the engine's, and a caller that sets nothing gets it.
#[test]
fn the_default_ceiling_is_the_engines() {
    let database = peopled("cache-default");
    assert_eq!(
        database.statement_cache_limit(),
        inillucent_engine::DEFAULT_STATEMENT_CACHE,
        "the driver's default and the engine's disagree"
    );
    // A default of zero would turn the cache off for everybody, so the
    // constant's own value is asserted rather than a comparison the compiler
    // can fold: `> 0` on a `const` is a constant the lint rightly refuses.
    assert_eq!(
        inillucent_engine::DEFAULT_STATEMENT_CACHE,
        1_000,
        "the default ceiling moved; the note on `OpenOptions::statement_cache` has to move          with it"
    );
}
