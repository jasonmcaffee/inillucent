//! What this engine can and cannot do, declared and checked.
//!
//! Invariant: **every row here is asserted in both directions by a test that
//! runs the construct.** A capability declared supported whose probe fails is a
//! lie the driver was telling. A capability declared *unsupported* whose probe
//! now **succeeds** fails the same test, saying the engine has grown it and
//! this table is out of date.
//!
//! The second half is the point, and it is the difference between this and
//! `java.sql.DatabaseMetaData`. JDBC has had `supportsFullOuterJoins()` for
//! thirty years and its answers are famously unreliable, because every driver
//! hand-writes them and nothing checks them: a list of claims nobody verifies
//! decays into a list of claims that were true once, and an application ends up
//! refusing to offer something that has worked for six months. Checking only
//! the supported half would leave exactly that failure open, since the engine
//! is under active development and its gaps are scheduled to close.
//!
//! This is the instrument `docs/invariants/layering.toml` already applies to
//! the dependency graph - *"checked, not documented: an architecture rule that
//! is only written down is a rule that has already been broken somewhere"* -
//! pointed at a different contract.
//!
//! ## Where the gaps come from
//!
//! Each gap traces to a deliberately red qualification suite in
//! `inillucent-compat` naming it. This table is that list in a form an
//! application can read at run time. It is not a wish list and closing any of
//! its gaps is Phase 6's work, not this crate's.

/// Whether the engine does a thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Support {
    /// It does not, and asking will be refused rather than answered wrongly.
    No = 0,
    /// It does.
    Yes = 1,
    /// It does, with a limit the note names.
    Partial = -1,
}

impl Support {
    /// Returns the word a binding prints and the conformance suite writes.
    pub fn name(self) -> &'static str {
        match self {
            Support::No => "no",
            Support::Yes => "yes",
            Support::Partial => "partial",
        }
    }
}

/// How a row proves itself.
///
/// The polarity is per row rather than global, because "supported" does not
/// always mean "the statement runs". A foreign key is enforced when the
/// violating insert is **refused**, so a probe that only asked whether a
/// statement succeeded would report an engine with no integrity checking at all
/// as having foreign keys.
#[derive(Clone, Copy, Debug)]
pub enum Probe {
    /// Supported means the statement runs.
    Runs {
        /// Statements that build the fixture.
        setup: &'static [&'static str],
        /// The statement under test.
        sql: &'static str,
    },
    /// Supported means the statement is refused.
    Refuses {
        /// Statements that build the fixture.
        setup: &'static [&'static str],
        /// The statement under test.
        sql: &'static str,
    },
    /// Supported means the query's first cell renders as this.
    ///
    /// For the capabilities whose failure is a *wrong answer* rather than a
    /// refusal - `ALTER TABLE ... ADD COLUMN ... DEFAULT` leaves existing rows
    /// NULL, which no error accompanies.
    Answers {
        /// Statements that build the fixture.
        setup: &'static [&'static str],
        /// The query under test.
        sql: &'static str,
        /// What its first cell must be, rendered.
        expect: &'static str,
    },
    /// Supported means the query answers this **after** the test has registered
    /// a function and a collation on the connection.
    ///
    /// A separate kind because what is being tested is the registration, not
    /// the statement: the statement only exists to prove that what was
    /// registered can be reached from SQL. A registry no statement can read is
    /// the exact failure this row used to describe, so a probe that only
    /// checked the registration returned `Ok` would have proved nothing.
    Registers {
        /// The query that calls what was registered.
        sql: &'static str,
        /// What its first cell must be, rendered.
        expect: &'static str,
    },
    /// There is nothing to run, because the driver exposes no entry point for
    /// it. The note has to say why.
    Nothing,
}

/// One capability.
#[derive(Clone, Copy, Debug)]
pub struct Capability {
    /// The name an application asks by. Stable; a row is never renamed.
    pub name: &'static str,
    /// Whether the engine does it.
    pub support: Support,
    /// What an application is being told, in a sentence.
    pub note: &'static str,
    /// How the claim is checked.
    pub probe: Probe,
}

/// Every capability, in the order an application reads them.
///
/// Measured against the engine at `ec0d84f`, and kept true by
/// `tests/capability.rs` rather than by anybody remembering to look.
///
/// **The `no` rows were added in task-1980 (task-1979, section 8.2, gap 6).**
/// The table had twenty four rows and not one of them said `no`, so an
/// application following `AGENTS.md`'s advice to ask before composing was told
/// only what worked and never what did not - which is the half that makes the
/// answer worth asking for. Each `no` row runs a statement the engine refuses
/// today, and `tests/capability.rs` fails when one of them starts working, so
/// closing a gap is a change to this table rather than a silent drift.
///
/// What is here is the gaps a *statement* reaches. The refusal census behind
/// this counted sixty two distinct messages, and most of them name a bound form
/// no SQL text can produce - "missing expression", "unknown column", "expected
/// a select core" - which are invariants of the binder rather than features an
/// application can ask for. A row for one of those would have no probe, and a
/// row with no probe is the unchecked claim this table exists to avoid.
pub static CAPABILITIES: &[Capability] = &[
    // —— what it does ——————————————————————————————————————————————
    Capability {
        name: "ddl",
        support: Support::Yes,
        note: "CREATE TABLE, CREATE INDEX, DROP and ALTER TABLE run against the file.",
        probe: Probe::Runs {
            setup: &[],
            sql: "CREATE TABLE cap_ddl (a INTEGER PRIMARY KEY, b TEXT)",
        },
    },
    Capability {
        name: "dml",
        support: Support::Yes,
        note: "INSERT, UPDATE and DELETE, with the changed-row count reported.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_dml (a INTEGER PRIMARY KEY, b TEXT)"],
            sql: "INSERT INTO cap_dml VALUES (1, 'one')",
        },
    },
    Capability {
        name: "parameters",
        support: Support::Yes,
        note: "Values bound as ?1, ?2, rather than pasted into the statement.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_param (a INTEGER PRIMARY KEY)"],
            sql: "SELECT a FROM cap_param WHERE a = ?1",
        },
    },
    Capability {
        name: "transactions",
        support: Support::Yes,
        note: "BEGIN, COMMIT and ROLLBACK, with the undo buffer that makes ROLLBACK real.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_txn (a INTEGER PRIMARY KEY)"],
            sql: "BEGIN",
        },
    },
    Capability {
        name: "savepoints",
        support: Support::Yes,
        note: "SAVEPOINT, RELEASE and ROLLBACK TO.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_save (a INTEGER PRIMARY KEY)", "BEGIN"],
            sql: "SAVEPOINT one",
        },
    },
    Capability {
        name: "returning",
        support: Support::Yes,
        note: "RETURNING on an INSERT, UPDATE or DELETE.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_ret (a INTEGER PRIMARY KEY, b TEXT)"],
            sql: "INSERT INTO cap_ret VALUES (1, 'one') RETURNING a",
        },
    },
    Capability {
        name: "inner_join",
        support: Support::Yes,
        note: "A join of two tables on an equality.",
        probe: Probe::Runs {
            setup: &[
                "CREATE TABLE cap_l (a INTEGER PRIMARY KEY)",
                "CREATE TABLE cap_r (a INTEGER PRIMARY KEY)",
            ],
            sql: "SELECT cap_l.a FROM cap_l JOIN cap_r ON cap_l.a = cap_r.a",
        },
    },
    Capability {
        name: "aggregates",
        support: Support::Yes,
        note: "COUNT, SUM, MIN, MAX, AVG and GROUP BY.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_agg (a INTEGER PRIMARY KEY, b TEXT)"],
            sql: "SELECT b, count(*) FROM cap_agg GROUP BY b",
        },
    },
    Capability {
        name: "explain_query_plan",
        support: Support::Yes,
        note: "EXPLAIN QUERY PLAN describes the operator chain. Plain EXPLAIN does not: \
               there is no bytecode to list.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_plan (a INTEGER PRIMARY KEY)"],
            sql: "EXPLAIN QUERY PLAN SELECT a FROM cap_plan",
        },
    },
    Capability {
        name: "integrity_check",
        support: Support::Yes,
        note: "PRAGMA integrity_check walks every tree.",
        probe: Probe::Runs {
            setup: &[],
            sql: "PRAGMA integrity_check",
        },
    },
    Capability {
        name: "schema_introspection",
        support: Support::Yes,
        note: "sqlite_schema is a readable table and PRAGMA table_info, index_list, \
               index_info and table_list are answered.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_schema (a INTEGER PRIMARY KEY)"],
            sql: "SELECT type, name FROM sqlite_schema",
        },
    },
    Capability {
        name: "outer_join",
        support: Support::Yes,
        note: "LEFT, RIGHT and FULL OUTER JOIN, with the ON condition evaluated per \
               candidate pair - which is what distinguishes no partner from a partner \
               that failed the condition. An earlier build answered them as inner joins \
               and six rows came back as four nulls; the outer join operator was built \
               afterward, and 21 join statements are graded against the pinned SQLite.",
        probe: Probe::Runs {
            setup: &[
                "CREATE TABLE cap_ol (a INTEGER PRIMARY KEY)",
                "CREATE TABLE cap_or (a INTEGER PRIMARY KEY)",
            ],
            sql: "SELECT cap_ol.a FROM cap_ol LEFT JOIN cap_or ON cap_ol.a = cap_or.a",
        },
    },
    Capability {
        name: "recursive_cte",
        support: Support::Yes,
        note: "WITH RECURSIVE, seeds then steps until a pass produces nothing. UNION \
               de-duplicates against everything produced so far and UNION ALL does not, \
               which is the difference between a graph walk that terminates on a cycle \
               and one that does not.",
        probe: Probe::Runs {
            setup: &[],
            sql: "WITH RECURSIVE counter(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM counter \
                  WHERE n < 3) SELECT n FROM counter",
        },
    },
    Capability {
        name: "derived_table",
        support: Support::Yes,
        note: "A subquery used as a term in FROM, as the outermost term or as an inner \
               one. Its rows are read once rather than once per outer row, because a \
               derived table has no free variable to re-evaluate.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_derived (a INTEGER PRIMARY KEY)"],
            sql: "SELECT x FROM (SELECT a AS x FROM cap_derived)",
        },
    },
    Capability {
        name: "foreign_keys",
        support: Support::Yes,
        note: "Enforced when PRAGMA foreign_keys is ON, which is off by default exactly \
               as in SQLite. A key is compiled to a trigger and the executor fires it, so \
               ON DELETE CASCADE and a hand-written DELETE cannot disagree; the 19 cases \
               in foreign_keys.rs are graded against the pinned SQLite.",
        probe: Probe::Refuses {
            setup: &[
                "CREATE TABLE cap_parent (a INTEGER PRIMARY KEY)",
                "CREATE TABLE cap_child (a INTEGER PRIMARY KEY, p INTEGER REFERENCES cap_parent(a))",
                "PRAGMA foreign_keys = ON",
            ],
            sql: "INSERT INTO cap_child VALUES (1, 999)",
        },
    },
    Capability {
        name: "triggers",
        support: Support::Yes,
        note: "CREATE TRIGGER is stored and fires: BEFORE and AFTER, on INSERT, UPDATE \
               and DELETE, with WHEN, with OLD and NEW, and with RAISE. It was refused \
               for a while rather than stored and never fired, which is the wrong answer \
               this row used to warn about.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE cap_trig (a INTEGER PRIMARY KEY)"],
            sql: "CREATE TRIGGER cap_t AFTER INSERT ON cap_trig BEGIN \
                  UPDATE cap_trig SET a = a; END",
        },
    },
    Capability {
        name: "attach",
        support: Support::Yes,
        note: "ATTACH and DETACH work, with SQLite's name resolution. A connection is a set of \
               schemas, each with its own file, buffer pool and log; a transaction that writes \
               two of them is committed through a super-journal, so it commits both or neither.",
        probe: Probe::Runs {
            setup: &[],
            sql: "ATTACH DATABASE ':memory:' AS other",
        },
    },
    Capability {
        name: "temp_tables",
        support: Support::Yes,
        note: "Temporary tables, indexes, views and triggers, in a database that belongs to one \
               connection and reaches no file.",
        probe: Probe::Runs {
            setup: &[],
            sql: "CREATE TEMP TABLE cap_temp (a INTEGER)",
        },
    },
    Capability {
        name: "strict_tables",
        support: Support::Yes,
        note: "STRICT is enforced on write, after the column's affinity has been applied: \
               an integer written to a TEXT column becomes text and is \
               accepted, and a value still of the wrong class is refused with \
               SQLITE_CONSTRAINT_DATATYPE.",
        probe: Probe::Refuses {
            setup: &["CREATE TABLE cap_strict (a INTEGER) STRICT"],
            sql: "INSERT INTO cap_strict VALUES ('not a number')",
        },
    },
    // —— what it does not ————————————————————————————————————————
    Capability {
        name: "add_column_default",
        support: Support::Yes,
        note: "ALTER TABLE ... ADD COLUMN ... DEFAULT fills the rows that were already \
               there, which is SQLite's rule. It used to leave them NULL - a wrong answer \
               rather than a refusal, which is why this one is probed by its value rather \
               than by whether the statement runs.",
        probe: Probe::Answers {
            setup: &[
                "CREATE TABLE cap_add (a INTEGER PRIMARY KEY)",
                "INSERT INTO cap_add VALUES (1)",
                "ALTER TABLE cap_add ADD COLUMN b TEXT DEFAULT 'filled'",
            ],
            sql: "SELECT b FROM cap_add WHERE a = 1",
            expect: "filled",
        },
    },
    Capability {
        name: "large_values",
        support: Support::Yes,
        note: "A value larger than a page is stored outside it, in a contiguous run,                whatever its column is declared - a text in a column declared BLOB, or in                one declared nothing at all, as much as a text in a column declared TEXT.                The reference says what the bytes read back as when the column would say                something else, which is what makes that possible; until it did, an                undeclared column refused about 32 KB. Probed by `typeof` rather than by                whether the insert runs, because the way this fails is a text coming back                as a blob.",
        probe: Probe::Answers {
            setup: &[
                "CREATE TABLE cap_large (a)",
                "INSERT INTO cap_large VALUES (replace(hex(zeroblob(40000)), '0', 'p'))",
            ],
            sql: "SELECT typeof(a) || ':' || length(a) FROM cap_large",
            expect: "text:80000",
        },
    },
    Capability {
        name: "user_functions",
        support: Support::Yes,
        note: "A scalar function an application wrote, registered on the connection and \
               called from SQL by name. It is registered with the engine's external \
               flags, so a statement may call it and a schema may not - not a DEFAULT, a \
               CHECK, a generated column or a view - which is the safe assumption about \
               code the engine did not write.",
        probe: Probe::Registers {
            sql: "SELECT driver_probe(2)",
            expect: "4",
        },
    },
    Capability {
        name: "user_collations",
        support: Support::Yes,
        note: "A collating sequence an application wrote, named by COLLATE. It decides \
               the order rows are STORED in and not merely the order they come back in, \
               so an index on a column declared with one is built with it - which is why \
               a comparator that answers differently on two runs is a fault the engine \
               cannot detect.",
        probe: Probe::Registers {
            sql: "SELECT 'B' = 'b' COLLATE driver_probe_ci",
            expect: "1",
        },
    },
    Capability {
        name: "cancel",
        support: Support::Partial,
        note: "A running statement can be stopped, and the limit is *when*. `cancel` sets \
               a flag the executor reads at every leaf of a scan and every batch a result \
               collects, so a long scan, a large result and a slow join all stop with \
               `interrupted` and leave the connection usable. What it does not interrupt is \
               a single operator part-way through one indivisible piece of work: a sort of \
               what it has already read finishes. Draw a Stop button; do not promise it is \
               instant.",
        probe: Probe::Nothing,
    },
    Capability {
        name: "readonly_open",
        support: Support::Partial,
        note: "Read-only is enforced by this driver above the engine, not by the file \
               handle: a statement that does not bind to a SELECT is refused. That is the \
               binder's classification rather than a scan of the text, but the file is \
               still open for writing and another handle could write it.",
        probe: Probe::Nothing,
    },
    Capability {
        name: "distinct_in_a_scalar_function",
        support: Support::Yes,
        note: "DISTINCT inside a function that is not an aggregate is ignored, as SQLite ignores it: `abs(DISTINCT a)` answers the same as `abs(a)`.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER)"],
            sql: "SELECT abs(DISTINCT a) FROM t",
        },
    },
    // —— what it does not do ——————————————————————————————————————
    Capability {
        name: "attach_with_key",
        support: Support::No,
        note: "ATTACH takes a path and a name: the KEY clause, which SQLite's own build answers only with the encryption extension, is refused.",
        probe: Probe::Runs {
            setup: &[],
            sql: "ATTACH DATABASE 'other.rdb' AS o KEY 'k'",
        },
    },
    Capability {
        name: "row_value_in_subquery",
        support: Support::No,
        note: "A row value on the left of IN takes a value list and not a query: `(a, b) IN (SELECT x, y FROM s)` is refused.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER, b INTEGER)", "CREATE TABLE s (x INTEGER, y INTEGER)"],
            sql: "SELECT 1 FROM t WHERE (a, b) IN (SELECT x, y FROM s)",
        },
    },
    Capability {
        name: "computed_limit",
        support: Support::No,
        note: "LIMIT and OFFSET take a constant or a parameter: an expression such as `LIMIT 1 + 1`, and a value that is not an integer, are refused.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER)"],
            sql: "SELECT a FROM t LIMIT 1 + 1",
        },
    },
    Capability {
        name: "update_delete_limit",
        support: Support::Yes,
        // **Probed by its value.** `ORDER BY` on a write used to be parsed and
        // then dropped by the binder, so a probe that only asked whether the
        // statement ran would pass an engine that deleted an arbitrary row.
        // Without the order this deletes the row with the lowest rowid, 1.
        note: "DELETE and UPDATE take ORDER BY, LIMIT and OFFSET, as SQLite does when it is compiled with SQLITE_ENABLE_UPDATE_DELETE_LIMIT, so a batch loop of `DELETE ... LIMIT 1000` runs. An ORDER BY with no LIMIT is refused, as it is there.",
        probe: Probe::Answers {
            setup: &[
                "CREATE TABLE cap_limited (a INTEGER)",
                "INSERT INTO cap_limited VALUES (1), (2), (3), (4), (5)",
            ],
            sql: "DELETE FROM cap_limited RETURNING a ORDER BY a DESC LIMIT 1",
            expect: "5",
        },
    },
    Capability {
        name: "subquery_value_in_a_virtual_table",
        support: Support::Yes,
        note: "A scalar subquery is a value an INSERT or UPDATE of a virtual table, such as an FTS5 table, can write: `INSERT INTO docs(title) VALUES ((SELECT title FROM shelf LIMIT 1))`.",
        probe: Probe::Runs {
            setup: &[
                "CREATE TABLE cap_shelf (title TEXT)",
                "INSERT INTO cap_shelf VALUES ('dune')",
                "CREATE VIRTUAL TABLE cap_docs USING fts5(title)",
            ],
            sql: "INSERT INTO cap_docs(title) VALUES ((SELECT title FROM cap_shelf LIMIT 1))",
        },
    },
    Capability {
        name: "subquery_in_a_trigger_body",
        support: Support::Yes,
        note: "A statement in a trigger body may use a subquery as a value, in a WHEN guard and in its WHERE, whatever the statement that fired the trigger held.",
        probe: Probe::Runs {
            setup: &[
                "CREATE TABLE cap_running (id INTEGER PRIMARY KEY, v INTEGER, total INTEGER)",
                "CREATE TRIGGER cap_running_total AFTER INSERT ON cap_running BEGIN UPDATE cap_running SET total = (SELECT sum(v) FROM cap_running) WHERE id = new.id; END",
            ],
            sql: "INSERT INTO cap_running (id, v) VALUES (1, (SELECT 10))",
        },
    },
    Capability {
        name: "load_extension",
        support: Support::No,
        note: "There is no C extension interface to load a shared library into, so load_extension() refuses. Full text search (FTS5) and vector search (HNSW) are built in rather than loaded, which covers the usual reason to load one.",
        probe: Probe::Runs {
            setup: &[],
            sql: "SELECT load_extension('cap_missing_extension')",
        },
    },
    Capability {
        name: "window_in_derived_table",
        support: Support::Yes,
        note: "A window function inside a derived table in FROM, a common table expression or a view runs, so a rank computed in an inner query can be filtered in an outer one.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER)"],
            sql: "SELECT * FROM (SELECT row_number() OVER () AS n FROM t)",
        },
    },
    Capability {
        name: "window_in_compound_arm",
        support: Support::Yes,
        note: "A window function inside an arm of a UNION, EXCEPT or INTERSECT runs. It                used to be refused, because the binder threw away the arm's window calls                when it left the arm's block and the physical pass was then handed a                WindowRef in a statement that claimed to have no windows (task-2042).",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER)"],
            sql: "SELECT a FROM t UNION ALL SELECT row_number() OVER () FROM t",
        },
    },
    Capability {
        name: "aggregate_in_compound_arm",
        support: Support::Yes,
        // **Probed by its value, not by whether the statement runs.** The
        // refusal this row records had a second shape that answered: an arm
        // with a GROUP BY planned a grouped aggregate with no accumulators in
        // it and projected one column past the end of the group key, so
        // `SELECT 1 UNION ALL SELECT count(*) FROM t GROUP BY a` came back
        // with a blank where the count belongs. A `Probe::Runs` row would
        // have called that supported.
        note: "An aggregate inside an arm of a UNION, EXCEPT or INTERSECT that is not the                first runs and returns its value. It used to be refused - and, with a GROUP                BY on that arm, to answer one blank row per group instead (task-2042).",
        probe: Probe::Answers {
            setup: &[
                "CREATE TABLE cap_compound_agg (a INTEGER)",
                "INSERT INTO cap_compound_agg VALUES (1), (2), (3)",
            ],
            // The sum of the whole compound rather than its first cell,
            // because the first cell is the head arm's 9 and the head arm was
            // never the broken one. 9 + 1 + 1 + 1; the grouped arm answering
            // blanks gives 9, and `sum` ignores a NULL rather than reporting
            // it, which is what makes the two numbers different.
            sql: "SELECT sum(v) FROM                   (SELECT 9 AS v UNION ALL SELECT count(*) FROM cap_compound_agg GROUP BY a)",
            expect: "12",
        },
    },
    Capability {
        name: "compound_ordered_by_expression",
        support: Support::No,
        note: "A compound query is ordered by a result column or its position, not by an expression over one.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER)"],
            sql: "SELECT a FROM t UNION SELECT a FROM t ORDER BY a + 1",
        },
    },
    Capability {
        name: "multi_column_vector_index",
        support: Support::No,
        note: "A vector index is over one column: `CREATE INDEX ... USING inillucent_hnsw (a, b)` is refused.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a VECTOR(4), b VECTOR(4))"],
            sql: "CREATE INDEX t_v ON t USING inillucent_hnsw (a, b)",
        },
    },
    Capability {
        name: "writing_to_a_view",
        support: Support::No,
        note: "A view is read only: an INSERT, UPDATE or DELETE against one is refused, and an INSTEAD OF trigger is the way to write through it.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER)", "CREATE VIEW v AS SELECT a FROM t"],
            sql: "INSERT INTO v(a) VALUES (1)",
        },
    },
    Capability {
        name: "nested_explain",
        support: Support::No,
        note: "EXPLAIN takes a statement, not another EXPLAIN.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER)"],
            sql: "EXPLAIN EXPLAIN SELECT a FROM t",
        },
    },
    Capability {
        name: "insert_select_into_virtual_table",
        support: Support::Yes,
        note: "`INSERT INTO d(body) SELECT body FROM t` fills an FTS5 or inillucent_search table from a query. The query is read in full before the first row is written, so a query that reads the table being filled sees it as it was when the statement started.",
        probe: Probe::Runs {
            setup: &["CREATE VIRTUAL TABLE d USING fts5(body)", "CREATE TABLE t (body TEXT)"],
            sql: "INSERT INTO d(body) SELECT body FROM t",
        },
    },
    Capability {
        name: "on_conflict_partial_index",
        support: Support::No,
        note: "An ON CONFLICT target names a column list of an ordinary unique index; a partial index's WHERE clause in the target is refused.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER, b INTEGER)", "CREATE UNIQUE INDEX t_a ON t(a) WHERE b > 0"],
            sql: "INSERT INTO t(a,b) VALUES (1,1) ON CONFLICT(a) WHERE b > 0 DO NOTHING",
        },
    },
    Capability {
        name: "on_conflict_expression_index",
        support: Support::No,
        note: "An ON CONFLICT target names columns; an expression such as `ON CONFLICT(lower(a))` is refused.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a TEXT)", "CREATE UNIQUE INDEX t_l ON t(lower(a))"],
            sql: "INSERT INTO t(a) VALUES ('x') ON CONFLICT(lower(a)) DO NOTHING",
        },
    },
    Capability {
        name: "correlated_in_over_a_grouped_block",
        support: Support::No,
        note: "A correlated IN subquery runs, and one whose block groups, limits, or is itself a compound query is refused: the lowering pushes the equality into the block's WHERE, which is applied before either.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER, b INTEGER)", "CREATE TABLE s (x INTEGER, y INTEGER)"],
            sql: "SELECT a FROM t WHERE a IN (SELECT x FROM s WHERE s.y = t.b GROUP BY x)",
        },
    },
    Capability {
        name: "returning_inside_a_trigger",
        support: Support::No,
        note: "RETURNING runs on a statement and is refused inside a trigger body.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER)", "CREATE TABLE u (a INTEGER)"],
            sql: "CREATE TRIGGER g AFTER INSERT ON t BEGIN INSERT INTO u(a) VALUES (1) RETURNING a; END",
        },
    },
    Capability {
        name: "fts5_unavailable_tokenizer",
        support: Support::No,
        note: "The FTS5 tokenizers are ascii, unicode61 and porter; a name this build has not got, such as trigram or icu, is refused rather than silently replaced.",
        probe: Probe::Runs {
            setup: &[],
            sql: "CREATE VIRTUAL TABLE d USING fts5(body, tokenize='trigram')",
        },
    },
    Capability {
        name: "fts5_detail_option",
        support: Support::No,
        note: "The FTS5 index stores full positions: `detail='none'` and `detail='column'` are refused rather than accepted and ignored.",
        probe: Probe::Runs {
            setup: &[],
            sql: "CREATE VIRTUAL TABLE d USING fts5(body, detail='none')",
        },
    },
    Capability {
        name: "fts5_columnsize_option",
        support: Support::No,
        note: "The FTS5 index stores one size per column: `columnsize=0` is refused rather than accepted and ignored.",
        probe: Probe::Runs {
            setup: &[],
            sql: "CREATE VIRTUAL TABLE d USING fts5(body, columnsize=0)",
        },
    },
    Capability {
        name: "changing_a_schema_row",
        support: Support::No,
        note: "An INSERT into sqlite_schema under PRAGMA writable_schema records a virtual table, which is what a dump replays; an UPDATE or a DELETE of a schema row is refused.",
        probe: Probe::Runs {
            setup: &["CREATE TABLE t (a INTEGER)"],
            sql: "UPDATE sqlite_schema SET sql = 'x' WHERE name = 't'",
        },
    },
];

/// Looks a capability up by name.
///
/// @param name - the capability's stable name
pub fn capability(name: &str) -> Option<&'static Capability> {
    CAPABILITIES.iter().find(|entry| entry.name == name)
}

/// Reports whether the engine does something, by name.
///
/// `None` when the name is not one this version knows, which a caller should
/// treat as "no" and not as "yes": a capability that has never been declared
/// has certainly never been checked.
///
/// @param name - the capability's stable name
pub fn supports(name: &str) -> Option<Support> {
    capability(name).map(|entry| entry.support)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name is how an application asks, so two rows sharing one would make an
    /// answer ambiguous.
    #[test]
    fn every_capability_name_is_distinct() {
        let mut names: Vec<&str> = CAPABILITIES.iter().map(|entry| entry.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two capabilities share a name");
    }

    /// A row with nothing to run has to say why, or it is an unchecked claim
    /// wearing the same clothes as a checked one.
    #[test]
    fn a_row_with_no_probe_explains_itself() {
        for entry in CAPABILITIES {
            if matches!(entry.probe, Probe::Nothing) {
                assert!(
                    entry.note.len() > 60,
                    "{} has no probe and no explanation of why not",
                    entry.name
                );
            }
        }
    }

    /// Every row says something. A note is what an application shows a person
    /// in place of the control it is not drawing.
    #[test]
    fn every_capability_has_a_note() {
        for entry in CAPABILITIES {
            assert!(!entry.note.is_empty(), "{} says nothing", entry.name);
            assert!(
                entry.note.ends_with('.'),
                "{}'s note is not a sentence",
                entry.name
            );
        }
    }

    /// An unknown name answers `None` rather than `Some(No)`, because they mean
    /// different things: one is a checked absence and the other is ignorance.
    #[test]
    fn an_unknown_capability_is_not_reported_as_absent() {
        assert_eq!(supports("outer_join"), Some(Support::Yes));
        assert_eq!(supports("ddl"), Some(Support::Yes));
        assert_eq!(supports("time_travel"), None);
    }
}
