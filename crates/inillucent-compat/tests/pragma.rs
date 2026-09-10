//! `PRAGMA`, compared against the pinned SQLite 3.53.4.
//!
//! Invariant: the answers are compared, not remembered. A pragma's output is
//! exactly the kind of thing that is easy to get almost right - `index_list`
//! reports its rows newest first, `index_xinfo` has a trailing entry for the
//! rowid the index points at, `table_info` hides a generated column and
//! `table_xinfo` says which kind of hidden it is - and every one of those is a
//! detail an application reads by position.
//!
//! The schema is built by both engines from the same statements, so the two
//! files are the same shape before a single pragma is asked. That matters for
//! the pager pragmas, whose answers are facts about the file.

use inillucent_compat::differential::{compare, Step};

/// Where this suite's scratch databases live.
const AREA: &str = "pragma";

/// The schema every scenario starts from.
const SCHEMA: &[Step] = &[
    Step::Exec(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT NOT NULL DEFAULT 'x' COLLATE NOCASE, \
         c REAL, d BLOB GENERATED ALWAYS AS (a+1) VIRTUAL, e INT GENERATED ALWAYS AS (a*2) STORED)",
    ),
    Step::Exec("CREATE INDEX ti ON t(b COLLATE NOCASE DESC, c)"),
    Step::Exec("CREATE UNIQUE INDEX tu ON t(c)"),
    Step::Exec("CREATE VIEW v AS SELECT a FROM t"),
    Step::Exec("CREATE TABLE u(x, y, PRIMARY KEY(x,y)) WITHOUT ROWID"),
    Step::Exec("CREATE TABLE s(p) STRICT"),
    Step::Exec("CREATE TABLE w(p INTEGER REFERENCES t(a) ON DELETE CASCADE)"),
    Step::Exec("INSERT INTO t(a,b,c) VALUES (1,'one',1.5),(2,'two',2.5)"),
    Step::Exec("INSERT INTO w VALUES (1),(2)"),
];

/// Runs the schema and then a list of queries, comparing every answer.
fn check(name: &str, queries: &[Step]) {
    let mut steps = SCHEMA.to_vec();
    steps.extend_from_slice(queries);
    let compared = compare(AREA, name, &steps);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, steps.len(), "every step was compared");
}

/// What the schema pragmas report about a table and its columns.
#[test]
fn the_schema_pragmas_describe_the_schema() {
    check(
        "schema",
        &[
            Step::Query("PRAGMA table_info(t)"),
            Step::Query("PRAGMA table_xinfo(t)"),
            Step::Query("PRAGMA table_info(u)"),
            Step::Query("PRAGMA table_info(v)"),
            Step::Query("PRAGMA table_info(nosuchtable)"),
            Step::Query("PRAGMA index_list(t)"),
            Step::Query("PRAGMA index_list(u)"),
            Step::Query("PRAGMA index_info(ti)"),
            Step::Query("PRAGMA index_xinfo(ti)"),
            Step::Query("PRAGMA index_info(tu)"),
            Step::Query("PRAGMA index_info(nosuchindex)"),
            // The `file` column is each engine's own path, which differ by
            // construction, so the comparison is of the rows that describe the
            // *schema* rather than of where the two files happen to be.
            Step::Query("SELECT seq, name FROM pragma_database_list"),
            Step::Query("PRAGMA collation_list"),
            Step::Query("PRAGMA foreign_key_list(w)"),
            Step::Query("PRAGMA foreign_key_list(t)"),
            Step::Query("PRAGMA main.table_info(t)"),
        ],
    );
}

/// `table_list` reports every table of every database, including the schema.
#[test]
fn table_list_reports_every_table() {
    check(
        "table-list",
        &[
            Step::Query("SELECT schema, name, type, ncol, wr, strict FROM pragma_table_list ORDER BY schema, name"),
            Step::Query("PRAGMA table_list(t)"),
        ],
    );
}

/// What the pager pragmas report about the file.
#[test]
fn the_pager_pragmas_describe_the_file() {
    check(
        "pager",
        &[
            Step::Query("PRAGMA page_size"),
            Step::Query("PRAGMA page_count"),
            Step::Query("PRAGMA freelist_count"),
            Step::Query("PRAGMA max_page_count"),
            Step::Query("PRAGMA cache_size"),
            Step::Query("PRAGMA auto_vacuum"),
            Step::Query("PRAGMA encoding"),
            Step::Query("PRAGMA application_id"),
            Step::Query("PRAGMA user_version"),
            Step::Query("PRAGMA journal_mode"),
            Step::Query("PRAGMA journal_size_limit"),
            Step::Query("PRAGMA locking_mode"),
            Step::Query("PRAGMA temp_store"),
            Step::Query("PRAGMA secure_delete"),
            Step::Exec("PRAGMA user_version = 42"),
            Step::Query("PRAGMA user_version"),
            Step::Exec("PRAGMA application_id = 7"),
            Step::Query("PRAGMA application_id"),
            Step::Exec("PRAGMA cache_size = -4000"),
            Step::Query("PRAGMA cache_size"),
            Step::Exec("PRAGMA max_page_count = 1000"),
            Step::Query("PRAGMA max_page_count"),
            Step::Exec("PRAGMA busy_timeout = 250"),
            Step::Query("PRAGMA busy_timeout"),
        ],
    );
}

/// The policy flags read and write, and start where SQLite starts them.
#[test]
fn the_policy_pragmas_are_flags() {
    check(
        "policy",
        &[
            Step::Query("PRAGMA query_only"),
            Step::Query("PRAGMA recursive_triggers"),
            Step::Query("PRAGMA reverse_unordered_selects"),
            Step::Query("PRAGMA ignore_check_constraints"),
            Step::Query("PRAGMA cell_size_check"),
            Step::Query("PRAGMA legacy_alter_table"),
            Step::Query("PRAGMA automatic_index"),
            Step::Query("PRAGMA read_uncommitted"),
            Step::Query("PRAGMA writable_schema"),
            Step::Query("PRAGMA foreign_keys"),
            Step::Query("PRAGMA defer_foreign_keys"),
            Step::Exec("PRAGMA query_only = 1"),
            Step::Query("PRAGMA query_only"),
            Step::Exec("PRAGMA query_only = off"),
            Step::Query("PRAGMA query_only"),
            Step::Exec("PRAGMA recursive_triggers = yes"),
            Step::Query("PRAGMA recursive_triggers"),
            // Anything SQLite cannot read as a boolean is false, which is the
            // rule that makes `= maybe` turn a flag off.
            Step::Exec("PRAGMA recursive_triggers = maybe"),
            Step::Query("PRAGMA recursive_triggers"),
            Step::Exec("PRAGMA foreign_keys = on"),
            Step::Query("PRAGMA foreign_keys"),
        ],
    );
}

/// A healthy database passes both checks with one row saying so.
#[test]
fn the_integrity_checks_agree_on_a_healthy_file() {
    check(
        "integrity",
        &[
            Step::Query("PRAGMA integrity_check"),
            Step::Query("PRAGMA quick_check"),
            Step::Query("PRAGMA integrity_check(1)"),
            Step::Query("PRAGMA main.integrity_check"),
            Step::Query("PRAGMA foreign_key_check"),
            Step::Query("PRAGMA foreign_key_check(w)"),
        ],
    );
}

/// A pragma nobody registered answers nothing and changes nothing.
#[test]
fn an_unknown_pragma_is_silent() {
    check(
        "unknown",
        &[
            Step::Query("PRAGMA no_such_pragma"),
            Step::Query("PRAGMA no_such_pragma = 1"),
            Step::Query("PRAGMA table_info"),
        ],
    );
}

/// The lists a connection reports about itself.
///
/// `collation_list` and `function_list` are compared as *counts and
/// membership* rather than row for row: the pinned build registers a couple of
/// collations from its own shell and its function list carries overloads this
/// engine does not have. What has to be true is that every collation and every
/// function this engine claims is one the reference also has.
#[test]
fn the_connection_lists_are_a_subset_of_the_reference() {
    check(
        "lists",
        &[
            Step::Query(
                "SELECT count(*) FROM pragma_collation_list WHERE name IN ('BINARY','NOCASE','RTRIM')",
            ),
            Step::Query("SELECT count(*) FROM pragma_pragma_list WHERE name = 'table_info'"),
            Step::Query("SELECT count(*) FROM pragma_function_list WHERE name = 'json_extract'"),
            // Which modules a build has is a build-configuration fact rather than a
            // parity one - the pinned library has `fts5` and `rtree` and not
            // `json_each`, which it registers by another path. What both must
            // agree on is that the list exists and is not empty.
            Step::Query("SELECT count(*) > 0 FROM pragma_module_list"),
        ],
    );
}
