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

/// Every suite that needs the pinned reference says so.
///
/// The map's `requires` is what makes `--strict` able to report a run that
/// evidenced nothing. It is easy to add a differential suite and forget the
/// field, and the result is a suite that is silently allowed to skip.
#[test]
fn every_differential_target_declares_what_it_needs() {
    let map = map();
    let bare: Vec<String> = map
        .rows
        .iter()
        .filter(|row| row.tier == "differential" && row.requires.is_empty())
        .map(|row| row.target.label())
        .collect();
    assert!(
        bare.is_empty(),
        "these differential suites declare no prerequisite, so a run without the \
         pinned reference would look like a pass:\n  {}",
        bare.join("\n  ")
    );
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
    const UNTESTED: [(&str, &str, &str); 5] = [
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
