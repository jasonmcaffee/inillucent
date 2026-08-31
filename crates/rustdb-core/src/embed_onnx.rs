//! `nomic-embed-text-v1.5` run as ONNX inside the calling process.
//!
//! This is the replacement for the `llama-server` child process the baseline
//! talks to over HTTP. Same model, no separate process, no network hop.
//!
//! The model file exports token vectors, not one vector per text: its single
//! output `last_hidden_state` has shape `[batch, sequence, 768]`. Turning that
//! into one embedding is this module's job, and the order of those steps is what
//! decides whether the result matches what the baseline produced.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::Value;
use tokenizers::Tokenizer;

use crate::distance::normalize;
use crate::embed::{document_prefix, query_prefix, Embedder};

/// Which processor runs the model.
///
/// The default is the processor, which is what the engine shipped with and what
/// a machine with no CUDA install can do. `Cuda` names a specific card, so a box
/// with two of them can run one embedder per card and halve the wall clock of a
/// corpus embedding run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// ONNX Runtime's default CPU execution provider.
    Cpu,
    /// The CUDA execution provider, pinned to one card by its ordinal.
    Cuda(i32),
}

impl Device {
    /// Parses `cpu`, `cuda` (card 0) or `cuda:N`.
    /// @param text - the device name as written on a command line
    pub fn parse(text: &str) -> Result<Device> {
        let text = text.trim().to_ascii_lowercase();
        if text == "cpu" {
            return Ok(Device::Cpu);
        }
        if text == "cuda" {
            return Ok(Device::Cuda(0));
        }
        if let Some(rest) = text.strip_prefix("cuda:") {
            let id: i32 = rest.parse().with_context(|| format!("{rest} is not a card ordinal"))?;
            return Ok(Device::Cuda(id));
        }
        anyhow::bail!("unknown device {text}, expected cpu, cuda or cuda:N")
    }

    /// A short label for progress output.
    pub fn label(&self) -> String {
        match self {
            Device::Cpu => "cpu".to_string(),
            Device::Cuda(id) => format!("cuda:{id}"),
        }
    }
}

/// How to turn token vectors into one embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Average the token vectors, weighted by the attention mask so padding
    /// contributes nothing.
    Mean,
}

#[derive(Debug, Clone)]
pub struct OnnxOptions {
    /// Width to keep. 768 is the full output; 512, 256, 128 and 64 are the
    /// Matryoshka widths the model was trained to support.
    pub dims: usize,
    /// Apply layer normalisation to the pooled vector before truncating.
    ///
    /// The model card documents this step for Matryoshka use, and
    /// `sentence-transformers` performs it. llama.cpp does not, so the vectors
    /// already in PostgreSQL were produced without it. Which setting reproduces
    /// those vectors is a question with a measurable answer, which is why this is
    /// a flag rather than a decision baked into the code.
    pub layer_norm: bool,
    /// Longest sequence handed to the model. The baseline splits at 1900
    /// tokens against a 2048 context; the exported model accepts more, but
    /// staying at the same bound keeps behaviour comparable.
    pub max_tokens: usize,
    pub pooling: Pooling,
    /// Texts per inference call.
    pub batch_size: usize,
    /// Threads ONNX Runtime uses inside one operator. `None` leaves its default,
    /// which is a single thread and leaves most of the machine idle. Embedding a
    /// whole corpus is the case where this matters.
    pub intra_threads: Option<usize>,
    /// Which processor runs the model.
    pub device: Device,
    /// Ceiling on `texts in a batch x longest sequence in it, squared`.
    ///
    /// Attention allocates one score per pair of positions per head, so its
    /// memory grows with the SQUARE of the sequence length, not with the token
    /// count. `batch_size` alone therefore does not bound it: 64 texts at the
    /// 1900 token limit asks for 64 x 12 x 1900^2 x 4 bytes, which is 11.1 GB and
    /// is exactly the allocation that failed 45% of the way through a corpus. At
    /// the default this is 3.1 GB for the attention scores, so several sessions
    /// fit on one card at once.
    pub max_batch_cells: usize,
    /// Ceiling on the device memory arena, in bytes, per session. `None` lets it
    /// grow without bound, which is fine for one session and is not fine for two
    /// on one card: the first grows into all the free memory and the second then
    /// cannot allocate its attention buffer.
    pub device_memory_limit: Option<usize>,
}

impl Default for OnnxOptions {
    fn default() -> Self {
        OnnxOptions {
            dims: 768,
            layer_norm: false,
            max_tokens: 1900,
            pooling: Pooling::Mean,
            batch_size: 16,
            intra_threads: None,
            device: Device::Cpu,
            max_batch_cells: 24_000_000,
            device_memory_limit: None,
        }
    }
}

pub struct OnnxEmbedder {
    session: std::sync::Mutex<Session>,
    tokenizer: Tokenizer,
    options: OnnxOptions,
}

impl OnnxEmbedder {
    /// Load from a directory holding `model.onnx` (or the file named by
    /// `model_file`) plus `tokenizer.json`.
    pub fn open(dir: impl AsRef<Path>, options: OnnxOptions) -> Result<Self> {
        Self::open_model(dir, "model.onnx", options)
    }

    pub fn open_model(
        dir: impl AsRef<Path>,
        model_file: &str,
        options: OnnxOptions,
    ) -> Result<Self> {
        let dir: PathBuf = dir.as_ref().to_path_buf();
        let model_path = dir.join(model_file);
        let tokenizer_path = dir.join("tokenizer.json");

        let mut builder = Session::builder().context("creating an ONNX session builder")?;
        if let Some(threads) = options.intra_threads {
            // ort's builder returns its error carrying the builder itself, which is
            // not a plain error type, so the message is rebuilt rather than wrapped.
            builder = builder
                .with_intra_threads(threads)
                .map_err(|e| anyhow::anyhow!("setting the ONNX intra operator thread count: {e}"))?;
        }
        if let Device::Cuda(device_id) = options.device {
            preload_cuda_dylibs();
            // error_on_failure, deliberately. ort's default is to log the failure and
            // fall back to the processor, which is the worst outcome available here: an
            // embedding run meant to take an hour silently becomes one that takes a day,
            // and nothing in the output says why.
            builder = builder
                .with_execution_providers([ort::ep::CUDA::default()
                    .with_device_id(device_id)
                    // Extend the arena by exactly what was asked for. The default
                    // rounds up to the next power of two, which on a card holding
                    // two sessions means the first one reserves memory it never
                    // uses and the second one fails on an allocation that would
                    // have fitted.
                    .with_arena_extend_strategy(ort::ep::ArenaExtendStrategy::SameAsRequested)
                    .with_memory_limit(options.device_memory_limit.unwrap_or(usize::MAX))
                    .build()
                    .error_on_failure()])
                .map_err(|e| anyhow::anyhow!("registering the CUDA execution provider on card {device_id}: {e}"))?;
        }
        let session = builder
            .commit_from_file(&model_path)
            .with_context(|| format!("loading {}", model_path.display()))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading {}: {e}", tokenizer_path.display()))?;

        Ok(OnnxEmbedder {
            session: std::sync::Mutex::new(session),
            tokenizer,
            options,
        })
    }

    pub fn options(&self) -> &OnnxOptions {
        &self.options
    }

    /// Embed already prefixed texts. Callers that want the task prefixes applied
    /// should use `embed_documents` or `embed_query`.
    pub fn embed_prefixed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Tokenized once, up front, for two reasons. Every sequence in a batch is
        // padded to the longest one in it, so mixing a 6 character chunk with a
        // 6000 character one makes the short one cost as much as the long one, and
        // grouping by length removes that waste. And the batches have to respect a
        // memory ceiling that depends on the true token count rather than on the
        // character count, which only the tokenizer knows.
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenizing: {e}"))?;
        let lengths: Vec<usize> = encodings
            .iter()
            .map(|e| e.get_ids().len().min(self.options.max_tokens).max(1))
            .collect();

        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        for batch in plan_batches(&lengths, self.options.batch_size, self.options.max_batch_cells) {
            self.run_group(&batch, &encodings, &mut out)?;
        }
        Ok(out)
    }

    /// Runs one batch of already tokenized texts and writes each vector back into
    /// the caller's slot.
    /// @param batch - indices into `encodings`, all of a similar length
    /// @param encodings - the tokenizer output for the whole call
    /// @param out - the result vector, indexed the same way
    fn run_group(
        &self,
        batch: &[usize],
        encodings: &[tokenizers::Encoding],
        out: &mut [Vec<f32>],
    ) -> Result<()> {
        let picked: Vec<&tokenizers::Encoding> = batch.iter().map(|&i| &encodings[i]).collect();
        for (&i, v) in batch.iter().zip(self.run_encodings(&picked)?) {
            out[i] = v;
        }
        Ok(())
    }

    /// Tokenizes and runs one batch with no length grouping. Kept for the tests,
    /// which hand it a handful of short strings where grouping changes nothing.
    #[cfg(test)]
    fn run_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenizing: {e}"))?;
        let borrowed: Vec<&tokenizers::Encoding> = encodings.iter().collect();
        self.run_encodings(&borrowed)
    }

    fn run_encodings(&self, encodings: &[&tokenizers::Encoding]) -> Result<Vec<Vec<f32>>> {
        if encodings.is_empty() {
            return Ok(Vec::new());
        }
        // Truncate to the configured bound, then pad every sequence in the batch
        // to the longest one, because the model takes a rectangular tensor.
        let lengths: Vec<usize> = encodings
            .iter()
            .map(|e| e.get_ids().len().min(self.options.max_tokens))
            .collect();
        let width = lengths.iter().copied().max().unwrap_or(1).max(1);
        let batch = encodings.len();

        let mut ids = vec![0i64; batch * width];
        let mut mask = vec![0i64; batch * width];
        let types = vec![0i64; batch * width];

        for (row, encoding) in encodings.iter().enumerate() {
            let take = lengths[row];
            for col in 0..take {
                ids[row * width + col] = encoding.get_ids()[col] as i64;
                // Trust our own truncation rather than the encoding's mask, so a
                // truncated tail cannot be marked as present.
                mask[row * width + col] = 1;
            }
        }

        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("the ONNX session mutex was poisoned"))?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => Value::from_array(([batch, width], ids))?,
                "attention_mask" => Value::from_array(([batch, width], mask.clone()))?,
                "token_type_ids" => Value::from_array(([batch, width], types))?,
            ])
            .with_context(|| format!("running the model on {batch} texts of {width} tokens"))?;

        let (shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .context("reading last_hidden_state")?;
        let hidden = *shape.last().context("output had no trailing dimension")? as usize;
        anyhow::ensure!(
            hidden >= self.options.dims,
            "model outputs {hidden} dimensions, cannot produce {}",
            self.options.dims
        );

        let mut result = Vec::with_capacity(batch);
        for row in 0..batch {
            let mut pooled = vec![0f32; hidden];
            let mut counted = 0f32;
            for col in 0..width {
                if mask[row * width + col] == 0 {
                    continue;
                }
                let start = (row * width + col) * hidden;
                let token = &data[start..start + hidden];
                for (acc, v) in pooled.iter_mut().zip(token) {
                    *acc += *v;
                }
                counted += 1.0;
            }
            if counted > 0.0 {
                for v in pooled.iter_mut() {
                    *v /= counted;
                }
            }

            if self.options.layer_norm {
                layer_norm(&mut pooled);
            }
            pooled.truncate(self.options.dims);
            normalize(&mut pooled);
            result.push(pooled);
        }
        Ok(result)
    }
}

/// Groups texts into batches that are cheap to run and small enough to fit.
///
/// Two constraints, and only the first is obvious. Every sequence in a batch is
/// padded to the longest one in it, so texts of a similar length belong together;
/// that is what the sort is for. And attention allocates one score per pair of
/// positions per head, so the memory a batch needs grows with the SQUARE of its
/// longest sequence — `batch_size` alone does not bound it, and 64 texts at 1,900
/// tokens asks for 11.1 GB in one allocation.
///
/// A batch that would exceed the cell budget is closed early. A single text that
/// exceeds it on its own is still run: refusing it would drop a chunk from the
/// corpus, and one long sequence alone is the smallest that allocation can be.
/// @param lengths - token count per text, already truncated to the model bound
/// @param batch_size - most texts in one batch
/// @param max_cells - ceiling on `texts in the batch x longest, squared`
fn plan_batches(lengths: &[usize], batch_size: usize, max_cells: usize) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..lengths.len()).collect();
    order.sort_by_key(|&i| lengths[i]);

    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut batch: Vec<usize> = Vec::with_capacity(batch_size.max(1));
    let mut widest = 0usize;
    for &i in &order {
        let width = widest.max(lengths[i]);
        let cells = (batch.len() + 1).saturating_mul(width).saturating_mul(width);
        let full = batch.len() >= batch_size.max(1) || (!batch.is_empty() && cells > max_cells);
        if full {
            batches.push(std::mem::take(&mut batch));
            widest = 0;
        }
        widest = widest.max(lengths[i]);
        batch.push(i);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

/// Loads the CUDA and cuDNN shared libraries before the execution provider asks
/// for them, so the EP finds them without their directories being on PATH.
///
/// The directories come from `RUSTDB_CUDA_BIN` and `RUSTDB_CUDNN_BIN`. With
/// neither set nothing is preloaded and the EP falls back to the usual search
/// order, which is what a machine with CUDA already on PATH wants. Runs once:
/// ort's preloader intentionally leaks its handles, so repeating it per session
/// would leak per session.
fn preload_cuda_dylibs() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let cuda = std::env::var("RUSTDB_CUDA_BIN").ok().map(PathBuf::from);
        let cudnn = std::env::var("RUSTDB_CUDNN_BIN").ok().map(PathBuf::from);
        if cuda.is_none() && cudnn.is_none() {
            return;
        }
        if let Err(e) = ort::ep::cuda::preload_dylibs(cuda.as_deref(), cudnn.as_deref()) {
            eprintln!("warning: preloading the CUDA libraries failed: {e}");
        }
    });
}

/// Layer normalisation with no learned scale or shift: subtract the mean, divide
/// by the standard deviation. `1e-12` matches `layer_norm_epsilon` in the model's
/// own `config.json`.
fn layer_norm(v: &mut [f32]) {
    let n = v.len() as f32;
    if n == 0.0 {
        return;
    }
    let mean = v.iter().sum::<f32>() / n;
    let variance = v.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / n;
    let denominator = (variance + 1e-12).sqrt();
    for x in v.iter_mut() {
        *x = (*x - mean) / denominator;
    }
}

impl Embedder for OnnxEmbedder {
    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts.iter().map(|t| document_prefix(t)).collect();
        self.embed_prefixed(&prefixed)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let prefixed = vec![query_prefix(text)];
        Ok(self
            .embed_prefixed(&prefixed)?
            .into_iter()
            .next()
            .context("the model returned no embedding")?)
    }

    fn dimensions(&self) -> usize {
        self.options.dims
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distance::dot;

    fn model_dir() -> Option<PathBuf> {
        let dir = std::env::var("RUSTDB_ONNX_DIR").ok().map(PathBuf::from).or_else(|| {
            let home = std::env::var("HOME").ok()?;
            Some(PathBuf::from(home).join(".cache/rust-db-models/nomic-embed-text-v1.5"))
        })?;
        if dir.join("model.onnx").exists() && dir.join("tokenizer.json").exists() {
            Some(dir)
        } else {
            None
        }
    }

    /// Every test here needs the weights and the onnxruntime dylib. When either is
    /// absent the test reports that and passes, rather than failing for a reason
    /// that has nothing to do with the code.
    macro_rules! embedder_or_skip {
        ($opts:expr) => {
            match model_dir() {
                None => {
                    eprintln!("skipping: no ONNX weights found");
                    return;
                }
                Some(dir) => match OnnxEmbedder::open(&dir, $opts) {
                    Ok(e) => e,
                    Err(err) => {
                        eprintln!("skipping: could not load the model ({err:#})");
                        return;
                    }
                },
            }
        };
    }

    #[test]
    fn produces_one_unit_vector_of_the_configured_width_per_text() {
        for dims in [768usize, 512, 256, 128, 64] {
            let e = embedder_or_skip!(OnnxOptions { dims, ..Default::default() });
            let v = e
                .embed_documents(&["offer eligibility rules".to_string(), "unrelated".to_string()])
                .unwrap();
            assert_eq!(v.len(), 2);
            for x in &v {
                assert_eq!(x.len(), dims);
                assert!((dot(x, x) - 1.0).abs() < 1e-4, "width {dims} not unit length");
            }
        }
    }

    #[test]
    fn the_same_text_embeds_identically_twice() {
        let e = embedder_or_skip!(OnnxOptions::default());
        let a = e.embed_query("how does offer eligibility work").unwrap();
        let b = e.embed_query("how does offer eligibility work").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn related_text_scores_higher_than_unrelated_text() {
        let e = embedder_or_skip!(OnnxOptions::default());
        let q = e.embed_query("how are offers made eligible for a member").unwrap();
        let docs = e
            .embed_documents(&[
                "Offer eligibility is evaluated against the member's profile.".to_string(),
                "The espresso machine in the third floor kitchen is broken.".to_string(),
            ])
            .unwrap();
        let related = dot(&q, &docs[0]);
        let unrelated = dot(&q, &docs[1]);
        assert!(
            related > unrelated + 0.05,
            "related {related} did not clearly beat unrelated {unrelated}"
        );
    }

    /// Batching must not change any individual result, or a batch boundary would
    /// silently shift the vectors a document was indexed with.
    #[test]
    fn batching_does_not_change_the_result() {
        let e = embedder_or_skip!(OnnxOptions { batch_size: 2, ..Default::default() });
        let texts: Vec<String> = (0..5)
            .map(|i| format!("chunk number {i} about offer eligibility and redemption"))
            .collect();
        let batched = e.embed_documents(&texts).unwrap();
        for (i, t) in texts.iter().enumerate() {
            let alone = e.embed_documents(std::slice::from_ref(t)).unwrap();
            let agreement = dot(&batched[i], &alone[0]);
            assert!(
                agreement > 0.9999,
                "text {i} differed between batched and single: cosine {agreement}"
            );
        }
    }

    /// Padding shorter texts up to the longest in the batch must not leak into
    /// the pooled vector, which is what the attention mask is for.
    #[test]
    fn a_short_text_is_unaffected_by_a_long_one_in_the_same_batch() {
        let e = embedder_or_skip!(OnnxOptions::default());
        let short = "offer".to_string();
        let long = "offer ".repeat(200);
        let together = e.embed_documents(&[short.clone(), long]).unwrap();
        let alone = e.embed_documents(&[short]).unwrap();
        let agreement = dot(&together[0], &alone[0]);
        assert!(agreement > 0.9999, "padding leaked into the result: cosine {agreement}");
    }

    #[test]
    fn the_query_prefix_and_the_document_prefix_differ() {
        let e = embedder_or_skip!(OnnxOptions::default());
        let text = "offer eligibility rules";
        let as_query = e.embed_query(text).unwrap();
        let as_document = e.embed_documents(&[text.to_string()]).unwrap();
        // The model is trained so these are not the same vector.
        assert!(dot(&as_query, &as_document[0]) < 0.9999);
    }

    /// Layer normalisation makes no material difference to this model's output,
    /// at any width, and that is worth locking in so nobody expects otherwise.
    ///
    /// The reason is algebraic. Layer normalisation computes `(x - mean) / sd`.
    /// Dividing every component by the same `sd` is a uniform scale, and the final
    /// L2 normalisation divides by the vector's length, so the scale cancels
    /// exactly. What remains is subtracting `mean` from each component, and the
    /// mean of a pooled vector from this model is small next to the components
    /// themselves, so the direction barely moves. Cosine distance sees only
    /// direction.
    ///
    /// It still matters that the flag exists: the model card documents the step
    /// for Matryoshka use, so its absence would look like an oversight rather
    /// than a measured decision. This test records the measurement.
    #[test]
    fn layer_norm_is_immaterial_after_l2_normalisation() {
        for dims in [768usize, 512, 256, 128, 64] {
            let plain = embedder_or_skip!(OnnxOptions { dims, layer_norm: false, ..Default::default() });
            let normed = embedder_or_skip!(OnnxOptions { dims, layer_norm: true, ..Default::default() });
            let a = plain.embed_query("offer eligibility").unwrap();
            let b = normed.embed_query("offer eligibility").unwrap();
            let agreement = dot(&a, &b);
            assert!(
                agreement > 0.999,
                "at {dims} dimensions layer_norm moved the vector more than expected: cosine {agreement}"
            );
        }
    }

    #[test]
    fn layer_norm_of_a_constant_vector_is_finite() {
        // Zero variance would divide by zero without the epsilon.
        let mut v = vec![3.0f32; 8];
        layer_norm(&mut v);
        assert!(v.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn an_empty_input_returns_an_empty_result() {
        let e = embedder_or_skip!(OnnxOptions::default());
        assert!(e.embed_documents(&[]).unwrap().is_empty());
    }

    /// The batch planner is the thing that stopped a whole corpus run dying 45% of
    /// the way through, so its two constraints are worth asserting directly.
    #[test]
    fn batches_respect_the_count_and_the_attention_budget() {
        // Long sequences: the cell budget binds long before the count does.
        let lengths = vec![1900usize; 64];
        let batches = plan_batches(&lengths, 64, 24_000_000);
        assert!(batches.len() > 1, "64 sequences of 1900 tokens must not be one batch");
        for b in &batches {
            let width = b.iter().map(|&i| lengths[i]).max().unwrap();
            assert!(b.len() * width * width <= 24_000_000 || b.len() == 1);
            assert!(b.len() <= 64);
        }

        // Short sequences: the count binds and the budget never does.
        let short = vec![8usize; 500];
        let batches = plan_batches(&short, 16, 24_000_000);
        assert_eq!(batches.len(), 500 / 16 + usize::from(500 % 16 != 0));
        assert!(batches.iter().all(|b| b.len() <= 16));
    }

    /// Every text has to be embedded exactly once, whatever the batching does. A
    /// planner that dropped one would pair every later vector with the wrong chunk.
    #[test]
    fn every_text_lands_in_exactly_one_batch() {
        let lengths: Vec<usize> = (0..311).map(|i| 1 + (i * 37) % 2000).collect();
        let batches = plan_batches(&lengths, 32, 24_000_000);
        let mut seen: Vec<usize> = batches.iter().flatten().copied().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..lengths.len()).collect::<Vec<_>>());
    }

    /// A single sequence too large for the budget is still run rather than dropped:
    /// refusing it would silently remove a chunk from the corpus.
    #[test]
    fn one_oversized_text_is_still_given_a_batch() {
        let batches = plan_batches(&[4000], 32, 1_000);
        assert_eq!(batches, vec![vec![0]]);
    }

    /// Texts of a similar length share a batch, because every sequence is padded to
    /// the longest one in it and mixing lengths pays for padding.
    #[test]
    fn batches_group_texts_of_a_similar_length() {
        let lengths = vec![1000, 5, 1000, 5, 1000, 5];
        let batches = plan_batches(&lengths, 3, usize::MAX);
        for b in &batches {
            let widths: Vec<usize> = b.iter().map(|&i| lengths[i]).collect();
            assert!(widths.iter().all(|w| *w == widths[0]), "mixed lengths in one batch: {widths:?}");
        }
    }

    /// `cuda:N` is how a caller names a card, and a name that is not a device has to
    /// be refused rather than silently becoming the processor.
    #[test]
    fn devices_parse_from_their_names() {
        assert_eq!(Device::parse("cpu").unwrap(), Device::Cpu);
        assert_eq!(Device::parse("cuda").unwrap(), Device::Cuda(0));
        assert_eq!(Device::parse("CUDA:1").unwrap(), Device::Cuda(1));
        assert_eq!(Device::parse(" cuda:3 ").unwrap(), Device::Cuda(3));
        assert!(Device::parse("gpu").is_err());
        assert!(Device::parse("cuda:x").is_err());
        assert_eq!(Device::Cuda(1).label(), "cuda:1");
    }
}
