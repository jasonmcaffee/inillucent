//! Document and chunk storage with dictionary encoded filter columns.
//!
//! Every attribute a filter touches is a fixed width integer here, and the string
//! it stands for is interned once at build time. The filtered vector traversal
//! evaluates the predicate on every node it visits, so predicate evaluation is in
//! the innermost loop; comparing two `u16`s in a packed array is a different cost
//! from comparing two `String`s scattered across the heap.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Range;

use crate::binio;

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

    /// Every interned value containing `needle`, compared without case.
    ///
    /// This is how a substring predicate is answered without putting a substring
    /// comparison in the innermost loop. The baseline writes its sender filter as
    /// `from_address ILIKE '%x%' OR from_name ILIKE '%x%'`, which a dictionary
    /// encoded engine cannot express as an equality — and compiling it to "no
    /// matching id" would silently select nothing. Resolving it here, once per
    /// query against a dictionary of tens of thousands of entries, yields an id
    /// set the hot loop tests with the same integer comparison it already uses.
    ///
    /// Returns an ascending vector, empty when nothing matches - which the caller
    /// must read as "select nothing" rather than "no constraint".
    /// @param needle - the substring to look for, matched case-insensitively
    pub fn find_containing(&self, needle: &str) -> Vec<u32> {
        if needle.is_empty() {
            return Vec::new();
        }
        let lowered = needle.to_lowercase();
        self.values
            .iter()
            .enumerate()
            .filter(|(_, v)| contains_ignoring_case(v, &lowered))
            .map(|(i, _)| i as u32)
            .collect()
    }

    /// Every interned value, in identifier order. The filter drawer a UI builds
    /// reads this back rather than querying the corpus for its own vocabulary.
    pub fn values(&self) -> &[String] {
        &self.values
    }
}

/// Whether `haystack` contains `needle`, which must already be lowercase.
///
/// The obvious implementation lowercases the haystack, which allocates a `String`
/// per candidate - and this runs over an entire dictionary once per query, so on a
/// mail corpus that is a few hundred thousand allocations to answer one filter.
/// An all-ASCII haystack, which nearly every address and display name is, can be
/// compared in place. Anything else falls back to the allocating path rather than
/// guessing at Unicode case folding, because a name with an accent in it has to
/// keep matching.
/// @param haystack - the interned value
/// @param needle - the search text, already lowercased
fn contains_ignoring_case(haystack: &str, needle: &str) -> bool {
    if !haystack.is_ascii() || !needle.is_ascii() {
        return haystack.to_lowercase().contains(needle);
    }
    if needle.len() > haystack.len() {
        return false;
    }
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Sentinel for "this document has no updated_at". Sorts below every real
/// timestamp, so an `updated_after` filter excludes it, matching the SQL
/// behaviour where `NULL >= x` is not true. An `updated_before` filter has to
/// exclude it too, for the same reason: `NULL <= x` is not true either.
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
    /// Slice into `Store::attribute_arena` of `(attribute name, value)` id pairs.
    ///
    /// One arena rather than a map per document: a mail corpus gives every
    /// message a participant list, and 66,000 `HashMap`s cost more in headers
    /// than the ids they hold.
    pub attributes: Range<u32>,
    /// Named boolean flags, one bit each, named by `Store::flag_names`.
    ///
    /// A boolean is not a label. `has_attachment` riding in as a label pollutes
    /// the label vocabulary a filter drawer reads back, and there is no reason to
    /// spend a dictionary entry and an arena slot on one bit.
    pub flags: u32,
    pub deleted: bool,
    /// How many chunks belong to this document, live or not.
    ///
    /// Maintained on insert so that tombstoning a document can decrement the live
    /// counts without scanning for its chunks. Those counts choose between the
    /// graph and an exhaustive scan, so leaving them stale mis-routes queries.
    pub chunk_count: u32,
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
    /// Names of the boolean flags, in bit order.
    pub flag_names: Dictionary,
    /// Names of the multi-valued attribute sets, such as `participant`.
    pub attribute_names: Dictionary,
    /// One value dictionary per attribute set, indexed by attribute name id.
    ///
    /// Separate dictionaries rather than one shared: a substring resolution scans
    /// the dictionary it is given, and scanning every participant address to
    /// answer a question about labels would make the cheap case pay for the
    /// expensive one.
    pub attribute_values: Vec<Dictionary>,

    /// Label identifiers, referenced by `Document::labels`.
    pub label_arena: Vec<u32>,
    /// Attribute name and value id pairs, referenced by `Document::attributes`.
    pub attribute_arena: Vec<(u32, u32)>,
    /// Heading string identifiers, referenced by `Chunk::heading_path`.
    pub heading_arena: Vec<u32>,
    /// All chunk text, contiguous. Chunks hold byte ranges into it.
    pub text: String,
    /// The identifier the source system uses for each chunk, parallel to `chunks`.
    ///
    /// A chunk needs an identity of its own. `Document::url` is a document
    /// attribute, taken from the first chunk seen for that document, so carrying a
    /// chunk identifier there gives every chunk of a document the first one's id -
    /// silently, and only visibly wrong once a caller tries to open the second
    /// chunk of a message. Empty for a corpus whose chunks have no identifier.
    pub chunk_external_ids: Vec<String>,

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
    /// Chunks belonging to a soft deleted document. Live plus deleted is the whole
    /// corpus; the ratio is what decides when a compaction is worth its minutes.
    pub deleted_chunks: u32,

    /// Chunk identifiers grouped by source, ascending within each group.
    ///
    /// An exhaustive scan under a source predicate would otherwise evaluate the
    /// predicate against every chunk in the corpus to find the few that pass:
    /// on the graded corpus, 186,781 evaluations to reach the 17,641 chunks of the
    /// smallest source but one. With this it visits only the
    /// chunks of that source. Soft deleted chunks are included, because the
    /// predicate still has to reject them.
    pub chunks_by_source: Vec<Vec<u32>>,

    /// Source id and external id to document, so an append or a tombstone finds an
    /// existing document without rebuilding a map of the whole corpus.
    ///
    /// Rebuilding it per call was fine while `add_chunks` ran once; an append path
    /// that runs on every sync turns it into 66,000 string clones for the sake of
    /// 181 new chunks. Derived rather than stored, so the on-disk form does not
    /// carry a second copy of every external id.
    ///
    /// It holds only live documents. That is what makes an edit work: tombstone
    /// the old document, append the new chunks, and the append cannot find the
    /// dead row to attach them to, so it opens a fresh one.
    #[serde(skip)]
    doc_lookup: HashMap<(u32, String), u32>,
    /// Whether `doc_lookup` reflects the documents. A store read from disk arrives
    /// with it empty; a corpus with tombstones has fewer entries than documents,
    /// so the two lengths cannot be compared to answer this.
    #[serde(skip)]
    doc_lookup_ready: bool,
}

/// One chunk as the caller supplies it, before interning.
#[derive(Clone, Default)]
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
    /// The identifier the source system uses for this chunk, if it has one.
    pub external_chunk_id: Option<String>,
    pub labels: Vec<String>,
    /// Named multi-valued attributes, such as a participant set of addresses.
    pub attributes: Vec<(String, Vec<String>)>,
    /// Names of the boolean flags that are true for this document.
    pub flags: Vec<String>,
    pub deleted: bool,
}

impl Store {
    pub fn n_chunks(&self) -> usize {
        self.chunks.len()
    }

    pub fn n_documents(&self) -> usize {
        self.documents.len()
    }

    /// The source system's identifier for one chunk, or an empty string when the
    /// corpus supplied none.
    pub fn chunk_external_id(&self, chunk: u32) -> &str {
        self.chunk_external_ids
            .get(chunk as usize)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    pub fn content(&self, chunk: u32) -> &str {
        let c = &self.chunks[chunk as usize];
        &self.text[c.content.start as usize..c.content.end as usize]
    }

    pub fn labels_of(&self, doc: u32) -> &[u32] {
        let d = &self.documents[doc as usize];
        &self.label_arena[d.labels.start as usize..d.labels.end as usize]
    }

    /// The attribute name and value id pairs one document carries.
    pub fn attributes_of(&self, doc: u32) -> &[(u32, u32)] {
        let d = &self.documents[doc as usize];
        &self.attribute_arena[d.attributes.start as usize..d.attributes.end as usize]
    }

    /// The dictionary of values interned for one named attribute set.
    /// @param name - the attribute name, such as participant
    pub fn attribute_dictionary(&self, name: &str) -> Option<&Dictionary> {
        let id = self.attribute_names.get(name)?;
        self.attribute_values.get(id as usize)
    }

    /// The bit one named flag occupies, if the corpus ever set it.
    /// @param name - the flag name, such as has_attachment
    pub fn flag_bit(&self, name: &str) -> Option<u32> {
        self.flag_names.get(name).map(|id| 1u32 << id)
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

    /// Share of the corpus that is tombstoned, from 0 to 1. The trigger a
    /// compaction schedule needs, which the caller cannot compute for itself.
    pub fn deleted_ratio(&self) -> f32 {
        if self.chunks.is_empty() {
            return 0.0;
        }
        self.deleted_chunks as f32 / self.chunks.len() as f32
    }

    pub fn heading_path(&self, chunk: u32) -> Vec<&str> {
        let c = &self.chunks[chunk as usize];
        self.heading_arena[c.heading_path.start as usize..c.heading_path.end as usize]
            .iter()
            .filter_map(|id| self.headings.value(*id))
            .collect()
    }

    /// The live document one source and external id pair names, if there is one.
    ///
    /// A tombstoned document is deliberately not findable: the two callers are a
    /// tombstone, for which "already dead" and "not here" are the same answer, and
    /// an append, which must not attach new chunks to a dead row.
    /// @param source - the source name, before interning
    /// @param external_id - the identifier the source system uses
    pub fn find_document(&mut self, source: &str, external_id: &str) -> Option<u32> {
        let source_id = self.sources.get(source)?;
        self.ensure_doc_lookup();
        self.doc_lookup
            .get(&(source_id, external_id.to_string()))
            .copied()
    }

    /// Marks one document unreachable and corrects the live counts its chunks fed.
    ///
    /// Returns how many chunks stopped being live, which is zero for a document
    /// that was already tombstoned, so a caller can distinguish "removed" from
    /// "was not there". Nothing is erased: the chunks stay in the graph, where
    /// they are still useful stepping stones, and the predicate rejects them.
    /// @param doc - the document identifier
    pub fn tombstone_document(&mut self, doc: u32) -> u32 {
        let Some(document) = self.documents.get_mut(doc as usize) else {
            return 0;
        };
        if document.deleted {
            return 0;
        }
        document.deleted = true;
        let source = document.source as usize;
        let count = document.chunk_count;
        let key = (document.source, document.external_id.clone());
        if let Some(per_source) = self.live_chunks_per_source.get_mut(source) {
            *per_source = per_source.saturating_sub(count);
        }
        self.live_chunks = self.live_chunks.saturating_sub(count);
        self.deleted_chunks += count;
        if self.doc_lookup_ready {
            self.doc_lookup.remove(&key);
        }
        count
    }

    /// Builds the source and external id index over the live documents if it is
    /// not already present.
    ///
    /// Deferred rather than eager because a store read from disk arrives with the
    /// map empty, and a caller that only ever searches never needs it.
    fn ensure_doc_lookup(&mut self) {
        if self.doc_lookup_ready {
            return;
        }
        self.doc_lookup.clear();
        self.doc_lookup.reserve(self.documents.len());
        for (i, d) in self.documents.iter().enumerate() {
            if d.deleted {
                continue;
            }
            self.doc_lookup
                .insert((d.source, d.external_id.clone()), i as u32);
        }
        self.doc_lookup_ready = true;
    }

    /// Ingest chunks, grouping them into documents by source and external id.
    ///
    /// Returns the chunk identifiers in insertion order. Document attributes are
    /// taken from the first chunk seen for that document, matching the current
    /// stack where these columns live on the document row and every chunk of a
    /// document shares them.
    pub fn add_chunks(&mut self, inputs: Vec<ChunkInput>) -> Vec<u32> {
        self.ensure_doc_lookup();

        let mut ids = Vec::with_capacity(inputs.len());
        for input in inputs {
            let source = self.sources.intern(&input.source);
            let key = (source, input.external_doc_id.clone());

            let doc = match self.doc_lookup.get(&key) {
                Some(d) => *d,
                None => {
                    let d = self.push_document(source, &input);
                    // A document that arrives already tombstoned never enters the
                    // lookup, for the same reason one that is tombstoned later
                    // leaves it: nothing may attach live chunks to a dead row.
                    if !input.deleted {
                        self.doc_lookup.insert(key, d);
                    }
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

            let chunk_id = self.chunks.len() as u32;
            {
                let src = self.documents[doc as usize].source as usize;
                while self.chunks_by_source.len() <= src {
                    self.chunks_by_source.push(Vec::new());
                }
                self.chunks_by_source[src].push(chunk_id);
            }
            self.documents[doc as usize].chunk_count += 1;
            if self.documents[doc as usize].deleted {
                self.deleted_chunks += 1;
            } else {
                let src = self.documents[doc as usize].source as usize;
                while self.live_chunks_per_source.len() <= src {
                    self.live_chunks_per_source.push(0);
                }
                self.live_chunks_per_source[src] += 1;
                self.live_chunks += 1;
            }

            self.chunks.push(Chunk {
                doc,
                chunk_index: input.chunk_index,
                heading_path: h_start..h_end,
                content: t_start..t_end,
            });
            self.chunk_external_ids
                .push(input.external_chunk_id.clone().unwrap_or_default());
            ids.push(chunk_id);
        }
        ids
    }

    /// Interns one new document's attributes and appends its row.
    /// @param source - the already interned source id
    /// @param input - the first chunk seen for this document
    fn push_document(&mut self, source: u32, input: &ChunkInput) -> u32 {
        let label_start = self.label_arena.len() as u32;
        for l in &input.labels {
            let id = self.labels.intern(l);
            self.label_arena.push(id);
        }
        let label_end = self.label_arena.len() as u32;

        let attribute_start = self.attribute_arena.len() as u32;
        for (name, values) in &input.attributes {
            let name_id = self.attribute_names.intern(name);
            while self.attribute_values.len() <= name_id as usize {
                self.attribute_values.push(Dictionary::default());
            }
            for value in values {
                let value_id = self.attribute_values[name_id as usize].intern(value);
                self.attribute_arena.push((name_id, value_id));
            }
        }
        let attribute_end = self.attribute_arena.len() as u32;

        let mut flags = 0u32;
        for name in &input.flags {
            let bit = self.flag_names.intern(name);
            assert!(bit < 32, "a store supports at most 32 named flags");
            flags |= 1u32 << bit;
        }

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
            attributes: attribute_start..attribute_end,
            flags,
            deleted: input.deleted,
            chunk_count: 0,
            title: input.title.clone(),
            url: input.url.clone(),
            external_id: input.external_doc_id.clone(),
        });
        d
    }
}


/// One chunk's fixed-width on-disk record.
///
/// Declared as a plain-old-data struct so 598,560 of them are one 19 MB write and
/// one 19 MB read rather than six million field-at-a-time calls.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ChunkRecord {
    doc: u32,
    chunk_index: u32,
    heading_start: u32,
    heading_end: u32,
    content_start: u64,
    content_end: u64,
}

impl Dictionary {
    /// Writes the interned values in identifier order. The lookup is not written:
    /// it is the same information a second time, and rebuilding it on load costs
    /// less than reading it.
    pub fn write_to(&self, w: &mut impl Write) -> std::io::Result<()> {
        binio::write_u64(w, self.values.len() as u64)?;
        for value in &self.values {
            binio::write_str(w, value)?;
        }
        Ok(())
    }

    pub fn read_from(r: &mut impl Read) -> std::io::Result<Dictionary> {
        let n = binio::read_u64(r)? as usize;
        let mut d = Dictionary {
            values: Vec::with_capacity(n),
            lookup: HashMap::with_capacity(n),
        };
        for id in 0..n {
            let value = binio::read_str(r)?;
            d.lookup.insert(value.clone(), id as u32);
            d.values.push(value);
        }
        Ok(d)
    }
}

impl Store {
    /// Writes the store as fixed-width records and contiguous arenas.
    ///
    /// The text arena goes out as its own bytes rather than as an escaped JSON
    /// string, which is the whole reason this exists: it is 450 MB on the real
    /// corpus and JSON made both saving and loading it a parse.
    pub fn write_to(&self, w: &mut impl Write) -> std::io::Result<()> {
        binio::write_u64(w, self.documents.len() as u64)?;
        for d in &self.documents {
            binio::write_u32(w, d.source)?;
            binio::write_u32(w, d.space_key.unwrap_or(binio::NONE_ID))?;
            binio::write_u32(w, d.author.unwrap_or(binio::NONE_ID))?;
            binio::write_u32(w, d.author_id.unwrap_or(binio::NONE_ID))?;
            binio::write_i64(w, d.updated_at)?;
            binio::write_u32(w, d.labels.start)?;
            binio::write_u32(w, d.labels.end)?;
            binio::write_u32(w, d.attributes.start)?;
            binio::write_u32(w, d.attributes.end)?;
            binio::write_u32(w, d.flags)?;
            binio::write_u32(w, d.chunk_count)?;
            w.write_all(&[u8::from(d.deleted)])?;
            binio::write_str(w, &d.title)?;
            binio::write_str(w, &d.url)?;
            binio::write_str(w, &d.external_id)?;
        }

        let records: Vec<ChunkRecord> = self
            .chunks
            .iter()
            .map(|c| ChunkRecord {
                doc: c.doc,
                chunk_index: c.chunk_index,
                heading_start: c.heading_path.start,
                heading_end: c.heading_path.end,
                content_start: c.content.start,
                content_end: c.content.end,
            })
            .collect();
        binio::write_u64(w, records.len() as u64)?;
        w.write_all(bytemuck::cast_slice(&records))?;
        binio::write_u64(w, self.chunk_external_ids.len() as u64)?;
        for id in &self.chunk_external_ids {
            binio::write_str(w, id)?;
        }

        for dictionary in [
            &self.sources,
            &self.spaces,
            &self.authors,
            &self.author_ids,
            &self.labels,
            &self.headings,
            &self.flag_names,
            &self.attribute_names,
        ] {
            dictionary.write_to(w)?;
        }
        binio::write_u64(w, self.attribute_values.len() as u64)?;
        for dictionary in &self.attribute_values {
            dictionary.write_to(w)?;
        }

        binio::write_u32_slice(w, &self.label_arena)?;
        let attributes: Vec<u32> = self
            .attribute_arena
            .iter()
            .flat_map(|(name, value)| [*name, *value])
            .collect();
        binio::write_u32_slice(w, &attributes)?;
        binio::write_u32_slice(w, &self.heading_arena)?;
        binio::write_text(w, &self.text)?;

        binio::write_u32_slice(w, &self.live_chunks_per_source)?;
        binio::write_u32(w, self.live_chunks)?;
        binio::write_u32(w, self.deleted_chunks)?;
        binio::write_u64(w, self.chunks_by_source.len() as u64)?;
        for group in &self.chunks_by_source {
            binio::write_u32_slice(w, group)?;
        }
        Ok(())
    }

    /// Reads a store written by `write_to`.
    pub fn read_from(r: &mut impl Read) -> std::io::Result<Store> {
        let optional = |v: u32| if v == binio::NONE_ID { None } else { Some(v) };

        let n_documents = binio::read_u64(r)? as usize;
        let mut documents = Vec::with_capacity(n_documents);
        for _ in 0..n_documents {
            let source = binio::read_u32(r)?;
            let space_key = optional(binio::read_u32(r)?);
            let author = optional(binio::read_u32(r)?);
            let author_id = optional(binio::read_u32(r)?);
            let updated_at = binio::read_i64(r)?;
            let labels_start = binio::read_u32(r)?;
            let labels_end = binio::read_u32(r)?;
            let attributes_start = binio::read_u32(r)?;
            let attributes_end = binio::read_u32(r)?;
            let flags = binio::read_u32(r)?;
            let chunk_count = binio::read_u32(r)?;
            let mut deleted = [0u8; 1];
            r.read_exact(&mut deleted)?;
            documents.push(Document {
                source,
                space_key,
                author,
                author_id,
                updated_at,
                labels: labels_start..labels_end,
                attributes: attributes_start..attributes_end,
                flags,
                deleted: deleted[0] != 0,
                chunk_count,
                title: binio::read_str(r)?,
                url: binio::read_str(r)?,
                external_id: binio::read_str(r)?,
            });
        }

        let n_chunks = binio::read_u64(r)? as usize;
        let records = binio::read_pod_vec::<ChunkRecord>(r, n_chunks)?;
        let chunks: Vec<Chunk> = records
            .iter()
            .map(|c| Chunk {
                doc: c.doc,
                chunk_index: c.chunk_index,
                heading_path: c.heading_start..c.heading_end,
                content: c.content_start..c.content_end,
            })
            .collect();

        let n_chunk_ids = binio::read_u64(r)? as usize;
        let mut chunk_external_ids = Vec::with_capacity(n_chunk_ids);
        for _ in 0..n_chunk_ids {
            chunk_external_ids.push(binio::read_str(r)?);
        }

        let sources = Dictionary::read_from(r)?;
        let spaces = Dictionary::read_from(r)?;
        let authors = Dictionary::read_from(r)?;
        let author_ids = Dictionary::read_from(r)?;
        let labels = Dictionary::read_from(r)?;
        let headings = Dictionary::read_from(r)?;
        let flag_names = Dictionary::read_from(r)?;
        let attribute_names = Dictionary::read_from(r)?;
        let n_attribute_sets = binio::read_u64(r)? as usize;
        let mut attribute_values = Vec::with_capacity(n_attribute_sets);
        for _ in 0..n_attribute_sets {
            attribute_values.push(Dictionary::read_from(r)?);
        }

        let label_arena = binio::read_u32_vec(r)?;
        let flat = binio::read_u32_vec(r)?;
        let attribute_arena: Vec<(u32, u32)> =
            flat.chunks_exact(2).map(|p| (p[0], p[1])).collect();
        let heading_arena = binio::read_u32_vec(r)?;
        let text = binio::read_text(r)?;

        let live_chunks_per_source = binio::read_u32_vec(r)?;
        let live_chunks = binio::read_u32(r)?;
        let deleted_chunks = binio::read_u32(r)?;
        let n_groups = binio::read_u64(r)? as usize;
        let mut chunks_by_source = Vec::with_capacity(n_groups);
        for _ in 0..n_groups {
            chunks_by_source.push(binio::read_u32_vec(r)?);
        }

        Ok(Store {
            documents,
            chunks,
            sources,
            spaces,
            authors,
            author_ids,
            labels,
            headings,
            flag_names,
            attribute_names,
            attribute_values,
            label_arena,
            attribute_arena,
            heading_arena,
            text,
            chunk_external_ids,
            live_chunks_per_source,
            live_chunks,
            deleted_chunks,
            chunks_by_source,
            doc_lookup: HashMap::new(),
            doc_lookup_ready: false,
        })
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
            external_chunk_id: None,
            labels: vec!["design".to_string()],
            attributes: Vec::new(),
            flags: Vec::new(),
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

    /// A chunk needs an identity of its own. Carrying it on the document row gave
    /// every chunk of a document the first chunk's identifier, which is only
    /// visibly wrong once something tries to open the second chunk of a message.
    #[test]
    fn every_chunk_keeps_its_own_external_identifier() {
        let mut s = Store::default();
        let mut first = input("email", "d1", 0, "one");
        first.external_chunk_id = Some("chunk-a".into());
        let mut second = input("email", "d1", 1, "two");
        second.external_chunk_id = Some("chunk-b".into());
        s.add_chunks(vec![first, second]);

        assert_eq!(s.chunk_external_id(0), "chunk-a");
        assert_eq!(s.chunk_external_id(1), "chunk-b");
        // Both chunks belong to one document, which is exactly the case the
        // document-level field could not express.
        assert_eq!(s.chunks[0].doc, s.chunks[1].doc);
    }

    #[test]
    fn a_corpus_with_no_chunk_identifiers_reports_an_empty_one() {
        let mut s = Store::default();
        s.add_chunks(vec![input("email", "d1", 0, "one")]);
        assert_eq!(s.chunk_external_id(0), "");
        assert_eq!(s.chunk_external_id(99), "");
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
