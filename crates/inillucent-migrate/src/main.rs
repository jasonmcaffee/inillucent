//! The migration tool.
//!
//! Invariant: this program reads the source and writes somewhere else. There is
//! no flag that makes it write to the source, and no flag that makes it delete
//! anything. What it can be told is where to put the result, whether to publish
//! it or stop at a verified staging file, and which SQLite to prove the file
//! with.
//!
//! Two sources, named by which one is given:
//!
//! ```text
//! inillucent-migrate <source-index-dir> <destination.db> [--no-publish] [--sqlite <path>]
//! inillucent-migrate --sqlite-file <source.db> <destination.rdb>
//! ```
//!
//! The second is task-1834's: a SQLite database file into the new engine's
//! trees, verified by counts and digests and published by a rename. It takes
//! no `--no-publish`, because it never publishes anything it has not verified
//! and always leaves the staging file behind when it does not.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

use std::path::PathBuf;
use std::process::ExitCode;

use inillucent_migrate::{migrate, sqlite, Plan};

/// Runs the migration named on the command line.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // **A flag's value is not a positional argument.** Filtering only the
    // `--`-prefixed words left `--sqlite-file src.db dest.rdb` with two
    // positionals, the first of which was the source - so the destination read
    // as the source and the tool refused to overwrite the file it had just been
    // asked to read. Skipping the word after a flag that takes one is what
    // makes the two forms parse the same way.
    let takes_a_value = ["--sqlite", "--sqlite-file"];
    let mut positional: Vec<&String> = Vec::new();
    let mut skip = false;
    for argument in &arguments {
        if skip {
            skip = false;
            continue;
        }
        if argument.starts_with("--") {
            skip = takes_a_value.contains(&argument.as_str());
            continue;
        }
        positional.push(argument);
    }
    // The SQLite-file source, named by its own flag rather than guessed at from
    // the shape of the path: a directory and a file are both just paths, and a
    // tool that decided which migration to run by looking at the source would
    // pick wrongly exactly once, on somebody's real data.
    if let Some(source) = flag(&arguments, "--sqlite-file") {
        let Some(destination) = positional.first() else {
            eprintln!("usage: inillucent-migrate --sqlite-file <source.db> <destination.rdb>");
            return ExitCode::FAILURE;
        };
        return migrate_sqlite_file(&source, &PathBuf::from(destination.as_str()));
    }
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

/// Migrates one SQLite database file into the new engine.
///
/// Every check is printed whether it passed or not, because a migration that is
/// wrong is worth describing completely: knowing that the counts are right and
/// one table's digest is not is a different problem from knowing that nothing
/// arrived.
///
/// @param source - the SQLite file to read, never written
/// @param destination - where the verified database is published
fn migrate_sqlite_file(source: &PathBuf, destination: &PathBuf) -> ExitCode {
    match sqlite::migrate(source, destination) {
        Ok(report) => {
            println!(
                "{} tables, {} rows",
                report.inventory.tables.len(),
                report
                    .inventory
                    .tables
                    .iter()
                    .map(|table| table.rows)
                    .sum::<u64>()
            );
            for check in &report.checks {
                println!(
                    "  {} {} {}",
                    if check.passed { "pass" } else { "FAIL" },
                    check.name,
                    check.detail
                );
            }
            if report.passed() {
                println!("published: {}", report.destination.display());
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "verification failed; nothing was published. the staging file is at {}",
                    report.staged.display()
                );
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("{}", error.detail().unwrap_or(error.message()));
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--flag value` argument.
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}
