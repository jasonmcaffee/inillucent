//! Saving and loading an index.
//!
//! An index is a directory of files. The store, the graph and the lexical index
//! are serialized with a version stamped header each, so a file written by a
//! different layout is refused rather than misread. Vectors are written as a raw
//! little endian f32 array, which is what makes loading them a read rather than a
//! parse.
//!
//! There is no daemon, no port and no background process. Opening an index is
//! opening files.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
pub const FORMAT_VERSION: u32 = 2;
const MAGIC: &[u8; 8] = b"RUSTDBIX";

fn header(w: &mut impl Write, kind: u8) -> Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&[kind])?;
    Ok(())
}

fn check_header(r: &mut impl Read, kind: u8) -> Result<()> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).context("reading the file header")?;
    if &magic != MAGIC {
        anyhow::bail!("not a rust-db index file");
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

const KIND_STORE: u8 = 1;
const KIND_VECTORS: u8 = 2;
const KIND_GRAPH: u8 = 3;
const KIND_CONFIG: u8 = 4;

fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

/// What the index needs in order to be rebuilt from disk. The lexical index and
/// the int8 codes are derived rather than stored: both are a deterministic
/// function of data that is already here, and recomputing them costs less than
/// the disk they would occupy.
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
    // Everything below this line is what version 1 lost.
    fusion: SavedFusion,
    lexical_coverage: f32,
    lexical_proximity: f32,
    lexical_tier: bool,
    lexical_phrase: f32,
    lexical_rescore_depth: usize,
    adaptive_fusion: bool,
    adaptive: SavedAdaptive,
    mmr_lambda: f32,
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

pub fn save(index: &Index, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).context("creating the index directory")?;

    {
        let mut w = BufWriter::new(File::create(path(dir, "store.bin"))?);
        header(&mut w, KIND_STORE)?;
        serde_json::to_writer(&mut w, index.store()).context("writing the store")?;
        w.flush()?;
    }
    {
        let mut w = BufWriter::new(File::create(path(dir, "vectors.bin"))?);
        header(&mut w, KIND_VECTORS)?;
        let raw = index.vectors().raw();
        w.write_all(&(index.vectors().dims() as u32).to_le_bytes())?;
        w.write_all(&(index.vectors().len() as u32).to_le_bytes())?;
        // One write of the whole buffer; f32 little endian is the on disk form.
        let bytes: &[u8] = bytemuck::cast_slice(raw);
        w.write_all(bytes)?;
        w.flush()?;
    }
    {
        let cfg = index.config();
        let saved = SavedConfig {
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
            fusion: cfg.fusion.into(),
            lexical_coverage: cfg.lexical_coverage,
            lexical_proximity: cfg.lexical_proximity,
            lexical_tier: cfg.lexical_tier,
            lexical_phrase: cfg.lexical_phrase,
            lexical_rescore_depth: cfg.lexical_rescore_depth,
            adaptive_fusion: cfg.adaptive_fusion,
            adaptive: cfg.adaptive.into(),
            mmr_lambda: cfg.mmr_lambda,
        };
        let mut w = BufWriter::new(File::create(path(dir, "config.bin"))?);
        header(&mut w, KIND_CONFIG)?;
        serde_json::to_writer(&mut w, &saved)?;
        w.flush()?;
    }
    {
        // The graph is written as its adjacency lists. Rebuilding it instead would
        // cost minutes on this corpus, which is the one structure worth storing.
        let mut w = BufWriter::new(File::create(path(dir, "graph.bin"))?);
        header(&mut w, KIND_GRAPH)?;
        index.write_graph(&mut w)?;
        w.flush()?;
    }
    Ok(())
}

pub fn load(dir: &Path) -> Result<Index> {
    let saved: SavedConfig = {
        let mut r = BufReader::new(File::open(path(dir, "config.bin"))?);
        check_header(&mut r, KIND_CONFIG)?;
        serde_json::from_reader(r).context("reading the config")?
    };
    let store: Store = {
        let mut r = BufReader::new(File::open(path(dir, "store.bin"))?);
        check_header(&mut r, KIND_STORE)?;
        serde_json::from_reader(r).context("reading the store")?
    };
    let vectors: VectorSet = {
        let mut r = BufReader::new(File::open(path(dir, "vectors.bin"))?);
        check_header(&mut r, KIND_VECTORS)?;
        let mut buf4 = [0u8; 4];
        r.read_exact(&mut buf4)?;
        let dims = u32::from_le_bytes(buf4) as usize;
        r.read_exact(&mut buf4)?;
        let n = u32::from_le_bytes(buf4) as usize;
        let mut bytes = vec![0u8; dims * n * 4];
        r.read_exact(&mut bytes)?;
        let floats: &[f32] = bytemuck::cast_slice(&bytes);
        VectorSet::from_raw(dims, floats.to_vec())
    };
    let graph: Hnsw = {
        let mut r = BufReader::new(File::open(path(dir, "graph.bin"))?);
        check_header(&mut r, KIND_GRAPH)?;
        Hnsw::read_graph(
            &mut r,
            HnswParams {
                m: saved.hnsw_m,
                ef_construction: saved.hnsw_ef_construction,
                ef_search: saved.hnsw_ef_search,
                seed: saved.hnsw_seed,
                exhaustive_below: saved.hnsw_exhaustive_below,
            },
        )?
    };

    let config = IndexConfig {
        dims: saved.dims,
        quantized: saved.quantized,
        oversample: saved.oversample,
        candidates: saved.candidates,
        per_doc_cap: saved.per_doc_cap,
        lexical_prefix: saved.lexical_prefix,
        hnsw: HnswParams {
            m: saved.hnsw_m,
            ef_construction: saved.hnsw_ef_construction,
            ef_search: saved.hnsw_ef_search,
            seed: saved.hnsw_seed,
            exhaustive_below: saved.hnsw_exhaustive_below,
        },
        fusion: saved.fusion.to_fusion()?,
        lexical_coverage: saved.lexical_coverage,
        lexical_proximity: saved.lexical_proximity,
        lexical_tier: saved.lexical_tier,
        lexical_phrase: saved.lexical_phrase,
        lexical_rescore_depth: saved.lexical_rescore_depth,
        adaptive_fusion: saved.adaptive_fusion,
        adaptive: (&saved.adaptive).into(),
        mmr_lambda: saved.mmr_lambda,
    };

    Index::from_parts(config, store, vectors, graph)
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
                title: format!("title {i}"),
                url: format!("https://x/{i}"),
                space_key: Some("ENG".into()),
                author: Some("Ada".into()),
                author_id: Some("u1".into()),
                updated_at: Some(1000 + i as i64),
                labels: vec!["design".into()],
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
        p.push(format!("rustdb-persist-test-{name}-{}", std::process::id()));
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

        let query = original.vectors().get(11).to_vec();
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

        // Corrupt the version stamp in the config header.
        let p = path(&dir, "config.bin");
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

        let query = original.vectors().get(11).to_vec();
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

}
