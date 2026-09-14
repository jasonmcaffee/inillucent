//! Term normalization built to match PostgreSQL's `english` text search
//! configuration, so the lexical comparison measures ranking rather than
//! tokenization.
//!
//! The live `rag` database confirms `english` stems with `english_stem`, which is
//! the Snowball English stemmer: `to_tsvector('english', 'eligibility eligible
//! offers offering redeemed redeeming')` yields `'elig':1,2 'offer':3,4
//! 'redeem':5,6`. `rust-stemmers` implements the same Snowball algorithm, so the
//! two agree term for term.
//!
//! Invariant: **the same text produces the same terms on the way in and on the
//! way out.** This is the one function the index and the query both call,
//! because a corpus stemmed one way and a query stemmed another is a search
//! that silently finds nothing.

use rust_stemmers::{Algorithm, Stemmer};

/// PostgreSQL's English stopword list, from `share/tsearch_data/english.stop`.
/// Reproduced rather than referenced because term parity with the baseline is the
/// point, and a different stopword list would shift every score.
pub const ENGLISH_STOPWORDS: &[&str] = &[
    "i",
    "me",
    "my",
    "myself",
    "we",
    "our",
    "ours",
    "ourselves",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
    "he",
    "him",
    "his",
    "himself",
    "she",
    "her",
    "hers",
    "herself",
    "it",
    "its",
    "itself",
    "they",
    "them",
    "their",
    "theirs",
    "themselves",
    "what",
    "which",
    "who",
    "whom",
    "this",
    "that",
    "these",
    "those",
    "am",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "have",
    "has",
    "had",
    "having",
    "do",
    "does",
    "did",
    "doing",
    "a",
    "an",
    "the",
    "and",
    "but",
    "if",
    "or",
    "because",
    "as",
    "until",
    "while",
    "of",
    "at",
    "by",
    "for",
    "with",
    "about",
    "against",
    "between",
    "into",
    "through",
    "during",
    "before",
    "after",
    "above",
    "below",
    "to",
    "from",
    "up",
    "down",
    "in",
    "out",
    "on",
    "off",
    "over",
    "under",
    "again",
    "further",
    "then",
    "once",
    "here",
    "there",
    "when",
    "where",
    "why",
    "how",
    "all",
    "any",
    "both",
    "each",
    "few",
    "more",
    "most",
    "other",
    "some",
    "such",
    "no",
    "nor",
    "not",
    "only",
    "own",
    "same",
    "so",
    "than",
    "too",
    "very",
    "s",
    "t",
    "can",
    "will",
    "just",
    "don",
    "should",
    "now",
];

/// The one normalizer both indexing and searching call.
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
            // An address is recognised before the general compound rule, and is
            // never stemmed. `compound_identifier` would keep
            // `dana@3rivers.example.com` whole by accident, because its domain happens
            // to hold a digit, and shatter `whitmore@example.com`, whose local
            // part would then be stemmed into `jasonlmcaffe` - a string that
            // appears nowhere. Half a mailbox's addresses indexing as themselves
            // and half dissolving is worse than either.
            match email_address(word) {
                Some(address) => out.push(address),
                None => {
                    if let Some(compound) = compound_identifier(word) {
                        out.push(compound);
                    }
                }
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

/// The whole address, lowercased, when a word is one.
///
/// PostgreSQL's `english` configuration has an `email` token type and keeps a
/// whole address as one unstemmed term:
///
/// ```text
/// to_tsvector('english','dana@3rivers.example.com whitmore@example.com')
///   => 'dana@3rivers.example.com':1 'whitmore@example.com':2
/// ```
///
/// The shape is deliberately narrow - one `@`, a non-empty local part, and a
/// domain of at least two dot-separated labels - because the point is to
/// recognise addresses, not to keep every word that happens to contain an at
/// sign. The parts are still emitted and still stemmed by the caller, so a query
/// for a bare local part or a domain matches exactly as it did before; what is
/// new is that the address also survives as itself.
/// @param word - one whitespace-separated word, possibly punctuated
fn email_address(word: &str) -> Option<String> {
    let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric());
    if trimmed.len() < 5 || trimmed.len() > 64 {
        return None;
    }
    let (local, domain) = trimmed.split_once('@')?;
    if domain.contains('@') || local.is_empty() {
        return None;
    }
    let ok_local = local
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '\''));
    if !ok_local || !local.chars().any(|c| c.is_alphanumeric()) {
        return None;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return None;
    }
    let ok_domain = domain
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-'));
    // A top level label of letters, which is what separates an address from a
    // version string or a filename that happens to carry an at sign.
    let tld_is_alphabetic = labels
        .last()
        .is_some_and(|l| l.len() >= 2 && l.chars().all(|c| c.is_alphabetic()));
    if !ok_domain || !tld_is_alphabetic {
        return None;
    }
    Some(trimmed.to_lowercase())
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
        assert_eq!(
            t.terms("OFFERS Offers offers"),
            vec!["offer", "offer", "offer"]
        );
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
        assert!(t
            .terms("src/search/vector")
            .contains(&"src/search/vector".to_string()));
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
    fn an_email_address_survives_whole_and_unstemmed() {
        let t = Tokenizer::default();
        // The measured failure: this address used to shatter into
        // `["whitmor", "exampl", "com"]`, so a search for one person's address
        // quietly matched every message mentioning the provider. The address is
        // now a term, and it is not put through the stemmer.
        let terms = t.terms("whitmore@example.com");
        assert!(
            terms.contains(&"whitmore@example.com".to_string()),
            "got {terms:?}"
        );
        assert!(
            !terms.contains(&"whitmor@example.com".to_string()),
            "the address itself was stemmed: {terms:?}"
        );
    }

    #[test]
    fn every_address_shape_is_kept_whole_not_only_the_lucky_ones() {
        let t = Tokenizer::default();
        // `dana@3rivers.example.com` used to survive only because its domain holds a
        // digit, which made the general compound rule fire. Both must survive now.
        for address in [
            "whitmore@example.com",
            "dana@3rivers.example.com",
            "Terri.Shaw@example.org",
            "first+tag@sub.example.co.uk",
        ] {
            let terms = t.terms(address);
            assert!(
                terms.contains(&address.to_lowercase()),
                "{address} did not survive: {terms:?}"
            );
        }
    }

    #[test]
    fn an_address_is_still_split_so_a_part_query_matches() {
        let t = Tokenizer::default();
        let terms = t.terms("whitmore@example.com");
        assert!(terms.contains(&"exampl".to_string()), "got {terms:?}");
        assert!(terms.contains(&"com".to_string()));
    }

    #[test]
    fn surrounding_punctuation_does_not_change_the_address() {
        let t = Tokenizer::default();
        for form in [
            "<jason@example.com>",
            "jason@example.com,",
            "(jason@example.com)",
        ] {
            assert!(
                t.terms(form).contains(&"jason@example.com".to_string()),
                "{form} did not produce the address"
            );
        }
    }

    #[test]
    fn a_word_carrying_an_at_sign_is_not_an_address() {
        // Tested against the recogniser rather than the whole tokenizer, because
        // the general compound rule may still keep some of these whole; what
        // matters is that none of them is treated as an address and exempted from
        // stemming.
        for word in [
            "@mentions",
            "cost@10",
            "a@b",
            "x@y.1",
            "two@@ats.com",
            "user@localhost",
        ] {
            assert_eq!(
                email_address(word),
                None,
                "{word} was mistaken for an address"
            );
        }
    }

    #[test]
    fn a_bare_local_part_still_matches_through_the_split_terms() {
        let t = Tokenizer::default();
        // The parts are still emitted and still stemmed, so the two sides agree:
        // a document holding the address and a query holding only the local part
        // both produce the same stem.
        let document = t.terms("From: whitmore@example.com");
        let query = t.query_terms("whitmore");
        assert_eq!(query.len(), 1);
        assert!(
            document.contains(&query[0]),
            "{document:?} does not hold {query:?}"
        );
    }

    #[test]
    fn an_address_is_emitted_once_rather_than_twice() {
        let t = Tokenizer::default();
        // The compound rule would also have kept this one, so the address rule has
        // to replace it rather than run beside it, or the term is double counted
        // and its term frequency is wrong.
        let terms = t.terms("dana@3rivers.example.com");
        let count = terms
            .iter()
            .filter(|x| *x == "dana@3rivers.example.com")
            .count();
        assert_eq!(count, 1, "got {terms:?}");
    }

    #[test]
    fn absurdly_long_runs_are_skipped() {
        let t = Tokenizer::default();
        let blob = "a".repeat(200);
        assert!(t.terms(&blob).is_empty());
    }
}
