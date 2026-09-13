//! Finding a model, and proving it is the model it says it is.
//!
//! A manifest describes a model; this resolves one from a directory and checks
//! that the weights and the tokenizer beside it are the ones the manifest names.
//! Both halves matter and for the same reason: the harness now runs eight models
//! against each other, and the cheapest way for that comparison to be wrong is
//! for one arm to have quietly opened another arm's weights, or the same weights
//! with a tokenizer that was updated underneath them. A digest is what turns that
//! from a thing nobody would notice into a refusal with a name on it.
//!
//! The baseline is a special case in one direction only. A model directory with
//! no `model.json` and the baseline's name resolves to
//! `ModelManifest::nomic_v1_5()`, so a command line written before manifests
//! existed still runs and still produces the same numbers. Any other unmanifested
//! directory is refused, because guessing a model's prefixes is exactly the kind
//! of helpfulness that produces a card nobody can trust.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use inillucent_base::hash::Sha256;
use inillucent_core::model::ModelManifest;

/// The file a model directory describes itself with.
pub const MANIFEST_FILE: &str = "model.json";

/// Where downloaded and trained models live.
///
/// On J:, not C:, because a set of eight embedding models is a hundred gigabytes
/// and C: filling up is a machine-wide outage. `INILLUCENT_MODELS` overrides it.
pub fn default_models_root() -> PathBuf {
    match std::env::var("INILLUCENT_MODELS") {
        Ok(path) if !path.trim().is_empty() => PathBuf::from(path),
        _ => PathBuf::from("J:/inillucent-embeddings/models"),
    }
}

/// A model directory and what it says it holds.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub dir: PathBuf,
    pub manifest: ModelManifest,
    /// Whether the manifest was read from disk or supplied from the baseline
    /// constant. A card prints this, because "the harness assumed the prefixes"
    /// and "the model declared its prefixes" are different claims.
    pub manifest_on_disk: bool,
}

/// Read a model's manifest out of its own directory.
///
/// @param dir - the model directory
/// @param fallback_model_file - the weights file to assume for the unmanifested
///   baseline, so a pre-manifest command line keeps working unchanged
pub fn resolve_dir(dir: &Path, fallback_model_file: &str) -> Result<ResolvedModel> {
    let path = dir.join(MANIFEST_FILE);
    if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let manifest: ModelManifest = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        return Ok(ResolvedModel { dir: dir.to_path_buf(), manifest, manifest_on_disk: true });
    }

    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let baseline = ModelManifest::nomic_v1_5();
    anyhow::ensure!(
        name == baseline.id,
        "{} has no {MANIFEST_FILE}, and only {} may run without one. Write a manifest naming \
         this model's width, prefixes, pooling and token bound: running it on another model's \
         contract is how a head-to-head measures the harness instead of the model",
        dir.display(),
        baseline.id
    );
    Ok(ResolvedModel {
        dir: dir.to_path_buf(),
        manifest: ModelManifest { model_file: fallback_model_file.to_string(), ..baseline },
        manifest_on_disk: false,
    })
}

/// Read a model's manifest by its id, under a models root.
/// @param root - where models live, one directory per id
/// @param id - the model id, which is also its directory name
pub fn resolve_id(root: &Path, id: &str) -> Result<ResolvedModel> {
    let dir = root.join(id);
    anyhow::ensure!(
        dir.is_dir(),
        "no model directory {} under {}. The cache names {id} as the model that produced it, so \
         either the model was moved or the cache belongs to a different machine",
        id,
        root.display()
    );
    let resolved = resolve_dir(&dir, "model.onnx")?;
    anyhow::ensure!(
        resolved.manifest.id == id,
        "{} holds a manifest whose id is {}, not {id}. A directory named for one model and \
         describing another is the first step of an arm running the wrong weights",
        dir.display(),
        resolved.manifest.id
    );
    Ok(resolved)
}

/// SHA-256 of a file, streamed.
pub fn file_digest(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to digest it", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.hex())
}

impl ResolvedModel {
    /// Check the files beside the manifest are the files it names.
    ///
    /// Only the digests the manifest actually declares are checked. An empty
    /// digest is a manifest that has not been sealed yet, which is a different
    /// state from a manifest that is wrong, and conflating them would make
    /// `models seal` impossible to run for the first time.
    pub fn verify_files(&self) -> Result<()> {
        for (label, name, declared) in [
            ("weights", self.manifest.model_file.as_str(), self.manifest.weights_sha256.as_str()),
            ("tokenizer", "tokenizer.json", self.manifest.tokenizer_sha256.as_str()),
        ] {
            if declared.is_empty() {
                continue;
            }
            let path = self.dir.join(name);
            let actual = file_digest(&path)?;
            anyhow::ensure!(
                actual == declared,
                "the {label} at {} digests to {}, and {}'s manifest names {}. One of them has \
                 been replaced since the manifest was sealed; nothing graded against this \
                 directory would describe the model the card would name",
                path.display(),
                crate::corpus::short(&actual),
                self.manifest.id,
                crate::corpus::short(declared)
            );
        }
        Ok(())
    }

    /// The digest of this manifest, as a cache header stores it.
    pub fn digest(&self) -> String {
        crate::corpus::manifest_digest(&self.manifest)
    }

    /// Fill in the weights and tokenizer digests from the files on disk and write
    /// the manifest back. This is how a manifest is sealed after a download.
    pub fn seal(&mut self) -> Result<PathBuf> {
        self.manifest.weights_sha256 = file_digest(&self.dir.join(&self.manifest.model_file))?;
        self.manifest.tokenizer_sha256 = file_digest(&self.dir.join("tokenizer.json"))?;
        let path = self.dir.join(MANIFEST_FILE);
        std::fs::write(&path, serde_json::to_string_pretty(&self.manifest)?)
            .with_context(|| format!("writing {}", path.display()))?;
        self.manifest_on_disk = true;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_core::model::{Pooling, Prefixes};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("inillucent-models-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_manifest_on_disk_is_read_back_exactly() {
        let root = scratch("read-back");
        let dir = root.join("some-model");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = ModelManifest {
            id: "some-model".into(),
            dims: 1024,
            mrl_widths: vec![1024],
            prefixes: Prefixes::query_only("query: "),
            pooling: Pooling::Cls,
            max_tokens: 512,
            layer_norm: false,
            model_file: "model_fp32.onnx".into(),
            token_type_ids: false,
            backend: inillucent_core::model::Backend::Onnx,
            output: inillucent_core::model::Output::TokenEmbeddings,
            output_name: String::new(),
            tokenizer_sha256: String::new(),
            weights_sha256: String::new(),
            recipe_git_sha: None,
            source: None,
        };
        std::fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let resolved = resolve_id(&root, "some-model").unwrap();
        assert_eq!(resolved.manifest, manifest);
        assert!(resolved.manifest_on_disk);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_unmanifested_directory_that_is_not_the_baseline_is_refused() {
        let root = scratch("unmanifested");
        let dir = root.join("gte-modernbert-base");
        std::fs::create_dir_all(&dir).unwrap();
        let err = resolve_id(&root, "gte-modernbert-base").unwrap_err().to_string();
        assert!(err.contains("has no model.json"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_unmanifested_baseline_still_resolves_to_the_constants_it_always_used() {
        let root = scratch("baseline");
        let dir = root.join("nomic-embed-text-v1.5");
        std::fs::create_dir_all(&dir).unwrap();
        let resolved = resolve_id(&root, "nomic-embed-text-v1.5").unwrap();
        assert!(!resolved.manifest_on_disk);
        assert_eq!(resolved.manifest.dims, 768);
        assert_eq!(resolved.manifest.max_tokens, 1900);
        assert_eq!(resolved.manifest.prefixes.query, "search_query: ");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_directory_named_for_one_model_and_describing_another_is_refused() {
        let root = scratch("mismatch");
        let dir = root.join("granite-embedding-english-r2");
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = ModelManifest::nomic_v1_5();
        manifest.id = "nomic-embed-text-v1.5".into();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let err = resolve_id(&root, "granite-embedding-english-r2").unwrap_err().to_string();
        assert!(err.contains("whose id is"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_replaced_weights_file_fails_verification_by_name() {
        let root = scratch("replaced");
        let dir = root.join("some-model");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model.onnx"), b"the weights").unwrap();
        std::fs::write(dir.join("tokenizer.json"), b"{}").unwrap();
        let mut resolved = ResolvedModel {
            dir: dir.clone(),
            manifest: ModelManifest { id: "some-model".into(), ..ModelManifest::nomic_v1_5() },
            manifest_on_disk: false,
        };
        resolved.seal().unwrap();
        resolved.verify_files().unwrap();

        std::fs::write(dir.join("model.onnx"), b"other weights").unwrap();
        let err = resolved.verify_files().unwrap_err().to_string();
        assert!(err.contains("has been replaced"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_unsealed_manifest_verifies_rather_than_failing_on_an_empty_digest() {
        let root = scratch("unsealed");
        let dir = root.join("some-model");
        std::fs::create_dir_all(&dir).unwrap();
        let resolved = ResolvedModel {
            dir,
            manifest: ModelManifest { id: "some-model".into(), ..ModelManifest::nomic_v1_5() },
            manifest_on_disk: false,
        };
        // No files at all, and no declared digests: nothing to contradict.
        resolved.verify_files().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }
}

/// The embedding invariants, run against **every** installed arm rather than
/// against the baseline alone.
///
/// `embed_onnx.rs` asserts these for `nomic-embed-text-v1.5`, which was the
/// right scope while there was one model. It is the wrong scope now: the whole
/// point of the manifest is that eight models share one code path, and an
/// invariant that holds for the one model somebody tested with is not an
/// invariant.
///
/// This is not hypothetical either. `snowflake-arctic-embed-m-v2.0` ships
/// `padding: BatchLongest` in its own `tokenizer.json`, and with that padding
/// attended to as though it were text, `a_short_text_is_unaffected_by_a_long_one`
/// fails on that arm and passes on every other - which is exactly the shape of
/// bug this file exists to catch, and exactly the one that was caught by hand
/// instead, because nothing ran these checks anywhere but on the baseline.
///
/// Every model under the models root that has a manifest and its weights is
/// exercised. A machine with no models installed reports that and passes, rather
/// than failing for a reason that has nothing to do with the code.
#[cfg(test)]
mod arms {
    use super::*;
    use inillucent_core::distance::dot;
    use inillucent_core::embed::Embedder;
    use inillucent_core::embed_onnx::{Device, OnnxEmbedder};
    use inillucent_core::model::Backend;

    /// Distinct, varied, and long enough that a batch has something to pad.
    fn texts() -> Vec<String> {
        vec![
            "offer".to_string(),
            "offer eligibility is evaluated against the member profile".to_string(),
            "fn compute_offer_eligibility(member_id: u64) -> Result<Vec<Offer>>".to_string(),
            "PROJ-4821 blocked on the redemption service returning 502s".to_string(),
            "the espresso machine on the third floor is broken again ".repeat(30),
        ]
    }

    /// Every installed arm that this process can load: a manifest, its weights,
    /// its tokenizer, and an ONNX backend. A served arm needs a running server
    /// and is not something a unit test may assume.
    fn installed() -> Vec<ResolvedModel> {
        let root = default_models_root();
        let Ok(entries) = std::fs::read_dir(&root) else {
            eprintln!("skipping: no models root at {}", root.display());
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join(MANIFEST_FILE).exists() {
                continue;
            }
            let Ok(model) = resolve_dir(&dir, "model.onnx") else { continue };
            if model.manifest.backend != Backend::Onnx {
                continue;
            }
            if !dir.join(&model.manifest.model_file).exists() || !dir.join("tokenizer.json").exists()
            {
                continue;
            }
            out.push(model);
        }
        out.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        out
    }

    /// Open one arm on the processor. The processor, not a card: this is a
    /// correctness check, the texts are five, and a test that needs a free GPU is
    /// a test that gets skipped.
    fn open(model: &ResolvedModel) -> Option<OnnxEmbedder> {
        match OnnxEmbedder::open_manifest(&model.dir, &model.manifest, 8, Device::Cpu) {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!("skipping {}: {err:#}", model.manifest.id);
                None
            }
        }
    }

    #[test]
    fn every_arm_embeds_the_same_text_identically_twice() {
        let models = installed();
        if models.is_empty() {
            eprintln!("no arms installed; skipping");
            return;
        }
        for model in &models {
            let Some(e) = open(model) else { continue };
            let a = e.embed_query("how does offer eligibility work").unwrap();
            let b = e.embed_query("how does offer eligibility work").unwrap();
            assert_eq!(a, b, "{} is not deterministic", model.manifest.id);
            assert_eq!(a.len(), model.manifest.dims, "{} width", model.manifest.id);
            assert!(
                (dot(&a, &a) - 1.0).abs() < 1e-4,
                "{} does not return a unit vector",
                model.manifest.id
            );
        }
    }

    /// A batch boundary must not move any individual vector, or which chunks
    /// happened to be embedded together would change what a document was indexed
    /// with.
    #[test]
    fn batching_does_not_change_any_arms_result() {
        let models = installed();
        if models.is_empty() {
            eprintln!("no arms installed; skipping");
            return;
        }
        for model in &models {
            let Some(e) = open(model) else { continue };
            let texts = texts();
            let batched = e.embed_documents(&texts).unwrap();
            for (i, t) in texts.iter().enumerate() {
                let alone = e.embed_documents(std::slice::from_ref(t)).unwrap();
                let agreement = dot(&batched[i], &alone[0]);
                assert!(
                    agreement > 0.9999,
                    "{}: text {i} differed between batched and single, cosine {agreement}",
                    model.manifest.id
                );
            }
        }
    }

    /// Padding a short text up to the longest in its batch must not reach the
    /// pooled vector. This is the one that `snowflake-arctic-embed-m-v2.0` failed
    /// before its tokenizer's own padding was disarmed on load.
    #[test]
    fn no_arm_lets_a_long_text_leak_into_a_short_one_in_the_same_batch() {
        let models = installed();
        if models.is_empty() {
            eprintln!("no arms installed; skipping");
            return;
        }
        for model in &models {
            let Some(e) = open(model) else { continue };
            let short = "offer".to_string();
            let long = "offer eligibility rules and redemption windows ".repeat(40);
            let together = e.embed_documents(&[short.clone(), long]).unwrap();
            let alone = e.embed_documents(&[short]).unwrap();
            let agreement = dot(&together[0], &alone[0]);
            assert!(
                agreement > 0.9999,
                "{}: padding leaked into the result, cosine {agreement}",
                model.manifest.id
            );
        }
    }

    /// A query and a document must not embed to the same vector on an arm whose
    /// manifest gives them different prefixes, and *must* on an arm whose
    /// manifest gives them the same one. Both directions, because a prefix that
    /// is silently dropped and a prefix that is silently added are both real.
    #[test]
    fn every_arm_applies_exactly_the_prefixes_its_manifest_declares() {
        let models = installed();
        if models.is_empty() {
            eprintln!("no arms installed; skipping");
            return;
        }
        let text = "offer eligibility rules";
        for model in &models {
            let Some(e) = open(model) else { continue };
            let as_query = e.embed_query(text).unwrap();
            let as_document = e.embed_documents(&[text.to_string()]).unwrap();
            let agreement = dot(&as_query, &as_document[0]);
            let symmetric = model.manifest.prefixes.query == model.manifest.prefixes.document;
            if symmetric {
                assert!(
                    agreement > 0.9999,
                    "{} declares one prefix for both sides and yet they differ, cosine {agreement}",
                    model.manifest.id
                );
            } else {
                assert!(
                    agreement < 0.9999,
                    "{} declares different prefixes for query and document and yet they agree to \
                     cosine {agreement}, so one of them is not being applied",
                    model.manifest.id
                );
            }
        }
    }

    /// Every arm's tokenizer reports the text's own token count, not a padded or
    /// pre-truncated one. The truncation share on the card is only a measurement
    /// if this holds for every column of it.
    #[test]
    fn every_arms_token_count_is_the_texts_own() {
        let models = installed();
        if models.is_empty() {
            eprintln!("no arms installed; skipping");
            return;
        }
        for model in &models {
            let Some(e) = open(model) else { continue };
            e.embed_documents(&["one two three four five six".to_string()]).unwrap();
            let facts = e.truncation();
            assert_eq!(facts.texts, 1, "{}", model.manifest.id);
            assert_eq!(facts.truncated, 0, "{}", model.manifest.id);
            assert!(
                facts.tokens > 3 && facts.tokens < 64,
                "{} tokenized a six word text to {} tokens, which is a padded or fixed count",
                model.manifest.id,
                facts.tokens
            );
        }
    }
}
