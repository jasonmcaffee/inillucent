//! What an embedding model is, as data.
//!
//! The harness used to know one model. Its width, its four task prefixes, its
//! pooling and its truncation bound were constants, and the model's name was a
//! string literal in three report files. That is fine while there is one model
//! and it is exactly wrong the moment two of them are compared: a constant that
//! belongs to one arm cannot describe the other, and a comparison whose two
//! sides do not agree on what they are is the failure mode this repository has
//! measured five times.
//!
//! So a model is a manifest. Every property the harness needs in order to embed
//! text the way the model's author intended lives in one serialisable value that
//! travels beside the weights, gets written into the cache header, and is named
//! on every card. `ModelManifest::nomic_v1_5()` reproduces the constants this
//! file replaced, byte for byte, so the existing card is unchanged by the
//! generalisation.
//!
//! Nothing here hashes anything. The digest fields are carried as data, computed
//! by whoever has a hash function and a file to read, because this crate is a
//! leaf with no dependencies to spend on one.
//!
//! Invariant: **a model's width, prefixes, pooling and truncation bound travel
//! together as data.** They were constants and a string literal in three
//! report files, which is fine for one model and exactly wrong for two: a
//! constant that describes one of them describes the other incorrectly, and
//! nothing says so.

use serde::{Deserialize, Serialize};

/// The four task prefixes an instruction-tuned embedding model is trained with.
///
/// They are strings rather than a flag because every model spells them
/// differently and several use none at all. Omitting a model's prefixes, or
/// applying a prefix to a model trained without them, is measurably worse in
/// both directions, so the prefix is part of the model rather than part of the
/// caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prefixes {
    /// What a query is prefixed with.
    pub query: String,
    /// What a document is prefixed with.
    pub document: String,
    /// What text being clustered is prefixed with.
    pub clustering: String,
    /// What text being classified is prefixed with.
    pub classification: String,
}

impl Prefixes {
    /// What `nomic-embed-text-v1.5` is trained with. The trailing space is part
    /// of the prefix and is why these are written out rather than assembled.
    pub fn nomic() -> Prefixes {
        Prefixes {
            query: "search_query: ".to_string(),
            document: "search_document: ".to_string(),
            clustering: "clustering: ".to_string(),
            classification: "classification: ".to_string(),
        }
    }

    /// A model trained without prefixes. Not the same as a model whose prefixes
    /// are unknown: this says the author asked for none.
    pub fn none() -> Prefixes {
        Prefixes {
            query: String::new(),
            document: String::new(),
            clustering: String::new(),
            classification: String::new(),
        }
    }

    /// An asymmetric model that instructs only the query side, which is what most
    /// of the 2025-26 encoders do.
    /// @param query - the query instruction, including its trailing separator
    pub fn query_only(query: &str) -> Prefixes {
        Prefixes {
            query: query.to_string(),
            document: String::new(),
            clustering: String::new(),
            classification: String::new(),
        }
    }
}

/// How the token vectors a model outputs become one embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pooling {
    /// Average the token vectors, weighted by the attention mask so padding
    /// contributes nothing.
    Mean,
    /// The first position, which is the classification token for a BERT-family
    /// model and is what the GTE, Granite and Arctic encoders read.
    Cls,
    /// The last unmasked position, which is what a decoder-backbone embedder
    /// such as the Qwen3 family reads.
    LastToken,
}

/// What executes a model's weights.
///
/// A property of the model rather than of the run, because it is decided by what
/// the model's authors published: `nomic-embed-text-v2-moe` is a mixture of
/// experts with no ONNX export anywhere and a GGUF that `llama-server` serves,
/// and no command-line flag changes that. Where the server listens is a run
/// setting and lives elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// ONNX Runtime, in this process.
    Onnx,
    /// A `llama-server` over loopback HTTP.
    LlamaCpp,
}

/// Which of an ONNX export's outputs is the model's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// One vector per token, pooled here. The default, and the right choice for
    /// almost every model: pooling identically across arms is what makes two
    /// arms comparable, and an exporter that pooled its own way would be
    /// contributing a difference the card would attribute to the model.
    TokenEmbeddings,
    /// A vector the export already produced. Used only where the model's real
    /// contract includes layers that come *after* pooling and this module
    /// therefore cannot reproduce it - EmbeddingGemma's two dense projection
    /// heads are the case that forced this to exist. Recorded in the manifest so
    /// the card can say which arms were pooled here and which were not.
    SentenceEmbedding,
}

/// Everything the harness needs in order to run a model the way its author
/// intended, plus enough provenance to say which model a number came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelManifest {
    /// The name a card prints and a cache header stores. Also the directory name
    /// under the models root, so a header can find its own manifest.
    pub id: String,
    /// The width the model outputs, before any Matryoshka narrowing.
    pub dims: usize,
    /// The widths the model was trained to be truncated to. A model with no
    /// Matryoshka training lists only its full width.
    pub mrl_widths: Vec<usize>,
    /// The four task prefixes this model was trained with.
    pub prefixes: Prefixes,
    /// How the token vectors become one vector.
    pub pooling: Pooling,
    /// Longest sequence handed to the model. Chunks above it are truncated, and
    /// the card prints how many were, so a model that quietly saw less text than
    /// its rivals is visible rather than merely faster.
    pub max_tokens: usize,
    /// Layer-normalise the pooled vector before truncating. Documented by Nomic
    /// for Matryoshka use; the vectors already in PostgreSQL were produced
    /// without it, so it stays a recorded choice rather than a hidden one.
    #[serde(default)]
    pub layer_norm: bool,
    /// The ONNX file inside the model directory.
    #[serde(default = "default_model_file")]
    pub model_file: String,
    /// Whether the export takes a `token_type_ids` input. BERT-family exports do;
    /// most ModernBERT-class and decoder exports do not, and feeding one an input
    /// it never declared fails the session rather than being ignored.
    #[serde(default = "default_true")]
    pub token_type_ids: bool,
    /// What executes the weights.
    #[serde(default = "default_backend")]
    pub backend: Backend,
    /// Which output carries the model's answer. Almost always the per-token one.
    #[serde(default = "default_output")]
    pub output: Output,
    /// The exact ONNX output name to read, when the export does not use one of
    /// the names this harness looks for. Empty means "look for the usual names".
    #[serde(default)]
    pub output_name: String,
    /// SHA-256 of `tokenizer.json`, lowercase hex. A tokenizer change is a model
    /// change and is otherwise invisible.
    #[serde(default)]
    pub tokenizer_sha256: String,
    /// SHA-256 of the weights file named by `model_file`, lowercase hex.
    #[serde(default)]
    pub weights_sha256: String,
    /// The training-code revision, for a model we trained. Absent for one we
    /// downloaded.
    #[serde(default)]
    pub recipe_git_sha: Option<String>,
    /// Where the weights came from, so a download can be repeated or deleted.
    #[serde(default)]
    pub source: Option<String>,
}

fn default_model_file() -> String {
    "model.onnx".to_string()
}

fn default_true() -> bool {
    true
}

fn default_output() -> Output {
    Output::TokenEmbeddings
}

fn default_backend() -> Backend {
    Backend::Onnx
}

impl ModelManifest {
    /// The baseline, spelled out. Every field here reproduces a constant this
    /// module replaced, so an arm configured from this manifest embeds exactly
    /// what the harness embedded before there were manifests.
    pub fn nomic_v1_5() -> ModelManifest {
        ModelManifest {
            id: "nomic-embed-text-v1.5".to_string(),
            dims: 768,
            mrl_widths: vec![64, 128, 256, 512, 768],
            prefixes: Prefixes::nomic(),
            pooling: Pooling::Mean,
            max_tokens: 1900,
            layer_norm: false,
            model_file: default_model_file(),
            token_type_ids: true,
            backend: Backend::Onnx,
            output: Output::TokenEmbeddings,
            output_name: String::new(),
            tokenizer_sha256: String::new(),
            weights_sha256: String::new(),
            recipe_git_sha: None,
            source: Some("https://huggingface.co/nomic-ai/nomic-embed-text-v1.5".to_string()),
        }
    }

    /// The name of the manifest file inside a model directory.
    pub const FILE: &'static str = "model.json";

    /// Reads a model directory's manifest.
    ///
    /// One reader, because a manifest read two ways is a manifest that two
    /// callers can disagree about - and the disagreement would be about
    /// prefixes and pooling, which decide what the vectors mean rather than how
    /// fast they arrive.
    ///
    /// @param dir - the model directory
    pub fn read(dir: &std::path::Path) -> Result<ModelManifest, String> {
        let path = dir.join(Self::FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("reading {}: {error}", path.display()))?;
        serde_json::from_str(&text).map_err(|error| format!("parsing {}: {error}", path.display()))
    }

    /// Writes this manifest into a model directory.
    ///
    /// @param dir - the model directory
    pub fn write(&self, dir: &std::path::Path) -> Result<(), String> {
        let path = dir.join(Self::FILE);
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| format!("serializing the manifest: {error}"))?;
        std::fs::write(&path, text).map_err(|error| format!("writing {}: {error}", path.display()))
    }

    /// Apply the document prefix.
    pub fn document_prefix(&self, text: &str) -> String {
        format!("{}{}", self.prefixes.document, text)
    }

    /// Apply the query prefix.
    pub fn query_prefix(&self, text: &str) -> String {
        format!("{}{}", self.prefixes.query, text)
    }

    /// The widths a Matryoshka lane should report, always ascending and always
    /// ending at the model's full width even when the manifest forgot to say so.
    pub fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self
            .mrl_widths
            .iter()
            .copied()
            .filter(|w| *w > 0 && *w <= self.dims)
            .collect();
        if !widths.contains(&self.dims) {
            widths.push(self.dims);
        }
        widths.sort_unstable();
        widths.dedup();
        widths
    }

    /// The bytes a digest of this manifest is taken over.
    ///
    /// Not the file's bytes, deliberately. A manifest that round-trips through an
    /// editor on Windows comes back with different bytes and identical meaning,
    /// and a digest that moves under that is the line-ending failure this
    /// repository has already paid for once. Every field is written with its
    /// length in front so no two different manifests can produce the same
    /// sequence, and the weights and tokenizer digests are included because a
    /// manifest that names different weights is a different manifest.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut field = |value: &str| {
                out.extend_from_slice(&(value.len() as u64).to_le_bytes());
                out.extend_from_slice(value.as_bytes());
            };
            field(&self.id);
            field(&self.dims.to_string());
            field(
                &self
                    .mrl_widths
                    .iter()
                    .map(|w| w.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
            field(&self.prefixes.query);
            field(&self.prefixes.document);
            field(&self.prefixes.clustering);
            field(&self.prefixes.classification);
            field(match self.pooling {
                Pooling::Mean => "mean",
                Pooling::Cls => "cls",
                Pooling::LastToken => "last_token",
            });
            field(&self.max_tokens.to_string());
            field(&self.layer_norm.to_string());
            field(&self.model_file);
            field(&self.token_type_ids.to_string());
            field(match self.backend {
                Backend::Onnx => "onnx",
                Backend::LlamaCpp => "llama_cpp",
            });
            field(match self.output {
                Output::TokenEmbeddings => "token_embeddings",
                Output::SentenceEmbedding => "sentence_embedding",
            });
            field(&self.output_name);
            field(&self.tokenizer_sha256);
            field(&self.weights_sha256);
            field(self.recipe_git_sha.as_deref().unwrap_or(""));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_baseline_manifest_reproduces_the_constants_it_replaced() {
        let m = ModelManifest::nomic_v1_5();
        assert_eq!(m.dims, 768);
        assert_eq!(m.max_tokens, 1900);
        assert_eq!(m.document_prefix("hello"), "search_document: hello");
        assert_eq!(m.query_prefix("hello"), "search_query: hello");
        assert_eq!(m.widths(), vec![64, 128, 256, 512, 768]);
        assert_eq!(m.pooling, Pooling::Mean);
        assert!(!m.layer_norm);
    }

    #[test]
    fn a_manifest_round_trips_through_json() {
        let m = ModelManifest::nomic_v1_5();
        let text = serde_json::to_string_pretty(&m).unwrap();
        let back: ModelManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn the_canonical_digest_separates_manifests_a_naive_join_would_confuse() {
        // "ab" then "c" and "a" then "bc" are the same bytes without the lengths.
        let mut left = ModelManifest::nomic_v1_5();
        left.id = "ab".into();
        left.model_file = "c".into();
        let mut right = ModelManifest::nomic_v1_5();
        right.id = "a".into();
        right.model_file = "bc".into();
        assert_ne!(left.canonical_bytes(), right.canonical_bytes());
    }

    #[test]
    fn changing_one_prefix_changes_the_canonical_digest() {
        let base = ModelManifest::nomic_v1_5();
        let mut changed = base.clone();
        changed.prefixes.query = "query: ".into();
        assert_ne!(base.canonical_bytes(), changed.canonical_bytes());
    }

    #[test]
    fn a_manifest_that_forgot_its_own_width_still_reports_it() {
        let mut m = ModelManifest::nomic_v1_5();
        m.mrl_widths = vec![256, 128];
        assert_eq!(m.widths(), vec![128, 256, 768]);
    }

    #[test]
    fn widths_above_the_model_width_are_not_reported() {
        let mut m = ModelManifest::nomic_v1_5();
        m.dims = 256;
        m.mrl_widths = vec![64, 128, 256, 512, 768];
        assert_eq!(m.widths(), vec![64, 128, 256]);
    }
}
