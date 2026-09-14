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
    "shell_reported_an_error",
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
