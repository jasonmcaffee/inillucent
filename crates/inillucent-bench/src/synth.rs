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
use inillucent_core::embed_onnx::Device;
use inillucent_core::model::{Backend, ModelManifest};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

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
    let file = File::open(path).with_context(|| {
        format!(
            "opening {}. Run the fetch and extract scripts first",
            path.display()
        )
    })?;
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
                        Some(current) => candidate.abs_diff(ideal) < current.abs_diff(ideal),
                    };
                    if better {
                        found = Some(candidate);
                    }
                }
            }
            end = found.unwrap_or(ideal);
        }
        let piece: String = chars.get(start..end).unwrap_or(&[]).iter().collect();
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
    "Ada", "Bo", "Cai", "Dara", "Eli", "Fen", "Gita", "Hale", "Ines", "Jo", "Kian", "Lore", "Mira",
    "Nils", "Oona", "Pav", "Quill", "Rune", "Sena", "Tov", "Uma", "Vero", "Wren", "Xan", "Yara",
    "Zev", "Anwen", "Bram", "Cleo", "Dov", "Esme", "Faro", "Gwen",
];
const FAMILY: &[&str] = &[
    "Almeida",
    "Bergstrom",
    "Calder",
    "Dunne",
    "Eriksen",
    "Falk",
    "Grieve",
    "Halloran",
    "Ibarra",
    "Jarosz",
    "Keller",
    "Lindqvist",
    "Moreau",
    "Nakhle",
    "Ostrand",
    "Pereira",
    "Quintero",
    "Rasmussen",
    "Sandoval",
    "Thorne",
    "Ueda",
    "Vasquez",
    "Whitlock",
    "Ximenes",
    "Yoshida",
    "Zabala",
    "Aldridge",
    "Boone",
    "Cortese",
    "Delgado",
];

/// Space names per source, used where a source shares a fixed set of spaces.
/// A repository name stands in for a space in the code shaped source, a channel
/// name in the message shaped source and a project key in the ticket shaped one.
const CHANNELS: &[&str] = &[
    "general",
    "engineering",
    "platform-team",
    "release-notes",
    "incident-response",
    "design-review",
    "data-eng",
    "search-quality",
    "onboarding",
    "infra",
    "security",
    "product",
    "analytics",
    "mobile",
    "web",
    "api-design",
    "billing",
    "support",
    "docs",
    "tooling",
    "performance",
    "testing",
    "hiring",
    "random",
];
/// One entry of a fixed table, by an index of any size.
///
/// **Every table in this module is a non-empty constant and every index here
/// is reduced modulo its length, so the fallback is unreachable.** It is
/// written as a fallback rather than as an `unwrap` because the alternative
/// under `deny(clippy::indexing_slicing)` is twenty `unwrap`s in a corpus
/// generator, and one of those firing halfway through leaves a half-written
/// corpus on disk for somebody to grade.
///
/// @param table - the fixed table to choose from
/// @param at - any index; it is reduced modulo the table's length
fn from_table<'a>(table: &[&'a str], at: usize) -> &'a str {
    if table.is_empty() {
        return "";
    }
    table.get(at % table.len()).copied().unwrap_or("")
}

const PROJECTS: &[&str] = &[
    "PLAT", "SRCH", "DATA", "INFRA", "WEB", "MOB", "API", "BILL", "SUP", "DOC", "TOOL", "PERF",
    "SEC", "ANL", "REL", "DES", "ONB", "QA", "OPS", "ML", "CORE", "EXP",
];

/// A deterministic author list of the requested size.
fn authors_for(name: &str, count: usize) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(count);
    let mut seen = std::collections::HashSet::new();
    let mut i = 0usize;
    while out.len() < count {
        let given = from_table(GIVEN, i);
        let family = from_table(FAMILY, i / GIVEN.len() + i * 7);
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
            let display = format!("{} {}", from_table(GIVEN, out.len()), out.len());
            let id = format!("{}-u{:04}", name, out.len() + 1);
            out.push((display, id));
        }
    }
    out
}

/// Label vocabulary. Labels are filtered on by one scenario, so they need a
/// realistic distribution: a few common ones and a long tail.
const LABELS: &[&str] = &[
    "reference",
    "runbook",
    "decision-record",
    "postmortem",
    "how-to",
    "architecture",
    "onboarding",
    "deprecated",
    "draft",
    "reviewed",
    "external",
    "internal",
    "roadmap",
    "spike",
    "migration",
    "performance",
    "security",
    "accessibility",
    "analytics",
    "experiment",
    "retired",
    "template",
    "faq",
    "glossary",
    "policy",
    "meeting-notes",
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
        let label = from_table(LABELS, i.min(LABELS.len().saturating_sub(1))).to_string();
        if !out.contains(&label) {
            out.push(label);
        }
    }
    for e in extra.iter().take(2) {
        let cleaned: String = e
            .chars()
            .map(|c| {
                if c.is_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
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
    let project = from_table(PROJECTS, rng.gen_range(0..PROJECTS.len()));
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
    let last = headings.len().saturating_sub(1);
    let section = if total <= 1 {
        0
    } else {
        (nth * headings.len() / total).min(last)
    };
    let first = section + 1 - depth.min(section + 1);
    let mut path = Vec::with_capacity(depth);
    // `section` is clamped to the last heading and `first` is no greater than
    // it, so the range is one the slice holds.
    for h in headings.get(first..=section).unwrap_or(&[]) {
        let h = h.trim();
        if h.is_empty() || path.iter().any(|p| p == h) {
            continue;
        }
        path.push(h.to_string());
    }
    if path.is_empty() {
        if let Some(only) = headings.get(section) {
            path.push(only.trim().to_string());
        }
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
        .filter(|p| {
            matches!(
                p.material,
                Material::Articles | Material::DesignFiles | Material::Boards
            )
        })
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
        // `pick` is an index `needs.iter().enumerate()` produced, so this is
        // the same element written where the compiler can see it.
        let Some(need) = needs.get_mut(i) else { break };
        need.documents_left -= 1;
        need.chars_left = (need.chars_left - article.text.len() as f64).max(0.0);
        let Some(slice) = out.get_mut(need.name) else {
            // Every source in `needs` is given a slice above, so this cannot
            // happen; the article is left in the pool rather than dropped
            // silently into a source nobody asked for.
            break;
        };
        slice.push(article);
    }
    out
}

/// One raw item taken from a pool, reshaped into the fields a document needs.
///
/// **The seven fields used to be a seven-tuple built by a 166-line `match`
/// inside `build_source`.** Four of them are `String` and two are
/// `Option<String>`, so the tuple's order was the only thing saying which was
/// the title and which the url, and each material arm restated that order from
/// memory.
struct RawDocument {
    /// The document's title, which is also the identity family's query.
    title: String,
    /// The section headings, deepest last, that a chunk's breadcrumb is cut from.
    headings: Vec<String>,
    /// What a real system would have recorded as labels or components.
    categories: Vec<String>,
    /// The raw item's own timestamp, when it carried one.
    timestamp: Option<String>,
    /// The text the chunks are cut out of.
    text: String,
    /// The space, channel or project this document belongs to.
    space: Option<String>,
    /// A url in the reserved `example.invalid` domain.
    url: String,
    /// How many chunks to cut, which the article materials cap at what their
    /// raw text actually has room for.
    chunks: usize,
}

/// Documents for one source, consuming from the pools so no raw item is used
/// twice anywhere in the corpus.
///
/// **Three stages, each its own function.** The chunk counts are decided for
/// every document before any text is read, then one raw item is taken per
/// document, then the document is assembled from it. A pool that runs out ends
/// the loop, and `build` reports the shortfall against the plan.
///
/// @param plan - the source being built
/// @param pools - the raw material, consumed as it is used
/// @param rng - the source's own generator, so a source is reproducible
fn build_source(plan: &SourcePlan, pools: &mut Pools, rng: &mut StdRng) -> Result<Vec<Document>> {
    let authors = authors_for(plan.name, plan.authors);
    let mut docs: Vec<Document> = Vec::with_capacity(plan.documents);
    let counts = chunk_counts_for(plan, rng);

    // Documents needing the most text are filled first, so the longest raw items
    // go where they are needed and nothing is wasted.
    let mut order: Vec<usize> = (0..counts.len()).collect();
    order.sort_by(|a, b| counts.get(*b).cmp(&counts.get(*a)));

    for &slot in &order {
        let want = counts.get(slot).copied().unwrap_or(0);
        if want == 0 {
            continue;
        }
        let Some(raw) = raw_document_for(plan, pools, rng, want)? else {
            break;
        };
        if let Some(doc) = document_from(plan, rng, &authors, raw) {
            docs.push(doc);
        }
    }

    Ok(docs)
}

/// How many chunks each of this source's documents gets.
///
/// **Drawn from the measured shape and then scaled to the measured total.**
/// Drawing alone reproduces the distribution and misses the size; setting every
/// document to the mean would hit the size and make the per document result cap
/// measure nothing.
///
/// @param plan - the source being built
/// @param rng - the source's own generator
fn chunk_counts_for(plan: &SourcePlan, rng: &mut StdRng) -> Vec<usize> {
    let mut counts: Vec<usize> = (0..plan.documents)
        .map(|_| chunks_per_doc(rng, plan.per_doc))
        .collect();
    let drawn: usize = counts.iter().sum();
    if drawn == 0 || plan.chunks == 0 {
        return counts;
    }
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
        guard += 1;
        let Some(count) = counts.get_mut(i) else {
            continue;
        };
        if total < plan.chunks && *count < plan.per_doc.3 {
            *count += 1;
            total += 1;
        } else if total > plan.chunks && *count > plan.per_doc.0 {
            *count -= 1;
            total -= 1;
        }
    }
    counts
}

/// One raw item for one document, taken from whichever pool this source's
/// material comes from.
///
/// `Ok(None)` means that pool is exhausted, which ends the source.
///
/// @param plan - the source being built
/// @param pools - the raw material, consumed as it is used
/// @param rng - the source's own generator
/// @param want - how many chunks this document is meant to hold
fn raw_document_for(
    plan: &SourcePlan,
    pools: &mut Pools,
    rng: &mut StdRng,
    want: usize,
) -> Result<Option<RawDocument>> {
    match plan.material {
        Material::Articles | Material::DesignFiles | Material::Boards => {
            raw_from_articles(plan, pools, rng, want)
        }
        Material::Discussions => Ok(raw_from_discussions(plan, pools, rng, want)),
        Material::Code => Ok(raw_from_code(plan, pools, want)),
        Material::Issues => Ok(raw_from_issues(plan, pools, rng, want)),
    }
}

/// A document from the article pool, which three of the six sources stand on.
///
/// The slice is this source's own, longest first, paired rank for rank with
/// documents ordered by how much text they need.
///
/// @param plan - the source being built
/// @param pools - the raw material, consumed as it is used
/// @param rng - the source's own generator
/// @param want - how many chunks this document is meant to hold
fn raw_from_articles(
    plan: &SourcePlan,
    pools: &mut Pools,
    rng: &mut StdRng,
    want: usize,
) -> Result<Option<RawDocument>> {
    let slice = pools
        .articles
        .get_mut(plan.name)
        .with_context(|| format!("source {} was never allocated an article slice", plan.name))?;
    let Some(a) = slice.pop() else {
        return Ok(None);
    };
    let a = &a;
    // A document cannot hold more chunks than its article has text for.
    // Capping here rather than skipping keeps the document, which is what
    // makes the document count reachable.
    let available = ((a.text.len() as f64) * expansion(plan.material)) as usize;
    let want = want
        .min((available / plan.mean_chars).max(plan.per_doc.0))
        .max(1);
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
    Ok(Some(RawDocument {
        title: a.title.clone(),
        headings: a.headings.clone(),
        categories: a.categories.clone(),
        timestamp: a.timestamp.clone(),
        text: a.text.clone(),
        space,
        url,
        chunks: want,
    }))
}

/// A document from the discussion pool: one thread out of a talk page.
///
/// A thread rather than the whole page, which is one heading and the exchange
/// under it. The title names the page and the thread, both written by people,
/// which keeps it descriptive and unique.
///
/// @param plan - the source being built
/// @param pools - the raw material, consumed as it is used
/// @param rng - the source's own generator
/// @param want - how many chunks this document is meant to hold
fn raw_from_discussions(
    plan: &SourcePlan,
    pools: &mut Pools,
    rng: &mut StdRng,
    want: usize,
) -> Option<RawDocument> {
    let t = pools.talk.pop()?;
    let threads = split_threads(&t.text, &t.headings);
    let (heading, body) = threads
        .into_iter()
        .max_by_key(|(_, b)| b.len())
        .unwrap_or((String::new(), t.text.clone()));
    let title = if heading.trim().is_empty() {
        t.title.clone()
    } else {
        format!(
            "{}: {}",
            t.title.trim_start_matches("Talk:"),
            heading.trim()
        )
    };
    let channel = from_table(
        CHANNELS,
        rng.gen_range(
            0..CHANNELS.len().min(match plan.spaces {
                Spaces::Shared(n) => n,
                Spaces::PerDocument => CHANNELS.len(),
            }),
        ),
    );
    Some(RawDocument {
        title,
        headings: if heading.trim().is_empty() {
            vec![t.title.clone()]
        } else {
            vec![heading]
        },
        categories: Vec::new(),
        timestamp: t.timestamp.clone(),
        text: body,
        space: Some(channel.to_string()),
        url: format!("https://example.invalid/{}/{}", plan.name, rng.gen::<u32>()),
        chunks: want,
    })
}

/// A document from the code pool: one source file.
///
/// **The repository and the path together are the title.** The path alone was
/// written by a person, is unique and is full of the compound identifiers the
/// tokenizer has specific handling for, but it holds no spaces - and the
/// document identity ground truth requires two or more words, so with the path
/// alone this source contributed no graded queries at all, silently.
///
/// @param plan - the source being built
/// @param pools - the raw material, consumed as it is used
/// @param want - how many chunks this document is meant to hold
fn raw_from_code(plan: &SourcePlan, pools: &mut Pools, want: usize) -> Option<RawDocument> {
    let c = pools.code.pop()?;
    let title = format!("{} {}", c.repo, c.path);
    let dirs: Vec<String> = Path::new(&c.path)
        .parent()
        .map(|p| {
            p.components()
                .map(|x| x.as_os_str().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    Some(RawDocument {
        title,
        headings: dirs,
        categories: vec![c.repo.clone()],
        timestamp: None,
        text: c.text.clone(),
        space: Some(c.repo.clone()),
        url: format!(
            "https://example.invalid/{}/{}/{}",
            plan.name, c.repo, c.path
        ),
        chunks: want,
    })
}

/// A document from the issue pool: one ticket.
///
/// The ticket gets a key of its own, so the ticket shaped source reads like a
/// tracker and its keys are searchable literals. The repository stands in for
/// the component a real tracker records.
///
/// @param plan - the source being built
/// @param pools - the raw material, consumed as it is used
/// @param rng - the source's own generator
/// @param want - how many chunks this document is meant to hold
fn raw_from_issues(
    plan: &SourcePlan,
    pools: &mut Pools,
    rng: &mut StdRng,
    want: usize,
) -> Option<RawDocument> {
    let i = pools.issues.pop()?;
    let project = from_table(
        PROJECTS,
        rng.gen_range(
            0..PROJECTS.len().min(match plan.spaces {
                Spaces::Shared(n) => n,
                Spaces::PerDocument => PROJECTS.len(),
            }),
        ),
    );
    let key = format!("{project}-{}", 1000 + (i.number % 9000));
    let mut categories = vec![i.repo.clone()];
    categories.extend(i.labels.iter().cloned());
    Some(RawDocument {
        title: format!("{key} {}", i.title),
        headings: vec![
            "Description".to_string(),
            "Steps to reproduce".to_string(),
            "Acceptance".to_string(),
        ],
        categories,
        timestamp: i.updated_at.clone(),
        text: i.body.clone(),
        space: Some(project.to_string()),
        url: format!("https://example.invalid/{}/{}", plan.name, key),
        chunks: want,
    })
}

/// One raw item as a document: reformatted into the register its source stands
/// in for, split into chunks, and given the author, timestamp and labels a real
/// system would have recorded.
///
/// `None` when the text did not split into a single chunk, which is a raw item
/// too short for this source's chunk length rather than an error.
///
/// @param plan - the source being built
/// @param rng - the source's own generator
/// @param authors - this source's author list
/// @param raw - the raw item taken from the pool
fn document_from(
    plan: &SourcePlan,
    rng: &mut StdRng,
    authors: &[(String, String)],
    raw: RawDocument,
) -> Option<Document> {
    let RawDocument {
        title,
        headings,
        categories,
        timestamp,
        text,
        space,
        url,
        chunks: want,
    } = raw;

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
        return None;
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

    let (author, author_id) = match authors.get(rng.gen_range(0..authors.len().max(1))) {
        Some((name, id)) => (Some(name.clone()), Some(id.clone())),
        None => (None, None),
    };

    // Spread over the same span the original corpus covered, so the
    // `updated_after` filter selects a comparable share.
    let updated_at = timestamp
        .as_deref()
        .and_then(epoch_seconds)
        .or(Some(1_515_628_800 + rng.gen_range(0..274_000_000)));

    Some(Document {
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
    })
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
            let name = headings
                .get(frame % headings.len().max(1))
                .map(String::as_str)
                .unwrap_or("Frame");
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
    /// This chunk as the input the index and the cache both take.
    ///
    /// **The conversion is here rather than in the index, because the
    /// generated corpus is the only thing that has a `SynthChunk`.** What it
    /// drops is the external chunk identifier: the generator has no source
    /// system to have taken one from, and inventing one would put a
    /// synthetic identifier in a field a real corpus fills from a real API.
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
///
/// **Four stages, each its own function: read the raw material, fill every
/// source's documents from it, decide the order they are laid down in, and
/// write them.** The order stage is the one that is easy to lose: a corpus
/// written source by source throughout would make a prefix of it
/// unrepresentative of the whole, which is what `strided_sample` exists for,
/// and one written interleaved throughout would make the stride pointless.
///
/// @param derived - the directory the public material was extracted to
/// @param out - where the JSONL corpus goes
/// @param scale - a multiplier on every source's document and chunk targets
pub fn build(derived: &Path, out: &Path, scale: f64) -> Result<BuildReport> {
    let mut rng = StdRng::seed_from_u64(SEED);
    let plan_list = plans(scale);
    let mut pools = read_the_raw_material(derived, &plan_list)?;
    let all = fill_every_source(&plan_list, &mut pools, &mut rng)?;
    let ordered = in_ingestion_order(all, &mut rng);
    write_the_corpus(out, &ordered)
}

/// Every pool of raw material, read off disk and allocated per source.
///
/// **Two article pools, and both are used when both are present.** The Simple
/// English dump is small and its articles are short; the English dump supplies
/// the long articles the page shaped and design file shaped sources need, and a
/// much larger vocabulary. The pipeline still works with only the small one
/// available, which is what the `path.exists()` test is for.
///
/// @param derived - the directory the public material was extracted to
/// @param plan_list - the sources being built, which decide the article split
fn read_the_raw_material(derived: &Path, plan_list: &[SourcePlan]) -> Result<Pools> {
    eprintln!("reading the public material from {}", derived.display());
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
        let words = t
            .split_whitespace()
            .filter(|w| w.chars().any(char::is_alphanumeric))
            .count();
        let usable = t.chars().count() >= 12 && t.chars().count() <= 160 && words >= 2;
        // Longest usable titles first, so the largest documents also get gradeable
        // titles rather than the pool's leftovers.
        (!usable, std::cmp::Reverse(a.text.len()))
    });

    // Each article based source gets its own slice. They are stored ascending by
    // length because documents are filled from the back, largest need first.
    let mut allocated = allocate_articles(articles, plan_list);
    for slice in allocated.values_mut() {
        slice.reverse();
    }
    let mut pools = Pools {
        articles: allocated,
        talk,
        code,
        issues,
    };
    // Popped from the back, so the order is made deliberate rather than incidental.
    pools.talk.sort_by_key(|t| t.text.len());
    pools.code.sort_by_key(|c| c.text.len());
    pools.issues.sort_by_key(|i| i.body.len());
    Ok(pools)
}

/// Every source's documents, and a line per source saying what it reached
/// against its target.
///
/// A source that falls short says so rather than being silently smaller: the
/// shortfall is a pool that ran out, which is a fact about the extracted
/// material rather than about the plan.
///
/// @param plan_list - the sources being built
/// @param pools - the raw material, consumed as it is used
/// @param rng - the corpus generator
fn fill_every_source(
    plan_list: &[SourcePlan],
    pools: &mut Pools,
    rng: &mut StdRng,
) -> Result<Vec<Document>> {
    let mut all: Vec<Document> = Vec::new();
    for plan in plan_list {
        let docs = build_source(plan, pools, rng)?;
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
    Ok(all)
}

/// The documents in the order they are laid down.
///
/// Grouped by phase, and inside the tail the sources are interleaved - which is
/// what makes a prefix of the corpus unrepresentative of the whole and
/// `strided_sample` necessary.
///
/// @param all - every document, grouped by source
/// @param rng - the corpus generator, which shuffles the tail
fn in_ingestion_order(all: Vec<Document>, rng: &mut StdRng) -> Vec<Document> {
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
    tail.shuffle(rng);
    bulk.into_iter().chain(tail).collect()
}

/// Write the corpus as JSONL, and report what was written.
///
/// Document identifiers are assigned in the order the documents are laid down,
/// so the identifier ordering correlates with source exactly as it did in the
/// corpus this one reproduces.
///
/// @param out - where the JSONL corpus goes
/// @param ordered - the documents, in ingestion order
fn write_the_corpus(out: &Path, ordered: &[Document]) -> Result<BuildReport> {
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
                && t.split_whitespace()
                    .filter(|w| w.chars().any(char::is_alphanumeric))
                    .count()
                    >= 2
        })
        .count();

    let mut rows: Vec<(String, usize, usize, usize)> = per_source
        .into_iter()
        .map(|(s, (docs, chunks, chars))| {
            (
                s.to_string(),
                docs,
                chunks,
                chars.checked_div(chunks).unwrap_or(0),
            )
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

    let done = vectors_already_written(&vectors_path, bytes_per)?;
    if done >= chunks.len() {
        eprintln!("every chunk is already embedded");
        // A run that embedded nothing cannot report how much was truncated, so it
        // counts with the tokenizer alone rather than writing a zero that would
        // read as "nothing was cut".
        let truncated = crate::truncation::count_truncated(model_dir, manifest, &chunks)?;
        return assemble_cache(
            &chunks,
            &vectors_path,
            cache_path,
            manifest,
            seeds,
            truncated,
        );
    }

    let embedders = open_arms(model, options, devices)?;

    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&vectors_path)
        .with_context(|| format!("opening {}", vectors_path.display()))?;

    // A window is `window_batches` batches per device. It has to be several rather
    // than one: the devices are synchronised at the end of every window, so a window
    // of one batch each pays that synchronisation on every 32 chunks and the faster
    // card spends its time waiting. Measured on two 5090s at batch 32: one batch per
    // device ran 613/sec against 450 for a single card, 1.36x rather than 2x.
    let window = options.batch_size * embedders.len() * window_batches.max(1);
    write_every_window(
        &embedders,
        &chunks,
        &mut out,
        done,
        report_every,
        window,
        dims,
    )?;
    drop(out);

    let truncated =
        truncation_over_the_whole_corpus(&embedders, &chunks, done, model_dir, manifest)?;

    assemble_cache(
        &chunks,
        &vectors_path,
        cache_path,
        manifest,
        seeds,
        truncated,
    )
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
            arm.backend_label(options, &model.manifest.id),
            load.elapsed().as_secs_f64()
        );
        return Ok(vec![arm]);
    }
    anyhow::ensure!(!devices.is_empty(), "no devices to embed on");
    let mut embedders = Vec::with_capacity(devices.len());
    for device in devices {
        let load = std::time::Instant::now();
        let arm = Arm::open(
            model,
            &ArmOptions {
                device: *device,
                ..options.clone()
            },
        )
        .with_context(|| {
            format!(
                "opening the ONNX embedder on {}. Is ORT_DYLIB_PATH set?",
                device.label()
            )
        })?;
        eprintln!(
            "  session ready on {} in {:.1}s",
            device.label(),
            load.elapsed().as_secs_f64()
        );
        embedders.push(arm);
    }
    Ok(embedders)
}

/// How many whole vectors the vector file already holds.
///
/// **Rounded down, and the file is then truncated to that count.** A run
/// interrupted mid-write leaves a partial trailing vector, and a resume that
/// trusted it would pair every later chunk with the wrong record - so the
/// partial one is discarded and redone.
///
/// @param vectors_path - the append-only vector file
/// @param bytes_per - one vector's width in bytes
fn vectors_already_written(vectors_path: &Path, bytes_per: usize) -> Result<usize> {
    let done = match std::fs::metadata(vectors_path) {
        Ok(m) => (m.len() as usize) / bytes_per,
        Err(_) => 0,
    };
    if done > 0 {
        eprintln!(
            "resuming: {done} vectors already written to {}",
            vectors_path.display()
        );
        let file = std::fs::OpenOptions::new().write(true).open(vectors_path)?;
        file.set_len((done * bytes_per) as u64)?;
    }
    Ok(done)
}

/// Embeds the corpus one window at a time and appends every vector in corpus
/// order.
///
/// **Corpus order is the invariant.** Position N in the vector file is chunk N
/// whichever device ran it, which is what the resume above depends on and what
/// makes every vector belong to the right chunk.
///
/// @param embedders - one arm per device
/// @param chunks - the corpus, in corpus order
/// @param out - the append-only vector file
/// @param done - the chunk to start at, from a previous run
/// @param report_every - chunks between progress lines
/// @param window - chunks handed to the devices between synchronisations
/// @param dims - the width every vector has to have
fn write_every_window(
    embedders: &[Arm],
    chunks: &[SynthChunk],
    out: &mut std::fs::File,
    done: usize,
    report_every: usize,
    window: usize,
    dims: usize,
) -> Result<()> {
    let start = std::time::Instant::now();
    let mut position = done;
    let mut since_report = 0usize;
    while position < chunks.len() {
        let end = (position + window).min(chunks.len());
        let texts: Vec<String> = chunks
            .get(position..end)
            .with_context(|| format!("chunks {position} to {end} of {}", chunks.len()))?
            .iter()
            .map(|c| sanitize_for_model(&c.content))
            .collect();
        let vectors = embed_window(embedders, &texts)
            .with_context(|| format!("embedding chunks {position} to {end}"))?;
        anyhow::ensure!(
            vectors.len() == texts.len(),
            "the embedder returned {} vectors for {} texts",
            vectors.len(),
            texts.len()
        );
        for v in &vectors {
            anyhow::ensure!(
                v.len() == dims,
                "the embedder returned {} dimensions",
                v.len()
            );
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
    Ok(())
}

/// How many chunks of the whole corpus hit the model's token bound.
///
/// **Summed across every session, plus a recount of whatever an earlier run
/// had already embedded.** Without the recount the share the card prints would
/// describe the tail this process happened to reach rather than the corpus, and
/// a resumed run that embedded its last hundred chunks would publish a
/// truncation share measured over a hundred chunks.
///
/// @param embedders - one arm per device, each holding its own counts
/// @param chunks - the corpus, in corpus order
/// @param done - how many chunks an earlier run had already embedded
/// @param model_dir - the model directory, for the tokenizer the recount uses
/// @param manifest - the model's manifest, which states the token bound
fn truncation_over_the_whole_corpus(
    embedders: &[Arm],
    chunks: &[SynthChunk],
    done: usize,
    model_dir: &str,
    manifest: &ModelManifest,
) -> Result<usize> {
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
        let embedded = chunks.get(..done).unwrap_or(chunks);
        truncated += crate::truncation::count_truncated(model_dir, manifest, embedded)?;
    }
    Ok(truncated)
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
    if let [only] = embedders {
        return only.embed_documents(texts);
    }
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(texts.get(i).map(String::len).unwrap_or(0)));
    let mut assigned: Vec<Vec<usize>> = vec![Vec::new(); embedders.len()];
    let mut load: Vec<usize> = vec![0; embedders.len()];
    for i in order {
        let lightest = load
            .iter()
            .enumerate()
            .min_by_key(|(_, bytes)| **bytes)
            .map(|(d, _)| d)
            .unwrap_or(0);
        // `lightest` is an index into `load`, which has one entry per
        // embedder, and `assigned` was built the same length.
        if let Some(bytes) = load.get_mut(lightest) {
            *bytes += texts.get(i).map(String::len).unwrap_or(0);
        }
        if let Some(slice) = assigned.get_mut(lightest) {
            slice.push(i);
        }
    }
    // Back into corpus order within each device, so the length sorting the embedder
    // does for batching starts from the same arrangement a single device would see.
    for slice in assigned.iter_mut() {
        slice.sort_unstable();
    }

    let batches: Vec<Vec<String>> = assigned
        .iter()
        .map(|ids| {
            ids.iter()
                .filter_map(|&i| texts.get(i).cloned())
                .collect::<Vec<String>>()
        })
        .collect();
    let results: Vec<Result<Vec<Vec<f32>>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = batches
            .iter()
            .zip(embedders.iter())
            .map(|(batch, embedder)| scope.spawn(move || embedder.embed_documents(batch)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("an embedding thread panicked")))
            })
            .collect()
    });

    let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
    for (ids, result) in assigned.iter().zip(results) {
        let vectors = result?;
        anyhow::ensure!(
            vectors.len() == ids.len(),
            "a device returned {} vectors for {} texts",
            vectors.len(),
            ids.len()
        );
        for (&i, v) in ids.iter().zip(vectors) {
            let slot = out
                .get_mut(i)
                .with_context(|| format!("text {i} of the {} in this window", texts.len()))?;
            *slot = v;
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
        // `chunks_exact(4)` yields slices of exactly four bytes, so the
        // array conversion is the same fact stated where it can be checked.
        let v: Vec<f32> = buf
            .chunks_exact(4)
            .filter_map(|b| <[u8; 4]>::try_from(b).ok())
            .map(f32::from_le_bytes)
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
    let corpus = crate::corpus::Corpus {
        chunks: inputs,
        vectors,
        dims,
        header,
    };
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
// The two halves that answer a question of their own.
// ---------------------------------------------------------------------------

/// Checking the generated corpus before paying for the embedding run.
mod check;
/// Loading the generated corpus into PostgreSQL for the pgvector baseline.
///
/// The only part of this module that talks to a database, and the schema it
/// writes is the one `engine.rs`'s baseline SQL was written against.
mod postgres;

pub use check::check;
pub use postgres::load_postgres;

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
            assert!(
                n > 400 && n < 1200,
                "chunk of {n} characters is not near 800"
            );
        }
    }

    #[test]
    fn prose_chunking_does_not_split_inside_a_word() {
        let text =
            "Alpha beta gamma. Delta epsilon zeta. Eta theta iota. Kappa lambda mu. ".repeat(40);
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
            .map(|i| {
                format!("fn handler_{i}(request: Request) -> Response {{ dispatch(request) }}")
            })
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
        let headings = vec![
            "Why is this slow?".to_string(),
            "What about the cache?".to_string(),
        ];
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
                assert!(
                    headings.contains(element),
                    "invented a heading: {element:?}"
                );
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
            vec![
                "First", "First", "First", "Second", "Second", "Second", "Third", "Third", "Third"
            ]
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
            let got = allocated
                .get(plan.name)
                .expect("an article source was not allocated");
            assert_eq!(
                got.len(),
                plan.documents,
                "{} received {} articles for {} documents",
                plan.name,
                got.len(),
                plan.documents
            );
            let have: usize = got.iter().map(|a| a.text.len()).sum();
            let need =
                (plan.chunks as f64 * plan.mean_chars as f64 / expansion(plan.material)) as usize;
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
            assert!(
                (ratio - 0.5).abs() < 0.01,
                "{} scaled to {ratio}",
                part.name
            );
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
        assert!(
            share > 0.03 && share < 0.15,
            "{share} of chunks carried an identifier"
        );
    }

    #[test]
    fn ticket_keys_look_like_a_tracker_wrote_them() {
        let mut rng = StdRng::seed_from_u64(5);
        for _ in 0..200 {
            let key = ticket_key(&mut rng);
            let (project, number) = key.split_once('-').expect("a ticket key has a dash");
            assert!(PROJECTS.contains(&project), "unknown project {project}");
            let n: u32 = number
                .parse()
                .expect("the tail of a ticket key is a number");
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
        let actual = breadcrumbed("A document title", &headings[..1], "")
            .chars()
            .count();
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
