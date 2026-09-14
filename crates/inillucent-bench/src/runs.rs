//! What a graded run leaves behind, beyond its aggregate numbers.
//!
//! An aggregate score card can be read but it cannot be interrogated. It says
//! nDCG moved by 0.018; it cannot say which queries moved, whether the difference
//! is larger than the noise, which chunk was returned instead of the right one, or
//! what the ranker was thinking when it chose. Answering any of those meant
//! running the whole suite again with a debugger attached, and re-judging a run
//! after correcting a relevance judgement meant re-running retrieval, which is
//! most of the cost.
//!
//! So every run now writes two things beside the card:
//!
//! - a **manifest**: the commit, the corpus, the model, the seeds, the hardware,
//!   the exact command, and every ranking setting the arm was configured with.
//!   Without it a number is not reproducible, only repeatable by whoever still
//!   remembers what they typed.
//! - a **per-query record**: one line per engine per query, with the ranking, the
//!   component scores, the latency and the metrics that query contributed to.
//!   This is what makes the paired statistics possible at all, and what lets a
//!   failure be looked at rather than guessed at.
//!
//! Both are JSON Lines beside the card, in a directory named by run identity.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// Everything needed to say what a number describes.
#[derive(Serialize, Clone)]
pub struct RunManifest {
    pub run_id: String,
    pub generated_at_unix: u64,
    /// The revision the harness was built from, and whether the tree was dirty.
    /// A dirty tree does not invalidate a run, but a number from one cannot be
    /// reproduced from the repository alone and the card has to say so.
    pub git_commit: String,
    pub git_dirty: bool,
    pub command: String,
    pub corpus: CorpusFacts,
    pub model_dir: String,
    pub model_file: String,
    /// The model's own name, which is what a card prints and what a reader
    /// compares between two runs. The directory is only where it happened to live.
    pub model_id: String,
    /// The digest of the manifest that decided the prefixes, the pooling, the
    /// width and the token bound. Two runs of "the same model" whose manifests
    /// differ are two runs of two models.
    pub model_manifest_sha256: String,
    pub model_dims: usize,
    pub model_max_tokens: usize,
    /// What the cache said it was, verbatim.
    pub cache_header: crate::corpus::CacheHeader,
    pub device: String,
    /// The database, with any password removed. A run manifest is a file people
    /// paste into tickets.
    pub database: String,
    pub seeds: BTreeMap<String, u64>,
    pub arm: BTreeMap<String, String>,
    pub host: HostFacts,
    /// Query counts per family, so a reader can see how much evidence each slice
    /// of the report rests on before reading the slice.
    pub query_counts: BTreeMap<String, usize>,
    /// The smallest difference the run was willing to call an improvement, per
    /// metric, declared here because declaring it after seeing the results would
    /// make it meaningless.
    pub practical_thresholds: BTreeMap<String, f64>,
}

#[derive(Serialize, Clone)]
pub struct CorpusFacts {
    pub chunks: usize,
    pub documents: usize,
    pub dimensions: usize,
    pub cache_path: String,
    pub cache_bytes: u64,
    /// Size and modification time rather than a hash of 750 MB. Cheap, and enough
    /// to catch the mistake this field exists for: two runs compared against each
    /// other that were not run on the same corpus.
    pub cache_modified_unix: u64,
}

#[derive(Serialize, Clone)]
pub struct HostFacts {
    pub os: String,
    pub arch: String,
    pub logical_cpus: usize,
}

/// One engine's answer to one query, with everything needed to re-judge it.
#[derive(Serialize)]
pub struct QueryRecord<'a> {
    pub family: &'a str,
    pub query_id: &'a str,
    pub query: &'a str,
    pub engine: &'a str,
    pub source: &'a str,
    pub answerable: bool,
    pub latency_ms: f64,
    pub returned: usize,
    pub hits: Vec<HitRecord>,
    /// The metrics this query contributed, by name. These are the per-query series
    /// the paired tests consume, written out so a comparison can be recomputed
    /// from the files rather than from a rerun.
    pub metrics: BTreeMap<String, f64>,
}

#[derive(Serialize)]
pub struct HitRecord {
    pub rank: usize,
    pub key: String,
    pub score: f64,
    /// The relevance grade the judgements give this hit, so a reader can see the
    /// mistake rather than infer it.
    pub grade: u8,
}

/// A run's output directory, with the files open.
pub struct RunWriter {
    dir: PathBuf,
    per_query: BufWriter<File>,
    written: usize,
}

impl RunWriter {
    /// Create `<root>/<run_id>/` and open the per-query file.
    /// @param root - where runs are collected
    /// @param run_id - this run's identity, used as the directory name
    pub fn create(root: &Path, run_id: &str) -> Result<RunWriter> {
        let dir = root.join(run_id);
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating the run directory {}", dir.display()))?;
        let per_query = BufWriter::new(File::create(dir.join("per-query.jsonl"))?);
        Ok(RunWriter { dir, per_query, written: 0 })
    }

    /// Append one engine's answer to one query.
    pub fn record(&mut self, record: &QueryRecord<'_>) -> Result<()> {
        serde_json::to_writer(&mut self.per_query, record)?;
        self.per_query.write_all(b"\n")?;
        self.written += 1;
        Ok(())
    }

    pub fn written(&self) -> usize {
        self.written
    }

    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write the manifest and flush. Called last, so a manifest on disk means the
    /// run finished: a directory holding per-query records and no manifest is a
    /// run that died, and should be read as one.
    pub fn finish(mut self, manifest: &RunManifest) -> Result<PathBuf> {
        self.per_query.flush()?;
        let path = self.dir.join("manifest.json");
        fs::write(&path, serde_json::to_string_pretty(manifest)?)?;
        Ok(self.dir)
    }
}

/// Seconds since the epoch.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A run identity that sorts chronologically and names the commit it came from.
/// @param commit - the short revision, or "unknown"
pub fn run_id(commit: &str) -> String {
    format!("{}-{}", now_unix(), &commit[..commit.len().min(8)])
}

/// The current revision and whether the working tree is dirty.
///
/// Shelling out to git rather than embedding the revision at build time, because
/// a build-time constant goes stale the moment the harness is run from a tree that
/// has been edited since it was compiled, which during a tuning session is most of
/// the time.
pub fn git_revision(repo: &Path) -> (String, bool) {
    let run = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let commit = run(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = run(&["status", "--porcelain"]).map(|s| !s.is_empty()).unwrap_or(false);
    (commit, dirty)
}

/// A connection string with its password removed, for a file people share.
pub fn redact(url: &str) -> String {
    // postgres://user:secret@host/db -> postgres://user:***@host/db
    let Some(scheme_end) = url.find("://") else { return url.to_string() };
    let (scheme, rest) = url.split_at(scheme_end + 3);
    let Some(at) = rest.find('@') else { return url.to_string() };
    let (credentials, host) = rest.split_at(at);
    match credentials.split_once(':') {
        Some((user, _)) => format!("{scheme}{user}:***{host}"),
        None => url.to_string(),
    }
}

/// The command that produced this run, with credentials and directories removed.
///
/// **The card is a tracked file, so the command line in it is published.** This
/// used to join `std::env::args()` as typed, and `--database-url` is one of the
/// arguments: the committed card carried a PostgreSQL password in clear, beside
/// the redacted copy of the same URL two rows below it, along with the absolute
/// paths of two disks on the machine that produced it (task-1946, H6).
///
/// The command is still the reproducibility record the card exists for, so it is
/// redacted rather than dropped. Each token is treated one of three ways:
///
/// - a token that parses as a URL with credentials goes through `redact`, so the
///   user, the host, the port and the database survive and the password does not;
/// - a token that looks like a path keeps its last component only, so the card
///   says `corpus.cache` rather than which drive it was on;
/// - everything else is kept as typed, because the flags and their values are
///   what makes the run repeatable.
///
/// @returns the command line, safe to commit
pub fn command_line() -> String {
    std::env::args()
        .map(|token| published_token(&token))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One command line token, as it may appear in a file people share.
///
/// @param token - the argument as typed
/// @returns the argument with a credential or a directory removed
fn published_token(token: &str) -> String {
    if token.contains("://") {
        return redact(token);
    }
    if !looks_like_a_path(token) {
        return token.to_string();
    }
    published_path(token)
}

/// A path as it may appear in a file people share: its last component only.
///
/// **The card names four files besides the command, and all four were absolute.**
/// The corpus cache, the model weights, the per-query records and the run
/// manifest each went into the card as `path.display()`, which on the machine
/// that produced the committed card meant two drive letters nobody else has
/// (task-1946, H6). The file name is what identifies the artifact; the drive it
/// sat on identifies the machine and nothing else.
///
/// @param path - the path as it was resolved
/// @returns its last component, or the path unchanged when it has none
pub fn published_path(path: &str) -> String {
    // A trailing separator would otherwise make the last component empty, and a
    // card that says nothing is worse than one that names a directory.
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => path.to_string(),
    }
}

/// Records where the corpus cache and the model weights were, by name.
///
/// The card names four files besides the command, and all four used to go in as
/// absolute paths (task-1946, H6). These two are the run's inputs; the other two
/// are its outputs and go through `note_run_files`. They live here rather than at
/// the two call sites because both callers need the same shortening and both are
/// already over the length ratchet in `crates/inillucent-compat/tests/policy.rs`.
///
/// @param provenance - the card's provenance table
/// @param cache - where the corpus cache was read from
/// @param model_dir - the directory the weights were read from
/// @param model_file - the weights file inside it
pub fn note_inputs(
    provenance: &mut BTreeMap<String, String>,
    cache: &Path,
    model_dir: &str,
    model_file: &str,
) {
    let cache = published_path(&cache.display().to_string());
    let model = published_path(model_dir);
    provenance.insert("corpus cache".into(), cache);
    provenance.insert("embedding model".into(), format!("{model}/{model_file}"));
}

/// Records where the run's per-query records and manifest are, by run rather
/// than by disk.
///
/// The directory's own name is the run id, which is a row of its own; naming it
/// again beside each file identifies the artifact without naming the disk it
/// sits on.
///
/// @param provenance - the card's provenance table
/// @param records - how many per-query lines were written
/// @param dir - the run directory
pub fn note_run_files(provenance: &mut BTreeMap<String, String>, records: usize, dir: &Path) {
    let run = published_path(&dir.display().to_string());
    provenance.insert(
        "per-query records".into(),
        format!("{records} lines in {run}/per-query.jsonl"),
    );
    provenance.insert("run manifest".into(), format!("{run}/manifest.json"));
}

/// Whether a token names a place on a disk rather than a value.
///
/// A separator is the signal, and a leading dash is the counter-signal: a flag
/// such as `--per-source` holds no separator, and a Windows drive prefix such as
/// a drive letter and a separator holds one. `cuda:0` holds a colon and no separator, so it survives
/// as typed, which matters because the device is part of what the run measured.
///
/// @param token - the argument as typed
fn looks_like_a_path(token: &str) -> bool {
    if token.starts_with('-') {
        return false;
    }
    token.contains('/') || token.contains('\\')
}

pub fn host_facts() -> HostFacts {
    HostFacts {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        logical_cpus: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_is_removed_from_a_connection_string() {
        assert_eq!(
            redact("postgres://postgres:hunter2@127.0.0.1:5433/inillucent_synth"),
            "postgres://postgres:***@127.0.0.1:5433/inillucent_synth"
        );
    }

    #[test]
    fn a_connection_string_without_a_password_is_left_alone() {
        assert_eq!(
            redact("postgres://127.0.0.1:5433/inillucent_synth"),
            "postgres://127.0.0.1:5433/inillucent_synth"
        );
        assert_eq!(redact("not a url"), "not a url");
    }

    #[test]
    fn command_line_redacts_credentials_and_paths() {
        // The argv that produced the committed card, which carried the password
        // and both drives in clear until task-1946's H6.
        let argv = [
            r"C:\dev\inillucent\target\release\inillucent-bench.exe",
            "grade",
            "--database-url",
            "postgres://postgres:hunter2@127.0.0.1:5433/inillucent_synth",
            "--cache",
            "J:/embeddings/corpus.cache",
            "--model-dir",
            "J:/embeddings/models/nomic-embed-text-v1.5",
            "--device",
            "cuda:0",
            "--per-source",
            "100",
            "--out",
            "C:/dev/inillucent/inillucent-scorecard.md",
        ];
        let published = argv
            .iter()
            .map(|token| published_token(token))
            .collect::<Vec<_>>()
            .join(" ");

        assert!(
            !published.contains("hunter2"),
            "the password survived: {published}"
        );
        assert!(
            published.contains("postgres://postgres:***@127.0.0.1:5433/inillucent_synth"),
            "the rest of the URL did not survive: {published}"
        );
        for directory in [r"C:\dev", "C:/dev", "J:/embeddings"] {
            assert!(
                !published.contains(directory),
                "{directory} survived: {published}"
            );
        }
        for kept in [
            "inillucent-bench.exe",
            "grade",
            "corpus.cache",
            "nomic-embed-text-v1.5",
            "--device",
            "cuda:0",
            "--per-source",
            "100",
            "inillucent-scorecard.md",
        ] {
            assert!(
                published.contains(kept),
                "{kept} was lost, and the command is the reproducibility record: {published}"
            );
        }
    }

    #[test]
    fn a_run_writes_its_records_and_then_its_manifest() {
        let root = std::env::temp_dir().join(format!("inillucent-runs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut w = RunWriter::create(&root, "test-run").unwrap();
        w.record(&QueryRecord {
            family: "heading",
            query_id: "q1",
            query: "how offer eligibility works",
            engine: "inillucent",
            source: "confluence",
            answerable: true,
            latency_ms: 1.5,
            returned: 2,
            hits: vec![HitRecord { rank: 1, key: "7#0".into(), score: 0.9, grade: 3 }],
            metrics: [("ndcg@10".to_string(), 1.0)].into_iter().collect(),
        })
        .unwrap();
        assert_eq!(w.written(), 1);

        let manifest = RunManifest {
            run_id: "test-run".into(),
            generated_at_unix: 0,
            git_commit: "abc".into(),
            git_dirty: false,
            command: "inillucent-bench grade".into(),
            corpus: CorpusFacts {
                chunks: 1,
                documents: 1,
                dimensions: 768,
                cache_path: "corpus.cache".into(),
                cache_bytes: 0,
                cache_modified_unix: 0,
            },
            model_dir: "m".into(),
            model_file: "model.onnx".into(),
            model_id: "nomic-embed-text-v1.5".into(),
            model_manifest_sha256: "abc".into(),
            model_dims: 768,
            model_max_tokens: 1900,
            cache_header: crate::corpus::CacheHeader {
                version: 4,
                corpus_sha256: "c".into(),
                model_id: "nomic-embed-text-v1.5".into(),
                manifest_sha256: "abc".into(),
                dims: 768,
                max_tokens: 1900,
                chunk_count: 1,
                truncated_chunks: 0,
                query_seed_digest: "s".into(),
            },
            device: "cpu".into(),
            database: "postgres://x/y".into(),
            seeds: BTreeMap::new(),
            arm: BTreeMap::new(),
            host: host_facts(),
            query_counts: BTreeMap::new(),
            practical_thresholds: BTreeMap::new(),
        };
        let dir = w.finish(&manifest).unwrap();

        let lines = fs::read_to_string(dir.join("per-query.jsonl")).unwrap();
        assert_eq!(lines.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(lines.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["engine"], "inillucent");
        assert_eq!(parsed["hits"][0]["grade"], 3);
        assert!(fs::read_to_string(dir.join("manifest.json")).unwrap().contains("test-run"));
        fs::remove_dir_all(&root).ok();
    }
}
