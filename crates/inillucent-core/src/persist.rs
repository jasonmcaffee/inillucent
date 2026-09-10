//! Saving and loading an index.
//!
//! An index directory holds numbered generation directories and a `current` file
//! naming the live one. Each generation holds the store, the vectors, the graph,
//! the lexical index and the configuration, each with a version stamped header, so
//! a file written by a different layout is refused rather than misread.
//!
//! Saving never writes over what a reader is using. It writes a new generation,
//! makes every file durable, and then replaces the one small pointer file - which
//! is the only step that has to be atomic, and the only one the platform performs
//! atomically. A crash anywhere before that leaves the previous generation intact
//! and still current, which is the difference between an index that survives a
//! power loss and one that has to be rebuilt from a database.
//!
//! Everything else is a raw byte array: vectors are little endian f32, the store
//! is fixed width records and contiguous arenas, and loading them is a read rather
//! than a parse.
//!
//! There is no daemon, no port and no background process. Opening an index is
//! opening files.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::bm25::Bm25Index;
use crate::hnsw::{Hnsw, HnswParams};
use crate::index::{Index, IndexConfig};
use crate::rank::{AdaptiveWeights, Fusion};
use crate::store::Store;
use crate::vectors::VectorSet;

/// Bumped whenever any file layout changes.
///
/// Version 2 added the ranking settings that version 1 dropped. A version 1 index
/// reopened by this build would answer differently from the index that was saved,
/// which is the failure the version stamp exists to prevent, so it is refused
/// rather than read with defaults substituted.
///
/// Version 3 writes the store as fixed-width binary rather than JSON, adds the
/// lexical index as a file of its own, and lays the whole index out as numbered
/// generations behind a pointer file. A version 2 index cannot be read; the way
/// back is a rebuild from whatever the caller's source of truth is, which for
/// every current caller is a database.
pub const FORMAT_VERSION: u32 = 3;
const MAGIC: &[u8; 8] = b"INILLUCX";

/// The magic written before the engine was renamed from its working name.
///
/// Kept, and still accepted on read, because the eight bytes are a label rather than a
/// format: the layout behind them is identical, and every index already on disk carries
/// the old label. Refusing them would turn a rename into a rebuild of every corpus. Only
/// the new magic is ever written, so an index re-saved by this build stops carrying it.
const LEGACY_MAGIC: &[u8; 8] = b"RUSTDBIX";

/// Reports whether an eight-byte header is one this build recognises, under either name.
/// @param magic - the first eight bytes of the file
fn is_known_magic(magic: &[u8; 8]) -> bool {
    magic == MAGIC || magic == LEGACY_MAGIC
}

fn header(w: &mut impl Write, kind: u8) -> Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&[kind])?;
    Ok(())
}

/// Reads and checks the magic and the version, leaving the section tag unread.
///
/// Separate from `check_header` so a readability probe can ask "is this an index
/// this build can open" without caring which section it happens to be looking at.
fn check_header_version(r: &mut impl Read) -> Result<()> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).context("reading the file header")?;
    if !is_known_magic(&magic) {
        anyhow::bail!("not an inillucent index file");
    }
    let mut version = [0u8; 4];
    r.read_exact(&mut version)?;
    let version = u32::from_le_bytes(version);
    if version != FORMAT_VERSION {
        anyhow::bail!(
            "index was written by format version {version}, this build reads version {FORMAT_VERSION}"
        );
    }
    Ok(())
}

fn check_header(r: &mut impl Read, kind: u8) -> Result<()> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).context("reading the file header")?;
    if !is_known_magic(&magic) {
        anyhow::bail!("not an inillucent index file");
    }
    let mut version = [0u8; 4];
    r.read_exact(&mut version)?;
    let version = u32::from_le_bytes(version);
    if version != FORMAT_VERSION {
        anyhow::bail!(
            "index was written by format version {version}, this build reads version {FORMAT_VERSION}"
        );
    }
    let mut got = [0u8; 1];
    r.read_exact(&mut got)?;
    if got[0] != kind {
        anyhow::bail!("index file holds the wrong section");
    }
    Ok(())
}

/// Where the first vector sits in `vectors.bin`.
///
/// Eight magic bytes, a four byte format version and a one byte section tag, then
/// the two `u32` counts the vector section carries. Named rather than computed at the one call site, because a
/// filed vector set reads by offset and an offset that is wrong by four bytes
/// produces vectors that are subtly wrong rather than an error.
const VECTOR_HEADER_BYTES: u64 = 8 + 4 + 1 + 8;

const KIND_STORE: u8 = 1;
const KIND_VECTORS: u8 = 2;
const KIND_GRAPH: u8 = 3;
const KIND_CONFIG: u8 = 4;
const KIND_LEXICAL: u8 = 5;

fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

/// What the index needs in order to be rebuilt from disk. The int8 codes are the
/// one structure still derived rather than stored: a code depends on nothing but
/// its own vector, and re-encoding a set of vectors that were just read is one
/// linear pass.
///
/// Every field of `IndexConfig` is here, which was not true before. `SavedConfig`
/// used to carry `lexical_prefix` and no other ranking setting, so an index built
/// with a measured fusion, coverage exponent, proximity weight or tiering choice
/// reopened with the compiled-in defaults instead and answered differently from
/// the index that had been saved — silently, because nothing about the reopened
/// index looked wrong. A saved index is a configuration as much as it is data, and
/// the two have to travel together.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedConfig {
    dims: usize,
    quantized: bool,
    oversample: f32,
    candidates: usize,
    per_doc_cap: usize,
    lexical_prefix: bool,
    hnsw_m: usize,
    hnsw_ef_construction: usize,
    hnsw_ef_search: usize,
    hnsw_seed: u64,
    hnsw_exhaustive_below: usize,
    hnsw_entry_points: usize,
    hnsw_keep_pruned_connections: bool,
    hnsw_build_threads: usize,
    // Everything below this line is what version 1 lost.
    fusion: SavedFusion,
    lexical_coverage: f32,
    lexical_proximity: f32,
    lexical_tier: bool,
    lexical_phrase: f32,
    lexical_rescore_depth: usize,
    lexical_heading_boost: f32,
    adaptive_fusion: bool,
    adaptive: SavedAdaptive,
    mmr_lambda: f32,
    /// Whether the vectors are read into memory when this index is opened.
    ///
    /// Defaulted on read, so an index saved before this option existed opens with
    /// the vectors left in the file - which is the new default and is what the
    /// index that wrote the file would also do today.
    #[serde(default)]
    resident_vectors: bool,
}

/// `Fusion` in a form that survives a round trip through JSON.
///
/// Written as a named method plus its parameters rather than as a serde enum so
/// that adding a method later does not change how the existing ones are spelled
/// on disk.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedFusion {
    method: String,
    vector_weight: f32,
    rrf_k: f32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedAdaptive {
    base: f32,
    out_of_vocabulary_gain: f32,
    identifier_gain: f32,
    separation_gain: f32,
    coverage_gain: f32,
    floor: f32,
    ceiling: f32,
}

impl From<Fusion> for SavedFusion {
    fn from(f: Fusion) -> Self {
        match f {
            Fusion::ReciprocalRank { k } => SavedFusion {
                method: "rrf".into(),
                vector_weight: 0.0,
                rrf_k: k,
            },
            Fusion::NormalizedScore { vector_weight } => SavedFusion {
                method: "minmax".into(),
                vector_weight,
                rrf_k: 0.0,
            },
            Fusion::Convex { vector_weight } => SavedFusion {
                method: "convex".into(),
                vector_weight,
                rrf_k: 0.0,
            },
            Fusion::TheoreticalMinMax { vector_weight } => SavedFusion {
                method: "tmm".into(),
                vector_weight,
                rrf_k: 0.0,
            },
        }
    }
}

impl SavedFusion {
    /// The fusion this record describes, or an error naming the method it holds.
    /// An unknown method is refused rather than defaulted, for the same reason the
    /// version stamp is checked: an index that answers differently from the one
    /// that was saved is worse than an index that will not open.
    fn to_fusion(&self) -> Result<Fusion> {
        Ok(match self.method.as_str() {
            "rrf" => Fusion::ReciprocalRank { k: self.rrf_k },
            "minmax" => Fusion::NormalizedScore { vector_weight: self.vector_weight },
            "convex" => Fusion::Convex { vector_weight: self.vector_weight },
            "tmm" => Fusion::TheoreticalMinMax { vector_weight: self.vector_weight },
            other => anyhow::bail!("the saved index names an unknown fusion method {other}"),
        })
    }
}

impl From<AdaptiveWeights> for SavedAdaptive {
    fn from(a: AdaptiveWeights) -> Self {
        SavedAdaptive {
            base: a.base,
            out_of_vocabulary_gain: a.out_of_vocabulary_gain,
            identifier_gain: a.identifier_gain,
            separation_gain: a.separation_gain,
            coverage_gain: a.coverage_gain,
            floor: a.floor,
            ceiling: a.ceiling,
        }
    }
}

impl From<&SavedAdaptive> for AdaptiveWeights {
    fn from(a: &SavedAdaptive) -> Self {
        AdaptiveWeights {
            base: a.base,
            out_of_vocabulary_gain: a.out_of_vocabulary_gain,
            identifier_gain: a.identifier_gain,
            separation_gain: a.separation_gain,
            coverage_gain: a.coverage_gain,
            floor: a.floor,
            ceiling: a.ceiling,
        }
    }
}


impl From<&IndexConfig> for SavedConfig {
    fn from(cfg: &IndexConfig) -> Self {
        SavedConfig {
            dims: cfg.dims,
            quantized: cfg.quantized,
            oversample: cfg.oversample,
            candidates: cfg.candidates,
            per_doc_cap: cfg.per_doc_cap,
            lexical_prefix: cfg.lexical_prefix,
            hnsw_m: cfg.hnsw.m,
            hnsw_ef_construction: cfg.hnsw.ef_construction,
            hnsw_ef_search: cfg.hnsw.ef_search,
            hnsw_seed: cfg.hnsw.seed,
            hnsw_exhaustive_below: cfg.hnsw.exhaustive_below,
            hnsw_entry_points: cfg.hnsw.entry_points,
            hnsw_keep_pruned_connections: cfg.hnsw.keep_pruned_connections,
            hnsw_build_threads: cfg.hnsw.build_threads,
            resident_vectors: cfg.resident_vectors,
            fusion: cfg.fusion.into(),
            lexical_coverage: cfg.lexical_coverage,
            lexical_proximity: cfg.lexical_proximity,
            lexical_tier: cfg.lexical_tier,
            lexical_phrase: cfg.lexical_phrase,
            lexical_rescore_depth: cfg.lexical_rescore_depth,
            lexical_heading_boost: cfg.lexical_heading_boost,
            adaptive_fusion: cfg.adaptive_fusion,
            adaptive: cfg.adaptive.into(),
            mmr_lambda: cfg.mmr_lambda,
        }
    }
}

impl SavedConfig {
    /// The graph parameters this record describes.
    fn hnsw_params(&self) -> HnswParams {
        HnswParams {
            m: self.hnsw_m,
            ef_construction: self.hnsw_ef_construction,
            ef_search: self.hnsw_ef_search,
            seed: self.hnsw_seed,
            exhaustive_below: self.hnsw_exhaustive_below,
            entry_points: self.hnsw_entry_points,
            keep_pruned_connections: self.hnsw_keep_pruned_connections,
            build_threads: self.hnsw_build_threads,
        }
    }

    /// The whole index configuration this record describes.
    ///
    /// Every field of `IndexConfig` travels with the index. A saved index is a
    /// configuration as much as it is data: one built with a measured fusion,
    /// coverage exponent or proximity weight and reopened with the compiled-in
    /// defaults answers differently from the index that was saved, silently,
    /// because nothing about it looks wrong.
    fn to_config(&self) -> Result<IndexConfig> {
        Ok(IndexConfig {
            dims: self.dims,
            quantized: self.quantized,
            oversample: self.oversample,
            candidates: self.candidates,
            per_doc_cap: self.per_doc_cap,
            lexical_prefix: self.lexical_prefix,
            hnsw: self.hnsw_params(),
            resident_vectors: self.resident_vectors,
            fusion: self.fusion.to_fusion()?,
            lexical_coverage: self.lexical_coverage,
            lexical_proximity: self.lexical_proximity,
            lexical_tier: self.lexical_tier,
            lexical_phrase: self.lexical_phrase,
            lexical_rescore_depth: self.lexical_rescore_depth,
            lexical_heading_boost: self.lexical_heading_boost,
            adaptive_fusion: self.adaptive_fusion,
            adaptive: (&self.adaptive).into(),
            mmr_lambda: self.mmr_lambda,
        })
    }
}

/// The name of the file that says which generation directory is the live one.
const CURRENT: &str = "current";
/// The prefix of a generation directory.
const GENERATION: &str = "g";
/// How many superseded generations survive a save.
///
/// One, not zero: a search running against the previous generation's files must
/// not have them removed out from under it while the pointer is swapped.
const KEEP_GENERATIONS: usize = 1;

fn generation_dir(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("{GENERATION}{generation:012}"))
}

/// The generation the pointer file names, or `None` for a directory that has
/// never been saved to.
fn read_current(dir: &Path) -> Option<u64> {
    let text = fs::read_to_string(dir.join(CURRENT)).ok()?;
    text.trim().strip_prefix(GENERATION)?.parse().ok()
}

/// Every generation directory present, ascending.
fn existing_generations(dir: &Path) -> Vec<u64> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<u64> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.strip_prefix(GENERATION)?.parse::<u64>().ok()
        })
        .collect();
    found.sort_unstable();
    found
}

/// Writes one file, flushing it to the disk rather than to the operating system's
/// cache.
///
/// `flush` on a `BufWriter` only moves bytes out of the process. Without the
/// `sync_all` a power loss can leave a file that the directory entry says is
/// there and whose contents were never written, which is the failure the
/// generation pointer exists to make survivable - and it only works if the
/// generation's own files are durable before the pointer starts naming them.
/// @param path - where to write
/// @param kind - the section tag stamped into the header
/// @param write - what to write after the header
fn write_file(
    path: &Path,
    kind: u8,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<()>,
) -> Result<()> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    header(&mut w, kind)?;
    write(&mut w)?;
    w.flush()?;
    w.into_inner()
        .map_err(|e| anyhow::anyhow!("flushing {}: {e}", path.display()))?
        .sync_all()
        .with_context(|| format!("syncing {}", path.display()))?;
    Ok(())
}

/// Saves an index into a new generation directory and then points at it.
///
/// The old shape wrote the four files in place, with no temp-and-rename and no
/// fsync, so a crash or a power loss part way through left an index that would
/// not open - and the store was 450 MB of escaped JSON, which made the window
/// wide. Here a save never touches the files a reader is using: it writes a new
/// generation, makes it durable, and then replaces one small pointer file, which
/// is the only step that has to be atomic. A crash at any point before that leaves
/// the previous generation exactly as it was.
/// @param index - the index to write
/// @param dir - the index directory, created if absent
pub fn save(index: &Index, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).context("creating the index directory")?;
    let previous = read_current(dir);
    let generation = previous.map(|g| g + 1).unwrap_or(1);
    let target = generation_dir(dir, generation);
    let _ = fs::remove_dir_all(&target);
    fs::create_dir_all(&target).context("creating the generation directory")?;

    write_file(&target.join("store.bin"), KIND_STORE, |w| {
        index.store().write_to(w).context("writing the store")
    })?;
    write_file(&target.join("vectors.bin"), KIND_VECTORS, |w| {
        let vectors = index.vectors();
        w.write_all(&(vectors.dims() as u32).to_le_bytes())?;
        w.write_all(&(vectors.len() as u32).to_le_bytes())?;
        // f32 little endian is the on disk form. A resident set is one write of
        // its buffer; a filed one is copied out of the generation it was loaded
        // from, a block at a time, followed by anything appended since. Either
        // way the bytes written here are the bytes a load reads back.
        vectors.write_to(w)?;
        Ok(())
    })?;
    write_file(&target.join("config.bin"), KIND_CONFIG, |w| {
        serde_json::to_writer(w, &SavedConfig::from(index.config()))?;
        Ok(())
    })?;
    write_file(&target.join("graph.bin"), KIND_GRAPH, |w| {
        index.write_graph(w)
    })?;
    write_file(&target.join("lexical.bin"), KIND_LEXICAL, |w| {
        index.write_lexical(w)
    })?;

    // The pointer, last and on its own. Written beside its target and renamed over
    // the old one, which is the one operation the platform performs atomically.
    let pointer = dir.join(CURRENT);
    let staging = dir.join("current.tmp");
    {
        let mut w = File::create(&staging)?;
        write!(w, "{GENERATION}{generation:012}")?;
        w.sync_all()?;
    }
    fs::rename(&staging, &pointer).context("publishing the new generation")?;

    // Only now is anything older unreferenced. One superseded generation is kept
    // so a reader that opened the previous one keeps its files.
    let keep: Vec<u64> = existing_generations(dir)
        .into_iter()
        .rev()
        .take(KEEP_GENERATIONS + 1)
        .collect();
    for old in existing_generations(dir) {
        if !keep.contains(&old) {
            let _ = fs::remove_dir_all(generation_dir(dir, old));
        }
    }
    Ok(())
}

/// How many times `load` re-reads the pointer when the generation it named is
/// reclaimed out from under it.
///
/// Two, because the only way this happens is a save publishing a newer generation
/// and then reclaiming an older one between the pointer read and the file opens.
/// One retry covers a single such save; a second covers a reader unlucky enough to
/// be lapped twice. A reader that loses three times in a row is not racing a save,
/// it is looking at a directory something else is deleting.
const LOAD_ATTEMPTS: usize = 3;

/// Opens the generation the pointer names.
///
/// A directory with no pointer, or one naming a generation that is not there, is
/// an error rather than a best guess: an index that answers differently from the
/// one that was saved is worse than an index that will not open.
///
/// Reading the pointer and opening the generation's files are two steps, and a save
/// can publish a newer generation and reclaim an older one in between - so a slow
/// reader can be handed a number whose directory is gone by the time it opens the
/// fifth file. The reclaim keeps one superseded generation precisely to make that
/// window small, and re-reading the pointer closes it: the generation that replaced
/// the one that vanished is the one the reader wanted anyway.
/// @param dir - the index directory
pub fn load(dir: &Path) -> Result<Index> {
    load_with(dir, None)
}

/// Opens the live generation, choosing where the vectors go.
///
/// **Residency follows the caller, not the file.** Every other setting in `IndexConfig` is written
/// with the index and read back with it, because an index that answers differently from the one that
/// was saved is the failure the format version exists to prevent - a fusion method or a coverage
/// exponent is part of what the index *is*. Where the vectors are held is not: it is a decision about
/// the process doing the opening, like how wide to search or how many places to start from, and
/// freezing it into the file means a machine that cannot spare two gigabytes cannot open an index
/// that was saved on one that could.
///
/// `None` takes what the file was saved with, which is what a caller with no opinion should get.
///
/// @param dir - the index directory
/// @param resident_vectors - hold the vectors on the heap, or leave them in the file
pub fn load_with(dir: &Path, resident_vectors: Option<bool>) -> Result<Index> {
    let mut last: Option<anyhow::Error> = None;
    for _ in 0..LOAD_ATTEMPTS {
        let generation = read_current(dir).ok_or_else(|| {
            anyhow::anyhow!(
                "{} holds no index: there is no current generation pointer",
                dir.display()
            )
        })?;
        let target = generation_dir(dir, generation);
        match load_generation(&target, resident_vectors) {
            Ok(index) => return Ok(index),
            // Only a vanished generation is retried. A corrupt or wrong-version
            // file is refused, because re-reading a pointer cannot fix it and
            // retrying would only hide it.
            Err(error) if !target.join("config.bin").exists() => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        anyhow::anyhow!("{} was rewritten while it was being read", dir.display())
    }))
}

/// Whether `dir` holds a readable index of this format version.
///
/// The question a service asks at startup, so it can rebuild from its own source
/// of truth instead of failing. Checked by opening the header rather than by
/// testing for the files, because a half-written generation has the files.
/// @param dir - the index directory
pub fn is_readable(dir: &Path) -> bool {
    let Some(generation) = read_current(dir) else {
        return false;
    };
    let g = generation_dir(dir, generation);
    ["store.bin", "vectors.bin", "config.bin", "graph.bin", "lexical.bin"]
        .iter()
        .all(|name| {
            File::open(g.join(name))
                .ok()
                .map(|f| {
                    let mut r = BufReader::new(f);
                    check_header_version(&mut r).is_ok()
                })
                .unwrap_or(false)
        })
}

/// Reads every file of one generation directory into an index.
///
/// @param dir - the generation directory
/// @param resident_vectors - hold the vectors on the heap, or `None` to take what was saved
fn load_generation(dir: &Path, resident_vectors: Option<bool>) -> Result<Index> {
    let saved: SavedConfig = {
        let mut r = BufReader::new(File::open(path(dir, "config.bin"))?);
        check_header(&mut r, KIND_CONFIG)?;
        serde_json::from_reader(r).context("reading the config")?
    };
    let store: Store = {
        let mut r = BufReader::with_capacity(1 << 20, File::open(path(dir, "store.bin"))?);
        check_header(&mut r, KIND_STORE)?;
        Store::read_from(&mut r).context("reading the store")?
    };
    // **The vectors are left in the file unless the caller asked for them.** They
    // are the largest thing an index holds - 1.85 GB for a 601,862 chunk corpus at
    // 768 dimensions - and every process that opens the index used to pay for them
    // whether or not it ever ran a semantic search. `resident_vectors` asks for the
    // old behaviour; the header is read either way, because the width and the count
    // are what say where a vector is.
    let vectors: VectorSet = {
        let vector_path = path(dir, "vectors.bin");
        let mut r = BufReader::with_capacity(1 << 20, File::open(&vector_path)?);
        check_header(&mut r, KIND_VECTORS)?;
        let mut buf4 = [0u8; 4];
        r.read_exact(&mut buf4)?;
        let dims = u32::from_le_bytes(buf4) as usize;
        r.read_exact(&mut buf4)?;
        let n = u32::from_le_bytes(buf4) as usize;
        if resident_vectors.unwrap_or(saved.resident_vectors) {
            VectorSet::from_raw(dims, crate::binio::read_pod_vec::<f32>(&mut r, dims * n)?)
        } else {
            // The header is eight magic bytes, one kind byte, then the two counts.
            let offset = VECTOR_HEADER_BYTES;
            VectorSet::from_file(dims, n, File::open(&vector_path)?, offset)
        }
    };
    let params = saved.hnsw_params();
    let graph: Hnsw = {
        let mut r = BufReader::with_capacity(1 << 20, File::open(path(dir, "graph.bin"))?);
        check_header(&mut r, KIND_GRAPH)?;
        Hnsw::read_graph(&mut r, params)?
    };
    let lexical: Bm25Index = {
        let mut r = BufReader::with_capacity(1 << 20, File::open(path(dir, "lexical.bin"))?);
        check_header(&mut r, KIND_LEXICAL)?;
        Bm25Index::read_from(&mut r).context("reading the lexical index")?
    };

    Index::from_parts(saved.to_config()?, store, vectors, graph, Some(lexical))
}

/// The tag that opens an index written as one byte stream.
const KIND_STREAM: u8 = 6;

/// Writes a whole index as one self-describing byte stream.
///
/// `save` writes five files into a directory because that is what a reader
/// mapping them wants. A caller that has nowhere to put a directory - a search
/// index whose generations live inside a database file, which is what makes them
/// commit and roll back with the rows they describe - wants the same five
/// sections one after another with their lengths in front, and that is what this
/// is. The sections and their encodings are the same ones `save` writes, so the
/// two forms describe the same index and neither is a second format.
/// @param index - the committed index to write
/// @param w - where the bytes go
pub fn write_index(index: &Index, w: &mut impl Write) -> Result<()> {
    header(w, KIND_STREAM)?;
    let config = serde_json::to_vec(&SavedConfig::from(index.config()))?;
    section(w, &config)?;
    let mut store = Vec::new();
    index
        .store()
        .write_to(&mut store)
        .context("writing the store")?;
    section(w, &store)?;
    let mut vectors = Vec::new();
    {
        let set = index.vectors();
        vectors.extend_from_slice(&(set.dims() as u32).to_le_bytes());
        vectors.extend_from_slice(&(set.len() as u32).to_le_bytes());
        set.write_to(&mut vectors)?;
    }
    section(w, &vectors)?;
    let mut graph = Vec::new();
    index.write_graph(&mut graph)?;
    section(w, &graph)?;
    let mut lexical = Vec::new();
    index.write_lexical(&mut lexical)?;
    section(w, &lexical)?;
    Ok(())
}

/// Reads back an index that `write_index` wrote.
///
/// A truncated or reordered stream is an error rather than a partial index: the
/// caller holding these bytes is a database that can tell the difference between
/// "corrupt" and "empty", and an index that answers from half a corpus is the
/// failure nobody notices.
/// @param r - the bytes `write_index` produced
pub fn read_index(r: &mut impl Read) -> Result<Index> {
    check_header(r, KIND_STREAM)?;
    let saved: SavedConfig =
        serde_json::from_slice(&read_section(r)?).context("reading the config")?;
    let store = Store::read_from(&mut read_section(r)?.as_slice()).context("reading the store")?;
    let vectors = {
        let bytes = read_section(r)?;
        let mut cursor = bytes.as_slice();
        let mut buf4 = [0u8; 4];
        cursor.read_exact(&mut buf4)?;
        let dims = u32::from_le_bytes(buf4) as usize;
        cursor.read_exact(&mut buf4)?;
        let count = u32::from_le_bytes(buf4) as usize;
        VectorSet::from_raw(
            dims,
            crate::binio::read_pod_vec::<f32>(&mut cursor, dims * count)?,
        )
    };
    let params = saved.hnsw_params();
    let graph = Hnsw::read_graph(&mut read_section(r)?.as_slice(), params)?;
    let lexical = Bm25Index::read_from(&mut read_section(r)?.as_slice())
        .context("reading the lexical index")?;
    Index::from_parts(saved.to_config()?, store, vectors, graph, Some(lexical))
}

/// Writes one length-prefixed section.
fn section(w: &mut impl Write, bytes: &[u8]) -> Result<()> {
    w.write_all(&(bytes.len() as u64).to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

/// Reads one length-prefixed section.
///
/// The length is checked against a ceiling before it is used to allocate,
/// because these bytes may have come out of a database file somebody else could
/// write to, and a corrupt length is the cheapest way to turn a read into an
/// out-of-memory abort.
fn read_section(r: &mut impl Read) -> Result<Vec<u8>> {
    let mut length = [0u8; 8];
    r.read_exact(&mut length)
        .context("reading a section length")?;
    let length = u64::from_le_bytes(length);
    const CEILING: u64 = 1 << 40;
    if length > CEILING {
        anyhow::bail!("index section claims {length} bytes, which is not a length this build reads");
    }
    let mut bytes = vec![0u8; length as usize];
    r.read_exact(&mut bytes).context("reading a section")?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distance::normalize;
    use crate::filter::Filter;
    use crate::store::ChunkInput;

    fn small_index() -> Index {
        let mut index = Index::new(IndexConfig { dims: 16, quantized: true, ..Default::default() });
        let chunks: Vec<ChunkInput> = (0..200)
            .map(|i| ChunkInput {
                source: if i % 3 == 0 { "slack" } else { "confluence" }.into(),
                external_doc_id: format!("d{}", i / 2),
                chunk_index: (i % 2) as u32,
                heading_path: vec![format!("h{i}")],
                content: format!("chunk {i} about offer eligibility token{i}"),
                external_chunk_id: Some(format!("chunk-{i}")),
                title: format!("title {i}"),
                url: format!("https://x/{i}"),
                space_key: Some("ENG".into()),
                author: Some("Ada".into()),
                author_id: Some("u1".into()),
                updated_at: Some(1000 + i as i64),
                labels: vec!["design".into()],
                attributes: Vec::new(),
                flags: Vec::new(),
                deleted: i == 7,
            })
            .collect();
        let vectors: Vec<Vec<f32>> = (0..200)
            .map(|i| {
                let mut v: Vec<f32> = (0..16).map(|d| ((i * 16 + d) as f32 * 0.07).sin()).collect();
                normalize(&mut v);
                v
            })
            .collect();
        index.add(chunks, &vectors);
        index.commit();
        index
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("inillucent-persist-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn a_saved_index_answers_the_same_queries_after_loading() {
        let dir = temp_dir("roundtrip");
        let original = small_index();
        save(&original, &dir).unwrap();
        let loaded = load(&dir).unwrap();

        assert_eq!(loaded.store().n_chunks(), original.store().n_chunks());
        assert_eq!(loaded.store().n_documents(), original.store().n_documents());

        let query = original.vectors().copy_of(11);
        let f_orig = original.compile(&Filter::default());
        let f_load = loaded.compile(&Filter::default());

        let a: Vec<u32> = original
            .vector_search(&query, &f_orig, 10, Some(64))
            .iter()
            .map(|n| n.chunk)
            .collect();
        let b: Vec<u32> = loaded
            .vector_search(&query, &f_load, 10, Some(64))
            .iter()
            .map(|n| n.chunk)
            .collect();
        assert_eq!(a, b, "the graph did not survive the round trip");

        let la: Vec<u32> = original.lexical_search("offer eligibility", &f_orig, 10).iter().map(|h| h.chunk).collect();
        let lb: Vec<u32> = loaded.lexical_search("offer eligibility", &f_load, 10).iter().map(|h| h.chunk).collect();
        assert_eq!(la, lb, "the lexical index did not survive the round trip");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn filter_metadata_survives_the_round_trip() {
        let dir = temp_dir("filters");
        let original = small_index();
        save(&original, &dir).unwrap();
        let loaded = load(&dir).unwrap();

        for filter in [
            Filter::source("slack"),
            Filter { labels: Some(vec!["design".into()]), ..Default::default() },
            Filter { author: Some("Ada".into()), ..Default::default() },
            Filter { updated_after: Some(1100), ..Default::default() },
        ] {
            let a = original.compile(&filter).pass_count();
            let b = loaded.compile(&filter).pass_count();
            assert_eq!(a, b, "pass count differed for {filter:?}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_soft_deleted_document_is_still_soft_deleted_after_loading() {
        let dir = temp_dir("deleted");
        let original = small_index();
        save(&original, &dir).unwrap();
        let loaded = load(&dir).unwrap();
        let deleted_before = original.store().documents.iter().filter(|d| d.deleted).count();
        let deleted_after = loaded.store().documents.iter().filter(|d| d.deleted).count();
        assert_eq!(deleted_before, deleted_after);
        assert!(deleted_after > 0, "the fixture should contain a deleted document");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_wrong_format_version_is_refused_rather_than_misread() {
        let dir = temp_dir("version");
        let original = small_index();
        save(&original, &dir).unwrap();

        // Corrupt the version stamp in the config header of the live generation.
        let p = generation_dir(&dir, read_current(&dir).unwrap()).join("config.bin");
        let mut bytes = fs::read(&p).unwrap();
        bytes[8] = 99;
        fs::write(&p, bytes).unwrap();

        let err = match load(&dir) {
            Ok(_) => panic!("a corrupted version stamp was accepted"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("format version"),
            "expected a version complaint, got: {err:#}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_that_is_not_an_index_is_refused() {
        let dir = temp_dir("garbage");
        fs::create_dir_all(&dir).unwrap();
        fs::write(path(&dir, "config.bin"), b"this is not an index at all").unwrap();
        assert!(load(&dir).map(|_| ()).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    /// The defect this format version exists to fix. An index built with every
    /// ranking setting away from its default used to reopen with the compiled-in
    /// defaults, so a saved index answered differently from the index that had
    /// been saved, and nothing said so.
    #[test]
    fn every_ranking_setting_survives_the_round_trip() {
        let dir = temp_dir("ranking-config");
        let mut original = small_index();
        original.set_fusion(Fusion::TheoreticalMinMax { vector_weight: 0.62 });
        original.set_lexical_coverage(2.25);
        original.set_lexical_proximity(0.4);
        original.set_lexical_prefix(true);
        original.set_lexical_tier(true);
        original.set_lexical_phrase(0.7);
        original.set_lexical_rescore_depth(11);
        original.set_mmr_lambda(0.8);
        original.set_adaptive_fusion(
            true,
            AdaptiveWeights {
                base: 0.4,
                out_of_vocabulary_gain: 0.3,
                identifier_gain: 0.25,
                separation_gain: 0.15,
                coverage_gain: 0.1,
                floor: 0.1,
                ceiling: 0.9,
            },
        );

        save(&original, &dir).unwrap();
        let loaded = load(&dir).unwrap();

        let a = original.config();
        let b = loaded.config();
        assert_eq!(
            format!("{:?}", a.fusion),
            format!("{:?}", b.fusion),
            "the fusion method did not survive"
        );
        assert_eq!(a.lexical_coverage, b.lexical_coverage);
        assert_eq!(a.lexical_proximity, b.lexical_proximity);
        assert_eq!(a.lexical_prefix, b.lexical_prefix);
        assert_eq!(a.lexical_tier, b.lexical_tier);
        assert_eq!(a.lexical_phrase, b.lexical_phrase);
        assert_eq!(a.lexical_rescore_depth, b.lexical_rescore_depth);
        assert_eq!(a.mmr_lambda, b.mmr_lambda);
        assert_eq!(a.adaptive_fusion, b.adaptive_fusion);
        assert_eq!(format!("{:?}", a.adaptive), format!("{:?}", b.adaptive));

        fs::remove_dir_all(&dir).ok();
    }

    /// The settings surviving is not the point on its own; the point is that the
    /// reopened index gives the same answers. A non-default configuration is
    /// deliberately used, because the defaults would agree either way.
    #[test]
    fn a_non_default_index_answers_identically_after_reopening() {
        let dir = temp_dir("ranking-answers");
        let mut original = small_index();
        original.set_fusion(Fusion::TheoreticalMinMax { vector_weight: 0.62 });
        original.set_lexical_coverage(2.25);
        original.set_lexical_proximity(0.4);
        original.set_lexical_tier(true);
        original.set_lexical_phrase(0.7);
        save(&original, &dir).unwrap();
        let loaded = load(&dir).unwrap();

        let query = original.vectors().copy_of(11);
        let f_orig = original.compile(&Filter::default());
        let f_load = loaded.compile(&Filter::default());
        let a: Vec<(u32, f32)> = original
            .hybrid_search("offer eligibility", &query, &f_orig, 10, Some(64))
            .iter()
            .map(|h| (h.chunk, h.score))
            .collect();
        let b: Vec<(u32, f32)> = loaded
            .hybrid_search("offer eligibility", &query, &f_load, 10, Some(64))
            .iter()
            .map(|h| (h.chunk, h.score))
            .collect();
        assert_eq!(a, b, "the reopened index ranked differently");
        assert!(!a.is_empty(), "the fixture should return hits at all");

        fs::remove_dir_all(&dir).ok();
    }


    #[test]
    fn a_save_publishes_a_new_generation_rather_than_overwriting_the_old_one() {
        let dir = temp_dir("generations");
        let index = small_index();
        save(&index, &dir).unwrap();
        let first = read_current(&dir).unwrap();
        save(&index, &dir).unwrap();
        let second = read_current(&dir).unwrap();

        assert!(second > first, "the pointer did not move: {first} then {second}");
        assert!(
            generation_dir(&dir, first).join("store.bin").exists(),
            "the superseded generation was removed while a reader could still hold it"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A crash during a save must leave the previous index openable. Simulated by
    /// writing a new generation's files and never publishing the pointer, which is
    /// exactly the state a process killed mid-save leaves behind.
    #[test]
    fn a_half_written_generation_is_ignored_and_the_previous_one_still_opens() {
        let dir = temp_dir("crash");
        let index = small_index();
        save(&index, &dir).unwrap();
        let live = read_current(&dir).unwrap();

        let orphan = generation_dir(&dir, live + 1);
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("store.bin"), b"truncated garbage").unwrap();

        let reopened = load(&dir).unwrap();
        assert_eq!(reopened.store().n_chunks(), index.store().n_chunks());
        assert!(is_readable(&dir));
        fs::remove_dir_all(&dir).ok();
    }

    /// A reader that is handed a generation number and finds the directory gone
    /// has been lapped by a save; re-reading the pointer gets it the generation
    /// that replaced the one it lost.
    #[test]
    fn a_generation_reclaimed_mid_read_sends_the_reader_to_the_current_one() {
        let dir = temp_dir("reclaimed");
        let index = small_index();
        save(&index, &dir).unwrap();
        save(&index, &dir).unwrap();
        let live = read_current(&dir).unwrap();

        // Point at a generation that no longer exists, as a save that reclaimed it
        // would leave things for a reader mid-flight, then let the pointer move on.
        let vanished = generation_dir(&dir, live + 1);
        fs::write(dir.join("current.tmp"), format!("g{:012}", live + 1)).unwrap();
        fs::rename(dir.join("current.tmp"), dir.join("current")).unwrap();
        assert!(!vanished.exists());
        assert!(load(&dir).is_err(), "a pointer to nothing has nothing to fall back to");

        // With the pointer naming a generation that is there, the read succeeds.
        fs::write(dir.join("current.tmp"), format!("g{live:012}")).unwrap();
        fs::rename(dir.join("current.tmp"), dir.join("current")).unwrap();
        assert_eq!(load(&dir).unwrap().store().n_chunks(), index.store().n_chunks());
        fs::remove_dir_all(&dir).ok();
    }

    /// A corrupt file is refused rather than retried. Re-reading a pointer cannot
    /// repair a bad file, and retrying would turn a loud failure into a slow one.
    #[test]
    fn a_corrupt_generation_is_refused_rather_than_retried() {
        let dir = temp_dir("corrupt-no-retry");
        let index = small_index();
        save(&index, &dir).unwrap();
        let live = generation_dir(&dir, read_current(&dir).unwrap());
        fs::write(live.join("store.bin"), b"not an index").unwrap();

        let error = match load(&dir) {
            Ok(_) => panic!("a corrupt store was accepted"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("index file") || format!("{error:#}").contains("header"),
            "expected a header complaint, got: {error:#}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_that_was_never_saved_to_is_not_readable() {
        let dir = temp_dir("empty");
        fs::create_dir_all(&dir).unwrap();
        assert!(!is_readable(&dir));
        assert!(load(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    /// Only three generations may ever be on disk: the live one and one superseded
    /// one, plus whatever the save in progress is writing. Otherwise a nightly
    /// compaction fills the disk with 2.4 GB copies of a corpus.
    #[test]
    fn superseded_generations_are_reclaimed() {
        let dir = temp_dir("reclaim");
        let index = small_index();
        for _ in 0..5 {
            save(&index, &dir).unwrap();
        }
        let generations = existing_generations(&dir);
        assert!(
            generations.len() <= 2,
            "{} generations left on disk: {generations:?}",
            generations.len()
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// The reason the lexical index is now a file: deriving it means running the
    /// analyzer over the whole corpus. Reading it back has to produce exactly the
    /// index that was written, or a reopened server ranks differently from the one
    /// that saved.
    #[test]
    fn the_lexical_index_survives_the_round_trip_exactly() {
        let dir = temp_dir("lexical");
        let original = small_index();
        save(&original, &dir).unwrap();
        let loaded = load(&dir).unwrap();

        let a = original.compile(&Filter::default());
        let b = loaded.compile(&Filter::default());
        for query in ["offer eligibility", "chunk 42 token42", "title"] {
            let left = original.lexical_search(query, &a, 20);
            let right = loaded.lexical_search(query, &b, 20);
            assert_eq!(left.len(), right.len(), "{query} returned different counts");
            for (l, r) in left.iter().zip(right.iter()) {
                assert_eq!(l.chunk, r.chunk, "{query} ranked differently after reloading");
                assert_eq!(l.score.to_bits(), r.score.to_bits(), "{query} scored differently");
                assert_eq!(l.matched_terms, r.matched_terms);
            }
        }
        fs::remove_dir_all(&dir).ok();
    }

    /// Everything the new store format carries has to come back: flags, named
    /// attribute sets, per-document chunk counts and the tombstone accounting.
    #[test]
    fn the_new_store_columns_survive_the_round_trip() {
        let dir = temp_dir("columns");
        let mut original = small_index();
        original.append(
            vec![ChunkInput {
                source: "email".into(),
                external_doc_id: "mail-1".into(),
                chunk_index: 0,
                content: "a message from terri".into(),
                title: "subject".into(),
                url: "u".into(),
                author: Some("Terri Shaw".into()),
                author_id: Some("terri@example.org".into()),
                updated_at: Some(4242),
                attributes: vec![(
                    "participant".to_string(),
                    vec!["terri@example.org".into(), "jason@example.com".into()],
                )],
                flags: vec!["has_attachment".into()],
                ..Default::default()
            }],
            &[{
                let mut v = vec![0.25f32; 16];
                normalize(&mut v);
                v
            }],
        );
        original.tombstone("slack", "d3");

        save(&original, &dir).unwrap();
        let mut loaded = load(&dir).unwrap();

        assert_eq!(loaded.store().live_chunks, original.store().live_chunks);
        assert_eq!(loaded.store().deleted_chunks, original.store().deleted_chunks);
        assert!(loaded.store().flag_bit("has_attachment").is_some());
        assert_eq!(loaded.store().chunk_external_id(0), original.store().chunk_external_id(0));
        assert!(loaded.store().attribute_dictionary("participant").is_some());

        // The filters that read those columns must behave identically.
        let with_attachment =
            loaded.compile(&Filter::default().with_flag("has_attachment", true));
        assert_eq!(with_attachment.pass_count(), 1);
        let by_participant = loaded.compile(
            &Filter::default()
                .with_attribute(crate::filter::AttributeFilter::containing("participant", "jason@")),
        );
        assert_eq!(by_participant.pass_count(), 1);
        let before = loaded.compile(&Filter { updated_before: Some(4242), ..Default::default() });
        assert!(before.pass_count() > 0);

        // And the document lookup rebuilds over live documents only, so a
        // tombstoned document stays tombstoned across a restart.
        assert!(!loaded.tombstone("slack", "d3"));
        fs::remove_dir_all(&dir).ok();
    }

    /// A reopened index must be appendable, which means every counter the append
    /// path reads has to have survived the round trip too.
    #[test]
    fn a_reopened_index_can_still_be_appended_to() {
        let dir = temp_dir("append-after-load");
        let original = small_index();
        save(&original, &dir).unwrap();
        let mut loaded = load(&dir).unwrap();

        let before = loaded.store().n_chunks();
        let stats = loaded.append(
            vec![ChunkInput {
                source: "slack".into(),
                external_doc_id: "fresh".into(),
                content: "a chunk about tirzepatide".into(),
                title: "fresh".into(),
                url: "u".into(),
                ..Default::default()
            }],
            &[{
                let mut v = vec![0.5f32; 16];
                normalize(&mut v);
                v
            }],
        );
        assert!(stats.committed);
        assert_eq!(loaded.store().n_chunks(), before + 1);
        let f = loaded.compile(&Filter::default());
        assert_eq!(loaded.lexical_search("tirzepatide", &f, 5).len(), 1);

        // And it saves again, into a further generation.
        save(&loaded, &dir).unwrap();
        assert_eq!(load(&dir).unwrap().store().n_chunks(), before + 1);
        fs::remove_dir_all(&dir).ok();
    }

    /// Every index already on disk was written under the engine's old name, whose
    /// eight-byte magic differs from the one this build writes. The bytes after the
    /// magic are identical, so a rename must not cost a rebuild: this rewrites a
    /// saved generation's headers back to the old magic and insists the index still
    /// opens and still answers.
    #[test]
    fn an_index_written_under_the_old_magic_still_opens() {
        let dir = temp_dir("legacy-magic");
        let original = small_index();
        save(&original, &dir).unwrap();

        // Age every file in the live generation back to the pre-rename header.
        let generation = fs::read_to_string(dir.join("current")).unwrap();
        let generation = dir.join(generation.trim());
        for entry in fs::read_dir(&generation).unwrap() {
            let path = entry.unwrap().path();
            let mut bytes = fs::read(&path).unwrap();
            assert_eq!(&bytes[..8], MAGIC, "a fresh save writes the current magic");
            bytes[..8].copy_from_slice(LEGACY_MAGIC);
            fs::write(&path, &bytes).unwrap();
        }

        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.store().n_chunks(), original.store().n_chunks());
        let filter = loaded.compile(&Filter::default());
        assert_eq!(loaded.lexical_search("token5", &filter, 5).len(), 1);

        // And re-saving moves it onto the current magic without a rebuild.
        save(&loaded, &dir).unwrap();
        let generation = fs::read_to_string(dir.join("current")).unwrap();
        let config = fs::read(dir.join(generation.trim()).join("config.bin")).unwrap();
        assert_eq!(&config[..8], MAGIC);
        fs::remove_dir_all(&dir).ok();
    }
}
