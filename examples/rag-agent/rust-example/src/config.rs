//! The settings that decide how documents become chunks and vectors.
//!
//! Every setting here changes what ends up in the index. That is why
//! [`IndexSettings::fingerprint_salt`] exists: the salt goes into every
//! document's fingerprint, so changing the chunk size or the context mode makes
//! the next sync re-embed every document. Without it, a database would keep
//! vectors built by the old settings next to text cut by the new ones, and every
//! search would still return rows.

use std::time::Duration;

/// The embedding model `inillucent setup-embeddings all` installs.
pub const MODEL_NAME: &str = "nomic-embed-text-v1.5";

/// How many numbers the model returns for one text.
pub const DIMENSIONS: usize = 768;

/// The prefix the model was trained with on stored text.
///
/// `nomic-embed-text-v1.5` expects `search_document: ` in front of a passage
/// and `search_query: ` in front of a question. A text without its prefix still
/// gets a vector, and searches still return rows. The rows are worse matches,
/// and nothing reports it.
pub const DOCUMENT_PREFIX: &str = "search_document: ";

/// The prefix the model was trained with on questions.
pub const QUERY_PREFIX: &str = "search_query: ";

/// How a document is cut into chunks. All lengths are in bytes of UTF-8 text.
#[derive(Clone, Debug, PartialEq)]
pub struct ChunkSettings {
    /// The length a chunk is packed up to, in whole sentences.
    pub target_chars: usize,
    /// How much of the end of one chunk the next chunk repeats.
    pub overlap_chars: usize,
    /// No chunk is longer than this. A longer sentence is cut at spaces.
    pub max_chars: usize,
    /// A last chunk with less new text than this is joined to the one before.
    pub min_chars: usize,
}

impl Default for ChunkSettings {
    /// About 250 tokens a chunk, with about 20% overlap.
    ///
    /// 1,000 characters is inside the 512 token range that published chunking
    /// studies report as the usual best default, and close to the 1,022
    /// character average of the command line example, so the two examples can
    /// be compared.
    fn default() -> Self {
        ChunkSettings { target_chars: 1000, overlap_chars: 200, max_chars: 2000, min_chars: 250 }
    }
}

/// What is written in front of each chunk before it is embedded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ContextMode {
    /// The document title only: `Heraclitus: <chunk>`.
    Title,
    /// The title and the document's first sentence, which on Wikipedia says
    /// who or what the article is about.
    Lead,
}

impl ContextMode {
    /// Returns the name used in fingerprints and reports.
    pub fn name(self) -> &'static str {
        match self {
            ContextMode::Title => "title",
            ContextMode::Lead => "lead",
        }
    }
}

/// Everything that decides the contents of the index.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexSettings {
    /// How documents are cut into chunks.
    pub chunking: ChunkSettings,
    /// What goes in front of each chunk before it is embedded.
    pub context: ContextMode,
}

impl IndexSettings {
    /// Returns the text mixed into every document fingerprint.
    ///
    /// The model name is in it too. A different model makes vectors that cannot
    /// be compared with the stored ones, so a model change has to re-embed
    /// everything.
    pub fn fingerprint_salt(&self) -> String {
        let c = &self.chunking;
        format!(
            "model={MODEL_NAME};target={};overlap={};max={};min={};context={};chunker=2",
            c.target_chars,
            c.overlap_chars,
            c.max_chars,
            c.min_chars,
            self.context.name()
        )
    }
}

/// Reads a duration written as `90s`, `15m`, `2h` or `off`.
///
/// `off` and `0` mean the server never syncs on a timer. It still syncs once at
/// start and whenever the `sync_now` tool asks.
///
/// @param text - the value given on the command line
pub fn parse_interval(text: &str) -> Result<Option<Duration>, String> {
    let text = text.trim();
    if text == "off" || text == "0" {
        return Ok(None);
    }
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let count: u64 = number.parse().map_err(|_| format!("`{text}` is not a duration such as 90s, 15m or 2h"))?;
    let seconds = match unit {
        "" | "s" => count,
        "m" => count * 60,
        "h" => count * 3600,
        _ => return Err(format!("`{text}` has the unit `{unit}`; use s, m or h")),
    };
    Ok(Some(Duration::from_secs(seconds)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_read_in_seconds_minutes_and_hours() {
        assert_eq!(parse_interval("90s").unwrap(), Some(Duration::from_secs(90)));
        assert_eq!(parse_interval("15m").unwrap(), Some(Duration::from_secs(900)));
        assert_eq!(parse_interval("2h").unwrap(), Some(Duration::from_secs(7200)));
        assert_eq!(parse_interval("off").unwrap(), None);
        assert!(parse_interval("soon").is_err());
    }

    #[test]
    fn a_different_setting_makes_a_different_salt() {
        let one = IndexSettings { chunking: ChunkSettings::default(), context: ContextMode::Title };
        let two = IndexSettings { context: ContextMode::Lead, ..one.clone() };
        assert_ne!(one.fingerprint_salt(), two.fingerprint_salt());
    }
}
