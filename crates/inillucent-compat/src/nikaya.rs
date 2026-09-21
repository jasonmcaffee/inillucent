//! Nikaya's corpus, in miniature, for the stories that run its startup.
//!
//! Invariant: **the schema and the sequence are copied from
//! `C:/jason/dev/nikaya/server`, and none of its data is.** What makes a
//! consumer story worth having is that it is the consumer's program; a schema
//! invented here to look like Nikaya's would be a schema nobody runs. What
//! makes it safe to check in is that the values are generated - Nikaya's corpus
//! is private mail.
//!
//! Two suites build this corpus: `crates/inillucent/tests/story_nikaya.rs`,
//! which runs the startup migration in process at every arm, and
//! `crates/inillucent-compat/tests/story_nikaya_crash.rs`, which kills a real
//! shell between the `ALTER TABLE` and the `CREATE INDEX` that follows it. They
//! read the same rows back, so the two failures are comparable.

// **This module may panic**, for the reason `stories.rs` beside it may: it is a
// helper every story uses, it lives in `src/` so the crate's test-only
// relaxation does not reach it, and what it panics on is a statement that will
// not run, with the statement in the message.
#![allow(clippy::panic)]

use inillucent_engine::connect::Connection;

use crate::stories::{ask, body, run, vector};

/// The three tables Nikaya's corpus is, trimmed to the columns a story reads
/// back.
///
/// Copied from `migrations/inillucent/001_init.sql`. `document.body_text` is
/// what carries the large values: a message body there runs from a line to
/// forty kilobytes, which is either side of the extent threshold at 4,096 byte
/// pages and at 32,768 byte pages both.
pub const CORPUS_SCHEMA: &str = "\
CREATE TABLE document (\
  id                TEXT PRIMARY KEY,\
  source_account_id TEXT NOT NULL,\
  kind              TEXT NOT NULL CHECK (kind IN ('email', 'attachment')),\
  provider_id       TEXT NOT NULL,\
  title             TEXT NOT NULL DEFAULT '',\
  body_text         TEXT NOT NULL DEFAULT '',\
  content_hash      TEXT NOT NULL,\
  source_timestamp  INTEGER,\
  indexed_at        INTEGER,\
  created_at        INTEGER NOT NULL,\
  updated_at        INTEGER NOT NULL,\
  UNIQUE (source_account_id, kind, provider_id)\
);\
CREATE INDEX document_source_timestamp_idx ON document (source_timestamp DESC);\
CREATE INDEX document_kind_idx ON document (kind);\
CREATE TABLE chunk (\
  id                 TEXT PRIMARY KEY,\
  document_id        TEXT NOT NULL REFERENCES document(id) ON DELETE CASCADE,\
  ordinal            INTEGER NOT NULL,\
  content            TEXT NOT NULL,\
  heading            TEXT NOT NULL DEFAULT '',\
  char_count         INTEGER NOT NULL,\
  content_hash       TEXT NOT NULL,\
  chunker_version    TEXT NOT NULL,\
  created_at         INTEGER NOT NULL,\
  embedding_attempts INTEGER NOT NULL DEFAULT 0,\
  UNIQUE (document_id, ordinal)\
);\
CREATE INDEX chunk_document_idx ON chunk (document_id);\
CREATE TABLE chunk_embedding (\
  chunk_id        TEXT NOT NULL REFERENCES chunk(id) ON DELETE CASCADE,\
  embedding_model TEXT NOT NULL,\
  dimension       INTEGER NOT NULL,\
  embedding       VECTOR(16) NOT NULL,\
  indexed_at      INTEGER NOT NULL,\
  PRIMARY KEY (chunk_id, embedding_model)\
);";

/// The migration ledger `db.rs` creates before it can read it.
pub const LEDGER: &str = "CREATE TABLE IF NOT EXISTS schema_migration (\
  name       TEXT PRIMARY KEY,\
  applied_at INTEGER NOT NULL\
)";

/// The model name Nikaya's vectors are written under.
pub const MODEL: &str = "bge-small-en-v1.5";

/// How wide the vectors are here.
///
/// Nikaya's are 768. Sixteen is enough for the column to be a `VECTOR(N)` with
/// its width checked on write, which is what the stories assert about it, and
/// 768 floats a row times a few thousand rows is a fixture rather than a seed.
pub const WIDTH: usize = 16;

/// Writes the corpus: documents, their chunks, and a vector per chunk.
///
/// **Written a batch at a time rather than a statement at a time.** One
/// `execute_batch` per row compiles a statement per row, and with the bodies
/// this corpus carries that was most of the story's time. The rows are the same
/// rows; what changed is how many compiles they cost.
///
/// @param connection - the connection to write through
/// @param documents - how many documents to write
pub fn seed(connection: &Connection<'_>, documents: usize) {
    run(connection, CORPUS_SCHEMA);
    run(connection, "BEGIN");
    let mut batch = String::new();
    for number in 0..documents {
        // One body in five is large enough to leave the leaf, which is what
        // makes this a corpus rather than a table of short rows: the migration
        // that corrupted a neighbour corrupted one whose values were out of
        // line.
        let size = match number % 5 {
            0 => 40_000,
            1 => 4_500,
            _ => 180,
        };
        batch.push_str(&format!(
            "INSERT INTO document VALUES ('doc-{number:05}', 'account-1', 'email', \
             'provider-{number:05}', 'subject {number}', '{}', 'hash-{number:05}', {}, NULL, \
             {}, {});",
            body(number, size).replace('\'', ""),
            1_700_000_000 + number as i64,
            1_700_000_000,
            1_700_000_000
        ));
        for ordinal in 0..2 {
            let chunk = format!("chunk-{number:05}-{ordinal}");
            batch.push_str(&format!(
                "INSERT INTO chunk VALUES ('{chunk}', 'doc-{number:05}', {ordinal}, \
                 '{}', 'heading {ordinal}', 240, 'chunkhash-{number:05}-{ordinal}', \
                 'v3', {}, 0);",
                body(number * 7 + ordinal, 240).replace('\'', ""),
                1_700_000_000
            ));
            batch.push_str(&format!(
                "INSERT INTO chunk_embedding VALUES ('{chunk}', '{MODEL}', {WIDTH}, \
                 '{}', {});",
                vector(number * 7 + ordinal, WIDTH),
                1_700_000_000
            ));
        }
        if number % 16 == 15 {
            run(connection, &batch);
            batch.clear();
        }
    }
    if !batch.is_empty() {
        run(connection, &batch);
    }
    run(connection, "COMMIT");
}

/// Reads every column of every table back, as one block of text.
///
/// **`count(*)` is never the assertion, and that is the whole point.** The table
/// `93c7261` corrupted answered `count(*)` correctly while its rows were
/// unreadable, so what this returns is every column of every row - the bodies
/// as their lengths and their first and last runs of bytes, because forty
/// kilobytes of text is not something a failure message can carry and a length
/// that matches with bytes that do not is the failure a length alone misses.
///
/// @param connection - the connection to read through
pub fn every_column(connection: &Connection<'_>) -> String {
    let documents = ask(
        connection,
        "SELECT id, source_account_id, kind, provider_id, title, length(body_text), \
         substr(body_text, 1, 24), substr(body_text, -24), content_hash, source_timestamp, \
         indexed_at, created_at, updated_at FROM document ORDER BY id",
    );
    let chunks = ask(
        connection,
        "SELECT id, document_id, ordinal, content, heading, char_count, content_hash, \
         chunker_version, created_at, embedding_attempts FROM chunk ORDER BY id",
    );
    let vectors = ask(
        connection,
        "SELECT chunk_id, embedding_model, dimension, indexed_at, \
         round(vector_distance_l2(embedding, embedding), 6) FROM chunk_embedding \
         ORDER BY chunk_id, embedding_model",
    );
    format!("documents:\n{documents}\nchunks:\n{chunks}\nvectors:\n{vectors}")
}
