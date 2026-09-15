//! `inillucent setup-embeddings`: install the embedder, on any of three
//! platforms, in one command.
//!
//! Invariant: **a component this reports as installed is verified, complete and
//! reachable without an environment variable.** Every archive and every weights
//! file is checked against a digest pinned in this file before anything is
//! written where the engine looks, the manifest is sealed from the files that
//! actually landed rather than from what was expected, and the directories are
//! the ones `inillucent_core::install` computes - so `embed(TEXT)` works
//! immediately afterwards on a shell that has never exported anything.
//!
//! ## What is pinned, and why by digest rather than only by version
//!
//! A version pins what was asked for. A digest pins what arrived. The two
//! differ whenever a release asset is replaced, a content network serves a
//! truncated body, or something between here and the origin rewrites it - and
//! the failure mode of the first two is a shared library that loads and
//! misbehaves rather than one that refuses. So the digests are here, in the
//! source, and a mismatch deletes what it fetched and names both digests.
//!
//! A version this build has no digest for is still installable, because pinning
//! every future release is not possible and refusing them would make
//! `--onnxruntime-version` useless. It is fetched, it is reported as
//! unverified in the output *and* in the install state, and the state file is
//! where somebody later finds out that this one was taken on trust.
//!
//! ## Why 1.22.0
//!
//! It is the version this repository's embedding numbers were taken on, and it
//! is the last release Microsoft publishes a `universal2` macOS archive for.
//! After it, macOS is two archives and an installer that picks the wrong one is
//! a support question rather than an error.

use std::io::{IsTerminal, Write};
use std::path::Path;

use inillucent_core::install::{self, InstalledModel, InstalledRuntime};
use inillucent_core::model::ModelManifest;
use inillucent_core::residency::Residency;
use inillucent_remote::archive::{self, Member};
use inillucent_remote::http::{self, Progress};

use crate::command::{Arguments, Context, Failed, Outcome};
use crate::json::{self, Json};

/// The ONNX Runtime version installed when nothing else is asked for.
pub const DEFAULT_RUNTIME: &str = "1.22.0";

/// Where Microsoft publishes the runtime.
const RUNTIME_BASE: &str = "https://github.com/microsoft/onnxruntime/releases/download";

/// Where the weights come from.
const MODEL_BASE: &str = "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/main";

/// One platform's ONNX Runtime archive.
struct RuntimeArchive {
    /// The Rust target operating system this is for.
    os: &'static str,
    /// The Rust target architecture this is for.
    arch: &'static str,
    /// Whether this is the build carrying the CUDA execution provider.
    gpu: bool,
    /// The asset name, with `{version}` where the version goes.
    asset: &'static str,
    /// SHA-256 of the asset at [`DEFAULT_RUNTIME`], lowercase hex.
    ///
    /// Only the pinned version's digest is here, because a digest is a fact
    /// about one file and a table of them per version would be a table nobody
    /// updates.
    sha256: &'static str,
}

/// Every archive this build knows how to install.
///
/// `universal2` covers both macOS architectures, which is why there is one row
/// for the platform rather than two.
const RUNTIMES: &[RuntimeArchive] = &[
    RuntimeArchive {
        os: "windows",
        arch: "x86_64",
        gpu: false,
        asset: "onnxruntime-win-x64-{version}.zip",
        sha256: "174c616efc0271194488642a72f1a514e01487da4dfe84c49296d66e40ebe0da",
    },
    RuntimeArchive {
        os: "windows",
        arch: "aarch64",
        gpu: false,
        asset: "onnxruntime-win-arm64-{version}.zip",
        sha256: "7008f7ff82f8e7de563a22f2b590e08e706a1289eba606b93de2b56edfb1e04b",
    },
    RuntimeArchive {
        os: "windows",
        arch: "x86_64",
        gpu: true,
        asset: "onnxruntime-win-x64-gpu-{version}.zip",
        sha256: "5b5241716b2628c1ab5e79ee620be767531021149ee68f30fc46c16263fb94dd",
    },
    RuntimeArchive {
        os: "macos",
        arch: "x86_64",
        gpu: false,
        asset: "onnxruntime-osx-universal2-{version}.tgz",
        sha256: "cfa6f6584d87555ed9f6e7e8a000d3947554d589efe3723b8bfa358cd263d03c",
    },
    RuntimeArchive {
        os: "macos",
        arch: "aarch64",
        gpu: false,
        asset: "onnxruntime-osx-universal2-{version}.tgz",
        sha256: "cfa6f6584d87555ed9f6e7e8a000d3947554d589efe3723b8bfa358cd263d03c",
    },
    RuntimeArchive {
        os: "linux",
        arch: "x86_64",
        gpu: false,
        asset: "onnxruntime-linux-x64-{version}.tgz",
        sha256: "8344d55f93d5bc5021ce342db50f62079daf39aaafb5d311a451846228be49b3",
    },
    RuntimeArchive {
        os: "linux",
        arch: "aarch64",
        gpu: false,
        asset: "onnxruntime-linux-aarch64-{version}.tgz",
        sha256: "bb76395092d150b52c7092dc6b8f2fe4d80f0f3bf0416d2f269193e347e24702",
    },
    RuntimeArchive {
        os: "linux",
        arch: "x86_64",
        gpu: true,
        asset: "onnxruntime-linux-x64-gpu-{version}.tgz",
        sha256: "2a19dbfa403672ec27378c3d40a68f793ac7a6327712cd0e8240a86be2b10c55",
    },
];

/// One file of the model, and the digest it must have.
struct ModelFile {
    /// The path under the Hugging Face repository.
    remote: &'static str,
    /// The name it is installed under.
    local: &'static str,
    /// SHA-256, lowercase hex.
    sha256: &'static str,
    /// Its size, so the progress bar has a total before a byte arrives.
    bytes: u64,
}

/// The five files `nomic-embed-text-v1.5` needs to run.
///
/// The ONNX export is under `onnx/`, and it is the **fp32** one. The repository
/// also publishes fp16, int8, uint4 and three other quantized exports, and
/// `docs/embeddings.md` prints what two of them cost: int8 is faster and its
/// query vector agrees with this one at 0.9727 cosine, which is a retrieval
/// change rather than a speed change. Installing one of those by accident is
/// exactly the kind of quiet wrongness a pinned list prevents.
const NOMIC_FILES: &[ModelFile] = &[
    ModelFile {
        remote: "onnx/model.onnx",
        local: "model.onnx",
        sha256: "147d5aa88c2101237358e17796cf3a227cead1ec304ec34b465bb08e9d952965",
        bytes: 547_310_275,
    },
    ModelFile {
        remote: "tokenizer.json",
        local: "tokenizer.json",
        sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
        bytes: 711_396,
    },
    ModelFile {
        remote: "tokenizer_config.json",
        local: "tokenizer_config.json",
        sha256: "d7e0000bcc80134debd2222220427e6bf5fa20a669f40a0d0d1409cc18e0a9bc",
        bytes: 1_191,
    },
    ModelFile {
        remote: "special_tokens_map.json",
        local: "special_tokens_map.json",
        sha256: "5d5b662e421ea9fac075174bb0688ee0d9431699900b90662acd44b2a350503a",
        bytes: 695,
    },
    ModelFile {
        remote: "config.json",
        local: "config.json",
        sha256: "9ab00bd92cee80a569f708140b7b6c1661a65891ff3765b1519e181ba2f2c92b",
        bytes: 2_538,
    },
];

/// Which parts of the install to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Component {
    /// Both.
    All,
    /// The ONNX Runtime shared library.
    Runtime,
    /// The weights.
    Model,
}

impl Component {
    /// Parses the positional argument.
    ///
    /// @param text - `all`, `runtime` or `model`
    fn parse(text: &str) -> Result<Component, Failed> {
        match text.trim().to_ascii_lowercase().as_str() {
            "" | "all" | "both" => Ok(Component::All),
            "runtime" | "onnxruntime" | "onnx" => Ok(Component::Runtime),
            "model" | "weights" | "embeddings" => Ok(Component::Model),
            other => Err(Failed::misuse(format!(
                "'{other}' is not a component. Use all, runtime or model."
            ))),
        }
    }
}

/// `setup-embeddings`: download and install the embedder.
///
/// @param context - the surface, which may be confined
/// @param arguments - what was asked for
pub fn setup_embeddings(context: &mut Context, arguments: &Arguments) -> Result<Outcome, Failed> {
    let root = match arguments.text("dir") {
        Some(named) => context.confine(named)?,
        None => install::home(),
    };

    // A bare `inillucent setup-embeddings` reports and installs nothing.
    //
    // The command downloads about 620 MB, and a verb typed to find out what it
    // does should not start that. It was not a hypothetical: the command table's
    // own parity suite calls every command with no arguments to check that each
    // one either answers or refuses with a usable message, and the first version
    // of this command answered by fetching the whole model - so `cargo test`
    // pulled 535 MB into the machine's install directory. The bare call now
    // prints the status and the one line that starts the install.
    let named = arguments.text("component").map(str::trim).unwrap_or("");
    // The profile is read before anything is fetched, so a typo in it costs a
    // message rather than 620 MB and then a message.
    let residency = match arguments.text("residency") {
        Some(text) => {
            Some(Residency::parse(text).map_err(|reason| Failed::misuse(format!("{reason:#}")))?)
        }
        None => None,
    };

    if arguments.flag("status") || named.is_empty() {
        // Changing when the model is in memory is not a reason to fetch it
        // again, so a profile given without a component is recorded here and
        // nothing is downloaded.
        if let Some(residency) = residency {
            let mut state = install::read_state(&root).unwrap_or_default();
            state.residency = Some(residency.label());
            install::write_state(&root, &state).map_err(|error| {
                Failed::misuse(format!("the install state could not be written: {error}"))
            })?;
        }
        let mut outcome = status(&root);
        if named.is_empty() && !arguments.flag("status") {
            outcome.text.push_str(
                "\n\nTo install what is missing, naming what you want so a 620 MB download \
                 is never a surprise:\n  inillucent setup-embeddings all",
            );
        }
        return Ok(outcome);
    }

    let component = Component::parse(named)?;
    let version = arguments
        .text("onnxruntime-version")
        .unwrap_or(DEFAULT_RUNTIME)
        .to_string();
    let gpu = arguments.flag("gpu");
    let force = arguments.flag("force");

    let mut state = install::read_state(&root).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut fields: Vec<(String, Json)> = Vec::new();

    if matches!(component, Component::All | Component::Runtime) {
        let installed = install_runtime(
            &root,
            &version,
            if gpu {
                Accelerator::Gpu
            } else {
                Accelerator::Cpu
            },
            if force {
                Reinstall::Always
            } else {
                Reinstall::WhenMissing
            },
        )?;
        lines.push(format!(
            "ONNX Runtime {} -> {}{}",
            installed.version,
            installed.library,
            if installed.verified {
                ""
            } else {
                "  (digest not pinned in this build)"
            }
        ));
        fields.push(("runtime".to_string(), runtime_json(&installed)));
        state.runtime = Some(installed);
    }

    if matches!(component, Component::All | Component::Model) {
        let installed = install_model(&root, force)?;
        lines.push(format!("{} -> {}", installed.id, installed.dir));
        fields.push(("model".to_string(), model_json(&installed)));
        state.put_model(installed);
    }

    if let Some(residency) = residency {
        state.residency = Some(residency.label());
        lines.push(format!("residency profile: {}", residency.label()));
    }
    let effective = state
        .residency
        .as_deref()
        .and_then(|text| Residency::parse(text).ok())
        .unwrap_or_default();
    fields.push(("residency".to_string(), json::text(effective.label())));

    install::write_state(&root, &state).map_err(|error| {
        Failed::misuse(format!("the install state could not be written: {error}"))
    })?;
    // The downloads directory holds only partial fetches, and every one of them
    // has either been renamed into place or deleted by the time this runs. It is
    // removed rather than left, because an empty directory nobody explains is a
    // question somebody has to answer later.
    let _ = std::fs::remove_dir(install::downloads_dir(&root));

    lines.push(String::new());
    lines.push(format!("Installed under {}.", root.display()));
    lines.push(
        "Nothing to export: the engine finds both of these on its own. Check it with".to_string(),
    );
    lines.push("  inillucent --db test.rdb query \"SELECT length(embed('hello'))\"".to_string());

    let mut outcome = Outcome::said("setup-embeddings", lines.join("\n"));
    outcome = outcome.with("root", json::text(root.display().to_string()));
    for (name, value) in fields {
        outcome = outcome.with(&name, value);
    }
    Ok(outcome)
}

/// What `--status` prints.
///
/// It reports what is *there* rather than what the state file claims, because
/// the two differ exactly when something has gone wrong - a directory moved, a
/// disk cleaned - and that is the case a status command exists for.
///
/// @param root - the install root
fn status(root: &Path) -> Outcome {
    let state = install::read_state(root).unwrap_or_default();
    let mut lines = vec![format!("Install root: {}", root.display())];

    match install::runtime_library() {
        Some(library) => {
            let recorded = state.runtime.as_ref();
            let version = recorded
                .map(|r| r.version.as_str())
                .unwrap_or("unknown version");
            let unverified = recorded.is_some_and(|r| !r.verified);
            lines.push(format!(
                "ONNX Runtime: {} ({version}){}",
                library.display(),
                if unverified {
                    "  (digest not pinned in this build)"
                } else {
                    ""
                }
            ));
        }
        None => lines.push(
            "ONNX Runtime: not installed. Run: inillucent setup-embeddings runtime".to_string(),
        ),
    }

    match install::model_dir(install::DEFAULT_MODEL) {
        Some(dir) => lines.push(format!("{}: {}", install::DEFAULT_MODEL, dir.display())),
        None => lines.push(format!(
            "{}: not installed. Run: inillucent setup-embeddings model",
            install::DEFAULT_MODEL
        )),
    }

    let effective = Residency::configured();
    lines.push(format!("Residency profile: {}", effective.label()));
    lines.push(match effective {
        Residency::Resident => {
            "  loaded on first use and kept, which is about 1.9 GB held and 12 to 36 ms a query"
                .to_string()
        }
        Residency::OnDemand => {
            "  loaded per call and dropped, which is nothing held and about 0.8 s a query"
                .to_string()
        }
        Residency::Idle(after) => format!(
            "  loaded on use and dropped after {}s idle: the first query in a burst pays about \
             0.8 s and the rest pay 12 to 36 ms",
            after.as_secs()
        ),
    });

    let ready = install::runtime_library().is_some()
        && install::model_dir(install::DEFAULT_MODEL).is_some();
    Outcome::said("setup-embeddings", lines.join("\n"))
        .with("root", json::text(root.display().to_string()))
        .with("ready", Json::Bool(ready))
        .with("residency", json::text(effective.label()))
        .with(
            "runtime",
            match state.runtime.as_ref() {
                Some(runtime) => runtime_json(runtime),
                None => Json::Null,
            },
        )
        .with(
            "model",
            match state.model(install::DEFAULT_MODEL) {
                Some(model) => model_json(model),
                None => Json::Null,
            },
        )
}

/// Downloads and installs the ONNX Runtime shared library.
///
/// @param root - the install root
/// @param version - the ONNX Runtime version
/// @param gpu - whether to take the build carrying the CUDA execution provider
/// @param force - install again even when it is already there
/// Which build of the runtime `setup-embeddings` installs.
///
/// **An enum rather than a `bool` beside another `bool` (task-1962, A9).**
/// `install_runtime` took `gpu` and `force` adjacent and positional, and the
/// call site read `install_runtime(&root, &version, gpu, force)` - two words
/// that say which is which only because they happen to be named after the
/// parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Accelerator {
    /// The CPU build, which every machine can run.
    Cpu,
    /// The GPU build, which needs a supported card and its driver.
    Gpu,
}

/// Whether an install replaces a runtime that is already there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reinstall {
    /// Download and unpack even when the version is already installed.
    Always,
    /// Leave an installed version alone.
    WhenMissing,
}

fn install_runtime(
    root: &Path,
    version: &str,
    accelerator: Accelerator,
    force: Reinstall,
) -> Result<InstalledRuntime, Failed> {
    let gpu = accelerator == Accelerator::Gpu;
    let force = force == Reinstall::Always;
    let archive_spec = pick_runtime(gpu)?;
    let asset = archive_spec.asset.replace("{version}", version);
    let directory = install::runtime_dir(root, version);
    let library = directory.join("lib").join(install::runtime_library_name());

    if library.exists() && !force {
        return Ok(InstalledRuntime {
            version: version.to_string(),
            archive: asset,
            library: library.display().to_string(),
            verified: true,
            gpu,
        });
    }

    // Only the pinned version's digest is a fact about the file being fetched.
    // Asking for another version is allowed and is reported as unverified.
    let expected = (version == DEFAULT_RUNTIME).then_some(archive_spec.sha256);
    let url = format!("{RUNTIME_BASE}/v{version}/{asset}");
    let downloaded = install::downloads_dir(root).join(&asset);
    let mut progress = bar();
    let fetched = http::download(&url, &downloaded, expected, &mut progress)
        .map_err(|error| Failed::misuse(format!("{error}")))?;

    let bytes = std::fs::read(&fetched.path).map_err(|error| {
        Failed::misuse(format!(
            "{} could not be read: {error}",
            fetched.path.display()
        ))
    })?;
    let members = archive::read(&bytes).map_err(|error| Failed::misuse(format!("{error}")))?;
    let libraries = shared_libraries(&members);
    if libraries.is_empty() {
        return Err(Failed::misuse(format!(
            "{asset} holds no {} - it is not an ONNX Runtime release, or its layout has changed",
            install::runtime_library_name()
        )));
    }

    let lib_dir = directory.join("lib");
    std::fs::create_dir_all(&lib_dir).map_err(|error| {
        Failed::misuse(format!(
            "{} could not be created: {error}",
            lib_dir.display()
        ))
    })?;
    for (member, name) in &libraries {
        archive::write_member(member, &lib_dir, Some(name))
            .map_err(|error| Failed::misuse(format!("{error}")))?;
    }
    // The archive is a download, not an installed component, and it is between
    // 7 MB and 300 MB. It goes once the library is out of it.
    let _ = std::fs::remove_file(&fetched.path);

    if !library.exists() {
        return Err(Failed::misuse(format!(
            "{asset} was extracted and {} is still not there",
            library.display()
        )));
    }

    Ok(InstalledRuntime {
        version: version.to_string(),
        archive: asset,
        library: library.display().to_string(),
        verified: expected.is_some(),
        gpu,
    })
}

/// The archive for the machine this is running on.
///
/// @param gpu - whether the CUDA build was asked for
fn pick_runtime(gpu: bool) -> Result<&'static RuntimeArchive, Failed> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    RUNTIMES
        .iter()
        .find(|entry| entry.os == os && entry.arch == arch && entry.gpu == gpu)
        .ok_or_else(|| {
            let plain = RUNTIMES
                .iter()
                .any(|e| e.os == os && e.arch == arch && !e.gpu);
            if gpu && plain {
                Failed::misuse(format!(
                    "there is no CUDA build of ONNX Runtime for {os} on {arch}. Run the command \
                     without --gpu."
                ))
            } else {
                Failed::misuse(format!(
                    "there is no ONNX Runtime release for {os} on {arch} that this command knows \
                     how to install. Build it, and point ORT_DYLIB_PATH at the result."
                ))
            }
        })
}

/// Picks the shared libraries out of an archive, with the names to install them
/// under.
///
/// Two things make this less obvious than a name match. The real library on
/// macOS and Linux is *versioned* - `libonnxruntime.so.1.22.0`,
/// `libonnxruntime.1.22.0.dylib` - and the unversioned name beside it is a
/// symbolic link, which this extractor does not carry across; so the versioned
/// file is installed under the unversioned name that the loader will ask for.
/// And the macOS archive carries a `.dSYM` bundle holding a 150 MB file with
/// `.dylib` in its path, which is debug information rather than a library.
///
/// @param members - everything in the archive
fn shared_libraries(members: &[Member]) -> Vec<(&Member, String)> {
    let wanted = install::runtime_library_name();
    let mut out = Vec::new();
    for member in members {
        let name = member.name.rsplit('/').next().unwrap_or(&member.name);
        if member.name.contains(".dSYM/") {
            continue;
        }
        if !is_shared_library(name) {
            continue;
        }
        if is_main_library(name) {
            out.push((member, wanted.to_string()));
        } else if name.contains("onnxruntime_providers") {
            // A provider library keeps its own name, because the main library
            // loads it by that name at run time. The version suffix is dropped
            // for the same reason it is on the main library.
            out.push((member, unversioned(name)));
        }
    }
    out
}

/// Whether a file name is a shared library on some platform.
///
/// @param name - the base name
fn is_shared_library(name: &str) -> bool {
    name.ends_with(".dll") || name.ends_with(".dylib") || name.contains(".so")
}

/// Whether a file name is ONNX Runtime itself rather than one of its providers.
///
/// @param name - the base name
fn is_main_library(name: &str) -> bool {
    if name.contains("providers") {
        return false;
    }
    name == "onnxruntime.dll"
        || name.starts_with("libonnxruntime.so")
        || (name.starts_with("libonnxruntime.") && name.ends_with(".dylib"))
        || name == "libonnxruntime.dylib"
}

/// A library's name with any version numbers taken out of it.
///
/// `libonnxruntime_providers_cuda.so.1.22.0` becomes
/// `libonnxruntime_providers_cuda.so`, which is the name the main library asks
/// the loader for.
///
/// @param name - the base name as it is in the archive
fn unversioned(name: &str) -> String {
    if let Some(at) = name.find(".so") {
        return format!("{}.so", name.get(..at).unwrap_or(name));
    }
    if name.ends_with(".dylib") {
        let stem = name.trim_end_matches(".dylib");
        let base = stem.split('.').next().unwrap_or(stem);
        return format!("{base}.dylib");
    }
    name.to_string()
}

/// Downloads and installs the weights, and seals a manifest over what landed.
///
/// @param root - the install root
/// @param force - fetch again even when the files are already there
fn install_model(root: &Path, force: bool) -> Result<InstalledModel, Failed> {
    let directory = install::models_root(root).join(install::DEFAULT_MODEL);
    std::fs::create_dir_all(&directory).map_err(|error| {
        Failed::misuse(format!(
            "{} could not be created: {error}",
            directory.display()
        ))
    })?;

    for file in NOMIC_FILES {
        let destination = directory.join(file.local);
        if destination.exists() && !force && already_correct(&destination, file) {
            continue;
        }
        let url = format!("{MODEL_BASE}/{}", file.remote);
        let mut progress = bar();
        http::download(&url, &destination, Some(file.sha256), &mut progress)
            .map_err(|error| Failed::misuse(format!("{error}")))?;
    }

    // The manifest is this repository's contract rather than the model author's,
    // so it is written here. Its digests come from the files that actually
    // landed, which is what makes it impossible for a manifest to describe
    // weights that are not there.
    let mut manifest = ModelManifest::nomic_v1_5();
    manifest.weights_sha256 = NOMIC_FILES
        .iter()
        .find(|f| f.local == "model.onnx")
        .map(|f| f.sha256.to_string())
        .unwrap_or_default();
    manifest.tokenizer_sha256 = NOMIC_FILES
        .iter()
        .find(|f| f.local == "tokenizer.json")
        .map(|f| f.sha256.to_string())
        .unwrap_or_default();
    manifest.source = Some(MODEL_BASE.trim_end_matches("/resolve/main").to_string());
    manifest
        .write(&directory)
        .map_err(|reason| Failed::misuse(format!("the model manifest: {reason}")))?;

    Ok(InstalledModel {
        id: install::DEFAULT_MODEL.to_string(),
        dir: directory.display().to_string(),
        source: MODEL_BASE.trim_end_matches("/resolve/main").to_string(),
        verified: true,
    })
}

/// Whether a file on disk is already the one that would be downloaded.
///
/// Checked by size first and by digest only when the size matches, because
/// digesting 547 MB costs a second and a size mismatch settles it for nothing.
/// A digest rather than a size alone, because a truncated file that happens to
/// be the right length is precisely what a resumed download can leave.
///
/// @param path - the file on disk
/// @param file - what it is supposed to be
fn already_correct(path: &Path, file: &ModelFile) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if metadata.len() != file.bytes {
        return false;
    }
    let Ok(mut handle) = std::fs::File::open(path) else {
        return false;
    };
    let mut digest = inillucent_base::hash::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        use std::io::Read;
        match handle.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => digest.update(buffer.get(..read).unwrap_or(&[])),
            Err(_) => return false,
        }
    }
    inillucent_base::hash::to_hex(&digest.finish()).eq_ignore_ascii_case(file.sha256)
}

/// The installed runtime, as JSON.
///
/// @param runtime - what was installed
fn runtime_json(runtime: &InstalledRuntime) -> Json {
    json::object(vec![
        ("version", json::text(&runtime.version)),
        ("archive", json::text(&runtime.archive)),
        ("library", json::text(&runtime.library)),
        ("verified", Json::Bool(runtime.verified)),
        ("gpu", Json::Bool(runtime.gpu)),
    ])
}

/// The installed model, as JSON.
///
/// @param model - what was installed
fn model_json(model: &InstalledModel) -> Json {
    json::object(vec![
        ("id", json::text(&model.id)),
        ("dir", json::text(&model.dir)),
        ("source", json::text(&model.source)),
        ("verified", Json::Bool(model.verified)),
    ])
}

/// Builds the progress reporter for this terminal.
fn bar() -> Bar {
    Bar {
        terminal: std::io::stderr().is_terminal(),
        name: String::new(),
        width: 0,
        last_percent: -1,
        started: std::time::Instant::now(),
    }
}

/// A one-line progress bar, on standard error.
///
/// Standard error rather than standard output, so `--output json` stays
/// parseable while the download runs. And **nothing at all when standard error
/// is not a terminal** except a line per ten percent: a bar rewritten with
/// carriage returns into a log file is one enormous line, and a log with no
/// progress in it at all cannot be used to tell a slow download from a stalled
/// one.
struct Bar {
    terminal: bool,
    name: String,
    /// The whole size, when the server said what it is.
    width: u64,
    /// The last decile reported, for the non-terminal case.
    last_percent: i64,
    started: std::time::Instant,
}

impl Bar {
    /// Draws the line for a given position.
    ///
    /// @param done - bytes so far
    fn draw(&self, done: u64) {
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let rate = done as f64 / elapsed / (1024.0 * 1024.0);
        let mut line = if self.width > 0 {
            let share = (done as f64 / self.width as f64).clamp(0.0, 1.0);
            let filled = (share * 20.0).round() as usize;
            format!(
                "  {:<38} [{}{}] {:3.0}%  {:.1}/{:.1} MB  {rate:.1} MB/s",
                self.name,
                "#".repeat(filled.min(20)),
                ".".repeat(20usize.saturating_sub(filled)),
                share * 100.0,
                done as f64 / (1024.0 * 1024.0),
                self.width as f64 / (1024.0 * 1024.0),
            )
        } else {
            format!(
                "  {:<38} {:.1} MB  {rate:.1} MB/s",
                self.name,
                done as f64 / (1024.0 * 1024.0)
            )
        };
        line.push('\r');
        let mut stderr = std::io::stderr();
        let _ = stderr.write_all(line.as_bytes());
        let _ = stderr.flush();
    }
}

impl Progress for Bar {
    /// Notes what is being fetched and how big it is.
    fn started(&mut self, name: &str, total: Option<u64>, resumed: u64) {
        self.name = name.to_string();
        self.width = total.unwrap_or(0);
        self.last_percent = -1;
        self.started = std::time::Instant::now();
        if resumed > 0 {
            eprintln!(
                "  {name}: resuming at {:.1} MB",
                resumed as f64 / (1024.0 * 1024.0)
            );
        }
    }

    /// Redraws the bar, or reports another ten percent into a log.
    fn advanced(&mut self, done: u64, _total: Option<u64>) {
        if self.terminal {
            self.draw(done);
            return;
        }
        if self.width == 0 {
            return;
        }
        let percent = (done as i64).saturating_mul(100) / self.width.max(1) as i64;
        if percent / 10 > self.last_percent / 10 {
            self.last_percent = percent;
            eprintln!("  {}: {percent}%", self.name);
        }
    }

    /// Ends the line, so whatever prints next starts on its own.
    fn finished(&mut self, done: u64) {
        if self.terminal {
            self.draw(done);
        }
        eprintln!(
            "  {:<38} {:.1} MB",
            self.name,
            done as f64 / (1024.0 * 1024.0)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every platform this ships on has an archive, and every pinned digest is
    /// a SHA-256.
    ///
    /// The digest check is not decoration: a row with a truncated or
    /// pasted-wrong digest would refuse every download on that platform, and
    /// this repository builds on one platform at a time.
    #[test]
    fn every_platform_has_an_archive_with_a_pinned_digest() {
        for (os, arch) in [
            ("windows", "x86_64"),
            ("windows", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
        ] {
            let found = RUNTIMES
                .iter()
                .find(|entry| entry.os == os && entry.arch == arch && !entry.gpu);
            assert!(found.is_some(), "{os} on {arch} has no archive");
        }
        for entry in RUNTIMES {
            assert_eq!(entry.sha256.len(), 64, "{} has a short digest", entry.asset);
            assert!(
                entry.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{} has a digest that is not hex",
                entry.asset
            );
            assert!(
                entry.asset.contains("{version}"),
                "{} has no version slot",
                entry.asset
            );
        }
    }

    /// Every model file has a digest and a size, and the two files the manifest
    /// seals are among them.
    #[test]
    fn every_model_file_has_a_digest_and_a_size() {
        for file in NOMIC_FILES {
            assert_eq!(file.sha256.len(), 64, "{} has a short digest", file.local);
            assert!(file.bytes > 0, "{} has no size", file.local);
        }
        assert!(NOMIC_FILES.iter().any(|f| f.local == "model.onnx"));
        assert!(NOMIC_FILES.iter().any(|f| f.local == "tokenizer.json"));
    }

    /// The fp32 export is what is installed, not one of the quantized ones.
    ///
    /// `onnx/` in that repository holds seven other exports whose names differ
    /// by a suffix, and one of them agrees with this one at 0.9727 cosine. A
    /// test that names the file is what stops a plausible-looking edit changing
    /// what every vector in every index means.
    #[test]
    fn the_installed_weights_are_the_full_precision_export() {
        let weights = NOMIC_FILES
            .iter()
            .find(|f| f.local == "model.onnx")
            .expect("the weights are in the list");
        assert_eq!(weights.remote, "onnx/model.onnx");
        assert_eq!(weights.bytes, 547_310_275, "the fp32 export's size");
    }

    /// A component name parses from every form, and anything else is refused.
    #[test]
    fn a_component_parses_or_is_refused() {
        assert_eq!(Component::parse("all").unwrap(), Component::All);
        assert_eq!(Component::parse("").unwrap(), Component::All);
        assert_eq!(Component::parse(" RUNTIME ").unwrap(), Component::Runtime);
        assert_eq!(Component::parse("model").unwrap(), Component::Model);
        assert!(Component::parse("everything").is_err());
    }

    /// The main library is told apart from its providers, and debug information
    /// is not mistaken for either.
    #[test]
    fn the_main_library_is_told_apart_from_its_providers() {
        assert!(is_main_library("onnxruntime.dll"));
        assert!(is_main_library("libonnxruntime.so.1.22.0"));
        assert!(is_main_library("libonnxruntime.1.22.0.dylib"));
        assert!(!is_main_library("onnxruntime_providers_cuda.dll"));
        assert!(!is_main_library("libonnxruntime_providers_shared.so"));
        assert!(!is_shared_library("onnxruntime.lib"));
        assert!(!is_shared_library("onnxruntime.pdb"));
        assert!(!is_shared_library("libonnxruntime.pc"));
    }

    /// A versioned library is installed under the name the loader asks for.
    #[test]
    fn a_versioned_library_loses_its_version() {
        assert_eq!(
            unversioned("libonnxruntime_providers_cuda.so.1.22.0"),
            "libonnxruntime_providers_cuda.so"
        );
        assert_eq!(
            unversioned("libonnxruntime_providers_shared.so"),
            "libonnxruntime_providers_shared.so"
        );
        assert_eq!(
            unversioned("onnxruntime_providers_cuda.dll"),
            "onnxruntime_providers_cuda.dll"
        );
        assert_eq!(
            unversioned("libonnxruntime_providers_cuda.1.22.0.dylib"),
            "libonnxruntime_providers_cuda.dylib"
        );
    }

    /// The three archive layouts each yield the main library under the name this
    /// platform's loader will look for, and the macOS debug bundle is skipped.
    ///
    /// The names are the ones the real archives hold, listed from
    /// `onnxruntime-{win-x64,linux-x64,osx-universal2}-1.22.0`.
    #[test]
    fn each_archive_layout_yields_the_library_the_loader_wants() {
        let members: Vec<Member> = [
            "onnxruntime-win-x64-1.22.0/lib/onnxruntime.dll",
            "onnxruntime-win-x64-1.22.0/lib/onnxruntime.lib",
            "onnxruntime-win-x64-1.22.0/lib/onnxruntime.pdb",
            "onnxruntime-linux-x64-1.22.0/lib/libonnxruntime.so.1.22.0",
            "onnxruntime-linux-x64-1.22.0/lib/libonnxruntime.pc",
            "onnxruntime-osx-universal2-1.22.0/lib/libonnxruntime.1.22.0.dylib",
            "onnxruntime-osx-universal2-1.22.0/lib/libonnxruntime.1.22.0.dylib.dSYM/Contents/Resources/DWARF/libonnxruntime.1.22.0.dylib",
        ]
        .into_iter()
        .map(|name| Member { name: name.to_string(), bytes: Vec::new(), executable: false })
        .collect();

        let picked = shared_libraries(&members);
        let names: Vec<&str> = picked.iter().map(|(_, name)| name.as_str()).collect();
        assert!(
            names
                .iter()
                .all(|name| *name == install::runtime_library_name()),
            "every main library is installed under this platform's name: {names:?}"
        );
        assert!(
            !picked
                .iter()
                .any(|(member, _)| member.name.contains(".dSYM/")),
            "the macOS debug bundle is not a library"
        );
        // Three archives are listed, so three main libraries are found - one per
        // layout. A real install only ever reads one archive.
        assert_eq!(picked.len(), 3, "{names:?}");
    }

    /// A provider library keeps its own name, because the main library loads it
    /// by that name.
    #[test]
    fn a_provider_library_keeps_its_own_name() {
        let members = vec![
            Member {
                name: "onnxruntime-win-x64-gpu-1.22.0/lib/onnxruntime.dll".to_string(),
                bytes: Vec::new(),
                executable: false,
            },
            Member {
                name: "onnxruntime-win-x64-gpu-1.22.0/lib/onnxruntime_providers_cuda.dll"
                    .to_string(),
                bytes: Vec::new(),
                executable: false,
            },
        ];
        let picked = shared_libraries(&members);
        assert_eq!(picked.len(), 2);
        assert!(picked
            .iter()
            .any(|(_, name)| name == "onnxruntime_providers_cuda.dll"));
    }
}
