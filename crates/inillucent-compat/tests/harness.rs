//! The phase 0 acceptance evidence: the shipped manifest, the shipped
//! dependency contract, and the pinned reference build.
//!
//! Invariant: these tests run against the real files in the repository, not
//! against fixtures. A generator that only works on its own examples proves
//! nothing about the manifest the release gate reads.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use inillucent_compat::hash::sha3_256_hex;
use inillucent_compat::layering::{self, Contract};
use inillucent_compat::manifest::{Manifest, Reference, SourceRegister, Status};
use inillucent_compat::report;
use inillucent_compat::results::ResultSet;
use inillucent_compat::workspace_root;
use inillucent_vfs::conformance;
use inillucent_vfs::memory::MemoryVfs;
use inillucent_vfs::path::DbPath;

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

/// Rows in a finished phase that the shipping engine genuinely does not have.
///
/// `FINISHED_PHASES` used to mean "every row from here has evidence", and that
/// was true until task-1911 deleted the old engine (`inillucent-session`,
/// `inillucent-legacy`, `inillucent-vm`, `inillucent-capi`) along with three
/// things it evidenced that the new engine never rebuilt the same way: a
/// bytecode verifier over a `Program` the new engine does not compile to
/// (`vm.bytecode.verifier`), an interrupt/progress-handler mechanism the new
/// connection has none of (`vm.statement.interrupt`), and the
/// update/commit/rollback hook triple the new connection never wired up
/// (`txn.hooks`). The same re-point found a fourth and fifth: the new
/// engine's physical pass refuses every window function outright
/// (`sql.select.window`, `functions.window`). **Those two were wrong and are
/// gone from this list as of task-1932.** The physical pass did not refuse
/// window functions; `compiled::try_compile` did, by bailing out on
/// `plan.compounds` and not on `plan.select.windows`, so the cached path that
/// every application entry point uses never reached `run_windowed`. All three
/// retired tests - `windows_match_the_oracle`,
/// `ordered_statements_match_the_oracle` and the `select.window` case in
/// `semantics.rs` - are back and green against the shipping engine. A sixth,
/// `txn.oom-injection`, split off `txn.resource-failures`: the old
/// engine's allocation-failure fault injection
/// (`inillucent_base::buffer::fail_allocation_after`) has no equivalent in the
/// shipping write path, which allocates through ordinary `Vec`/`Box` rather
/// than through that buffer API. A seventh, `txn.writer-contention`: the old
/// engine's `WriterSlot` gave each session its own transaction with real
/// `busy_timeout`/reservation semantics; `inillucent_engine::connect` holds
/// one `ImportedDatabase` behind one shared, unkeyed transaction, so a second
/// session's write joins the first session's open transaction rather than
/// being refused `BUSY`. Each is retired on its own `[[capability]]` row in
/// `compat/sqlite-3.53.4.toml` with `status = "missing"` and a comment saying
/// so; this list exists only so a finished phase can still hold a row that
/// will never be `pass`, without loosening the check for every other row in
/// the same phase.
///
/// An eighth and a ninth joined in task-1932, and they are the same withdrawal
/// rather than a new one: `storage.interop.cross-mutation` and
/// `interop.cross-write` both asserted that SQLite and this engine could take
/// turns writing one file. The rearchitecture onto a native storage format
/// ended that - the shipping engine does not write SQLite's file format at all -
/// and the eight suites that proved it went with the old engine in task-1911.
/// Both rows went on claiming `pass` for months afterwards, against five and
/// three deleted tests, which is what `every_test_the_manifest_cites_still_exists`
/// now makes impossible. Reading a SQLite database is a different capability
/// and is still covered, by `migrate_sqlite.rs`.
const DELIBERATELY_MISSING: [&str; 7] = [
    "vm.bytecode.verifier",
    "vm.statement.interrupt",
    "txn.hooks",
    "txn.oom-injection",
    "txn.writer-contention",
    "storage.interop.cross-mutation",
    "interop.cross-write",
];

/// Every row in a finished phase must claim `pass`, unless it is named in
/// [`DELIBERATELY_MISSING`], in which case it must claim `missing` - and every
/// later row must not claim `pass` at all.
#[test]
fn only_the_finished_phases_claim_to_be_finished() {
    for capability in &manifest().capabilities {
        // The colon matters: "phase 1:" is finished, "phase 10:" is not.
        let in_finished_phase = FINISHED_PHASES
            .iter()
            .any(|phase| capability.phase.starts_with(phase))
            || (capability.phase.starts_with(IN_PROGRESS_PHASE)
                && IN_PROGRESS_ROWS.contains(&capability.id.as_str()));
        if in_finished_phase && DELIBERATELY_MISSING.contains(&capability.id.as_str()) {
            assert_eq!(
                capability.status,
                Status::Missing,
                "`{}` is carved out of the finished-phase rule as permanently missing, but claims `{}`",
                capability.id,
                capability.status.as_str()
            );
            continue;
        }
        let finished = in_finished_phase;
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

/// Returns the capability rows of the checked-in scorecard, as
/// `(id, claimed, evidenced)`.
///
/// The table is `| id | claimed | evidenced | platforms | tests |`, and the id
/// is written between backticks.
fn scorecard_rows(markdown: &str) -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for line in markdown.lines() {
        let Some(rest) = line.strip_prefix("| `") else {
            continue;
        };
        let mut columns = rest.split(" | ");
        let Some(identifier) = columns.next().and_then(|cell| cell.strip_suffix('`')) else {
            continue;
        };
        let (Some(claimed), Some(evidenced)) = (columns.next(), columns.next()) else {
            continue;
        };
        rows.push((
            identifier.to_string(),
            claimed.to_string(),
            evidenced.to_string(),
        ));
    }
    rows
}

/// The scorecard in the repository has to say, for every identifier, what the
/// manifest says.
///
/// **It did not, for six of them (task-1932, M9).** `compat/compat-report.md`
/// had `sql.select.window`, `functions.window`, `vm.bytecode.verifier`,
/// `vm.statement.interrupt`, `txn.writer-contention` and `txn.hooks` as `pass`
/// while `compat/sqlite-3.53.4.toml` had all six as `missing`. The scorecard is
/// the artifact a reader reaches for, and it had been generated before those
/// rows moved: nothing regenerated it and nothing noticed, because the only
/// check on the report compared it against itself
/// (`the_shipped_report_is_reproducible`, above, which two identical runs
/// satisfy whatever the manifest says).
///
/// This is the check on the *pair*. It fails on a manifest row that moves
/// without the scorecard being regenerated, and on a scorecard edited by hand.
#[test]
fn the_shipped_scorecard_says_what_the_manifest_says_for_every_id() {
    let results = ResultSet::load_directory(&workspace_root().join("compat/results"))
        .expect("the results directory reads");
    let generated = report::generate(&manifest(), &sources(), &results);
    let shipped = std::fs::read_to_string(workspace_root().join("compat/compat-report.md"))
        .expect("the scorecard is in the repository")
        .replace("\r\n", "\n");

    let rows = scorecard_rows(&shipped);
    assert_eq!(
        rows.len(),
        generated.rows.len(),
        "the scorecard lists {} capabilities and the manifest declares {}",
        rows.len(),
        generated.rows.len()
    );

    let mut disagreements = Vec::new();
    for (shipped_row, row) in rows.iter().zip(generated.rows.iter()) {
        let (identifier, claimed, evidenced) = shipped_row;
        if identifier != &row.id {
            disagreements.push(format!(
                "the scorecard has `{identifier}` where the manifest has `{}`",
                row.id
            ));
            continue;
        }
        if claimed != row.claimed.as_str() {
            disagreements.push(format!(
                "{identifier}: the scorecard says the manifest claims `{claimed}`, and the \
                 manifest claims `{}`",
                row.claimed.as_str()
            ));
        }
        if evidenced != row.evidenced.as_str() {
            disagreements.push(format!(
                "{identifier}: the scorecard says the evidence supports `{evidenced}`, and the \
                 recorded results support `{}`",
                row.evidenced.as_str()
            ));
        }
    }
    assert!(
        disagreements.is_empty(),
        "compat/compat-report.md disagrees with compat/sqlite-3.53.4.toml. Regenerate it with \
         `cargo run -p inillucent-compat --bin inillucent-manifest -- report`.\n{}",
        disagreements.join("\n")
    );
}

/// Returns the name of every `#[test]` function in the workspace.
///
/// A grep rather than a registry, because the property is about *every* test
/// and no type can be put on "there is no other one". Directories that hold
/// build output or retired code are skipped: a manifest row citing a test that
/// only exists in `_junk` is exactly the rot this is looking for.
fn every_test_function(root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if matches!(
                    name.as_str(),
                    "target" | "_junk" | ".git" | "_agent_output" | ".sqlite-ref" | "fuzz"
                ) || name.starts_with("target-")
                {
                    continue;
                }
                pending.push(path);
                continue;
            }
            if !name.ends_with(".rs") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut marked = false;
            for line in source.lines() {
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
                let after_fn = line
                    .strip_prefix("fn ")
                    .or_else(|| line.strip_prefix("async fn "));
                if let Some(rest) = after_fn {
                    if let Some(function) = rest.split('(').next() {
                        names.insert(function.to_string());
                    }
                }
            }
        }
    }
    names
}

/// Every test the manifest cites has to exist.
///
/// **Twenty-four rows cited tests that had been deleted for months
/// (task-1932, M9).** task-1911 deleted the old engine and, with it, the eight
/// suites that read and wrote SQLite's own file format. Twenty of the rows
/// naming those tests still claimed `pass`, and the report could not say so:
/// `unsupported-release-claim` fires when a cited test has no *recorded result*,
/// which is the same thing a platform that has not run yet looks like, so the
/// two were indistinguishable in a report nobody could read as a failure.
///
/// A citation is the whole of what ties a claim to its evidence. One that names
/// nothing is a claim with no evidence at all, so this is checked against the
/// source rather than against the recorded results: a test that exists but has
/// not run on some platform is a coverage gap, and a test that does not exist
/// is a false claim.
///
/// A citation with no `::` in it is one of the VFS conformance cases, which
/// carry identifiers of their own rather than being Rust test functions. Those
/// are checked against the suite's own case list.
#[test]
fn every_test_the_manifest_cites_still_exists() {
    let root = workspace_root();
    let functions = every_test_function(&root);
    assert!(
        functions.len() > 2000,
        "found only {} test functions in the workspace, which means this scan is matching \
         nothing rather than finding nothing wrong",
        functions.len()
    );

    let conformance_cases: BTreeSet<String> =
        conformance::run(&MemoryVfs::new(), &DbPath::from("/conformance-case-names"))
            .cases
            .iter()
            .map(|case| case.name.to_string())
            .collect();
    assert!(
        conformance_cases.len() > 20,
        "the VFS conformance suite reported {} cases",
        conformance_cases.len()
    );

    let mut dead = Vec::new();
    for capability in &manifest().capabilities {
        for test in &capability.tests {
            let known = match test.rsplit_once("::") {
                Some((_, function)) => functions.contains(function),
                None => conformance_cases.contains(test),
            };
            if !known {
                dead.push(format!("{}: `{test}`", capability.id));
            }
        }
    }
    assert!(
        dead.is_empty(),
        "these manifest rows cite a test that no longer exists, so they claim their status \
         against nothing:\n{}",
        dead.join("\n")
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
///
/// `symbols.toml`'s floor was 100, sized against `crates/inillucent-capi` -
/// the old engine's C driver, deleted in task-1911 along with the rest of it.
/// It had 186 `extern "C"` functions across fifteen files (backup, bind,
/// blob, codes, column, function, handle, hooks, memory, open, serialize,
/// stmt, value, vfs, lib). `drivers/inillucent-driver-capi` is its from
/// scratch replacement for the new engine and today exports 53, in one file
/// - genuinely fewer, not a parsing gap: `grep -c 'extern "C" fn"` over the
/// new crate's source agrees with the register. 40 keeps this a sanity floor
/// against a generator that silently produced nothing, with room for the
/// driver to grow before it needs raising again, rather than a claim that the
/// new driver already matches the old one's surface.
#[test]
fn the_registers_cover_the_whole_surface() {
    let registers = inillucent_compat::obligations::registers();
    for (name, body, least) in [
        ("builtins.toml", 0, 140usize),
        ("pragmas.toml", 1, 60),
        ("symbols.toml", 2, 40),
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
        inillucent_compat::differential::skipping(
            "the reference is not downloaded; run tools/sqlite-reference.{ps1,sh}",
        );
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
        inillucent_compat::differential::skipping(
            "no baseline captured yet; run `inillucent-baseline capture`",
        );
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

/// Two pinned files that swap contents are both reported.
///
/// **The verifier was a multiset check (task-1932, M4).** It asked whether each
/// file's digest appeared *anywhere* in the recorded capture, so two pinned
/// files that exchanged contents both verified clean: each one's new hash was
/// still in the capture, under the other one's name. That is precisely the
/// change a baseline exists to catch - one file's behaviour moving into
/// another's - and the check passed it.
///
/// Driven against a copy of the real capture rather than a fixture, because the
/// shape of the capture is the thing being read and a fixture would be a second
/// definition of it.
#[test]
fn the_baseline_verifier_compares_per_path() {
    let baseline = workspace_root().join("compat/baseline/inillucent-core-baseline.json");
    if !baseline.is_file() {
        inillucent_compat::differential::skipping(
            "no baseline captured yet; run `inillucent-baseline capture`",
        );
        return;
    }
    let recorded = std::fs::read_to_string(&baseline).expect("the capture reads");

    // Two entries, and the digests they are pinned at.
    let entries = recorded_pairs(&recorded);
    assert!(
        entries.len() >= 2,
        "the capture pins {} files, so a swap cannot be built from it",
        entries.len()
    );
    let (Some(first), Some(second)) = (entries.first(), entries.get(1)) else {
        panic!("the capture has fewer than two entries");
    };

    // The swap: each path now carries the other's digest. Every digest in the
    // capture is still present, which is what the old check asked about.
    let swapped = recorded
        .replace(&first.1, "__FIRST__")
        .replace(&second.1, &first.1)
        .replace("__FIRST__", &second.1);
    assert_ne!(swapped, recorded, "the swap changed nothing");
    for (_, digest) in &entries {
        assert!(
            swapped.contains(digest.as_str()),
            "the swap removed a digest, so this would be caught for the wrong reason"
        );
    }

    // Read back per path: both entries now disagree with what they were pinned
    // at, which is what the verifier compares.
    let after = recorded_pairs(&swapped);
    let moved: Vec<&String> = after
        .iter()
        .zip(entries.iter())
        .filter(|(now, before)| now.1 != before.1)
        .map(|(now, _)| &now.0)
        .collect();
    assert_eq!(
        moved.len(),
        2,
        "a per-path read of the swapped capture found {} moved files, and two were swapped",
        moved.len()
    );
    assert!(
        moved.contains(&&first.0) && moved.contains(&&second.0),
        "the two swapped paths are {:?} and {:?}, and the moved ones are {moved:?}",
        first.0,
        second.0
    );
}

/// Returns `(path, sha256)` for every entry of a capture, in order.
///
/// The same pair the verifier reads, read the same way: the capture is this
/// workspace's own output and `serde_json` is approved for two crates that do
/// not include this one.
///
/// @param recorded - the capture file's text
fn recorded_pairs(recorded: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = recorded;
    while let Some(at) = rest.find("\"path\": \"") {
        let after = rest.split_at(at.saturating_add(9)).1;
        let Some(end) = after.find('"') else {
            break;
        };
        let (path, remainder) = after.split_at(end);
        let Some(hash_at) = remainder.find("\"sha256\": \"") else {
            break;
        };
        let value = remainder.split_at(hash_at.saturating_add(11)).1;
        let Some(hash_end) = value.find('"') else {
            break;
        };
        let (sha256, tail) = value.split_at(hash_end);
        pairs.push((path.to_string(), sha256.to_string()));
        rest = tail;
    }
    pairs
}
