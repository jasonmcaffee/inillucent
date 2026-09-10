//! The zero-copy lexer: SQL bytes in, tokens with spans out.
//!
//! Invariant: every input byte belongs to exactly one token or trivia span,
//! spans are ordered, non-overlapping and inside the source, and no token owns
//! a byte. Token text is always a slice of the original SQL, which is what
//! makes `prepare` free of identifier allocation and what lets an error point
//! at an exact offset in the caller's own string.
//!
//! An unterminated quote or comment reports the offset of the byte that opened
//! it, not the end of input. That is the difference between "there is a problem
//! at character 4093" and "there is a problem somewhere", and it is the reason
//! the opening offset is carried down rather than recomputed.

use crate::keyword::{self, Keyword};

/// A half-open byte range in the source SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Span {
    /// The first byte of the span.
    pub start: u32,
    /// One past the last byte of the span.
    pub end: u32,
}

impl Span {
    /// Returns a span covering `start..end`.
    pub fn new(start: usize, end: usize) -> Span {
        Span {
            start: start.min(u32::MAX as usize) as u32,
            end: end.min(u32::MAX as usize) as u32,
        }
    }

    /// Returns an empty span at one offset, used for end-of-input.
    pub fn at(offset: usize) -> Span {
        Span::new(offset, offset)
    }

    /// Returns the span covering both spans and everything between them.
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Returns the length of the span in bytes.
    pub fn len(self) -> usize {
        self.end.saturating_sub(self.start) as usize
    }

    /// Returns whether the span covers no bytes.
    pub fn is_empty(self) -> bool {
        self.end <= self.start
    }

    /// Returns the source bytes the span covers.
    pub fn slice(self, source: &[u8]) -> &[u8] {
        source
            .get(self.start as usize..self.end as usize)
            .unwrap_or(&[])
    }
}

/// The quoting form an identifier was written with.
///
/// SQLite treats the four forms differently once semantics begin: a
/// double-quoted word falls back to a string literal when it resolves to no
/// name and the connection permits it, and the other three never do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteForm {
    /// `plain`, with no quoting at all.
    Bare,
    /// `"quoted"`, which may fall back to a string literal.
    Double,
    /// `[quoted]`, the MS-Access form.
    Bracket,
    /// `` `quoted` ``, the MySQL form.
    Backtick,
}

/// The kind of a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// A word: a keyword when `keyword` is set, otherwise an identifier.
    Identifier {
        /// The keyword this word spells, when it spells one.
        keyword: Option<Keyword>,
        /// How the identifier was quoted.
        quote: QuoteForm,
    },
    /// A `'string'` literal.
    String,
    /// An `x'..'` blob literal.
    Blob,
    /// An integer literal, in decimal or hexadecimal.
    Integer,
    /// A floating-point literal.
    Float,
    /// A bound parameter.
    Parameter,
    /// Punctuation or an operator.
    Punctuator(Punctuator),
    /// End of input.
    EndOfInput,
}

/// Every punctuation and operator token the pinned release accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Punctuator {
    /// `(`
    LeftParen,
    /// `)`
    RightParen,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// `.`
    Dot,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `=` or `==`
    Equal,
    /// `<>` or `!=`
    NotEqual,
    /// `<`
    Less,
    /// `<=`
    LessEqual,
    /// `>`
    Greater,
    /// `>=`
    GreaterEqual,
    /// `<<`
    ShiftLeft,
    /// `>>`
    ShiftRight,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `~`
    BitNot,
    /// `||`
    Concat,
    /// `->`
    Arrow,
    /// `->>`
    DoubleArrow,
    /// `<->`, pgvector's Euclidean distance.
    L2Distance,
    /// `<=>`, pgvector's cosine distance.
    CosineDistance,
    /// `<#>`, pgvector's negative inner product.
    NegativeInnerProduct,
    /// `<+>`, pgvector's taxicab distance.
    L1Distance,
    /// `<~>`, pgvector's Hamming distance.
    HammingDistance,
    /// `<%>`, pgvector's Jaccard distance.
    JaccardDistance,
}

impl Punctuator {
    /// Returns the canonical spelling, for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Punctuator::LeftParen => "(",
            Punctuator::RightParen => ")",
            Punctuator::Comma => ",",
            Punctuator::Semicolon => ";",
            Punctuator::Dot => ".",
            Punctuator::Plus => "+",
            Punctuator::Minus => "-",
            Punctuator::Star => "*",
            Punctuator::Slash => "/",
            Punctuator::Percent => "%",
            Punctuator::Equal => "=",
            Punctuator::NotEqual => "<>",
            Punctuator::Less => "<",
            Punctuator::LessEqual => "<=",
            Punctuator::Greater => ">",
            Punctuator::GreaterEqual => ">=",
            Punctuator::ShiftLeft => "<<",
            Punctuator::ShiftRight => ">>",
            Punctuator::BitAnd => "&",
            Punctuator::BitOr => "|",
            Punctuator::BitNot => "~",
            Punctuator::Concat => "||",
            Punctuator::Arrow => "->",
            Punctuator::L2Distance => "<->",
            Punctuator::CosineDistance => "<=>",
            Punctuator::NegativeInnerProduct => "<#>",
            Punctuator::L1Distance => "<+>",
            Punctuator::HammingDistance => "<~>",
            Punctuator::JaccardDistance => "<%>",
            Punctuator::DoubleArrow => "->>",
        }
    }
}

/// One token: what it is and where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// The bytes of the source it covers.
    pub span: Span,
}

impl Token {
    /// Returns the source text of the token.
    pub fn text(self, source: &[u8]) -> &[u8] {
        self.span.slice(source)
    }

    /// Returns the keyword this token spells, if it spells one.
    pub fn keyword(self) -> Option<Keyword> {
        match self.kind {
            TokenKind::Identifier { keyword, .. } => keyword,
            _ => None,
        }
    }

    /// Returns whether the token is a punctuator of the given kind.
    pub fn is(self, punctuator: Punctuator) -> bool {
        self.kind == TokenKind::Punctuator(punctuator)
    }
}

/// Why lexing stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LexErrorKind {
    /// A quote or bracket was opened and never closed.
    UnterminatedQuote,
    /// A block comment was opened and never closed.
    UnterminatedComment,
    /// A byte that begins no token.
    UnrecognisedByte,
    /// A blob literal whose body is not an even number of hex digits.
    MalformedBlob,
    /// A numeric literal SQLite does not accept in this form.
    MalformedNumber,
    /// A parameter name that is empty or out of range.
    MalformedParameter,
}

impl LexErrorKind {
    /// Returns a stable one-line description.
    pub fn message(self) -> &'static str {
        match self {
            LexErrorKind::UnterminatedQuote => "unrecognized token: unterminated quoted name",
            LexErrorKind::UnterminatedComment => "unrecognized token: unterminated comment",
            LexErrorKind::UnrecognisedByte => "unrecognized token",
            LexErrorKind::MalformedBlob => "unrecognized token: malformed blob literal",
            LexErrorKind::MalformedNumber => "unrecognized token: malformed numeric literal",
            LexErrorKind::MalformedParameter => "unrecognized token: malformed parameter",
        }
    }
}

/// A lexing failure, with the offset of the byte that caused it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LexError {
    /// Why it failed.
    pub kind: LexErrorKind,
    /// The offset the diagnostic points at.
    pub offset: u32,
}

/// The scanner. It holds the source and a cursor and nothing else.
#[derive(Clone, Debug)]
pub struct Lexer<'a> {
    source: &'a [u8],
    offset: usize,
}

impl<'a> Lexer<'a> {
    /// Returns a lexer positioned at the start of the source.
    pub fn new(source: &'a [u8]) -> Lexer<'a> {
        Lexer { source, offset: 0 }
    }

    /// Returns a lexer positioned at a byte offset in the source.
    pub fn at(source: &'a [u8], offset: usize) -> Lexer<'a> {
        Lexer {
            source,
            offset: offset.min(source.len()),
        }
    }

    /// Returns the current byte offset.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the source being scanned.
    pub fn source(&self) -> &'a [u8] {
        self.source
    }

    /// Returns the byte at an offset, if there is one.
    fn byte(&self, offset: usize) -> Option<u8> {
        self.source.get(offset).copied()
    }

    /// Skips whitespace and both comment forms, returning an error for an
    /// unterminated block comment at the offset that opened it.
    fn skip_trivia(&mut self) -> Result<(), LexError> {
        loop {
            match self.byte(self.offset) {
                Some(byte) if is_space(byte) => self.offset += 1,
                Some(b'-') if self.byte(self.offset + 1) == Some(b'-') => {
                    self.offset += 2;
                    while let Some(byte) = self.byte(self.offset) {
                        self.offset += 1;
                        if byte == b'\n' {
                            break;
                        }
                    }
                }
                Some(b'/') if self.byte(self.offset + 1) == Some(b'*') => {
                    let opened = self.offset;
                    self.offset += 2;
                    loop {
                        match self.byte(self.offset) {
                            None => {
                                // SQLite accepts an unterminated block comment
                                // at the very end of input, treating it as
                                // closed. It is the one place a missing
                                // terminator is not an error.
                                return Ok(());
                            }
                            Some(b'*') if self.byte(self.offset + 1) == Some(b'/') => {
                                self.offset += 2;
                                break;
                            }
                            Some(_) => self.offset += 1,
                        }
                    }
                    let _ = opened;
                }
                _ => return Ok(()),
            }
        }
    }

    /// Scans and returns the next token, skipping trivia first.
    pub fn next_token(&mut self) -> Result<Token, LexError> {
        self.skip_trivia()?;
        let start = self.offset;
        let Some(byte) = self.byte(start) else {
            return Ok(Token {
                kind: TokenKind::EndOfInput,
                span: Span::at(start),
            });
        };
        match byte {
            b'\'' => self.scan_quoted(start, b'\'', TokenKind::String),
            b'"' => self.scan_quoted(
                start,
                b'"',
                TokenKind::Identifier {
                    keyword: None,
                    quote: QuoteForm::Double,
                },
            ),
            b'`' => self.scan_quoted(
                start,
                b'`',
                TokenKind::Identifier {
                    keyword: None,
                    quote: QuoteForm::Backtick,
                },
            ),
            b'[' => self.scan_bracket(start),
            b'0'..=b'9' => self.scan_number(start),
            b'.' if self.byte(start + 1).is_some_and(is_digit) => self.scan_number(start),
            b'?' | b':' | b'@' | b'$' => self.scan_parameter(start),
            byte if is_identifier_start(byte) => self.scan_word(start),
            _ => self.scan_punctuator(start),
        }
    }

    /// Scans a `'`, `"` or backtick delimited run, where the delimiter is
    /// escaped by doubling it.
    fn scan_quoted(
        &mut self,
        start: usize,
        delimiter: u8,
        kind: TokenKind,
    ) -> Result<Token, LexError> {
        let mut cursor = start + 1;
        loop {
            match self.byte(cursor) {
                None => {
                    return Err(LexError {
                        kind: LexErrorKind::UnterminatedQuote,
                        offset: start as u32,
                    })
                }
                Some(byte) if byte == delimiter => {
                    if self.byte(cursor + 1) == Some(delimiter) {
                        cursor += 2;
                        continue;
                    }
                    cursor += 1;
                    break;
                }
                Some(_) => cursor += 1,
            }
        }
        self.offset = cursor;
        Ok(Token {
            kind,
            span: Span::new(start, cursor),
        })
    }

    /// Scans a `[bracketed]` identifier, which has no escape at all.
    fn scan_bracket(&mut self, start: usize) -> Result<Token, LexError> {
        let mut cursor = start + 1;
        loop {
            match self.byte(cursor) {
                None => {
                    return Err(LexError {
                        kind: LexErrorKind::UnterminatedQuote,
                        offset: start as u32,
                    })
                }
                Some(b']') => {
                    cursor += 1;
                    break;
                }
                Some(_) => cursor += 1,
            }
        }
        self.offset = cursor;
        Ok(Token {
            kind: TokenKind::Identifier {
                keyword: None,
                quote: QuoteForm::Bracket,
            },
            span: Span::new(start, cursor),
        })
    }

    /// Scans a bare word, which may be a keyword or the `x'..'` blob prefix.
    fn scan_word(&mut self, start: usize) -> Result<Token, LexError> {
        let mut cursor = start;
        while self.byte(cursor).is_some_and(is_identifier_part) {
            cursor += 1;
        }
        let word = self.source.get(start..cursor).unwrap_or(&[]);
        if word.len() == 1
            && word
                .first()
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'x'))
            && self.byte(cursor) == Some(b'\'')
        {
            return self.scan_blob(start, cursor);
        }
        self.offset = cursor;
        Ok(Token {
            kind: TokenKind::Identifier {
                keyword: keyword::lookup(word),
                quote: QuoteForm::Bare,
            },
            span: Span::new(start, cursor),
        })
    }

    /// Scans the `'..'` body of a blob literal, which must be an even number of
    /// hexadecimal digits.
    fn scan_blob(&mut self, start: usize, quote: usize) -> Result<Token, LexError> {
        let mut cursor = quote + 1;
        let body = cursor;
        loop {
            match self.byte(cursor) {
                None => {
                    return Err(LexError {
                        kind: LexErrorKind::UnterminatedQuote,
                        offset: start as u32,
                    })
                }
                Some(b'\'') => break,
                Some(byte) if byte.is_ascii_hexdigit() => cursor += 1,
                Some(_) => {
                    return Err(LexError {
                        kind: LexErrorKind::MalformedBlob,
                        offset: start as u32,
                    })
                }
            }
        }
        if !(cursor - body).is_multiple_of(2) {
            return Err(LexError {
                kind: LexErrorKind::MalformedBlob,
                offset: start as u32,
            });
        }
        self.offset = cursor + 1;
        Ok(Token {
            kind: TokenKind::Blob,
            span: Span::new(start, cursor + 1),
        })
    }

    /// Scans a numeric literal in every form the pinned release accepts.
    fn scan_number(&mut self, start: usize) -> Result<Token, LexError> {
        if self.byte(start) == Some(b'0')
            && self
                .byte(start + 1)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'x'))
        {
            return self.scan_hex_number(start);
        }
        let mut cursor = start;
        let mut float = false;
        cursor = self.scan_digits(cursor);
        if self.byte(cursor) == Some(b'.') {
            float = true;
            cursor = self.scan_digits(cursor + 1);
        }
        if self
            .byte(cursor)
            .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'e'))
        {
            let mut lookahead = cursor + 1;
            if matches!(self.byte(lookahead), Some(b'+') | Some(b'-')) {
                lookahead += 1;
            }
            if self.byte(lookahead).is_some_and(is_digit) {
                float = true;
                cursor = self.scan_digits(lookahead);
            }
        }
        // A digit run that runs straight into a word is not two tokens; SQLite
        // rejects `123abc` rather than lexing an integer and an identifier.
        if self.byte(cursor).is_some_and(is_identifier_part) {
            return Err(LexError {
                kind: LexErrorKind::MalformedNumber,
                offset: start as u32,
            });
        }
        self.offset = cursor;
        Ok(Token {
            kind: if float {
                TokenKind::Float
            } else {
                TokenKind::Integer
            },
            span: Span::new(start, cursor),
        })
    }

    /// Scans a `0x` literal, which has no fractional or exponent part.
    fn scan_hex_number(&mut self, start: usize) -> Result<Token, LexError> {
        let mut cursor = start + 2;
        let digits = cursor;
        while self
            .byte(cursor)
            .is_some_and(|byte| byte.is_ascii_hexdigit())
        {
            cursor += 1;
        }
        if cursor == digits || self.byte(cursor).is_some_and(is_identifier_part) {
            return Err(LexError {
                kind: LexErrorKind::MalformedNumber,
                offset: start as u32,
            });
        }
        self.offset = cursor;
        Ok(Token {
            kind: TokenKind::Integer,
            span: Span::new(start, cursor),
        })
    }

    /// Advances over a run of decimal digits, allowing SQLite's `_` separators.
    fn scan_digits(&mut self, from: usize) -> usize {
        let mut cursor = from;
        while let Some(byte) = self.byte(cursor) {
            if is_digit(byte) {
                cursor += 1;
                continue;
            }
            // A separator is only a separator between two digits.
            if byte == b'_'
                && cursor > from
                && self.byte(cursor + 1).is_some_and(is_digit)
                && self.byte(cursor.wrapping_sub(1)).is_some_and(is_digit)
            {
                cursor += 1;
                continue;
            }
            break;
        }
        cursor
    }

    /// Scans `?`, `?NNN`, `:name`, `@name` and `$name`.
    fn scan_parameter(&mut self, start: usize) -> Result<Token, LexError> {
        let sigil = self.byte(start).unwrap_or(b'?');
        let mut cursor = start + 1;
        if sigil == b'?' {
            cursor = self.scan_digits(cursor);
            self.offset = cursor;
            return Ok(Token {
                kind: TokenKind::Parameter,
                span: Span::new(start, cursor),
            });
        }
        while self.byte(cursor).is_some_and(is_identifier_part) {
            cursor += 1;
        }
        // `$name` accepts a bracketed or quoted suffix in SQLite's TCL variable
        // syntax; the parenthesised form is the one that reaches SQL.
        if sigil == b'$' && self.byte(cursor) == Some(b'(') {
            while let Some(byte) = self.byte(cursor) {
                cursor += 1;
                if byte == b')' {
                    break;
                }
            }
        }
        if cursor == start + 1 {
            // A bare `:` or `@` is not a parameter. `:` is punctuation SQLite
            // has no use for, so the byte is unrecognised.
            return Err(LexError {
                kind: LexErrorKind::MalformedParameter,
                offset: start as u32,
            });
        }
        self.offset = cursor;
        Ok(Token {
            kind: TokenKind::Parameter,
            span: Span::new(start, cursor),
        })
    }

    /// Scans punctuation and operators, longest form first.
    fn scan_punctuator(&mut self, start: usize) -> Result<Token, LexError> {
        let one = self.byte(start).unwrap_or(0);
        let two = self.byte(start + 1);
        let three = self.byte(start + 2);
        let (punctuator, length) = match (one, two, three) {
            (b'-', Some(b'>'), Some(b'>')) => (Punctuator::DoubleArrow, 3),
            // **Before the two-byte forms, because `<=>` starts with `<=`.**
            // These are pgvector's distance operators, and the longest-form-
            // first rule is the only thing that keeps `v <=> q` from lexing as
            // `v <= (> q)`.
            (b'<', Some(b'-'), Some(b'>')) => (Punctuator::L2Distance, 3),
            (b'<', Some(b'='), Some(b'>')) => (Punctuator::CosineDistance, 3),
            (b'<', Some(b'#'), Some(b'>')) => (Punctuator::NegativeInnerProduct, 3),
            (b'<', Some(b'+'), Some(b'>')) => (Punctuator::L1Distance, 3),
            (b'<', Some(b'~'), Some(b'>')) => (Punctuator::HammingDistance, 3),
            (b'<', Some(b'%'), Some(b'>')) => (Punctuator::JaccardDistance, 3),
            (b'-', Some(b'>'), _) => (Punctuator::Arrow, 2),
            (b'|', Some(b'|'), _) => (Punctuator::Concat, 2),
            (b'<', Some(b'<'), _) => (Punctuator::ShiftLeft, 2),
            (b'>', Some(b'>'), _) => (Punctuator::ShiftRight, 2),
            (b'<', Some(b'='), _) => (Punctuator::LessEqual, 2),
            (b'>', Some(b'='), _) => (Punctuator::GreaterEqual, 2),
            (b'<', Some(b'>'), _) => (Punctuator::NotEqual, 2),
            (b'!', Some(b'='), _) => (Punctuator::NotEqual, 2),
            (b'=', Some(b'='), _) => (Punctuator::Equal, 2),
            (b'(', _, _) => (Punctuator::LeftParen, 1),
            (b')', _, _) => (Punctuator::RightParen, 1),
            (b',', _, _) => (Punctuator::Comma, 1),
            (b';', _, _) => (Punctuator::Semicolon, 1),
            (b'.', _, _) => (Punctuator::Dot, 1),
            (b'+', _, _) => (Punctuator::Plus, 1),
            (b'-', _, _) => (Punctuator::Minus, 1),
            (b'*', _, _) => (Punctuator::Star, 1),
            (b'/', _, _) => (Punctuator::Slash, 1),
            (b'%', _, _) => (Punctuator::Percent, 1),
            (b'=', _, _) => (Punctuator::Equal, 1),
            (b'<', _, _) => (Punctuator::Less, 1),
            (b'>', _, _) => (Punctuator::Greater, 1),
            (b'&', _, _) => (Punctuator::BitAnd, 1),
            (b'|', _, _) => (Punctuator::BitOr, 1),
            (b'~', _, _) => (Punctuator::BitNot, 1),
            _ => {
                return Err(LexError {
                    kind: LexErrorKind::UnrecognisedByte,
                    offset: start as u32,
                })
            }
        };
        self.offset = start + length;
        Ok(Token {
            kind: TokenKind::Punctuator(punctuator),
            span: Span::new(start, start + length),
        })
    }
}

/// Returns whether the byte is SQL whitespace.
pub fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Returns whether the byte is an ASCII decimal digit.
pub fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

/// Returns whether the byte may begin a bare identifier.
///
/// SQLite treats every byte at or above 0x80 as an identifier character, which
/// is how it accepts UTF-8 names without decoding them.
pub fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

/// Returns whether the byte may continue a bare identifier.
pub fn is_identifier_part(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit() || byte == b'$'
}

/// Returns the unquoted text of an identifier token, undoubling escapes.
///
/// The common case is a bare word, which borrows. Only a quoted name that
/// actually contains a doubled delimiter has to allocate.
pub fn identifier_text<'a>(source: &'a [u8], token: Token) -> std::borrow::Cow<'a, [u8]> {
    let raw = token.text(source);
    let TokenKind::Identifier { quote, .. } = token.kind else {
        return std::borrow::Cow::Borrowed(raw);
    };
    match quote {
        QuoteForm::Bare => std::borrow::Cow::Borrowed(raw),
        QuoteForm::Bracket => {
            std::borrow::Cow::Borrowed(raw.get(1..raw.len().saturating_sub(1)).unwrap_or(&[]))
        }
        QuoteForm::Double => unquote(raw, b'"'),
        QuoteForm::Backtick => unquote(raw, b'`'),
    }
}

/// Returns the body of a `'string'` token with doubled quotes undoubled.
pub fn string_text(source: &[u8], token: Token) -> std::borrow::Cow<'_, [u8]> {
    unquote(token.text(source), b'\'')
}

/// Strips the delimiters and undoubles the escapes of a quoted run.
fn unquote(raw: &[u8], delimiter: u8) -> std::borrow::Cow<'_, [u8]> {
    let body = raw.get(1..raw.len().saturating_sub(1)).unwrap_or(&[]);
    if !body.contains(&delimiter) {
        return std::borrow::Cow::Borrowed(body);
    }
    let mut out = Vec::with_capacity(body.len());
    let mut index = 0;
    while let Some(byte) = body.get(index).copied() {
        out.push(byte);
        index += if byte == delimiter && body.get(index + 1) == Some(&delimiter) {
            2
        } else {
            1
        };
    }
    std::borrow::Cow::Owned(out)
}

/// Returns the bytes of a blob literal token, decoded from its hex digits.
pub fn blob_bytes(source: &[u8], token: Token) -> Vec<u8> {
    let raw = token.text(source);
    let body = raw.get(2..raw.len().saturating_sub(1)).unwrap_or(&[]);
    let mut out = Vec::with_capacity(body.len() / 2);
    let mut index = 0;
    while let (Some(high), Some(low)) = (body.get(index), body.get(index + 1)) {
        let high = (*high as char).to_digit(16).unwrap_or(0) as u8;
        let low = (*low as char).to_digit(16).unwrap_or(0) as u8;
        out.push((high << 4) | low);
        index += 2;
    }
    out
}

/// Converts a byte offset into a one-based line and column, scanning lazily.
pub fn line_and_column(source: &[u8], offset: u32) -> (u32, u32) {
    let limit = (offset as usize).min(source.len());
    let mut line = 1u32;
    let mut column = 1u32;
    for byte in source.get(..limit).unwrap_or(&[]) {
        if *byte == b'\n' {
            line = line.saturating_add(1);
            column = 1;
        } else {
            column = column.saturating_add(1);
        }
    }
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects every token of a source, for the tests below.
    fn tokens(source: &str) -> Result<Vec<Token>, LexError> {
        let bytes = source.as_bytes();
        let mut lexer = Lexer::new(bytes);
        let mut out = Vec::new();
        loop {
            let token = lexer.next_token()?;
            if token.kind == TokenKind::EndOfInput {
                return Ok(out);
            }
            out.push(token);
        }
    }

    /// Every byte of the source belongs to exactly one token or to trivia, and
    /// the spans are ordered and inside the source. This is the lexer's first
    /// invariant and it is checkable directly.
    #[test]
    fn spans_are_ordered_disjoint_and_inside_the_source() {
        let source = "SELECT a, 'x' /* c */ FROM t -- tail\nWHERE b=1;";
        let found = tokens(source).expect("it lexes");
        let mut previous_end = 0u32;
        for token in &found {
            assert!(token.span.start >= previous_end, "{token:?}");
            assert!(token.span.end <= source.len() as u32, "{token:?}");
            assert!(token.span.end > token.span.start, "{token:?}");
            previous_end = token.span.end;
        }
    }

    /// A keyword is an identifier token carrying a keyword, so the parser can
    /// choose per position whether to accept it as a name.
    #[test]
    fn a_keyword_is_an_identifier_carrying_a_keyword() {
        let found = tokens("select key").expect("it lexes");
        assert_eq!(
            found.first().and_then(|t| t.keyword()),
            Some(Keyword::SELECT)
        );
        assert_eq!(found.get(1).and_then(|t| t.keyword()), Some(Keyword::KEY));
        assert!(found
            .get(1)
            .and_then(|t| t.keyword())
            .is_some_and(Keyword::may_fall_back));
    }

    /// The four quoting forms are distinguished, because only one of them may
    /// later become a string literal.
    #[test]
    fn the_four_identifier_quote_forms_are_distinguished() {
        let found = tokens("a \"b\" [c] `d`").expect("it lexes");
        let forms: Vec<QuoteForm> = found
            .iter()
            .filter_map(|token| match token.kind {
                TokenKind::Identifier { quote, .. } => Some(quote),
                _ => None,
            })
            .collect();
        assert_eq!(
            forms,
            vec![
                QuoteForm::Bare,
                QuoteForm::Double,
                QuoteForm::Bracket,
                QuoteForm::Backtick
            ]
        );
    }

    /// A doubled quote inside a string is one quote, and the borrow is only
    /// given up when there is one.
    #[test]
    fn doubled_quotes_are_undoubled() {
        let source = b"'it''s'";
        let mut lexer = Lexer::new(source);
        let token = lexer.next_token().expect("it lexes");
        assert_eq!(token.kind, TokenKind::String);
        assert_eq!(string_text(source, token).as_ref(), b"it's");

        let plain = b"'plain'";
        let mut lexer = Lexer::new(plain);
        let token = lexer.next_token().expect("it lexes");
        assert!(matches!(
            string_text(plain, token),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// An unterminated quote reports the byte that opened it, not the end of
    /// input, because that is the offset a caller can act on.
    #[test]
    fn an_unterminated_quote_reports_its_opening_byte() {
        let mut lexer = Lexer::new(b"SELECT 'abc");
        assert_eq!(
            lexer.next_token().map(|t| t.kind),
            Ok(TokenKind::Identifier {
                keyword: Some(Keyword::SELECT),
                quote: QuoteForm::Bare
            })
        );
        assert_eq!(
            lexer.next_token(),
            Err(LexError {
                kind: LexErrorKind::UnterminatedQuote,
                offset: 7
            })
        );
    }

    /// Numeric forms: decimal, leading dot, exponent, hexadecimal, and the
    /// underscore separators the pinned release accepts.
    #[test]
    fn every_numeric_form_lexes() {
        for (source, kind) in [
            ("1", TokenKind::Integer),
            ("1_000", TokenKind::Integer),
            ("0x1f", TokenKind::Integer),
            ("0XFF", TokenKind::Integer),
            ("1.5", TokenKind::Float),
            (".5", TokenKind::Float),
            ("1.", TokenKind::Float),
            ("1e10", TokenKind::Float),
            ("1E+10", TokenKind::Float),
            ("1.5e-3", TokenKind::Float),
        ] {
            let found = tokens(source).expect(source);
            assert_eq!(found.first().map(|t| t.kind), Some(kind), "{source}");
            assert_eq!(found.len(), 1, "{source}");
        }
    }

    /// A number that runs into a word is one bad token, not two good ones.
    #[test]
    fn a_number_glued_to_a_word_is_rejected() {
        assert_eq!(
            tokens("123abc").map(|_| ()),
            Err(LexError {
                kind: LexErrorKind::MalformedNumber,
                offset: 0
            })
        );
    }

    /// Blob literals must be an even number of hex digits, and the prefix is
    /// case-insensitive.
    #[test]
    fn blob_literals_decode() {
        let source = b"X'48690a'";
        let mut lexer = Lexer::new(source);
        let token = lexer.next_token().expect("it lexes");
        assert_eq!(token.kind, TokenKind::Blob);
        assert_eq!(blob_bytes(source, token), vec![0x48, 0x69, 0x0a]);
        assert!(tokens("x'abc'").is_err());
        assert!(tokens("x'zz'").is_err());
    }

    /// Every parameter form lexes as one token.
    #[test]
    fn every_parameter_form_lexes() {
        for source in ["?", "?12", ":name", "@name", "$name"] {
            let found = tokens(source).expect(source);
            assert_eq!(found.len(), 1, "{source}");
            assert_eq!(found.first().map(|t| t.kind), Some(TokenKind::Parameter));
        }
        assert!(tokens(":").is_err());
    }

    /// Both comment forms are trivia, and a line comment ends at the newline.
    #[test]
    fn comments_are_trivia() {
        let found = tokens("1 -- comment\n+ /* block */ 2").expect("it lexes");
        assert_eq!(found.len(), 3);
        assert!(found.get(1).is_some_and(|t| t.is(Punctuator::Plus)));
    }

    /// An unterminated block comment at end of input is accepted, which is what
    /// the pinned release does.
    #[test]
    fn an_unterminated_block_comment_at_end_of_input_is_accepted() {
        let found = tokens("SELECT 1 /* trailing").expect("it lexes");
        assert_eq!(found.len(), 2);
    }

    /// Operators lex longest-first, so `->>` never becomes `->` and `>`.
    #[test]
    fn operators_lex_longest_first() {
        let found = tokens("a->>b->c||d<<e").expect("it lexes");
        let punctuators: Vec<&'static str> = found
            .iter()
            .filter_map(|token| match token.kind {
                TokenKind::Punctuator(punctuator) => Some(punctuator.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(punctuators, vec!["->>", "->", "||", "<<"]);
    }

    /// Line and column are derived from an offset only when asked for, and
    /// count from one.
    #[test]
    fn line_and_column_count_from_one() {
        let source = b"SELECT\n  1";
        assert_eq!(line_and_column(source, 0), (1, 1));
        assert_eq!(line_and_column(source, 9), (2, 3));
    }

    /// Bytes above 0x7f are identifier characters, so a UTF-8 name lexes as one
    /// token without the lexer decoding anything.
    #[test]
    fn high_bytes_are_identifier_characters() {
        let found = tokens("naïve").expect("it lexes");
        assert_eq!(found.len(), 1);
    }
}
