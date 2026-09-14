//! Where inillucent keeps what it installed, and what it recorded about it.
//!
//! Invariant: **there is one rule for where the embedder lives, and both sides
//! of the install read it from here.** `inillucent setup-embeddings` writes into
//! these directories and the engine looks in them, and the two agree because
//! they are the same function rather than two implementations of the same
//! sentence. A second copy of a path rule is a machine where the installer
//! reports success and the engine reports that no model is loaded, and neither
//! message names the other's directory.
//!
//! Nothing here loads a model or links a machine learning runtime. It is path
//! arithmetic, four environment variables and one small JSON file, which is why
//! it is not behind the `onnx` feature: the command that installs the runtime
//! must be able to say where it is going without linking the runtime it is
//! about to fetch.
//!
//! ## The layout
//!
//! ```text
//! <home>/
//!   runtime/onnxruntime-<version>/lib/onnxruntime.dll   the shared library
//!   models/<model id>/                                  weights, tokenizer, manifest
//!   embeddings.json                                     what is installed, and the profile
//!   downloads/                                          partial fetches, removed on success
//! ```
//!
//! `<home>` is `INILLUCENT_HOME` when it is set, and otherwise the directory the
//! platform keeps per-user application data in. It is per-user rather than
//! system-wide on purpose: installing half a gigabyte of weights should not need
//! an administrator, and two people on one machine wanting different models
//! should not have to negotiate.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::ModelManifest;

/// The environment variable that overrides the install root.
pub const HOME_VAR: &str = "INILLUCENT_HOME";

/// The environment variable naming a model directory directly.
///
/// It predates this module and keeps working: a machine that has the weights
/// somewhere the installer would never have put them says so with this, and
/// nothing here overrules it.
pub const MODEL_DIR_VAR: &str = "INILLUCENT_ONNX_DIR";

/// The environment variable `ort` reads to find the ONNX Runtime library.
///
/// Also an override rather than a requirement. When it is set, the installed
/// runtime is not consulted at all.
pub const RUNTIME_VAR: &str = "ORT_DYLIB_PATH";

/// The environment variable that overrides the residency profile for one
/// process.
pub const RESIDENCY_VAR: &str = "INILLUCENT_EMBED_RESIDENCY";

/// The model the setup command installs when it is not told otherwise.
pub const DEFAULT_MODEL: &str = "nomic-embed-text-v1.5";

/// The file inside the home directory that records what was installed.
pub const STATE_FILE: &str = "embeddings.json";

/// The environment variable naming extra directories to look for weights in.
///
/// A list separated by the platform's path separator - `;` on Windows, `:`
/// elsewhere - searched after the install root and before nothing. Each entry
/// holds one directory per model id, the way `models_root` does.
///
/// **It exists because a drive letter is machine configuration and was tracked
/// source.** This list used to be a constant naming a second drive on the
/// machine the engine was written on, where the grading corpus keeps its eight
/// models. That path means nothing to anybody else, and a path to one
/// developer's disk is exactly what a public repository must not carry
/// (task-1946, H7). A machine that keeps its weights somewhere unusual says so
/// here; nothing about that belongs in the source.
pub const MODEL_ROOTS_VAR: &str = "INILLUCENT_MODEL_ROOTS";

/// The directory the documentation has told people to put weights in since the
/// embedder arrived, looked in when the install root does not hold them.
const LEGACY_MODEL_ROOTS: [&str; 1] = ["~/.cache/inillucent-models"];

/// Every extra root to search, in order: the ones `INILLUCENT_MODEL_ROOTS`
/// names, then the documented one.
///
/// @returns the directories, home-expanded, skipping any that cannot be resolved
fn extra_model_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(named) = non_empty_var(MODEL_ROOTS_VAR) {
        for entry in named.split(SEPARATOR) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if let Some(root) = expand_home(entry) {
                roots.push(root);
            }
        }
    }
    for root in LEGACY_MODEL_ROOTS {
        if let Some(root) = expand_home(root) {
            roots.push(root);
        }
    }
    roots
}

/// The character `MODEL_ROOTS_VAR` separates directories with.
#[cfg(windows)]
const SEPARATOR: char = ';';

/// The character `MODEL_ROOTS_VAR` separates directories with.
#[cfg(not(windows))]
const SEPARATOR: char = ':';

/// The install root.
///
/// @returns the directory inillucent keeps installed components in
pub fn home() -> PathBuf {
    if let Some(named) = non_empty_var(HOME_VAR) {
        return PathBuf::from(named);
    }
    platform_home().unwrap_or_else(|| PathBuf::from(".inillucent"))
}

/// The platform's own per-user application data directory, with `inillucent`
/// under it.
///
/// A last resort of `.inillucent` in the working directory rather than a
/// failure, because a machine with neither `HOME` nor `LOCALAPPDATA` set is
/// usually a container, and a container that can still install into the working
/// directory is more useful than one that refuses.
fn platform_home() -> Option<PathBuf> {
    // **One binding per platform rather than one `return` per platform.** Only
    // one of the three is compiled, so each was the function's whole body and
    // each ended with a `return` that clippy reads as needless - correctly, in
    // the sense that a block ending in `return` says with a keyword what a tail
    // expression says by position. The bindings say the same thing and leave
    // the value where the compiler can see it (task-1932, H9).
    #[cfg(windows)]
    let found = non_empty_var("LOCALAPPDATA")
        .map(|local| PathBuf::from(local).join("inillucent"))
        .or_else(|| {
            non_empty_var("USERPROFILE").map(|p| {
                PathBuf::from(p)
                    .join("AppData")
                    .join("Local")
                    .join("inillucent")
            })
        });
    #[cfg(target_os = "macos")]
    let found = non_empty_var("HOME").map(|p| {
        PathBuf::from(p)
            .join("Library")
            .join("Application Support")
            .join("inillucent")
    });
    #[cfg(not(any(windows, target_os = "macos")))]
    let found = non_empty_var("XDG_DATA_HOME")
        .map(|data| PathBuf::from(data).join("inillucent"))
        .or_else(|| {
            non_empty_var("HOME").map(|p| {
                PathBuf::from(p)
                    .join(".local")
                    .join("share")
                    .join("inillucent")
            })
        });
    found
}

/// Where ONNX Runtime installs go, one directory per version.
///
/// @param root - the install root
pub fn runtime_root(root: &Path) -> PathBuf {
    root.join("runtime")
}

/// Where one version of ONNX Runtime goes.
///
/// @param root - the install root
/// @param version - the ONNX Runtime version, e.g. `1.22.0`
pub fn runtime_dir(root: &Path, version: &str) -> PathBuf {
    runtime_root(root).join(format!("onnxruntime-{version}"))
}

/// Where models go, one directory per model id.
///
/// @param root - the install root
pub fn models_root(root: &Path) -> PathBuf {
    root.join("models")
}

/// Where partial downloads go while they are being fetched.
///
/// @param root - the install root
pub fn downloads_dir(root: &Path) -> PathBuf {
    root.join("downloads")
}

/// The file name of the ONNX Runtime shared library on this platform.
pub fn runtime_library_name() -> &'static str {
    #[cfg(windows)]
    {
        "onnxruntime.dll"
    }
    #[cfg(target_os = "macos")]
    {
        "libonnxruntime.dylib"
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        "libonnxruntime.so"
    }
}

/// The installed ONNX Runtime shared library, when there is one.
///
/// `ORT_DYLIB_PATH` wins, because a machine that has been told exactly which
/// library to use has usually been told for a reason. Otherwise the newest
/// installed version is taken, so installing a second version supersedes the
/// first without anything having to remove it.
pub fn runtime_library() -> Option<PathBuf> {
    if let Some(named) = non_empty_var(RUNTIME_VAR) {
        let path = PathBuf::from(named);
        return path.exists().then_some(path);
    }
    let root = home();
    if let Some(state) = read_state(&root) {
        if let Some(runtime) = state.runtime.as_ref() {
            let path = PathBuf::from(&runtime.library);
            if path.exists() {
                return Some(path);
            }
        }
    }
    newest_runtime(&runtime_root(&root))
}

/// The library of the newest ONNX Runtime under a runtime root.
///
/// "Newest" is by version, compared component by component as numbers, because
/// a string comparison puts `1.9.0` after `1.22.0` and would pick the older
/// library on a machine that had installed both.
///
/// @param root - the directory holding one subdirectory per version
fn newest_runtime(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for entry in std::fs::read_dir(root).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(version) = name.strip_prefix("onnxruntime-") else {
            continue;
        };
        let library = entry.path().join("lib").join(runtime_library_name());
        if !library.exists() {
            continue;
        }
        let ordered = version_key(version);
        let better = match best.as_ref() {
            None => true,
            Some((seen, _)) => ordered > *seen,
        };
        if better {
            best = Some((ordered, library));
        }
    }
    best.map(|(_, library)| library)
}

/// A version string as a list of numbers, for ordering.
///
/// A component that is not a number sorts as zero rather than failing: a
/// directory named `onnxruntime-nightly` should not stop a machine finding the
/// numbered install beside it.
///
/// @param version - e.g. `1.22.0`
fn version_key(version: &str) -> Vec<u64> {
    version
        .split(['.', '-'])
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

/// The directory holding one model's weights, when it can be found.
///
/// Looked for in this order: the directory `INILLUCENT_ONNX_DIR` names, the
/// install root, then the directories `INILLUCENT_MODEL_ROOTS` names, then the
/// documented `~/.cache/inillucent-models`. A
/// directory only counts when it holds both the weights and the tokenizer,
/// because half a model produces a failure at the first embedding rather than at
/// the point somebody could still fix it.
///
/// @param id - the model id, which is also its directory name
pub fn model_dir(id: &str) -> Option<PathBuf> {
    if let Some(named) = non_empty_var(MODEL_DIR_VAR) {
        let dir = PathBuf::from(named);
        // The variable names one directory, and a caller asks for one model id.
        // Handing back a directory holding a different model would be the worst
        // failure available here: the session opens, the vectors come out, and
        // every neighbour they are ever compared against was made by something
        // else. So the directory has to agree - either its manifest names the
        // model that was asked for, or it has no manifest at all, which is the
        // case this variable was written for and which only the baseline model
        // is allowed to be.
        let agrees = match ModelManifest::read(&dir) {
            Ok(manifest) => manifest.id == id,
            Err(_) => id == DEFAULT_MODEL,
        };
        if agrees && complete_model(&dir) {
            return Some(dir);
        }
    }
    let installed = models_root(&home()).join(id);
    if complete_model(&installed) {
        return Some(installed);
    }
    for root in extra_model_roots() {
        let dir = root.join(id);
        if complete_model(&dir) {
            return Some(dir);
        }
    }
    None
}

/// Whether a directory holds a model that can actually be opened.
///
/// The weights file is read from `model.json` when there is one, because a
/// directory holding `model_int8.onnx` and a manifest naming it is a complete
/// model and a check for `model.onnx` would call it empty.
///
/// @param dir - the candidate directory
pub fn complete_model(dir: &Path) -> bool {
    if !dir.join("tokenizer.json").exists() {
        return false;
    }
    let named = std::fs::read_to_string(dir.join("model.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("model_file")?.as_str().map(str::to_string));
    match named {
        Some(file) => dir.join(file).exists(),
        None => dir.join("model.onnx").exists(),
    }
}

/// Expands a leading `~/` against the home directory.
///
/// @param path - a path that may begin with `~/`
fn expand_home(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => non_empty_var("HOME")
            .or_else(|| non_empty_var("USERPROFILE"))
            .map(|home| PathBuf::from(home).join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

/// An environment variable, when it is set to something that is not blank.
///
/// A variable set to the empty string means "unset" here rather than "the empty
/// path": a shell that exports `INILLUCENT_HOME=` should get the default, not a
/// refusal about a directory with no name.
///
/// @param name - the variable
fn non_empty_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// What one installed ONNX Runtime is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledRuntime {
    /// The ONNX Runtime version.
    pub version: String,
    /// The archive it came out of, so a reinstall can be compared against it.
    pub archive: String,
    /// The full path of the shared library.
    pub library: String,
    /// Whether the archive's bytes matched a digest this build had pinned.
    ///
    /// Recorded rather than assumed, because a version the installer has no
    /// pinned digest for is still installable and the state file is where a
    /// reader finds out that this one was taken on trust.
    pub verified: bool,
    /// Whether this is a build carrying the CUDA execution provider.
    pub gpu: bool,
}

/// What one installed model is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledModel {
    /// The model id, which is its directory name.
    pub id: String,
    /// The directory holding it.
    pub dir: String,
    /// Where the files were fetched from.
    pub source: String,
    /// Whether every file matched a pinned digest.
    pub verified: bool,
}

/// What `setup-embeddings` recorded, and what the engine reads back.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    /// The installed runtime, when one has been installed.
    #[serde(default)]
    pub runtime: Option<InstalledRuntime>,
    /// Every installed model.
    #[serde(default)]
    pub models: Vec<InstalledModel>,
    /// The residency profile, as it was written on the command line.
    ///
    /// Stored as the text rather than as a parsed policy so that a file written
    /// by a newer build naming a profile this one does not know is a refusal
    /// naming the profile, rather than a deserialization failure naming a line
    /// number.
    #[serde(default)]
    pub residency: Option<String>,
}

impl State {
    /// The installed model of a given id, when there is one.
    ///
    /// @param id - the model id
    pub fn model(&self, id: &str) -> Option<&InstalledModel> {
        self.models.iter().find(|model| model.id == id)
    }

    /// Records an installed model, replacing any earlier record of the same id.
    ///
    /// @param model - what was installed
    pub fn put_model(&mut self, model: InstalledModel) {
        self.models.retain(|existing| existing.id != model.id);
        self.models.push(model);
    }
}

/// The state file inside an install root.
///
/// @param root - the install root
pub fn state_path(root: &Path) -> PathBuf {
    root.join(STATE_FILE)
}

/// Reads the state file, or `None` when there is not one that parses.
///
/// A file that does not parse reads as absent rather than as an error, because
/// the caller is usually the engine trying to find a model and the right
/// behaviour there is to fall back to the search path - not to refuse a query
/// over a file the query never asked about. The setup command reports the parse
/// failure, which is where a person can act on it.
///
/// @param root - the install root
pub fn read_state(root: &Path) -> Option<State> {
    let text = std::fs::read_to_string(state_path(root)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes the state file, creating the install root if it is not there.
///
/// @param root - the install root
/// @param state - what to record
pub fn write_state(root: &Path, state: &State) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let text = serde_json::to_string_pretty(state)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    std::fs::write(state_path(root), text)
}

/// Serializes every test that moves one of this module's environment variables.
///
/// An environment variable is process-wide and `cargo test` runs a crate's cases
/// in one binary on several threads, so two cases racing over one is not a
/// possibility - it is what happens. The cases here point `INILLUCENT_ONNX_DIR`
/// and `INILLUCENT_HOME` at directories they made, and `residency`'s cases
/// resolve a real model through the same variables, so the lock spans both
/// modules rather than sitting inside one of them.
///
/// Poison-tolerant on purpose: a case that panics with it held has already
/// failed and said why, and turning that into a second failure in every later
/// case would bury the message that matters.
#[cfg(test)]
pub(crate) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime directory is named for its version, so two versions coexist.
    #[test]
    fn each_runtime_version_gets_its_own_directory() {
        let root = Path::new("/tmp/x");
        assert_ne!(runtime_dir(root, "1.22.0"), runtime_dir(root, "1.23.0"));
        assert!(runtime_dir(root, "1.22.0").ends_with("onnxruntime-1.22.0"));
    }

    /// Versions order as numbers, so 1.22.0 is newer than 1.9.0.
    ///
    /// A string comparison gets this backwards, and the consequence is a machine
    /// that installed both loading the older library and failing on a session
    /// option the newer one supports.
    #[test]
    fn a_version_orders_by_its_numbers_and_not_by_its_text() {
        assert!(version_key("1.22.0") > version_key("1.9.0"));
        assert!(version_key("1.22.1") > version_key("1.22.0"));
        assert!(
            "1.22.0" < "1.9.0",
            "the string order is the one this avoids"
        );
    }

    /// A non-numeric component sorts as zero rather than stopping the scan.
    #[test]
    fn a_version_with_a_word_in_it_still_orders() {
        assert_eq!(version_key("nightly"), vec![0]);
        assert!(version_key("1.22.0") > version_key("nightly"));
    }

    /// A blank variable reads as unset.
    #[test]
    fn a_blank_variable_is_not_a_value() {
        let _held = env_guard();
        let name = "INILLUCENT_TEST_BLANK_VAR";
        std::env::set_var(name, "   ");
        assert_eq!(non_empty_var(name), None);
        std::env::set_var(name, "value");
        assert_eq!(non_empty_var(name), Some("value".to_string()));
        std::env::remove_var(name);
    }

    /// A directory with a tokenizer and no weights is not a model.
    #[test]
    fn half_a_model_does_not_count_as_one() {
        let dir = std::env::temp_dir().join("inillucent-install-half-model");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        assert!(!complete_model(&dir), "no weights");
        std::fs::write(dir.join("model.onnx"), "not really a graph").unwrap();
        assert!(complete_model(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A manifest naming a different weights file is what decides completeness.
    ///
    /// Without this, a directory holding `model_int8.onnx` and a manifest saying
    /// so is reported as having no model, and the installer's own `--weights`
    /// choice becomes unusable.
    #[test]
    fn the_manifest_decides_which_weights_file_has_to_be_there() {
        let dir = std::env::temp_dir().join("inillucent-install-named-weights");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        std::fs::write(
            dir.join("model.json"),
            r#"{"model_file":"model_int8.onnx"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("model.onnx"), "the wrong one").unwrap();
        assert!(
            !complete_model(&dir),
            "the manifest names a file that is not there"
        );
        std::fs::write(dir.join("model_int8.onnx"), "the right one").unwrap();
        assert!(complete_model(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The state file round trips, and a model is replaced rather than doubled.
    #[test]
    fn recording_a_model_twice_leaves_one_record() {
        let root = std::env::temp_dir().join("inillucent-install-state");
        let _ = std::fs::remove_dir_all(&root);
        let mut state = State::default();
        state.put_model(InstalledModel {
            id: "m".to_string(),
            dir: "a".to_string(),
            source: "s".to_string(),
            verified: false,
        });
        state.put_model(InstalledModel {
            id: "m".to_string(),
            dir: "b".to_string(),
            source: "s".to_string(),
            verified: true,
        });
        assert_eq!(state.models.len(), 1);
        assert_eq!(state.model("m").map(|m| m.dir.as_str()), Some("b"));

        write_state(&root, &state).unwrap();
        assert_eq!(read_state(&root), Some(state));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `INILLUCENT_ONNX_DIR` is only used for the model it actually holds.
    ///
    /// The variable names one directory and a caller asks for one model id, so a
    /// build that honoured it unconditionally would hand `nomic-embed-text-v2-moe`
    /// a directory holding `nomic-embed-text-v1.5`. Nothing would error: the
    /// session opens, vectors come out, and every neighbour they are compared
    /// against was made by a different model. Nikaya found this by asking for the
    /// model its corpus was built with and being given another one.
    #[test]
    fn the_override_directory_is_only_used_for_the_model_it_holds() {
        let _held = env_guard();
        let dir = std::env::temp_dir().join("inillucent-install-override");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        std::fs::write(dir.join("model.onnx"), "weights").unwrap();
        std::fs::write(dir.join("model.json"), r#"{"id":"model-a","dims":768,"mrl_widths":[768],"prefixes":{"query":"","document":"","clustering":"","classification":""},"pooling":"mean","max_tokens":512}"#).unwrap();

        let previous = std::env::var(MODEL_DIR_VAR).ok();
        std::env::set_var(MODEL_DIR_VAR, &dir);
        assert_eq!(model_dir("model-a").as_deref(), Some(dir.as_path()));
        assert_ne!(
            model_dir("model-b").as_deref(),
            Some(dir.as_path()),
            "a directory holding model-a is not model-b"
        );
        match previous {
            Some(value) => std::env::set_var(MODEL_DIR_VAR, value),
            None => std::env::remove_var(MODEL_DIR_VAR),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A directory with no manifest still answers for the baseline model, which
    /// is what the variable was written for and what every machine using it has.
    #[test]
    fn a_directory_with_no_manifest_still_answers_for_the_baseline_model() {
        let _held = env_guard();
        let dir = std::env::temp_dir().join("inillucent-install-unmanifested");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        std::fs::write(dir.join("model.onnx"), "weights").unwrap();

        let previous = std::env::var(MODEL_DIR_VAR).ok();
        std::env::set_var(MODEL_DIR_VAR, &dir);
        assert_eq!(model_dir(DEFAULT_MODEL).as_deref(), Some(dir.as_path()));
        assert_ne!(model_dir("something-else").as_deref(), Some(dir.as_path()));
        match previous {
            Some(value) => std::env::set_var(MODEL_DIR_VAR, value),
            None => std::env::remove_var(MODEL_DIR_VAR),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A state file that does not parse reads as absent.
    #[test]
    fn an_unreadable_state_file_does_not_stop_a_lookup() {
        let root = std::env::temp_dir().join("inillucent-install-bad-state");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(state_path(&root), "{ this is not json").unwrap();
        assert_eq!(read_state(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `INILLUCENT_HOME` wins over the platform directory.
    #[test]
    fn the_home_variable_overrides_the_platform_directory() {
        let _held = env_guard();
        let previous = std::env::var(HOME_VAR).ok();
        std::env::set_var(HOME_VAR, "/somewhere/else");
        assert_eq!(home(), PathBuf::from("/somewhere/else"));
        match previous {
            Some(value) => std::env::set_var(HOME_VAR, value),
            None => std::env::remove_var(HOME_VAR),
        }
    }
}
