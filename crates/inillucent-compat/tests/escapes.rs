//! Every escaped defect is held by a test that exists.
//!
//! Invariant: **`tests/escapes.toml` names a real test for every defect that
//! reached a user, or says in words why nothing holds it yet, and the tree is
//! what decides which.** "We added a test for that" is a claim, and until this
//! file existed it was a claim in a commit message with nothing checking it.
//!
//! ## What the guard resolves, and why the whole path matters
//!
//! A `held_by` entry is `<package>::<target>::<test>`, and every part of it is
//! checked. `harness.rs`'s citation check matches on the function name alone
//! because a manifest row cites a test it does not own; a ledger row is
//! different - it says *this suite holds it*, and a name that resolves in some
//! other file is a row pointing at the wrong evidence. A scenario adds an arm:
//! `inillucent::application::a_virtual_table_rolls_back_with_its_transaction::default`
//! resolves to the `scenario!` invocation in that file plus an arm the matrix
//! declares, because the arms are expanded by a macro and no `fn default` is
//! written anywhere.
//!
//! ## The three ways a row can be wrong
//!
//! 1. It names a test that does not exist - the rot this file is for.
//! 2. It holds nothing and does not say why. `held_by = []` with no `open` is a
//!    row that reads as an escape somebody is looking after and is not.
//! 3. It holds something *and* says why nothing holds it. That happens when a
//!    ticket adds the test and forgets to take the `open` line out, and it
//!    leaves the ledger claiming an escape is uncovered while it is covered.
//!
//! ## And the ledger has to cover §3.2
//!
//! `tests/inillucent-e2e-scenarios-tdd.md` §3.2 is the table of escapes this
//! whole design was written from. A row of it with no row here is an escape
//! that was analysed, written up, and then not tracked - which is the state the
//! repository was already in when this ticket started.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use inillucent_compat::toml_lite::{self, Table, Value};
use inillucent_compat::workspace_root;

/// Reads the ledger, failing with the parser's own line number.
fn ledger() -> Vec<Table> {
    let path = workspace_root().join("tests/escapes.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|why| panic!("{}: {why}", path.display()));
    match toml_lite::parse(&text) {
        Ok(document) => document.array("escape").to_vec(),
        Err(why) => panic!("{}: {why}", path.display()),
    }
}

/// A row's string field, or a failure naming the row.
///
/// @param row - the `[[escape]]` table
/// @param key - the field
fn required(row: &Table, key: &str) -> String {
    let named = row
        .get("ref")
        .and_then(Value::as_str)
        .unwrap_or("<a row with no ref>");
    match row.get(key).and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => text.to_string(),
        _ => panic!("the escape `{named}` has no `{key}`, which every row needs"),
    }
}

/// Where a package's sources live.
///
/// Two roots, because the driver and its C ABI are under `drivers/` and
/// everything else is under `crates/`. Resolved by looking rather than by a
/// table, so a crate that moves does not make this file quietly wrong.
///
/// @param package - the crate name
fn package_root(package: &str) -> Option<PathBuf> {
    let root = workspace_root();
    for parent in ["crates", "drivers"] {
        let candidate = root.join(parent).join(package);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Every `#[test]` function name in one file.
///
/// The same walk `harness::every_test_function` does over the whole workspace,
/// narrowed to a file: an attribute, then possibly more attributes, then the
/// `fn` line.
///
/// @param text - the file's text
fn tests_in(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut marked = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("#[test]") {
            marked = true;
            continue;
        }
        if !marked {
            continue;
        }
        if line.starts_with("#[") {
            continue;
        }
        marked = false;
        if let Some(rest) = line.strip_prefix("fn ") {
            if let Some(function) = rest.split('(').next() {
                names.insert(function.to_string());
            }
        }
    }
    names
}

/// Every story a file expands with `scenario!`.
///
/// The invocation is `scenario!(name, story);` or the same across three lines
/// after `cargo fmt` has had it, so the name is read from whatever follows the
/// opening parenthesis, comma or line break.
///
/// @param text - the file's text
fn stories_in(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut awaiting = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if line.starts_with("scenario!(") {
            let rest = trimmed.trim_start_matches("scenario!(");
            match rest.split(',').next().map(str::trim) {
                Some(name) if !name.is_empty() => {
                    names.insert(name.to_string());
                }
                // `scenario!(` alone on its line: the name is on the next one.
                _ => awaiting = true,
            }
            continue;
        }
        if awaiting {
            awaiting = false;
            if let Some(name) = trimmed.split(',').next() {
                let name = name.trim();
                if !name.is_empty() {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

/// Reads every `.rs` file under a directory, recursively.
///
/// @param directory - where to start
fn rust_files(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().map(|end| end == "rs").unwrap_or(false) {
                found.push(path);
            }
        }
    }
    found
}

/// Resolves one `held_by` entry, returning why it does not resolve.
///
/// `<package>::lib::<test>` is a unit test: the function has to exist somewhere
/// under that crate's `src/`. `<package>::<target>::<test>` is an integration
/// test in `tests/<target>.rs`. A fourth segment is a scenario's arm.
///
/// @param entry - the `held_by` string
/// @param arms - the arm names the matrix declares
fn unresolved(entry: &str, arms: &BTreeSet<String>) -> Option<String> {
    let parts: Vec<&str> = entry.split("::").collect();
    let (package, target) = match (parts.first(), parts.get(1)) {
        (Some(package), Some(target)) => (*package, *target),
        _ => return Some(format!("`{entry}` is not `<package>::<target>::<test>`")),
    };
    let Some(root) = package_root(package) else {
        return Some(format!("`{entry}` names no crate `{package}`"));
    };

    if target == "lib" {
        let Some(function) = parts.last() else {
            return Some(format!("`{entry}` names no test"));
        };
        let found = rust_files(&root.join("src")).into_iter().any(|file| {
            std::fs::read_to_string(&file)
                .map(|text| tests_in(&text).contains(*function))
                .unwrap_or(false)
        });
        return match found {
            true => None,
            false => Some(format!(
                "`{entry}`: no `#[test] fn {function}` under {}/src",
                root.display()
            )),
        };
    }

    let file = root.join("tests").join(format!("{target}.rs"));
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Some(format!("`{entry}`: there is no {}", file.display()));
    };

    match parts.len() {
        3 => {
            let function = parts.get(2).copied().unwrap_or_default();
            match tests_in(&text).contains(function) {
                true => None,
                false => Some(format!(
                    "`{entry}`: {} has no `#[test] fn {function}`",
                    file.display()
                )),
            }
        }
        4 => {
            let story = parts.get(2).copied().unwrap_or_default();
            let arm = parts.get(3).copied().unwrap_or_default();
            if !stories_in(&text).contains(story) {
                return Some(format!(
                    "`{entry}`: {} does not expand a story called `{story}`",
                    file.display()
                ));
            }
            match arms.contains(arm) {
                true => None,
                false => Some(format!(
                    "`{entry}`: `{arm}` is not an arm the matrix declares ({arms:?})"
                )),
            }
        }
        _ => Some(format!(
            "`{entry}` has {} segments; a test has three and a scenario four",
            parts.len()
        )),
    }
}

/// Every `held_by` entry names a test that exists.
///
/// **Rename any test this file cites and it fails**, which is the whole of what
/// it is for: a ledger that points at nothing reads as coverage and is not.
#[test]
fn every_held_by_names_a_test_that_exists() {
    let rows = ledger();
    assert!(
        rows.len() >= 10,
        "the ledger has {} rows, which means it is being read wrongly rather than that the \
         history is short: tests/inillucent-e2e-scenarios-tdd.md section 3.2 alone names ten",
        rows.len()
    );
    let arms: BTreeSet<String> =
        inillucent_compat::matrix::arms(inillucent_compat::matrix::Kind::Full)
            .iter()
            .map(|arm| arm.test_name())
            .collect();

    let mut cited = 0usize;
    let mut broken: Vec<String> = Vec::new();
    for row in &rows {
        let reference = required(row, "ref");
        let held = row
            .get("held_by")
            .and_then(Value::as_list)
            .unwrap_or_else(|| panic!("the escape `{reference}` has no `held_by` list"));
        for entry in held {
            cited += 1;
            if let Some(why) = unresolved(entry, &arms) {
                broken.push(format!("{reference}: {why}"));
            }
        }
    }
    assert!(
        cited >= 10,
        "the ledger cites {cited} tests in total, which is too few for the walk to be reading \
         the `held_by` lists at all"
    );
    assert!(
        broken.is_empty(),
        "these ledger rows name a test that is not there, so the escape they claim to hold is \
         held by nothing:\n  {}",
        broken.join("\n  ")
    );
}

/// A row holds a test, or says why it does not. Never both, never neither.
#[test]
fn a_row_that_holds_nothing_says_why() {
    let mut wrong: Vec<String> = Vec::new();
    let mut open = 0usize;
    for row in &ledger() {
        let reference = required(row, "ref");
        let _ = required(row, "surface");
        let _ = required(row, "what");
        let _ = required(row, "found_by");
        let held = row
            .get("held_by")
            .and_then(Value::as_list)
            .unwrap_or_default();
        let reason = row.get("open").and_then(Value::as_str).unwrap_or("");
        match (held.is_empty(), reason.trim().is_empty()) {
            (true, true) => wrong.push(format!(
                "{reference} holds nothing and gives no reason, so it reads as an escape \
                 somebody is looking after"
            )),
            (false, false) => wrong.push(format!(
                "{reference} names {} tests and still carries `open`; take the `open` line out \
                 when the test lands",
                held.len()
            )),
            (true, false) => open += 1,
            (false, true) => {}
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    // Rule 1.2: an assertion about open rows that never saw one asserts nothing
    // about the shape it is checking. The ledger is allowed to reach zero open
    // rows, and when it does this line is what says so out loud.
    println!("the ledger carries {open} open rows");
}

/// Every escape in the design document's §3.2 has a row here.
///
/// **The table in the document is where the analysis is; the ledger is where
/// the tracking is.** Ten escapes were written up, and before this file nothing
/// connected any of them to a test. A row of §3.2 with no ledger row is an
/// escape that was understood and then dropped.
#[test]
fn every_escape_the_design_names_has_a_row() {
    let path = workspace_root().join("tests/inillucent-e2e-scenarios-tdd.md");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|why| panic!("{}: {why}", path.display()));
    let Some(after) = text
        .split("| escape | found by | what the suite lacked |")
        .nth(1)
    else {
        panic!(
            "{} no longer holds the section 3.2 table this guard reads",
            path.display()
        );
    };
    // The header's own line ends where the split did, so the first line is
    // empty and the second is markdown's `|---|---|---|` separator. Dropping
    // the separator by shape rather than by position means a table that gains a
    // column does not make this guard read its own separator as an escape.
    let table: Vec<&str> = after
        .lines()
        .skip(1)
        .take_while(|line| line.starts_with('|'))
        .filter(|line| !line.chars().all(|letter| matches!(letter, '|' | '-' | ' ')))
        .collect();
    assert!(
        table.len() >= 10,
        "read {} rows out of the section 3.2 table, so the scan is wrong rather than the table \
         short",
        table.len()
    );

    let references: Vec<String> = ledger().iter().map(|row| required(row, "ref")).collect();
    let mut untracked: Vec<&str> = Vec::new();
    for row in &table {
        if !references
            .iter()
            .any(|reference| row.contains(reference.as_str()))
        {
            untracked.push(row);
        }
    }
    assert!(
        untracked.is_empty(),
        "these escapes are analysed in section 3.2 of tests/inillucent-e2e-scenarios-tdd.md and \
         have no row in tests/escapes.toml, so nothing tracks whether a test holds them:\n  {}",
        untracked.join("\n  ")
    );
}

/// Every difference the comparison document argues for is a case that asserts
/// it.
///
/// **The variant was decorative for months, and a document was the only place
/// seven measured differences lived.** `semantics.rs` declared `Differs`,
/// carried `#[allow(dead_code)]` on it so the compiler would stop saying so,
/// and constructed it zero times, while `docs/feature-comparison.md` published
/// "the seven rows that are not the same" with a measurement behind each one.
/// Rule 1.3 says a known difference is recorded as a test that asserts it; a
/// difference that only a document knows about can be closed, or widened, with
/// nothing going red either way.
///
/// **Named rather than counted, and the count was tried first.** Asserting that
/// the two sides hold the same *number* of differences read as true and was
/// not: `semantics.rs` also carries differences of the engine's own that the
/// comparison document does not list - `alias.limit` is one - so the count
/// disagreed the moment one of those was added, about nothing. What has to hold
/// is that each of the seven the document argues for has a case, by name.
///
/// A difference that is closed loses its case **and** its row, and this is what
/// makes those two the same action rather than two people remembering.
#[test]
fn the_differs_variant_is_used_as_often_as_the_comparison_records() {
    // The seven `docs/feature-comparison.md` records, as the case that asserts
    // each one. Closing a difference deletes both halves of a row here.
    const MEASURED: [(&str, &str); 7] = [
        (
            "pragma.page.size",
            "the page size is fixed, so the pragma reports rather than sets",
        ),
        (
            "shell.recover.page.size",
            "`.recover` prints the page size it found",
        ),
        (
            "shell.limit.trigger.depth",
            "the trigger depth limit is a different number",
        ),
        (
            "explain.bytecode",
            "the bytecode is this engine's, not SQLite's",
        ),
        (
            "shell.vfslist",
            "there is one VFS and it is named for this engine",
        ),
        (
            "shell.stats",
            "the statistics are the ones this storage has",
        ),
        // The one the document argues for in prose rather than in a table row.
        (
            "functions.sqlite.offset",
            "`sqlite_offset` names the page, not the record",
        ),
    ];

    let root = workspace_root();
    let semantics =
        std::fs::read_to_string(root.join("crates/inillucent-compat/tests/semantics.rs"))
            .expect("semantics.rs is readable");
    let constructed = semantics
        .lines()
        .filter(|line| line.trim() == "expect: Differs,")
        .count();
    assert!(
        constructed > 0,
        "`Expect::Differs` is constructed by no case in semantics.rs, so rule 1.3 is being \
         applied to nothing - which is the state task-2036 found it in"
    );
    assert!(
        !semantics.contains("#[allow(dead_code)]\n    Differs,"),
        "`Expect::Differs` carries `#[allow(dead_code)]` again, which is how it stopped being \
         noticed the first time"
    );

    let comparison = std::fs::read_to_string(root.join("docs/feature-comparison.md"))
        .expect("feature-comparison.md is readable");
    // **The legend is not a difference.** The document explains its own column
    // with a row reading `| **differs** | both answer, and the answers are not
    // the same |`, and the first version of this counter read it as a seventh
    // row - which made the arithmetic come out right for the wrong reason.
    let recorded = comparison
        .lines()
        .filter(|line| line.starts_with("| ") && line.contains("**differs"))
        .filter(|line| !line.starts_with("| **differs** |"))
        .count();
    assert_eq!(
        recorded,
        MEASURED.len().saturating_sub(1),
        "docs/feature-comparison.md marks {recorded} rows `**differs**` and this test names {}, \
         one of which - `sqlite_offset` - the document argues for in prose at the end of its \
         shell section rather than in a table row. A difference that is closed loses its row \
         and its case together; one that is found gains both.",
        MEASURED.len()
    );

    let mut absent: Vec<String> = Vec::new();
    for (case, about) in MEASURED {
        let Some(after) = semantics.split(&format!("name: \"{case}\"")).nth(1) else {
            absent.push(format!("{case}: no case of that name ({about})"));
            continue;
        };
        // The case's own block, which ends where the next one begins.
        let block = after.split("        name: \"").next().unwrap_or(after);
        if !block.contains("expect: Differs,") {
            absent.push(format!(
                "{case}: the case no longer expects a difference ({about})"
            ));
        }
    }
    assert!(
        absent.is_empty(),
        "docs/feature-comparison.md argues for these differences and semantics.rs does not \
         assert them:\n  {}\n\
         A difference nothing asserts can be closed, or widened, with nothing going red.",
        absent.join("\n  ")
    );
    assert!(
        constructed >= MEASURED.len(),
        "semantics.rs constructs `Expect::Differs` {constructed} times and the comparison \
         document argues for {}",
        MEASURED.len()
    );
}
