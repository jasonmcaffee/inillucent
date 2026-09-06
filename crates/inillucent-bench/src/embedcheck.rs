//! Do the vectors in the cache still belong to the text in the cache, and were
//! they made by the model the cache says made them?
//!
//! The corpus is built in two steps that can drift apart. The text is assembled
//! first and the embedding run takes hours, appending vectors to a side file as it
//! goes. Rebuild the text without rerunning the embedding and every vector is then
//! paired with the wrong chunk. Nothing would fail: the index would build, the
//! queries would return rows, and every retrieval score would be quietly wrong
//! because the vectors describe other documents.
//!
//! So this re-embeds a sample of the chunks the cache holds and checks each
//! vector against the one stored beside it. Reproducing the same text through the
//! same model has to give the same vector, so the expectation here is agreement to
//! within floating point noise, not mere similarity.
//!
//! Since the harness compares models against each other, "the same model" stopped
//! being a fact about the machine and became a fact about the cache. A version 4
//! cache names the model and the manifest that produced it, and this resolves that
//! model rather than whichever one the command line defaulted to. Pointing it at
//! another installed model is not a way to check a cache: it is a way to be told,
//! by name, that the two do not match.
//!
//! It also checks the two structural properties every scenario assumes: that each
//! vector has the corpus width, and that it is a unit vector, since cosine
//! distance is computed as a dot product and a vector that is not normalised would
//! silently score too high.

use std::path::Path;

use anyhow::{Context, Result};
use inillucent_core::distance::dot;

use crate::arm::{Arm, ArmOptions};
use crate::corpus::{short, Corpus};
use crate::metrics::percentile;
use crate::models::{self, ResolvedModel};

/// Agreement below this is treated as a mismatch rather than as noise. Re-running
/// one model over one text is deterministic apart from the last bits of the
/// floating point accumulation, which lands far above this.
const AGREEMENT_FLOOR: f64 = 0.9995;

fn describe(label: &str, mut values: Vec<f64>) {
    if values.is_empty() {
        println!("  {label}: no samples");
        return;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    println!(
        "  {label}: n={} min={:.6} p5={:.6} p50={:.6} mean={:.6} max={:.6}",
        values.len(),
        values[0],
        percentile(&values, 0.05),
        percentile(&values, 0.50),
        mean,
        values[values.len() - 1]
    );
}

/// Resolve the model a cache was embedded with.
///
/// A version 4 cache names it, and the name wins: that is the whole point of the
/// header. A caller may still pass a directory, and then the two have to agree —
/// a directory that holds a different model than the header names is refused
/// rather than silently used, because "check this cache against some other model"
/// is not a question with a useful answer.
/// @param corpus - the loaded cache, for its header
/// @param models_root - where models live, one directory per id
/// @param model_dir - an explicit directory, for a legacy cache that names nothing
/// @param model_file - the weights file to assume for the unmanifested baseline
fn model_for(
    corpus: &Corpus,
    models_root: &Path,
    model_dir: Option<&str>,
    model_file: &str,
) -> Result<ResolvedModel> {
    let named = if corpus.header.has_provenance() {
        Some(models::resolve_id(models_root, &corpus.header.model_id)?)
    } else {
        None
    };

    let given = match model_dir {
        Some(dir) => Some(models::resolve_dir(Path::new(dir), model_file)?),
        None => None,
    };

    match (named, given) {
        (Some(named), Some(given)) => {
            anyhow::ensure!(
                named.manifest.id == given.manifest.id,
                "the cache says it was embedded with {}, and {} holds {}. Re-embedding a \
                 cache's text through a different model does not check the cache; it produces \
                 the vectors of a model the cache never saw",
                named.manifest.id,
                given.dir.display(),
                given.manifest.id
            );
            Ok(given)
        }
        (Some(named), None) => Ok(named),
        (None, Some(given)) => Ok(given),
        (None, None) => anyhow::bail!(
            "this cache carries no provenance and no --model-dir was given, so there is nothing \
             that says which model should reproduce its vectors"
        ),
    }
}

/// @param corpus - the loaded cache
/// @param models_root - where models live, one directory per id
/// @param model_dir - an explicit model directory, for a cache with no header
/// @param model_file - weights file assumed for the unmanifested baseline
/// @param samples - chunks re-embedded, spread across the whole corpus
/// @param options - where and how hard to run the arm, including the llama.cpp
///   endpoint for a served model
pub fn run(
    corpus: &Corpus,
    models_root: &Path,
    model_dir: Option<&str>,
    model_file: &str,
    samples: usize,
    options: &ArmOptions,
) -> Result<()> {
    println!("cache: {}", corpus.header.describe());
    let resolved = model_for(corpus, models_root, model_dir, model_file)?;
    let manifest = &resolved.manifest;
    println!(
        "model: {}/{} ({}{})",
        resolved.dir.display(),
        manifest.model_file,
        manifest.id,
        if resolved.manifest_on_disk { "" } else { ", manifest assumed from the baseline constants" }
    );

    // The manifest the cache was written against, against the manifest on disk
    // now. A prefix or a token bound that moved since the run is invisible in the
    // weights and changes every vector.
    if corpus.header.has_provenance() {
        let now = resolved.digest();
        anyhow::ensure!(
            now == corpus.header.manifest_sha256,
            "{}'s manifest now digests to {}, and the cache was embedded against {}. Something \
             in the model's contract - a prefix, the pooling, the token bound, the width - has \
             changed since these vectors were made, so re-embedding cannot reproduce them",
            manifest.id,
            short(&now),
            short(&corpus.header.manifest_sha256)
        );
        anyhow::ensure!(
            corpus.header.dims == manifest.dims,
            "the cache holds {} dimensional vectors and {}'s manifest declares {}",
            corpus.header.dims,
            manifest.id,
            manifest.dims
        );
    }
    resolved.verify_files()?;

    // Structural checks over every vector, which cost nothing next to embedding.
    let mut wrong_width = 0usize;
    let mut norms = Vec::with_capacity(corpus.len());
    for v in &corpus.vectors {
        if v.len() != corpus.dims {
            wrong_width += 1;
            continue;
        }
        norms.push(dot(v, v).sqrt() as f64);
    }
    println!("\nstructure");
    println!("  {} chunks at {} dimensions", corpus.len(), corpus.dims);
    println!("  vectors of the wrong width: {wrong_width}");
    describe("vector length", norms.clone());
    let unnormalised = norms.iter().filter(|n| (**n - 1.0).abs() > 1e-3).count();
    println!("  vectors that are not unit length: {unnormalised}");

    // Agreement, on a sample spread across the whole corpus rather than a prefix,
    // because chunk order correlates with source and a prefix is one source.
    let stride = (corpus.len() / samples.max(1)).max(1);
    let chosen: Vec<usize> = (0..corpus.len()).step_by(stride).take(samples).collect();
    anyhow::ensure!(!chosen.is_empty(), "the cache holds no chunks");

    // Through the arm rather than through the ONNX embedder directly, because
    // `nomic-embed-text-v2-moe` has no ONNX anywhere and is served by
    // `llama-server`. A check that could only verify the models it happened to be
    // written for would leave the one arm whose vectors travel over a socket -
    // the arm with the most ways to be wrong - as the only one nobody checked.
    let embedder = Arm::open(&resolved, options)
        .context("opening the model to re-embed the sample. Is ORT_DYLIB_PATH set?")?;

    println!(
        "\nagreement between the stored vectors and the model, on {} chunks spread across the corpus",
        chosen.len()
    );
    let texts: Vec<String> = chosen
        .iter()
        .map(|&i| crate::synth::sanitize_for_model(&corpus.chunks[i].content))
        .collect();
    let fresh = embedder.embed_documents(&texts).context("re-embedding the sample")?;

    let mut agreements = Vec::with_capacity(chosen.len());
    let mut mismatched = Vec::new();
    for (row, &i) in chosen.iter().enumerate() {
        let mut stored = corpus.vectors[i].clone();
        inillucent_core::distance::normalize(&mut stored);
        anyhow::ensure!(
            fresh[row].len() == stored.len(),
            "the model produced {} dimensions and the cache holds {}",
            fresh[row].len(),
            stored.len()
        );
        let agreement = dot(&fresh[row], &stored) as f64;
        agreements.push(agreement);
        if agreement < AGREEMENT_FLOOR {
            mismatched.push((i, agreement));
        }
    }
    describe("cosine(stored, re-embedded)", agreements.clone());

    let facts = embedder.truncation();
    println!(
        "  the sample tokenized to {:.1} tokens per chunk, {} of {} past the {} token bound",
        facts.tokens_per_text(),
        facts.truncated,
        facts.texts,
        manifest.max_tokens
    );

    if mismatched.is_empty() {
        println!("  every sampled vector matches its text");
    } else {
        println!(
            "  {} of {} sampled vectors do not match their text, worst first:",
            mismatched.len(),
            chosen.len()
        );
        mismatched.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        for (i, agreement) in mismatched.iter().take(5) {
            println!(
                "    chunk {i} ({}, {} chars): cosine {agreement:.4}",
                corpus.chunks[*i].source,
                corpus.chunks[*i].content.len()
            );
        }
        anyhow::bail!(
            "the cache pairs text with vectors that were not made from it by {}. Rerun \
             synth-embed after deleting the vectors file, or rebuild the corpus and embed it \
             again",
            manifest.id
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::CacheHeader;
    use inillucent_core::model::{ModelManifest, Pooling, Prefixes};
    use inillucent_core::store::ChunkInput;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("inillucent-embedcheck-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Two installed models, so a header naming one can be pointed at the other.
    fn install(root: &Path, id: &str) -> ModelManifest {
        let dir = root.join("models").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = ModelManifest {
            id: id.to_string(),
            dims: 8,
            mrl_widths: vec![8],
            prefixes: Prefixes::none(),
            pooling: Pooling::Mean,
            max_tokens: 512,
            layer_norm: false,
            model_file: "model.onnx".into(),
            token_type_ids: false,
            backend: inillucent_core::model::Backend::Onnx,
            output: inillucent_core::model::Output::TokenEmbeddings,
            output_name: String::new(),
            tokenizer_sha256: String::new(),
            weights_sha256: String::new(),
            recipe_git_sha: None,
            source: None,
        };
        std::fs::write(
            dir.join(crate::models::MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        manifest
    }

    fn corpus_with(header: CacheHeader) -> Corpus {
        let chunks: Vec<ChunkInput> = (0..4)
            .map(|i| ChunkInput {
                source: "confluence".into(),
                external_doc_id: format!("d{i}"),
                chunk_index: 0,
                heading_path: vec![],
                content: format!("chunk {i}"),
                title: "t".into(),
                url: "u".into(),
                space_key: None,
                author: None,
                author_id: None,
                updated_at: None,
                external_chunk_id: None,
                labels: vec![],
                attributes: Vec::new(),
                flags: Vec::new(),
                deleted: false,
            })
            .collect();
        let vectors = (0..4)
            .map(|_| {
                let mut v = vec![1.0f32; 8];
                inillucent_core::distance::normalize(&mut v);
                v
            })
            .collect();
        Corpus { chunks, vectors, dims: 8, header }
    }

    fn header_naming(model: &ModelManifest) -> CacheHeader {
        CacheHeader {
            version: 4,
            corpus_sha256: "corpus".into(),
            model_id: model.id.clone(),
            manifest_sha256: crate::corpus::manifest_digest(model),
            dims: model.dims,
            max_tokens: model.max_tokens,
            chunk_count: 4,
            truncated_chunks: 0,
            query_seed_digest: "seeds".into(),
        }
    }

    /// The check the whole header exists for: a cache says which model made it,
    /// and re-embedding its text through a different installed model is refused
    /// rather than reported as a very low agreement.
    ///
    /// Reported as a mismatch would be worse than useless. The numbers would be
    /// real cosines between real vectors, so the output would look like a
    /// measurement of drift when it is actually a measurement of two models.
    #[test]
    fn pointing_the_check_at_a_different_installed_model_is_refused_by_name() {
        let root = scratch("swapped");
        let mine = install(&root, "model-a");
        install(&root, "model-b");
        let corpus = corpus_with(header_naming(&mine));
        let other = root.join("models").join("model-b");
        let err = model_for(
            &corpus,
            &root.join("models"),
            Some(&other.display().to_string()),
            "model.onnx",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("the cache says it was embedded with model-a"), "{err}");
        assert!(err.contains("model-b"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_cache_that_names_its_model_needs_no_directory_on_the_command_line() {
        let root = scratch("named");
        let mine = install(&root, "model-a");
        let corpus = corpus_with(header_naming(&mine));
        let resolved = model_for(&corpus, &root.join("models"), None, "model.onnx").unwrap();
        assert_eq!(resolved.manifest.id, "model-a");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_legacy_cache_with_no_directory_has_nothing_to_check_against() {
        let root = scratch("legacy");
        let corpus = corpus_with(CacheHeader {
            version: 3,
            corpus_sha256: String::new(),
            model_id: String::new(),
            manifest_sha256: String::new(),
            dims: 8,
            max_tokens: 0,
            chunk_count: 4,
            truncated_chunks: 0,
            query_seed_digest: String::new(),
        });
        let err = model_for(&corpus, &root.join("models"), None, "model.onnx")
            .unwrap_err()
            .to_string();
        assert!(err.contains("carries no provenance"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A cache naming a model that is not installed says so, rather than falling
    /// back to whichever model the command line happened to default to.
    #[test]
    fn a_cache_naming_a_model_that_is_not_installed_is_refused_by_name() {
        let root = scratch("absent");
        let mine = install(&root, "model-a");
        std::fs::remove_dir_all(root.join("models").join("model-a")).unwrap();
        let corpus = corpus_with(header_naming(&mine));
        let err = model_for(&corpus, &root.join("models"), None, "model.onnx")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no model directory model-a"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }
}
