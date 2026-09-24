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
    /// `host:port` for a llama.cpp arm that has no entry in `endpoint_overrides`.
    pub endpoint: String,
    /// `host:port` for one named model, when several GGUF arms are compared at once.
    ///
    /// Gate C1 times the student's q8_0 against `nomic-embed-text-v2-moe`'s f16 **and**
    /// its q8_0 through the same llama.cpp build, which is three served arms on one
    /// card. A single global endpoint cannot express that: each GGUF needs its own
    /// `llama-server`, because a server holds one model. Like `max_batch_cells` this
    /// is a property of the machine and not of the model, so it lives here and not in
    /// the manifest, and setting it moves no manifest digest and invalidates no cache.
    pub endpoint_overrides: std::collections::BTreeMap<String, String>,
    /// Tokens per request for a llama.cpp arm, at or below the server's `-b`.
    pub token_budget: usize,
    /// Texts per request for a llama.cpp arm, at or below the server's `-np`.
    pub max_texts: usize,
    /// Requests a llama.cpp arm keeps in flight at once.
    ///
    /// One by default, and deliberately: a served model's throughput depends on how
    /// many requests are in flight far more than on the model, so the cost lane holds
    /// it at one and its numbers stay comparable between arms and with what task-1818
    /// recorded. Embedding a corpus asks for more, because there the round trip is
    /// waste rather than measurement - v2-moe's corpus embed ran at 64.6 chunks a
    /// second falling to 12 with the card at 6 per cent, entirely on latency.
    pub concurrency: usize,
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
            endpoint_overrides: std::collections::BTreeMap::new(),
            // Well under the 8,192 physical batch a `llama-server` is started
            // with here. Sixty-four real chunks from this corpus measured 17,029
            // tokens, so a batch sized by count rather than by tokens fails on
            // real text and passes on whatever short fixture it was tested with.
            token_budget: 6_000,
            max_texts: 32,
            concurrency: 1,
            max_batch_cells: 24_000_000,
        }
    }
}

impl ArmOptions {
    /// Where this model's `llama-server` listens.
    ///
    /// The model's own override when it has one, and the shared endpoint otherwise, so a
    /// card with one GGUF arm needs no override at all and a card with three names each.
    /// @param model_id - the model's id, as its manifest declares it
    pub fn endpoint_for(&self, model_id: &str) -> &str {
        self.endpoint_overrides
            .get(model_id)
            .map_or(self.endpoint.as_str(), |e| e.as_str())
    }
}

impl Arm {
    /// Open a model, on whichever backend its manifest declares.
    /// @param model - the resolved model and its manifest
    /// @param options - the machine settings
    pub fn open(model: &ResolvedModel, options: &ArmOptions) -> Result<Arm> {
        anyhow::ensure!(
            model.manifest.runnable,
            "{}'s manifest says this harness does not run it (`runnable: false`): its vectors              come from outside, through cache-from-vectors",
            model.manifest.id
        );
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
                let (host, port) = split_endpoint(options.endpoint_for(&model.manifest.id))?;
                Ok(Arm::Llama(Box::new(LlamaCppEmbedder::connect(
                    &model.dir,
                    &host,
                    port,
                    &model.manifest,
                    options.token_budget,
                    options.max_texts,
                    options.concurrency,
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

    /// Whether this arm's vectors travel over a socket to a `llama-server`.
    ///
    /// Asked by `embed-check`, which has one thing to say about a disagreement that
    /// is only true of a served arm: `llama-server` picks a slot by longest common
    /// prefix once every slot has held a prompt, and reuses that slot's cached keys
    /// and values for the matching tokens rather than recomputing them. Every text a
    /// corpus is embedded from shares the model's document prefix, so from the second
    /// pass against one server onward every request reuses something, and the reused
    /// values were computed in a different batch. An in-process ONNX graph has no
    /// slots and no cache, so the same sentence printed under it would send its reader
    /// after a cause that cannot apply.
    pub fn is_served(&self) -> bool {
        matches!(self, Arm::Llama(_))
    }

    /// How much of the text handed to this arm the model actually saw.
    pub fn truncation(&self) -> TruncationFacts {
        match self {
            Arm::Onnx(e) => e.truncation(),
            Arm::Llama(e) => e.truncation(),
        }
    }

    /// A label for progress output, so a log says which backend produced a rate.
    ///
    /// The endpoint in the label is the one this arm actually connected to, not the
    /// shared default, so a card comparing three GGUF arms does not print the same
    /// endpoint against three different servers.
    /// @param options - the machine settings the arm was opened with
    /// @param model_id - the model's id, for its endpoint override
    pub fn backend_label(&self, options: &ArmOptions, model_id: &str) -> String {
        match self {
            Arm::Onnx(_) => options.device.label(),
            Arm::Llama(_) => format!("llama.cpp at {}", options.endpoint_for(model_id)),
        }
    }
}

/// Split `host:port`, refusing anything that is not one.
fn split_endpoint(endpoint: &str) -> Result<(String, u16)> {
    let (host, port) = endpoint
        .rsplit_once(':')
        .with_context(|| format!("{endpoint} is not host:port"))?;
    let port: u16 = port
        .parse()
        .with_context(|| format!("{port} is not a port"))?;
    anyhow::ensure!(!host.is_empty(), "{endpoint} names no host");
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_splits_into_a_host_and_a_port() {
        assert_eq!(
            split_endpoint("127.0.0.1:8189").unwrap(),
            ("127.0.0.1".into(), 8189)
        );
        assert_eq!(
            split_endpoint("localhost:1").unwrap(),
            ("localhost".into(), 1)
        );
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
        assert!(ArmOptions::default()
            .endpoint
            .ends_with(&DEFAULT_LLAMA_PORT.to_string()));
    }

    /// Gate C1(a) compares three GGUF arms - the student's q8_0 against v2-moe's f16
    /// and q8_0 - and a `llama-server` serves one model, so three servers on three
    /// ports have to be addressable from one card. Before this, every llama.cpp arm
    /// read the same global endpoint and the second and third arms would have been
    /// timed against the first one's model while the card reported three model ids.
    #[test]
    fn each_model_can_name_its_own_server() {
        let mut options = ArmOptions {
            endpoint: "127.0.0.1:8189".into(),
            ..Default::default()
        };
        options
            .endpoint_overrides
            .insert("v2moe-q8".into(), "127.0.0.1:8190".into());
        options
            .endpoint_overrides
            .insert("student-q8".into(), "127.0.0.1:8191".into());
        assert_eq!(
            options.endpoint_for("nomic-embed-text-v2-moe"),
            "127.0.0.1:8189"
        );
        assert_eq!(options.endpoint_for("v2moe-q8"), "127.0.0.1:8190");
        assert_eq!(options.endpoint_for("student-q8"), "127.0.0.1:8191");
    }

    /// An override is only useful if it is a real endpoint, and a typo in one would
    /// otherwise surface as a connection refused against a port nobody chose.
    #[test]
    fn an_overridden_endpoint_is_still_split_into_a_host_and_a_port() {
        let mut options = ArmOptions::default();
        options
            .endpoint_overrides
            .insert("student-q8".into(), "127.0.0.1:8191".into());
        assert_eq!(
            split_endpoint(options.endpoint_for("student-q8")).unwrap(),
            ("127.0.0.1".into(), 8191)
        );
        options
            .endpoint_overrides
            .insert("broken".into(), "127.0.0.1".into());
        assert!(split_endpoint(options.endpoint_for("broken")).is_err());
    }

    /// A served arm's throughput depends on how many requests are in flight far more
    /// than on the model: measured on this box against a 32 slot server, one request
    /// gives 28 texts a second and eight give 621. So the default is one, and a number
    /// the cost lane produces means the same thing between arms and against what
    /// task-1818 recorded. A caller that wants the other behaviour asks for it.
    #[test]
    fn a_served_arm_keeps_one_request_in_flight_unless_asked() {
        assert_eq!(ArmOptions::default().concurrency, 1);
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
