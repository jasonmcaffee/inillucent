//! The documentation says things that are true of the engine beside it.
//!
//! Invariant: **a claim in `docs/` that names something checkable is checked.**
//! Prose goes stale silently, which is what makes it different from code: a
//! function that stops being called fails to compile, and a page that describes
//! a command that no longer exists carries on reading perfectly well. A docs
//! review found exactly that - a case study still describing a
//! recovery failure the engine had fixed, in a document a person reads while
//! deciding whether to migrate.
//!
//! What is checked here is deliberately narrow. Three things go stale on their
//! own and can be compared against something the build already knows:
//!
//! 1. **Links between pages.** A relative link to a file that is not there is a
//!    dead end for whoever followed it, and the index is the page most likely
//!    to grow one.
//! 2. **Commands the documentation names.** The command table is generated from
//!    one array; a page naming `inillucent frobnicate` is naming something that
//!    was renamed or never existed.
//! 3. **Every page being reachable from the index.** A page nothing links to is
//!    a page nobody updates.
//!
//! What is *not* checked is prose. A test that asserted on sentences would fail
//! on a rewording, and a check that fails for a reason nobody cares about is a
//! check people learn to re-run until it passes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use inillucent_compat::workspace_root;

/// Returns every markdown file under `docs/`.
fn pages() -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(&workspace_root().join("docs"), &mut found);
    found.sort();
    found
}

/// Collects every `.md` under a directory.
///
/// @param directory - where to look
/// @param into - the list to extend
fn walk(directory: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, into);
        } else if path.extension().and_then(|kind| kind.to_str()) == Some("md") {
            into.push(path);
        }
    }
}

/// Returns every relative link target a page carries.
///
/// Only `](...)` links, and only relative ones: an external URL is somebody
/// else's to keep working, and a test that reached the network would fail on a
/// train.
///
/// @param text - the page's markdown
fn links(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes.get(at) == Some(&']') && bytes.get(at.saturating_add(1)) == Some(&'(') {
            let mut end = at.saturating_add(2);
            let mut target = String::new();
            while let Some(character) = bytes.get(end) {
                if *character == ')' {
                    break;
                }
                target.push(*character);
                end = end.saturating_add(1);
            }
            let target = target.split('#').next().unwrap_or("").trim().to_string();
            let external =
                target.starts_with("http") || target.starts_with("mailto:") || target.is_empty();
            if !external {
                found.push(target);
            }
            at = end;
        }
        at = at.saturating_add(1);
    }
    found
}

/// Every relative link in `docs/` points at a file that exists.
#[test]
fn no_page_links_to_something_that_is_not_there() {
    let mut broken: Vec<String> = Vec::new();
    for page in pages() {
        let Ok(text) = std::fs::read_to_string(&page) else {
            continue;
        };
        let Some(directory) = page.parent() else {
            continue;
        };
        for target in links(&text) {
            if !directory.join(&target).exists() {
                broken.push(format!("{} -> {target}", page.display()));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "these documentation links point at nothing:\n{}",
        broken.join("\n")
    );
}

/// Every page under `docs/` is reachable from the index.
///
/// A page nothing links to is a page nobody updates, which is how a document
/// describing a fixed defect stays on disk describing it.
#[test]
fn every_page_is_reachable_from_the_index() {
    let root = workspace_root().join("docs");
    let index = root.join("README.md");
    let Ok(text) = std::fs::read_to_string(&index) else {
        panic!("docs/README.md is missing");
    };
    let linked: BTreeSet<PathBuf> = links(&text)
        .iter()
        .map(|target| root.join(target))
        .filter_map(|path| path.canonicalize().ok())
        .collect();

    let mut orphans: Vec<String> = Vec::new();
    for page in pages() {
        if page == index {
            continue;
        }
        let Ok(canonical) = page.canonicalize() else {
            continue;
        };
        if linked.contains(&canonical) {
            continue;
        }
        // A page the index does not name may still be reached from one it does,
        // which is how the case studies hang off their own section.
        let reached = pages().iter().any(|other| {
            other != &page
                && std::fs::read_to_string(other)
                    .map(|body| {
                        other.parent().is_some_and(|directory| {
                            links(&body).iter().any(|target| {
                                directory
                                    .join(target)
                                    .canonicalize()
                                    .is_ok_and(|resolved| resolved == canonical)
                            })
                        })
                    })
                    .unwrap_or(false)
        });
        if !reached {
            orphans.push(page.display().to_string());
        }
    }
    assert!(
        orphans.is_empty(),
        "these pages are linked from nothing, so nobody will find them to update them:\n{}",
        orphans.join("\n")
    );
}

/// Every `inillucent <verb>` the documentation names is a command that exists.
///
/// The command table is generated from one array, so a page naming a verb that
/// is not in it is naming something renamed or never built - which reads
/// perfectly well and fails the first time somebody types it.
///
/// **Only inside a fenced code block or an inline code span.** A first pass
/// looked at any line beginning `inillucent `, and reported "inillucent has",
/// "inillucent is" and nine other sentences. A check that reports prose is a
/// check people learn to ignore.
#[test]
fn every_command_the_documentation_names_exists() {
    let known: BTreeSet<String> = inillucent_cli::command::COMMANDS
        .iter()
        .map(|command| command.name.to_string())
        .collect();

    let mut unknown: Vec<String> = Vec::new();
    for page in pages() {
        let Ok(text) = std::fs::read_to_string(&page) else {
            continue;
        };
        let mut fenced = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                continue;
            }
            for verb in verbs_named(line, fenced) {
                if !known.contains(&verb) {
                    unknown.push(format!("{}: inillucent {verb}", page.display()));
                }
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "the documentation names commands the command table does not have:\n{}",
        unknown.join("\n")
    );
}

/// Returns the verbs one line names, when it is a line that names verbs.
///
/// Inside a fence, `inillucent <word>` at the start of a command is a verb.
/// Outside one, only an inline code span counts - and a span is where a page
/// writes a command it wants the reader to type.
///
/// @param line - the line to read
/// @param fenced - whether it is inside a fenced code block
fn verbs_named(line: &str, fenced: bool) -> Vec<String> {
    let mut found = Vec::new();
    let candidates: Vec<String> = match fenced {
        true => vec![line.to_string()],
        // Inline spans only, and each is read as its own command line.
        false => line
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect(),
    };
    for candidate in candidates {
        let trimmed = candidate
            .trim()
            .trim_start_matches("$ ")
            .trim_start_matches("> ");
        let Some(rest) = trimmed.strip_prefix("inillucent ") else {
            continue;
        };
        // The first word that is not an option is the verb. `--db app.rdb exec`
        // is how every example in this repository is written.
        let mut words = rest.split_whitespace();
        let mut verb = None;
        while let Some(word) = words.next() {
            if word.starts_with('-') {
                // An option that takes a value swallows the next word, and the
                // ones this command line has all do.
                let _ = words.next();
                continue;
            }
            verb = Some(word.to_string());
            break;
        }
        let Some(verb) = verb else { continue };
        // A word that is not lower-case alphanumeric with dashes is a value or
        // a path rather than a verb.
        if verb.is_empty()
            || !verb
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            continue;
        }
        found.push(verb);
    }
    found
}
