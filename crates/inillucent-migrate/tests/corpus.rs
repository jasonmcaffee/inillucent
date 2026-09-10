//! Migrating an index built from real text, at the size a release ships at.
//!
//! Invariant: the migration is exercised on a corpus nobody wrote for it. The
//! neighbouring suite builds a small corpus with one of everything, which is
//! the right shape for proving that labels, flags, attributes and tombstones
//! all survive - but every document in it is the same length, drawn from the
//! same twelve words, and a corpus like that cannot expose the things that go
//! wrong at scale: a chunk longer than a page, a term that occurs in almost
//! every document, a vocabulary in the thousands, a graph with enough nodes
//! that its neighbour lists matter.
//!
//! So this one is built from the repository's own prose - the module comments
//! and design notes checked in beside the code - which is text somebody wrote
//! to be read, at whatever lengths it happened to come out. No deployed legacy
//! index exists on this machine to migrate; this is the closest thing to one
//! that can be reproduced from a clean checkout, which is what a release
//! artifact has to be.
//!
//! The corpus therefore *changes when the code does*, and that is deliberate
//! rather than tolerated: it is a light fuzz over real text, and it has already
//! earned its keep twice. It found that the legacy engine's lexical ranking
//! depends on the `k` it was asked for, and it found that a rebuilt vector
//! graph can answer a query differently from the incrementally-grown one it was
//! copied from. Both were contracts the migration had stated wrongly, not
//! defects the corpus introduced. What follows from it is that every check here
//! has to be about a contract rather than about a particular ranking - which is
//! what the verification now is.
//!
//! The embedding is a hashed bag of words rather than a learned one. That is
//! not a model and is not claimed to be: what the migration has to preserve is
//! that the vectors come out the other side bit-for-bit and rank the same, and
//! a deterministic embedding proves that without making the test depend on a
//! model file.

use std::path::{Path, PathBuf};

use inillucent_base::hash::Sha256;
use inillucent_core::distance::normalize;
use inillucent_core::index::{Index, IndexConfig};
use inillucent_core::store::ChunkInput;
use inillucent_migrate::{migrate, Plan};

/// How wide the hashed embedding is.
const DIMS: usize = 64;

/// How many characters of prose make one chunk.
const CHUNK: usize = 600;

/// The most documents to draw, so the suite stays a suite.
const DOCUMENTS: usize = 400;

/// Returns the repository root.
fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Returns a fresh scratch directory.
fn scratch(name: &str) -> PathBuf {
    let root = repository()
        .join("_agent_output/migrate")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    root
}

/// One document drawn from a file: its path, and the prose inside it.
struct Document {
    path: String,
    text: String,
}

/// Collects the prose of every Rust and Markdown file under a directory.
///
/// The prose, not the code: a doc comment is what somebody wrote in English,
/// and indexing the surrounding Rust would build a corpus of punctuation. A
/// file with fewer than a few hundred characters of comment is skipped, because
/// a document of one line tells the retrieval nothing and would just inflate
/// the count.
/// @param directory - where to walk
/// @param into - the documents collected so far
fn gather(directory: &Path, into: &mut Vec<Document>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if into.len() >= DOCUMENTS {
            return;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if name.starts_with('.') || name == "target" || name == "_agent_output" {
            continue;
        }
        if path.is_dir() {
            gather(&path, into);
            continue;
        }
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let text = match extension {
            "rs" => prose_of(&contents),
            "md" => contents,
            _ => continue,
        };
        if text.len() < 400 {
            continue;
        }
        let relative = path
            .strip_prefix(repository())
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        into.push(Document {
            path: relative,
            text,
        });
    }
}

/// Returns the English out of a Rust file: its `//!` and `///` comments.
fn prose_of(source: &str) -> String {
    let mut out = String::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let text = trimmed
            .strip_prefix("//!")
            .or_else(|| trimmed.strip_prefix("///"));
        if let Some(text) = text {
            out.push_str(text.trim());
            out.push(' ');
        }
    }
    out
}

/// Splits one document's prose into chunks on whitespace boundaries.
fn split(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.len().saturating_add(word.len()) > CHUNK && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Returns a deterministic hashed bag-of-words embedding of one chunk.
///
/// Each word lands in one dimension by its hash and contributes its inverse
/// length, so long words weigh less than short ones and two chunks about the
/// same thing point in a similar direction. It is a stand-in for a model, and
/// the only property the migration needs from it is that it is a function of
/// the text.
/// @param text - the chunk
fn embedding(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0f32; DIMS];
    for word in text.split_whitespace() {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in word.to_ascii_lowercase().bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let slot = (hash % DIMS as u64) as usize;
        if let Some(value) = vector.get_mut(slot) {
            *value += 1.0 / (word.len() as f32).max(1.0);
        }
    }
    normalize(&mut vector);
    vector
}

/// Builds the legacy index from the repository's prose.
///
/// Returns the index it saved and how many chunks went into it.
/// @param directory - where the legacy index is written
fn build_source(directory: &Path) -> (Index, usize) {
    let mut documents = Vec::new();
    gather(&repository().join("crates"), &mut documents);
    gather(&repository().join("docs"), &mut documents);
    assert!(
        documents.len() > 80,
        "the repository should have more prose than this: {} documents",
        documents.len()
    );

    let mut inputs: Vec<ChunkInput> = Vec::new();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    for (ordinal, document) in documents.iter().enumerate() {
        // Every eleventh document is tombstoned, so the copy carries tombstones
        // at a realistic rate rather than at a rate chosen to be convenient.
        let deleted = ordinal % 11 == 10;
        let crate_name = document
            .path
            .split('/')
            .nth(1)
            .unwrap_or("docs")
            .to_string();
        for (index, text) in split(&document.text).into_iter().enumerate() {
            vectors.push(embedding(&text));
            inputs.push(ChunkInput {
                source: if document.path.ends_with(".md") {
                    "design".to_string()
                } else {
                    "code".to_string()
                },
                external_doc_id: document.path.clone(),
                chunk_index: index as u32,
                heading_path: vec![crate_name.clone()],
                content: text,
                title: document.path.clone(),
                url: format!("https://example.test/{}", document.path),
                space_key: Some(crate_name.clone()),
                author: Some("Jason".to_string()),
                author_id: Some("u0".to_string()),
                updated_at: Some(1_700_000_000 + ordinal as i64),
                external_chunk_id: Some(format!("{}#{index}", document.path)),
                labels: vec![crate_name.clone(), "inillucent".to_string()],
                attributes: vec![("path".to_string(), vec![document.path.clone()])],
                // Not keyed off the file's extension: the walk fills up on Rust
                // before it reaches the design notes, and a corpus with one
                // flagged document would report that flags survive on the
                // strength of a single row.
                flags: if ordinal % 7 == 0 {
                    vec!["design_note".to_string()]
                } else {
                    Vec::new()
                },
                deleted,
            });
        }
    }
    let chunks = inputs.len();
    let mut index = Index::new(IndexConfig {
        dims: DIMS,
        ..IndexConfig::default()
    });
    index.add(inputs, &vectors);
    index.commit();
    inillucent_core::persist::save(&index, directory).expect("the legacy index saves");
    (index, chunks)
}

/// Returns a digest of every file under a directory.
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

/// An index of the repository's own prose migrates, and every check passes.
#[test]
fn a_release_sized_index_migrates_and_every_check_passes() {
    let root = scratch("release");
    let source_dir = root.join("index");
    let (built, chunks) = build_source(&source_dir);
    let before = tree_digest(&source_dir);

    let plan = Plan::new(&source_dir, root.join("corpus.db"));
    let outcome = migrate(&plan).expect("the migration runs");

    let failed: Vec<String> = outcome
        .checks
        .iter()
        .filter(|check| !check.passed)
        .map(|check| format!("{}: {}", check.name, check.detail))
        .collect();
    assert!(
        failed.is_empty(),
        "{}",
        failed.join(
            "
"
        )
    );
    assert_eq!(outcome.chunks, chunks as u64, "every chunk was copied");
    assert_eq!(outcome.documents, built.store().n_documents() as u64);
    assert!(
        outcome.chunks > 400,
        "the corpus should be release sized: {} chunks",
        outcome.chunks
    );
    assert_eq!(
        outcome.published.as_deref(),
        Some(plan.destination.as_path())
    );

    // The source is exactly what it was, which is the property the whole tool
    // exists to have and the one a reviewer cannot check by reading.
    assert_eq!(
        tree_digest(&source_dir),
        before,
        "the source directory was modified"
    );
}
