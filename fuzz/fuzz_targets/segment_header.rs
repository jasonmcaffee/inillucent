//! Fuzzes the HNSW and BM25 segment readers with arbitrary bytes.
//!
//! Invariant: **a count read out of a segment blob is checked against the bytes
//! remaining before anything is allocated from it.** Both readers used to size
//! an allocation straight from a header field: `hnsw.rs` from `n_nodes` and
//! three siblings, `bm25.rs` from `n_terms` as a raw `u64`, each with no
//! ceiling and no comparison against what was left to read - and both are
//! reached from an ordinary `SELECT` over an `inillucent_search` table, so the
//! bytes are whatever is in the database file (task-2066 section 4.1.12, which
//! asks for this target in section 4.4.7).
//!
//! A header claiming `u64::MAX` nodes is the case that abort the process rather
//! than refusing, and it is one input. What a fuzzer adds is every shape
//! between that and a valid segment: a count that is plausible until the third
//! list, a length that is exactly one byte past the buffer, a node count that
//! fits while the neighbour lists do not.
//!
//! The assertion is only that neither reader panics and neither runs the
//! process out of memory. A refusal is the right answer and so is a graph the
//! reader is willing to return, because a decoder is obliged to be *safe* on
//! bytes that pass its checks; being *right* about them is what the checksum
//! over the blob is for, and that is a different test.

#![no_main]

use libfuzzer_sys::fuzz_target;

use inillucent_core::bm25::Bm25Index;
use inillucent_core::hnsw::{Hnsw, HnswParams};

fuzz_target!(|data: &[u8]| {
    // The parameters are the caller's rather than the file's, so they are held
    // at something ordinary: what is under test is the header inside `data`.
    let params = HnswParams::default();
    let mut reader = data;
    let _ = Hnsw::read_graph(&mut reader, params);

    let mut reader = data;
    let _ = Bm25Index::read_from(&mut reader);
});
