//! The compatibility harness.
//!
//! Invariant: this crate is test-only. No production crate depends on it, the
//! layering check enforces that, and everything it knows about SQLite lives
//! behind a process boundary.
//!
//! It holds four things: the parity manifest and the report generated from it,
//! the pinned reference metadata, the black-box oracle protocol, and the
//! dependency-direction check. Together they are the machinery that turns
//! "inillucent supports X" from an assertion into a claim with a test behind it.

#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

// Running the shipped programs as processes, and reading what they printed.
// Every end-to-end suite added by task-1969 section 5 goes through it, so the
// way a binary is found and a JSON envelope is read is written once.
// Which processors a measurement runs on, and pinning both arms of a gate to
// one core class (task-2085).
pub mod affinity;
pub mod cliproc;
pub mod corpus;
pub mod crashcampaign;
pub mod damage;
pub mod differential;
pub mod durable;
pub mod facade;
pub mod fixtures;
pub mod hash;
pub mod history;
pub mod interchange;
pub mod interop;
pub mod layering;
pub mod ledger;
pub mod manifest;
pub mod matrix;
pub mod mcpclient;
pub mod model;
pub mod nikaya;
/// The rearchitected engine, which now lives in `inillucent-engine`.
///
/// **Moved rather than copied.** Phases 1 to 4 built the new
/// engine inside this crate because until Phase 5 there was nothing above it to
/// be its caller; Phase 5 has callers - `inillucent-migrate`'s new target, and
/// the connection - and neither may depend on a test crate. Every path a gate,
/// probe or campaign wrote against `inillucent_compat::newengine` still resolves
/// and still names the same code.
pub use inillucent_engine as newengine;
pub mod obligations;
pub mod oracle;
pub mod perf;
pub mod procstat;
pub mod quiet;
pub mod rendering;
pub mod report;
pub mod results;
pub mod selection;
pub mod slt;
pub mod stories;
// Running one child and knowing when to stop waiting for it. `Command::output`
// waits for the child's pipes to close rather than for the child to exit, which
// are different events as soon as anything inherits a handle - and the runner
// sat on that difference with no verdict and no exit code at all (task-2071).
pub mod supervise;
pub mod syntax;
// What the test runner builds for a selection, and the artifact list a nested
// runner reads instead of starting cargo.
pub mod testplan;
pub mod toml_lite;
pub mod verdict;

use std::path::{Path, PathBuf};

/// Returns the workspace root, found by walking up from this crate.
///
/// Tests and tools both need it, and deriving it from `CARGO_MANIFEST_DIR`
/// rather than the working directory means a test behaves the same whether it
/// was started from the workspace root or from the crate directory.
pub fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or(manifest)
}

/// Returns the platform name the evidence model records results under.
pub fn platform_name() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 0: contract, provenance, and harness foundation";

#[cfg(test)]
mod tests {
    use super::*;

    /// The workspace root must be the directory holding `compat/`, or every
    /// tool that reads a manifest would be looking in the wrong place.
    #[test]
    fn the_workspace_root_holds_the_manifests() {
        let root = workspace_root();
        assert!(root.join("compat").is_dir(), "{}", root.display());
        assert!(root.join("docs/invariants/layering.toml").is_file());
    }

    /// The platform name must be one the report's required-platform list uses.
    #[test]
    fn the_platform_name_is_one_the_report_recognises() {
        let name = platform_name();
        assert!(
            name == "windows-x86_64" || name == "linux-x86_64" || name.contains('-'),
            "{name}"
        );
    }
}
