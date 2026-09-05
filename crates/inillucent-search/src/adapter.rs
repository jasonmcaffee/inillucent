//! The retrieval contract, stated once so that two implementations of it can be
//! compared.
//!
//! Invariant: the operations the existing engine already exposes - append a
//! document, tombstone one, replace one, build, search - keep their meaning
//! when the storage underneath them changes. The point of writing them down as
//! a trait is not abstraction for its own sake. It is that the phase this crate
//! belongs to has to prove that moving a corpus from a generation directory
//! into a database did not change what the engine answers, and "the same
//! answers" is only a checkable claim if both sides can be asked the same
//! question through the same door.
//!
//! There are exactly two implementations:
//!
//! - `inillucent_core::index::Index`, below - the direct, in-process engine, whose
//!   behaviour this phase is required *not* to change. The implementation here
//!   is a forwarding one, deliberately: every method is the call the existing
//!   callers already make.
//! - the SQL-backed one, in the crate above this that can open a database. It
//!   does the same things to rows in a `inillucent_search` table.
//!
//! An equivalence test drives both with one script and compares the rankings.

use inillucent_base::DbResult;
use inillucent_core::filter::Filter;
use inillucent_core::index::{Branches, Index};
use inillucent_core::rank::HitOrigin;
use inillucent_core::store::ChunkInput;

/// One question put to a retrieval index.
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// The query text, empty when only the vector branch should run.
    pub text: String,
    /// The query vector, empty when only the lexical branch should run.
    pub vector: Vec<f32>,
    /// How many hits to return.
    pub limit: usize,
    /// The traversal breadth, when the index is an approximate one.
    pub recall: Option<f32>,
}

/// One answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// The chunk's external identifier, which is what a caller filed it under.
    pub id: String,
    /// The fused score, which orders the list.
    pub score: f32,
    /// How good the hit is in absolute terms, in `[0, 1]`.
    pub confidence: f32,
    /// Which branch or branches produced it.
    pub origin: HitOrigin,
}

/// What a retrieval index can be asked to do.
///
/// Every method takes `&mut self`, including `search`. The direct engine does
/// not need it; a SQL-backed one does, because asking a question is running a
/// statement. Making the contract the wider of the two is what lets one test
/// drive both.
pub trait RetrievalIndex {
    /// Adds documents to a built index without rebuilding it, returning how
    /// many chunks landed.
    fn append(&mut self, chunks: Vec<ChunkInput>, embeddings: &[Vec<f32>]) -> DbResult<usize>;

    /// Makes one document unreachable, reporting whether there was one.
    ///
    /// The two implementations differ here, and the difference is real rather
    /// than an oversight. The direct engine *tombstones*: the chunk stays in
    /// the inverted index and in the corpus statistics, and is excluded when a
    /// query filters it out - which is what makes a delete cost nothing and is
    /// why `deleted_ratio` exists to say when a rebuild is worth it. A search
    /// table *deletes*: the row is gone, and the document frequencies and
    /// average length are those of the corpus that is left.
    ///
    /// Both make the document unreachable, immediately and identically. What
    /// can differ afterwards is the ordering of results deep in a list, because
    /// the two are scoring against corpora of different sizes until the legacy
    /// index is rebuilt. A caller who needs the legacy behaviour keeps the row
    /// and filters it in SQL, which is what the migration's own `document`
    /// table is for.
    fn tombstone(&mut self, source: &str, external_doc_id: &str) -> DbResult<bool>;

    /// Replaces one document's chunks, returning how many chunks landed.
    fn replace(
        &mut self,
        source: &str,
        external_doc_id: &str,
        chunks: Vec<ChunkInput>,
        embeddings: &[Vec<f32>],
    ) -> DbResult<usize>;

    /// Builds every queryable structure, so the index answers.
    fn build(&mut self) -> DbResult<()>;

    /// Answers one query.
    fn search(&mut self, query: &Query) -> DbResult<Vec<Hit>>;

    /// Returns how many live chunks the index holds.
    fn live_chunks(&mut self) -> DbResult<usize>;
}

impl RetrievalIndex for Index {
    /// Forwards to the engine's own append.
    fn append(&mut self, chunks: Vec<ChunkInput>, embeddings: &[Vec<f32>]) -> DbResult<usize> {
        Ok(Index::append(self, chunks, embeddings).chunks_added)
    }

    /// Forwards to the engine's own tombstone.
    fn tombstone(&mut self, source: &str, external_doc_id: &str) -> DbResult<bool> {
        Ok(Index::tombstone(self, source, external_doc_id))
    }

    /// Forwards to the engine's own replace.
    fn replace(
        &mut self,
        source: &str,
        external_doc_id: &str,
        chunks: Vec<ChunkInput>,
        embeddings: &[Vec<f32>],
    ) -> DbResult<usize> {
        Ok(Index::replace_document(self, source, external_doc_id, chunks, embeddings).chunks_added)
    }

    /// Forwards to the engine's own commit.
    fn build(&mut self) -> DbResult<()> {
        Index::commit(self);
        Ok(())
    }

    /// Runs the same branches the SQL path runs, through the same entry point.
    fn search(&mut self, query: &Query) -> DbResult<Vec<Hit>> {
        let filter = self.compile(&Filter::default());
        let branches = branches_for(query);
        let Some(branches) = branches else {
            return Ok(Vec::new());
        };
        let width = query.recall.and_then(|recall| {
            if recall >= 1.0 {
                Some(usize::MAX)
            } else {
                None
            }
        });
        let (hits, _) = self.search_branches(
            &query.text,
            &query.vector,
            &filter,
            query.limit.max(1),
            width,
            branches,
        );
        let store = self.store();
        Ok(hits
            .into_iter()
            .map(|hit| Hit {
                id: store.chunk_external_id(hit.chunk).to_string(),
                score: hit.score,
                confidence: hit.confidence,
                origin: hit.origin,
            })
            .collect())
    }

    /// Returns the live chunk count the store keeps.
    fn live_chunks(&mut self) -> DbResult<usize> {
        let store = self.store();
        let total = store.n_chunks();
        let deleted = (store.deleted_ratio() * total as f32).round() as usize;
        Ok(total.saturating_sub(deleted))
    }
}

/// Returns which branches a query asks for, or nothing when it asks for none.
pub fn branches_for(query: &Query) -> Option<Branches> {
    let has_text = !query.text.trim().is_empty();
    let has_vector = !query.vector.is_empty();
    match (has_text, has_vector) {
        (true, true) => Some(Branches::Both),
        (true, false) => Some(Branches::Lexical),
        (false, true) => Some(Branches::Vector),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_core::index::IndexConfig;

    fn chunk(id: &str, text: &str) -> ChunkInput {
        ChunkInput {
            source: "test".to_string(),
            external_doc_id: id.to_string(),
            chunk_index: 0,
            heading_path: Vec::new(),
            content: text.to_string(),
            title: String::new(),
            url: String::new(),
            space_key: None,
            author: None,
            author_id: None,
            updated_at: None,
            external_chunk_id: Some(id.to_string()),
            labels: Vec::new(),
            attributes: Vec::new(),
            flags: Vec::new(),
            deleted: false,
        }
    }

    /// The direct engine answers through the contract exactly as it does
    /// through its own methods.
    #[test]
    fn the_direct_engine_answers_through_the_contract() {
        let mut index = Index::new(IndexConfig {
            dims: 1,
            ..IndexConfig::default()
        });
        RetrievalIndex::build(&mut index).expect("built");
        RetrievalIndex::append(
            &mut index,
            vec![
                chunk("a", "offer eligibility rules for the discount"),
                chunk("b", "unrelated text about weather"),
            ],
            &[vec![0.0], vec![0.0]],
        )
        .expect("appended");
        let hits = RetrievalIndex::search(
            &mut index,
            &Query {
                text: "offer eligibility".to_string(),
                limit: 5,
                ..Query::default()
            },
        )
        .expect("searched");
        assert_eq!(hits.first().map(|hit| hit.id.as_str()), Some("a"));
        assert_eq!(RetrievalIndex::live_chunks(&mut index).expect("counted"), 2);
    }

    /// A tombstoned document stops being returned.
    #[test]
    fn a_tombstoned_document_stops_being_returned() {
        let mut index = Index::new(IndexConfig {
            dims: 1,
            ..IndexConfig::default()
        });
        RetrievalIndex::build(&mut index).expect("built");
        RetrievalIndex::append(
            &mut index,
            vec![chunk("a", "offer eligibility")],
            &[vec![0.0]],
        )
        .expect("appended");
        assert!(RetrievalIndex::tombstone(&mut index, "test", "a").expect("tombstoned"));
        let hits = RetrievalIndex::search(
            &mut index,
            &Query {
                text: "offer".to_string(),
                limit: 5,
                ..Query::default()
            },
        )
        .expect("searched");
        assert!(hits.is_empty());
    }

    /// A query that names neither branch asks for nothing.
    #[test]
    fn a_query_with_no_terms_runs_no_branch() {
        assert!(branches_for(&Query::default()).is_none());
    }
}
