//! Reads the documents a sync compares against the database.
//!
//! The source is either a JSONL file with one document per line, which is how
//! the shared corpus in `../corpus/greek-philosophy.jsonl` is stored, or a
//! folder of `.md` and `.txt` files, which is how most people keep the
//! documents they want an agent to search. The rest of the server sees only
//! [`SourceDocument`], so it does not know which one it read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One document as the source holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceDocument {
    /// What identifies the document from one sync to the next: its URL in a
    /// JSONL file, its path relative to the folder otherwise.
    pub key: String,
    /// The title shown with every result, and indexed for keyword search.
    pub title: String,
    /// Where a reader can see the original.
    pub url: String,
    /// The full text.
    pub text: String,
}

/// One line of a JSONL corpus. Other fields on the line are ignored.
#[derive(Deserialize)]
struct CorpusLine {
    title: String,
    url: String,
    text: String,
}

/// Reads every document from a JSONL file or a folder.
///
/// Two documents with the same key are an error. Letting the second one win
/// would make the first disappear from the index with no message.
///
/// @param source - a `.jsonl` file, or a folder to read `.md` and `.txt` files from
pub fn read_source(source: &Path) -> Result<Vec<SourceDocument>, String> {
    let documents = if source.is_dir() { read_folder(source)? } else { read_jsonl(source)? };
    let mut seen = BTreeMap::new();
    for document in &documents {
        if let Some(earlier) = seen.insert(document.key.clone(), document.title.clone()) {
            return Err(format!(
                "{} holds two documents with the key {}: `{earlier}` and `{}`",
                source.display(),
                document.key,
                document.title
            ));
        }
    }
    Ok(documents)
}

/// Reads a JSONL file: one JSON object per line with `title`, `url` and `text`.
///
/// @param path - the file
fn read_jsonl(path: &Path) -> Result<Vec<SourceDocument>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut documents = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: CorpusLine = serde_json::from_str(line).map_err(|error| format!("{} line {}: {error}", path.display(), number + 1))?;
        documents.push(SourceDocument { key: parsed.url.clone(), title: parsed.title, url: parsed.url, text: parsed.text });
    }
    Ok(documents)
}

/// Reads every `.md` and `.txt` file under a folder, in path order.
///
/// A Markdown file's title is its first `# ` heading, and a file with no
/// heading is titled by its file name. The URL is the file's path, so an
/// agent can cite it.
///
/// @param folder - the folder to read
fn read_folder(folder: &Path) -> Result<Vec<SourceDocument>, String> {
    let mut files = Vec::new();
    collect_files(folder, &mut files)?;
    files.sort();
    let mut documents = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        let relative = file.strip_prefix(folder).unwrap_or(&file).to_string_lossy().replace('\\', "/");
        let title = markdown_title(&text).unwrap_or_else(|| file_stem(&file));
        documents.push(SourceDocument { key: relative, title, url: file.to_string_lossy().replace('\\', "/"), text });
    }
    Ok(documents)
}

/// Adds every `.md` and `.txt` file under a folder to a list.
///
/// @param folder - the folder to walk
/// @param into - the list the files are added to
fn collect_files(folder: &Path, into: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(folder).map_err(|error| format!("cannot read {}: {error}", folder.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, into)?;
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("md" | "txt")) {
            into.push(path);
        }
    }
    Ok(())
}

/// Returns the text of a Markdown file's first level one heading.
///
/// @param text - the file's contents
fn markdown_title(text: &str) -> Option<String> {
    text.lines().find_map(|line| line.strip_prefix("# ")).map(|title| title.trim().to_string())
}

/// Returns a file's name without its extension.
///
/// @param path - the file
fn file_stem(path: &Path) -> String {
    path.file_stem().map(|stem| stem.to_string_lossy().to_string()).unwrap_or_default()
}
