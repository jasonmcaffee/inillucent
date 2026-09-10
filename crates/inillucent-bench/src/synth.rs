//! Building the graded corpus from public data.
//!
//! The engine used to be graded on a live PostgreSQL database holding one
//! organisation's Confluence, Slack, JIRA, GitHub, Figma and Miro content. That
//! content cannot be published, so it cannot be part of a public repository and
//! nobody outside that organisation could reproduce a single number on the score
//! card. This module replaces it with a corpus assembled from public sources.
//!
//! It is a synthetic corpus in the sense that matters: the six sources, the
//! documents, the titles, the authors, the spaces, the labels and the identifiers
//! are constructed here. The sentences inside the chunks are real public text,
//! because generated filler does not exercise a lexical index. Term frequencies,
//! sentence length, vocabulary growth and the way rare words cluster are all
//! properties BM25 scoring depends on, and text from a template has none of them.
//!
//! What the corpus has to preserve, and why each one matters:
//!
//! - **The size and the per source split.** Which retrieval path a filtered query
//!   takes is decided by how many chunks pass the predicate, and the crossover
//!   between walking the graph and scanning exactly sits inside the range these
//!   six sources span. Change the sizes and the scenarios stop measuring what
//!   they were written to measure.
//! - **Chunk order that correlates with source.** In the original database chunks
//!   were numbered in ingestion order, so the first 77,600 were all one source. A
//!   prefix of the corpus is therefore not a sample of it, which is why
//!   `strided_sample` exists. The phases below reproduce that.
//! - **Titles written to describe their own content, unique per document.** The
//!   document identity ground truth uses a title as a query and counts that
//!   document's chunks as correct, skipping any title two documents share.
//!   Templated or repeated titles would quietly empty that ground truth.
//! - **Rare literal identifiers.** One scenario measures finding a token that
//!   appears in at most five chunks. Real source code supplies function names and
//!   file paths; ticket keys are threaded through the prose sources the way a real
//!   document references a ticket.
//!
//! Every source draws from a disjoint pool. If a Wikipedia article supplied both
//! a page chunk and a design file chunk, a query matching one would match its
//! twin in another source, and every filtered measurement would be distorted by
//! content that exists twice.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use inillucent_core::embed_onnx::{count_truncation, Device};
use inillucent_core::model::{Backend, ModelManifest};

use crate::arm::{Arm, ArmOptions};
use inillucent_core::store::ChunkInput;
use serde::{Deserialize, Serialize};

/// Fixed so two runs of the builder produce a byte identical corpus.
const SEED: u64 = 0x5115_7A57;

/// The order documents are laid down in, which reproduces the ingestion order of
/// the original database: large single source blocks first, then a tail in which
/// every source interleaves. `Phase::Tail` is what stops a prefix of the corpus
/// from being a usable sample of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    PagesBulk,
    MessagesAndTickets,
    CodeBulk,
    BoardsBulk,
    Tail,
}

/// How a source assigns `space_key`, which stands for a Confluence space, a
/// repository, a channel, a project or a single file depending on the source.
#[derive(Debug, Clone, Copy)]
enum Spaces {
    /// A fixed number of spaces shared across the source's documents.
    Shared(usize),
    /// One space per document, which is how a design file or a board behaves.
    PerDocument,
}

/// Where a source's text comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Material {
    /// Encyclopedia articles, kept as prose with section headings.
    Articles,
    /// Discussion pages, split into threads.
    Discussions,
    /// Source files, split on line boundaries.
    Code,
    /// Issue threads.
    Issues,
    /// Articles reformatted as the text layers of a design file.
    DesignFiles,
    /// Articles reformatted as the notes on a board.
    Boards,
}

/// The measured shape of one source. Every number here was read out of the
/// original database rather than chosen, which is why they are not round.
#[derive(Debug, Clone)]
struct SourcePlan {
    name: &'static str,
    material: Material,
    documents: usize,
    chunks: usize,
    /// Target mean chunk length in characters.
    mean_chars: usize,
    /// Chunks per document as (minimum, median, ninetieth percentile, maximum).
    /// Reproducing the shape matters more than the mean: a source whose documents
    /// are all the same size would make the per document result cap meaningless.
    per_doc: (usize, usize, usize, usize),
    /// Share of chunks at heading depth 0, 1, 2 and 3.
    heading_depth: [f64; 4],
    spaces: Spaces,
    /// Distinct authors, or zero for a source that records none.
    authors: usize,
    mean_labels: f64,
    /// Share of this source's documents laid down in the bulk phase, the rest
    /// going to the interleaved tail.
    bulk_share: f64,
    bulk_phase: Phase,
}

/// The six sources at the scale that was measured. A scale factor multiplies the
/// document and chunk counts, so a larger or a smaller corpus keeps these
/// proportions.
fn plans(scale: f64) -> Vec<SourcePlan> {
    let s = |n: usize| ((n as f64) * scale).round() as usize;
    vec![
        SourcePlan {
            name: "confluence",
            material: Material::Articles,
            documents: s(12_291),
            chunks: s(93_915),
            mean_chars: 825,
            per_doc: (1, 5, 17, 157),
            heading_depth: [0.084, 0.475, 0.347, 0.094],
            spaces: Spaces::Shared(1),
            authors: 473,
            mean_labels: 2.26,
            bulk_share: 0.83,
            bulk_phase: Phase::PagesBulk,
        },
        SourcePlan {
            name: "github",
            material: Material::Code,
            documents: s(5_664),
            chunks: s(47_543),
            mean_chars: 650,
            per_doc: (3, 8, 12, 85),
            heading_depth: [0.0, 0.123, 0.775, 0.102],
            spaces: Spaces::Shared(8),
            authors: 106,
            mean_labels: 3.31,
            bulk_share: 0.80,
            bulk_phase: Phase::CodeBulk,
        },
        SourcePlan {
            name: "slack",
            material: Material::Discussions,
            documents: s(16_067),
            chunks: s(17_675),
            mean_chars: 1_284,
            per_doc: (1, 1, 1, 15),
            heading_depth: [0.0, 0.9967, 0.0022, 0.0011],
            spaces: Spaces::Shared(51),
            authors: 416,
            mean_labels: 2.0,
            bulk_share: 0.15,
            bulk_phase: Phase::MessagesAndTickets,
        },
        SourcePlan {
            name: "jira",
            material: Material::Issues,
            documents: s(1_967),
            chunks: s(11_160),
            mean_chars: 509,
            per_doc: (2, 4, 12, 53),
            heading_depth: [0.0, 0.187, 0.467, 0.346],
            spaces: Spaces::Shared(22),
            authors: 80,
            mean_labels: 1.73,
            bulk_share: 0.45,
            bulk_phase: Phase::MessagesAndTickets,
        },
        SourcePlan {
            name: "figma",
            material: Material::DesignFiles,
            documents: s(1_014),
            chunks: s(9_149),
            mean_chars: 2_222,
            per_doc: (1, 5, 19, 66),
            heading_depth: [0.001, 0.125, 0.874, 0.0],
            spaces: Spaces::PerDocument,
            authors: 0,
            mean_labels: 2.0,
            bulk_share: 0.45,
            bulk_phase: Phase::BoardsBulk,
        },
        SourcePlan {
            name: "miro",
            material: Material::Boards,
            documents: s(2_363),
            chunks: s(7_397),
            mean_chars: 1_186,
            per_doc: (1, 2, 3, 164),
            heading_depth: [0.0, 0.539, 0.452, 0.008],
            spaces: Spaces::PerDocument,
            authors: 0,
            mean_labels: 2.0,
            bulk_share: 0.88,
            bulk_phase: Phase::BoardsBulk,
        },
    ]
}

// ---------------------------------------------------------------------------
// The public material, as the extraction scripts write it.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawArticle {
    title: String,
    #[serde(default)]
    headings: Vec<String>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    timestamp: Option<String>,
    text: String,
}

#[derive(Debug, Deserialize)]
struct RawTalk {
    title: String,
    #[serde(default)]
    headings: Vec<String>,
    #[serde(default)]
    timestamp: Option<String>,
    text: String,
}

#[derive(Debug, Deserialize)]
struct RawCode {
    repo: String,
    path: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct RawIssue {
    repo: String,
    number: u64,
    title: String,
    body: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let file = File::open(path)
        .with_context(|| format!("opening {}. Run the fetch and extract scripts first", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(&line) {
            Ok(v) => out.push(v),
            // One malformed line in a downloaded dump should not lose the corpus.
            Err(_) => continue,
        }
    }
    Ok(out)
}

/// Seconds since the epoch from an ISO 8601 timestamp, without pulling in a date
/// library for the one field that needs it.
fn epoch_seconds(iso: &str) -> Option<i64> {
    let bytes = iso.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| iso.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days from the civil date, by Howard Hinnant's algorithm.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + s)
}

// ---------------------------------------------------------------------------
// Splitting text into chunks.
// ---------------------------------------------------------------------------

/// Put the document title and the heading path at the front of the chunk text, the
/// way the corpus this one reproduces did.
///
/// This is not decoration, and it is not a guess. Measured on that corpus, **every**
/// chunk contained its document title in its body, 186,860 of 186,860, and every
/// chunk carrying a heading contained that heading too, 178,967 of 178,967. The
/// title sits at offset 1, so it comes first, and the headings follow it joined by
/// ` > `, which is the shape those rows have.
///
/// It decides what two whole scenario families measure. One queries with a document
/// title and counts that document's chunks as correct; the other queries with a
/// section heading. With the title and the heading absent from the text there is
/// almost nothing for either engine to match, and both collapse together: querying
/// by heading measured 0.078 success@10 against 0.933 on the original corpus, and
/// querying by title measured 0.450 success@1 against 0.878. A corpus that omits
/// them is not a harder corpus, it is one where those questions cannot be answered.
fn breadcrumbed(title: &str, path: &[String], body: &str) -> String {
    let mut prefix = String::with_capacity(title.len() + 64);
    prefix.push_str(title.trim());
    for h in path {
        prefix.push_str(" > ");
        prefix.push_str(h.trim());
    }
    format!("{prefix}\n\n{body}")
}

/// How many characters the breadcrumb is expected to take, so the body can be cut
/// shorter and the whole chunk still land near the measured mean length. The
/// measured means include the breadcrumb, because the corpus they were measured on
/// carried it.
fn breadcrumb_estimate(title: &str, headings: &[String], depth_shares: &[f64; 4]) -> usize {
    let mean_heading = if headings.is_empty() {
        0
    } else {
        headings.iter().map(|h| h.chars().count()).sum::<usize>() / headings.len()
    };
    let expected_depth: f64 = depth_shares
        .iter()
        .enumerate()
        .map(|(d, share)| d as f64 * share)
        .sum();
    title.chars().count() + (expected_depth * (mean_heading + 3) as f64).round() as usize + 2
}

/// Split prose into pieces of roughly `target` characters, breaking at a sentence
/// end where one is nearby so a chunk does not begin mid sentence. Chunking that
/// ignored sentence boundaries would give the lexical index truncated words and
/// the embedder fragments, neither of which is what a real ingestion pipeline
/// produces.
fn split_prose(text: &str, target: usize, want: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chars.len() && out.len() < want {
        // Every piece is about `target` long. Text beyond what `want` pieces need
        // is left unused rather than poured into the final piece: a last chunk
        // holding the rest of a long article would pull the source's mean chunk
        // length far above the size it is supposed to reproduce.
        let ideal = (start + target).min(chars.len());
        let mut end = ideal;
        if end < chars.len() {
            // Prefer a sentence end within a fifth of the target, so a chunk does
            // not begin mid sentence. A truncated word would give the lexical index
            // a term that does not exist and the embedder a fragment.
            let window = (target / 5).max(40);
            let low = ideal.saturating_sub(window).max(start + 1);
            let high = (ideal + window).min(chars.len());
            // The boundary nearest the ideal length, rather than the last one found.
            // Taking the last biased every chunk long: the search walks upward and
            // only stops once past the ideal, so it preferred a late boundary and the
            // measured mean came out several per cent above the target.
            let mut found: Option<usize> = None;
            for (i, character) in chars.iter().enumerate().take(high).skip(low) {
                if matches!(character, '.' | '!' | '?' | '\n') {
                    let candidate = i + 1;
                    let better = match found {
                        None => true,
                        Some(current) => {
                            candidate.abs_diff(ideal) < current.abs_diff(ideal)
                        }
                    };
                    if better {
                        found = Some(candidate);
                    }
                }
            }
            end = found.unwrap_or(ideal);
        }
        let piece: String = chars[start..end].iter().collect();
        let piece = piece.trim().to_string();
        if !piece.is_empty() {
            out.push(piece);
        }
        start = end;
    }
    out
}

/// Split code on line boundaries, which is how a code aware chunker behaves and
/// what keeps function names and paths intact.
fn split_code(text: &str, target: usize, want: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut current = String::new();
    for line in lines {
        if current.len() + line.len() + 1 > target && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            if out.len() >= want {
                return out;
            }
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() && out.len() < want {
        out.push(current);
    }
    out
}

/// Split a discussion page into threads at its section headings, which is what
/// makes a message shaped document: one exchange, not a whole page.
fn split_threads(text: &str, headings: &[String]) -> Vec<(String, String)> {
    if headings.is_empty() {
        return vec![(String::new(), text.to_string())];
    }
    // The dump gives the headings but not their offsets, so they are located in
    // the text. A heading that cannot be found is skipped rather than guessed at.
    let mut marks: Vec<(usize, String)> = Vec::new();
    let mut from = 0usize;
    for h in headings {
        if h.trim().is_empty() {
            continue;
        }
        if let Some(at) = text[from..].find(h.as_str()) {
            let abs = from + at;
            marks.push((abs, h.clone()));
            from = abs + h.len();
        }
    }
    if marks.is_empty() {
        return vec![(String::new(), text.to_string())];
    }
    let mut out = Vec::new();
    for (i, (at, heading)) in marks.iter().enumerate() {
        let end = marks.get(i + 1).map(|(n, _)| *n).unwrap_or(text.len());
        let body = text[*at + heading.len()..end].trim();
        if body.len() >= 120 {
            out.push((heading.clone(), body.to_string()));
        }
    }
    if out.is_empty() {
        vec![(String::new(), text.to_string())]
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// Synthetic identity: the people, places and labels around the text.
// ---------------------------------------------------------------------------

/// Name parts combined into author names. These are invented so the corpus
/// carries no real person's name, and there are enough combinations that the
/// largest source's 473 authors are all distinct.
const GIVEN: &[&str] = &[
    "Ada", "Bo", "Cai", "Dara", "Eli", "Fen", "Gita", "Hale", "Ines", "Jo", "Kian", "Lore",
    "Mira", "Nils", "Oona", "Pav", "Quill", "Rune", "Sena", "Tov", "Uma", "Vero", "Wren",
    "Xan", "Yara", "Zev", "Anwen", "Bram", "Cleo", "Dov", "Esme", "Faro", "Gwen",
];
const FAMILY: &[&str] = &[
    "Almeida", "Bergstrom", "Calder", "Dunne", "Eriksen", "Falk", "Grieve", "Halloran",
    "Ibarra", "Jarosz", "Keller", "Lindqvist", "Moreau", "Nakhle", "Ostrand", "Pereira",
    "Quintero", "Rasmussen", "Sandoval", "Thorne", "Ueda", "Vasquez", "Whitlock", "Ximenes",
    "Yoshida", "Zabala", "Aldridge", "Boone", "Cortese", "Delgado",
];

/// Space names per source, used where a source shares a fixed set of spaces.
/// A repository name stands in for a space in the code shaped source, a channel
/// name in the message shaped source and a project key in the ticket shaped one.
const CHANNELS: &[&str] = &[
    "general", "engineering", "platform-team", "release-notes", "incident-response",
    "design-review", "data-eng", "search-quality", "onboarding", "infra", "security",
    "product", "analytics", "mobile", "web", "api-design", "billing", "support",
    "docs", "tooling", "performance", "testing", "hiring", "random",
];
const PROJECTS: &[&str] = &[
    "PLAT", "SRCH", "DATA", "INFRA", "WEB", "MOB", "API", "BILL", "SUP", "DOC", "TOOL",
    "PERF", "SEC", "ANL", "REL", "DES", "ONB", "QA", "OPS", "ML", "CORE", "EXP",
];

/// A deterministic author list of the requested size.
fn authors_for(name: &str, count: usize) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(count);
    let mut seen = std::collections::HashSet::new();
    let mut i = 0usize;
    while out.len() < count {
        let given = GIVEN[i % GIVEN.len()];
        let family = FAMILY[(i / GIVEN.len() + i * 7) % FAMILY.len()];
        let display = format!("{given} {family}");
        if seen.insert(display.clone()) {
            // The identifier is stable and obviously synthetic, which matters
            // because one filter matches on the identifier rather than the name.
            let id = format!("{}-u{:04}", name, out.len() + 1);
            out.push((display, id));
        }
        i += 1;
        if i > count * 40 {
            // Exhausted the combinations; number the remainder.
            let display = format!("{} {}", GIVEN[out.len() % GIVEN.len()], out.len());
            let id = format!("{}-u{:04}", name, out.len() + 1);
            out.push((display, id));
        }
    }
    out
}

/// Label vocabulary. Labels are filtered on by one scenario, so they need a
/// realistic distribution: a few common ones and a long tail.
const LABELS: &[&str] = &[
    "reference", "runbook", "decision-record", "postmortem", "how-to", "architecture",
    "onboarding", "deprecated", "draft", "reviewed", "external", "internal", "roadmap",
    "spike", "migration", "performance", "security", "accessibility", "analytics",
    "experiment", "retired", "template", "faq", "glossary", "policy", "meeting-notes",
];

fn labels_for(rng: &mut StdRng, mean: f64, extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Real label counts cluster at the mean rather than spreading uniformly, so
    // the count is the mean plus or minus one.
    let base = mean.floor() as usize;
    let count = base + usize::from(rng.gen::<f64>() < mean - base as f64);
    for _ in 0..count {
        // A skewed draw, so a handful of labels are common and the rest are rare.
        let i = (rng.gen::<f64>().powf(2.0) * LABELS.len() as f64) as usize;
        let label = LABELS[i.min(LABELS.len() - 1)].to_string();
        if !out.contains(&label) {
            out.push(label);
        }
    }
    for e in extra.iter().take(2) {
        let cleaned: String = e
            .chars()
            .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect();
        let cleaned = cleaned.trim_matches('-').to_string();
        if !cleaned.is_empty() && cleaned.len() <= 40 && !out.contains(&cleaned) {
            out.push(cleaned);
        }
    }
    out
}

/// A ticket key like a real project uses. These are threaded through the prose
/// sources because real documents reference tickets, and because the identifier
/// scenario needs rare literal tokens to search for.
fn ticket_key(rng: &mut StdRng) -> String {
    let project = PROJECTS[rng.gen_range(0..PROJECTS.len())];
    format!("{project}-{}", rng.gen_range(100..9999))
}

/// Sprinkle identifiers into a chunk of prose. The rate is low, so most chunks
/// carry none and the ones that do carry a token appearing in only a few places,
/// which is exactly what the identifier scenario measures.
fn thread_identifiers(rng: &mut StdRng, text: &mut String, source: &str) {
    let roll: f64 = rng.gen();
    if roll < 0.055 {
        text.push_str(&format!(" See {} for the decision.", ticket_key(rng)));
    }
    if roll > 0.965 {
        // A build or release identifier, another shape of rare literal token.
        text.push_str(&format!(
            " Recorded in {}-{}.{}.{} during the review.",
            source,
            rng.gen_range(1..9),
            rng.gen_range(0..40),
            rng.gen_range(0..200)
        ));
    }
}

/// Chunk counts per document that reproduce the measured quantiles.
///
/// A uniform draw is mapped through the four measured points, so half the
/// documents land between the minimum and the median, forty per cent between the
/// median and the ninetieth percentile, and a tenth stretch out to the maximum.
/// The long tail is what makes the per document result cap do any work.
fn chunks_per_doc(rng: &mut StdRng, q: (usize, usize, usize, usize)) -> usize {
    let (lo, med, p90, hi) = q;
    let u: f64 = rng.gen();
    let value = if u < 0.5 {
        lo as f64 + (u / 0.5) * (med - lo) as f64
    } else if u < 0.9 {
        med as f64 + ((u - 0.5) / 0.4) * (p90 - med) as f64
    } else {
        // Squared so the extreme is genuinely rare rather than merely uncommon.
        let t = ((u - 0.9) / 0.1_f64).powf(2.5);
        p90 as f64 + t * (hi - p90) as f64
    };
    (value.round() as usize).clamp(lo, hi)
}

/// Heading depth drawn from the measured distribution for a source.
fn heading_depth(rng: &mut StdRng, shares: &[f64; 4]) -> usize {
    let u: f64 = rng.gen();
    let mut acc = 0.0;
    for (depth, share) in shares.iter().enumerate() {
        acc += share;
        if u < acc {
            return depth;
        }
    }
    // Falls here only when the shares do not quite sum to one.
    shares.len() - 1
}

// ---------------------------------------------------------------------------
// Assembling documents.
// ---------------------------------------------------------------------------

/// A document before it is laid down in ingestion order.
struct Document {
    source: &'static str,
    title: String,
    /// Chunk bodies, with their heading path already decided.
    chunks: Vec<(Vec<String>, String)>,
    space_key: Option<String>,
    author: Option<String>,
    author_id: Option<String>,
    updated_at: Option<i64>,
    labels: Vec<String>,
    url: String,
    deleted: bool,
    phase: Phase,
}

/// Build a heading path of the requested depth for the chunk at `nth` of `total`.
///
/// The leaf has to be the heading of the section the chunk actually falls in, or
/// the natural language ground truth is unanswerable. That ground truth takes a
/// leaf heading as the query and counts the chunks under it as correct, so if the
/// heading is unrelated to the chunk's text, no engine can find it and the scenario
/// measures nothing.
///
/// An earlier version picked headings by `(nth + level * 3) % headings.len()`,
/// which spread them out but attached them to chunks at random. The introduction of
/// the article on amphetamine was filed under `Uses / Binge eating disorder`, and
/// both engines scored close to zero on every heading scenario: 0.14 and 0.09
/// success@10 against 0.93 and 0.53 on a corpus where the headings meant something.
///
/// The dumps give section headings in document order and the article text is in the
/// same order, so a chunk's position in the document maps onto its section. The
/// path is then that section and the headings immediately before it, which is an
/// approximation of a hierarchy: consecutive headings in an article are often a
/// section followed by its subsections. The approximation is in the ancestors. The
/// leaf, which is the part the ground truth queries, is the chunk's own section.
fn heading_path(depth: usize, headings: &[String], nth: usize, total: usize) -> Vec<String> {
    if depth == 0 || headings.is_empty() {
        return Vec::new();
    }
    // Which section this chunk falls in, from how far through the document it is.
    let section = if total <= 1 {
        0
    } else {
        (nth * headings.len() / total).min(headings.len() - 1)
    };
    let first = section + 1 - depth.min(section + 1);
    let mut path = Vec::with_capacity(depth);
    for h in &headings[first..=section] {
        let h = h.trim();
        if h.is_empty() || path.iter().any(|p| p == h) {
            continue;
        }
        path.push(h.to_string());
    }
    if path.is_empty() {
        path.push(headings[section].trim().to_string());
    }
    path
}

/// Choose which documents go in the bulk phase and which in the interleaved tail.
fn phase_of(rng: &mut StdRng, plan: &SourcePlan) -> Phase {
    if rng.gen::<f64>() < plan.bulk_share {
        plan.bulk_phase
    } else {
        Phase::Tail
    }
}

struct Pools {
    /// Articles already allocated to each source. The three article based sources
    /// get disjoint slices, so no article supplies text to two sources.
    articles: HashMap<&'static str, Vec<RawArticle>>,
    talk: Vec<RawTalk>,
    code: Vec<RawCode>,
    issues: Vec<RawIssue>,
}

/// How much the reformatting of an article grows it. A design file repeats a
/// `Text layer:` prefix per label and a board repeats `Note:` per note, so both
/// need less raw text than their chunk budget suggests. Measured by building the
/// corpus and comparing raw characters consumed against chunk characters written.
fn expansion(material: Material) -> f64 {
    match material {
        Material::DesignFiles => 1.9,
        Material::Boards => 1.6,
        _ => 1.0,
    }
}

/// Share the article pool between the three article based sources.
///
/// Walking the pool from the longest article down, each article goes to whichever
/// source currently has the largest shortfall of text per document it still has to
/// fill. A source stops receiving articles once it has one per document.
///
/// Handing each source a contiguous block instead does not work. The design file
/// source needs the most text per document, so it would take the longest articles
/// in the pool, most of that text would go unused because it needs only a thousand
/// documents, and the page shaped source would be left with articles too short to
/// reach its chunk count.
fn allocate_articles(
    articles: Vec<RawArticle>,
    plans: &[SourcePlan],
) -> HashMap<&'static str, Vec<RawArticle>> {
    struct Need {
        name: &'static str,
        documents_left: usize,
        chars_left: f64,
    }

    let mut needs: Vec<Need> = plans
        .iter()
        .filter(|p| matches!(p.material, Material::Articles | Material::DesignFiles | Material::Boards))
        .map(|p| Need {
            name: p.name,
            documents_left: p.documents,
            // Raw characters required, before the reformatting that grows a design
            // file or a board.
            chars_left: (p.chunks as f64 * p.mean_chars as f64) / expansion(p.material),
        })
        .collect();

    let mut sorted = articles;
    sorted.sort_by_key(|a| std::cmp::Reverse(a.text.len()));

    let mut out: HashMap<&'static str, Vec<RawArticle>> = needs
        .iter()
        .map(|n| (n.name, Vec::with_capacity(n.documents_left)))
        .collect();

    for article in sorted {
        // The source whose remaining documents each need the most text.
        let pick = needs
            .iter()
            .enumerate()
            .filter(|(_, n)| n.documents_left > 0)
            .max_by(|(_, a), (_, b)| {
                let ra = a.chars_left / a.documents_left as f64;
                let rb = b.chars_left / b.documents_left as f64;
                ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i);
        let Some(i) = pick else { break };
        needs[i].documents_left -= 1;
        needs[i].chars_left = (needs[i].chars_left - article.text.len() as f64).max(0.0);
        out.get_mut(needs[i].name).expect("allocated above").push(article);
    }
    out
}

/// Documents for one source, consuming from the pools so no raw item is used
/// twice anywhere in the corpus.
#[allow(clippy::too_many_lines)]
fn build_source(plan: &SourcePlan, pools: &mut Pools, rng: &mut StdRng) -> Result<Vec<Document>> {
    let authors = authors_for(plan.name, plan.authors);
    let mut docs: Vec<Document> = Vec::with_capacity(plan.documents);

    // Decide the chunk count of every document first, then scale the counts so
    // they sum to the source's measured chunk total. Drawing and then correcting
    // keeps the measured shape and still hits the measured size.
    let mut counts: Vec<usize> = (0..plan.documents)
        .map(|_| chunks_per_doc(rng, plan.per_doc))
        .collect();
    let drawn: usize = counts.iter().sum();
    if drawn > 0 && plan.chunks > 0 {
        let factor = plan.chunks as f64 / drawn as f64;
        for c in counts.iter_mut() {
            *c = ((*c as f64) * factor).round() as usize;
            *c = (*c).max(plan.per_doc.0);
        }
        // Correct the rounding residue one chunk at a time, on documents that can
        // absorb it without leaving the measured range.
        let mut total: usize = counts.iter().sum();
        let mut guard = 0usize;
        while total != plan.chunks && guard < plan.documents * 60 {
            let i = rng.gen_range(0..counts.len());
            if total < plan.chunks && counts[i] < plan.per_doc.3 {
                counts[i] += 1;
                total += 1;
            } else if total > plan.chunks && counts[i] > plan.per_doc.0 {
                counts[i] -= 1;
                total -= 1;
            }
            guard += 1;
        }
    }

    // Documents needing the most text are filled first, so the longest raw items
    // go where they are needed and nothing is wasted.
    let mut order: Vec<usize> = (0..counts.len()).collect();
    order.sort_by(|a, b| counts[*b].cmp(&counts[*a]));

    for &slot in &order {
        let mut want = counts[slot];
        if want == 0 {
            continue;
        }

        let (title, headings, categories, timestamp, text, space, url) = match plan.material {
            Material::Articles | Material::DesignFiles | Material::Boards => {
                // This source's own slice of the article pool, longest first, paired
                // rank for rank with documents ordered by how much text they need.
                let slice = pools.articles.get_mut(plan.name).expect("every article source is allocated a slice");
                let a = match slice.pop() {
                    Some(a) => a,
                    None => break, // Slice exhausted; the caller reports the shortfall.
                };
                let a = &a;
                // A document cannot hold more chunks than its article has text for.
                // Capping here rather than skipping keeps the document, which is what
                // makes the document count reachable.
                let available = ((a.text.len() as f64) * expansion(plan.material)) as usize;
                want = want.min((available / plan.mean_chars).max(plan.per_doc.0)).max(1);
                let space = match plan.spaces {
                    Spaces::Shared(1) => Some("ENG".to_string()),
                    Spaces::Shared(n) => Some(format!("SPACE{:02}", rng.gen_range(0..n))),
                    Spaces::PerDocument => Some(format!(
                        "{}-{}",
                        plan.name,
                        a.title
                            .chars()
                            .filter(|c| c.is_alphanumeric())
                            .take(24)
                            .collect::<String>()
                            .to_lowercase()
                    )),
                };
                let url = format!(
                    "https://example.invalid/{}/{}",
                    plan.name,
                    a.title.replace(' ', "_")
                );
                (
                    a.title.clone(),
                    a.headings.clone(),
                    a.categories.clone(),
                    a.timestamp.clone(),
                    a.text.clone(),
                    space,
                    url,
                )
            }
            Material::Discussions => {
                let t = match pools.talk.pop() {
                    Some(t) => t,
                    None => break,
                };
                // A thread, not a whole page: one heading and the exchange under it.
                let threads = split_threads(&t.text, &t.headings);
                let (heading, body) = threads
                    .into_iter()
                    .max_by_key(|(_, b)| b.len())
                    .unwrap_or((String::new(), t.text.clone()));
                // The title names the page and the thread, both written by people,
                // which keeps it descriptive and unique.
                let title = if heading.trim().is_empty() {
                    t.title.clone()
                } else {
                    format!("{}: {}", t.title.trim_start_matches("Talk:"), heading.trim())
                };
                let channel = CHANNELS[rng.gen_range(0..CHANNELS.len().min(match plan.spaces {
                    Spaces::Shared(n) => n,
                    Spaces::PerDocument => CHANNELS.len(),
                }))];
                let space = Some(channel.to_string());
                let url = format!("https://example.invalid/{}/{}", plan.name, rng.gen::<u32>());
                (
                    title,
                    if heading.trim().is_empty() { vec![t.title.clone()] } else { vec![heading] },
                    Vec::new(),
                    t.timestamp.clone(),
                    body,
                    space,
                    url,
                )
            }
            Material::Code => {
                let c = match pools.code.pop() {
                    Some(c) => c,
                    None => break,
                };
                // The repository and the path together are the title. The path alone
                // was written by a person, is unique and is full of the compound
                // identifiers the tokenizer has specific handling for, but it holds
                // no spaces, and the document identity ground truth requires two or
                // more words. With the path alone this source contributed no graded
                // queries at all, silently.
                let title = format!("{} {}", c.repo, c.path);
                let dirs: Vec<String> = Path::new(&c.path)
                    .parent()
                    .map(|p| {
                        p.components()
                            .map(|x| x.as_os_str().to_string_lossy().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let url = format!("https://example.invalid/{}/{}/{}", plan.name, c.repo, c.path);
                (title, dirs, vec![c.repo.clone()], None, c.text.clone(), Some(c.repo.clone()), url)
            }
            Material::Issues => {
                let i = match pools.issues.pop() {
                    Some(i) => i,
                    None => break,
                };
                let project = PROJECTS[rng.gen_range(0..PROJECTS.len().min(match plan.spaces {
                    Spaces::Shared(n) => n,
                    Spaces::PerDocument => PROJECTS.len(),
                }))];
                // A ticket key of its own, so the ticket shaped source reads like a
                // tracker and its keys are searchable literals.
                let key = format!("{project}-{}", 1000 + (i.number % 9000));
                let title = format!("{key} {}", i.title);
                let url = format!("https://example.invalid/{}/{}", plan.name, key);
                // The repository stands in for the component a real tracker records.
                let mut categories = vec![i.repo.clone()];
                categories.extend(i.labels.iter().cloned());
                (
                    title,
                    vec!["Description".to_string(), "Steps to reproduce".to_string(), "Acceptance".to_string()],
                    categories,
                    i.updated_at.clone(),
                    i.body.clone(),
                    Some(project.to_string()),
                    url,
                )
            }
        };

        // Reformat the raw text into the register the source is standing in for.
        let text = match plan.material {
            Material::DesignFiles => as_design_file(&title, &headings, &text),
            Material::Boards => as_board(&title, &text),
            _ => text,
        };

        // The breadcrumb is part of the chunk, so the body gets what is left of the
        // target after it. A floor keeps a long title from collapsing the body.
        let body_target = plan
            .mean_chars
            .saturating_sub(breadcrumb_estimate(&title, &headings, &plan.heading_depth))
            .max(plan.mean_chars / 3);
        let bodies = match plan.material {
            Material::Code => split_code(&text, body_target, want),
            _ => split_prose(&text, body_target, want),
        };
        if bodies.is_empty() {
            continue;
        }

        // Needed so a chunk's position in the document can be mapped onto the
        // section it falls in.
        let chunk_count = bodies.len();
        let mut chunks = Vec::with_capacity(chunk_count);
        for (nth, mut body) in bodies.into_iter().enumerate() {
            if plan.material != Material::Code {
                thread_identifiers(rng, &mut body, plan.name);
            }
            let depth = heading_depth(rng, &plan.heading_depth);
            let path = heading_path(depth, &headings, nth, chunk_count);
            chunks.push((path.clone(), breadcrumbed(&title, &path, &body)));
        }

        let (author, author_id) = if authors.is_empty() {
            (None, None)
        } else {
            let (name, id) = &authors[rng.gen_range(0..authors.len())];
            (Some(name.clone()), Some(id.clone()))
        };

        // Spread over the same span the original corpus covered, so the
        // `updated_after` filter selects a comparable share.
        let updated_at = timestamp
            .as_deref()
            .and_then(epoch_seconds)
            .or(Some(1_515_628_800 + rng.gen_range(0..274_000_000)));

        docs.push(Document {
            source: plan.name,
            title,
            chunks,
            space_key: space,
            author,
            author_id,
            updated_at,
            labels: labels_for(rng, plan.mean_labels, &categories),
            url,
            // A very small share of documents are soft deleted, as in the original,
            // because every filter has to exclude them and a corpus with none would
            // never catch a query that forgets to.
            deleted: rng.gen::<f64>() < 0.0016,
            phase: phase_of(rng, plan),
        });
    }

    Ok(docs)
}

/// An article rewritten as the text layers of a design file. A design file has no
/// prose: it has a frame tree and short labels, so the text is broken into layer
/// names under frame headings. This is the one source with no public equivalent,
/// so its register is constructed rather than borrowed.
fn as_design_file(title: &str, headings: &[String], text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 512);
    out.push_str(&format!("Design file: {title}\n"));
    let mut frame = 0usize;
    for (i, sentence) in text.split_terminator(['.', '!', '?']).enumerate() {
        let s = sentence.trim();
        if s.is_empty() {
            continue;
        }
        if i % 6 == 0 {
            let name = headings.get(frame % headings.len().max(1)).map(String::as_str).unwrap_or("Frame");
            out.push_str(&format!("\nFrame {}: {}\n", frame + 1, name));
            frame += 1;
        }
        // Layer names are short, so a long sentence becomes several labels.
        for part in s.split(", ") {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            out.push_str(&format!("  Text layer: {p}\n"));
        }
    }
    out
}

/// An article rewritten as the notes on a board: clusters of short notes with a
/// heading per cluster, which is what a board's exported text looks like.
fn as_board(title: &str, text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 512);
    out.push_str(&format!("Board: {title}\n"));
    let mut cluster = 0usize;
    for (i, sentence) in text.split_terminator(['.', '!', '?']).enumerate() {
        let s = sentence.trim();
        if s.is_empty() {
            continue;
        }
        if i % 5 == 0 {
            cluster += 1;
            out.push_str(&format!("\nCluster {cluster}\n"));
        }
        out.push_str(&format!("  Note: {s}\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// The corpus, in ingestion order.
// ---------------------------------------------------------------------------

/// One chunk as written to the intermediate file, before embedding. This is the
/// corpus in text form: it is what gets embedded, and what gets loaded into
/// PostgreSQL, so both engines are guaranteed the same content.
#[derive(Debug, Serialize, Deserialize)]
pub struct SynthChunk {
    /// The document's identifier, which becomes the PostgreSQL primary key, so
    /// the key both engines report is identical.
    pub doc_id: i64,
    pub source: String,
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

impl SynthChunk {
    pub fn to_input(&self) -> ChunkInput {
        ChunkInput {
            source: self.source.clone(),
            external_doc_id: self.doc_id.to_string(),
            chunk_index: self.chunk_index,
            heading_path: self.heading_path.clone(),
            content: self.content.clone(),
            title: self.title.clone(),
            url: self.url.clone(),
            space_key: self.space_key.clone(),
            author: self.author.clone(),
            author_id: self.author_id.clone(),
            updated_at: self.updated_at,
            external_chunk_id: None,
            labels: self.labels.clone(),
            attributes: Vec::new(),
            flags: Vec::new(),
            deleted: self.deleted,
        }
    }
}

pub struct BuildReport {
    pub chunks: usize,
    pub documents: usize,
    pub per_source: Vec<(String, usize, usize, usize)>,
    pub unique_titles: usize,
    pub identity_usable: usize,
}

/// Build the corpus and write it as JSONL.
pub fn build(derived: &Path, out: &Path, scale: f64) -> Result<BuildReport> {
    let mut rng = StdRng::seed_from_u64(SEED);

    eprintln!("reading the public material from {}", derived.display());
    // Two article pools. The Simple English dump is small and its articles are
    // short; the English dump supplies the long articles the page shaped and design
    // file shaped sources need, and a much larger vocabulary. Both are used when
    // both are present, so the pipeline works with only the small one available.
    let mut articles: Vec<RawArticle> = Vec::new();
    for name in ["enwiki-articles.jsonl", "articles.jsonl"] {
        let path = derived.join(name);
        if path.exists() {
            let mut pool: Vec<RawArticle> = read_jsonl(&path)?;
            eprintln!("  {} supplied {} articles", name, pool.len());
            articles.append(&mut pool);
        }
    }
    anyhow::ensure!(
        !articles.is_empty(),
        "no articles found in {}. Run scripts/extract-wikipedia.py first",
        derived.display()
    );
    let talk: Vec<RawTalk> = read_jsonl(&derived.join("talk.jsonl"))?;
    let code: Vec<RawCode> = read_jsonl(&derived.join("code.jsonl"))?;
    let issues: Vec<RawIssue> = read_jsonl(&derived.join("issues.jsonl"))?;
    eprintln!(
        "  {} articles in total, {} discussions, {} source files, {} issues",
        articles.len(),
        talk.len(),
        code.len(),
        issues.len()
    );

    // A title has to be usable as a query for the document identity ground truth:
    // long enough to be specific and at least two words. Articles whose titles
    // qualify are put first, so the sources that draw from the front of the pool
    // get titles that can be graded. The rest stay in the pool, because a corpus
    // in which every title is a usable query would be an easier corpus than the
    // real one.
    articles.sort_by_key(|a| {
        let t = a.title.trim();
        let words = t.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).count();
        let usable = t.chars().count() >= 12 && t.chars().count() <= 160 && words >= 2;
        // Longest usable titles first, so the largest documents also get gradeable
        // titles rather than the pool's leftovers.
        (!usable, std::cmp::Reverse(a.text.len()))
    });

    let plan_list = plans(scale);
    // Each article based source gets its own slice. They are stored ascending by
    // length because documents are filled from the back, largest need first.
    let mut allocated = allocate_articles(articles, &plan_list);
    for slice in allocated.values_mut() {
        slice.reverse();
    }
    let mut pools = Pools { articles: allocated, talk, code, issues };
    // Popped from the back, so the order is made deliberate rather than incidental.
    pools.talk.sort_by_key(|t| t.text.len());
    pools.code.sort_by_key(|c| c.text.len());
    pools.issues.sort_by_key(|i| i.body.len());

    let mut all: Vec<Document> = Vec::new();
    for plan in &plan_list {
        let docs = build_source(plan, &mut pools, &mut rng)?;
        let chunks: usize = docs.iter().map(|d| d.chunks.len()).sum();
        eprintln!(
            "  {:<11} {:>6} documents {:>7} chunks (target {} / {})",
            plan.name,
            docs.len(),
            chunks,
            plan.documents,
            plan.chunks
        );
        if docs.len() < plan.documents {
            eprintln!(
                "    warning: {} short of the target, the source pool ran out",
                plan.documents - docs.len()
            );
        }
        all.extend(docs);
    }

    // Ingestion order. Documents are grouped by phase, and inside the tail the
    // sources are interleaved, which is what makes a prefix of the corpus
    // unrepresentative of the whole and `strided_sample` necessary.
    let mut bulk: Vec<Document> = Vec::new();
    let mut tail: Vec<Document> = Vec::new();
    for d in all {
        if d.phase == Phase::Tail {
            tail.push(d);
        } else {
            bulk.push(d);
        }
    }
    bulk.sort_by_key(|d| d.phase);
    tail.shuffle(&mut rng);
    let ordered: Vec<Document> = bulk.into_iter().chain(tail).collect();

    // Write, assigning document identifiers in the order they are laid down so the
    // identifier ordering correlates with source exactly as it did originally.
    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut w = BufWriter::new(file);
    let mut per_source: HashMap<&str, (usize, usize, usize)> = HashMap::new();
    let mut titles: HashMap<String, usize> = HashMap::new();
    let mut total_chunks = 0usize;

    for (i, d) in ordered.iter().enumerate() {
        let doc_id = (i + 1) as i64;
        *titles.entry(d.title.clone()).or_insert(0) += 1;
        let entry = per_source.entry(d.source).or_insert((0, 0, 0));
        entry.0 += 1;
        entry.1 += d.chunks.len();
        for (index, (path, body)) in d.chunks.iter().enumerate() {
            entry.2 += body.chars().count();
            let record = SynthChunk {
                doc_id,
                source: d.source.to_string(),
                chunk_index: index as u32,
                heading_path: path.clone(),
                content: body.clone(),
                title: d.title.clone(),
                url: d.url.clone(),
                space_key: d.space_key.clone(),
                author: d.author.clone(),
                author_id: d.author_id.clone(),
                updated_at: d.updated_at,
                labels: d.labels.clone(),
                deleted: d.deleted,
            };
            serde_json::to_writer(&mut w, &record)?;
            w.write_all(b"\n")?;
            total_chunks += 1;
        }
    }
    w.flush()?;

    let unique_titles = titles.values().filter(|n| **n == 1).count();
    let identity_usable = titles
        .iter()
        .filter(|(t, n)| {
            **n == 1
                && t.chars().count() >= 12
                && t.chars().count() <= 160
                && t.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).count() >= 2
        })
        .count();

    let mut rows: Vec<(String, usize, usize, usize)> = per_source
        .into_iter()
        .map(|(s, (docs, chunks, chars))| {
            (s.to_string(), docs, chunks, chars.checked_div(chunks).unwrap_or(0))
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.2));

    Ok(BuildReport {
        chunks: total_chunks,
        documents: ordered.len(),
        per_source: rows,
        unique_titles,
        identity_usable,
    })
}

/// Read the corpus back, in file order, with every text field cleaned.
///
/// Cleaning happens here rather than at each caller because this is the one place
/// all three consumers pass through: the embedding run, the cache assembly and the
/// PostgreSQL load. Applying it anywhere else would let them disagree.
///
/// Two reasons it has to happen at all. PostgreSQL rejects a NUL byte in a `text`
/// column outright, so a single one anywhere in the corpus fails the load: five
/// chunks drawn from source files carried one. And the embedding run strips control
/// characters before handing text to the tokenizer, so leaving them in the stored
/// text would mean the vectors describe slightly different text than the engines
/// index. Cleaning once, here, keeps the vectors, the cache and the database
/// describing the same bytes.
pub fn read_corpus(path: &Path) -> Result<Vec<SynthChunk>> {
    let mut chunks: Vec<SynthChunk> = read_jsonl(path)?;
    for c in chunks.iter_mut() {
        c.content = sanitize_for_model(&c.content);
        c.title = sanitize_for_model(&c.title);
        c.url = sanitize_for_model(&c.url);
        for h in c.heading_path.iter_mut() {
            *h = sanitize_for_model(h);
        }
        for l in c.labels.iter_mut() {
            *l = sanitize_for_model(l);
        }
        if let Some(v) = c.space_key.as_mut() {
            *v = sanitize_for_model(v);
        }
        if let Some(v) = c.author.as_mut() {
            *v = sanitize_for_model(v);
        }
    }
    Ok(chunks)
}

/// Where the corpus material lives by default. Kept out of the repository because
/// it is large and rebuildable, and because the licences of the public sources are
/// satisfied by attribution rather than by redistribution.
pub fn default_derived_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/inillucent-corpus/derived")
}

// ---------------------------------------------------------------------------
// Embedding the corpus.
// ---------------------------------------------------------------------------

/// Embeds every chunk of the corpus in corpus order and writes the cache both
/// engines read.
///
/// Resumable: the vector file is append only and holds fixed width records, so the
/// count of whole records already in it is the count of chunks already done.
///
/// `devices` names the processors the work is spread across. One device is the
/// original arrangement. Several run one session each, in lockstep over a window
/// of the corpus, so the file is still written in corpus order: chunk N is record
/// N whatever ran it.
/// @param corpus_path - the corpus JSONL produced by synth-build
/// @param cache_path - the cache written at the end, which the harness loads
/// @param model - the resolved model, with the manifest that says what it is
/// @param seeds - the harness query seed table, digested into the header
/// @param options - the machine settings, including the llama.cpp endpoint
/// @param report_every - chunks between progress lines
/// @param devices - the processors to spread an ONNX arm across
/// @param window_batches - batches handed to each device between synchronisations
pub fn embed(
    corpus_path: &Path,
    cache_path: &Path,
    model: &crate::models::ResolvedModel,
    seeds: &std::collections::BTreeMap<String, u64>,
    options: &ArmOptions,
    report_every: usize,
    devices: &[Device],
    window_batches: usize,
) -> Result<()> {
    let manifest = &model.manifest;
    let model_dir = model.dir.display().to_string();
    let model_dir = model_dir.as_str();
    let chunks = read_corpus(corpus_path)?;
    eprintln!("corpus holds {} chunks", chunks.len());
    eprintln!(
        "model: {} at {} dimensions, {:?} pooling, {} token bound, {:?} backend, document \
         prefix {:?}",
        manifest.id,
        manifest.dims,
        manifest.pooling,
        manifest.max_tokens,
        manifest.backend,
        manifest.prefixes.document
    );

    let vectors_path = cache_path.with_extension("vectors");
    let dims = manifest.dims;
    let bytes_per = dims * 4;

    // How many vectors already exist, rounded down so a partly written vector from
    // an interrupted run is redone rather than trusted.
    let done = match std::fs::metadata(&vectors_path) {
        Ok(m) => (m.len() as usize) / bytes_per,
        Err(_) => 0,
    };
    if done > 0 {
        eprintln!("resuming: {done} vectors already written to {}", vectors_path.display());
        // Discard any partial trailing vector.
        let file = std::fs::OpenOptions::new().write(true).open(&vectors_path)?;
        file.set_len((done * bytes_per) as u64)?;
    }
    if done >= chunks.len() {
        eprintln!("every chunk is already embedded");
        // A run that embedded nothing cannot report how much was truncated, so it
        // counts with the tokenizer alone rather than writing a zero that would
        // read as "nothing was cut".
        let truncated = count_truncated(model_dir, manifest, &chunks)?;
        return assemble_cache(&chunks, &vectors_path, cache_path, manifest, seeds, truncated);
    }

    let embedders = open_arms(model, options, devices)?;

    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&vectors_path)
        .with_context(|| format!("opening {}", vectors_path.display()))?;

    let start = std::time::Instant::now();
    let mut position = done;
    let mut since_report = 0usize;
    // A window is `window_batches` batches per device. It has to be several rather
    // than one: the devices are synchronised at the end of every window, so a window
    // of one batch each pays that synchronisation on every 32 chunks and the faster
    // card spends its time waiting. Measured on two 5090s at batch 32: one batch per
    // device ran 613/sec against 450 for a single card, 1.36x rather than 2x.
    let window = options.batch_size * embedders.len() * window_batches.max(1);
    while position < chunks.len() {
        let end = (position + window).min(chunks.len());
        let texts: Vec<String> = chunks[position..end]
            .iter()
            .map(|c| sanitize_for_model(&c.content))
            .collect();
        let vectors = embed_window(&embedders, &texts)
            .with_context(|| format!("embedding chunks {position} to {end}"))?;
        anyhow::ensure!(
            vectors.len() == texts.len(),
            "the embedder returned {} vectors for {} texts",
            vectors.len(),
            texts.len()
        );
        // Appended in corpus order, so position N in this file is chunk N. The
        // resume above depends on that, and so does every vector belonging to the
        // right chunk.
        for v in &vectors {
            anyhow::ensure!(v.len() == dims, "the embedder returned {} dimensions", v.len());
            for x in v {
                out.write_all(&x.to_le_bytes())?;
            }
        }
        position = end;
        since_report += texts.len();

        if since_report >= report_every || position == chunks.len() {
            // Flushed before reporting, so the progress printed is progress that
            // would survive an interruption.
            out.flush()?;
            let elapsed = start.elapsed().as_secs_f64();
            let rate = (position - done) as f64 / elapsed.max(0.001);
            let left = (chunks.len() - position) as f64 / rate.max(0.001);
            eprintln!(
                "  {}/{} chunks ({:.1}%) at {:.1}/sec, {:.0} min remaining",
                position,
                chunks.len(),
                100.0 * position as f64 / chunks.len() as f64,
                rate,
                left / 60.0
            );
            since_report = 0;
        }
    }
    out.flush()?;
    drop(out);

    // Summed across every session, plus a recount of whatever an earlier run had
    // already embedded, so the share the card prints describes the whole corpus
    // rather than the tail this process happened to reach.
    let texts: usize = embedders.iter().map(|e| e.truncation().texts).sum();
    let tokens: usize = embedders.iter().map(|e| e.truncation().tokens).sum();
    let mut truncated: usize = embedders.iter().map(|e| e.truncation().truncated).sum();
    if texts > 0 {
        eprintln!(
            "  {tokens} tokens over {texts} chunks ({:.1} per chunk); {truncated} ({:.2}%) hit the {} token bound",
            tokens as f64 / texts as f64,
            100.0 * truncated as f64 / texts as f64,
            manifest.max_tokens
        );
    }
    if done > 0 {
        truncated += count_truncated(model_dir, manifest, &chunks[..done])?;
    }

    assemble_cache(&chunks, &vectors_path, cache_path, manifest, seeds, truncated)
}

/// How many of these chunks a model tokenizer takes past its truncation bound,
/// counted without running the model.
///
/// Needed on a resumed run: the sessions this process opened only saw the chunks
/// this process embedded, and a truncation share that silently described the tail
/// of the corpus would be exactly the kind of number that reads as a measurement
/// and is not one.
/// @param model_dir - the model directory
/// @param manifest - what the model is
/// @param chunks - the chunks to count over
fn count_truncated(
    model_dir: &str,
    manifest: &ModelManifest,
    chunks: &[SynthChunk],
) -> Result<usize> {
    if manifest.backend == Backend::LlamaCpp {
        // A served arm has no local tokenizer to count with, so a resumed run
        // cannot recount what an earlier run truncated. Reporting a zero would
        // be worse than reporting nothing, so this says so and the header
        // records what this process actually saw.
        eprintln!(
            "  note: {} is served rather than loaded, so a resumed run cannot recount the \
             truncation an earlier run performed",
            manifest.id
        );
        return Ok(0);
    }
    let texts: Vec<String> =
        chunks.iter().map(|c| sanitize_for_model(&c.content)).collect();
    Ok(count_truncation(model_dir, manifest, &texts)?.truncated)
}

/// Opens one embedder per device.
///
/// On the processor this is deliberately one session and one batch at a time,
/// which is the fastest arrangement measured on this model. Two things that look
/// like they should help do not:
///
/// Raising ONNX Runtime's intra operator thread count makes it slower, not faster:
/// 11.3 texts a second at the default, 9.6 at six threads, 8.0 at eighteen on an
/// eighteen core machine.
///
/// Running several CPU sessions at once is worse still. Four sessions embedding
/// different slices in parallel ran at 2.0 texts a second against 7.4 for one
/// session, while using *less* total CPU, 299% against 490%. Less CPU at lower
/// throughput means the sessions are contending for something other than
/// arithmetic: each one streams its own 550 MB copy of the weights, and the machine
/// runs out of memory bandwidth long before it runs out of cores.
///
/// A GPU is the opposite case, and that reasoning does not carry over: each card
/// holds its own copy of the weights in its own memory and shares no bandwidth with
/// the other, so two cards really are twice the throughput.
/// @param model - the resolved model
/// @param options - the machine settings
/// @param devices - the processors to open an ONNX session on
fn open_arms(
    model: &crate::models::ResolvedModel,
    options: &ArmOptions,
    devices: &[Device],
) -> Result<Vec<Arm>> {
    // A served arm is one server however many cards are named: the devices are
    // the server's business, and opening four clients to one process would
    // measure the queue rather than the model.
    if model.manifest.backend == Backend::LlamaCpp {
        let load = std::time::Instant::now();
        let arm = Arm::open(model, options)?;
        eprintln!(
            "  {} ready in {:.1}s",
            arm.backend_label(options),
            load.elapsed().as_secs_f64()
        );
        return Ok(vec![arm]);
    }
    anyhow::ensure!(!devices.is_empty(), "no devices to embed on");
    let mut embedders = Vec::with_capacity(devices.len());
    for device in devices {
        let load = std::time::Instant::now();
        let arm = Arm::open(model, &ArmOptions { device: *device, ..options.clone() })
            .with_context(|| {
                format!("opening the ONNX embedder on {}. Is ORT_DYLIB_PATH set?", device.label())
            })?;
        eprintln!("  session ready on {} in {:.1}s", device.label(), load.elapsed().as_secs_f64());
        embedders.push(arm);
    }
    Ok(embedders)
}

/// Embeds one window of texts, spread across the embedders and reassembled in the
/// caller's order.
///
/// The split is by total text length rather than by count, because a window can hold
/// a 6 character chunk beside a 6227 character one and an even split of *chunks* is
/// not an even split of *work*. Longest first into whichever device is least loaded
/// is the classic greedy bound, and it also absorbs a genuinely slower card: the one
/// that finishes its share first is simply given more of the next window.
///
/// Scoped threads rather than rayon: each embedder owns a session that must not be
/// shared, so this is one slice per embedder rather than a work queue. With one
/// embedder it is a direct call and spawns nothing.
/// @param embedders - one per device
/// @param texts - the window, already sanitized, in corpus order
fn embed_window(embedders: &[Arm], texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if embedders.len() == 1 {
        return embedders[0].embed_documents(texts);
    }
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(texts[i].len()));
    let mut assigned: Vec<Vec<usize>> = vec![Vec::new(); embedders.len()];
    let mut load: Vec<usize> = vec![0; embedders.len()];
    for i in order {
        let lightest = load
            .iter()
            .enumerate()
            .min_by_key(|(_, bytes)| **bytes)
            .map(|(d, _)| d)
            .unwrap_or(0);
        load[lightest] += texts[i].len();
        assigned[lightest].push(i);
    }
    // Back into corpus order within each device, so the length sorting the embedder
    // does for batching starts from the same arrangement a single device would see.
    for slice in assigned.iter_mut() {
        slice.sort_unstable();
    }

    let batches: Vec<Vec<String>> = assigned
        .iter()
        .map(|ids| ids.iter().map(|&i| texts[i].clone()).collect())
        .collect();
    let results: Vec<Result<Vec<Vec<f32>>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = batches
            .iter()
            .zip(embedders.iter())
            .map(|(batch, embedder)| scope.spawn(move || embedder.embed_documents(batch)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("an embedding thread panicked"))))
            .collect()
    });

    let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
    for (ids, result) in assigned.iter().zip(results) {
        let vectors = result?;
        anyhow::ensure!(vectors.len() == ids.len(), "a device returned {} vectors for {} texts", vectors.len(), ids.len());
        for (&i, v) in ids.iter().zip(vectors) {
            out[i] = v;
        }
    }
    Ok(out)
}


/// Turn the corpus text and the vector file into the cache the harness loads,
/// with the header that says which corpus and which model made it.
/// @param chunks - the corpus, in corpus order
/// @param vectors_path - the append-only vector file the embedding run wrote
/// @param cache_path - where the cache goes
/// @param manifest - the model whose vectors these are
/// @param seeds - the harness query seed table, digested into the header
/// @param truncated - how many chunks hit the model token bound
pub fn assemble_cache(
    chunks: &[SynthChunk],
    vectors_path: &Path,
    cache_path: &Path,
    manifest: &ModelManifest,
    seeds: &std::collections::BTreeMap<String, u64>,
    truncated: usize,
) -> Result<()> {
    use std::io::Read;

    let dims = manifest.dims;
    let bytes_per = dims * 4;
    let size = std::fs::metadata(vectors_path)?.len() as usize;
    let have = size / bytes_per;
    anyhow::ensure!(
        have >= chunks.len(),
        "only {have} of {} chunks are embedded; rerun the embed step to finish",
        chunks.len()
    );

    eprintln!("assembling {} into the cache", cache_path.display());
    let mut reader = BufReader::new(File::open(vectors_path)?);
    let mut vectors = Vec::with_capacity(chunks.len());
    let mut buf = vec![0u8; bytes_per];
    for _ in 0..chunks.len() {
        reader.read_exact(&mut buf)?;
        let v: Vec<f32> = buf
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        vectors.push(v);
    }

    let inputs: Vec<ChunkInput> = chunks.iter().map(SynthChunk::to_input).collect();
    let header = crate::corpus::CacheHeader {
        version: 4,
        corpus_sha256: crate::corpus::corpus_digest(&inputs),
        model_id: manifest.id.clone(),
        manifest_sha256: crate::corpus::manifest_digest(manifest),
        dims,
        max_tokens: manifest.max_tokens,
        chunk_count: inputs.len(),
        truncated_chunks: truncated,
        query_seed_digest: crate::corpus::seed_digest(seeds),
    };
    let corpus = crate::corpus::Corpus { chunks: inputs, vectors, dims, header };
    crate::corpus::save_cache(&corpus, cache_path)?;
    let bytes = std::fs::metadata(cache_path)?.len();
    eprintln!(
        "wrote {} ({:.1} MB) holding {} chunks",
        cache_path.display(),
        bytes as f64 / 1e6,
        corpus.chunks.len()
    );
    eprintln!("  header: {}", corpus.header.describe());
    Ok(())
}

/// Drop control characters, keeping the three whitespace characters that carry
/// meaning. Downloaded text carries the occasional control character, and a NUL in
/// particular cannot be stored in a PostgreSQL `text` column at all.
///
/// Applying this twice gives the same result as applying it once, which matters
/// because `read_corpus` cleans the corpus and the embedding step cleans each text
/// again before tokenizing.
pub fn sanitize_for_model(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == ' ' || *c == '\n' || *c == '\t')
        .collect()
}

// ---------------------------------------------------------------------------
// Loading the corpus into PostgreSQL for the baseline.
// ---------------------------------------------------------------------------

/// The schema the pgvector baseline queries. It is the schema the original stack
/// used, reproduced here so the baseline SQL in `engine.rs` runs unchanged: the
/// same two tables, the same columns it joins and filters on, the same HNSW index
/// with the same parameters, and the same English full text index.
const SCHEMA: &str = "
    DROP TABLE IF EXISTS chunks;
    DROP TABLE IF EXISTS documents;

    CREATE TABLE documents (
      id           bigint PRIMARY KEY,
      source       text NOT NULL,
      source_id    text NOT NULL,
      space_key    text,
      title        text NOT NULL,
      url          text NOT NULL,
      author       text,
      author_id    text,
      created_at   timestamptz,
      updated_at   timestamptz,
      labels       text[] NOT NULL DEFAULT '{}',
      content_hash text NOT NULL,
      deleted_at   timestamptz,
      synced_at    timestamptz NOT NULL DEFAULT now(),
      UNIQUE (source, source_id)
    );

    CREATE TABLE chunks (
      id              bigserial PRIMARY KEY,
      document_id     bigint NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
      chunk_index     integer NOT NULL,
      heading_path    text[] NOT NULL DEFAULT '{}',
      content         text NOT NULL,
      token_count     integer,
      embedding       vector(768),
      embedding_model text,
      UNIQUE (document_id, chunk_index)
    );
";

/// The indexes, created after the rows are inserted because building an HNSW index
/// once over a full table is far quicker than maintaining it per insert.
const INDEXES: &[(&str, &str)] = &[
    ("documents_source_idx", "CREATE INDEX documents_source_idx ON documents (source)"),
    ("documents_deleted_at_idx", "CREATE INDEX documents_deleted_at_idx ON documents (deleted_at)"),
    ("documents_space_key_idx", "CREATE INDEX documents_space_key_idx ON documents (space_key)"),
    ("documents_author_id_idx", "CREATE INDEX documents_author_id_idx ON documents (author_id)"),
    ("documents_updated_at_idx", "CREATE INDEX documents_updated_at_idx ON documents (updated_at DESC)"),
    ("documents_labels_gin_idx", "CREATE INDEX documents_labels_gin_idx ON documents USING gin (labels)"),
    ("chunks_document_id_idx", "CREATE INDEX chunks_document_id_idx ON chunks (document_id)"),
    (
        "chunks_content_fts",
        "CREATE INDEX chunks_content_fts ON chunks USING gin (to_tsvector('english', content))",
    ),
    (
        "chunks_embedding_hnsw",
        "CREATE INDEX chunks_embedding_hnsw ON chunks USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64)",
    ),
];

/// Makes sure the connected database can store and index a vector, whichever way
/// pgvector was installed into it.
///
/// `CREATE EXTENSION vector` is the normal path and is tried first. It fails on a
/// cluster where pgvector was installed by running its SQL with absolute paths to
/// the shared library, which is what a machine does when the PostgreSQL install
/// directory is not writable — the types, operators and both access methods are
/// all there, but no `vector.control` is on the extension path, so the extension
/// does not exist by name. Refusing to run there would be refusing over a name.
/// So the failure is only fatal when the type really is absent.
/// @param client - a connection to the database being loaded
fn ensure_pgvector(client: &mut postgres::Client) -> Result<()> {
    if client.batch_execute("CREATE EXTENSION IF NOT EXISTS vector").is_ok() {
        return Ok(());
    }
    let row = client
        .query_one("SELECT to_regtype('vector') IS NOT NULL", &[])
        .context("checking whether the vector type exists")?;
    let present: bool = row.get(0);
    anyhow::ensure!(
        present,
        "this database has no pgvector: CREATE EXTENSION vector failed and there is no vector type. \
         Install the extension, or run pgvector's SQL with absolute paths to vector.dll/vector.so."
    );
    Ok(())
}

/// Insert the corpus and its vectors, then build the indexes.
///
/// The vectors written here are the same bytes the cache holds, so the two engines
/// are compared on identical input. Nothing is recomputed.
pub fn load_postgres(
    url: &str,
    chunks: &[SynthChunk],
    corpus: &crate::corpus::Corpus,
    build_indexes: bool,
) -> Result<()> {
    use pgvector::Vector;
    use postgres::{Client, NoTls};

    anyhow::ensure!(
        chunks.len() == corpus.chunks.len(),
        "the corpus file holds {} chunks and the cache holds {}; rebuild the cache",
        chunks.len(),
        corpus.chunks.len()
    );

    let mut client = Client::connect(url, NoTls).with_context(|| {
        format!("connecting to {url}. Create the database first: createdb inillucent_synth")
    })?;

    eprintln!("creating the schema");
    ensure_pgvector(&mut client)?;
    client.batch_execute(SCHEMA).context("creating the schema")?;

    // One row per document, taken from the first chunk that mentions it.
    eprintln!("inserting documents");
    let mut seen: HashMap<i64, ()> = HashMap::new();
    let mut documents = 0usize;
    {
        let mut tx = client.transaction()?;
        let statement = tx.prepare(
            "INSERT INTO documents
               (id, source, source_id, space_key, title, url, author, author_id,
                created_at, updated_at, labels, content_hash, deleted_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
        )?;
        for c in chunks {
            if seen.insert(c.doc_id, ()).is_some() {
                continue;
            }
            let updated = c.updated_at.map(|s| {
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(s.max(0) as u64)
            });
            // A soft deleted document carries a deletion time, which is what every
            // filter excludes on.
            let deleted_at = if c.deleted {
                updated.or(Some(std::time::SystemTime::now()))
            } else {
                None
            };
            tx.execute(
                &statement,
                &[
                    &c.doc_id,
                    &c.source,
                    &format!("{}-{}", c.source, c.doc_id),
                    &c.space_key,
                    &c.title,
                    &c.url,
                    &c.author,
                    &c.author_id,
                    &updated,
                    &updated,
                    &c.labels,
                    &format!("{:016x}", c.doc_id),
                    &deleted_at,
                ],
            )?;
            documents += 1;
            if documents.is_multiple_of(5_000) {
                eprintln!("  {documents} documents");
            }
        }
        tx.commit()?;
    }
    eprintln!("  {documents} documents inserted");

    eprintln!("inserting chunks and their vectors");
    {
        let mut tx = client.transaction()?;
        let statement = tx.prepare(
            "INSERT INTO chunks
               (document_id, chunk_index, heading_path, content, token_count, embedding, embedding_model)
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )?;
        let model = "nomic-embed-text-v1.5";
        for (i, c) in chunks.iter().enumerate() {
            let vector = Vector::from(corpus.vectors[i].clone());
            // A rough token count, which the original column also held; nothing
            // queries it, but leaving it null would misrepresent the schema.
            let tokens = (c.content.len() / 4) as i32;
            tx.execute(
                &statement,
                &[
                    &c.doc_id,
                    &(c.chunk_index as i32),
                    &c.heading_path,
                    &c.content,
                    &tokens,
                    &vector,
                    &model,
                ],
            )?;
            if (i + 1) % 20_000 == 0 {
                eprintln!("  {}/{} chunks", i + 1, chunks.len());
            }
        }
        tx.commit()?;
    }
    eprintln!("  {} chunks inserted", chunks.len());

    if build_indexes {
        for (name, sql) in INDEXES {
            let start = std::time::Instant::now();
            eprintln!("building {name}");
            client.batch_execute(sql).with_context(|| format!("building {name}"))?;
            eprintln!("  {name} in {:.1}s", start.elapsed().as_secs_f64());
        }
        client.batch_execute("ANALYZE documents; ANALYZE chunks;")?;
    } else {
        eprintln!("skipping indexes as asked; the baseline needs them before grading");
    }

    Ok(())
}


// ---------------------------------------------------------------------------
// Checking the corpus before paying for the embedding run.
// ---------------------------------------------------------------------------

/// Report whether the corpus supports the three ground truths, and whether it has
/// the structural properties the scenarios rely on.
///
/// This exists because embedding the corpus takes hours. A corpus that cannot
/// supply identifier queries, or whose chunk order does not correlate with source,
/// produces a score card with empty or misleading scenarios, and finding that out
/// after the embedding run wastes most of a day. Every count here is produced by
/// the same functions the graded run uses, so agreement is not a matter of
/// reimplementing the filters and hoping they match.
pub fn check(corpus_path: &Path, per_source: usize) -> Result<()> {
    let chunks = read_corpus(corpus_path)?;
    anyhow::ensure!(!chunks.is_empty(), "the corpus file is empty");
    let inputs: Vec<ChunkInput> = chunks.iter().map(SynthChunk::to_input).collect();
    let keys: Vec<String> = chunks
        .iter()
        .map(|c| format!("{}#{}", c.doc_id, c.chunk_index))
        .collect();

    println!("corpus: {} chunks", chunks.len());

    // Keys have to be unique, or two different chunks would be the same answer.
    let unique: std::collections::HashSet<&String> = keys.iter().collect();
    println!("  distinct keys: {} of {}", unique.len(), keys.len());
    anyhow::ensure!(unique.len() == keys.len(), "the corpus contains duplicate keys");

    // Chunk order against source. The scenarios sample with a stride precisely
    // because a prefix is one source, so that property is asserted, not assumed.
    let tenth = chunks.len() / 10;
    let prefix_sources: std::collections::BTreeSet<&str> =
        chunks[..tenth].iter().map(|c| c.source.as_str()).collect();
    let all_sources: std::collections::BTreeSet<&str> =
        chunks.iter().map(|c| c.source.as_str()).collect();
    println!(
        "  sources in the first tenth: {:?}, in the whole corpus: {:?}",
        prefix_sources, all_sources
    );
    anyhow::ensure!(
        prefix_sources.len() < all_sources.len(),
        "a prefix of the corpus covers every source, so the ingestion order was not reproduced \
         and strided sampling is measuring nothing"
    );

    // The last tenth should interleave, which is what makes a stride work.
    let tail_sources: std::collections::BTreeSet<&str> = chunks[chunks.len() - tenth..]
        .iter()
        .map(|c| c.source.as_str())
        .collect();
    println!("  sources in the last tenth: {:?}", tail_sources);
    anyhow::ensure!(
        tail_sources.len() == all_sources.len(),
        "the tail does not interleave every source"
    );

    println!("\nground truth");
    let identity = crate::queryset::document_identity_queries(&inputs, &keys, per_source, 11);
    let mut by_source: HashMap<&str, usize> = HashMap::new();
    for q in &identity {
        *by_source.entry(q.source.as_str()).or_insert(0) += 1;
    }
    println!("  document identity queries: {} in total", identity.len());
    // Iterating the sources present in the tally would skip a source that supplies
    // none, which is exactly the failure worth catching: the code shaped source
    // contributed nothing until its titles were given a second word.
    for source in &all_sources {
        let count = by_source.get(source).copied().unwrap_or(0);
        println!("    {source:<11} {count}");
        anyhow::ensure!(
            count >= per_source.min(10),
            "{source} supplies only {count} document identity queries; its titles are not usable \
             as queries, which empties the ground truth that grades fusion"
        );
    }

    let headings = crate::queryset::heading_queries(&inputs, &keys, per_source * 3, 12);
    println!("  natural language heading queries: {}", headings.len());
    anyhow::ensure!(
        headings.len() >= per_source,
        "only {} heading queries; headings need to be 15 or more characters and three or more \
         words, appearing in at most four chunks",
        headings.len()
    );

    let identifiers = crate::queryset::identifier_queries(&inputs, &keys, per_source * 3, 13);
    println!("  rare identifier queries: {}", identifiers.len());
    anyhow::ensure!(
        identifiers.len() >= per_source,
        "only {} identifier queries; the corpus needs more rare literal tokens, which come from \
         source code and from the ticket keys threaded through the prose sources",
        identifiers.len()
    );
    if let Some(example) = identifiers.first() {
        println!(
            "    for example {:?}, correct in {} chunk(s)",
            example.text,
            example.correct.len()
        );
    }

    // Can the ground truths be *answered*, not merely generated?
    //
    // This is the check that was missing, and its absence cost two full embedding
    // runs. The generation checks above pass happily on a corpus where the queries
    // are unanswerable: a title query whose document never mentions its own title, or
    // a heading query whose chunks do not contain the heading. Both engines then
    // score near zero together, which reads like a corpus that is merely harder.
    //
    // The corpus this one reproduces had both properties at 100%: every chunk carried
    // its document title in its body, and every chunk with a heading carried that
    // heading. So the check is that a correct answer contains the query text, which
    // is the weakest thing that makes the question answerable at all.
    println!("\nare the ground truths answerable");
    let content_of: HashMap<&str, &str> = keys
        .iter()
        .zip(chunks.iter())
        .map(|(k, c)| (k.as_str(), c.content.as_str()))
        .collect();

    // An identifier query is not a substring of the chunk. The generator strips every
    // character that is not alphanumeric, a dash or an underscore from a whitespace
    // separated word, so `exists_method(return_value)` yields the token
    // `exists_methodreturn_value`, which appears nowhere literally. Checking those by
    // substring would report a defect that is really a property of the generator, so
    // they are checked by applying the same filtering to the chunk's own words.
    let filtered_words = |content: &str| -> Vec<String> {
        content
            .split_whitespace()
            .map(|raw| {
                raw.chars()
                    .filter(|ch| ch.is_alphanumeric() || *ch == '-' || *ch == '_')
                    .collect::<String>()
            })
            .collect()
    };

    for (label, queries, substring, floor) in [
        ("document identity", &identity, true, 0.99),
        ("natural language headings", &headings, true, 0.99),
        ("rare identifiers", &identifiers, false, 0.99),
    ] {
        let mut answerable = 0usize;
        let mut total = 0usize;
        for q in queries.iter() {
            let needle = q.text.trim().to_lowercase();
            let reachable = q.correct.iter().any(|k| {
                content_of
                    .get(k.as_str())
                    .map(|c| {
                        if substring {
                            c.to_lowercase().contains(&needle)
                        } else {
                            // Lowercased the same way the needle was, rather than
                            // compared ASCII case insensitively. Tokens are cut with
                            // `char::is_alphanumeric`, which is Unicode aware, so a
                            // token can hold a letter outside ASCII: `Bogotá-2019`
                            // lowercases to `bogotá-2019` while an ASCII fold leaves
                            // the accented letter alone and the two never match. That
                            // reported a perfectly answerable query as unanswerable.
                            filtered_words(c).iter().any(|w| w.to_lowercase() == needle)
                        }
                    })
                    .unwrap_or(false)
            });
            total += 1;
            answerable += usize::from(reachable);
        }
        let share = if total == 0 { 0.0 } else { answerable as f64 / total as f64 };
        println!("  {label:<26} {answerable}/{total} have a correct chunk containing the query text ({:.1}%)", share * 100.0);
        anyhow::ensure!(
            share >= floor,
            "only {:.1}% of {label} queries have any correct chunk that contains the query text. \
             Those queries cannot be answered by either engine, so the scenario measures nothing \
             and both engines will score near zero on it. In the corpus this one reproduces, every \
             chunk carried its document title and its heading in its own text",
            share * 100.0
        );
    }

    // The two structural properties that make the above true, asserted directly so a
    // change to the chunk format cannot quietly remove them.
    let mut with_title = 0usize;
    let mut with_heading = 0usize;
    let mut heading_bearing = 0usize;
    for c in &chunks {
        if c.content.to_lowercase().contains(&c.title.trim().to_lowercase()) {
            with_title += 1;
        }
        if let Some(leaf) = c.heading_path.last() {
            heading_bearing += 1;
            if c.content.to_lowercase().contains(&leaf.trim().to_lowercase()) {
                with_heading += 1;
            }
        }
    }
    println!(
        "  chunks carrying their own title: {with_title}/{} ({:.1}%)",
        chunks.len(),
        100.0 * with_title as f64 / chunks.len() as f64
    );
    println!(
        "  chunks carrying their own leaf heading: {with_heading}/{heading_bearing} ({:.1}%)",
        100.0 * with_heading as f64 / heading_bearing.max(1) as f64
    );
    anyhow::ensure!(
        with_title * 100 >= chunks.len() * 99,
        "only {with_title} of {} chunks contain their document title; the original corpus had all \
         of them, and the document identity scenario depends on it",
        chunks.len()
    );
    anyhow::ensure!(
        heading_bearing == 0 || with_heading * 100 >= heading_bearing * 99,
        "only {with_heading} of {heading_bearing} chunks with a heading contain that heading; the \
         original corpus had all of them, and the heading scenarios depend on it"
    );

    // Deleted documents exist so that a filter forgetting to exclude them fails a
    // test rather than passing quietly.
    let deleted = chunks.iter().filter(|c| c.deleted).count();
    println!("\n  soft deleted chunks: {deleted}");
    anyhow::ensure!(deleted > 0, "no soft deleted documents, so no scenario can catch a filter that forgets them");

    println!("\nthe corpus supports every graded scenario");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_seconds_matches_known_timestamps() {
        assert_eq!(epoch_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_seconds("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(epoch_seconds("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        // A leap year boundary, which an arithmetic slip gets wrong by a day.
        assert_eq!(epoch_seconds("2024-03-01T00:00:00Z"), Some(1_709_251_200));
        assert_eq!(epoch_seconds("nonsense"), None);
        assert_eq!(epoch_seconds(""), None);
    }

    #[test]
    fn prose_chunks_stay_near_the_target_length() {
        // The failure this guards against is a last chunk that swallows the rest of
        // the document, which pulled one source's measured mean to nearly three
        // times its target.
        let sentence = "The index stores the chunk text beside its document attributes. ";
        let text = sentence.repeat(300);
        let pieces = split_prose(&text, 800, 5);
        assert_eq!(pieces.len(), 5);
        for p in &pieces {
            let n = p.chars().count();
            assert!(n > 400 && n < 1200, "chunk of {n} characters is not near 800");
        }
    }

    #[test]
    fn prose_chunking_does_not_split_inside_a_word() {
        let text = "Alpha beta gamma. Delta epsilon zeta. Eta theta iota. Kappa lambda mu. ".repeat(40);
        for piece in split_prose(&text, 200, 6) {
            let trimmed = piece.trim();
            assert!(!trimmed.is_empty());
            // Every chunk begins at a word boundary, so no chunk starts mid word.
            let first = trimmed.split_whitespace().next().unwrap();
            assert!(
                text.contains(first),
                "chunk began with {first:?}, which is not a word in the source"
            );
        }
    }

    #[test]
    fn prose_chunking_asks_for_no_more_than_the_text_holds() {
        let pieces = split_prose("Two short sentences. That is all there is.", 500, 9);
        assert_eq!(pieces.len(), 1);
    }

    #[test]
    fn code_chunks_break_on_line_boundaries() {
        let code = (0..200)
            .map(|i| format!("fn handler_{i}(request: Request) -> Response {{ dispatch(request) }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let pieces = split_code(&code, 400, 6);
        assert!(!pieces.is_empty());
        for p in &pieces {
            for line in p.lines() {
                assert!(
                    line.is_empty() || code.contains(line),
                    "a line was cut in half: {line:?}"
                );
            }
        }
    }

    #[test]
    fn threads_split_at_headings_and_keep_the_heading() {
        let text = "Preamble text here. Why is this slow? Because the scan is exhaustive. \
                    What about the cache? It only helps the second call.";
        let headings = vec!["Why is this slow?".to_string(), "What about the cache?".to_string()];
        let threads = split_threads(text, &headings);
        assert_eq!(threads.len(), 1, "only bodies of 120+ characters are kept");
        let long = "x".repeat(200);
        let text = format!("Intro. Why is this slow? {long} What about the cache? {long}");
        let threads = split_threads(&text, &headings);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].0, "Why is this slow?");
        assert!(threads[0].1.contains(&long));
    }

    #[test]
    fn a_page_with_no_headings_is_one_thread() {
        let threads = split_threads("Just one continuous message with no sections at all.", &[]);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].0, "");
    }

    #[test]
    fn chunks_per_doc_reproduces_the_measured_quantiles() {
        let mut rng = StdRng::seed_from_u64(7);
        let q = (1, 5, 17, 157);
        let mut draws: Vec<usize> = (0..20_000).map(|_| chunks_per_doc(&mut rng, q)).collect();
        draws.sort_unstable();
        let at = |p: f64| draws[((draws.len() as f64) * p) as usize];
        assert!(at(0.5) >= 4 && at(0.5) <= 6, "median was {}", at(0.5));
        assert!(at(0.9) >= 15 && at(0.9) <= 19, "p90 was {}", at(0.9));
        assert!(*draws.first().unwrap() >= 1);
        assert!(*draws.last().unwrap() <= 157);
    }

    #[test]
    fn heading_depth_follows_the_measured_shares() {
        let mut rng = StdRng::seed_from_u64(9);
        let shares = [0.084, 0.475, 0.347, 0.094];
        let mut counts = [0usize; 4];
        for _ in 0..40_000 {
            counts[heading_depth(&mut rng, &shares)] += 1;
        }
        for (depth, share) in shares.iter().enumerate() {
            let seen = counts[depth] as f64 / 40_000.0;
            assert!(
                (seen - share).abs() < 0.02,
                "depth {depth} came out at {seen:.3}, expected about {share}"
            );
        }
    }

    #[test]
    fn heading_paths_are_built_from_the_documents_own_headings() {
        let headings = vec![
            "Rolling out the change".to_string(),
            "Measuring the result".to_string(),
            "Open questions".to_string(),
        ];
        assert!(heading_path(0, &headings, 0, 9).is_empty());
        for depth in 1..=3 {
            let path = heading_path(depth, &headings, 1, 9);
            assert!(!path.is_empty() && path.len() <= depth);
            for element in &path {
                assert!(headings.contains(element), "invented a heading: {element:?}");
            }
            // A repeated element would make the leaf ambiguous as a query.
            let unique: std::collections::HashSet<&String> = path.iter().collect();
            assert_eq!(unique.len(), path.len());
        }
    }

    #[test]
    fn the_leaf_heading_is_the_section_the_chunk_falls_in() {
        // This is the property the natural language ground truth depends on. With
        // headings attached to chunks at random, that ground truth is unanswerable
        // and both engines score near zero on it.
        let headings: Vec<String> = vec!["First".into(), "Second".into(), "Third".into()];
        let total = 9;
        let leaves: Vec<String> = (0..total)
            .map(|nth| heading_path(1, &headings, nth, total)[0].clone())
            .collect();
        assert_eq!(
            leaves,
            vec!["First", "First", "First", "Second", "Second", "Second", "Third", "Third", "Third"]
        );
        // The leaf advances through the document and never goes backwards.
        let mut positions: Vec<usize> = leaves
            .iter()
            .map(|l| headings.iter().position(|h| h == l).unwrap())
            .collect();
        let sorted = {
            let mut c = positions.clone();
            c.sort_unstable();
            c
        };
        assert_eq!(positions, sorted);
        positions.dedup();
        assert_eq!(positions, vec![0, 1, 2], "every section should be reached");
    }

    #[test]
    fn a_deeper_path_keeps_the_chunks_own_section_as_the_leaf() {
        let headings: Vec<String> = vec!["A".into(), "B".into(), "C".into(), "D".into()];
        for nth in 0..8 {
            let leaf1 = heading_path(1, &headings, nth, 8);
            let leaf3 = heading_path(3, &headings, nth, 8);
            assert_eq!(
                leaf1.last(),
                leaf3.last(),
                "depth changed the leaf at chunk {nth}, so the ancestors were prepended wrongly"
            );
        }
    }

    #[test]
    fn a_document_with_no_headings_gets_an_empty_path() {
        assert!(heading_path(2, &[], 0, 4).is_empty());
    }

    #[test]
    fn authors_are_distinct_and_carry_stable_identifiers() {
        let authors = authors_for("confluence", 473);
        assert_eq!(authors.len(), 473);
        let names: std::collections::HashSet<&String> = authors.iter().map(|(n, _)| n).collect();
        assert_eq!(names.len(), 473, "author display names must not repeat");
        let ids: std::collections::HashSet<&String> = authors.iter().map(|(_, i)| i).collect();
        assert_eq!(ids.len(), 473);
        // Regenerating gives the same list, so a rebuild does not reshuffle authors.
        assert_eq!(authors_for("confluence", 473), authors);
    }

    #[test]
    fn every_source_is_allocated_articles_in_proportion_to_its_need() {
        let plan_list = plans(1.0);
        // Articles long enough that no source is starved, so the test measures the
        // allocation rather than a shortage.
        let articles: Vec<RawArticle> = (0..20_000)
            .map(|i| RawArticle {
                title: format!("Article number {i} about a subject"),
                headings: vec!["First section".to_string()],
                categories: vec![],
                timestamp: None,
                text: "sentence. ".repeat(2_000 + (i % 3_000)),
            })
            .collect();
        let allocated = allocate_articles(articles, &plan_list);
        for plan in &plan_list {
            if !matches!(
                plan.material,
                Material::Articles | Material::DesignFiles | Material::Boards
            ) {
                continue;
            }
            let got = allocated.get(plan.name).expect("an article source was not allocated");
            assert_eq!(
                got.len(),
                plan.documents,
                "{} received {} articles for {} documents",
                plan.name,
                got.len(),
                plan.documents
            );
            let have: usize = got.iter().map(|a| a.text.len()).sum();
            let need = (plan.chunks as f64 * plan.mean_chars as f64 / expansion(plan.material)) as usize;
            assert!(
                have >= need,
                "{} was allocated {have} characters but needs {need}",
                plan.name
            );
        }
    }

    #[test]
    fn design_files_and_boards_are_reformatted_rather_than_copied() {
        let text = "The rollout begins on Tuesday. Every region is included, with one exception. \
                    The exception is documented separately.";
        let design = as_design_file("Checkout redesign", &["Frame one".to_string()], text);
        assert!(design.starts_with("Design file: Checkout redesign"));
        assert!(design.contains("Text layer:"));
        let board = as_board("Retro board", text);
        assert!(board.starts_with("Board: Retro board"));
        assert!(board.contains("Note:"));
        // The reformatting is what makes these sources lexically distinct from the
        // page shaped source built from the same kind of article.
        assert!(!design.contains("Note:"));
        assert!(!board.contains("Text layer:"));
    }

    #[test]
    fn the_planned_sizes_match_what_was_measured() {
        let total: usize = plans(1.0).iter().map(|p| p.chunks).sum();
        assert_eq!(total, 186_839);
        let documents: usize = plans(1.0).iter().map(|p| p.documents).sum();
        assert_eq!(documents, 39_366);
        // Scaling keeps the proportions between sources.
        let half = plans(0.5);
        for (full, part) in plans(1.0).iter().zip(&half) {
            assert_eq!(part.name, full.name);
            let ratio = part.chunks as f64 / full.chunks as f64;
            assert!((ratio - 0.5).abs() < 0.01, "{} scaled to {ratio}", part.name);
        }
    }

    #[test]
    fn identifiers_are_threaded_in_at_a_rate_that_leaves_them_rare() {
        let mut rng = StdRng::seed_from_u64(3);
        let mut carrying = 0usize;
        for _ in 0..4_000 {
            let mut text = "An ordinary sentence of prose. ".to_string();
            let before = text.len();
            thread_identifiers(&mut rng, &mut text, "confluence");
            if text.len() != before {
                carrying += 1;
            }
        }
        let share = carrying as f64 / 4_000.0;
        // Rare enough that a token appears in only a few chunks, which is what the
        // identifier scenario requires, but common enough to supply candidates.
        assert!(share > 0.03 && share < 0.15, "{share} of chunks carried an identifier");
    }

    #[test]
    fn ticket_keys_look_like_a_tracker_wrote_them() {
        let mut rng = StdRng::seed_from_u64(5);
        for _ in 0..200 {
            let key = ticket_key(&mut rng);
            let (project, number) = key.split_once('-').expect("a ticket key has a dash");
            assert!(PROJECTS.contains(&project), "unknown project {project}");
            let n: u32 = number.parse().expect("the tail of a ticket key is a number");
            assert!((100..9999).contains(&n));
            // The identifier scenario only considers tokens of six characters or
            // more that mix letters with digits or a separator.
            assert!(key.len() >= 6);
        }
    }

    #[test]
    fn labels_cluster_around_the_measured_mean() {
        let mut rng = StdRng::seed_from_u64(11);
        let mut total = 0usize;
        let runs = 5_000;
        for _ in 0..runs {
            total += labels_for(&mut rng, 2.26, &[]).len();
        }
        let mean = total as f64 / runs as f64;
        assert!((mean - 2.26).abs() < 0.3, "mean label count was {mean}");
    }

    #[test]
    fn labels_take_the_sources_own_categories_too() {
        let mut rng = StdRng::seed_from_u64(13);
        let labels = labels_for(&mut rng, 1.0, &["Months of the Year".to_string()]);
        assert!(
            labels.iter().any(|l| l == "months-of-the-year"),
            "a category was not carried through: {labels:?}"
        );
    }

    #[test]
    fn a_synth_chunk_converts_to_the_engines_input() {
        let chunk = SynthChunk {
            doc_id: 42,
            source: "confluence".into(),
            chunk_index: 3,
            heading_path: vec!["Section".into()],
            content: "body".into(),
            title: "A descriptive title".into(),
            url: "https://example.invalid/x".into(),
            space_key: Some("ENG".into()),
            author: Some("Ada Almeida".into()),
            author_id: Some("confluence-u0001".into()),
            updated_at: Some(1_700_000_000),
            labels: vec!["reference".into()],
            deleted: false,
        };
        let input = chunk.to_input();
        // The document identifier is the key both engines report, so it has to
        // survive the conversion exactly.
        assert_eq!(input.external_doc_id, "42");
        assert_eq!(input.chunk_index, 3);
        assert_eq!(input.source, "confluence");
        assert_eq!(input.title, "A descriptive title");
    }

    #[test]
    fn every_chunk_carries_its_title_and_its_leaf_heading_in_its_text() {
        // Measured on the corpus this one reproduces: 186,860 of 186,860 chunks
        // contained their document title, and 178,967 of 178,967 chunks with a
        // heading contained that heading. Two graded scenarios query by title and by
        // heading, so a corpus without them makes those questions unanswerable rather
        // than harder, and both engines collapse together on them.
        let path = vec!["Rollout".to_string(), "Regions".to_string()];
        let text = breadcrumbed("Release process", &path, "Every region is included.");
        assert!(text.contains("Release process"));
        assert!(text.contains("Regions"));
        assert!(text.starts_with("Release process > Rollout > Regions"));
        assert!(text.contains("Every region is included."));

        // A chunk with no heading still carries its title.
        let bare = breadcrumbed("Release process", &[], "Body text.");
        assert!(bare.starts_with("Release process\n\n"));
        assert!(bare.contains("Body text."));
    }

    #[test]
    fn the_breadcrumb_estimate_leaves_the_body_room_to_hit_the_target() {
        // The measured mean chunk lengths include the breadcrumb, so the body has to
        // be cut shorter by roughly what the breadcrumb takes.
        let headings = vec!["A fairly long section heading".to_string()];
        let shares = [0.084, 0.475, 0.347, 0.094];
        let estimate = breadcrumb_estimate("A document title", &headings, &shares);
        let actual = breadcrumbed("A document title", &headings[..1], "").chars().count();
        // The estimate is an average over the depth distribution, so it lands within
        // a heading's length of a single realised breadcrumb rather than exactly on it.
        assert!(
            estimate + 40 > actual && actual + 40 > estimate,
            "estimate {estimate} is nowhere near a realised breadcrumb of {actual}"
        );
    }

    #[test]
    fn sanitizing_is_idempotent() {
        // `read_corpus` cleans the corpus and the embedding step cleans each text
        // again before tokenizing. If the second pass changed anything, the stored
        // text and the text the vector was made from would drift apart.
        let dirty = "a\u{0}b\tc\nd\u{7}e";
        let once = sanitize_for_model(dirty);
        assert_eq!(sanitize_for_model(&once), once);
    }

    #[test]
    fn a_nul_byte_is_removed_because_postgres_cannot_store_one() {
        // Five chunks drawn from source files carried a NUL byte, and PostgreSQL
        // rejects one in a text column outright, failing the whole load.
        let cleaned = sanitize_for_model("fn main() {\u{0} let x = 1; }");
        assert!(!cleaned.contains('\u{0}'));
        assert!(cleaned.contains("fn main()"));
        assert!(cleaned.contains("let x = 1;"));
    }

    #[test]
    fn sanitizing_keeps_the_text_and_drops_control_characters() {
        let dirty = "line one\nline\u{0}two\ttabbed\u{7}";
        let clean = sanitize_for_model(dirty);
        assert!(clean.contains("line one\nline"));
        assert!(clean.contains("two\ttabbed"));
        assert!(!clean.contains('\u{0}'));
        assert!(!clean.contains('\u{7}'));
    }
}
