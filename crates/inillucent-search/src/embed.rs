//! `embed(TEXT)`: an embedding, computed inside the database.
//!
//! Invariant: **the model is loaded once and never guessed at.** A caller who
//! asks for an embedding gets one from the model this build was pointed at, or
//! a refusal naming what is missing - never a vector of zeroes, never a vector
//! from a different model, and never a silent NULL. An embedding whose
//! provenance is unknown is worse than no embedding: it goes into an index, and
//! every neighbour it is ever compared against is wrong.
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
//! `inillucent_core::install::model_dir`, which is the same function
//! `inillucent setup-embeddings` writes into. So a person who has run that
//! command has a working `embed(TEXT)` with nothing exported by hand, and
//! `INILLUCENT_ONNX_DIR` stays as the override for a machine that keeps its
//! weights somewhere the installer would never have put them.
//!
//! ## When the model is in memory
//!
//! Whatever [`Residency::configured`] says, which is
//! `INILLUCENT_EMBED_RESIDENCY` first, then the profile the install recorded,
//! then `idle:5m`. It matters here more than anywhere else: a `.rdb` file gets
//! opened by processes that answer one question and exit, and holding 1.9 GB of
//! weights for the life of one of those is the failure
//! `docs/vector-residency.md` already describes for the vectors.

use std::sync::{Arc, OnceLock};

use inillucent_base::{error, DbResult};
use inillucent_core::embed_onnx::OnnxOptions;
use inillucent_core::install;
use inillucent_core::model::ModelManifest;
use inillucent_core::residency::{ManagedEmbedder, Residency};
use inillucent_value::Value;

/// The model this function embeds with.
const MODEL: &str = install::DEFAULT_MODEL;

/// The managed embedder, and whether building one was even possible.
///
/// A `OnceLock` because resolving the model directory reads the file system and
/// two statements embedding at once should not both do it. The manager itself
/// is what decides when the weights are in memory, so this holding a value does
/// not mean the model is loaded.
static EMBEDDER: OnceLock<Option<ManagedEmbedder>> = OnceLock::new();

/// Builds the managed embedder, when this machine has a model to build it over.
///
/// The manifest is read from the model directory when it has one, and falls
/// back to the baseline contract for `nomic-embed-text-v1.5` when it does not -
/// which is the same rule the grading harness applies, and for the same reason:
/// the baseline is the one model whose contract this repository knows by heart,
/// and any other model without a manifest would be run on somebody else's
/// prefixes.
fn build() -> Option<ManagedEmbedder> {
    let dir = install::model_dir(MODEL)?;
    let manifest = ModelManifest::read(&dir).unwrap_or_else(|_| ModelManifest::nomic_v1_5());
    let options = OnnxOptions::for_model(&manifest);
    Some(ManagedEmbedder::new(
        &dir,
        manifest.model_file.clone(),
        options,
        Residency::configured(),
    ))
}

/// The refusal a machine with no model installed gets.
///
/// It names the command that fixes it rather than the variable that would work
/// around it, because a person reading this has almost always never installed
/// the model and the command is one line.
fn no_model() -> error::DbError {
    error::misuse(format!(
        "embed: no embedding model is installed. Run `inillucent setup-embeddings` to download \
         {MODEL} and the ONNX Runtime it needs, or set {} to a directory that already holds them",
        install::MODEL_DIR_VAR
    ))
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
    let Some(embedder) = EMBEDDER.get_or_init(build) else {
        return Err(no_model());
    };
    let vectors = embedder
        .embed_prefixed(&[text])
        .map_err(|reason| error::misuse(format!("embed: {reason:#}")))?;
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

    /// The refusal names the command that installs the model.
    ///
    /// A person reading it has almost always never run the installer, and a
    /// message that named only an environment variable would send them to
    /// download five files by hand instead.
    #[test]
    fn the_refusal_names_the_command_that_fixes_it() {
        let message = format!("{}", no_model());
        assert!(message.contains("inillucent setup-embeddings"), "{message}");
        assert!(message.contains(MODEL), "{message}");
    }
}
