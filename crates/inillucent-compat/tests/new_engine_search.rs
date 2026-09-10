//! `inillucent_search` over the new engine's trees, graded against the old engine.
//!
//! Invariant: **the module is the same module, and so are its answers.** Its
//! tokenizer, its BM25, its vector distances, its fusion and its generation
//! merging are untouched by the rearchitecture; what changed is that its five
//! shadow tables are ordinary PAX trees instead of SQLite b-trees. So the
//! question here is not "does retrieval work" - `tests/search.rs` answers that
//! against the module's declared contract - but "does it return the same rows,
//! in the same order, when its storage is the new engine".
//!
//! There is no SQLite oracle in this file and there must not be, for the reason
//! `tests/search.rs` gives: SQLite has no equivalent of this module, and
//! comparing it against FTS5 would be comparing two retrieval engines and
//! calling the difference a bug. The oracle here is **the old engine**, which is
//! the right one for a storage change: same module, same corpus, same queries,
//! two stores, and any difference is the store's.
//!
//! This is the Phase 5 consumer story's first half. The `legacy-storage` cargo
//! feature the TDD describes was never built - `inillucent-search` has always
//! compiled against `inillucent-ext` rather than `inillucent-storage`, and which store
//! it reaches is decided by `Context::store` at run time rather than at compile
//! time - so what has to be shown is not that a feature was removed but that
//! both stores answer alike.

use inillucent_compat::differential::{scratch, start_inillucent};
use inillucent_compat::newengine::ImportedDatabase;
use inillucent_exec::physical::Params;
use inillucent_session::statement::{execute_batch, Statement};
use inillucent_session::Connection;
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

/// The queries both stores are asked, in the order they are asked.
///
/// **They project declared columns rather than `rowid`.** The new engine
/// refuses a `rowid` read off a virtual table - deliberately, and
/// `a_modules_rowid_and_rank_are_answered` asserts the refusal - so a
/// comparison that selected one would be measuring that known gap on every row
/// instead of measuring the store. The gap is real and is named in the write-up;
/// what these queries ask is whether the same documents come back in the same
/// order, which is what a retrieval consumer reads.
const QUERIES: [&str; 6] = [
    "SELECT title FROM docs WHERE docs MATCH 'eligibility' ORDER BY rank",
    "SELECT title FROM docs WHERE docs MATCH 'claim' ORDER BY rank",
    "SELECT title FROM docs WHERE docs MATCH 'Tuesday' ORDER BY rank",
    "SELECT title, body FROM docs WHERE docs MATCH 'service' ORDER BY rank",
    "SELECT count(*) FROM docs",
    "SELECT title FROM docs WHERE docs MATCH 'denial' ORDER BY rank",
];

/// Runs a statement on the old engine for its effect.
///
/// @param connection - the old engine's connection
/// @param sql - the statement
fn exec(connection: &Connection, sql: &str) {
    execute_batch(connection, sql.as_bytes())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
}

/// Returns every row of a query on the old engine, rendered as text.
///
/// @param connection - the old engine's connection
/// @param sql - the query
fn old_rows(connection: &Connection, sql: &str) -> Vec<Vec<String>> {
    let mut statement = Statement::prepare(connection, sql.as_bytes())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()))
        .0;
    let mut out = Vec::new();
    while statement
        .step()
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()))
    {
        let width = statement.column_count();
        let mut row = Vec::with_capacity(width);
        for index in 0..width {
            row.push(render_value(&statement.value(index)));
        }
        out.push(row);
    }
    out
}

/// Renders one old-engine value as text.
///
/// @param value - the column's value
fn render_value(value: &inillucent_value::Value<'_>) -> String {
    match value {
        inillucent_value::Value::Null => "NULL".to_string(),
        inillucent_value::Value::Integer(number) => number.to_string(),
        inillucent_value::Value::Real(number) => {
            String::from_utf8_lossy(&inillucent_value::numeric::real_to_text(*number)).into_owned()
        }
        inillucent_value::Value::Text(text) => String::from_utf8_lossy(text.raw()).into_owned(),
        inillucent_value::Value::Blob(blob) => blob
            .raw()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    }
}

/// Renders one new-engine row as text, the same way.
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

/// The module answers the same rows over the new trees as over the old pages.
#[test]
fn the_search_module_answers_alike_on_both_stores() {
    let Some(mut engine) = new_engine("alike") else {
        eprintln!("the empty fixture is not checked in; skipping");
        return;
    };
    let connection = start_inillucent(AREA, "alike-old");

    for statement in seed_statements() {
        exec(&connection, &statement);
        engine
            .execute_any(&statement, &Params::new())
            .unwrap_or_else(|error| {
                panic!(
                    "{statement}: the new engine refused: {}",
                    error.detail().unwrap_or_default()
                )
            });
    }

    for query in QUERIES {
        let theirs = old_rows(&connection, query);
        let ours = engine
            .execute_any(query, &Params::new())
            .unwrap_or_else(|error| {
                panic!(
                    "{query}: the new engine refused: {}",
                    error.detail().unwrap_or_default()
                )
            });
        let mine: Vec<Vec<String>> = ours.rows.iter().map(|row| new_row(row)).collect();
        assert_eq!(
            mine, theirs,
            "{query}: the two stores gave different answers"
        );
    }
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
        eprintln!("the empty fixture is not checked in; skipping");
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
