//! One model, however it happens to run.
//!
//! Seven of the eight arms are ONNX graphs this process loads. The eighth,
//! `nomic-embed-text-v2-moe`, is a mixture of experts with no ONNX export
//! anywhere and a GGUF that `llama-server` can serve. That is a difference in how
//! the weights are executed and it must not become a difference in how the arm is
//! measured, so both go behind one type and every call site takes that type.
//!
//! What is deliberately *not* hidden here is the truncation count. Each backend
//! counts it its own way - the ONNX side from its tokenizer, the llama.cpp side
//! from the server's - and both report it through the same `TruncationFacts`, so
//! the card's truncation column means one thing across every column.

use anyhow::{Context, Result};

use inillucent_core::embed::Embedder;
use inillucent_core::embed_onnx::{Device, OnnxEmbedder, TruncationFacts};
use inillucent_core::model::Backend;

use crate::llamacpp::LlamaCppEmbedder;
use crate::models::ResolvedModel;

/// Where a `llama-server` arm expects its server, when nothing says otherwise.
///
/// Not Nikaya's 8087. Nikaya serves the same model on that port and pointing a
/// 185,000-chunk embedding run at a production service is not a benchmark, it is
/// an outage with a score attached.
pub const DEFAULT_LLAMA_PORT: u16 = 8189;

pub enum Arm {
    Onnx(Box<OnnxEmbedder>),
    Llama(Box<LlamaCppEmbedder>),
}

/// Where and how hard to run an arm. The manifest says what the model is; this
/// says what the machine should do about it.
#[derive(Debug, Clone)]
pub struct ArmOptions {
    pub batch_size: usize,
    pub device: Device,
    /// `host:port` for a llama.cpp arm.
    pub endpoint: String,
    /// Tokens per request for a llama.cpp arm, at or below the server's `-b`.
    pub token_budget: usize,
    /// Texts per request for a llama.cpp arm, at or below the server's `-np`.
    pub max_texts: usize,
    /// Ceiling on `texts in an ONNX batch x longest sequence in it, squared`.
    ///
    /// A machine setting rather than a model property, which is why it is here
    /// and deliberately **not** in the manifest: putting it there would make the
    /// manifest digest move whenever somebody tuned a batch for a different card,
    /// and every cache on disk would stop being readable for a reason that has
    /// nothing to do with any model.
    ///
    /// It is a count of attention cells, and what a cell *costs* depends on the
    /// model's head count, which the budget does not know. The default was
    /// calibrated on a 12-head encoder, where it works out at about 2.8 GB per
    /// batch. `qwen3-embedding-0.6b` has 16 heads, where the same budget asks for
    /// 3.7 GB, and it failed twice on this card at exactly that: once on a single
    /// 5,327-token chunk and once on 29 texts of 895 tokens. Lower it for a
    /// model with more heads than the default assumes.
    pub max_batch_cells: usize,
}

impl Default for ArmOptions {
    fn default() -> Self {
        ArmOptions {
            batch_size: 16,
            device: Device::Cpu,
            endpoint: format!("127.0.0.1:{DEFAULT_LLAMA_PORT}"),
            // Well under the 8,192 physical batch a `llama-server` is started
            // with here. Sixty-four real chunks from this corpus measured 17,029
            // tokens, so a batch sized by count rather than by tokens fails on
            // real text and passes on whatever short fixture it was tested with.
            token_budget: 6_000,
            max_texts: 32,
            max_batch_cells: 24_000_000,
        }
    }
}

impl Arm {
    /// Open a model, on whichever backend its manifest declares.
    /// @param model - the resolved model and its manifest
    /// @param options - the machine settings
    pub fn open(model: &ResolvedModel, options: &ArmOptions) -> Result<Arm> {
        match model.manifest.backend {
            Backend::Onnx => {
                let onnx = inillucent_core::embed_onnx::OnnxOptions {
                    batch_size: options.batch_size,
                    device: options.device,
                    max_batch_cells: options.max_batch_cells,
                    ..inillucent_core::embed_onnx::OnnxOptions::for_model(&model.manifest)
                };
                Ok(Arm::Onnx(Box::new(OnnxEmbedder::open_model(
                    &model.dir,
                    &model.manifest.model_file,
                    onnx,
                )?)))
            }
            Backend::LlamaCpp => {
                let (host, port) = split_endpoint(&options.endpoint)?;
                Ok(Arm::Llama(Box::new(LlamaCppEmbedder::connect(
                    &model.dir,
                    &host,
                    port,
                    &model.manifest,
                    options.token_budget,
                    options.max_texts,
                )?)))
            }
        }
    }

    /// Embed corpus text, with this model's document prefix.
    pub fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        match self {
            Arm::Onnx(e) => e.embed_documents(texts),
            Arm::Llama(e) => e.embed_documents(texts),
        }
    }

    /// Embed queries, with this model's query prefix.
    pub fn embed_queries(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let prefix = &self.manifest_prefix_query();
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        match self {
            Arm::Onnx(e) => e.embed_prefixed(&prefixed),
            Arm::Llama(e) => e.embed_prefixed(&prefixed),
        }
    }

    fn manifest_prefix_query(&self) -> String {
        match self {
            Arm::Onnx(e) => e.options().prefixes.query.clone(),
            Arm::Llama(e) => e.manifest().prefixes.query.clone(),
        }
    }

    /// How much of the text handed to this arm the model actually saw.
    pub fn truncation(&self) -> TruncationFacts {
        match self {
            Arm::Onnx(e) => e.truncation(),
            Arm::Llama(e) => e.truncation(),
        }
    }

    /// A label for progress output, so a log says which backend produced a rate.
    pub fn backend_label(&self, options: &ArmOptions) -> String {
        match self {
            Arm::Onnx(_) => options.device.label(),
            Arm::Llama(_) => format!("llama.cpp at {}", options.endpoint),
        }
    }
}

/// Split `host:port`, refusing anything that is not one.
fn split_endpoint(endpoint: &str) -> Result<(String, u16)> {
    let (host, port) = endpoint
        .rsplit_once(':')
        .with_context(|| format!("{endpoint} is not host:port"))?;
    let port: u16 = port.parse().with_context(|| format!("{port} is not a port"))?;
    anyhow::ensure!(!host.is_empty(), "{endpoint} names no host");
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_splits_into_a_host_and_a_port() {
        assert_eq!(split_endpoint("127.0.0.1:8189").unwrap(), ("127.0.0.1".into(), 8189));
        assert_eq!(split_endpoint("localhost:1").unwrap(), ("localhost".into(), 1));
    }

    #[test]
    fn an_endpoint_without_a_port_is_refused() {
        assert!(split_endpoint("127.0.0.1").is_err());
        assert!(split_endpoint(":8189").is_err());
        assert!(split_endpoint("127.0.0.1:not-a-port").is_err());
    }

    /// The default must not be Nikaya's port. Pointing a corpus embedding run at
    /// the production mail service would be a benchmark that takes a service
    /// down, and a constant is exactly the kind of thing that gets copied.
    #[test]
    fn the_default_llama_port_is_not_nikayas() {
        assert_ne!(DEFAULT_LLAMA_PORT, 8087);
        assert!(ArmOptions::default().endpoint.ends_with(&DEFAULT_LLAMA_PORT.to_string()));
    }

    #[test]
    fn the_default_token_budget_leaves_room_under_an_eight_thousand_batch() {
        let options = ArmOptions::default();
        assert!(options.token_budget <= 8192);
        // Sixty-four real chunks measured 17,029 tokens, so a count-based batch
        // of 64 would have been more than twice the physical batch.
        assert!(options.max_texts * 266 > options.token_budget);
    }
}
