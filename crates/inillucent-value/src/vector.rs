//! A vector written as text, and the bytes one is stored as.
//!
//! Invariant: **there is one parser for `'[1, 0, 0]'`, and every path that
//! accepts that spelling reaches it.** There were three (task-2066 §4.1.2), and
//! the one path that had none of them is the one `docs/vector-search.md`
//! documents: the HNSW probe read the TEXT literal's raw bytes and divided the
//! byte length by four, so `'[1,0,0]'` - seven bytes - was a one dimension
//! vector and was refused against a three dimension index.
//!
//! What made it invisible is worth stating, because it is the shape rather than
//! the bug. Without the index the same statement is answered correctly by a
//! different parser, so an application develops against a small table, adds the
//! index for speed, and every vector query starts failing. And every test of
//! the indexed probe in `vector.rs` and `vector_metric.rs` builds its query
//! vector as a hex blob through a local `literal()` helper, so the whole vector
//! suite was green while the documented spelling failed.
//!
//! This crate is where the parser lives because it is the one crate all four
//! callers already depend on: `inillucent-scalar` (layer 5),
//! `inillucent-exec`, `inillucent-search` (layer 7) and `inillucent-cli` each
//! name `inillucent-value` (layer 1) today. `docs/invariants/layering.toml`
//! does not let `inillucent-search` depend on `inillucent-scalar`, so putting
//! it there would have meant widening the dependency contract to close a
//! defect - which is the wrong trade when a lower crate serves everybody
//! without one.

/// Returns the numbers of a JSON array, or `None` for anything else.
///
/// A hand parser rather than the JSON reader, because the whole grammar here is
/// `[` a comma separated list of numbers `]`: anything with a string, an
/// object, a nested array or a name in it is not a vector, and answering `None`
/// for it is what leaves the caller's ordinary refusal in place.
///
/// **An empty array answers `Some(vec![])` rather than `None`**, because
/// "this text is not a vector" and "this text is a vector of nothing" are
/// different answers and only the caller knows which one matters. A width
/// checked caller rejects it against the declared width; a caller with no
/// declared width rejects it itself.
///
/// @param text - the value's bytes
#[must_use]
pub fn numbers_from_json(text: &[u8]) -> Option<Vec<f64>> {
    let held = std::str::from_utf8(text).ok()?.trim();
    let inner = held.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut numbers = Vec::new();
    for part in inner.split(',') {
        numbers.push(part.trim().parse::<f64>().ok()?);
    }
    Some(numbers)
}

/// Returns a vector written as a JSON array of numbers, at 32-bit width.
///
/// `None` for text that is not a JSON array of numbers, and for the empty
/// array: a vector of no dimensions cannot be compared with anything, so no
/// caller of this wants one.
///
/// @param text - the value's bytes
#[must_use]
pub fn vector_from_json(text: &[u8]) -> Option<Vec<f32>> {
    let numbers = numbers_from_json(text)?;
    if numbers.is_empty() {
        return None;
    }
    Some(numbers.into_iter().map(|number| number as f32).collect())
}

/// Returns the bytes a vector is stored as: little-endian 32-bit floats.
///
/// The same bytes `inillucent_search` writes, which is what makes a column of
/// vectors and the retrieval store's own copies interchangeable.
///
/// @param numbers - the vector's components
#[must_use]
pub fn encode(numbers: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(numbers.len().saturating_mul(4));
    for number in numbers {
        bytes.extend_from_slice(&number.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spelling `docs/vector-search.md` documents reads as three numbers.
    ///
    /// Seven bytes, three dimensions. The defect was a reader that answered
    /// `7 / 4 = 1`, so this is the case that separates the two.
    #[test]
    fn the_documented_spelling_is_three_dimensions_not_one() {
        let found = vector_from_json(b"[1,0,0]").expect("a JSON array of numbers");
        assert_eq!(found, vec![1.0, 0.0, 0.0]);
    }

    /// Spacing and decimal points do not change the width.
    ///
    /// `'[1.0,0.0,0]'` is eleven bytes, so a byte length reader called it two
    /// dimensions - a different wrong answer from the same cause, which is why
    /// both spellings are here.
    #[test]
    fn spacing_and_decimals_do_not_change_the_width() {
        for spelling in [
            &b"[1.0,0.0,0]"[..],
            b"[ 1 , 0 , 0 ]",
            b"  [1, 0.0, 0e0]  ",
            b"[1e0,0,-0.0]",
        ] {
            let found = vector_from_json(spelling).expect("a JSON array of numbers");
            assert_eq!(
                found.len(),
                3,
                "{} read as {} dimensions",
                String::from_utf8_lossy(spelling),
                found.len()
            );
        }
    }

    /// Text that is not a JSON array of numbers is not a vector.
    ///
    /// The other half of the contract: the fallback to raw bytes is only safe
    /// while this answers `None` for everything that is not an array of
    /// numbers, so a blob whose bytes happen to begin with `[` is still read as
    /// a blob.
    #[test]
    fn anything_that_is_not_an_array_of_numbers_is_not_a_vector() {
        for spelling in [
            &b"[1, \"two\", 3]"[..],
            b"[[1,2],[3,4]]",
            b"{\"a\": 1}",
            b"1,0,0",
            b"[1,0,0",
            b"1,0,0]",
            b"",
            b"[]",
            b"[1,,0]",
            b"[0x01,0,0]",
        ] {
            assert!(
                vector_from_json(spelling).is_none(),
                "{} was read as a vector",
                String::from_utf8_lossy(spelling)
            );
        }
    }

    /// An empty array is numbers-of-nothing, not "not an array".
    ///
    /// The one place the two entry points disagree on purpose. `insert.rs`
    /// compares the count against the declared width and wants to say "zero
    /// where the column declares three"; every other caller wants `None`.
    #[test]
    fn the_empty_array_is_an_array_of_no_numbers() {
        assert_eq!(numbers_from_json(b"[]"), Some(Vec::new()));
        assert_eq!(numbers_from_json(b"[ ]"), Some(Vec::new()));
        assert!(vector_from_json(b"[]").is_none());
    }

    /// The bytes are the ones the index stores.
    #[test]
    fn the_encoding_is_little_endian_thirty_two_bit() {
        assert_eq!(
            encode(&[1.0, 0.0, 0.0]),
            vec![0x00, 0x00, 0x80, 0x3F, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert!(encode(&[]).is_empty());
    }

    /// Text in, bytes out, and the width survives.
    ///
    /// The round trip the probe path needs: the documented literal has to
    /// produce exactly the twelve bytes a hex blob of the same vector does,
    /// because the two spellings must answer the same rows.
    #[test]
    fn the_documented_literal_encodes_to_the_same_bytes_as_its_hex_blob() {
        let parsed = vector_from_json(b"[1,0,0]").expect("a JSON array of numbers");
        let hex = vec![0x00u8, 0x00, 0x80, 0x3F, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(encode(&parsed), hex);
    }
}
