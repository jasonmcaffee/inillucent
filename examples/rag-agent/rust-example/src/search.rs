//! The four ways the server can search, and reciprocal rank fusion.
//!
//! | Mode | What ranks the chunks |
//! |---|---|
//! | `hybrid` | the `inillucent_search` table, with the question's words and its vector in one query. The engine fuses the two rankings and adds a `confidence` |
//! | `vector` | cosine distance over the plain `chunk.v` column |
//! | `keyword` | the `inillucent_search` table with the words only: BM25 with proximity and phrase adjustments |
//! | `rrf` | the `vector` list and the `keyword` list, fused here with reciprocal rank fusion |
//!
//! `hybrid` and `rrf` answer the same need in two places. `hybrid` lets the
//! database fuse the lists in one query. `rrf` is the method most RAG systems
//! use when the vector store and the keyword index are separate programs, and
//! it works on any two ranked lists. On this corpus `rrf` ranks the right
//! article higher, so it is the default. The README has the measurements.
//!
//! Every hit, in every mode but `keyword`, also carries its cosine `distance`
//! from the question. It is the one number here that tells a question on
//! another subject apart: those score above about 0.4 on this corpus.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::store::Store;

/// The constant in reciprocal rank fusion, `1 / (60 + rank)`.
///
/// 60 is the value in Cormack, Clarke and Buettcher's 2009 paper that
/// introduced the method, and the usual default. A larger constant flattens the
/// difference between first and tenth place, so a chunk both lists rank
/// moderately beats one that only one list ranks first.
pub const RRF_CONSTANT: f64 = 60.0;

/// How many results each list contributes to `rrf`, as a multiple of `k`.
///
/// A chunk ranked 12th by keyword and 2nd by vector should still be fused.
/// Asking each list for only `k` results would drop it before fusion.
const RRF_DEPTH: usize = 3;

/// Common words left out of the keyword query.
///
/// A question such as "what did the Stoics believe about death" would
/// otherwise match every chunk containing "the" or "about". BM25 gives such
/// words little weight, and removing them keeps the query short and the
/// keyword ranking focused on the words that carry the question.
const STOP_WORDS: [&str; 64] = [
    "a", "about", "after", "all", "also", "an", "and", "any", "are", "as", "at", "be", "been", "but", "by", "can", "could", "did", "do",
    "does", "for", "from", "had", "has", "have", "he", "her", "his", "how", "i", "if", "in", "into", "is", "it", "its", "me", "my", "no",
    "not", "of", "on", "or", "our", "she", "so", "than", "that", "the", "their", "them", "then", "there", "they", "this", "to", "was",
    "we", "were", "what", "when", "which", "who", "why",
];

/// How to search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Keyword and vector ranking fused by the `inillucent_search` table.
    Hybrid,
    /// Cosine distance over the `VECTOR(768)` column.
    Vector,
    /// BM25 over the `inillucent_search` table.
    Keyword,
    /// The vector and keyword lists fused here with reciprocal rank fusion.
    Rrf,
}

impl Mode {
    /// Every mode, in the order the evaluation reports them.
    pub const ALL: [Mode; 4] = [Mode::Hybrid, Mode::Vector, Mode::Keyword, Mode::Rrf];

    /// Returns the mode's name as the tools spell it.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Hybrid => "hybrid",
            Mode::Vector => "vector",
            Mode::Keyword => "keyword",
            Mode::Rrf => "rrf",
        }
    }
}

/// One search, as the `search` tool receives it.
#[derive(Clone, Debug)]
pub struct SearchRequest {
    /// The question, in plain words.
    pub query: String,
    /// How to search.
    pub mode: Mode,
    /// How many chunks to return, from 1 to 20.
    pub k: usize,
    /// Only chunks from the document with this exact title.
    pub title: Option<String>,
}

/// One chunk a search returned.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Hit {
    /// The position in the results, from 1.
    pub rank: usize,
    /// The chunk's id, which `get_passage` takes.
    pub chunk_id: i64,
    /// The document's title.
    pub title: String,
    /// The document's URL.
    pub url: String,
    /// The chunk's text.
    pub text: String,
    /// `hybrid` and `keyword`: the score the engine ranked by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// `hybrid` and `keyword`: how good the hit is on a fixed scale from 0 to 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// `hybrid` and `keyword`: which search found it, `lexical`, `vector` or `both`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Every mode but `keyword`: the cosine distance from the question. 0 is the same direction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
    /// `rrf`: the fused score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rrf_score: Option<f64>,
    /// `rrf`: the chunk's rank in the vector list, if it was in the top three times `k`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_rank: Option<usize>,
    /// `rrf`: the chunk's rank in the keyword list, if it was in it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyword_rank: Option<usize>,
}

/// What a search returns.
#[derive(Clone, Debug, Serialize)]
pub struct SearchResult {
    /// The question as it was asked.
    pub query: String,
    /// The mode used.
    pub mode: Mode,
    /// The keyword expression sent to the index, when the mode used one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    /// Milliseconds for the whole search.
    pub elapsed_ms: f64,
    /// The chunks, best first.
    pub hits: Vec<Hit>,
}

/// Runs one search.
///
/// @param store - the database
/// @param request - what to search for and how
pub fn search(store: &Store, request: &SearchRequest) -> Result<SearchResult, String> {
    let query = request.query.trim();
    if query.is_empty() {
        return Err("the query is empty. Pass the question in `query`".to_string());
    }
    if !(1..=20).contains(&request.k) {
        return Err(format!("k is {}; it has to be from 1 to 20", request.k));
    }
    let started = Instant::now();
    let keywords = keyword_expression(query);
    // Every mode but `keyword` compares the question's meaning. The question
    // is passed as text and embedded by `embed(TEXT)` inside each statement.
    let question = if request.mode == Mode::Keyword { None } else { Some(query) };
    let title = request.title.as_deref();
    let k = request.k;
    let mut hits = match request.mode {
        Mode::Vector => vector_hits(store, query, k, title)?,
        Mode::Keyword => match &keywords {
            Some(expression) => table_hits(store, Some(expression), None, k, title)?,
            None => Vec::new(),
        },
        Mode::Hybrid => table_hits(store, keywords.as_deref(), question, k, title)?,
        Mode::Rrf => rrf_hits(store, keywords.as_deref(), query, k, title)?,
    };
    fill_chunks(store, &mut hits, question)?;
    Ok(SearchResult {
        query: query.to_string(),
        mode: request.mode,
        keywords: if request.mode == Mode::Vector { None } else { keywords },
        elapsed_ms: milliseconds(started),
        hits,
    })
}

/// Turns a question into an FTS5 expression: each word quoted, joined by `OR`.
///
/// Quoting matters. FTS5 reads `-`, `"`, `*`, `(` and the words `AND`, `OR`
/// and `NOT` as operators, so "what is Plato's cave?" passed as it is would be a
/// syntax error, and "Stoics - who were they" would exclude a word. Returns
/// nothing when every word is a stop word.
///
/// @param question - the question in plain words
pub fn keyword_expression(question: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    for word in question.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.chars().count() > 1 && !STOP_WORDS.contains(&word.as_str()) && !words.contains(&word) {
            words.push(word);
        }
    }
    if words.is_empty() {
        return None;
    }
    Some(words.iter().map(|word| format!("\"{word}\"")).collect::<Vec<_>>().join(" OR "))
}

/// Fuses ranked lists with reciprocal rank fusion.
///
/// Each item scores `1 / (constant + rank)` for every list it appears in, with
/// ranks counted from 1, and the scores are added. The result is best first,
/// with each item's rank in every list.
///
/// @param lists - the ranked lists of ids, best first
/// @param constant - the constant, usually [`RRF_CONSTANT`]
pub fn reciprocal_rank_fusion(lists: &[Vec<i64>], constant: f64) -> Vec<(i64, f64, Vec<Option<usize>>)> {
    let mut fused: Vec<(i64, f64, Vec<Option<usize>>)> = Vec::new();
    for (list_index, list) in lists.iter().enumerate() {
        for (position, id) in list.iter().enumerate() {
            let rank = position + 1;
            let entry = match fused.iter().position(|(existing, _, _)| existing == id) {
                Some(at) => &mut fused[at],
                None => {
                    fused.push((*id, 0.0, vec![None; lists.len()]));
                    fused.last_mut().expect("just pushed")
                }
            };
            entry.1 += 1.0 / (constant + rank as f64);
            entry.2[list_index] = Some(rank);
        }
    }
    fused.sort_by(|a, b| b.1.total_cmp(&a.1));
    fused
}

/// Searches the plain vector column.
///
/// @param store - the database
/// @param question - the question, embedded by the statement
/// @param k - how many chunks
/// @param title - an optional document title
fn vector_hits(store: &Store, question: &str, k: usize, title: Option<&str>) -> Result<Vec<Hit>, String> {
    Ok(store
        .vector_hits(question, k, title)?
        .into_iter()
        .map(|(chunk_id, distance)| Hit { chunk_id, distance: Some(round(distance)), ..Hit::default() })
        .collect())
}

/// Searches the `inillucent_search` table by keywords, by vector, or by both.
///
/// @param store - the database
/// @param keywords - the FTS5 expression, if any
/// @param question - the question to embed, if the mode compares meaning
/// @param k - how many chunks
/// @param title - an optional document title
fn table_hits(store: &Store, keywords: Option<&str>, question: Option<&str>, k: usize, title: Option<&str>) -> Result<Vec<Hit>, String> {
    Ok(store
        .search_table_hits(keywords, question, k, title)?
        .into_iter()
        .map(|hit| Hit {
            chunk_id: hit.chunk_id,
            score: Some(round(hit.score)),
            confidence: Some(round(hit.confidence)),
            origin: Some(hit.origin),
            ..Hit::default()
        })
        .collect())
}

/// Runs the vector search and the keyword search, then fuses them with RRF.
///
/// @param store - the database
/// @param keywords - the FTS5 expression, if the question has any words left
/// @param question - the question, embedded by the vector statement
/// @param k - how many chunks to return
/// @param title - an optional document title
fn rrf_hits(store: &Store, keywords: Option<&str>, question: &str, k: usize, title: Option<&str>) -> Result<Vec<Hit>, String> {
    let depth = k * RRF_DEPTH;
    let vector_list = store.vector_hits(question, depth, title)?;
    let by_vector: Vec<i64> = vector_list.iter().map(|(id, _)| *id).collect();
    let by_keyword: Vec<i64> = match keywords {
        Some(expression) => store.search_table_hits(Some(expression), None, depth, title)?.into_iter().map(|h| h.chunk_id).collect(),
        None => Vec::new(),
    };
    Ok(reciprocal_rank_fusion(&[by_vector, by_keyword], RRF_CONSTANT)
        .into_iter()
        .take(k)
        .map(|(chunk_id, score, ranks)| Hit {
            chunk_id,
            rrf_score: Some((score * 1_000_000.0).round() / 1_000_000.0),
            vector_rank: ranks[0],
            keyword_rank: ranks[1],
            // The vector statement already measured this chunk's distance, so
            // the fill does not have to embed the question a second time.
            distance: vector_list.iter().find(|(id, _)| *id == chunk_id).map(|(_, distance)| round(*distance)),
            ..Hit::default()
        })
        .collect())
}

/// Adds each hit's rank, title, URL, text and distance from the question.
///
/// @param store - the database
/// @param hits - the hits, best first
/// @param question - the question, when the mode compares meaning
fn fill_chunks(store: &Store, hits: &mut [Hit], question: Option<&str>) -> Result<(), String> {
    let ids: Vec<i64> = hits.iter().map(|hit| hit.chunk_id).collect();
    // Each statement that names `embed(...)` embeds the question again, about
    // 10 ms. The distance is asked for only when a hit does not have one yet.
    let missing = hits.iter().any(|hit| hit.distance.is_none());
    let rows = store.chunk_rows(&ids, if missing { question } else { None })?;
    for (index, hit) in hits.iter_mut().enumerate() {
        hit.rank = index + 1;
        if let Some(row) = rows.get(&hit.chunk_id) {
            hit.title = row.title.clone();
            hit.url = row.url.clone();
            hit.text = row.text.clone();
            hit.distance = hit.distance.or(row.distance.map(round));
        }
    }
    Ok(())
}

/// Returns the milliseconds since an instant, to a tenth.
///
/// @param since - when the timing started
fn milliseconds(since: Instant) -> f64 {
    (since.elapsed().as_secs_f64() * 10_000.0).round() / 10.0
}

/// Rounds a score to four places for display.
///
/// @param value - the score
fn round(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_becomes_quoted_words_joined_by_or() {
        assert_eq!(keyword_expression("what is Plato's cave?").as_deref(), Some("\"plato\" OR \"cave\""));
        assert_eq!(keyword_expression("the Stoics - who were they").as_deref(), Some("\"stoics\""));
        assert_eq!(keyword_expression("what is it"), None);
    }

    #[test]
    fn a_chunk_in_both_lists_beats_one_in_a_single_list() {
        let fused = reciprocal_rank_fusion(&[vec![1, 2, 3], vec![3, 4, 1]], RRF_CONSTANT);
        assert_eq!(fused[0].0, 1, "ranked 1st and 3rd");
        assert_eq!(fused[1].0, 3, "ranked 3rd and 1st");
        assert_eq!(fused[0].2, vec![Some(1), Some(3)]);
        let four = fused.iter().find(|(id, _, _)| *id == 4).expect("4 is in the result");
        assert_eq!(four.2, vec![None, Some(2)]);
    }
}
