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
//! 4. **What `agent-skills/` tells an agent to send and to read.** A field name
//!    and a count are both exact, and both were wrong: `elapsedMs` for a member
//!    the shell writes as `elapsed_ms`, and tool and command counts a release
//!    behind. A skill page is read by a program, so a wrong name there is a
//!    parse failure with no diagnosis rather than a sentence somebody
//!    discounts.
//!
//! What is *not* checked is prose. A test that asserted on sentences would fail
//! on a rewording, and a check that fails for a reason nobody cares about is a
//! check people learn to re-run until it passes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use inillucent_cli::command::{self, Outcome};
use inillucent_cli::json::{self, Json};
use inillucent_cli::mcp;
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

/// Returns every `agent-skills/**/*.md` page.
///
/// The skills are read by an agent rather than by a person, which makes a
/// wrong field name worse there than in prose: an agent writes the key it was
/// told to write and gets a parse error it cannot diagnose, because the page
/// that taught it the name is the page it would check against.
fn skill_pages() -> Vec<PathBuf> {
    let mut pages = Vec::new();
    walk(&workspace_root().join("agent-skills"), &mut pages);
    pages.retain(|page| page.extension().is_some_and(|kind| kind == "md"));
    assert!(
        pages.len() >= 3,
        "found {} skill pages, which means this is looking in the wrong place",
        pages.len()
    );
    pages
}

/// Returns the fenced ```json blocks of a page.
///
/// @param text - the whole page
fn json_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```json") {
        let after = rest.split_at(start).1;
        let Some(body) = after.strip_prefix("```json") else {
            break;
        };
        let Some(end) = body.find("```") else {
            break;
        };
        let (block, remainder) = body.split_at(end);
        blocks.push(block.to_string());
        rest = remainder;
    }
    blocks
}

/// Every field name the skills show in a shell result has to be one the shell
/// actually writes.
///
/// **`elapsedMs` was in two of them (task-1932, M9).**
/// `agent-skills/inillucent-quickstart/SKILL.md` and
/// `agent-skills/inillucent-mcp/SKILL.md` both showed a `--output json` result
/// with an `elapsedMs` member. `Outcome::to_json` writes `elapsed_ms`, and has
/// as long as it has existed. An agent that read the page and keyed on
/// `elapsedMs` would have read `undefined` on every call, with nothing to tell
/// it why.
///
/// A block counts as a shell result when it has `ok` and `command`, which is
/// what separates one from the MCP configuration and protocol examples on the
/// same pages. Command-specific members are allowed through by name, because
/// `Outcome::with` lets a verb add its own and the general shape cannot know
/// them.
#[test]
fn every_result_field_the_skills_show_is_one_the_shell_writes() {
    let shape = Outcome::said("query", "").to_json();
    let Json::Object(members) = &shape else {
        panic!("an outcome does not render as an object");
    };
    let mut written: BTreeSet<String> = members.iter().map(|(name, _)| name.clone()).collect();
    assert!(
        written.contains("elapsed_ms") && written.contains("row_count"),
        "the outcome shape is not the one this test was written against: {written:?}"
    );
    // What `Outcome::with` adds. Each is a member one verb writes, so it is
    // real and it is not in the general shape.
    for extra in EXTRA_MEMBERS {
        written.insert((*extra).to_string());
    }

    let mut wrong = Vec::new();
    for page in skill_pages() {
        let text = std::fs::read_to_string(&page).unwrap_or_default();
        for block in json_blocks(&text) {
            let Ok(Json::Object(shown)) = json::parse(&block) else {
                continue;
            };
            let names: Vec<&String> = shown.iter().map(|(name, _)| name).collect();
            let is_a_result = names.iter().any(|name| name.as_str() == "ok")
                && names.iter().any(|name| name.as_str() == "command");
            if !is_a_result {
                continue;
            }
            for name in names {
                if !written.contains(name) {
                    wrong.push(format!(
                        "{}: shows `{name}`, which the shell never writes",
                        page.display()
                    ));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "these skill pages show a result field the shell does not write:\n{}",
        wrong.join("\n")
    );
}

/// The members individual verbs add of their own.
///
/// `Outcome::with` lets a verb answer something the general shape cannot -
/// `pragma` reports `page_size`, `import` reports `wrote`, `batch` reports
/// `transaction`. They are written out here rather than grepped for, because a
/// grep would accept a member added under a computed name and that is the one
/// case this is protecting against. A new one fails here until it is added.
const EXTRA_MEMBERS: &[&str] = &[
    "cache_hits",
    "cache_misses",
    "checks",
    "cli",
    "ddl",
    "destination",
    "driver",
    "engine",
    "free_pages",
    "indexes",
    "page_count",
    "page_size",
    "path",
    "pool_bytes",
    "query",
    "ready",
    "residency",
    "root",
    "rows",
    "server",
    "source",
    "table",
    "tables",
    "transaction",
    "transport",
    "wrote",
];

/// The counts the skills state have to be the counts the registry has.
///
/// **They were one behind on three pages (task-1932, M9).**
/// `inillucent-mcp/SKILL.md` said 27 tools and `inillucent-embed/SKILL.md` and
/// `inillucent-quickstart/SKILL.md` said 29 commands, while `registry.rs` had
/// 30 commands and `mcp::tools()` served 28 of them. A number in prose is the
/// one kind of documentation that can be checked exactly, so it is.
///
/// The sentences are named rather than scanned for. A rule like "every integer
/// near the word commands" reads the exit status out of `| 2 | the command
/// line was not one anybody could act on |` and the dot-command count out of
/// the line below it, so it would fail on numbers that are right. Naming the
/// three sentences costs a line when somebody rewords one, and that line is
/// where they are reminded the number has to move with the registry.
#[test]
fn the_skills_state_the_number_of_commands_and_tools_there_are() {
    let commands = command::COMMANDS.len();
    let tools = mcp::tools().len();
    assert!(commands > tools, "every tool is a command");

    // Each row is the text before the number, the text after it, and what the
    // number has to be.
    let claims: [(&str, &str, usize); 3] = [
        ("serves ", " of the CLI's commands as MCP tools", tools),
        ("serves ", " of those commands over MCP", tools),
        ("all ", " commands", commands),
    ];

    let mut wrong = Vec::new();
    let mut found = 0usize;
    for page in skill_pages() {
        let text = std::fs::read_to_string(&page).unwrap_or_default();
        for (before, after, expected) in claims {
            let mut rest = text.as_str();
            while let Some(at) = rest.find(before) {
                let tail = rest.split_at(at.saturating_add(before.len())).1;
                let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
                let remainder = tail.split_at(digits.len()).1;
                if !digits.is_empty() && remainder.starts_with(after) {
                    found = found.saturating_add(1);
                    if digits.parse::<usize>().ok() != Some(expected) {
                        wrong.push(format!(
                            "{}: says `{before}{digits}{after}`, and there are {expected}",
                            page.display()
                        ));
                    }
                }
                rest = tail;
            }
        }
    }
    assert!(
        found >= 3,
        "matched {found} counted sentences in the skills, and there are three. A reworded \
         sentence needs its row in `claims` reworded with it, or the number stops being \
         checked at all."
    );
    assert!(
        wrong.is_empty(),
        "these skill pages state a count the engine does not have:\n{}",
        wrong.join("\n")
    );
}

/// Returns some text with every run of whitespace as one space.
///
/// @param text - the text to collapse
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// Returns the source of every crate's `lib.rs`, with the crate's name.
///
/// A workspace member with no library is not here, which is why the counts
/// below are out of 28 rather than out of the 29 members: `inillucent-bench`
/// is a binary and has nowhere to put a crate attribute.
fn crate_libraries() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut libraries = Vec::new();
    for group in ["crates", "drivers"] {
        let Ok(entries) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.flat_map(|entry| entry.map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            // **The crate root, which is `main.rs` in a binary crate.** This
            // read `src/lib.rs` and nothing else until task-1973, so
            // `inillucent-bench` - `main.rs` plus modules, no library - was
            // invisible to it, and `docs/repository.md` could say "28 of the 29
            // crates deny" with the twenty-ninth excused for having "no library
            // to put the attributes in". A `#![deny(..)]` is a crate root inner
            // attribute and `main.rs` is a crate root, which is what
            // `crates/inillucent-search/src/bin/write_latency.rs` already
            // relies on.
            let lib = path.join("src/lib.rs");
            let root_file = if lib.is_file() {
                lib
            } else {
                path.join("src/main.rs")
            };
            let Ok(source) = std::fs::read_to_string(&root_file) else {
                continue;
            };
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            libraries.push((name, source));
        }
    }
    assert!(
        libraries.len() > 20,
        "found {} crate libraries, which means this is looking in the wrong place",
        libraries.len()
    );
    libraries
}

/// `docs/repository.md` states how many crates carry the lint attributes, and
/// the two numbers are the ones the crates actually carry.
///
/// **These are the two facts that had already drifted (task-1913).** The page
/// said 26 crates denied the four lints and 22 forbade `unsafe`; the crates
/// said 28 and 21. `tools/doc-facts/check.mjs` measures both and reports the
/// disagreement, and it is run by nothing - no test, no script, no packaging
/// step - so it caught this the day somebody ran it by hand and not before.
/// This is the same measurement as a test, which is what makes the number in
/// the page fail a build when a new crate arrives without its attributes.
#[test]
fn the_repository_page_counts_the_crates_under_each_lint_correctly() {
    let libraries = crate_libraries();
    let forbidding = libraries
        .iter()
        .filter(|(_, source)| source.contains("forbid(unsafe_code)"))
        .count();
    let four = ["unwrap_used", "expect_used", "panic", "indexing_slicing"];
    let denies_four = libraries
        .iter()
        .filter(|(_, source)| four.iter().all(|lint| source.contains(lint)))
        .count();

    let page = workspace_root().join("docs/repository.md");
    let text = std::fs::read_to_string(&page).expect("the repository page reads");
    // **Matched on the words rather than the typography.** The sentence wraps
    // across two lines, and a checkout with CRLF endings holds a different
    // string from one with LF - so a literal that carried the newline passed on
    // one machine and failed on the next for a reason that has nothing to do
    // with the numbers it is checking. It failed exactly that way the first
    // time this landed beside a rebase (task-1913).
    let text = collapse(&text);
    let denied = format!("{denies_four} of the 29 crates deny");
    let forbidden = format!("and {forbidding} forbid `unsafe`");
    assert!(
        text.contains(&denied),
        "docs/repository.md does not say `{denied}`, and {denies_four} crates deny the four lints. \
         The crates that do not: {:?}",
        libraries
            .iter()
            .filter(|(_, source)| !four.iter().all(|lint| source.contains(lint)))
            .map(|(name, _)| name.as_str())
            .collect::<Vec<&str>>()
    );
    assert!(
        text.contains(&forbidden),
        "docs/repository.md does not say `and {forbidding} forbid `unsafe``, and \
         {forbidding} crates forbid it. The crates that do not: {:?}",
        libraries
            .iter()
            .filter(|(_, source)| !source.contains("forbid(unsafe_code)"))
            .map(|(name, _)| name.as_str())
            .collect::<Vec<&str>>()
    );
}

/// Returns every workspace member's crate name, read from the root manifest.
///
/// The manifest's own list rather than a directory walk, so a crate that is in
/// the tree but not a member does not count as existing.
fn workspace_members() -> BTreeSet<String> {
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("the root manifest");
    let mut names = BTreeSet::new();
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "members = [" {
            inside = true;
            continue;
        }
        if inside {
            if trimmed == "]" {
                break;
            }
            let path = trimmed.trim_matches(|c| c == '"' || c == ',');
            if let Some(name) = path.rsplit('/').next() {
                if !name.is_empty() {
                    names.insert(name.to_string());
                }
            }
        }
    }
    assert!(
        names.len() > 20,
        "the root manifest's member list was not read: {names:?}"
    );
    // The binary targets too. `inillucent-shell`, `inillucent-mcp` and the
    // profiling harnesses are things a reader can run; they are named the same
    // way a crate is and they exist, so a front page naming one is naming
    // something real.
    for page in crate_front_pages() {
        let Some(manifest) = page.parent().and_then(Path::parent) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(manifest.join("Cargo.toml")) else {
            continue;
        };
        let mut in_bin = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_bin = trimmed == "[[bin]]";
                continue;
            }
            if in_bin {
                if let Some(rest) = trimmed.strip_prefix("name = ") {
                    names.insert(rest.trim_matches('"').to_string());
                }
            }
        }
    }
    names
}

/// Returns every crate front page: the `src/lib.rs` of each workspace member.
///
/// The front page and not every file, because that is what a `cargo doc`
/// reader and a `cargo add` reader land on, and it is where A3 found the claim
/// that four deleted crates were in the tree. A note further down a module
/// about a crate that used to exist is history and reads correctly.
fn crate_front_pages() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut found = Vec::new();
    for directory in ["crates", "drivers"] {
        let Ok(entries) = std::fs::read_dir(root.join(directory)) else {
            continue;
        };
        for entry in entries.flatten() {
            let page = entry.path().join("src").join("lib.rs");
            if page.is_file() {
                found.push(page);
            }
        }
    }
    found.sort();
    found
}

/// Returns the `inillucent-*` crate names a line of documentation claims exist.
///
/// Narrow in three ways, each of them deliberate:
///
/// - **The hyphenated spelling only.** `inillucent_engine` is a Rust path the
///   compiler already checks, and `inillucent` on its own is the product's name
///   as often as the crate's.
/// - **Inside backticks only**, because that is how this tree writes the name
///   of a thing. A crate name that is not in backticks is prose, and prose
///   about a crate that used to exist is history rather than a claim.
/// - **Not inside a fenced code block**, which a doctest is. A path in an
///   example is a string the example uses, not a claim about the workspace -
///   this test's first draft failed on the temporary directory names in the
///   driver's own doctests, which is the false positive that shaped the rule.
///
/// @param line - one line of a doc comment, with its `///` or `//!` marker
fn crate_names_in(line: &str) -> Vec<String> {
    let bytes: Vec<char> = line.chars().collect();
    let in_backticks = |at: usize, end: usize| {
        let before = line[..char_offset(&bytes, at)].chars().last();
        let after = line[char_offset(&bytes, end)..].chars().next();
        before == Some('`') && after == Some('`')
    };
    let mut found = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        if !line[char_offset(&bytes, at)..].starts_with("inillucent-") {
            at += 1;
            continue;
        }
        let mut end = at;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == '-' || bytes[end] == '_')
        {
            end += 1;
        }
        let name: String = bytes[at..end].iter().collect();
        let name = name.trim_end_matches(['-', '_']).to_string();
        let end = at.saturating_add(name.chars().count());
        if name.len() > "inillucent-".len() && in_backticks(at, end) {
            found.push(name);
        }
        at = end.max(at + 1);
    }
    found
}

/// Returns the byte offset of a character position.
///
/// @param chars - the line as characters
/// @param at - the character position
fn char_offset(chars: &[char], at: usize) -> usize {
    chars.iter().take(at).map(|c| c.len_utf8()).sum()
}

/// No crate's front page names a workspace member that does not exist.
///
/// **The facade's front page named four deleted crates and promised a type it
/// did not export (task-1961, A3).** `crates/inillucent/src/lib.rs` described
/// `inillucent-legacy`, `inillucent-capi`, `inillucent-session` and
/// `inillucent-vm` as in the tree and said what would happen to them "when the
/// driver lands" - the driver had landed and all four were deleted. It was the
/// first documentation a `cargo add inillucent` reader saw, and nothing
/// failed: a crate name in a doc comment is a string.
///
/// Narrow on purpose, in three ways. Only `src/lib.rs`, because that is the
/// page a `cargo doc` reader lands on; only the hyphenated spelling, because
/// `inillucent_engine` is a Rust path the compiler already checks; and only
/// `//!` and `///` lines, because a name inside code is checked too.
#[test]
fn no_crate_front_page_names_a_member_that_does_not_exist() {
    let members = workspace_members();
    let mut wrong: Vec<String> = Vec::new();
    for path in crate_front_pages() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut fenced = false;
        for (number, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("//!") && !trimmed.starts_with("///") {
                continue;
            }
            // A fenced block inside a doc comment is an example, and a path in
            // an example is a string rather than a claim.
            let body = trimmed.trim_start_matches(['/', '!']).trim_start();
            if body.starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if fenced {
                continue;
            }
            for named in crate_names_in(trimmed) {
                if !members.contains(&named) {
                    wrong.push(format!(
                        "{}:{}: names `{named}`, which is not a workspace member",
                        path.display(),
                        number + 1
                    ));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "crate front pages name crates that do not exist:\n{}",
        wrong.join("\n")
    );
}

/// Every skill copy is byte for byte the page in `agent-skills/`.
///
/// **The skills were in a directory no agent reads (task-1961, S1).**
/// `agent-skills/README.md` told a person to symlink them into the agent's own
/// skills directory by hand and nothing in the repository ran it, so every
/// fresh clone started with the eight skills invisible to Claude Code's own
/// matcher, which reads a project's `.claude/skills/`. The copies are committed
/// now, and this is what stops them drifting from the source they were copied
/// from - which is the one failure a copy has that a symlink does not.
#[test]
fn every_skill_copy_matches_its_source() {
    let root = workspace_root();
    let source = root.join("agent-skills");
    let mut skills: Vec<String> = std::fs::read_dir(&source)
        .expect("agent-skills/ is there")
        .flatten()
        .filter(|entry| entry.path().join("SKILL.md").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    skills.sort();
    assert!(
        skills.len() >= 8,
        "agent-skills/ holds {} skill(s), which is fewer than the eight it had when this was \
         written - if one was deleted, delete its copies too",
        skills.len()
    );

    let mut wrong: Vec<String> = Vec::new();
    for target in [".claude/skills", ".agents/skills"] {
        for skill in &skills {
            let from = source.join(skill).join("SKILL.md");
            let to = root.join(target).join(skill).join("SKILL.md");
            let expected = std::fs::read(&from).expect("the source skill reads");
            match std::fs::read(&to) {
                Err(_) => wrong.push(format!("{target}/{skill}/SKILL.md is missing")),
                Ok(found) if found != expected => {
                    wrong.push(format!("{target}/{skill}/SKILL.md differs from its source"))
                }
                Ok(_) => {}
            }
        }
        // A copy of a skill that no longer exists is worse than a missing one:
        // an agent reads it and acts on a page nobody maintains.
        if let Ok(entries) = std::fs::read_dir(root.join(target)) {
            for entry in entries.flatten() {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if entry.path().is_dir() && !skills.contains(&name) {
                    wrong.push(format!("{target}/{name} is a copy of a skill that is gone"));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "the skill copies are out of date; run `node tools/sync-skills.mjs`:\n{}",
        wrong.join("\n")
    );
}

/// The per-agent instruction files exist, point at `AGENTS.md`, and say nothing else.
///
/// **One instruction document, and a pointer for each agent that looks
/// somewhere else (task-1961, S2).** `AGENTS.md` was the only root file, which
/// is Codex's convention and nobody else's: Claude Code reads `CLAUDE.md`,
/// Gemini CLI reads `GEMINI.md`, Cursor reads `.cursor/rules/*.mdc`. Each of
/// those now holds a pointer and no content, because two copies of an
/// instruction document is one that goes stale.
#[test]
fn every_agent_pointer_file_points_at_agents_md() {
    let root = workspace_root();
    for relative in ["CLAUDE.md", "GEMINI.md", ".cursor/rules/inillucent.mdc"] {
        let path = root.join(relative);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{relative} is missing; it points an agent at AGENTS.md"));
        let lines = text.lines().count();
        assert!(
            lines < 20,
            "{relative} is {lines} lines. It is a pointer: the instructions live in AGENTS.md, and \
             a second copy of them is the one that goes stale"
        );
        assert!(
            text.contains("AGENTS.md"),
            "{relative} does not name AGENTS.md, so the agent that reads it is told nothing"
        );
    }
}

/// Every page in `docs/` is listed in `docs/README.md`.
///
/// **The index cannot drift from the directory (task-1961, D6).** There was
/// already a check that every page is *reachable* - that something links to it
/// - which a page linked from one other page passes while being absent from the
/// index a reader actually starts at.
#[test]
fn every_page_is_listed_in_the_index() {
    let root = workspace_root();
    let index = std::fs::read_to_string(root.join("docs/README.md")).expect("the index is there");
    let mut missing: Vec<String> = Vec::new();
    for page in pages() {
        let Ok(relative) = page.strip_prefix(root.join("docs")) else {
            continue;
        };
        let name = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if name == "README.md" {
            continue;
        }
        if !index.contains(&name) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "docs/README.md does not list {}. A page the index does not carry is a page nobody \
         finds and nobody updates",
        missing.join(", ")
    );
}

/// Every method a language package's API table names exists in its binding.
///
/// **Four READMEs had an install line, an example, and no method reference
/// (task-1961, D5).** A reader who wanted to know what the binding could do had
/// to read the source, which defeats the point of shipping a binding. The
/// tables are the reference; this is what stops one of them naming a method
/// that was renamed or never existed.
///
/// **It lives here rather than in each package's own suite**, which is what the
/// finding asked for, and the reason is that the package suites are not run by
/// `tools/validate` and three of the four need a toolchain this machine does
/// not have. A check that would run on somebody's laptop once is a check that
/// does not run. This one runs on every build, reads the same tables, and
/// greps the same sources.
#[test]
fn every_method_a_package_table_names_exists_in_its_binding() {
    let root = workspace_root();
    // (the README, the sources a name may be declared in)
    let packages: [(&str, &[&str]); 4] = [
        (
            "packages/npm/inillucent/README.md",
            &[
                "packages/npm/inillucent/index.mjs",
                "packages/npm/inillucent/resolve.mjs",
            ],
        ),
        // **The tracked binding, not the copy the wheel ships.**
        // `packages/python/src/inillucent/driver.py` is in `.gitignore`:
        // `packages/python/build.py` stages it into the package from here, so
        // a clone does not have it and this table would have been checked
        // against a file that is not there. A `git worktree` is what found
        // that, being a clone.
        (
            "packages/python/README.md",
            &["drivers/bindings/python/inillucent.py"],
        ),
        (
            "packages/php/README.md",
            &[
                "packages/php/src/Inillucent.php",
                "packages/php/src/Error.php",
            ],
        ),
        ("packages/go/README.md", &["packages/go/inillucent.go"]),
    ];

    let mut missing: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for (readme, sources) in packages {
        // **Read with the line endings normalised.** `core.autocrlf` is true
        // on the machine this is developed on, so a fresh checkout holds these
        // files with CRLF, and a split on a heading delimited by a newline
        // finds nothing in one. The first version of this passed here and
        // failed in a `git worktree`, which is a fresh checkout, and that is
        // what found it.
        let text = std::fs::read_to_string(root.join(readme))
            .unwrap_or_else(|_| panic!("{readme} is there"))
            .replace("\r\n", "\n");
        let Some(table) = text.split("\n## The API\n").nth(1) else {
            missing.push(format!("{readme} has no `## The API` table"));
            continue;
        };
        let body: String = sources
            .iter()
            .map(|relative| std::fs::read_to_string(root.join(relative)).unwrap_or_default())
            .collect::<Vec<String>>()
            .join("\n");
        assert!(
            !body.is_empty(),
            "none of {sources:?} could be read, so {readme}'s table is checked against              nothing. A source a clone does not carry - anything under `.gitignore` - is              not the one to check a table against"
        );
        for name in table_names(table) {
            checked = checked.saturating_add(1);
            if !body.contains(&name) {
                missing.push(format!(
                    "{readme} names `{name}`, which {sources:?} does not declare"
                ));
            }
        }
    }
    assert!(
        checked > 40,
        "only {checked} method name(s) were read out of the four tables, so this test is \
         checking almost nothing"
    );
    assert!(
        missing.is_empty(),
        "a package's API table names something its binding does not have:\n{}",
        missing.join("\n")
    );
}

/// Returns the identifier in each row of an API table's first column.
///
/// The first column is written as `` `Name.method(args)` ``, `` `new Name(x)` ``
/// or `` `Name::method()` ``; what is looked for in the source is the last
/// identifier before the parentheses, which is what the language declares.
///
/// @param table - everything after the `## The API` heading
fn table_names(table: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in table.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') || trimmed.starts_with("|---") {
            continue;
        }
        let Some(first) = trimmed.trim_start_matches('|').split('|').next() else {
            continue;
        };
        let cell = first.trim().trim_matches('`').trim();
        if cell.is_empty() || cell == "what" {
            continue;
        }
        // `new Inillucent(path)` declares `class Inillucent`; `Args` declares
        // `type Args`. Either way the identifier is what is looked for.
        let head = cell.split('(').next().unwrap_or(cell);
        let identifier = head
            .rsplit(['.', ':', ' '])
            .next()
            .unwrap_or(head)
            .trim()
            .to_string();
        if !identifier.is_empty() {
            found.push(identifier);
        }
    }
    found
}

/// Every ratio and speed claim in the roadmap also appears in the performance page.
///
/// **Nothing tested `docs/roadmap.md` at all (task-1961, section 9.1).**
/// `grep -n roadmap tools/doc-facts/check.mjs crates/inillucent-compat/tests/documentation.rs`
/// returned nothing, and two of the thirteen items it carried had been built
/// inside the ticket that wrote them without the text being updated. The case
/// this catches is the one that was found by reading: the roadmap said
/// `write.insert.batch` was `0.50x` where `docs/performance.md` had moved to
/// `0.70x`, so the document telling somebody what to work on named a number
/// that was a release out of date.
///
/// **It is one direction on purpose.** The performance page carries far more
/// numbers than the roadmap does; what is checked is that the roadmap does not
/// carry one the page has moved past.
///
/// **A number the roadmap is asking for is exempt**, and the phrasing is what
/// marks it: a bar, a condition an item is accepted on ("within 1.5x", "at or
/// above its current 1.43x", "above 1.50x"). Those are targets rather than
/// measurements, and the performance page has no reason to carry a number
/// nothing has measured yet. Where such a condition names a *current* value -
/// `fts.query` at 1.43x - that value is not published anywhere today, which is
/// worth fixing when the item is worked and the family is re-measured; it is
/// not something this test can assert into existence.
#[test]
fn every_measured_number_the_roadmap_carries_is_in_the_performance_page() {
    let root = workspace_root();
    let roadmap =
        std::fs::read_to_string(root.join("docs/roadmap.md")).expect("the roadmap is there");
    let performance = std::fs::read_to_string(root.join("docs/performance.md"))
        .expect("the performance page is there");

    let mut wrong: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for (number, line) in ratios_in(&roadmap) {
        // A line that states a bar is stating what is being asked for rather
        // than what was measured, and the page has no reason to carry it.
        let folded = line.to_lowercase();
        let asking_for_it = ["bar", "asks for", "wants", "within", "at or above", "above"]
            .iter()
            .any(|phrase| folded.contains(phrase));
        if asking_for_it {
            continue;
        }
        checked = checked.saturating_add(1);
        if !performance.contains(&number) {
            wrong.push(format!(
                "the roadmap says `{number}`, which docs/performance.md does not"
            ));
        }
    }
    assert!(
        checked > 3,
        "only {checked} measured number(s) were read out of docs/roadmap.md, so this test is \
         checking almost nothing"
    );
    assert!(
        wrong.is_empty(),
        "the roadmap carries a number the performance page has moved past:\n{}",
        wrong.join("\n")
    );
}

/// Returns every `N.NNx` ratio in a document, with the line it is on.
///
/// @param text - the document
fn ratios_in(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for line in text.lines() {
        let characters: Vec<char> = line.chars().collect();
        let mut at = 0usize;
        while at < characters.len() {
            if !characters[at].is_ascii_digit() {
                at = at.saturating_add(1);
                continue;
            }
            let start = at;
            let mut end = at;
            let mut dots = 0usize;
            while end < characters.len()
                && (characters[end].is_ascii_digit() || characters[end] == '.')
            {
                if characters[end] == '.' {
                    dots = dots.saturating_add(1);
                }
                end = end.saturating_add(1);
            }
            let is_ratio = dots == 1 && characters.get(end) == Some(&'x');
            if is_ratio {
                let mut number: String = characters[start..end].iter().collect();
                number.push('x');
                found.push((number, line.to_string()));
            }
            at = end.max(at.saturating_add(1));
        }
    }
    found
}

/// Every prerequisite the map declares is on the page.
///
/// **The page described one machine's run rather than the shape (task-1969,
/// 4.15).** It listed the five suites that one `--strict` pass on one desktop
/// happened to name and said "`tools/doc-facts/check.mjs` accepts those four
/// prerequisites and no others". Both lists were true of that machine and of
/// nothing else: a checkout without the oracle, the pinned shell, a C compiler,
/// Python with `ssl` or `openssl` sees thirty or forty suites named, and a
/// reader had no way to tell an expected absence from a new one.
///
/// So the page carries the prerequisites instead, and this reads them out of
/// `tests/selection.toml` and fails when one is missing from the table. The
/// counts are checked too, because a row that says 28 beside a prerequisite 29
/// rows declare is a number somebody will trust.
#[test]
fn the_prerequisite_table_names_every_value_in_the_map() {
    let root = workspace_root();
    let page = std::fs::read_to_string(root.join("docs/repository.md")).expect("the page");
    let table = between(&page, "<!-- requires:begin -->", "<!-- requires:end -->");
    let map = inillucent_compat::selection::Map::load(&root.join("tests/selection.toml"))
        .expect("the selection map");

    let mut declared: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for row in &map.rows {
        for value in &row.requires {
            *declared.entry(value.clone()).or_insert(0) += 1;
        }
    }
    assert!(
        declared.len() >= 10,
        "the map declares {} distinct prerequisites, which means this is reading the wrong \
         file rather than that the workspace needs almost nothing",
        declared.len()
    );

    let mut missing: Vec<String> = Vec::new();
    let mut miscounted: Vec<String> = Vec::new();
    for (value, count) in &declared {
        let Some(row) = table
            .lines()
            .find(|line| line.starts_with(&format!("| `{value}` |")))
        else {
            missing.push(value.clone());
            continue;
        };
        let written: Option<usize> = row
            .split('|')
            .nth(2)
            .and_then(|cell| cell.trim().parse().ok());
        if written != Some(*count) {
            miscounted.push(format!(
                "`{value}`: the table says {} and {count} row(s) declare it",
                row.split('|').nth(2).unwrap_or("").trim()
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "these prerequisites are declared in tests/selection.toml and are not in the table \
         in docs/repository.md between `<!-- requires:begin -->` and `<!-- requires:end -->`:\n  {}\n\
         A reader on a machine without one of them sees `--strict` name a suite and has \
         nowhere to find out what it needs.",
        missing.join("\n  ")
    );
    assert!(
        miscounted.is_empty(),
        "these counts in the table disagree with the map:\n  {}",
        miscounted.join("\n  ")
    );
}

/// Every workspace member is in the coverage table, excluded by name, or
/// explained in a sentence beside it.
///
/// **The table listed 25 crates against a workspace of 29 and said why for
/// three (task-1969, 4.12).** The fourth is `inillucent`, the facade, whose
/// body is a re-export and which therefore emits no regions at all - true, and
/// written nowhere, so a reader counting the rows found four missing and no
/// answer. Nothing read the page back at any point: `tools/coverage.mjs`
/// printed the table to standard output and stopped.
#[test]
fn the_coverage_block_names_every_workspace_member_that_is_measured() {
    let root = workspace_root();
    let page = std::fs::read_to_string(root.join("docs/repository.md")).expect("the page");
    let table = between(&page, "<!-- coverage:begin -->", "<!-- coverage:end -->");
    let tool = std::fs::read_to_string(root.join("tools/coverage.mjs")).expect("the coverage tool");
    let excluded = between(&tool, "const EXCLUDED = [", "]");

    let mut absent: Vec<String> = Vec::new();
    for member in workspace_member_crates() {
        if table.contains(&format!("| `{member}` |")) {
            continue;
        }
        // Excluded from the run by name, which `tools/coverage.mjs` holds and
        // the page states, or named in a sentence on the page that says why it
        // has no row.
        if excluded.contains(&format!("'{member}'")) && page.contains(&format!("`{member}`")) {
            continue;
        }
        if page.contains(&format!("the fourth is `{member}`"))
            || page.contains(&format!("and the fourth is `{member}`"))
        {
            continue;
        }
        absent.push(member);
    }
    assert!(
        absent.is_empty(),
        "these workspace members have no row in the coverage table, are not in \
         `tools/coverage.mjs`'s EXCLUDED, and are not named in a sentence on the page:\n  {}\n\
         Re-run `node tools/coverage.mjs --per-crate --write`, or say on the page why the \
         crate emits no regions.",
        absent.join("\n  ")
    );
}

/// The per-tier table in the testing standard counts what the map holds.
///
/// **Three documents gave three target counts and nothing compared any of them
/// to the map (task-1969, 4.14).** `docs/repository.md` said 170, this
/// document said 169 and its per-tier table said `engine` 53 and
/// `differential` 30 where the map had 58 and 31, and `tools/doc-facts/check.mjs`
/// compared a written count against what the *runner* reported - so a document
/// that agreed with a stale run passed.
///
/// Only the target column is checked. The test count per tier is a property of
/// a run rather than of the map, and a test that read it out of the map would
/// be asserting a number the map does not hold.
#[test]
fn the_per_tier_table_matches_the_map() {
    let root = workspace_root();
    let standard = std::fs::read_to_string(root.join("tests/inillucent-testing-tdd.md"))
        .expect("the standard");
    let map = inillucent_compat::selection::Map::load(&root.join("tests/selection.toml"))
        .expect("the selection map");

    let mut counted: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for row in &map.rows {
        *counted.entry(row.tier.clone()).or_insert(0) += 1;
    }

    let mut wrong: Vec<String> = Vec::new();
    let mut read = 0usize;
    for tier in &map.tiers {
        let Some(line) = standard
            .lines()
            .find(|line| line.starts_with(&format!("| `{}` |", tier.name)))
        else {
            wrong.push(format!("`{}` has no row in the per-tier table", tier.name));
            continue;
        };
        read += 1;
        let written: Option<usize> = line
            .split('|')
            .nth(2)
            .and_then(|cell| cell.trim().replace(',', "").parse().ok());
        let held = counted.get(&tier.name).copied().unwrap_or(0);
        if written != Some(held) {
            wrong.push(format!(
                "`{}`: the table says {} and the map has {held}",
                tier.name,
                line.split('|').nth(2).unwrap_or("").trim()
            ));
        }
    }
    assert!(
        read >= 5,
        "read {read} tier rows out of the per-tier table, which means this is reading the \
         wrong table rather than that the suite has almost no tiers"
    );
    assert!(
        wrong.is_empty(),
        "the per-tier table in tests/inillucent-testing-tdd.md disagrees with \
         tests/selection.toml:\n  {}\n\
         The map is what the runner runs, so the map is the number.",
        wrong.join("\n  ")
    );
}

/// Returns every workspace member's crate name, and nothing else.
///
/// `workspace_members` above adds every `[[bin]]` name as well, because a front
/// page naming `inillucent-shell` is naming something real. A coverage table has
/// one row per *crate*, so this reads the member list alone.
fn workspace_member_crates() -> BTreeSet<String> {
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("the root manifest");
    let mut names = BTreeSet::new();
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "members = [" {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if trimmed == "]" {
            break;
        }
        // The member list carries section comments - "Phase 1 foundation.",
        // "The driver (task-1837): ..." - and a blank line between groups.
        // Reading those as paths produced rows like `README.md` and half a
        // sentence, which then read as crates with no coverage row.
        if trimmed.is_empty() || trimmed.starts_with('#') || !trimmed.starts_with('"') {
            continue;
        }
        let path = trimmed
            .trim_matches(|c| c == '"' || c == ',')
            .trim_matches('"');
        if let Some(name) = path.rsplit('/').next() {
            if !name.is_empty() {
                names.insert(name.to_string());
            }
        }
    }
    assert!(
        names.len() > 20,
        "the root manifest's member list was not read: {names:?}"
    );
    names
}

/// Returns the text between two markers, or panics naming the one that is
/// missing.
///
/// @param text - the document
/// @param opens - the marker the block starts after
/// @param closes - the marker the block ends before
fn between<'a>(text: &'a str, opens: &str, closes: &str) -> &'a str {
    let start = text
        .find(opens)
        .unwrap_or_else(|| panic!("the document has no {opens}"));
    let rest = text
        .get(start.saturating_add(opens.len())..)
        .unwrap_or_default();
    let end = rest
        .find(closes)
        .unwrap_or_else(|| panic!("the document has no {closes} after {opens}"));
    rest.get(..end).unwrap_or_default()
}
