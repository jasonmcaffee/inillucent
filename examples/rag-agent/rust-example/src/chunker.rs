//! Cuts a document into overlapping chunks of whole sentences.
//!
//! A chunk is the unit that gets one vector and that a search returns. Three
//! rules decide where the cuts go:
//!
//! 1. **Cut between sentences.** A chunk that stops half way through a
//!    sentence gets a vector for half an idea, and the model reading it later
//!    sees a fragment. Only a sentence longer than `max_chars` is cut inside,
//!    and then at a space.
//! 2. **Pack sentences up to `target_chars`.** About 1,000 characters, which is
//!    about 250 tokens for this model.
//! 3. **Repeat the end of each chunk at the start of the next.** An answer that
//!    crosses a cut is then whole in one of the two chunks. The repeated part
//!    is whole sentences, at least `overlap_chars` long when the sentences
//!    allow it and never more than half a chunk.
//!
//! Every chunk records where it starts and ends in the document, as byte
//! offsets. `get_passage` uses them to return a chunk with its neighbours as
//! one span of the original text, so the overlap is not printed twice.

use std::ops::Range;

use crate::config::{ChunkSettings, ContextMode, DOCUMENT_PREFIX};

/// One chunk of a document.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    /// The chunk's position in the document, from 0.
    pub ordinal: usize,
    /// Where the chunk starts in the normalised document text, in bytes.
    pub start: usize,
    /// Where the chunk ends, in bytes, exclusive.
    pub end: usize,
    /// The chunk's text: `document[start..end]`.
    pub text: String,
}

/// Words that end with a full stop without ending a sentence.
///
/// Wikipedia writes `c. 4 BC`, `fl. 450 BC`, `(b. 1920)` and `St. Paul`. Each
/// of these would otherwise end a sentence in the middle.
const ABBREVIATIONS: [&str; 22] = [
    "c", "ca", "cf", "vol", "vols", "no", "ch", "st", "mr", "mrs", "dr", "fl", "ed", "eds", "etc", "e.g", "i.e", "vs", "p", "pp", "b", "d",
];

/// Collapses runs of spaces and keeps paragraph breaks.
///
/// Chunk offsets are positions in the text this returns, so the database
/// stores this text and not the original.
///
/// @param text - a document as the source holds it
pub fn normalise(text: &str) -> String {
    let paragraphs: Vec<String> = text
        .split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|paragraph| !paragraph.is_empty())
        .collect();
    paragraphs.join("\n\n")
}

/// Cuts a normalised document into chunks.
///
/// @param text - the output of [`normalise`]
/// @param settings - the lengths to pack to
pub fn chunk_text(text: &str, settings: &ChunkSettings) -> Vec<Chunk> {
    let pieces = sentence_pieces(text, settings);
    let spans = pack(&pieces, settings);
    spans
        .into_iter()
        .enumerate()
        .map(|(ordinal, span)| Chunk { ordinal, start: span.start, end: span.end, text: text[span].to_string() })
        .collect()
}

/// Returns the text that is embedded for one chunk.
///
/// The document title goes in front of every chunk. A chunk in the middle of
/// the Seneca article may say only "he" and "his letters", and without the
/// title its vector does not know whose letters they are. In `lead` mode the
/// document's first sentence follows the title. The first chunk already starts
/// with that sentence, so it is not added twice.
///
/// @param title - the document title
/// @param lead - the document's first sentence
/// @param chunk - the chunk
/// @param mode - what to put in front
pub fn embedding_input(title: &str, lead: &str, chunk: &Chunk, mode: ContextMode) -> String {
    match mode {
        ContextMode::Lead if chunk.ordinal > 0 => format!("{DOCUMENT_PREFIX}{title}\n{lead}\n\n{}", chunk.text),
        _ => format!("{DOCUMENT_PREFIX}{title}\n\n{}", chunk.text),
    }
}

/// Returns the document's first sentence, cut to 300 bytes.
///
/// @param text - the normalised document
pub fn lead_sentence(text: &str) -> String {
    let first = split_sentences(text).into_iter().next().map(|range| &text[range]).unwrap_or("");
    let mut end = first.len().min(300);
    while !first.is_char_boundary(end) {
        end -= 1;
    }
    first[..end].to_string()
}

/// Splits text into sentences, then cuts any sentence longer than `max_chars`.
///
/// @param text - the normalised document
/// @param settings - the lengths
fn sentence_pieces(text: &str, settings: &ChunkSettings) -> Vec<Range<usize>> {
    let mut pieces = Vec::new();
    for sentence in split_sentences(text) {
        if sentence.len() <= settings.max_chars {
            pieces.push(sentence);
        } else {
            pieces.extend(cut_at_spaces(text, sentence, settings.target_chars));
        }
    }
    pieces
}

/// Returns the byte range of every sentence, without surrounding spaces.
///
/// A sentence ends at `.`, `?` or `!`, after any closing quote or bracket,
/// when a space and then a capital letter, a digit or an opening quote
/// follow, and the word before the full stop is not an initial or one of
/// [`ABBREVIATIONS`]. A paragraph break always ends a sentence.
///
/// @param text - the normalised document
pub fn split_sentences(text: &str) -> Vec<Range<usize>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        if c == '\n' {
            push_trimmed(text, start..at, &mut sentences);
            start = at + 1;
        } else if matches!(c, '.' | '?' | '!') {
            let mut after = i + 1;
            while after < chars.len() && matches!(chars[after].1, '"' | '\'' | '”' | '’' | ')' | ']') {
                after += 1;
            }
            let space = chars.get(after).is_some_and(|(_, next)| *next == ' ');
            let opens = chars.get(after + 1).is_some_and(|(_, next)| opens_sentence(*next));
            if space && opens && !(c == '.' && is_abbreviation(text, at)) {
                push_trimmed(text, start..chars[after].0, &mut sentences);
                start = chars[after].0;
                i = after;
            }
        }
        i += 1;
    }
    push_trimmed(text, start..text.len(), &mut sentences);
    sentences
}

/// Reports whether a character can start a sentence.
///
/// @param c - the first character after the space
fn opens_sentence(c: char) -> bool {
    c.is_uppercase() || c.is_ascii_digit() || matches!(c, '"' | '\'' | '“' | '‘' | '(')
}

/// Reports whether the full stop at `dot` ends an abbreviation or an initial.
///
/// @param text - the document
/// @param dot - the byte position of the full stop
fn is_abbreviation(text: &str, dot: usize) -> bool {
    let word_start = text[..dot].rfind([' ', '(', '\n']).map(|at| at + 1).unwrap_or(0);
    let word = &text[word_start..dot];
    let is_initial = word.chars().count() == 1 && word.chars().all(char::is_uppercase);
    is_initial || ABBREVIATIONS.contains(&word.to_lowercase().as_str())
}

/// Adds a range to the list with its leading and trailing spaces removed.
///
/// @param text - the document
/// @param range - the range to add
/// @param into - the list
fn push_trimmed(text: &str, range: Range<usize>, into: &mut Vec<Range<usize>>) {
    let slice = &text[range.clone()];
    let start = range.start + (slice.len() - slice.trim_start().len());
    let end = range.end - (slice.len() - slice.trim_end().len());
    if start < end {
        into.push(start..end);
    }
}

/// Cuts one long sentence into pieces of at most `target` bytes, at spaces.
///
/// @param text - the document
/// @param sentence - the sentence's range
/// @param target - the longest piece wanted
fn cut_at_spaces(text: &str, sentence: Range<usize>, target: usize) -> Vec<Range<usize>> {
    let mut pieces = Vec::new();
    let mut start = sentence.start;
    while sentence.end - start > target {
        let window = &text[start..floor_boundary(text, start + target)];
        let cut = window.rfind(' ').filter(|at| *at > 0).map(|at| start + at).unwrap_or(start + window.len());
        push_trimmed(text, start..cut, &mut pieces);
        start = cut;
    }
    push_trimmed(text, start..sentence.end, &mut pieces);
    pieces
}

/// Returns the largest character boundary at or before a byte position.
///
/// @param text - the document
/// @param at - the byte position
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Packs sentences into overlapping spans.
///
/// @param pieces - the sentences, in order
/// @param settings - the lengths
fn pack(pieces: &[Range<usize>], settings: &ChunkSettings) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut first = 0;
    while first < pieces.len() {
        let start = pieces[first].start;
        let mut last = first;
        while last + 1 < pieces.len() && pieces[last + 1].end - start <= settings.target_chars {
            last += 1;
        }
        spans.push(start..pieces[last].end);
        if last + 1 >= pieces.len() {
            break;
        }
        first = overlap_start(pieces, first, last, settings);
    }
    fold_short_tail(&mut spans, settings);
    spans
}

/// Returns the sentence the next chunk starts at.
///
/// Walks back from the chunk's last sentence, adding sentences to the overlap
/// until it holds at least `overlap_chars`. It stops before the overlap would
/// pass half a chunk, and it never goes back as far as the chunk's first
/// sentence, so every chunk moves forward by at least one sentence.
///
/// @param pieces - the sentences
/// @param first - the chunk's first sentence
/// @param last - the chunk's last sentence
/// @param settings - the lengths
fn overlap_start(pieces: &[Range<usize>], first: usize, last: usize, settings: &ChunkSettings) -> usize {
    let mut next = last + 1;
    if settings.overlap_chars == 0 {
        return next;
    }
    while next - 1 > first {
        let carried = pieces[last].end - pieces[next - 1].start;
        if carried > settings.target_chars / 2 {
            break;
        }
        next -= 1;
        if carried >= settings.overlap_chars {
            break;
        }
    }
    next
}

/// Joins the last chunk to the one before when it adds too little new text.
///
/// Without this, a document often ends in a chunk that is mostly overlap and
/// one short sentence, which retrieves badly and repeats its neighbour.
///
/// @param spans - the chunks' ranges
/// @param settings - the lengths
fn fold_short_tail(spans: &mut Vec<Range<usize>>, settings: &ChunkSettings) {
    if spans.len() < 2 {
        return;
    }
    let last = spans[spans.len() - 1].clone();
    let before = spans[spans.len() - 2].clone();
    let new_text = last.end.saturating_sub(before.end);
    if new_text < settings.min_chars && last.end - before.start <= settings.max_chars {
        spans.pop();
        if let Some(previous) = spans.last_mut() {
            previous.end = last.end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a document of numbered sentences, each about 80 bytes long.
    fn sentences(count: usize) -> String {
        (0..count)
            .map(|n| format!("Sentence number {n} says something about the philosophers of ancient Greece here."))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn abbreviations_and_initials_do_not_end_a_sentence() {
        let text = "Seneca (c. 4 BC - AD 65) wrote letters. St. Paul read A. N. Whitehead? Yes. See vol. 2 for no. 5.";
        let found: Vec<&str> = split_sentences(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(
            found,
            vec!["Seneca (c. 4 BC - AD 65) wrote letters.", "St. Paul read A. N. Whitehead?", "Yes.", "See vol. 2 for no. 5."]
        );
    }

    #[test]
    fn no_chunk_is_longer_than_the_maximum() {
        let long = format!("{} {}", "word ".repeat(900).trim(), sentences(40));
        let settings = ChunkSettings::default();
        for chunk in chunk_text(&long, &settings) {
            assert!(chunk.text.len() <= settings.max_chars, "a chunk of {} bytes", chunk.text.len());
        }
    }

    #[test]
    fn neighbouring_chunks_share_text() {
        let text = sentences(60);
        let chunks = chunk_text(&text, &ChunkSettings::default());
        assert!(chunks.len() > 3);
        for pair in chunks.windows(2) {
            assert!(pair[1].start < pair[0].end, "chunk {} does not overlap chunk {}", pair[1].ordinal, pair[0].ordinal);
            assert!(pair[1].start > pair[0].start, "chunk {} does not move forward", pair[1].ordinal);
        }
    }

    #[test]
    fn the_chunks_cover_every_sentence() {
        let text = sentences(37);
        let chunks = chunk_text(&text, &ChunkSettings::default());
        assert_eq!(chunks.first().map(|c| c.start), Some(0));
        assert_eq!(chunks.last().map(|c| c.end), Some(text.len()));
        for pair in chunks.windows(2) {
            assert!(pair[1].start <= pair[0].end, "a gap between chunk {} and chunk {}", pair[0].ordinal, pair[1].ordinal);
        }
        for chunk in &chunks {
            assert_eq!(chunk.text, text[chunk.start..chunk.end]);
        }
    }

    #[test]
    fn a_short_document_is_one_chunk() {
        let chunks = chunk_text("Thales said everything is water.", &ChunkSettings::default());
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn overlap_can_be_turned_off() {
        let settings = ChunkSettings { overlap_chars: 0, ..ChunkSettings::default() };
        let chunks = chunk_text(&sentences(60), &settings);
        for pair in chunks.windows(2) {
            assert!(pair[1].start >= pair[0].end);
        }
    }
}
