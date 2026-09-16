//! An ONNX text embedder run inside the calling process.
//!
//! This is the replacement for the `llama-server` child process the baseline
//! talks to over HTTP. Same model, no separate process, no network hop.
//!
//! The model file exports token vectors, not one vector per text: its output
//! `last_hidden_state` has shape `[batch, sequence, hidden]`. Turning that into
//! one embedding is this module's job, and the order of those steps is what
//! decides whether the result matches what the baseline produced.
//!
//! Which steps those are is the model's business, not this module's, so every
//! one of them - the four task prefixes, the pooling, the truncation bound, the
//! width, whether the export takes `token_type_ids` - comes from a
//! [`ModelManifest`]. `OnnxOptions::default()`
//! still reproduces `nomic-embed-text-v1.5` exactly, so the baseline arm is
//! unchanged by the generalisation and the existing score card is unmoved.
//!
//! Invariant: **the model is told what it was trained to be told, and what it
//! could not see is counted.** The four task prefixes, the pooling and the
//! truncation bound are properties of the export rather than conventions of the
//! caller, and applying the wrong one is measurably worse in both directions.
//! A text longer than the bound is embedded from a prefix of itself, and the
//! count of those is kept - a model whose tokenizer is more verbose sees less
//! of each chunk than its rivals and would otherwise look merely faster.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::Value;
use tokenizers::Tokenizer;

use crate::distance::normalize;
use crate::embed::Embedder;
use crate::model::{ModelManifest, Output, Pooling, Prefixes};

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
            let id: i32 = rest
                .parse()
                .with_context(|| format!("{rest} is not a card ordinal"))?;
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

/// How much work ONNX Runtime does on the graph while a session loads.
///
/// It is a knob rather than a constant because the two things it trades against
/// are both real and pull in opposite directions: every optimizer pass costs
/// wall clock at load, and several of them - operator fusion above all - are
/// what make the inference that follows fast. A process that loads the model
/// once and then embeds a corpus wants all of them. A process that loads the
/// model to embed one query and then drops it is paying for passes whose
/// benefit it will use exactly once, and `docs/embeddings.md` prints what each
/// level costs on this model.
///
/// The names are ONNX Runtime's own, so a reader can look up which passes are
/// in which level without translating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Optimization {
    /// No passes at all. The graph is run as it was exported.
    Disable,
    /// Constant folding, redundant-node elimination and the other passes that
    /// need no knowledge of the execution provider.
    Basic,
    /// Basic, plus the provider-aware passes: attention fusion is the one that
    /// matters for a transformer encoder.
    Extended,
    /// Everything, including the layout passes. ONNX Runtime's default, and this
    /// module's.
    All,
}

impl Optimization {
    /// Parses `disable`, `basic`, `extended` or `all`.
    /// @param text - the level as written on a command line
    pub fn parse(text: &str) -> Result<Optimization> {
        match text.trim().to_ascii_lowercase().as_str() {
            "disable" | "none" | "off" => Ok(Optimization::Disable),
            "basic" | "level1" => Ok(Optimization::Basic),
            "extended" | "level2" => Ok(Optimization::Extended),
            "all" | "level3" => Ok(Optimization::All),
            other => anyhow::bail!(
                "unknown optimization level {other}, expected disable, basic, extended or all"
            ),
        }
    }

    /// A short label for a report column.
    pub fn label(&self) -> &'static str {
        match self {
            Optimization::Disable => "disable",
            Optimization::Basic => "basic",
            Optimization::Extended => "extended",
            Optimization::All => "all",
        }
    }

    /// The ONNX Runtime level this stands for.
    fn level(&self) -> ort::session::builder::GraphOptimizationLevel {
        use ort::session::builder::GraphOptimizationLevel;
        match self {
            Optimization::Disable => GraphOptimizationLevel::Disable,
            Optimization::Basic => GraphOptimizationLevel::Level1,
            Optimization::Extended => GraphOptimizationLevel::Level2,
            // `All`, not `Level3`. ort maps `Level3` to `ORT_ENABLE_LAYOUT`,
            // which is the value 3 and was only added to the C API in ONNX
            // Runtime 1.23 - an older runtime refuses it outright with
            // "graph_optimization_level is not valid" and the session never
            // opens. `ORT_ENABLE_ALL` is 99 and has meant "every pass" since
            // the enum existed, so it is the one value that opens a session on
            // every runtime this can be pointed at.
            Optimization::All => GraphOptimizationLevel::All,
        }
    }
}

/// Everything about one exported model that the engine has to be told rather
/// than able to read out of the graph.
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
    /// How the token vectors become one vector.
    pub pooling: Pooling,
    /// The four task prefixes, from the model's manifest. Applying the wrong
    /// one, or applying one to a model trained without them, is measurably worse
    /// in both directions, so it is a property of the model rather than a
    /// convention of the harness.
    pub prefixes: Prefixes,
    /// Whether the export declares a `token_type_ids` input. BERT-family exports
    /// do; ModernBERT-class and decoder exports generally do not, and feeding an
    /// input a graph never declared fails the session outright.
    pub token_type_ids: bool,
    /// Which output carries the answer, and under what name.
    pub output: Output,
    /// The name that output carries in the graph.
    pub output_name: String,
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
    /// How much graph optimization to do while loading.
    pub optimization: Optimization,
    /// Where to write the optimized graph, when the caller wants one written.
    ///
    /// ONNX Runtime can serialize the graph it produced after its passes ran.
    /// Loading *that* file at [`Optimization::Disable`] is the same computation
    /// with the passes already paid for, which is the one strategy that makes a
    /// repeated load cheaper without changing what the model computes.
    pub optimized_model_path: Option<PathBuf>,
}

impl Default for OnnxOptions {
    fn default() -> Self {
        OnnxOptions {
            dims: 768,
            layer_norm: false,
            max_tokens: 1900,
            pooling: Pooling::Mean,
            prefixes: Prefixes::nomic(),
            token_type_ids: true,
            output: Output::TokenEmbeddings,
            output_name: String::new(),
            batch_size: 16,
            intra_threads: None,
            device: Device::Cpu,
            max_batch_cells: 24_000_000,
            device_memory_limit: None,
            optimization: Optimization::All,
            optimized_model_path: None,
        }
    }
}

impl OnnxOptions {
    /// Runtime options that reproduce one model's contract.
    ///
    /// The manifest decides everything about what the model is; the caller keeps
    /// deciding everything about how hard the machine is worked. Splitting them
    /// this way is what lets one arm be swapped for another without a single
    /// batching or device decision moving with it.
    /// @param manifest - the model being run
    pub fn for_model(manifest: &ModelManifest) -> OnnxOptions {
        OnnxOptions {
            dims: manifest.dims,
            layer_norm: manifest.layer_norm,
            max_tokens: manifest.max_tokens,
            pooling: manifest.pooling,
            prefixes: manifest.prefixes.clone(),
            token_type_ids: manifest.token_type_ids,
            output: manifest.output,
            output_name: manifest.output_name.clone(),
            ..Default::default()
        }
    }

    /// The same, with the machine settings a caller has already chosen.
    /// @param manifest - the model being run
    /// @param batch_size - texts per inference call
    /// @param device - the processor to open the session on
    pub fn for_model_on(
        manifest: &ModelManifest,
        batch_size: usize,
        device: Device,
    ) -> OnnxOptions {
        OnnxOptions {
            batch_size,
            device,
            ..OnnxOptions::for_model(manifest)
        }
    }
}

/// An embedder over an ONNX export, with the counters that say how much text
/// the model actually saw.
pub struct OnnxEmbedder {
    session: std::sync::Mutex<Session>,
    tokenizer: Tokenizer,
    options: OnnxOptions,
    /// Texts handed to the model, and how many of them were longer than
    /// `max_tokens` and therefore embedded from a prefix of themselves.
    ///
    /// Counted rather than inferred, because a model whose tokenizer is more
    /// verbose sees less of each chunk than its rivals do and would otherwise
    /// look merely faster. The card prints the share; this is where it comes from.
    seen: std::sync::atomic::AtomicUsize,
    truncated: std::sync::atomic::AtomicUsize,
    tokens: std::sync::atomic::AtomicUsize,
}

/// How much text a model actually saw, over the run so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TruncationFacts {
    /// How many texts were handed to the model.
    pub texts: usize,
    /// How many of them were longer than `max_tokens` and were therefore
    /// embedded from a prefix of themselves.
    pub truncated: usize,
    /// Total tokens after truncation, which is what the model was charged for.
    pub tokens: usize,
}

impl TruncationFacts {
    /// Returns the share of texts that were truncated, from 0 to 1.
    pub fn share(&self) -> f64 {
        if self.texts == 0 {
            0.0
        } else {
            self.truncated as f64 / self.texts as f64
        }
    }

    /// Returns the mean tokens per text after truncation, which is what the
    /// model was charged for.
    pub fn tokens_per_text(&self) -> f64 {
        if self.texts == 0 {
            0.0
        } else {
            self.tokens as f64 / self.texts as f64
        }
    }
}

impl OnnxEmbedder {
    /// Load from a directory holding `model.onnx` (or the file named by
    /// `model_file`) plus `tokenizer.json`.
    pub fn open(dir: impl AsRef<Path>, options: OnnxOptions) -> Result<Self> {
        Self::open_model(dir, "model.onnx", options)
    }

    /// Load the model a manifest describes, from the directory holding it.
    ///
    /// The one entry point an arm should use: the manifest names the weights
    /// file, so a caller cannot open one model's graph with another model's
    /// contract by passing the file name separately.
    /// @param dir - the model directory
    /// @param manifest - what the model is
    /// @param batch_size - texts per inference call
    /// @param device - the processor to open the session on
    pub fn open_manifest(
        dir: impl AsRef<Path>,
        manifest: &ModelManifest,
        batch_size: usize,
        device: Device,
    ) -> Result<Self> {
        let file = manifest.model_file.clone();
        Self::open_model(
            dir,
            &file,
            OnnxOptions::for_model_on(manifest, batch_size, device),
        )
    }

    /// The output this arm reads, resolved against what the export actually
    /// offers.
    ///
    /// `optimum` names the per-token output `last_hidden_state`; some
    /// sentence-transformers exports name it `token_embeddings`; several emit
    /// both that and an already-pooled `sentence_embedding`. Pooling is this
    /// module's job and has to be identical across arms, so a pooled output is
    /// never picked up by accident - only a manifest that names it gets it, and
    /// the card then says which arms were pooled here and which were not.
    /// @param outputs - what the session returned
    fn output_name(&self, outputs: &ort::session::SessionOutputs<'_>) -> Result<String> {
        if !self.options.output_name.is_empty() {
            let wanted = self.options.output_name.clone();
            anyhow::ensure!(
                outputs.get(wanted.as_str()).is_some(),
                "the manifest names the output {wanted}, and the export offers {:?}",
                outputs.keys().collect::<Vec<_>>()
            );
            return Ok(wanted);
        }
        let candidates: &[&str] = match self.options.output {
            Output::TokenEmbeddings => &["last_hidden_state", "token_embeddings", "hidden_states"],
            Output::SentenceEmbedding => &["sentence_embedding", "text_embeds", "embeddings"],
        };
        for wanted in candidates {
            if outputs.get(*wanted).is_some() {
                return Ok((*wanted).to_string());
            }
        }
        anyhow::bail!(
            "the export offers none of {candidates:?}; it named {:?}. Either re-export it or \
             name the output in the manifest's output_name",
            outputs.keys().collect::<Vec<_>>()
        )
    }

    /// How much of the text handed to this session the model actually saw.
    pub fn truncation(&self) -> TruncationFacts {
        use std::sync::atomic::Ordering::Relaxed;
        TruncationFacts {
            texts: self.seen.load(Relaxed),
            truncated: self.truncated.load(Relaxed),
            tokens: self.tokens.load(Relaxed),
        }
    }

    /// Opens one exported model and its tokenizer.
    ///
    /// @param dir - the directory holding the export and `tokenizer.json`
    /// @param model_file - which file in it is the graph
    /// @param options - what the graph cannot say about itself
    pub fn open_model(
        dir: impl AsRef<Path>,
        model_file: &str,
        options: OnnxOptions,
    ) -> Result<Self> {
        let dir: PathBuf = dir.as_ref().to_path_buf();
        let model_path = dir.join(model_file);
        let tokenizer_path = dir.join("tokenizer.json");

        use_installed_runtime();

        let mut builder = Session::builder().context("creating an ONNX session builder")?;
        builder = builder
            .with_optimization_level(options.optimization.level())
            .map_err(|e| anyhow::anyhow!("setting the ONNX graph optimization level: {e}"))?;
        if let Some(path) = options.optimized_model_path.as_ref() {
            builder = builder.with_optimized_model_path(path).map_err(|e| {
                anyhow::anyhow!("asking for the optimized graph at {}: {e}", path.display())
            })?;
        }
        if let Some(threads) = options.intra_threads {
            // ort's builder returns its error carrying the builder itself, which is
            // not a plain error type, so the message is rebuilt rather than wrapped.
            builder = builder.with_intra_threads(threads).map_err(|e| {
                anyhow::anyhow!("setting the ONNX intra operator thread count: {e}")
            })?;
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
                .map_err(|e| {
                    anyhow::anyhow!(
                        "registering the CUDA execution provider on card {device_id}: {e}"
                    )
                })?;
        }
        let session = builder
            .commit_from_file(&model_path)
            .with_context(|| format!("loading {}", model_path.display()))?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading {}: {e}", tokenizer_path.display()))?;
        disarm_tokenizer(&mut tokenizer);

        Ok(OnnxEmbedder {
            session: std::sync::Mutex::new(session),
            tokenizer,
            options,
            seen: std::sync::atomic::AtomicUsize::new(0),
            truncated: std::sync::atomic::AtomicUsize::new(0),
            tokens: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Returns the options this embedder was opened with.
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

        {
            use std::sync::atomic::Ordering::Relaxed;
            let cut = encodings
                .iter()
                .filter(|e| e.get_ids().len() > self.options.max_tokens)
                .count();
            self.seen.fetch_add(texts.len(), Relaxed);
            self.truncated.fetch_add(cut, Relaxed);
            self.tokens
                .fetch_add(lengths.iter().sum::<usize>(), Relaxed);
        }

        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        for batch in plan_batches(
            &lengths,
            self.options.batch_size,
            self.options.max_batch_cells,
        ) {
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
        // `batch` holds indices into `encodings` that `plan_batches` produced
        // from `encodings.len()`, so each is in range; `filter_map` says that to
        // the compiler instead (task-1932, H9).
        let picked: Vec<&tokenizers::Encoding> =
            batch.iter().filter_map(|&i| encodings.get(i)).collect();
        for (&i, v) in batch.iter().zip(self.run_encodings(&picked)?) {
            if let Some(slot) = out.get_mut(i) {
                *slot = v;
            }
        }
        Ok(())
    }

    /// Tokenizes and runs one batch with no length grouping. Kept for the tests,
    /// which hand it a handful of short strings where grouping changes nothing.
    ///
    /// Nothing calls it today. It stays because it is the one path that runs the
    /// model without the length grouping, so a test that suspects the grouping
    /// has something to compare against; `allow` rather than deletion says that
    /// deliberately.
    #[cfg(test)]
    #[allow(dead_code)]
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
            let take = lengths.get(row).copied().unwrap_or(0);
            let encoded = encoding.get_attention_mask();
            let source = encoding.get_ids();
            for col in 0..take {
                let at = row.saturating_mul(width).saturating_add(col);
                let (Some(slot), Some(id)) = (ids.get_mut(at), source.get(col)) else {
                    continue;
                };
                *slot = i64::from(*id);
                // The encoding's own mask AND our truncation. The second half is
                // what stops a truncated tail being marked present. The first is
                // what stops a tokenizer that padded for itself - one of the eight
                // models here ships that setting - having its padding attended to
                // as though it were text. `disarm_tokenizer` clears that setting
                // on load, and this is the belt to its braces: a mask built from
                // a length alone cannot tell the two apart, and a wrong mask is a
                // wrong vector that nothing downstream would notice.
                if let Some(slot) = mask.get_mut(at) {
                    *slot = i64::from(encoded.get(col).copied().unwrap_or(1) != 0);
                }
            }
        }

        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("the ONNX session mutex was poisoned"))?;
        let inputs = build_inputs(&session, batch, width, ids, &mask, types)?;
        let outputs = session.run(inputs).map_err(|e| {
            // A batch of one is the smallest this planner can make, so when one
            // fails there is no batching left to adjust and the only lever is the
            // model's own truncation bound. Saying so here is the difference
            // between a diagnosable failure and an ONNX Runtime allocation
            // message: `qwen3-embedding-0.6b` died 46% through this corpus on a
            // single 5,327-token chunk asking for 3.76 GB of attention scores,
            // and the bound in its manifest was the fix.
            let hint = if batch == 1 && width > 1024 {
                format!(
                    ". This was one text on its own at {width} tokens, so no batching change can \
                     make it smaller: attention allocates one score per pair of positions per \
                     head, which grows with the square of the width. The lever is max_tokens in \
                     this model's manifest, currently {}. Measure how many chunks a lower bound \
                     would truncate before choosing one",
                    self.options.max_tokens
                )
            } else {
                String::new()
            };
            anyhow::anyhow!("{e}").context(format!(
                "running the model on {batch} texts of {width} tokens{hint}"
            ))
        })?;

        let name = self.output_name(&outputs)?;
        let (shape, data) = outputs
            .get(name.as_str())
            .with_context(|| format!("{name} is not among the model's outputs"))?
            .try_extract_tensor::<f32>()
            .with_context(|| format!("reading {name}"))?;
        let hidden = *shape.last().context("output had no trailing dimension")? as usize;

        // An export that pooled for itself hands back one vector per text, and
        // there is nothing left for this module to pool. Only a manifest that
        // explicitly says so reaches here; the default is the per-token output,
        // pooled here, because pooling identically across arms is what makes two
        // arms comparable at all.
        if self.options.output == Output::SentenceEmbedding {
            anyhow::ensure!(
                shape.len() == 2 && shape.first().copied().unwrap_or(0) as usize == batch,
                "{name} has shape {shape:?}; a pooled output must be [batch, hidden]"
            );
            anyhow::ensure!(
                hidden >= self.options.dims,
                "{name} is {hidden} wide and the manifest asks for {}",
                self.options.dims
            );
            let mut result = Vec::with_capacity(batch);
            for row in 0..batch {
                let start = row.saturating_mul(hidden);
                let Some(slice) = data.get(start..start.saturating_add(hidden)) else {
                    anyhow::bail!("{name} is shorter than its own shape says");
                };
                let mut pooled = slice.to_vec();
                if self.options.layer_norm {
                    layer_norm(&mut pooled);
                }
                pooled.truncate(self.options.dims);
                normalize(&mut pooled);
                result.push(pooled);
            }
            return Ok(result);
        }

        anyhow::ensure!(
            shape.len() == 3,
            "{name} has shape {shape:?}; the pooling here needs [batch, sequence, hidden]"
        );
        anyhow::ensure!(
            hidden >= self.options.dims,
            "model outputs {hidden} dimensions, cannot produce {}",
            self.options.dims
        );

        let mut result = Vec::with_capacity(batch);
        for row in 0..batch {
            // An empty slice for a position the tensor does not hold, which
            // contributes nothing to a mean and gives a zero vector for a `Cls`
            // or `LastToken` pick - the same outcome an index would have
            // reached by ending the process (task-1932, H9).
            let token_at = |col: usize| -> &[f32] {
                let start = row
                    .saturating_mul(width)
                    .saturating_add(col)
                    .saturating_mul(hidden);
                data.get(start..start.saturating_add(hidden)).unwrap_or(&[])
            };
            let masked = |col: usize| -> bool {
                mask.get(row.saturating_mul(width).saturating_add(col))
                    .copied()
                    .unwrap_or(0)
                    == 0
            };
            let mut pooled = match self.options.pooling {
                // Every unmasked position, averaged. Padding contributes nothing
                // because the mask says it is not there.
                Pooling::Mean => {
                    let mut acc = vec![0f32; hidden];
                    let mut counted = 0f32;
                    for col in 0..width {
                        if masked(col) {
                            continue;
                        }
                        for (a, v) in acc.iter_mut().zip(token_at(col)) {
                            *a += *v;
                        }
                        counted += 1.0;
                    }
                    if counted > 0.0 {
                        for v in acc.iter_mut() {
                            *v /= counted;
                        }
                    }
                    acc
                }
                // The classification position, which these exports place first.
                Pooling::Cls => token_at(0).to_vec(),
                // The last position the mask admits. Left padding would put it
                // elsewhere, and this batcher pads on the right, which is what
                // makes the scan from the end correct.
                Pooling::LastToken => {
                    let last = (0..width).rev().find(|&col| !masked(col));
                    token_at(last.unwrap_or(0)).to_vec()
                }
            };

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

/// How much of a set of texts a model would truncate, without running it.
///
/// The tokenizer alone answers this, and answering it without a session is what
/// lets a resumed embedding run still report a truncation share over the whole
/// corpus rather than over the part one process happened to embed. A share that
/// silently described a tail would read as a measurement and would not be one.
/// @param dir - the model directory, holding `tokenizer.json`
/// @param manifest - the model, for its document prefix and its token bound
/// @param texts - the raw texts, before any prefix
pub fn count_truncation(
    dir: impl AsRef<Path>,
    manifest: &ModelManifest,
    texts: &[String],
) -> Result<TruncationFacts> {
    if texts.is_empty() {
        return Ok(TruncationFacts::default());
    }
    let path = dir.as_ref().join("tokenizer.json");
    let mut tokenizer = Tokenizer::from_file(&path)
        .map_err(|e| anyhow::anyhow!("loading {}: {e}", path.display()))?;
    // The same disarming `open_model` does, and for the same reason, in the
    // second place it was needed and the first place it was forgotten. Without
    // it this counts a tokenizer's own truncation as "nothing was truncated":
    // `snowflake-arctic-embed-m-v2.0` caps itself at 512, so counting against an
    // 8,192 bound through an armed tokenizer finds zero chunks over it, and the
    // card printed 0.00% for an arm whose embedding run had counted 13.
    disarm_tokenizer(&mut tokenizer);
    let mut facts = TruncationFacts::default();
    for window in texts.chunks(1024) {
        let prefixed: Vec<String> = window
            .iter()
            .map(|t| format!("{}{t}", manifest.prefixes.document))
            .collect();
        let encoded = tokenizer
            .encode_batch(prefixed, true)
            .map_err(|e| anyhow::anyhow!("tokenizing to count truncation: {e}"))?;
        for e in &encoded {
            let len = e.get_ids().len();
            facts.texts += 1;
            facts.tokens += len.min(manifest.max_tokens).max(1);
            if len > manifest.max_tokens {
                facts.truncated += 1;
            }
        }
    }
    Ok(facts)
}

/// Take the padding and the truncation out of a tokenizer's own configuration.
///
/// A `tokenizer.json` may carry both, and one of the eight models compared here
/// does: `snowflake-arctic-embed-m-v2.0` ships `padding: BatchLongest` and
/// `truncation: max_length 512`, inherited from its sentence-transformers setup.
/// Left in place, each of those quietly breaks a different measurement.
///
/// Padding, because this module pads the batch itself and marks every position it
/// filled as present; a tokenizer that has already padded hands back an encoding
/// whose length is the padded length, and the model then attends to padding as if
/// it were text. Truncation, because the token count this module reads back is
/// then the *post-truncation* count, so a chunk the tokenizer had already cut
/// looks like a chunk that fitted, the truncation share reports zero, and the arm
/// is graded as an 8,192-token model that never saw more than 512 tokens of
/// anything. The first is a wrong vector; the second is a card that states the
/// opposite of what happened, which is worse.
///
/// So both are cleared, here, once, for every model. Truncation is this module's
/// decision and it is taken from the manifest, where it is recorded, digested
/// into the cache header, and printed on the card.
/// @param tokenizer - the loaded tokenizer, modified in place
fn disarm_tokenizer(tokenizer: &mut Tokenizer) {
    tokenizer.with_padding(None);
    if let Err(e) = tokenizer.with_truncation(None) {
        // `with_truncation(None)` cannot fail in this version, and if a later one
        // makes it fallible the failure has to be loud: silently keeping the
        // tokenizer's own bound is exactly the measurement error above.
        eprintln!("warning: could not clear the tokenizer's own truncation: {e}");
    }
}

/// Fill in exactly the inputs a graph declares, driven by the graph.
///
/// Encoder exports differ in what they ask for and the differences are not
/// optional: a BERT-family export declares `token_type_ids` and a ModernBERT one
/// does not, and handing a graph an input it never declared is a hard session
/// error rather than a value quietly ignored. The decoder-backbone exports go
/// further and declare `position_ids` plus a full past-key-value cache, because
/// they were exported for generation; run once with no history, every one of
/// those cache tensors is simply empty.
///
/// Reading the requirement off the session rather than off the manifest is
/// deliberate. The graph is the authority on what the graph needs, a manifest
/// field saying "this one also wants position ids" would be a second copy of a
/// fact that can be looked up, and a second copy is a thing that can disagree.
/// @param session - the loaded graph, for its declared inputs
/// @param batch - texts in this batch
/// @param width - padded sequence length
/// @param ids - token ids, row major
/// @param mask - attention mask, row major
/// @param types - token type ids, row major, all zero
fn build_inputs<'a>(
    session: &Session,
    batch: usize,
    width: usize,
    ids: Vec<i64>,
    mask: &[i64],
    types: Vec<i64>,
) -> Result<
    Vec<(
        std::borrow::Cow<'a, str>,
        ort::session::SessionInputValue<'a>,
    )>,
> {
    let mut ids = Some(ids);
    let mut types = Some(types);
    let mut out: Vec<(
        std::borrow::Cow<'a, str>,
        ort::session::SessionInputValue<'a>,
    )> = Vec::new();
    for input in session.inputs() {
        let name = input.name().to_string();
        let value: Value = match name.as_str() {
            "input_ids" => Value::from_array((
                [batch, width],
                ids.take().context("input_ids was declared twice")?,
            ))?
            .into(),
            "attention_mask" => Value::from_array(([batch, width], mask.to_vec()))?.into(),
            "token_type_ids" => Value::from_array((
                [batch, width],
                types.take().context("token_type_ids was declared twice")?,
            ))?
            .into(),
            // Right-padded, so position i is position i for every row. A left
            // padded batch would need the offset, and this batcher does not
            // produce one.
            "position_ids" => {
                let mut positions = Vec::with_capacity(batch * width);
                for _ in 0..batch {
                    positions.extend(0..width as i64);
                }
                Value::from_array(([batch, width], positions))?.into()
            }
            other if other.starts_with("past_key_values.") => empty_cache_tensor(input, batch)
                .with_context(|| format!("building the empty past-key-value tensor {other}"))?,
            other => anyhow::bail!(
                "the export declares an input this embedder does not know how to fill: {other}. \
                 Re-export it for feature extraction, or teach build_inputs what it means - \
                 guessing at a tensor the model will read is how an arm silently embeds noise"
            ),
        };
        out.push((std::borrow::Cow::Owned(name), value.into()));
    }
    anyhow::ensure!(
        ids.is_none(),
        "the export declares no input_ids; its inputs are {:?}",
        session
            .inputs()
            .iter()
            .map(|i| i.name())
            .collect::<Vec<_>>()
    );
    Ok(out)
}

/// A past-key-value tensor with no history in it.
///
/// Shape comes from the declaration: the batch dimension and the sequence
/// dimension are dynamic, everything else - the number of key/value heads and the
/// head width - is fixed by the model and is written into the graph. Filling in
/// the fixed dimensions from the graph rather than from a table is what lets one
/// implementation serve every decoder export.
/// @param input - the declared input
/// @param batch - texts in this batch
fn empty_cache_tensor(input: &ort::value::Outlet, batch: usize) -> Result<Value> {
    let ort::value::ValueType::Tensor { shape, .. } = input.dtype() else {
        anyhow::bail!("{} is not a tensor", input.name());
    };
    let dims: Vec<i64> = shape.iter().copied().collect();
    anyhow::ensure!(
        dims.len() == 4,
        "{} has {} dimensions; a past-key-value tensor has four",
        input.name(),
        dims.len()
    );
    // Dimension 0 is the batch and dimension 2 is the history, which is empty.
    // Both are declared dynamic (-1); the other two are the model's own numbers.
    let mut resolved = [0usize; 4];
    for (i, d) in dims.iter().enumerate() {
        let value = match i {
            0 => batch,
            2 => 0,
            _ => {
                anyhow::ensure!(
                    *d > 0,
                    "{} leaves dimension {i} dynamic, so there is no way to know how wide the \
                     model's attention heads are",
                    input.name()
                );
                *d as usize
            }
        };
        // The `dims.len() == 4` check above makes this in range; `get_mut` is
        // how that is said to the compiler (task-1932, H9).
        if let Some(slot) = resolved.get_mut(i) {
            *slot = value;
        }
    }
    Ok(Value::from_array((resolved, Vec::<f32>::new()))?.into())
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
    order.sort_by_key(|&i| lengths.get(i).copied().unwrap_or(0));

    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut batch: Vec<usize> = Vec::with_capacity(batch_size.max(1));
    let mut widest = 0usize;
    for &i in &order {
        let width = widest.max(lengths.get(i).copied().unwrap_or(0));
        let cells = (batch.len() + 1)
            .saturating_mul(width)
            .saturating_mul(width);
        let full = batch.len() >= batch_size.max(1) || (!batch.is_empty() && cells > max_cells);
        if full {
            batches.push(std::mem::take(&mut batch));
            widest = 0;
        }
        widest = widest.max(lengths.get(i).copied().unwrap_or(0));
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
/// The directories come from `INILLUCENT_CUDA_BIN` and `INILLUCENT_CUDNN_BIN`. With
/// neither set nothing is preloaded and the EP falls back to the usual search
/// order, which is what a machine with CUDA already on PATH wants. Runs once:
/// ort's preloader intentionally leaks its handles, so repeating it per session
/// would leak per session.
/// Points `ort` at the ONNX Runtime `inillucent setup-embeddings` installed,
/// when nothing has already said where to find one.
///
/// This is what makes the setup command's promise true. `ort` under
/// `load-dynamic` resolves the shared library the first time any of its APIs is
/// used: `ORT_DYLIB_PATH` if it is set, and otherwise the bare file name handed
/// to the system loader, which finds a copy on `PATH` or does not. Neither of
/// those knows about a per-user install directory, so without this a person who
/// ran the setup command would still have to export a variable before the
/// engine could use what they installed.
///
/// It runs once and it never overrules a caller. `install::runtime_library`
/// returns whatever `ORT_DYLIB_PATH` names when that is set, so an operator who
/// has pinned a specific library keeps it; and a failure to load the installed
/// one is left to the session to report, because the loader's own message names
/// the file and a message from here would only name the attempt.
fn use_installed_runtime() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        if std::env::var("ORT_DYLIB_PATH").is_ok_and(|value| !value.trim().is_empty()) {
            return;
        }
        let Some(library) = crate::install::runtime_library() else { return };
        match ort::init_from(&library) {
            Ok(builder) => {
                builder.commit();
            }
            Err(error) => eprintln!(
                "warning: the ONNX Runtime at {} would not load ({error}); falling back to the system loader",
                library.display()
            ),
        }
    });
}

fn preload_cuda_dylibs() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let cuda = std::env::var("INILLUCENT_CUDA_BIN").ok().map(PathBuf::from);
        let cudnn = std::env::var("INILLUCENT_CUDNN_BIN")
            .ok()
            .map(PathBuf::from);
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
        let prefix = &self.options.prefixes.document;
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        self.embed_prefixed(&prefixed)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let prefixed = vec![format!("{}{text}", self.options.prefixes.query)];
        self.embed_prefixed(&prefixed)?
            .into_iter()
            .next()
            .context("the model returned no embedding")
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
        // The roots are the installer's, read from the environment rather than
        // written here: this list used to hold a drive letter from the machine
        // the engine was written on (task-1946, H7).
        let roots = std::env::var(crate::install::MODEL_ROOTS_VAR)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                value
                    .split(if cfg!(windows) { ';' } else { ':' })
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let dir = std::env::var("INILLUCENT_ONNX_DIR")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                for root in roots
                    .iter()
                    .map(String::as_str)
                    .chain(["~/.cache/inillucent-models"])
                {
                    let root = match root.strip_prefix("~/") {
                        Some(rest) => PathBuf::from(std::env::var("HOME").ok()?).join(rest),
                        None => PathBuf::from(root),
                    };
                    let dir = root.join("nomic-embed-text-v1.5");
                    if dir.join("model.onnx").exists() {
                        return Some(dir);
                    }
                }
                inillucent_base::testing::skipping("no model root holds nomic-embed-text-v1.5");
                None
            })?;
        if dir.join("model.onnx").exists() && dir.join("tokenizer.json").exists() {
            Some(dir)
        } else {
            // The five cases that call this reported green on a machine with no
            // weights and said nothing at all (task-1946, H10).
            inillucent_base::testing::skipping(&format!(
                "{} holds no complete model",
                dir.display()
            ));
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
                    inillucent_base::testing::skipping("no ONNX weights found");
                    return;
                }
                Some(dir) => match OnnxEmbedder::open(&dir, $opts) {
                    Ok(e) => e,
                    Err(err) => {
                        inillucent_base::testing::skipping(&format!(
                            "could not load the model ({err:#})"
                        ));
                        return;
                    }
                },
            }
        };
    }

    #[test]
    fn produces_one_unit_vector_of_the_configured_width_per_text() {
        for dims in [768usize, 512, 256, 128, 64] {
            let e = embedder_or_skip!(OnnxOptions {
                dims,
                ..Default::default()
            });
            let v = e
                .embed_documents(&[
                    "offer eligibility rules".to_string(),
                    "unrelated".to_string(),
                ])
                .unwrap();
            assert_eq!(v.len(), 2);
            for x in &v {
                assert_eq!(x.len(), dims);
                assert!(
                    (dot(x, x) - 1.0).abs() < 1e-4,
                    "width {dims} not unit length"
                );
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
        let q = e
            .embed_query("how are offers made eligible for a member")
            .unwrap();
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
        let e = embedder_or_skip!(OnnxOptions {
            batch_size: 2,
            ..Default::default()
        });
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
        assert!(
            agreement > 0.9999,
            "padding leaked into the result: cosine {agreement}"
        );
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
            let plain = embedder_or_skip!(OnnxOptions {
                dims,
                layer_norm: false,
                ..Default::default()
            });
            let normed = embedder_or_skip!(OnnxOptions {
                dims,
                layer_norm: true,
                ..Default::default()
            });
            let a = plain.embed_query("offer eligibility").unwrap();
            let b = normed.embed_query("offer eligibility").unwrap();
            let agreement = dot(&a, &b);
            assert!(
                agreement > 0.999,
                "at {dims} dimensions layer_norm moved the vector more than expected: cosine {agreement}"
            );
        }
    }

    /// The prefixes come from the manifest, and a manifest that names the wrong
    /// one produces a different vector for the same text.
    ///
    /// This is the generalisation of `the_query_prefix_and_the_document_prefix_differ`
    /// and it is the check that matters once there are eight models: the prefix
    /// is no longer a constant anybody can read off this file, so the only thing
    /// standing between an arm and another arm's prefix is that the manifest
    /// decides and the manifest is digested into the cache header.
    #[test]
    fn a_manifest_naming_the_wrong_query_prefix_moves_the_vector() {
        let baseline = ModelManifest::nomic_v1_5();
        let right = embedder_or_skip!(OnnxOptions::for_model(&baseline));
        let wrong = embedder_or_skip!(OnnxOptions::for_model(&ModelManifest {
            prefixes: Prefixes::query_only("query: "),
            ..baseline.clone()
        }));
        let text = "how does offer eligibility work";
        let a = right.embed_query(text).unwrap();
        let b = wrong.embed_query(text).unwrap();
        let agreement = dot(&a, &b);
        assert!(
            agreement < 0.999,
            "the wrong query prefix produced an all but identical vector (cosine {agreement}); \
             if a prefix cannot be detected here it cannot be detected anywhere"
        );
    }

    /// A model asked for no prefix at all is a third case, and it must differ
    /// from both of the above. Several of the 2025-26 encoders are trained this
    /// way, and applying a prefix to one of them is measurably worse.
    #[test]
    fn a_manifest_asking_for_no_prefix_differs_from_one_that_asks_for_nomics() {
        let baseline = ModelManifest::nomic_v1_5();
        let prefixed = embedder_or_skip!(OnnxOptions::for_model(&baseline));
        let bare = embedder_or_skip!(OnnxOptions::for_model(&ModelManifest {
            prefixes: Prefixes::none(),
            ..baseline.clone()
        }));
        let text = "offer eligibility rules";
        let agreement = dot(
            &prefixed.embed_query(text).unwrap(),
            &bare.embed_query(text).unwrap(),
        );
        assert!(agreement < 0.999, "cosine {agreement}");
    }

    /// Truncation is counted, not assumed, and a text past the bound is counted.
    ///
    /// The card prints a truncation share per arm, and a share that was always
    /// zero would read as "no model truncated anything" rather than as "nobody
    /// counted". This is the fixture that says the counter works.
    #[test]
    fn a_text_past_the_token_bound_is_counted_as_truncated() {
        let manifest = ModelManifest {
            max_tokens: 128,
            ..ModelManifest::nomic_v1_5()
        };
        let e = embedder_or_skip!(OnnxOptions::for_model(&manifest));
        // Distinct words, so the tokenizer cannot collapse them: about 3,000
        // tokens against a 128 bound.
        let long: String = (0..3000)
            .map(|i| format!("token{i} "))
            .collect::<Vec<_>>()
            .concat();
        e.embed_documents(&["short".to_string(), long]).unwrap();
        let facts = e.truncation();
        assert_eq!(facts.texts, 2);
        assert_eq!(
            facts.truncated, 1,
            "the long text was not counted as truncated"
        );
        assert!((facts.share() - 0.5).abs() < 1e-9);
        // Tokens are counted after truncation, which is what the model was
        // charged for: 128 for the long one plus a handful for the short one.
        assert!(
            facts.tokens > 128 && facts.tokens < 160,
            "{} tokens",
            facts.tokens
        );
    }

    /// A truncated text is embedded from its prefix rather than dropped, and the
    /// vector is still a unit vector of the right width.
    #[test]
    fn a_truncated_text_is_still_embedded() {
        let manifest = ModelManifest {
            max_tokens: 64,
            ..ModelManifest::nomic_v1_5()
        };
        let e = embedder_or_skip!(OnnxOptions::for_model(&manifest));
        let long: String = (0..2000)
            .map(|i| format!("token{i} "))
            .collect::<Vec<_>>()
            .concat();
        let v = e.embed_documents(&[long]).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].len(), 768);
        assert!((dot(&v[0], &v[0]) - 1.0).abs() < 1e-4);
    }

    /// Classification pooling reads a different position than mean pooling, so
    /// the two must not produce the same vector. Without this, a manifest that
    /// asked for the wrong pooling would be silently ignored and the arm would be
    /// graded as a model it is not.
    #[test]
    fn cls_pooling_and_mean_pooling_are_not_the_same_vector() {
        let baseline = ModelManifest::nomic_v1_5();
        let mean = embedder_or_skip!(OnnxOptions::for_model(&baseline));
        let cls = embedder_or_skip!(OnnxOptions::for_model(&ModelManifest {
            pooling: Pooling::Cls,
            ..baseline.clone()
        }));
        let last = embedder_or_skip!(OnnxOptions::for_model(&ModelManifest {
            pooling: Pooling::LastToken,
            ..baseline.clone()
        }));
        let text = "offer eligibility is evaluated against the member profile";
        let m = mean.embed_documents(&[text.to_string()]).unwrap().remove(0);
        let c = cls.embed_documents(&[text.to_string()]).unwrap().remove(0);
        let l = last.embed_documents(&[text.to_string()]).unwrap().remove(0);
        assert!(
            dot(&m, &c) < 0.999,
            "mean and cls agreed to {}",
            dot(&m, &c)
        );
        assert!(
            dot(&c, &l) < 0.999,
            "cls and last-token agreed to {}",
            dot(&c, &l)
        );
        for v in [&m, &c, &l] {
            assert!((dot(v, v) - 1.0).abs() < 1e-4);
        }
    }

    /// Last-token pooling has to find the last position the mask admits, not the
    /// last column of the padded tensor, or every short text in a batch with a
    /// long one would be pooled from padding.
    #[test]
    fn last_token_pooling_ignores_the_padding_a_longer_text_added() {
        let e = embedder_or_skip!(OnnxOptions::for_model(&ModelManifest {
            pooling: Pooling::LastToken,
            ..ModelManifest::nomic_v1_5()
        }));
        let short = "offer".to_string();
        let long = "offer eligibility rules ".repeat(60);
        let together = e.embed_documents(&[short.clone(), long]).unwrap();
        let alone = e.embed_documents(&[short]).unwrap();
        let agreement = dot(&together[0], &alone[0]);
        assert!(
            agreement > 0.9999,
            "padding leaked into the last token: cosine {agreement}"
        );
    }

    /// A tokenizer that pads or truncates for itself is disarmed on load.
    ///
    /// This is not hypothetical. `snowflake-arctic-embed-m-v2.0` ships
    /// `padding: BatchLongest` and `truncation: max_length 512` in its
    /// `tokenizer.json`, and the first smoke run of that arm reported exactly
    /// 512.0 tokens for every chunk in a corpus whose median chunk is about 240 -
    /// the tokenizer had padded every text to the batch maximum and cut anything
    /// past 512, and both the vectors and the truncation share were wrong.
    #[test]
    fn a_tokenizer_that_pads_and_truncates_for_itself_is_disarmed() {
        let Some(dir) = model_dir() else {
            inillucent_base::testing::skipping("no ONNX weights found");
            return;
        };
        let mut armed = match Tokenizer::from_file(dir.join("tokenizer.json")) {
            Ok(t) => t,
            Err(e) => {
                inillucent_base::testing::skipping(&format!("{e}"));
                return;
            }
        };
        armed.with_padding(Some(tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::Fixed(64),
            ..Default::default()
        }));
        armed
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 8,
                ..Default::default()
            }))
            .unwrap();
        let texts = vec!["offer eligibility rules for a member".to_string()];
        let before = armed.encode_batch(texts.clone(), true).unwrap();
        // Padded up to 64 and cut down to 8: the length says nothing true.
        assert_eq!(before[0].get_ids().len(), 64);

        disarm_tokenizer(&mut armed);
        let after = armed.encode_batch(texts, true).unwrap();
        assert!(
            after[0].get_ids().len() > 4 && after[0].get_ids().len() < 20,
            "after disarming, the length is the text's own: {}",
            after[0].get_ids().len()
        );
        assert!(after[0].get_attention_mask().iter().all(|m| *m == 1));
    }

    /// And the same through the whole embedder: a session opened on a tokenizer
    /// carrying its own bounds still reports the text's real token count, so the
    /// truncation share on the card describes the model rather than the tokenizer
    /// configuration it happened to ship with.
    #[test]
    fn the_embedder_reports_the_texts_own_token_count_not_a_padded_one() {
        let manifest = ModelManifest {
            max_tokens: 4096,
            ..ModelManifest::nomic_v1_5()
        };
        let e = embedder_or_skip!(OnnxOptions::for_model(&manifest));
        e.embed_documents(&["one two three four five six".to_string()])
            .unwrap();
        let facts = e.truncation();
        assert_eq!(facts.texts, 1);
        assert_eq!(facts.truncated, 0);
        assert!(
            facts.tokens > 3 && facts.tokens < 32,
            "a six word text tokenized to {} tokens, which is a padded count",
            facts.tokens
        );
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
        assert!(
            batches.len() > 1,
            "64 sequences of 1900 tokens must not be one batch"
        );
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
            assert!(
                widths.iter().all(|w| *w == widths[0]),
                "mixed lengths in one batch: {widths:?}"
            );
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
