//! Syntax diagnostics: what was wrong, and exactly where.
//!
//! Invariant: every parse failure carries the byte offset of the token that
//! caused it, and that offset is compared against the pinned release in tests.
//! A message may be worded differently from SQLite's; an offset may not differ,
//! because an offset is what an editor underlines and what a caller reports.
//!
//! The expected-token set is deliberately the smallest useful one rather than
//! the full first-set of the production. A list of forty keywords is not a
//! diagnostic, it is a grammar dump.

use inillucent_base::{DbError, PrimaryCode};

use crate::lexer::{LexError, LexErrorKind, Span};

/// Why a parse failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// The lexer refused a byte sequence.
    Lex(LexErrorKind),
    /// A token appeared where the grammar did not allow it.
    Unexpected {
        /// What was found, as source text.
        found: String,
        /// The smallest useful set of things that would have been accepted.
        expected: Vec<&'static str>,
    },
    /// The statement ended before the production did.
    UnexpectedEnd {
        /// What would have continued it.
        expected: Vec<&'static str>,
    },
    /// A construct the grammar has but this phase does not implement.
    Unsupported(&'static str),
    /// A statement the schema refuses, in the reference's own wording.
    ///
    /// It is not a syntax error and does not read as one: the statement parsed
    /// and the schema will not have it, which is what `foreign key mismatch`
    /// says.
    Refused(String),
    /// A hard limit was exceeded.
    LimitExceeded(&'static str),
}

/// A parse failure with its location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// Why it failed.
    pub kind: ParseErrorKind,
    /// Where it failed.
    pub span: Span,
}

impl ParseError {
    /// Returns a failure at a span.
    pub fn new(kind: ParseErrorKind, span: Span) -> ParseError {
        ParseError { kind, span }
    }

    /// Returns the byte offset a caller should point at.
    pub fn offset(&self) -> u32 {
        self.span.start
    }

    /// Returns the one-line message.
    pub fn message(&self) -> String {
        match &self.kind {
            ParseErrorKind::Lex(kind) => kind.message().to_string(),
            ParseErrorKind::Unexpected { found, expected } => {
                // **The expected set is not printed.** The reference never
                // names what it wanted - every syntax failure it reports is
                // `near "X": syntax error` and nothing more - and a message
                // that adds `, expected ;` is a message no transcript
                // comparison can match. The set is still carried, because it is
                // what `expected()` answers and the parser's own tests read it;
                // it is only the rendering that stops at the reference's words.
                let _ = expected;
                format!(r#"near "{found}": syntax error"#)
            }
            ParseErrorKind::UnexpectedEnd { expected } => {
                if expected.is_empty() {
                    "incomplete input".to_string()
                } else {
                    format!("incomplete input, expected {}", join_expected(expected))
                }
            }
            ParseErrorKind::Unsupported(what) => format!("unsupported: {what}"),
            ParseErrorKind::Refused(message) => message.clone(),
            ParseErrorKind::LimitExceeded(what) => format!("{what} exceeded"),
        }
    }

    /// Returns the stable result code this failure reports as.
    ///
    /// A syntax error is `SQLITE_ERROR`, which is what the pinned release
    /// returns from `prepare`. A limit is `SQLITE_TOOBIG` where SQLite uses it
    /// and `SQLITE_ERROR` where SQLite reports the limit as a parse error,
    /// which is the case for parser depth and compound depth.
    pub fn code(&self) -> PrimaryCode {
        match &self.kind {
            ParseErrorKind::LimitExceeded("string or blob too big") => PrimaryCode::TooBig,
            _ => PrimaryCode::Error,
        }
    }
}

impl From<LexError> for ParseError {
    /// Lifts a lexer failure into a parse failure at the same offset.
    fn from(error: LexError) -> ParseError {
        ParseError {
            kind: ParseErrorKind::Lex(error.kind),
            span: Span::at(error.offset as usize),
        }
    }
}

impl From<ParseError> for DbError {
    /// Converts a parse failure into the engine's stable error, keeping the
    /// offset so a caller can point at the character.
    fn from(error: ParseError) -> DbError {
        DbError::primary(error.code())
            .with_message(error.message())
            .with_sql_offset(error.offset())
    }
}

/// Renders an expected-token set the way a diagnostic reads it.
fn join_expected(expected: &[&'static str]) -> String {
    match expected {
        [] => String::new(),
        [only] => (*only).to_string(),
        [first, second] => format!("{first} or {second}"),
        _ => {
            let head: Vec<&str> = expected
                .get(..expected.len().saturating_sub(1))
                .unwrap_or(&[])
                .to_vec();
            let tail = expected.last().copied().unwrap_or("");
            format!("{}, or {tail}", head.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offset survives the conversion into the engine's error type, which
    /// is the whole point of carrying it.
    #[test]
    fn the_offset_reaches_the_engine_error() {
        let error = ParseError::new(
            ParseErrorKind::Unexpected {
                found: "FROM".to_string(),
                expected: vec!["an expression"],
            },
            Span::new(7, 11),
        );
        let db: DbError = error.into();
        assert_eq!(db.sql_offset(), Some(7));
        assert_eq!(db.code(), PrimaryCode::Error);
    }

    /// The expected set reads as a sentence at one, two, and more entries.
    #[test]
    fn the_expected_set_reads_as_a_sentence() {
        assert_eq!(join_expected(&["a"]), "a");
        assert_eq!(join_expected(&["a", "b"]), "a or b");
        assert_eq!(join_expected(&["a", "b", "c"]), "a, b, or c");
    }

    /// A lexer failure keeps its own offset when it becomes a parse failure.
    #[test]
    fn a_lex_failure_keeps_its_offset() {
        let error: ParseError = LexError {
            kind: LexErrorKind::UnterminatedQuote,
            offset: 12,
        }
        .into();
        assert_eq!(error.offset(), 12);
    }
}
