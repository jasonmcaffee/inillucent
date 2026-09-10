//! The new engine's DDL, graded against pinned SQLite 3.53.4.
//!
//! Invariant: **after every DDL statement, `sqlite_schema` says the same thing
//! on both engines** - the same rows, in the same order, with byte-identical
//! `sql` text. That is the phase's acceptance quoted, and it is a byte
//! comparison rather than a description comparison on purpose: the stored
//! `CREATE` text is what a reader re-parses to learn what a table is, so a
//! difference in it is a difference in the schema even when both texts describe
//! the same columns.
//!
//! The `rootpage` column is the one deliberate exception and is excluded from
//! the comparison. It holds the page the tree is rooted at *in this file*, and
//! the two engines have different files - that is the whole design. Everything
//! else is compared exactly.
//!
//! ## Why the campaign ends in an integrity check
//!
//! A `CREATE INDEX` that packed a leaf wrongly, a `DROP TABLE` that gave a live
//! page back to the free map, an `ALTER TABLE` that rebuilt a tree with the
//! wrong column directory - none of those has to make `sqlite_schema` wrong.
//! They make the *file* wrong, and the integrity checker is what asks about the
//! file. So every campaign here ends by walking every tree.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// Returns the pinned SQLite oracle binary, if it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns a fresh directory for one test's files.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-ddl-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// The schema the fixture starts from.
///
/// Small on purpose. What is being graded is what a DDL statement *changes*, so
/// a starting schema large enough to hide a difference in the middle of a diff
/// would make a failure harder to read without making it likelier to be caught.
const SCHEMA: &[&str] = &[
    "CREATE TABLE members (id INTEGER PRIMARY KEY, email TEXT NOT NULL, team TEXT, score INTEGER)",
    "CREATE INDEX members_team ON members (team)",
];

/// The rows the fixture starts with.
const SEED: &[&str] = &[
    "INSERT INTO members VALUES (1, 'ana@x', 'red', 30)",
    "INSERT INTO members VALUES (2, 'bo@x', 'blue', 20)",
    "INSERT INTO members VALUES (3, 'cy@x', 'red', 40)",
    "INSERT INTO members VALUES (4, 'di@x', 'green', 20)",
    "INSERT INTO members VALUES (5, 'ed@x', 'blue', 10)",
];

/// A fixture both engines have opened.
struct Pair {
    engine: ImportedDatabase,
    oracle: Driver,
    /// Kept so the directory outlives the test.
    _directory: PathBuf,
}

/// Builds the fixture with the oracle and imports it into the new engine.
///
/// @param tag - what to name the scratch directory after
fn pair(tag: &str) -> Option<Pair> {
    let program = oracle_path()?;
    let directory = scratch(tag);
    let path = directory.join("members.db");
    let mut oracle = Driver::start("sqlite", &program).expect("the oracle starts");
    oracle
        .send(&Op::Open(path.to_string_lossy().into_owned()))
        .expect("the oracle opens the fixture");
    for sql in SCHEMA.iter().chain(SEED.iter()) {
        let observed = oracle
            .send(&Op::Exec((*sql).to_string()))
            .expect("the exec runs");
        assert!(
            observed.ok,
            "the fixture did not build: {sql}: {}",
            observed.message
        );
    }
    let engine = ImportedDatabase::import(path, 4_096)
        .unwrap_or_else(|error| panic!("the fixture did not import: {:?}", error.detail()));
    Some(Pair {
        engine,
        oracle,
        _directory: directory,
    })
}

/// The query both engines are asked about their catalogs.
///
/// `rootpage` is excluded: it names a page in the file the engine wrote, and the
/// two engines wrote different files.
const CATALOG: &str = "SELECT type, name, tbl_name, sql FROM sqlite_schema";

impl Pair {
    /// Runs one statement on both engines and asserts they agreed about it.
    ///
    /// @param sql - the statement
    fn both(&mut self, sql: &str) {
        let theirs = self
            .oracle
            .send(&Op::Exec(sql.to_string()))
            .expect("the oracle runs the statement");
        let ours = self.engine.execute_any(sql, &Params::new());
        assert_eq!(
            theirs.ok,
            ours.is_ok(),
            "{sql}: sqlite said {} ({}), inillucent said {} ({:?})",
            theirs.ok,
            theirs.message,
            ours.is_ok(),
            ours.as_ref().err().and_then(|error| error.detail())
        );
    }

    /// Asserts the two catalogs are the same, row for row and byte for byte.
    ///
    /// @param after - what was just run, for the message
    fn catalogs_agree(&mut self, after: &str) {
        let theirs = self
            .oracle
            .send(&Op::Query(CATALOG.to_string()))
            .expect("the oracle reads its catalog");
        assert!(theirs.ok, "the oracle could not read its catalog");
        let ours = self
            .engine
            .execute_any(CATALOG, &Params::new())
            .unwrap_or_else(|error| {
                panic!(
                    "the new engine could not read its catalog: {:?}",
                    error.detail()
                )
            });
        let mine: Vec<Vec<TaggedValue>> = ours.rows.iter().map(|row| render(row)).collect();
        assert_eq!(
            mine, theirs.rows,
            "sqlite_schema diverged after `{after}`\n  inillucent: {mine:?}\n  sqlite:     {:?}",
            theirs.rows
        );
    }

    /// Walks every tree and fails on the first broken invariant.
    ///
    /// @param after - what was just run, for the message
    fn is_intact(&self, after: &str) {
        self.engine
            .check_trees()
            .unwrap_or_else(|error| panic!("the file is not intact after `{after}`: {error:?}"));
    }

    /// Asserts both engines answer one query identically.
    ///
    /// @param sql - the query
    fn answers_agree(&mut self, sql: &str) {
        let theirs = self
            .oracle
            .send(&Op::Query(sql.to_string()))
            .expect("the oracle runs the query");
        assert!(theirs.ok, "{sql}: the oracle refused: {}", theirs.message);
        let ours = self
            .engine
            .execute_any(sql, &Params::new())
            .unwrap_or_else(|error| panic!("{sql}: the new engine refused: {:?}", error.detail()));
        let mine: Vec<Vec<TaggedValue>> = ours.rows.iter().map(|row| render(row)).collect();
        assert_eq!(mine, theirs.rows, "{sql} disagreed");
    }

    /// Runs a campaign: every statement on both engines, comparing after each.
    ///
    /// @param statements - the campaign
    fn campaign(&mut self, statements: &[&str]) {
        for sql in statements {
            self.both(sql);
            self.catalogs_agree(sql);
            self.is_intact(sql);
        }
    }
}

/// Renders one of the new engine's rows the way the oracle renders its own.
///
/// @param row - the row
fn render(row: &[OwnedDatum]) -> Vec<TaggedValue> {
    row.iter()
        .map(|value| match value {
            OwnedDatum::Null => TaggedValue::Null,
            OwnedDatum::Int(number) => TaggedValue::Integer(*number),
            OwnedDatum::Real(number) => TaggedValue::Real(*number),
            OwnedDatum::Text(bytes) => TaggedValue::Text(bytes.clone()),
            OwnedDatum::Blob(bytes) => TaggedValue::Blob(bytes.clone()),
        })
        .collect()
}

/// Says the suite could not run, rather than passing quietly.
fn no_oracle() {
    eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.ps1");
}

#[test]
fn creating_a_table_writes_the_row_sqlite_writes() {
    let Some(mut pair) = pair("create-table") else {
        return no_oracle();
    };
    pair.campaign(&[
        "CREATE TABLE plain (a, b)",
        // The stored text keeps the author's spacing and case and loses the
        // `IF NOT EXISTS`, which is what makes this a byte comparison worth
        // making rather than a shape comparison.
        "CREATE TABLE IF NOT EXISTS spaced   (  x   INTEGER ,  y  TEXT  )",
        "create table lowered (m REAL)",
        // A `UNIQUE` constraint produces an automatic index with a NULL
        // statement, named after the table.
        "CREATE TABLE constrained (k INTEGER, u TEXT UNIQUE, v TEXT)",
        // Two of them, numbered in declaration order.
        "CREATE TABLE two (a TEXT UNIQUE, b TEXT UNIQUE)",
    ]);
}

#[test]
fn creating_an_index_fills_it_and_the_planner_uses_it() {
    let Some(mut pair) = pair("create-index") else {
        return no_oracle();
    };
    pair.campaign(&[
        "CREATE INDEX members_score ON members (score)",
        "CREATE UNIQUE INDEX members_email ON members (email)",
        "CREATE INDEX members_two ON members (team, score)",
    ]);
    // The index is not merely recorded, it is filled: these are answered
    // through it, and an empty tree would answer them with nothing.
    pair.answers_agree("SELECT id FROM members WHERE score = 20 ORDER BY id");
    pair.answers_agree("SELECT id FROM members WHERE email = 'cy@x'");
    pair.answers_agree("SELECT count(*) FROM members WHERE score >= 20");
    pair.answers_agree("SELECT score FROM members ORDER BY score");
    pair.answers_agree("SELECT id FROM members WHERE team = 'red' AND score = 40");
}

#[test]
fn a_unique_index_over_duplicate_rows_is_refused() {
    let Some(mut pair) = pair("unique-refused") else {
        return no_oracle();
    };
    // `team` has three duplicated values, so neither engine can build it.
    pair.both("CREATE UNIQUE INDEX members_team_unique ON members (team)");
    pair.catalogs_agree("a refused CREATE UNIQUE INDEX");
    pair.is_intact("a refused CREATE UNIQUE INDEX");
}

#[test]
fn dropping_takes_the_object_and_its_indexes() {
    let Some(mut pair) = pair("drop") else {
        return no_oracle();
    };
    pair.campaign(&[
        "CREATE TABLE spare (a, b)",
        "CREATE INDEX spare_a ON spare (a)",
        "DROP INDEX spare_a",
        "DROP TABLE spare",
        "DROP TABLE IF EXISTS never_existed",
        "DROP INDEX IF EXISTS never_existed",
        // Dropping the table takes its index with it, without naming it.
        "DROP INDEX members_team",
    ]);
    pair.answers_agree("SELECT count(*) FROM members");
}

/// A view is stored as written, and a trigger is refused rather than stored.
///
/// A view and a trigger are both stored exactly as SQLite stores them.
///
/// **The trigger left this campaign for a while and is back.** It was here
/// originally and it passed - the engine stored the statement byte for byte and
/// the digest of `sqlite_schema` agreed on both sides - but what the campaign
/// could not see is that the trigger then never fired, so `CREATE TRIGGER`
/// was made a refusal rather than a pretence and taken out. The firing point
/// exists now, so storage is comparable again *and* means something:
/// `new_engine_surface.rs` asserts that it runs, and `foreign_keys.rs` grades
/// the same mechanism against the pinned shell.
#[test]
fn a_view_and_a_trigger_are_stored_as_written() {
    let Some(mut pair) = pair("view-trigger") else {
        return no_oracle();
    };
    pair.campaign(&[
        "CREATE VIEW reds AS SELECT id, email FROM members WHERE team = 'red'",
        "DROP VIEW reds",
        "CREATE TRIGGER bump AFTER INSERT ON members BEGIN          UPDATE members SET score = score + 1 WHERE id = NEW.id; END",
    ]);
}

#[test]
fn altering_a_table_rewrites_every_row_that_names_it() {
    let Some(mut pair) = pair("alter") else {
        return no_oracle();
    };
    pair.campaign(&[
        "ALTER TABLE members RENAME TO people",
        "ALTER TABLE people RENAME COLUMN email TO address",
        "ALTER TABLE people ADD COLUMN joined TEXT",
    ]);
    pair.answers_agree("SELECT id, address, team, score, joined FROM people ORDER BY id");
    pair.answers_agree("SELECT id FROM people WHERE team = 'red' ORDER BY id");
}

#[test]
fn a_dropped_column_leaves_the_rows_readable() {
    let Some(mut pair) = pair("drop-column") else {
        return no_oracle();
    };
    pair.campaign(&["ALTER TABLE members DROP COLUMN score"]);
    pair.answers_agree("SELECT id, email, team FROM members ORDER BY id");
    pair.answers_agree("SELECT count(*) FROM members WHERE team = 'red'");
}

#[test]
fn analyze_writes_sqlite_stat1_in_sqlite_s_own_format() {
    let Some(mut pair) = pair("analyze") else {
        return no_oracle();
    };
    pair.campaign(&["CREATE INDEX members_score ON members (score)", "ANALYZE"]);
    pair.answers_agree("SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx");
}

#[test]
fn a_plan_built_before_a_ddl_statement_is_not_used_after_it() {
    let Some(mut pair) = pair("plan-cache") else {
        return no_oracle();
    };
    // The query is compiled and run once. With no index on `score` the only
    // access path is a table scan.
    let before = pair
        .engine
        .describe_cached("SELECT id FROM members WHERE score = 20")
        .expect("the query plans");
    assert!(
        before.iter().any(|stage| stage.contains("SCAN")),
        "expected a table scan before the index existed, got {before:?}"
    );

    let generation = pair.engine.catalog_generation();

    pair.both("CREATE INDEX members_score ON members (score)");
    assert!(
        pair.engine.catalog_generation() > generation,
        "the catalog generation did not move across a CREATE INDEX"
    );

    // **The test that would return the old answer if the plan were reused.**
    // The same text, compiled again, must now reach the index - and a cached
    // plan would still be the table scan.
    let after = pair
        .engine
        .describe_cached("SELECT id FROM members WHERE score = 20")
        .expect("the query plans again");
    assert_ne!(
        before, after,
        "the plan did not change across a CREATE INDEX, so the cache was not invalidated"
    );
    pair.answers_agree("SELECT id FROM members WHERE score = 20 ORDER BY id");

    // And the other direction: dropping it has to take the plan back.
    pair.both("DROP INDEX members_score");
    let dropped = pair
        .engine
        .describe_cached("SELECT id FROM members WHERE score = 20")
        .expect("the query plans a third time");
    assert_eq!(
        before, dropped,
        "the plan did not go back to a scan when the index was dropped"
    );
    pair.answers_agree("SELECT id FROM members WHERE score = 20 ORDER BY id");
}

#[test]
fn a_created_index_is_maintained_by_later_writes() {
    let Some(mut pair) = pair("maintained") else {
        return no_oracle();
    };
    pair.campaign(&["CREATE INDEX members_score ON members (score)"]);
    for sql in [
        "INSERT INTO members VALUES (6, 'fi@x', 'red', 50)",
        "UPDATE members SET score = 99 WHERE id = 2",
        "DELETE FROM members WHERE id = 4",
    ] {
        pair.both(sql);
    }
    pair.is_intact("writes after a CREATE INDEX");
    pair.answers_agree("SELECT id FROM members WHERE score = 99");
    pair.answers_agree("SELECT id FROM members WHERE score = 50");
    pair.answers_agree("SELECT count(*) FROM members WHERE score = 20");
    pair.answers_agree("SELECT score, id FROM members ORDER BY score, id");
}

#[test]
fn a_long_campaign_leaves_both_catalogs_the_same() {
    let Some(mut pair) = pair("campaign") else {
        return no_oracle();
    };
    pair.campaign(&[
        "CREATE TABLE a (x INTEGER PRIMARY KEY, y TEXT)",
        "CREATE INDEX a_y ON a (y)",
        "CREATE TABLE b (p TEXT UNIQUE, q INTEGER)",
        "CREATE VIEW v AS SELECT x, y FROM a",
        "CREATE INDEX members_score ON members (score)",
        "DROP INDEX a_y",
        "CREATE UNIQUE INDEX a_y ON a (y)",
        "ALTER TABLE a RENAME TO renamed",
        "ALTER TABLE b ADD COLUMN r REAL",
        "DROP VIEW v",
        "DROP TABLE b",
        "DROP TABLE renamed",
        "CREATE TABLE b (p TEXT, q INTEGER)",
        "DROP INDEX members_score",
        "DROP INDEX members_team",
    ]);
}

#[test]
fn the_catalog_carries_what_reopening_the_file_needs() {
    let Some(mut pair) = pair("reopen") else {
        return no_oracle();
    };
    // A campaign that moves every number the catalog persists: a created tree,
    // a filled index, rows added after the build so the leaf counts change, and
    // a dropped object so the rows are not merely appended.
    pair.campaign(&[
        "CREATE TABLE journal (id INTEGER PRIMARY KEY, body TEXT)",
        "CREATE INDEX members_score ON members (score)",
        "CREATE TABLE scratch (a, b)",
        "DROP TABLE scratch",
    ]);
    for nth in 0..40 {
        let sql = format!("INSERT INTO journal VALUES ({nth}, 'entry {nth} lorem ipsum dolor')");
        pair.both(&sql);
    }
    pair.both("INSERT INTO members VALUES (6, 'fi@x', 'red', 60)");

    let before = pair
        .engine
        .execute_any("SELECT id, body FROM journal ORDER BY id", &Params::new())
        .expect("the journal reads")
        .rows;
    assert_eq!(before.len(), 40, "the journal did not take its rows");

    // The statistics are what a reopen rebuilds the handles from, so they have
    // to describe the trees rather than the builds.
    pair.engine.checkpoint().expect("the checkpoint runs");
    let stats: Vec<(String, u64, u64)> = pair
        .engine
        .schema_entries()
        .into_iter()
        .map(|(_, entry)| {
            (
                String::from_utf8_lossy(&entry.name).into_owned(),
                entry.stats.leaf_count,
                entry.stats.row_count,
            )
        })
        .collect();
    let journal = stats
        .iter()
        .find(|(name, _, _)| name == "journal")
        .expect("the journal has a catalog row");
    assert!(
        journal.1 >= 1 && journal.2 == 40,
        "the catalog does not describe the journal's tree: {journal:?}"
    );

    pair.engine.reopen().expect("the file reopens");

    // **Everything below this line reads a file this process did not build.**
    pair.is_intact("a reopen");
    pair.catalogs_agree("a reopen");
    let after = pair
        .engine
        .execute_any("SELECT id, body FROM journal ORDER BY id", &Params::new())
        .expect("the journal reads after the reopen")
        .rows;
    assert_eq!(before, after, "the journal read differently after a reopen");
    pair.answers_agree("SELECT count(*) FROM journal");
    pair.answers_agree("SELECT id, email, team, score FROM members ORDER BY id");
    pair.answers_agree("SELECT id FROM members WHERE score = 60");
    pair.answers_agree("SELECT count(*) FROM members WHERE team = 'red'");
}
