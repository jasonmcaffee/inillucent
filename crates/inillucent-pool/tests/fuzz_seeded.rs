//! The seeded twins of the two `inillucent-pool` codec fuzz targets.
//!
//! Invariant: **the two pages whose contents are read before anything else is
//! trusted are refused when they are damaged, not believed.** The meta page
//! says how to read every other page in the file; an interior page's contents
//! are *addresses*, so one that arrives damaged and is believed sends every
//! descent below it somewhere else.
//!
//! The stable-toolchain twins of `fuzz/fuzz_targets/meta_page.rs` and
//! `interior_page.rs`, written for task-1961's T5. See
//! `crates/inillucent-base/tests/fuzz_seeded.rs` for why a seeded sweep is
//! worth having beside a fuzz target rather than instead of one.

use inillucent_pool::interior::{self, InteriorRef};
use inillucent_pool::meta::Meta;

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

/// The meta page decoder refuses anything that is not one.
///
/// The twin of `fuzz/fuzz_targets/meta_page.rs`, including the two-copy choice,
/// which is the path an open actually takes and has to be total over any pair
/// of pages including two damaged ones.
#[test]
fn the_meta_decoder_is_total_over_arbitrary_bytes() {
    let mut state = 0x1961_0021_u64;
    let mut refused = 0usize;
    for _ in 0..CASES {
        let input = bytes_of(&mut state, 256);
        refused = refused.saturating_add(usize::from(Meta::decode(&input).is_err()));
        let half = input.len() / 2;
        let (primary, shadow) = input.split_at(half);
        let _ = Meta::choose(primary, shadow);
    }
    assert!(
        refused > 0,
        "every generated buffer decoded as a meta page, which means the decoder is \
         accepting bytes no database wrote"
    );
}

/// Builds a buffer shaped like an interior page, with the fields mutated.
///
/// **Random bytes never parse as an interior page, and a sweep that only ever
/// exercises `parse`'s first refusal is a test of one `if`.** The kind byte is
/// one of six values at a fixed offset, so a blind generator reaches the
/// decoder roughly one buffer in fifteen hundred and reaches it holding a
/// header that fails the next check anyway. This starts from a page that says
/// it is an interior page and moves the four numbers the decoder reads - the
/// separator count, the key column count and the heap start - which is where a
/// decoder believes something it should have checked.
///
/// The numbers are written little-endian, which is how
/// `inillucent_pool::page::read_u16` and `read_u32` read them - a page's own
/// header is native-order where `inillucent_base::bytes` is big-endian for the
/// SQLite file format.
///
/// @param state - the generator's state, advanced in place
fn interior_shaped(state: &mut u64) -> Vec<u8> {
    /// The page kind byte, at `inillucent_pool::page::header::KIND`.
    const KIND: usize = 12;
    /// Where the separator count lives.
    const COUNT: usize = 32;
    /// Where the key column count lives.
    const KEY_COLUMNS: usize = 34;
    /// Where the heap start lives.
    const HEAP_START: usize = 36;

    let mut page = vec![0u8; 4_096];
    if let Some(slot) = page.get_mut(KIND) {
        // 1 is `PageKind::Interior`.
        *slot = 1;
    }
    // A count small enough that its directory fits, most of the time, and
    // sometimes one that does not - the refusal is worth reaching too.
    let count = (next(state) % 300) as u16;
    if let Some(slot) = page.get_mut(COUNT..COUNT + 2) {
        slot.copy_from_slice(&count.to_le_bytes());
    }
    let columns = (next(state) % 8) as u16;
    if let Some(slot) = page.get_mut(KEY_COLUMNS..KEY_COLUMNS + 2) {
        slot.copy_from_slice(&columns.to_le_bytes());
    }
    // A heap start that is inside the page most of the time, and outside it
    // sometimes.
    let heap = (next(state) % 5_000) as u32;
    if let Some(slot) = page.get_mut(HEAP_START..HEAP_START + 4) {
        slot.copy_from_slice(&heap.to_le_bytes());
    }
    // The slot array and the heap, filled with whatever the generator gives.
    for at in 40..page.len() {
        if let Some(slot) = page.get_mut(at) {
            *slot = next(state) as u8;
        }
    }
    page
}

/// The interior decoder reads no address its own header did not describe.
///
/// The twin of `fuzz/fuzz_targets/interior_page.rs`. Writeback runs over raw
/// bytes rather than a parsed page, so `swip_offsets_of` is swept the same way.
#[test]
fn the_interior_decoder_never_reads_an_address_it_was_not_given() {
    let mut state = 0x1961_0022_u64;
    let mut parsed = 0usize;
    for _ in 0..CASES / 20 {
        let input = interior_shaped(&mut state);
        let _ = interior::swip_offsets_of(&input);
        let Ok(page) = InteriorRef::parse(&input) else {
            continue;
        };
        parsed = parsed.saturating_add(1);
        let _ = page.validate();
        let _ = page.key_columns();
        let _ = page.level();
        for slot in 0..page.count().min(256) {
            let _ = page.key(slot);
        }
        for child in 0..page.children().min(256) {
            let _ = page.swip(child);
            let _ = page.swip_offset(child);
        }
        let _ = page.rightmost();
        let _ = page.swip_offsets();
        for probe in [&b""[..], b"\x00", b"\xFF\xFF\xFF\xFF", b"key"] {
            let _ = page.child_for(probe);
        }
    }
    assert!(
        parsed > 0,
        "no generated buffer parsed as an interior page, so nothing behind `parse` was read"
    );
}

/// Arbitrary bytes reach the decoder and never get past its first check.
///
/// The other half: a buffer that is not shaped like a page at all has to be
/// refused, and `swip_offsets_of` runs over raw bytes rather than a parsed
/// page, so it is swept the same way.
#[test]
fn arbitrary_bytes_are_refused_rather_than_parsed() {
    let mut state = 0x1961_0023_u64;
    let mut refused = 0usize;
    for _ in 0..CASES {
        let input = bytes_of(&mut state, 512);
        let _ = interior::swip_offsets_of(&input);
        if InteriorRef::parse(&input).is_err() {
            refused = refused.saturating_add(1);
            continue;
        }
    }
    assert!(
        refused > CASES / 2,
        "only {refused} of {CASES} arbitrary buffers were refused as interior pages"
    );
}
