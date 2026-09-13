//! The doclist byte format: one row's or one term's postings, varint-encoded.
//!
//! Invariant: **a doclist is a run of entries, each the rowid as a delta from
//! the previous one, then per column that has a position, the column number,
//! how many positions, and the positions as deltas.** Everything is a varint,
//! which is what makes a doclist for a common word small enough to be worth
//! keeping in one row - `%_idx`'s own row, since task-1911 folded the
//! dictionary and the doclist together; see the module's own doc comment.
//!
//! This file is the codec alone: encode, decode, and the two reads that answer
//! a narrower question without decoding everything - `last_doclist_rowid` for
//! "where does this end", `doclist_rows` for "which rows have a position in
//! this column". [`mod.rs`](super) and [`expr.rs`](super::expr) hold what does
//! the reading and writing; nothing here touches a shadow table.

use inillucent_base::varint;

/// Reads one varint, advancing the offset.
///
/// `pub(super)` rather than private: `Totals` in the parent module encodes
/// the same way and is not a doclist, so it reads through here rather than
/// keeping a second copy of the same eight lines.
pub(super) fn read_varint(bytes: &[u8], at: &mut usize) -> u64 {
    let Some(rest) = bytes.get(*at..) else {
        return 0;
    };
    let Ok(decoded) = varint::decode(rest) else {
        *at = bytes.len();
        return 0;
    };
    *at = at.saturating_add(decoded.len);
    decoded.value
}

/// Appends one varint.
pub(super) fn write_varint(out: &mut Vec<u8>, value: u64) {
    let mut buffer = [0u8; 9];
    if let Ok(used) = varint::encode(&mut buffer, value) {
        out.extend_from_slice(buffer.get(..used).unwrap_or(&[]));
    }
}

/// One row's appearance in one term's doclist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocEntry {
    /// Which row.
    pub rowid: i64,
    /// The positions the term appears at, by column.
    pub columns: Vec<(usize, Vec<u32>)>,
}

/// Encodes a doclist.
pub fn encode_doclist(entries: &[DocEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous = 0i64;
    for entry in entries {
        write_varint(&mut out, entry.rowid.wrapping_sub(previous) as u64);
        previous = entry.rowid;
        write_varint(&mut out, entry.columns.len() as u64);
        for (column, positions) in &entry.columns {
            write_varint(&mut out, *column as u64);
            write_varint(&mut out, positions.len() as u64);
            let mut last = 0u32;
            for position in positions {
                write_varint(&mut out, u64::from(position.wrapping_sub(last)));
                last = *position;
            }
        }
    }
    out
}

/// Returns the last rowid in an encoded doclist, without decoding it.
///
/// Walks the same structure `decode_doclist` walks and allocates nothing: it
/// keeps the running rowid and steps over each entry's varints. It answers
/// `None` unless the walk consumes the blob exactly, which is what makes it
/// safe to act on - a doclist this cannot account for byte-for-byte is one the
/// caller falls back to decoding, rather than one it appends to on a guess.
pub fn last_doclist_rowid(bytes: &[u8]) -> Option<i64> {
    let mut at = 0usize;
    let mut rowid = 0i64;
    let mut seen = false;
    while at < bytes.len() {
        rowid = rowid.wrapping_add(read_varint(bytes, &mut at) as i64);
        let columns = read_varint(bytes, &mut at) as usize;
        if columns > 4096 || at > bytes.len() {
            return None;
        }
        for _ in 0..columns {
            let _column = read_varint(bytes, &mut at);
            let count = read_varint(bytes, &mut at) as usize;
            if count > 1 << 24 || at > bytes.len() {
                return None;
            }
            for _ in 0..count {
                let _delta = read_varint(bytes, &mut at);
            }
            if at > bytes.len() {
                return None;
            }
        }
        seen = true;
    }
    if at == bytes.len() && seen {
        Some(rowid)
    } else {
        None
    }
}

/// Appends one entry to an encoded doclist, given its rowid delta.
///
/// The bytes it writes are exactly the bytes `encode_doclist` would write for
/// the same entry in the same position, which is the property that lets the
/// fast path below produce a doclist indistinguishable from a re-encoded one.
pub fn append_doclist_entry(out: &mut Vec<u8>, delta: i64, entry: &DocEntry) {
    write_varint(out, delta as u64);
    write_varint(out, entry.columns.len() as u64);
    for (column, positions) in &entry.columns {
        write_varint(out, *column as u64);
        write_varint(out, positions.len() as u64);
        let mut last = 0u32;
        for position in positions {
            write_varint(out, u64::from(position.wrapping_sub(last)));
            last = *position;
        }
    }
}

/// Decodes a doclist.
pub fn decode_doclist(bytes: &[u8]) -> Vec<DocEntry> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    let mut rowid = 0i64;
    while at < bytes.len() {
        rowid = rowid.wrapping_add(read_varint(bytes, &mut at) as i64);
        let columns = read_varint(bytes, &mut at) as usize;
        if columns > 4096 {
            break;
        }
        let mut per_column = Vec::with_capacity(columns);
        for _ in 0..columns {
            let column = read_varint(bytes, &mut at) as usize;
            let count = read_varint(bytes, &mut at) as usize;
            if count > 1 << 24 {
                return entries;
            }
            let mut positions = Vec::with_capacity(count.min(4096));
            let mut last = 0u32;
            for _ in 0..count {
                last = last.wrapping_add(read_varint(bytes, &mut at) as u32);
                positions.push(last);
            }
            per_column.push((column, positions));
        }
        entries.push(DocEntry {
            rowid,
            columns: per_column,
        });
        if at >= bytes.len() {
            break;
        }
    }
    entries
}

/// Collects the rows a doclist names, without decoding their positions.
///
/// A row is collected when it has a position in a column the caller asked for
/// and that the table declares - the same test [`super::expr::phrase_hits`]
/// applies at offset zero, decided by walking the varints rather than by
/// building the vectors that would prove it.
///
/// @param bytes - the doclist as `%_idx` holds it
/// @param wanted - the column a `column:term` filter named, if any
/// @param columns - how many columns the table declares
/// @param out - where the rowids are appended, in doclist order
pub fn doclist_rows(bytes: &[u8], wanted: Option<usize>, columns: usize, out: &mut Vec<i64>) {
    let mut at = 0usize;
    let mut rowid = 0i64;
    while at < bytes.len() {
        rowid = rowid.wrapping_add(read_varint(bytes, &mut at) as i64);
        let count = read_varint(bytes, &mut at) as usize;
        if count > 4096 {
            break;
        }
        let mut matched = false;
        for _ in 0..count {
            let column = read_varint(bytes, &mut at) as usize;
            let positions = read_varint(bytes, &mut at) as usize;
            if positions > 1 << 24 {
                return;
            }
            for _ in 0..positions {
                let _ = read_varint(bytes, &mut at);
            }
            if positions == 0 || column >= columns {
                continue;
            }
            if wanted.is_some_and(|asked| asked != column) {
                continue;
            }
            matched = true;
        }
        if matched {
            out.push(rowid);
        }
        if at >= bytes.len() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A doclist round-trips, positions and all.
    #[test]
    fn a_doclist_round_trips() {
        let entries = vec![
            DocEntry {
                rowid: 1,
                columns: vec![(0, vec![0, 3, 9])],
            },
            DocEntry {
                rowid: 40,
                columns: vec![(0, vec![2]), (1, vec![0, 1])],
            },
        ];
        let encoded = encode_doclist(&entries);
        assert_eq!(decode_doclist(&encoded), entries);
    }
}
