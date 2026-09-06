//! An embedding arm served by `llama-server` rather than by ONNX Runtime.
//!
//! `nomic-embed-text-v2-moe` is one of the two models the trained model has to
//! beat, and it is a mixture of experts with no ONNX export anywhere. It does
//! have a GGUF, and Nikaya already serves it that way, so this drives a second
//! `llama-server` behind the same interface every other arm uses. That also makes
//! the Phase 6 serving trial a re-use of this code rather than a second one.
//!
//! Two things about this path are not negotiable, and both are written down
//! because this machine has already been measured wrong by each of them.
//!
//! **Batch by tokens.** `llama-server` fails a whole request when the batch
//! exceeds its physical batch size, and it fails it with a 500 rather than by
//! processing what it can. Sixty-four real chunks from this corpus measured
//! 17,029 tokens against an 8,192 physical batch, so a batch sized by *count*
//! fails on real text and succeeds on the short fixture somebody tested it with.
//! Every batch here is closed on a token budget, counted with the model's own
//! tokenizer through the server's own `/tokenize`.
//!
//! **Time on distinct inputs.** A shared prefix collapses in the server's prompt
//! cache. A repeated-text benchmark on this box reported 300 chunks a second
//! against a real 85 to 134. Nothing here repeats a text, and the cost lane feeds
//! it stride-sampled chunks from across the corpus.

use std::time::Duration;

use std::path::Path;

use anyhow::{Context, Result};
use inillucent_core::distance::normalize;
use inillucent_core::embed::Embedder;
use inillucent_core::embed_onnx::TruncationFacts;
use inillucent_core::model::ModelManifest;
use tokenizers::Tokenizer;

use crate::http;

/// How long one request may take. An embedding batch of several thousand tokens
/// on a card that is also serving something else is slow; a minute is generous
/// and still far short of "wait out a dead server", which is the failure this
/// bound exists to avoid.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// A `llama-server` speaking the OpenAI embeddings shape.
pub struct LlamaCppEmbedder {
    host: String,
    port: u16,
    manifest: ModelManifest,
    /// Tokens in one request. Sized from the server's own physical batch, and the
    /// reason this is a token budget rather than a count is in the module note.
    token_budget: usize,
    /// Texts in one request, whatever the token budget allows. `llama-server`
    /// also bounds the number of sequences in a batch.
    max_texts: usize,
    /// The model's own tokenizer, loaded from the model directory.
    ///
    /// Not the server's `/tokenize`, after measuring what that costs: the corpus
    /// is 185,078 chunks and `/tokenize` takes one text per call, so counting
    /// through it is 185,078 HTTP round trips before a single vector is produced.
    /// The two are checked against each other once at connect time on a sample,
    /// which turns "the local tokenizer surely agrees" from an assumption into a
    /// measurement without paying for it per chunk.
    tokenizer: Tokenizer,
    seen: std::sync::atomic::AtomicUsize,
    truncated: std::sync::atomic::AtomicUsize,
    tokens: std::sync::atomic::AtomicUsize,
}

impl LlamaCppEmbedder {
    /// @param dir - the model directory, holding `tokenizer.json`
    /// @param host - where the server listens
    /// @param port - the port
    /// @param manifest - the model, for its prefixes, width and token bound
    /// @param token_budget - tokens per request, at or below the server's `-b`
    /// @param max_texts - texts per request, at or below the server's `-np`
    pub fn connect(
        dir: &Path,
        host: &str,
        port: u16,
        manifest: &ModelManifest,
        token_budget: usize,
        max_texts: usize,
    ) -> Result<LlamaCppEmbedder> {
        let health = http::get(host, port, "/health", Duration::from_secs(10))
            .with_context(|| format!("no llama-server answering on {host}:{port}"))?;
        anyhow::ensure!(
            health.contains(" 200 "),
            "the server on {host}:{port} is not ready: {}",
            health.lines().next().unwrap_or("").trim()
        );
        let tokenizer_path = dir.join("tokenizer.json");
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading {}: {e}", tokenizer_path.display()))?;
        // Same reason the ONNX path disarms its tokenizer: a `tokenizer.json`
        // that pads or truncates for itself reports a length that is not the
        // text's, and the truncation share on the card would describe the
        // tokenizer's configuration rather than the model.
        tokenizer.with_padding(None);
        let _ = tokenizer.with_truncation(None);

        let embedder = LlamaCppEmbedder {
            host: host.to_string(),
            port,
            manifest: manifest.clone(),
            token_budget,
            max_texts,
            tokenizer,
            seen: std::sync::atomic::AtomicUsize::new(0),
            truncated: std::sync::atomic::AtomicUsize::new(0),
            tokens: std::sync::atomic::AtomicUsize::new(0),
        };
        embedder.check_tokenizer_agrees_with_the_server()?;
        // One real round trip before anything is embedded, so a server that is
        // up but not serving embeddings fails here rather than at chunk 40,000.
        let probe = embedder.embed_prefixed(&["a probe".to_string()])?;
        anyhow::ensure!(
            probe.len() == 1 && probe[0].len() == manifest.dims,
            "the server returned {} dimensions and {}'s manifest declares {}",
            probe.first().map(|v| v.len()).unwrap_or(0),
            manifest.id,
            manifest.dims
        );
        embedder.reset_counts();
        Ok(embedder)
    }

    /// Check the local tokenizer counts what the server counts, once.
    ///
    /// The batch budget only protects anything if it is denominated in the units
    /// the server will use. A local tokenizer that is a few per cent short means
    /// a batch that overflows the physical batch on one request in fifty, and
    /// `llama-server` answers that with a 500 for the whole batch rather than a
    /// partial result - so the failure would arrive as a handful of missing
    /// chunks with no pattern to them.
    ///
    /// Short, varied texts including the punctuation and identifiers the corpus
    /// actually carries, because a tokenizer disagreement on plain prose is the
    /// least likely kind.
    fn check_tokenizer_agrees_with_the_server(&self) -> Result<()> {
        let samples = [
            "search_document: offer eligibility is evaluated against the member profile",
            "search_document: fn compute_offer_eligibility(member_id: u64) -> Result<Vec<Offer>>",
            "search_document: PROJ-4821 blocked on the redemption service returning 502s",
            "search_document: a much longer passage, repeated a few times over, so the              comparison covers a sequence long enough for a tokenizer to disagree about              where its pieces begin and end, and not only a single short sentence",
        ];
        for text in samples {
            let local = self
                .tokenizer
                .encode(text, true)
                .map_err(|e| anyhow::anyhow!("tokenizing locally: {e}"))?
                .get_ids()
                .len();
            let body = serde_json::json!({ "content": text }).to_string();
            let reply =
                http::post_json(&self.host, self.port, "/tokenize", &body, REQUEST_TIMEOUT)?;
            let parsed: serde_json::Value = serde_json::from_str(&reply)
                .with_context(|| format!("parsing /tokenize: {}", &reply[..reply.len().min(200)]))?;
            let served = parsed["tokens"]
                .as_array()
                .context("the /tokenize reply carried no tokens array")?
                .len();
            // Exact agreement is not required and would be brittle: llama.cpp may
            // add or omit a bracketing special token. A drift larger than that is
            // a different tokenizer, and a budget counted in the wrong units.
            let drift = (local as i64 - served as i64).abs();
            anyhow::ensure!(
                drift <= 2,
                "the tokenizer in the model directory counts {local} tokens where the server                  counts {served} for the same text. The batch budget is denominated in the                  server's tokens, so a disagreement this size overflows its physical batch on                  some request and fails the whole batch"
            );
        }
        Ok(())
    }

    fn reset_counts(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.seen.store(0, Relaxed);
        self.truncated.store(0, Relaxed);
        self.tokens.store(0, Relaxed);
    }

    pub fn truncation(&self) -> TruncationFacts {
        use std::sync::atomic::Ordering::Relaxed;
        TruncationFacts {
            texts: self.seen.load(Relaxed),
            truncated: self.truncated.load(Relaxed),
            tokens: self.tokens.load(Relaxed),
        }
    }

    pub fn manifest(&self) -> &ModelManifest {
        &self.manifest
    }

    /// Token counts from the model's own tokenizer, checked once against the
    /// server's at connect time.
    /// @param texts - already prefixed
    fn token_counts(&self, texts: &[String]) -> Result<Vec<usize>> {
        let encoded = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenizing: {e}"))?;
        // A margin on top of the count, because the check at connect time allows
        // a couple of tokens of drift and a budget that is exactly right is a
        // budget that is sometimes two tokens wrong.
        Ok(encoded.iter().map(|e| e.get_ids().len() + 2).collect())
    }

    /// Cut a text to at most `max_tokens`, at a token boundary, and prove it.
    ///
    /// The obvious cut - keep `len * bound / count` characters - is an estimate,
    /// and it is wrong exactly where it matters. Tokens are not uniform over
    /// characters: a passage whose tail is code or identifiers packs more tokens
    /// per character than its head, so a proportional cut leaves the tail over the
    /// bound. That is not hypothetical. It failed here at chunk 78,592 with
    /// `input (526 tokens) is larger than the max context size (512 tokens)`,
    /// after a proportional cut that was supposed to have brought it under 512.
    ///
    /// So the cut is made in tokens: keep the first `bound - MARGIN` ids and turn
    /// them back into text. The margin exists because the server's tokenizer is
    /// allowed to disagree with this one by a token or two, and the result is then
    /// re-encoded and checked rather than assumed - a decode and re-encode need
    /// not round-trip to the same length, and "need not" is how the first version
    /// of this was wrong.
    /// @param text - the prefixed text, known to be over the bound
    fn truncate_to_bound(&self, text: &str) -> Result<String> {
        /// Tokens held back, so a server that counts one or two more than this
        /// tokenizer does still has room.
        const MARGIN: usize = 8;
        let bound = self.manifest.max_tokens;
        let mut keep = bound.saturating_sub(MARGIN).max(1);
        let mut cut = text.to_string();
        for _ in 0..5 {
            let encoded = self
                .tokenizer
                .encode(cut.as_str(), true)
                .map_err(|e| anyhow::anyhow!("tokenizing to truncate: {e}"))?;
            if encoded.get_ids().len() <= bound {
                return Ok(cut);
            }
            let ids: Vec<u32> = encoded.get_ids().iter().copied().take(keep).collect();
            cut = self
                .tokenizer
                .decode(&ids, true)
                .map_err(|e| anyhow::anyhow!("decoding a truncated text: {e}"))?;
            keep = keep.saturating_sub(16).max(1);
        }
        // Five rounds of taking sixteen more tokens off and it still does not fit
        // is not a long text, it is a broken tokenizer round-trip, and sending it
        // would fail the whole batch on the server with a message about context
        // size that says nothing about the real cause.
        anyhow::bail!(
            "could not cut a text to {bound} tokens for {}: decoding and re-encoding does not \
             converge, which means this tokenizer does not round-trip",
            self.manifest.id
        )
    }

    /// Embed already prefixed texts, in requests bounded by tokens.
    pub fn embed_prefixed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let counts = self.token_counts(texts)?;
        let bound = self.manifest.max_tokens;

        // Truncation, done here rather than left to the server. The server
        // silently drops what does not fit, and a share reported as zero because
        // nobody counted is the failure this whole harness is built against.
        let mut prepared: Vec<String> = Vec::with_capacity(texts.len());
        let mut over: Vec<usize> = Vec::new();
        {
            use std::sync::atomic::Ordering::Relaxed;
            for (text, &n) in texts.iter().zip(&counts) {
                self.seen.fetch_add(1, Relaxed);
                self.tokens.fetch_add(n.min(bound), Relaxed);
                if n > bound {
                    self.truncated.fetch_add(1, Relaxed);
                    over.push(prepared.len());
                    prepared.push(String::new());
                } else {
                    prepared.push(text.clone());
                }
            }
        }

        // The over-long ones, cut in tokens and checked. Done after the counting
        // pass so the common case - every text under the bound - pays nothing.
        for &i in &over {
            prepared[i] = self.truncate_to_bound(&texts[i])?;
        }

        let mut out: Vec<Vec<f32>> = Vec::with_capacity(prepared.len());
        let mut start = 0usize;
        while start < prepared.len() {
            let mut end = start;
            let mut budget = 0usize;
            while end < prepared.len() {
                let cost = counts[end].min(bound).max(1);
                // A single text over the whole budget still goes on its own: the
                // alternative is dropping a chunk from the corpus.
                if end > start && (budget + cost > self.token_budget || end - start >= self.max_texts)
                {
                    break;
                }
                budget += cost;
                end += 1;
            }
            out.extend(self.request(&prepared[start..end])?);
            start = end;
        }
        anyhow::ensure!(
            out.len() == texts.len(),
            "the server returned {} vectors for {} texts",
            out.len(),
            texts.len()
        );
        Ok(out)
    }

    /// One `/v1/embeddings` request.
    fn request(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({ "input": texts }).to_string();
        let reply = http::post_json(&self.host, self.port, "/v1/embeddings", &body, REQUEST_TIMEOUT)
            .with_context(|| format!("embedding {} texts", texts.len()))?;
        let parsed: serde_json::Value = serde_json::from_str(&reply)
            .with_context(|| format!("parsing the reply: {}", &reply[..reply.len().min(300)]))?;
        let data = parsed["data"]
            .as_array()
            .context("the reply carried no data array")?;
        anyhow::ensure!(
            data.len() == texts.len(),
            "asked for {} embeddings and got {}",
            texts.len(),
            data.len()
        );
        // Returned in request order by index, but the index is checked rather
        // than assumed: a reordered reply would pair every vector with the wrong
        // chunk, which is exactly the failure `embed-check` exists to catch and
        // which is cheaper to prevent here.
        let mut out = vec![Vec::new(); texts.len()];
        for row in data {
            let index = row["index"].as_u64().context("a row carried no index")? as usize;
            anyhow::ensure!(index < texts.len(), "the reply indexed row {index} of {}", texts.len());
            let values = row["embedding"]
                .as_array()
                .context("a row carried no embedding")?;
            let mut v: Vec<f32> = values
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect();
            anyhow::ensure!(
                v.len() == self.manifest.dims,
                "the server returned {} dimensions and the manifest declares {}",
                v.len(),
                self.manifest.dims
            );
            // llama.cpp does not always normalise, and every scenario here
            // computes cosine as a dot product.
            normalize(&mut v);
            out[index] = v;
        }
        anyhow::ensure!(
            out.iter().all(|v| !v.is_empty()),
            "the reply skipped a row: some index appeared twice"
        );
        Ok(out)
    }

}

impl Embedder for LlamaCppEmbedder {
    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let prefix = &self.manifest.prefixes.document;
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        self.embed_prefixed(&prefixed)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let prefixed = vec![format!("{}{text}", self.manifest.prefixes.query)];
        self.embed_prefixed(&prefixed)?
            .into_iter()
            .next()
            .context("the server returned no embedding")
    }

    fn dimensions(&self) -> usize {
        self.manifest.dims
    }
}
