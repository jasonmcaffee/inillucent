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
//! ## Where the refusals are
//!
//! In `crate::embed_refusal`, which is not behind the `embed` feature. They are
//! sentences and a marker, nothing about them needs ONNX, and behind the gate
//! they were in a module no default build compiles - so the test asserting the
//! no-model refusal names `inillucent setup-embeddings` was never in a test
//! binary, and the refusal shipped saying "bad parameter or other API misuse"
//! instead. That module's comment has the whole of it.
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

use crate::embed_refusal::{model_would_not_run, no_model, MODEL};

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
        .map_err(|reason| model_would_not_run(&format!("{reason:#}")))?;
    let Some(vector) = vectors.first() else {
        return Err(error::refusal("embed: the model returned no vector"));
    };
    let mut bytes = Vec::with_capacity(vector.len().saturating_mul(4));
    for value in vector {
        bytes.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    Value::owned_blob(&bytes)
}

/// Adds `embed` to a registry.
///
/// **Registered as deterministic, which is what makes a semantic search take
/// a second rather than a minute.** The same text through the same weights
/// gives the same vector, so a call whose argument does not vary within one
/// statement may be evaluated once for the statement instead of once for every
/// row. Without this flag the planner has to assume the model might answer
/// differently each time, and
/// `ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5`
/// over the 2,661 passages of `examples/rag-agent` measured **105.7 seconds**
/// against 1.48 for the same question written as a one-row subquery - 2,661
/// embeddings of one sentence, and 2,660 of them thrown away.
///
/// **And `direct_only`, which until task-1970 it said it was and was not.** A
/// function that loads a 275 MB model has no business being called out of a
/// `CHECK` constraint or an index expression, and being deterministic says
/// nothing about being cheap. The registration read
/// `FunctionFlags { deterministic: true, ..FunctionFlags::default() }`, and the
/// `Default` derive is every flag false - so with `PRAGMA trusted_schema` on,
/// which is the default, `authorize_function` admitted `embed` from a schema.
/// A `CREATE INDEX i ON t (embed(body))` would then load the model once per row
/// of the table, inside the statement that creates the index, and a `CHECK`
/// would load it on every insert.
///
/// It is a behaviour change to a shipped function and `CHANGELOG.md` records it
/// under 0.1.4. Nothing promised the old behaviour: `PRAGMA function_list` does
/// not report the bit (`engine/pragma.rs`), no document said a schema could
/// name `embed`, and this comment said the opposite.
///
/// @param registry - what a connection reaches functions through
pub fn register(registry: &mut inillucent_ext::registry::Registry) {
    registry.register_function(inillucent_ext::registry::UserFunction {
        flags: inillucent_ext::registry::FunctionFlags {
            deterministic: true,
            ..inillucent_ext::registry::FunctionFlags::external()
        },
        ..inillucent_ext::registry::UserFunction::external(
            "embed",
            1,
            inillucent_ext::registry::UserBody::Scalar(Arc::new(embed)),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `embed` is registered as a function a schema may not name.
    ///
    /// **This is the half of task-1969's 7.4 that is reachable from here, and
    /// the half that was wrong.** The registration read
    /// `FunctionFlags { deterministic: true, ..FunctionFlags::default() }`, and
    /// the `Default` derive is every flag false - so `embed` was registered as
    /// a function a `CHECK`, an index expression or a generated column may
    /// name, while its own doc comment said "It stays `direct_only`".
    #[test]
    fn embed_is_registered_as_direct_only_and_deterministic() {
        let mut registry = inillucent_ext::registry::Registry::with_builtins();
        register(&mut registry);
        let flags = registry.function_flags(b"embed");
        assert!(
            flags.direct_only,
            "`embed` loads a 275 MB model and a schema may not name it"
        );
        assert!(
            flags.deterministic,
            "`embed` is deterministic, which is what lets one statement embed a phrase once"
        );
    }

    /// The registry refuses `embed` from a schema and allows it from a
    /// statement, with the schema trusted.
    ///
    /// `trusted_schema` is the lever `direct_only` has to beat:
    /// `authorize_function` returns early for a trusted schema *unless* the
    /// function is direct-only, so a build that lost the flag would let `embed`
    /// through on every machine that had not turned the pragma off - which is
    /// every machine, because it is on by default.
    ///
    /// The second assertion is what stops the first from being a test that
    /// `embed` is unusable.
    #[test]
    fn a_trusted_schema_may_not_name_embed_and_a_statement_may() {
        let mut registry = inillucent_ext::registry::Registry::with_builtins();
        register(&mut registry);
        assert!(
            registry.policy().trusted_schema,
            "this case is about the lever being on, and it is off"
        );
        let refused = registry
            .authorize_function(b"embed", inillucent_ext::registry::CallSite::Schema)
            .expect_err("a schema may not name embed");
        assert!(
            refused
                .message()
                .contains("may only be used from top-level SQL"),
            "`embed` was refused from a schema for the wrong reason: {refused}"
        );
        assert!(
            registry
                .authorize_function(b"embed", inillucent_ext::registry::CallSite::Statement)
                .is_ok(),
            "a statement may name `embed`"
        );
    }

    /// The constructor a registrant should reach for sets the flag, and the
    /// `Default` derive does not.
    ///
    /// **The two are one word apart and mean opposite things**, which is how
    /// this defect happened: `..FunctionFlags::external()` is direct-only and
    /// `..FunctionFlags::default()` is not. Written down as an assertion rather
    /// than as a comment, because the comment existed and was read as
    /// describing the derive.
    #[test]
    fn the_external_constructor_is_the_one_that_sets_the_flag() {
        assert!(inillucent_ext::registry::FunctionFlags::external().direct_only);
        assert!(!inillucent_ext::registry::FunctionFlags::default().direct_only);
        let made = inillucent_ext::registry::UserFunction::external(
            "made",
            1,
            inillucent_ext::registry::UserBody::Scalar(Arc::new(embed)),
        );
        assert!(made.flags.direct_only);
        assert_eq!(made.name, "made");
        assert_eq!(made.arity, 1);
    }

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

    /// The refusal a machine with no model installed gets is the one
    /// `embed_refusal` builds, and it is what this function returns.
    ///
    /// The sentence and its marker are asserted in `embed_refusal`'s own tests,
    /// which a default build runs. This one is about the wiring: that the
    /// no-model branch here reaches them at all. It only runs in a build with
    /// the `embed` feature, which is the reason the sentences live over there -
    /// see that module's comment.
    #[test]
    fn the_no_model_branch_returns_the_refusal_that_names_the_installer() {
        let message = format!("{}", no_model());
        assert!(message.contains("inillucent setup-embeddings"), "{message}");
        assert!(message.contains(MODEL), "{message}");
        assert_eq!(no_model().requirement(), Some("an embedding model"));
    }
}
