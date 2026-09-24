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
//! **The verdict is taken on the distribution, and on each chunk re-embedded
//! alone, and never on the single worst sample in a batch.** Task-1967 is the
//! reason, and the numbers are all measured on `nomic-embed-text-v2-moe` over the
//! 18,685 chunk corpus-small cache. A bound on the minimum failed that cache at
//! 0.999409 on one chunk of 2,000 while the other 1,999 sat above 0.99998, and the
//! failure survived a rebuild from zero — so the error's own remedy, delete the
//! vectors and embed again, provably could not clear it. The cache was never
//! wrong: that chunk's stored vector agrees with the chunk re-embedded **on its
//! own** at 0.999998, and the nearest of the other 18,684 stored vectors is 0.881.
//!
//! What varied was this check's own re-embedding, and what varied it was the
//! server. `llama-server` selects a slot by LRU while any slot is still unused and
//! by longest common prefix afterwards, reusing that slot's cached keys and values
//! for the matching tokens instead of recomputing them. Every text here begins with
//! `search_document: `, so the similarities run 0.10 to 0.31 and the 0.10 threshold
//! accepts all of them. Measured on one cache, one binary and one set of flags: the
//! first check after the server starts reads 0.999735 and passes, every later check
//! against that same process reads 0.999409 and failed, and three checks against a
//! server started with `--slot-prompt-similarity 0` read 0.999738, 0.999734 and
//! 0.999738. Both phases are stable, which is why comparing two runs could not see
//! it — neither was the first after a restart.
//!
//! So a per-chunk verdict is only taken under the one condition that has no batch
//! composition in it, and the bar for it sits in the gap between the two
//! populations rather than between one artefact and the next.
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
use inillucent_core::store::ChunkInput;

/// Agreement below this is further out than re-running one model over one text
/// should land, so a sample under it is re-embedded on its own and reported.
///
/// It is **not** on its own a verdict. A served arm reaches 0.999409 on an honest
/// chunk whose stored vector is correct (see the module note), and a bar that one
/// sample can cross decides a binary answer on the noisiest thing in a run of
/// 2,000. What this bound decides is which chunks are worth a second look.
const AGREEMENT_FLOOR: f64 = 0.9995;

/// Agreement below this, **for a chunk re-embedded on its own**, means the vector
/// was made from different text.
///
/// The bar sits in the gap between two measured populations rather than beside
/// either of them. On corpus-small with `nomic-embed-text-v2-moe`, the worst honest
/// disagreement measured across four server configurations is 0.999409, and the
/// closest of the 18,684 *other* stored vectors to a chunk is 0.881 — so 0.99 is
/// about a hundred times clear of the noise and ten times clear of a real
/// mispairing. Anything that is genuinely another text's vector is far below it,
/// and nothing this repository has measured from floating point or from a server's
/// prompt cache comes near it.
const MISPAIRED_BELOW: f64 = 0.99;

/// The share of the sample that may sit below `AGREEMENT_FLOOR` before the cache is
/// refused, whatever each of those chunks scores on its own.
///
/// The per-chunk bound above catches a vector made from another text. This catches
/// the shape that bound would miss: a part of the corpus whose vectors have shifted
/// by a little rather than by everything — a re-embed that resumed from the wrong
/// offset by a few chunks, say, where neighbouring text is similar enough to stay
/// above 0.99. Measured honest rate on this corpus is 1 in 2,000 at worst and 0 in
/// 2,000 on a freshly started server, so one per cent is twenty times the worst
/// reading and still far under any systematic share.
const LOW_SHARE: f64 = 0.01;

/// Chunks re-embedded on their own to decide the per-chunk verdict.
///
/// One request each, so this is bounded rather than proportional. `LOW_SHARE`
/// already refuses a sample with more than one per cent below the floor, and the
/// worst few are what a reader needs to see either way.
const ALONE_SAMPLES: usize = 8;

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
        values.first().copied().unwrap_or(0.0),
        percentile(&values, 0.05),
        percentile(&values, 0.50),
        mean,
        values.last().copied().unwrap_or(0.0)
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
        if resolved.manifest_on_disk {
            ""
        } else {
            ", manifest assumed from the baseline constants"
        }
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
        .map(|&i| {
            let chunk = chunk_at(corpus, i)?;
            Ok(crate::synth::sanitize_for_model(&chunk.content))
        })
        .collect::<Result<Vec<String>>>()?;
    let fresh = embedder
        .embed_documents(&texts)
        .context("re-embedding the sample")?;

    let agreements = agreements_against(corpus, &chosen, &fresh)?;
    describe("cosine(stored, re-embedded)", agreements.clone());

    let facts = embedder.truncation();
    println!(
        "  the sample tokenized to {:.1} tokens per chunk, {} of {} past the {} token bound",
        facts.tokens_per_text(),
        facts.truncated,
        facts.texts,
        manifest.max_tokens
    );

    // Every chunk the batch put under the floor, re-embedded on its own. This is
    // the measurement the old check never took and the one the verdict turns on:
    // a request holding one text has no batch composition in it, so it separates
    // "this vector was made from different text" from "this check's batched
    // re-embedding is the noisier side". The bound below reads the alone column.
    let mut low = below(&agreements, &chosen, AGREEMENT_FLOOR);
    measure_alone(&embedder, corpus, &mut low)?;
    report_low(corpus, &low, chosen.len(), embedder.is_served());

    verdict(&agreements, &low, chosen.len(), &manifest.id)
}

/// One chunk of the loaded cache, by the index a sample drew.
///
/// **Fallible, because a sample index the corpus does not hold means the
/// sampler and the cache disagree about how many chunks were loaded.** The
/// check would otherwise compare a vector against the wrong text, which is the
/// exact failure it exists to find.
///
/// @param corpus - the loaded cache
/// @param at - the chunk index the sample drew
fn chunk_at(corpus: &Corpus, at: usize) -> Result<&ChunkInput> {
    corpus.chunks.get(at).with_context(|| {
        format!(
            "chunk {at} is past the {} the cache holds",
            corpus.chunks.len()
        )
    })
}

/// One chunk's stored vector, by the same index.
///
/// @param corpus - the loaded cache
/// @param at - the chunk index the sample drew
fn stored_vector(corpus: &Corpus, at: usize) -> Result<Vec<f32>> {
    corpus.vectors.get(at).cloned().with_context(|| {
        format!(
            "chunk {at} has no stored vector: the cache holds {}",
            corpus.vectors.len()
        )
    })
}

/// Cosine between each sampled chunk's stored vector and its re-embedding.
/// @param corpus - the loaded cache, for the stored vectors
/// @param chosen - the sampled chunk indices, in the order they were embedded
/// @param fresh - the re-embedded vectors, one per chosen index
fn agreements_against(corpus: &Corpus, chosen: &[usize], fresh: &[Vec<f32>]) -> Result<Vec<f64>> {
    let mut agreements = Vec::with_capacity(chosen.len());
    for (row, &i) in chosen.iter().enumerate() {
        let mut stored = stored_vector(corpus, i)?;
        inillucent_core::distance::normalize(&mut stored);
        let made = fresh.get(row).with_context(|| {
            format!(
                "the model returned {} vectors for {} sampled chunks",
                fresh.len(),
                chosen.len()
            )
        })?;
        anyhow::ensure!(
            made.len() == stored.len(),
            "the model produced {} dimensions and the cache holds {}",
            made.len(),
            stored.len()
        );
        agreements.push(dot(made, &stored) as f64);
    }
    Ok(agreements)
}

/// One sampled chunk that agreed with its stored vector less well than it should.
struct Low {
    /// Its index in the corpus.
    index: usize,
    /// What it scored inside the batch the check sent.
    in_sample: f64,
    /// What it scores re-embedded on its own, for the worst `ALONE_SAMPLES` of them.
    alone: Option<f64>,
}

/// The sampled chunks below a bound, worst first.
/// @param agreements - one cosine per sampled chunk, in sample order
/// @param chosen - the sampled chunk indices, in the same order
/// @param bound - the cosine below which a chunk is collected
fn below(agreements: &[f64], chosen: &[usize], bound: f64) -> Vec<Low> {
    let mut low: Vec<Low> = agreements
        .iter()
        .zip(chosen)
        .filter(|(a, _)| **a < bound)
        .map(|(a, i)| Low {
            index: *i,
            in_sample: *a,
            alone: None,
        })
        .collect();
    low.sort_by(|a, b| {
        a.in_sample
            .partial_cmp(&b.in_sample)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    low
}

/// Re-embed the worst of the low chunks one text to a request, and record it.
///
/// One request each rather than one request holding all of them, because a request
/// holding several is the thing under suspicion. Bounded by `ALONE_SAMPLES` so a
/// badly wrong cache does not turn the check into a second embedding run.
/// @param embedder - the arm the sample was re-embedded through
/// @param corpus - the loaded cache, for the chunk text
/// @param low - the chunks below the floor, worst first; filled in place
fn measure_alone(embedder: &Arm, corpus: &Corpus, low: &mut [Low]) -> Result<()> {
    for entry in low.iter_mut().take(ALONE_SAMPLES) {
        let text = crate::synth::sanitize_for_model(&chunk_at(corpus, entry.index)?.content);
        let fresh = embedder
            .embed_documents(&[text])
            .with_context(|| format!("re-embedding chunk {} on its own", entry.index))?;
        let mut stored = stored_vector(corpus, entry.index)?;
        inillucent_core::distance::normalize(&mut stored);
        let made = fresh.first();
        anyhow::ensure!(
            fresh.len() == 1 && made.map(Vec::len) == Some(stored.len()),
            "re-embedding chunk {} on its own produced {} vectors of {} dimensions",
            entry.index,
            fresh.len(),
            made.map(Vec::len).unwrap_or(0)
        );
        let made = made.context("the reply carried no vector at all")?;
        entry.alone = Some(dot(made, &stored) as f64);
    }
    Ok(())
}

/// Whether the chunks that fell below the floor still agree with their stored
/// vectors when each is embedded on its own.
///
/// It decides whether the paragraph explaining a served arm's prompt cache is
/// printed. On a cache whose vectors sit against the wrong text every alone reading
/// is near zero, and printing "the stored vector is correct and this check is the
/// noisier side" under those numbers would send a reader away from a real failure
/// with a plausible sentence in their hand.
/// @param low - the chunks below the floor, the worst of them measured alone
fn stored_vectors_look_right(low: &[Low]) -> bool {
    !low.iter()
        .any(|e| e.alone.is_some_and(|a| a < MISPAIRED_BELOW))
}

/// Print the chunks below the floor, with both numbers and what the pair means.
/// @param corpus - the loaded cache, for each chunk's source and length
/// @param low - the chunks below the floor, worst first, already measured alone
/// @param sampled - how many chunks were sampled, for the share
/// @param served - whether the arm's vectors came over a socket
fn report_low(corpus: &Corpus, low: &[Low], sampled: usize, served: bool) {
    if low.is_empty() {
        println!("  every sampled vector agrees with its text at {AGREEMENT_FLOOR} or better");
        return;
    }
    println!(
        "  {} of {sampled} sampled vectors agree less well than {AGREEMENT_FLOOR}, worst first:",
        low.len()
    );
    for entry in low.iter().take(ALONE_SAMPLES) {
        let alone = match entry.alone {
            Some(a) => format!("{a:.6} embedded alone"),
            None => "not re-embedded alone".to_string(),
        };
        // This whole function prints; a chunk the corpus does not hold is
        // reported as such rather than ending the report.
        let Some(chunk) = corpus.chunks.get(entry.index) else {
            println!(
                "    chunk {} is past the {} the corpus holds",
                entry.index,
                corpus.chunks.len()
            );
            continue;
        };
        println!(
            "    chunk {} ({}, {} chars): {:.6} in the sample, {alone}",
            entry.index,
            chunk.source,
            chunk.content.len(),
            entry.in_sample
        );
    }

    if !stored_vectors_look_right(low) {
        return;
    }
    println!(
        "  A chunk that agrees with its stored vector when embedded alone has a stored vector that\n  \
         is correct, and the disagreement is in this check's own batched re-embedding."
    );
    if served {
        println!(
            "  On a served arm that is expected: `llama-server` picks a slot by longest common\n  \
             prefix once every slot has held a prompt, and reuses that slot's cached keys and\n  \
             values for the matching tokens rather than recomputing them. Every text here shares\n  \
             the model's document prefix, so from the second pass against one server onward every\n  \
             request reuses something. Start the server with `--slot-prompt-similarity 0` to take\n  \
             it out of the measurement, or restart it before checking."
        );
    }
}

/// Decide whether the cache's text and its vectors were made from each other.
///
/// Three bounds, none of which one sample can carry. The median says whether the
/// sample as a whole reproduces, which is what a rebuilt corpus paired with old
/// vectors fails. The share says whether a systematic part of it has shifted, which
/// is what a resume from the wrong offset fails. The per-chunk bound reads the
/// alone column and nothing else, because that is the one measurement with no batch
/// composition in it.
///
/// Each failure names what it measured and a remedy that can change it. The bound
/// this replaced named one that could not: it failed a cache on its worst sample
/// and told the operator to embed again, and embedding again reproduced the same
/// number to six figures because the cache was never the thing that varied.
/// @param agreements - one cosine per sampled chunk
/// @param low - the chunks below the floor, worst first, measured alone
/// @param sampled - how many chunks were sampled
/// @param model_id - the model that should reproduce these vectors
fn verdict(agreements: &[f64], low: &[Low], sampled: usize, model_id: &str) -> Result<()> {
    let mut sorted = agreements.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = percentile(&sorted, 0.50);
    anyhow::ensure!(
        median >= AGREEMENT_FLOOR,
        "the median sampled vector agrees with its text at {median:.6}, below {AGREEMENT_FLOOR}. \
         Re-running one model over one text reproduces its vector to within the last bits of the \
         floating point accumulation, so a whole sample this far out is not noise: the cache's \
         text and its vectors were not made from each other by {model_id}. Rebuild the corpus and \
         embed it again, or delete the vectors file and rerun synth-embed"
    );

    let share = low.len() as f64 / sampled.max(1) as f64;
    anyhow::ensure!(
        share <= LOW_SHARE,
        "{} of {sampled} sampled vectors ({:.2}%) agree with their text less well than \
         {AGREEMENT_FLOOR}, against a bound of {:.2}%. One chunk there is the noise of a served \
         arm; a share this size is a part of the corpus whose vectors have shifted off their \
         text. Delete the vectors file and rerun synth-embed, which rebuilds every vector rather \
         than resuming",
        low.len(),
        100.0 * share,
        100.0 * LOW_SHARE
    );

    let mispaired: Vec<&Low> = low
        .iter()
        .filter(|e| e.alone.is_some_and(|a| a < MISPAIRED_BELOW))
        .collect();
    if let Some(worst) = mispaired.first() {
        anyhow::bail!(
            "{} of {sampled} sampled vectors disagree with their text even when the chunk is \
             re-embedded on its own, worst chunk {} at {:.6}, below {MISPAIRED_BELOW}. A request \
             holding one text has no batch composition in it, so this is not the noise of a \
             served arm: these vectors were made from different text by {model_id}. Rebuild the \
             corpus and embed it again, or delete the vectors file and rerun synth-embed",
            mispaired.len(),
            worst.index,
            worst.alone.unwrap_or(0.0)
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
        let dir = std::env::temp_dir().join(format!(
            "inillucent-embedcheck-{}-{name}",
            std::process::id()
        ));
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
            runnable: true,
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
        Corpus {
            chunks,
            vectors,
            dims: 8,
            header,
        }
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
        assert!(
            err.contains("the cache says it was embedded with model-a"),
            "{err}"
        );
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

    /// Build a sample where every chunk reproduces cleanly, so a test can put one
    /// reading into it and know that reading is the only thing being judged.
    /// @param n - how many chunks the sample holds
    fn clean_sample(n: usize) -> Vec<f64> {
        vec![0.999998; n]
    }

    fn low_at(index: usize, in_sample: f64, alone: f64) -> Low {
        Low {
            index,
            in_sample,
            alone: Some(alone),
        }
    }

    /// The reading task-1967 was filed for, asserted as a pass.
    ///
    /// The numbers are the measured ones: `nomic-embed-text-v2-moe` over
    /// corpus-small, chunk 11574 at 0.999409 inside the check's batch and 0.999998
    /// when the same chunk is re-embedded on its own, with the other 1,999 samples
    /// above 0.99998. The cache is right — the nearest of the 18,684 other stored
    /// vectors to that chunk is 0.881 — and the old bound on the minimum failed it
    /// anyway, then told the operator to embed again. Embedding again reproduced
    /// 0.999409 to six figures, because the cache was never what varied.
    ///
    /// This discriminates: with `verdict` replaced by the bound it removed —
    /// refuse when any sample is under `AGREEMENT_FLOOR` — it fails.
    #[test]
    fn one_noisy_sample_does_not_fail_a_cache_whose_vector_is_right_when_embedded_alone() {
        let mut agreements = clean_sample(2000);
        agreements[1286] = 0.999409;
        let low = vec![low_at(11574, 0.999409, 0.999998)];
        assert!(
            verdict(&agreements, &low, 2000, "nomic-embed-text-v2-moe").is_ok(),
            "0.999409 in a batch and 0.999998 alone is a correct stored vector"
        );
    }

    /// The failure the check exists for: the corpus text was rebuilt and the
    /// embedding was not re-run, so every vector sits against another chunk's text.
    ///
    /// The median is what catches it, and the median is the one statistic a single
    /// sample cannot move. 0.640265 is the measured reading from running the real
    /// command over a real cache whose vector file was shifted by one chunk.
    #[test]
    fn a_cache_whose_text_and_vectors_are_not_each_others_is_refused_by_the_median() {
        let agreements = vec![0.640265; 2000];
        let low: Vec<Low> = (0..2000).map(|i| low_at(i, 0.640265, 0.640265)).collect();
        let err = verdict(&agreements, &low, 2000, "nomic-embed-text-v2-moe")
            .unwrap_err()
            .to_string();
        assert!(err.contains("the median sampled vector"), "{err}");
        assert!(err.contains("0.640265"), "{err}");
        assert!(err.contains("rerun synth-embed"), "{err}");
    }

    /// One vector made from different text, among 1,999 that are right.
    ///
    /// Neither of the other two bounds can see this: the median is 0.999998 and one
    /// chunk in 2,000 is 0.05 per cent against a 1 per cent share. Only the alone
    /// column decides it, and it decides it under the one condition that has no
    /// batch composition in it — which is what makes refusing here different from
    /// refusing on a batch's worst sample.
    #[test]
    fn a_vector_that_disagrees_with_its_text_embedded_alone_is_refused_on_its_own() {
        let mut agreements = clean_sample(2000);
        agreements[7] = 0.0021;
        let low = vec![low_at(7641, 0.0021, 0.002181)];
        let err = verdict(&agreements, &low, 2000, "nomic-embed-text-v2-moe")
            .unwrap_err()
            .to_string();
        assert!(err.contains("re-embedded on its own"), "{err}");
        assert!(err.contains("chunk 7641"), "{err}");
        assert!(err.contains("0.002181"), "{err}");
    }

    /// A part of the corpus whose vectors have shifted a little rather than wholly,
    /// which is what an embedding run resumed from the wrong offset produces when
    /// the neighbouring text is similar.
    ///
    /// Every one of these is above `MISPAIRED_BELOW` and would pass the per-chunk
    /// bound, and the median is 0.999998 and passes too. The share is the only
    /// thing that sees it, which is why the check has all three.
    #[test]
    fn a_systematic_share_below_the_floor_is_refused_although_each_one_is_fine_alone() {
        let mut agreements = clean_sample(2000);
        let low: Vec<Low> = (0..100)
            .map(|i| {
                agreements[i] = 0.9991;
                low_at(i, 0.9991, 0.9991)
            })
            .collect();
        let err = verdict(&agreements, &low, 2000, "nomic-embed-text-v2-moe")
            .unwrap_err()
            .to_string();
        assert!(err.contains("100 of 2000"), "{err}");
        assert!(err.contains("5.00%"), "{err}");
        assert!(err.contains("1.00%"), "{err}");
    }

    /// The bars sit in the gap between two measured populations, not beside either.
    ///
    /// Both numbers are measurements on corpus-small with
    /// `nomic-embed-text-v2-moe`: 0.999409 is the worst honest disagreement seen
    /// across four server configurations, and 0.881375 is the closest of the 18,684
    /// *other* stored vectors to a chunk, which is the best a genuinely mispaired
    /// vector manages. A future change that moves `MISPAIRED_BELOW` into either
    /// population fails here rather than in somebody's cache.
    #[test]
    fn the_mispairing_bar_sits_between_the_noise_and_a_real_mispairing() {
        // Measurements rather than settings, so they are bindings: the bar
        // is the constant and these are what it was placed between.
        let worst_honest_reading: f64 = 0.999409;
        let best_mispaired_reading: f64 = 0.881375;
        // A relation between two of this module's own constants, so it holds
        // when the crate compiles rather than when this test runs.
        const {
            assert!(MISPAIRED_BELOW < AGREEMENT_FLOOR);
        }
        assert!(
            best_mispaired_reading < MISPAIRED_BELOW,
            "a vector made from the nearest other chunk's text reads {best_mispaired_reading} \
             and has to fall below {MISPAIRED_BELOW}"
        );
        assert!(
            worst_honest_reading > MISPAIRED_BELOW,
            "the worst honest reading measured is {worst_honest_reading} and has to stay above \
             {MISPAIRED_BELOW}"
        );
    }

    /// The chunks below the floor come back worst first, and only the ones below it.
    #[test]
    fn the_low_chunks_are_collected_worst_first() {
        let agreements = vec![1.0, 0.9, 0.99999, 0.5, 0.9994];
        let chosen = vec![10, 20, 30, 40, 50];
        let low = below(&agreements, &chosen, AGREEMENT_FLOOR);
        assert_eq!(
            low.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![40, 20, 50]
        );
        assert_eq!(low[0].in_sample, 0.5);
        assert!(low.iter().all(|e| e.alone.is_none()));
    }

    /// The paragraph that says the cache is fine and this check is the noisier side
    /// is printed only when the alone column says so.
    ///
    /// Under a misaligned cache that sentence is true of nothing and reads as a
    /// reason to ignore the error directly beneath it.
    #[test]
    fn the_explanation_for_a_served_arm_is_withheld_when_a_vector_is_really_mispaired() {
        assert!(stored_vectors_look_right(&[low_at(
            11574, 0.999409, 0.999998
        )]));
        assert!(!stored_vectors_look_right(&[
            low_at(11574, 0.999409, 0.999998),
            low_at(7641, 0.0021, 0.002181),
        ]));
        assert!(
            stored_vectors_look_right(&[Low {
                index: 3,
                in_sample: 0.9,
                alone: None,
            }]),
            "a chunk past the alone sampling bound says nothing either way"
        );
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
