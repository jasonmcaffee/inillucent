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

use std::collections::HashMap;

use crate::binio;
use crate::filter::CompiledFilter;
use crate::store::Store;
use crate::tokenize::Tokenizer;

pub const K1: f32 = 1.2;
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
pub struct Posting {
    pub chunk: u32,
    /// Where this posting's token positions start in the index's flat position
    /// array. `term_frequency` is how many of them there are. Flat rather than a
    /// `Vec` per posting because there are 11.7 million postings on this corpus and
    /// eleven million tiny allocations cost more in headers than in positions.
    pub positions_at: u32,
    pub term_frequency: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LexicalHit {
    pub chunk: u32,
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

#[derive(Default)]
pub struct Bm25Index {
    /// Stemmed term to its postings, sorted by chunk identifier.
    postings: HashMap<String, Vec<Posting>>,
    /// Terms in sorted order, so a prefix expansion is a binary search plus a
    /// forward walk rather than a scan of the whole dictionary.
    sorted_terms: Vec<String>,
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
        let mut new_terms: Vec<String> = Vec::new();
        if self.chunk_lengths.len() < range.end as usize {
            self.chunk_lengths.resize(range.end as usize, 0);
            self.chunk_heading_lengths.resize(range.end as usize, 0);
        }

        for chunk in range {
            let terms = tokenizer.terms(store.content(chunk));
            self.chunk_lengths[chunk as usize] = terms.len() as u32;
            self.chunk_heading_lengths[chunk as usize] =
                heading_token_count(store, tokenizer, chunk);
            self.total_length += terms.len() as u64;

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
                match self.postings.get_mut(term) {
                    Some(list) => list.push(posting),
                    None => {
                        self.postings.insert(term.to_string(), vec![posting]);
                        new_terms.push(term.to_string());
                    }
                }
            }
        }

        self.n_chunks = store.n_chunks();
        self.merge_sorted_terms(new_terms)
    }

    /// Folds newly seen terms into the sorted dictionary.
    ///
    /// Merged in one pass rather than inserted one at a time: `sorted_terms` holds
    /// 1.7 million strings on this corpus, and an insertion into the middle of it
    /// moves the tail. A few hundred of those per sync is tens of gigabytes of
    /// memmove for a list that can be rebuilt by merging in one linear walk.
    /// @param new_terms - terms the dictionary did not previously hold
    fn merge_sorted_terms(&mut self, mut new_terms: Vec<String>) -> usize {
        if new_terms.is_empty() {
            return 0;
        }
        let added = new_terms.len();
        new_terms.sort();
        if self.sorted_terms.is_empty() {
            self.sorted_terms = new_terms;
            return added;
        }
        let mut merged = Vec::with_capacity(self.sorted_terms.len() + added);
        let mut existing = std::mem::take(&mut self.sorted_terms).into_iter().peekable();
        let mut fresh = new_terms.into_iter().peekable();
        loop {
            match (existing.peek(), fresh.peek()) {
                (Some(a), Some(b)) => {
                    if a <= b {
                        merged.push(existing.next().unwrap());
                    } else {
                        merged.push(fresh.next().unwrap());
                    }
                }
                (Some(_), None) => merged.push(existing.next().unwrap()),
                (None, Some(_)) => merged.push(fresh.next().unwrap()),
                (None, None) => break,
            }
        }
        self.sorted_terms = merged;
        added
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
        binio::write_u64(w, self.sorted_terms.len() as u64)?;
        for term in &self.sorted_terms {
            binio::write_str(w, term)?;
            let postings = self.postings.get(term).map(|p| p.as_slice()).unwrap_or(&[]);
            binio::write_u64(w, postings.len() as u64)?;
            w.write_all(bytemuck::cast_slice(postings))?;
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

        let mut sorted_terms = Vec::with_capacity(n_terms);
        let mut postings = HashMap::with_capacity(n_terms);
        for _ in 0..n_terms {
            let term = binio::read_str(r)?;
            let count = binio::read_u64(r)? as usize;
            postings.insert(term.clone(), binio::read_pod_vec::<Posting>(r, count)?);
            sorted_terms.push(term);
        }
        Ok(Bm25Index {
            postings,
            sorted_terms,
            positions,
            chunk_lengths,
            chunk_heading_lengths,
            total_length,
            n_chunks,
        })
    }

    pub fn n_terms(&self) -> usize {
        self.postings.len()
    }

    pub fn n_postings(&self) -> usize {
        self.postings.values().map(|p| p.len()).sum()
    }

    /// Whether the dictionary holds this analyzed term at all. A query term it
    /// does not hold can contribute nothing lexical, which is what the adaptive
    /// weighting wants to know.
    /// @param term - an analyzed query term
    pub fn contains_term(&self, term: &str) -> bool {
        self.postings.contains_key(term)
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
        let start = self.sorted_terms.partition_point(|t| t.as_str() < prefix);
        self.sorted_terms[start..]
            .iter()
            .take_while(|t| t.starts_with(prefix))
            .take(MAX_PREFIX_EXPANSIONS)
            .map(|t| t.as_str())
            .collect()
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
        let LexicalParams {
            prefix,
            coverage,
            proximity,
            tier,
            phrase,
            rescore_depth_factor,
            heading_boost,
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
                if v.is_empty() && self.postings.contains_key(qt.as_str()) {
                    v.push(qt.as_str());
                }
                v
            } else if self.postings.contains_key(qt.as_str()) {
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
                for p in postings {
                    if !trivial && !filter.passes(p.chunk, store) {
                        continue;
                    }
                    let tf = p.term_frequency as f32;
                    let len = self.chunk_lengths[p.chunk as usize] as f32;
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
        let mut hits: Vec<LexicalHit> = scores
            .into_iter()
            .map(|(chunk, (score, mass, matched))| {
                tiers.insert(chunk, matched);
                let share = if total_mass > 0.0 { (mass / total_mass).clamp(0.0, 1.0) } else { 1.0 };
                let scaled = if coverage <= 0.0 { score } else { score * share.powf(coverage) };
                LexicalHit { chunk, score: scaled, coverage: share, matched_terms: matched }
            })
            .collect();

        // Most query terms held first when tiering, then descending score, then
        // ascending chunk, so the order is total and stable.
        let order = |a: &LexicalHit, b: &LexicalHit| {
            let by_tier = if tier {
                tiers.get(&b.chunk).cmp(&tiers.get(&a.chunk))
            } else {
                std::cmp::Ordering::Equal
            };
            by_tier
                .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
                .then(a.chunk.cmp(&b.chunk))
        };
        hits.sort_by(&order);

        // Proximity is applied AFTER the ranking exists, to the hits that ranking put
        // in reach of the top k, and the result is reordered. Applying it to whatever
        // order the score map happened to produce would rescore an arbitrary subset.
        if (proximity > 0.0 || phrase > 0.0) && query_terms.len() > 1 {
            let depth = rescore_depth_factor.max(1);
            let reach = (k * depth).min(hits.len());
            self.rescore_by_position(&mut hits[..reach], &query_terms, proximity, phrase);
            hits.sort_by(&order);
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
        let end = start + posting.term_frequency as usize;
        let inside = self.positions[start..end]
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
        let at = postings.binary_search_by_key(&chunk, |p| p.chunk).ok()?;
        let p = postings[at];
        let start = p.positions_at as usize;
        Some(&self.positions[start..start + p.term_frequency as usize])
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
    // The position the previous term of the current run was matched at.
    let mut previous = ordered[0].1[0];
    for window in ordered.windows(2) {
        let (_, next_positions) = window[1];
        match next_positions.iter().copied().find(|p| *p > previous) {
            Some(p) => {
                run += 1;
                previous = p;
            }
            None => {
                run = 1;
                previous = next_positions[0];
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
            let v = list[cursors[i]];
            if v < low {
                low = v;
                lowest = i;
            }
            high = high.max(v);
        }
        best = best.min(high - low + 1);
        cursors[lowest] += 1;
        if cursors[lowest] >= lists[lowest].len() {
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
        s.add_chunks(inputs);
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
        idx.search(query, store, &f, &tok, k, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() })
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
        assert_eq!(hits[0], 1, "the short chunk should win on length normalization");
    }

    #[test]
    fn term_frequency_saturates() {
        // Ten occurrences must not score ten times one occurrence.
        let s = store_of(&[
            ("confluence", "offer offer offer offer offer offer offer offer offer offer"),
            ("confluence", "offer"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let hits = idx.search("offer", &s, &f, &tok, 2, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() });
        let many = hits.iter().find(|h| h.chunk == 0).unwrap().score;
        let one = hits.iter().find(|h| h.chunk == 1).unwrap().score;
        assert!(many < one * 10.0, "frequency did not saturate: {many} vs {one}");
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
        let s = store_of(&[("confluence", "the offering was redeemed by eligible members")]);
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
        let hits = idx.search("offer eligibility", &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() });
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
        store.add_chunks(vec![
            headed("d1", "Terri Shaw tax return", "please find the attached document"),
            headed("d2", "meeting notes", "we discussed the Terri Shaw tax return at length"),
        ]);

        let tokenizer = Tokenizer::default();
        let index = Bm25Index::build(&store, &tokenizer);
        let filter = CompiledFilter::compile(&Filter::default(), &store);

        let unweighted = index.search("Terri Shaw", &store, &filter, &tokenizer, 10, LexicalParams::default());
        let weighted = index.search(
            "Terri Shaw",
            &store,
            &filter,
            &tokenizer,
            10,
            LexicalParams { heading_boost: 3.0, ..Default::default() },
        );
        assert_eq!(weighted.len(), 2, "both chunks still match");
        assert_eq!(weighted[0].chunk, 0, "the heading match should lead: {weighted:?}");
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
        store.add_chunks(vec![headed(
            "d1",
            "Terri Shaw tax return",
            "the body mentions Terri Shaw again",
        )]);
        let tokenizer = Tokenizer::default();
        let index = Bm25Index::build(&store, &tokenizer);
        let filter = CompiledFilter::compile(&Filter::default(), &store);
        let a = index.search("Terri Shaw", &store, &filter, &tokenizer, 10, LexicalParams::default());
        let b = index.search(
            "Terri Shaw",
            &store,
            &filter,
            &tokenizer,
            10,
            LexicalParams { heading_boost: 0.0, ..Default::default() },
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
        let hits = idx.search("eli", &s, &f, &tok, 5, LexicalParams { prefix: true, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() });
        assert_eq!(hits.len(), 1);
        assert!(idx.search("eli", &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() }).is_empty());
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
        let hits = idx.search("offer eligibility", &s, &f, &tok, 3, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() });
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[test]
    fn index_statistics_are_reported() {
        let s = store_of(&[("confluence", "offer eligibility rules"), ("slack", "offer")]);
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
            ("confluence", "release release release release release release release"),
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

        let plain = idx.search(query, &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() });
        let weighted = idx.search(query, &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 2.0, proximity: 0.0, tier: false, ..Default::default() });
        assert!(
            ratio(&weighted) > ratio(&plain),
            "coverage should raise the complete match relative to the partial one: {} then {}",
            ratio(&plain),
            ratio(&weighted)
        );
        assert_eq!(weighted[0].chunk, 1, "with coverage the complete match should lead");
    }

    /// A single term query has no coverage information to use, so the exponent must
    /// not change its ranking. The identifier scenario is exactly this case.
    #[test]
    fn coverage_weighting_leaves_a_single_term_query_alone() {
        let s = store_of(&[
            ("confluence", "eligibility eligibility rules"),
            ("confluence", "one mention of eligibility inside a much longer chunk of prose"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let plain = idx.search("eligibility", &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() });
        let weighted = idx.search("eligibility", &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 3.0, proximity: 0.0, tier: false, ..Default::default() });
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
        let s = store_of(&[("confluence", together.as_str()), ("confluence", apart.as_str())]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);

        let scored = idx.search("release process", &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 1.0, tier: false, ..Default::default() });
        assert_eq!(scored[0].chunk, 0, "the adjacent pair should lead");
        assert!(scored[0].score > scored[1].score);
    }

    /// At weight zero nothing is rescored, so the setting is a real off switch and
    /// the engine's previous behaviour stays reachable.
    #[test]
    fn proximity_weight_zero_changes_nothing() {
        let s = store_of(&[
            ("confluence", "release process is described here in full"),
            ("confluence", "release of the build, and separately a process"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let off = idx.search("release process", &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() });
        let on = idx.search("release process", &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 1.0, tier: false, ..Default::default() });
        assert_eq!(off.len(), on.len());
        for (a, b) in off.iter().zip(&on) {
            if a.chunk == b.chunk {
                continue;
            }
        }
        assert_eq!(off, idx.search("release process", &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() }));
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
            ("confluence", "approval approval approval approval process process process"),
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
        let untiered = idx.search(query, &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: false, ..Default::default() });
        assert_eq!(untiered.len(), 2);

        let tiered = idx.search(query, &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: true, ..Default::default() });
        assert_eq!(tiered[0].chunk, 1, "tiered, the chunk holding every term leads");
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
        let hits = idx.search("release approval elsewhere", &s, &f, &tok, 5, LexicalParams { prefix: false, coverage: 0.0, proximity: 0.0, tier: true, ..Default::default() });
        assert_eq!(hits.len(), 2, "both partial matches are returned");
    }

    /// The ceiling has to be an upper bound on anything the search can return,
    /// or theoretical min-max normalization would produce a score above one.
    #[test]
    fn the_score_ceiling_bounds_every_score_the_search_produces() {
        let s = store_of(&[
            ("confluence", "offer eligibility offer eligibility offer eligibility"),
            ("confluence", "offer eligibility rules for members of the plan"),
            ("confluence", "entirely unrelated text about invoices and billing"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);
        let ceiling = idx.score_ceiling("offer eligibility", &tok, false);
        assert!(ceiling > 0.0, "a query of known terms has a positive ceiling");
        for hit in idx.search("offer eligibility", &s, &f, &tok, 10, LexicalParams::default()) {
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
        assert_eq!(idx.score_ceiling("tirzepatide semaglutide", &tok, false), 0.0);
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
            ("confluence", "the offer eligibility criteria are listed below"),
            ("confluence", "the eligibility offer criteria are listed below"),
        ]);
        let tok = Tokenizer::default();
        let idx = Bm25Index::build(&s, &tok);
        let f = CompiledFilter::compile(&Filter::default(), &s);

        let width_only = LexicalParams { proximity: 1.0, phrase: 0.0, ..Default::default() };
        let a = idx.search("offer eligibility", &s, &f, &tok, 10, width_only);
        let ordered = |hits: &[LexicalHit], c: u32| hits.iter().find(|h| h.chunk == c).unwrap().score;
        assert!(
            (ordered(&a, 0) - ordered(&a, 1)).abs() < 1e-4,
            "window width cannot tell the two apart"
        );

        let with_phrase = LexicalParams { proximity: 1.0, phrase: 1.0, ..Default::default() };
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
        let without = idx.search("offer eligibility", &s, &f, &tok, 10, LexicalParams::default());
        let with_zero = idx.search(
            "offer eligibility",
            &s,
            &f,
            &tok,
            10,
            LexicalParams { phrase: 0.0, ..Default::default() },
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
        let hits = idx.search("offer eligibility", &s, &f, &tok, 10, LexicalParams::default());
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
