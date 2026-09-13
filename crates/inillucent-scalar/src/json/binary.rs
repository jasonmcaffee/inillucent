//! The JSONB binary format.
//!
//! Invariant: a blob this module writes is byte-identical to the one the pinned
//! release writes for the same input, and a blob it reads is understood the
//! same way. This is not an internal representation that happens to be
//! persisted - `jsonb()` hands the bytes to the application, applications store
//! them in columns, and a database written by one engine is opened by the
//! other. A format that were merely equivalent would fail the first cross-open
//! fixture.
//!
//! The encoding is one header per element. The header's low four bits are the
//! element type and its high four bits are the payload size when the size is
//! eleven or less; 12, 13, 14 and 15 mean the size follows as one, two, four or
//! eight big-endian bytes. Containers carry their children in their payload,
//! which is what makes the whole document one contiguous blob with no pointers
//! and no alignment.

use inillucent_base::DbResult;

use super::node::Node;

/// `null`.
pub const TYPE_NULL: u8 = 0;
/// `true`.
pub const TYPE_TRUE: u8 = 1;
/// `false`.
pub const TYPE_FALSE: u8 = 2;
/// An RFC-8259 integer, as its text.
pub const TYPE_INT: u8 = 3;
/// A JSON5 integer, as its text.
pub const TYPE_INT5: u8 = 4;
/// An RFC-8259 float, as its text.
pub const TYPE_FLOAT: u8 = 5;
/// A JSON5 float, as its text.
pub const TYPE_FLOAT5: u8 = 6;
/// String content that needs no translation in either direction.
pub const TYPE_TEXT: u8 = 7;
/// String content carrying JSON escapes.
pub const TYPE_TEXTJ: u8 = 8;
/// String content carrying JSON5 escapes.
pub const TYPE_TEXT5: u8 = 9;
/// String content that still has to be escaped to become JSON.
pub const TYPE_TEXTRAW: u8 = 10;
/// An array.
pub const TYPE_ARRAY: u8 = 11;
/// An object.
pub const TYPE_OBJECT: u8 = 12;

/// Appends one element to a blob.
pub fn encode(node: &Node, out: &mut Vec<u8>) {
    match node {
        Node::Null => header(TYPE_NULL, 0, out),
        Node::True => header(TYPE_TRUE, 0, out),
        Node::False => header(TYPE_FALSE, 0, out),
        Node::Int(text) => payload(TYPE_INT, text.as_bytes(), out),
        Node::Int5(text) => payload(TYPE_INT5, text.as_bytes(), out),
        Node::Float(text) => payload(TYPE_FLOAT, text.as_bytes(), out),
        Node::Float5(text) => payload(TYPE_FLOAT5, text.as_bytes(), out),
        Node::Text(text) => payload(TYPE_TEXT, text.as_bytes(), out),
        Node::TextJ(text) => payload(TYPE_TEXTJ, text.as_bytes(), out),
        Node::Text5(text) => payload(TYPE_TEXT5, text.as_bytes(), out),
        Node::TextRaw(text) => payload(TYPE_TEXTRAW, text.as_bytes(), out),
        Node::Array(items) => {
            let mut body = Vec::new();
            for item in items {
                encode(item, &mut body);
            }
            payload(TYPE_ARRAY, &body, out);
        }
        Node::Object(members) => {
            let mut body = Vec::new();
            for (label, value) in members {
                encode(label, &mut body);
                encode(value, &mut body);
            }
            payload(TYPE_OBJECT, &body, out);
        }
    }
}

/// Returns the whole blob for one document.
pub fn to_blob(node: &Node) -> Vec<u8> {
    let mut out = Vec::new();
    encode(node, &mut out);
    out
}

/// Writes a header whose payload follows.
fn payload(kind: u8, body: &[u8], out: &mut Vec<u8>) {
    header(kind, body.len(), out);
    out.extend_from_slice(body);
}

/// Writes the smallest header that can describe a payload of this size.
fn header(kind: u8, size: usize, out: &mut Vec<u8>) {
    let kind = kind & 0x0f;
    if size <= 11 {
        out.push(((size as u8) << 4) | kind);
    } else if size <= 0xff {
        out.push(0xc0 | kind);
        out.push(size as u8);
    } else if size <= 0xffff {
        out.push(0xd0 | kind);
        out.extend_from_slice(&(size as u16).to_be_bytes());
    } else if size <= 0xffff_ffff {
        out.push(0xe0 | kind);
        out.extend_from_slice(&(size as u32).to_be_bytes());
    } else {
        out.push(0xf0 | kind);
        out.extend_from_slice(&(size as u64).to_be_bytes());
    }
}

/// One element's header, as read from a blob.
#[derive(Clone, Copy, Debug)]
pub struct ElementHeader {
    /// The element type in the low four bits of the first byte.
    pub kind: u8,
    /// How many bytes the header itself occupies.
    pub header_size: usize,
    /// How many payload bytes follow it.
    pub payload_size: usize,
}

impl ElementHeader {
    /// Returns the offset one past the whole element.
    pub fn end(&self, at: usize) -> usize {
        at.saturating_add(self.header_size)
            .saturating_add(self.payload_size)
    }
}

/// Reads one element header at an offset.
pub fn read_header(blob: &[u8], at: usize) -> DbResult<ElementHeader> {
    let first = *blob.get(at).ok_or_else(malformed)?;
    let kind = first & 0x0f;
    let marker = first >> 4;
    let (header_size, payload_size) = match marker {
        0..=11 => (1usize, usize::from(marker)),
        12 => (
            2usize,
            usize::from(*blob.get(at + 1).ok_or_else(malformed)?),
        ),
        13 => {
            // `try_from` on the slice rather than indexing it: `get` already
            // proved the length, and this is the form that says so to the
            // compiler as well as to a reader (task-1932, H9).
            let bytes = blob
                .get(at + 1..at + 3)
                .and_then(|head| <[u8; 2]>::try_from(head).ok())
                .ok_or_else(malformed)?;
            (3usize, usize::from(u16::from_be_bytes(bytes)))
        }
        14 => {
            let bytes = blob
                .get(at + 1..at + 5)
                .and_then(|head| <[u8; 4]>::try_from(head).ok())
                .ok_or_else(malformed)?;
            let size = u32::from_be_bytes(bytes);
            (5usize, size as usize)
        }
        15 => {
            let bytes = blob.get(at + 1..at + 9).ok_or_else(malformed)?;
            let mut size = [0u8; 8];
            size.copy_from_slice(bytes);
            let size = u64::from_be_bytes(size);
            if size > u64::from(u32::MAX) {
                return Err(malformed());
            }
            (9usize, size as usize)
        }
        _ => return Err(malformed()),
    };
    let header = ElementHeader {
        kind,
        header_size,
        payload_size,
    };
    if header.end(at) > blob.len() {
        return Err(malformed());
    }
    Ok(header)
}

/// Decodes one whole blob into a tree.
pub fn from_blob(blob: &[u8]) -> DbResult<Node> {
    let (node, end) = decode(blob, 0, 0)?;
    if end != blob.len() {
        return Err(malformed());
    }
    Ok(node)
}

/// Reports whether a blob is a well-formed JSONB document.
pub fn is_valid(blob: &[u8]) -> bool {
    from_blob(blob).is_ok()
}

/// Decodes one element, returning it and the offset past it.
fn decode(blob: &[u8], at: usize, depth: usize) -> DbResult<(Node, usize)> {
    if depth > 1000 {
        return Err(malformed());
    }
    let header = read_header(blob, at)?;
    let body_at = at.saturating_add(header.header_size);
    let body_end = body_at.saturating_add(header.payload_size);
    let body = blob.get(body_at..body_end).ok_or_else(malformed)?;
    let node = match header.kind {
        TYPE_NULL if body.is_empty() => Node::Null,
        TYPE_TRUE if body.is_empty() => Node::True,
        TYPE_FALSE if body.is_empty() => Node::False,
        TYPE_INT => Node::Int(text(body)?),
        TYPE_INT5 => Node::Int5(text(body)?),
        TYPE_FLOAT => Node::Float(text(body)?),
        TYPE_FLOAT5 => Node::Float5(text(body)?),
        TYPE_TEXT => Node::Text(text(body)?),
        TYPE_TEXTJ => Node::TextJ(text(body)?),
        TYPE_TEXT5 => Node::Text5(text(body)?),
        TYPE_TEXTRAW => Node::TextRaw(text(body)?),
        TYPE_ARRAY => {
            let mut items = Vec::new();
            let mut cursor = body_at;
            while cursor < body_end {
                let (item, next) = decode(blob, cursor, depth + 1)?;
                if next <= cursor || next > body_end {
                    return Err(malformed());
                }
                items.push(item);
                cursor = next;
            }
            Node::Array(items)
        }
        TYPE_OBJECT => {
            let mut members = Vec::new();
            let mut cursor = body_at;
            while cursor < body_end {
                let (label, after_label) = decode(blob, cursor, depth + 1)?;
                if !label.is_text() || after_label >= body_end {
                    return Err(malformed());
                }
                let (value, next) = decode(blob, after_label, depth + 1)?;
                if next <= cursor || next > body_end {
                    return Err(malformed());
                }
                members.push((label, value));
                cursor = next;
            }
            Node::Object(members)
        }
        _ => return Err(malformed()),
    };
    Ok((node, body_end))
}

/// Returns a payload as text, refusing bytes that are not UTF-8.
fn text(body: &[u8]) -> DbResult<String> {
    String::from_utf8(body.to_vec()).map_err(|_| malformed())
}

/// Returns the error every malformed document reports.
fn malformed() -> inillucent_base::DbError {
    inillucent_base::DbError::primary(inillucent_base::PrimaryCode::Error)
        .with_detail("malformed JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    /// Encodes a document from its text.
    fn blob(text: &str) -> Vec<u8> {
        to_blob(&parse::parse(text).expect("parses").node)
    }

    /// Renders bytes as upper-case hexadecimal, as `hex()` does.
    fn hex(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>()
    }

    /// The encodings the pinned release produces for the simple values.
    ///
    /// These are transcribed from `hex(jsonb(...))` run against the pinned
    /// 3.53.4 shell, which is the only thing that makes them a parity claim
    /// rather than a restatement of this file.
    #[test]
    fn simple_values_match_the_pinned_release() {
        assert_eq!(hex(&blob("null")), "00");
        assert_eq!(hex(&blob("true")), "01");
        assert_eq!(hex(&blob("false")), "02");
        assert_eq!(hex(&blob("1")), "1331");
        assert_eq!(hex(&blob("1.5")), "35312E35");
        assert_eq!(hex(&blob("\"ab\"")), "276162");
        assert_eq!(hex(&blob("[1,2]")), "4B13311332");
        assert_eq!(hex(&blob("{\"a\":1}")), "4C17611331");
        assert_eq!(hex(&blob("[]")), "0B");
        assert_eq!(hex(&blob("{}")), "0C");
    }

    /// The JSON5 element types, and the two spellings that are converted.
    #[test]
    fn json5_values_match_the_pinned_release() {
        assert_eq!(
            hex(&blob("{a:1, b:0x10, c:.5, d:+Infinity, e:NaN, f:'sq'}")),
            "CC2017611331176244307831301763262E35176455396539393917650017662773\
             71"
            .replace([' ', '\n'], "")
        );
    }

    /// A payload longer than eleven bytes takes the one-byte size header.
    #[test]
    fn a_long_payload_takes_a_wider_header() {
        let long = "\"".to_string() + &"x".repeat(20) + "\"";
        let encoded = blob(&long);
        assert_eq!(encoded[0], 0xc7);
        assert_eq!(encoded[1], 20);
    }

    /// Every encoding round-trips through the decoder unchanged.
    #[test]
    fn encodings_round_trip() {
        for text in [
            "null",
            "[1,2,[3,{\"a\":null}]]",
            "{\"a\":\"b\\\"c\",\"d\":[true,false]}",
            "{a:0x10, b:.5}",
        ] {
            let node = parse::parse(text).expect("parses").node;
            let bytes = to_blob(&node);
            assert_eq!(from_blob(&bytes).expect("decodes"), node, "{text}");
        }
    }

    /// A truncated or nonsensical blob is refused rather than misread.
    #[test]
    fn malformed_blobs_are_refused() {
        assert!(from_blob(&[]).is_err());
        assert!(from_blob(&[0x4b, 0x13]).is_err());
        assert!(from_blob(&[0x0d]).is_err());
        assert!(from_blob(&[0x1c, 0x00]).is_err());
        assert!(from_blob(&[0x11]).is_err());
    }

    /// An object whose label is not text is not a document.
    #[test]
    fn an_object_label_must_be_text() {
        assert!(from_blob(&[0x4c, 0x13, 0x31, 0x13, 0x32]).is_err());
    }
}
