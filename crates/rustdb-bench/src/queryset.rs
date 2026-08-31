//! The graded query sets, and the embedding of their queries.
//!
//! Queries are embedded once, in process, and both engines receive the identical
//! query vector. Query embedding therefore cancels out of the comparison entirely,
//! which is what lets the score card attribute a difference to the index rather
//! than to the model.

use rustdb_core::embed_onnx::{Device, OnnxEmbedder, OnnxOptions};
use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

use rustdb_core::store::ChunkInput;

/// A query with an objectively correct answer set.
#[derive(Clone, Serialize, Deserialize)]
pub struct GradedQuery {
    pub text: String,
    /// Source database chunk identifiers that count as correct. Empty for
    /// scenarios graded against exhaustive cosine rather than document identity.
    pub correct: Vec<String>,
    /// Which source the query is drawn from, so per source scoring is possible.
    pub source: String,
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
    for (i, c) in chunks.iter().enumerate() {
        if c.deleted {
            continue;
        }
        let key = (c.source.clone(), c.external_doc_id.clone());
        chunks_of_doc.entry(key.clone()).or_default().push(keys[i].clone());
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
        by_source.entry(key.0.clone()).or_default().push(GradedQuery {
            text: trimmed.to_string(),
            correct,
            source: key.0.clone(),
        });
    }

    let mut out = Vec::new();
    let mut sources: Vec<String> = by_source.keys().cloned().collect();
    sources.sort();
    for source in sources {
        let mut pool = by_source.remove(&source).unwrap();
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
    for (i, c) in chunks.iter().enumerate() {
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
                occurrences.entry(token).or_default().push(keys[i].clone());
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
        out.push(GradedQuery {
            text: token,
            correct: chunks,
            source: "any".to_string(),
        });
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
    for (i, c) in chunks.iter().enumerate() {
        if c.deleted || c.heading_path.is_empty() {
            continue;
        }
        let leaf = c.heading_path.last().unwrap().trim();
        if leaf.chars().count() < 15 || leaf.chars().count() > 120 {
            continue;
        }
        if leaf.split_whitespace().count() < 3 {
            continue;
        }
        by_heading.entry(leaf.to_string()).or_default().push(keys[i].clone());
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
        let source = source_of.get(&heading).cloned().unwrap_or_else(|| "any".into());
        out.push(GradedQuery { text: heading, correct: chunks, source });
    }
    out
}

/// Embed a batch of query strings with the in process model, applying the
/// `search_query: ` prefix the model is trained with.
///
/// This used to call a `llama-server` child process over HTTP. Running the model
/// in process instead removes the last thing the harness needed that was not in
/// this repository, so a graded run needs no server. task-21 measured the two
/// against each other: mean cosine 0.9860 over 200 chunks, with identical
/// success@1 and success@10 across 120 queries, so the substitution does not move
/// the scores.
pub fn embed_queries(model_dir: &str, model_file: &str, texts: &[String], device: Device) -> Result<Vec<Vec<f32>>> {
    use rustdb_core::embed::Embedder;

    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let embedder = OnnxEmbedder::open_model(model_dir, model_file, OnnxOptions { device, ..Default::default() })
        .context("opening the ONNX embedder for the query set. Is ORT_DYLIB_PATH set?")?;
    let cleaned: Vec<String> = texts.iter().map(|t| sanitize(t)).collect();
    // `embed_documents` would apply the document prefix; a query needs the query
    // prefix, and using the wrong one measurably degrades retrieval.
    let mut out = Vec::with_capacity(cleaned.len());
    for text in &cleaned {
        out.push(embedder.embed_query(text).context("embedding a query")?);
    }
    Ok(out)
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
    use rustdb_core::store::ChunkInput;

    fn chunk(source: &str, doc: &str, title: &str, content: &str, heading: Option<&str>) -> ChunkInput {
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
            labels: vec![],
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
            chunk("confluence", "d1", "Offer eligibility rules for members", "a", None),
            chunk("confluence", "d1", "Offer eligibility rules for members", "b", None),
        ]);
        let qs = document_identity_queries(&c, &keys, 10, 1);
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].text, "Offer eligibility rules for members");
        assert_eq!(qs[0].correct.len(), 2, "both chunks of the document count");
    }

    #[test]
    fn a_title_shared_by_two_documents_is_skipped_because_correct_is_ambiguous() {
        let (c, keys) = corpus_of(vec![
            chunk("confluence", "d1", "Weekly engineering sync notes", "a", None),
            chunk("confluence", "d2", "Weekly engineering sync notes", "b", None),
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
        let mut c1 = chunk("confluence", "d1", "Offer eligibility rules here", "a", None);
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
            chunks.push(chunk("github", &format!("d{i}"), "t", "common-id-42 appears everywhere", None));
        }
        // A rare one, in a single chunk.
        chunks.push(chunk("github", "rare", "t", "the token ENX-1932 appears once", None));
        let (c, keys) = corpus_of(chunks);
        let qs = identifier_queries(&c, &keys, 50, 5);
        let texts: Vec<&str> = qs.iter().map(|q| q.text.as_str()).collect();
        assert!(texts.contains(&"ENX-1932"), "got {texts:?}");
        assert!(!texts.contains(&"common-id-42"), "a common token must not be used");
    }

    #[test]
    fn heading_queries_need_a_multi_word_heading() {
        let (c, keys) = corpus_of(vec![
            chunk("confluence", "d1", "t", "body", Some("How offer eligibility is evaluated")),
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
