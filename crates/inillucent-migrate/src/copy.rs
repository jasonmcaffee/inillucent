//! Creating the destination schema and copying the corpus into it.
//!
//! Invariant: every write to the destination happens inside a bounded
//! transaction whose completion is recorded in the manifest before the next one
//! starts. A migration interrupted anywhere resumes from the last recorded
//! batch, and a batch is either wholly in the database or wholly absent -
//! because that is what a transaction is, and because the manifest line is
//! written after the commit rather than before it.
//!
//! The relational shape is the one the legacy store already has, made explicit:
//! documents, their labels, their attributes, their flags, and their chunks.
//! The dictionaries do not survive as tables and should not - interning is a
//! storage technique the legacy format needed because it holds one flat arena,
//! and a database that repeated a source name in every row would be storing the
//! same thing a different way. What has to survive is the *mapping*, and it
//! does: every interned value comes back as the text it stood for.
//!
//! The search table declares exactly one column and it holds the chunk's text
//! verbatim. That is what makes the migrated index score identically to the
//! source: same terms, same document lengths, same corpus statistics, produced
//! by the same single-pass build.

use inillucent_base::hash::Sha256;
use inillucent_base::DbResult;
use inillucent_core::store::Store;
use inillucent_engine::connect::{Connection, Statement};
use inillucent_tree::datum::OwnedDatum;

use crate::manifest::Manifest;

/// How many rows one copy transaction carries.
///
/// Bounded so that a migration of a large corpus does not hold one transaction
/// open across the whole run: an interrupted migration would then have nothing
/// to resume from, and the journal would have grown to the size of the corpus.
pub const BATCH: usize = 512;

/// The name of the search table the migration creates.
pub const SEARCH_TABLE: &str = "chunk_search";

/// The facet column that carries whether a chunk's document is tombstoned.
///
/// **The legacy default filter cannot be reproduced by a join** (task-2067).
/// The legacy engine excludes a tombstoned document's chunks inside the posting
/// scan, and `Bm25Index::top_k` then rescores the best `k *
/// rescore_depth_factor` of whatever was admitted - so dropping the tombstoned
/// rows from the answer instead gives a different rescore window and therefore
/// a different order, measured at nine of the top ten hits. The flag has to be
/// somewhere the ranking can see it before it ranks, which is what a facet
/// column is.
///
/// It is written beside `document.deleted` rather than instead of it: the
/// `document` table is the copy of the source's own row and every content check
/// compares it, and this is the same fact in the place a query can use.
pub const LIVE_FACET: &str = "live";

/// The value `LIVE_FACET` holds for a chunk whose document is not tombstoned.
pub const LIVE: &str = "1";

/// The value `LIVE_FACET` holds for a chunk of a tombstoned document.
///
/// Both states are written rather than one of them being absent, because a
/// facet resolves through the store's dictionary and a `NULL` would land there
/// as the empty string - a value a caller would have to know to ask for.
pub const TOMBSTONED: &str = "0";

/// Returns the schema statements a destination needs.
///
/// @param dims - the vector width the source index was built with
pub fn schema(dims: usize) -> Vec<String> {
    let mut statements = vec![
        "CREATE TABLE migration(k TEXT PRIMARY KEY, v TEXT)".to_string(),
        "CREATE TABLE document(\
           id INTEGER PRIMARY KEY, \
           source TEXT NOT NULL, \
           external_id TEXT NOT NULL, \
           title TEXT, \
           url TEXT, \
           space_key TEXT, \
           author TEXT, \
           author_id TEXT, \
           updated_at INTEGER, \
           deleted INTEGER NOT NULL, \
           chunk_count INTEGER NOT NULL)"
            .to_string(),
        "CREATE INDEX document_identity ON document(source, external_id)".to_string(),
        "CREATE TABLE document_label(document INTEGER NOT NULL, label TEXT NOT NULL)".to_string(),
        "CREATE INDEX document_label_document ON document_label(document)".to_string(),
        "CREATE TABLE document_flag(document INTEGER NOT NULL, flag TEXT NOT NULL)".to_string(),
        "CREATE INDEX document_flag_document ON document_flag(document)".to_string(),
        "CREATE TABLE document_attribute(\
           document INTEGER NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL)"
            .to_string(),
        "CREATE INDEX document_attribute_document ON document_attribute(document)".to_string(),
        "CREATE TABLE chunk(\
           id INTEGER PRIMARY KEY, \
           document INTEGER NOT NULL, \
           chunk_index INTEGER NOT NULL, \
           external_id TEXT, \
           heading TEXT, \
           content TEXT NOT NULL)"
            .to_string(),
        "CREATE INDEX chunk_document ON chunk(document)".to_string(),
    ];
    statements.push(format!(
        "CREATE VIRTUAL TABLE {SEARCH_TABLE} USING inillucent_search(content, \
         {LIVE_FACET} FACET, dims = {dims}, mode = 'exact', compact = 0)"
    ));
    statements
}

/// Creates the destination schema in one transaction.
pub fn create_schema(connection: &Connection<'_>, dims: usize) -> DbResult<()> {
    connection.execute_batch("BEGIN")?;
    for statement in schema(dims) {
        if let Err(failure) = connection.execute_batch(&statement) {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(failure);
        }
    }
    connection.execute_batch("COMMIT")
}

/// Copies the documents, their labels, their flags and their attributes.
///
/// Resumes from the manifest's checkpoint: a document already copied is skipped
/// rather than re-inserted, so a resumed run does not have to know whether the
/// previous one had committed its last batch.
/// @param connection - the destination
/// @param store - the legacy store
/// @param manifest - the log to checkpoint into
pub fn copy_documents(
    connection: &Connection<'_>,
    store: &Store,
    manifest: &mut Manifest,
) -> Result<u64, String> {
    let start = manifest.checkpoint("document") as usize;
    let total = store.n_documents();
    let mut copied = start as u64;
    let mut at = start;
    while at < total {
        let end = at.saturating_add(BATCH).min(total);
        connection
            .execute_batch("BEGIN")
            .map_err(|error| format!("cannot begin: {}", error.message()))?;
        let batch = write_documents(connection, store, at, end);
        match batch {
            Ok(()) => connection
                .execute_batch("COMMIT")
                .map_err(|error| format!("cannot commit: {}", error.message()))?,
            Err(failure) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(failure);
            }
        }
        copied = end as u64;
        at = end;
        manifest.record("copied", format!("document {copied}"))?;
    }
    Ok(copied)
}

/// Writes one batch of documents and everything hanging off them.
fn write_documents(
    connection: &Connection<'_>,
    store: &Store,
    from: usize,
    to: usize,
) -> Result<(), String> {
    for ordinal in from..to {
        let Some(document) = store.documents.get(ordinal) else {
            continue;
        };
        let id = ordinal as i64;
        let source = store
            .sources
            .value(document.source)
            .unwrap_or_default()
            .to_string();
        let space = document
            .space_key
            .and_then(|key| store.spaces.value(key))
            .map(str::to_string);
        let author = document
            .author
            .and_then(|key| store.authors.value(key))
            .map(str::to_string);
        let author_id = document
            .author_id
            .and_then(|key| store.author_ids.value(key))
            .map(str::to_string);
        let mut statement = connection
            .prepare(
                "INSERT INTO document(id, source, external_id, title, url, space_key, author, \
                 author_id, updated_at, deleted, chunk_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .map_err(|error| error.message().to_string())?;
        bind(&mut statement, 1, OwnedDatum::Int(id))?;
        bind_text(&mut statement, 2, &source)?;
        bind_text(&mut statement, 3, &document.external_id)?;
        bind_text(&mut statement, 4, &document.title)?;
        bind_text(&mut statement, 5, &document.url)?;
        bind_optional(&mut statement, 6, space.as_deref())?;
        bind_optional(&mut statement, 7, author.as_deref())?;
        bind_optional(&mut statement, 8, author_id.as_deref())?;
        bind(&mut statement, 9, OwnedDatum::Int(document.updated_at))?;
        bind(
            &mut statement,
            10,
            OwnedDatum::Int(i64::from(document.deleted)),
        )?;
        bind(
            &mut statement,
            11,
            OwnedDatum::Int(i64::from(document.chunk_count)),
        )?;
        drain(&mut statement)?;

        for label in store.labels_of(ordinal as u32) {
            let Some(text) = store.labels.value(*label) else {
                continue;
            };
            let text = text.to_string();
            let mut statement = connection
                .prepare("INSERT INTO document_label(document, label) VALUES (?1, ?2)")
                .map_err(|error| error.message().to_string())?;
            bind(&mut statement, 1, OwnedDatum::Int(id))?;
            bind_text(&mut statement, 2, &text)?;
            drain(&mut statement)?;
        }

        for (name, value) in store.attributes_of(ordinal as u32) {
            let Some(name_text) = store.attribute_names.value(*name) else {
                continue;
            };
            let name_text = name_text.to_string();
            let Some(values) = store.attribute_values.get(*name as usize) else {
                continue;
            };
            let Some(value_text) = values.value(*value) else {
                continue;
            };
            let value_text = value_text.to_string();
            let mut statement = connection
                .prepare(
                    "INSERT INTO document_attribute(document, name, value) VALUES (?1, ?2, ?3)",
                )
                .map_err(|error| error.message().to_string())?;
            bind(&mut statement, 1, OwnedDatum::Int(id))?;
            bind_text(&mut statement, 2, &name_text)?;
            bind_text(&mut statement, 3, &value_text)?;
            drain(&mut statement)?;
        }

        for (bit, name) in store.flag_names.values().iter().enumerate() {
            if document.flags & (1u32 << bit.min(31)) == 0 {
                continue;
            }
            let name = name.clone();
            let mut statement = connection
                .prepare("INSERT INTO document_flag(document, flag) VALUES (?1, ?2)")
                .map_err(|error| error.message().to_string())?;
            bind(&mut statement, 1, OwnedDatum::Int(id))?;
            bind_text(&mut statement, 2, &name)?;
            drain(&mut statement)?;
        }
    }
    Ok(())
}

/// Copies the chunks and, in the same transaction, indexes them.
///
/// The row and its index entry land together because they are the same
/// transaction - which is the property this whole phase exists to provide, and
/// it means an interrupted migration can never leave a destination whose chunks
/// are present and unsearchable.
pub fn copy_chunks(
    connection: &Connection<'_>,
    store: &Store,
    manifest: &mut Manifest,
) -> Result<u64, String> {
    let start = manifest.checkpoint("chunk") as usize;
    let total = store.n_chunks();
    let mut copied = start as u64;
    let mut at = start;
    while at < total {
        let end = at.saturating_add(BATCH).min(total);
        connection
            .execute_batch("BEGIN")
            .map_err(|error| format!("cannot begin: {}", error.message()))?;
        match write_chunks(connection, store, at, end) {
            Ok(()) => connection
                .execute_batch("COMMIT")
                .map_err(|error| format!("cannot commit: {}", error.message()))?,
            Err(failure) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(failure);
            }
        }
        copied = end as u64;
        at = end;
        manifest.record("copied", format!("chunk {copied}"))?;
    }
    Ok(copied)
}

/// Writes one batch of chunks, into the table and into the index.
fn write_chunks(
    connection: &Connection<'_>,
    store: &Store,
    from: usize,
    to: usize,
) -> Result<(), String> {
    let vectors = store.n_chunks();
    for ordinal in from..to.min(vectors) {
        let Some(chunk) = store.chunks.get(ordinal) else {
            continue;
        };
        let id = ordinal as i64;
        let content = store.content(ordinal as u32).to_string();
        let heading = store.heading_path(ordinal as u32).join(" > ");
        let external = store.chunk_external_id(ordinal as u32).to_string();
        let mut statement = connection
            .prepare(
                "INSERT INTO chunk(id, document, chunk_index, external_id, heading, content) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|error| error.message().to_string())?;
        bind(&mut statement, 1, OwnedDatum::Int(id))?;
        bind(&mut statement, 2, OwnedDatum::Int(i64::from(chunk.doc)))?;
        bind(
            &mut statement,
            3,
            OwnedDatum::Int(i64::from(chunk.chunk_index)),
        )?;
        bind_text(&mut statement, 4, &external)?;
        bind_text(&mut statement, 5, &heading)?;
        bind_text(&mut statement, 6, &content)?;
        drain(&mut statement)?;
    }
    Ok(())
}

/// Writes the search rows for a range of chunks, vectors included.
///
/// Separate from the chunk copy because the vectors come from the index rather
/// than the store, and because a caller migrating the relational tables of an
/// index whose search cannot be reproduced still wants the chunks.
pub fn copy_search(
    connection: &Connection<'_>,
    store: &Store,
    vectors: &inillucent_core::vectors::VectorSet,
    dims: usize,
    manifest: &mut Manifest,
) -> Result<u64, String> {
    let start = manifest.checkpoint("search") as usize;
    let total = store.n_chunks();
    let mut copied = start as u64;
    let mut at = start;
    while at < total {
        let end = at.saturating_add(BATCH).min(total);
        connection
            .execute_batch("BEGIN")
            .map_err(|error| format!("cannot begin: {}", error.message()))?;
        match write_search(connection, store, vectors, dims, at, end) {
            Ok(()) => connection
                .execute_batch("COMMIT")
                .map_err(|error| format!("cannot commit: {}", error.message()))?,
            Err(failure) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(failure);
            }
        }
        copied = end as u64;
        at = end;
        manifest.record("copied", format!("search {copied}"))?;
    }
    Ok(copied)
}

/// Writes one batch of search rows.
fn write_search(
    connection: &Connection<'_>,
    store: &Store,
    vectors: &inillucent_core::vectors::VectorSet,
    dims: usize,
    from: usize,
    to: usize,
) -> Result<(), String> {
    let sql = format!(
        "INSERT INTO {SEARCH_TABLE}(rowid, content, {LIVE_FACET}, vector) \
         VALUES (?1, ?2, ?3, ?4)"
    );
    for ordinal in from..to {
        let content = store.content(ordinal as u32).to_string();
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| error.message().to_string())?;
        bind(&mut statement, 1, OwnedDatum::Int(ordinal as i64))?;
        bind_text(&mut statement, 2, &content)?;
        bind_text(&mut statement, 3, live_value(store, ordinal as u32))?;
        if dims > 0 && ordinal < vectors.len() {
            let mut bytes = Vec::with_capacity(dims.saturating_mul(4));
            for value in vectors.copy_of(ordinal as u32) {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            statement
                .bind_blob(4, &bytes)
                .map_err(|error| error.message().to_string())?;
        } else {
            statement
                .bind_null(4)
                .map_err(|error| error.message().to_string())?;
        }
        drain(&mut statement)?;
    }
    Ok(())
}

/// Returns what the liveness facet holds for one chunk.
///
/// A chunk whose document the legacy store cannot name is read as live, which
/// is the same reading `CompiledFilter::passes` gives it: the tombstone is a
/// property of a document row, and a chunk with no document row has not been
/// tombstoned.
/// @param store - the legacy store
/// @param chunk - the chunk's ordinal, which is also its rowid in the copy
fn live_value(store: &Store, chunk: u32) -> &'static str {
    let tombstoned = store
        .doc_of(chunk)
        .and_then(|document| store.documents.get(document as usize))
        .is_some_and(|document| document.deleted);
    match tombstoned {
        true => TOMBSTONED,
        false => LIVE,
    }
}

/// Returns an ordered digest of one query's rows.
///
/// Ordered because the point is to compare two orderings, not two multisets: a
/// destination that holds every row in the wrong order would pass a count check
/// and a set check and still return a different top ten.
pub fn digest(connection: &Connection<'_>, sql: &str) -> Result<(u64, String), String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    let mut hasher = Sha256::new();
    let mut rows = 0u64;
    while statement
        .step()
        .map_err(|error| format!("{sql}: {}", error.message()))?
    {
        let row = statement.row();
        for index in 0..row.len() {
            match row.get(index) {
                Some(OwnedDatum::Null) | None => hasher.update(b"\x00"),
                Some(OwnedDatum::Int(number)) => {
                    hasher.update(b"\x01");
                    hasher.update(&number.to_le_bytes());
                }
                Some(OwnedDatum::Real(number)) => {
                    hasher.update(b"\x02");
                    hasher.update(&number.to_bits().to_le_bytes());
                }
                Some(OwnedDatum::Text(text)) => {
                    hasher.update(b"\x03");
                    hasher.update(text);
                }
                Some(OwnedDatum::Blob(blob)) => {
                    hasher.update(b"\x04");
                    hasher.update(blob);
                }
            }
            hasher.update(b"\x1f");
        }
        hasher.update(b"\x1e");
        rows = rows.saturating_add(1);
    }
    Ok((rows, hasher.hex()))
}

/// Binds one value, reporting a failure as text.
fn bind(statement: &mut Statement<'_>, index: u32, value: OwnedDatum) -> Result<(), String> {
    statement
        .bind(index, value)
        .map_err(|error| error.message().to_string())
}

/// Binds one text value.
fn bind_text(statement: &mut Statement<'_>, index: u32, value: &str) -> Result<(), String> {
    statement
        .bind_text(index, value)
        .map_err(|error| error.message().to_string())
}

/// Binds one optional text value, NULL when there is none.
fn bind_optional(
    statement: &mut Statement<'_>,
    index: u32,
    value: Option<&str>,
) -> Result<(), String> {
    match value {
        Some(text) => bind_text(statement, index, text),
        None => statement
            .bind_null(index)
            .map_err(|error| error.message().to_string()),
    }
}

/// Steps a statement to completion.
fn drain(statement: &mut Statement<'_>) -> Result<(), String> {
    while statement
        .step()
        .map_err(|error| error.message().to_string())?
    {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema declares a search table with one text column and the
    /// liveness facet beside it.
    ///
    /// One text column, because the migration puts the source chunk's text in
    /// it verbatim so that the terms and the corpus statistics are the source's
    /// - see `merge::chunk_of`. The facet is not text and does not join it,
    /// which is what keeps that true.
    #[test]
    fn the_search_table_has_one_text_column_and_the_liveness_facet() {
        let statements = schema(8);
        let search = statements
            .iter()
            .find(|statement| statement.contains("inillucent_search"))
            .expect("a search table");
        assert!(search.contains("(content, live FACET"), "{search}");
        assert!(search.contains("dims = 8"), "{search}");
        assert!(search.contains("mode = 'exact'"));
    }

    /// A lexical-only source produces a search table with no vector width.
    #[test]
    fn a_lexical_source_declares_no_vector_width() {
        let statements = schema(0);
        let search = statements
            .iter()
            .find(|statement| statement.contains("inillucent_search"))
            .expect("a search table");
        assert!(search.contains("dims = 0"), "{search}");
    }

    /// Every table the copy writes into is created by the schema.
    #[test]
    fn the_schema_creates_every_table_the_copy_writes() {
        let statements = schema(4).join("\n");
        for table in [
            "document",
            "document_label",
            "document_flag",
            "document_attribute",
            "chunk",
            "migration",
        ] {
            assert!(
                statements.contains(&format!("CREATE TABLE {table}(")),
                "{table} is not created"
            );
        }
    }
}
