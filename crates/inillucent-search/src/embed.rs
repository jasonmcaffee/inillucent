//! `embed(TEXT)`: an embedding, computed inside the database.
//!
//! Invariant: **the model is loaded once and never guessed at.** A caller who
//! asks for an embedding gets one from the model this build was pointed at, or
//! a refusal naming the variable that points at it - never a vector of zeroes,
//! never a vector from a different model, and never a silent NULL. An
//! embedding whose provenance is unknown is worse than no embedding: it goes
//! into an index, and every neighbour it is ever compared against is wrong.
//!
//! ## Why it is here
//!
//! The embedder is `inillucent_core::embed_onnx`, and the SQL engine is below
//! the retrieval engine in this workspace's layering - `inillucent-engine` may
//! not reach it, and neither may the shell. This crate is the one place that
//! already sits on both sides: it is where `inillucent_search` is registered,
//! and it links the retrieval engine to do it. So `embed` goes here, beside the
//! module, and costs the layering nothing.
//!
//! ## Where the model comes from
//!
//! `INILLUCENT_ONNX_DIR`, when it is set, and otherwise the two directories the
//! retrieval engine's own tests look in. The rule is deliberately the same one:
//! a build that can run those tests can run this function, and a build that
//! cannot says so in the same words.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use inillucent_base::{error, DbResult};
use inillucent_core::embed_onnx::{OnnxEmbedder, OnnxOptions};
use inillucent_value::Value;

/// The environment variable naming the directory the weights are in.
const MODEL_DIR: &str = "INILLUCENT_ONNX_DIR";

/// The model this function embeds with.
const MODEL: &str = "nomic-embed-text-v1.5";

/// Where the weights are looked for when the variable is not set.
const ROOTS: [&str; 2] = [
    "J:/inillucent-embeddings/models",
    "~/.cache/inillucent-models",
];

/// The loaded embedder, and whether loading was even attempted.
///
/// A `Mutex` because the session is not `Sync` in the way a shared reference
/// would need, and because two statements embedding at once would otherwise
/// have to load it twice. The lock is held for one call to the model.
static EMBEDDER: OnceLock<Mutex<Option<OnnxEmbedder>>> = OnceLock::new();

/// Returns the directory the weights are in, when there is one.
fn model_dir() -> Option<PathBuf> {
    if let Some(named) = std::env::var(MODEL_DIR).ok().map(PathBuf::from) {
        return named.join("model.onnx").exists().then_some(named);
    }
    for root in ROOTS {
        let root = match root.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var("HOME").ok()?).join(rest),
            None => PathBuf::from(root),
        };
        let dir = root.join(MODEL);
        if dir.join("model.onnx").exists() && dir.join("tokenizer.json").exists() {
            return Some(dir);
        }
    }
    None
}

/// Returns one text's embedding as the bytes a `VECTOR(n)` column holds.
///
/// Little-endian 32-bit floats, which is the layout every vector in this engine
/// has - so the answer can be stored, indexed and compared without a
/// conversion.
///
/// @param arguments - the one text to embed
fn embed(arguments: &[Value<'static>]) -> DbResult<Value<'static>> {
    let text = match arguments.first() {
        // NULL in, NULL out, which is what every other scalar function does
        // with one: there is no text to embed, and a zero vector would be a
        // claim about a document that does not exist.
        None | Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Text(text)) => String::from_utf8_lossy(text.raw()).into_owned(),
        Some(Value::Blob(blob)) => String::from_utf8_lossy(blob.raw()).into_owned(),
        Some(Value::Integer(number)) => number.to_string(),
        Some(Value::Real(number)) => number.to_string(),
    };
    let held = EMBEDDER.get_or_init(|| {
        Mutex::new(
            model_dir().and_then(|dir| OnnxEmbedder::open(&dir, OnnxOptions::default()).ok()),
        )
    });
    let Ok(mut guard) = held.lock() else {
        return Err(error::misuse("embed: the embedder is poisoned"));
    };
    let Some(embedder) = guard.as_mut() else {
        return Err(error::misuse(format!(
            "embed: no embedding model is loaded; set {MODEL_DIR} to the directory holding {MODEL}"
        )));
    };
    let vectors = embedder
        .embed_prefixed(&[text])
        .map_err(|reason| error::misuse(format!("embed: {reason}")))?;
    let Some(vector) = vectors.first() else {
        return Err(error::misuse("embed: the model returned no vector"));
    };
    let mut bytes = Vec::with_capacity(vector.len().saturating_mul(4));
    for value in vector {
        bytes.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    Value::owned_blob(&bytes)
}

/// Adds `embed` to a registry.
///
/// @param registry - what a connection reaches functions through
pub fn register(registry: &mut inillucent_ext::registry::Registry) {
    registry.register_function(inillucent_ext::registry::UserFunction {
        name: "embed".to_string(),
        arity: 1,
        flags: inillucent_ext::registry::FunctionFlags::default(),
        body: inillucent_ext::registry::UserBody::Scalar(Arc::new(embed)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NULL text embeds to NULL rather than to a vector of zeroes.
    #[test]
    fn a_null_text_has_no_embedding() {
        assert!(matches!(embed(&[Value::Null]), Ok(Value::Null)));
        assert!(matches!(embed(&[]), Ok(Value::Null)));
    }

    /// The function is registered under the name SQL calls it by.
    #[test]
    fn registering_makes_the_function_findable() {
        let mut registry = inillucent_ext::registry::Registry::default();
        register(&mut registry);
        assert!(registry.function(b"embed", 1).is_some());
        assert!(registry.function(b"EMBED", 1).is_some());
    }
}
