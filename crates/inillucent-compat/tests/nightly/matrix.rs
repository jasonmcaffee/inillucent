//! The SQL statement matrix at night: every triple of axis values for every
//! family, at every configuration arm, and the retained corpus.
//!
//! Invariant: **every generated case of every family runs at every arm on
//! files on both engines and is graded against the pinned SQLite, and each
//! group fails once, naming every case that disagreed and every `known.list`
//! line that no longer does.** The merge cadence runs the triples at two arms;
//! this runs them at all six. The cases and the grading are in
//! `inillucent_compat::statement_matrix`; section 8 of
//! `tasks/task-2135-sql-statement-matrix-tdd.md` says what each cadence runs.

/// Says the pinned SQLite oracle is missing, so the groups graded nothing.
fn oracle_missing() {
    inillucent_compat::differential::announce_skip();
}

/// One module per family, each with four groups.
macro_rules! nightly_families {
    ($($family:ident),+ $(,)?) => {
        $(
            mod $family {
                use super::oracle_missing;
                inillucent_compat::matrix_family!($family, Nightly, [g0, g1, g2, g3]);
            }
        )+
    };
}

nightly_families!(
    select,
    join,
    compound,
    cte,
    window,
    subquery,
    expression,
    function,
    insert,
    update,
    delete,
    ddl_table,
    ddl_index,
    ddl_view,
    trigger,
    constraint,
    transaction,
    vtab,
    schema,
    maintenance,
    pragma,
    vector,
    retained,
);
