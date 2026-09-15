//! The seeded twins of the three `inillucent-base` codec fuzz targets.
//!
//! Invariant: **every codec that reads bytes off a disk returns a value or an
//! error, on any input, and a change that breaks that fails a pull request.**
//! `fuzz/fuzz_targets/` holds eight libFuzzer targets over the codecs, and
//! libFuzzer needs a nightly compiler and a scheduled job: a regression that
//! only a fuzz run finds is a regression that ships. `docs/repository.md` says
//! every codec is also exercised with hundreds of thousands of seeded random
//! inputs; until task-1961's T5 that was true of four of the twelve targets -
//! `json`, `mysql`, `postgres` and `store` - and of none of the eight codecs
//! the sentence is about.
//!
//! This file is the stable-toolchain twin of `varint.rs`, `bigendian.rs` and
//! `page_header.rs`. The generator is deterministic - a fixed seed and xorshift
//! - so a failure is reproducible from the seed printed beside it, which is the
//! difference between this and a fuzz run: a fuzz corpus finds a case and then
//! has to be carried around, and a seeded sweep re-derives the same cases every
//! time on every machine.
//!
//! It is not a substitute for fuzzing and does not claim to be. libFuzzer's
//! coverage-guided search reaches inputs a blind sweep never will. What this
//! catches is a regression in the shape the targets already found once, in the
//! run that gates a merge.

use inillucent_base::bytes;
use inillucent_base::ids::PageId;
use inillucent_base::page::{self, PageSize};
use inillucent_base::varint;

/// How many inputs each sweep below reads.
const CASES: usize = 20_000;

/// Returns the next value of a deterministic generator.
///
/// xorshift64, which is the same generator the four existing `fuzz_seeded`
/// modules use, so a reader who has read one has read them all.
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

/// The varint decoder never panics, and never claims more bytes than it read.
///
/// The twin of `fuzz/fuzz_targets/varint.rs`. A varint is the first thing read
/// out of a record, so it is the first codec a corrupt page reaches.
#[test]
fn the_varint_decoder_is_total() {
    let mut state = 0x1961_0001_u64;
    let mut decoded = 0usize;
    for _ in 0..CASES {
        let input = bytes_of(&mut state, 16);
        if let Ok(value) = varint::decode(&input) {
            decoded = decoded.saturating_add(1);
            assert!(
                value.len <= input.len(),
                "decode claimed {} bytes of a {}-byte input",
                value.len,
                input.len()
            );
            assert!(value.len <= varint::MAX_LEN);
            let mut buffer = [0u8; varint::MAX_LEN];
            if varint::encoded_len(value.value) == value.len {
                let written =
                    varint::encode(&mut buffer, value.value).expect("nine bytes is enough");
                assert_eq!(
                    buffer.get(..written),
                    input.get(..value.len),
                    "decode and encode disagreed on {input:?}"
                );
            }
        }
    }
    assert!(
        decoded > CASES / 10,
        "only {decoded} of {CASES} inputs decoded at all, so the sweep exercised the \
         refusal path and nothing else"
    );
}

/// The checked big-endian readers are total at every offset.
///
/// The twin of `fuzz/fuzz_targets/bigendian.rs`. Every one of these is called
/// with an offset computed from a page's own header, which is to say with an
/// offset an attacker chooses.
#[test]
fn the_big_endian_readers_are_total() {
    let mut state = 0x1961_0002_u64;
    let mut read = 0usize;
    for _ in 0..CASES / 100 {
        let input = bytes_of(&mut state, 64);
        for offset in 0..input.len().saturating_add(9) {
            read = read.saturating_add(usize::from(bytes::read_u8(&input, offset).is_ok()));
            let _ = bytes::read_u16(&input, offset);
            let _ = bytes::read_u24(&input, offset);
            let _ = bytes::read_u32(&input, offset);
            let _ = bytes::read_u48(&input, offset);
            let _ = bytes::read_u64(&input, offset);
            let _ = bytes::read_f64(&input, offset);
        }
        let mut reader = bytes::ByteReader::new(&input);
        while reader.remaining() > 0 && reader.u8().is_ok() {}
    }
    assert!(
        read > 0,
        "no offset in any input was inside its buffer, so nothing was read"
    );
}

/// A page size off disk is validated, and every offset from it is checked.
///
/// The twin of `fuzz/fuzz_targets/page_header.rs`. A page size that was
/// believed without validation is a multiplication that lands anywhere in the
/// file.
#[test]
fn a_page_size_off_disk_is_validated_before_it_is_used() {
    let mut state = 0x1961_0003_u64;
    let mut accepted = 0usize;
    for _ in 0..CASES {
        let input = bytes_of(&mut state, 16);
        let mut encoded = [0u8; 2];
        for (slot, byte) in encoded.iter_mut().zip(input.iter()) {
            *slot = *byte;
        }
        let Ok(size) = PageSize::from_encoded(u16::from_be_bytes(encoded)) else {
            continue;
        };
        accepted = accepted.saturating_add(1);
        assert!(size.bytes().is_power_of_two(), "{} is not", size.bytes());
        assert!((512..=65_536).contains(&size.bytes()));
        for reserved in input.iter().take(8) {
            let _ = size.usable(*reserved);
        }
        let mut number = [0u8; 4];
        for (slot, byte) in number.iter_mut().zip(input.iter().skip(2)) {
            *slot = *byte;
        }
        if let Some(id) = PageId::new(u32::from_be_bytes(number)) {
            let offset =
                page::page_offset(size, id).expect("checked arithmetic cannot overflow here");
            assert_eq!(
                offset % u64::from(size.bytes()),
                0,
                "a page offset landed off a page boundary"
            );
        }
    }
    assert!(
        accepted > 0,
        "no generated header was a valid page size, so nothing was validated"
    );
}
