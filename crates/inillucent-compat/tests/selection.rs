//! The map of what to run has to match what there is to run.
//!
//! Invariant: **`tests/selection.toml` names every test target in the
//! workspace exactly once, names nothing that does not exist, and no `#[test]`
//! anywhere in the repository belongs to a target it does not name.** A test
//! selector fails silently by construction - it runs less than it should and
//! reports success - so the only useful check is one that compares the map
//! against the file system rather than against itself.
//!
//! ## The three ways the map can rot, and the test for each
//!
//! - **A new suite nobody added a row for.** It would never be selected, by
//!   `--changed` or by a tier, so it would sit in the tree looking like
//!   coverage and never run. `every_target_has_a_row` is the check.
//! - **A row for a suite that was renamed or removed.** The runner would fail
//!   to find an executable for it, which is loud - but only once somebody runs
//!   it. `every_row_names_a_real_target` fails at build time instead.
//! - **A test hiding somewhere the runner does not look.** This is the subtle
//!   one, and it nearly shipped: the obvious optimisation is to skip bin
//!   targets, because 43 of this workspace's 45 are benchmark instruments with
//!   no test in them. The other two are `inillucent-bench` and
//!   `inillucent-shell`, whose crates have **no library at all** and which hold
//!   179 tests between them. `no_test_hides_outside_the_map` attributes every
//!   source file carrying a `#[test]` to the target that compiles it and fails
//!   if that target has no row.
//!
//! ## And two the map can be wrong without rotting
//!
//! `covers` may only name packages that exist, and a tier may only be one that
//! is declared. Both are typos that would otherwise turn into a suite that is
//! never selected - a `covers` entry naming a crate that was renamed matches
//! nothing, forever, and says nothing about it.
//!
//! ## And one the map can be wrong in a way no check above can see
//!
//! Every check here reads the map through its parser, so a line the parser
//! drops is a line no check looks at.
//! `every_line_of_the_map_is_one_the_parser_reads` compares the file to itself
//! instead: a bare value the parser discards, or a key written twice in one row
//! where it keeps only the last, is a line somebody wrote and nothing acts on.

use std::collections::BTreeSet;

use inillucent_compat::layering;
use inillucent_compat::selection::{self, Kind, Map, Target};
use inillucent_compat::workspace_root;

/// Loads the shipped map.
fn map() -> Map {
    Map::load(&workspace_root().join("tests/selection.toml")).expect("the map parses")
}

/// Reads the workspace member list.
fn members() -> Vec<String> {
    layering::workspace_members(&workspace_root()).expect("the members parse")
}

/// Every target the file system has must have a row.
#[test]
fn every_target_has_a_row() {
    let map = map();
    let declared: BTreeSet<Target> = map.rows.iter().map(|row| row.target.clone()).collect();
    let real = selection::discover(&workspace_root(), &members()).expect("the targets are found");
    let missing: Vec<String> = real
        .iter()
        .filter(|target| !declared.contains(target))
        // A bin target with no tests in it needs no row: the runner does not
        // start it, and `no_test_hides_outside_the_map` is what proves that is
        // safe rather than assumed.
        .filter(|target| target.kind != Kind::Bin)
        .map(Target::label)
        .collect();
    assert!(
        missing.is_empty(),
        "these targets have no row in tests/selection.toml, so nothing runs them:\n  {}",
        missing.join("\n  ")
    );
}

/// And every row must name a target that exists.
#[test]
fn every_row_names_a_real_target() {
    let map = map();
    let real: BTreeSet<Target> = selection::discover(&workspace_root(), &members())
        .expect("the targets are found")
        .into_iter()
        .collect();
    let stale: Vec<String> = map
        .rows
        .iter()
        .filter(|row| !real.contains(&row.target))
        .map(|row| row.target.label())
        .collect();
    assert!(
        stale.is_empty(),
        "these rows name targets that are not in the workspace:\n  {}",
        stale.join("\n  ")
    );
}

/// No target holding a `#[test]` may be missing from the map.
///
/// This is the check that makes skipping most bin targets safe. It reads every
/// source file in the repository, attributes the ones carrying a test to the
/// target that compiles them, and fails if any of those targets has no row.
#[test]
fn no_test_hides_outside_the_map() {
    let map = map();
    let declared: BTreeSet<Target> = map.rows.iter().map(|row| row.target.clone()).collect();
    let holding = selection::targets_holding_tests(&workspace_root(), &members())
        .expect("the sources are read");
    let hidden: Vec<String> = holding
        .iter()
        .filter(|target| !declared.contains(target))
        .map(Target::label)
        .collect();
    assert!(
        hidden.is_empty(),
        "these targets hold a `#[test]` and have no row, so the runner never starts them:\n  {}",
        hidden.join("\n  ")
    );
}

/// Every row's tier must be one the file declares.
/// Every line of the map is one the parser reads.
///
/// **The two lines that made this worth writing (task-1970).** The file held
///
/// ```text
/// ["inillucent-engine", "inillucent-sql"]
/// requires = ["oracle"]
/// ```
///
/// directly under the `registers` row, with no `[[target]]` header above them
/// and no `covers = ` in front of the list - the remains of a row an earlier
/// edit removed half of. `toml_lite` reads a bare value as nothing and a
/// repeated key as the last one, so the file parsed, 188 rows came back, every
/// other check in this suite passed, and the only visible trace was that
/// `registers` had `requires = ["oracle"]` twice and the published count of
/// `oracle` rows was 27 where a line count said 28.
///
/// That is the failure this whole ticket is about: a contract file quietly
/// absorbing something nobody meant, and every check over it still reporting
/// green. A map is a file whose content is only ever read through a parser, so
/// the lines the parser ignores are exactly the lines nothing else looks at
/// either.
///
/// The shapes allowed are a blank line, a comment, one of the three array
/// headers, and `key = value` for a key this map defines. Anything else fails
/// and is quoted with its line number.
#[test]
fn every_line_of_the_map_is_one_the_parser_reads() {
    const KEYS: [&str; 12] = [
        "package",
        "kind",
        "name",
        "tier",
        "purpose",
        "exclusive",
        "covers",
        "requires",
        "features",
        "prefix",
        "packages",
        "reason",
    ];
    const HEADERS: [&str; 3] = ["[[target]]", "[[tier]]", "[[path]]"];

    let text = std::fs::read_to_string(workspace_root().join("tests/selection.toml"))
        .expect("the selection map");
    let mut stray: Vec<String> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        if HEADERS.contains(&trimmed) {
            seen.clear();
            continue;
        }
        let key = trimmed.split(" = ").next().unwrap_or("");
        if !trimmed.contains(" = ") || !KEYS.contains(&key) {
            stray.push(format!("{}: {trimmed}", index + 1));
            continue;
        }
        // The other half of the same defect. A bare value is dropped and a
        // repeated key resolves to the last one, and the row that carried both
        // read as correct in every check but the count.
        if !seen.insert(key) {
            stray.push(format!(
                "{}: {trimmed} - `{key}` is written twice in this row",
                index + 1
            ));
        }
    }
    assert!(
        stray.is_empty(),
        "these lines of tests/selection.toml are not a header, a comment or a key this map \
         defines once, so the parser folds them into the row above, drops them, or keeps only \
         the last of them:\n  {}\n\
         A line nothing reads is a line nothing checks.",
        stray.join("\n  ")
    );
}

#[test]
fn every_tier_is_declared() {
    let map = map();
    let declared: BTreeSet<&str> = map.tiers.iter().map(|tier| tier.name.as_str()).collect();
    let unknown: Vec<String> = map
        .rows
        .iter()
        .filter(|row| !declared.contains(row.tier.as_str()))
        .map(|row| format!("{} is in tier `{}`", row.target.label(), row.tier))
        .collect();
    assert!(unknown.is_empty(), "{}", unknown.join("\n"));
    // And every declared tier must be used, so a tier that stops meaning
    // anything is noticed rather than left to be asked for and answer nothing.
    let used = map.tiers_used();
    let empty: Vec<&str> = map
        .tiers
        .iter()
        .map(|tier| tier.name.as_str())
        .filter(|name| !used.contains(*name))
        .collect();
    assert!(
        empty.is_empty(),
        "these tiers are declared and hold nothing: {}",
        empty.join(", ")
    );
}

/// Every package a row claims to cover must exist.
///
/// A `covers` entry naming a crate that was renamed matches nothing for ever
/// and says nothing about it, which is the quietest way for a suite to stop
/// being selected.
#[test]
fn every_covered_package_exists() {
    let map = map();
    let manifests =
        layering::read_members(&workspace_root(), &members()).expect("the manifests parse");
    let real: BTreeSet<&str> = manifests
        .iter()
        .map(|manifest| manifest.name.as_str())
        .collect();
    let unknown: Vec<String> = map
        .rows
        .iter()
        .flat_map(|row| {
            row.covers
                .iter()
                .filter(|package| !real.contains(package.as_str()))
                .map(move |package| format!("{} covers `{package}`", row.target.label()))
        })
        .collect();
    assert!(
        unknown.is_empty(),
        "these rows name packages that do not exist:\n  {}",
        unknown.join("\n  ")
    );
}

/// A change to the deepest crate must select the whole suite, and a change to
/// nothing must select nothing.
///
/// These are the two ends of the selector, and both have a failure that looks
/// like success: selecting nothing runs no tests and reports a pass, and
/// selecting everything means the mechanism is not doing anything.
#[test]
fn the_selector_answers_at_both_ends() {
    let map = map();
    let members = members();
    let manifests =
        layering::read_members(&workspace_root(), &members).expect("the manifests parse");
    let graph = selection::dependents(&manifests);

    let nothing = selection::seeds_of(&map, &members, &manifests, &[]);
    assert!(
        selection::select(&map, &nothing, &graph).is_empty(),
        "an empty change selected something"
    );

    let base = selection::seeds_of(
        &map,
        &members,
        &manifests,
        &["crates/inillucent-base/src/lib.rs".to_string()],
    );
    let everything = selection::select(&map, &base, &graph);
    assert!(
        everything.len() > map.rows.len() / 2,
        "a change to the base crate selected only {} of {} targets",
        everything.len(),
        map.rows.len()
    );
}

/// A change to one leaf crate must select fewer targets than a change to the
/// base, or the selector is not narrowing anything.
///
/// `inillucent-cli` is the shell: nothing in the workspace depends on it, so a
/// change to it can only select suites that drive the shell.
#[test]
fn a_leaf_change_selects_less_than_a_root_change() {
    let map = map();
    let members = members();
    let manifests =
        layering::read_members(&workspace_root(), &members).expect("the manifests parse");
    let graph = selection::dependents(&manifests);

    let leaf = selection::seeds_of(
        &map,
        &members,
        &manifests,
        &["crates/inillucent-cli/src/main.rs".to_string()],
    );
    let base = selection::seeds_of(
        &map,
        &members,
        &manifests,
        &["crates/inillucent-base/src/lib.rs".to_string()],
    );
    let narrow = selection::select(&map, &leaf, &graph).len();
    let wide = selection::select(&map, &base, &graph).len();
    assert!(
        narrow > 0,
        "a change to the shell selected nothing, so the shell suites are unreachable"
    );
    assert!(
        narrow < wide,
        "a change to the shell selected {narrow} targets and a change to the \
         base selected {wide}; the selector is not narrowing anything"
    );
}

/// A path nothing declares selects everything.
///
/// The safe direction, and the one worth a test: a new top-level directory
/// should make a run loud rather than quietly test nothing.
#[test]
fn an_undeclared_path_is_loud() {
    let map = map();
    let members = members();
    let manifests =
        layering::read_members(&workspace_root(), &members).expect("the manifests parse");
    let choice = selection::seeds_of(
        &map,
        &members,
        &manifests,
        &["a-directory-nobody-declared/thing.rs".to_string()],
    );
    assert!(
        choice.selects_everything,
        "an undeclared path selected a subset instead of everything"
    );
    assert_eq!(choice.unmatched.len(), 1);
}

/// A suite that can skip declares what it needs, and a row that declares
/// something has a suite that can skip.
///
/// **This is what keeps the census at 61 of 61 (task-1969, 4.9).** Before it,
/// `every_differential_target_declares_what_it_needs` asked one tier for one
/// half of the rule, and the other eight tiers were unchecked in both
/// directions. The result was 47 suites in the middle: 43 that skipped, were
/// counted by `--strict` only because their helper panics, and told a reader
/// nothing about what was missing; and four - `gates_fail_closed`,
/// `inillucent-driver::import`, `inillucent-cli::lib`, `inillucent-tree::lib` -
/// that printed the marker, declared no prerequisite, and were dropped by
/// `testrun.rs`'s classifier before it looked at them.
///
/// It is checked in both directions because the two failures are different and
/// both shipped:
///
/// - **A suite that skips with no `requires`** is invisible to `--strict`
///   unless its helper happens to panic, and is absent from the prerequisite
///   table `docs/repository.md` generates from this map. That is the
///   `gates_fail_closed` shape: seven of its fifteen cases could not run and
///   the run said `ok`.
/// - **A `requires` on a suite that cannot skip** is a false entry in that same
///   table. `inillucent-compat::sql` and `::storage` declared `fixtures` and
///   neither skips - both `.expect()` on tracked files - and so did
///   `inillucent-migrate::corpus` and `::equivalence`, which build their
///   corpora themselves. A reader on a machine without the gate fixtures would
///   have read four suites as expected absences that in fact run everywhere.
///
/// What counts as "can skip" is a call to the one helper, by any of its three
/// spellings. The two files that *define* it are excluded by path, and so is
/// `src/bin/`: a program that says it cannot start is not a suite reporting a
/// hollow pass, and those targets have no row to declare anything on.
#[test]
fn every_target_that_can_skip_declares_it_and_vice_versa() {
    let root = workspace_root();
    let map = map();
    let mut undeclared: Vec<String> = Vec::new();
    let mut overdeclared: Vec<String> = Vec::new();
    let mut with_a_skip = 0usize;

    for row in &map.rows {
        let files = sources_of(&root, &row.target);
        let skips: Vec<String> = files
            .iter()
            .filter(|file| calls_the_skip_helper(file))
            .map(|file| {
                file.strip_prefix(&root)
                    .unwrap_or(file)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        if !skips.is_empty() {
            with_a_skip = with_a_skip.saturating_add(1);
        }
        match (skips.is_empty(), row.requires.is_empty()) {
            (false, true) => {
                undeclared.push(format!("{} - {}", row.target.label(), skips.join(", ")))
            }
            (true, false) => overdeclared.push(format!(
                "{} declares {:?} and no source of it calls the skip helper",
                row.target.label(),
                row.requires
            )),
            _ => {}
        }
    }

    assert!(
        with_a_skip >= 20,
        "found {with_a_skip} targets that can skip, which means this is reading the wrong \
         files rather than that the workspace has almost no skips"
    );
    assert!(
        undeclared.is_empty(),
        "these suites call the skip helper and their row declares no prerequisite, so \
         `inillucent-testrun --strict` drops the target before it examines it and the \
         prerequisite table in `docs/repository.md` cannot name it:\n  {}\n\
         Add `requires = [\"...\"]` to the row in `tests/selection.toml`.",
        undeclared.join("\n  ")
    );
    assert!(
        overdeclared.is_empty(),
        "these rows declare a prerequisite their suite cannot skip on, so the table \
         generated from this map tells a reader a suite does not run when it always \
         does:\n  {}\n\
         Either remove `requires` from the row, or make the suite skip through \
         `inillucent_compat::differential::skipping` when the thing is absent.",
        overdeclared.join("\n  ")
    );
}

/// Returns the source files that compile into one target.
///
/// A `test` target is one file by name; a `lib` is everything under `src/`
/// except the programs; a `bin` is either its own file under `src/bin/` or,
/// when it is the package's `main.rs`, every module beside it.
///
/// @param root - the workspace root
/// @param target - the target to resolve
fn sources_of(root: &std::path::Path, target: &Target) -> Vec<std::path::PathBuf> {
    let directory = package_directory(root, &target.package);
    match target.kind {
        Kind::Test => vec![directory.join("tests").join(format!("{}.rs", target.name))],
        // A program under `src/bin/` is its own file. A `main.rs` beside a
        // `lib.rs` is one file too: the modules under `src/` belong to the
        // library, which has its own row, and counting them twice would put a
        // library's skip on a program - which is what made
        // `inillucent-cli::inillucent-shell` read as a suite that skips on a
        // directory link. A `main.rs` with no `lib.rs` beside it does own every
        // module under `src/`, which is how `inillucent-bench`'s seven skips,
        // all of them in `models.rs`, belong to the `inillucent-bench` row.
        Kind::Bin => {
            let named = directory
                .join("src")
                .join("bin")
                .join(format!("{}.rs", target.name));
            let main = directory.join("src").join("main.rs");
            if named.is_file() {
                vec![named]
            } else if !main.is_file() {
                Vec::new()
            } else if directory.join("src").join("lib.rs").is_file() {
                vec![main]
            } else {
                under(&directory.join("src"))
            }
        }
        Kind::Lib => under(&directory.join("src")),
    }
}

/// Returns every `.rs` file under a directory, leaving the programs out.
///
/// `src/bin/` is excluded because those are targets of their own with rows of
/// their own: a program that says it cannot start is not a library's test
/// suite reporting a hollow pass.
///
/// @param directory - where to walk
fn under(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(here) = pending.pop() {
        if here.file_name().is_some_and(|name| name == "bin") {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&here) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|kind| kind == "rs") {
                found.push(path);
            }
        }
    }
    found
}

/// Returns the directory a package's manifest is in.
///
/// Read out of the workspace member list rather than assumed to be
/// `crates/<name>`, because the drivers are under `drivers/`.
///
/// @param root - the workspace root
/// @param package - the package name
fn package_directory(root: &std::path::Path, package: &str) -> std::path::PathBuf {
    for member in members() {
        if member.rsplit('/').next() == Some(package) {
            return root.join(member);
        }
    }
    root.join("crates").join(package)
}

/// Reports whether a source file calls the one skip helper.
///
/// The three spellings are the qualified paths, because a bare `skipping(` in a
/// file that imported it is the same call and a bare one in a file that did not
/// is a different function - which is the whole reason
/// `policy.rs`'s `no_test_file_defines_its_own_skip_helper` exists. The two
/// files that define the helper are excluded by path so that a definition does
/// not read as a call.
///
/// @param file - the file to read
fn calls_the_skip_helper(file: &std::path::Path) -> bool {
    let name = file.to_string_lossy().replace('\\', "/");
    if name.ends_with("crates/inillucent-base/src/testing.rs")
        || name.ends_with("crates/inillucent-compat/src/differential.rs")
        // `cliproc::program` announces on behalf of the suite that called it,
        // the way `differential::announce_skip` does. It is harness code in
        // `src/` rather than a suite, so counting it would put a `requires` on
        // `inillucent-compat::lib` naming a prerequisite that crate's own unit
        // tests do not have.
        || name.ends_with("crates/inillucent-compat/src/cliproc.rs")
    {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(file) else {
        return false;
    };
    // A file that imported the name calls it bare, and a file that did not
    // would be calling something else. `use inillucent_compat::differential::{compare, Step}`
    // is how the five differential suites that skip only through `compare` are
    // written, so the import is what tells a bare `compare(` from an unrelated
    // function of the same name.
    let imported = text.contains("use inillucent_base::testing::skipping");
    let brings_in = |name: &str| {
        text.lines().any(|line| {
            line.trim_start()
                .starts_with("use inillucent_compat::differential::")
                && line.contains(name)
        })
    };
    // `cliproc::program` announces for its caller too, and for the same reason
    // as `compare`: it has one way to answer `None` - the build did not produce
    // the binary - and it calls the helper itself before returning it, so
    // `let Some(binary) = program("inillucent") else { return; };` in the caller
    // is a skip that has already been announced. Announcing there rather than
    // at each of the forty call sites means the next case added cannot forget
    // it, which is the argument `cli_arguments.rs` made first.
    // The import may be written over several lines, which is what `rustfmt`
    // does to a `use` of eight names, so what is looked for is the module
    // rather than the name inside one line of its import list.
    let uses_a_program = text.contains("cliproc");
    let compares = brings_in("compare");
    let compares_queries = brings_in("compare_queries");
    let announces = brings_in("announce_skip") || brings_in("skipping");
    // Built rather than written, so this file does not carry the text
    // `policy.rs`'s `no_test_file_defines_its_own_skip_helper` forbids.
    let a_definition = format!("fn {}", "skipping(");
    text.lines().map(without_comments_or_literals).any(|code| {
        code.contains("differential::skipping(")
                || code.contains("differential::announce_skip(")
                || code.contains("testing::skipping(")
                || (imported && code.contains("skipping(") && !code.contains(&a_definition))
                // **`differential::compare` announces for its caller**, which
                // is the same allowance `policy.rs`'s `announces_by_saying_so`
                // makes and for the same reason: it has one way to return zero,
                // `start_oracle` answering `None`, after which it calls
                // `announce_skip()`. Seven differential suites skip only that
                // way - `dml_differential`, `json`, `planner`, `trigger_depth`,
                // `pragma`, `registers`, `result_names_and_codes` - and each
                // declares `requires = ["oracle"]` correctly.
                || code.contains("differential::compare")
                || (uses_a_program && code.contains("program(") && !code.contains("fn program("))
                || (compares && code.contains("compare("))
                || (compares_queries && code.contains("compare_queries("))
                || (announces
                    && (code.contains("skipping(") || code.contains("announce_skip("))
                    && !code.contains("fn "))
    })
}

/// Returns one line of source with its comment and its string literals removed.
///
/// **This file names the helper it looks for, so it would find itself.** It did:
/// the first run reported `inillucent-compat::selection` as a suite that skips
/// without declaring a prerequisite, because `calls_the_skip_helper` contains
/// the string `"differential::skipping("` as the thing it matches on. A scan
/// for a call has to read code rather than text, and the two things that are
/// not code on a line are what follows `//` and what sits between quotes.
///
/// Escapes are not handled and do not need to be: a `\"` inside a literal ends
/// the span early, which drops more text than it should and can only ever make
/// this miss a call, never invent one. A missed call is caught by the other
/// direction of the same test.
///
/// @param line - one line of Rust
fn without_comments_or_literals(line: &str) -> String {
    let code = line.split("//").next().unwrap_or("");
    let mut kept = String::with_capacity(code.len());
    let mut inside = false;
    for character in code.chars() {
        if character == '"' {
            inside = !inside;
            continue;
        }
        if !inside {
            kept.push(character);
        }
    }
    kept
}

/// Every feature a workspace crate declares is either built by the runner or
/// written down here as one nobody tests.
///
/// **A test behind a feature the build does not turn on is in no binary at
/// all (task-1913).** It is not skipped and it is not reported: `cargo test
/// --workspace` compiles with default features, so the code never exists and
/// the runner has nothing to count. The source still reads as coverage, which
/// makes it worse than a missing test - somebody looking for a case finds one.
///
/// Measured before the fix, by listing each crate's tests with the feature on
/// and off: `inillucent-core` had **287** tests with `onnx` and **260**
/// without, and `inillucent-search` **55** with `embed` and **52** without. So
/// thirty tests were in the tree, had never run, and nothing said so. task-1952
/// found the first of them the only way it could be found - by reading the
/// source and noticing the test it wanted was not in the output - and
/// `inillucent-search/src/lib.rs` already carries the lesson beside
/// `embed_refusal`, which was pulled out from behind the feature for exactly
/// this reason.
///
/// The rule is a decision rather than an analysis: a new feature is named by a
/// selection row, so the runner builds it, or it is listed below with why it
/// holds nothing worth running. Both answers are fine; not answering is not.
#[test]
fn every_feature_is_either_built_or_written_off() {
    // A feature the runner does not build, and why that is right.
    const UNTESTED: [(&str, &str, &str); 6] = [
        (
            "inillucent-base",
            "testing",
            "carries `src/testing.rs`, the one skip helper three production crates' test \
             modules and the compat harness call. It adds no test of its own - the \
             module's two cases run under `cfg(test)` in `inillucent-base::lib`, which \
             the map already names - and it is turned on by each consumer's \
             `[dev-dependencies]` rather than by a runner row, so that a shipped build \
             does not carry a function whose purpose is to panic",
        ),
        (
            "inillucent-storage",
            "check",
            "turns on the integrity checker in `src/check.rs`, which this crate's own tests reach \
             through `cfg(test)`; the crate's test count is the same with it on and off",
        ),
        (
            "inillucent-storage",
            "opcode-probe",
            "brackets page edits for the profiling instruments and is never on in a shipped \
             build; the crate's test count is the same with it on and off",
        ),
        (
            "inillucent-compat",
            "testrun",
            "builds the runner itself, which is why it exists - a runner that built its own \
             binary could not replace a file it was executing",
        ),
        (
            "inillucent-engine",
            "embed",
            "passes `inillucent-search/embed` through and adds no test of its own; the tests \
             behind it are the search crate's, and that row names the feature",
        ),
        (
            "inillucent-cli",
            "embed",
            "passes `inillucent-engine/embed` through and adds no test of its own, for the \
             same reason",
        ),
    ];

    let root = workspace_root();
    let built: BTreeSet<String> = map()
        .rows
        .iter()
        .flat_map(|row| row.features.iter().cloned())
        .collect();
    let excused: BTreeSet<String> = UNTESTED
        .iter()
        .map(|(package, feature, _)| format!("{package}/{feature}"))
        .collect();

    let mut unanswered: Vec<String> = Vec::new();
    let mut read = 0usize;
    for member in members() {
        // `members()` answers the paths the root manifest lists -
        // `crates/inillucent-core` - and the package name is the last
        // component. Joining the whole thing under `crates/` again produced
        // `crates/crates/inillucent-core`, which is not a file, so the loop
        // read no manifest and the check passed having compared nothing. It
        // was caught by reverting a row and watching this stay green, which is
        // the only way that kind of pass ever shows itself.
        let manifest = root.join(&member).join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        read = read.saturating_add(1);
        let package = member.rsplit('/').next().unwrap_or(&member).to_string();
        for feature in declared_features(&text) {
            let named = format!("{package}/{feature}");
            if built.contains(&named) || excused.contains(&named) {
                continue;
            }
            unanswered.push(named);
        }
    }
    assert!(
        read >= 20,
        "read {read} manifests, which means this is looking in the wrong place rather than          that the workspace has no features"
    );
    assert!(
        unanswered.is_empty(),
        "these features are neither built by the runner nor written off:\n  {}\n\
         A test behind a feature nothing turns on is in no binary and reads as coverage. \
         Either add `features = [\"<package>/<feature>\"]` to that package's row in \
         `tests/selection.toml`, or add the feature to `UNTESTED` in this test with the \
         reason it holds nothing worth running.",
        unanswered.join("\n  ")
    );
}

/// Returns the features a manifest's `[features]` table declares.
///
/// `default` is not one: it is the list the build already uses, so it can hide
/// nothing.
///
/// @param manifest - the text of a `Cargo.toml`
fn declared_features(manifest: &str) -> Vec<String> {
    let mut features = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == "[features]";
            continue;
        }
        if !inside || trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let Some((name, _)) = trimmed.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name == "default" || name.is_empty() {
            continue;
        }
        features.push(name.to_string());
    }
    features
}

/// A change to the transaction crate has to select the segment suites.
///
/// **It did not (task-1932, M11).** `segmented_generations`,
/// `segment_merge_bound` and `segment_delta_chain` declared
/// `covers = ["inillucent-search"]` and nothing else, and all three drive a
/// real database through `inillucent_engine::connect::Database` - the pool, the
/// tree, the log and the transaction crate, none of which the list named.
///
/// The review that found this said a change to `leaf.rs` selected none of the
/// three. That part is not true today and is worth writing down so nobody
/// re-derives it: `inillucent-search` depends on `inillucent-ext`, which
/// depends on `inillucent-catalog`, which depends on `inillucent-tree` and
/// `inillucent-pool` - so a tree change already reached all three suites
/// through the closure, and so did a pool change and a log change.
/// `inillucent-txn` is the one nothing carried: no crate between it and
/// `inillucent-search` exists, so a change to the transaction crate ran none of
/// the three suites that hold a segment's contents to what one exhaustive scan
/// answers. That is the case asserted here, and it is the case that fails if
/// `covers` is narrowed back.
#[test]
fn a_change_to_the_transaction_crate_selects_the_segment_suites() {
    let map = map();
    let members = members();
    let manifests =
        layering::read_members(&workspace_root(), &members).expect("the manifests parse");
    let graph = selection::dependents(&manifests);

    let changed = selection::seeds_of(
        &map,
        &members,
        &manifests,
        &["crates/inillucent-txn/src/lib.rs".to_string()],
    );
    let selected: BTreeSet<String> = selection::select(&map, &changed, &graph)
        .iter()
        .map(|row| format!("{}::{}", row.target.package, row.target.name))
        .collect();

    for suite in [
        "inillucent-compat::segmented_generations",
        "inillucent-compat::segment_merge_bound",
        "inillucent-compat::segment_delta_chain",
    ] {
        assert!(
            selected.contains(suite),
            "a change to `crates/inillucent-txn/src/lib.rs` did not select `{suite}`, which \
             drives a database through that crate and is one of the three suites that catch \
             a half merged segment"
        );
    }
}

/// Every row in the timing ledger has to name a target that exists.
///
/// **`inillucent-compat::capi` was in it, and its suite was deleted in
/// `963dd80` (task-1932, M11).** The ledger is what the runner packs its
/// parallel schedule from, so a row for a target nothing can run is a number
/// that is read on every run and can never be used - and, worse, it reads as
/// evidence that the suite is still being measured.
///
/// The ledger is parsed here rather than through the runner's own reader,
/// because the runner is behind the `testrun` feature and this suite is not.
/// Two `[[timing]]` keys is a small enough grammar to read directly.
#[test]
fn every_timing_row_names_a_live_target() {
    let text = std::fs::read_to_string(workspace_root().join("tests/timings.toml"))
        .expect("the timing ledger is in the repository");
    let live: BTreeSet<String> = map()
        .rows
        .iter()
        .map(|row| format!("{}::{}", row.target.package, row.target.name))
        .collect();

    let mut named = 0usize;
    let mut dead = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("target = \"") else {
            continue;
        };
        let Some(target) = rest.strip_suffix('"') else {
            continue;
        };
        named = named.saturating_add(1);
        // A lib harness is written `package` with no `::`, and the map names it
        // with the package's own name as the target name.
        let known = live.contains(target)
            || live.contains(&format!("{target}::{target}"))
            || map()
                .rows
                .iter()
                .any(|row| row.target.kind == Kind::Lib && row.target.package == target);
        if !known {
            dead.push(target.to_string());
        }
    }
    assert!(
        named > 50,
        "read {named} timing rows, which means this is parsing the wrong thing"
    );
    assert!(
        dead.is_empty(),
        "these rows in tests/timings.toml name a target that tests/selection.toml does not \
         declare, so the runner reads a number it can never use:\n{}",
        dead.join("\n")
    );
}
