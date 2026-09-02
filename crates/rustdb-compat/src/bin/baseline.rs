//! Captures - and later verifies - the untouched retrieval baseline.
//!
//! Invariant: the retrieval engine this workspace already ships is not allowed
//! to change while the relational engine is being built underneath it. This
//! tool records what "unchanged" means as a set of digests and a test run, and
//! `--verify` re-checks the same set later.
//!
//! It deliberately does not re-run the grading harness. That run needs a corpus
//! assembled from public sources and embedded on a GPU, and it is neither
//! present in a checkout nor reproducible in minutes; the scorecard it produced
//! is checked in, so the honest baseline is that artifact plus proof that the
//! code which produced it has not moved. The command line to reproduce it is
//! recorded in the capture so a later story does not have to guess.
//!
//! Usage:
//!
//! ```text
//! rustdb-baseline capture [--out <dir>]
//! rustdb-baseline verify  [--out <dir>]
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use rustdb_compat::hash::sha256_hex;
use rustdb_compat::report::json_string;
use rustdb_compat::workspace_root;

/// The files whose contents define "the retrieval engine is unchanged".
const TRACKED_TREES: [&str; 2] = ["crates/rustdb-core/src", "crates/rustdb-bench/src"];

/// The artifacts the last grading run produced.
const TRACKED_ARTIFACTS: [&str; 4] = [
    "rust-db-scorecard.json",
    "rust-db-scorecard.md",
    "crates/rustdb-core/Cargo.toml",
    "crates/rustdb-bench/Cargo.toml",
];

/// The command that regenerates the scorecard, recorded so a later story does
/// not have to reconstruct it.
const SCORECARD_COMMAND: &str =
    "scripts/fetch-public-corpus.sh && cargo run --release -p rustdb-bench -- score --out rust-db-scorecard.json";

/// Captures or verifies the baseline.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = arguments.first().map(String::as_str).unwrap_or("capture");
    let root = workspace_root();
    // The baseline is checked in rather than left in the agent output folder:
    // a later story has to be able to run `verify` from a fresh checkout.
    let out = flag(&arguments, "--out").unwrap_or_else(|| root.join("compat/baseline"));
    let outcome = match command {
        "capture" => capture(&root, &out),
        "verify" => verify(&root, &out),
        other => Err(format!("unknown command `{other}`")),
    };
    match outcome {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--flag value` argument.
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}

/// One tracked file and its digest.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Digest {
    path: String,
    bytes: u64,
    sha256: String,
}

/// Walks the tracked trees and files and digests each one.
fn digests(root: &Path) -> Result<Vec<Digest>, String> {
    let mut found = Vec::new();
    for tree in TRACKED_TREES {
        collect(root, &root.join(tree), &mut found)?;
    }
    for artifact in TRACKED_ARTIFACTS {
        let path = root.join(artifact);
        if path.is_file() {
            found.push(digest_of(root, &path)?);
        }
    }
    found.sort();
    Ok(found)
}

/// Adds every file under a directory to the digest list.
fn collect(root: &Path, directory: &Path, found: &mut Vec<Digest>) -> Result<(), String> {
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect(root, &path, found)?;
            continue;
        }
        found.push(digest_of(root, &path)?);
    }
    Ok(())
}

/// Digests one file, recording its path relative to the workspace root.
fn digest_of(root: &Path, path: &Path) -> Result<Digest, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    Ok(Digest {
        path: relative,
        bytes: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
    })
}

/// Runs the retrieval engine's own tests and returns the summary line.
fn run_core_tests(root: &Path) -> Result<(usize, usize), String> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()))
        .current_dir(root)
        .args(["test", "-p", "rustdb-core", "--no-fail-fast"])
        .output()
        .map_err(|error| format!("cannot run the retrieval tests: {error}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut passed = 0usize;
    let mut failed = 0usize;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        match rest.rsplit_once(" ... ").map(|(_, verdict)| verdict.trim()) {
            Some("ok") => passed = passed.saturating_add(1),
            Some("FAILED") => failed = failed.saturating_add(1),
            _ => {}
        }
    }
    Ok((passed, failed))
}

/// Reads the headline numbers out of the checked-in scorecard.
///
/// Only the summary line is extracted: the scorecard is over a megabyte of
/// per-query detail, and the thing a later story has to be able to compare is
/// the verdict, not the raw draws.
fn scorecard_headline(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("rust-db-scorecard.md")).ok()?;
    text.lines()
        .find(|line| line.contains("primary comparisons"))
        .map(|line| line.trim_matches('*').trim().to_string())
}

/// Writes the capture.
fn capture(root: &Path, out: &Path) -> Result<String, String> {
    let files = digests(root)?;
    let (passed, failed) = run_core_tests(root)?;
    if failed > 0 {
        return Err(format!(
            "the retrieval tests are not green: {failed} failed"
        ));
    }
    let headline =
        scorecard_headline(root).unwrap_or_else(|| "no scorecard headline found".to_string());
    std::fs::create_dir_all(out)
        .map_err(|error| format!("cannot create {}: {error}", out.display()))?;

    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!(
        "  \"captured_by\": {},\n",
        json_string("task-1782")
    ));
    json.push_str(&format!(
        "  \"platform\": {},\n",
        json_string(&rustdb_compat::platform_name())
    ));
    json.push_str(&format!("  \"core_tests_passed\": {passed},\n"));
    json.push_str(&format!("  \"core_tests_failed\": {failed},\n"));
    json.push_str(&format!(
        "  \"scorecard_headline\": {},\n",
        json_string(&headline)
    ));
    json.push_str(&format!(
        "  \"scorecard_command\": {},\n",
        json_string(SCORECARD_COMMAND)
    ));
    json.push_str("  \"files\": [\n");
    for (index, file) in files.iter().enumerate() {
        if index > 0 {
            json.push_str(",\n");
        }
        json.push_str(&format!(
            "    {{\"path\": {}, \"bytes\": {}, \"sha256\": {}}}",
            json_string(&file.path),
            file.bytes,
            json_string(&file.sha256)
        ));
    }
    json.push_str("\n  ]\n}\n");
    std::fs::write(out.join("rustdb-core-baseline.json"), &json)
        .map_err(|error| format!("cannot write the baseline: {error}"))?;

    let mut markdown = String::new();
    markdown.push_str("# rustdb-core baseline, captured by task-1782\n\n");
    markdown.push_str(
        "The retrieval engine as it stood before any relational work. `rustdb-baseline verify`\n",
    );
    markdown.push_str(
        "re-checks every digest below, so a later story can prove it did not disturb this.\n\n",
    );
    markdown.push_str(&format!(
        "- retrieval tests: **{passed} passed, {failed} failed**\n"
    ));
    markdown.push_str(&format!("- scorecard verdict: {headline}\n"));
    markdown.push_str(&format!(
        "- regenerate the scorecard with: `{SCORECARD_COMMAND}`\n"
    ));
    markdown.push_str(&format!("- tracked files: {}\n\n", files.len()));
    markdown.push_str("| file | bytes | sha256 |\n|---|---|---|\n");
    for file in &files {
        markdown.push_str(&format!(
            "| `{}` | {} | `{}` |\n",
            file.path, file.bytes, file.sha256
        ));
    }
    std::fs::write(out.join("rustdb-core-baseline.md"), markdown)
        .map_err(|error| format!("cannot write the baseline: {error}"))?;

    Ok(format!(
        "captured {} files, {passed} retrieval tests green, into {}",
        files.len(),
        out.display()
    ))
}

/// Re-checks a capture against the working tree.
fn verify(root: &Path, out: &Path) -> Result<String, String> {
    let path = out.join("rustdb-core-baseline.json");
    let recorded = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let current = digests(root)?;
    let mut changed = Vec::new();
    for file in &current {
        let needle = format!("\"sha256\": \"{}\"", file.sha256);
        if !recorded.contains(&needle) {
            changed.push(file.path.clone());
        }
    }
    let recorded_count = recorded.matches("\"sha256\":").count();
    if recorded_count != current.len() {
        changed.push(format!(
            "the file set changed: {recorded_count} recorded, {} now",
            current.len()
        ));
    }
    if !changed.is_empty() {
        return Err(format!(
            "the retrieval baseline moved:\n  {}",
            changed.join("\n  ")
        ));
    }
    Ok(format!(
        "{} files unchanged since the baseline",
        current.len()
    ))
}
