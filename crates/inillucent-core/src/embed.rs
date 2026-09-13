//! The embedder boundary.
//!
//! The engine never embeds anything itself. It takes vectors, which is what lets
//! the grading harness give both engines the identical vectors: the corpus is
//! embedded once and the same bytes go to the cache and to the database. If each
//! engine embedded independently, a score difference could come from the embedder,
//! and the harness would be measuring the embedding model instead of the index.
//!
//! Invariant: **the engine never embeds anything itself.** It takes vectors,
//! which is what lets a comparison hand both engines the identical bytes: a
//! score difference then comes from retrieval and cannot come from the
//! embedder.

use crate::distance::truncate_normalized;

/// The full width `nomic-embed-text-v1.5` outputs.
pub const NOMIC_DIMS: usize = 768;
/// Widths the Matryoshka training makes usable. A prefix of the embedding is
/// itself an embedding, once renormalized.
pub const MATRYOSHKA_WIDTHS: &[usize] = &[64, 128, 256, 512, 768];

/// Whatever turns text into vectors.
///
/// A document and a query are embedded by different calls because the model is
/// trained with a different prefix for each, and using one for the other
/// measurably degrades retrieval.
pub trait Embedder {
    /// Embeds a batch of documents.
    ///
    /// @param texts - the documents, already chunked
    fn embed_documents(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;

    /// Embeds one query.
    ///
    /// @param text - the query
    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>>;

    /// Returns how wide the vectors this embedder produces are.
    fn dimensions(&self) -> usize;
}

/// The prefixes `nomic-embed-text-v1.5` is trained with. Omitting them measurably
/// degrades retrieval, and the baseline applies exactly these.
pub fn document_prefix(text: &str) -> String {
    format!("search_document: {text}")
}

/// Returns a query with the prefix the model is trained to see on one.
///
/// @param text - the query
pub fn query_prefix(text: &str) -> String {
    format!("search_query: {text}")
}

/// An embedder over vectors that already exist, optionally narrowed to a
/// Matryoshka width. This is what the harness uses.
pub struct PrecomputedEmbedder {
    dims: usize,
}

impl PrecomputedEmbedder {
    /// Returns an embedder that narrows to one width.
    ///
    /// @param dims - the width, which must be one of [`MATRYOSHKA_WIDTHS`]
    pub fn new(dims: usize) -> Self {
        PrecomputedEmbedder { dims }
    }

    /// Narrow a stored 768 dimensional vector to this embedder's width.
    pub fn narrow(&self, v: &[f32]) -> Vec<f32> {
        if v.len() == self.dims {
            return v.to_vec();
        }
        truncate_normalized(v, self.dims)
    }
}

impl Embedder for PrecomputedEmbedder {
    fn embed_documents(&self, _texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("PrecomputedEmbedder holds existing vectors and cannot embed new text")
    }

    fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("PrecomputedEmbedder holds existing vectors and cannot embed new text")
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distance::dot;

    #[test]
    fn prefixes_match_what_the_model_expects() {
        assert_eq!(document_prefix("hello"), "search_document: hello");
        assert_eq!(query_prefix("hello"), "search_query: hello");
    }

    #[test]
    fn narrowing_produces_a_unit_vector_at_every_width() {
        let mut v: Vec<f32> = (0..NOMIC_DIMS).map(|i| (i as f32 * 0.01).sin()).collect();
        crate::distance::normalize(&mut v);
        for w in MATRYOSHKA_WIDTHS {
            let e = PrecomputedEmbedder::new(*w);
            let n = e.narrow(&v);
            assert_eq!(n.len(), *w);
            assert!((dot(&n, &n) - 1.0).abs() < 1e-5, "width {w}");
        }
    }

    #[test]
    fn narrowing_to_the_full_width_is_a_no_op() {
        let mut v: Vec<f32> = (0..NOMIC_DIMS).map(|i| (i as f32).cos()).collect();
        crate::distance::normalize(&mut v);
        let e = PrecomputedEmbedder::new(NOMIC_DIMS);
        assert_eq!(e.narrow(&v), v);
    }

    #[test]
    fn a_precomputed_embedder_refuses_to_embed_text() {
        let e = PrecomputedEmbedder::new(768);
        assert!(e.embed_query("anything").is_err());
        assert!(e.embed_documents(&["anything".to_string()]).is_err());
    }
}
