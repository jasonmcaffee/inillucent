//! Nested queries used as values, answered by the new engine.
//!
//! Invariant: **an uncorrelated subquery answers the question the data asks at
//! the moment it is asked.** The reduction that makes it cheap - evaluate it
//! once, before the chain is built - is only sound if "once" means once per
//! execution and not once per compile, because statements are cached by their
//! text and the data underneath them moves.
//!
//! The cases here are chosen for the ways that reduction could be wrong rather
//! than to enumerate syntax: a value that goes stale after a write, a `NOT IN`
//! against a set holding NULL, a subquery that reads a bound parameter, one
//! nested inside another, and a correlated one that must still be refused
//! rather than answered with the uncorrelated reading.
//!
//! Expected values are written out rather than compared against the engine's
//! own other path, because a test that asks one engine to check itself agrees
//! with whatever it does.

use inillucent_compat::workspace_root;
use inillucent_engine::connect::{Connection, Database};
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// Returns a database holding the fixture this file asks about.
///
/// `part` is deliberately NULL for one row, because the three-valued rule that
/// `NOT IN` follows is the case a folded set is most likely to get wrong.
///
/// @param name - the test's name, which names its file
fn fixture(name: &str) -> Database {
    let area = workspace_root().join("target/scratch/subquery");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    inillucent_base::testing::remove_database(&path);
    let database = Database::open(&path).expect("a fresh database opens");
    {
        let connection = database.session();
        connection
            .execute_batch(
                "CREATE TABLE item (id INTEGER PRIMARY KEY, name TEXT, price INTEGER); \
                 CREATE TABLE part (id INTEGER PRIMARY KEY, item_id INTEGER); \
                 INSERT INTO item(id, name, price) VALUES (1, 'anvil', 100); \
                 INSERT INTO item(id, name, price) VALUES (2, 'bell', 250); \
                 INSERT INTO item(id, name, price) VALUES (3, 'cog', 250); \
                 INSERT INTO item(id, name, price) VALUES (4, 'drum', 50); \
                 INSERT INTO part(id, item_id) VALUES (10, 2); \
                 INSERT INTO part(id, item_id) VALUES (11, 3); \
                 INSERT INTO part(id, item_id) VALUES (12, NULL)",
            )
            .expect("the fixture loads");
    }
    database
}

/// Returns the first column of every row, as integers.
///
/// @param connection - the connection to ask
/// @param sql - the query
fn integers(connection: &Connection<'_>, sql: &str) -> Vec<i64> {
    connection
        .query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        .iter()
        .map(|row| match row.first() {
            Some(OwnedDatum::Int(number)) => *number,
            Some(OwnedDatum::Null) => i64::MIN,
            other => panic!("{sql} answered {other:?}"),
        })
        .collect()
}

/// A scalar subquery answers with the block's first value, in both positions.
#[test]
fn a_scalar_subquery_answers_in_a_projection_and_in_a_filter() {
    let database = fixture("scalar");
    let connection = database.session();

    assert_eq!(
        integers(&connection, "SELECT (SELECT max(price) FROM item)"),
        vec![250],
        "a scalar subquery in the select list"
    );
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE price = (SELECT max(price) FROM item) ORDER BY id"
        ),
        vec![2, 3],
        "a scalar subquery as the right side of a comparison"
    );
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE price > (SELECT min(price) FROM item) ORDER BY id"
        ),
        vec![1, 2, 3]
    );
    // A block that produces nothing is NULL, not an error and not zero.
    assert_eq!(
        connection
            .query("SELECT (SELECT price FROM item WHERE id = 99)")
            .expect("an empty block is not an error"),
        vec![vec![OwnedDatum::Null]],
        "a scalar subquery over no rows is NULL"
    );
}

/// `IN` and `NOT IN` follow the three-valued rule, NULL in the set included.
///
/// The case that matters is `NOT IN` against a set holding NULL: SQLite answers
/// no rows, because "not equal to every member" cannot be established when one
/// member is unknown. A folded set that dropped the NULL would answer three.
#[test]
fn in_and_not_in_follow_the_three_valued_rule() {
    let database = fixture("in");
    let connection = database.session();

    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE id IN (SELECT item_id FROM part) ORDER BY id"
        ),
        vec![2, 3],
        "the NULL member matches nothing but does not remove the matches"
    );
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE id NOT IN (SELECT item_id FROM part) ORDER BY id"
        ),
        Vec::<i64>::new(),
        "NOT IN against a set holding NULL is unknown for every row"
    );
    // The same query with the NULL excluded, which is the control: if this also
    // answered nothing, the rule above would be passing for the wrong reason.
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE id NOT IN \
             (SELECT item_id FROM part WHERE item_id IS NOT NULL) ORDER BY id"
        ),
        vec![1, 4],
        "with no NULL in the set, NOT IN answers the complement"
    );
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE id IN (SELECT id FROM item WHERE price > 200) ORDER BY id"
        ),
        vec![2, 3]
    );
}

/// `EXISTS` and `NOT EXISTS` answer from whether the block produced a row.
#[test]
fn exists_answers_from_whether_the_block_produced_a_row() {
    let database = fixture("exists");
    let connection = database.session();

    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE EXISTS (SELECT 1 FROM part WHERE id = 10) ORDER BY id"
        ),
        vec![1, 2, 3, 4],
        "a block that produces a row lets every outer row through"
    );
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE EXISTS (SELECT 1 FROM part WHERE id = 99) ORDER BY id"
        ),
        Vec::<i64>::new(),
        "a block that produces nothing lets none through"
    );
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE NOT EXISTS (SELECT 1 FROM part WHERE id = 99) ORDER BY id"
        ),
        vec![1, 2, 3, 4],
        "NOT EXISTS is the negation and not a second reading of the same thing"
    );
}

/// The value is read again after the data it came from changes.
///
/// **The case the fold exists to get right.** Statements are cached by their
/// text, so a subquery folded into the cached plan would answer the second
/// execution with the first execution's data. The same connection runs the same
/// statement text on either side of an insert here for exactly that reason.
#[test]
fn a_folded_subquery_is_read_again_after_a_write() {
    let database = fixture("stale");
    let connection = database.session();

    let query = "SELECT (SELECT count(*) FROM item)";
    assert_eq!(integers(&connection, query), vec![4]);

    connection
        .execute_batch("INSERT INTO item(id, name, price) VALUES (5, 'edge', 10)")
        .expect("the insert applies");

    assert_eq!(
        integers(&connection, query),
        vec![5],
        "the same statement text answered with the count from before the insert"
    );

    // And the same for a set, which takes the other path through the fold.
    let membership = "SELECT id FROM item WHERE id IN (SELECT item_id FROM part) ORDER BY id";
    assert_eq!(integers(&connection, membership), vec![2, 3]);
    connection
        .execute_batch("INSERT INTO part(id, item_id) VALUES (13, 4)")
        .expect("the insert applies");
    assert_eq!(
        integers(&connection, membership),
        vec![2, 3, 4],
        "a folded IN set answered with the membership from before the insert"
    );
}

/// A subquery reads the parameters the outer statement was bound with.
#[test]
fn a_subquery_sees_the_statement_s_parameters() {
    let database = fixture("params");
    let connection = database.session();

    let rows = connection
        .query_with(
            "SELECT id FROM item WHERE price = (SELECT max(price) FROM item WHERE price < ?1) \
             ORDER BY id",
            &Params::from_values(vec![OwnedDatum::Int(250)]),
        )
        .expect("the query runs");
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|row| match row.first() {
            Some(OwnedDatum::Int(number)) => Some(*number),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec![1],
        "the block did not see the binding the outer statement was given"
    );
}

/// A subquery inside a subquery is answered innermost first.
#[test]
fn a_nested_subquery_is_answered_from_the_inside_out() {
    let database = fixture("nested");
    let connection = database.session();

    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE price = \
             (SELECT max(price) FROM item WHERE price < (SELECT max(price) FROM item)) \
             ORDER BY id"
        ),
        vec![1],
        "the inner block has to be answered before the outer one can run"
    );
}

/// A correlated subquery is answered per row, not folded and not refused.
///
/// It reads a column of the row being tested, so it has no single value and the
/// fold cannot stand in for it - which is why it was refused by name before
/// correlated subqueries were supported. `inillucent-exec`'s `correlate`
/// computes it beside the row, one column per block: the block is planned
/// **once** with its outer references rewritten into parameters, and each row
/// binds them and runs it.
///
/// Both directions are asserted, because a correlated `EXISTS` that answered
/// the same thing for every row would look right on a fixture where every row
/// matches.
#[test]
fn a_correlated_subquery_is_answered_per_row() {
    let database = fixture("correlated");
    let connection = database.session();

    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE EXISTS (SELECT 1 FROM part WHERE part.item_id = item.id) \
             ORDER BY id",
        ),
        integers(
            &connection,
            "SELECT DISTINCT item_id FROM part WHERE item_id IS NOT NULL ORDER BY item_id",
        ),
        "EXISTS answered a different set from the one the parts name"
    );
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE NOT EXISTS (SELECT 1 FROM part WHERE part.item_id = item.id) \
             ORDER BY id",
        ),
        integers(
            &connection,
            // `NOT IN` over a list holding a NULL is never true, which is
            // SQL's own rule and not this engine's - so the comparison excludes
            // it rather than comparing two different questions. The rule itself
            // is asserted below.
            "SELECT id FROM item WHERE id NOT IN              (SELECT item_id FROM part WHERE item_id IS NOT NULL) ORDER BY id",
        ),
        "NOT EXISTS and NOT IN disagreed about the same question"
    );
    // And the three-valued rule the exclusion above steps around: a `NOT IN`
    // whose list holds a NULL answers nothing at all.
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE id NOT IN (SELECT item_id FROM part) ORDER BY id",
        ),
        Vec::<i64>::new(),
        "NOT IN over a list holding NULL answered something"
    );
}

/// A subquery inside a compound arm is folded like any other.
///
/// A compound runs each arm through its own build, so this checks the table is
/// filled for the whole statement rather than for the arm that happened to be
/// built first - the arms share one numbering, and an arm whose slot was still
/// empty would be refused as though it were correlated.
#[test]
fn a_subquery_in_a_compound_arm_is_folded_too() {
    let database = fixture("compound");
    let connection = database.session();

    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM item WHERE id IN (SELECT item_id FROM part)              UNION              SELECT id FROM item WHERE price = (SELECT min(price) FROM item)              ORDER BY id"
        ),
        vec![2, 3, 4],
        "one arm's subquery was not folded"
    );
}

/// An `IN` over a subquery answers the same as the `IN` over the same literals.
///
/// The planner chooses the access path before the fold happens, so a subquery
/// in a `WHERE` is a residual predicate to it and the scan is not narrowed by
/// the folded set. That is a missing optimization rather than a wrong answer,
/// and this pins the answer so the optimization can be added later against a
/// test that already says what it must not change.
#[test]
fn a_folded_in_answers_the_same_as_the_literal_in() {
    let database = fixture("aslist");
    let connection = database.session();

    let folded = integers(
        &connection,
        "SELECT id FROM item WHERE id IN (SELECT item_id FROM part) ORDER BY id",
    );
    let literal = integers(
        &connection,
        "SELECT id FROM item WHERE id IN (2, 3, NULL) ORDER BY id",
    );
    assert_eq!(
        folded, literal,
        "the folded set is not the list it stands for"
    );
    assert_eq!(folded, vec![2, 3]);
}

/// A subquery in a `VALUES` list and in a `SET` is folded too.
///
/// These two are the paths with expressions and **no plan**: a `VALUES` list is
/// evaluated by the write path directly, and an `UPDATE`'s assignments are
/// evaluated after the plan has found the rows. The plan-shaped fold never sees
/// either, so before `fold_expressions` existed they came back as "a correlated
/// subquery used as a value" - which is what an unfilled slot looks like from
/// inside the physical pass, and a false statement about the query.
///
/// Both answers are SQLite 3.53.4's for the same script: `1|one`, `2|one`,
/// `3|three`.
#[test]
fn a_subquery_in_a_values_list_and_in_a_set_is_folded() {
    let database = fixture("novplan");
    let connection = database.session();
    connection
        .execute_batch("DELETE FROM item")
        .expect("the fixture table is emptied");
    connection
        .execute_batch(
            "INSERT INTO item(id, name, price) VALUES (1, 'one', 1);              INSERT INTO item(id, name, price) VALUES (2, 'two', 2)",
        )
        .expect("two rows load");

    connection
        .execute_batch(
            "INSERT INTO item(id, name, price) VALUES ((SELECT max(id) FROM item) + 1, 'three', 3)",
        )
        .expect("a subquery in a VALUES list is answered");
    connection
        .execute_batch("UPDATE item SET name = (SELECT name FROM item WHERE id = 1) WHERE id = 2")
        .expect("a subquery in a SET is answered");

    let names: Vec<String> = connection
        .query("SELECT name FROM item ORDER BY id")
        .expect("the rows read back")
        .iter()
        .map(|row| match row.first() {
            Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("name was {other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        vec!["one".to_string(), "one".to_string(), "three".to_string()],
        "the rowid the VALUES subquery computed, or the value the SET subquery read, is wrong"
    );
}

/// A compound query is usable as a derived table, for all four set operators.
///
/// **What this is a regression test for.** `SELECT ... FROM (a
/// UNION ALL b)` came back `the new engine's physical pass does not handle a
/// compound query yet`, and the refusal was in the wrong place: the executor has
/// answered compounds at the top level all along, and a derived table only wants
/// the rows they produce. `plan_stages` refuses a plan carrying arms, and the
/// materialised path went through it - so the arms were refused for a shape that
/// had nothing to do with why they cannot be planned as one pipeline.
///
/// The four operators are all checked because they are not one code path: `UNION
/// ALL` concatenates, `UNION` de-duplicates, and `EXCEPT` and `INTERSECT` need
/// the right arm complete before the left arm's first row can be judged. A fix
/// that reached only the first would look right on the query that reported it.
///
/// The expected values are written out. `price` is 250 twice on purpose, so
/// `UNION` and `UNION ALL` differ and a fix that answered one for the other is
/// visible.
#[test]
fn a_compound_query_is_usable_as_a_derived_table() {
    let database = fixture("compound-derived");
    let connection = database.session();

    // Two arms of two rows each, and 250 appears in both.
    let all =
        "SELECT price FROM item WHERE price >= 250 UNION ALL SELECT price FROM item WHERE id <= 2";
    assert_eq!(
        integers(&connection, &format!("SELECT count(*) FROM ({all})")),
        vec![4],
        "UNION ALL keeps every row of both arms"
    );
    assert_eq!(
        integers(&connection, &format!("SELECT sum(price) FROM ({all})")),
        vec![250 + 250 + 100 + 250],
        "and the values are the arms' own"
    );

    let distinct =
        "SELECT price FROM item WHERE price >= 250 UNION SELECT price FROM item WHERE id <= 2";
    assert_eq!(
        integers(&connection, &format!("SELECT count(*) FROM ({distinct})")),
        vec![2],
        "UNION keeps one of each value: 100 and 250"
    );

    let except = "SELECT price FROM item EXCEPT SELECT price FROM item WHERE price < 200";
    assert_eq!(
        integers(&connection, &format!("SELECT count(*) FROM ({except})")),
        vec![1],
        "EXCEPT leaves only 250"
    );

    let intersect = "SELECT price FROM item INTERSECT SELECT price FROM item WHERE id <= 2";
    assert_eq!(
        integers(&connection, &format!("SELECT count(*) FROM ({intersect})")),
        vec![2],
        "INTERSECT leaves 100 and 250"
    );

    // And the derived table is a term like any other: it can be filtered,
    // ordered and joined, which is what says the rows really did arrive in the
    // pipeline rather than being answered by a special case.
    assert_eq!(
        integers(
            &connection,
            &format!("SELECT price FROM ({distinct}) WHERE price > 200")
        ),
        vec![250],
        "a derived compound can be filtered"
    );
    assert_eq!(
        integers(
            &connection,
            &format!("SELECT count(*) FROM ({all}) AS c JOIN item i ON i.price = c.price")
        ),
        vec![6 + 1],
        "and joined: three rows of 250 against two items each, and one of 100 against one"
    );
}

/// `sqlite_sequence` can be written, which is how an AUTOINCREMENT counter is
/// reset.
///
/// **What this is a regression test for.** Every table whose
/// name begins with `sqlite_` was refused as a write target, which is right for
/// the schema and wrong for the two SQLite itself lets an application write.
/// `UPDATE sqlite_sequence SET seq = 0 WHERE name = 't'` is the documented way
/// to restart a counter and there was no other way to do it at all.
///
/// The counter is checked by *using* it: the assertion is the id the next insert
/// receives, not the row the update wrote. A fix that made the write succeed and
/// left the allocator reading its own cached number would pass the second and
/// fail the first.
#[test]
fn sqlite_sequence_can_be_written_and_the_counter_follows() {
    let area = workspace_root().join("target/scratch/sequence");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join("sequence.rdb");
    inillucent_base::testing::remove_database(&path);
    let database = Database::open(&path).expect("a fresh database opens");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE s (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);\
             INSERT INTO s (v) VALUES ('a');\
             INSERT INTO s (v) VALUES ('b');",
        )
        .expect("the fixture loads");
    assert_eq!(
        integers(
            &connection,
            "SELECT seq FROM sqlite_sequence WHERE name = 's'"
        ),
        vec![2]
    );

    connection
        .execute_batch("UPDATE sqlite_sequence SET seq = 100 WHERE name = 's'")
        .expect("the counter is written");
    assert_eq!(
        integers(
            &connection,
            "SELECT seq FROM sqlite_sequence WHERE name = 's'"
        ),
        vec![100],
        "the row says what was written"
    );
    connection
        .execute_batch("INSERT INTO s (v) VALUES ('c')")
        .expect("a row is inserted");
    assert_eq!(
        integers(&connection, "SELECT max(id) FROM s"),
        vec![101],
        "and the allocator read it: the next id is one past the number written"
    );

    // The other half of the documented use: deleting the row restarts the
    // counter from the table's own highest rowid rather than from one.
    connection
        .execute_batch("DELETE FROM sqlite_sequence WHERE name = 's'")
        .expect("the counter row is deleted");
    connection
        .execute_batch("INSERT INTO s (v) VALUES ('d')")
        .expect("a row is inserted");
    assert_eq!(integers(&connection, "SELECT max(id) FROM s"), vec![102]);

    // And the schema itself is still refused, which is what `PRAGMA
    // writable_schema` is for and is not what this changed.
    assert!(
        connection
            .execute_batch("DELETE FROM sqlite_schema WHERE name = 's'")
            .is_err(),
        "the schema table is still not writable"
    );
}

/// A `WHERE` conjunct that reads no subquery rejects a row before the row's
/// correlated block is answered.
///
/// **The block here fails on the rejected row, so the order is visible in the
/// answer rather than only in the clock** (task-2076). `abs` of the smallest
/// integer is an integer overflow, and the row holding it is the one
/// `v > 0` throws away. Before the change the correlation operator answered
/// every row the scan produced and the filter ran afterwards, so this query
/// failed. SQLite 3.53.4 answers it, in both orders the terms can be written
/// in. The third assertion below checks that the same block with nothing in
/// front of it does fail, so the first two are passing because the row was
/// skipped and not because `abs` stopped failing.
#[test]
fn a_cheap_conjunct_rejects_a_row_before_its_correlated_block_is_answered() {
    let database = fixture("gate_order");
    let connection = database.session();
    connection
        .execute_batch(
            "CREATE TABLE acct (id INTEGER PRIMARY KEY, v INTEGER); \
             CREATE TABLE one (n INTEGER); \
             INSERT INTO one(n) VALUES (1); \
             INSERT INTO acct(id, v) VALUES (1, 5); \
             INSERT INTO acct(id, v) VALUES (2, -9223372036854775808)",
        )
        .expect("the accounts load");

    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM acct WHERE v > 0 AND (SELECT abs(acct.v) FROM one) > 0",
        ),
        vec![1],
        "the block was answered for the row the cheap conjunct rejects"
    );
    assert_eq!(
        integers(
            &connection,
            "SELECT id FROM acct WHERE (SELECT abs(acct.v) FROM one) > 0 AND v > 0",
        ),
        vec![1],
        "writing the block first put it in front of the cheap conjunct"
    );
    assert!(
        connection
            .query("SELECT id FROM acct WHERE (SELECT abs(acct.v) FROM one) > 0")
            .is_err(),
        "abs of the smallest integer no longer fails, so the two answers above prove nothing"
    );
}

/// Moving the cheap conjuncts in front of a correlated block changes no answer.
///
/// Each case is one way the split between the conjuncts tested first and the
/// ones left in the filter could be wrong: a filter and a block that disagree,
/// a `NOT EXISTS`, a block in the projection with a filter beside it, an `OR`
/// that must stay whole because one side of it reads the block, an `AND`
/// nested inside parentheses, a conjunct whose verdict is NULL, and a join,
/// where the conjunct reads the inner table and the block the outer one.
/// Expected values are written out from the fixture: items 1 to 4 priced 100,
/// 250, 250 and 50, and parts naming items 2 and 3.
#[test]
fn a_filtered_correlated_query_answers_what_sqlite_answers() {
    let database = fixture("gate_answers");
    let connection = database.session();
    let exists = "EXISTS (SELECT 1 FROM part WHERE part.item_id = item.id)";
    let cases: Vec<(String, Vec<i64>)> = vec![
        (
            format!("SELECT id FROM item WHERE price < 200 AND {exists} ORDER BY id"),
            vec![],
        ),
        (
            format!("SELECT id FROM item WHERE price = 250 AND {exists} ORDER BY id"),
            vec![2, 3],
        ),
        (
            format!("SELECT id FROM item WHERE price < 200 AND NOT {exists} ORDER BY id"),
            vec![1, 4],
        ),
        (
            "SELECT id * 10 + (SELECT count(*) FROM part WHERE part.item_id = item.id) \
             FROM item WHERE price >= 100 ORDER BY id"
                .to_string(),
            vec![10, 21, 31],
        ),
        (
            format!("SELECT id FROM item WHERE price < 60 OR {exists} ORDER BY id"),
            vec![2, 3, 4],
        ),
        (
            format!(
                "SELECT id FROM item WHERE price > 60 AND (price < 200 OR {exists}) ORDER BY id"
            ),
            vec![1, 2, 3],
        ),
        (
            format!(
                "SELECT id FROM item WHERE price >= 100 AND (price > 200 AND {exists}) ORDER BY id"
            ),
            vec![2, 3],
        ),
        (
            format!(
                "SELECT id FROM item WHERE nullif(price, 100) > 0 AND NOT {exists} ORDER BY id"
            ),
            vec![4],
        ),
        (
            "SELECT item.id FROM item JOIN part ON part.item_id = item.id \
             WHERE part.id > 10 AND EXISTS \
             (SELECT 1 FROM item other WHERE other.price = item.price AND other.id <> item.id) \
             ORDER BY item.id"
                .to_string(),
            vec![3],
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(integers(&connection, &sql), expected, "{sql}");
    }
}
