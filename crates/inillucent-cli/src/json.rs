//! JSON, first-party.
//!
//! Invariant: this crate does not take a JSON dependency. `serde_json` is
//! allow-listed in `docs/invariants/layering.toml` for `inillucent-core` and
//! `inillucent-bench` only, and the reason is the one the dependency policy
//! gives for every other row - a production crate carries what it can argue
//! for. JSON is a grammar that fits on a napkin, the shell has been writing it
//! since `.mode json` existed, and the only new thing this ticket needs is the
//! other direction: an MCP server has to *read* a request.
//!
//! So this module holds both halves, and `render.rs`'s escaper moved into it
//! rather than being copied. Two JSON escapers in one crate is precisely the
//! kind of near-duplicate this repository's tests exist to catch, and the one
//! that would have been introduced here is the one that matters - the shell's
//! `.mode json` output and an MCP tool result would have disagreed about a
//! control character, in a way only a corpus with a tab in it would ever show.
//!
//! What is deliberately *not* here: streaming, arbitrary precision, and object
//! key lookup by hash. An MCP request is a few hundred bytes and a tool result
//! is written once.

/// A JSON value.
///
/// An object keeps its pairs in the order they were written, because a result
/// whose keys shuffle between runs cannot be compared byte for byte, and byte
/// comparison is how everything else in this repository is checked.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number that arrived, or is being written, as an integer.
    Int(i64),
    /// A number with a fractional part or an exponent.
    Real(f64),
    /// A string, already unescaped.
    Text(String),
    /// An array.
    Array(Vec<Json>),
    /// An object, in insertion order.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Returns the value stored under a key, when this is an object that has one.
    ///
    /// @param key - the member name
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(pairs) => pairs
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Returns this value as a string, for the variants that have one.
    ///
    /// A number or a boolean answers `None` rather than its rendering: a
    /// parameter declared as text and given `7` is a caller's mistake, and
    /// quietly reading it as `"7"` hides the mistake until the SQL is wrong.
    pub fn text(&self) -> Option<&str> {
        match self {
            Json::Text(text) => Some(text),
            _ => None,
        }
    }

    /// Returns this value as an integer, accepting a whole-numbered real.
    pub fn integer(&self) -> Option<i64> {
        match self {
            Json::Int(number) => Some(*number),
            // A JSON writer that has no integer type - JavaScript's, which is
            // most of them - writes `5` as a double. Refusing it would refuse
            // every `limit` an MCP client sends.
            Json::Real(number) if number.fract() == 0.0 => Some(*number as i64),
            _ => None,
        }
    }

    /// Returns this value as a boolean.
    pub fn boolean(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns the elements, when this is an array.
    pub fn array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Writes this value as compact JSON.
    pub fn write(&self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    /// Appends this value's rendering to a buffer.
    ///
    /// @param out - the buffer to append to
    fn write_into(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(number) => out.push_str(&number.to_string()),
            Json::Real(number) => out.push_str(&real(*number)),
            Json::Text(text) => {
                out.push('"');
                out.push_str(&escape(text));
                out.push('"');
            }
            Json::Array(items) => {
                out.push('[');
                for (nth, item) in items.iter().enumerate() {
                    if nth > 0 {
                        out.push(',');
                    }
                    item.write_into(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                out.push('{');
                for (nth, (name, value)) in pairs.iter().enumerate() {
                    if nth > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&escape(name));
                    out.push_str("\":");
                    value.write_into(out);
                }
                out.push('}');
            }
        }
    }

    /// Writes this value indented, for a human reading a result on a terminal.
    ///
    /// @param depth - how many levels in this value sits
    pub fn pretty(&self, depth: usize) -> String {
        let pad = "  ".repeat(depth + 1);
        let close = "  ".repeat(depth);
        match self {
            Json::Array(items) if !items.is_empty() => {
                let inner: Vec<String> = items
                    .iter()
                    .map(|item| format!("{pad}{}", item.pretty(depth + 1)))
                    .collect();
                format!("[\n{}\n{close}]", inner.join(",\n"))
            }
            Json::Object(pairs) if !pairs.is_empty() => {
                let inner: Vec<String> = pairs
                    .iter()
                    .map(|(name, value)| {
                        format!("{pad}\"{}\": {}", escape(name), value.pretty(depth + 1))
                    })
                    .collect();
                format!("{{\n{}\n{close}}}", inner.join(",\n"))
            }
            other => other.write(),
        }
    }
}

/// Renders a double the way JSON allows, which is never `NaN` or `Infinity`.
///
/// Neither has a JSON spelling, and a writer that emits the bare word produces
/// a document no parser will read back. A value the grammar cannot hold becomes
/// `null`, which is what every other JSON writer does with them.
///
/// @param number - the value to render
fn real(number: f64) -> String {
    if number.is_finite() {
        let rendered = format!("{number}");
        // `1` must not be written where `1.0` was meant: a reader that types
        // its result from the document would call it an integer.
        if rendered.contains(['.', 'e', 'E']) {
            rendered
        } else {
            format!("{rendered}.0")
        }
    } else {
        "null".to_string()
    }
}

/// Escapes the characters a JSON string may not carry raw.
///
/// One of three identical copies until task-1946's M4. The answer lives in
/// `inillucent_base::json` now; this name stays because the rest of this module
/// calls it.
///
/// @param text - the string to escape
pub fn escape(text: &str) -> String {
    inillucent_base::json::escape(text)
}

/// Builds an object from pairs, so a caller writes one line instead of five.
///
/// @param pairs - the members, in the order they should be written
pub fn object(pairs: Vec<(&str, Json)>) -> Json {
    Json::Object(
        pairs
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    )
}

/// Builds a JSON string.
///
/// @param text - the contents
pub fn text(text: impl Into<String>) -> Json {
    Json::Text(text.into())
}

/// Reads a JSON document, returning what it holds or why it could not be read.
///
/// @param source - the document
pub fn parse(source: &str) -> Result<Json, String> {
    let characters: Vec<char> = source.chars().collect();
    let mut reader = Reader {
        characters: &characters,
        at: 0,
        depth: 0,
    };
    reader.skip_space();
    let value = reader.value()?;
    reader.skip_space();
    if reader.at < reader.characters.len() {
        return Err(format!("trailing input at character {}", reader.at));
    }
    Ok(value)
}

/// A cursor over the document's characters.
struct Reader<'a> {
    /// The document.
    characters: &'a [char],
    /// How far in the cursor sits.
    at: usize,
    /// How many objects and arrays are open around the cursor.
    ///
    /// **Bounded, because this parser is recursive and its input is not
    /// trusted** (task-2066 §4.1.6). `value` calls `object` and `array`, each
    /// of which calls `value`, and nothing counted the nesting. A 240 KB line
    /// of 120,000 `[` overflowed the stack at exit 127 - in the CLI through
    /// `--params`, and in `inillucent-mcp` through a request line well inside
    /// the 1 MiB `MAX_REQUEST_BYTES`, which bounds the line and not what is
    /// inside it. With `panic = "abort"` a stack overflow is not catchable, so
    /// the server died with one line on stderr and the requests after it were
    /// never answered.
    ///
    /// Charging it here also stops the recursive `Drop` of a deep `Json`
    /// overflowing on the way out, which a check made anywhere later would not.
    depth: usize,
}

/// How deeply an object or array may nest.
///
/// A thousand, matching `MAX_DEPTH` in `inillucent-scalar/src/json/parse.rs`.
/// The two parsers read the same grammar from different callers, and a document
/// the SQL `json_valid()` refuses cleanly should not be one that ends this
/// process.
const MAX_DEPTH: usize = 1000;

impl Reader<'_> {
    /// Returns the character under the cursor without consuming it.
    fn peek(&self) -> Option<char> {
        self.characters.get(self.at).copied()
    }

    /// Consumes and returns the character under the cursor.
    fn next(&mut self) -> Option<char> {
        let character = self.peek();
        if character.is_some() {
            self.at += 1;
        }
        character
    }

    /// Advances past any whitespace.
    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(character) if character.is_whitespace()) {
            self.at += 1;
        }
    }

    /// Consumes an expected character, or says which one was missing.
    ///
    /// @param wanted - the character the grammar requires here
    fn expect(&mut self, wanted: char) -> Result<(), String> {
        match self.next() {
            Some(character) if character == wanted => Ok(()),
            Some(character) => Err(format!(
                "expected '{wanted}' at character {}, found '{character}'",
                self.at - 1
            )),
            None => Err(format!("expected '{wanted}', found end of input")),
        }
    }

    /// Charges one level of nesting, refusing past the bound.
    ///
    /// Paired with `ascend` around the *body* of `object` and `array` rather
    /// than held as a guard, because a guard borrowing the reader would stop
    /// the body reading from it at all. The pairing is in one place in each,
    /// with the body in its own function, so no `?` can leave a level charged -
    /// and a counter that leaks only on the error path is a parser that starts
    /// refusing valid documents after it has seen an invalid one.
    fn descend(&mut self) -> Result<(), String> {
        if self.depth >= MAX_DEPTH {
            return Err(format!(
                "nested more than {MAX_DEPTH} deep at character {}",
                self.at
            ));
        }
        self.depth = self.depth.saturating_add(1);
        Ok(())
    }

    /// Gives one level of nesting back.
    fn ascend(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Reads one value.
    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"') => Ok(Json::Text(self.string()?)),
            Some('t') => self.word("true", Json::Bool(true)),
            Some('f') => self.word("false", Json::Bool(false)),
            Some('n') => self.word("null", Json::Null),
            Some(character) if character == '-' || character.is_ascii_digit() => self.number(),
            Some(character) => Err(format!("unexpected '{character}' at character {}", self.at)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    /// Reads one of the three bare words.
    ///
    /// @param word - the literal expected here
    /// @param value - what it means
    fn word(&mut self, word: &str, value: Json) -> Result<Json, String> {
        for wanted in word.chars() {
            self.expect(wanted)?;
        }
        Ok(value)
    }

    /// Reads an object.
    fn object(&mut self) -> Result<Json, String> {
        self.descend()?;
        let produced = self.object_body();
        self.ascend();
        produced
    }

    /// Reads an object's contents, with its level already charged.
    fn object_body(&mut self) -> Result<Json, String> {
        self.expect('{')?;
        let mut pairs = Vec::new();
        self.skip_space();
        if self.peek() == Some('}') {
            self.at += 1;
            return Ok(Json::Object(pairs));
        }
        loop {
            self.skip_space();
            let name = self.string()?;
            self.skip_space();
            self.expect(':')?;
            self.skip_space();
            let value = self.value()?;
            pairs.push((name, value));
            self.skip_space();
            match self.next() {
                Some(',') => continue,
                Some('}') => return Ok(Json::Object(pairs)),
                Some(character) => {
                    return Err(format!(
                        "expected ',' or '}}' at character {}, found '{character}'",
                        self.at - 1
                    ))
                }
                None => return Err("unterminated object".to_string()),
            }
        }
    }

    /// Reads an array.
    fn array(&mut self) -> Result<Json, String> {
        self.descend()?;
        let produced = self.array_body();
        self.ascend();
        produced
    }

    /// Reads an array's contents, with its level already charged.
    fn array_body(&mut self) -> Result<Json, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_space();
        if self.peek() == Some(']') {
            self.at += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_space();
            items.push(self.value()?);
            self.skip_space();
            match self.next() {
                Some(',') => continue,
                Some(']') => return Ok(Json::Array(items)),
                Some(character) => {
                    return Err(format!(
                        "expected ',' or ']' at character {}, found '{character}'",
                        self.at - 1
                    ))
                }
                None => return Err("unterminated array".to_string()),
            }
        }
    }

    /// Reads a string, resolving its escapes.
    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.next() {
                Some('"') => return Ok(out),
                Some('\\') => match self.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('b') => out.push('\u{8}'),
                    Some('f') => out.push('\u{c}'),
                    Some('u') => out.push(self.escape_sequence()?),
                    Some(character) => {
                        return Err(format!("unknown escape '\\{character}'"));
                    }
                    None => return Err("unterminated escape".to_string()),
                },
                Some(character) => out.push(character),
                None => return Err("unterminated string".to_string()),
            }
        }
    }

    /// Reads the four hex digits after `\u`, joining a surrogate pair.
    ///
    /// A tool argument carrying an emoji arrives as two escapes in a document
    /// written by a JavaScript client, and a reader that took them one at a
    /// time would produce two unpaired halves - which is not a `char` and would
    /// have to become a replacement character. The text would survive the trip
    /// looking like a bug in the database.
    fn escape_sequence(&mut self) -> Result<char, String> {
        let first = self.hex4()?;
        if (0xD800..0xDC00).contains(&first) {
            self.expect('\\')?;
            self.expect('u')?;
            let second = self.hex4()?;
            if !(0xDC00..0xE000).contains(&second) {
                return Err("a high surrogate was not followed by a low one".to_string());
            }
            let combined = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
            return char::from_u32(combined).ok_or_else(|| "invalid surrogate pair".to_string());
        }
        char::from_u32(first).ok_or_else(|| format!("\\u{first:04x} is not a character"))
    }

    /// Reads exactly four hexadecimal digits.
    fn hex4(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            match self.next().and_then(|character| character.to_digit(16)) {
                Some(digit) => value = value * 16 + digit,
                None => return Err("a \\u escape needs four hexadecimal digits".to_string()),
            }
        }
        Ok(value)
    }

    /// Reads a number, keeping integers integral.
    fn number(&mut self) -> Result<Json, String> {
        let start = self.at;
        if self.peek() == Some('-') {
            self.at += 1;
        }
        let mut fractional = false;
        while let Some(character) = self.peek() {
            match character {
                '0'..='9' => self.at += 1,
                '.' | 'e' | 'E' | '+' | '-' => {
                    fractional = true;
                    self.at += 1;
                }
                _ => break,
            }
        }
        let literal: String = self
            .characters
            .get(start..self.at)
            .unwrap_or_default()
            .iter()
            .collect();
        if literal.is_empty() {
            return Err(format!("expected a number at character {start}"));
        }
        if !fractional {
            if let Ok(number) = literal.parse::<i64>() {
                return Ok(Json::Int(number));
            }
        }
        literal
            .parse::<f64>()
            .map(Json::Real)
            .map_err(|_| format!("'{literal}' is not a number"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four scalars read back as themselves.
    #[test]
    fn scalars_round_trip() {
        assert_eq!(parse("null"), Ok(Json::Null));
        assert_eq!(parse("true"), Ok(Json::Bool(true)));
        assert_eq!(parse("-12"), Ok(Json::Int(-12)));
        assert_eq!(parse("1.5"), Ok(Json::Real(1.5)));
        assert_eq!(parse("\"hi\""), Ok(text("hi")));
    }

    /// An integer stays an integer, which is what keeps a rowid exact.
    #[test]
    fn an_integer_is_not_a_double() {
        assert_eq!(parse("9007199254740993"), Ok(Json::Int(9007199254740993)));
        assert_eq!(Json::Int(7).write(), "7");
        assert_eq!(Json::Real(7.0).write(), "7.0");
    }

    /// A whole-numbered double still answers `integer()`, because most clients
    /// have no other way to write `5`.
    #[test]
    fn a_whole_double_reads_as_an_integer() {
        assert_eq!(Json::Real(5.0).integer(), Some(5));
        assert_eq!(Json::Real(5.5).integer(), None);
    }

    /// Objects keep their order, so two runs produce the same bytes.
    #[test]
    fn objects_keep_their_order() {
        let value = object(vec![("b", Json::Int(1)), ("a", Json::Int(2))]);
        assert_eq!(value.write(), "{\"b\":1,\"a\":2}");
    }

    /// Escapes survive both directions.
    #[test]
    fn escapes_round_trip() {
        let original = "a\"b\\c\nd\te\u{1}f";
        let written = text(original).write();
        assert_eq!(parse(&written), Ok(text(original)));
        assert!(written.contains("\\u0001"));
    }

    /// A surrogate pair becomes one character rather than two halves.
    #[test]
    fn a_surrogate_pair_becomes_one_character() {
        assert_eq!(parse("\"\\ud83d\\ude00\""), Ok(text("\u{1f600}")));
    }

    /// Nesting works, and a member is found by name.
    #[test]
    fn nesting_reads_back() {
        let value = parse("{\"a\": [1, {\"b\": null}], \"c\": \"d\"}").unwrap_or(Json::Null);
        assert_eq!(value.get("c").and_then(Json::text), Some("d"));
        let inner = value.get("a").and_then(Json::array).unwrap_or_default();
        assert_eq!(inner.len(), 2);
    }

    /// A document that does not parse says where it stopped.
    #[test]
    fn a_bad_document_is_refused() {
        assert!(parse("{\"a\": }").is_err());
        assert!(parse("[1, 2").is_err());
        assert!(parse("nul").is_err());
        assert!(parse("{} {}").is_err());
    }

    /// A value JSON cannot hold is written as null rather than as a bare word.
    #[test]
    fn a_non_finite_double_becomes_null() {
        assert_eq!(Json::Real(f64::NAN).write(), "null");
        assert_eq!(Json::Real(f64::INFINITY).write(), "null");
    }

    /// Whitespace anywhere the grammar allows it is skipped.
    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(
            parse("  {\n \"a\" : [ 1 , 2 ]\n}  "),
            parse("{\"a\":[1,2]}")
        );
    }
}

#[cfg(test)]
mod fuzz_seeded {
    /// How many inputs the seeded sweep below reads.
    const CASES: usize = 20_000;

    /// The characters the generator draws from.
    ///
    /// Weighted towards the ones a JSON parser branches on rather than uniform
    /// bytes, because a sweep of uniform bytes almost never produces a string
    /// that reaches past the first character and so exercises one branch.
    const ALPHABET: &[u8] = b"{}[]\",:0123456789.-+eEtruefalsnl /\t\n\r\0\xff";

    /// Returns the next value of a deterministic generator.
    ///
    /// @param state - the generator's state, advanced in place
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// The MCP request parser never panics on arbitrary text.
    ///
    /// **This is the parser every `tools/call` arrives through**, so its input
    /// is whatever an agent host sends, and the stable-toolchain twin of
    /// `fuzz/fuzz_targets/json.rs` is what keeps a regression in it failing a
    /// pull request rather than a scheduled job nobody reads.
    #[test]
    fn the_request_parser_never_panics_on_arbitrary_text() {
        let mut state = 0x1932_0003_u64;
        let mut parsed = 0usize;
        for _ in 0..CASES {
            let length = (next(&mut state) % 64) as usize;
            let bytes: Vec<u8> = (0..length)
                .map(|_| {
                    let at = (next(&mut state) as usize) % ALPHABET.len();
                    ALPHABET.get(at).copied().unwrap_or(b'?')
                })
                .collect();
            let text = String::from_utf8_lossy(&bytes).into_owned();
            parsed += usize::from(super::parse(&text).is_ok());
        }
        assert!(parsed <= CASES);
    }
}
