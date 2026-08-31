//! Term normalization built to match PostgreSQL's `english` text search
//! configuration, so the lexical comparison measures ranking rather than
//! tokenization.
//!
//! The live `rag` database confirms `english` stems with `english_stem`, which is
//! the Snowball English stemmer: `to_tsvector('english', 'eligibility eligible
//! offers offering redeemed redeeming')` yields `'elig':1,2 'offer':3,4
//! 'redeem':5,6`. `rust-stemmers` implements the same Snowball algorithm, so the
//! two agree term for term.

use rust_stemmers::{Algorithm, Stemmer};

/// PostgreSQL's English stopword list, from `share/tsearch_data/english.stop`.
/// Reproduced rather than referenced because term parity with the baseline is the
/// point, and a different stopword list would shift every score.
pub const ENGLISH_STOPWORDS: &[&str] = &[
    "i", "me", "my", "myself", "we", "our", "ours", "ourselves", "you", "your", "yours",
    "yourself", "yourselves", "he", "him", "his", "himself", "she", "her", "hers", "herself",
    "it", "its", "itself", "they", "them", "their", "theirs", "themselves", "what", "which",
    "who", "whom", "this", "that", "these", "those", "am", "is", "are", "was", "were", "be",
    "been", "being", "have", "has", "had", "having", "do", "does", "did", "doing", "a", "an",
    "the", "and", "but", "if", "or", "because", "as", "until", "while", "of", "at", "by",
    "for", "with", "about", "against", "between", "into", "through", "during", "before",
    "after", "above", "below", "to", "from", "up", "down", "in", "out", "on", "off", "over",
    "under", "again", "further", "then", "once", "here", "there", "when", "where", "why",
    "how", "all", "any", "both", "each", "few", "more", "most", "other", "some", "such",
    "no", "nor", "not", "only", "own", "same", "so", "than", "too", "very", "s", "t", "can",
    "will", "just", "don", "should", "now",
];

pub struct Tokenizer {
    stemmer: Stemmer,
    stopwords: std::collections::HashSet<&'static str>,
}

impl Default for Tokenizer {
    fn default() -> Self {
        Tokenizer {
            stemmer: Stemmer::create(Algorithm::English),
            stopwords: ENGLISH_STOPWORDS.iter().copied().collect(),
        }
    }
}

impl Tokenizer {
    /// Split on anything that is not alphanumeric, lowercase, drop stopwords,
    /// then stem. Identifiers such as `author_id` are also emitted split into
    /// their parts, because a query for `author` should match them.
    ///
    /// A compound identifier is additionally emitted whole. This is a deliberate
    /// departure from PostgreSQL rather than an oversight: `to_tsvector('english',
    /// 'ENX-1932')` yields `'enx':1 '-1932':2`, so Postgres destroys the ticket
    /// key, and `ts_debug` confirms it splits on the hyphen. Both engines then
    /// score badly on the queries this corpus is full of, JIRA keys, function
    /// names and file paths. Keeping the compound as its own term is the whole
    /// reason for building a specialized engine, and the score card measures what
    /// it buys instead of assuming it.
    pub fn terms(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for word in text.split_whitespace() {
            if let Some(compound) = compound_identifier(word) {
                out.push(compound);
            }
            for raw in word.split(|c: char| !c.is_alphanumeric()) {
                if raw.is_empty() {
                    continue;
                }
                let lower = raw.to_lowercase();
                if self.stopwords.contains(lower.as_str()) {
                    continue;
                }
                // A very long run of characters is almost always a base64 blob or
                // a hash rather than a word; indexing it costs a dictionary entry
                // and buys nothing.
                if lower.len() > 64 {
                    continue;
                }
                out.push(self.stemmer.stem(&lower).to_string());
            }
        }
        out
    }

    /// Query terms, which are normalized identically. Kept as a separate entry
    /// point because a query keeps duplicate terms (repeating a term is a signal
    /// of intent) while the document side collapses them into a frequency.
    pub fn query_terms(&self, text: &str) -> Vec<String> {
        self.terms(text)
    }
}

/// The whole identifier, when a word looks like one: it carries an internal
/// separator and at least one letter and one digit, or it is a dotted or slashed
/// path. Surrounding punctuation is trimmed so `(ENX-1932)` and `ENX-1932,` both
/// produce the same term. Returns `None` for ordinary hyphenated English, which
/// would otherwise fill the dictionary with terms nobody searches for.
fn compound_identifier(word: &str) -> Option<String> {
    let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric());
    if trimmed.len() < 4 || trimmed.len() > 64 {
        return None;
    }
    let has_separator = trimmed
        .chars()
        .any(|c| c == '-' || c == '_' || c == '.' || c == '/' || c == ':');
    if !has_separator {
        return None;
    }
    let has_alpha = trimmed.chars().any(|c| c.is_alphabetic());
    let has_digit = trimmed.chars().any(|c| c.is_ascii_digit());
    let separators = trimmed
        .chars()
        .filter(|c| *c == '-' || *c == '_' || *c == '.' || *c == '/' || *c == ':')
        .count();
    // A letter and a digit together reads as an identifier (ENX-1932, v1.5.2).
    // So does more than one separator (a.b.c, path/to/file, snake_case_name).
    // A single hyphen between two plain words does not, so "well-known" and
    // "long-running" stay out of the dictionary.
    let looks_like_identifier = (has_alpha && has_digit) || separators > 1 || trimmed.contains('_');
    if !looks_like_identifier {
        return None;
    }
    Some(trimmed.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_the_way_postgres_english_stems() {
        let t = Tokenizer::default();
        // Verified against the live database:
        //   to_tsvector('english','eligibility eligible offers offering redeemed redeeming')
        //   => 'elig':1,2 'offer':3,4 'redeem':5,6
        assert_eq!(
            t.terms("eligibility eligible offers offering redeemed redeeming"),
            vec!["elig", "elig", "offer", "offer", "redeem", "redeem"]
        );
    }

    #[test]
    fn drops_stopwords() {
        let t = Tokenizer::default();
        assert_eq!(t.terms("how does the offer work"), vec!["offer", "work"]);
    }

    #[test]
    fn splits_on_punctuation_and_underscores() {
        let t = Tokenizer::default();
        // `author_id` also yields the whole identifier, because an underscore
        // marks a name rather than ordinary prose. `space-key` does not: a single
        // hyphen between two plain words is English, not an identifier.
        assert_eq!(
            t.terms("author_id, space-key"),
            vec!["author_id", "author", "id", "space", "key"]
        );
    }

    #[test]
    fn lowercases_before_stemming() {
        let t = Tokenizer::default();
        assert_eq!(t.terms("OFFERS Offers offers"), vec!["offer", "offer", "offer"]);
    }

    #[test]
    fn a_ticket_key_is_kept_whole_as_well_as_split() {
        let t = Tokenizer::default();
        // Postgres yields only 'enx' and '-1932' here, losing the key itself.
        let terms = t.terms("ENX-1932");
        assert!(terms.contains(&"enx-1932".to_string()), "got {terms:?}");
        assert!(terms.contains(&"enx".to_string()));
        assert!(terms.contains(&"1932".to_string()));
    }

    #[test]
    fn surrounding_punctuation_does_not_change_the_identifier() {
        let t = Tokenizer::default();
        for form in ["ENX-1932", "(ENX-1932)", "ENX-1932,", "[ENX-1932]."] {
            assert!(
                t.terms(form).contains(&"enx-1932".to_string()),
                "{form} did not produce the identifier"
            );
        }
    }

    #[test]
    fn snake_case_and_dotted_and_slashed_names_are_kept_whole() {
        let t = Tokenizer::default();
        assert!(t.terms("author_id").contains(&"author_id".to_string()));
        assert!(t.terms("v1.5.2").contains(&"v1.5.2".to_string()));
        assert!(t.terms("src/search/vector").contains(&"src/search/vector".to_string()));
    }

    #[test]
    fn ordinary_hyphenated_english_is_not_treated_as_an_identifier() {
        let t = Tokenizer::default();
        // Otherwise the dictionary fills with compounds nobody searches for.
        for word in ["well-known", "long-running", "so-called"] {
            let terms = t.terms(word);
            assert!(
                !terms.iter().any(|x| x.contains('-')),
                "{word} produced a compound term: {terms:?}"
            );
        }
    }

    #[test]
    fn splitting_still_happens_so_a_part_query_matches() {
        let t = Tokenizer::default();
        let terms = t.terms("getUserProfile author_id");
        assert!(terms.contains(&"author".to_string()));
        assert!(terms.contains(&"id".to_string()));
    }

    #[test]
    fn empty_and_punctuation_only_input_yields_no_terms() {
        let t = Tokenizer::default();
        assert!(t.terms("").is_empty());
        assert!(t.terms("--- ... ,,,").is_empty());
    }

    #[test]
    fn a_query_of_only_stopwords_yields_no_terms() {
        let t = Tokenizer::default();
        assert!(t.query_terms("how do i do the of and").is_empty());
    }

    #[test]
    fn absurdly_long_runs_are_skipped() {
        let t = Tokenizer::default();
        let blob = "a".repeat(200);
        assert!(t.terms(&blob).is_empty());
    }
}
