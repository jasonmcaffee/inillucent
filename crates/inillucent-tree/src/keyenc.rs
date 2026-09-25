//! How a key tuple becomes the comparable bytes a tree is ordered by.
//!
//! Invariant: **the bytes this produces sort under `memcmp` in exactly the
//! order the key tuple sorts in.** That is what lets a descent compare a probe
//! against a separator with one `memcmp`, and what lets a bulk build sort by
//! the encoded bytes rather than by a second opinion about the order.
//!
//! Extracted whole from `paged.rs` in task-1932, nothing changed in the move.
//! It is one question - how a key becomes bytes - rather than a slice taken to
//! make a number fit, and `paged.rs` is one of the modules
//! `crates/inillucent-compat/tests/tooling/policy.rs` holds a line ceiling over.

use inillucent_value::collation::Collation;

use crate::datum::Datum;
use crate::key;
use crate::types::{ColumnSpec, PhysicalType};

/// How a tree's keys become comparable bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyEncoding {
    /// One `Int64` column, encoded as eight exact bytes.
    Rowid,
    /// Anything else, through the general memcmp encoding.
    General,
}

impl KeyEncoding {
    /// Chooses the encoding a column directory calls for.
    ///
    /// The rowid form is taken only for a single non-nullable `Int64` key
    /// column, because it has no way to represent a NULL, a text or a real -
    /// and a rowid, by definition, is never any of those.
    ///
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    pub fn choose(columns: &[ColumnSpec], key_columns: usize) -> KeyEncoding {
        if key_columns != 1 {
            return KeyEncoding::General;
        }
        match columns.first() {
            Some(spec) if spec.physical == PhysicalType::Int64 && !spec.nullable() => {
                KeyEncoding::Rowid
            }
            _ => KeyEncoding::General,
        }
    }

    /// Encodes a key tuple into comparable bytes.
    ///
    /// A rowid tree's probe that is not an integer falls back to the general
    /// encoding for that one comparison, which cannot match any separator and
    /// therefore lands the descent at the leftmost or rightmost leaf rather
    /// than somewhere arbitrary. The leaf's own search then answers correctly.
    ///
    /// @param self - the tree's encoding
    /// @param values - the key tuple
    pub fn encode(self, values: &[Datum<'_>]) -> Vec<u8> {
        self.encode_under(values, &[])
    }

    /// Encodes a key tuple whose columns have collations.
    ///
    /// @param values - the key tuple
    /// @param collations - one per column; short means BINARY for the rest
    pub fn encode_under(self, values: &[Datum<'_>], collations: &[Collation]) -> Vec<u8> {
        self.encode_ordered(values, collations, &[])
    }

    /// Encodes a key tuple whose columns have collations and directions.
    ///
    /// @param values - the key tuple
    /// @param collations - one per column; short means BINARY for the rest
    /// @param descending - one per column; short means ascending for the rest
    pub fn encode_ordered(
        self,
        values: &[Datum<'_>],
        collations: &[Collation],
        descending: &[bool],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(values, collations, descending, &mut out);
        out
    }

    /// Appends a key tuple's comparable bytes to a buffer.
    ///
    /// **The appending form is what lets a bulk build keep every key in one
    /// arena.** `CREATE INDEX` needs a key per row to sort by, and returning a
    /// `Vec` per row is one heap allocation per row - the exact cost that made
    /// the previous attempt at pre-encoded keys slower than the comparison sort
    /// it replaced. With a single buffer and a run of offsets there is no
    /// per-row allocation at all, and the bytes are the tree's own encoding, so
    /// sorting by `memcmp` over them is sorting by the order the tree is read
    /// in rather than by a second opinion about it.
    ///
    /// @param values - the key tuple
    /// @param collations - one per column; short means BINARY for the rest
    /// @param out - the buffer to append to
    pub fn encode_into(
        self,
        values: &[Datum<'_>],
        collations: &[Collation],
        descending: &[bool],
        out: &mut Vec<u8>,
    ) {
        match (self, values.first()) {
            (KeyEncoding::Rowid, Some(Datum::Int(number))) if values.len() == 1 => {
                out.extend_from_slice(&key::order_preserving_int(*number));
            }
            (KeyEncoding::Rowid, Some(Datum::Real(number))) if values.len() == 1 => {
                // A real probe against a rowid tree: clamp to the integer it
                // sits between, so the descent lands on the leaf that could
                // hold it rather than at an end of the tree.
                let clamped = if *number <= i64::MIN as f64 {
                    i64::MIN
                } else if *number >= i64::MAX as f64 {
                    i64::MAX
                } else {
                    number.floor() as i64
                };
                out.extend_from_slice(&key::order_preserving_int(clamped));
            }
            (KeyEncoding::Rowid, Some(Datum::Null)) => {
                out.extend_from_slice(&key::order_preserving_int(i64::MIN));
            }
            (KeyEncoding::Rowid, None) => {}
            (KeyEncoding::Rowid, Some(_)) => out.extend_from_slice(&[0xFF; 8]),
            (KeyEncoding::General, _) => {
                for (index, value) in values.iter().enumerate() {
                    let collation = collations.get(index).copied().unwrap_or(Collation::Binary);
                    let start = out.len();
                    key::encode_into_with(value, collation, out);
                    // **A descending column is its own bytes, inverted.** The
                    // encoding is order preserving, so complementing every byte
                    // of one column's span reverses that column's order and
                    // leaves every other column's alone - which is what a
                    // `DESC` key column means, and is why the comparison below
                    // and the sort the bulk build does over these bytes both
                    // come out right with no further change.
                    if descending.get(index).copied().unwrap_or(false) {
                        if let Some(span) = out.get_mut(start..) {
                            for byte in span {
                                *byte = !*byte;
                            }
                        }
                    }
                }
            }
        }
    }
}
