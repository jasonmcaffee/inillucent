//! The legacy direct index API, over a database.
//!
//! Invariant: this answers the same questions as `inillucent_core::index::Index`
//! and answers them the same way. It is the second implementation of
//! `inillucent_search::adapter::RetrievalIndex`; the first is the direct engine
//! itself, and the point of there being a trait at all is that one test script
//! can drive both and compare the rankings.
//!
//! What it does *not* do is reimplement retrieval. Every question ends up in
//! the same `inillucent-core` code the direct engine runs, because the search
//! table's base generation *is* a `inillucent-core` index - it is written by
//! `persist::write_index` and read by `persist::read_index`. The difference
//! between the two implementations is entirely where the bytes live and when
//! they become visible, which is exactly the difference this phase set out to
//! make.

use inillucent_base::{DbError, DbResult};
use inillucent_core::rank::HitOrigin;
use inillucent_core::store::ChunkInput;
use inillucent_engine::connect::{Connection, Database};
use inillucent_search::adapter::{Hit, Query, RetrievalIndex};
use inillucent_tree::datum::OwnedDatum;

use crate::copy;

/// A search table, opened as a retrieval index.
pub struct SqlIndex {
    /// The database handle. A connection is a borrow of it rather than a thing
    /// of its own, so one is made where it is used instead of being stored -
    /// storing it beside the database it borrows would be a self-referential
    /// struct for no gain.
    database: Database,
    table: String,
    dims: usize,
    /// Whether the table declares the liveness facet the migration writes.
    ///
    /// A table that declares none has no tombstoned rows to exclude: a search
    /// table's own `tombstone` deletes the row, so liveness only means anything
    /// where something outside the table records it, and the facet is where
    /// that record lives. So a query asking for live rows only against a table
    /// with no facet is asking for every row, which is what it gets.
    live_facet: bool,
}

impl SqlIndex {
    /// Opens a search table in an existing database.
    ///
    /// The table has to be there: creating one silently would hide a typo in a
    /// table name behind an empty result, which is the failure mode a retrieval
    /// system is worst at showing you.
    /// @param path - the database file
    /// @param table - the `inillucent_search` table's name
    pub fn open(path: impl AsRef<std::path::Path>, table: &str) -> DbResult<SqlIndex> {
        let database = Database::open(path)?;
        // One session for both reads. `Database::session` hands out a *new*
        // logical session each time it is called, and these two ask the same
        // shadow table the same kind of question at the same moment, so there
        // is no reason for them to be two.
        let (dims, live_facet) = {
            let connection = database.session();
            (
                declared_dims(&connection, table)?,
                declares_live_facet(&connection, table)?,
            )
        };
        Ok(SqlIndex {
            database,
            table: table.to_string(),
            dims,
            live_facet,
        })
    }

    /// Returns a connection, for a caller that wants ordinary SQL as well.
    pub fn connection(&self) -> Connection<'_> {
        self.database.session()
    }

    /// Walks every tree of the database and verifies its key order.
    pub fn check(&self) -> DbResult<()> {
        self.database.check()
    }

    /// Returns the vector width the table declared.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Returns the table's name.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Runs a maintenance command on the table.
    pub fn command(&self, command: &str) -> DbResult<()> {
        let sql = format!(
            "INSERT INTO {}({}) VALUES ('{}')",
            self.table, self.table, command
        );
        self.database.session().execute_batch(&sql)
    }
}

/// Says which of the `%_config` reads failed, on top of what the engine said.
///
/// **Both reads name the same table, so the engine's own message cannot tell
/// them apart** (task-2067). A run of the release-sized corpus suite came back
/// with `cannot reopen the staged database: no such table: chunk_search_config`
/// and there was no way to say which read had asked, which is the difference
/// between a defect somebody can look at and one somebody has to guess about.
/// It was not reproduced in 31 further runs of the two suites that exercise
/// this path, so the next occurrence is what there is to work with, and it
/// should arrive saying what it was doing.
/// @param table - the search table's name
/// @param key - the `%_config` key being read
/// @param error - what the engine said
fn reading(table: &str, key: &str, error: DbError) -> DbError {
    let said = error
        .detail()
        .unwrap_or_else(|| error.message())
        .to_string();
    error.with_detail(format!("reading {key} out of {table}_config: {said}"))
}

/// Returns the vector width a search table declared, from its own config rows.
fn declared_dims(connection: &Connection<'_>, table: &str) -> DbResult<usize> {
    let sql = format!("SELECT v FROM {table}_config WHERE k = 'dims'");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| reading(table, "dims", error))?;
    if !statement.step()? {
        return Err(DbError::primary(inillucent_base::PrimaryCode::Error)
            .with_detail(format!("{table} is not a inillucent_search table")));
    }
    let width = match statement.row().first() {
        Some(OwnedDatum::Int(number)) => *number as usize,
        Some(OwnedDatum::Text(text)) => String::from_utf8_lossy(text).trim().parse().unwrap_or(0),
        _ => 0,
    };
    Ok(width)
}

/// Reports whether a search table declared the liveness facet.
///
/// Read from the table's own `%_config` rather than from the `CREATE`
/// statement, for the reason the stored rows always win here: they are what the
/// existing rows were written against.
/// @param connection - a session on the database
/// @param table - the `inillucent_search` table's name
fn declares_live_facet(connection: &Connection<'_>, table: &str) -> DbResult<bool> {
    let sql = format!("SELECT v FROM {table}_config WHERE k = 'facets'");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| reading(table, "facets", error))?;
    if !statement.step()? {
        return Ok(false);
    }
    let Some(OwnedDatum::Text(value)) = statement.row().first() else {
        return Ok(false);
    };
    Ok(String::from_utf8_lossy(value)
        .split(',')
        .any(|name| name.trim() == copy::LIVE_FACET))
}

/// Encodes a vector the way a search table stores one.
fn vector_blob(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len().saturating_mul(4));
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Returns the rowid one external chunk identifier maps to.
///
/// The migration writes the source chunk ordinal as the rowid, so the mapping
/// is the identity for a migrated database - but the adapter does not assume
/// that, because a table somebody else filled has whatever rowids they chose.
fn rowid_of(id: &str) -> Option<i64> {
    id.parse::<i64>().ok()
}

impl RetrievalIndex for SqlIndex {
    /// Inserts each chunk as a row, in one transaction.
    ///
    /// One transaction rather than one per chunk, because that is what the
    /// direct engine's `append` is: a single unit of work whose result is
    /// visible all at once. A caller that batched differently would see a
    /// different number of intermediate states.
    fn append(&mut self, chunks: Vec<ChunkInput>, embeddings: &[Vec<f32>]) -> DbResult<usize> {
        if chunks.len() != embeddings.len() {
            return Err(DbError::primary(inillucent_base::PrimaryCode::Misuse)
                .with_detail("each chunk needs exactly one vector"));
        }
        self.database.session().execute_batch("BEGIN")?;
        let outcome = self.append_inside(&chunks, embeddings);
        match outcome {
            Ok(count) => {
                self.database.session().execute_batch("COMMIT")?;
                Ok(count)
            }
            Err(failure) => {
                let _ = self.database.session().execute_batch("ROLLBACK");
                Err(failure)
            }
        }
    }

    /// Deletes every row of a document, reporting whether there was one.
    fn tombstone(&mut self, _source: &str, external_doc_id: &str) -> DbResult<bool> {
        let Some(rowid) = rowid_of(external_doc_id) else {
            return Ok(false);
        };
        let sql = format!("SELECT count(*) FROM {}_content WHERE id = ?1", self.table);
        let mut probe = self.database.session().prepare(&sql)?;
        probe.bind_integer(1, rowid)?;
        let present = probe.step()?
            && probe
                .row()
                .first()
                .and_then(as_integer)
                .is_some_and(|count| count > 0);
        drop(probe);
        if !present {
            return Ok(false);
        }
        let sql = format!("DELETE FROM {} WHERE rowid = ?1", self.table);
        let mut statement = self.database.session().prepare(&sql)?;
        statement.bind_integer(1, rowid)?;
        while statement.step()? {}
        Ok(true)
    }

    /// Replaces one document, which for a search table is one row.
    fn replace(
        &mut self,
        source: &str,
        external_doc_id: &str,
        chunks: Vec<ChunkInput>,
        embeddings: &[Vec<f32>],
    ) -> DbResult<usize> {
        self.tombstone(source, external_doc_id)?;
        self.append(chunks, embeddings)
    }

    /// Folds the delta log into a new generation.
    ///
    /// The direct engine's `commit` builds every queryable structure in one
    /// pass, and this is the same act: compaction rebuilds the base generation
    /// from the rows, which is the single-pass build.
    fn build(&mut self) -> DbResult<()> {
        self.command("compact")
    }

    /// Answers one query through the table's own access path.
    fn search(&mut self, query: &Query) -> DbResult<Vec<Hit>> {
        let limit = query.limit.max(1);
        let has_text = !query.text.trim().is_empty();
        let has_vector = !query.vector.is_empty();
        if !has_text && !has_vector {
            return Ok(Vec::new());
        }
        let mut terms = vec![format!("k = {limit}")];
        if has_text {
            terms.push(format!("{} MATCH ?1", self.table));
        }
        if has_vector {
            terms.push("vector = ?2".to_string());
        }
        if let Some(recall) = query.recall {
            terms.push(format!("recall = {recall}"));
        }
        // **Inside the search rather than around it** (task-2067). This is a
        // constraint on the table's liveness facet, so the module compiles it
        // into the filter the scan runs under and the ranking never sees a
        // tombstoned chunk. Written as `AND live = '1'` outside the search it
        // would be the same rows and a different order, because the position
        // rescore reaches a fixed depth into whatever the scan admitted.
        if !query.include_deleted && self.live_facet {
            terms.push(format!("{} = '{}'", copy::LIVE_FACET, copy::LIVE));
        }
        let sql = format!(
            "SELECT rowid, score({}), confidence({}), origin({}) FROM {} WHERE {} ORDER BY rank",
            self.table,
            self.table,
            self.table,
            self.table,
            terms.join(" AND ")
        );
        let mut statement = self.database.session().prepare(&sql)?;
        if has_text {
            statement.bind_text(1, &query.text)?;
        }
        if has_vector {
            statement.bind_blob(2, &vector_blob(&query.vector))?;
        }
        let mut hits = Vec::new();
        while statement.step()? {
            let row = statement.row();
            let Some(id) = row.first().and_then(as_integer) else {
                continue;
            };
            hits.push(Hit {
                id: id.to_string(),
                score: row.get(1).and_then(as_real).unwrap_or(0.0) as f32,
                confidence: row.get(2).and_then(as_real).unwrap_or(0.0) as f32,
                origin: origin_of(row.get(3)),
            });
        }
        Ok(hits)
    }

    /// Returns how many rows the table holds.
    fn live_chunks(&mut self) -> DbResult<usize> {
        let sql = format!("SELECT count(*) FROM {}_content", self.table);
        let mut statement = self.database.session().prepare(&sql)?;
        if !statement.step()? {
            return Ok(0);
        }
        Ok(statement
            .row()
            .first()
            .and_then(as_integer)
            .unwrap_or(0)
            .max(0) as usize)
    }
}

impl SqlIndex {
    /// Inserts the rows of one append, inside a transaction the caller opened.
    fn append_inside(&mut self, chunks: &[ChunkInput], embeddings: &[Vec<f32>]) -> DbResult<usize> {
        // The facet is written whenever the table has one, from the chunk's own
        // tombstone. A row inserted with the column left NULL would read back as
        // the empty string, which is neither of the two values a liveness query
        // asks for - so it would be a row no search could ever return, and
        // nothing would say so.
        let sql = match self.live_facet {
            true => format!(
                "INSERT INTO {}(rowid, content, {}, vector) VALUES (?1, ?2, ?4, ?3)",
                self.table,
                copy::LIVE_FACET
            ),
            false => format!(
                "INSERT INTO {}(rowid, content, vector) VALUES (?1, ?2, ?3)",
                self.table
            ),
        };
        let mut count = 0usize;
        for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
            let Some(external) = chunk.external_chunk_id.as_deref() else {
                return Err(DbError::primary(inillucent_base::PrimaryCode::Misuse)
                    .with_detail("a chunk needs an external identifier to become a rowid"));
            };
            let Some(rowid) = rowid_of(external) else {
                return Err(DbError::primary(inillucent_base::PrimaryCode::Misuse)
                    .with_detail(format!("{external} is not a rowid")));
            };
            let mut statement = self.database.session().prepare(&sql)?;
            statement.bind_integer(1, rowid)?;
            statement.bind_text(2, &chunk.content)?;
            if self.dims > 0 {
                statement.bind_blob(3, &vector_blob(embedding))?;
            } else {
                statement.bind_null(3)?;
            }
            if self.live_facet {
                statement.bind_text(
                    4,
                    match chunk.deleted {
                        true => copy::TOMBSTONED,
                        false => copy::LIVE,
                    },
                )?;
            }
            while statement.step()? {}
            count = count.saturating_add(1);
        }
        Ok(count)
    }
}

/// Reads a hit origin back from the name the module reports.
fn origin_of(value: Option<&OwnedDatum>) -> HitOrigin {
    let text = match value {
        Some(OwnedDatum::Text(text)) => String::from_utf8_lossy(text).into_owned(),
        _ => String::new(),
    };
    match text.as_str() {
        "vector" => HitOrigin::Vector,
        "both" => HitOrigin::Both,
        _ => HitOrigin::Lexical,
    }
}

/// Returns a datum's integer value, when it holds one.
///
/// The old facade's `Value` carried these as methods. `OwnedDatum` is the
/// engine's own row value and deliberately does not convert silently, so the
/// two readings a caller wants are written out here rather than assumed.
///
/// @param value - the datum
fn as_integer(value: &OwnedDatum) -> Option<i64> {
    match value {
        OwnedDatum::Int(number) => Some(*number),
        _ => None,
    }
}

/// Returns a datum's real value, when it holds a number.
///
/// An integer answers here as well, because a score column holding a whole
/// number is still a score.
///
/// @param value - the datum
fn as_real(value: &OwnedDatum) -> Option<f64> {
    match value {
        OwnedDatum::Real(number) => Some(*number),
        OwnedDatum::Int(number) => Some(*number as f64),
        _ => None,
    }
}
