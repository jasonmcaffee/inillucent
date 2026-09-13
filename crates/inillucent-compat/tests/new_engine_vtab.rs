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

    /// Returns how many page fetches answering one query took.
    ///
    /// Every shadow row a module reads is at least one fetch, so this counts
    /// the reads without the module having to be instrumented to report them.
    ///
    /// @param sql - the query
    fn fetches_for(&mut self, sql: &str) -> u64 {
        let before = self.engine.pool_stats();
        let answered = self
            .engine
            .execute_any(sql, &Params::new())
            .unwrap_or_else(|error| panic!("{sql}: refused: {:?}", error.detail()));
        let after = self.engine.pool_stats();
        let _ = answered;
        after
            .hits
            .saturating_add(after.misses)
            .saturating_sub(before.hits.saturating_add(before.misses))
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
    inillucent_compat::differential::skipping(
        "the pinned SQLite oracle is not built; run tools/sqlite-reference.{ps1,sh}",
    );
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
fn a_modules_rowid_and_rank_are_answered() {
    let Some(mut pair) = pair("rowid") else {
        return no_oracle();
    };
    pair.both("CREATE VIRTUAL TABLE documents USING fts5(title, body)");
    for (title, body) in [
        ("one", "lorem ipsum dolor"),
        ("two", "lorem sit amet"),
        ("three", "nothing here"),
    ] {
        pair.both(&format!(
            "INSERT INTO documents(title, body) VALUES ('{title}', '{body}')"
        ));
    }
    pair.is_intact("the inserts");
    // **The columns a projection does not name are not materialised, so the
    // ones it does have to survive that.** `rowid` is not a declared column and
    // `rank` is a hidden one whose value is computed rather than stored; both
    // are read through the same path a `title` is, and a mask that dropped
    // either would answer NULL rather than fail.
    for probe in [
        "SELECT title, rank FROM documents WHERE documents MATCH 'lorem' ORDER BY rank",
        "SELECT rank FROM documents WHERE documents MATCH 'lorem' ORDER BY rank",
        "SELECT title, rank FROM documents WHERE documents MATCH 'lorem' ORDER BY title",
        "SELECT title FROM documents WHERE documents MATCH 'lorem' AND title > 'a' ORDER BY title",
    ] {
        pair.answers_agree(probe);
    }
    // **A rowid off a virtual table used to be refused, and now it answers.**
    // A materialised virtual scan handed the pipeline the module's declared
    // columns and nothing else, so there was no slot a rowid could come from
    // and the plan said so rather than answering NULL. That refusal was
    // asserted here, deliberately, because turning it into a *wrong* answer was
    // the failure worth guarding against.
    //
    // It is answered now: the module has always had the value - `rowid` is on
    // the `VirtualCursor` trait - and a materialised scan carries it beside the
    // columns when the query reads one. So the guard becomes the stronger
    // thing it was standing in for: the answer is compared against SQLite's.
    // `SELECT rowid FROM t WHERE t MATCH ...` is the shape every search adapter
    // is written in, which is how the gap was found.
    for probe in [
        "SELECT rowid FROM documents WHERE documents MATCH 'lorem' ORDER BY rowid",
        "SELECT rowid, title FROM documents WHERE documents MATCH 'lorem' ORDER BY rowid",
        "SELECT title FROM documents WHERE documents MATCH 'lorem' ORDER BY rowid",
    ] {
        pair.answers_agree(probe);
    }
}

#[test]
fn a_match_reads_a_bounded_number_of_shadow_rows() {
    let Some(mut pair) = pair("reads") else {
        return no_oracle();
    };
    pair.both("CREATE VIRTUAL TABLE documents USING fts5(title, body)");
    let words = ["lorem", "ipsum", "dolor", "sit", "amet"];
    for nth in 0..120usize {
        let body: String = (0..8)
            .map(|k| words[(nth * 3 + k) % words.len()])
            .collect::<Vec<_>>()
            .join(" ");
        pair.both(&format!(
            "INSERT INTO documents(title, body) VALUES ('doc {nth}', '{body}')"
        ));
    }
    pair.is_intact("the inserts");
    // **`count(*)` reads no documents and no scores.** A `MATCH` used to cost
    // one `%_content` read per declared column per matched row and one
    // `%_docsize` read per matched row, whatever the query asked for - so
    // counting a term that appears in a hundred and twenty documents read
    // three hundred and sixty shadow rows to answer with a number. It now
    // costs the descent: the totals row and the term's doclist.
    //
    // The bound is generous on purpose. It is here to catch the eager shapes
    // coming back, which are proportional to the matched rows, not to fix the
    // exact count of a query plan.
    let counted = pair.fetches_for("SELECT count(*) FROM documents WHERE documents MATCH 'lorem'");
    let projected = pair
        .fetches_for("SELECT title FROM documents WHERE documents MATCH 'lorem' ORDER BY title");
    assert!(
        counted * 4 < projected,
        "count(*) read {counted} rows and a projection read {projected}:          count(*) is reading the documents it is only counting"
    );
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

/// Returns the single integer one query answered with.
///
/// @param engine - the database to ask
/// @param sql - a query returning one row of one integer
fn one_number(engine: &mut ImportedDatabase, sql: &str) -> i64 {
    let answered = engine
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| panic!("{sql}: refused: {:?}", error.detail()));
    match answered.rows.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(number)) => *number,
        other => panic!("{sql} answered {other:?} rather than one integer"),
    }
}

#[test]
fn a_where_over_dbstat_is_applied() {
    let Some(mut pair) = pair("dbstat") else {
        return no_oracle();
    };
    pair.both("CREATE TABLE wide (id INTEGER PRIMARY KEY, body TEXT)");
    for nth in 0..400usize {
        pair.both(&format!(
            "INSERT INTO wide VALUES ({nth}, '{}')",
            "x".repeat(600)
        ));
    }
    // **The filter used to be discarded outright.** `virtual_path` takes every
    // offered predicate out of the residual on the promise that the scan puts
    // back what it did not apply, and the branch that answers `dbstat` out of
    // the connection returned before the recheck - so this query answered with
    // every page in the file, and `SELECT name FROM dbstat WHERE name='wide'`
    // answered with the name of a different tree.
    let all = one_number(&mut pair.engine, "SELECT count(*) FROM dbstat");
    let wide = one_number(
        &mut pair.engine,
        "SELECT count(*) FROM dbstat WHERE name = 'wide'",
    );
    let seeded = one_number(
        &mut pair.engine,
        "SELECT count(*) FROM dbstat WHERE name = 'seed'",
    );
    assert!(wide > 0, "the wide table has pages and dbstat found none");
    assert!(
        wide < all,
        "a filtered dbstat returned {wide} of {all} pages: the predicate was dropped"
    );
    assert_eq!(
        wide + seeded,
        one_number(
            &mut pair.engine,
            "SELECT count(*) FROM dbstat WHERE name IN ('wide', 'seed')",
        ),
        "two names did not add up to the same pages as one and one"
    );
    // Every name a filtered scan reports is the name it was filtered to.
    let answered = pair
        .engine
        .execute_any(
            "SELECT DISTINCT name FROM dbstat WHERE name = 'wide'",
            &Params::new(),
        )
        .expect("the query runs");
    assert_eq!(
        answered.rows.len(),
        1,
        "a filtered dbstat named other trees"
    );
}

#[test]
fn a_where_over_an_eponymous_function_is_applied() {
    let Some(mut pair) = pair("eponymous") else {
        return no_oracle();
    };
    pair.both("CREATE TABLE shaped (a INTEGER, b TEXT, c REAL)");
    // A table-valued function's argument arrives as an `Eq` on its hidden
    // column and is applied; a predicate on an ordinary column is the engine's
    // to test, and was being dropped.
    pair.answers_agree("SELECT name FROM pragma_table_info('shaped') WHERE name = 'b'");
    pair.answers_agree("SELECT count(*) FROM pragma_table_info('shaped') WHERE type = 'TEXT'");
    pair.answers_agree("SELECT name FROM pragma_table_info('shaped') WHERE cid > 0 ORDER BY cid");
    // `LIKE`, `GLOB` and `REGEXP` used to be refused outright on a virtual
    // table - the module had not promised them and the engine would not try -
    // which turned statements SQLite answers into errors.
    pair.answers_agree(
        "SELECT value FROM json_each('[\"aa\",\"ab\",\"bb\"]') WHERE value LIKE 'a%'",
    );
    pair.answers_agree(
        "SELECT value FROM json_each('[\"aa\",\"ab\",\"bb\"]') WHERE value GLOB 'a*'",
    );
    pair.answers_agree("SELECT name FROM pragma_table_info('shaped') WHERE name LIKE 'b%'");
}
