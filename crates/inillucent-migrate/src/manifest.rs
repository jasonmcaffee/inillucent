//! The migration manifest: an append-only log of everything that has been done.
//!
//! Invariant: a line is written only after the thing it describes is durable.
//! The manifest is therefore a write-ahead log read backwards - whatever the
//! last line says has happened, has happened, and everything after it has not.
//! That is what makes a migration resumable rather than restartable, and it is
//! the same argument a journal makes.
//!
//! It is a text file of `key value` lines rather than a JSON document, and the
//! reason is the resume: appending one line per completed batch is an operation
//! that either lands whole or does not land at all, whereas rewriting a
//! document leaves a window in which the file is neither the old state nor the
//! new one. A crash in that window would lose the record of work that was
//! actually done, and the migration would redo it - which is survivable here
//! only because the copy is idempotent, and relying on that is a worse design
//! than not needing it.
//!
//! It is also the rollback record. It names the source, every digest of it, the
//! staging path, the published path, and every verification that ran, so a
//! person deciding whether to go back to the original has the evidence in one
//! file rather than in a terminal that has scrolled away.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The manifest format version.
pub const VERSION: u32 = 1;

/// One line of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The key, which names what kind of line it is.
    pub key: String,
    /// The rest of the line.
    pub value: String,
}

/// An append-only migration log.
#[derive(Debug)]
pub struct Manifest {
    path: PathBuf,
    entries: Vec<Entry>,
}

impl Manifest {
    /// Opens the manifest beside a destination, reading whatever is there.
    ///
    /// A missing file is an empty manifest rather than an error: the first run
    /// of a migration has nothing to resume from, and that is the ordinary case
    /// rather than a failure.
    /// @param path - where the manifest lives
    pub fn open(path: impl AsRef<Path>) -> Result<Manifest, String> {
        let path = path.as_ref().to_path_buf();
        let mut entries = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let line = line.trim_end();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (key, value) = line.split_once(' ').unwrap_or((line, ""));
                entries.push(Entry {
                    key: key.to_string(),
                    value: value.to_string(),
                });
            }
        }
        Ok(Manifest { path, entries })
    }

    /// Returns where the manifest is written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns every entry, in the order they were written.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Appends one line and makes it durable before returning.
    ///
    /// `sync_all` on every line, because a manifest that is ahead of the disk
    /// is worse than no manifest: it would say a batch had been copied when the
    /// batch's own pages were still in a cache, and the resume would skip work
    /// that was never done.
    /// @param key - what kind of line this is
    /// @param value - the rest of the line
    pub fn record(&mut self, key: &str, value: impl AsRef<str>) -> Result<(), String> {
        let value = value.as_ref().replace(['\n', '\r'], " ");
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| format!("cannot open {}: {error}", self.path.display()))?;
        writeln!(file, "{key} {value}")
            .map_err(|error| format!("cannot write {}: {error}", self.path.display()))?;
        file.sync_all()
            .map_err(|error| format!("cannot sync {}: {error}", self.path.display()))?;
        self.entries.push(Entry {
            key: key.to_string(),
            value,
        });
        Ok(())
    }

    /// Returns the value of the last line with a key, if there is one.
    pub fn last(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
    }

    /// Returns every value written under a key, in order.
    pub fn all(&self, key: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
            .collect()
    }

    /// Returns the last checkpoint a copy reached, or zero.
    pub fn checkpoint(&self, stage: &str) -> u64 {
        self.all("copied")
            .into_iter()
            .filter_map(|line| line.split_once(' '))
            .filter(|(name, _)| *name == stage)
            .filter_map(|(_, count)| count.trim().parse::<u64>().ok())
            .max()
            .unwrap_or(0)
    }

    /// Returns whether a stage has been recorded as finished.
    pub fn finished(&self, stage: &str) -> bool {
        self.all("stage").contains(&stage)
    }

    /// Returns the verification results, as name and outcome.
    pub fn verifications(&self) -> Vec<(String, bool, String)> {
        self.all("verify")
            .into_iter()
            .filter_map(|line| {
                let (name, rest) = line.split_once(' ')?;
                let (verdict, detail) = rest.split_once(' ').unwrap_or((rest, ""));
                Some((name.to_string(), verdict == "ok", detail.to_string()))
            })
            .collect()
    }

    /// Renders the manifest as a report a person reads.
    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str("# Migration report\n\n");
        if let Some(source) = self.last("source.path") {
            let _ = writeln!(out, "- source: `{source}`");
        }
        if let Some(generation) = self.last("source.generation") {
            let _ = writeln!(out, "- source generation: `{generation}`");
        }
        if let Some(staging) = self.last("destination.staging") {
            let _ = writeln!(out, "- staging destination: `{staging}`");
        }
        if let Some(published) = self.last("destination.published") {
            let _ = writeln!(out, "- published to: `{published}`");
        } else {
            out.push_str("- **not published**\n");
        }
        if let Some(sequence) = self.last("target.commit_sequence") {
            let _ = writeln!(out, "- target commit sequence: `{sequence}`");
        }
        out.push_str("\n## Source sections\n\n| file | bytes | sha256 |\n|---|---:|---|\n");
        for line in self.all("source.file") {
            let mut parts = line.split_whitespace();
            let name = parts.next().unwrap_or("");
            let bytes = parts.next().unwrap_or("");
            let digest = parts.next().unwrap_or("");
            let _ = writeln!(out, "| `{name}` | {bytes} | `{digest}` |");
        }
        out.push_str("\n## Target tables\n\n| table | rows | digest |\n|---|---:|---|\n");
        for line in self.all("target.table") {
            let mut parts = line.split_whitespace();
            let name = parts.next().unwrap_or("");
            let rows = parts.next().unwrap_or("");
            let digest = parts.next().unwrap_or("");
            let _ = writeln!(out, "| `{name}` | {rows} | `{digest}` |");
        }
        out.push_str("\n## Verification\n\n| check | verdict | detail |\n|---|---|---|\n");
        for (name, ok, detail) in self.verifications() {
            let verdict = if ok { "pass" } else { "**FAIL**" };
            let _ = writeln!(out, "| `{name}` | {verdict} | {detail} |");
        }
        out.push_str("\n## Rollback\n\n");
        out.push_str("The source was neither modified nor removed. To go back, point the\n");
        out.push_str("application at the source path above; nothing has to be undone, because\n");
        out.push_str("nothing was done to it.\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "inillucent-migrate-manifest-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A manifest reads back what it wrote, in order.
    #[test]
    fn a_manifest_reads_back_what_it_wrote() {
        let path = scratch("roundtrip");
        {
            let mut manifest = Manifest::open(&path).expect("opened");
            manifest.record("source.path", "/tmp/index").expect("wrote");
            manifest.record("copied", "chunk 500").expect("wrote");
            manifest.record("copied", "chunk 1000").expect("wrote");
        }
        let reopened = Manifest::open(&path).expect("reopened");
        assert_eq!(reopened.last("source.path"), Some("/tmp/index"));
        assert_eq!(reopened.checkpoint("chunk"), 1000);
        assert_eq!(reopened.checkpoint("document"), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// A stage is finished only once it has been recorded.
    #[test]
    fn a_stage_is_finished_only_when_recorded() {
        let path = scratch("stages");
        let mut manifest = Manifest::open(&path).expect("opened");
        assert!(!manifest.finished("schema"));
        manifest.record("stage", "schema").expect("wrote");
        assert!(manifest.finished("schema"));
        assert!(!manifest.finished("verify"));
        let _ = std::fs::remove_file(&path);
    }

    /// A verification line carries its verdict and its detail.
    #[test]
    fn a_verification_carries_its_verdict() {
        let path = scratch("verify");
        let mut manifest = Manifest::open(&path).expect("opened");
        manifest
            .record("verify", "counts ok 128 documents")
            .expect("wrote");
        manifest
            .record("verify", "bm25 fail the top ten differed")
            .expect("wrote");
        let results = manifest.verifications();
        assert_eq!(results.len(), 2);
        assert!(results.first().map(|entry| entry.1).unwrap_or(false));
        assert!(!results.get(1).map(|entry| entry.1).unwrap_or(true));
        let _ = std::fs::remove_file(&path);
    }

    /// A newline in a value cannot break the line format.
    #[test]
    fn a_value_cannot_break_the_format() {
        let path = scratch("newline");
        let mut manifest = Manifest::open(&path).expect("opened");
        manifest
            .record("verify", "bm25 fail it went\nwrong")
            .expect("wrote");
        let reopened = Manifest::open(&path).expect("reopened");
        assert_eq!(reopened.entries().len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
