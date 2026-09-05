//! Do the vectors in the cache still belong to the text in the cache?
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
//! It also checks the two structural properties every scenario assumes: that each
//! vector has the corpus width, and that it is a unit vector, since cosine
//! distance is computed as a dot product and a vector that is not normalised would
//! silently score too high.

use anyhow::{Context, Result};
use inillucent_core::distance::dot;
use inillucent_core::embed::Embedder;
use inillucent_core::embed_onnx::{Device, OnnxEmbedder, OnnxOptions};

use crate::corpus::Corpus;
use crate::metrics::percentile;

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

pub fn run(
    corpus: &Corpus,
    model_dir: &str,
    model_file: &str,
    samples: usize,
    batch_size: usize,
    device: Device,
) -> Result<()> {
    println!("model: {model_dir}/{model_file}");
    println!("cache: {} chunks at {} dimensions", corpus.len(), corpus.dims);

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
    println!("  vectors of the wrong width: {wrong_width}");
    describe("vector length", norms.clone());
    let unnormalised = norms.iter().filter(|n| (**n - 1.0).abs() > 1e-3).count();
    println!("  vectors that are not unit length: {unnormalised}");

    // Agreement, on a sample spread across the whole corpus rather than a prefix,
    // because chunk order correlates with source and a prefix is one source.
    let stride = (corpus.len() / samples.max(1)).max(1);
    let chosen: Vec<usize> = (0..corpus.len()).step_by(stride).take(samples).collect();
    anyhow::ensure!(!chosen.is_empty(), "the cache holds no chunks");

    let embedder = OnnxEmbedder::open_model(
        model_dir,
        model_file,
        OnnxOptions { batch_size, device, ..Default::default() },
    )
    .context("loading the ONNX model. Is ORT_DYLIB_PATH set?")?;

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
        let agreement = dot(&fresh[row], &stored) as f64;
        agreements.push(agreement);
        if agreement < AGREEMENT_FLOOR {
            mismatched.push((i, agreement));
        }
    }
    describe("cosine(stored, re-embedded)", agreements.clone());

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
            "the cache pairs text with vectors that were not made from it. Rerun synth-embed \
             after deleting the vectors file, or rebuild the corpus and embed it again"
        );
    }

    Ok(())
}
