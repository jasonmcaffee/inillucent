//! Fuzzes the MySQL wire decoders with arbitrary bytes.
//!
//! Invariant: a packet a network peer controls is either decoded or refused. It
//! never panics, never indexes past the slice, and never allocates a buffer
//! sized by a length the packet itself carries without checking it against what
//! is there. The first two of those are what this target watches for; the third
//! is the defect H4 fixed, and a target that ran out of memory instead of
//! panicking would be reporting it too.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = inillucent_remote::mysql::fuzzing::greeting(data);
    let _ = inillucent_remote::mysql::fuzzing::column(data);
    // The column count comes out of the input rather than being fixed, because
    // a row decoder's loop is bounded by it and a fixed count would only ever
    // walk the same number of fields.
    let columns = usize::from(data.first().copied().unwrap_or(0)) % 64;
    let _ = inillucent_remote::mysql::fuzzing::row(data, columns);
});
