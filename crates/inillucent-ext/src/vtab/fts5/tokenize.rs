//! Turning text into the tokens an index is built from.
//!
//! Invariant: the same text produces the same tokens on the way in and on the
//! way out. That sounds obvious and is the single most common way a full-text
//! index goes wrong: a document indexed with one rule and searched with another
//! finds nothing, and nothing about the failure says why. So indexing and
//! searching call this and only this.
//!
//! Two tokenizers, which are the two the pinned release has by default.
//! `ascii` treats every byte outside `A-Za-z0-9` as a separator and folds
//! `A-Z`; `unicode61` treats every character outside the Unicode letter and
//! number categories as a separator, folds case, and strips the combining marks
//! that make `é` and `e` the same word. `unicode61` is the default, as it is
//! there.

/// Which tokenizer an index uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tokenizer {
    /// `ascii`: bytes outside `A-Za-z0-9` separate, `A-Z` folds.
    Ascii,
    /// `porter`: another tokenizer's tokens, each reduced to its stem.
    ///
    /// **A wrapper, which is what it is in SQLite too**: `tokenize='porter'`
    /// stems `unicode61`'s tokens and `tokenize='porter ascii'` stems `ascii`'s.
    /// It is what makes a search for `run` find `running`, and it was accepted
    /// and read as `unicode61` - a declaration the application trusts, doing
    /// nothing, which is the same shape `CHECK` and `STRICT` were in before
    /// each was enforced.
    ///
    /// The stemmer is Snowball English, which is Porter2 rather than the
    /// original Porter that SQLite ships. They agree on the ordinary
    /// inflections - `running`, `runs` and `run` all reduce to `run` in both -
    /// and differ on a handful of rare words; `docs/feature-comparison.md` says so.
    Porter(Box<Tokenizer>),
    /// `unicode61`: Unicode letters and numbers are tokens.
    Unicode61 {
        /// Whether diacritics are removed, which `remove_diacritics` decides.
        remove_diacritics: bool,
        /// Characters that are part of a token although their category is not.
        extra_tokens: Vec<char>,
        /// Characters that separate although their category says otherwise.
        separators: Vec<char>,
    },
}

impl Tokenizer {
    /// Returns the tokenizer a specification names.
    ///
    /// An unrecognised name is read as `unicode61`, which is what the pinned
    /// release falls back to for its own built-ins and is the safe direction:
    /// a schema naming a tokenizer this build has not got still opens, and its
    /// rows are still found by the words they contain.
    pub fn named(specification: &[Vec<u8>]) -> Tokenizer {
        let name = specification
            .first()
            .map(|part| String::from_utf8_lossy(part).to_ascii_lowercase())
            .unwrap_or_default();
        if name == "porter" {
            // Whatever follows `porter` is the tokenizer it wraps, and nothing
            // following it means the default one.
            let inner = specification.get(1..).unwrap_or(&[]);
            return Tokenizer::Porter(Box::new(Tokenizer::named(inner)));
        }
        if name == "ascii" {
            return Tokenizer::Ascii;
        }
        let mut remove_diacritics = true;
        let mut extra_tokens = Vec::new();
        let mut separators = Vec::new();
        let mut arguments = specification.iter().skip(1);
        while let Some(key) = arguments.next() {
            let key = String::from_utf8_lossy(key).to_ascii_lowercase();
            let Some(value) = arguments.next() else {
                break;
            };
            let value = String::from_utf8_lossy(value).into_owned();
            match key.as_str() {
                "remove_diacritics" => remove_diacritics = value.trim() != "0",
                "tokenchars" => extra_tokens.extend(value.chars()),
                "separators" => separators.extend(value.chars()),
                _ => {}
            }
        }
        Tokenizer::Unicode61 {
            remove_diacritics,
            extra_tokens,
            separators,
        }
    }

    /// Returns the tokens of one piece of text with where each one came from.
    ///
    /// **Byte offsets into the original text**, which is what `highlight()` and
    /// `snippet()` need and what `tokens` throws away: a token is folded, and
    /// case-folding, diacritic removal and stemming all change its length, so
    /// there is no way to find a token's bytes again by searching for it. The
    /// offsets are recorded while the characters are being read, which is the
    /// one place they are known.
    ///
    /// The Porter tokenizer stems, and a stem is not a substring of the word it
    /// came from - so its spans are the *unstemmed* tokenizer's spans and its
    /// tokens are the stems, which is the pairing a highlight wants: find the
    /// word by its span, decide whether it matched by its stem.
    ///
    /// @param text - the column's bytes
    pub fn spans(&self, text: &[u8]) -> Vec<(Vec<u8>, usize, usize)> {
        if let Tokenizer::Porter(inner) = self {
            let stemmer = rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::English);
            return inner
                .spans(text)
                .into_iter()
                .map(|(token, start, end)| {
                    let word = String::from_utf8_lossy(&token).into_owned();
                    (stemmer.stem(&word).into_owned().into_bytes(), start, end)
                })
                .collect();
        }
        let text = String::from_utf8_lossy(text);
        let mut spans = Vec::new();
        let mut current = String::new();
        let mut start = 0usize;
        let mut at = 0usize;
        for character in text.chars() {
            let width = character.len_utf8();
            if self.is_token_character(character) {
                if current.is_empty() {
                    start = at;
                }
                self.fold_into(character, &mut current);
                at = at.saturating_add(width);
                continue;
            }
            if !current.is_empty() {
                spans.push((core::mem::take(&mut current).into_bytes(), start, at));
            }
            at = at.saturating_add(width);
        }
        if !current.is_empty() {
            spans.push((current.into_bytes(), start, at));
        }
        spans
    }

    /// Returns the tokens of one piece of text, in the order they appear.
    pub fn tokens(&self, text: &[u8]) -> Vec<Vec<u8>> {
        if let Tokenizer::Porter(inner) = self {
            let stemmer = rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::English);
            return inner
                .tokens(text)
                .into_iter()
                .map(|token| {
                    let word = String::from_utf8_lossy(&token).into_owned();
                    stemmer.stem(&word).into_owned().into_bytes()
                })
                .collect();
        }
        let text = String::from_utf8_lossy(text);
        let mut tokens = Vec::new();
        let mut current = String::new();
        for character in text.chars() {
            if self.is_token_character(character) {
                self.fold_into(character, &mut current);
                continue;
            }
            if !current.is_empty() {
                tokens.push(core::mem::take(&mut current).into_bytes());
            }
        }
        if !current.is_empty() {
            tokens.push(current.into_bytes());
        }
        tokens
    }

    /// Returns whether a character is part of a token.
    fn is_token_character(&self, character: char) -> bool {
        match self {
            // Unreachable: `tokens` answers a porter tokenizer from the
            // tokenizer it wraps and never walks the text itself.
            Tokenizer::Porter(inner) => inner.is_token_character(character),
            Tokenizer::Ascii => character.is_ascii_alphanumeric(),
            Tokenizer::Unicode61 {
                extra_tokens,
                separators,
                ..
            } => {
                if separators.contains(&character) {
                    return false;
                }
                if extra_tokens.contains(&character) {
                    return true;
                }
                character.is_alphanumeric()
            }
        }
    }

    /// Folds one character onto the end of the token being built.
    ///
    /// **Written into rather than returned**, because a `Vec<char>` per
    /// character is an allocation per character: the folded form of `a` is one
    /// character, and describing it cost a heap allocation and a free. The gate
    /// tokenises fifty-five thousand characters per round of
    /// `extension.fts.build` and spent a fifth of the workload's module time
    /// here.
    ///
    /// A character folds to more than one - the German sharp s lowercases to
    /// `ss` - so the shape has to stay one-to-many; it is the collection that
    /// went, not the generality.
    ///
    /// @param character - the character to fold
    /// @param out - the token being built
    fn fold_into(&self, character: char, out: &mut String) {
        match self {
            // Unreachable for the same reason as `is_token_character`: a porter
            // tokenizer stems what the tokenizer it wraps produced.
            Tokenizer::Porter(inner) => inner.fold_into(character, out),
            Tokenizer::Ascii => out.push(character.to_ascii_lowercase()),
            Tokenizer::Unicode61 {
                remove_diacritics, ..
            } => {
                for folded in character.to_lowercase() {
                    out.push(if *remove_diacritics {
                        strip_diacritic(folded)
                    } else {
                        folded
                    });
                }
            }
        }
    }
}

/// Returns a Latin character with its diacritic removed.
///
/// The table is the Latin-1 and Latin Extended-A range, which is what
/// `remove_diacritics=1` covers in practice and what makes `café` and `cafe`
/// the same word. Anything outside it is left alone rather than guessed at:
/// stripping a mark from a script whose marks change the word would be worse
/// than not stripping it.
fn strip_diacritic(character: char) -> char {
    const FOLDED: &str = "aaaaaaaceeeeiiiidnooooo\u{f7}ouuuuypy";
    let code = character as u32;
    if (0xe0..=0xff).contains(&code) {
        let index = (code - 0xe0) as usize;
        if let Some(replacement) = FOLDED.chars().nth(index) {
            return replacement;
        }
    }
    // Latin Extended-A alternates an accented letter with its plain form in
    // several runs; the ones that matter are handled by lower-casing first.
    match character {
        '\u{101}' | '\u{103}' | '\u{105}' => 'a',
        '\u{107}' | '\u{109}' | '\u{10b}' | '\u{10d}' => 'c',
        '\u{10f}' | '\u{111}' => 'd',
        '\u{113}' | '\u{115}' | '\u{117}' | '\u{119}' | '\u{11b}' => 'e',
        '\u{11d}' | '\u{11f}' | '\u{121}' | '\u{123}' => 'g',
        '\u{125}' | '\u{127}' => 'h',
        '\u{129}' | '\u{12b}' | '\u{12d}' | '\u{12f}' | '\u{131}' => 'i',
        '\u{135}' => 'j',
        '\u{137}' => 'k',
        '\u{13a}' | '\u{13c}' | '\u{13e}' | '\u{140}' | '\u{142}' => 'l',
        '\u{144}' | '\u{146}' | '\u{148}' => 'n',
        '\u{14d}' | '\u{14f}' | '\u{151}' => 'o',
        '\u{155}' | '\u{157}' | '\u{159}' => 'r',
        '\u{15b}' | '\u{15d}' | '\u{15f}' | '\u{161}' => 's',
        '\u{163}' | '\u{165}' | '\u{167}' => 't',
        '\u{169}' | '\u{16b}' | '\u{16d}' | '\u{16f}' | '\u{171}' | '\u{173}' => 'u',
        '\u{175}' => 'w',
        '\u{177}' | '\u{17a}' => 'y',
        '\u{17c}' | '\u{17e}' => 'z',
        other => other,
    }
}

/// Reads a `tokenize = '...'` specification into its words.
///
/// The value is a quoted string holding the tokenizer's name and its arguments,
/// each of which may itself be quoted. `tokenize = "unicode61 remove_diacritics
/// 0"` is three words, and `tokenize = "unicode61 tokenchars '_-'"` is four.
pub fn parse_specification(value: &str) -> Vec<Vec<u8>> {
    let words = split_words(strip_quotes(value.trim()));
    if words.is_empty() {
        return vec![b"unicode61".to_vec()];
    }
    words
}

/// Splits text into words, honouring quotes and their doubling.
///
/// A column argument is quoted *per identifier* - `"a b" UNINDEXED` is two
/// words - while a `tokenize` value is one quoted string holding several. The
/// difference is only whether the caller strips the outer quotes first, so the
/// scan is shared and the stripping is not.
pub fn split_words(value: &str) -> Vec<Vec<u8>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut quote: Option<char> = None;
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        match quote {
            Some(open) if character == open => {
                // A doubled quote is one quote, which is how SQL escapes it -
                // `'unicode61 tokenchars ''_-'''` is three words and the third
                // is `_-`.
                if characters.peek() == Some(&open) {
                    characters.next();
                    current.push(open);
                    continue;
                }
                quote = None;
                words.push(core::mem::take(&mut current).into_bytes());
                quoted = false;
            }
            Some(_) => current.push(character),
            None if character == '\'' || character == '"' || character == '`' => {
                if !current.is_empty() {
                    words.push(core::mem::take(&mut current).into_bytes());
                }
                quote = Some(character);
                quoted = true;
            }
            None if character.is_whitespace() => {
                if !current.is_empty() {
                    words.push(core::mem::take(&mut current).into_bytes());
                }
            }
            None => current.push(character),
        }
    }
    if !current.is_empty() || quoted {
        words.push(current.into_bytes());
    }
    words.retain(|word| !word.is_empty());
    words
}

/// Removes one layer of quotes from a value.
fn strip_quotes(text: &str) -> &str {
    let bytes = text.as_bytes();
    let (Some(first), Some(last)) = (bytes.first(), bytes.last()) else {
        return text;
    };
    let quoted = bytes.len() >= 2
        && matches!(
            (first, last),
            (b'"', b'"') | (b'\'', b'\'') | (b'`', b'`') | (b'[', b']')
        );
    if quoted {
        return text.get(1..text.len() - 1).unwrap_or(text);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders tokens as strings for a comparison.
    fn tokens(tokenizer: &Tokenizer, text: &str) -> Vec<String> {
        tokenizer
            .tokens(text.as_bytes())
            .into_iter()
            .map(|token| String::from_utf8_lossy(&token).into_owned())
            .collect()
    }

    /// The default splits on anything that is not a letter or a number.
    #[test]
    fn the_default_splits_on_punctuation() {
        let tokenizer = Tokenizer::named(&[]);
        assert_eq!(
            tokens(&tokenizer, "The quick, brown fox!"),
            ["the", "quick", "brown", "fox"]
        );
        assert_eq!(tokens(&tokenizer, "a1 b-2"), ["a1", "b", "2"]);
    }

    /// `ascii` treats everything outside `A-Za-z0-9` as a separator.
    #[test]
    fn ascii_keeps_to_ascii() {
        let tokenizer = Tokenizer::named(&[b"ascii".to_vec()]);
        assert_eq!(
            tokens(&tokenizer, "Caf\u{e9} au lait"),
            ["caf", "au", "lait"]
        );
    }

    /// The default removes diacritics, so an accented word is the plain one.
    #[test]
    fn diacritics_are_removed_by_default() {
        let tokenizer = Tokenizer::named(&[]);
        assert_eq!(tokens(&tokenizer, "Caf\u{e9}"), ["cafe"]);
        let kept = Tokenizer::named(&[
            b"unicode61".to_vec(),
            b"remove_diacritics".to_vec(),
            b"0".to_vec(),
        ]);
        assert_eq!(tokens(&kept, "Caf\u{e9}"), ["caf\u{e9}"]);
    }

    /// `tokenchars` adds characters and `separators` removes them.
    #[test]
    fn the_character_lists_are_honoured() {
        let joined =
            Tokenizer::named(&[b"unicode61".to_vec(), b"tokenchars".to_vec(), b"-".to_vec()]);
        assert_eq!(tokens(&joined, "well-known"), ["well-known"]);
        let split =
            Tokenizer::named(&[b"unicode61".to_vec(), b"separators".to_vec(), b"x".to_vec()]);
        assert_eq!(tokens(&split, "axb"), ["a", "b"]);
    }

    /// A specification splits into words, quotes and all.
    #[test]
    fn a_specification_splits_into_words() {
        assert_eq!(parse_specification("'ascii'"), vec![b"ascii".to_vec()]);
        assert_eq!(
            parse_specification("\"unicode61 remove_diacritics 0\""),
            vec![
                b"unicode61".to_vec(),
                b"remove_diacritics".to_vec(),
                b"0".to_vec()
            ]
        );
        assert_eq!(
            parse_specification("'unicode61 tokenchars ''_-'''"),
            vec![
                b"unicode61".to_vec(),
                b"tokenchars".to_vec(),
                b"_-".to_vec(),
            ]
        );
    }

    /// An empty specification is the default tokenizer.
    #[test]
    fn an_empty_specification_is_the_default() {
        assert_eq!(parse_specification(""), vec![b"unicode61".to_vec()]);
        assert!(matches!(
            Tokenizer::named(&parse_specification("")),
            Tokenizer::Unicode61 { .. }
        ));
    }
}
