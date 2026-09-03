//! Rendering a document back to JSON text, and reading one element as SQL.
//!
//! Invariant: RFC-8259 goes out, whatever came in. A document parsed from JSON5
//! keeps its spelling in the tree and in the blob - that is what makes
//! `jsonb()` reproducible - but every text rendering converts: `0x10` is
//! written `16`, `.5` is written `0.5`, `+Infinity` is written `9e999`, a
//! single-quoted string is written with double quotes, and a `\x41` escape is
//! written `A`. The pinned release does exactly this, and the reason is
//! that the output of `json()` is meant to be readable by something that has
//! never heard of JSON5.

use super::node::{escape, Node};

/// Renders a document as the minified JSON text `json()` returns.
pub fn to_text(node: &Node) -> String {
    let mut out = String::new();
    write(node, &mut out);
    out
}

/// Renders a document as the indented text `json_pretty()` returns.
pub fn to_pretty(node: &Node, indent: &str) -> String {
    let mut out = String::new();
    write_pretty(node, indent, 0, &mut out);
    out
}

/// Writes one element in minified form.
fn write(node: &Node, out: &mut String) {
    match node {
        Node::Null => out.push_str("null"),
        Node::True => out.push_str("true"),
        Node::False => out.push_str("false"),
        Node::Int(text) | Node::Float(text) => out.push_str(text),
        Node::Int5(text) => out.push_str(&integer5_to_json(text)),
        Node::Float5(text) => out.push_str(&float5_to_json(text)),
        Node::Text(text) | Node::TextJ(text) => {
            out.push('"');
            out.push_str(text);
            out.push('"');
        }
        Node::Text5(text) => {
            out.push('"');
            out.push_str(&text5_to_json(text));
            out.push('"');
        }
        Node::TextRaw(text) => {
            out.push('"');
            out.push_str(&escape(text));
            out.push('"');
        }
        Node::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Node::Object(members) => {
            out.push('{');
            for (position, (label, value)) in members.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                write(label, out);
                out.push(':');
                write(value, out);
            }
            out.push('}');
        }
    }
}

/// Writes one element indented, the way `json_pretty()` lays it out.
///
/// An empty array or object stays on one line, which is what the pinned
/// release does and what makes `json_pretty('{"a":{}}')` two lines rather
/// than four.
fn write_pretty(node: &Node, indent: &str, depth: usize, out: &mut String) {
    match node {
        Node::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push_str(",\n");
                }
                push_indent(out, indent, depth + 1);
                write_pretty(item, indent, depth + 1, out);
            }
            out.push('\n');
            push_indent(out, indent, depth);
            out.push(']');
        }
        Node::Object(members) if !members.is_empty() => {
            out.push_str("{\n");
            for (position, (label, value)) in members.iter().enumerate() {
                if position > 0 {
                    out.push_str(",\n");
                }
                push_indent(out, indent, depth + 1);
                write(label, out);
                out.push_str(": ");
                write_pretty(value, indent, depth + 1, out);
            }
            out.push('\n');
            push_indent(out, indent, depth);
            out.push('}');
        }
        other => write(other, out),
    }
}

/// Writes one level's worth of indentation.
fn push_indent(out: &mut String, indent: &str, depth: usize) {
    for _ in 0..depth {
        out.push_str(indent);
    }
}

/// Converts a JSON5 integer to its RFC-8259 spelling.
pub fn integer5_to_json(text: &str) -> String {
    match integer5_value(text) {
        Some(value) => value.to_string(),
        None => text.trim_start_matches('+').to_string(),
    }
}

/// Returns the value of a JSON5 integer literal.
///
/// Hexadecimal wraps rather than saturating, which is what SQLite's own
/// conversion does; a literal that does not fit is a strange thing to write
/// and neither engine reports it.
pub fn integer5_value(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let value = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        let mut accumulator: u64 = 0;
        if hex.is_empty() {
            return None;
        }
        for digit in hex.chars() {
            let digit = digit.to_digit(16)?;
            accumulator = accumulator.wrapping_mul(16).wrapping_add(u64::from(digit));
        }
        accumulator as i64
    } else {
        digits.parse::<i64>().ok()?
    };
    Some(if negative {
        value.wrapping_neg()
    } else {
        value
    })
}

/// Converts a JSON5 float to its RFC-8259 spelling.
///
/// The two spellings JSON5 adds are a missing integer part and a missing
/// fraction, so the conversion is to supply the zero that is not written.
pub fn float5_to_json(text: &str) -> String {
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'-') => ("-", &text[1..]),
        Some(b'+') => ("", &text[1..]),
        _ => ("", text),
    };
    let mut body = rest.to_string();
    if body.starts_with('.') {
        body.insert(0, '0');
    }
    if body.ends_with('.') {
        body.push('0');
    }
    // A trailing point before an exponent - `1.e5` - is the same omission in
    // the middle of the literal rather than at its end.
    if let Some(exponent) = body.find(['e', 'E']) {
        if body[..exponent].ends_with('.') {
            body.insert(exponent, '0');
        }
    }
    format!("{sign}{body}")
}

/// Converts JSON5 string content to content a JSON reader accepts.
pub fn text5_to_json(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut characters = content.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            match character {
                '"' => out.push_str("\\\""),
                character if character < ' ' || character == '\u{7f}' => {
                    out.push_str(&escape(&character.to_string()));
                }
                character => out.push(character),
            }
            continue;
        }
        match characters.next() {
            None => out.push('\\'),
            // The escapes JSON already has pass through unchanged.
            Some(escape @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u')) => {
                out.push('\\');
                out.push(escape);
            }
            // JSON5's own escapes, each written as the code point JSON spells.
            Some('\'') => out.push('\''),
            Some('0') => out.push_str("\\u0000"),
            Some('v') => out.push_str("\\u000b"),
            Some('x') => {
                let high = characters.next().unwrap_or('0');
                let low = characters.next().unwrap_or('0');
                out.push_str(&format!("\\u00{high}{low}"));
            }
            // A backslash before a line break is a continuation: both go.
            Some('\n') => {}
            Some('\r') => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
            }
            Some(other) => out.push(other),
        }
    }
    out
}

/// Returns string content with every escape resolved, as SQL text.
pub fn unescape(node: &Node) -> String {
    let content = match node {
        Node::Text(text) => return text.clone(),
        Node::TextRaw(text) => return text.clone(),
        Node::TextJ(text) | Node::Text5(text) => text,
        _ => return String::new(),
    };
    let mut out = String::with_capacity(content.len());
    let mut characters = content.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            None => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('v') => out.push('\u{b}'),
            Some('0') => out.push('\0'),
            Some('\'') => out.push('\''),
            Some('x') => {
                let mut value = 0u32;
                for _ in 0..2 {
                    let Some(digit) = characters.peek().and_then(|digit| digit.to_digit(16)) else {
                        break;
                    };
                    characters.next();
                    value = value * 16 + digit;
                }
                out.push(char::from_u32(value).unwrap_or('\u{fffd}'));
            }
            Some('u') => {
                let value = read_hex4(&mut characters);
                // A high surrogate takes its low partner with it, which is how
                // JSON spells a character outside the basic plane.
                if (0xd800..0xdc00).contains(&value) {
                    let mut lookahead = characters.clone();
                    if lookahead.next() == Some('\\') && lookahead.next() == Some('u') {
                        let low = read_hex4(&mut lookahead);
                        if (0xdc00..0xe000).contains(&low) {
                            characters = lookahead;
                            let combined = 0x1_0000 + ((value - 0xd800) << 10) + (low - 0xdc00);
                            out.push(char::from_u32(combined).unwrap_or('\u{fffd}'));
                            continue;
                        }
                    }
                }
                out.push(char::from_u32(value).unwrap_or('\u{fffd}'));
            }
            Some('\n') => {}
            Some('\r') => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
            }
            Some(other) => out.push(other),
        }
    }
    out
}

/// Reads four hexadecimal digits, treating a short run as zeros.
fn read_hex4(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> u32 {
    let mut value = 0u32;
    for _ in 0..4 {
        let Some(digit) = characters.peek().and_then(|digit| digit.to_digit(16)) else {
            break;
        };
        characters.next();
        value = value * 16 + digit;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    /// Renders text through a parse, as `json()` does.
    fn round(text: &str) -> String {
        to_text(&parse::parse(text).expect("parses").node)
    }

    /// A strict document comes back with its own spelling.
    #[test]
    fn strict_documents_keep_their_spelling() {
        assert_eq!(round(" { \"a\" : 1.50 } "), "{\"a\":1.50}");
        assert_eq!(round("[1e3]"), "[1e3]");
    }

    /// Every JSON5 number spelling is converted on the way out.
    #[test]
    fn json5_numbers_are_converted() {
        assert_eq!(
            round("[0x10, 0xFF, -0x1f, .5, 5., -.25, +7, 1.]"),
            "[16,255,-31,0.5,5.0,-0.25,7,1.0]"
        );
    }

    /// Every JSON5 string spelling is converted on the way out.
    #[test]
    fn json5_strings_are_converted() {
        assert_eq!(
            round(r#"["a\x41b", "a\'b", "a\0b", "a\vb", "q\"q"]"#),
            r#"["a\u0041b","a'b","a\u0000b","a\u000bb","q\"q"]"#
        );
        assert_eq!(round("['a\"b', 'c']"), "[\"a\\\"b\",\"c\"]");
    }

    /// The indented form matches the pinned release's layout.
    #[test]
    fn pretty_matches_the_pinned_layout() {
        let node = parse::parse("{\"a\":[1,{\"b\":2}],\"c\":\"d\"}")
            .expect("parses")
            .node;
        assert_eq!(
            to_pretty(&node, "    "),
            "{\n    \"a\": [\n        1,\n        {\n            \"b\": 2\n        }\n    ],\n    \"c\": \"d\"\n}"
        );
        let empty = parse::parse("{\"a\":{}}").expect("parses").node;
        assert_eq!(to_pretty(&empty, "    "), "{\n    \"a\": {}\n}");
    }

    /// Escapes resolve to the characters they name when read as SQL text.
    #[test]
    fn escapes_resolve_to_sql_text() {
        assert_eq!(unescape(&Node::TextJ("a\\u0041b".to_string())), "aAb");
        assert_eq!(unescape(&Node::TextJ("a\\nb".to_string())), "a\nb");
        assert_eq!(unescape(&Node::Text5("a\\x41b".to_string())), "aAb");
        assert_eq!(
            unescape(&Node::TextJ("\\ud83d\\ude00".to_string())),
            "\u{1f600}"
        );
    }
}
