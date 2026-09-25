//! `inillucent_search` over the new engine's trees.
//!
//! Invariant: **the module is the same module, and so are its answers.** Its
//! tokenizer, its BM25, its vector distances, its fusion and its generation
//! merging are untouched by the rearchitecture; what changed is that its five
//! shadow tables are ordinary PAX trees instead of SQLite b-trees. So the
//! question here is not "does retrieval work" - `tests/search.rs` answers that
//! against the module's declared contract - but "does the new engine's storage
//! answer this corpus the way the module is supposed to".
//!
//! **This file used to grade the new engine against the old one**, which was
//! genuinely the right oracle for a storage change and could not have been
//! SQLite - `tests/search.rs` already gives the reason: SQLite has no
//! equivalent of this module, and comparing it against FTS5 would be comparing
//! two retrieval engines and calling the difference a bug. Deleting the old
//! engine deletes that comparison along with the crate, so what is asserted
//! here now is the concrete rows this corpus is known to produce - a fixed
//! expectation rather than a second store to check against. `tests/search.rs`'s
//! own contract tests are what prove the module's behaviour is correct in the
//! first place; this file only has to prove the new engine's storage keeps
//! answering it the same way from one run to the next.
//!
//! This is the Phase 5 consumer story's first half. The `legacy-storage` cargo
//! feature the TDD describes was never built - `inillucent-search` has always
//! compiled against `inillucent-ext` rather than `inillucent-storage`, and which store
//! it reaches is decided by `Context::store` at run time rather than at compile
//! time.

use inillucent_compat::differential::scratch;
use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// Where this suite's scratch databases live.
const AREA: &str = "new-engine-search";

/// The corpus, which is `tests/search.rs`'s so the two suites grade one thing.
const CORPUS: [(i64, &str, &str); 5] = [
    (
        1,
        "eligibility rules",
        "a member is eligible when the plan covers the service",
    ),
    (
        2,
        "claim submission",
        "submit the claim within ninety days of the service date",
    ),
    (
        3,
        "appeal window",
        "an appeal must be filed within sixty days of the denial",
    ),
    (
        4,
        "weather report",
        "rain is expected on Tuesday with a chance of hail",
    ),
    (
        5,
        "discount schedule",
        "the launch shipped on a Tuesday with no discount",
    ),
];

/// Renders one new-engine row as text, the same way the old helper here used
/// to render both engines' rows, so a captured expectation reads the way this
/// file has always printed one.
///
/// @param row - the row's values
fn new_row(row: &[OwnedDatum]) -> Vec<String> {
    row.iter()
        .map(|value| match value {
            OwnedDatum::Null => "NULL".to_string(),
            OwnedDatum::Int(number) => number.to_string(),
            OwnedDatum::Real(number) => {
                String::from_utf8_lossy(&inillucent_value::numeric::real_to_text(*number))
                    .into_owned()
            }
            OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            OwnedDatum::Blob(bytes) => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        })
        .collect()
}

/// Returns the statements that build and fill the corpus.
fn seed_statements() -> Vec<String> {
    let mut out =
        vec!["CREATE VIRTUAL TABLE docs USING inillucent_search(title, body)".to_string()];
    for (id, title, body) in CORPUS {
        out.push(format!(
            "INSERT INTO docs(rowid, title, body) VALUES ({id}, '{title}', '{body}')"
        ));
    }
    out
}

/// Opens a new-engine database with an empty schema to build on.
///
/// The import path is the only way into the new engine today, so the fixture is
/// an empty SQLite file - which is a real file with a real header and no
/// objects, not a special case.
///
/// @param tag - what to name the scratch file after
fn new_engine(tag: &str) -> Option<ImportedDatabase> {
    let path = scratch(AREA, tag, "newengine");
    // An empty database is a hundred-byte header and nothing else, which is
    // exactly what SQLite writes for `sqlite3 new.db "PRAGMA user_version"`.
    let source = inillucent_compat::workspace_root().join("compat/fixtures/empty-p4096-utf8.db");
    if !source.is_file() {
        return None;
    }
    std::fs::copy(&source, &path).ok()?;
    ImportedDatabase::import(path, 32_768).ok()
}

/// The module answers this corpus the way its five documents say it should,
/// over the new engine's own trees.
///
/// **What this asserts, and why it is a set for two of the six queries rather
/// than an ordered row list.** The old engine used to be run beside this one so
/// the two answers could be compared directly, which pinned the exact BM25
/// rank order without this file having to know it - the two stores either
/// agreed or one of them was wrong. With the old engine gone there is nothing
/// left to read that order from except running this engine and writing down
/// what it says, which needs a build of `inillucent-search` that this working
/// tree did not have while this rewrite was done (see the comment above the
/// two set-based assertions). Every other query here has only one document
/// containing its term as a whole word, so the row list is exact regardless of
/// ranking. `'Tuesday'` and `'service'` each match two documents by construction
/// of the corpus below, and those two assertions check the *set* of titles
/// rather than their order until somebody with a working build can capture the
/// real ranking and tighten them to `assert_eq!` on the row list, the way the
/// other four already are.
#[test]
fn the_search_module_answers_the_corpus_it_was_given() {
    let Some(mut engine) = new_engine("answers") else {
        inillucent_compat::differential::skipping("the empty fixture is not checked in");
        return;
    };

    for statement in seed_statements() {
        engine
            .execute_any(&statement, &Params::new())
            .unwrap_or_else(|error| {
                panic!(
                    "{statement}: the new engine refused: {}",
                    error.detail().unwrap_or_default()
                )
            });
    }

    let mut run = |query: &str| -> Vec<Vec<String>> {
        engine
            .execute_any(query, &Params::new())
            .unwrap_or_else(|error| {
                panic!(
                    "{query}: the new engine refused: {}",
                    error.detail().unwrap_or_default()
                )
            })
            .rows
            .iter()
            .map(|row| new_row(row))
            .collect()
    };

    // Each of these terms appears as a whole word in exactly one document's
    // title or body, so the row list is exact no matter how the module ranks -
    // there is only one row to rank.
    assert_eq!(
        run("SELECT title FROM docs WHERE docs MATCH 'eligibility' ORDER BY rank"),
        vec![vec!["eligibility rules".to_string()]],
        "'eligibility' is only in document 1's title"
    );
    assert_eq!(
        run("SELECT title FROM docs WHERE docs MATCH 'claim' ORDER BY rank"),
        vec![vec!["claim submission".to_string()]],
        "'claim' is only in document 2's title and body"
    );
    assert_eq!(
        run("SELECT title FROM docs WHERE docs MATCH 'denial' ORDER BY rank"),
        vec![vec!["appeal window".to_string()]],
        "'denial' is only in document 3's body"
    );
    assert_eq!(
        run("SELECT count(*) FROM docs"),
        vec![vec!["5".to_string()]],
        "the corpus below has five rows"
    );

    // 'Tuesday' is in documents 4 and 5's bodies; 'service' is in documents 1
    // and 2's bodies. Both are two-row answers, and the row order between them
    // is BM25's to decide - a set comparison still catches a wrong document, a
    // missing one, or a phantom third row, which is most of what this test
    // existed to catch even when it had a second store to compare against.
    let mut tuesday = run("SELECT title FROM docs WHERE docs MATCH 'Tuesday' ORDER BY rank");
    tuesday.sort();
    assert_eq!(
        tuesday,
        vec![
            vec!["discount schedule".to_string()],
            vec!["weather report".to_string()],
        ],
        "'Tuesday' is in documents 4 and 5's bodies"
    );
    let mut service = run("SELECT title, body FROM docs WHERE docs MATCH 'service' ORDER BY rank");
    service.sort();
    let mut expected = vec![
        vec![
            "claim submission".to_string(),
            "submit the claim within ninety days of the service date".to_string(),
        ],
        vec![
            "eligibility rules".to_string(),
            "a member is eligible when the plan covers the service".to_string(),
        ],
    ];
    expected.sort();
    assert_eq!(
        service, expected,
        "'service' is in documents 1 and 2's bodies"
    );
}

/// A search table's shadow tables are ordinary tables in the new engine.
///
/// The invariant the whole store rests on: there is no side file and no second
/// write path, so the five shadow tables are listed by `sqlite_schema` and are
/// walked by the integrity checker like any other tree. A module that had kept
/// its own storage would pass every query above and fail this.
#[test]
fn the_shadow_tables_are_ordinary_trees() {
    let Some(mut engine) = new_engine("shadows") else {
        inillucent_compat::differential::skipping("the empty fixture is not checked in");
        return;
    };
    for statement in seed_statements() {
        engine
            .execute_any(&statement, &Params::new())
            .unwrap_or_else(|error| {
                panic!(
                    "{statement}: the new engine refused: {}",
                    error.detail().unwrap_or_default()
                )
            });
    }

    let listed = engine
        .execute_any(
            "SELECT name FROM sqlite_schema WHERE name LIKE 'docs%' ORDER BY name",
            &Params::new(),
        )
        .expect("the schema reads");
    let names: Vec<String> = listed
        .rows
        .iter()
        .filter_map(|row| row.first())
        .map(|value| match value {
            OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            other => format!("{other:?}"),
        })
        .collect();
    for suffix in ["config", "content", "delta", "gen", "state"] {
        assert!(
            names.iter().any(|name| name == &format!("docs_{suffix}")),
            "docs_{suffix} is not in the schema: {names:?}"
        );
    }

    // And every tree the database holds still checks out structurally, which is
    // what says the module wrote rows rather than bytes.
    engine.check_trees().expect("every tree is intact");
}
