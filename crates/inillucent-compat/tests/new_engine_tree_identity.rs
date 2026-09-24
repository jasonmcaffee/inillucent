//! A tree's identity is in the file, and every process derives the same one.
//!
//! Invariant: **the number a tree is known by is read from the bytes, never
//! handed out by a counter.** Every logical row record in the write-ahead log
//! carries `tree: self.tree_id()`, so the identifier is not private bookkeeping
//! however much it looks like it — a reader that numbered trees differently
//! from the writer would hand recovery's row records to the wrong tree, which is
//! a wrong answer rather than a refusal.
//!
//! There used to be three numberings and none of them was in the
//! file: the import used the *source* SQLite file's root pages, DDL counted up
//! from `FIRST_CREATED_ROOT` in a counter that restarted at every open, and an
//! `open` numbered objects 1, 2, 3… in catalog order. They agreed only by
//! accident, and nothing checked.
//!
//! This is what checks. It is deliberately blunt: it compares the identifiers a
//! *writer* used against the ones a *reader* derives from the same file, so any
//! caller that reintroduces a counter fails it — a counter cannot reproduce the
//! writer's numbers, because the writer's are above `FIRST_CREATED_ROOT` for a
//! database built by DDL and are the source file's root pages for one built by
//! import.

use std::collections::HashSet;
use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;

/// The pool these databases get; they are small.
const FRAMES: usize = 256;

/// The page size, which is the engine's default.
const PAGE_SIZE: usize = 32_768;

/// Returns a clean path for one test's database.
///
/// @param name - the test's name
fn scratch(name: &str) -> PathBuf {
    let area = workspace_root().join("target/scratch/identity");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    inillucent_base::testing::remove_database(&path);
    path
}

/// The identifiers a database holds, sorted so two are comparable.
///
/// @param database - the database to ask
fn identifiers(database: &ImportedDatabase) -> Vec<(String, u64)> {
    let mut held = database.tree_identifiers();
    held.sort();
    held
}

/// A writer and a reader agree about every tree's identifier.
#[test]
fn the_identifier_a_writer_used_is_the_one_a_reader_derives() {
    let path = scratch("writer-and-reader");
    let written = {
        let mut database =
            ImportedDatabase::create(path.clone(), PAGE_SIZE, FRAMES).expect("a fresh database");
        for statement in [
            "CREATE TABLE alpha (id INTEGER PRIMARY KEY, body TEXT)",
            "CREATE TABLE bravo (id INTEGER PRIMARY KEY, kind INTEGER)",
            "CREATE INDEX alpha_by_body ON alpha(body)",
            "CREATE INDEX bravo_by_kind ON bravo(kind)",
            "INSERT INTO alpha(id, body) VALUES (1, 'one')",
            "INSERT INTO bravo(id, kind) VALUES (1, 7)",
        ] {
            database
                .execute_any(statement, &Params::new())
                .unwrap_or_else(|error| {
                    panic!("{statement}: {}", error.detail().unwrap_or_default())
                });
        }
        let held = identifiers(&database);
        database.checkpoint().expect("the database checkpoints");
        held
    };

    assert!(
        written.len() >= 4,
        "the writer should hold four objects, it held {written:?}"
    );

    let reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(
        identifiers(&reopened),
        written,
        "the reader derived different tree identifiers from the writer's"
    );
}

/// No object carries a zero or a duplicate identifier.
///
/// Zero is what a row written before this existed reads as, and `open` refuses
/// it rather than guessing; a duplicate would send two trees' row records to one
/// tree. Both are cheap to check and neither is checkable from a query.
#[test]
fn every_identifier_is_present_and_distinct() {
    let path = scratch("distinct");
    let mut database = ImportedDatabase::create(path, PAGE_SIZE, FRAMES).expect("a fresh database");
    for statement in [
        "CREATE TABLE one (id INTEGER PRIMARY KEY, a TEXT)",
        "CREATE TABLE two (id INTEGER PRIMARY KEY, b TEXT)",
        "CREATE INDEX one_by_a ON one(a)",
        "CREATE INDEX two_by_b ON two(b)",
    ] {
        database
            .execute_any(statement, &Params::new())
            .unwrap_or_else(|error| panic!("{statement}: {}", error.detail().unwrap_or_default()));
    }

    let held = identifiers(&database);
    let mut seen = HashSet::new();
    for (name, id) in &held {
        assert_ne!(*id, 0, "{name} carries no identifier");
        assert!(seen.insert(*id), "{name} reuses identifier {id}");
    }
    assert!(held.len() >= 4, "four objects expected, got {held:?}");
}

/// An imported database's identifiers survive a close and an open too.
///
/// The import numbers by the source file's root pages, which is a different
/// numbering from DDL's — so this is the second of the three that had to
/// collapse onto one derivation, checked the same way.
#[test]
fn an_imported_database_keeps_its_identifiers() {
    let fixture = workspace_root().join("compat/fixtures/basic-p4096-utf8.db");
    if !fixture.is_file() {
        inillucent_compat::differential::skipping("the fixture corpus is not checked in");
        return;
    }
    let path = scratch("imported");
    inillucent_base::testing::remove_database(&path);
    let written = {
        let mut database = ImportedDatabase::import_into(fixture, path.clone(), PAGE_SIZE, FRAMES)
            .expect("the fixture imports");
        let held = identifiers(&database);
        database.checkpoint().expect("the database checkpoints");
        held
    };
    assert!(
        !written.is_empty(),
        "the fixture should have imported some objects"
    );
    for (name, id) in &written {
        assert_ne!(*id, 0, "{name} carries no identifier after an import");
    }

    let reopened = ImportedDatabase::open(path, PAGE_SIZE, FRAMES).expect("the file opens");
    assert_eq!(
        identifiers(&reopened),
        written,
        "an imported database's identifiers changed across a close and an open"
    );
}
