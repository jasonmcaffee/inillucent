//! Fuzzes the search index decoders with arbitrary bytes.
//!
//! Invariant: the segment list, the merge state and a vector are read out of
//! the database file, so their input is whatever is on disk - a file somebody
//! else could write to, and a file a crash left half written. Each either
//! decodes or refuses, and `decode_vector` never claims more floats than the
//! bytes could carry.

#![no_main]

use libfuzzer_sys::fuzz_target;
use inillucent_search::store;

fuzz_target!(|data: &[u8]| {
    let _ = store::decode_segments(data);
    let _ = store::decode_merge_state(data);
    let _ = store::decode_merge_states(data);
    let floats = store::decode_vector(data);
    assert!(floats.len() <= data.len() / 4, "decode_vector read past its input");
});
