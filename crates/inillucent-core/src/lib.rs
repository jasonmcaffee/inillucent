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
pub mod model;
pub mod persist;
pub mod quantize;
pub mod rank;
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
