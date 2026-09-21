//! What a caller reads back besides the rows: column names, failure codes, and
//! the counters a failed statement leaves behind.
//!
//! Invariant: **every part of an answer is the pinned SQLite 3.53.4's, not only
//! the rows.** A driver binds by column name, branches on the primary result
//! code, and reads `changes()` after a statement that did not work; each of
//! those is exact, and each of them was wrong here while every row was right.
//!
//! `differential::compare` is what grades all three at once - the row-only
//! harnesses in `advanced_sql.rs` and the `grade` helpers elsewhere compare
//! rendered rows and stop, which is why these three went unnoticed:
//!
//! 1. A result column with no alias is named after the text it was written as,
//!    cut out of the statement by the expression's span. The span ran to the
//!    *interned* name's position, and interning deduplicates, so the second
//!    `COLLATE NOCASE` in one SELECT resolved to the first one's entry and the
//!    column was called `NOCASE, 'a' < 'B'` - a slice that starts inside one
//!    column and ends inside the one before it. `OVER w` named twice did the
//!    same thing (task-1913).
//! 2. A function that refuses answered primary code 21, `SQLITE_MISUSE`, where
//!    SQLite answers 1. 21 is what SQLite reports for misusing the C API,
//!    which is not something a caller writing SQL can do, so a driver
//!    branching on the code saw a class of failure that had not happened.
//! 3. The counters after a statement that failed part way through stepping.
//!    This one was the *oracle* rather than the engine: its reply on that path
//!    carried no state at all and the reader defaults a missing field to zero,
//!    so `changes()` was compared against a 0 nobody measured.

use inillucent_compat::differential::{compare, Step};

/// Where this suite's scratch databases live.
const AREA: &str = "result-names-and-codes";

/// Runs a list of steps against both engines, failing on any difference.
///
/// @param name - the scenario's name, which names its scratch database
/// @param steps - the statements, in order
fn check(name: &str, steps: &[Step]) {
    let compared = compare(AREA, name, steps);
    if compared == 0 {
        // `compare` has already announced the skip, and under `--strict` it
        // panicked rather than returning at all.
        return;
    }
    assert_eq!(compared, steps.len(), "every step was compared");
}

/// A collation named twice in one SELECT names both columns correctly.
///
/// The first column was always right, which is what made this hard to see: the
/// first occurrence of a name interns itself and carries its own position, and
/// only the second and later ones borrow it.
#[test]
fn a_collation_named_twice_does_not_rename_the_second_column() {
    check(
        "collate-twice",
        &[
            Step::Query("SELECT 'a' = 'A' COLLATE NOCASE, 'a' < 'B' COLLATE NOCASE, 'a' < 'B'"),
            Step::Query("SELECT 1 COLLATE BINARY, 2 COLLATE BINARY, 3 COLLATE BINARY"),
            Step::Query("SELECT 'x' COLLATE RTRIM, 'y' COLLATE RTRIM"),
        ],
    );
}

/// A window named twice in one SELECT names both columns correctly.
#[test]
fn a_window_named_twice_does_not_rename_the_second_column() {
    check(
        "over-twice",
        &[
            Step::Exec("CREATE TABLE t (n INTEGER)"),
            Step::Exec("INSERT INTO t VALUES (1), (2), (3)"),
            Step::Query(
                "SELECT n, first_value(n) OVER w, last_value(n) OVER w FROM t \
                 WINDOW w AS (ORDER BY n)",
            ),
            Step::Query(
                "SELECT row_number() OVER w, rank() OVER w, dense_rank() OVER w FROM t \
                 WINDOW w AS (ORDER BY n)",
            ),
        ],
    );
}

/// A table named twice on the right of `IN` names both columns correctly.
///
/// The same interned span, in the other place that closed an expression with
/// one. `x IN t` is the table-name form of `IN`, so the column's name runs to
/// wherever `t` was first written rather than to this mention of it.
#[test]
fn a_table_named_twice_on_the_right_of_in_does_not_rename_the_second_column() {
    check(
        "in-table-twice",
        &[
            Step::Exec("CREATE TABLE n (v INTEGER)"),
            Step::Exec("INSERT INTO n VALUES (1), (2)"),
            Step::Query("SELECT 1 IN n, 2 IN n, 3 IN n"),
        ],
    );
}

/// `abs()` of the smallest integer refuses, with the reference's own code.
///
/// Its absolute value is one past the largest integer. This engine answered
/// `9.22337203685478e18` - a real, and a wrong answer a caller has no way to
/// tell from a right one.
#[test]
fn abs_of_the_smallest_integer_refuses_the_way_the_reference_does() {
    check(
        "abs-overflow",
        &[
            Step::Query("SELECT abs(-9223372036854775808)"),
            Step::Query("SELECT abs(-9223372036854775807), abs(-1.5), abs('x'), abs(NULL)"),
        ],
    );
}

/// An integer `sum()` that overflows refuses, with the reference's own code.
///
/// The refusal was already right and the code was not: 21 where SQLite answers
/// 1.
#[test]
fn an_aggregate_overflow_refuses_with_the_references_code() {
    check(
        "sum-overflow",
        &[
            Step::Exec("CREATE TABLE big (n INTEGER)"),
            Step::Exec("INSERT INTO big VALUES (9223372036854775807), (9223372036854775807)"),
            Step::Query("SELECT sum(n) FROM big"),
            Step::Query("SELECT total(n), avg(n) FROM big"),
        ],
    );
}

/// An aggregate in a compound arm after the first answers, and names its
/// column the way the reference names it.
///
/// **This is the code half of task-2042; `semantics.rs` holds the rows.** The
/// binder threw away the `aggregates` of every arm but the head, so the
/// physical pass was handed a result column that was still a
/// `BoundExpr::Aggregate` in a plan that claimed to aggregate nothing, and
/// refused it: `the new engine's physical pass does not handle the expression
/// an aggregate yet`, reported as primary code 21 where SQLite answers rows.
/// All four compound operators did it and every aggregate function did it,
/// while the same aggregate in the *head* arm answered - which is what named
/// the binder rather than the executor.
///
/// The column name is compared for the same reason the rest of this file
/// compares it: a compound's result columns are named by its **first** arm, so
/// `SELECT 1 UNION ALL SELECT count(*) FROM t` has a column called `1` and not
/// one called `count(*)`. A driver binding by name reads that.
#[test]
fn an_aggregate_in_a_later_compound_arm_answers_the_way_the_reference_does() {
    check(
        "compound-arm-aggregate",
        &[
            Step::Exec("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)"),
            Step::Exec("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)"),
            Step::Query("SELECT 1 UNION ALL SELECT count(*) FROM t"),
            Step::Query("SELECT 1 UNION SELECT count(*) FROM t"),
            Step::Query("SELECT 1 EXCEPT SELECT count(*) FROM t"),
            Step::Query("SELECT 3 INTERSECT SELECT count(*) FROM t"),
            Step::Query("SELECT 1 UNION ALL SELECT sum(a) FROM t"),
            Step::Query("SELECT count(*) FROM t UNION ALL SELECT 2"),
        ],
    );
}

/// A grouped aggregate in a later compound arm answers its counts, not blanks.
///
/// **The half that answered rather than refusing, which is why it is here and
/// not only in the acceptance suites.** `plan_select_with` reads
/// `AggregationMode::Grouped` whenever `group_by` has a term in it and
/// nothing else, so an arm whose aggregates the binder had discarded still
/// planned and built a grouped aggregate - one with no accumulators in it. The
/// projection then read column `group_width + 0`, which is one past the end of
/// a grouped row that carries only the key,
/// and reading past the end of a row is this engine's NULL. `SELECT 1 UNION
/// ALL SELECT count(*) FROM t GROUP BY a` answered `1` and then three empty
/// values, exit code 0, with nothing for a caller to branch on.
///
/// It is also why the steps below ask for the counts of groups of different
/// sizes rather than one group: every accumulator being absent gives the same
/// NULL whatever the data, so a single group would pass against an engine that
/// had lost the aggregate again and simply agreed by accident.
#[test]
fn a_grouped_aggregate_in_a_later_compound_arm_answers_its_counts() {
    check(
        "compound-arm-grouped",
        &[
            Step::Exec("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, g TEXT)"),
            Step::Exec("INSERT INTO t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, 30, 'x'), (4, 20, 'z')"),
            Step::Query("SELECT 1 UNION ALL SELECT count(*) FROM t GROUP BY a"),
            Step::Query("SELECT 1 UNION ALL SELECT count(*) FROM t GROUP BY g ORDER BY 1"),
            Step::Query("SELECT 0 UNION ALL SELECT count(*) FROM t GROUP BY g HAVING count(*) > 1"),
            Step::Query(
                "SELECT g, count(*) FROM t GROUP BY g                  UNION ALL SELECT g, sum(a) FROM t GROUP BY g ORDER BY 1, 2",
            ),
        ],
    );
}

/// A window function in a later compound arm answers too.
///
/// The binder takes `windows` off itself in the same place it takes
/// `aggregates`, so this was refused by the same root cause - `the expression
/// a WindowRef expression` - and is fixed by the same line. It is asserted
/// separately because a change that restored one list and not the other would
/// leave every aggregate case green.
#[test]
fn a_window_function_in_a_later_compound_arm_answers() {
    check(
        "compound-arm-window",
        &[
            Step::Exec("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)"),
            Step::Exec("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)"),
            Step::Query("SELECT 1 UNION ALL SELECT row_number() OVER (ORDER BY a) FROM t"),
            Step::Query("SELECT 1 UNION ALL SELECT sum(a) OVER (ORDER BY a) FROM t"),
        ],
    );
}

/// A statement that fails part way through leaves the counters the reference
/// leaves.
///
/// **This is the case the oracle could not answer.** Its reply on that path
/// carried no `changes`, `total_changes`, `last_insert_rowid` or `autocommit`,
/// and the reader defaults each to zero - so the comparison graded the
/// engine's real counters against four numbers nobody had measured, and
/// `autocommit` against a `false` that is wrong for every statement outside a
/// transaction. The rows written before the failure are what make it mean
/// something: with `changes()` at zero, a harness that fabricates zero and one
/// that measures it agree.
#[test]
fn a_failed_statement_leaves_the_counters_the_reference_leaves() {
    check(
        "counters-after-a-failure",
        &[
            Step::Exec("CREATE TABLE counted (n INTEGER)"),
            Step::Exec("INSERT INTO counted VALUES (1), (2), (3)"),
            Step::Query("SELECT changes(), total_changes()"),
            Step::Query("SELECT abs(-9223372036854775808)"),
            Step::Query("SELECT changes(), total_changes()"),
        ],
    );
}

/// `HAVING` with no `GROUP BY` answers, and names its column, the way the
/// reference does.
///
/// **It was a syntax error here (task-2040).** The grammar read `HAVING` only
/// inside the `GROUP BY` arm, so `SELECT count(*) AS n FROM t HAVING n > 0`
/// stopped at `near "HAVING": syntax error` where SQLite answers the count - a
/// statement with an aggregate and no `GROUP BY` is one group over the whole
/// table, and the `HAVING` filters that one group.
///
/// The column name is graded here as well as the rows because the alias is
/// what the `HAVING` refers back to: `n` has to name the result column *and*
/// resolve inside the `HAVING`, and a binder that dropped one of those would
/// still answer the right number under a different heading.
#[test]
fn having_with_no_group_by_answers_as_the_reference_does() {
    check(
        "having-no-group-by",
        &[
            Step::Exec("CREATE TABLE h (a INTEGER PRIMARY KEY, b INTEGER)"),
            Step::Exec("INSERT INTO h VALUES (1, 30), (2, 10), (3, 20)"),
            Step::Query("SELECT count(*) AS n FROM h HAVING n > 0"),
            Step::Query("SELECT count(*) AS n FROM h HAVING n > 9"),
            Step::Query("SELECT sum(b) AS s, avg(b) FROM h HAVING s > 50"),
            Step::Query("SELECT max(b) AS m, a FROM h HAVING m > 25"),
            Step::Query("SELECT count(*) FROM h HAVING b > 0"),
        ],
    );
}

/// `HAVING` on a query that does not aggregate refuses with the reference's own
/// code, which is 1 rather than 21.
///
/// **This is the half of task-2040 that stays refused, and the reason the fix
/// is not "accept everything the grammar now parses".** SQLite makes a
/// statement an aggregating one by finding an aggregate among the *result
/// columns* and by nothing else: an aggregate that appears only in the
/// `HAVING`, or only in the `ORDER BY`, does not. So all four of these are
/// `HAVING clause on a non-aggregate query` there, and a binder that asked
/// `self.aggregates.is_empty()` after binding the `HAVING` would have answered
/// rows for two of them.
///
/// The code is graded rather than the sentence alone because the point of the
/// ticket was the *class* of the failure. It used to be
/// `ParseErrorKind::Unsupported`, which `inillucent-driver` reports as
/// `unsupported` and the command line as exit code 3 - the pair that means
/// "this engine has not built that yet". No release will build this one, so a
/// caller branching on it would be waiting for a feature that is not coming.
#[test]
fn having_on_a_non_aggregate_query_refuses_with_the_references_code() {
    check(
        "having-non-aggregate",
        &[
            Step::Exec("CREATE TABLE h (a INTEGER PRIMARY KEY, b INTEGER)"),
            Step::Exec("INSERT INTO h VALUES (1, 30), (2, 10), (3, 20)"),
            Step::Query("SELECT b FROM h HAVING b > 15"),
            Step::Query("SELECT 1 FROM h HAVING count(*) > 0"),
            Step::Query("SELECT 1 FROM h HAVING 1 ORDER BY count(*)"),
            Step::Query("SELECT 1 HAVING 1"),
            Step::Query("SELECT b FROM h GROUP BY b HAVING b > 15"),
        ],
    );
}
