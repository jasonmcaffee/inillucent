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
