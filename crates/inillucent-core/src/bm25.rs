//! Inverted index with BM25 ranking.
//!
//! PostgreSQL full text search ranks with `ts_rank_cd`, which has no document length
//! normalization and no term frequency saturation, and it requires every query
//! term to be present because the terms are joined with `&`. On the real corpus
//! "how does offer eligibility work" matches 525 chunks under those semantics
//! against 32,659 that contain `offer` or `eligibility`.
//!
//! BM25 addresses all three. `b` normalizes for length, so a 2,222 character
//! Figma chunk stops outscoring a 509 character JIRA chunk merely by being
//! longer. `k1` saturates frequency. And because `idf` weights rare terms far
//! above common ones, scoring any term rather than requiring all of them does not
//! flood the results.
//!
//! Invariant: **a term is normalized the same way when it is indexed and when
//! it is searched for.** A document indexed under one rule and queried under
//! another finds nothing, and nothing about the failure says why - so both
//! sides call `crate::tokenize` and neither has a rule of its own.

use std::collections::HashMap;

use crate::binio;
use crate::filter::CompiledFilter;
use crate::store::Store;
use crate::tokenize::Tokenizer;

/// BM25's term-frequency saturation constant. Above it, repeating a term in a
/// chunk stops adding much.
pub const K1: f32 = 1.2;

/// BM25's length-normalization constant, from 0 (ignore length) to 1
/// (normalize fully).
pub const B: f32 = 0.75;

/// How many dictionary terms one prefix query term may expand to. Prefix
/// matching is kept because PostgreSQL offers it through `:*` and identifiers matter
/// here, but an unbounded expansion lets one short term dominate the query.
const MAX_PREFIX_EXPANSIONS: usize = 64;

/// How far down the BM25 ranking proximity rescoring reaches, as a multiple of the
/// requested `k`. Deep enough that a hit ranked well below the cut can still be
/// promoted past one above it, shallow enough that the cost stays a rounding error
/// next to the scoring pass.
const RESCORE_DEPTH_FACTOR: usize = 6;

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
/// One term's appearance in one chunk.
pub struct Posting {
    /// Which chunk.
    pub chunk: u32,
    /// Where this posting's token positions start in the index's flat position
    /// array. `term_frequency` is how many of them there are. Flat rather than a
    /// `Vec` per posting because there are 11.7 million postings on this corpus and
    /// eleven million tiny allocations cost more in headers than in positions.
    pub positions_at: u32,
    /// How many times the term appears in the chunk, which is also how many
    /// positions `positions_at` addresses.
    pub term_frequency: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// One chunk the lexical search returned.
pub struct LexicalHit {
    /// The chunk identifier.
    pub chunk: u32,
    /// Its BM25 score.
    pub score: f32,
    /// Share of the query's inverse document frequency mass this chunk holds, in
    /// `[0, 1]`. Carried out of the index rather than recomputed because fusion
    /// and the adaptive weighting both want to know how much of the question a
    /// hit actually answered, and the search already computed it.
    pub coverage: f32,
    /// How many distinct query terms this chunk holds at all.
    pub matched_terms: u32,
}

/// Everything about lexical ranking that is a setting rather than a structure.
///
/// Gathered into one type because the list kept growing: `search` took nine
/// positional arguments, five of which were ranking dials, and a caller could
/// transpose two `f32`s without the compiler noticing. It is `Copy` and its
/// `Default` is the configuration the engine ships, so a caller changing one dial
/// writes `LexicalParams { phrase: 0.5, ..Default::default() }`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LexicalParams {
    /// Whether a query term also matches the terms it prefixes.
    pub prefix: bool,
    /// Exponent on the matched share of the query's idf mass.
    pub coverage: f32,
    /// How much of a score is scaled by how tightly the matched terms sit
    /// together. 0 is bag of words, 1 lets position decide.
    pub proximity: f32,
    /// Rank by how many query terms a chunk holds first, by score second.
    pub tier: bool,
    /// How much of a score is scaled by whether the matched terms appear in the
    /// query's own order, on top of merely appearing close together.
    pub phrase: f32,
    /// How far down the BM25 ranking the position-aware rescoring reaches, as a
    /// multiple of the requested `k`.
    pub rescore_depth_factor: usize,
    /// How much a term occurring in a chunk's heading is worth above the same term
    /// in its body.
    ///
    /// PostgreSQL expresses this as `setweight(to_tsvector(heading), 'A') ||
    /// setweight(to_tsvector(body), 'B')`, so a person's name in a subject line
    /// outranks the same name mentioned in a body. Nothing is *lost* without it -
    /// the heading is part of the chunk text and its terms are indexed - only the
    /// boost. 0 is off, which is what the engine has always done; the coverage and
    /// proximity weighting may already recover most of what the boost was buying,
    /// so this is a dial to measure rather than a default to assume.
    pub heading_boost: f32,
}

impl Default for LexicalParams {
    fn default() -> Self {
        LexicalParams {
            prefix: false,
            coverage: 3.0,
            proximity: 1.0,
            tier: false,
            phrase: 0.0,
            rescore_depth_factor: RESCORE_DEPTH_FACTOR,
            heading_boost: 0.0,
        }
    }
}

/// One term's postings, which may sit in two pieces.
///
/// The flat array holds what was there when it was last built and the overflow
/// holds what has been appended since, so a term written to after a load has
/// both. Both pieces are sorted by chunk identifier and the overflow's chunks
/// are all above the flat piece's, because chunks are only ever appended.
#[derive(Clone, Copy)]
pub(crate) struct PostingList<'a> {
    /// What the flat array holds for this term.
    head: &'a [Posting],
    /// What has been appended to it since the array was built.
    tail: &'a [Posting],
}

impl<'a> PostingList<'a> {
    /// How many postings the term has.
    pub(crate) fn len(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    /// Every posting, in ascending chunk order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &'a Posting> {
        self.head.iter().chain(self.tail.iter())
    }

    /// The posting for one chunk, found by binary search.
    ///
    /// **Two searches rather than one**, because the two pieces are contiguous
    /// separately and not together. Each is sorted, and every chunk in the
    /// overflow is above every chunk in the flat array, so the pair behaves as
    /// one sorted sequence.
    ///
    /// @param chunk - the chunk being looked for
    pub(crate) fn at_chunk(&self, chunk: u32) -> Option<&'a Posting> {
        if let Ok(at) = self.head.binary_search_by_key(&chunk, |p| p.chunk) {
            return self.head.get(at);
        }
        let at = self.tail.binary_search_by_key(&chunk, |p| p.chunk).ok()?;
        self.tail.get(at)
    }
}

impl<'a> IntoIterator for PostingList<'a> {
    type Item = &'a Posting;
    type IntoIter = std::iter::Chain<std::slice::Iter<'a, Posting>, std::slice::Iter<'a, Posting>>;

    fn into_iter(self) -> Self::IntoIter {
        self.head.iter().chain(self.tail.iter())
    }
}

/// The dictionary and its postings, as flat arrays with an overflow map.
///
/// **What this replaces, and what it cost** (task-2066 §4.3.8). It was a
/// `HashMap<String, Vec<Posting>>` beside a `Vec<String>` of the same terms in
/// sorted order. On the 600,589 chunk corpus that is 1,705,097 terms, and the
/// representation charged for each of them three times over: a `String` in the
/// map and a second `String` in the sorted list, a `Vec<Posting>` header and
/// its own heap block however few postings the term has, and a hash map slot
/// with its stored hash and its load factor. `docs/roadmap.md` measured the
/// result at **890.3 MiB resident for 614.8 MiB on disk** - 53% of the whole
/// index's resident bytes, and the largest single part of it.
///
/// Flat arrays charge once. The terms are one byte array with a start per term;
/// the postings are one array with a start per term; a lookup is a binary search
/// over the starts rather than a hash. Nothing is allocated per term at all.
///
/// **The overflow is what makes appending still possible.** A search table is
/// added to after it is loaded, and rebuilding the flat arrays on every append
/// would move every byte of them. So an append to a term already in the arrays
/// goes into `overflow`, keyed by the term's index, and a term seen for the
/// first time goes into `fresh`. Both are folded back in by [`Postings::flatten`],
/// which a save calls. The maps hold what has arrived since the last one, not
/// the corpus.
///
/// `fresh` is a `BTreeMap` rather than a hash map because the dictionary has to
/// be readable in sorted order - a prefix expansion is a binary search and a
/// forward walk - and a sorted container gives that without a second copy of
/// its keys, which is the thing this type exists to stop paying for.
#[derive(Default)]
pub(crate) struct Postings {
    /// Every flat term's bytes, concatenated in sorted order.
    term_bytes: Vec<u8>,
    /// Where each flat term starts in `term_bytes`, with a closing sentinel.
    ///
    /// Empty when there are no flat terms; otherwise `len() == terms + 1`.
    term_starts: Vec<u32>,
    /// Where each flat term's postings start in `flat`, with a closing sentinel.
    posting_starts: Vec<u32>,
    /// Every flat term's postings, in term order and ascending chunk order.
    flat: Vec<Posting>,
    /// Postings appended to a flat term since the arrays were built.
    overflow: HashMap<u32, Vec<Posting>>,
    /// Terms first seen since the arrays were built.
    fresh: std::collections::BTreeMap<String, Vec<Posting>>,
}

impl Postings {
    /// How many distinct terms the dictionary holds.
    pub(crate) fn len(&self) -> usize {
        self.flat_terms().saturating_add(self.fresh.len())
    }

    /// How many term-in-chunk appearances it holds.
    pub(crate) fn total(&self) -> usize {
        self.flat
            .len()
            .saturating_add(self.overflow.values().map(Vec::len).sum::<usize>())
            .saturating_add(self.fresh.values().map(Vec::len).sum::<usize>())
    }

    /// How many terms the flat arrays hold.
    fn flat_terms(&self) -> usize {
        self.term_starts.len().saturating_sub(1)
    }

    /// One flat term's text.
    ///
    /// @param at - the term's index in the flat arrays
    fn flat_term(&self, at: usize) -> &str {
        let from = self.term_starts.get(at).copied().unwrap_or(0) as usize;
        let to = self
            .term_starts
            .get(at.saturating_add(1))
            .copied()
            .unwrap_or(0) as usize;
        self.term_bytes
            .get(from..to)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }

    /// The index of a flat term, or where it would be inserted.
    ///
    /// @param term - the term to look for
    fn locate(&self, term: &str) -> Result<usize, usize> {
        let mut low = 0usize;
        let mut high = self.flat_terms();
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            match self.flat_term(middle).cmp(term) {
                std::cmp::Ordering::Less => low = middle.saturating_add(1),
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(middle),
            }
        }
        Err(low)
    }

    /// One flat term's postings.
    ///
    /// @param at - the term's index in the flat arrays
    fn flat_postings(&self, at: usize) -> &[Posting] {
        let from = self.posting_starts.get(at).copied().unwrap_or(0) as usize;
        let to = self
            .posting_starts
            .get(at.saturating_add(1))
            .copied()
            .unwrap_or(0) as usize;
        self.flat.get(from..to).unwrap_or(&[])
    }

    /// The postings for one term, in both pieces.
    ///
    /// @param term - the analyzed term
    pub(crate) fn get(&self, term: &str) -> Option<PostingList<'_>> {
        match self.locate(term) {
            Ok(at) => Some(PostingList {
                head: self.flat_postings(at),
                tail: self
                    .overflow
                    .get(&(at as u32))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            }),
            Err(_) => self.fresh.get(term).map(|list| PostingList {
                head: list.as_slice(),
                tail: &[],
            }),
        }
    }

    /// Whether the dictionary holds this term at all.
    ///
    /// @param term - the analyzed term
    pub(crate) fn contains(&self, term: &str) -> bool {
        self.locate(term).is_ok() || self.fresh.contains_key(term)
    }

    /// Appends one posting, and says whether the term was new.
    ///
    /// @param term - the analyzed term
    /// @param posting - the posting to append
    pub(crate) fn push(&mut self, term: &str, posting: Posting) -> bool {
        match self.locate(term) {
            Ok(at) => {
                self.overflow.entry(at as u32).or_default().push(posting);
                false
            }
            Err(_) => match self.fresh.get_mut(term) {
                Some(list) => {
                    list.push(posting);
                    false
                }
                None => {
                    self.fresh.insert(term.to_string(), vec![posting]);
                    true
                }
            },
        }
    }

    /// Every term in sorted order, with its postings.
    ///
    /// The flat terms and the fresh ones are each sorted, so this is a merge.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, PostingList<'_>)> {
        let mut flat = 0usize;
        let mut fresh = self.fresh.iter().peekable();
        let total = self.len();
        std::iter::from_fn(move || {
            let next_flat = (flat < self.flat_terms()).then(|| self.flat_term(flat));
            let take_flat = match (next_flat, fresh.peek()) {
                (Some(left), Some((right, _))) => left <= right.as_str(),
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => return None,
            };
            if take_flat {
                let at = flat;
                flat = flat.saturating_add(1);
                return Some((
                    self.flat_term(at),
                    PostingList {
                        head: self.flat_postings(at),
                        tail: self
                            .overflow
                            .get(&(at as u32))
                            .map(Vec::as_slice)
                            .unwrap_or(&[]),
                    },
                ));
            }
            let (term, list) = fresh.next()?;
            Some((
                term.as_str(),
                PostingList {
                    head: list.as_slice(),
                    tail: &[],
                },
            ))
        })
        .take(total)
    }

    /// The terms that begin with a prefix, capped.
    ///
    /// @param prefix - what each term must start with
    /// @param cap - how many to return at most
    pub(crate) fn expand_prefix(&self, prefix: &str, cap: usize) -> Vec<&str> {
        let start = match self.locate(prefix) {
            Ok(at) => at,
            Err(at) => at,
        };
        let mut found: Vec<&str> = Vec::new();
        for at in start..self.flat_terms() {
            let term = self.flat_term(at);
            if !term.starts_with(prefix) {
                break;
            }
            found.push(term);
            if found.len() >= cap {
                return found;
            }
        }
        for term in self.fresh.range(prefix.to_string()..) {
            if !term.0.starts_with(prefix) {
                break;
            }
            found.push(term.0.as_str());
            if found.len() >= cap {
                break;
            }
        }
        found
    }

    /// Appends one term and its postings to the flat arrays, in sorted order.
    ///
    /// The caller supplies the terms already sorted, which is what the file
    /// holds: `write_to` writes them in dictionary order.
    ///
    /// @param term - the term
    /// @param postings - its postings, ascending by chunk
    pub(crate) fn push_flat(&mut self, term: &str, postings: &[Posting]) {
        if self.term_starts.is_empty() {
            self.term_starts.push(0);
            self.posting_starts.push(0);
        }
        self.term_bytes.extend_from_slice(term.as_bytes());
        self.term_starts.push(self.term_bytes.len() as u32);
        self.flat.extend_from_slice(postings);
        self.posting_starts.push(self.flat.len() as u32);
    }

    /// Folds the maps back in once they hold an eighth of the postings.
    ///
    /// **An append must not rebuild, and the maps must not grow without
    /// bound**, and those pull opposite ways. Rebuilding on every append moves
    /// every byte of the flat arrays for one posting; never rebuilding leaves a
    /// `HashMap` and a `BTreeMap` accumulating the per-term cost this type
    /// exists to stop paying. An eighth is the amortised middle: a rebuild
    /// costs `O(n)` and happens once per `n/8` postings added, so an append
    /// pays a small constant, and the maps never hold more than a ninth of the
    /// index.
    ///
    /// The threshold is a share rather than a count because the number that
    /// matters is what the maps cost beside what the arrays cost, and an index
    /// with a thousand postings and one with thirty million both want the same
    /// answer to that.
    pub(crate) fn flatten_if_heavy(&mut self) {
        let in_maps = self
            .overflow
            .values()
            .map(Vec::len)
            .sum::<usize>()
            .saturating_add(self.fresh.values().map(Vec::len).sum::<usize>());
        if in_maps == 0 || in_maps.saturating_mul(8) < self.flat.len() {
            return;
        }
        self.flatten();
    }

    /// Folds the overflow and the fresh terms back into the flat arrays.
    ///
    /// Everything after this is one contiguous run per term again, and the maps
    /// start empty.
    fn flatten(&mut self) {
        if self.overflow.is_empty() && self.fresh.is_empty() {
            return;
        }
        let mut rebuilt = Postings::default();
        let held: Vec<(String, Vec<Posting>)> = self
            .iter()
            .map(|(term, list)| (term.to_string(), list.iter().copied().collect()))
            .collect();
        for (term, postings) in &held {
            rebuilt.push_flat(term, postings);
        }
        *self = rebuilt;
    }
}

/// The inverted index: every stemmed term, the chunks holding it, and where.
#[derive(Default)]
pub struct Bm25Index {
    /// Every stemmed term and its postings, sorted by term and then by chunk.
    ///
    /// See [`Postings`] for what this used to be and what that cost.
    postings: Postings,
    /// Every posting's token positions, concatenated in posting order.
    positions: Vec<u32>,
    chunk_lengths: Vec<u32>,
    /// How many of each chunk's leading tokens came from its heading.
    ///
    /// The heading is the start of the chunk text, so "is this occurrence in the
    /// heading" is "is its position below this number" - which costs one `u32` per
    /// chunk rather than a field tag on all 32 million postings.
    chunk_heading_lengths: Vec<u32>,
    total_length: u64,
    n_chunks: usize,
    /// Every field `index_chunks` has added since [`Self::start_recording`] was
    /// called, or since the last [`Self::drain_recording`] - `None` while
    /// nobody is checkpointing this index, so an ordinary append pays nothing
    /// extra for bookkeeping it will never read. See [`LexicalDelta`].
    recording: Option<Recording>,
}

/// What [`Bm25Index::start_recording`] accumulates before it is drained into
/// a [`LexicalDelta`] with a settled range.
#[derive(Default)]
struct Recording {
    /// The first chunk `index_chunks` was called with while recording, if
    /// it has been called at all.
    start: Option<u32>,
    end: u32,
    chunk_lengths: Vec<u32>,
    chunk_heading_lengths: Vec<u32>,
    positions: Vec<u32>,
    postings: Vec<(String, Posting)>,
}

/// Exactly what one or more calls to `index_chunks` added, in a form that
/// can be replayed onto a *different* `Bm25Index` - one reconstructed from
/// an earlier checkpoint, whose own `positions` array is a different length
/// - without re-tokenising anything.
///
/// This is the lexical half of a segment delta's checkpoint
/// (`inillucent_search::module::SearchTable::continue_merge`,
/// `inillucent_core::persist`'s segment delta format): re-running
/// `index_chunks` on replay would cost what building the lexical index cost
/// in the first place, on every single chain resolution, which is exactly
/// the blow-up `write_latency`'s worst-commit measurement caught before this
/// existed - a chain of even a few links redoing real tokenisation work on
/// every reload rather than copying already-computed postings.
pub struct LexicalDelta {
    /// The chunk range this delta covers.
    pub range: std::ops::Range<u32>,
    /// `chunk_lengths[range]`, in range order.
    pub chunk_lengths: Vec<u32>,
    /// `chunk_heading_lengths[range]`, in range order.
    pub chunk_heading_lengths: Vec<u32>,
    /// The token positions these chunks' postings point into, appended in
    /// the order they were computed.
    pub positions: Vec<u32>,
    /// Every posting added while recording, term first - a new term gets one
    /// entry the first time it is seen and another for every later chunk
    /// that also holds it, exactly as `postings` itself would.
    /// `Posting::positions_at` here is relative to this delta's own
    /// `positions`, not to the index's whole array, since the two are
    /// different lengths at record time and at replay time.
    pub postings: Vec<(String, Posting)>,
}

impl Bm25Index {
    /// Build over every chunk in `store`, in chunk identifier order.
    pub fn build(store: &Store, tokenizer: &Tokenizer) -> Bm25Index {
        let mut idx = Bm25Index::default();
        idx.index_chunks(store, tokenizer, 0..store.n_chunks() as u32);
        idx
    }

    /// Index one contiguous range of chunks, appending to whatever is already
    /// here. Returns how many terms the dictionary had never seen.
    ///
    /// This is the whole of what an append needs, and it needs nothing clever,
    /// because the layout was already append-safe. Postings are sorted by chunk
    /// identifier and new chunks take the next identifiers, so appending keeps
    /// each list sorted; positions live in one flat array addressed by offset and
    /// are appended at the end, so every existing offset stays valid. No byte
    /// already written moves.
    ///
    /// `idf`, the mean length and the chunk count are properties of the whole
    /// index and are updated here rather than kept per segment. That is the reason
    /// this is one growing index and not a base plus a delta merged at query time:
    /// a term that is rare across 598,560 chunks and common across today's 181
    /// would get two incompatible scores, and fusion has nothing to reconcile them
    /// with.
    /// @param store - the store the chunks were added to
    /// @param tokenizer - the analyzer, shared with querying
    /// @param range - the chunk identifiers to index, ascending
    pub fn index_chunks(
        &mut self,
        store: &Store,
        tokenizer: &Tokenizer,
        range: std::ops::Range<u32>,
    ) -> usize {
        let mut added = 0usize;
        if self.chunk_lengths.len() < range.end as usize {
            self.chunk_lengths.resize(range.end as usize, 0);
            self.chunk_heading_lengths.resize(range.end as usize, 0);
        }
        if let Some(recording) = self.recording.as_mut() {
            recording.start.get_or_insert(range.start);
            recording.end = range.end;
        }

        for chunk in range {
            let terms = tokenizer.terms(store.content(chunk).as_ref());
            // Both arrays were grown to cover the range above, so a chunk they
            // do not cover is a chunk this build has no room for; skipping it
            // is what an index would have panicked over (task-1932, H9).
            let length = terms.len() as u32;
            let heading_length = heading_token_count(store, tokenizer, chunk);
            let (Some(length_slot), Some(heading_slot)) = (
                self.chunk_lengths.get_mut(chunk as usize).map(|slot| {
                    *slot = length;
                    length
                }),
                self.chunk_heading_lengths
                    .get_mut(chunk as usize)
                    .map(|slot| {
                        *slot = heading_length;
                        heading_length
                    }),
            ) else {
                continue;
            };
            self.total_length += terms.len() as u64;
            if let Some(recording) = self.recording.as_mut() {
                recording.chunk_lengths.push(length_slot);
                recording.chunk_heading_lengths.push(heading_slot);
            }

            // Positions as well as counts. Term frequency says a chunk mentions two
            // query words; positions say whether it mentions them next to each other,
            // which is the difference between a chunk about the phrase and a chunk
            // that happens to contain both words a paragraph apart.
            let mut occurrences: HashMap<&str, Vec<u32>> = HashMap::new();
            for (at, t) in terms.iter().enumerate() {
                occurrences.entry(t.as_str()).or_default().push(at as u32);
            }
            // Sorted, so a rebuild produces byte identical postings and the term
            // ordering inside one chunk cannot depend on the hash seed.
            let mut ordered: Vec<(&str, Vec<u32>)> = occurrences.into_iter().collect();
            ordered.sort_by(|a, b| a.0.cmp(b.0));
            for (term, at) in ordered {
                let positions_at = self.positions.len() as u32;
                self.positions.extend_from_slice(&at);
                let posting = Posting {
                    chunk,
                    term_frequency: at.len() as u32,
                    positions_at,
                };
                if self.postings.push(term, posting) {
                    added = added.saturating_add(1);
                }
                if let Some(recording) = self.recording.as_mut() {
                    recording.postings.push((
                        term.to_string(),
                        Posting {
                            chunk,
                            term_frequency: at.len() as u32,
                            positions_at: recording.positions.len() as u32,
                        },
                    ));
                    recording.positions.extend_from_slice(&at);
                }
            }
        }

        self.n_chunks = store.n_chunks();
        self.postings.flatten_if_heavy();
        added
    }

    /// Starts recording every field `index_chunks` adds, so a caller can
    /// later replay exactly what happened onto a different instance without
    /// re-tokenising anything. See [`LexicalDelta`].
    pub fn start_recording(&mut self) {
        self.recording.get_or_insert_with(Recording::default);
    }

    /// Returns and clears whatever has been recorded since
    /// [`Self::start_recording`] or the last call to this method, or `None`
    /// when nothing was ever indexed while recording was on.
    pub fn drain_recording(&mut self) -> Option<LexicalDelta> {
        let recording = self.recording.take()?;
        self.recording = Some(Recording::default());
        let start = recording.start?;
        Some(LexicalDelta {
            range: start..recording.end,
            chunk_lengths: recording.chunk_lengths,
            chunk_heading_lengths: recording.chunk_heading_lengths,
            positions: recording.positions,
            postings: recording.postings,
        })
    }

    /// Applies a previously recorded delta directly - extending
    /// `chunk_lengths`, `positions` and each touched term's postings with
    /// exactly the values `index_chunks` computed the first time - instead
    /// of re-tokenising the chunks that produced it.
    ///
    /// This is what makes replaying a segment delta chain cost what copying
    /// bytes costs rather than what building the lexical index cost: an
    /// index_chunks equivalent to the same range would tokenise the same
    /// text again on every single chain resolution.
    /// @param delta - what one checkpoint's own fold added
    pub fn apply_lexical_delta(&mut self, delta: &LexicalDelta) {
        if self.chunk_lengths.len() < delta.range.end as usize {
            self.chunk_lengths.resize(delta.range.end as usize, 0);
            self.chunk_heading_lengths
                .resize(delta.range.end as usize, 0);
        }
        for (offset, length) in delta.chunk_lengths.iter().enumerate() {
            let Some(chunk) = delta.range.start.checked_add(offset as u32) else {
                break;
            };
            if let Some(slot) = self.chunk_lengths.get_mut(chunk as usize) {
                *slot = *length;
            }
            self.total_length += u64::from(*length);
        }
        for (offset, length) in delta.chunk_heading_lengths.iter().enumerate() {
            let Some(chunk) = delta.range.start.checked_add(offset as u32) else {
                break;
            };
            if let Some(slot) = self.chunk_heading_lengths.get_mut(chunk as usize) {
                *slot = *length;
            }
        }
        let positions_base = self.positions.len() as u32;
        self.positions.extend_from_slice(&delta.positions);
        for (term, relative) in &delta.postings {
            let posting = Posting {
                chunk: relative.chunk,
                term_frequency: relative.term_frequency,
                positions_at: positions_base.saturating_add(relative.positions_at),
            };
            self.postings.push(term, posting);
        }
        self.n_chunks = self.n_chunks.max(delta.range.end as usize);
        self.postings.flatten_if_heavy();
    }

    /// Writes the inverted index, so a cold start reads it instead of rebuilding
    /// it.
    ///
    /// It used to be derived on load, on the argument that it is a deterministic
    /// function of data already on disk and recomputing costs less than the disk
    /// it would take. On a 186,000-chunk corpus that held. At 598,560 chunks it is
    /// **26.6 seconds of every start**, because deriving it means running the
    /// analyzer over 450 MB of text, and the file it avoids is a few hundred
    /// megabytes on a machine with terabytes.
    ///
    /// Postings are written in `sorted_terms` order so the reader rebuilds the map
    /// and the sorted list from one pass.
    pub fn write_to(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        binio::write_u64(w, self.n_chunks as u64)?;
        binio::write_u64(w, self.total_length)?;
        binio::write_u32_slice(w, &self.chunk_lengths)?;
        binio::write_u32_slice(w, &self.chunk_heading_lengths)?;
        binio::write_u32_slice(w, &self.positions)?;
        binio::write_u64(w, self.postings.len() as u64)?;
        // The merged order, so the file is written in dictionary order whether
        // or not the arrays have been flattened since the last append. A term
        // whose postings sit in two pieces is written as one run, which is what
        // the reader builds back into one flat entry.
        for (term, list) in self.postings.iter() {
            binio::write_str(w, term)?;
            binio::write_u64(w, list.len() as u64)?;
            for posting in list.iter() {
                w.write_all(bytemuck::bytes_of(posting))?;
            }
        }
        Ok(())
    }

    /// Reads an index written by `write_to`.
    pub fn read_from(r: &mut impl std::io::Read) -> std::io::Result<Bm25Index> {
        let n_chunks = binio::read_u64(r)? as usize;
        let total_length = binio::read_u64(r)?;
        let chunk_lengths = binio::read_u32_vec(r)?;
        let chunk_heading_lengths = binio::read_u32_vec(r)?;
        let positions = binio::read_u32_vec(r)?;
        let n_terms = binio::read_u64(r)? as usize;

        // **Neither reserves from `n_terms`** (task-2066 §4.1.12). It is a raw
        // `u64` out of the segment header with no ceiling and no check against
        // the bytes remaining, and `HashMap::with_capacity` on a hostile one
        // allocates before a single term has been read. Growing as the terms
        // arrive costs a few reallocations on a real index and nothing on a
        // claimed length the file cannot satisfy, which then fails at the first
        // `read_str`.
        let mut postings = Postings::default();
        for _ in 0..n_terms {
            let term = binio::read_str(r)?;
            let count = binio::read_u64(r)? as usize;
            postings.push_flat(&term, &binio::read_pod_vec::<Posting>(r, count)?);
        }
        Ok(Bm25Index {
            postings,
            positions,
            chunk_lengths,
            chunk_heading_lengths,
            total_length,
            n_chunks,
            recording: None,
        })
    }

    /// Returns how many distinct stemmed terms the index holds.
    pub fn n_terms(&self) -> usize {
        self.postings.len()
    }

    /// Returns how many term-in-chunk appearances the index holds.
    pub fn n_postings(&self) -> usize {
        self.postings.total()
    }

    /// Whether the dictionary holds this analyzed term at all. A query term it
    /// does not hold can contribute nothing lexical, which is what the adaptive
    /// weighting wants to know.
    /// @param term - an analyzed query term
    pub fn contains_term(&self, term: &str) -> bool {
        self.postings.contains(term)
    }

    fn mean_length(&self) -> f32 {
        if self.n_chunks == 0 {
            return 0.0;
        }
        self.total_length as f32 / self.n_chunks as f32
    }

    /// `ln(1 + (N - df + 0.5) / (df + 0.5))`, the form that stays positive for
    /// every `df`, unlike the classical form which goes negative once a term
    /// appears in more than half the collection.
    fn idf(&self, document_frequency: usize) -> f32 {
        let n = self.n_chunks as f32;
        let df = document_frequency as f32;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// Terms in the dictionary that begin with `prefix`, capped.
    fn expand_prefix(&self, prefix: &str) -> Vec<&str> {
        self.postings.expand_prefix(prefix, MAX_PREFIX_EXPANSIONS)
    }

    /// Score every chunk matching any query term, return the best `k`.
    ///
    /// `prefix` controls whether each query term also matches the terms it
    /// prefixes. When on, a term's expansions share that term's contribution
    /// rather than each contributing fully, so one short term cannot outvote the
    /// rest of the query.
    ///
    /// `coverage` is the exponent on how much of the query a chunk actually
    /// contains, measured as the share of the query's total inverse document
    /// frequency that the chunk matched. At 0 it is off and this is plain BM25
    /// over any term, which is what the engine shipped with. Above 0 a chunk
    /// holding one word of a six word question is pushed below one holding five,
    /// however often it repeats that word.
    ///
    /// This is the half of PostgreSQL's behaviour worth keeping. `to_tsquery`
    /// joins terms with `&` and so demands all of them, which returns 14 rows of
    /// 50 on this corpus and misses the rest; scoring any term returns a full 50
    /// but lets a chunk that matched only the commonest word sit among them.
    /// Weighting by coverage is the same preference expressed as a gradient
    /// rather than as a gate: everything is still reachable, and the chunks that
    /// answer more of the question rank first.
    /// `proximity` is the other half of what PostgreSQL gets for free. `ts_rank_cd`
    /// is cover density ranking: it rewards a chunk whose query terms sit close
    /// together, so a chunk that is *about* the phrase outranks one that mentions
    /// the same words in different paragraphs. BM25 has no notion of where a term
    /// occurred, and on this corpus that is exactly where the two engines diverged:
    /// inillucent found more of the right chunks (success@10 0.90 against 0.80) and put
    /// them lower (success@1 0.52 against 0.62).
    ///
    /// Only the leading candidates are rescored. Computing a covering window costs
    /// more than a dot product and almost every chunk BM25 scored is not going to be
    /// returned, so the window is computed for `RESCORE_DEPTH` chunks and the rest
    /// keep their BM25 order, which they were going to keep anyway.
    /// `tier` is the strongest form of the same idea as `coverage`. PostgreSQL joins
    /// query terms with `&`, so a chunk missing one word is not a worse answer, it is
    /// not an answer: it never appears. That is a very effective prior on a corpus
    /// this size, where thousands of chunks contain *some* of any question. Tiering
    /// reproduces it without losing the recall: chunks holding every term come first,
    /// then chunks holding all but one, and so on, so a query whose terms nothing
    /// holds together still returns its best partial matches instead of nothing.
    /// `phrase` is the strictest position feature of the three. Proximity asks how
    /// wide the smallest window holding the matched terms is; phrase asks whether
    /// those terms appear inside that window in the order the query wrote them.
    /// "offer eligibility rules" and "rules for eligibility of an offer" have the
    /// same window width and are not the same answer.
    /// @param query - the raw query text
    /// @param store - chunk metadata, read by the filter
    /// @param filter - the compiled predicate
    /// @param tokenizer - the analyzer, shared with indexing
    /// @param k - how many hits to return
    /// @param params - the ranking dials, all of which are settings rather than
    ///   properties of the built index
    pub fn search(
        &self,
        query: &str,
        store: &Store,
        filter: &CompiledFilter,
        tokenizer: &Tokenizer,
        k: usize,
        params: LexicalParams,
    ) -> Vec<LexicalHit> {
        // `proximity`, `phrase` and `rescore_depth_factor` are `top_k`'s, which takes
        // `params` whole rather than three more arguments.
        let LexicalParams {
            prefix,
            coverage,
            tier,
            heading_boost,
            ..
        } = params;
        if k == 0 || filter.is_dead() || self.n_chunks == 0 {
            return Vec::new();
        }
        let query_terms = tokenizer.query_terms(query);
        if query_terms.is_empty() {
            return Vec::new();
        }

        let mean_len = self.mean_length();
        let trivial = filter.is_trivial();
        // Per chunk: the BM25 score, and how much of the query's idf mass it holds.
        // Per chunk: the BM25 score, how much of the query's idf mass it holds, and
        // how many distinct query terms it holds at all.
        let mut scores: HashMap<u32, (f32, f32, u32)> = HashMap::new();
        let mut total_mass = 0.0f32;
        // One query term at a time, so a chunk that matched the same term through
        // several prefix expansions counts that term's mass once rather than once
        // per expansion.
        let mut per_term: HashMap<u32, f32> = HashMap::new();

        for qt in &query_terms {
            // Each distinct surface term contributes once per matching variant,
            // weighted so the whole term contributes at most as much as an exact
            // term would.
            let variants: Vec<&str> = if prefix {
                let mut v = self.expand_prefix(qt);
                if v.is_empty() && self.postings.contains(qt.as_str()) {
                    v.push(qt.as_str());
                }
                v
            } else if self.postings.contains(qt.as_str()) {
                vec![qt.as_str()]
            } else {
                vec![]
            };
            if variants.is_empty() {
                continue;
            }

            // The term's own weight for coverage. A query term matches if any of
            // its variants does, so its effective document frequency is the size
            // of the union, of which the sum is the upper bound.
            let union_df: usize = variants
                .iter()
                .filter_map(|v| self.postings.get(*v))
                .map(|p| p.len())
                .sum();
            let term_mass = self.idf(union_df.min(self.n_chunks));
            total_mass += term_mass;

            per_term.clear();
            // An exact match on the query term should not be diluted by its own
            // expansions, so it keeps full weight and expansions share the rest.
            for variant in &variants {
                let Some(postings) = self.postings.get(*variant) else {
                    continue;
                };
                let weight = if *variant == qt.as_str() {
                    1.0
                } else {
                    1.0 / variants.len() as f32
                };
                let idf = self.idf(postings.len());
                for p in postings.iter() {
                    if !trivial && !filter.passes(p.chunk, store) {
                        continue;
                    }
                    let tf = p.term_frequency as f32;
                    let len = self
                        .chunk_lengths
                        .get(p.chunk as usize)
                        .copied()
                        .unwrap_or(0) as f32;
                    let norm = if mean_len > 0.0 { len / mean_len } else { 1.0 };
                    let mut contribution =
                        idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * norm));
                    if heading_boost > 0.0 {
                        contribution *= 1.0 + heading_boost * self.heading_share(p);
                    }
                    *per_term.entry(p.chunk).or_insert(0.0) += weight * contribution;
                }
            }
            for (chunk, contribution) in per_term.drain() {
                let entry = scores.entry(chunk).or_insert((0.0, 0.0, 0));
                entry.0 += contribution;
                entry.1 += term_mass;
                entry.2 += 1;
            }
        }

        // The tier is carried alongside the hit rather than folded into the score,
        // because folding it in would need a constant bigger than any possible score
        // difference, and there is no such constant that is also safe.
        let mut tiers: HashMap<u32, u32> = HashMap::new();
        let hits: Vec<LexicalHit> = scores
            .into_iter()
            .map(|(chunk, (score, mass, matched))| {
                tiers.insert(chunk, matched);
                let share = if total_mass > 0.0 {
                    (mass / total_mass).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let scaled = if coverage <= 0.0 {
                    score
                } else {
                    score * share.powf(coverage)
                };
                LexicalHit {
                    chunk,
                    score: scaled,
                    coverage: share,
                    matched_terms: matched,
                }
            })
            .collect();

        self.top_k(hits, k, &query_terms, &tiers, tier, params)
    }

    /// Orders the answer and returns the `k` rows that reach the top.
    ///
    /// Split out of [`Bm25Index::search`] in task-2006, which the argument below took
    /// past its recorded length. The ratchet in `policy.rs` asks for an extraction
    /// rather than a raised number, and this is the whole of one step: everything after
    /// the scores exist.
    ///
    /// @param hits - one hit per chunk that matched, in whatever order the score map
    ///   produced
    /// @param k - how many rows the caller asked for
    /// @param query_terms - the query's terms, for the proximity rescore
    /// @param tiers - how many query terms each chunk held, for the tiered order
    /// @param tier - whether to order by that count first
    /// @param params - the ranking knobs, for the proximity and phrase weights
    fn top_k(
        &self,
        mut hits: Vec<LexicalHit>,
        k: usize,
        query_terms: &[String],
        tiers: &HashMap<u32, u32>,
        tier: bool,
        params: LexicalParams,
    ) -> Vec<LexicalHit> {
        let LexicalParams {
            proximity,
            phrase,
            rescore_depth_factor,
            ..
        } = params;
        // Most query terms held first when tiering, then descending score, then
        // ascending chunk, so the order is total and stable.
        let order = |a: &LexicalHit, b: &LexicalHit| {
            let by_tier = if tier {
                tiers.get(&b.chunk).cmp(&tiers.get(&a.chunk))
            } else {
                std::cmp::Ordering::Equal
            };
            by_tier
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then(a.chunk.cmp(&b.chunk))
        };
        // **The rows that reach the top are selected rather than sorted** (task-2000,
        // design 9). A term that occurs in a tenth of the corpus produces a hit per
        // chunk it occurs in, and the answer is `k` of them - so a full sort orders
        // thousands of rows to throw all but ten away. `select_nth_unstable_by` puts
        // the top `reach` at the front in linear time and leaves them unordered, which
        // is all either branch below needs: the rescore reads the head as a **set** and
        // scores each hit from its own positions, and whichever sort follows is the one
        // that orders the answer.
        //
        // The answer is the same answer. `order` is total - most query terms, then
        // descending score, then ascending chunk, and a hit is one chunk - so the top
        // `reach` is one set rather than a choice between ties, and the sort that
        // follows the selection sees exactly the rows the old full sort would have put
        // in front of it. The score card agrees: its ranking verdicts are byte for byte
        // what they were, 15 better, 1 equivalent, 1 inconclusive, 0 worse, every
        // correctness gate passing.
        //
        // **And it does not move the number design 9 aimed it at.** The hybrid search
        // with a title as the query reads **4.629 ms against 4.636** over the graded
        // corpus, which is nothing, against a 3.5 ms target. So the sort was not what
        // that query was waiting for: the two legs run under `rayon::join` and the
        // vector leg's p50 is 0.58 ms, so the 4.6 ms is the lexical leg, and it is in
        // reading the postings and scoring them rather than in ordering the result. It
        // is kept because selecting is strictly less work than sorting for the same
        // answer, and it claims no ratio.
        let rescoring = (proximity > 0.0 || phrase > 0.0) && query_terms.len() > 1;
        let reach = match rescoring {
            true => k
                .saturating_mul(rescore_depth_factor.max(1))
                .min(hits.len()),
            false => k.min(hits.len()),
        };
        if reach > 0 && reach < hits.len() {
            hits.select_nth_unstable_by(reach.saturating_sub(1), &order);
        }
        // Proximity is applied AFTER the ranking exists, to the hits that ranking put
        // in reach of the top k, and the result is reordered. Applying it to whatever
        // order the score map happened to produce would rescore an arbitrary subset.
        if rescoring {
            if let Some(head) = hits.get_mut(..reach) {
                self.rescore_by_position(head, query_terms, proximity, phrase);
            }
            // **The whole vector, not the head.** A rescore can lower a head hit's
            // score below a hit it was ahead of, and the one it was ahead of has not
            // been rescored, so the comparison that decides the answer is between all
            // of them. Sorting only the head would keep a demoted hit in the answer.
            hits.sort_by(&order);
        } else {
            // Nothing moved, so only the selected rows need ordering.
            if let Some(head) = hits.get_mut(..reach) {
                head.sort_by(&order);
            }
            hits.truncate(reach);
        }
        hits.truncate(k);
        hits
    }

    /// What share of one posting's occurrences fell inside its chunk's heading.
    ///
    /// Computed from the positions the index already records rather than from a
    /// per-posting field tag, because the heading is a prefix of the chunk text and
    /// so is exactly the positions below the heading length.
    /// @param posting - the posting to weigh
    fn heading_share(&self, posting: &Posting) -> f32 {
        let heading_length = self
            .chunk_heading_lengths
            .get(posting.chunk as usize)
            .copied()
            .unwrap_or(0);
        if heading_length == 0 || posting.term_frequency == 0 {
            return 0.0;
        }
        let start = posting.positions_at as usize;
        let end = start.saturating_add(posting.term_frequency as usize);
        let inside = self
            .positions
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .filter(|at| **at < heading_length)
            .count();
        inside as f32 / posting.term_frequency as f32
    }

    /// The largest score `search` could hand back for this query, if some chunk
    /// held every query term at maximum term frequency and minimum length.
    ///
    /// This is the ceiling `Fusion::TheoreticalMinMax` needs. Per-list min-max
    /// normalization maps the best hit of every non-flat list to exactly 1.0, so a
    /// hopeless lexical list is presented to fusion as confidently as a perfect
    /// one and the fused score carries no absolute meaning. Scaling by a bound
    /// that does not depend on the results keeps a weak list weak, which is what
    /// lets an unanswerable query be recognised as unanswerable.
    ///
    /// The bound is `sum over query terms of idf * (k1 + 1)`, which is where the
    /// BM25 term saturates as term frequency grows without limit. Coverage,
    /// proximity and phrase weighting all multiply by a factor in `[0, 1]`, so
    /// they cannot push a score above it.
    /// @param query - the raw query text
    /// @param tokenizer - the analyzer, shared with indexing
    /// @param prefix - whether a query term also matches the terms it prefixes,
    ///   which changes the document frequency each term is weighted by
    pub fn score_ceiling(&self, query: &str, tokenizer: &Tokenizer, prefix: bool) -> f32 {
        let mut ceiling = 0.0f32;
        for qt in tokenizer.query_terms(query) {
            let union_df: usize = if prefix {
                self.expand_prefix(&qt)
                    .iter()
                    .filter_map(|v| self.postings.get(*v))
                    .map(|p| p.len())
                    .sum()
            } else {
                self.postings.get(qt.as_str()).map(|p| p.len()).unwrap_or(0)
            };
            if union_df == 0 {
                continue;
            }
            ceiling += self.idf(union_df.min(self.n_chunks)) * (K1 + 1.0);
        }
        ceiling
    }

    /// Rescales the hits it is given by how tightly their matched query terms sit
    /// together.
    ///
    /// The factor is `matched terms / width of the smallest window holding one of
    /// each`, which is 1 for an exact phrase and falls towards 0 as the terms spread
    /// out. It is blended rather than multiplied in, so `proximity` is a dial from
    /// "ignore position" to "position decides", and a chunk matching one term is
    /// never punished for a proximity it cannot have.
    ///
    /// The caller passes only the leaders, because computing a covering window costs
    /// more than scoring does and almost every chunk BM25 scored was never going to
    /// be returned.
    /// `phrase` blends in a second, stricter factor: the longest run of query
    /// terms that occur in the query's own order with nothing of the query
    /// between them, as a share of the matched terms. A chunk containing the
    /// query as a phrase scores 1 on it; a chunk holding the same words scattered
    /// and reordered scores close to 0. Order is the part of a question that
    /// survives paraphrase least well, so it is a separate dial from width rather
    /// than folded into it.
    /// @param hits - the leaders, rescored in place
    /// @param query_terms - the analyzed query
    /// @param proximity - the blend weight for window width
    /// @param phrase - the blend weight for in-order runs
    fn rescore_by_position(
        &self,
        hits: &mut [LexicalHit],
        query_terms: &[String],
        proximity: f32,
        phrase: f32,
    ) {
        // One position list per query term, reused across chunks. `ordered` keeps
        // the same lists paired with the term's place in the query, which is what
        // the in-order run needs and the window width does not.
        let mut lists: Vec<&[u32]> = Vec::with_capacity(query_terms.len());
        let mut ordered: Vec<(usize, &[u32])> = Vec::with_capacity(query_terms.len());
        for hit in hits.iter_mut() {
            lists.clear();
            ordered.clear();
            for (at, term) in query_terms.iter().enumerate() {
                if let Some(slice) = self.positions_of(term, hit.chunk) {
                    lists.push(slice);
                    ordered.push((at, slice));
                }
            }
            if lists.len() < 2 {
                continue;
            }
            if proximity > 0.0 {
                if let Some(span) = smallest_window(&lists) {
                    let tightness = (lists.len() as f32 / span as f32).clamp(0.0, 1.0);
                    hit.score *= 1.0 - proximity + proximity * tightness;
                }
            }
            if phrase > 0.0 {
                let run = longest_ordered_run(&ordered);
                let share = (run as f32 / lists.len() as f32).clamp(0.0, 1.0);
                hit.score *= 1.0 - phrase + phrase * share;
            }
        }
    }

    /// Where `term` occurs inside `chunk`, or `None` if it does not occur there.
    ///
    /// Postings are appended in ascending chunk order by `build`, so this is a
    /// binary search rather than a scan.
    /// @param term - an analyzed query term
    /// @param chunk - the chunk being rescored
    fn positions_of(&self, term: &str, chunk: u32) -> Option<&[u32]> {
        let postings = self.postings.get(term)?;
        let p = postings.at_chunk(chunk)?;
        let start = p.positions_at as usize;
        self.positions
            .get(start..start.saturating_add(p.term_frequency as usize))
    }
}

/// The longest run of matched query terms that occur in the chunk in the order
/// the query wrote them, allowing other words in between.
///
/// Each term is given the position of its first occurrence at or after the
/// previous term's chosen position, which is the greedy earliest-match a phrase
/// search does. A run breaks when a term has no occurrence after the one before
/// it, and the walk restarts from that term, so "eligibility offer rules" scores
/// a run of two rather than one.
///
/// Terms the chunk does not hold are simply absent from `ordered`, so a chunk
/// matching terms one and three of a three word query can still score a run of
/// two: it is being asked whether what it did match came in order, not whether it
/// matched everything. Coverage weighting is what judges the latter.
/// @param ordered - matched query terms as (place in the query, ascending position list)
fn longest_ordered_run(ordered: &[(usize, &[u32])]) -> usize {
    if ordered.len() < 2 {
        return ordered.len();
    }
    let mut best = 1usize;
    let mut run = 1usize;
    // The position the previous term of the current run was matched at. A term
    // with no positions is not a matched term, so the caller never supplies one
    // - and answering `ordered.len()` for the shape that cannot be walked is
    // the same answer the length check above gives (task-1932, H9).
    let Some(&(_, first_positions)) = ordered.first() else {
        return ordered.len();
    };
    let Some(&start) = first_positions.first() else {
        return ordered.len();
    };
    let mut previous = start;
    for window in ordered.windows(2) {
        let Some((_, next_positions)) = window.get(1) else {
            continue;
        };
        match next_positions.iter().copied().find(|p| *p > previous) {
            Some(p) => {
                run += 1;
                previous = p;
            }
            None => {
                run = 1;
                let Some(&restart) = next_positions.first() else {
                    continue;
                };
                previous = restart;
            }
        }
        best = best.max(run);
    }
    best
}

/// The width of the smallest window of token positions holding one occurrence of
/// every list, or `None` if any list is empty.
///
/// The classic sweep: hold one cursor per list, take the window between the
/// smallest and largest cursor, then advance the smallest. Every window that could
/// be smallest is considered exactly once, so this is linear in the total number of
/// positions rather than exponential in the number of terms.
/// @param lists - one ascending position list per matched query term
fn smallest_window(lists: &[&[u32]]) -> Option<u32> {
    if lists.iter().any(|l| l.is_empty()) {
        return None;
    }
    let mut cursors = vec![0usize; lists.len()];
    let mut best = u32::MAX;
    loop {
        let mut lowest = 0usize;
        let mut low = u32::MAX;
        let mut high = 0u32;
        for (i, list) in lists.iter().enumerate() {
            // Every cursor starts at zero and is advanced only while it is
            // inside its list, so this is always there; `get` says so in the
            // form the compiler keeps (task-1932, H9).
            let Some(v) = cursors.get(i).and_then(|at| list.get(*at)).copied() else {
                return Some(best);
            };
            if v < low {
                low = v;
                lowest = i;
            }
            high = high.max(v);
        }
        best = best.min(high.saturating_sub(low).saturating_add(1));
        let (Some(cursor), Some(list)) = (cursors.get_mut(lowest), lists.get(lowest)) else {
            return Some(best);
        };
        *cursor = cursor.saturating_add(1);
        if *cursor >= list.len() {
            return Some(best);
        }
    }
}

/// How many tokens one chunk's heading contributes to the front of its text.
///
/// Nikaya writes a chunk as its heading followed by its body, which is the same
/// arrangement PostgreSQL weights with `setweight`. Tokenizing the heading on its
/// own gives the count because the tokenizer splits on whitespace and the join
/// between heading and body is whitespace, so the terms of the whole are the terms
/// of the heading followed by the terms of the body.
/// @param store - the store holding the chunk
/// @param tokenizer - the analyzer, shared with querying
/// @param chunk - the chunk identifier
fn heading_token_count(store: &Store, tokenizer: &Tokenizer, chunk: u32) -> u32 {
    let heading = store.heading_path(chunk).join(" ");
    if heading.is_empty() {
        return 0;
    }
    tokenizer.terms(&heading).len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Filter;
    use crate::store::{ChunkInput, Store};

    fn store_of(contents: &[(&str, &str)]) -> Store {
        let mut s = Store::default();
        let inputs = contents
            .iter()
            .enumerate()
            .map(|(i, (source, content))| ChunkInput {
                source: source.to_string(),
                external_doc_id: format!("d{i}"),
                chunk_index: 0,
                heading_path: vec![],
                content: content.to_string(),
                title: format!("t{i}"),
                url: format!("u{i}"),
                space_key: None,
                author: None,
                author_id: None,
                updated_at: Some(i as i64),
                external_chunk_id: None,
                labels: vec![],
                attributes: Vec::new(),
                flags: Vec::new(),
                deleted: false,
            })
            .collect();
        s.add_chunks(inputs).expect("the chunks are added");
        s
    }

    /// One chunk whose text begins with its heading, which is how a mail corpus
    /// writes them and what the heading boost reads.
    fn headed(doc: &str, heading: &str, body: &str) -> ChunkInput {
        ChunkInput {
            source: "email".into(),
            external_doc_id: doc.into(),
            heading_path: vec![heading.into()],
            content: format!("{heading}\n\n{body}"),
            title: heading.into(),
            url: format!("u/{doc}"),
            ..Default::default()
        }
    }

    fn run(store: &Store, query: &str, k: usize) -> Vec<u32> {
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(store, &tok);
        let f = CompiledFilter::compile(&Filter::default(), store);
        idx.search(
            query,
            store,
            &f,
            &tok,
            k,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        )
        .into_iter()
        .map(|h| h.chunk)
        .collect()
    }

    #[test]
    fn ranks_the_chunk_containing_the_query_term_first() {
        let s = store_of(&[
            ("confluence", "nothing relevant at all here"),
            ("confluence", "offer eligibility rules for members"),
            ("confluence", "unrelated content about invoices"),
        ]);
        assert_eq!(run(&s, "offer eligibility", 3)[0], 1);
    }

    /// The weakness in PostgreSQL full text search: it requires every term.
    #[test]
    fn a_chunk_matching_some_terms_is_still_returned() {
        let s = store_of(&[
            ("confluence", "offer rules"),
            ("confluence", "completely different subject"),
        ]);
        // Only `offer` is present; `eligibility` is absent from every chunk.
        let hits = run(&s, "offer eligibility work", 5);
        assert_eq!(hits, vec![0], "a partial match must still be returned");
    }

    #[test]
    fn length_normalization_prefers_the_shorter_chunk_at_equal_term_counts() {
        let long_tail = "filler ".repeat(300);
        let s = store_of(&[
            ("confluence", &format!("offer {long_tail}")),
            ("confluence", "offer"),
        ]);
        let hits = run(&s, "offer", 2);
        assert_eq!(
            hits[0], 1,
            "the short chunk should win on length normalization"
        );
    }

    #[test]
    fn term_frequency_saturates() {
        // Ten occurrences must not score ten times one occurrence.
        let s = store_of(&[
            (
                "confluence",
                "offer offer offer offer offer offer offer offer offer offer",
            ),
            ("confluence", "offer"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let hits = idx.search(
            "offer",
            &s,
            &f,
            &tok,
            2,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        let many = hits.iter().find(|h| h.chunk == 0).unwrap().score;
        let one = hits.iter().find(|h| h.chunk == 1).unwrap().score;
        assert!(
            many < one * 10.0,
            "frequency did not saturate: {many} vs {one}"
        );
    }

    #[test]
    fn a_rare_term_outweighs_a_common_one() {
        let mut contents: Vec<(&str, &str)> = vec![("confluence", "common word everywhere")];
        for _ in 0..200 {
            contents.push(("confluence", "common word everywhere"));
        }
        contents.push(("confluence", "common word everywhere plus tirzepatide"));
        let s = store_of(&contents);
        let hits = run(&s, "common tirzepatide", 3);
        assert_eq!(
            hits[0] as usize,
            contents.len() - 1,
            "the chunk with the rare term should rank first"
        );
    }

    #[test]
    fn stemming_lets_a_query_match_an_inflected_form() {
        let s = store_of(&[(
            "confluence",
            "the offering was redeemed by eligible members",
        )]);
        assert_eq!(run(&s, "offer redeem eligibility", 5), vec![0]);
    }

    #[test]
    fn honours_the_filter() {
        let s = store_of(&[
            ("confluence", "offer eligibility"),
            ("slack", "offer eligibility"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::source("slack"), &s);
        let hits = idx.search(
            "offer eligibility",
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        assert_eq!(hits.iter().map(|h| h.chunk).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn a_query_of_only_stopwords_returns_nothing() {
        let s = store_of(&[("confluence", "offer eligibility")]);
        assert!(run(&s, "how do i the of and", 5).is_empty());
    }

    /// PostgreSQL puts a subject line in weight class A and a body in class B, so
    /// a person's name in a subject outranks the same name in a body. With the
    /// boost off the two are indistinguishable, which is the gap this closes.
    #[test]
    fn a_heading_term_can_be_weighted_above_the_same_term_in_a_body() {
        let mut store = Store::default();
        store
            .add_chunks(vec![
                headed(
                    "d1",
                    "Terri Shaw tax return",
                    "please find the attached document",
                ),
                headed(
                    "d2",
                    "meeting notes",
                    "we discussed the Terri Shaw tax return at length",
                ),
            ])
            .expect("the chunks are added");

        let tokenizer = Tokenizer::default();
        let index = Bm25Index::build(&store, &tokenizer);
        let filter = CompiledFilter::compile(&Filter::default(), &store);

        let unweighted = index.search(
            "Terri Shaw",
            &store,
            &filter,
            &tokenizer,
            10,
            LexicalParams::default(),
        );
        let weighted = index.search(
            "Terri Shaw",
            &store,
            &filter,
            &tokenizer,
            10,
            LexicalParams {
                heading_boost: 3.0,
                ..Default::default()
            },
        );
        assert_eq!(weighted.len(), 2, "both chunks still match");
        assert_eq!(
            weighted[0].chunk, 0,
            "the heading match should lead: {weighted:?}"
        );
        let gap = |hits: &[LexicalHit]| {
            let a = hits.iter().find(|h| h.chunk == 0).unwrap().score;
            let b = hits.iter().find(|h| h.chunk == 1).unwrap().score;
            a - b
        };
        assert!(
            gap(&weighted) > gap(&unweighted),
            "the boost did not widen the gap: {:?} against {:?}",
            gap(&weighted),
            gap(&unweighted)
        );
    }

    #[test]
    fn the_heading_boost_is_off_by_default_and_changes_nothing() {
        let mut store = Store::default();
        store
            .add_chunks(vec![headed(
                "d1",
                "Terri Shaw tax return",
                "the body mentions Terri Shaw again",
            )])
            .expect("the chunks are added");
        let tokenizer = Tokenizer::default();
        let index = Bm25Index::build(&store, &tokenizer);
        let filter = CompiledFilter::compile(&Filter::default(), &store);
        let a = index.search(
            "Terri Shaw",
            &store,
            &filter,
            &tokenizer,
            10,
            LexicalParams::default(),
        );
        let b = index.search(
            "Terri Shaw",
            &store,
            &filter,
            &tokenizer,
            10,
            LexicalParams {
                heading_boost: 0.0,
                ..Default::default()
            },
        );
        assert_eq!(a[0].score.to_bits(), b[0].score.to_bits());
    }

    #[test]
    fn an_empty_query_returns_nothing() {
        let s = store_of(&[("confluence", "offer")]);
        assert!(run(&s, "", 5).is_empty());
    }

    #[test]
    fn a_term_absent_from_the_corpus_returns_nothing() {
        let s = store_of(&[("confluence", "offer eligibility")]);
        assert!(run(&s, "tirzepatide", 5).is_empty());
    }

    #[test]
    fn prefix_matching_finds_a_longer_term() {
        let s = store_of(&[("confluence", "eligibility rules")]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        // "elig" is the stem of eligibility, so an exact search already matches.
        // Use a genuine prefix of the stem to exercise expansion.
        let hits = idx.search(
            "eli",
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: true,
                coverage: 0.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        assert_eq!(hits.len(), 1);
        assert!(idx
            .search(
                "eli",
                &s,
                &f,
                &tok,
                5,
                LexicalParams {
                    prefix: false,
                    coverage: 0.0,
                    proximity: 0.0,
                    tier: false,
                    ..Default::default()
                }
            )
            .is_empty());
    }

    #[test]
    fn results_are_ordered_by_descending_score() {
        let s = store_of(&[
            ("confluence", "offer"),
            ("confluence", "offer offer eligibility"),
            ("confluence", "offer eligibility"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let hits = idx.search(
            "offer eligibility",
            &s,
            &f,
            &tok,
            3,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[test]
    fn index_statistics_are_reported() {
        let s = store_of(&[
            ("confluence", "offer eligibility rules"),
            ("slack", "offer"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        assert_eq!(idx.n_terms(), 3); // offer, elig, rule
        assert_eq!(idx.n_postings(), 4);
    }

    /// Coverage weighting exists for the chunk that repeats one common query word
    /// and knows nothing about the rest of the question. Whether that chunk wins
    /// without the weighting depends on the collection statistics; what has to hold
    /// is that the weighting moves the complete match up, and moves it up enough to
    /// lead.
    #[test]
    fn coverage_weighting_prefers_the_chunk_holding_more_of_the_query() {
        let s = store_of(&[
            // Holds one query word, many times over.
            (
                "confluence",
                "release release release release release release release",
            ),
            // Holds all three, once each.
            ("confluence", "release process approval steps for the team"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let query = "release process approval";

        let ratio = |hits: &[LexicalHit]| {
            let partial = hits.iter().find(|h| h.chunk == 0).unwrap().score;
            let complete = hits.iter().find(|h| h.chunk == 1).unwrap().score;
            complete / partial
        };

        let plain = idx.search(
            query,
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        let weighted = idx.search(
            query,
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 2.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        assert!(
            ratio(&weighted) > ratio(&plain),
            "coverage should raise the complete match relative to the partial one: {} then {}",
            ratio(&plain),
            ratio(&weighted)
        );
        assert_eq!(
            weighted[0].chunk, 1,
            "with coverage the complete match should lead"
        );
    }

    /// A single term query has no coverage information to use, so the exponent must
    /// not change its ranking. The identifier scenario is exactly this case.
    #[test]
    fn coverage_weighting_leaves_a_single_term_query_alone() {
        let s = store_of(&[
            ("confluence", "eligibility eligibility rules"),
            (
                "confluence",
                "one mention of eligibility inside a much longer chunk of prose",
            ),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let plain = idx.search(
            "eligibility",
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        let weighted = idx.search(
            "eligibility",
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 3.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        assert_eq!(
            plain.iter().map(|h| h.chunk).collect::<Vec<_>>(),
            weighted.iter().map(|h| h.chunk).collect::<Vec<_>>()
        );
    }

    /// Proximity is the half of `ts_rank_cd` BM25 lacks: two chunks can hold the same
    /// words the same number of times and mean entirely different things.
    #[test]
    fn proximity_prefers_the_chunk_whose_query_terms_sit_together() {
        let filler = "padding words that carry no query terms at all ".repeat(6);
        let together = format!("{filler} release process {filler}");
        let apart = format!("release {filler} something else entirely {filler} process");
        let s = store_of(&[
            ("confluence", together.as_str()),
            ("confluence", apart.as_str()),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);

        let scored = idx.search(
            "release process",
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 1.0,
                tier: false,
                ..Default::default()
            },
        );
        assert_eq!(scored[0].chunk, 0, "the adjacent pair should lead");
        assert!(scored[0].score > scored[1].score);
    }

    /// At weight zero nothing is rescored, so the setting is a real off switch and
    /// the engine's previous behaviour stays reachable.
    #[test]
    fn proximity_weight_zero_changes_nothing() {
        let s = store_of(&[
            ("confluence", "release process is described here in full"),
            (
                "confluence",
                "release of the build, and separately a process",
            ),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let off = idx.search(
            "release process",
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        let on = idx.search(
            "release process",
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 1.0,
                tier: false,
                ..Default::default()
            },
        );
        assert_eq!(off.len(), on.len());
        for (a, b) in off.iter().zip(&on) {
            if a.chunk == b.chunk {
                continue;
            }
        }
        assert_eq!(
            off,
            idx.search(
                "release process",
                &s,
                &f,
                &tok,
                5,
                LexicalParams {
                    prefix: false,
                    coverage: 0.0,
                    proximity: 0.0,
                    tier: false,
                    ..Default::default()
                }
            )
        );
    }

    /// The covering window is the whole of the proximity signal, so it is worth
    /// checking against hand worked cases rather than only through scores.
    #[test]
    fn the_smallest_covering_window_is_found() {
        // Adjacent: two terms, width two.
        assert_eq!(smallest_window(&[&[0], &[1]]), Some(2));
        // The best window is at the end, not at the front.
        assert_eq!(smallest_window(&[&[0, 30], &[20, 31]]), Some(2));
        // Three lists, the tightest cover in the middle.
        assert_eq!(smallest_window(&[&[0, 10], &[11], &[12, 40]]), Some(3));
        // One occurrence of everything at the same position: width one.
        assert_eq!(smallest_window(&[&[5], &[5]]), Some(1));
        // A term that does not occur has no window.
        assert_eq!(smallest_window(&[&[1, 2], &[]]), None);
    }

    /// Positions are what the proximity pass reads, so they have to be the token
    /// offsets of the analyzed text rather than of the raw string.
    #[test]
    fn positions_are_recorded_for_every_occurrence() {
        let s = store_of(&[("confluence", "alpha beta alpha gamma alpha")]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let terms = tok.terms("alpha beta alpha gamma alpha");
        let alpha = &terms[0];
        let positions = idx.positions_of(alpha, 0).expect("alpha occurs in chunk 0");
        assert_eq!(positions.len(), 3, "three occurrences");
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "ascending");
        assert!(positions.iter().all(|p| (*p as usize) < terms.len()));
    }

    /// Tiering is the ordering PostgreSQL gets from joining query terms with `&`,
    /// without the part where a chunk missing one word disappears. A chunk holding
    /// every term leads however weak its score, and the partial matches are still
    /// there underneath it.
    #[test]
    fn tiering_puts_every_term_above_a_higher_scoring_partial_match() {
        let s = store_of(&[
            // A strong score on two of the three terms, repeated hard.
            (
                "confluence",
                "approval approval approval approval process process process",
            ),
            // All three, buried in a long chunk, so its BM25 score is much lower.
            (
                "confluence",
                "the release notes mention approval and the process for it somewhere in a \
                 chunk that runs on at considerable length about unrelated matters, padding \
                 the length until the normalisation has bitten hard indeed",
            ),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let query = "release approval process";

        // Whether score alone would have ranked the complete match first depends on the
        // collection statistics, which is the whole reason tiering is an ordering rather
        // than a score adjustment: it does not have to out-argue term frequency.
        let untiered = idx.search(
            query,
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: false,
                ..Default::default()
            },
        );
        assert_eq!(untiered.len(), 2);

        let tiered = idx.search(
            query,
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: true,
                ..Default::default()
            },
        );
        assert_eq!(
            tiered[0].chunk, 1,
            "tiered, the chunk holding every term leads"
        );
        assert_eq!(tiered.len(), 2, "and the partial match is still returned");
    }

    /// The point of tiering over a hard `&` is that a query no chunk holds completely
    /// still returns its best partial matches rather than nothing at all.
    #[test]
    fn tiering_still_returns_partial_matches_when_nothing_holds_the_whole_query() {
        let s = store_of(&[
            ("confluence", "release notes for the quarter"),
            ("confluence", "approval workflow for expenses"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let hits = idx.search(
            "release approval elsewhere",
            &s,
            &f,
            &tok,
            5,
            LexicalParams {
                prefix: false,
                coverage: 0.0,
                proximity: 0.0,
                tier: true,
                ..Default::default()
            },
        );
        assert_eq!(hits.len(), 2, "both partial matches are returned");
    }

    /// The ceiling has to be an upper bound on anything the search can return,
    /// or theoretical min-max normalization would produce a score above one.
    #[test]
    fn the_score_ceiling_bounds_every_score_the_search_produces() {
        let s = store_of(&[
            (
                "confluence",
                "offer eligibility offer eligibility offer eligibility",
            ),
            (
                "confluence",
                "offer eligibility rules for members of the plan",
            ),
            (
                "confluence",
                "entirely unrelated text about invoices and billing",
            ),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let ceiling = idx.score_ceiling("offer eligibility", &tok, false);
        assert!(
            ceiling > 0.0,
            "a query of known terms has a positive ceiling"
        );
        for hit in idx.search(
            "offer eligibility",
            &s,
            &f,
            &tok,
            10,
            LexicalParams::default(),
        ) {
            assert!(
                hit.score <= ceiling + 1e-4,
                "score {} exceeded the ceiling {ceiling}",
                hit.score
            );
        }
    }

    /// A query whose every term is absent from the dictionary can produce no
    /// lexical evidence at all, so its ceiling is zero rather than a small number.
    #[test]
    fn a_query_of_unknown_terms_has_a_zero_ceiling() {
        let s = store_of(&[("confluence", "offer eligibility rules")]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        assert_eq!(
            idx.score_ceiling("tirzepatide semaglutide", &tok, false),
            0.0
        );
    }

    #[test]
    fn the_dictionary_reports_which_query_terms_it_holds() {
        let s = store_of(&[("confluence", "offer eligibility rules")]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let known = tok.query_terms("eligibility");
        assert!(idx.contains_term(&known[0]));
        let unknown = tok.query_terms("tirzepatide");
        assert!(!idx.contains_term(&unknown[0]));
    }

    /// Proximity asks how wide the window holding the matched terms is; phrase
    /// asks whether they came in the query's order inside it. Two chunks with the
    /// same window width and opposite order have to be separated by phrase and
    /// only by phrase.
    #[test]
    fn the_phrase_weight_separates_two_chunks_proximity_cannot() {
        let s = store_of(&[
            (
                "confluence",
                "the offer eligibility criteria are listed below",
            ),
            (
                "confluence",
                "the eligibility offer criteria are listed below",
            ),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);

        let width_only = LexicalParams {
            proximity: 1.0,
            phrase: 0.0,
            ..Default::default()
        };
        let a = idx.search("offer eligibility", &s, &f, &tok, 10, width_only);
        let ordered =
            |hits: &[LexicalHit], c: u32| hits.iter().find(|h| h.chunk == c).unwrap().score;
        assert!(
            (ordered(&a, 0) - ordered(&a, 1)).abs() < 1e-4,
            "window width cannot tell the two apart"
        );

        let with_phrase = LexicalParams {
            proximity: 1.0,
            phrase: 1.0,
            ..Default::default()
        };
        let b = idx.search("offer eligibility", &s, &f, &tok, 10, with_phrase);
        assert!(
            ordered(&b, 0) > ordered(&b, 1),
            "the chunk holding the query in order should win: {} vs {}",
            ordered(&b, 0),
            ordered(&b, 1)
        );
    }

    /// Phrase weighting at zero must leave the ranking exactly as it was, or the
    /// setting could not be turned off.
    #[test]
    fn a_phrase_weight_of_zero_changes_nothing() {
        let s = store_of(&[
            ("confluence", "offer eligibility rules for members"),
            ("confluence", "eligibility of an offer, described elsewhere"),
            ("confluence", "members and their offer records"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let without = idx.search(
            "offer eligibility",
            &s,
            &f,
            &tok,
            10,
            LexicalParams::default(),
        );
        let with_zero = idx.search(
            "offer eligibility",
            &s,
            &f,
            &tok,
            10,
            LexicalParams {
                phrase: 0.0,
                ..Default::default()
            },
        );
        assert_eq!(without, with_zero);
    }

    #[test]
    fn every_hit_carries_the_share_of_the_query_it_holds() {
        let s = store_of(&[
            ("confluence", "offer eligibility rules"),
            ("confluence", "offer records only"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let hits = idx.search(
            "offer eligibility",
            &s,
            &f,
            &tok,
            10,
            LexicalParams::default(),
        );
        let both = hits.iter().find(|h| h.chunk == 0).unwrap();
        let one = hits.iter().find(|h| h.chunk == 1).unwrap();
        assert_eq!(both.matched_terms, 2);
        assert_eq!(one.matched_terms, 1);
        assert!((both.coverage - 1.0).abs() < 1e-6);
        assert!(one.coverage < 1.0);
    }

    #[test]
    fn the_longest_ordered_run_counts_terms_that_came_in_order() {
        // Query places 0, 1, 2 at chunk positions: in order, then reversed.
        let in_order: Vec<(usize, &[u32])> = vec![(0, &[1]), (1, &[4]), (2, &[9])];
        assert_eq!(longest_ordered_run(&in_order), 3);
        let reversed: Vec<(usize, &[u32])> = vec![(0, &[9]), (1, &[4]), (2, &[1])];
        assert_eq!(longest_ordered_run(&reversed), 1);
        let partial: Vec<(usize, &[u32])> = vec![(0, &[1]), (1, &[4]), (2, &[2])];
        assert_eq!(longest_ordered_run(&partial), 2);
    }
}

#[cfg(test)]
mod postings_tests {
    use super::*;

    /// Builds a dictionary whose terms are all in the flat arrays.
    ///
    /// @param terms - the terms, which must already be sorted
    fn flat_of(terms: &[(&str, &[u32])]) -> Postings {
        let mut postings = Postings::default();
        for (term, chunks) in terms {
            let list: Vec<Posting> = chunks
                .iter()
                .map(|chunk| Posting {
                    chunk: *chunk,
                    term_frequency: 1,
                    positions_at: 0,
                })
                .collect();
            postings.push_flat(term, &list);
        }
        postings
    }

    /// One posting for a chunk.
    ///
    /// @param chunk - which chunk
    fn posting(chunk: u32) -> Posting {
        Posting {
            chunk,
            term_frequency: 1,
            positions_at: 0,
        }
    }

    /// **A term whose postings sit in two pieces reads as one list.**
    ///
    /// This is the whole risk of the overflow: every reader of a posting list
    /// used to hold a `&Vec<Posting>` and now holds two slices, so a reader
    /// that forgot the second would answer with the postings the index had when
    /// it was loaded and none of the ones added since - silently, and only for
    /// terms that were appended to.
    #[test]
    fn a_term_appended_to_after_a_load_reads_as_one_list() {
        let mut postings = flat_of(&[("alpha", &[1, 4]), ("beta", &[2])]);
        postings.push("alpha", posting(9));
        postings.push("beta", posting(7));

        let alpha = postings.get("alpha").expect("alpha is in the dictionary");
        assert_eq!(alpha.len(), 3, "the appended posting is missing");
        assert_eq!(
            alpha.iter().map(|p| p.chunk).collect::<Vec<u32>>(),
            vec![1, 4, 9],
            "the two pieces did not read in chunk order"
        );
        assert_eq!(postings.total(), 5, "the total lost the appended postings");
    }

    /// **A term seen for the first time after a load is found.**
    ///
    /// It goes in `fresh` rather than in the flat arrays, and every lookup, the
    /// merged iteration and the prefix expansion have to find it there.
    #[test]
    fn a_term_first_seen_after_a_load_is_found_everywhere() {
        let mut postings = flat_of(&[("alpha", &[1]), ("gamma", &[2])]);
        assert!(postings.push("beta", posting(3)), "beta was not new");
        assert!(!postings.push("beta", posting(4)), "beta was new twice");

        assert!(postings.contains("beta"));
        assert_eq!(
            postings.get("beta").map(|list| list.len()),
            Some(2),
            "the new term's postings are not both there"
        );
        assert_eq!(
            postings.len(),
            3,
            "the term count did not include the new one"
        );
        assert_eq!(
            postings.iter().map(|(term, _)| term).collect::<Vec<&str>>(),
            vec!["alpha", "beta", "gamma"],
            "the merged order is not sorted"
        );
    }

    /// **A prefix expansion reaches both the flat terms and the new ones.**
    ///
    /// The flat terms are found by binary search and the new ones by a range on
    /// the sorted map, and a version that searched only the first would stop
    /// matching a term the moment it was added.
    #[test]
    fn a_prefix_expansion_reaches_both_halves() {
        let mut postings = flat_of(&[("prefix", &[1]), ("prefixed", &[2]), ("zebra", &[3])]);
        postings.push("prefixing", posting(4));

        let mut found = postings.expand_prefix("prefix", 10);
        found.sort_unstable();
        assert_eq!(
            found,
            vec!["prefix", "prefixed", "prefixing"],
            "the expansion missed a term on one side or picked up one that does not match"
        );
        assert!(
            postings.expand_prefix("zeb", 10).contains(&"zebra"),
            "a prefix past the new terms found nothing"
        );
        assert!(
            postings.expand_prefix("nothing", 10).is_empty(),
            "a prefix no term starts with matched something"
        );
        assert_eq!(
            postings.expand_prefix("prefix", 2).len(),
            2,
            "the cap was not applied"
        );
    }

    /// **A posting in the overflow is found by chunk.**
    ///
    /// `positions_of` binary searches the list, and the list is two sorted
    /// pieces rather than one. A search of the first piece alone would fail to
    /// find any chunk added after the load, which the phrase rescoring would
    /// read as "this term does not occur in this chunk".
    #[test]
    fn a_posting_in_the_overflow_is_found_by_its_chunk() {
        let mut postings = flat_of(&[("alpha", &[1, 4, 6])]);
        postings.push("alpha", posting(11));
        let alpha = postings.get("alpha").expect("alpha is there");

        for chunk in [1u32, 4, 6, 11] {
            assert_eq!(
                alpha.at_chunk(chunk).map(|p| p.chunk),
                Some(chunk),
                "chunk {chunk} was not found"
            );
        }
        assert!(
            alpha.at_chunk(5).is_none(),
            "a chunk the term does not occur in was found"
        );
        assert!(
            alpha.at_chunk(99).is_none(),
            "a chunk past the end was found"
        );
    }

    /// **Folding the maps back in changes nothing a reader can see.**
    ///
    /// `flatten_if_heavy` rebuilds the arrays behind whoever is using them, so
    /// the one thing that must be true is that the answers do not move. The
    /// threshold is exercised from both sides: below it nothing is folded, and
    /// above it everything is.
    #[test]
    fn folding_the_maps_back_in_changes_no_answer() {
        let flat: Vec<(&str, &[u32])> = vec![
            ("alpha", &[1, 2, 3, 4, 5, 6, 7, 8]),
            ("beta", &[1, 2, 3, 4, 5, 6, 7, 8]),
        ];
        let mut postings = flat_of(&flat);
        postings.push("alpha", posting(20));
        postings.push("delta", posting(21));

        let before: Vec<(String, Vec<u32>)> = postings
            .iter()
            .map(|(term, list)| (term.to_string(), list.iter().map(|p| p.chunk).collect()))
            .collect();

        // Two postings against sixteen flat ones is under the eighth, so this
        // must leave the maps alone.
        postings.flatten_if_heavy();
        assert!(
            postings.get("delta").is_some(),
            "the new term was lost by a fold that should not have happened"
        );

        // Enough to cross it.
        for chunk in 22..30 {
            postings.push("alpha", posting(chunk));
        }
        postings.flatten_if_heavy();

        let after: Vec<(String, Vec<u32>)> = postings
            .iter()
            .map(|(term, list)| (term.to_string(), list.iter().map(|p| p.chunk).collect()))
            .collect();
        assert_eq!(
            after.len(),
            before.len(),
            "folding changed how many terms the dictionary holds"
        );
        for (term, chunks) in &before {
            let held = after
                .iter()
                .find(|(name, _)| name == term)
                .map(|(_, chunks)| chunks)
                .unwrap_or_else(|| panic!("{term} was lost by the fold"));
            assert!(
                chunks.iter().all(|chunk| held.contains(chunk)),
                "{term} lost postings in the fold: {chunks:?} against {held:?}"
            );
        }
        assert_eq!(
            postings.get("alpha").map(|list| list.len()),
            Some(17),
            "alpha lost the postings appended after the fold"
        );
    }

    /// **An index written with an overflow reads back as one flat index.**
    ///
    /// The file format did not change, and this is what says so: a dictionary
    /// with postings in both pieces is written, read back, and asked the same
    /// questions. A writer that wrote only the flat piece would produce a file
    /// that loads and is missing every posting added since the last save.
    #[test]
    fn an_index_written_with_an_overflow_reads_back_whole() {
        let mut index = Bm25Index {
            n_chunks: 12,
            total_length: 24,
            chunk_lengths: vec![2; 12],
            chunk_heading_lengths: vec![0; 12],
            positions: vec![0; 24],
            ..Bm25Index::default()
        };
        index.postings = flat_of(&[("alpha", &[1, 4]), ("gamma", &[2])]);
        index.postings.push("alpha", posting(9));
        index.postings.push("beta", posting(3));

        let mut bytes = Vec::new();
        index.write_to(&mut bytes).expect("the index writes");
        let read = Bm25Index::read_from(&mut bytes.as_slice()).expect("the index reads back");

        assert_eq!(read.n_terms(), 3, "a term was lost in the round trip");
        assert_eq!(read.n_postings(), 5, "a posting was lost in the round trip");
        for (term, chunks) in [
            ("alpha", vec![1u32, 4, 9]),
            ("beta", vec![3]),
            ("gamma", vec![2]),
        ] {
            let list = read
                .postings
                .get(term)
                .unwrap_or_else(|| panic!("{term} is not in the index that was read back"));
            assert_eq!(
                list.iter().map(|p| p.chunk).collect::<Vec<u32>>(),
                chunks,
                "{term}'s postings did not survive the round trip"
            );
        }
    }
}
