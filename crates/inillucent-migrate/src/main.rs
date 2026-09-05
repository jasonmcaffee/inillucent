//! The migration tool.
//!
//! Invariant: this program reads the source and writes somewhere else. There is
//! no flag that makes it write to the source, and no flag that makes it delete
//! anything. What it can be told is where to put the result, whether to publish
//! it or stop at a verified staging file, and which SQLite to prove the file
//! with.
//!
//! ```text
//! inillucent-migrate <source-index-dir> <destination.db> [--no-publish] [--sqlite <path>]
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

use std::path::PathBuf;
use std::process::ExitCode;

use inillucent_migrate::{migrate, Plan};

/// Runs the migration named on the command line.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let positional: Vec<&String> = arguments
        .iter()
        .filter(|argument| !argument.starts_with("--"))
        .collect();
    let (Some(source), Some(destination)) = (positional.first(), positional.get(1)) else {
        eprintln!(
            "usage: inillucent-migrate <source-index-dir> <destination.db> [--no-publish] \
             [--sqlite <path>]"
        );
        return ExitCode::FAILURE;
    };
    let mut plan = Plan::new(source, destination);
    plan.publish = !arguments.iter().any(|argument| argument == "--no-publish");
    plan.sqlite = flag(&arguments, "--sqlite");
    match migrate(&plan) {
        Ok(outcome) => {
            println!(
                "copied {} documents and {} chunks",
                outcome.documents, outcome.chunks
            );
            for check in &outcome.checks {
                println!(
                    "  {} {} {}",
                    if check.passed { "pass" } else { "FAIL" },
                    check.name,
                    check.detail
                );
            }
            println!("manifest: {}", outcome.manifest.display());
            match &outcome.published {
                Some(path) => {
                    println!("published: {}", path.display());
                    ExitCode::SUCCESS
                }
                None if outcome.verified() => {
                    println!("verified, not published");
                    ExitCode::SUCCESS
                }
                None => {
                    eprintln!("verification failed; nothing was published");
                    ExitCode::FAILURE
                }
            }
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--flag value` argument.
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}
