//! Measures how well each search mode finds the right article.
//!
//! `questions.json` holds questions and the Wikipedia article whose chunks have
//! to come back in the top `k`. A question with `"article": null` is one the
//! corpus cannot answer. For those the evaluation records how confident the
//! best hit looked, because an agent has to be able to tell that it should
//! say "the articles do not cover this".
//!
//! The two numbers per mode:
//!
//! | Number | Meaning |
//! |---|---|
//! | found | how many answerable questions had a chunk of the right article in the top `k` |
//! | MRR | mean reciprocal rank: 1 when the right article is always first, 0.5 when it is always second |

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::search::{search, Mode, SearchRequest};
use crate::store::Store;

/// One evaluation question.
#[derive(Clone, Debug, Deserialize)]
pub struct Question {
    /// The question, as an agent would pass it.
    pub question: String,
    /// The article that answers it, or nothing when the corpus has no answer.
    pub article: Option<String>,
}

/// The result of one question in one mode.
#[derive(Clone, Debug, Serialize)]
pub struct Answer {
    /// The question.
    pub question: String,
    /// The article it needed, if any.
    pub article: Option<String>,
    /// The rank of the first chunk from that article, if it was in the top `k`.
    pub rank: Option<usize>,
    /// The titles of the top `k` hits.
    pub titles: Vec<String>,
    /// The best hit's confidence, in the modes that report one.
    pub top_confidence: Option<f64>,
    /// The best hit's cosine distance, in `vector` mode.
    pub top_distance: Option<f64>,
    /// Milliseconds for the search.
    pub elapsed_ms: f64,
}

/// The summary for one mode.
#[derive(Clone, Debug, Serialize)]
pub struct ModeSummary {
    /// The mode.
    pub mode: Mode,
    /// Answerable questions whose article was in the top `k`.
    pub found: usize,
    /// How many questions are answerable.
    pub answerable: usize,
    /// Mean reciprocal rank over the answerable questions.
    pub mrr: f64,
    /// The median search time in milliseconds.
    pub median_ms: f64,
    /// Every question's result.
    pub answers: Vec<Answer>,
}

/// Reads the questions file.
///
/// @param path - `questions.json`
pub fn read_questions(path: &Path) -> Result<Vec<Question>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// Asks every question in every mode and summarises each mode.
///
/// Each question is asked once before timing starts, so the first mode does
/// not pay for loading the model.
///
/// @param store - the database
/// @param questions - the questions
/// @param modes - the modes to compare
/// @param k - how many hits count as found
pub fn evaluate(store: &Store, questions: &[Question], modes: &[Mode], k: usize) -> Result<Vec<ModeSummary>, String> {
    store.embed("search_query: warm the model")?;
    let mut summaries = Vec::new();
    for mode in modes {
        let mut answers = Vec::new();
        for question in questions {
            answers.push(ask(store, question, *mode, k)?);
        }
        summaries.push(summarise(*mode, answers));
    }
    Ok(summaries)
}

/// Asks one question in one mode.
///
/// @param store - the database
/// @param question - the question
/// @param mode - the mode
/// @param k - how many hits to ask for
fn ask(store: &Store, question: &Question, mode: Mode, k: usize) -> Result<Answer, String> {
    let request = SearchRequest { query: question.question.clone(), mode, k, title: None };
    let result = search(store, &request)?;
    let titles: Vec<String> = result.hits.iter().map(|hit| hit.title.clone()).collect();
    let rank = question.article.as_ref().and_then(|article| titles.iter().position(|t| t == article)).map(|at| at + 1);
    Ok(Answer {
        question: question.question.clone(),
        article: question.article.clone(),
        rank,
        top_confidence: result.hits.first().and_then(|hit| hit.confidence),
        top_distance: result.hits.first().and_then(|hit| hit.distance),
        titles,
        elapsed_ms: result.elapsed_ms,
    })
}

/// Adds up one mode's answers.
///
/// @param mode - the mode
/// @param answers - every question's result in that mode
fn summarise(mode: Mode, answers: Vec<Answer>) -> ModeSummary {
    let answerable: Vec<&Answer> = answers.iter().filter(|a| a.article.is_some()).collect();
    let found = answerable.iter().filter(|a| a.rank.is_some()).count();
    let reciprocal: f64 = answerable.iter().filter_map(|a| a.rank).map(|rank| 1.0 / rank as f64).sum();
    let mrr = if answerable.is_empty() { 0.0 } else { reciprocal / answerable.len() as f64 };
    let mut times: Vec<f64> = answers.iter().map(|a| a.elapsed_ms).collect();
    times.sort_by(f64::total_cmp);
    let median_ms = times.get(times.len() / 2).copied().unwrap_or(0.0);
    ModeSummary { mode, found, answerable: answerable.len(), mrr: (mrr * 1000.0).round() / 1000.0, median_ms, answers }
}

/// Writes the summaries as a Markdown table, one row per mode.
///
/// @param summaries - the result of [`evaluate`]
pub fn markdown_table(summaries: &[ModeSummary]) -> String {
    let mut table = String::from("| mode | found in top k | MRR | median ms |\n|---|---:|---:|---:|\n");
    for summary in summaries {
        table.push_str(&format!(
            "| {} | {} of {} | {:.3} | {:.1} |\n",
            summary.mode.name(),
            summary.found,
            summary.answerable,
            summary.mrr,
            summary.median_ms
        ));
    }
    table
}
