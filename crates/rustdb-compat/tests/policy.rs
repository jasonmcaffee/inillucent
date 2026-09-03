//! The checks that keep the engine's own rules true.
//!
//! Invariant: every rule `docs/dependency-policy.md` states about unsafe code,
//! documentation, formatting, and provenance is checked here. A rule that is
//! only written down has already been broken somewhere.
//!
//! These cover the phase 0-1 crates only. `rustdb-core` and `rustdb-bench`
//! predate the policy and are deliberately left alone; task-1782 does not touch
//! the retrieval engine, and reformatting it would make that claim harder to
//! verify rather than easier.

use std::path::{Path, PathBuf};
use std::process::Command;

use rustdb_compat::workspace_root;

/// The crates the policy applies to.
const GOVERNED: [&str; 11] = [
    "rustdb-base",
    "rustdb-vfs",
    "rustdb-sim",
    "rustdb-value",
    "rustdb-storage",
    "rustdb-sql",
    "rustdb-catalog",
    "rustdb-vm",
    "rustdb-session",
    "rustdb",
    "rustdb-compat",
];

/// The only files allowed to contain `unsafe`.
///
/// The first two are the operating-system boundary, which cannot be crossed in
/// safe Rust. Everything else in the engine is safe code, and the crate-level
/// `forbid(unsafe_code)` in `rustdb-base` says so to the compiler as well.
///
/// The third is a measurement binary, not the engine: a global allocator is the
/// only way to count heap allocations, and `GlobalAlloc` is an unsafe trait. It
/// is admitted here rather than quietly because the charter is about what the
/// engine is made of, and a baseline tool that never ships is not part of it -
/// but a file with `unsafe` in it should still have to say why, in writing, in
/// a list somebody reads.
const UNSAFE_ALLOWED: [&str; 3] = [
    "crates/rustdb-vfs/src/os/windows.rs",
    "crates/rustdb-vfs/src/os/unix.rs",
    "crates/rustdb-compat/src/bin/sqlperf.rs",
];

/// Returns every `.rs` file under a directory.
fn rust_files(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found
}

/// Returns the path relative to the workspace root, with forward slashes.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every `unsafe` block must sit in one of the two operating-system files, and
/// each must carry a safety argument. An unsafe block with no written reason is
/// one nobody can review.
#[test]
fn unsafe_code_is_confined_and_justified() {
    let root = workspace_root();
    let mut offenders = Vec::new();
    let mut justified = 0usize;
    for crate_name in GOVERNED {
        for file in rust_files(&root.join("crates").join(crate_name)) {
            let name = relative(&root, &file);
            // This file names the word in every check it makes.
            if name.ends_with("tests/policy.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("the source reads");
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if !line.contains("unsafe ") && !line.contains("unsafe{") {
                    continue;
                }
                // The policy statements themselves mention the word.
                if line.trim_start().starts_with("//") || line.contains("unsafe_code") {
                    continue;
                }
                if !UNSAFE_ALLOWED.contains(&name.as_str()) {
                    offenders.push(format!("{name}:{}: {}", index + 1, line.trim()));
                    continue;
                }
                let start = index.saturating_sub(8);
                let preceding = lines.get(start..index).unwrap_or(&[]);
                if preceding.iter().any(|line| line.contains("SAFETY:")) {
                    justified += 1;
                } else {
                    offenders.push(format!("{name}:{}: no SAFETY comment", index + 1));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
    assert!(
        justified >= 5,
        "the operating-system boundary should have several justified unsafe blocks, found {justified}"
    );
}

/// Every governed crate must deny undocumented public items, so a public
/// function without a doc comment is a build error rather than a review note.
#[test]
fn every_governed_crate_denies_undocumented_items() {
    let root = workspace_root();
    for crate_name in GOVERNED {
        let lib = root.join("crates").join(crate_name).join("src/lib.rs");
        let text = std::fs::read_to_string(&lib).expect("the crate root reads");
        assert!(
            text.contains("#![deny(missing_docs)]"),
            "{crate_name} does not deny missing docs"
        );
        for lint in [
            "clippy::indexing_slicing",
            "clippy::unwrap_used",
            "clippy::expect_used",
            "clippy::panic",
        ] {
            assert!(text.contains(lint), "{crate_name} does not deny {lint}");
        }
    }
}

/// Every module must open with a comment, and the first paragraph must state
/// the invariant it keeps. This is the rule that makes the codebase readable
/// six months from now, and it is cheap enough to check.
#[test]
fn every_module_states_its_invariant() {
    let root = workspace_root();
    let mut offenders = Vec::new();
    for crate_name in GOVERNED {
        for file in rust_files(&root.join("crates").join(crate_name)) {
            let name = relative(&root, &file);
            let text = std::fs::read_to_string(&file).expect("the source reads");
            if !text.starts_with("//!") {
                offenders.push(format!("{name}: no module comment"));
                continue;
            }
            let header: String = text
                .lines()
                .take_while(|line| line.starts_with("//!") || line.trim().is_empty())
                .collect::<Vec<&str>>()
                .join("\n");
            if !header.contains("Invariant:") {
                offenders.push(format!("{name}: the module comment states no invariant"));
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
}

/// The governed crates must be formatted, so a diff shows what changed rather
/// than how it was wrapped.
#[test]
fn the_governed_crates_are_formatted() {
    let root = workspace_root();
    let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
    command.current_dir(&root).arg("fmt");
    for crate_name in GOVERNED {
        command.arg("-p").arg(crate_name);
    }
    let output = command
        .args(["--", "--check"])
        .output()
        .expect("cargo fmt runs");
    assert!(
        output.status.success(),
        "run `cargo fmt` on the governed crates:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Nothing in the reference register may be a production dependency. The
/// register is the record of what was consulted; the moment one of its entries
/// becomes a dependency, the first-party charter is broken.
#[test]
fn no_reference_is_a_production_dependency() {
    let root = workspace_root();
    let text = std::fs::read_to_string(root.join("docs/reference-register.toml"))
        .expect("the register reads");
    assert!(text.contains("production_dependency = false"));
    assert!(
        !text.contains("production_dependency = true"),
        "a consulted reference has become a dependency"
    );
    let entries = text.matches("[[reference]]").count();
    let declarations = text
        .lines()
        .filter(|line| line.trim_start().starts_with("production_dependency"))
        .count();
    assert_eq!(
        entries, declarations,
        "every reference must declare whether it is a production dependency"
    );
    assert!(
        entries >= 8,
        "the register looks incomplete: {entries} entries"
    );
}

/// The dependency policy has to name every third-party crate the layering
/// contract allows, so the argument for each one is written down somewhere a
/// reviewer will find it.
#[test]
fn the_dependency_policy_covers_what_the_contract_allows() {
    let root = workspace_root();
    let policy =
        std::fs::read_to_string(root.join("docs/dependency-policy.md")).expect("the policy reads");
    for named in ["libc", "windows-sys", "allow-list", "first-party"] {
        assert!(
            policy.contains(named),
            "the policy does not mention `{named}`"
        );
    }
    assert!(
        policy.contains("other database engine installed"),
        "the policy has lost its ownership rule"
    );
}
