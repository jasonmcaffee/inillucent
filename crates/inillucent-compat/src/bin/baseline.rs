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
//! ## Amendments
//!
//! A later phase is eventually *supposed* to touch the retrieval engine -
//! phase 13 gives it a transactional home, and phase 14 optimises it - and the
//! answer to that cannot be "recapture the baseline", because a recapture
//! silently blesses whatever else happened to be in the tree at the time. So a
//! change is declared instead, in `compat/baseline/inillucent-core-amendments.toml`:
//! one entry per changed file, naming the ticket, the reason, and the digest
//! the file is allowed to have. `verify` accepts exactly those files at exactly
//! those digests and still fails on anything else, so the guard keeps its edge
//! while the engine is allowed to move deliberately.
//!
//! Usage:
//!
//! ```text
//! inillucent-baseline capture [--out <dir>]
//! inillucent-baseline verify  [--out <dir>]
//! inillucent-baseline amend <path> --ticket <task-N> --reason <text> [--out <dir>]
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use inillucent_compat::hash::sha256_hex;
use inillucent_compat::report::json_string;
use inillucent_compat::workspace_root;

/// The files whose contents define "the retrieval engine is unchanged".
const TRACKED_TREES: [&str; 2] = ["crates/inillucent-core/src", "crates/inillucent-bench/src"];

/// The artifacts the last grading run produced.
const TRACKED_ARTIFACTS: [&str; 4] = [
    "inillucent-scorecard.json",
    "inillucent-scorecard.md",
    "crates/inillucent-core/Cargo.toml",
    "crates/inillucent-bench/Cargo.toml",
];

/// The command that regenerates the scorecard, recorded so a later story does
/// not have to reconstruct it.
const SCORECARD_COMMAND: &str =
    "scripts/fetch-public-corpus.sh && cargo run --release -p inillucent-bench -- score --out inillucent-scorecard.json";

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
        "amend" => amend(&root, &out, &arguments),
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

/// Returns a file's bytes with every CRLF reduced to LF.
///
/// The guard is a statement about the retrieval engine's *content*, so it has
/// to be one the content alone decides. Hashing the working copy's raw bytes
/// made it a statement about the checkout instead: this repository is used on
/// Windows with `core.autocrlf=true`, so a file git rewrites gains a byte per
/// line and its digest changes although not one character of it did.
///
/// That is not hypothetical. Five files were reported as moved when the working
/// tree was clean and none of them had been edited since before the baseline was
/// captured - the byte difference was exactly each file's line count, and
/// `persist.rs` matched its own earlier amendment digest once the line endings
/// were normalised. The same check would also disagree with itself between this
/// machine and the Linux evidence run, which is the platform matrix the release
/// is supposed to be qualified on.
///
/// Normalising here rather than at the call sites keeps `capture`, `verify` and
/// `amend` hashing the same thing, which is the property that makes an
/// amendment recorded on one machine verify on another.
fn content_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let raw =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut normalised = Vec::with_capacity(raw.len());
    let mut index = 0usize;
    while let Some(byte) = raw.get(index) {
        // A lone CR is left alone: it is not a line ending this repository
        // produces, and silently rewriting one would hide a real change.
        if *byte == b'\r' && raw.get(index.saturating_add(1)) == Some(&b'\n') {
            index = index.saturating_add(1);
            continue;
        }
        normalised.push(*byte);
        index = index.saturating_add(1);
    }
    Ok(normalised)
}

/// Digests one file, recording its path relative to the workspace root.
///
/// The size recorded is the normalised size, for the same reason the digest is
/// of the normalised bytes: both have to mean the same thing on both platforms.
fn digest_of(root: &Path, path: &Path) -> Result<Digest, String> {
    let bytes = content_bytes(path)?;
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
        .args(["test", "-p", "inillucent-core", "--no-fail-fast"])
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
    let text = std::fs::read_to_string(root.join("inillucent-scorecard.md")).ok()?;
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
        json_string("inillucent-baseline")
    ));
    json.push_str(&format!(
        "  \"platform\": {},\n",
        json_string(&inillucent_compat::platform_name())
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
    std::fs::write(out.join("inillucent-core-baseline.json"), &json)
        .map_err(|error| format!("cannot write the baseline: {error}"))?;

    let mut markdown = String::new();
    markdown.push_str("# inillucent-core baseline, captured by inillucent-baseline\n\n");
    markdown.push_str(
        "The retrieval engine as it stood before any relational work. `inillucent-baseline verify`\n",
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
    std::fs::write(out.join("inillucent-core-baseline.md"), markdown)
        .map_err(|error| format!("cannot write the baseline: {error}"))?;

    Ok(format!(
        "captured {} files, {passed} retrieval tests green, into {}",
        files.len(),
        out.display()
    ))
}

/// The file that records deliberate, reviewed changes to the retrieval engine.
const AMENDMENTS: &str = "inillucent-core-amendments.toml";

/// One declared change to a tracked file.
#[derive(Clone, Debug)]
struct Amendment {
    path: String,
    ticket: String,
    reason: String,
    sha256: String,
}

/// Reads the declared amendments, or an empty list when there are none.
fn amendments(out: &Path) -> Result<Vec<Amendment>, String> {
    let path = out.join(AMENDMENTS);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let document = inillucent_compat::toml_lite::parse(&text)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let mut found = Vec::new();
    for table in document.array("amendment") {
        let field = |name: &str| {
            table
                .get(name)
                .and_then(inillucent_compat::toml_lite::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("an amendment is missing `{name}`"))
        };
        found.push(Amendment {
            path: field("path")?,
            ticket: field("ticket")?,
            reason: field("reason")?,
            sha256: field("sha256")?,
        });
    }
    Ok(found)
}

/// Returns the capture's digests, by path.
///
/// The capture is this tool's own output, so its shape is known: one object per
/// file with a `"path"` and a `"sha256"`. Read by scanning for the pair rather
/// than with a JSON parser, for the same reason the rest of this binary does -
/// `docs/invariants/layering.toml` approves `serde_json` for two crates and
/// this is not one of them.
///
/// @param recorded - the capture file's text
fn recorded_digests(recorded: &str) -> std::collections::BTreeMap<&str, &str> {
    let mut pinned = std::collections::BTreeMap::new();
    let mut rest = recorded;
    while let Some(at) = rest.find("\"path\": \"") {
        let after = rest.split_at(at.saturating_add(9)).1;
        let Some(end) = after.find('"') else {
            break;
        };
        let (path, remainder) = after.split_at(end);
        let Some(hash_at) = remainder.find("\"sha256\": \"") else {
            break;
        };
        let value = remainder.split_at(hash_at.saturating_add(11)).1;
        let Some(hash_end) = value.find('"') else {
            break;
        };
        let (sha256, tail) = value.split_at(hash_end);
        pinned.insert(path, sha256);
        rest = tail;
    }
    pinned
}

/// Re-checks a capture against the working tree.
///
/// A file may differ from the capture only when an amendment names it *and*
/// pins the digest it now has. That is deliberately strict: a declared file
/// whose contents have moved again since the declaration is an undeclared
/// change, and is reported as one.
fn verify(root: &Path, out: &Path) -> Result<String, String> {
    let path = out.join("inillucent-core-baseline.json");
    let recorded = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let declared = amendments(out)?;
    let current = digests(root)?;
    // **Per path, because a hash that appears *somewhere* is not the same
    // question (task-1932, M4).** This used to ask whether the file's digest
    // was anywhere in the recorded JSON, so two pinned files that swapped
    // contents both verified clean: each one's new hash was still in the
    // capture, under the other one's name. That is precisely the change a
    // baseline exists to catch.
    let pinned = recorded_digests(&recorded);
    let mut changed = Vec::new();
    let mut amended = 0usize;
    for file in &current {
        if pinned.get(file.path.as_str()) == Some(&file.sha256.as_str()) {
            continue;
        }
        match declared
            .iter()
            .find(|amendment| amendment.path == file.path)
        {
            Some(amendment) if amendment.sha256 == file.sha256 => {
                amended = amended.saturating_add(1);
            }
            Some(amendment) => changed.push(format!(
                "{} has moved again since {} declared it",
                file.path, amendment.ticket
            )),
            None => changed.push(file.path.clone()),
        }
    }
    let recorded_count = recorded.matches("\"sha256\":").count();
    let added = declared
        .iter()
        .filter(|amendment| !recorded.contains(&format!("\"path\": \"{}\"", amendment.path)))
        .count();
    if recorded_count.saturating_add(added) != current.len() {
        changed.push(format!(
            "the file set changed: {recorded_count} recorded plus {added} added, {} now",
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
        "{} files unchanged since the baseline, {amended} changed by declared amendment",
        current.len().saturating_sub(amended)
    ))
}

/// Declares one changed file, recording the digest it is allowed to have.
///
/// The reason is required and is not decoration: the whole value of the guard
/// is that somebody reading the file later can tell a deliberate change from a
/// drift, and a list of paths with no reasons is a list nobody can audit.
fn amend(root: &Path, out: &Path, arguments: &[String]) -> Result<String, String> {
    let target = arguments.get(1).ok_or_else(|| {
        "usage: inillucent-baseline amend <path> --ticket <task-N> --reason <text>".to_string()
    })?;
    let ticket = text_flag(arguments, "--ticket")
        .ok_or_else(|| "an amendment needs --ticket".to_string())?;
    let reason = text_flag(arguments, "--reason")
        .ok_or_else(|| "an amendment needs --reason".to_string())?;
    let relative = target.replace('\\', "/");
    let current = digests(root)?;
    let file = current
        .iter()
        .find(|file| file.path == relative)
        .ok_or_else(|| format!("{relative} is not a tracked file"))?;
    let mut declared = amendments(out)?;
    declared.retain(|amendment| amendment.path != relative);
    declared.push(Amendment {
        path: relative.clone(),
        ticket: ticket.clone(),
        reason: reason.clone(),
        sha256: file.sha256.clone(),
    });
    declared.sort_by(|left, right| left.path.cmp(&right.path));
    let mut text = String::new();
    text.push_str("# Deliberate, reviewed changes to the retrieval engine.\n#\n");
    text.push_str("# `inillucent-baseline verify` accepts a tracked file that differs from the\n");
    text.push_str("# capture only when it is named here at exactly the digest it now has, so a\n");
    text.push_str("# second, undeclared change to the same file still fails the guard.\n#\n");
    text.push_str(
        "# Written by `inillucent-baseline amend`; edit through that rather than by hand.\n\n",
    );
    for amendment in &declared {
        text.push_str("[[amendment]]\n");
        text.push_str(&format!("path = \"{}\"\n", amendment.path));
        text.push_str(&format!("ticket = \"{}\"\n", amendment.ticket));
        text.push_str(&format!(
            "reason = \"{}\"\n",
            amendment.reason.replace('"', "'")
        ));
        text.push_str(&format!("sha256 = \"{}\"\n\n", amendment.sha256));
    }
    std::fs::create_dir_all(out)
        .map_err(|error| format!("cannot create {}: {error}", out.display()))?;
    std::fs::write(out.join(AMENDMENTS), text)
        .map_err(|error| format!("cannot write the amendments: {error}"))?;
    Ok(format!(
        "{relative} is declared changed by {ticket}; {} amendment(s) recorded",
        declared.len()
    ))
}

/// Returns the value of a `--flag value` argument as text.
fn text_flag(arguments: &[String], name: &str) -> Option<String> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).cloned()
}
