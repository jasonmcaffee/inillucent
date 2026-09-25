//! The FTS5 module, compared against the pinned SQLite 3.53.4.
//!
//! Invariant: the *answers* are the claim, and the answers include the score.
//! FTS5's segment format inside `%_data` is not a published format the way the
//! R-Tree's node format is, so this suite does not hand a file from one engine
//! to the other - it asks both engines the same questions and requires the same
//! rows, in the same order, with the same `bm25()` to the last digit. The five
//! shadow table *names* and the layouts of `%_content`, `%_docsize` and
//! `%_config` are compared, because an application reads those.

use inillucent_compat::differential::{compare, Step};
use inillucent_compat::facade::Database;
use inillucent_compat::oracle::{Driver, Op};

/// Where this suite's scratch databases live.
const AREA: &str = "fts5";

/// The schema every scenario starts from.
const SCHEMA: &[Step] = &[
    Step::Exec("CREATE VIRTUAL TABLE docs USING fts5(title, body)"),
    Step::Exec("INSERT INTO docs VALUES ('The quick brown fox', 'jumps over the lazy dog')"),
    Step::Exec("INSERT INTO docs VALUES ('A slow green turtle', 'walks past the sleepy cat')"),
    Step::Exec("INSERT INTO docs VALUES ('Quick foxes and quick dogs', 'run quickly')"),
    Step::Exec("INSERT INTO docs VALUES ('The dog barks', 'and the fox runs away quick')"),
];

/// Runs the schema and then a list of steps, comparing every answer.
fn check(name: &str, steps: &[Step]) {
    let mut all = SCHEMA.to_vec();
    all.extend_from_slice(steps);
    let compared = compare(AREA, name, &all);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, all.len(), "every step was compared");
}

/// The five shadow tables carry the documented names.
#[test]
fn the_shadow_tables_are_the_documented_ones() {
    check(
        "shadow",
        &[Step::Query(
            "SELECT name, type FROM sqlite_master ORDER BY name",
        )],
    );
}

/// `%_content` holds the rowid and one column per indexed column.
#[test]
fn the_content_table_holds_the_rows() {
    check(
        "content",
        &[
            Step::Query("SELECT * FROM docs_content ORDER BY id"),
            Step::Query("SELECT k, v FROM docs_config ORDER BY k"),
        ],
    );
}

/// A single term finds the rows it appears in, in rowid order.
#[test]
fn a_term_finds_its_rows() {
    check(
        "term",
        &[
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'fox'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'nothing'"),
            Step::Query("SELECT rowid, title FROM docs WHERE docs MATCH 'turtle'"),
        ],
    );
}

/// A quoted phrase requires the words adjacent and in order.
#[test]
fn a_phrase_requires_the_words_together() {
    check(
        "phrase",
        &[
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH '\"quick brown\"'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH '\"brown quick\"'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH '\"quick fox\"'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH '\"the lazy dog\"'"),
        ],
    );
}

/// AND, OR and NOT combine matches the way the grammar says.
#[test]
fn the_operators_combine_matches() {
    check(
        "operators",
        &[
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick AND dog'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick OR turtle'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick NOT dog'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'fox AND dog OR turtle'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH '(quick OR slow) AND fox'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick dog'"),
        ],
    );
}

/// A trailing star matches every term with that prefix.
#[test]
fn a_prefix_matches_every_term_that_starts_with_it() {
    check(
        "prefix",
        &[
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick*'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'run*'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'z*'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH '\"jump*\"'"),
        ],
    );
}

/// A column filter restricts a match to one column.
#[test]
fn a_column_filter_restricts_the_match() {
    check(
        "column",
        &[
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'title:quick'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'body:quick'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'title:fox'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'body:dog AND title:quick'"),
        ],
    );
}

/// NEAR requires the terms within a distance of each other.
#[test]
fn near_requires_the_terms_close_together() {
    check(
        "near",
        &[
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'NEAR(quick fox, 2)'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'NEAR(quick fox, 0)'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'NEAR(quick dogs, 3)'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'NEAR(fox dog)'"),
        ],
    );
}

/// `rank` is the bm25 score, and ascending order is best first.
#[test]
fn the_rank_is_bm25_and_ascends() {
    check(
        "rank",
        &[
            Step::Query("SELECT rowid, rank FROM docs WHERE docs MATCH 'quick' ORDER BY rank"),
            Step::Query("SELECT rowid, rank FROM docs WHERE docs MATCH 'fox' ORDER BY rank"),
            Step::Query(
                "SELECT rowid, bm25(docs) FROM docs WHERE docs MATCH 'quick' ORDER BY rowid",
            ),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'dog' ORDER BY rank"),
        ],
    );
}

/// A row that is deleted stops matching, and one that is updated matches its
/// new text rather than its old.
#[test]
fn the_index_follows_the_rows() {
    check(
        "changes",
        &[
            Step::Exec("DELETE FROM docs WHERE rowid = 1"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'brown'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick'"),
            Step::Exec("UPDATE docs SET body = 'a purple aardvark' WHERE rowid = 2"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'sleepy'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'aardvark'"),
            Step::Query("SELECT rowid, body FROM docs ORDER BY rowid"),
            Step::Exec("INSERT INTO docs(rowid, title, body) VALUES (9, 'nine', 'a quick nine')"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick'"),
            Step::Query("SELECT count(*) FROM docs"),
        ],
    );
}

/// A rowid lookup and a plain scan both work without a match.
#[test]
fn a_scan_and_a_rowid_lookup_need_no_match() {
    check(
        "scan",
        &[
            Step::Query("SELECT rowid, title FROM docs ORDER BY rowid"),
            Step::Query("SELECT title FROM docs WHERE rowid = 3"),
            Step::Query("SELECT count(*) FROM docs WHERE rowid > 2"),
            Step::Query("SELECT rowid FROM docs WHERE rowid = 99"),
        ],
    );
}

/// An unindexed column is stored and returned but never matched.
#[test]
fn an_unindexed_column_is_stored_but_not_matched() {
    check(
        "unindexed",
        &[
            Step::Exec("CREATE VIRTUAL TABLE notes USING fts5(subject, tag UNINDEXED)"),
            Step::Exec("INSERT INTO notes VALUES ('a rare word', 'rare')"),
            Step::Exec("INSERT INTO notes VALUES ('an ordinary word', 'common')"),
            Step::Query("SELECT rowid, tag FROM notes ORDER BY rowid"),
            Step::Query("SELECT rowid FROM notes WHERE notes MATCH 'rare'"),
            Step::Query("SELECT rowid FROM notes WHERE notes MATCH 'common'"),
        ],
    );
}

/// The tokenizer folds case and splits on punctuation.
#[test]
fn the_tokenizer_folds_case_and_splits_on_punctuation() {
    check(
        "tokenizer",
        &[
            Step::Exec("CREATE VIRTUAL TABLE t USING fts5(x)"),
            Step::Exec("INSERT INTO t VALUES ('Hello, WORLD! (again)')"),
            Step::Exec("INSERT INTO t VALUES ('e-mail user@example.com')"),
            Step::Query("SELECT rowid FROM t WHERE t MATCH 'hello'"),
            Step::Query("SELECT rowid FROM t WHERE t MATCH 'WORLD'"),
            Step::Query("SELECT rowid FROM t WHERE t MATCH 'again'"),
            Step::Query("SELECT rowid FROM t WHERE t MATCH 'mail'"),
            Step::Query("SELECT rowid FROM t WHERE t MATCH 'example'"),
        ],
    );
}

/// The special commands are writes to the table's own hidden column.
#[test]
fn the_special_commands_are_accepted() {
    check(
        "commands",
        &[
            Step::Exec("INSERT INTO docs(docs) VALUES('integrity-check')"),
            Step::Exec("INSERT INTO docs(docs) VALUES('rebuild')"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick'"),
            Step::Query("SELECT rowid, title FROM docs ORDER BY rowid"),
            Step::Exec("INSERT INTO docs(docs) VALUES('optimize')"),
            Step::Exec("INSERT INTO docs(docs) VALUES('flush')"),
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('merge', 16)"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'fox'"),
            Step::Query("PRAGMA integrity_check"),
        ],
    );
}

/// A rebuild puts back exactly the index the inserts built.
#[test]
fn a_rebuild_restores_every_answer() {
    check(
        "rebuild",
        &[
            Step::Exec("DELETE FROM docs WHERE rowid = 2"),
            Step::Exec("INSERT INTO docs(rowid, title, body) VALUES (7, 'seven quick', 'foxes')"),
            Step::Exec("INSERT INTO docs(docs) VALUES('rebuild')"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'turtle'"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'foxes'"),
            Step::Query("SELECT rowid, rank FROM docs WHERE docs MATCH 'quick' ORDER BY rank"),
            Step::Query("SELECT count(*) FROM docs"),
        ],
    );
}

/// A command nobody defined is refused rather than stored as a row.
#[test]
fn an_unknown_command_is_refused() {
    check(
        "unknown",
        &[
            Step::Exec("INSERT INTO docs(docs) VALUES('nonsense')"),
            // A setting written without a value is not a command either, and
            // a value written to a name nobody defined is refused rather than
            // kept.
            Step::Exec("INSERT INTO docs(docs) VALUES('pgsz')"),
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('nonsense', 1)"),
            Step::Exec("INSERT INTO docs(docs) VALUES('delete-all')"),
            Step::Query("SELECT count(*) FROM docs"),
            Step::Query("SELECT k, v FROM docs_config ORDER BY k"),
        ],
    );
}

/// A setting is kept where an application can read it back.
#[test]
fn the_settings_are_written_to_the_config_table() {
    check(
        "settings",
        &[
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('pgsz', 64)"),
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('automerge', 4)"),
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('crisismerge', 8)"),
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('usermerge', 4)"),
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('deletemerge', 10)"),
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('secure-delete', 1)"),
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('rank', 'bm25(10.0,1.0)')"),
            Step::Query("SELECT k, v FROM docs_config ORDER BY k"),
            Step::Exec("INSERT INTO docs(docs, rank) VALUES('pgsz', 128)"),
            Step::Query("SELECT k, v FROM docs_config ORDER BY k"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick'"),
        ],
    );
}

/// M5 (task-1920): a tokenizer this build has not got is refused by name.
///
/// **A substituted tokenizer is a wrong answer.** `Tokenizer::named` read any
/// name it did not recognise as `unicode61`, on the reasoning that a schema
/// naming a tokenizer this build has not got should still open. But the
/// tokenizer decides what `MATCH` means: `tokenize='trigram'` makes it a
/// substring search in SQLite, and `unicode61` makes it a whole-word search.
/// So `CREATE VIRTUAL TABLE t USING fts5(body, tokenize='trigram')` succeeded,
/// `SELECT ... WHERE t MATCH 'ell'` ran, and it answered a different question
/// from the one it was asked with no error anywhere.
///
/// **This case is deliberately not a `compare` step.** SQLite answers these
/// statements and this engine refuses them, which `compare` would report as a
/// failure - correctly, because it is a difference. It is a *recorded*
/// difference: exit code 3, `unsupported`, and the `FTS5 tokenizer options`
/// row of `docs/feature-comparison.md` says which names are implemented.
/// Building `trigram` is a separate piece of work; what this asserts is that
/// asking for it says so.
///
/// Both sides are exercised: the reference's own answer is taken first, so
/// what is recorded as the difference is measured rather than assumed.
#[test]
fn a_tokenizer_this_build_has_not_got_is_refused_rather_than_substituted() {
    let Some(program) = inillucent_compat::differential::sqlite_oracle() else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    // What SQLite does with the same statements.
    let reference = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fts5-trigram.db");
    let _ = std::fs::remove_file(&reference);
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("the oracle greets");
    driver
        .send(&Op::Open(reference.display().to_string()))
        .expect("the oracle opens");
    for statement in [
        "CREATE VIRTUAL TABLE t USING fts5(body, tokenize='trigram')",
        "INSERT INTO t VALUES ('hello world')",
    ] {
        let observation = driver
            .send(&Op::Exec(statement.to_string()))
            .expect("the oracle answers");
        assert!(
            observation.ok,
            "SQLite refused {statement}: {}",
            observation.message
        );
    }
    let substring = driver
        .send(&Op::Query(
            "SELECT rowid FROM t WHERE t MATCH 'ell'".to_string(),
        ))
        .expect("the oracle answers");
    assert!(substring.ok);
    assert_eq!(
        substring.rows.len(),
        1,
        "trigram makes MATCH a substring search in SQLite, which is the whole \
         difference substituting unicode61 hid"
    );

    // What this engine does: refuse, by name, at `CREATE VIRTUAL TABLE`.
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fts5-trigram.rdb");
    inillucent_base::testing::remove_database(&path);
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    for name in ["trigram", "icu", "porter2"] {
        let refused = connection
            .execute(&format!(
                "CREATE VIRTUAL TABLE t_{name} USING fts5(body, tokenize='{name}')"
            ))
            .expect_err("a tokenizer this build has not got must be refused");
        assert_eq!(
            refused.unsupported(),
            Some(format!("the fts5 tokenizer {name}").as_str()),
            "the refusal must name the tokenizer: {refused:?}"
        );
        assert!(
            connection
                .query(&format!("SELECT rowid FROM t_{name}"))
                .is_err(),
            "the refused CREATE must leave no table behind"
        );
    }
    // The names this build does have still work, so the refusal is about the
    // name rather than about the clause.
    for (suffix, specification) in [
        ("default", ""),
        ("unicode", ", tokenize='unicode61'"),
        ("ascii", ", tokenize='ascii'"),
        ("porter", ", tokenize='porter'"),
        ("porter_ascii", ", tokenize=\"porter ascii\""),
    ] {
        connection
            .execute(&format!(
                "CREATE VIRTUAL TABLE ok_{suffix} USING fts5(body{specification})"
            ))
            .unwrap_or_else(|error| panic!("{suffix}: {error:?}"));
    }
}

/// `VACUUM` leaves one `sqlite_master` row per name.
///
/// **It used to write a second row for every shadow table (task-1979, R2).**
/// The rebuild replayed every `CREATE TABLE` it found, the shadow ones
/// included, and then the `CREATE VIRTUAL TABLE` made a second set under the
/// same names: six rows became eleven for six names and the file roughly
/// doubled. `integrity-check` said `ok` and the table still answered, so
/// nothing but the catalog itself showed it.
#[test]
fn vacuum_leaves_one_schema_row_per_name() {
    check(
        "vacuum",
        &[
            Step::Exec("VACUUM"),
            Step::Query("SELECT name, type FROM sqlite_master ORDER BY name"),
            Step::Query("SELECT count(*), count(DISTINCT name) FROM sqlite_master"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'quick' ORDER BY rowid"),
            Step::Query("SELECT * FROM docs_content ORDER BY id"),
        ],
    );
}

/// `content=''` makes a contentless table, and it is one.
///
/// **It used to be accepted and ignored (task-1979, R5).** The option fell
/// through to the catch-all that records an option and changes nothing, so
/// `%_content` was created anyway, every column read back the document text
/// where SQLite answers NULL, and `delete-all` - the only way to empty a
/// contentless table - was refused unconditionally with a message naming the
/// two kinds of table it is for. An application chooses the option precisely so
/// that the source text is not written into the database, and it was.
#[test]
fn a_contentless_table_stores_no_text() {
    check(
        "contentless",
        &[
            Step::Exec("CREATE VIRTUAL TABLE cl USING fts5(body, content='')"),
            Step::Exec("INSERT INTO cl(rowid, body) VALUES (1, 'hello world')"),
            Step::Exec("INSERT INTO cl(rowid, body) VALUES (2, 'goodbye world')"),
            Step::Query("SELECT name FROM sqlite_master WHERE name LIKE 'cl%' ORDER BY name"),
            Step::Query("SELECT rowid, body FROM cl WHERE cl MATCH 'world' ORDER BY rowid"),
            Step::Query("SELECT rowid FROM cl ORDER BY rowid"),
            Step::Query("SELECT rowid FROM cl WHERE rowid = 2"),
            Step::Query("SELECT count(*) FROM cl"),
            // Both engines refuse this, which the comparison grades by code.
            Step::Query("DELETE FROM cl WHERE rowid = 1"),
            Step::Exec("INSERT INTO cl(cl) VALUES('delete-all')"),
            Step::Query("SELECT count(*) FROM cl"),
            Step::Query("SELECT rowid FROM cl WHERE cl MATCH 'world' ORDER BY rowid"),
        ],
    );
}

/// The options this build cannot honour are refused rather than ignored.
///
/// **Both used to be accepted and changed nothing (task-1979, R15).**
/// `detail='none'` says the index holds no positions, and SQLite refuses a
/// phrase query against one - this engine stored the positions anyway and
/// answered the phrase query, which is an answer the schema says is not
/// available. The refusal carries `unsupported`, so the command line exits 3
/// and a caller can tell "not built" from "your statement is wrong".
///
/// It is not a differential case: SQLite implements all three, so the two
/// engines disagree here on purpose and the claim is about the refusal.
#[test]
fn the_fts5_options_this_build_cannot_honour_are_refused() {
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fts5-options.rdb");
    inillucent_base::testing::remove_database(&path);
    let database = Database::open(&path).expect("the database opens");
    let connection = database.session().expect("the connection opens");
    for (name, option) in [
        ("detail_none", "detail='none'"),
        ("detail_column", "detail='column'"),
        ("columnsize", "columnsize=0"),
    ] {
        let refused = connection
            .execute(&format!(
                "CREATE VIRTUAL TABLE r_{name} USING fts5(body, {option})"
            ))
            .expect_err("an option this build cannot honour must be refused");
        assert!(
            refused.unsupported().is_some(),
            "{option} must be refused as unsupported: {refused:?}"
        );
        assert!(
            connection
                .query(&format!("SELECT rowid FROM r_{name}"))
                .is_err(),
            "the refused CREATE must leave no table behind"
        );
    }
    // The values this build does store are accepted, so the refusal is about
    // the value rather than about the option.
    for (suffix, option) in [
        ("full", "detail='full'"),
        ("sized", "columnsize=1"),
        ("prefixed", "prefix='2 3'"),
    ] {
        connection
            .execute(&format!(
                "CREATE VIRTUAL TABLE ok_{suffix} USING fts5(body, {option})"
            ))
            .unwrap_or_else(|error| panic!("{option}: {error:?}"));
    }
}

/// `INSERT ... SELECT` fills the index from an ordinary table and from itself.
///
/// **It was refused as unsupported,** and `INSERT INTO docs(title, body) SELECT
/// ... FROM src` is the FTS5 backfill idiom: the first statement somebody
/// writes after creating the table. The rowids, the matches, `bm25()` and
/// `changes()` are compared with SQLite's. The insert that reads `docs` itself
/// checks that the query is read in full before the first row is written: a
/// statement that saw its own new rows would never finish, or would add more
/// than four.
#[test]
fn an_insert_select_fills_the_index() {
    check(
        "insert_select",
        &[
            Step::Exec("CREATE TABLE src (id INTEGER PRIMARY KEY, title TEXT, body TEXT)"),
            Step::Exec(
                "INSERT INTO src VALUES (10, 'A red kite', 'soars over the quick hills'), \
                 (11, 'The last fox', 'sleeps in the lazy sun')",
            ),
            Step::Exec("INSERT INTO docs (rowid, title, body) SELECT id, title, body FROM src"),
            Step::Query("SELECT changes()"),
            Step::Query("SELECT rowid, title FROM docs WHERE docs MATCH 'fox' ORDER BY rowid"),
            Step::Query(
                "SELECT rowid, round(bm25(docs), 6) FROM docs WHERE docs MATCH 'quick' ORDER BY rank",
            ),
            Step::Exec("INSERT INTO docs (title, body) SELECT title, body FROM docs WHERE rowid < 3"),
            Step::Query("SELECT changes()"),
            Step::Query("SELECT count(*) FROM docs"),
            Step::Query("SELECT rowid FROM docs WHERE docs MATCH 'turtle' ORDER BY rowid"),
        ],
    );
}
