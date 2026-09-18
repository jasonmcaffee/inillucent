//! The graded query sets, and the embedding of their queries.
//!
//! Queries are embedded once, in process, and both engines receive the identical
//! query vector. Query embedding therefore cancels out of the comparison entirely,
//! which is what lets the score card attribute a difference to the index rather
//! than to the model.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

use inillucent_core::store::ChunkInput;

/// A query with an objectively correct answer set.
#[derive(Clone, Serialize, Deserialize)]
pub struct GradedQuery {
    /// Stable identity, so a saved run can be re-judged against corrected
    /// judgements without running retrieval again, and so a per-query record can
    /// be joined back to the query that produced it.
    pub id: String,
    pub text: String,
    /// Source database chunk identifiers that count as correct. Empty for
    /// scenarios graded against exhaustive cosine rather than document identity,
    /// and for queries nothing answers.
    pub correct: Vec<String>,
    /// Graded judgements: the chunk key and how relevant it is, on the scale
    /// `GRADE_ANSWER` down to irrelevant. The binary `correct` set is the top of
    /// this, kept because the metrics that predate grading still use it and their
    /// numbers stay comparable with earlier cards.
    pub graded: Vec<(String, u8)>,
    /// Which source the query is drawn from, so per source scoring is possible.
    pub source: String,
    /// Which family the query belongs to, which is the slice a report has to be
    /// able to break results down by. A global mean that improves by sacrificing
    /// identifier queries is not an improvement.
    pub family: String,
    /// Whether anything in the corpus answers it at all. A false here is not a
    /// defect in the query: it is the case a retrieval system fails at most
    /// invisibly, and it is scored by its own family.
    pub answerable: bool,
}

impl GradedQuery {
    /// A query whose ground truth is a set rather than a grading, which is the
    /// shape the three original families produce. Everything in `correct` is
    /// answer bearing.
    /// @param id - stable identity
    /// @param text - the query
    /// @param correct - chunk keys that count as correct
    /// @param source - which source it was drawn from
    /// @param family - the slice it belongs to
    pub fn binary(
        id: String,
        text: String,
        correct: Vec<String>,
        source: String,
        family: &str,
    ) -> GradedQuery {
        let graded = correct.iter().map(|k| (k.clone(), GRADE_ANSWER)).collect();
        GradedQuery {
            id,
            text,
            correct,
            graded,
            source,
            family: family.to_string(),
            answerable: true,
        }
    }

    /// The judgements as a lookup, which is what the graded metrics take.
    /// @param space - the key to ordinal mapping the run is using
    pub fn grades(&self, space: &mut crate::scenarios::KeySpace) -> HashMap<u32, u8> {
        self.graded.iter().map(|(k, g)| (space.id(k), *g)).collect()
    }
}

/// Document identity queries: use a document's own title as the query, and count
/// any chunk of that document as correct.
///
/// Titles in this corpus are written by people to describe their own content, so
/// they behave like real queries, and the answer is objective without anyone
/// judging results. This is the only ground truth of the three that grades
/// fusion, because it grades the whole pipeline.
///
/// The bias is stated rather than hidden: a title shares vocabulary with its own
/// body, which flatters lexical retrieval. It flatters it equally for both
/// engines, so the comparison stays fair even though the absolute number is
/// optimistic.
pub fn document_identity_queries(
    chunks: &[ChunkInput],
    keys: &[String],
    per_source: usize,
    seed: u64,
) -> Vec<GradedQuery> {
    // Group chunk keys by document, and remember each document's title and source.
    let mut chunks_of_doc: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut title_of_doc: HashMap<(String, String), String> = HashMap::new();
    for (c, key_of_chunk) in chunks.iter().zip(keys) {
        if c.deleted {
            continue;
        }
        let key = (c.source.clone(), c.external_doc_id.clone());
        chunks_of_doc
            .entry(key.clone())
            .or_default()
            .push(key_of_chunk.clone());
        title_of_doc.insert(key, c.title.clone());
    }

    // A title has to be usable as a query: long enough to be specific, and not
    // shared with another document, or "correct" would be ambiguous.
    let mut title_counts: HashMap<&str, usize> = HashMap::new();
    for t in title_of_doc.values() {
        *title_counts.entry(t.as_str()).or_insert(0) += 1;
    }

    let mut by_source: HashMap<String, Vec<GradedQuery>> = HashMap::new();
    let mut docs: Vec<(&(String, String), &String)> = title_of_doc.iter().collect();
    // Sort for determinism before sampling, since HashMap order is not stable.
    docs.sort_by(|a, b| a.0.cmp(b.0));

    let mut rng = StdRng::seed_from_u64(seed);
    for (key, title) in docs {
        let trimmed = title.trim();
        if trimmed.chars().count() < 12 || trimmed.chars().count() > 160 {
            continue;
        }
        if title_counts.get(trimmed).copied().unwrap_or(0) != 1 {
            continue;
        }
        // At least two alphanumeric words, or the "query" is a bare identifier.
        if trimmed
            .split_whitespace()
            .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
            .count()
            < 2
        {
            continue;
        }
        let correct = chunks_of_doc.get(key).cloned().unwrap_or_default();
        if correct.is_empty() {
            continue;
        }
        by_source
            .entry(key.0.clone())
            .or_default()
            .push(GradedQuery::binary(
                format!("identity-{}-{}", key.0, key.1),
                trimmed.to_string(),
                correct,
                key.0.clone(),
                "document identity",
            ));
    }

    let mut out = Vec::new();
    let mut sources: Vec<String> = by_source.keys().cloned().collect();
    sources.sort();
    for source in sources {
        // The name came out of this map's own keys, so this is that fact
        // written where the compiler can check it.
        let Some(mut pool) = by_source.remove(&source) else {
            continue;
        };
        // Sample without replacement, deterministically.
        let take = per_source.min(pool.len());
        for _ in 0..take {
            let i = rng.gen_range(0..pool.len());
            out.push(pool.swap_remove(i));
        }
    }
    out
}

/// Identifier queries: rare literal strings that a lexical index must find.
///
/// Drawn from the corpus itself so the answer is objective, and restricted to
/// terms appearing in few chunks, because a term appearing everywhere measures
/// nothing.
pub fn identifier_queries(
    chunks: &[ChunkInput],
    keys: &[String],
    wanted: usize,
    seed: u64,
) -> Vec<GradedQuery> {
    // Candidate identifiers: tokens that look like a ticket key, a function name
    // or a versioned name, rather than an English word.
    let mut occurrences: HashMap<String, Vec<String>> = HashMap::new();
    for (c, key_of_chunk) in chunks.iter().zip(keys) {
        if c.deleted {
            continue;
        }
        let mut seen: HashSet<String> = HashSet::new();
        for raw in c.content.split(|ch: char| ch.is_whitespace()) {
            let token: String = raw
                .chars()
                .filter(|ch| ch.is_alphanumeric() || *ch == '-' || *ch == '_')
                .collect();
            if token.len() < 6 || token.len() > 40 {
                continue;
            }
            let has_digit = token.chars().any(|c| c.is_ascii_digit());
            let has_alpha = token.chars().any(|c| c.is_alphabetic());
            let has_separator = token.contains('-') || token.contains('_');
            // A mix of letters and digits, or an explicit separator, is what
            // distinguishes an identifier from a word.
            if !(has_alpha && (has_digit || has_separator)) {
                continue;
            }
            if seen.insert(token.clone()) {
                occurrences
                    .entry(token)
                    .or_default()
                    .push(key_of_chunk.clone());
            }
        }
    }

    let mut candidates: Vec<(String, Vec<String>)> = occurrences
        .into_iter()
        .filter(|(_, chunks)| !chunks.is_empty() && chunks.len() <= 5)
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::new();
    let take = wanted.min(candidates.len());
    for _ in 0..take {
        let i = rng.gen_range(0..candidates.len());
        let (token, chunks) = candidates.swap_remove(i);
        let token = token.to_string();
        out.push(GradedQuery::binary(
            format!("identifier-{token}"),
            token.clone(),
            chunks,
            "any".to_string(),
            "identifier",
        ));
    }
    out
}

/// Natural language queries, drawn from headings so they read like questions a
/// person would type rather than like a title.
pub fn heading_queries(
    chunks: &[ChunkInput],
    keys: &[String],
    wanted: usize,
    seed: u64,
) -> Vec<GradedQuery> {
    let mut by_heading: HashMap<String, Vec<String>> = HashMap::new();
    let mut source_of: HashMap<String, String> = HashMap::new();
    for (c, key_of_chunk) in chunks.iter().zip(keys) {
        if c.deleted {
            continue;
        }
        // The deepest heading, which is also the `is_empty` guard this used to
        // carry separately.
        let Some(leaf) = c.heading_path.last().map(|h| h.trim()) else {
            continue;
        };
        if leaf.chars().count() < 15 || leaf.chars().count() > 120 {
            continue;
        }
        if leaf.split_whitespace().count() < 3 {
            continue;
        }
        by_heading
            .entry(leaf.to_string())
            .or_default()
            .push(key_of_chunk.clone());
        source_of.insert(leaf.to_string(), c.source.clone());
    }

    let mut candidates: Vec<(String, Vec<String>)> = by_heading
        .into_iter()
        .filter(|(_, chunks)| chunks.len() <= 4)
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::new();
    let take = wanted.min(candidates.len());
    for _ in 0..take {
        let i = rng.gen_range(0..candidates.len());
        let (heading, chunks) = candidates.swap_remove(i);
        let source = source_of
            .get(&heading)
            .cloned()
            .unwrap_or_else(|| "any".into());
        out.push(GradedQuery::binary(
            format!("heading-{source}-{heading}"),
            heading.clone(),
            chunks,
            source,
            "heading",
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Harder query families, with graded relevance.
// ---------------------------------------------------------------------------

/// Relevance grades, on the four point scale the TREC passage judgements use.
///
/// The distinction that matters is between 3 and 2. A chunk of the right document
/// is not the same thing as the chunk that answers, and an agent handed the
/// second one has to go and find the first. Every family above this line grades
/// binary, which is why every chunk of a document counted as correct and why the
/// hybrid figures were optimistic.
pub const GRADE_ANSWER: u8 = 3;
pub const GRADE_SUPPORTING: u8 = 2;

/// English function words, dropped when a query is built out of a sentence.
///
/// Short and deliberately not exhaustive: this is not an analyzer, it is the list
/// of words whose presence or absence in a query changes nothing about which
/// chunk answers it. The tokenizer has its own stopword handling for indexing.
const FUNCTION_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "could", "did", "do",
    "does", "for", "from", "had", "has", "have", "he", "her", "his", "how", "i", "if", "in",
    "into", "is", "it", "its", "may", "might", "more", "most", "must", "no", "not", "of", "on",
    "one", "or", "other", "our", "out", "over", "she", "should", "so", "some", "such", "than",
    "that", "the", "their", "them", "then", "there", "these", "they", "this", "those", "to", "two",
    "under", "up", "was", "we", "were", "what", "when", "where", "which", "while", "who", "why",
    "will", "with", "would", "you", "your",
];

fn is_function_word(word: &str) -> bool {
    FUNCTION_WORDS.contains(&word)
}

/// A word reduced to its comparable form: lowercase, outer punctuation removed.
fn normalized(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// How many chunks each lowercase word appears in, over the slice being graded.
///
/// Needed because the hard families choose which words to *remove* from a query
/// by how rare they are, and rarity is a property of this corpus rather than of
/// English. One pass over the corpus, the same cost the identifier family already
/// pays.
pub fn document_frequencies(chunks: &[ChunkInput]) -> HashMap<String, u32> {
    let mut df: HashMap<String, u32> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    for c in chunks {
        if c.deleted {
            continue;
        }
        seen.clear();
        for raw in c.content.split_whitespace() {
            let w = normalized(raw);
            if w.len() < 3 || w.chars().all(|ch| ch.is_ascii_digit()) {
                continue;
            }
            if seen.insert(w.clone()) {
                *df.entry(w).or_insert(0) += 1;
            }
        }
    }
    df
}

/// The body of a chunk: everything after the breadcrumb line the builder puts at
/// the front.
///
/// Every chunk in this corpus begins `Title > Heading` followed by a blank line,
/// because the corpus it reproduces did. That breadcrumb is why a title query
/// matches every chunk of its document, and it is exactly what a query built to
/// avoid document-level leakage must not draw from.
fn body_of(content: &str) -> &str {
    match content.find("\n\n") {
        Some(at) => &content[at + 2..],
        None => content,
    }
}

/// Split prose into sentences on terminal punctuation followed by a space.
fn sentences(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, byte) in bytes.iter().enumerate() {
        let c = *byte as char;
        if (c == '.' || c == '?' || c == '!')
            && bytes
                .get(i + 1)
                .map(|n| (*n as char).is_whitespace())
                .unwrap_or(i + 1 == bytes.len())
        {
            // The terminal punctuation is one of three ASCII bytes, so `i` is
            // the last byte of a character and `i + 1` is a boundary.
            if let Some(piece) = text.get(start..=i) {
                let piece = piece.trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
            }
            start = i + 1;
        }
    }
    let tail = text.get(start..).unwrap_or("").trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Words that appear in the chunk's breadcrumb, which a query must not reuse.
fn breadcrumb_words(chunk: &ChunkInput) -> HashSet<String> {
    let mut out = HashSet::new();
    for part in
        std::iter::once(chunk.title.as_str()).chain(chunk.heading_path.iter().map(|h| h.as_str()))
    {
        for raw in part.split_whitespace() {
            let w = normalized(raw);
            if !w.is_empty() {
                out.insert(w);
            }
        }
    }
    out
}

/// Passage evidence queries: a question about one specific passage, built so the
/// passage is the only chunk that answers it.
///
/// This is the family the other three could not be. Title and heading queries are
/// answered by *any* chunk of a document, because the builder writes the title and
/// the heading into the front of every chunk; the ground truth is therefore a
/// container rather than an answer, and both engines are graded on finding the
/// right page rather than the right paragraph. An agent needs the paragraph.
///
/// A query is built from one body sentence, with two things removed:
///
/// - **every word of the chunk's own breadcrumb**, so the query cannot be answered
///   by the title text that all of that document's chunks share. Without this the
///   family collapses back into document identity.
/// - **the two rarest remaining content words**, which are the strongest lexical
///   anchors in the sentence. Removing them is the vocabulary gap: what is left is
///   a description of the passage in ordinary words, the way somebody half
///   remembering a fact would ask for it, rather than a quotation of it.
///
/// What remains is still drawn from the passage, which is a stated bias and a
/// weaker one than the families it supplements: it removes the container leak and
/// the two terms that make the match trivial, and it keeps the ground truth
/// objective, which an LLM-written paraphrase would not.
///
/// Grades: the passage itself is answer bearing, the rest of its document is
/// supporting context that does not answer, everything else is irrelevant.
/// @param chunks - the corpus slice being graded
/// @param keys - the shared chunk key per ordinal
/// @param df - document frequencies over the same slice
/// @param per_source - how many queries to draw from each source
/// @param seed - fixes the sample
pub fn passage_evidence_queries(
    chunks: &[ChunkInput],
    keys: &[String],
    df: &HashMap<String, u32>,
    per_source: usize,
    seed: u64,
) -> Vec<GradedQuery> {
    // Which chunks belong to which document, so the rest of a document can be
    // graded as supporting rather than as answer bearing.
    let mut chunks_of_doc: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, c) in chunks.iter().enumerate() {
        if c.deleted {
            continue;
        }
        chunks_of_doc
            .entry((c.source.as_str(), c.external_doc_id.as_str()))
            .or_default()
            .push(i);
    }

    let mut by_source: HashMap<String, Vec<GradedQuery>> = HashMap::new();
    for (i, (c, key_of_chunk)) in chunks.iter().zip(keys).enumerate() {
        if c.deleted {
            continue;
        }
        let Some(text) = passage_query_from(c, df) else {
            continue;
        };
        let siblings = chunks_of_doc
            .get(&(c.source.as_str(), c.external_doc_id.as_str()))
            .cloned()
            .unwrap_or_default();
        let mut graded: Vec<(String, u8)> = vec![(key_of_chunk.clone(), GRADE_ANSWER)];
        for s in siblings {
            // The sibling ordinals came from a walk of these same two slices,
            // so a missing key is a defect rather than a corpus a grade can be
            // guessed for; it is left ungraded, which reads as irrelevant.
            if s != i {
                if let Some(sibling) = keys.get(s) {
                    graded.push((sibling.clone(), GRADE_SUPPORTING));
                }
            }
        }
        by_source
            .entry(c.source.clone())
            .or_default()
            .push(GradedQuery {
                id: format!("passage-{key_of_chunk}"),
                text,
                correct: vec![key_of_chunk.clone()],
                graded,
                source: c.source.clone(),
                family: "passage evidence".to_string(),
                answerable: true,
            });
    }
    sample_per_source(by_source, per_source, seed)
}

/// One passage query, or `None` when the chunk has no sentence that can carry one.
///
/// The filters are all about whether the result would still identify the passage:
/// a sentence needs enough content words that removing the two rarest leaves a
/// description, and the words that remain have to be ordinary enough that they do
/// not simply re-anchor the query on a different rare token.
/// @param chunk - the chunk the query is built from
/// @param df - document frequencies over the graded slice
fn passage_query_from(chunk: &ChunkInput, df: &HashMap<String, u32>) -> Option<String> {
    let breadcrumb = breadcrumb_words(chunk);
    let body = body_of(&chunk.content);

    for sentence in sentences(body) {
        let words: Vec<&str> = sentence.split_whitespace().collect();
        if words.len() < 12 || words.len() > 45 {
            continue;
        }
        // Content words that are not part of the breadcrumb, with their rarity.
        let mut content: Vec<(usize, String, u32)> = Vec::new();
        for (at, raw) in words.iter().enumerate() {
            let w = normalized(raw);
            if w.len() < 4 || is_function_word(&w) || breadcrumb.contains(&w) {
                continue;
            }
            content.push((at, w.clone(), df.get(&w).copied().unwrap_or(0)));
        }
        if content.len() < 7 {
            continue;
        }
        // Drop the two rarest content words: the lexical anchors that would make
        // the match trivial for any index at all.
        let mut by_rarity = content.clone();
        by_rarity.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)));
        let dropped: HashSet<usize> = by_rarity.iter().take(2).map(|(at, _, _)| *at).collect();

        let kept: Vec<&str> = words
            .iter()
            .enumerate()
            .filter(|(at, raw)| {
                let w = normalized(raw);
                !dropped.contains(at) && !breadcrumb.contains(&w)
            })
            .map(|(_, raw)| *raw)
            .collect();
        let text = kept.join(" ");
        let trimmed = text
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        if trimmed.split_whitespace().count() < 8 {
            continue;
        }
        return Some(trimmed);
    }
    None
}

/// How a query is made harder without changing what answers it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Perturbation {
    /// Two adjacent characters of the longest term swapped, which is the commonest
    /// typing mistake there is.
    Typo,
    /// Only the three rarest content words kept, which is what people type when
    /// they are searching rather than writing.
    Shorthand,
}

impl Perturbation {
    /// What the score card calls the perturbed family.
    ///
    /// @see [`Perturbation`] for what each one does to a query.
    pub fn label(&self) -> &'static str {
        match self {
            Perturbation::Typo => "typo",
            Perturbation::Shorthand => "shorthand",
        }
    }
}

/// The same information need, expressed the way a person in a hurry expresses it.
///
/// The ground truth does not move: these are the queries of the family they are
/// derived from, so the difference between the two scores is exactly what the
/// perturbation cost. That is the whole point — an absolute score on a typo pack
/// says very little, and the gap between a clean query and its typo says how
/// brittle the retrieval is.
/// @param base - the queries to perturb, already graded
/// @param how - which perturbation to apply
/// @param df - document frequencies, used to decide which words are rare
pub fn perturbed_queries(
    base: &[GradedQuery],
    how: Perturbation,
    df: &HashMap<String, u32>,
) -> Vec<GradedQuery> {
    let mut out = Vec::new();
    for q in base {
        let Some(text) = perturb(&q.text, how, df) else {
            continue;
        };
        out.push(GradedQuery {
            id: format!("{}-{}", how.label(), q.id),
            text,
            correct: q.correct.clone(),
            graded: q.graded.clone(),
            source: q.source.clone(),
            family: format!("{}, {}", q.family, how.label()),
            answerable: q.answerable,
        });
    }
    out
}

/// Apply one perturbation, or `None` when the query has nothing to perturb.
fn perturb(text: &str, how: Perturbation, df: &HashMap<String, u32>) -> Option<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 3 {
        return None;
    }
    match how {
        Perturbation::Typo => {
            // The rarest word the query holds, not the longest. A transposition in
            // a common word costs almost nothing, because every other word still
            // anchors the match and the query is a whole sentence; a transposition
            // in the word that carries the query's information is the mistake that
            // actually loses the passage, and it is also the word people really do
            // misspell. It has to be at least six characters, because a mistake in
            // a short word is likely to land on another real word and stop being a
            // typo at all.
            let (at, rarest) = words
                .iter()
                .enumerate()
                .filter(|(_, w)| w.chars().all(|c| c.is_alphanumeric()) && w.chars().count() >= 6)
                .min_by_key(|(_, w)| df.get(&normalized(w)).copied().unwrap_or(0))?;
            let chars: Vec<char> = rarest.chars().collect();
            if chars.len() < 6 {
                return None;
            }
            // An interior swap, so the first letter is intact and the word still
            // looks like the word somebody meant.
            let mid = chars.len() / 2;
            let mut swapped = chars.clone();
            swapped.swap(mid - 1, mid);
            if swapped == chars {
                return None;
            }
            let mut out: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            // `at` is the position the `enumerate` above gave, over this same
            // word list.
            *out.get_mut(at)? = swapped.into_iter().collect();
            Some(out.join(" "))
        }
        Perturbation::Shorthand => {
            let mut content: Vec<(String, u32)> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for raw in &words {
                let w = normalized(raw);
                if w.len() < 4 || is_function_word(&w) || !seen.insert(w.clone()) {
                    continue;
                }
                let rarity = df.get(&w).copied().unwrap_or(0);
                content.push((w, rarity));
            }
            if content.len() < 4 {
                return None;
            }
            content.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
            content.truncate(3);
            // Back into the order they appeared, so the result reads like a phrase
            // rather than like a rarity ranking.
            let keep: HashSet<&str> = content.iter().map(|(w, _)| w.as_str()).collect();
            let mut kept: Vec<String> = Vec::new();
            let mut used: HashSet<String> = HashSet::new();
            for raw in &words {
                let w = normalized(raw);
                if keep.contains(w.as_str()) && used.insert(w.clone()) {
                    kept.push(w);
                }
            }
            Some(kept.join(" "))
        }
    }
}

/// Queries nothing in the corpus answers.
///
/// Built by taking the content words of two documents drawn from two different
/// sources and mixing them. The corpus builder gives each source a disjoint pool
/// of source material, so no chunk holds material from both, and the result is a
/// question that sounds entirely plausible and has no answer.
///
/// These are the queries a retrieval system is most likely to get wrong in the way
/// that matters most, because it cannot fail visibly: it returns ten confident
/// looking passages about nothing, and an agent writes an answer out of them.
/// They carry no positive grades at all, and are scored by their own family rather
/// than by recall, where an empty reference set would otherwise be scored as
/// perfect.
/// @param chunks - the corpus slice being graded
/// @param df - document frequencies over the same slice
/// @param wanted - how many queries to build
/// @param seed - fixes the sample
pub fn unanswerable_queries(
    chunks: &[ChunkInput],
    df: &HashMap<String, u32>,
    wanted: usize,
    seed: u64,
) -> Vec<GradedQuery> {
    // Distinctive content words per document, per source.
    let mut per_source: HashMap<String, Vec<(String, Vec<String>)>> = HashMap::new();
    let mut seen_docs: HashSet<(String, String)> = HashSet::new();
    for c in chunks {
        if c.deleted {
            continue;
        }
        let key = (c.source.clone(), c.external_doc_id.clone());
        if !seen_docs.insert(key) {
            continue;
        }
        let mut words: Vec<(String, u32)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for raw in body_of(&c.content).split_whitespace() {
            let w = normalized(raw);
            if w.len() < 5 || is_function_word(&w) || !seen.insert(w.clone()) {
                continue;
            }
            let rarity = df.get(&w).copied().unwrap_or(0);
            // Rare, but not unique: a token appearing once is an identifier, and
            // an identifier makes the query obviously unanswerable rather than
            // plausibly so.
            if !(3..=400).contains(&rarity) {
                continue;
            }
            words.push((w, rarity));
        }
        if words.len() < 4 {
            continue;
        }
        words.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        words.truncate(4);
        per_source.entry(c.source.clone()).or_default().push((
            c.external_doc_id.clone(),
            words.into_iter().map(|(w, _)| w).collect(),
        ));
    }

    let mut sources: Vec<String> = per_source.keys().cloned().collect();
    sources.sort();
    if sources.len() < 2 {
        return Vec::new();
    }
    for list in per_source.values_mut() {
        list.sort();
    }

    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::new();
    let mut attempts = 0usize;
    while out.len() < wanted && attempts < wanted * 20 {
        attempts += 1;
        // Two source names drawn at random. `get` rather than an index
        // because a draw is the one place a bound is computed rather than
        // written, and this loop already gives up after `wanted * 20` tries.
        let Some(a) = sources.get(rng.gen_range(0..sources.len())) else {
            continue;
        };
        let Some(b) = sources.get(rng.gen_range(0..sources.len())) else {
            continue;
        };
        if a == b {
            continue;
        }
        let (Some(left), Some(right)) = (per_source.get(a), per_source.get(b)) else {
            continue;
        };
        if left.is_empty() || right.is_empty() {
            continue;
        }
        let (Some((ld, lw)), Some((rd, rw))) = (
            left.get(rng.gen_range(0..left.len())),
            right.get(rng.gen_range(0..right.len())),
        ) else {
            continue;
        };
        let mut terms: Vec<String> = lw.iter().take(2).cloned().collect();
        terms.extend(rw.iter().take(2).cloned());
        // A query whose halves happen to share a word is not disjoint any more.
        let unique: HashSet<&String> = terms.iter().collect();
        if unique.len() != terms.len() {
            continue;
        }
        out.push(GradedQuery {
            id: format!("unanswerable-{a}-{ld}-{b}-{rd}"),
            text: terms.join(" "),
            correct: Vec::new(),
            graded: Vec::new(),
            source: "any".to_string(),
            family: "unanswerable".to_string(),
            answerable: false,
        });
    }
    out
}

/// Questions whose answer needs evidence from two documents in two sources.
///
/// One heading from each, joined. Both are answer bearing, so a ranking that finds
/// one of the two is half an answer and is scored as half rather than as a
/// success. This is the family that separates "found something relevant" from
/// "found what the answer needs", and it is the shape an agent's hardest questions
/// actually take.
///
/// It is also where all-terms lexical semantics fail completely: no chunk contains
/// both headings, so PostgreSQL's `to_tsquery`, which joins terms with `&`,
/// matches nothing at all and the baseline is left with only its vector side.
/// @param chunks - the corpus slice being graded
/// @param keys - the shared chunk key per ordinal
/// @param wanted - how many queries to build
/// @param seed - fixes the sample
pub fn multi_source_queries(
    chunks: &[ChunkInput],
    keys: &[String],
    wanted: usize,
    seed: u64,
) -> Vec<GradedQuery> {
    // Usable headings, per source: the same shape the heading family requires, so
    // each half of the query is independently answerable.
    let mut by_heading: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (c, key_of_chunk) in chunks.iter().zip(keys) {
        if c.deleted {
            continue;
        }
        let Some(leaf) = c.heading_path.last().map(|h| h.trim()) else {
            continue;
        };
        if leaf.chars().count() < 15 || leaf.chars().count() > 90 {
            continue;
        }
        if leaf.split_whitespace().count() < 3 {
            continue;
        }
        by_heading
            .entry((c.source.clone(), leaf.to_string()))
            .or_default()
            .push(key_of_chunk.clone());
    }
    let mut per_source: HashMap<String, Vec<(String, Vec<String>)>> = HashMap::new();
    for ((source, heading), chunk_keys) in by_heading {
        // Same ceiling the heading family uses: a heading spread over many chunks
        // is a section rather than a subject.
        if chunk_keys.len() > 4 {
            continue;
        }
        per_source
            .entry(source)
            .or_default()
            .push((heading, chunk_keys));
    }
    let mut sources: Vec<String> = per_source.keys().cloned().collect();
    sources.sort();
    if sources.len() < 2 {
        return Vec::new();
    }
    for list in per_source.values_mut() {
        list.sort();
    }

    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::new();
    let mut attempts = 0usize;
    while out.len() < wanted && attempts < wanted * 20 {
        attempts += 1;
        // Two source names drawn at random. `get` rather than an index
        // because a draw is the one place a bound is computed rather than
        // written, and this loop already gives up after `wanted * 20` tries.
        let Some(a) = sources.get(rng.gen_range(0..sources.len())) else {
            continue;
        };
        let Some(b) = sources.get(rng.gen_range(0..sources.len())) else {
            continue;
        };
        if a == b {
            continue;
        }
        let (Some(left), Some(right)) = (per_source.get(a), per_source.get(b)) else {
            continue;
        };
        if left.is_empty() || right.is_empty() {
            continue;
        }
        let (Some((lh, lk)), Some((rh, rk))) = (
            left.get(rng.gen_range(0..left.len())),
            right.get(rng.gen_range(0..right.len())),
        ) else {
            continue;
        };
        if lh == rh {
            continue;
        }
        let mut graded: Vec<(String, u8)> = Vec::new();
        for k in lk.iter().chain(rk) {
            graded.push((k.clone(), GRADE_ANSWER));
        }
        let mut correct: Vec<String> = lk.clone();
        correct.extend(rk.clone());
        out.push(GradedQuery {
            id: format!("multi-{a}-{b}-{}", out.len()),
            text: format!("{lh} and {rh}"),
            correct,
            graded,
            source: "any".to_string(),
            family: "multi-source".to_string(),
            answerable: true,
        });
    }
    out
}

/// Take `per_source` queries from each source, deterministically and without
/// replacement. Shared by the families that are built per source.
fn sample_per_source(
    mut by_source: HashMap<String, Vec<GradedQuery>>,
    per_source: usize,
    seed: u64,
) -> Vec<GradedQuery> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut sources: Vec<String> = by_source.keys().cloned().collect();
    sources.sort();
    let mut out = Vec::new();
    for source in sources {
        // The name came out of this map's own keys.
        let Some(mut pool) = by_source.remove(&source) else {
            continue;
        };
        // The pool is built by walking the corpus in order, which is stable, but
        // sort anyway so the sample cannot depend on iteration order.
        pool.sort_by(|a, b| a.id.cmp(&b.id));
        let take = per_source.min(pool.len());
        for _ in 0..take {
            let i = rng.gen_range(0..pool.len());
            out.push(pool.swap_remove(i));
        }
    }
    out
}

/// Embed a batch of query strings with the in process model, applying the
/// `search_query: ` prefix the model is trained with.
///
/// This used to call a `llama-server` child process over HTTP. Running the model
/// in process instead removes the last thing the harness needed that was not in
/// this repository, so a graded run needs no server. Measuring the two
/// against each other gave mean cosine 0.9860 over 200 chunks, with identical
/// success@1 and success@10 across 120 queries, so the substitution does not move
/// the scores.
#[allow(dead_code)]
pub fn embed_queries(
    model: &crate::models::ResolvedModel,
    options: &crate::arm::ArmOptions,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let embedder = open_query_embedder(model, options)?;
    embed_with(&embedder, texts)
}

/// Open one embedding session, for a caller that has several query families to
/// embed.
///
/// Opening a CUDA session costs tens of seconds, and the graded run has nine
/// query families. Opening one per family spent more time loading the model than
/// running it.
/// @param model_dir - directory holding the weights
/// @param manifest - what the model is, including which weights file to open
/// @param device - the processor to open the session on
pub fn open_query_embedder(
    model: &crate::models::ResolvedModel,
    options: &crate::arm::ArmOptions,
) -> Result<crate::arm::Arm> {
    crate::arm::Arm::open(
        model,
        &crate::arm::ArmOptions {
            batch_size: DEFAULT_QUERY_BATCH,
            ..options.clone()
        },
    )
    .context("opening the embedder for the query set. Is ORT_DYLIB_PATH set?")
}

/// Texts per inference call when embedding queries. Queries are short, so this is
/// bounded by the session's own attention budget long before it is bounded by
/// memory; the value only has to be larger than one, which it was not before the
/// families were batched at all.
const DEFAULT_QUERY_BATCH: usize = 16;

/// Embed one family through an already open session.
///
/// Batched rather than one query at a time. `embed_prefixed` groups by true token
/// count and respects the attention memory ceiling, which is the same path the
/// corpus embedding takes; embedding queries one at a time was leaving the card's
/// query embedding twenty times slower than it needed to be for no benefit.
/// @param embedder - an open session
/// @param texts - the raw query strings
pub fn embed_with(embedder: &crate::arm::Arm, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    // The query prefix comes from the arm's own manifest, applied by the arm
    // rather than here. Using the wrong prefix measurably degrades retrieval, and
    // using one model's prefix on another model is worse than using none.
    let sanitized: Vec<String> = texts.iter().map(|t| sanitize(t)).collect();
    embedder
        .embed_queries(&sanitized)
        .context("embedding a query set")
}

/// Drop control characters and quotes. Downloaded text carries the occasional
/// control character, which contributes nothing to an embedding.
fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .collect::<String>()
        .replace('"', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_core::store::ChunkInput;

    fn chunk(
        source: &str,
        doc: &str,
        title: &str,
        content: &str,
        heading: Option<&str>,
    ) -> ChunkInput {
        ChunkInput {
            source: source.into(),
            external_doc_id: doc.into(),
            chunk_index: 0,
            heading_path: heading.map(|h| vec![h.to_string()]).unwrap_or_default(),
            content: content.into(),
            title: title.into(),
            url: "u".into(),
            space_key: None,
            author: None,
            author_id: None,
            updated_at: None,
            external_chunk_id: None,
            labels: vec![],
            attributes: Vec::new(),
            flags: Vec::new(),
            deleted: false,
        }
    }

    fn corpus_of(chunks: Vec<ChunkInput>) -> (Vec<ChunkInput>, Vec<String>) {
        let keys: Vec<String> = (0..chunks.len()).map(|i| format!("k{i}")).collect();
        (chunks, keys)
    }

    #[test]
    fn document_identity_queries_use_the_title_and_mark_every_chunk_correct() {
        let (c, keys) = corpus_of(vec![
            chunk(
                "confluence",
                "d1",
                "Offer eligibility rules for members",
                "a",
                None,
            ),
            chunk(
                "confluence",
                "d1",
                "Offer eligibility rules for members",
                "b",
                None,
            ),
        ]);
        let qs = document_identity_queries(&c, &keys, 10, 1);
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].text, "Offer eligibility rules for members");
        assert_eq!(qs[0].correct.len(), 2, "both chunks of the document count");
    }

    #[test]
    fn a_title_shared_by_two_documents_is_skipped_because_correct_is_ambiguous() {
        let (c, keys) = corpus_of(vec![
            chunk(
                "confluence",
                "d1",
                "Weekly engineering sync notes",
                "a",
                None,
            ),
            chunk(
                "confluence",
                "d2",
                "Weekly engineering sync notes",
                "b",
                None,
            ),
        ]);
        assert!(document_identity_queries(&c, &keys, 10, 1).is_empty());
    }

    #[test]
    fn a_one_word_or_very_short_title_is_skipped() {
        let (c, keys) = corpus_of(vec![
            chunk("confluence", "d1", "Notes", "a", None),
            chunk("confluence", "d2", "Standup", "b", None),
        ]);
        assert!(document_identity_queries(&c, &keys, 10, 1).is_empty());
    }

    #[test]
    fn deleted_chunks_never_become_ground_truth() {
        let mut c1 = chunk(
            "confluence",
            "d1",
            "Offer eligibility rules here",
            "a",
            None,
        );
        c1.deleted = true;
        let (c, keys) = corpus_of(vec![c1]);
        assert!(document_identity_queries(&c, &keys, 10, 1).is_empty());
    }

    #[test]
    fn query_generation_is_deterministic_for_a_seed() {
        let chunks: Vec<ChunkInput> = (0..40)
            .map(|i| {
                chunk(
                    "confluence",
                    &format!("d{i}"),
                    &format!("Distinct engineering document number {i}"),
                    "content",
                    None,
                )
            })
            .collect();
        let (c, keys) = corpus_of(chunks);
        let a = document_identity_queries(&c, &keys, 5, 99);
        let b = document_identity_queries(&c, &keys, 5, 99);
        assert_eq!(
            a.iter().map(|q| q.text.clone()).collect::<Vec<_>>(),
            b.iter().map(|q| q.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn identifier_queries_pick_rare_tokens_only() {
        let mut chunks = Vec::new();
        // A common identifier, in many chunks.
        for i in 0..20 {
            chunks.push(chunk(
                "github",
                &format!("d{i}"),
                "t",
                "common-id-42 appears everywhere",
                None,
            ));
        }
        // A rare one, in a single chunk.
        chunks.push(chunk(
            "github",
            "rare",
            "t",
            "the token ENX-1932 appears once",
            None,
        ));
        let (c, keys) = corpus_of(chunks);
        let qs = identifier_queries(&c, &keys, 50, 5);
        let texts: Vec<&str> = qs.iter().map(|q| q.text.as_str()).collect();
        assert!(texts.contains(&"ENX-1932"), "got {texts:?}");
        assert!(
            !texts.contains(&"common-id-42"),
            "a common token must not be used"
        );
    }

    #[test]
    fn heading_queries_need_a_multi_word_heading() {
        let (c, keys) = corpus_of(vec![
            chunk(
                "confluence",
                "d1",
                "t",
                "body",
                Some("How offer eligibility is evaluated"),
            ),
            chunk("confluence", "d2", "t2", "body", Some("Notes")),
        ]);
        let qs = heading_queries(&c, &keys, 10, 1);
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].text, "How offer eligibility is evaluated");
    }

    #[test]
    fn sanitize_removes_control_characters_and_quotes() {
        assert_eq!(sanitize("a\u{0007}b\"c"), "ab c");
    }
}
