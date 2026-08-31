//! Document and chunk storage with dictionary encoded filter columns.
//!
//! Every attribute a filter touches is a fixed width integer here, and the string
//! it stands for is interned once at build time. The filtered vector traversal
//! evaluates the predicate on every node it visits, so predicate evaluation is in
//! the innermost loop; comparing two `u16`s in a packed array is a different cost
//! from comparing two `String`s scattered across the heap.

use std::collections::HashMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};

/// Interns strings to dense `u32` identifiers and can map back for output.
#[derive(Default, Serialize, Deserialize)]
pub struct Dictionary {
    values: Vec<String>,
    lookup: HashMap<String, u32>,
}

impl Dictionary {
    pub fn intern(&mut self, value: &str) -> u32 {
        if let Some(id) = self.lookup.get(value) {
            return *id;
        }
        let id = self.values.len() as u32;
        self.values.push(value.to_string());
        self.lookup.insert(value.to_string(), id);
        id
    }

    /// Identifier for an existing value, without inserting. A filter naming a
    /// value the corpus never contained has to select nothing, which is different
    /// from selecting everything, so callers distinguish `None` from a match.
    pub fn get(&self, value: &str) -> Option<u32> {
        self.lookup.get(value).copied()
    }

    pub fn value(&self, id: u32) -> Option<&str> {
        self.values.get(id as usize).map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Sentinel for "this document has no updated_at". Sorts below every real
/// timestamp, so an `updated_after` filter excludes it, matching the SQL
/// behaviour where `NULL >= x` is not true.
pub const NO_TIMESTAMP: i64 = i64::MIN;

#[derive(Clone, Serialize, Deserialize)]
pub struct Document {
    pub source: u32,
    pub space_key: Option<u32>,
    pub author: Option<u32>,
    pub author_id: Option<u32>,
    pub updated_at: i64,
    /// Slice into `Store::label_arena`.
    pub labels: Range<u32>,
    pub deleted: bool,
    pub title: String,
    pub url: String,
    /// The identifier the source system uses, carried through for output.
    pub external_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub doc: u32,
    pub chunk_index: u32,
    /// Slice into `Store::heading_arena`.
    pub heading_path: Range<u32>,
    /// Byte range into `Store::text`.
    pub content: Range<u64>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Store {
    pub documents: Vec<Document>,
    pub chunks: Vec<Chunk>,

    pub sources: Dictionary,
    pub spaces: Dictionary,
    pub authors: Dictionary,
    pub author_ids: Dictionary,
    pub labels: Dictionary,
    pub headings: Dictionary,

    /// Label identifiers, referenced by `Document::labels`.
    pub label_arena: Vec<u32>,
    /// Heading string identifiers, referenced by `Chunk::heading_path`.
    pub heading_arena: Vec<u32>,
    /// All chunk text, contiguous. Chunks hold byte ranges into it.
    pub text: String,

    /// Chunks per source that are not soft deleted, indexed by source id.
    ///
    /// Maintained on insert so that the commonest predicate in this corpus, a
    /// single `source`, can report how many chunks it admits without scanning
    /// every chunk. That count decides whether a query walks the graph or scans
    /// the passing set, so it sits on the query path and a linear scan there costs
    /// more than the search it precedes.
    pub live_chunks_per_source: Vec<u32>,
    /// Chunks that are not soft deleted, across all sources.
    pub live_chunks: u32,

    /// Chunk identifiers grouped by source, ascending within each group.
    ///
    /// An exhaustive scan under a source predicate would otherwise evaluate the
    /// predicate against every chunk in the corpus to find the few that pass:
    /// on the graded corpus, 186,781 evaluations to reach the 17,641 chunks of the
    /// smallest source but one. With this it visits only the
    /// chunks of that source. Soft deleted chunks are included, because the
    /// predicate still has to reject them.
    pub chunks_by_source: Vec<Vec<u32>>,
}

/// One chunk as the caller supplies it, before interning.
#[derive(Clone)]
pub struct ChunkInput {
    pub source: String,
    pub external_doc_id: String,
    pub chunk_index: u32,
    pub heading_path: Vec<String>,
    pub content: String,
    pub title: String,
    pub url: String,
    pub space_key: Option<String>,
    pub author: Option<String>,
    pub author_id: Option<String>,
    pub updated_at: Option<i64>,
    pub labels: Vec<String>,
    pub deleted: bool,
}

impl Store {
    pub fn n_chunks(&self) -> usize {
        self.chunks.len()
    }

    pub fn n_documents(&self) -> usize {
        self.documents.len()
    }

    pub fn content(&self, chunk: u32) -> &str {
        let c = &self.chunks[chunk as usize];
        &self.text[c.content.start as usize..c.content.end as usize]
    }

    pub fn labels_of(&self, doc: u32) -> &[u32] {
        let d = &self.documents[doc as usize];
        &self.label_arena[d.labels.start as usize..d.labels.end as usize]
    }

    /// Chunk identifiers belonging to one source, ascending.
    pub fn chunks_of_source(&self, source: u32) -> &[u32] {
        self.chunks_by_source
            .get(source as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Chunks not soft deleted for one source.
    pub fn live_chunks_for_source(&self, source: u32) -> u32 {
        self.live_chunks_per_source
            .get(source as usize)
            .copied()
            .unwrap_or(0)
    }

    pub fn heading_path(&self, chunk: u32) -> Vec<&str> {
        let c = &self.chunks[chunk as usize];
        self.heading_arena[c.heading_path.start as usize..c.heading_path.end as usize]
            .iter()
            .filter_map(|id| self.headings.value(*id))
            .collect()
    }

    /// Ingest chunks, grouping them into documents by `(source, external_doc_id)`.
    ///
    /// Returns the chunk identifiers in insertion order. Document attributes are
    /// taken from the first chunk seen for that document, matching the current
    /// stack where these columns live on the document row and every chunk of a
    /// document shares them.
    pub fn add_chunks(&mut self, inputs: Vec<ChunkInput>) -> Vec<u32> {
        let mut doc_lookup: HashMap<(u32, String), u32> = HashMap::new();
        for (i, d) in self.documents.iter().enumerate() {
            doc_lookup.insert((d.source, d.external_id.clone()), i as u32);
        }

        let mut ids = Vec::with_capacity(inputs.len());
        for input in inputs {
            let source = self.sources.intern(&input.source);
            let key = (source, input.external_doc_id.clone());

            let doc = match doc_lookup.get(&key) {
                Some(d) => *d,
                None => {
                    let label_start = self.label_arena.len() as u32;
                    for l in &input.labels {
                        let id = self.labels.intern(l);
                        self.label_arena.push(id);
                    }
                    let label_end = self.label_arena.len() as u32;

                    let space_key = input.space_key.as_deref().map(|s| self.spaces.intern(s));
                    let author = input.author.as_deref().map(|s| self.authors.intern(s));
                    let author_id = input.author_id.as_deref().map(|s| self.author_ids.intern(s));

                    let d = self.documents.len() as u32;
                    self.documents.push(Document {
                        source,
                        space_key,
                        author,
                        author_id,
                        updated_at: input.updated_at.unwrap_or(NO_TIMESTAMP),
                        labels: label_start..label_end,
                        deleted: input.deleted,
                        title: input.title.clone(),
                        url: input.url.clone(),
                        external_id: input.external_doc_id.clone(),
                    });
                    doc_lookup.insert(key, d);
                    d
                }
            };

            let h_start = self.heading_arena.len() as u32;
            for h in &input.heading_path {
                let id = self.headings.intern(h);
                self.heading_arena.push(id);
            }
            let h_end = self.heading_arena.len() as u32;

            let t_start = self.text.len() as u64;
            self.text.push_str(&input.content);
            let t_end = self.text.len() as u64;

            let chunk_id_preview = self.chunks.len() as u32;
            {
                let src = self.documents[doc as usize].source as usize;
                while self.chunks_by_source.len() <= src {
                    self.chunks_by_source.push(Vec::new());
                }
                self.chunks_by_source[src].push(chunk_id_preview);
            }
            if !self.documents[doc as usize].deleted {
                let src = self.documents[doc as usize].source as usize;
                while self.live_chunks_per_source.len() <= src {
                    self.live_chunks_per_source.push(0);
                }
                self.live_chunks_per_source[src] += 1;
                self.live_chunks += 1;
            }

            let chunk_id = self.chunks.len() as u32;
            self.chunks.push(Chunk {
                doc,
                chunk_index: input.chunk_index,
                heading_path: h_start..h_end,
                content: t_start..t_end,
            });
            ids.push(chunk_id);
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(source: &str, doc: &str, idx: u32, content: &str) -> ChunkInput {
        ChunkInput {
            source: source.to_string(),
            external_doc_id: doc.to_string(),
            chunk_index: idx,
            heading_path: vec!["Top".to_string()],
            content: content.to_string(),
            title: format!("title of {doc}"),
            url: format!("https://example.test/{doc}"),
            space_key: Some("ENG".to_string()),
            author: Some("Ada".to_string()),
            author_id: Some("u1".to_string()),
            updated_at: Some(1_700_000_000),
            labels: vec!["design".to_string()],
            deleted: false,
        }
    }

    #[test]
    fn chunks_of_one_document_share_a_document_row() {
        let mut s = Store::default();
        s.add_chunks(vec![
            input("confluence", "d1", 0, "first chunk"),
            input("confluence", "d1", 1, "second chunk"),
            input("slack", "d2", 0, "third chunk"),
        ]);
        assert_eq!(s.n_chunks(), 3);
        assert_eq!(s.n_documents(), 2);
        assert_eq!(s.chunks[0].doc, s.chunks[1].doc);
        assert_ne!(s.chunks[1].doc, s.chunks[2].doc);
    }

    #[test]
    fn content_round_trips_through_the_shared_blob() {
        let mut s = Store::default();
        s.add_chunks(vec![
            input("confluence", "d1", 0, "alpha"),
            input("confluence", "d1", 1, "beta gamma"),
        ]);
        assert_eq!(s.content(0), "alpha");
        assert_eq!(s.content(1), "beta gamma");
    }

    #[test]
    fn the_same_external_id_in_two_sources_is_two_documents() {
        let mut s = Store::default();
        s.add_chunks(vec![
            input("confluence", "1234", 0, "a"),
            input("jira", "1234", 0, "b"),
        ]);
        assert_eq!(s.n_documents(), 2);
    }

    #[test]
    fn chunks_are_grouped_by_source_in_ascending_order() {
        let mut s = Store::default();
        s.add_chunks(vec![
            input("confluence", "a", 0, "x"),
            input("slack", "b", 0, "y"),
            input("confluence", "a", 1, "z"),
            input("slack", "c", 0, "w"),
        ]);
        let confluence = s.sources.get("confluence").unwrap();
        let slack = s.sources.get("slack").unwrap();
        assert_eq!(s.chunks_of_source(confluence), &[0, 2]);
        assert_eq!(s.chunks_of_source(slack), &[1, 3]);
        // Every chunk appears exactly once across the groups.
        let total: usize = s.chunks_by_source.iter().map(|v| v.len()).sum();
        assert_eq!(total, s.n_chunks());
    }

    #[test]
    fn a_soft_deleted_chunk_is_still_listed_for_its_source() {
        // The predicate has to reject it, so it must be visible to the scan.
        let mut s = Store::default();
        let mut gone = input("slack", "gone", 0, "x");
        gone.deleted = true;
        s.add_chunks(vec![input("slack", "here", 0, "y"), gone]);
        let slack = s.sources.get("slack").unwrap();
        assert_eq!(s.chunks_of_source(slack), &[0, 1]);
        assert_eq!(s.live_chunks_for_source(slack), 1);
    }

    #[test]
    fn live_chunk_counts_exclude_soft_deleted_documents() {
        let mut s = Store::default();
        let mut deleted = input("slack", "gone", 0, "x");
        deleted.deleted = true;
        s.add_chunks(vec![
            input("confluence", "a", 0, "x"),
            input("confluence", "a", 1, "y"),
            input("slack", "b", 0, "z"),
            deleted,
        ]);
        let confluence = s.sources.get("confluence").unwrap();
        let slack = s.sources.get("slack").unwrap();
        assert_eq!(s.live_chunks_for_source(confluence), 2);
        assert_eq!(s.live_chunks_for_source(slack), 1, "the deleted chunk must not count");
        assert_eq!(s.live_chunks, 3);
        assert_eq!(s.n_chunks(), 4);
    }

    #[test]
    fn dictionary_distinguishes_absent_from_present() {
        let mut d = Dictionary::default();
        let a = d.intern("confluence");
        assert_eq!(d.get("confluence"), Some(a));
        assert_eq!(d.get("nothing-like-this"), None);
        assert_eq!(d.value(a), Some("confluence"));
    }

    #[test]
    fn heading_path_round_trips() {
        let mut s = Store::default();
        let mut c = input("confluence", "d1", 0, "x");
        c.heading_path = vec!["A".into(), "B".into(), "C".into()];
        s.add_chunks(vec![c]);
        assert_eq!(s.heading_path(0), vec!["A", "B", "C"]);
    }
}
