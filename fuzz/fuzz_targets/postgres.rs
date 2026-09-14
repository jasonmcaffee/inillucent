//! Fuzzes the PostgreSQL wire decoders with arbitrary bytes.
//!
//! Invariant: a message a network peer controls is either decoded or refused,
//! and never panics or indexes past the slice. `decode_row_description` reads a
//! field count out of the message and `decode_data_row` reads a length per
//! column, which are the two numbers an attacker picks.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = inillucent_remote::postgres::fuzzing::row_description(data);
    let _ = inillucent_remote::postgres::fuzzing::data_row(data);
});
