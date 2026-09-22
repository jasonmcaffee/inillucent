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
use inillucent_tree::datum::OwnedDatum;

use crate::workspace_root;

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

/// The consumer's own statements, read from the corpus they were extracted to.
///
/// **Shared because two things read it** (task-2066 §4.3.11).
/// `story_workload_replay` replays these 211 statements for correctness and
/// `inillucent-workloadperf` times them, and a format with two readers is a
/// format whose readers drift. The parser lived in the story until the second
/// reader existed.
/// One statement out of the workload file.
pub struct Statement {
    /// Where it is in Nikaya's source, which is also its allow list key.
    pub source: String,
    /// The SQL.
    pub sql: String,
    /// One value per placeholder.
    pub params: Vec<OwnedDatum>,
}

/// Reads the workload file: the schema, and the statements with their values.
///
/// The format is two `-- section:` markers and then one block per statement,
/// described in the file's own header. A parse that finds nothing is a failure
/// rather than an empty run, because an empty corpus passes every assertion
/// below (rule 1.2).
pub fn workload() -> (String, Vec<Statement>) {
    let path = workspace_root().join("tests/workloads/nikaya/statements.sql");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|why| panic!("{}: {why}", path.display()));

    // **A marker is a whole line, and this is the second version of that.** The
    // first split on the text `-- section: schema` anywhere, and the file's own
    // header describes the format - so the split landed inside the sentence
    // explaining the marker and handed the schema builder a paragraph of prose,
    // which the engine then reported as a syntax error near a backtick.
    let schema: String = text
        .lines()
        .skip_while(|line| line.trim() != "-- section: schema")
        .take_while(|line| line.trim() != "-- section: statements")
        .skip(1)
        .collect::<Vec<&str>>()
        .join("\n");
    assert!(
        schema.contains("CREATE TABLE"),
        "{} has no `-- section: schema` line, or nothing under it",
        path.display()
    );
    let statements_part: String = text
        .lines()
        .skip_while(|line| line.trim() != "-- section: statements")
        .skip(1)
        .collect::<Vec<&str>>()
        .join("\n");
    assert!(
        statements_part.contains("-- statement:"),
        "{} has no `-- section: statements` line, or nothing under it",
        path.display()
    );

    let mut statements: Vec<Statement> = Vec::new();
    let mut source = String::new();
    let mut params: Vec<OwnedDatum> = Vec::new();
    let mut sql = String::new();
    for line in statements_part.lines() {
        if let Some(rest) = line.strip_prefix("-- statement: ") {
            if !sql.trim().is_empty() {
                statements.push(Statement {
                    source: std::mem::take(&mut source),
                    sql: std::mem::take(&mut sql).trim().to_string(),
                    params: std::mem::take(&mut params),
                });
            }
            source = rest.trim().to_string();
            sql.clear();
            params.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix("-- params: ") {
            params = parse_params(rest.trim(), &source);
            continue;
        }
        if line.starts_with("-- ") {
            continue;
        }
        sql.push_str(line);
        sql.push('\n');
    }
    if !sql.trim().is_empty() {
        statements.push(Statement {
            source,
            sql: sql.trim().to_string(),
            params,
        });
    }

    assert!(
        statements.len() >= 100,
        "read {} statements out of {}, which means the format is being parsed wrongly rather \
         than that Nikaya has almost no SQL in it",
        statements.len(),
        path.display()
    );
    (schema, statements)
}

/// Reads a `-- params:` line, which is a JSON array of strings and numbers.
///
/// Hand written rather than taken from a JSON reader, because the whole of what
/// is in these arrays is a quoted string or an integer, and the values are
/// written by `tools/extract-nikaya-workload.py` rather than by a person.
///
/// **It splits on the commas between values, not on every comma.** Nikaya binds
/// an embedding as a string that is itself a bracketed list -
/// `"[0.1,0.2,0.3,0.4]"` is one parameter of
/// `repositories/chunks.rs:115` - and a split on every comma turned that one
/// value into four fragments, the first of them `"[0.1`, which is neither a
/// string nor an integer. The parameter a consumer actually binds is the thing
/// this file exists to replay, so the reader has to keep it whole.
///
/// @param text - the array, as it appears in the file
/// @param source - which statement it belongs to, for the failure message
pub fn parse_params(text: &str, source: &str) -> Vec<OwnedDatum> {
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("{source}: `{text}` is not a JSON array"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    split_values(inner)
        .iter()
        .map(|item| {
            let item = item.trim();
            match item
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
            {
                Some(text) => OwnedDatum::Text(text.as_bytes().to_vec()),
                None => match item.parse::<i64>() {
                    Ok(number) => OwnedDatum::Int(number),
                    Err(_) => panic!("{source}: `{item}` is neither a string nor an integer"),
                },
            }
        })
        .collect()
}

/// Splits an array's body on the commas that separate its values.
///
/// A comma inside a quoted string belongs to the string. There is no escaping
/// to handle: `tools/extract-nikaya-workload.py` writes the file and refuses a
/// value holding a quote, so a `"` here always opens or closes one.
///
/// @param inner - the text between the array's brackets
fn split_values(inner: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in inner.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            ',' if !quoted => {
                values.push(current.clone());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    values.push(current);
    values
}

/// Fills the consumer's tables with the rows its statements run against.
///
/// Moved here beside [`workload`] for the same reason (task-2066 §4.3.11): the
/// story replays the corpus and `inillucent-workloadperf` times it, and a
/// second copy of rows shaped to satisfy somebody else's `NOT NULL`s, `CHECK`s
/// and foreign keys is a copy that drifts from the schema it was written for.
///
/// @param connection - a connection whose schema is the corpus's own
/// Writes a handful of rows into every table the schema declares.
///
/// The values match what `tools/extract-nikaya-workload.py` binds - `row-0001`
/// for a text column and `1` for an integer one - so a statement whose
/// predicate is `= ?1` matches a row rather than nothing. A statement that
/// matched nothing would still be a real test of the binder and the planner,
/// which is what this suite is about, but it would be a weaker test of the read
/// path and it would not exercise a large value at all.
///
/// @param connection - the connection to write through
pub fn seed_the_workload(connection: &Connection<'_>) {
    for table in TABLES {
        run(connection, table);
    }
}

/// The rows every statement runs against.
///
/// Written out rather than generated, because the columns they have to satisfy
/// are Nikaya's `NOT NULL`s, `CHECK`s and foreign keys: a generator would be a
/// second copy of the schema, kept in step by hand.
const TABLES: &[&str] = &[
    "INSERT INTO source_account VALUES ('row-0001', 'gmail', 'row-0001', 'row-0001', \
     'authorized', 1, 1);",
    "INSERT INTO gmail_sync_state VALUES ('row-0001', 1, 'row-0001', 1, 1, 1, 1, 1);",
    "INSERT INTO document VALUES ('row-0001', 'row-0001', 'email', 'row-0001', 'row-0001', \
     'row-0001', 'row-0001', NULL, 'row-0001', 1, NULL, 1, 1, 1);",
    "INSERT INTO email_message VALUES ('row-0001', 'row-0001', 'row-0001', 'row-0001', \
     'row-0001', 'row-0001', 1, '[]', 'row-0001', 1, 1, 0);",
    "INSERT INTO email_participant (document_id, kind, display_name, address, ordinal) \
     VALUES ('row-0001', 'from', 'row-0001', 'row-0001', 1);",
    "INSERT INTO attachment VALUES ('row-0001', 'row-0001', 'row-0001', 1, 'row-0001', \
     'row-0001', 'extracted', 'row-0001', 'row-0001', NULL, 1, 1, '{}');",
    "INSERT INTO message_attachment (document_id, attachment_id, gmail_part_id, \
     gmail_attachment_id, content_id, original_filename, is_inline, ordinal) \
     VALUES ('row-0001', 'row-0001', 'row-0001', 'row-0001', 'row-0001', 'row-0001', 0, 1);",
    "INSERT INTO attachment_download_queue (document_id, gmail_message_id, gmail_part_id, \
     gmail_attachment_id, original_filename, mime_type, content_id, is_inline, declared_size, \
     ordinal, status, attempts, error_summary, created_at) \
     VALUES ('row-0001', 'row-0001', 'row-0001', 'row-0001', 'row-0001', 'row-0001', \
     'row-0001', 0, 1, 1, 'queued', 0, NULL, 1);",
    "INSERT INTO chunk VALUES ('row-0001', 'row-0001', 1, 'row-0001', 'row-0001', 'row-0001', \
     1, 'row-0001', 'row-0001', 1, 0, NULL, NULL);",
    "INSERT INTO chunk_embedding VALUES ('row-0001', 'row-0001', 4, '[0.1,0.2,0.3,0.4]', 1);",
    "INSERT INTO sync_job (source_account_id, job_type, status, counters, checkpoint, attempts, \
     error_summary, started_at, finished_at, created_at, updated_at) \
     VALUES ('row-0001', 'row-0001', 'queued', '{}', 'row-0001', 0, NULL, 1, 1, 1, 1);",
    "INSERT INTO document_meta VALUES ('row-0001', 'email', 'row-0001', 'row-0001', '[]', \
     'row-0001', 0, 1);",
    "INSERT INTO app_user VALUES ('row-0001', 'row-0001', 'owner', 'row-0001', 1, 1);",
    "INSERT INTO app_session VALUES ('row-0001', 'row-0001', 1, 1, 1);",
    "INSERT INTO agent_session VALUES ('row-0001', 'row-0001', 1, 1);",
    "INSERT INTO embedding_queue VALUES ('row-0001', 1);",
    "INSERT INTO schema_migration VALUES ('row-0001', 1);",
];
