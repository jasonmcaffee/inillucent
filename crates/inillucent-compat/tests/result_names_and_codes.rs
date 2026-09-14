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
