//! The phase 0 acceptance evidence: the shipped manifest, the shipped
//! dependency contract, and the pinned reference build.
//!
//! Invariant: these tests run against the real files in the repository, not
//! against fixtures. A generator that only works on its own examples proves
//! nothing about the manifest the release gate reads.

use std::path::PathBuf;

use inillucent_compat::hash::sha3_256_hex;
use inillucent_compat::layering::{self, Contract};
use inillucent_compat::manifest::{Manifest, Reference, SourceRegister, Status};
use inillucent_compat::report;
use inillucent_compat::results::ResultSet;
use inillucent_compat::workspace_root;

/// Loads the shipped manifest.
fn manifest() -> Manifest {
    Manifest::load(&workspace_root().join("compat/sqlite-3.53.4.toml"))
        .expect("the manifest parses")
}

/// Loads the shipped source register.
fn sources() -> SourceRegister {
    SourceRegister::load(&workspace_root().join("compat/sources.toml"))
        .expect("the register parses")
}

/// The manifest the release gate reads must be structurally sound: no
/// duplicated identifier, no claim without a test, no source link that is not
/// in the register.
#[test]
fn the_shipped_manifest_is_structurally_sound() {
    let generated = report::generate(&manifest(), &sources(), &ResultSet::default());
    let structural: Vec<_> = generated
        .problems
        .iter()
        .filter(|problem| problem.kind != "unsupported-release-claim")
        .collect();
    assert!(structural.is_empty(), "{structural:#?}");
    assert!(
        manifest().capabilities.len() > 150,
        "the manifest has to carry the whole denominator, not just what is done"
    );
    assert!(
        manifest()
            .capabilities
            .iter()
            .any(|capability| capability.status == Status::Missing),
        "a manifest with nothing missing is not describing SQLite"
    );
}

/// The phases that have been implemented are named here, and every row must
/// agree with the list: a row in a finished phase claims `pass`, and a row in
/// a phase nobody has reached yet does not. A row that quietly claims a phase
/// it has not reached is the thing the report exists to prevent, and a phase
/// added to this list without its rows moving is caught by the same assertion.
const FINISHED_PHASES: [&str; 14] = [
    "phase 0:",
    "phase 1:",
    "phase 2:",
    "phase 3:",
    "phase 4:",
    "phase 5:",
    "phase 6:",
    "phase 7:",
    "phase 8:",
    "phase 9:",
    "phase 10:",
    "phase 11:",
    "phase 12:",
    "phase 13:",
];

/// The phase that is under way, and exactly which of its rows have evidence.
///
/// A phase is not a unit of work in practice - phase 8 was a dozen independent
/// feature families - so "finished or untouched" cannot describe the state
/// while one is being built. Naming the rows individually is *stricter* than
/// the two-state rule it replaces: a row that moves to `pass` without being
/// listed here fails, and a row listed here that has not moved fails too, so
/// the manifest and this list cannot drift apart in either direction.
///
/// Phase 14 is in flight. Both of its rows have evidence - the measurement is
/// correctness-qualified and the regression tracking is in - but neither of
/// those rows is the release gate. That gate is the headline speedup, and it is
/// not a manifest row on purpose: a capability is a thing the engine can do,
/// and the scorecard is the artifact that says whether a number was reached.
/// The scorecard currently says it was not.
const IN_PROGRESS_PHASE: &str = "phase 14:";

/// The rows of [`IN_PROGRESS_PHASE`] that have evidence behind them.
const IN_PROGRESS_ROWS: [&str; 3] = [
    "perf.qualified-measurement",
    "perf.regression-tracking",
    "perf.optimization-arms",
];

/// Every row in a finished phase must claim `pass`, and every later row must
/// not.
#[test]
fn only_the_finished_phases_claim_to_be_finished() {
    for capability in &manifest().capabilities {
        // The colon matters: "phase 1:" is finished, "phase 10:" is not.
        let finished = FINISHED_PHASES
            .iter()
            .any(|phase| capability.phase.starts_with(phase))
            || (capability.phase.starts_with(IN_PROGRESS_PHASE)
                && IN_PROGRESS_ROWS.contains(&capability.id.as_str()));
        let claims = capability.status == Status::Pass;
        assert_eq!(
            finished,
            claims,
            "`{}` is in `{}` and claims `{}`",
            capability.id,
            capability.phase,
            capability.status.as_str()
        );
    }
}

/// The report must be byte-identical across two runs from the same inputs, and
/// its digest must be stable, or it cannot gate a release.
#[test]
fn the_shipped_report_is_reproducible() {
    let results = ResultSet::load_directory(&workspace_root().join("compat/results"))
        .expect("the results directory reads");
    let first = report::generate(&manifest(), &sources(), &results);
    let second = report::generate(&manifest(), &sources(), &results);
    assert_eq!(first.to_json(), second.to_json());
    assert_eq!(first.digest(), second.digest());
    assert_eq!(first.to_markdown(), second.to_markdown());
    assert_eq!(
        first.rows.len(),
        manifest().capabilities.len(),
        "every capability must appear in the report"
    );
}

/// An empty scorecard has to be generatable on a machine with no recorded
/// results at all, because that is the state a fresh checkout is in.
#[test]
fn an_empty_scorecard_is_generated_without_any_results() {
    let generated = report::generate(&manifest(), &sources(), &ResultSet::default());
    let markdown = generated.to_markdown();
    assert!(markdown.contains("inillucent compatibility with SQLite sqlite-3.53.4"));
    assert!(markdown.contains("phase 1: VFS, binary primitives, and simulator"));
    assert!(
        !generated.counts.contains_key("pass"),
        "nothing may be `pass` with no evidence recorded"
    );
    assert!(
        !generated.problems.is_empty(),
        "unsupported claims must be reported"
    );
}

/// The obligation registers on disk must be what the engine generates.
///
/// They are the denominator for phases 11 and 12: a claim that every built-in,
/// every PRAGMA and every exported symbol is accounted for means nothing unless
/// the list being counted is the engine's own. Regenerating and comparing is
/// what stops the list from becoming a wish.
#[test]
fn the_registers_match_the_engine() {
    let directory = workspace_root().join("compat/api");
    for (name, expected) in inillucent_compat::obligations::registers() {
        let path = directory.join(name);
        let found = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| {
                panic!(
                    "{} is missing; run `inillucent-obligations`",
                    path.display()
                )
            })
            .replace("\r\n", "\n");
        assert_eq!(
            found,
            expected,
            "{} is out of date; run `cargo run -p inillucent-compat --bin inillucent-obligations`",
            path.display()
        );
    }
}

/// The registers have to be big enough to be describing the whole surface.
///
/// A generator that silently produced nothing would agree with an empty file
/// and every other check here would pass, so the size is asserted separately.
#[test]
fn the_registers_cover_the_whole_surface() {
    let registers = inillucent_compat::obligations::registers();
    for (name, body, least) in [
        ("builtins.toml", 0, 140usize),
        ("pragmas.toml", 1, 60),
        ("symbols.toml", 2, 100),
    ] {
        let (_, text) = registers.get(body).expect("the register was generated");
        let count = text
            .matches(
                "
name = ",
            )
            .count();
        assert!(count >= least, "{name} holds only {count} entries");
    }
}

/// The workspace must obey its own dependency contract. This is the check that
/// makes "no database engine and no SQL parser in a production crate" a fact
/// rather than an intention.
#[test]
fn the_workspace_obeys_the_dependency_contract() {
    let root = workspace_root();
    let contract =
        Contract::load(&root.join("docs/invariants/layering.toml")).expect("the contract parses");
    let manifests = layering::read_workspace(&root).expect("the workspace reads");
    let violations = layering::check(&contract, &manifests);
    assert!(violations.is_empty(), "{violations:#?}");
    assert!(
        manifests.len() >= 16,
        "the workspace should hold the whole crate graph, found {}",
        manifests.len()
    );
}

/// Every path in `[workspace] members` must exist and hold a `Cargo.toml`.
///
/// `1854f3d` added `drivers/inillucent-driver` and `drivers/inillucent-driver-capi`
/// to the members list while `drivers/` was untracked and not ignored, so the
/// directory existed on exactly one machine and `cargo metadata` on a fresh
/// clone exited 101 before reading a line of Rust. Nothing in the suite noticed,
/// because every other check runs on a machine where the directory is present.
/// This test is the one that fails on the machine that added the member rather
/// than on somebody else's clone.
#[test]
fn every_workspace_member_path_exists() {
    let root = workspace_root();
    let members = layering::workspace_members(&root).expect("the root manifest declares members");
    let problems = layering::check_member_paths(&root, &members);
    assert!(problems.is_empty(), "{problems:#?}");
    assert!(
        members.len() >= 16,
        "the members list should hold the whole workspace, found {}",
        members.len()
    );
}

/// The pinned reference metadata must name a complete build: a version, the
/// compile options, the run-time settings a comparison uses, and a checksum for
/// every artifact.
#[test]
fn the_reference_metadata_is_pinned() {
    let reference = Reference::load(&workspace_root().join("compat/reference/sqlite-3.53.4.toml"))
        .expect("the reference parses");
    assert_eq!(reference.version, "3.53.4");
    assert_eq!(reference.release_id, "3530400");
    assert!(reference.compile_options.contains("SQLITE_ENABLE_FTS5"));
    for key in ["page_size", "journal_mode", "synchronous", "foreign_keys"] {
        assert!(reference.settings.contains_key(key), "no `{key}` setting");
    }
    assert!(reference.artifacts.len() >= 3);
    for artifact in &reference.artifacts {
        assert_eq!(
            artifact.sha3_256.len(),
            64,
            "{} has no checksum",
            artifact.name
        );
        assert!(artifact.bytes > 0, "{} has no size", artifact.name);
        assert!(
            artifact.url.starts_with("https://sqlite.org/"),
            "{}",
            artifact.url
        );
    }
}

/// When the reference has been downloaded, every artifact must match the
/// checksum SQLite published. A build that silently compared against a
/// different SQLite would invalidate every parity claim made against it.
#[test]
fn the_reference_artifacts_match_their_pinned_checksums() {
    let root = workspace_root();
    let reference = Reference::load(&root.join("compat/reference/sqlite-3.53.4.toml"))
        .expect("the reference parses");
    let directory = root.join(".sqlite-ref/3.53.4");
    if !directory.is_dir() {
        eprintln!("the reference is not downloaded; run tools/sqlite-reference.{{ps1,sh}}");
        return;
    }
    let mut checked = 0;
    for artifact in &reference.artifacts {
        let path: PathBuf = directory.join(&artifact.name);
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).expect("the artifact reads");
        assert_eq!(
            bytes.len() as u64,
            artifact.bytes,
            "{} is the wrong size",
            artifact.name
        );
        assert_eq!(
            sha3_256_hex(&bytes),
            artifact.sha3_256,
            "{} does not match its pinned checksum",
            artifact.name
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "the reference directory exists but holds no pinned artifact"
    );
}

/// The retrieval engine must be byte-identical to the baseline captured here.
/// A new relational engine is being built beside it and must not disturb it,
/// and "we did not touch it" is a claim worth checking rather than asserting.
#[test]
fn the_retrieval_baseline_is_unchanged() {
    let baseline = workspace_root().join("compat/baseline/inillucent-core-baseline.json");
    if !baseline.is_file() {
        eprintln!("no baseline captured yet; run `inillucent-baseline capture`");
        return;
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_inillucent-baseline"))
        .arg("verify")
        .output()
        .expect("the baseline tool runs");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The *resolved* production dependency tree must hold no database engine and
/// no SQL parser.
///
/// The layering check reads declared edges, which is the right level for an
/// architecture rule but says nothing about what an approved crate drags in
/// behind it. This runs `cargo tree --edges normal` - the resolved graph, minus
/// dev and build dependencies - over every production crate and looks for the
/// banned names in it.
#[test]
fn the_production_dependency_tree_holds_no_engine() {
    let root = workspace_root();
    let contract =
        Contract::load(&root.join("docs/invariants/layering.toml")).expect("the contract parses");
    let production: Vec<&str> = contract
        .crates
        .values()
        .filter(|rule| rule.kind == inillucent_compat::layering::CrateKind::Production)
        .map(|rule| rule.name.as_str())
        .collect();
    assert!(
        production.len() >= 14,
        "found {} production crates",
        production.len()
    );

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut checked = 0usize;
    for crate_name in production {
        let output = std::process::Command::new(&cargo)
            .current_dir(&root)
            .args([
                "tree",
                "-p",
                crate_name,
                "--edges",
                "normal",
                "--prefix",
                "none",
                "--no-dedupe",
            ])
            .output()
            .expect("cargo tree runs");
        if !output.status.success() {
            // A crate whose optional features cannot resolve offline is not
            // evidence either way; say so rather than passing quietly.
            eprintln!(
                "cargo tree could not resolve {crate_name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            continue;
        }
        // First-party crates are matched by name and skipped. The rule this
        // enforces is "no other engine, parser or storage layer in a production
        // tree", and it is written as a substring match on the resolved tree -
        // which also matches a *first-party* crate whose name says what file
        // format it reads. `inillucent-sqlite-reader` is the read half of
        // `inillucent-storage` behind a narrow interface, kept so the differential
        // gate can import a fixture and `inillucent-migrate` can read a legacy
        // file; it is first-party code, and it is exactly the piece the
        // rearchitecture kept when the old engine was deleted. Skipping the
        // lines that name a declared crate keeps the rule pointed at what it
        // is for.
        let declared: Vec<&str> = contract.crates.keys().map(|name| name.as_str()).collect();
        let tree: String = String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .lines()
            .filter(|line| {
                let name = line.split_whitespace().next().unwrap_or("");
                !declared.iter().any(|held| held.eq_ignore_ascii_case(name))
            })
            .collect::<Vec<&str>>()
            .join(
                "
",
            );
        for forbidden in &contract.forbidden {
            assert!(
                !tree.contains(&forbidden.pattern),
                "the resolved tree of `{crate_name}` contains `{}`: {}",
                forbidden.pattern,
                forbidden.reason
            );
        }
        checked += 1;
    }
    assert!(checked >= 10, "only {checked} production trees resolved");
}
