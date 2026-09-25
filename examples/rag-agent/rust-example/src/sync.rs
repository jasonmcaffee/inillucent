//! Brings the database in step with the source documents.
//!
//! A sync reads every source document, compares it with what the database
//! holds, and writes only the difference:
//!
//! | The document is | The sync |
//! |---|---|
//! | in the source and not the database | chunks it, embeds it and writes it |
//! | in both, with a different fingerprint | does the same, replacing the old chunks |
//! | in both, with the same fingerprint | skips it. Nothing is embedded |
//! | in the database and not the source | deletes it and its chunks |
//!
//! The fingerprint is a SHA-256 over the document and over the settings that
//! shaped its chunks (see `config.rs`). Embedding is the slow part of a sync,
//! about 30 ms a chunk once the model is loaded, so skipping unchanged
//! documents is what makes a sync every few minutes affordable.
//!
//! Each document is written in its own transaction, and its chunks are
//! embedded before that transaction opens. A sync that stops half way leaves
//! every document in either its old state or its new one, and searches keep
//! running while the sync embeds.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::chunker::{chunk_text, embedding_input, lead_sentence, normalise};
use crate::clock::utc_now;
use crate::config::IndexSettings;
use crate::corpus::{read_source, SourceDocument};
use crate::store::{DocumentWrite, Store, StoredDocument};

/// What a running sync is doing, for `sync_status` and for search results.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Progress {
    /// Whether a sync is running now.
    pub running: bool,
    /// When the running sync started.
    pub started_at: Option<String>,
    /// How many documents the running sync has to embed and write.
    pub documents_to_write: usize,
    /// How many of those it has written.
    pub documents_written: usize,
    /// The document it is embedding now.
    pub current_document: Option<String>,
    /// How many chunks it has embedded so far.
    pub chunks_embedded: usize,
}

/// What one sync did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncReport {
    /// When it started.
    pub started_at: String,
    /// When it finished.
    pub finished_at: String,
    /// The file or folder it read.
    pub source: String,
    /// How many documents the source holds.
    pub documents_in_source: usize,
    /// Titles of documents written for the first time.
    pub added: Vec<String>,
    /// Titles of documents written again because they changed.
    pub updated: Vec<String>,
    /// Titles of documents deleted because the source no longer has them.
    pub removed: Vec<String>,
    /// How many documents were skipped because nothing changed.
    pub unchanged: usize,
    /// How many chunks were embedded and written.
    pub chunks_written: usize,
    /// How many chunks the database held before and after.
    pub chunks_before: i64,
    /// How many chunks the database holds now.
    pub chunks_after: i64,
    /// Seconds spent in `embed(TEXT)`.
    pub embed_seconds: f64,
    /// Seconds the whole sync took.
    pub total_seconds: f64,
    /// Documents that could not be written, and why. The sync carries on past them.
    pub errors: Vec<String>,
}

/// What the comparison decided for each document.
struct Plan<'a> {
    /// Documents to embed and write, each with its fingerprint and whether it is new.
    write: Vec<(&'a SourceDocument, String, bool)>,
    /// Stored documents the source no longer has.
    remove: Vec<StoredDocument>,
    /// How many documents are the same in both.
    unchanged: usize,
}

/// Runs one sync from `source` into `store`.
///
/// @param store - the database
/// @param source - a JSONL file or a folder of documents
/// @param settings - the chunking and context settings
/// @param progress - where the sync reports what it is doing
pub fn run_sync(store: &Store, source: &Path, settings: &IndexSettings, progress: &Mutex<Progress>) -> Result<SyncReport, String> {
    let started = Instant::now();
    let documents = read_source(source)?;
    let stored = store.stored_documents()?;
    let plan = plan_changes(&documents, stored.into_values().collect(), settings);
    let mut report = SyncReport {
        started_at: utc_now(),
        source: source.display().to_string(),
        documents_in_source: documents.len(),
        unchanged: plan.unchanged,
        chunks_before: store.counts()?.1,
        ..SyncReport::default()
    };
    update(progress, |p| p.documents_to_write = plan.write.len());
    let mut embed_time = 0.0;
    for (document, fingerprint, is_new) in &plan.write {
        match index_document(store, document, fingerprint, settings, progress) {
            Ok((chunks, seconds)) => {
                report.chunks_written += chunks;
                embed_time += seconds;
                let list = if *is_new { &mut report.added } else { &mut report.updated };
                list.push(document.title.clone());
            }
            Err(error) => report.errors.push(format!("{}: {error}", document.title)),
        }
        update(progress, |p| p.documents_written += 1);
    }
    for gone in &plan.remove {
        match store.delete_document(gone.id) {
            Ok(()) => report.removed.push(gone.title.clone()),
            Err(error) => report.errors.push(format!("{}: {error}", gone.title)),
        }
    }
    report.chunks_after = store.counts()?.1;
    report.embed_seconds = round(embed_time);
    report.total_seconds = round(started.elapsed().as_secs_f64());
    report.finished_at = utc_now();
    let json = serde_json::to_string(&report).map_err(|error| error.to_string())?;
    store.record_sync(&report.finished_at, &json)?;
    Ok(report)
}

/// Compares the source with the database and decides what to do with each document.
///
/// @param documents - every document in the source
/// @param stored - every document in the database
/// @param settings - the settings mixed into each fingerprint
fn plan_changes<'a>(documents: &'a [SourceDocument], stored: Vec<StoredDocument>, settings: &IndexSettings) -> Plan<'a> {
    let salt = settings.fingerprint_salt();
    let by_key: std::collections::HashMap<&str, &StoredDocument> = stored.iter().map(|s| (s.key.as_str(), s)).collect();
    let mut plan = Plan { write: Vec::new(), remove: Vec::new(), unchanged: 0 };
    for document in documents {
        let fingerprint = fingerprint(document, &salt);
        match by_key.get(document.key.as_str()) {
            Some(existing) if existing.fingerprint == fingerprint => plan.unchanged += 1,
            Some(_) => plan.write.push((document, fingerprint, false)),
            None => plan.write.push((document, fingerprint, true)),
        }
    }
    let in_source: HashSet<&str> = documents.iter().map(|d| d.key.as_str()).collect();
    plan.remove = stored.iter().filter(|s| !in_source.contains(s.key.as_str())).cloned().collect();
    plan
}

/// Chunks one document, embeds every chunk, and writes it.
///
/// Returns the number of chunks and the seconds spent embedding them.
///
/// @param store - the database
/// @param document - the document
/// @param fingerprint - its fingerprint, stored so the next sync can skip it
/// @param settings - the chunking and context settings
/// @param progress - where the sync reports what it is doing
fn index_document(
    store: &Store, document: &SourceDocument, fingerprint: &str, settings: &IndexSettings, progress: &Mutex<Progress>,
) -> Result<(usize, f64), String> {
    update(progress, |p| p.current_document = Some(document.title.clone()));
    let body = normalise(&document.text);
    let lead = lead_sentence(&body);
    let started = Instant::now();
    let mut chunks = Vec::new();
    for chunk in chunk_text(&body, &settings.chunking) {
        let vector = store.embed(&embedding_input(&document.title, &lead, &chunk, settings.context))?;
        chunks.push((chunk, vector));
        update(progress, |p| p.chunks_embedded += 1);
    }
    let seconds = started.elapsed().as_secs_f64();
    let count = chunks.len();
    let synced_at = utc_now();
    store.write_document(&DocumentWrite {
        key: &document.key,
        title: &document.title,
        url: &document.url,
        body: &body,
        fingerprint,
        synced_at: &synced_at,
        chunks,
    })?;
    Ok((count, seconds))
}

/// Returns the SHA-256 of a document and the index settings, as hex.
///
/// The fields are separated by a zero byte, so moving text from the title into
/// the body changes the fingerprint.
///
/// @param document - the document
/// @param salt - the settings, from `IndexSettings::fingerprint_salt`
pub fn fingerprint(document: &SourceDocument, salt: &str) -> String {
    let mut hash = Sha256::new();
    for part in [salt, &document.title, &document.url, &document.text] {
        hash.update(part.as_bytes());
        hash.update([0u8]);
    }
    hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Changes the shared progress under its lock.
///
/// @param progress - the shared progress
/// @param change - what to change
fn update(progress: &Mutex<Progress>, change: impl FnOnce(&mut Progress)) {
    let mut guard = progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    change(&mut guard);
}

/// Rounds seconds to milliseconds for the report.
///
/// @param seconds - the value to round
fn round(seconds: f64) -> f64 {
    (seconds * 1000.0).round() / 1000.0
}
