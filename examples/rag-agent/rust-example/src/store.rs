//! The database: its schema, and every SQL statement the server runs.
//!
//! Keeping all the SQL in one file means a reader can see the whole contract
//! between the server and inillucent in one place. The other modules call the
//! methods here and never write SQL themselves.
//!
//! ## Two tables hold the same vectors
//!
//! `chunk.v` is a plain `VECTOR(768)` column, searched with
//! `ORDER BY vector_distance_cos(...)`. `chunk_search` is an `inillucent_search`
//! table that holds each chunk's title, text and vector and ranks by keyword,
//! by vector, or by both in one query. `chunk.id` and `chunk_search.rowid` are
//! the same number. An application would normally pick one of the two. This
//! example keeps both so the README can compare them on the same data.
//!
//! ## One statement at a time
//!
//! [`SharedDatabase`] opens the database on a thread of its own and runs one
//! statement at a time from any number of threads. The MCP loop and the sync
//! thread both use it. A transaction holds the database for its whole length,
//! so the sync embeds a document's chunks before it opens the transaction that
//! writes them. See `sync.rs`.

use std::collections::HashMap;
use std::path::Path;

use inillucent::{Rows, SharedDatabase, Value};
use serde::Serialize;

use crate::config::DIMENSIONS;

/// The schema. Run once, when the database has no `document` table.
const SCHEMA: &str = "
CREATE TABLE document (
  id          INTEGER PRIMARY KEY,
  source_key  TEXT NOT NULL UNIQUE,
  title       TEXT NOT NULL,
  url         TEXT NOT NULL,
  body        TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  synced_at   TEXT NOT NULL
);
CREATE TABLE chunk (
  id          INTEGER PRIMARY KEY,
  document_id INTEGER NOT NULL,
  ordinal     INTEGER NOT NULL,
  start_byte  INTEGER NOT NULL,
  end_byte    INTEGER NOT NULL,
  text        TEXT NOT NULL,
  v           VECTOR(768)
);
CREATE INDEX chunk_document ON chunk (document_id, ordinal);
CREATE VIRTUAL TABLE chunk_search USING inillucent_search(title, text, document FACET, dims = 768);
CREATE TABLE sync_log (id INTEGER PRIMARY KEY, finished_at TEXT NOT NULL, report TEXT NOT NULL);
";

/// The database the server searches and the sync writes.
#[derive(Clone)]
pub struct Store {
    db: SharedDatabase,
}

/// What the sync needs to know about a stored document.
#[derive(Clone, Debug)]
pub struct StoredDocument {
    /// The key the source knows it by.
    pub key: String,
    /// The title, for the sync report.
    pub title: String,
    /// The row id in `document`.
    pub id: i64,
    /// The fingerprint written at the last sync.
    pub fingerprint: String,
}

/// A document ready to be written, with its chunks and their vectors.
pub struct DocumentWrite<'a> {
    /// The key the source knows it by.
    pub key: &'a str,
    /// The title.
    pub title: &'a str,
    /// The URL.
    pub url: &'a str,
    /// The normalised text the chunk offsets point into.
    pub body: &'a str,
    /// The fingerprint of the text and the settings.
    pub fingerprint: &'a str,
    /// When the sync wrote it.
    pub synced_at: &'a str,
    /// The chunks, each with its vector as little endian 32 bit floats.
    pub chunks: Vec<(crate::chunker::Chunk, Vec<u8>)>,
}

/// One chunk and the document it came from, as a search result shows it.
#[derive(Clone, Debug, Serialize)]
pub struct ChunkRow {
    /// The chunk's id, which `get_passage` takes.
    pub chunk_id: i64,
    /// The document's title.
    pub title: String,
    /// The document's URL.
    pub url: String,
    /// The chunk's text.
    pub text: String,
    /// The cosine distance from the question, when the caller passed the question's vector.
    pub distance: Option<f64>,
}

/// A chunk with its neighbours, as one span of the document.
#[derive(Clone, Debug, Serialize)]
pub struct Passage {
    /// The document's title.
    pub title: String,
    /// The document's URL.
    pub url: String,
    /// The chunks the span covers, first to last.
    pub chunk_ids: Vec<i64>,
    /// The span of the document text, with no text repeated.
    pub text: String,
}

/// One row of `list_documents`.
#[derive(Clone, Debug, Serialize)]
pub struct DocumentSummary {
    /// The title.
    pub title: String,
    /// The URL.
    pub url: String,
    /// How many chunks the document was cut into.
    pub chunks: i64,
    /// When the sync last wrote it.
    pub synced_at: String,
}

impl Store {
    /// Opens the database, creating the file and the schema when they are missing.
    ///
    /// @param path - the `.rdb` file
    pub fn open(path: &Path) -> Result<Store, String> {
        if let Some(folder) = path.parent().filter(|folder| !folder.as_os_str().is_empty()) {
            std::fs::create_dir_all(folder).map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
        }
        let db = SharedDatabase::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        let store = Store { db };
        store.ensure_schema()?;
        Ok(store)
    }

    /// Creates the tables the first time the database is opened.
    fn ensure_schema(&self) -> Result<(), String> {
        let found = self.query("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'document'", &[])?;
        if integer(&found, 0, 0) == 0 {
            self.db.execute_batch(SCHEMA).map_err(|error| format!("cannot create the schema: {error}"))?;
        }
        Ok(())
    }

    /// Embeds one text with `embed(TEXT)` and returns the vector's bytes.
    ///
    /// The vector comes back as a blob of 768 little endian 32 bit floats,
    /// which is the format a `VECTOR(768)` column stores. The caller binds the
    /// same blob to every statement that needs it, so each text is embedded
    /// once. The prefix is the caller's job.
    ///
    /// @param text - the text, with its `search_document: ` or `search_query: ` prefix
    pub fn embed(&self, text: &str) -> Result<Vec<u8>, String> {
        let rows = self.query("SELECT embed(?1)", &[Value::Text(text.to_string())])?;
        match rows.value(0, 0).and_then(Value::bytes) {
            Some(bytes) if bytes.len() == DIMENSIONS * 4 => Ok(bytes.to_vec()),
            Some(bytes) => Err(format!("embed(TEXT) returned {} bytes; expected {}", bytes.len(), DIMENSIONS * 4)),
            None => Err("embed(TEXT) returned no vector".to_string()),
        }
    }

    /// Returns every stored document by its source key.
    pub fn stored_documents(&self) -> Result<HashMap<String, StoredDocument>, String> {
        let rows = self.query("SELECT source_key, id, fingerprint, title FROM document", &[])?;
        let mut found = HashMap::new();
        for row in 0..rows.rows.len() {
            let document = StoredDocument {
                key: text(&rows, row, 0),
                title: text(&rows, row, 3),
                id: integer(&rows, row, 1),
                fingerprint: text(&rows, row, 2),
            };
            found.insert(document.key.clone(), document);
        }
        Ok(found)
    }

    /// Replaces a document and all its chunks in one transaction.
    ///
    /// A document that is already stored keeps its id. Its old chunks are
    /// deleted from both tables before the new ones are written, so a search
    /// sees either the old version or the new one and never a mixture.
    ///
    /// @param write - the document and its embedded chunks
    pub fn write_document(&self, write: &DocumentWrite) -> Result<(), String> {
        let tx = self.db.begin().map_err(|error| format!("cannot begin: {error}"))?;
        let existing = tx
            .query("SELECT id FROM document WHERE source_key = ?1", &[Value::Text(write.key.to_string())], 1)
            .map_err(|error| error.to_string())?;
        let id = match existing.value(0, 0) {
            Some(Value::Integer(id)) => {
                delete_chunks(&tx, *id)?;
                tx.execute("DELETE FROM document WHERE id = ?1", &[Value::Integer(*id)]).map_err(|e| e.to_string())?;
                *id
            }
            _ => next_id(&tx, "document")?,
        };
        tx.execute(
            "INSERT INTO document (id, source_key, title, url, body, fingerprint, synced_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &[
                Value::Integer(id),
                Value::Text(write.key.to_string()),
                Value::Text(write.title.to_string()),
                Value::Text(write.url.to_string()),
                Value::Text(write.body.to_string()),
                Value::Text(write.fingerprint.to_string()),
                Value::Text(write.synced_at.to_string()),
            ],
        )
        .map_err(|error| format!("cannot write document `{}`: {error}", write.title))?;
        insert_chunks(&tx, id, write)?;
        tx.commit().map_err(|error| format!("cannot commit `{}`: {error}", write.title))
    }

    /// Deletes a document and its chunks in one transaction.
    ///
    /// @param id - the document's row id
    pub fn delete_document(&self, id: i64) -> Result<(), String> {
        let tx = self.db.begin().map_err(|error| format!("cannot begin: {error}"))?;
        delete_chunks(&tx, id)?;
        tx.execute("DELETE FROM document WHERE id = ?1", &[Value::Integer(id)]).map_err(|e| e.to_string())?;
        tx.commit().map_err(|error| format!("cannot commit a delete: {error}"))
    }

    /// Returns how many documents and chunks are stored.
    pub fn counts(&self) -> Result<(i64, i64), String> {
        let rows = self.query("SELECT (SELECT count(*) FROM document), (SELECT count(*) FROM chunk)", &[])?;
        Ok((integer(&rows, 0, 0), integer(&rows, 0, 1)))
    }

    /// Returns how many rows each table holds and how many chunks have a vector.
    ///
    /// The tests use it to check that the two tables agree after a sync.
    pub fn table_counts(&self) -> Result<serde_json::Value, String> {
        let rows = self.query(
            "SELECT (SELECT count(*) FROM chunk), (SELECT count(v) FROM chunk), (SELECT count(*) FROM chunk_search),
                    (SELECT min(vector_dims(v)) FROM chunk), (SELECT max(vector_dims(v)) FROM chunk)",
            &[],
        )?;
        Ok(serde_json::json!({
            "chunk_rows": integer(&rows, 0, 0),
            "chunk_vectors": integer(&rows, 0, 1),
            "chunk_search_rows": integer(&rows, 0, 2),
            "narrowest_vector": integer(&rows, 0, 3),
            "widest_vector": integer(&rows, 0, 4),
        }))
    }

    /// Returns the nearest chunks by cosine distance over the plain column.
    ///
    /// This compares the question with every stored vector. With about 3,000
    /// chunks that takes a few milliseconds. `CREATE INDEX ... USING
    /// inillucent_hnsw (v)` would make the same query walk a graph instead;
    /// the README says when that is worth it.
    ///
    /// @param query - the question's vector
    /// @param k - how many chunks to return
    /// @param title - only chunks of the document with this title
    pub fn vector_hits(&self, query: &[u8], k: usize, title: Option<&str>) -> Result<Vec<(i64, f64)>, String> {
        let mut params = vec![Value::Blob(query.to_vec())];
        let filter = match title {
            Some(title) => {
                params.push(Value::Text(title.to_string()));
                "WHERE document_id IN (SELECT id FROM document WHERE title = ?2)"
            }
            None => "",
        };
        let sql = format!("SELECT id, vector_distance_cos(v, ?1) AS distance FROM chunk {filter} ORDER BY distance LIMIT {k}");
        let rows = self.query(&sql, &params)?;
        Ok((0..rows.rows.len()).map(|row| (integer(&rows, row, 0), real(&rows, row, 1))).collect())
    }

    /// Returns chunks ranked by the `inillucent_search` table.
    ///
    /// With a keyword expression and a vector, the engine ranks by both and
    /// fuses the two lists itself. With only the keyword expression, it ranks
    /// by BM25 with its proximity and phrase adjustments. Each hit comes back
    /// with `score`, `confidence` and `origin`.
    ///
    /// @param keywords - an FTS5 expression, or nothing for a vector only search
    /// @param vector - the question's vector, or nothing for a keyword only search
    /// @param k - how many hits the search collects
    /// @param title - only chunks of the document with this title, filtered inside the search
    pub fn search_table_hits(
        &self, keywords: Option<&str>, vector: Option<&[u8]>, k: usize, title: Option<&str>,
    ) -> Result<Vec<SearchTableHit>, String> {
        let mut conditions = Vec::new();
        let mut params = Vec::new();
        if let Some(keywords) = keywords {
            params.push(Value::Text(keywords.to_string()));
            conditions.push(format!("chunk_search MATCH ?{}", params.len()));
        }
        if let Some(vector) = vector {
            params.push(Value::Blob(vector.to_vec()));
            conditions.push(format!("vector = ?{}", params.len()));
        }
        if let Some(title) = title {
            params.push(Value::Text(title.to_string()));
            conditions.push(format!("document = ?{}", params.len()));
        }
        conditions.push(format!("k = {k}"));
        let sql = format!(
            "SELECT rowid, score(chunk_search), confidence(chunk_search), origin(chunk_search)
             FROM chunk_search WHERE {} ORDER BY rank",
            conditions.join(" AND ")
        );
        let rows = self.query(&sql, &params)?;
        Ok((0..rows.rows.len())
            .map(|row| SearchTableHit {
                chunk_id: integer(&rows, row, 0),
                score: real(&rows, row, 1),
                confidence: real(&rows, row, 2),
                origin: text(&rows, row, 3),
            })
            .collect())
    }

    /// Returns the title, URL and text of each chunk, by id, and its distance from the question.
    ///
    /// The distance is computed for every hit whatever mode found it, so an
    /// agent can always read how close the best hit is in meaning. See the
    /// README: on this corpus the distance is what tells a question on another
    /// subject apart.
    ///
    /// @param ids - the chunk ids; every one is an integer, so they are written into the SQL
    /// @param question - the question's vector, or nothing to leave the distance out
    pub fn chunk_rows(&self, ids: &[i64], question: Option<&[u8]>) -> Result<HashMap<i64, ChunkRow>, String> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
        let (distance, params) = match question {
            Some(vector) => ("vector_distance_cos(c.v, ?1)", vec![Value::Blob(vector.to_vec())]),
            None => ("NULL", Vec::new()),
        };
        let sql = format!(
            "SELECT c.id, d.title, d.url, c.text, {distance} FROM chunk c JOIN document d ON d.id = c.document_id WHERE c.id IN ({list})"
        );
        let rows = self.query(&sql, &params)?;
        let mut found = HashMap::new();
        for row in 0..rows.rows.len() {
            let chunk = ChunkRow {
                chunk_id: integer(&rows, row, 0),
                title: text(&rows, row, 1),
                url: text(&rows, row, 2),
                text: text(&rows, row, 3),
                distance: match rows.value(row, 4) {
                    Some(Value::Real(distance)) => Some(*distance),
                    _ => None,
                },
            };
            found.insert(chunk.chunk_id, chunk);
        }
        Ok(found)
    }

    /// Returns a chunk with up to `neighbors` chunks on each side, as one span.
    ///
    /// The span runs from the first chunk's start to the last chunk's end in
    /// the stored document text. Neighbouring chunks overlap, so joining their
    /// texts would print the shared sentences twice. Cutting the span from the
    /// document prints each sentence once.
    ///
    /// @param chunk_id - the chunk a search returned
    /// @param neighbors - how many chunks to add on each side
    pub fn passage(&self, chunk_id: i64, neighbors: i64) -> Result<Option<Passage>, String> {
        let found = self.query("SELECT document_id, ordinal FROM chunk WHERE id = ?1", &[Value::Integer(chunk_id)])?;
        if found.rows.is_empty() {
            return Ok(None);
        }
        let (document, ordinal) = (integer(&found, 0, 0), integer(&found, 0, 1));
        let span = self.query(
            "SELECT c.id, c.start_byte, c.end_byte, d.title, d.url, d.body FROM chunk c JOIN document d ON d.id = c.document_id
             WHERE c.document_id = ?1 AND c.ordinal BETWEEN ?2 AND ?3 ORDER BY c.ordinal",
            &[Value::Integer(document), Value::Integer(ordinal - neighbors), Value::Integer(ordinal + neighbors)],
        )?;
        let last = span.rows.len().saturating_sub(1);
        let (start, end) = (integer(&span, 0, 1) as usize, integer(&span, last, 2) as usize);
        let body = text(&span, 0, 5);
        let cut = body.get(start..end).ok_or_else(|| format!("chunk {chunk_id} points outside its document"))?;
        Ok(Some(Passage {
            title: text(&span, 0, 3),
            url: text(&span, 0, 4),
            chunk_ids: (0..span.rows.len()).map(|row| integer(&span, row, 0)).collect(),
            text: cut.to_string(),
        }))
    }

    /// Returns every document whose title contains a filter, with its chunk count.
    ///
    /// @param filter - text the title has to contain, ignoring case; empty for every document
    pub fn documents(&self, filter: &str) -> Result<Vec<DocumentSummary>, String> {
        let rows = self.query(
            "SELECT d.title, d.url, (SELECT count(*) FROM chunk c WHERE c.document_id = d.id), d.synced_at
             FROM document d WHERE instr(lower(d.title), lower(?1)) > 0 ORDER BY d.title",
            &[Value::Text(filter.to_string())],
        )?;
        Ok((0..rows.rows.len())
            .map(|row| DocumentSummary {
                title: text(&rows, row, 0),
                url: text(&rows, row, 1),
                chunks: integer(&rows, row, 2),
                synced_at: text(&rows, row, 3),
            })
            .collect())
    }

    /// Writes a finished sync's report to `sync_log`.
    ///
    /// @param finished_at - when the sync finished
    /// @param report - the report as JSON text
    pub fn record_sync(&self, finished_at: &str, report: &str) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO sync_log (finished_at, report) VALUES (?1, ?2)",
                &[Value::Text(finished_at.to_string()), Value::Text(report.to_string())],
            )
            .map(|_| ())
            .map_err(|error| format!("cannot record the sync: {error}"))
    }

    /// Returns the most recent sync report, as JSON text.
    pub fn last_sync(&self) -> Result<Option<String>, String> {
        let rows = self.query("SELECT report FROM sync_log ORDER BY id DESC LIMIT 1", &[])?;
        Ok(rows.value(0, 0).and_then(Value::text).map(str::to_string))
    }

    /// Runs one query and returns every row.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2` and so on
    fn query(&self, sql: &str, params: &[Value]) -> Result<Rows, String> {
        self.db.query_all(sql, params).map_err(|error| error.to_string())
    }
}

/// One hit from the `inillucent_search` table.
#[derive(Clone, Debug)]
pub struct SearchTableHit {
    /// The chunk's id.
    pub chunk_id: i64,
    /// The score the engine ranked by.
    pub score: f64,
    /// How good the hit is on a fixed scale from 0 to 1.
    pub confidence: f64,
    /// Which search found it: `lexical`, `vector` or `both`.
    pub origin: String,
}

/// Deletes a document's chunks from both tables.
///
/// `chunk_search` is deleted from by rowid, one chunk at a time, because the
/// table is a virtual table and its rows are keyed by the chunk ids.
///
/// @param tx - the open transaction
/// @param document - the document's row id
fn delete_chunks(tx: &inillucent::SharedTransaction, document: i64) -> Result<(), String> {
    let ids = tx
        .query("SELECT id FROM chunk WHERE document_id = ?1", &[Value::Integer(document)], usize::MAX)
        .map_err(|error| error.to_string())?;
    for row in 0..ids.rows.len() {
        tx.execute("DELETE FROM chunk_search WHERE rowid = ?1", &[Value::Integer(integer(&ids, row, 0))])
            .map_err(|error| format!("cannot delete from chunk_search: {error}"))?;
    }
    tx.execute("DELETE FROM chunk WHERE document_id = ?1", &[Value::Integer(document)])
        .map_err(|error| format!("cannot delete chunks: {error}"))?;
    Ok(())
}

/// Writes a document's chunks to `chunk` and to `chunk_search`, with the same ids.
///
/// @param tx - the open transaction
/// @param document - the document's row id
/// @param write - the document and its chunks
fn insert_chunks(tx: &inillucent::SharedTransaction, document: i64, write: &DocumentWrite) -> Result<(), String> {
    let first = next_id(tx, "chunk")?;
    for (offset, (chunk, vector)) in write.chunks.iter().enumerate() {
        let id = first + offset as i64;
        tx.execute(
            "INSERT INTO chunk (id, document_id, ordinal, start_byte, end_byte, text, v) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &[
                Value::Integer(id),
                Value::Integer(document),
                Value::Integer(chunk.ordinal as i64),
                Value::Integer(chunk.start as i64),
                Value::Integer(chunk.end as i64),
                Value::Text(chunk.text.clone()),
                Value::Blob(vector.clone()),
            ],
        )
        .map_err(|error| format!("cannot write chunk {id}: {error}"))?;
        tx.execute(
            "INSERT INTO chunk_search (rowid, title, text, document, vector) VALUES (?1, ?2, ?3, ?4, ?5)",
            &[
                Value::Integer(id),
                Value::Text(write.title.to_string()),
                Value::Text(chunk.text.clone()),
                Value::Text(write.title.to_string()),
                Value::Blob(vector.clone()),
            ],
        )
        .map_err(|error| format!("cannot write chunk {id} to chunk_search: {error}"))?;
    }
    Ok(())
}

/// Returns one more than the largest id in a table.
///
/// @param tx - the open transaction, so no other writer can take the same id
/// @param table - `document` or `chunk`
fn next_id(tx: &inillucent::SharedTransaction, table: &str) -> Result<i64, String> {
    let rows = tx.query(&format!("SELECT coalesce(max(id), 0) + 1 FROM {table}"), &[], 1).map_err(|error| error.to_string())?;
    Ok(integer(&rows, 0, 0))
}

/// Reads an integer cell, or 0 when the cell is missing or not an integer.
///
/// @param rows - the result
/// @param row - the row, from zero
/// @param column - the column, from zero
fn integer(rows: &Rows, row: usize, column: usize) -> i64 {
    match rows.value(row, column) {
        Some(Value::Integer(value)) => *value,
        Some(Value::Real(value)) => *value as i64,
        _ => 0,
    }
}

/// Reads a real cell, or 0.0 when the cell is missing or not a number.
///
/// @param rows - the result
/// @param row - the row, from zero
/// @param column - the column, from zero
fn real(rows: &Rows, row: usize, column: usize) -> f64 {
    match rows.value(row, column) {
        Some(Value::Real(value)) => *value,
        Some(Value::Integer(value)) => *value as f64,
        _ => 0.0,
    }
}

/// Reads a text cell, or an empty string.
///
/// @param rows - the result
/// @param row - the row, from zero
/// @param column - the column, from zero
fn text(rows: &Rows, row: usize, column: usize) -> String {
    rows.value(row, column).and_then(Value::text).unwrap_or_default().to_string()
}
