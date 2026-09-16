//! How much of a corpus a model would truncate, counted without running it.
//!
//! One idea and one number, kept out of `synth` because it is read as a measurement and has
//! been wrong twice. It is gate C3's subject: the share of chunks a model's tokenizer takes
//! past its bound, which decides whether a context window is big enough for this corpus.
//!
//! Both times it was wrong it read *low*, which is the direction that does not get noticed.
//! `snowflake-arctic-embed-m-v2.0` reported 0.00 per cent because its own `tokenizer.json`
//! carries `truncation: max_length 512`, so an armed tokenizer found nothing over the bound it
//! had already applied. `nomic-embed-text-v2-moe` reported 0.36 per cent because a served arm
//! was skipped here and its cache header fell back to the counter of whichever process finished
//! a run that had been resumed four times; the corpus figure is 2.32 per cent.

use anyhow::Result;
use inillucent_core::embed_onnx::count_truncation;
use inillucent_core::model::ModelManifest;

use crate::synth::{sanitize_for_model, SynthChunk};

/// How many of these chunks a model tokenizer takes past its truncation bound,
/// counted without running the model.
///
/// Needed on a resumed run: the sessions this process opened only saw the chunks
/// this process embedded, and a truncation share that silently described the tail
/// of the corpus would be exactly the kind of number that reads as a measurement
/// and is not one.
/// @param model_dir - the model directory
/// @param manifest - what the model is
/// @param chunks - the chunks to count over
pub fn count_truncated(
    model_dir: &str,
    manifest: &ModelManifest,
    chunks: &[SynthChunk],
) -> Result<usize> {
    // A served arm is counted here too, and used not to be. It was skipped on the grounds
    // that it has no local tokenizer, which is not true: `llamacpp.rs` loads `tokenizer.json`
    // from this same directory and counts every request with it, checking it against the
    // server's own `/tokenize` on a sample when it connects. `count_truncation` needs that
    // file and the manifest and nothing else.
    //
    // What the old early return cost: `nomic-embed-text-v2-moe`'s Phase 0 cache header
    // recorded 664 truncated chunks, 0.36 per cent, because its embedding run was resumed
    // four times and the header kept the last process's counter. The corpus figure is 4,299,
    // 2.32 per cent - six times larger, and the number gate C3 is read from.
    let texts: Vec<String> = chunks
        .iter()
        .map(|c| sanitize_for_model(&c.content))
        .collect();
    Ok(count_truncation(model_dir, manifest, &texts)?.truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_core::model::Backend;

    /// A tokenizer that splits on spaces and knows three words. Enough to count with, and
    /// small enough to write in a test, so this case does not need a model on the machine.
    ///
    /// @param dir - where `tokenizer.json` goes
    fn write_word_tokenizer(dir: &std::path::Path) {
        let json = r#"{
          "version": "1.0",
          "truncation": null,
          "padding": null,
          "added_tokens": [],
          "normalizer": null,
          "pre_tokenizer": {"type": "Whitespace"},
          "post_processor": null,
          "decoder": null,
          "model": {
            "type": "WordLevel",
            "vocab": {"alpha": 0, "beta": 1, "[UNK]": 2},
            "unk_token": "[UNK]"
          }
        }"#;
        std::fs::write(dir.join("tokenizer.json"), json).unwrap();
    }

    /// One manifest, two backends, so the only thing the assertion can be reading is the
    /// backend.
    ///
    /// @param backend - the backend to declare
    /// @param max_tokens - the bound to count against
    fn counting_manifest(backend: Backend, max_tokens: usize) -> ModelManifest {
        ModelManifest {
            id: "counting".to_string(),
            dims: 8,
            mrl_widths: vec![8],
            prefixes: inillucent_core::model::Prefixes::none(),
            pooling: inillucent_core::model::Pooling::Mean,
            max_tokens,
            layer_norm: false,
            model_file: "model.gguf".into(),
            token_type_ids: false,
            backend,
            output: inillucent_core::model::Output::TokenEmbeddings,
            output_name: String::new(),
            tokenizer_sha256: String::new(),
            weights_sha256: String::new(),
            recipe_git_sha: None,
            source: None,
        }
    }

    /// A served arm's truncation is counted over the corpus, exactly as a loaded arm's is.
    ///
    /// It used to return zero on the grounds that a served arm has no local tokenizer, which
    /// is not true - `llamacpp.rs` counts every request with `tokenizer.json` from this same
    /// directory. The header then fell back to the running counter of whichever process
    /// finished the run, and `nomic-embed-text-v2-moe`'s Phase 0 embedding was resumed four
    /// times because the server slows down, so its cache recorded 664 truncated chunks over
    /// the last 16,378 it embedded. The corpus figure is 4,299 - 2.32 per cent against the
    /// 0.36 per cent on the card, and the number gate C3 is read from.
    #[test]
    fn a_served_arm_counts_its_truncation_like_a_loaded_one() {
        let dir = std::env::temp_dir().join("inillucent-served-truncation");
        std::fs::create_dir_all(&dir).unwrap();
        write_word_tokenizer(&dir);
        let path = dir.to_string_lossy().to_string();

        // Three short chunks and two over a four token bound.
        let chunks: Vec<SynthChunk> = [
            "alpha beta",
            "alpha",
            "beta beta",
            "alpha beta alpha beta alpha",
            "beta alpha beta alpha beta alpha",
        ]
        .iter()
        .enumerate()
        .map(|(i, text)| SynthChunk {
            doc_id: i as i64,
            source: "test".to_string(),
            chunk_index: 0,
            heading_path: Vec::new(),
            content: (*text).to_string(),
            title: String::new(),
            url: String::new(),
            space_key: None,
            author: None,
            author_id: None,
            updated_at: None,
            labels: Vec::new(),
            deleted: false,
        })
        .collect();

        let served = count_truncated(&path, &counting_manifest(Backend::LlamaCpp, 4), &chunks)
            .expect("counting a served arm");
        let loaded = count_truncated(&path, &counting_manifest(Backend::Onnx, 4), &chunks)
            .expect("counting a loaded arm");
        assert_eq!(
            loaded, 2,
            "two of the five chunks are over a four token bound"
        );
        assert_eq!(
            served, loaded,
            "a served arm has the same tokenizer and must reach the same count: \
             {served} against {loaded}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
