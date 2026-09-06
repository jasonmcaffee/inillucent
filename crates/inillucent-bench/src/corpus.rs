//! Reading a corpus out of PostgreSQL, and caching it locally.
//!
//! The corpus this engine is graded on is built by `synth.rs` and embedded once,
//! and both engines then read the identical vectors: `synth-embed` writes them to
//! the cache and `synth-load` writes the same bytes into PostgreSQL. That is what
//! makes a score difference attributable to indexing and ranking rather than to
//! the embedding model.
//!
//! This module is the other direction, for grading against a corpus that already
//! lives in a database with this schema. The pull costs minutes and produces
//! roughly 600 MB of vectors, so it is cached in a plain binary file that loads by
//! a single read.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use inillucent_base::hash::Sha256;
use pgvector::Vector;
use postgres::{Client, NoTls};
use inillucent_core::model::ModelManifest;
use inillucent_core::store::ChunkInput;

/// The layout that carries no provenance: text, vectors, width, count. Still
/// read, because the corpus embedded before there were manifests is a real cache
/// holding real vectors and re-embedding it to add a header would cost ten hours
/// and change no number. Never written.
const CACHE_MAGIC_V3: &[u8; 8] = b"RDBCACH3";
/// The layout that says what it is: which corpus, which model, which manifest,
/// which truncation bound, how many chunks. A comparison between two caches is
/// only honest if both can answer those, so this is what `synth-embed` writes.
const CACHE_MAGIC_V4: &[u8; 8] = b"INLCACH4";

/// Everything a cache has to be able to say about itself before its numbers are
/// compared with another cache's.
///
/// This board has had five instruments quietly measure something other than what
/// they claimed, and every one of them would have been caught by an input that
/// could state its own identity. A vector file cannot; a vector file with this in
/// front of it can.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CacheHeader {
    /// 3 for a legacy cache, 4 for one written since the header existed.
    pub version: u32,
    /// Digest of the corpus text this cache was embedded from, computed over the
    /// chunks rather than the file, so it does not move when line endings do.
    pub corpus_sha256: String,
    /// The model id from the manifest the embedding run used.
    pub model_id: String,
    /// Digest of that manifest's canonical bytes.
    pub manifest_sha256: String,
    pub dims: usize,
    /// The truncation bound the run applied, so a model that saw less text than
    /// its rivals cannot hide behind a faster number.
    pub max_tokens: usize,
    pub chunk_count: usize,
    /// How many chunks were longer than `max_tokens` and were embedded from a
    /// prefix of themselves.
    pub truncated_chunks: usize,
    /// Digest of the harness's query seed table at the time this cache was
    /// written. Two caches embedded either side of a change to the seeds describe
    /// different questions, and comparing them would be comparing two suites.
    pub query_seed_digest: String,
}

impl CacheHeader {
    /// What a legacy cache can honestly say about itself, which is its width and
    /// its count and nothing else.
    fn legacy(dims: usize, chunk_count: usize) -> CacheHeader {
        CacheHeader {
            version: 3,
            corpus_sha256: String::new(),
            model_id: String::new(),
            manifest_sha256: String::new(),
            dims,
            max_tokens: 0,
            chunk_count,
            truncated_chunks: 0,
            query_seed_digest: String::new(),
        }
    }

    /// Whether this header carries the provenance a head-to-head needs. A legacy
    /// cache does not, and saying so is the difference between refusing and
    /// pretending.
    pub fn has_provenance(&self) -> bool {
        self.version >= 4 && !self.corpus_sha256.is_empty() && !self.model_id.is_empty()
    }

    /// A short label for a card or an error message.
    pub fn describe(&self) -> String {
        if self.has_provenance() {
            format!(
                "{} ({} chunks, {} dims, corpus {}, manifest {})",
                self.model_id,
                self.chunk_count,
                self.dims,
                short(&self.corpus_sha256),
                short(&self.manifest_sha256)
            )
        } else {
            format!("a version {} cache with no provenance ({} chunks, {} dims)", self.version, self.chunk_count, self.dims)
        }
    }
}

/// The first twelve characters of a digest, which is what a human compares.
pub fn short(digest: &str) -> String {
    digest.chars().take(12).collect()
}

/// The digest a cache header stores for the corpus it was embedded from.
///
/// Taken over the chunk fields the engines index rather than over the file's
/// bytes, for two reasons this repository has paid for. A JSONL file that has
/// been through a Windows editor has different bytes and identical meaning, and a
/// digest that moves under that is a false alarm that costs a re-embedding run.
/// And the corpus reaches the embedder through `read_corpus`, which sanitizes
/// every text, so the file's bytes are not what was embedded anyway - these are.
///
/// Every field is length-prefixed, so no two different corpora can produce the
/// same byte sequence by moving a boundary.
/// @param chunks - the corpus, in corpus order
pub fn corpus_digest(chunks: &[ChunkInput]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"inillucent-corpus-v1");
    hasher.update(&(chunks.len() as u64).to_le_bytes());
    for c in chunks {
        for field in [c.source.as_str(), c.external_doc_id.as_str(), c.content.as_str()] {
            hasher.update(&(field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        hasher.update(&(c.chunk_index as u64).to_le_bytes());
    }
    hasher.hex()
}

/// The digest of a model manifest, over its meaning rather than its file bytes.
pub fn manifest_digest(manifest: &ModelManifest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"inillucent-manifest-v1");
    hasher.update(&manifest.canonical_bytes());
    hasher.hex()
}

/// The digest of a query seed table.
/// @param seeds - the named seeds every query family is generated from
pub fn seed_digest(seeds: &std::collections::BTreeMap<String, u64>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"inillucent-seeds-v1");
    for (name, value) in seeds {
        hasher.update(&(name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update(&value.to_le_bytes());
    }
    hasher.hex()
}

pub struct Corpus {
    pub chunks: Vec<ChunkInput>,
    pub vectors: Vec<Vec<f32>>,
    pub dims: usize,
    /// What this cache says it is. A legacy cache says only its shape.
    pub header: CacheHeader,
}

impl Corpus {
    pub fn len(&self) -> usize {
        self.chunks.len()
    }
}

/// The query that defines the corpus. It mirrors the joins and the soft delete
/// rule the baseline's search uses, so the two engines see the same rows.
const CORPUS_SQL: &str = "
    SELECT
      c.id::text            AS chunk_id,
      d.source              AS source,
      d.id::text            AS doc_id,
      c.chunk_index         AS chunk_index,
      c.heading_path        AS heading_path,
      c.content             AS content,
      d.title               AS title,
      d.url                 AS url,
      d.space_key           AS space_key,
      d.author              AS author,
      d.author_id           AS author_id,
      d.updated_at          AS updated_at,
      d.labels              AS labels,
      (d.deleted_at IS NOT NULL) AS deleted,
      c.embedding           AS embedding
    FROM chunks c
    JOIN documents d ON d.id = c.document_id
    WHERE c.embedding IS NOT NULL
    ORDER BY c.id
";

pub fn load_from_postgres(url: &str, limit: Option<usize>) -> Result<Corpus> {
    let mut client = Client::connect(url, NoTls).context("connecting to PostgreSQL")?;

    let sql = match limit {
        Some(n) => format!("{CORPUS_SQL} LIMIT {n}"),
        None => CORPUS_SQL.to_string(),
    };

    // A portal keeps the 186k row result set off the client heap all at once.
    let mut chunks = Vec::new();
    let mut vectors = Vec::new();
    let mut dims = 0usize;

    let mut transaction = client.transaction()?;
    let statement = transaction.prepare(&sql)?;
    let portal = transaction.bind(&statement, &[])?;

    loop {
        let rows = transaction.query_portal(&portal, 5_000)?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            let embedding: Vector = row.get("embedding");
            let v = embedding.to_vec();
            if dims == 0 {
                dims = v.len();
            }
            if v.len() != dims {
                anyhow::bail!("mixed vector widths in the corpus: {} and {}", dims, v.len());
            }

            let updated: Option<std::time::SystemTime> = row.get("updated_at");
            let updated_at = updated.map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            });

            chunks.push(ChunkInput {
                source: row.get("source"),
                external_doc_id: row.get("doc_id"),
                chunk_index: row.get::<_, i32>("chunk_index") as u32,
                heading_path: row.get::<_, Vec<String>>("heading_path"),
                content: row.get("content"),
                title: row.get("title"),
                url: row.get("url"),
                space_key: row.get("space_key"),
                author: row.get("author"),
                author_id: row.get("author_id"),
                updated_at,
                external_chunk_id: None,
                labels: row.get::<_, Vec<String>>("labels"),
                attributes: Vec::new(),
                flags: Vec::new(),
                deleted: row.get("deleted"),
            });
            vectors.push(v);
        }
        eprintln!("  loaded {} chunks", chunks.len());
    }

    // Pulled out of PostgreSQL rather than embedded here, so the only provenance
    // this can honestly claim is its own shape.
    let header = CacheHeader::legacy(dims, chunks.len());
    Ok(Corpus { chunks, vectors, dims, header })
}

fn write_string(w: &mut impl Write, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

fn read_string(r: &mut impl Read) -> Result<String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8(buf)?)
}

fn write_opt_string(w: &mut impl Write, s: &Option<String>) -> Result<()> {
    match s {
        Some(v) => {
            w.write_all(&[1u8])?;
            write_string(w, v)
        }
        None => {
            w.write_all(&[0u8])?;
            Ok(())
        }
    }
}

fn read_opt_string(r: &mut impl Read) -> Result<Option<String>> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    if tag[0] == 0 {
        Ok(None)
    } else {
        Ok(Some(read_string(r)?))
    }
}

/// Write the cache, header first.
///
/// The header's width and count are taken from the data rather than from the
/// header the caller handed in, because those two are the fields a caller can get
/// wrong and the only two the file itself can settle.
pub fn save_cache(corpus: &Corpus, path: &Path) -> Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    w.write_all(CACHE_MAGIC_V4)?;
    w.write_all(&(corpus.dims as u32).to_le_bytes())?;
    w.write_all(&(corpus.len() as u32).to_le_bytes())?;
    write_string(&mut w, &corpus.header.corpus_sha256)?;
    write_string(&mut w, &corpus.header.model_id)?;
    write_string(&mut w, &corpus.header.manifest_sha256)?;
    write_string(&mut w, &corpus.header.query_seed_digest)?;
    w.write_all(&(corpus.header.max_tokens as u32).to_le_bytes())?;
    w.write_all(&(corpus.header.truncated_chunks as u32).to_le_bytes())?;

    for (c, v) in corpus.chunks.iter().zip(&corpus.vectors) {
        write_string(&mut w, &c.source)?;
        write_string(&mut w, &c.external_doc_id)?;
        w.write_all(&c.chunk_index.to_le_bytes())?;
        w.write_all(&(c.heading_path.len() as u32).to_le_bytes())?;
        for h in &c.heading_path {
            write_string(&mut w, h)?;
        }
        write_string(&mut w, &c.content)?;
        write_string(&mut w, &c.title)?;
        write_string(&mut w, &c.url)?;
        write_opt_string(&mut w, &c.space_key)?;
        write_opt_string(&mut w, &c.author)?;
        write_opt_string(&mut w, &c.author_id)?;
        w.write_all(&c.updated_at.unwrap_or(i64::MIN).to_le_bytes())?;
        w.write_all(&(c.labels.len() as u32).to_le_bytes())?;
        for l in &c.labels {
            write_string(&mut w, l)?;
        }
        w.write_all(&[c.deleted as u8])?;
        for x in v {
            w.write_all(&x.to_le_bytes())?;
        }
    }
    w.flush()?;
    Ok(())
}

/// Read only what a cache says about itself.
///
/// Separate from `load_cache` because the checks that decide whether two caches
/// may be compared are all answerable from a few hundred bytes at the front of
/// each file, and loading eight caches to discover that two of them describe
/// different corpora costs minutes and six gigabytes for an answer the header
/// already had. Refuse before spending.
pub fn read_header(path: &Path) -> Result<CacheHeader> {
    let mut r = BufReader::new(
        File::open(path).with_context(|| format!("opening the cache {}", path.display()))?,
    );
    read_header_from(&mut r, path)
}

fn read_header_from(r: &mut impl Read, path: &Path) -> Result<CacheHeader> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    let version = if &magic == CACHE_MAGIC_V4 {
        4u32
    } else if &magic == CACHE_MAGIC_V3 {
        3
    } else {
        anyhow::bail!(
            "{} is not a cache this harness reads; its first eight bytes are {:?}, and the \
             layouts known here are RDBCACH3 and INLCACH4",
            path.display(),
            String::from_utf8_lossy(&magic)
        );
    };
    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let dims = u32::from_le_bytes(buf4) as usize;
    r.read_exact(&mut buf4)?;
    let n = u32::from_le_bytes(buf4) as usize;
    if version == 3 {
        return Ok(CacheHeader::legacy(dims, n));
    }
    let corpus_sha256 = read_string(r)?;
    let model_id = read_string(r)?;
    let manifest_sha256 = read_string(r)?;
    let query_seed_digest = read_string(r)?;
    r.read_exact(&mut buf4)?;
    let max_tokens = u32::from_le_bytes(buf4) as usize;
    r.read_exact(&mut buf4)?;
    let truncated_chunks = u32::from_le_bytes(buf4) as usize;
    Ok(CacheHeader {
        version,
        corpus_sha256,
        model_id,
        manifest_sha256,
        dims,
        max_tokens,
        chunk_count: n,
        truncated_chunks,
        query_seed_digest,
    })
}

pub fn load_cache(path: &Path) -> Result<Corpus> {
    let mut r = BufReader::new(
        File::open(path).with_context(|| format!("opening the cache {}", path.display()))?,
    );
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    let version = if &magic == CACHE_MAGIC_V4 {
        4u32
    } else if &magic == CACHE_MAGIC_V3 {
        3
    } else {
        anyhow::bail!(
            "{} is not a cache this harness reads; its first eight bytes are {:?}, and the \
             layouts known here are RDBCACH3 and INLCACH4",
            path.display(),
            String::from_utf8_lossy(&magic)
        );
    };
    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let dims = u32::from_le_bytes(buf4) as usize;
    r.read_exact(&mut buf4)?;
    let n = u32::from_le_bytes(buf4) as usize;

    let header = if version == 4 {
        let corpus_sha256 = read_string(&mut r)?;
        let model_id = read_string(&mut r)?;
        let manifest_sha256 = read_string(&mut r)?;
        let query_seed_digest = read_string(&mut r)?;
        r.read_exact(&mut buf4)?;
        let max_tokens = u32::from_le_bytes(buf4) as usize;
        r.read_exact(&mut buf4)?;
        let truncated_chunks = u32::from_le_bytes(buf4) as usize;
        CacheHeader {
            version,
            corpus_sha256,
            model_id,
            manifest_sha256,
            dims,
            max_tokens,
            chunk_count: n,
            truncated_chunks,
            query_seed_digest,
        }
    } else {
        CacheHeader::legacy(dims, n)
    };

    let mut chunks = Vec::with_capacity(n);
    let mut vectors = Vec::with_capacity(n);
    let mut buf8 = [0u8; 8];
    let mut tag = [0u8; 1];

    for _ in 0..n {
        let source = read_string(&mut r)?;
        let external_doc_id = read_string(&mut r)?;
        r.read_exact(&mut buf4)?;
        let chunk_index = u32::from_le_bytes(buf4);
        r.read_exact(&mut buf4)?;
        let n_headings = u32::from_le_bytes(buf4) as usize;
        let mut heading_path = Vec::with_capacity(n_headings);
        for _ in 0..n_headings {
            heading_path.push(read_string(&mut r)?);
        }
        let content = read_string(&mut r)?;
        let title = read_string(&mut r)?;
        let url = read_string(&mut r)?;
        let space_key = read_opt_string(&mut r)?;
        let author = read_opt_string(&mut r)?;
        let author_id = read_opt_string(&mut r)?;
        r.read_exact(&mut buf8)?;
        let raw_updated = i64::from_le_bytes(buf8);
        let updated_at = if raw_updated == i64::MIN { None } else { Some(raw_updated) };
        r.read_exact(&mut buf4)?;
        let n_labels = u32::from_le_bytes(buf4) as usize;
        let mut labels = Vec::with_capacity(n_labels);
        for _ in 0..n_labels {
            labels.push(read_string(&mut r)?);
        }
        r.read_exact(&mut tag)?;
        let deleted = tag[0] != 0;

        let mut v = vec![0f32; dims];
        for x in v.iter_mut() {
            r.read_exact(&mut buf4)?;
            *x = f32::from_le_bytes(buf4);
        }

        chunks.push(ChunkInput {
            source,
            external_doc_id,
            chunk_index,
            heading_path,
            content,
            title,
            url,
            space_key,
            author,
            author_id,
            updated_at,
            external_chunk_id: None,
            labels,
            attributes: Vec::new(),
            flags: Vec::new(),
            deleted,
        });
        vectors.push(v);
    }

    Ok(Corpus { chunks, vectors, dims, header })
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_core::model::{ModelManifest, Prefixes};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("inillucent-corpus-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn chunk(i: usize, content: &str) -> ChunkInput {
        ChunkInput {
            source: "confluence".into(),
            external_doc_id: format!("doc{}", i / 3),
            chunk_index: (i % 3) as u32,
            heading_path: vec!["Section".into()],
            content: content.into(),
            title: "Title".into(),
            url: "u".into(),
            space_key: Some("SPACE".into()),
            author: None,
            author_id: None,
            updated_at: Some(17),
            external_chunk_id: None,
            labels: vec!["a".into()],
            attributes: Vec::new(),
            flags: Vec::new(),
            deleted: false,
        }
    }

    fn corpus_of(n: usize, dims: usize, header: CacheHeader) -> Corpus {
        let chunks: Vec<ChunkInput> = (0..n).map(|i| chunk(i, &format!("body {i}"))).collect();
        let vectors: Vec<Vec<f32>> = (0..n)
            .map(|i| {
                let mut v: Vec<f32> = (0..dims).map(|d| ((i + d) as f32).sin()).collect();
                inillucent_core::distance::normalize(&mut v);
                v
            })
            .collect();
        Corpus { chunks, vectors, dims, header }
    }

    fn header_for(n: usize, dims: usize) -> CacheHeader {
        CacheHeader {
            version: 4,
            corpus_sha256: "corpus-digest".into(),
            model_id: "some-model".into(),
            manifest_sha256: "manifest-digest".into(),
            dims,
            max_tokens: 512,
            chunk_count: n,
            truncated_chunks: 7,
            query_seed_digest: "seed-digest".into(),
        }
    }

    #[test]
    fn a_cache_round_trips_its_header_and_its_contents() {
        let root = scratch("roundtrip");
        let path = root.join("a.cache");
        let written = corpus_of(9, 8, header_for(9, 8));
        save_cache(&written, &path).unwrap();

        let read = load_cache(&path).unwrap();
        assert_eq!(read.header, written.header);
        assert_eq!(read.len(), 9);
        assert_eq!(read.dims, 8);
        for i in 0..9 {
            assert_eq!(read.chunks[i].content, written.chunks[i].content);
            assert_eq!(read.chunks[i].external_doc_id, written.chunks[i].external_doc_id);
            assert_eq!(read.vectors[i], written.vectors[i]);
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// The header alone must be readable without paying for the vectors, because
    /// that is what lets a head-to-head refuse before it spends minutes loading
    /// caches it is about to reject.
    #[test]
    fn the_header_reads_the_same_whether_or_not_the_vectors_are_loaded() {
        let root = scratch("header-only");
        let path = root.join("a.cache");
        save_cache(&corpus_of(20, 16, header_for(20, 16)), &path).unwrap();
        assert_eq!(read_header(&path).unwrap(), load_cache(&path).unwrap().header);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A cache the harness has never seen is refused with its own first bytes in
    /// the message, rather than being read as whatever the current layout is.
    #[test]
    fn a_cache_of_an_unknown_layout_is_refused_by_name() {
        let root = scratch("unknown");
        let path = root.join("bad.cache");
        std::fs::write(&path, b"NOTACACHE\x00\x00\x00\x00").unwrap();
        let err = read_header(&path).unwrap_err().to_string();
        assert!(err.contains("is not a cache this harness reads"), "{err}");
        assert!(err.contains("NOTACACH"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The corpus digest is taken over the chunks rather than over a file, and
    /// this is the property that buys: the same corpus written with different
    /// line endings, or read back after sanitisation, digests the same.
    #[test]
    fn the_corpus_digest_does_not_move_when_line_endings_do() {
        let unix: Vec<ChunkInput> = (0..5).map(|i| chunk(i, "first line\nsecond line")).collect();
        let windows: Vec<ChunkInput> = (0..5)
            .map(|i| chunk(i, &crate::synth::sanitize_for_model("first line\r\nsecond line")))
            .collect();
        assert_eq!(corpus_digest(&unix), corpus_digest(&windows));
    }

    #[test]
    fn the_corpus_digest_moves_when_a_single_character_does() {
        let a: Vec<ChunkInput> = (0..5).map(|i| chunk(i, "offer eligibility")).collect();
        let mut b = a.clone();
        b[3].content = "offer eligibilaty".into();
        assert_ne!(corpus_digest(&a), corpus_digest(&b));
    }

    /// A boundary moved between two fields must change the digest, or two
    /// different corpora could share one. The lengths in front of every field
    /// are what make that true.
    #[test]
    fn the_corpus_digest_separates_corpora_a_naive_join_would_confuse() {
        let mut a = vec![chunk(0, "ab")];
        a[0].source = "conf".into();
        a[0].external_doc_id = "luence".into();
        let mut b = vec![chunk(0, "ab")];
        b[0].source = "confl".into();
        b[0].external_doc_id = "uence".into();
        assert_ne!(corpus_digest(&a), corpus_digest(&b));
    }

    #[test]
    fn the_corpus_digest_moves_when_a_chunk_is_reordered() {
        let a: Vec<ChunkInput> = (0..5).map(|i| chunk(i, &format!("body {i}"))).collect();
        let mut b = a.clone();
        b.swap(1, 2);
        assert_ne!(corpus_digest(&a), corpus_digest(&b));
    }

    /// A manifest digest that moved with a JSON reformat would fire on every
    /// checkout; one that does not move when a prefix changes would never fire
    /// at all. Both directions are asserted.
    #[test]
    fn the_manifest_digest_follows_the_meaning_and_not_the_file() {
        let a = ModelManifest::nomic_v1_5();
        let b = ModelManifest::nomic_v1_5();
        assert_eq!(manifest_digest(&a), manifest_digest(&b));
        let changed = ModelManifest { prefixes: Prefixes::none(), ..a.clone() };
        assert_ne!(manifest_digest(&a), manifest_digest(&changed));
        let widened = ModelManifest { max_tokens: 8192, ..a.clone() };
        assert_ne!(manifest_digest(&a), manifest_digest(&widened));
    }

    /// The baseline manifest's digest is pinned, so a change to the manifest
    /// *schema* is a deliberate, visible act.
    ///
    /// This test exists because the absence of it cost an hour. A field was added
    /// to `ModelManifest` half way through a Phase 0 embedding run; every
    /// manifest's canonical digest moved, and every cache written before the
    /// change was refused by `grade-embedding` - correctly, but for a reason that
    /// had nothing to do with any model. The guard behaved perfectly and the
    /// schema change was the thing nobody had noticed.
    ///
    /// So: adding, removing or reordering a field in `canonical_bytes` now breaks
    /// this test with a message saying what it means. Updating the constant is
    /// the right response *and* the notice that every cache on disk has just
    /// become unreadable to a head-to-head and has to be re-stamped by re-running
    /// `synth-embed`, which resumes from the vectors it already has.
    #[test]
    fn the_baseline_manifests_digest_is_pinned_so_a_schema_change_is_visible() {
        const PINNED: &str =
            "9085067aee7e46475a13396368c0d4a85416e4814896dc1415cfa7390cdc0309";
        let digest = manifest_digest(&ModelManifest::nomic_v1_5());
        assert_eq!(
            digest, PINNED,
            "the manifest schema changed. Every cache header on disk records the digest its \
             manifest had when it was written, so every one of them now names a manifest that \
             no longer exists and grade-embedding will refuse them all. Re-run synth-embed for \
             each arm - it resumes from the vectors already on disk and re-stamps the header - \
             then run embed-check on each cache, and update this constant."
        );
    }

    #[test]
    fn the_seed_digest_moves_when_one_seed_does() {
        let mut seeds: std::collections::BTreeMap<String, u64> =
            [("identity".to_string(), 11u64), ("heading".to_string(), 12)].into_iter().collect();
        let before = seed_digest(&seeds);
        seeds.insert("heading".into(), 13);
        assert_ne!(before, seed_digest(&seeds));
        // And when a family is added, which is the change a pairwise value
        // comparison would miss.
        seeds.insert("heading".into(), 12);
        assert_eq!(before, seed_digest(&seeds));
        seeds.insert("paraphrase".into(), 17);
        assert_ne!(before, seed_digest(&seeds));
    }

    #[test]
    fn a_header_with_no_provenance_says_so_rather_than_pretending() {
        let legacy = CacheHeader::legacy(768, 100);
        assert!(!legacy.has_provenance());
        assert!(legacy.describe().contains("no provenance"));
        let full = header_for(100, 768);
        assert!(full.has_provenance());
        assert!(full.describe().contains("some-model"));
    }

    /// A version 4 header whose digests were never filled in is not provenance,
    /// whatever its version number says.
    #[test]
    fn a_version_four_header_with_empty_digests_is_not_provenance() {
        let mut h = header_for(100, 768);
        h.corpus_sha256 = String::new();
        assert!(!h.has_provenance());
        let mut h = header_for(100, 768);
        h.model_id = String::new();
        assert!(!h.has_provenance());
    }
}
