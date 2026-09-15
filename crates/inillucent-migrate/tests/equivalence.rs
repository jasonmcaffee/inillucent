//! The same script, run against the direct engine and against a database.
//!
//! Invariant: the two implementations of `RetrievalIndex` answer identically.
//! That is what "the legacy direct index API is preserved through an adapter"
//! has to mean if it is to mean anything checkable - not that a similar API
//! exists, but that a caller who swapped one for the other would not be able to
//! tell from the answers.
//!
//! The corpus is deliberately one chunk per document and holds no tombstone.
//! Both of those are conditions under which the two engines are *supposed* to
//! agree exactly, and saying so is the point rather than a hedge: the legacy
//! engine caps how many chunks of one document a result may hold, and a search
//! table has one row per document, so a corpus with several chunks per document
//! is one where the two answer different questions on purpose. That difference
//! is verified where it belongs, in the migration's own grouped check.

use std::path::{Path, PathBuf};

use inillucent_core::distance::normalize;
use inillucent_core::index::{Index, IndexConfig};
use inillucent_core::store::ChunkInput;
use inillucent_migrate::copy;
use inillucent_migrate::index::SqlIndex;
use inillucent_search::adapter::{Query, RetrievalIndex};

/// How wide the vectors are.
const DIMS: usize = 6;

/// The words the corpus is made of.
const WORDS: [&str; 10] = [
    "eligibility",
    "discount",
    "account",
    "launch",
    "forecast",
    "schedule",
    "renewal",
    "invoice",
    "threshold",
    "supplier",
];

/// Returns a fresh scratch directory.
fn scratch(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("_agent_output/equivalence")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    root
}

/// Builds one chunk, filed as its own document so no cap can bind.
fn chunk(ordinal: usize) -> ChunkInput {
    let text = (0..4)
        .map(|offset| {
            WORDS
                .get((ordinal + offset * 3) % WORDS.len())
                .copied()
                .unwrap_or("word")
        })
        .collect::<Vec<&str>>()
        .join(" ");
    ChunkInput {
        source: "inillucent_search".to_string(),
        external_doc_id: ordinal.to_string(),
        chunk_index: 0,
        heading_path: Vec::new(),
        content: format!("{text} in row {ordinal}"),
        title: String::new(),
        url: String::new(),
        space_key: None,
        author: None,
        author_id: None,
        updated_at: None,
        external_chunk_id: Some(ordinal.to_string()),
        labels: Vec::new(),
        attributes: Vec::new(),
        flags: Vec::new(),
        deleted: false,
    }
}

/// Returns one chunk's vector.
fn vector(ordinal: usize) -> Vec<f32> {
    let mut values: Vec<f32> = (0..DIMS)
        .map(|dimension| (((ordinal * DIMS + dimension) as f32) * 0.41).cos())
        .collect();
    normalize(&mut values);
    values
}

/// Opens an empty database holding one search table.
fn open_sql(root: &Path, dims: usize) -> SqlIndex {
    let path = root.join("corpus.db");
    {
        let database =
            inillucent_engine::connect::Database::open(&path).expect("the database opens");
        copy::create_schema(&database.session(), dims).expect("the schema builds");
    }
    SqlIndex::open(&path, copy::SEARCH_TABLE).expect("the search table opens")
}

/// Returns the ids of one query's hits, in order.
fn ids(index: &mut dyn RetrievalIndex, query: &Query) -> Vec<String> {
    index
        .search(query)
        .expect("the search runs")
        .into_iter()
        .map(|hit| hit.id)
        .collect()
}

/// Returns the scores of one query's hits, in order.
fn scores(index: &mut dyn RetrievalIndex, query: &Query) -> Vec<f32> {
    index
        .search(query)
        .expect("the search runs")
        .into_iter()
        .map(|hit| hit.score)
        .collect()
}

/// Runs the same script against both and compares at every step.
#[test]
fn the_two_implementations_answer_identically() {
    let root = scratch("script");
    let mut direct = Index::new(IndexConfig {
        dims: DIMS,
        ..IndexConfig::default()
    });
    RetrievalIndex::build(&mut direct).expect("the direct engine builds");
    let mut sql = open_sql(&root, DIMS);

    let first: Vec<ChunkInput> = (0..40).map(chunk).collect();
    let embeddings: Vec<Vec<f32>> = (0..40).map(vector).collect();
    assert_eq!(
        RetrievalIndex::append(&mut direct, first.clone(), &embeddings).expect("appended"),
        RetrievalIndex::append(&mut sql, first, &embeddings).expect("appended")
    );
    RetrievalIndex::build(&mut direct).expect("built");
    RetrievalIndex::build(&mut sql).expect("built");

    let queries = [
        Query {
            text: "eligibility discount".to_string(),
            limit: 8,
            ..Query::default()
        },
        Query {
            text: "renewal".to_string(),
            limit: 5,
            ..Query::default()
        },
        Query {
            vector: vector(7),
            limit: 6,
            ..Query::default()
        },
        Query {
            text: "schedule invoice".to_string(),
            vector: vector(11),
            limit: 6,
            ..Query::default()
        },
    ];
    for query in &queries {
        assert_eq!(
            ids(&mut direct, query),
            ids(&mut sql, query),
            "the two rankings differ for {query:?}"
        );
        let left = scores(&mut direct, query);
        let right = scores(&mut sql, query);
        assert_eq!(left.len(), right.len());
        for (a, b) in left.iter().zip(right.iter()) {
            assert!((a - b).abs() < 1.0e-5, "{a} against {b} for {query:?}");
        }
    }

    assert_eq!(
        RetrievalIndex::live_chunks(&mut direct).expect("counted"),
        RetrievalIndex::live_chunks(&mut sql).expect("counted")
    );
}

/// A tombstone makes the same row unreachable in both, and the two then score
/// against corpora of different sizes.
///
/// This is the one place the two implementations legitimately part company, and
/// the test says so precisely rather than asserting something weaker and
/// calling it agreement. The direct engine tombstones - the chunk stays in the
/// inverted index and is filtered at query time - so its document frequencies
/// and average length are unchanged. A search table deletes, so its statistics
/// are those of the corpus that is left. Both make the document unreachable
/// immediately; what can then differ is the ordering of results far enough down
/// a list for a few hundredths of a BM25 score to matter.
#[test]
fn a_tombstone_removes_the_same_row_from_both() {
    let root = scratch("tombstone");
    let mut direct = Index::new(IndexConfig {
        dims: DIMS,
        ..IndexConfig::default()
    });
    RetrievalIndex::build(&mut direct).expect("built");
    let mut sql = open_sql(&root, DIMS);
    let chunks: Vec<ChunkInput> = (0..20).map(chunk).collect();
    let embeddings: Vec<Vec<f32>> = (0..20).map(vector).collect();
    RetrievalIndex::append(&mut direct, chunks.clone(), &embeddings).expect("appended");
    RetrievalIndex::append(&mut sql, chunks, &embeddings).expect("appended");

    assert!(RetrievalIndex::tombstone(&mut direct, "inillucent_search", "7").expect("tombstoned"));
    assert!(RetrievalIndex::tombstone(&mut sql, "inillucent_search", "7").expect("tombstoned"));
    assert!(!RetrievalIndex::tombstone(&mut sql, "inillucent_search", "999").expect("absent"));

    // Deep enough to hold every row that matches at all, so what is compared
    // is which rows are reachable rather than where a truncation happened to
    // fall.
    let query = Query {
        text: "eligibility launch account".to_string(),
        limit: 64,
        ..Query::default()
    };
    let mut left = ids(&mut direct, &query);
    let mut right = ids(&mut sql, &query);
    assert!(!left.contains(&"7".to_string()), "{left:?}");
    assert!(!right.contains(&"7".to_string()), "{right:?}");
    assert_eq!(
        left.first(),
        right.first(),
        "the leader is the same row: {left:?} against {right:?}"
    );
    left.sort();
    right.sort();
    assert_eq!(
        left, right,
        "the same rows are reachable, whatever the deep ordering"
    );
}

/// A replace re-indexes the row on both sides.
#[test]
fn a_replace_reindexes_on_both_sides() {
    let root = scratch("replace");
    let mut direct = Index::new(IndexConfig {
        dims: DIMS,
        ..IndexConfig::default()
    });
    RetrievalIndex::build(&mut direct).expect("built");
    let mut sql = open_sql(&root, DIMS);
    let chunks: Vec<ChunkInput> = (0..16).map(chunk).collect();
    let embeddings: Vec<Vec<f32>> = (0..16).map(vector).collect();
    RetrievalIndex::append(&mut direct, chunks.clone(), &embeddings).expect("appended");
    RetrievalIndex::append(&mut sql, chunks, &embeddings).expect("appended");

    let mut replacement = chunk(3);
    replacement.content = "tirzepatide dosing schedule for the trial".to_string();
    let replacement_vector = vec![vector(3)];
    RetrievalIndex::replace(
        &mut direct,
        "inillucent_search",
        "3",
        vec![replacement.clone()],
        &replacement_vector,
    )
    .expect("replaced");
    RetrievalIndex::replace(
        &mut sql,
        "inillucent_search",
        "3",
        vec![replacement],
        &replacement_vector,
    )
    .expect("replaced");

    let query = Query {
        text: "tirzepatide".to_string(),
        limit: 5,
        ..Query::default()
    };
    assert_eq!(ids(&mut direct, &query), vec!["3".to_string()]);
    assert_eq!(ids(&mut sql, &query), vec!["3".to_string()]);
}

/// A query that names neither branch returns nothing from either.
#[test]
fn an_empty_query_returns_nothing_from_either() {
    let root = scratch("empty");
    let mut direct = Index::new(IndexConfig {
        dims: DIMS,
        ..IndexConfig::default()
    });
    RetrievalIndex::build(&mut direct).expect("built");
    let mut sql = open_sql(&root, DIMS);
    let query = Query {
        limit: 5,
        ..Query::default()
    };
    assert!(ids(&mut direct, &query).is_empty());
    assert!(ids(&mut sql, &query).is_empty());
}
