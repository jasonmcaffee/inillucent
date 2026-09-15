//! The seeded twins of the two `inillucent-tree` codec fuzz targets.
//!
//! Invariant: **a leaf page that arrived from anywhere is read through its own
//! header and never past it, and a key's bytes order the way its values do.**
//! The first is the contract every page decoder has; the second is the one that
//! makes the memcmp encoding worth having, and an encoding coarser than the
//! comparison it stands for is a wrong answer rather than a slow one - two
//! distinct rowids that encode alike send a descent to one leaf and collapse
//! into one group.
//!
//! The stable-toolchain twins of `fuzz/fuzz_targets/leaf_page.rs` and
//! `memcmp_key.rs`, written for task-1961's T5. See
//! `crates/inillucent-base/tests/fuzz_seeded.rs` for why a seeded sweep is
//! worth having beside a fuzz target rather than instead of one.

use inillucent_tree::datum::Datum;
use inillucent_tree::key;
use inillucent_tree::leaf::{compare_rows, LeafRef};

/// How many inputs each sweep below reads.
const CASES: usize = 20_000;

/// Returns the next value of a deterministic generator.
///
/// @param state - the generator's state, advanced in place
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Returns a buffer of up to `most` pseudo-random bytes.
///
/// @param state - the generator's state, advanced in place
/// @param most - the longest buffer to produce
fn bytes_of(state: &mut u64, most: usize) -> Vec<u8> {
    let length = (next(state) as usize) % most.max(1);
    (0..length).map(|_| next(state) as u8).collect()
}

/// Builds one value out of a slice of the input.
///
/// The same five-way split `fuzz/fuzz_targets/memcmp_key.rs` uses, so the two
/// sweeps cover the same shapes.
///
/// @param bytes - the slice to read a value out of
fn value(bytes: &[u8]) -> Datum<'_> {
    match bytes.first().copied().unwrap_or(0) % 5 {
        0 => Datum::Null,
        1 => {
            let mut raw = [0u8; 8];
            for (slot, byte) in raw.iter_mut().zip(bytes.iter().skip(1)) {
                *slot = *byte;
            }
            Datum::Int(i64::from_le_bytes(raw))
        }
        2 => {
            let mut raw = [0u8; 8];
            for (slot, byte) in raw.iter_mut().zip(bytes.iter().skip(1)) {
                *slot = *byte;
            }
            Datum::Real(f64::from_le_bytes(raw))
        }
        3 => Datum::Text(bytes.get(1..).unwrap_or(&[])),
        _ => Datum::Blob(bytes.get(1..).unwrap_or(&[])),
    }
}

/// Builds a buffer shaped like a PAX leaf, with the header numbers mutated.
///
/// **Random bytes never parse as a leaf, and a sweep that only ever exercises
/// the first refusal is a test of one `if`.** `parse` checks nine things before
/// it builds a `LeafRef`, so a blind generator is refused by the kind byte and
/// never reaches the other eight. This writes a page that says it is a leaf and
/// then moves the six numbers the decoder reads, so the sweep lands on the
/// checks that matter: a column directory that runs past the page, a heap start
/// outside it, a delta area that starts before the directory ends.
///
/// The numbers are little-endian, which is how `inillucent_pool::page::read_u16`
/// and `read_u32` read a page's own header.
///
/// @param state - the generator's state, advanced in place
fn leaf_shaped(state: &mut u64) -> Vec<u8> {
    /// The page kind byte.
    const KIND: usize = 12;
    /// The common flags byte.
    const FLAGS: usize = 13;
    /// Where the row count lives.
    const ROW_COUNT: usize = 32;
    /// Where the delta count lives.
    const DELTA_COUNT: usize = 34;
    /// Where the column count lives.
    const COLUMN_COUNT: usize = 36;
    /// Where the key column count lives.
    const KEY_COLUMNS: usize = 38;
    /// Where the heap start lives.
    const HEAP_START: usize = 40;
    /// Where the delta area start lives.
    const DELTA_START: usize = 44;
    /// Bit 2 of the flags byte: the delta area holds rows.
    const HAS_DELTA: u8 = 0b0000_0100;

    let mut page = vec![0u8; 4_096];
    // Everything after the header first, so the writes below survive it.
    for at in 64..page.len() {
        if let Some(slot) = page.get_mut(at) {
            *slot = next(state) as u8;
        }
    }
    let delta_count = (next(state) % 40) as u16;
    let flags = if delta_count > 0 { HAS_DELTA } else { 0 };
    let put16 = |page: &mut Vec<u8>, at: usize, value: u16| {
        if let Some(slot) = page.get_mut(at..at.saturating_add(2)) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    };
    let put32 = |page: &mut Vec<u8>, at: usize, value: u32| {
        if let Some(slot) = page.get_mut(at..at.saturating_add(4)) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    };
    if let Some(slot) = page.get_mut(KIND) {
        // 2 is `PageKind::Leaf`.
        *slot = 2;
    }
    if let Some(slot) = page.get_mut(FLAGS) {
        *slot = flags;
    }
    put16(&mut page, ROW_COUNT, (next(state) % 400) as u16);
    put16(&mut page, DELTA_COUNT, delta_count);
    let columns = 1 + (next(state) % 24) as u16;
    put16(&mut page, COLUMN_COUNT, columns);
    put16(
        &mut page,
        KEY_COLUMNS,
        1 + (next(state) % u64::from(columns)) as u16,
    );
    // The directory ends at 64 + columns * 8 for a narrow page. A delta area
    // between there and the heap, and a heap inside the page, most of the time.
    let directory_end = 64u32.saturating_add(u32::from(columns).saturating_mul(8));
    let delta_start = directory_end.saturating_add((next(state) % 512) as u32);
    put32(&mut page, DELTA_START, delta_start);
    put32(
        &mut page,
        HEAP_START,
        delta_start.saturating_add((next(state) % 3_000) as u32),
    );
    page
}

/// The PAX leaf decoder reads nothing its header did not describe.
///
/// The twin of `fuzz/fuzz_targets/leaf_page.rs`, and like it this does not
/// merely parse: it parses and then reads everything the header claims,
/// because a decoder that validates its header and then trusts an offset
/// inside it is the shape of every page-decoder bug there has ever been.
#[test]
fn the_leaf_decoder_never_reads_past_its_own_header() {
    let mut state = 0x1961_0011_u64;
    let mut parsed = 0usize;
    for _ in 0..CASES / 20 {
        let input = leaf_shaped(&mut state);
        let Ok(leaf) = LeafRef::parse(&input) else {
            continue;
        };
        parsed = parsed.saturating_add(1);
        let _ = leaf.integrity();
        let rows = leaf.row_count().min(256);
        let columns = leaf.column_count().min(32);
        for row in 0..rows {
            let _ = leaf.is_tombstoned(row);
            for column in 0..columns {
                let _ = leaf.value(row, column);
            }
        }
        for entry in 0..leaf.delta_count().min(32) {
            let _ = leaf.delta_row(entry);
            for column in 0..columns {
                let _ = leaf.delta_value(entry, column);
            }
        }
        for column in 0..columns {
            if let Ok(mini) = leaf.column(column) {
                let _ = mini.any_exception();
                let _ = mini.all_typed();
                for row in 0..rows.min(32) {
                    let _ = mini.class_at(row);
                    let _ = mini.value(row);
                    let _ = mini.heap_slice(row);
                }
            }
        }
        let _ = leaf.search(&[Datum::Int(0)]);
        let _ = leaf.search(&[Datum::Text(b"probe")]);
        let _ = leaf.lower_bound(&[Datum::Int(1)]);
        let _ = leaf.upper_bound(&[Datum::Int(1)]);
        let _ = leaf.live();
    }
    // **A count, not a comment.** A generator whose bytes never form a page
    // that parses would exercise `parse`'s refusal and nothing behind it, and
    // the sweep would report green having read no page at all - which is the
    // defect class task-1961's T1 is about, in a test rather than a gate.
    assert!(
        parsed > 0,
        "no generated buffer parsed as a leaf, so nothing behind `parse` was read"
    );
}

/// Arbitrary bytes are refused rather than parsed as a leaf.
///
/// The other half of the contract: a buffer that is not shaped like a page has
/// to be turned away at the header rather than read.
#[test]
fn arbitrary_bytes_are_refused_rather_than_parsed_as_a_leaf() {
    let mut state = 0x1961_0012_u64;
    let mut refused = 0usize;
    for _ in 0..CASES {
        let input = bytes_of(&mut state, 512);
        if LeafRef::parse(&input).is_err() {
            refused = refused.saturating_add(1);
        }
    }
    assert!(
        refused > CASES / 2,
        "only {refused} of {CASES} arbitrary buffers were refused as leaves"
    );
}

/// Comparing two keys as bytes gives the answer comparing their values gives.
///
/// The twin of `fuzz/fuzz_targets/memcmp_key.rs`. NaN is excluded rather than
/// asserted on: it is not orderable and the encoding puts it at one end
/// deliberately.
#[test]
fn the_key_encoding_orders_the_way_the_values_do() {
    let mut state = 0x1961_0013_u64;
    let mut compared = 0usize;
    for _ in 0..CASES {
        let input = bytes_of(&mut state, 32);
        if input.len() < 4 {
            continue;
        }
        let half = input.len() / 2;
        let (left_bytes, right_bytes) = input.split_at(half);
        let left = [value(left_bytes)];
        let right = [value(right_bytes)];
        let has_nan = [left[0], right[0]]
            .iter()
            .any(|held| matches!(held, Datum::Real(number) if number.is_nan()));
        if has_nan {
            continue;
        }
        compared = compared.saturating_add(1);
        let by_value = compare_rows(&left, &right, 1);
        let by_bytes = key::encode(&left)
            .as_bytes()
            .cmp(key::encode(&right).as_bytes());
        assert_eq!(
            by_value, by_bytes,
            "{left:?} against {right:?}: the values order one way and their keys the other"
        );
    }
    assert!(
        compared > CASES / 10,
        "only {compared} of {CASES} pairs were compared, so the sweep asserted almost nothing"
    );
}
