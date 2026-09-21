//! A retrieval application's whole lifecycle, at every arm.
//!
//! Invariant: **after every phase of an ingest-and-search lifecycle, what the
//! index answers is what a brute force computation over the test's own model of
//! the documents answers.** Ingest, query, update a third, delete a third, roll
//! a batch back, checkpoint, reopen, query again - and the comparison is made
//! after each one, not only at the end.
//!
//! ## What is brute forced, and what is not
//!
//! Three questions have an exact answer the test can compute, and those are the
//! ones asserted:
//!
//! - **which documents match a term.** A term either appears in a document's
//!   text or it does not, and the model knows which. The set the index returns
//!   has to be that set.
//! - **which documents are nearest a vector.** The `VECTOR(N)` column carries no
//!   HNSW index here, so `ORDER BY vector_distance_cos(...)` is an exhaustive
//!   scan and therefore exact - which is the same reason Nikaya leaves its own
//!   vectors unindexed, written down in `003_embedded_flag.sql`'s neighbour.
//!   The test computes cosine distance over the model and compares the order.
//! - **which documents exist.** Every phase ends by reading the whole corpus
//!   back and comparing it to the model, column by column.
//!
//! What is *not* asserted is BM25's ranking against a second BM25 written here.
//! A ranking function compared against a reimplementation of itself grades the
//! reimplementation; `crates/inillucent-compat/tests/fts5_parity.rs` grades the
//! ranking against the pinned SQLite, which is a reference rather than a copy.
//! What this file adds is that the ranking is **stable across a reopen** and
//! that every row it returns is a live document, which is the failure a
//! lifecycle produces and a single-phase test cannot see.
//!
//! ## Why the counts are what they are
//!
//! task-2033 refuses on **row 42 of 200** at a 4,096 byte page, so a few hundred
//! documents is past the point where the defect appears and past the point
//! where an FTS5 leaf splits. The `nightly` form runs 2,500, which is where the
//! TDD set it.

use std::collections::BTreeMap;
use std::path::Path;

use inillucent_compat::matrix::{Arm, Scale};
use inillucent_compat::scenario;
use inillucent_compat::stories::{ask, open, reopen_and_check, run, vector};
use inillucent_compat::workspace_root;
use inillucent_engine::connect::Connection;

/// How wide a vector is here.
///
/// Sixteen rather than a real model's 768: what this story asserts about a
/// vector is that the nearest ones come back in the right order, and the order
/// is decided by the same arithmetic at any width.
const WIDTH: usize = 16;

/// The terms a document's body is built from.
///
/// Chosen so that each one appears in a known fraction of the corpus - `ledger`
/// in every document, `segment` in every second, and so on down - because what
/// the lexical assertion compares is a *set*, and a term that matched
/// everything or nothing would compare an empty set to an empty set.
const TERMS: [(&str, usize); 5] = [
    ("ledger", 1),
    ("segment", 2),
    ("extent", 3),
    ("catalog", 7),
    ("rarity", 97),
];

/// The test's own view of the corpus: what should be in the index.
type Model = BTreeMap<i64, Document>;

/// One document, as the model holds it.
#[derive(Clone, PartialEq, Debug)]
struct Document {
    /// The title.
    title: String,
    /// The body, which is the terms this document carries.
    body: String,
    /// The vector, as the JSON array the column is written with.
    vector: String,
}

/// Builds the document numbered `id`.
///
/// @param id - the document's rowid
fn document(id: i64) -> Document {
    let mut body = String::new();
    for (term, every) in TERMS {
        if id as usize % every == 0 {
            body.push_str(term);
            body.push(' ');
        }
    }
    body.push_str(&format!("number{id}"));
    Document {
        title: format!("note {id}"),
        body,
        vector: vector(id as usize, WIDTH),
    }
}

/// The schema: an FTS5 index, a hybrid index, and the rows themselves.
///
/// Three tables because a retrieval application has three: the rows it owns,
/// the keyword index over them, and the vector index. They are written in one
/// transaction per phase, which is what makes a rollback able to take all three
/// back together.
const SCHEMA: &str = "\
CREATE TABLE doc (\
  id    INTEGER PRIMARY KEY,\
  title TEXT NOT NULL,\
  body  TEXT NOT NULL,\
  v     VECTOR(16) NOT NULL\
);\
CREATE VIRTUAL TABLE documents USING fts5(title, body);\
CREATE VIRTUAL TABLE hybrid USING inillucent_search(title, body);";

/// Writes one document into all three tables.
///
/// @param model - the test's own view, updated in step
/// @param id - the document's rowid
fn insert_into(model: &mut Model, id: i64) -> String {
    let made = document(id);
    let statement = format!(
        "INSERT INTO doc VALUES ({id}, '{}', '{}', '{}');\
         INSERT INTO documents(rowid, title, body) VALUES ({id}, '{}', '{}');\
         INSERT INTO hybrid(rowid, title, body) VALUES ({id}, '{}', '{}');",
        made.title, made.body, made.vector, made.title, made.body, made.title, made.body
    );
    model.insert(id, made);
    statement
}

/// Reads the whole corpus back and renders it the way the model renders.
///
/// @param connection - the connection to read through
fn corpus(connection: &Connection<'_>) -> String {
    ask(connection, "SELECT id, title, body FROM doc ORDER BY id")
}

/// Renders the model the way `corpus` renders the table.
///
/// @param model - the test's own view
fn modelled(model: &Model) -> String {
    model
        .iter()
        .map(|(id, made)| format!("{id},{},{}", made.title, made.body))
        .collect::<Vec<String>>()
        .join("\n")
}

/// The documents the model says carry a term.
///
/// @param model - the test's own view
/// @param term - the term
fn matching(model: &Model, term: &str) -> Vec<i64> {
    model
        .iter()
        .filter(|(_, made)| made.body.split_whitespace().any(|word| word == term))
        .map(|(id, _)| *id)
        .collect()
}

/// The rowids an FTS5 `MATCH` returns, sorted.
///
/// Sorted because what is compared here is the *set*: the ranking is graded
/// against the pinned SQLite in `fts5_parity.rs`, and grading it here against
/// arithmetic written in this file would be grading this file.
///
/// @param connection - the connection to ask
/// @param term - the term
fn matched(connection: &Connection<'_>, term: &str) -> Vec<i64> {
    let said = ask(
        connection,
        &format!("SELECT rowid FROM documents WHERE documents MATCH '{term}' ORDER BY rowid"),
    );
    said.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.parse::<i64>().ok())
        .collect()
}

/// Cosine distance between two vectors written as JSON arrays.
///
/// The same arithmetic `vector_distance_cos` computes, written here so the
/// order the engine returns can be compared to an order this file derived. Both
/// vectors are normalised, so this is `1 - cos`.
///
/// @param left - one JSON array
/// @param right - the other
fn cosine(left: &str, right: &str) -> f64 {
    let read = |text: &str| -> Vec<f64> {
        text.trim_matches(|letter| letter == '[' || letter == ']')
            .split(',')
            .filter_map(|part| part.trim().parse::<f64>().ok())
            .collect()
    };
    let (one, other) = (read(left), read(right));
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (a, b) in one.iter().zip(other.iter()) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    let scale = (left_norm.sqrt() * right_norm.sqrt()).max(f64::MIN_POSITIVE);
    1.0 - dot / scale
}

/// The rowids nearest a vector, in order, according to the model.
///
/// @param model - the test's own view
/// @param probe - the query vector, as a JSON array
/// @param top - how many to return
fn nearest(model: &Model, probe: &str, top: usize) -> Vec<i64> {
    let mut scored: Vec<(f64, i64)> = model
        .iter()
        .map(|(id, made)| (cosine(&made.vector, probe), *id))
        .collect();
    // By distance, then by rowid, which is what the engine's own ORDER BY says
    // when two distances are equal.
    scored.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.1.cmp(&right.1))
    });
    scored.into_iter().take(top).map(|(_, id)| id).collect()
}

/// The rowids the engine says are nearest, in order.
///
/// @param connection - the connection to ask
/// @param probe - the query vector, as a JSON array
/// @param top - how many to return
fn nearest_in_the_file(connection: &Connection<'_>, probe: &str, top: usize) -> Vec<i64> {
    let said = ask(
        connection,
        &format!("SELECT id FROM doc ORDER BY vector_distance_cos(v, '{probe}'), id LIMIT {top}"),
    );
    said.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.parse::<i64>().ok())
        .collect()
}

/// Every question this story knows the answer to, asked and checked.
///
/// @param connection - the connection to ask
/// @param model - the test's own view
/// @param arm - the configuration this run is at
/// @param phase - which phase has just finished, for the failure message
fn agrees_with_the_model(connection: &Connection<'_>, model: &Model, arm: &Arm, phase: &str) {
    assert_eq!(
        corpus(connection),
        modelled(model),
        "after {phase} at the {} arm, the rows are not the model's rows",
        arm.name
    );
    for (term, _) in TERMS {
        assert_eq!(
            matched(connection, term),
            matching(model, term),
            "after {phase} at the {} arm, the documents matching `{term}` are not the ones that \
             carry it",
            arm.name
        );
    }
    let probe = vector(7, WIDTH);
    assert_eq!(
        nearest_in_the_file(connection, &probe, 20),
        nearest(model, &probe, 20),
        "after {phase} at the {} arm, the twenty nearest vectors are not the twenty nearest",
        arm.name
    );
    // The hybrid index answers over the same documents. Its ranking is its own,
    // so what is asserted is that every row it returns is a document that is
    // still there - the failure a delete phase produces is a hit on a row that
    // has gone.
    let fused = ask(
        connection,
        "SELECT rowid FROM hybrid('ledger', 20) ORDER BY rank",
    );
    let returned: Vec<i64> = fused
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.parse::<i64>().ok())
        .collect();
    assert!(
        !returned.is_empty(),
        "after {phase} at the {} arm, the hybrid index answered nothing for a term every \
         document carries",
        arm.name
    );
    for id in &returned {
        assert!(
            model.contains_key(id),
            "after {phase} at the {} arm, the hybrid index returned document {id}, which is not \
             in the corpus any more",
            arm.name
        );
    }
}

/// Whether task-2033's row is allow listed, and against what.
///
/// The same arrangement `differential_part8.rs` has: one line, a tab, the
/// ticket. **An entry that no longer describes a failure is itself a failure**,
/// so when task-2033 lands this story goes red until the line is removed.
fn allow_listed(key: &str) -> Option<String> {
    let path = workspace_root().join("tests/workloads/rag/allow.list");
    let text = std::fs::read_to_string(&path).ok()?;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((listed, said)) = line.split_once('\t') else {
            panic!("{}: an allow line has no tab: {line}", path.display());
        };
        if listed.trim() == key {
            return Some(said.trim().to_string());
        }
    }
    None
}

/// Fails when an allow listed arm did not fail.
///
/// **This is the half that makes the list worth having.** A list that only
/// grows is a list of things nobody will take off it, so an entry that no
/// longer describes a failure is itself a failure - and when task-2033 lands,
/// this is what turns red and says which lines to delete.
///
/// @param arm - the configuration this run is at
/// @param story - the story's name, which with the arm is the allow list key
fn refuse_to_be_allow_listed_for_nothing(arm: &Arm, story: &str) {
    let key = format!("{story}::{}", arm.test_name());
    if let Some(said) = allow_listed(&key) {
        panic!(
            "`{key}` is in tests/workloads/rag/allow.list against `{said}` and the story ran to \
             the end without being refused. Delete the line: a fixed defect left listed reads \
             as coverage and is not."
        );
    }
}

/// How many documents are written one statement at a time before the batches
/// start.
///
/// Past 42, which is the row task-2033 refuses on, so the defect is inside the
/// part of the ingest that reproduces it rather than just past the end of it.
const AUTOCOMMITTED: i64 = 80;

/// Either records the refusal against its ticket, or fails with what it was.
///
/// **The allow list is read rather than the refusal being tolerated.** A story
/// that caught the error and carried on would be a story that passes on a
/// broken engine; a story that has no entry fails and prints what to write.
///
/// @param arm - the configuration this run is at
/// @param id - the document the ingest was refused on
/// @param why - the refusal
fn refused_or_allow_listed(arm: &Arm, story: &str, id: i64, why: &inillucent_base::DbError) {
    let key = format!("{story}::{}", arm.test_name());
    match allow_listed(&key) {
        Some(said) => println!(
            "{key} refused an ordinary FTS5 insert on document {id}: {} ({:?}); allow listed \
             against {said}",
            why.message(),
            why.code()
        ),
        None => panic!(
            "ingesting document {id} at the {} arm was refused: {} ({:?}). This is task-2033's \
             shape - an ordinary FTS5 insert declining at a page size the engine builds at. If \
             it is back, add `{key}` to tests/workloads/rag/allow.list with the ticket that \
             owns it.",
            arm.name,
            why.message(),
            why.code()
        ),
    }
}

/// Ingest, search, update, delete, roll back, checkpoint, reopen, search.
///
/// **This is where task-2033 first fails**, at the `sqlite-page` and
/// `small-pool` arms: an ordinary FTS5 insert refuses with `SQLITE_CORRUPT` on
/// row 42 at a 4,096 byte page. The refusal is caught and checked against
/// `tests/workloads/rag/allow.list`, so the story records the defect rather
/// than being deleted around it - and the moment the engine stops refusing, the
/// allow list entry is what makes this red.
fn ingest_and_search(arm: &Arm, area: &Path) {
    let path = area.join("rag.rdb");
    let documents = Scale::from_env().pick(300, 2_500) as i64;
    let mut model: Model = Model::new();

    let database = open(arm, &path);
    let connection = database.session();
    run(&connection, SCHEMA);

    // Phase one: ingest. **Two ways, and the difference matters.** The first
    // `AUTOCOMMITTED` documents go in one statement at a time, which is what an
    // application does when a document arrives and is written; the rest go in
    // batches of fifty inside a transaction, which is what a bulk pass does.
    //
    // The two are different write paths through FTS5's shadow tables - a
    // transaction's worth of inserts flushes its delta once, and a statement's
    // worth flushes per statement - and **task-2033 is only in the first of
    // them.** Written entirely in batches this story passed at every arm and
    // reported nothing, which is why both are here.
    for id in 1..=AUTOCOMMITTED.min(documents) {
        let statement = insert_into(&mut model, id);
        if let Err(why) = connection.execute_batch(&statement) {
            refused_or_allow_listed(arm, "ingest_and_search", id, &why);
            return;
        }
    }
    let mut batch = String::from("BEGIN;");
    for id in (AUTOCOMMITTED + 1)..=documents {
        batch.push_str(&insert_into(&mut model, id));
        if id % 50 == 0 {
            batch.push_str("COMMIT;");
            if let Err(why) = connection.execute_batch(&batch) {
                refused_or_allow_listed(arm, "ingest_and_search", id, &why);
                return;
            }
            batch = String::from("BEGIN;");
        }
    }
    batch.push_str("COMMIT;");
    run(&connection, &batch);
    agrees_with_the_model(&connection, &model, arm, "the ingest");

    // Phase two: a third of the documents are rewritten, which is what an
    // application does when a source changes. The index has to lose the old
    // terms as well as gain the new ones.
    run(&connection, "BEGIN");
    for id in (1..=documents).step_by(3) {
        let replaced = Document {
            title: format!("revised {id}"),
            body: format!("revision rarity number{id}"),
            vector: vector(id as usize + 1_000, WIDTH),
        };
        run(
            &connection,
            &format!(
                "UPDATE doc SET title = '{}', body = '{}', v = '{}' WHERE id = {id};\
                 UPDATE documents SET title = '{}', body = '{}' WHERE rowid = {id};\
                 UPDATE hybrid SET title = '{}', body = '{}' WHERE rowid = {id};",
                replaced.title,
                replaced.body,
                replaced.vector,
                replaced.title,
                replaced.body,
                replaced.title,
                replaced.body
            ),
        );
        model.insert(id, replaced);
    }
    run(&connection, "COMMIT");
    agrees_with_the_model(&connection, &model, arm, "the update");

    // Phase three: a third are deleted. A hit on a deleted row is the failure
    // this phase exists to find.
    run(&connection, "BEGIN");
    for id in (2..=documents).step_by(3) {
        run(
            &connection,
            &format!(
                "DELETE FROM doc WHERE id = {id};\
                 DELETE FROM documents WHERE rowid = {id};\
                 DELETE FROM hybrid WHERE rowid = {id};"
            ),
        );
        model.remove(&id);
    }
    run(&connection, "COMMIT");
    agrees_with_the_model(&connection, &model, arm, "the delete");

    // Phase four: a batch of inserts is rolled back, so the index has to lose
    // what it staged. The model is not told about them at all.
    run(&connection, "BEGIN");
    let mut abandoned: Model = Model::new();
    for id in (documents + 1)..=(documents + 40) {
        run(&connection, &insert_into(&mut abandoned, id));
    }
    run(&connection, "ROLLBACK");
    agrees_with_the_model(&connection, &model, arm, "the rollback");

    // Phase five: a checkpoint, and then the same questions again.
    run(&connection, "PRAGMA wal_checkpoint");
    agrees_with_the_model(&connection, &model, arm, "the checkpoint");
    let ranked_before = ask(
        &connection,
        "SELECT rowid FROM hybrid('ledger', 20) ORDER BY rank",
    );

    // Phase six: a reopen, from a handle that wrote none of it.
    drop(connection);
    drop(database);
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    agrees_with_the_model(&connection, &model, arm, "the reopen");
    assert_eq!(
        ask(
            &connection,
            "SELECT rowid FROM hybrid('ledger', 20) ORDER BY rank"
        ),
        ranked_before,
        "the hybrid index ranks the same documents differently after a reopen, at the {} arm",
        arm.name
    );
    refuse_to_be_allow_listed_for_nothing(arm, "ingest_and_search");
}

scenario!(ingest_and_search, ingest_and_search);

/// An ordinary FTS5 ingest, exactly the shape task-2033 reports.
///
/// **Two hundred documents, one `INSERT` at a time, no other table.** This is
/// the ticket's own reproduction, run at every arm rather than at one page
/// size, and it is the narrowest form of what a retrieval application does
/// first. It is a separate story from `ingest_and_search` because narrowing was
/// what took the work: the lifecycle story writes three tables in one
/// statement, and interleaving writes to other trees changes which leaf an
/// FTS5 row lands on - so a lifecycle that passes says nothing about whether
/// the narrow shape does.
///
/// The text is the ticket's, word for word. Every document carries `lorem`,
/// `ipsum` and six more common terms, so by row 42 a common term's doclist is
/// large against a 4,096 byte leaf, which is the ticket's own reading of where
/// the refusal comes from.
fn an_ordinary_fts5_ingest(arm: &Arm, area: &Path) {
    let path = area.join("fts5.rdb");
    let documents = Scale::from_env().pick(200, 2_500) as i64;

    let database = open(arm, &path);
    let connection = database.session();
    run(
        &connection,
        "CREATE VIRTUAL TABLE documents USING fts5(title, body)",
    );

    for at in 0..documents {
        let statement = format!(
            "INSERT INTO documents(title, body) VALUES ('note {at}', \
             'lorem ipsum dolor sit amet number {at} consectetur adipiscing elit')"
        );
        if let Err(why) = connection.execute_batch(&statement) {
            refused_or_allow_listed(arm, "an_ordinary_fts5_ingest", at, &why);
            return;
        }
    }

    // The rows are all there, the index answers over them, and both survive a
    // reopen - which is what task-2033's own "what done looks like" asks for.
    assert_eq!(
        ask(&connection, "SELECT count(*) FROM documents"),
        documents.to_string(),
        "{documents} documents were ingested at the {} arm and fewer came back",
        arm.name
    );
    let matched_before = ask(
        &connection,
        "SELECT count(*) FROM documents WHERE documents MATCH 'lorem'",
    );
    assert_eq!(
        matched_before,
        documents.to_string(),
        "every document carries `lorem` and the index does not agree, at the {} arm",
        arm.name
    );
    // Row 42 in particular, because that is the row the refusal lands on: the
    // forty-second document is the one whose number is 41, and it has to be
    // both in the table and in the index.
    assert_eq!(
        ask(&connection, "SELECT title FROM documents WHERE rowid = 42"),
        "note 41",
        "document 42 - the one task-2033 refuses on - is not in the table, at the {} arm",
        arm.name
    );
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM documents WHERE documents MATCH 'consectetur'"
        ),
        documents.to_string(),
        "every document carries `consectetur` and the index does not agree, at the {} arm",
        arm.name
    );

    drop(connection);
    drop(database);
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    assert_eq!(
        ask(
            &connection,
            "SELECT count(*) FROM documents WHERE documents MATCH 'lorem'"
        ),
        matched_before,
        "the index answers differently after a reopen, at the {} arm",
        arm.name
    );
    refuse_to_be_allow_listed_for_nothing(arm, "an_ordinary_fts5_ingest");
}

scenario!(an_ordinary_fts5_ingest, an_ordinary_fts5_ingest);
