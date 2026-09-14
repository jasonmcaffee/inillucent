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
//!
//! Invariant: **a file written by a different layout is refused rather than
//! misread, and no length read out of one sizes a buffer before the bytes are
//! known to be there.** Each section carries a version-stamped header, and each
//! is read in bounded steps: these bytes come out of a database file somebody
//! else could have written, and an allocation sized from a number in them is
//! not an error that can be returned - it aborts the process.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::binio;
use crate::bm25::Bm25Index;
use crate::hnsw::{Hnsw, HnswParams};
use crate::index::{Index, IndexConfig};
use crate::rank::{AdaptiveWeights, Fusion};
use crate::store::{ChunkInput, Store};
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
    r.read_exact(&mut magic)
        .context("reading the file header")?;
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
    r.read_exact(&mut magic)
        .context("reading the file header")?;
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
    /// Which distance the vectors were stored to answer - `""` or `"cosine"`
    /// for cosine, `"l2"` for L2.
    ///
    /// **Defaulted on read, to the only metric this build ever wrote before
    /// this field existed.** Every generation on disk before this ticket was
    /// built under cosine, because cosine was the only metric there was, so
    /// an absent value here is not an unknown - it is the answer, the same
    /// way an absent `resident_vectors` above it means the file backing that
    /// predates the setting. Bumping `FORMAT_VERSION` over this would refuse
    /// every one of those generations outright, which is the opposite of
    /// "the old one reads as cosine"; `#[serde(default)]` is what this file
    /// already uses for exactly this kind of addition, and reading the old
    /// generations is the reason to keep using it here too.
    #[serde(default)]
    metric: String,
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
            "minmax" => Fusion::NormalizedScore {
                vector_weight: self.vector_weight,
            },
            "convex" => Fusion::Convex {
                vector_weight: self.vector_weight,
            },
            "tmm" => Fusion::TheoreticalMinMax {
                vector_weight: self.vector_weight,
            },
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
            metric: match cfg.metric {
                crate::distance::Metric::Cosine => "cosine".to_string(),
                crate::distance::Metric::L2 => "l2".to_string(),
            },
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
            metric: match self.metric.trim() {
                // Empty is a generation saved before this field existed, and
                // this build never wrote anything but cosine until now - so
                // it is read as cosine rather than refused. See the field's
                // own comment for why that is a read default and not a
                // format-version bump.
                "" | "cosine" => crate::distance::Metric::Cosine,
                "l2" => crate::distance::Metric::L2,
                // A name from a build newer than this one, or a corrupted
                // record: refused rather than defaulted, the same rule
                // `SavedFusion::to_fusion` applies to an unknown method - an
                // index that answers differently from the one that was saved
                // is worse than one that will not open.
                other => anyhow::bail!("the saved index names an unknown metric {other}"),
            },
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
    [
        "store.bin",
        "vectors.bin",
        "config.bin",
        "graph.bin",
        "lexical.bin",
    ]
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
    // Read once, ahead of the vectors: the metric it names decides whether
    // those bytes are normalized or raw, which `VectorSet` has to be told at
    // construction rather than guess from the floats themselves.
    let config = saved.to_config()?;
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
            VectorSet::from_raw(
                dims,
                config.metric,
                crate::binio::read_pod_vec::<f32>(&mut r, dims * n)?,
            )
        } else {
            // The header is eight magic bytes, one kind byte, then the two counts.
            let offset = VECTOR_HEADER_BYTES;
            VectorSet::from_file(dims, config.metric, n, File::open(&vector_path)?, offset)
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

    Index::from_parts(config, store, vectors, graph, Some(lexical))
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
    // Read before the vectors, for the same reason `load_generation` does:
    // the metric decides whether these bytes are normalized or raw.
    let config = saved.to_config()?;
    let store = Store::read_from(&mut read_section(r)?.as_slice()).context("reading the store")?;
    let vectors = {
        let bytes = read_section(r)?;
        let mut cursor = bytes.as_slice();
        let mut buf4 = [0u8; 4];
        cursor.read_exact(&mut buf4)?;
        let dims = u32::from_le_bytes(buf4) as usize;
        cursor.read_exact(&mut buf4)?;
        let count = u32::from_le_bytes(buf4) as usize;
        // Both numbers come out of the file, so their product is one an
        // attacker chooses too: `u32::MAX * u32::MAX` is within a factor of two
        // of `usize::MAX` on a 64-bit target, and a wrapped product would ask
        // for a small buffer and then be read into as if it were a large one.
        let wanted = dims
            .checked_mul(count)
            .ok_or_else(|| anyhow::anyhow!("a vector section claims {count} vectors of {dims}"))?;
        VectorSet::from_raw(
            dims,
            config.metric,
            crate::binio::read_pod_vec::<f32>(&mut cursor, wanted)?,
        )
    };
    let params = saved.hnsw_params();
    let graph = Hnsw::read_graph(&mut read_section(r)?.as_slice(), params)?;
    let lexical = Bm25Index::read_from(&mut read_section(r)?.as_slice())
        .context("reading the lexical index")?;
    Index::from_parts(config, store, vectors, graph, Some(lexical))
}

/// Writes one length-prefixed section.
fn section(w: &mut impl Write, bytes: &[u8]) -> Result<()> {
    w.write_all(&(bytes.len() as u64).to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

/// Reads one length-prefixed section.
///
/// The bytes may have come out of a database file somebody else could write to,
/// so the length is a number an attacker chooses.
///
/// **The ceiling below is not what makes this safe, and it used to be all there
/// was (task-1932, H4).** It was `1 << 40`, and a claimed length anywhere under
/// a terabyte went straight into `vec![0u8; length as usize]` before
/// `read_exact` found out whether the file had the bytes. An allocation that
/// large does not return an error: it goes through `handle_alloc_error`, which
/// aborts the process. One corrupt byte in a `.rdb` segment row - reached on an
/// ordinary `SELECT`, through `inillucent_search`'s `load_segment_bytes` - took
/// the whole process down instead of returning `inillucent_search: unreadable
/// segment`. The doc comment here named exactly that failure as the one it
/// existed to prevent.
///
/// `binio::read_records` is what prevents it now: the buffer grows as the bytes
/// arrive, so a length the source cannot satisfy costs one megabyte and then
/// fails with `UnexpectedEof`. The ceiling stays as an early refusal that names
/// the number, and comes down to 64 GiB - a section is one generation's store,
/// vectors, graph or lexical index, and this reads the whole of it into memory,
/// so a section larger than that could not be used even if it were real.
fn read_section(r: &mut impl Read) -> Result<Vec<u8>> {
    let mut length = [0u8; 8];
    r.read_exact(&mut length)
        .context("reading a section length")?;
    let length = u64::from_le_bytes(length);
    const CEILING: u64 = 1 << 36;
    if length > CEILING {
        anyhow::bail!(
            "index section claims {length} bytes, which is not a length this build reads"
        );
    }
    let bytes =
        crate::binio::read_pod_vec::<u8>(r, length as usize).context("reading a section")?;
    Ok(bytes)
}

// -- segment deltas: a segment writable in pieces ---------------------------
//
// `write_index`/`read_index` above serialise a whole index in one call, which
// is right for a flush (the batch is already bounded) and for `compact`
// building a clean generation from scratch, but wrong for a size tiered
// merge's own checkpoint: the accumulator it is folding into can already be a
// large fraction of the corpus, so re-serialising the whole thing on every
// checkpoint - what `crates/inillucent-search` used to do - makes one commit's
// write proportional to the corpus however finely the folding work itself is
// spread across commits (`docs/roadmap.md` item 10, phase 2).
//
// A segment delta is the fix: a checkpoint's bytes are a pointer to whatever
// it continues, plus only the rows that checkpoint itself folded. Reading one
// back means walking the chain of pointers and replaying each link's own
// content, in order, onto whatever the chain bottoms out at - an ordinary
// `KIND_STREAM` blob, or another segment delta one level further back. A
// chain therefore costs a caller who resolves it the same thing a single
// large blob always cost to read; what changes is that *writing* the newest
// link never touches the bytes of any earlier one.
//
// **Replay has to reproduce the real work, not redo it.** The store and the
// vectors are replayed as operations - `Store::add_chunks`, `VectorSet::push`
// - because appending to them is a pure, cheap function of the rows given,
// no more expensive on replay than it was to begin with. The graph and the
// lexical index are the opposite: an insert's cost is a search proportional
// to the graph it is searching, and indexing a chunk means tokenising its
// text, so *re-running* either on every chain resolution costs what building
// them cost the first time, on every single reload. An early version of this
// format replayed the graph and the lexical index as operations too, the
// same as the store, and it measured *worse* than the whole-accumulator
// rewrite it was replacing - `write_latency`'s 100,000 document arm went from
// about 105 seconds to that, not down from it, because reloading a chain
// several links deep re-tokenised and re-inserted everything in it, every
// time. So the graph and the lexical index are instead recorded as *content*
// - which adjacency lists actually ended up different, which postings
// actually got added - captured once, as a byproduct of the one real fold
// that already had to happen, and replayed by copying that content directly
// (`Hnsw::apply_touched`, `Bm25Index::apply_lexical_delta`). Replaying a
// chain then costs what copying bytes costs, which is what serialising it
// always cost, on either side of this ticket.

/// The tag marking a segment written as a chain of small checkpoints rather
/// than as one monolithic stream.
///
/// A delta's own bytes are never mixed into a `KIND_STREAM` blob and a
/// `KIND_STREAM` blob is never asked to carry a base pointer - the two tags
/// are how a caller holding a `%_gen` row's bytes tells which reader to use
/// before it has read anything past the header, the same way `KIND_STORE`
/// through `KIND_LEXICAL` already say which section of `save`'s directory
/// form a file holds. See [`is_segment_delta`].
const KIND_SEGMENT_DELTA: u8 = 7;

/// Points at the id this delta continues: an eight byte little endian `i64`.
const PART_BASE: u8 = 1;
/// One folded input's own batch: the chunks and vectors it contributed, then
/// the `(source, external id)` pairs it bare-tombstoned - in that order,
/// because a later batch's tombstone may need an *earlier* batch's put to
/// already have landed (a bare tombstone in one input can shadow a live
/// chunk a different, older input still carries for the same id), which is
/// exactly the order `merge::fold_segment_recording` applies them in live.
/// One segment delta carries one of these per input the checkpoint folded,
/// in fold order - never one combined batch for the whole checkpoint, which
/// would let a later input's tombstone run before an earlier input's put
/// that it depends on.
const PART_BATCH: u8 = 2;
/// The graph's recorded content for this checkpoint: its entry point, the
/// top layer of every node added, and every adjacency list that changed -
/// see [`GraphRecording`].
const PART_GRAPH: u8 = 3;
/// The lexical index's recorded content for this checkpoint - see
/// [`crate::bm25::LexicalDelta`].
const PART_LEXICAL: u8 = 4;
/// Present only on a chain's final link: the chunk and document counts the
/// fully replayed chain must produce, and the marker that makes this link -
/// and therefore the chain up to it - a complete, readable segment.
///
/// **This is the whole of how a partial segment stays invisible.** Every
/// earlier link a merge writes while it is still folding inputs has no seal
/// at all, and [`parse_segment_delta`] reports that plainly rather than
/// guessing: `ParsedDelta::sealed` is `None` for one, and a caller that
/// requires a finished segment - a query, `finish_merge`, an integrity check -
/// is the one that decides whether the absence of a seal is refused,
/// exactly as `crates/inillucent-search`'s `load_segment` does for every
/// caller except the merge itself resuming its own checkpoint.
const PART_SEAL: u8 = 5;

/// A ceiling on any count this format reads before allocating for it - the
/// same defence [`read_section`]'s byte ceiling is, restated for a count of
/// items rather than a count of bytes, because a `%_gen` row is exactly as
/// untrusted as any other database page.
const MAX_PART_LIST: u64 = 100_000_000;

/// Whether `bytes` is a segment delta rather than an ordinary `KIND_STREAM`
/// blob.
///
/// Reads nothing but the one byte the two forms disagree on - the kind byte
/// `header` always writes at offset twelve, after the eight byte magic and
/// the four byte version - so a caller can decide which reader to use before
/// paying for a full parse. A buffer too short to hold a header answers
/// `false` rather than erroring: whichever reader is tried next will refuse
/// it with a proper header complaint.
/// @param bytes - the stored bytes
pub fn is_segment_delta(bytes: &[u8]) -> bool {
    bytes.get(12) == Some(&KIND_SEGMENT_DELTA)
}

/// One folded input's own contribution to a checkpoint: what it put, and
/// what it bare-tombstoned. See `PART_BATCH` for why a checkpoint that
/// folded several inputs keeps them as separate batches rather than one
/// combined list.
#[derive(Default)]
pub struct DeltaBatch {
    /// The chunks this input contributed, each with its vector, in fold order.
    pub puts: Vec<(ChunkInput, Vec<f32>)>,
    /// The `(source, external id)` pairs this input bare-tombstoned.
    pub tombstoned: Vec<(String, String)>,
}

/// What a checkpoint's own fold recorded in the graph: its entry point, the
/// top layer of every node added since the checkpoint this one continues,
/// and every adjacency list that ended up different - the *result* of the
/// insert, not the insert itself. See the section header above for why.
#[derive(Default)]
pub struct GraphRecording {
    /// The graph's entry point after this checkpoint's fold.
    pub entry: Option<u32>,
    /// How many levels the graph has after this checkpoint's fold.
    ///
    /// **Needed even though every touched entry already names its own
    /// layer.** A node promoted to a level higher than the graph has ever
    /// held leaves every layer above the old top legitimately empty at
    /// every node - "empty lists above the old entry's level", exactly what
    /// a live insert leaves behind - so a level like that can carry zero
    /// touched entries and still have to exist when the graph is
    /// serialised. Without this a replay that only ever grows `layers` in
    /// response to a touched entry would never create that level at all.
    pub layers_len: usize,
    /// The top layer of every node added since the checkpoint this one
    /// continues, in node order.
    pub node_top_tail: Vec<u8>,
    /// Every `(layer, node)` this checkpoint's fold changed, with that
    /// node's current neighbour list at that layer.
    pub touched: Vec<(u8, u32, Vec<u32>)>,
}

/// Writes one checkpoint of a segment being built up in pieces.
///
/// This is the write side of [`parse_segment_delta`]; see that function, the
/// tags' own doc comments and the section header above for the format and
/// the invariant it keeps.
/// @param w - where the bytes go
/// @param base - the id this checkpoint continues, or `None` to start a
///   chain with no prior content at all
/// @param batches - one entry per input this checkpoint folded, in fold order
/// @param graph - what this checkpoint's fold changed in the graph
/// @param lexical - what this checkpoint's fold added to the lexical index,
///   or `None` if nothing was indexed (a checkpoint that only tombstoned)
/// @param seal - `Some((chunks, documents))` once every input a merge owns
///   has been folded, sealing the chain as a complete segment; `None` for a
///   checkpoint a later commit will still extend
pub fn write_segment_delta(
    w: &mut impl Write,
    base: Option<i64>,
    batches: &[DeltaBatch],
    graph: &GraphRecording,
    lexical: Option<&crate::bm25::LexicalDelta>,
    seal: Option<(u64, u64)>,
) -> Result<()> {
    header(w, KIND_SEGMENT_DELTA)?;
    if let Some(id) = base {
        write_part(w, PART_BASE, |buf| {
            buf.extend_from_slice(&id.to_le_bytes());
            Ok(())
        })?;
    }
    for batch in batches {
        write_part(w, PART_BATCH, |buf| {
            buf.extend_from_slice(&(batch.puts.len() as u64).to_le_bytes());
            for (chunk, vector) in &batch.puts {
                write_chunk_input(buf, chunk)?;
                write_vector_f32(buf, vector);
            }
            buf.extend_from_slice(&(batch.tombstoned.len() as u64).to_le_bytes());
            for (source, id) in &batch.tombstoned {
                binio::write_str(buf, source)?;
                binio::write_str(buf, id)?;
            }
            Ok(())
        })?;
    }
    write_part(w, PART_GRAPH, |buf| {
        buf.extend_from_slice(&graph.entry.unwrap_or(u32::MAX).to_le_bytes());
        buf.extend_from_slice(&(graph.layers_len as u64).to_le_bytes());
        buf.extend_from_slice(&(graph.node_top_tail.len() as u64).to_le_bytes());
        buf.extend_from_slice(&graph.node_top_tail);
        buf.extend_from_slice(&(graph.touched.len() as u64).to_le_bytes());
        for (layer, node, neighbours) in &graph.touched {
            buf.push(*layer);
            buf.extend_from_slice(&node.to_le_bytes());
            buf.extend_from_slice(&(neighbours.len() as u64).to_le_bytes());
            buf.extend_from_slice(bytemuck::cast_slice(neighbours));
        }
        Ok(())
    })?;
    if let Some(lexical) = lexical {
        write_part(w, PART_LEXICAL, |buf| {
            buf.extend_from_slice(&lexical.range.start.to_le_bytes());
            buf.extend_from_slice(&lexical.range.end.to_le_bytes());
            buf.extend_from_slice(&(lexical.chunk_lengths.len() as u64).to_le_bytes());
            buf.extend_from_slice(bytemuck::cast_slice(&lexical.chunk_lengths));
            buf.extend_from_slice(&(lexical.chunk_heading_lengths.len() as u64).to_le_bytes());
            buf.extend_from_slice(bytemuck::cast_slice(&lexical.chunk_heading_lengths));
            buf.extend_from_slice(&(lexical.positions.len() as u64).to_le_bytes());
            buf.extend_from_slice(bytemuck::cast_slice(&lexical.positions));
            buf.extend_from_slice(&(lexical.postings.len() as u64).to_le_bytes());
            for (term, posting) in &lexical.postings {
                binio::write_str(buf, term)?;
                buf.extend_from_slice(&posting.chunk.to_le_bytes());
                buf.extend_from_slice(&posting.positions_at.to_le_bytes());
                buf.extend_from_slice(&posting.term_frequency.to_le_bytes());
            }
            Ok(())
        })?;
    }
    if let Some((chunks, documents)) = seal {
        write_part(w, PART_SEAL, |buf| {
            buf.extend_from_slice(&chunks.to_le_bytes());
            buf.extend_from_slice(&documents.to_le_bytes());
            Ok(())
        })?;
    }
    Ok(())
}

/// Writes one length-prefixed part: a one byte tag, an eight byte length,
/// then whatever `body` wrote into a scratch buffer.
///
/// Buffered rather than streamed straight to `w`, because the length has to
/// be written before the bytes it counts and `w` is not assumed seekable -
/// the same reason `write_index` builds each of its own sections into a
/// `Vec<u8>` first.
/// @param w - where the part goes
/// @param tag - which part this is
/// @param body - writes the part's payload into a fresh buffer
fn write_part(
    w: &mut impl Write,
    tag: u8,
    body: impl FnOnce(&mut Vec<u8>) -> Result<()>,
) -> Result<()> {
    let mut buf = Vec::new();
    body(&mut buf)?;
    w.write_all(&[tag])?;
    w.write_all(&(buf.len() as u64).to_le_bytes())?;
    w.write_all(&buf)?;
    Ok(())
}

/// Writes one chunk's every field, so a delta can replay
/// `Index::replace_document` with the exact input it was given rather than a
/// derived approximation of it.
/// @param w - where the bytes go
/// @param chunk - the chunk to write
fn write_chunk_input(w: &mut impl Write, chunk: &ChunkInput) -> Result<()> {
    binio::write_str(w, &chunk.source)?;
    binio::write_str(w, &chunk.external_doc_id)?;
    binio::write_u32(w, chunk.chunk_index)?;
    write_str_vec(w, &chunk.heading_path)?;
    binio::write_str(w, &chunk.content)?;
    binio::write_str(w, &chunk.title)?;
    binio::write_str(w, &chunk.url)?;
    write_opt_str(w, chunk.space_key.as_deref())?;
    write_opt_str(w, chunk.author.as_deref())?;
    write_opt_str(w, chunk.author_id.as_deref())?;
    write_opt_i64(w, chunk.updated_at)?;
    write_opt_str(w, chunk.external_chunk_id.as_deref())?;
    write_str_vec(w, &chunk.labels)?;
    w.write_all(&(chunk.attributes.len() as u64).to_le_bytes())?;
    for (name, values) in &chunk.attributes {
        binio::write_str(w, name)?;
        write_str_vec(w, values)?;
    }
    write_str_vec(w, &chunk.flags)?;
    w.write_all(&[u8::from(chunk.deleted)])?;
    Ok(())
}

/// A `Vec<String>` as a count and each string, length prefixed.
fn write_str_vec(w: &mut impl Write, values: &[String]) -> Result<()> {
    w.write_all(&(values.len() as u64).to_le_bytes())?;
    for value in values {
        binio::write_str(w, value)?;
    }
    Ok(())
}

/// An `Option<&str>` as a one byte presence flag and, when present, the text.
fn write_opt_str(w: &mut impl Write, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => {
            w.write_all(&[1])?;
            binio::write_str(w, value)?;
        }
        None => w.write_all(&[0])?,
    }
    Ok(())
}

/// An `Option<i64>` as a one byte presence flag and, when present, the value.
fn write_opt_i64(w: &mut impl Write, value: Option<i64>) -> Result<()> {
    match value {
        Some(value) => {
            w.write_all(&[1])?;
            binio::write_i64(w, value)?;
        }
        None => w.write_all(&[0])?,
    }
    Ok(())
}

/// A vector as a count and its little endian `f32` bytes.
fn write_vector_f32(buf: &mut Vec<u8>, vector: &[f32]) {
    buf.extend_from_slice(&(vector.len() as u64).to_le_bytes());
    buf.extend_from_slice(bytemuck::cast_slice(vector));
}

/// What one segment delta's own bytes record, before its base chain is
/// resolved - resolving it means fetching another id's bytes, which only the
/// caller (a shadow table row, a generation directory) knows how to do.
pub struct ParsedDelta {
    /// The id this delta continues, or `None` for one with no prior content.
    pub base: Option<i64>,
    /// One entry per input this checkpoint folded, in fold order.
    pub batches: Vec<DeltaBatch>,
    /// What this checkpoint's fold changed in the graph.
    pub graph: GraphRecording,
    /// What this checkpoint's fold added to the lexical index, or `None` if
    /// nothing was indexed.
    pub lexical: Option<crate::bm25::LexicalDelta>,
    /// `Some((chunks, documents))` once this is the chain's sealed, complete
    /// link; `None` for a checkpoint a later commit will still extend.
    pub sealed: Option<(u64, u64)>,
}

/// Reads one segment delta's own parts back, without resolving its base.
///
/// A truncated or malformed delta is refused rather than partially trusted:
/// every read here is bounds checked against what is actually left in the
/// buffer, the same rule `read_section` follows for bytes that may have
/// come out of a database another process could write to.
/// @param bytes - one `%_gen` row's worth of bytes, already known to be a
///   segment delta by [`is_segment_delta`]
pub fn parse_segment_delta(bytes: &[u8]) -> Result<ParsedDelta> {
    let mut cursor: &[u8] = bytes;
    check_header(&mut cursor, KIND_SEGMENT_DELTA)?;
    let mut base = None;
    let mut batches = Vec::new();
    let mut graph = GraphRecording::default();
    let mut lexical = None;
    let mut sealed = None;
    while !cursor.is_empty() {
        let tag = take_u8(&mut cursor)?;
        let length = take_u64(&mut cursor)? as usize;
        let payload = take(&mut cursor, length)?;
        let mut inner: &[u8] = payload;
        match tag {
            PART_BASE => base = Some(take_i64(&mut inner)?),
            PART_BATCH => batches.push(take_delta_batch(&mut inner)?),
            PART_GRAPH => graph = take_graph_recording(&mut inner)?,
            PART_LEXICAL => lexical = Some(take_lexical_delta(&mut inner)?),
            PART_SEAL => {
                let chunks = take_u64(&mut inner)?;
                let documents = take_u64(&mut inner)?;
                sealed = Some((chunks, documents));
            }
            other => anyhow::bail!("a segment delta holds an unrecognised part {other}"),
        }
    }
    Ok(ParsedDelta {
        base,
        batches,
        graph,
        lexical,
        sealed,
    })
}

/// Replays one delta's own recorded content onto an already resolved base
/// index.
///
/// The store and the vectors are replayed as operations
/// (`Index::tombstone`/`Index::append_store_and_vectors`), which is cheap and
/// exact because both are pure functions of the rows given. The graph and
/// the lexical index are replayed from their own recorded *content*
/// (`Index::apply_graph_recording`/`Index::apply_lexical_recording`) instead
/// of being reinserted or re-tokenised - see this section's header comment
/// for why redoing either on every chain resolution is not an option.
/// @param base - the index this delta continues, already committed
/// @param delta - one delta's own parsed content
pub fn apply_segment_delta(mut base: Index, delta: &ParsedDelta) -> Result<Index> {
    for batch in &delta.batches {
        for (chunk, vector) in &batch.puts {
            base.tombstone(&chunk.source, &chunk.external_doc_id);
            base.append_store_and_vectors(vec![chunk.clone()], std::slice::from_ref(vector))?;
        }
        for (source, id) in &batch.tombstoned {
            base.tombstone(source, id);
        }
    }
    base.apply_graph_recording(
        delta.graph.entry,
        delta.graph.layers_len,
        &delta.graph.node_top_tail,
        &delta.graph.touched,
    );
    if let Some(lexical) = &delta.lexical {
        base.apply_lexical_recording(lexical);
    }
    Ok(base)
}

/// Reads one `PART_BATCH` payload back.
fn take_delta_batch(cursor: &mut &[u8]) -> Result<DeltaBatch> {
    let n_puts = take_u64(cursor)?;
    if n_puts > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible put batch of {n_puts}");
    }
    let mut puts = Vec::with_capacity((n_puts as usize).min(1024));
    for _ in 0..n_puts {
        let chunk = take_chunk_input(cursor)?;
        let vector = take_vector(cursor)?;
        puts.push((chunk, vector));
    }
    let n_tombstoned = take_u64(cursor)?;
    if n_tombstoned > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible tombstone batch of {n_tombstoned}");
    }
    let mut tombstoned = Vec::with_capacity((n_tombstoned as usize).min(1024));
    for _ in 0..n_tombstoned {
        let source = take_str(cursor)?;
        let id = take_str(cursor)?;
        tombstoned.push((source, id));
    }
    Ok(DeltaBatch { puts, tombstoned })
}

/// Reads one `PART_GRAPH` payload back.
fn take_graph_recording(cursor: &mut &[u8]) -> Result<GraphRecording> {
    let raw_entry = take_u32(cursor)?;
    let entry = if raw_entry == u32::MAX {
        None
    } else {
        Some(raw_entry)
    };
    let layers_len = take_u64(cursor)?;
    if layers_len > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible layer count of {layers_len}");
    }
    let layers_len = layers_len as usize;
    let node_top_len = take_u64(cursor)?;
    if node_top_len > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible node count of {node_top_len}");
    }
    let node_top_tail = take(cursor, node_top_len as usize)?.to_vec();
    let n_touched = take_u64(cursor)?;
    if n_touched > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible touched-node count of {n_touched}");
    }
    let mut touched = Vec::with_capacity((n_touched as usize).min(1024));
    for _ in 0..n_touched {
        let layer = take_u8(cursor)?;
        let node = take_u32(cursor)?;
        let degree = take_u64(cursor)?;
        if degree > MAX_PART_LIST {
            anyhow::bail!("a segment delta claims an implausible degree of {degree}");
        }
        let neighbour_bytes = take(cursor, (degree as usize).saturating_mul(4))?;
        let mut neighbours = vec![0u32; degree as usize];
        bytemuck::cast_slice_mut::<u32, u8>(&mut neighbours).copy_from_slice(neighbour_bytes);
        touched.push((layer, node, neighbours));
    }
    Ok(GraphRecording {
        entry,
        layers_len,
        node_top_tail,
        touched,
    })
}

/// Reads one `PART_LEXICAL` payload back.
fn take_lexical_delta(cursor: &mut &[u8]) -> Result<crate::bm25::LexicalDelta> {
    let start = take_u32(cursor)?;
    let end = take_u32(cursor)?;
    let n_lengths = take_u64(cursor)?;
    if n_lengths > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible chunk count of {n_lengths}");
    }
    let chunk_lengths = take_u32_vec(cursor, n_lengths as usize)?;
    let n_heading = take_u64(cursor)?;
    if n_heading > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible heading count of {n_heading}");
    }
    let chunk_heading_lengths = take_u32_vec(cursor, n_heading as usize)?;
    let n_positions = take_u64(cursor)?;
    if n_positions > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible position count of {n_positions}");
    }
    let positions = take_u32_vec(cursor, n_positions as usize)?;
    let n_postings = take_u64(cursor)?;
    if n_postings > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible posting count of {n_postings}");
    }
    let mut postings = Vec::with_capacity((n_postings as usize).min(1024));
    for _ in 0..n_postings {
        let term = take_str(cursor)?;
        let chunk = take_u32(cursor)?;
        let positions_at = take_u32(cursor)?;
        let term_frequency = take_u32(cursor)?;
        postings.push((
            term,
            crate::bm25::Posting {
                chunk,
                positions_at,
                term_frequency,
            },
        ));
    }
    Ok(crate::bm25::LexicalDelta {
        range: start..end,
        chunk_lengths,
        chunk_heading_lengths,
        positions,
        postings,
    })
}

/// Reads `n` `u32`s off the front of `cursor`.
/// @param cursor - the remaining bytes, advanced past what is taken
/// @param n - how many `u32`s to take
fn take_u32_vec(cursor: &mut &[u8], n: usize) -> Result<Vec<u32>> {
    let bytes = take(cursor, n.saturating_mul(4))?;
    let mut values = vec![0u32; n];
    bytemuck::cast_slice_mut::<u32, u8>(&mut values).copy_from_slice(bytes);
    Ok(values)
}

/// Takes `n` bytes off the front of `cursor`, refusing rather than panicking
/// when fewer than `n` remain.
/// @param cursor - the remaining bytes, advanced past what is taken
/// @param n - how many bytes to take
fn take<'a>(cursor: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if cursor.len() < n {
        anyhow::bail!("a segment delta ended before it should have");
    }
    let (head, tail) = cursor.split_at(n);
    *cursor = tail;
    Ok(head)
}

fn take_u8(cursor: &mut &[u8]) -> Result<u8> {
    Ok(take(cursor, 1)?.first().copied().unwrap_or(0))
}

fn take_u32(cursor: &mut &[u8]) -> Result<u32> {
    let bytes = take(cursor, 4)?;
    let array: [u8; 4] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("a segment delta's integer is malformed"))?;
    Ok(u32::from_le_bytes(array))
}

fn take_u64(cursor: &mut &[u8]) -> Result<u64> {
    let bytes = take(cursor, 8)?;
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("a segment delta's integer is malformed"))?;
    Ok(u64::from_le_bytes(array))
}

fn take_i64(cursor: &mut &[u8]) -> Result<i64> {
    let bytes = take(cursor, 8)?;
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("a segment delta's integer is malformed"))?;
    Ok(i64::from_le_bytes(array))
}

fn take_str(cursor: &mut &[u8]) -> Result<String> {
    let len = take_u32(cursor)? as usize;
    let bytes = take(cursor, len)?;
    String::from_utf8(bytes.to_vec())
        .map_err(|_| anyhow::anyhow!("a segment delta holds text that is not UTF-8"))
}

fn take_opt_str(cursor: &mut &[u8]) -> Result<Option<String>> {
    match take_u8(cursor)? {
        0 => Ok(None),
        _ => Ok(Some(take_str(cursor)?)),
    }
}

fn take_opt_i64(cursor: &mut &[u8]) -> Result<Option<i64>> {
    match take_u8(cursor)? {
        0 => Ok(None),
        _ => Ok(Some(take_i64(cursor)?)),
    }
}

fn take_str_vec(cursor: &mut &[u8]) -> Result<Vec<String>> {
    let n = take_u64(cursor)?;
    if n > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible list length of {n}");
    }
    let mut out = Vec::with_capacity((n as usize).min(1024));
    for _ in 0..n {
        out.push(take_str(cursor)?);
    }
    Ok(out)
}

fn take_chunk_input(cursor: &mut &[u8]) -> Result<ChunkInput> {
    let source = take_str(cursor)?;
    let external_doc_id = take_str(cursor)?;
    let chunk_index = take_u32(cursor)?;
    let heading_path = take_str_vec(cursor)?;
    let content = take_str(cursor)?;
    let title = take_str(cursor)?;
    let url = take_str(cursor)?;
    let space_key = take_opt_str(cursor)?;
    let author = take_opt_str(cursor)?;
    let author_id = take_opt_str(cursor)?;
    let updated_at = take_opt_i64(cursor)?;
    let external_chunk_id = take_opt_str(cursor)?;
    let labels = take_str_vec(cursor)?;
    let n_attributes = take_u64(cursor)?;
    if n_attributes > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible attribute count of {n_attributes}");
    }
    let mut attributes = Vec::with_capacity((n_attributes as usize).min(1024));
    for _ in 0..n_attributes {
        let name = take_str(cursor)?;
        let values = take_str_vec(cursor)?;
        attributes.push((name, values));
    }
    let flags = take_str_vec(cursor)?;
    let deleted = take_u8(cursor)? != 0;
    Ok(ChunkInput {
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
        external_chunk_id,
        labels,
        attributes,
        flags,
        deleted,
    })
}

fn take_vector(cursor: &mut &[u8]) -> Result<Vec<f32>> {
    let n = take_u64(cursor)?;
    if n > MAX_PART_LIST {
        anyhow::bail!("a segment delta claims an implausible vector width of {n}");
    }
    let n = n as usize;
    let bytes = take(cursor, n.saturating_mul(4))?;
    let mut vector = vec![0f32; n];
    bytemuck::cast_slice_mut::<f32, u8>(&mut vector).copy_from_slice(bytes);
    Ok(vector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distance::normalize;
    use crate::filter::Filter;
    use crate::store::ChunkInput;

    /// H4 (task-1920): a section length the file does not have is an error,
    /// not an abort.
    ///
    /// **What this used to do.** `read_section` checked the claimed length
    /// against a 1 TiB ceiling and then ran `vec![0u8; length as usize]` before
    /// `read_exact` found out whether the bytes were there. An allocation of a
    /// few gigabytes does not return an error - it goes through
    /// `handle_alloc_error`, which aborts the process - so one corrupt byte in
    /// a `.rdb` segment row, reached on an ordinary `SELECT` through
    /// `inillucent_search`'s `load_segment_bytes`, took the whole process down
    /// instead of returning `inillucent_search: unreadable segment`.
    ///
    /// The lengths below are the interesting three: a claim just under the old
    /// ceiling, a claim over the new one, and `u64::MAX`. Each is followed by
    /// four real bytes, so what the source can actually supply is four orders
    /// of magnitude short of what it claims.
    ///
    /// A test that aborts the process is not a test that fails - the harness
    /// reports the whole binary as crashed and says nothing about which case -
    /// which is why the assertion is on the `Err` rather than on a message.
    #[test]
    fn a_section_claiming_more_bytes_than_the_source_has_is_refused() {
        for claimed in [
            2u64 * 1024 * 1024 * 1024,
            (1u64 << 40) - 1,
            1u64 << 36,
            u64::MAX,
        ] {
            let mut bytes: Vec<u8> = Vec::new();
            header(&mut bytes, KIND_STREAM).expect("the header writes");
            bytes.extend_from_slice(&claimed.to_le_bytes());
            bytes.extend_from_slice(b"abcd");
            let read = read_index(&mut bytes.as_slice());
            assert!(
                read.is_err(),
                "a section claiming {claimed} bytes over a four-byte source was accepted"
            );
        }
    }

    /// H4 (task-1920): the same for the counts inside a section.
    ///
    /// `read_pod_vec`, `read_u32_vec`, `read_str` and `read_text` all sized a
    /// buffer from a count they had just read and allocated it before asking
    /// whether the records were there - the same shape as `read_section`, one
    /// layer in. The vectors section is the one that matters most: its `dims`
    /// and `count` are two `u32`s out of the file, and their product is what is
    /// allocated, so `4294967295 * 4294967295` records of four bytes each is
    /// both an absurd allocation and a multiplication that wraps.
    #[test]
    fn a_record_count_larger_than_the_source_is_refused() {
        for (dims, count) in [
            (1_000_000u32, 1_000_000u32),
            (u32::MAX, u32::MAX),
            (16, 500_000_000),
        ] {
            let mut section_bytes: Vec<u8> = Vec::new();
            section_bytes.extend_from_slice(&dims.to_le_bytes());
            section_bytes.extend_from_slice(&count.to_le_bytes());
            section_bytes.extend_from_slice(b"abcd");
            let read = crate::binio::read_pod_vec::<f32>(
                &mut section_bytes.get(8..).unwrap_or_default(),
                (dims as usize).saturating_mul(count as usize),
            );
            assert!(
                read.is_err(),
                "{dims} x {count} records over a four-byte source was accepted"
            );
        }
        // And a string length, which the store reads per chunk.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(b"abcd");
        assert!(crate::binio::read_str(&mut bytes.as_slice()).is_err());
    }

    fn small_index() -> Index {
        let mut index = Index::new(IndexConfig {
            dims: 16,
            quantized: true,
            ..Default::default()
        });
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
                let mut v: Vec<f32> = (0..16)
                    .map(|d| ((i * 16 + d) as f32 * 0.07).sin())
                    .collect();
                normalize(&mut v);
                v
            })
            .collect();
        index.add(chunks, &vectors).expect("the chunks are added");
        index.commit();
        index
    }

    /// The same fixture, built under L2 rather than cosine, with raw
    /// (unnormalized) vectors - normalizing them would defeat the point of a
    /// test that exists to prove L2 keeps their magnitude.
    fn small_l2_index() -> Index {
        let mut index = Index::new(IndexConfig {
            dims: 16,
            metric: crate::distance::Metric::L2,
            ..Default::default()
        });
        let chunks: Vec<ChunkInput> = (0..40)
            .map(|i| ChunkInput {
                source: "confluence".into(),
                external_doc_id: format!("d{i}"),
                content: format!("chunk {i}"),
                title: format!("title {i}"),
                url: format!("https://x/{i}"),
                ..Default::default()
            })
            .collect();
        let vectors: Vec<Vec<f32>> = (0..40)
            .map(|i| {
                (0..16)
                    .map(|d| ((i * 16 + d) as f32 * 0.07).sin() * 3.0)
                    .collect()
            })
            .collect();
        index.add(chunks, &vectors).expect("the chunks are added");
        index.commit();
        index
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "inillucent-persist-test-{name}-{}",
            std::process::id()
        ));
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
            .expect("the query is this index's width and finite")
            .iter()
            .map(|n| n.chunk)
            .collect();
        let b: Vec<u32> = loaded
            .vector_search(&query, &f_load, 10, Some(64))
            .expect("the query is this index's width and finite")
            .iter()
            .map(|n| n.chunk)
            .collect();
        assert_eq!(a, b, "the graph did not survive the round trip");

        let la: Vec<u32> = original
            .lexical_search("offer eligibility", &f_orig, 10)
            .iter()
            .map(|h| h.chunk)
            .collect();
        let lb: Vec<u32> = loaded
            .lexical_search("offer eligibility", &f_load, 10)
            .iter()
            .map(|h| h.chunk)
            .collect();
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
            Filter {
                labels: Some(vec!["design".into()]),
                ..Default::default()
            },
            Filter {
                author: Some("Ada".into()),
                ..Default::default()
            },
            Filter {
                updated_after: Some(1100),
                ..Default::default()
            },
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
        let deleted_before = original
            .store()
            .documents
            .iter()
            .filter(|d| d.deleted)
            .count();
        let deleted_after = loaded
            .store()
            .documents
            .iter()
            .filter(|d| d.deleted)
            .count();
        assert_eq!(deleted_before, deleted_after);
        assert!(
            deleted_after > 0,
            "the fixture should contain a deleted document"
        );
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
        original.set_fusion(Fusion::TheoreticalMinMax {
            vector_weight: 0.62,
        });
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
        original.set_fusion(Fusion::TheoreticalMinMax {
            vector_weight: 0.62,
        });
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
            .expect("the query is this index's width and finite")
            .iter()
            .map(|h| (h.chunk, h.score))
            .collect();
        let b: Vec<(u32, f32)> = loaded
            .hybrid_search("offer eligibility", &query, &f_load, 10, Some(64))
            .expect("the query is this index's width and finite")
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

        assert!(
            second > first,
            "the pointer did not move: {first} then {second}"
        );
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
        assert!(
            load(&dir).is_err(),
            "a pointer to nothing has nothing to fall back to"
        );

        // With the pointer naming a generation that is there, the read succeeds.
        fs::write(dir.join("current.tmp"), format!("g{live:012}")).unwrap();
        fs::rename(dir.join("current.tmp"), dir.join("current")).unwrap();
        assert_eq!(
            load(&dir).unwrap().store().n_chunks(),
            index.store().n_chunks()
        );
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
                assert_eq!(
                    l.chunk, r.chunk,
                    "{query} ranked differently after reloading"
                );
                assert_eq!(
                    l.score.to_bits(),
                    r.score.to_bits(),
                    "{query} scored differently"
                );
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
        original
            .append(
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
            )
            .expect("the chunks are added");
        original.tombstone("slack", "d3");

        save(&original, &dir).unwrap();
        let mut loaded = load(&dir).unwrap();

        assert_eq!(loaded.store().live_chunks, original.store().live_chunks);
        assert_eq!(
            loaded.store().deleted_chunks,
            original.store().deleted_chunks
        );
        assert!(loaded.store().flag_bit("has_attachment").is_some());
        assert_eq!(
            loaded.store().chunk_external_id(0),
            original.store().chunk_external_id(0)
        );
        assert!(loaded.store().attribute_dictionary("participant").is_some());

        // The filters that read those columns must behave identically.
        let with_attachment = loaded.compile(&Filter::default().with_flag("has_attachment", true));
        assert_eq!(with_attachment.pass_count(), 1);
        let by_participant = loaded.compile(&Filter::default().with_attribute(
            crate::filter::AttributeFilter::containing("participant", "jason@"),
        ));
        assert_eq!(by_participant.pass_count(), 1);
        let before = loaded.compile(&Filter {
            updated_before: Some(4242),
            ..Default::default()
        });
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
        let stats = loaded
            .append(
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
            )
            .expect("the append runs");
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

    /// Rewrites a generation's `config.bin` JSON body in place, keeping the
    /// same header. What a byte-for-byte editor of the file would produce,
    /// which is the only way to manufacture a generation this build never
    /// actually wrote - the same technique `an_index_written_under_the_old_magic_still_opens`
    /// uses on the header rather than the body.
    /// @param dir - the generation directory
    /// @param edit - transforms the parsed JSON object
    fn rewrite_config_json(dir: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
        let config_path = path(dir, "config.bin");
        let bytes = fs::read(&config_path).unwrap();
        let header_len = 8 + 4 + 1;
        let (header, body) = bytes.split_at(header_len);
        let mut value: serde_json::Value = serde_json::from_slice(body).unwrap();
        edit(&mut value);
        let mut rewritten = header.to_vec();
        serde_json::to_writer(&mut rewritten, &value).unwrap();
        fs::write(&config_path, rewritten).unwrap();
    }

    /// A generation saved before this ticket has no `metric` key in its
    /// `config.bin` at all - stripping the key, rather than setting it to
    /// `"cosine"`, is what actually reproduces that file rather than a
    /// same-effect stand-in for it. It has to keep answering as a cosine
    /// index, not fail to open.
    #[test]
    fn a_generation_with_no_stored_metric_reads_as_cosine() {
        let dir = temp_dir("no-metric-key");
        let original = small_index();
        save(&original, &dir).unwrap();
        let generation = generation_dir(&dir, read_current(&dir).unwrap());

        rewrite_config_json(&generation, |value| {
            if let Some(object) = value.as_object_mut() {
                let removed = object.remove("metric");
                assert!(
                    removed.is_some(),
                    "the fixture must have written a metric to remove"
                );
            }
        });

        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.config().metric, crate::distance::Metric::Cosine);
        // And it still answers a real query, not just an equal config.
        let query = original.vectors().copy_of(11);
        let filter = loaded.compile(&Filter::default());
        assert!(!loaded
            .vector_search(&query, &filter, 5, Some(64))
            .expect("the query is this index's width and finite")
            .is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    /// A metric this build does not have is refused, the same rule
    /// `SavedFusion::to_fusion` applies to an unknown fusion method.
    #[test]
    fn a_generation_naming_an_unknown_metric_is_refused() {
        let dir = temp_dir("unknown-metric");
        let original = small_index();
        save(&original, &dir).unwrap();
        let generation = generation_dir(&dir, read_current(&dir).unwrap());

        rewrite_config_json(&generation, |value| {
            value["metric"] = serde_json::Value::String("manhattan".to_string());
        });

        let error = match load(&dir) {
            Ok(_) => panic!("an unknown metric name must not be silently read"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("manhattan"),
            "the refusal should name the unrecognised metric: {error:#}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// An L2 index round trips: the metric survives, the vectors keep their
    /// raw magnitude rather than being normalized on the way back in, and the
    /// graph answers the same nearest neighbour before and after reloading.
    #[test]
    fn an_l2_index_survives_the_round_trip_unnormalized() {
        let dir = temp_dir("l2-roundtrip");
        let original = small_l2_index();
        save(&original, &dir).unwrap();
        let loaded = load(&dir).unwrap();

        assert_eq!(loaded.config().metric, crate::distance::Metric::L2);
        for id in [0u32, 7, 39] {
            assert_eq!(
                loaded.vectors().copy_of(id),
                original.vectors().copy_of(id),
                "an L2 vector must not be normalized by a save/load round trip"
            );
        }

        let query = original.vectors().copy_of(3);
        let f_orig = original.compile(&Filter::default());
        let f_load = loaded.compile(&Filter::default());
        let a: Vec<u32> = original
            .vector_search(&query, &f_orig, 5, Some(64))
            .expect("the query is this index's width and finite")
            .iter()
            .map(|n| n.chunk)
            .collect();
        let b: Vec<u32> = loaded
            .vector_search(&query, &f_load, 5, Some(64))
            .expect("the query is this index's width and finite")
            .iter()
            .map(|n| n.chunk)
            .collect();
        assert_eq!(a, b, "the L2 graph did not survive the round trip");
        fs::remove_dir_all(&dir).ok();
    }

    /// The byte-stream form (`write_index`/`read_index`, what a shadow table
    /// row holds) carries the metric the same way the directory form does.
    #[test]
    fn the_byte_stream_form_carries_l2_through_the_round_trip() {
        let original = small_l2_index();
        let mut bytes = Vec::new();
        write_index(&original, &mut bytes).unwrap();
        let loaded = read_index(&mut bytes.as_slice()).unwrap();
        assert_eq!(loaded.config().metric, crate::distance::Metric::L2);
        assert_eq!(loaded.vectors().copy_of(5), original.vectors().copy_of(5));
    }

    // -- segment deltas ------------------------------------------------------

    /// The batch one merge checkpoint would fold: a handful of brand new
    /// documents plus one tombstone of a document already in the base -
    /// `small_index`'s own chunk 0 is source `slack`, document `d0`.
    fn small_batch() -> (Vec<(ChunkInput, Vec<f32>)>, Vec<(String, String)>) {
        let puts: Vec<(ChunkInput, Vec<f32>)> = (0..5)
            .map(|i| {
                let chunk = ChunkInput {
                    source: "slack".into(),
                    external_doc_id: format!("extra{i}"),
                    content: format!("an extra chunk {i} about tirzepatide"),
                    title: format!("extra {i}"),
                    url: "u".into(),
                    labels: vec!["fresh".into()],
                    ..Default::default()
                };
                let mut v: Vec<f32> = (0..16)
                    .map(|d| ((i * 16 + d) as f32 * 0.05).cos())
                    .collect();
                normalize(&mut v);
                (chunk, v)
            })
            .collect();
        let tombstoned = vec![("slack".to_string(), "d0".to_string())];
        (puts, tombstoned)
    }

    /// Folds one batch onto an index the same way
    /// `inillucent_search::merge::fold_segment_recording` does live: tombstone
    /// then append, one document at a time, then the bare tombstones.
    /// @param index - the index being folded into
    /// @param puts - the chunks and vectors to replace or add
    /// @param tombstoned - the `(source, external id)` pairs to bare-tombstone
    fn fold_batch(
        index: &mut Index,
        puts: &[(ChunkInput, Vec<f32>)],
        tombstoned: &[(String, String)],
    ) {
        for (chunk, vector) in puts {
            index
                .replace_document(
                    &chunk.source,
                    &chunk.external_doc_id,
                    vec![chunk.clone()],
                    std::slice::from_ref(vector),
                )
                .expect("the chunks are added");
        }
        for (source, id) in tombstoned {
            index.tombstone(source, id);
        }
    }

    /// A segment written in pieces reads back byte identical to the same
    /// segment written whole: the same base, the same batch folded onto two
    /// independent copies of it - one the ordinary way, one with recording
    /// on so its graph and lexical content can be captured, chained through
    /// a delta and replayed - serialised through the ordinary `write_index`
    /// both times so the comparison is over exactly what a query or a later
    /// merge would see.
    ///
    /// **Fails without the change**: `write_segment_delta`, `parse_segment_delta`
    /// and `apply_segment_delta` do not exist before this ticket, so there is
    /// no "in pieces" side to compare against - the strongest form "fails
    /// without the change" takes.
    #[test]
    fn a_segment_written_in_pieces_reads_back_byte_identical_to_one_written_whole() {
        let base = small_index();
        let mut base_bytes = Vec::new();
        write_index(&base, &mut base_bytes).unwrap();
        let (puts, tombstoned) = small_batch();

        // Written whole: the batch folded in one pass onto a freshly loaded
        // copy of the base, exactly what a live fold does.
        let mut whole = read_index(&mut base_bytes.as_slice()).unwrap();
        fold_batch(&mut whole, &puts, &tombstoned);
        let mut whole_bytes = Vec::new();
        write_index(&whole, &mut whole_bytes).unwrap();

        // The same fold, on another fresh copy, with recording on - this is
        // the one real fold a checkpoint pays for; everything after it is a
        // copy of what was recorded, never a second fold.
        let mut accumulator = read_index(&mut base_bytes.as_slice()).unwrap();
        let before_nodes = accumulator.graph_shape().1;
        accumulator.start_recording();
        fold_batch(&mut accumulator, &puts, &tombstoned);
        let graph = GraphRecording {
            entry: accumulator.graph_shape().0,
            layers_len: accumulator.graph_shape().2,
            node_top_tail: accumulator.graph_node_top_tail(before_nodes),
            touched: accumulator.drain_graph_recording(),
        };
        let lexical = accumulator.drain_lexical_recording();
        assert!(
            !graph.touched.is_empty(),
            "the fixture's batch must touch the graph"
        );
        assert!(
            lexical.is_some(),
            "the fixture's batch must touch the lexical index"
        );

        // Written in pieces: a delta referencing the base by an id, carrying
        // only the recorded content, read back through the same chain a
        // merge checkpoint's own reload uses.
        let mut delta_bytes = Vec::new();
        write_segment_delta(
            &mut delta_bytes,
            Some(1),
            &[DeltaBatch {
                puts: puts.clone(),
                tombstoned: tombstoned.clone(),
            }],
            &graph,
            lexical.as_ref(),
            Some((
                accumulator.store().n_chunks() as u64,
                accumulator.store().n_documents() as u64,
            )),
        )
        .unwrap();
        let parsed = parse_segment_delta(&delta_bytes).unwrap();
        assert_eq!(parsed.base, Some(1));
        assert_eq!(parsed.batches.len(), 1);
        assert_eq!(parsed.batches[0].puts.len(), puts.len());
        assert_eq!(parsed.batches[0].tombstoned, tombstoned);
        let resolved_base = read_index(&mut base_bytes.as_slice()).unwrap();
        let pieces = apply_segment_delta(resolved_base, &parsed).expect("the delta applies");
        let mut pieces_bytes = Vec::new();
        write_index(&pieces, &mut pieces_bytes).unwrap();

        assert_eq!(
            pieces_bytes, whole_bytes,
            "a segment written in pieces must read back byte identical to the same segment written whole"
        );
    }

    /// A checkpoint written before a merge has folded every input carries no
    /// seal, and `parse_segment_delta` reports that plainly rather than
    /// guessing one - this is the fact `inillucent_search::merge::load_segment`
    /// refuses on, and `load_segment_resumable` accepts.
    #[test]
    fn a_delta_with_no_seal_reports_none() {
        let (puts, tombstoned) = small_batch();
        let mut bytes = Vec::new();
        write_segment_delta(
            &mut bytes,
            Some(1),
            &[DeltaBatch { puts, tombstoned }],
            &GraphRecording::default(),
            None,
            None,
        )
        .unwrap();
        let parsed = parse_segment_delta(&bytes).unwrap();
        assert_eq!(parsed.base, Some(1));
        assert!(
            parsed.sealed.is_none(),
            "an unfinished checkpoint must not report a seal"
        );
    }

    /// The final checkpoint of a merge carries a seal, and its counts survive
    /// the round trip - what a caller compares its replayed content against
    /// before trusting the chain as complete.
    #[test]
    fn a_sealed_delta_reports_its_seal_counts() {
        let mut bytes = Vec::new();
        write_segment_delta(
            &mut bytes,
            None,
            &[],
            &GraphRecording::default(),
            None,
            Some((42, 7)),
        )
        .unwrap();
        let parsed = parse_segment_delta(&bytes).unwrap();
        assert_eq!(parsed.base, None);
        assert_eq!(parsed.sealed, Some((42, 7)));
    }

    /// `is_segment_delta` tells the two stream forms apart from the header
    /// alone, before either is parsed - what lets a caller holding a `%_gen`
    /// row's bytes choose which reader to use.
    #[test]
    fn is_segment_delta_distinguishes_the_two_stream_forms() {
        let original = small_index();
        let mut stream_bytes = Vec::new();
        write_index(&original, &mut stream_bytes).unwrap();
        assert!(!is_segment_delta(&stream_bytes));

        let mut delta_bytes = Vec::new();
        write_segment_delta(
            &mut delta_bytes,
            Some(1),
            &[],
            &GraphRecording::default(),
            None,
            None,
        )
        .unwrap();
        assert!(is_segment_delta(&delta_bytes));
    }

    /// A delta whose payload is truncated mid part is refused rather than
    /// panicking or silently reading past the end - the same "no unwrap, no
    /// indexing on untrusted bytes" rule `read_section` already follows.
    #[test]
    fn a_truncated_delta_is_refused_not_panicked_on() {
        let (puts, tombstoned) = small_batch();
        let mut bytes = Vec::new();
        write_segment_delta(
            &mut bytes,
            Some(1),
            &[DeltaBatch { puts, tombstoned }],
            &GraphRecording::default(),
            None,
            Some((5, 5)),
        )
        .unwrap();
        bytes.truncate(bytes.len() - 5);
        let error = match parse_segment_delta(&bytes) {
            Ok(_) => panic!("a truncated delta must not parse"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("ended before it should have"),
            "{error:#}"
        );
    }
}
