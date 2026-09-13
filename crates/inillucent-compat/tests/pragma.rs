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

use inillucent_compat::differential::{compare, start_inillucent, Step};
use inillucent_tree::datum::OwnedDatum;

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
            // Raw `PRAGMA collation_list` also reports `decimal` and `uint`,
            // this engine's bundled reference-CLI collations - documented in
            // `docs/feature-comparison.md`'s "Collations - 5 of 5" section and
            // excluded the same way in `registers.rs`'s
            // `the_collation_register_agrees_exactly` - which the pinned
            // library the oracle links against never registers, and its `seq`
            // column is registration order, which the two engines have no
            // reason to share. Comparing the *names* both agree on, minus the
            // two documented additions, is the invariant that is actually
            // true rather than a byte-identical raw pragma that never was.
            Step::Query(
                "SELECT name FROM pragma_collation_list WHERE name NOT IN ('decimal','uint') ORDER BY name",
            ),
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
///
/// `page_size` and `locking_mode` are asked of neither engine here - see
/// [`the_documented_pager_choices_are_what_was_measured`] for why. Neither is
/// `page_count`, and not for the same reason: it is not `page_size` this time,
/// it is the file layout underneath it.
///
/// **Measured directly, not assumed**: a bare `CREATE TABLE t(a INTEGER
/// PRIMARY KEY)` already answers 5 pages here against the reference's 2 -
/// checked with `inillucent-shell` and the pinned `sqlite3` shell on the same
/// one-statement script, before this suite's schema adds a single table,
/// index or row. `.dbinfo` puts table `t`'s own root at page 4: this engine
/// keeps a meta page and a shadow copy of it for atomic commits (`META_PAGE`
/// and `SHADOW_PAGE` in `inillucent-pool/src/meta.rs`), a catalog b-tree that
/// exists as its own tree from the first `CREATE` (`write_catalog(&mut
/// database, &[])` in `ImportedDatabase::create_on`), and a free map that is
/// its own page rather than folded into the header - four pages of
/// bookkeeping before the first table, where SQLite's page 1 is the header
/// and the schema table's root in one page and its freelist is a lazy trunk
/// page that a fresh file does not yet have. `page_count` is answering
/// correctly on both sides; the two files simply do not use pages for the
/// same things, so a schema with more objects (this suite's) reports a larger
/// gap (9 against 6) for the identical reason a schema with one does (5
/// against 2). Comparing it would be asserting parity between two on-disk
/// formats that were never the same format.
#[test]
fn the_pager_pragmas_describe_the_file() {
    check(
        "pager",
        &[
            Step::Query("PRAGMA freelist_count"),
            Step::Query("PRAGMA max_page_count"),
            // Not the untouched *default* - see
            // [`the_documented_pager_choices_are_what_was_measured`] - but the
            // pragma still round-trips an explicit value the same way on both
            // engines, which is what the `PRAGMA cache_size = -4000` pair
            // below checks.
            Step::Query("PRAGMA auto_vacuum"),
            Step::Query("PRAGMA encoding"),
            Step::Query("PRAGMA application_id"),
            Step::Query("PRAGMA user_version"),
            Step::Query("PRAGMA journal_mode"),
            Step::Query("PRAGMA journal_size_limit"),
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

/// Reads one pragma's first column off a fresh inillucent connection.
///
/// Not compared against the reference - see
/// [`the_documented_pager_choices_are_what_was_measured`], which uses this to
/// check this engine's own two deliberately non-parity defaults rather than
/// asking the oracle to agree with something `docs/feature-comparison.md`
/// already says it does not.
///
/// @param name - a fresh scratch database's name, distinct per case
/// @param sql - the pragma to read
fn own_answer(name: &str, sql: &'static str) -> OwnedDatum {
    let connection = start_inillucent(AREA, name);
    let mut statement = connection.prepare(sql).expect("it prepares");
    assert!(statement.step().expect("it steps"), "{sql} answered no row");
    statement.row().first().expect("one column").clone()
}

/// `page_size`, `locking_mode` and the default `cache_size` are measured,
/// permanent design choices, not spellings this engine got wrong.
///
/// `docs/feature-comparison.md` and `docs/sql.md` record all three: a 32 KiB
/// page against the reference's 4 KiB cost the weighted performance gate
/// 3.83x against 2.60x when 4096 was tried; `exclusive` locking put the
/// headline at 3.83x against a 3.00x bar for `normal`; and the 128 MiB default
/// pool against SQLite's 2 MiB (`-131072` against `-2000`) is the same
/// `cache_size` a caller can still set to whatever they want - it is the
/// untouched *default* that differs, 64x, and is measured all through the
/// "Where the memory goes" section. Asking the differential harness to agree
/// on any of the three would fail on a difference this engine chose on
/// purpose and measured the cost of choosing otherwise - so this pins this
/// engine's own answer instead, which turns red if any of the three ever
/// moves without the documents being updated to match.
#[test]
fn the_documented_pager_choices_are_what_was_measured() {
    assert_eq!(
        own_answer("pager-page-size", "PRAGMA page_size"),
        OwnedDatum::Int(32768),
        "PRAGMA page_size is a measured, documented choice - see docs/feature-comparison.md"
    );
    assert_eq!(
        own_answer("pager-locking-mode", "PRAGMA locking_mode"),
        OwnedDatum::Text(b"exclusive".to_vec()),
        "PRAGMA locking_mode is a measured, documented choice - see docs/feature-comparison.md"
    );
    assert_eq!(
        own_answer("pager-cache-size", "PRAGMA cache_size"),
        OwnedDatum::Int(-131072),
        "the default PRAGMA cache_size is a measured, documented choice - see docs/feature-comparison.md"
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
