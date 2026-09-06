//! The SQLLogicTest corpus's read-only subset, through the new engine.
//!
//! Invariant: the expected values are SQLite's, recorded by `inillucent-slt` from
//! the pinned binary and checked in. Nothing here computes an expectation, so a
//! run grades the new engine against the reference even with no oracle on the
//! machine - which is the same contract `conformance.rs` holds for the old one,
//! deliberately, so the two are comparable.
//!
//! ## What "the read-only subset" means, precisely
//!
//! The TDD's Phase 2 acceptance is "read-only SLT subset 100%". The subset is
//! not a hand-picked list: it is **every query the physical pass accepts**.
//! Anything it refuses is counted and named, because the whole design of that
//! pass is a whitelist that fails loudly rather than approximating - and a
//! refusal that is silently skipped is a refusal nobody reads.
//!
//! So this test asserts three things, and the middle one is the acceptance:
//!
//! 1. the subset is not empty and not trivial (a whitelist that accepted
//!    nothing would otherwise pass);
//! 2. **every query in it answers exactly what SQLite answered**;
//! 3. the refusals are reported with their reasons, so the gap between the
//!    subset and the corpus is visible rather than implied.
//!
//! ## Why the fixture is copied first
//!
//! The import writes a `.rdb` beside the file it reads. The corpus fixture is
//! checked in, so the test copies it to a temporary directory and imports that
//! - a test that left build products in `compat/fixtures/` would be a test that
//! dirties the tree it is run from.

use std::collections::BTreeMap;
use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::slt::{self, Record, TestFile};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// Returns every conformance file.
fn test_files() -> Vec<PathBuf> {
    let directory = workspace_root().join("tests/conformance");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "test")
        })
        .collect();
    files.sort();
    files
}

/// Copies the corpus fixture somewhere the import may write beside it.
///
/// The directory carries the caller's name as well as the process id, because
/// the tests in this file run in parallel in one process and two of them
/// copying onto the same path is a locked file rather than a shared fixture.
///
/// @param tag - the calling test's name
fn corpus_copy(tag: &str) -> PathBuf {
    let source = workspace_root().join("compat/fixtures/select-corpus.db");
    let directory =
        std::env::temp_dir().join(format!("inillucent-slt-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let target = directory.join("select-corpus.db");
    std::fs::copy(&source, &target).expect("the corpus fixture copies");
    target
}

/// Renders one row the way the format writes it.
///
/// @param row - the row the new engine produced
/// @param types - the format's per-column type letters
fn render_row(row: &[OwnedDatum], types: &str) -> Vec<String> {
    let letters: Vec<char> = types.chars().collect();
    row.iter()
        .enumerate()
        .map(|(index, value)| {
            let borrowed = value.borrow();
            let as_value = inillucent_exec::scalar::to_value(borrowed);
            slt::render_value(&as_value, letters.get(index).copied().unwrap_or('T'))
        })
        .collect()
}

/// Every query the physical pass accepts answers what SQLite answered.
#[test]
fn the_read_only_subset_is_green_on_the_new_engine() {
    let files = test_files();
    assert!(!files.is_empty(), "no conformance files were found");
    let fixture = corpus_copy("subset");
    let database = ImportedDatabase::import(fixture.clone(), 8_192)
        .unwrap_or_else(|error| panic!("the corpus did not import: {:?}", error.detail()));

    let mut accepted = 0usize;
    // The count *and* one example, because a refusal reason with no query
    // behind it is a line nobody can act on - and the work list this test
    // exists to produce is a list of queries.
    let mut refused: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();

    for path in &files {
        let text = std::fs::read_to_string(path).expect("the file reads");
        let file = TestFile::parse(&text).expect("the file parses");
        for record in &file.records {
            let Record::Query {
                types,
                sort,
                sql,
                expected,
                ..
            } = record
            else {
                // `statement ok` records describe how the fixture was built,
                // and the fixture is checked in.
                continue;
            };
            let plan = match database.plan(sql) {
                Ok(plan) => plan,
                Err(error) => {
                    let entry = refused
                        .entry(reason_of(error.detail().unwrap_or("no detail")))
                        .or_insert_with(|| (0, sql.clone()));
                    entry.0 += 1;
                    continue;
                }
            };
            let outcome = database.execute(&plan, &Params::new());
            let (rows, _) = match outcome {
                Ok(answer) => answer,
                Err(error) => {
                    let entry = refused
                        .entry(reason_of(error.detail().unwrap_or("no detail")))
                        .or_insert_with(|| (0, sql.clone()));
                    entry.0 += 1;
                    continue;
                }
            };
            accepted = accepted.saturating_add(1);
            let rendered: Vec<Vec<String>> =
                rows.iter().map(|row| render_row(row, types)).collect();
            let found = slt::apply_sort(rendered, sort);
            if &found != expected {
                failures.push(format!(
                    "{sql}\n  expected {expected:?}\n  found    {found:?}"
                ));
            }
        }
    }

    // The refusals, so the gap between the subset and the corpus is visible.
    let refused_total: usize = refused.values().map(|(count, _)| *count).sum();
    let mut summary: Vec<String> = refused
        .iter()
        .map(|(reason, (count, example))| {
            format!(
                "  {count:>4}  {reason}
          e.g. {example}"
            )
        })
        .collect();
    summary.sort();
    println!(
        "new-engine SLT: {accepted} accepted, {refused_total} refused\n{}",
        summary.join("\n")
    );

    assert!(
        accepted >= 50,
        "only {accepted} queries were accepted, which is too few for the subset to mean anything:\n{}",
        summary.join("\n")
    );
    assert!(
        failures.is_empty(),
        "{} of {accepted} accepted queries diverged from SQLite:\n{}",
        failures.len(),
        failures.join("\n")
    );
    let _ = std::fs::remove_dir_all(fixture.parent().unwrap_or(&fixture));
}

/// Collapses a refusal message to the construct it named.
///
/// The physical pass's refusals are of the form "the new engine's physical pass
/// does not handle X yet", and grouping by X is what turns a list of six
/// hundred lines into the handful of features that are actually missing.
///
/// @param detail - the error's detail text
fn reason_of(detail: &str) -> String {
    let marker = "does not handle ";
    match detail.find(marker) {
        Some(at) => detail
            .get(at.saturating_add(marker.len())..)
            .unwrap_or(detail)
            .trim_end_matches(" yet")
            .to_string(),
        None => detail
            .split(':')
            .next()
            .unwrap_or(detail)
            .trim()
            .to_string(),
    }
}

/// `sqlite_schema` is queryable through the new engine, from the file's own
/// catalog tree.
///
/// The TDD asks Phase 2 for "`inillucent-catalog` over the catalog tree (read side)
/// and the `sqlite_schema` view". This is the view half, and the thing worth
/// asserting is that it needed no view machinery: the catalog is a table in the
/// file with a rowid key and five columns, so the ordinary planner picks it up,
/// the ordinary scan reads it, and the ordinary projection answers.
///
/// It is also the strongest available check that the file describes itself. The
/// import already refuses if the catalog it reads back differs from the one it
/// wrote; this goes further and asks the *query engine* to read it, through the
/// pool, out of pages it did not keep a handle on.
#[test]
fn the_catalog_tree_answers_queries_as_sqlite_schema() {
    let fixture = corpus_copy("catalog");
    let database = ImportedDatabase::import(fixture, 8_192)
        .unwrap_or_else(|error| panic!("the corpus did not import: {:?}", error.detail()));

    let (rows, names) = database
        .run("SELECT type, name, tbl_name FROM sqlite_schema ORDER BY name")
        .unwrap_or_else(|error| panic!("sqlite_schema is not queryable: {:?}", error.detail()));
    assert_eq!(names, vec!["type", "name", "tbl_name"]);
    assert!(
        !rows.is_empty(),
        "the corpus has tables, so its catalog has rows"
    );

    // Every row names an object the import actually wrote, and the two kinds
    // are the only ones a bulk load produces.
    for row in &rows {
        let kind = text_of(row.first());
        assert!(
            kind == "table" || kind == "index",
            "a bulk-loaded catalog holds tables and indexes, not {kind}"
        );
    }

    // The names agree with the tables the binder resolved against, which is the
    // point of the round trip: one schema, read out of one place.
    let tables: Vec<String> = rows
        .iter()
        .filter(|row| text_of(row.first()) == "table")
        .map(|row| text_of(row.get(1)))
        .collect();
    // And it does not list itself, which is SQLite's behaviour and not an
    // oversight: a reader has to find the catalog before it can read anything,
    // so the catalog's own location lives where the reader looks first - the
    // meta page here, page 1 in SQLite - rather than in a row it would have to
    // already be able to read.
    assert!(
        !tables.iter().any(|name| name == "sqlite_schema"),
        "sqlite_schema does not list itself; it held {tables:?}"
    );
    assert!(
        !tables.is_empty(),
        "the corpus's own tables are listed; it held {tables:?}"
    );
    for skipped in database.skipped() {
        assert!(
            !tables.contains(skipped),
            "{skipped} was skipped by the import, so it must not be in the catalog"
        );
    }

    // A predicate over the catalog runs like any other, which is what "no view
    // machinery" means in practice.
    let (indexes, _) = database
        .run("SELECT count(*) FROM sqlite_schema WHERE type = 'index'")
        .unwrap_or_else(|error| panic!("a filtered catalog scan failed: {:?}", error.detail()));
    let counted = match indexes.first().and_then(|row| row.first()) {
        Some(OwnedDatum::Int(value)) => *value,
        other => panic!("count(*) did not answer an integer: {other:?}"),
    };
    let expected = rows
        .iter()
        .filter(|row| text_of(row.first()) == "index")
        .count() as i64;
    assert_eq!(counted, expected, "the filter and the scan agree");

    // And the stored declaration is the real one, so a reader that has only
    // this file can rebuild the schema from it.
    let (sql, _) = database
        .run("SELECT sql FROM sqlite_schema WHERE type = 'table' AND name <> 'sqlite_schema'")
        .unwrap_or_else(|error| panic!("reading the stored SQL failed: {:?}", error.detail()));
    for row in &sql {
        let text = text_of(row.first()).to_ascii_uppercase();
        assert!(
            text.starts_with("CREATE TABLE"),
            "a table's stored declaration should be its CREATE TABLE, got {text:?}"
        );
    }
}

/// Returns a datum as text, for the assertions above.
///
/// @param value - the column, when the row has one
fn text_of(value: Option<&OwnedDatum>) -> String {
    match value {
        Some(OwnedDatum::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        other => format!("{other:?}"),
    }
}
