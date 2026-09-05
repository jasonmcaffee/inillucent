//! The JSON value tree, in the shape the binary format stores it.
//!
//! Invariant: a node remembers the text it was parsed from. `json('{"a":1.50}')`
//! answers `{"a":1.50}` and not `{"a":1.5}`, and `jsonb('{"b":0x10}')` stores
//! the four bytes `0x10` rather than the two bytes `16`, because the pinned
//! release stores what it read and converts only where the output format
//! demands it. A tree of `f64` and `String` would be a different database:
//! every number would be renormalised on the way through, and every JSONB blob
//! would differ from the one SQLite writes for the same input.
//!
//! The variants are therefore the binary format's element types rather than
//! JSON's six value kinds, which is what makes the encoder a walk rather than a
//! translation.

/// One node of a parsed JSON document.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// `null`.
    Null,
    /// `true`.
    True,
    /// `false`.
    False,
    /// An integer in RFC-8259 spelling, stored as that spelling.
    Int(String),
    /// An integer only JSON5 accepts: hexadecimal, or with a leading `+`.
    Int5(String),
    /// A float in RFC-8259 spelling.
    Float(String),
    /// A float only JSON5 accepts: a bare leading or trailing point.
    Float5(String),
    /// String content that needs no escaping in either direction.
    Text(String),
    /// String content carrying JSON escapes, stored with the backslashes.
    TextJ(String),
    /// String content carrying escapes only JSON5 accepts.
    Text5(String),
    /// Raw text from SQL, which has to be escaped when it is rendered.
    TextRaw(String),
    /// An array.
    Array(Vec<Node>),
    /// An object, as label/value pairs. A label is always one of the text
    /// kinds; the parser refuses anything else.
    Object(Vec<(Node, Node)>),
}

impl Node {
    /// Returns the name `json_type()` gives this node.
    pub fn type_name(&self) -> &'static str {
        match self {
            Node::Null => "null",
            Node::True => "true",
            Node::False => "false",
            Node::Int(_) | Node::Int5(_) => "integer",
            Node::Float(_) | Node::Float5(_) => "real",
            Node::Text(_) | Node::TextJ(_) | Node::Text5(_) | Node::TextRaw(_) => "text",
            Node::Array(_) => "array",
            Node::Object(_) => "object",
        }
    }

    /// Returns whether the node is a text node of any spelling.
    pub fn is_text(&self) -> bool {
        matches!(
            self,
            Node::Text(_) | Node::TextJ(_) | Node::Text5(_) | Node::TextRaw(_)
        )
    }

    /// Returns whether the node holds children.
    pub fn is_container(&self) -> bool {
        matches!(self, Node::Array(_) | Node::Object(_))
    }

    /// Builds a text node for SQL text that is about to be rendered as JSON.
    ///
    /// `json_object`, `json_array` and the group aggregates escape what they
    /// are given on the way in, so the content they store is already JSON: it
    /// is `TEXT` when nothing needed escaping and `TEXTJ` when something did.
    pub fn text_escaped(content: &str) -> Node {
        if content.chars().all(needs_no_escape) {
            Node::Text(content.to_string())
        } else {
            Node::TextJ(escape(content))
        }
    }

    /// Builds a text node for SQL text stored into an existing document.
    ///
    /// `json_insert`, `json_set`, `json_replace` and `json_patch` store the
    /// bytes they were handed and mark them `TEXTRAW`, which is the format
    /// saying "this has not been escaped yet". The two spellings render
    /// identically; they differ in the blob, and the blob is what a cross-open
    /// fixture compares.
    pub fn text_raw(content: &str) -> Node {
        Node::TextRaw(content.to_string())
    }
}

/// Returns whether a character can sit in a JSON string with no escape.
fn needs_no_escape(character: char) -> bool {
    character != '"' && character != '\\' && character >= ' ' && character != '\u{7f}'
}

/// Escapes SQL text into the body of a JSON string.
pub fn escape(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for character in content.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            character if character < ' ' || character == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clean text needs no escaping and takes the smallest element type.
    #[test]
    fn clean_text_is_a_plain_text_node() {
        assert_eq!(Node::text_escaped("ab"), Node::Text("ab".to_string()));
    }

    /// A quote forces the escaped spelling, and the backslash is stored.
    #[test]
    fn a_quote_forces_the_escaped_spelling() {
        assert_eq!(
            Node::text_escaped("b\"c"),
            Node::TextJ("b\\\"c".to_string())
        );
    }

    /// A control character is escaped as its four-digit code point.
    #[test]
    fn control_characters_are_escaped_numerically() {
        assert_eq!(escape("a\u{1}b"), "a\\u0001b");
        assert_eq!(escape("a\nb"), "a\\nb");
    }

    /// The stored spelling of a value written by SQL is raw, whatever it holds.
    #[test]
    fn stored_sql_text_is_always_raw() {
        assert_eq!(Node::text_raw("v"), Node::TextRaw("v".to_string()));
    }
}
