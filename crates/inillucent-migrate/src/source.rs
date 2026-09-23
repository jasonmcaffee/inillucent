//! The legacy index, inventoried and never touched.
//!
//! Invariant: **nothing in this file opens the source for writing.** It reads
//! the generation pointer, digests every file of the generation it names, and
//! loads the index through `inillucent_core::persist::load` - the reader the
//! existing engine already uses. There is no code path here that renames,
//! truncates, deletes or writes anything under the source directory, and that
//! is the property the whole migration rests on: if the destination turns out
//! wrong, going back is not an undo, it is pointing the application at a
//! directory that never changed.
//!
//! The digests are what make "the source did not move underneath us" a
//! checkable claim rather than an assumption. A resumed migration re-digests
//! the generation and compares; a source that has been rebuilt in the meantime
//! is a different corpus, and continuing to copy from it would produce a
//! destination that is half of one and half of the other.

use std::path::{Path, PathBuf};

use inillucent_base::hash::Sha256;
use inillucent_core::index::Index;

/// The files a generation directory holds, in the order they are digested.
pub const SECTIONS: [&str; 5] = [
    "config.bin",
    "graph.bin",
    "lexical.bin",
    "store.bin",
    "vectors.bin",
];

/// One digested section of the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// The file's name inside the generation directory.
    pub name: String,
    /// How many bytes it holds.
    pub bytes: u64,
    /// Its SHA-256, as lowercase hexadecimal.
    pub sha256: String,
}

/// What the source directory holds, and proof of which bytes it held.
pub struct Source {
    /// The index directory the caller named.
    pub path: PathBuf,
    /// The generation directory the pointer named.
    pub generation: PathBuf,
    /// The digested sections, in a stable order.
    pub sections: Vec<Section>,
    /// The loaded index.
    pub index: Index,
}

impl Source {
    /// Reads and digests a legacy index directory without writing to it.
    ///
    /// @param path - the index directory, holding `current` and `gNNN/`
    pub fn open(path: impl AsRef<Path>) -> Result<Source, String> {
        let path = path.as_ref().to_path_buf();
        let generation = current_generation(&path)?;
        let mut sections = Vec::new();
        for name in SECTIONS {
            let file = generation.join(name);
            let bytes = std::fs::read(&file)
                .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            sections.push(Section {
                name: name.to_string(),
                bytes: bytes.len() as u64,
                sha256: hasher.hex(),
            });
        }
        let mut index = inillucent_core::persist::load(&path)
            .map_err(|error| format!("cannot open the legacy index: {error}"))?;
        // **A migration reads every chunk, so it holds the text** (task-2066
        // §4.3.8). A load leaves the chunk text in `store.bin` and reads a range
        // per result, which is what a search wants. This copies the whole corpus
        // and digests it, so one positional read per chunk would be six hundred
        // thousand reads for text the next line is going to touch anyway.
        index.make_text_resident();
        Ok(Source {
            path,
            generation,
            sections,
            index,
        })
    }

    /// Returns the generation directory's own name, which is its identity.
    pub fn generation_name(&self) -> String {
        self.generation
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Returns one digest line, in the form the manifest records.
    pub fn manifest_lines(&self) -> Vec<String> {
        self.sections
            .iter()
            .map(|section| format!("{} {} {}", section.name, section.bytes, section.sha256))
            .collect()
    }

    /// Returns whether the source still holds the bytes a manifest recorded.
    ///
    /// The comparison is on the whole set rather than on any one file, because
    /// a generation is only meaningful whole: a store that matches beside a
    /// graph that does not is not a corpus this tool can copy.
    /// @param recorded - the `source.file` lines from an earlier run
    pub fn matches(&self, recorded: &[&str]) -> bool {
        if recorded.is_empty() {
            return false;
        }
        let mine = self.manifest_lines();
        recorded.len() == mine.len()
            && recorded
                .iter()
                .all(|line| mine.iter().any(|own| own == line))
    }

    /// Returns why this source cannot be migrated faithfully, if it cannot.
    ///
    /// One reason exists and it is worth refusing rather than approximating.
    /// The legacy store keeps a chunk's heading path as a separate structure,
    /// and a migrated search table has one column holding the chunk's text: the
    /// terms and the corpus statistics are identical, and the heading structure
    /// is not carried into the index. With `lexical_heading_boost` at zero -
    /// which is the measured default and what every shipped index uses - that
    /// structure contributes nothing to any score, so the two indexes answer
    /// identically. With a non-zero boost it would contribute, and the migrated
    /// index would rank differently from the source in a way nothing about it
    /// looked wrong. So that source is refused, and the refusal says why.
    pub fn refusal(&self) -> Option<String> {
        let boost = self.index.config().lexical_heading_boost;
        if boost != 0.0 {
            return Some(format!(
                "this index was built with lexical_heading_boost = {boost}, and a migrated search \
                 table indexes a chunk's text without its heading structure - so the migrated \
                 index would score differently from the source. Rebuild the source with the \
                 boost at zero, or migrate the relational tables only."
            ));
        }
        None
    }
}

/// Returns the generation directory the pointer file names.
fn current_generation(path: &Path) -> Result<PathBuf, String> {
    let pointer = path.join("current");
    let text = std::fs::read_to_string(&pointer)
        .map_err(|error| format!("cannot read {}: {error}", pointer.display()))?;
    let name = text.trim();
    if name.is_empty() {
        return Err(format!("{} names no generation", pointer.display()));
    }
    let directory = path.join(name);
    if !directory.is_dir() {
        return Err(format!(
            "{} names {name}, which is not there",
            pointer.display()
        ));
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory with no pointer is refused rather than guessed at.
    #[test]
    fn a_directory_with_no_pointer_is_refused() {
        let mut path = std::env::temp_dir();
        path.push(format!("inillucent-migrate-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&path);
        assert!(Source::open(&path).is_err());
        let _ = std::fs::remove_dir_all(&path);
    }

    /// The digest lines are the whole set, in a stable order.
    #[test]
    fn the_section_list_is_stable() {
        let mut sorted = SECTIONS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, SECTIONS.to_vec(), "the section list is sorted");
        assert_eq!(SECTIONS.len(), 5);
    }
}
