//! Runs the engine suites and records what they actually did.
//!
//! Invariant: a result row exists because a test ran and reported an outcome on
//! this machine, not because anyone said it passes. The rows this writes are
//! the only thing that can move a capability to `pass` in the compatibility
//! report.
//!
//! Two kinds of evidence are collected. The VFS conformance suite is run here,
//! in process, against all three implementations, because its cases have stable
//! identifiers of their own and each case is a capability. Everything else is
//! collected by running `cargo test` per package and reading libtest's own
//! per-test outcome lines, so the recorder cannot disagree with the test run.
//!
//! Usage: `cargo run -p inillucent-compat --bin inillucent-evidence -- [--out <dir>]`

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use inillucent_compat::results::{ResultSet, TestResult};
use inillucent_compat::{platform_name, workspace_root};
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_vfs::conformance::{self, Outcome};
use inillucent_vfs::memory::MemoryVfs;
use inillucent_vfs::os::OsVfs;
use inillucent_vfs::path::DbPath;

/// The packages whose tests are the evidence.
///
/// Every crate that carries engine behaviour is here, including the C ABI and
/// the shell: a manifest row that names one of their tests can only be believed
/// if the run that produced the results actually ran it.
const PACKAGES: [&str; 17] = [
    "inillucent-base",
    "inillucent-vfs",
    "inillucent-sim",
    "inillucent-value",
    "inillucent-storage",
    "inillucent-transaction",
    "inillucent-sql",
    "inillucent-catalog",
    "inillucent-ext",
    "inillucent-search",
    "inillucent-vm",
    "inillucent-session",
    "inillucent",
    "inillucent-capi",
    "inillucent-cli",
    "inillucent-migrate",
    "inillucent-compat",
];

/// Collects the evidence and writes it, returning non-zero when a suite failed.
fn main() -> ExitCode {
    let root = workspace_root();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let out = argument(&arguments, "--out").unwrap_or_else(|| root.join("compat/results"));
    match collect(&root, &out) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(failures) => {
            eprintln!("{failures} recorded failures");
            ExitCode::FAILURE
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--flag value` argument.
fn argument(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}

/// Runs every suite and writes the result file, returning the failure count.
fn collect(root: &Path, out: &Path) -> Result<usize, String> {
    let platform = platform_name();
    let mut set = ResultSet::default();
    let mut failures = 0usize;

    failures += record_conformance(&mut set, &platform)?;
    for package in PACKAGES {
        failures += record_package(root, &mut set, &platform, package)?;
    }

    std::fs::create_dir_all(out)
        .map_err(|error| format!("cannot create {}: {error}", out.display()))?;
    let file = out.join(format!("{platform}.jsonl"));
    std::fs::write(&file, set.to_jsonl())
        .map_err(|error| format!("cannot write {}: {error}", file.display()))?;
    println!("{} results written to {}", set.len(), file.display());
    Ok(failures)
}

/// Runs the VFS conformance suite against all three implementations.
fn record_conformance(set: &mut ResultSet, platform: &str) -> Result<usize, String> {
    let mut failures = 0usize;
    let mut root = std::env::temp_dir();
    root.push(format!("inillucent-evidence-{}", std::process::id()));
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("cannot create a workspace: {error}"))?;

    let memory = MemoryVfs::new();
    let os = OsVfs::new();
    let simulator = SimVfs::new(SimConfig {
        seed: 1782,
        ..SimConfig::default()
    });
    let runs: Vec<(&dyn inillucent_vfs::contract::Vfs, DbPath)> = vec![
        (&memory, DbPath::from("/memory")),
        (&os, DbPath::new(root.clone())),
        (&simulator, DbPath::from("/sim")),
    ];
    for (vfs, workspace) in runs {
        let report = conformance::run(vfs, &workspace);
        for case in &report.cases {
            let passed = match &case.outcome {
                Outcome::Passed => true,
                Outcome::Skipped(_) => continue,
                Outcome::Failed(reason) => {
                    eprintln!("{}: {} failed: {reason}", report.vfs, case.name);
                    failures = failures.saturating_add(1);
                    false
                }
            };
            set.record(TestResult {
                test_id: case.name.to_string(),
                platform: platform.to_string(),
                passed,
                seed: 1782,
                artifact_sha256: String::new(),
            });
        }
        println!(
            "{}: {} of {} cases passed",
            report.vfs,
            report.passed(),
            report.cases.len()
        );
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(failures)
}

/// Runs one package's tests and records libtest's own outcome for each.
fn record_package(
    root: &Path,
    set: &mut ResultSet,
    platform: &str,
    package: &str,
) -> Result<usize, String> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()))
        .current_dir(root)
        .args([
            "test",
            "-p",
            package,
            "--no-fail-fast",
            "--",
            "--test-threads=4",
        ])
        .output()
        .map_err(|error| format!("cannot run cargo test -p {package}: {error}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut failures = 0usize;
    let mut recorded = 0usize;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name, verdict)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let passed = match verdict.trim() {
            "ok" => true,
            "FAILED" => false,
            _ => continue,
        };
        if !passed {
            failures = failures.saturating_add(1);
        }
        recorded = recorded.saturating_add(1);
        set.record(TestResult {
            test_id: format!("{package}::{}", name.trim()),
            platform: platform.to_string(),
            passed,
            seed: 0,
            artifact_sha256: String::new(),
        });
    }
    println!("{package}: {recorded} tests recorded, {failures} failed");
    Ok(failures)
}
