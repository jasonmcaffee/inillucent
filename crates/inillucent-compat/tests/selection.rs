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
