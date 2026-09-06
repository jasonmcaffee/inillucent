//! FTS5 and the R-Tree over the new engine's trees, graded against pinned
//! SQLite 3.53.4.
//!
//! Invariant: **the module is the same module.** Its tokenizers, its ranking,
//! its segment merges and the R-Tree's node logic are untouched; what changed is
//! where its shadow tables live. So the question these tests ask is not "does
//! FTS5 work" - the old engine's suites answer that - but "does it give the same
//! answers when its rows are in PAX trees instead of SQLite b-trees".
//!
//! ## What is graded
//!
//! Every answer against SQLite's, plus two things a query cannot see:
//!
//! - the **shadow tables are ordinary tables**, so `sqlite_schema` lists them by
//!   the names SQLite gives them and the integrity checker walks them;
//! - the constraints the module did **not** promise to apply are re-tested by
//!   the engine. `documents MATCH 'lorem'` is the module's to apply and it does;
//!   the R-Tree's `minX > ?` it takes only when it can use it to prune, and the
//!   rest is the engine's. A scan that trusted the module for both answered
//!   every row.

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

/// A fixture both engines have opened.
struct Pair {
    engine: ImportedDatabase,
    oracle: Driver,
    /// Kept so the directory outlives the test.
    _directory: PathBuf,
}

/// Builds an empty fixture and imports it into the new engine.
///
/// @param tag - what to name the scratch directory after
fn pair(tag: &str) -> Option<Pair> {
    let program = oracle_path()?;
    let directory = std::env::temp_dir().join(format!(
        "inillucent-vtab-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let path = directory.join("modules.db");
    let mut oracle = Driver::start("sqlite", &program).expect("the oracle starts");
    oracle
        .send(&Op::Open(path.to_string_lossy().into_owned()))
        .expect("the oracle opens the fixture");
    // One ordinary table, so the file has a schema before either engine adds a
    // virtual one - which is the shape the import expects.
    let observed = oracle
        .send(&Op::Exec(
            "CREATE TABLE seed (id INTEGER PRIMARY KEY, note TEXT)".to_string(),
        ))
        .expect("the exec runs");
    assert!(observed.ok, "the fixture did not build");
    let engine = ImportedDatabase::import(path, 8_192)
        .unwrap_or_else(|error| panic!("the fixture did not import: {:?}", error.detail()));
    Some(Pair {
        engine,
        oracle,
        _directory: directory,
    })
}

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
        let mine: Vec<Vec<TaggedValue>> = ours
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        OwnedDatum::Null => TaggedValue::Null,
                        OwnedDatum::Int(number) => TaggedValue::Integer(*number),
                        OwnedDatum::Real(number) => TaggedValue::Real(*number),
                        OwnedDatum::Text(bytes) => TaggedValue::Text(bytes.clone()),
                        OwnedDatum::Blob(bytes) => TaggedValue::Blob(bytes.clone()),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(mine, theirs.rows, "{sql} disagreed");
    }

    /// Walks every tree and fails on the first broken invariant.
    ///
    /// @param after - what was just run, for the message
    fn is_intact(&self, after: &str) {
        self.engine
            .check_trees()
            .unwrap_or_else(|error| panic!("the file is not intact after `{after}`: {error:?}"));
    }
}

/// Says the suite could not run, rather than passing quietly.
fn no_oracle() {
    eprintln!("the pinned SQLite oracle is not built; run tools/sqlite-reference.ps1");
}

#[test]
fn creating_an_fts5_table_creates_the_tables_sqlite_creates() {
    let Some(mut pair) = pair("fts-create") else {
        return no_oracle();
    };
    pair.both("CREATE VIRTUAL TABLE documents USING fts5(title, body)");
    pair.is_intact("CREATE VIRTUAL TABLE");
    // **The shadow tables are ordinary tables**, so both engines list the same
    // names. `rootpage` is excluded for the reason the DDL suite gives: it names
    // a page in the file the engine wrote.
    pair.answers_agree("SELECT type, name, tbl_name FROM sqlite_schema ORDER BY name");
}

#[test]
fn fts5_answers_a_match_over_the_new_trees() {
    let Some(mut pair) = pair("fts-match") else {
        return no_oracle();
    };
    pair.both("CREATE VIRTUAL TABLE documents USING fts5(title, body)");
    for (title, body) in [
        ("one", "lorem ipsum dolor sit amet"),
        ("two", "lorem sit amet consectetur"),
        ("three", "nothing to see here"),
        ("four", "dolor and lorem together"),
        ("five", "consectetur adipiscing"),
    ] {
        pair.both(&format!(
            "INSERT INTO documents(title, body) VALUES ('{title}', '{body}')"
        ));
    }
    pair.is_intact("the inserts");
    for probe in [
        "SELECT count(*) FROM documents",
        "SELECT count(*) FROM documents WHERE documents MATCH 'lorem'",
        "SELECT count(*) FROM documents WHERE documents MATCH 'dolor'",
        "SELECT count(*) FROM documents WHERE documents MATCH 'nothing'",
        "SELECT count(*) FROM documents WHERE documents MATCH 'absent'",
        "SELECT title FROM documents ORDER BY title",
        "SELECT title, body FROM documents ORDER BY title",
    ] {
        pair.answers_agree(probe);
    }
}

#[test]
fn the_rtree_answers_a_bounding_box_over_the_new_trees() {
    let Some(mut pair) = pair("rtree") else {
        return no_oracle();
    };
    pair.both("CREATE VIRTUAL TABLE boxes USING rtree(id, minX, maxX, minY, maxY)");
    for nth in 1..=20i64 {
        let low = nth * 10;
        pair.both(&format!(
            "INSERT INTO boxes(id, minX, maxX, minY, maxY) VALUES ({nth}, {low}, {}, {low}, {})",
            low + 5,
            low + 5
        ));
    }
    pair.is_intact("the inserts");
    for probe in [
        "SELECT count(*) FROM boxes",
        // **The constraint the module did not promise is the engine's.** The
        // R-Tree takes what it can prune with and leaves the rest; a scan that
        // trusted it for both answered with every box.
        "SELECT count(*) FROM boxes WHERE minX > 50 AND maxX < 150",
        "SELECT count(*) FROM boxes WHERE minX > 0",
        "SELECT id FROM boxes WHERE minY > 100 ORDER BY id",
        "SELECT id, minX, maxX FROM boxes ORDER BY id",
    ] {
        pair.answers_agree(probe);
    }
}

#[test]
fn a_module_and_an_ordinary_table_live_in_one_file() {
    let Some(mut pair) = pair("mixed") else {
        return no_oracle();
    };
    pair.both("CREATE VIRTUAL TABLE documents USING fts5(title, body)");
    pair.both("INSERT INTO seed VALUES (1, 'first')");
    pair.both("INSERT INTO documents(title, body) VALUES ('a', 'lorem')");
    pair.both("INSERT INTO seed VALUES (2, 'second')");
    pair.both("CREATE INDEX seed_note ON seed (note)");
    pair.both("INSERT INTO documents(title, body) VALUES ('b', 'ipsum')");
    pair.is_intact("a mixed campaign");
    pair.answers_agree("SELECT id, note FROM seed ORDER BY id");
    pair.answers_agree("SELECT count(*) FROM documents WHERE documents MATCH 'lorem'");
    pair.answers_agree("SELECT id FROM seed WHERE note = 'second'");
    pair.answers_agree("SELECT type, name FROM sqlite_schema ORDER BY name");
}
