//! The retrieval engine: HNSW vectors, a BM25 inverted index, and the store
//! that addresses both.
//!
//! Invariant: **this crate decodes the retrieval index straight off the
//! database file, so every read of it is a read of bytes somebody else could
//! have written.** It sits under `inillucent-engine` and is reached on an
//! ordinary `SELECT` through `inillucent-search`'s segment loader. It was
//! outside every lint and policy check the engine crates are held to until
//! task-1932: no `deny(missing_docs)`, none of the four panic lints, and no
//! module here stated an invariant. That is the wrong crate to leave
//! unexamined, which is the whole of the argument for the attributes below.

#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod binio;
pub mod bm25;
pub mod distance;
pub mod embed;
#[cfg(feature = "onnx")]
pub mod embed_onnx;
pub mod filter;
pub mod flat;
pub mod hnsw;
pub mod index;
pub mod install;
pub mod model;
pub mod persist;
pub mod quantize;
pub mod rank;
pub mod residency;
pub mod store;
pub mod tokenize;
pub mod vectors;

/// An `Index` can be shared across threads, asserted at compile time.
///
/// Every search takes `&self` and every field is plain owned data, so this should
/// hold - but "should hold" is how a server discovers at run time that a
/// dependency stopped being `Sync`. `Tokenizer` wraps a third-party stemmer, which
/// is exactly the kind of thing that changes underneath a version bump. Asserting
/// it here means that change breaks the build instead of the deployment.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<index::Index>();
    assert_send_sync::<tokenize::Tokenizer>();
    assert_send_sync::<store::Store>();
};
