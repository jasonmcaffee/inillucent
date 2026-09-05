//! Fuzzes the meta page decoder with arbitrary page bytes.
//!
//! Invariant: the meta page is the first thing read out of a file and the one
//! that says how to read everything else - its page size, its catalog root, its
//! free-map head. A file that is not a database, or one whose first page has
//! been damaged, must be refused rather than believed.

#![no_main]

use libfuzzer_sys::fuzz_target;
use inillucent_pool::meta::Meta;

fuzz_target!(|data: &[u8]| {
    let _ = Meta::decode(data);
    // The two-copy choice is the path an open actually takes, and it has to be
    // total over any pair of pages including two damaged ones.
    let half = data.len() / 2;
    let (primary, shadow) = data.split_at(half);
    let _ = Meta::choose(primary, shadow);
});
