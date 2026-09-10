pub mod binio;
pub mod distance;
pub mod filter;
pub mod flat;
pub mod hnsw;
pub mod store;
pub mod vectors;
pub mod bm25;
pub mod tokenize;
pub mod quantize;
pub mod rank;
pub mod embed;
pub mod model;
pub mod index;
pub mod install;
pub mod persist;
pub mod residency;
#[cfg(feature = "onnx")]
pub mod embed_onnx;

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
