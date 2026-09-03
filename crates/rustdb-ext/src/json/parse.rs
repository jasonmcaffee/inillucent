//! The JSON and JSON5 text parser.
//!
//! Invariant: what the pinned release accepts, this accepts, and it records
//! *which* dialect each piece came from. Since 3.45 SQLite reads JSON5 -
//! unquoted keys, single-quoted strings, trailing commas, comments,
//! hexadecimal integers, bare leading and trailing decimal points, `Infinity`
//! and `NaN` - and it remembers the difference, because `json_valid(x, 1)` asks
//! whether the text was strict RFC-8259 and `json_valid(x, 2)` asks whether it
//! was JSON5. A parser that normalised on the way in could not answer the first
//! question at all.
//!
//! Two JSON5 spellings are converted rather than recorded, because the format
//! itself has nowhere to put them: `Infinity` becomes the float `9e999` and
//! `NaN` becomes `null`, which is what the pinned release stores.

use super::node::Node;

/// Why a document would not parse, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseFailure {
    /// The one-based byte position `json_error_position()` reports.
    pub position: usize,
}

/// What a successful parse learned about the text it read.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    /// The document.
    pub node: Node,
    /// Whether anything outside RFC-8259 was used.
    pub used_json5: bool,
}

/// Parses one JSON or JSON5 document, which must be the whole of the text.
pub fn parse(text: &str) -> Result<Parsed, ParseFailure> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        at: 0,
        used_json5: false,
        depth: 0,
    };
    parser.skip_space();
    let node = parser.value()?;
    parser.skip_space();
    if parser.at != parser.bytes.len() {
        return Err(parser.fail());
    }
    Ok(Parsed {
        node,
        used_json5: parser.used_json5,
    })
}

/// How deep a document may nest before it is refused.
///
/// SQLite's own limit, and it is a limit rather than a preference: the parser
/// descends recursively, so an unbounded document is a stack overflow rather
/// than an error. Refusing it is the only way the failure can be reported.
const MAX_DEPTH: usize = 1000;

/// The parser's position in one document.
struct Parser<'text> {
    bytes: &'text [u8],
    at: usize,
    used_json5: bool,
    depth: usize,
}

impl Parser<'_> {
    /// Returns the failure for the current position.
    ///
    /// The position is one-based and counts bytes, which is what
    /// `json_error_position()` reports; a zero answer from that function means
    /// the document parsed.
    fn fail(&self) -> ParseFailure {
        ParseFailure {
            position: self.at.saturating_add(1),
        }
    }

    /// Returns the byte at the cursor.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    /// Returns the byte at an offset from the cursor.
    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.at.saturating_add(offset)).copied()
    }

    /// Skips whitespace and, in JSON5, comments.
    fn skip_space(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) => self.at += 1,
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    self.used_json5 = true;
                    self.at += 2;
                    while !matches!(self.peek(), None | Some(b'\n')) {
                        self.at += 1;
                    }
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    self.used_json5 = true;
                    self.at += 2;
                    while self.peek().is_some()
                        && !(self.peek() == Some(b'*') && self.peek_at(1) == Some(b'/'))
                    {
                        self.at += 1;
                    }
                    if self.peek().is_some() {
                        self.at += 2;
                    }
                }
                // JSON5 counts the Unicode space separators as whitespace. The
                // ones that matter in practice are the no-break space and the
                // byte-order mark, both of which arrive in real documents.
                Some(0xc2) if self.peek_at(1) == Some(0xa0) => {
                    self.used_json5 = true;
                    self.at += 2;
                }
                Some(0xef) if self.peek_at(1) == Some(0xbb) && self.peek_at(2) == Some(0xbf) => {
                    self.used_json5 = true;
                    self.at += 3;
                }
                _ => return,
            }
        }
    }

    /// Parses one value.
    fn value(&mut self) -> Result<Node, ParseFailure> {
        if self.depth >= MAX_DEPTH {
            return Err(self.fail());
        }
        match self.peek() {
            None => Err(self.fail()),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string(b'"'),
            Some(b'\'') => {
                self.used_json5 = true;
                self.string(b'\'')
            }
            Some(b't') if self.word(b"true") => Ok(Node::True),
            Some(b'f') if self.word(b"false") => Ok(Node::False),
            Some(b'n') if self.word(b"null") => Ok(Node::Null),
            Some(b'N') if self.word(b"NaN") => {
                self.used_json5 = true;
                Ok(Node::Null)
            }
            Some(b'I') if self.word(b"Infinity") => {
                self.used_json5 = true;
                Ok(Node::Float("9e999".to_string()))
            }
            _ => self.number(),
        }
    }

    /// Consumes a bare keyword when it is the next thing in the text.
    ///
    /// The check that follows matters: `nullx` is not `null` followed by `x`,
    /// it is a document that does not parse, and a bare `match` on the first
    /// byte would accept it.
    fn word(&mut self, keyword: &[u8]) -> bool {
        let end = self.at.saturating_add(keyword.len());
        if self.bytes.get(self.at..end) != Some(keyword) {
            return false;
        }
        if self
            .bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return false;
        }
        self.at = end;
        true
    }

    /// Parses an object.
    fn object(&mut self) -> Result<Node, ParseFailure> {
        self.at += 1;
        self.depth += 1;
        let mut members = Vec::new();
        loop {
            self.skip_space();
            if self.peek() == Some(b'}') {
                if !members.is_empty() {
                    // A `}` straight after a comma is JSON5's trailing comma.
                    self.used_json5 = true;
                }
                self.at += 1;
                break;
            }
            let label = self.label()?;
            self.skip_space();
            if self.peek() != Some(b':') {
                return Err(self.fail());
            }
            self.at += 1;
            self.skip_space();
            let value = self.value()?;
            members.push((label, value));
            self.skip_space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    break;
                }
                _ => return Err(self.fail()),
            }
        }
        self.depth -= 1;
        Ok(Node::Object(members))
    }

    /// Parses an object label, which JSON5 also spells without quotes.
    fn label(&mut self) -> Result<Node, ParseFailure> {
        match self.peek() {
            Some(b'"') => self.string(b'"'),
            Some(b'\'') => {
                self.used_json5 = true;
                self.string(b'\'')
            }
            Some(byte) if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' => {
                self.used_json5 = true;
                let start = self.at;
                while self.peek().is_some_and(|byte| {
                    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
                }) {
                    self.at += 1;
                }
                let name = self.slice(start, self.at)?;
                Ok(Node::text_escaped(&name))
            }
            _ => Err(self.fail()),
        }
    }

    /// Parses an array.
    fn array(&mut self) -> Result<Node, ParseFailure> {
        self.at += 1;
        self.depth += 1;
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.peek() == Some(b']') {
                if !items.is_empty() {
                    self.used_json5 = true;
                }
                self.at += 1;
                break;
            }
            let item = self.value()?;
            items.push(item);
            self.skip_space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    break;
                }
                _ => return Err(self.fail()),
            }
        }
        self.depth -= 1;
        Ok(Node::Array(items))
    }

    /// Returns a byte range of the source as text.
    fn slice(&self, start: usize, end: usize) -> Result<String, ParseFailure> {
        let bytes = self.bytes.get(start..end).ok_or_else(|| self.fail())?;
        String::from_utf8(bytes.to_vec()).map_err(|_| self.fail())
    }

    /// Parses a string, and decides which of the three text kinds it is.
    ///
    /// The content is kept exactly as written, escapes included, so that
    /// `json()` can hand back the spelling it read. What the scan decides is
    /// only which element type the content belongs to, which is the question
    /// the binary format asks and the renderer answers.
    fn string(&mut self, quote: u8) -> Result<Node, ParseFailure> {
        self.at += 1;
        let start = self.at;
        let mut has_escape = false;
        // The quote decides the dialect of the *document*; the content
        // decides the element type. `'sq'` is JSON5 text holding a plain
        // `TEXT` element, because nothing in `sq` is spelt differently in
        // the two dialects - and the pinned release stores it as `TEXT`.
        let mut needs_json5 = false;
        if quote == b'\'' {
            self.used_json5 = true;
        }
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.fail());
            };
            if byte == quote {
                break;
            }
            if byte == b'"' {
                // A double quote inside a JSON5 single-quoted string has to be
                // escaped on the way out, so the content is not plain text.
                needs_json5 = true;
                self.at += 1;
                continue;
            }
            if byte < 0x20 {
                // A raw control character is JSON5 only, and it too has to be
                // escaped on the way out.
                needs_json5 = true;
                self.at += 1;
                continue;
            }
            if byte != b'\\' {
                self.at += 1;
                continue;
            }
            has_escape = true;
            self.at += 1;
            let Some(escape) = self.peek() else {
                return Err(self.fail());
            };
            match escape {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.at += 1,
                b'u' => {
                    self.at += 1;
                    for _ in 0..4 {
                        if !self.peek().is_some_and(|byte| byte.is_ascii_hexdigit()) {
                            return Err(self.fail());
                        }
                        self.at += 1;
                    }
                }
                // JSON5's own escapes: a line continuation, `\0`, `\v`, `\'`
                // and a two-digit hexadecimal byte.
                b'\n' | b'\'' | b'0' | b'v' | b'\r' => {
                    needs_json5 = true;
                    self.at += 1;
                }
                b'x' => {
                    needs_json5 = true;
                    self.at += 1;
                    for _ in 0..2 {
                        if !self.peek().is_some_and(|byte| byte.is_ascii_hexdigit()) {
                            return Err(self.fail());
                        }
                        self.at += 1;
                    }
                }
                _ => return Err(self.fail()),
            }
        }
        let content = self.slice(start, self.at)?;
        self.at += 1;
        if needs_json5 {
            self.used_json5 = true;
            return Ok(Node::Text5(content));
        }
        if has_escape {
            return Ok(Node::TextJ(content));
        }
        Ok(Node::Text(content))
    }

    /// Parses a number, in either dialect.
    fn number(&mut self) -> Result<Node, ParseFailure> {
        let start = self.at;
        let mut json5 = false;
        if matches!(self.peek(), Some(b'+')) {
            json5 = true;
            self.at += 1;
        } else if matches!(self.peek(), Some(b'-')) {
            self.at += 1;
        }
        if self.word(b"Infinity") {
            self.used_json5 = true;
            let negative = self.bytes.get(start) == Some(&b'-');
            return Ok(Node::Float(if negative {
                "-9e999".to_string()
            } else {
                "9e999".to_string()
            }));
        }
        if self.word(b"NaN") {
            self.used_json5 = true;
            return Ok(Node::Null);
        }
        // Hexadecimal, which only JSON5 has and which is always an integer.
        if self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x' | b'X')) {
            self.at += 2;
            let digits = self.at;
            while self.peek().is_some_and(|byte| byte.is_ascii_hexdigit()) {
                self.at += 1;
            }
            if self.at == digits {
                return Err(self.fail());
            }
            self.used_json5 = true;
            return Ok(Node::Int5(self.slice(start, self.at)?));
        }
        let mut is_float = false;
        let integer_digits = self.digits();
        if integer_digits == 0 && self.peek() != Some(b'.') {
            return Err(self.fail());
        }
        // RFC-8259 forbids a leading zero on a multi-digit integer; JSON5 does
        // too, so this is a parse failure in both dialects.
        let first = self.bytes.get(start.saturating_add(usize::from(matches!(
            self.bytes.get(start),
            Some(b'-' | b'+')
        ))));
        if integer_digits > 1 && first == Some(&b'0') {
            return Err(self.fail());
        }
        if self.peek() == Some(b'.') {
            is_float = true;
            self.at += 1;
            let fraction_digits = self.digits();
            if integer_digits == 0 && fraction_digits == 0 {
                return Err(self.fail());
            }
            if integer_digits == 0 || fraction_digits == 0 {
                // `.5` and `5.` are JSON5 spellings of a float.
                json5 = true;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err(self.fail());
            }
        }
        let text = self.slice(start, self.at)?;
        if json5 {
            self.used_json5 = true;
        }
        Ok(match (is_float, json5) {
            (true, false) => Node::Float(text),
            (true, true) => Node::Float5(text),
            (false, false) => Node::Int(text),
            (false, true) => Node::Int5(text),
        })
    }

    /// Consumes decimal digits, returning how many there were.
    fn digits(&mut self) -> usize {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        self.at.saturating_sub(start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses text, expecting it to succeed.
    fn ok(text: &str) -> Parsed {
        parse(text).expect("the document parses")
    }

    /// Strict JSON parses and is not reported as JSON5.
    #[test]
    fn strict_json_is_not_json5() {
        let parsed = ok(r#"{"a":1,"b":[2,3.5,null,true,false]}"#);
        assert!(!parsed.used_json5);
        assert_eq!(parsed.node.type_name(), "object");
    }

    /// A number keeps the spelling it was written with.
    #[test]
    fn numbers_keep_their_spelling() {
        assert_eq!(ok("1.50").node, Node::Float("1.50".to_string()));
        assert_eq!(ok("-3").node, Node::Int("-3".to_string()));
        assert_eq!(ok("1e3").node, Node::Float("1e3".to_string()));
    }

    /// The JSON5 spellings parse and are recorded as JSON5.
    #[test]
    fn json5_spellings_are_recorded() {
        let parsed = ok("{a:1, b:0x10, c:.5, d:+Infinity, e:NaN, f:'sq', g:[1,2,],}");
        assert!(parsed.used_json5);
        let Node::Object(members) = parsed.node else {
            panic!("an object");
        };
        assert_eq!(members[1].1, Node::Int5("0x10".to_string()));
        assert_eq!(members[2].1, Node::Float5(".5".to_string()));
        assert_eq!(members[3].1, Node::Float("9e999".to_string()));
        assert_eq!(members[4].1, Node::Null);
        assert_eq!(members[5].1, Node::Text("sq".to_string()));
    }

    /// Comments are JSON5 and are skipped wherever whitespace is allowed.
    #[test]
    fn comments_are_skipped() {
        let parsed = ok("[1,/*c*/2, // trailing\n3]");
        assert!(parsed.used_json5);
        let Node::Array(items) = parsed.node else {
            panic!("an array");
        };
        assert_eq!(items.len(), 3);
    }

    /// An escape makes a string `TEXTJ`, and no escape leaves it `TEXT`.
    #[test]
    fn string_kinds_follow_their_escapes() {
        assert_eq!(ok(r#""ab""#).node, Node::Text("ab".to_string()));
        assert_eq!(ok(r#""a\nb""#).node, Node::TextJ("a\\nb".to_string()));
        assert_eq!(ok(r#""a\x41""#).node, Node::Text5("a\\x41".to_string()));
    }

    /// A leading zero is refused, in both dialects.
    #[test]
    fn a_leading_zero_is_refused() {
        assert!(parse("01").is_err());
        assert!(parse("[01]").is_err());
        assert_eq!(ok("0").node, Node::Int("0".to_string()));
    }

    /// A failure reports a one-based byte position.
    #[test]
    fn failures_report_a_position() {
        assert_eq!(parse("[1,2").unwrap_err().position, 5);
        assert_eq!(parse("").unwrap_err().position, 1);
    }

    /// Trailing text after a complete value is a failure, not a second value.
    #[test]
    fn trailing_text_is_refused() {
        assert!(parse("1 2").is_err());
        assert!(parse("nullx").is_err());
    }

    /// Nesting is bounded, so a hostile document is an error and not a crash.
    #[test]
    fn nesting_is_bounded() {
        let deep = "[".repeat(2000);
        assert!(parse(&deep).is_err());
    }
}
