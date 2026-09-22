//! Migrating a real legacy index, and proving the original never moved.
//!
//! Invariant: the source directory is byte-identical before and after. Every
//! test here digests it on the way in and on the way out, because "copy and
//! verify, never delete" is the property the whole tool exists to have and it
//! is the one a reviewer cannot check by reading.
//!
//! The corpus is small but not simple: it has documents with several chunks,
//! labels, attributes, flags, a tombstoned document, and vectors - because each
//! of those is a separate thing the copy can lose, and a corpus of plain text
//! rows would prove only that plain text rows survive.

use std::path::{Path, PathBuf};

use inillucent_base::hash::Sha256;
use inillucent_core::distance::normalize;
use inillucent_core::filter::Filter;
use inillucent_core::index::{Branches, Index, IndexConfig};
use inillucent_core::store::ChunkInput;
use inillucent_migrate::manifest::Manifest;
use inillucent_migrate::{migrate, Plan};

/// How wide the test vectors are.
const DIMS: usize = 8;

/// Returns a fresh scratch directory for one scenario.
fn scratch(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("_agent_output/migrate")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    root
}

/// Builds one chunk of the test corpus.
fn chunk(document: usize, index: u32, text: &str, deleted: bool) -> ChunkInput {
    ChunkInput {
        source: if document.is_multiple_of(2) {
            "notes"
        } else {
            "mail"
        }
        .to_string(),
        external_doc_id: format!("doc-{document}"),
        chunk_index: index,
        heading_path: vec![format!("Section {index}")],
        content: text.to_string(),
        title: format!("Document {document}"),
        url: format!("https://example.test/{document}"),
        space_key: Some("ENG".to_string()),
        author: Some(
            if document.is_multiple_of(3) {
                "Ada"
            } else {
                "Grace"
            }
            .to_string(),
        ),
        author_id: Some(format!("u{}", document % 3)),
        updated_at: Some(1_700_000_000 + document as i64),
        external_chunk_id: Some(format!("doc-{document}-{index}")),
        labels: vec!["design".to_string(), format!("team-{}", document % 2)],
        attributes: vec![(
            "participant".to_string(),
            vec![format!("person{}@example.test", document % 4)],
        )],
        flags: if document.is_multiple_of(5) {
            vec!["has_attachment".to_string()]
        } else {
            Vec::new()
        },
        deleted,
    }
}

/// The words the corpus is built from, so queries drawn from it find things.
const VOCABULARY: [&str; 12] = [
    "eligibility",
    "discount",
    "account",
    "launch",
    "forecast",
    "schedule",
    "renewal",
    "invoice",
    "tirzepatide",
    "threshold",
    "quarterly",
    "supplier",
];

/// Builds and saves a legacy index directory.
fn build_source(directory: &Path, documents: usize, chunks_each: u32) -> Index {
    let mut inputs = Vec::new();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut ordinal = 0usize;
    for document in 0..documents {
        // Every fourth document is tombstoned, so the copy has to carry
        // tombstones and the retrieval comparison has to exclude them.
        let deleted = document % 4 == 3;
        for index in 0..chunks_each {
            let words: Vec<&str> = (0..5)
                .map(|offset| {
                    VOCABULARY
                        .get((ordinal + offset * 3) % VOCABULARY.len())
                        .copied()
                        .unwrap_or("word")
                })
                .collect();
            let text = format!(
                "{} about {} and the {} for {}",
                words.join(" "),
                document,
                index,
                ordinal
            );
            inputs.push(chunk(document, index, &text, deleted));
            let mut vector: Vec<f32> = (0..DIMS)
                .map(|dimension| (((ordinal * DIMS + dimension) as f32) * 0.37).sin())
                .collect();
            normalize(&mut vector);
            vectors.push(vector);
            ordinal += 1;
        }
    }
    let mut index = Index::new(IndexConfig {
        dims: DIMS,
        ..IndexConfig::default()
    });
    index.add(inputs, &vectors).expect("the chunks are added");
    index.commit();
    inillucent_core::persist::save(&index, directory).expect("the legacy index saves");
    index
}

/// Returns a digest of every file under a directory, so "unchanged" is checkable.
fn tree_digest(directory: &Path) -> String {
    let mut files: Vec<PathBuf> = Vec::new();
    collect(directory, &mut files);
    files.sort();
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.to_string_lossy().as_bytes());
        if let Ok(bytes) = std::fs::read(&file) {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
    }
    hasher.hex()
}

/// Adds every file under a directory to a list.
fn collect(directory: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, into);
        } else {
            into.push(path);
        }
    }
}

/// A real index migrates, verifies every check, and publishes.
#[test]
fn a_legacy_index_migrates_and_every_check_passes() {
    let root = scratch("full");
    let source_dir = root.join("index");
    let built = build_source(&source_dir, 24, 3);
    let before = tree_digest(&source_dir);

    let plan = Plan::new(&source_dir, root.join("corpus.db"));
    let outcome = migrate(&plan).expect("the migration runs");

    for check in &outcome.checks {
        assert!(check.passed, "{} failed: {}", check.name, check.detail);
    }
    // The legacy store makes a fresh document whenever an existing one is
    // tombstoned, so the document count is a property of the corpus rather than
    // of the loop that wrote it - and the copy has to carry whatever it is.
    assert_eq!(outcome.documents, built.store().n_documents() as u64);
    assert_eq!(outcome.chunks, 72);
    assert_eq!(
        outcome.published.as_deref(),
        Some(plan.destination.as_path())
    );
    assert!(plan.destination.is_file());
    assert!(!plan.staging.exists(), "the staging file was renamed away");

    // The whole point: the source is exactly what it was.
    assert_eq!(
        tree_digest(&source_dir),
        before,
        "the migration wrote to the source"
    );
    assert!(
        inillucent_core::persist::load(&source_dir).is_ok(),
        "the source still opens"
    );
}

/// The manifest records the source, the destination and every verification.
#[test]
fn the_manifest_is_the_rollback_record() {
    let root = scratch("manifest");
    let source_dir = root.join("index");
    build_source(&source_dir, 8, 2);
    let plan = Plan::new(&source_dir, root.join("corpus.db"));
    let outcome = migrate(&plan).expect("the migration runs");
    assert!(outcome.verified());

    let manifest = Manifest::open(&outcome.manifest).expect("the manifest reads");
    assert_eq!(
        manifest.last("source.path"),
        Some(source_dir.display().to_string().as_str())
    );
    assert_eq!(
        manifest.all("source.file").len(),
        5,
        "every section digested"
    );
    assert!(manifest.finished("schema"));
    assert!(manifest.finished("build"));
    assert!(manifest.finished("verify"));
    assert!(manifest.finished("publish"));
    assert!(manifest.last("source.retained").is_some());
    assert!(!manifest.verifications().is_empty());
    assert!(manifest
        .verifications()
        .iter()
        .all(|(_, passed, _)| *passed));

    let report = root.join("corpus.db.migration-report.md");
    let text = std::fs::read_to_string(&report).expect("the report is written");
    assert!(text.contains("## Verification"), "{text}");
    assert!(text.contains("## Rollback"));
}

/// An interrupted migration resumes from the batch it had committed.
#[test]
fn an_interrupted_migration_resumes() {
    let root = scratch("resume");
    let source_dir = root.join("index");
    let source = build_source(&source_dir, 20, 2);
    let plan = Plan::new(&source_dir, root.join("corpus.db"));

    // Do by hand exactly what an interrupted run would have left behind: the
    // manifest header, the schema, and one committed batch of documents.
    {
        let mut manifest = Manifest::open(&plan.manifest).expect("the manifest opens");
        let inventory =
            inillucent_migrate::source::Source::open(&source_dir).expect("the source opens");
        manifest.record("manifest", "1").expect("recorded");
        manifest
            .record("source.path", source_dir.display().to_string())
            .expect("recorded");
        manifest
            .record("source.generation", inventory.generation_name())
            .expect("recorded");
        for line in inventory.manifest_lines() {
            manifest.record("source.file", line).expect("recorded");
        }
        let database =
            inillucent_engine::connect::Database::open(&plan.staging).expect("the staging opens");
        let connection = database.session();
        inillucent_migrate::copy::create_schema(&connection, DIMS).expect("the schema builds");
        manifest.record("stage", "schema").expect("recorded");
        inillucent_migrate::copy::copy_documents(&connection, source.store(), &mut manifest)
            .expect("documents copy");
    }
    let partial = Manifest::open(&plan.manifest).expect("the manifest reads");
    assert_eq!(
        partial.checkpoint("document"),
        source.store().n_documents() as u64
    );
    assert_eq!(partial.checkpoint("chunk"), 0, "nothing was chunked yet");

    let outcome = migrate(&plan).expect("the migration resumes");
    for check in &outcome.checks {
        assert!(check.passed, "{} failed: {}", check.name, check.detail);
    }
    assert!(plan.destination.is_file());
}

/// A source that has been rebuilt since the migration started is refused.
#[test]
fn a_source_that_moved_is_refused_rather_than_half_copied() {
    let root = scratch("moved");
    let source_dir = root.join("index");
    build_source(&source_dir, 6, 2);
    let plan = Plan::new(&source_dir, root.join("corpus.db"));
    {
        let mut manifest = Manifest::open(&plan.manifest).expect("the manifest opens");
        manifest
            .record("source.file", "store.bin 1 deadbeef")
            .expect("recorded");
    }
    let refused = migrate(&plan);
    assert!(refused.is_err(), "{refused:?}");
    let message = refused.err().unwrap_or_default();
    assert!(message.contains("changed"), "{message}");
}

/// A destination that already exists is never written over.
#[test]
fn an_existing_destination_is_never_overwritten() {
    let root = scratch("existing");
    let source_dir = root.join("index");
    build_source(&source_dir, 4, 1);
    let destination = root.join("corpus.db");
    std::fs::write(&destination, b"not a database").expect("the file is written");
    let plan = Plan::new(&source_dir, &destination);
    let refused = migrate(&plan);
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        std::fs::read(&destination).expect("it still reads"),
        b"not a database",
        "the file was left alone"
    );
}

/// Rolling back is opening the source, which never changed.
#[test]
fn rolling_back_is_opening_the_source() {
    let root = scratch("rollback");
    let source_dir = root.join("index");
    let original = build_source(&source_dir, 10, 2);
    let expected = original.store().n_chunks();
    let plan = Plan::new(&source_dir, root.join("corpus.db"));
    let outcome = migrate(&plan).expect("the migration runs");
    assert!(outcome.verified());
    assert!(plan.destination.is_file());

    // The application decides the destination is wrong and goes back. There is
    // nothing to undo: the directory is the one it always was.
    let reopened = inillucent_core::persist::load(&source_dir).expect("the source reopens");
    assert_eq!(reopened.store().n_chunks(), expected);
    let filter = reopened.compile(&inillucent_core::filter::Filter::default());
    assert!(!reopened.lexical_search("discount", &filter, 5).is_empty());
}

/// A lexical-only source migrates too, with no vector column.
#[test]
fn a_lexical_only_source_migrates() {
    let root = scratch("lexical");
    let source_dir = root.join("index");
    let mut index = Index::new(IndexConfig {
        dims: 1,
        ..IndexConfig::default()
    });
    let inputs: Vec<ChunkInput> = (0..12)
        .map(|ordinal| {
            chunk(
                ordinal / 2,
                (ordinal % 2) as u32,
                &format!(
                    "{} and {} in row {ordinal}",
                    VOCABULARY
                        .get(ordinal % VOCABULARY.len())
                        .copied()
                        .unwrap_or("word"),
                    VOCABULARY
                        .get((ordinal + 5) % VOCABULARY.len())
                        .copied()
                        .unwrap_or("word")
                ),
                false,
            )
        })
        .collect();
    let vectors: Vec<Vec<f32>> = (0..12).map(|_| vec![1.0f32]).collect();
    index.add(inputs, &vectors).expect("the chunks are added");
    index.commit();
    inillucent_core::persist::save(&index, &source_dir).expect("the legacy index saves");

    let plan = Plan::new(&source_dir, root.join("corpus.db"));
    let outcome = migrate(&plan).expect("the migration runs");
    for check in &outcome.checks {
        assert!(check.passed, "{} failed: {}", check.name, check.detail);
    }
}

/// How many documents the tombstone-ranking corpus holds.
///
/// Large enough that a probe drawn from it matches more chunks than
/// `Bm25Index::top_k` rescores - the engine's default is `candidates` of 50
/// times a `rescore_depth_factor` of 6, so 300 - because that window is the
/// mechanism this corpus exists to exercise and below it the two orderings
/// cannot differ.
const RANKING_DOCUMENTS: usize = 400;

/// Builds a corpus where excluding the tombstoned chunks changes the order.
///
/// Every chunk holds the same handful of terms, at a distance apart and a
/// length that both vary with the ordinal, so BM25 orders them and the position
/// rescore moves them. Every eleventh document is tombstoned, which is the rate
/// the release-sized corpus next door uses.
///
/// Deliberately synthetic and deliberately fixed. The neighbouring `corpus`
/// suite builds its corpus out of the repository's own prose at run time, which
/// is what found this defect and is worth keeping - but it also means the
/// condition comes and goes as the repository's text changes, so it cannot be
/// the thing that holds the fix. This corpus is the same shape every run.
/// @param directory - where the legacy index is written
fn build_ranking_source(directory: &Path) -> Index {
    let mut inputs = Vec::new();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    for document in 0..RANKING_DOCUMENTS {
        let deleted = document % 11 == 10;
        let filler: String = (0..(document % 17))
            .map(|n| format!("filler{n} "))
            .collect();
        let tail: String = (0..(document % 7)).map(|n| format!("tail{n} ")).collect();
        let text = format!("alpha {filler}beta {tail}gamma delta epsilon body{document}");
        let mut input = chunk(document, 0, &text, deleted);
        input.external_chunk_id = Some(format!("{document}"));
        inputs.push(input);
        let mut vector: Vec<f32> = (0..DIMS)
            .map(|dimension| (((document * DIMS + dimension) as f32) * 0.37).sin())
            .collect();
        normalize(&mut vector);
        vectors.push(vector);
    }
    let mut index = Index::new(IndexConfig {
        dims: DIMS,
        ..IndexConfig::default()
    });
    index.add(inputs, &vectors).expect("the chunks are added");
    index.commit();
    inillucent_core::persist::save(&index, directory).expect("the legacy index saves");
    index
}

/// Returns the legacy engine's lexical ranking for one query.
///
/// @param index - the legacy index
/// @param query - the query text
/// @param include_deleted - whether a tombstoned document's chunks may answer
/// @param k - how many hits to ask for
fn legacy_ranking(index: &Index, query: &str, include_deleted: bool, k: usize) -> Vec<u32> {
    let filter = index.compile(&Filter {
        include_deleted,
        ..Filter::default()
    });
    let (hits, _) = index
        .search_branches(query, &[], &filter, k, None, Branches::Lexical)
        .expect("the legacy index answers");
    hits.iter().map(|hit| hit.chunk).collect()
}

/// Excluding a tombstoned chunk during the search is not the same answer as
/// removing it afterwards, and this corpus is built so that it shows.
///
/// **The precondition of the test below, asserted rather than assumed**
/// (task-2067). `a_tombstoned_corpus_migrates_and_ranks_identically` is only
/// worth running on a corpus where the two paths disagree; on any other corpus
/// it would pass whether the migration filtered inside the search or after it,
/// and would hold nothing. So this measures the disagreement first, and fails
/// if a later change to the engine's ranking makes this corpus degenerate.
///
/// The mechanism is `Bm25Index::top_k`. Proximity and phrase rescoring reaches
/// `k * rescore_depth_factor` hits, only ever lowers a score, and leaves
/// everything below that window at its full score to compete against rescored
/// ones - so which hits are inside the window depends on which chunks the scan
/// admitted. Nothing else about the scoring moves: the inverse document
/// frequency comes from the whole posting list, the mean length from the whole
/// corpus, and the coverage share from the query's own terms, so each chunk's
/// score is what it was either way.
#[test]
fn removing_a_tombstoned_chunk_after_the_search_changes_the_order() {
    let root = scratch("ranking-precondition");
    let index = build_ranking_source(&root.join("index"));
    let store = index.store();
    let dead: Vec<u32> = (0..store.n_chunks() as u32)
        .filter(|chunk| {
            store
                .doc_of(*chunk)
                .and_then(|document| store.documents.get(document as usize))
                .is_some_and(|document| document.deleted)
        })
        .collect();
    assert!(!dead.is_empty(), "the corpus holds tombstoned chunks");

    let query = "gamma delta epsilon body264 alpha filler0";
    let inside = legacy_ranking(&index, query, false, 10);
    let afterwards: Vec<u32> = legacy_ranking(&index, query, true, 10)
        .into_iter()
        .filter(|chunk| !dead.contains(chunk))
        .collect();
    assert_ne!(
        inside, afterwards,
        "this corpus no longer exercises the rescore window, so the migration \
         check built on it would hold nothing"
    );
}

/// A corpus whose tombstones change the ranking migrates, and every check
/// passes.
///
/// **The regression test for task-2067.** `filter.deleted` used to ask the two
/// sides different questions - the legacy index with its filter applied during
/// the search, the copy with no filter at all, its tombstoned rows dropped from
/// the answer and a deeper draw taken to make up the shortfall. On this corpus
/// that came back with one hit of ten in common. It passes now because the copy
/// carries the flag on the search table's own `live` facet, which the module
/// compiles into the filter the scan runs under, so both sides exclude the same
/// rows at the same point in the same pipeline.
#[test]
fn a_tombstoned_corpus_migrates_and_ranks_identically() {
    let root = scratch("ranking");
    let source_dir = root.join("index");
    let built = build_ranking_source(&source_dir);
    let before = tree_digest(&source_dir);

    let plan = Plan::new(&source_dir, root.join("corpus.db"));
    let outcome = migrate(&plan).expect("the migration runs");

    for check in &outcome.checks {
        assert!(check.passed, "{} failed: {}", check.name, check.detail);
    }
    // Named rather than left to the loop above: a check that stopped being
    // produced at all would otherwise pass this test silently, and these two
    // are the ones it exists for.
    for name in ["filter.deleted", "filter.unreachable"] {
        assert!(
            outcome.checks.iter().any(|check| check.name == name),
            "{name} was not among the checks that ran"
        );
    }
    assert_eq!(outcome.chunks, RANKING_DOCUMENTS as u64);
    assert_eq!(outcome.documents, built.store().n_documents() as u64);
    assert_eq!(
        tree_digest(&source_dir),
        before,
        "the source directory was modified"
    );
}
