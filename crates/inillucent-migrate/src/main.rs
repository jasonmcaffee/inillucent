//! The migration tool.
//!
//! Invariant: this program reads the source and writes somewhere else. There is
//! no flag that makes it write to the source, and no flag that makes it delete
//! anything. What it can be told is where to put the result, whether to publish
//! it or stop at a verified staging file, and which SQLite to prove the file
//! with.
//!
//! Three sources, named by which one is given:
//!
//! ```text
//! inillucent-migrate <source-index-dir> <destination.db> [--no-publish]
//! inillucent-migrate --sqlite-file <source.db> <destination.rdb>
//! inillucent-migrate --from <postgres://…|mysql://…> <destination.rdb> [--batch N]
//! ```
//!
//! The second migrates a SQLite database file into the new engine's
//! trees, verified by counts and digests and published by a rename. It takes
//! no `--no-publish`, because it never publishes anything it has not verified.
//! When it builds a staging file and then does not publish it, the staging file
//! is left where it fell and its path is printed.
//!
//! **"Always" was too strong and is now stated where the test can reach it
//! (task-1969, 7.5).** The staging file exists between the build and the
//! rename, so a run refused before the build - a destination that is already
//! there, a source that is not a database, a source that cannot be opened -
//! leaves none, and there is nothing for a reader to go looking for.
//! `tests/cli.rs` asserts both halves across a process boundary: the refusals
//! write nothing at the destination and leave no staging file, and a run that
//! publishes says so and leaves a file that opens.
//!
//! The third migrates from a **running** PostgreSQL or MySQL server, read
//! over its own wire protocol inside one repeatable-read snapshot. It holds the
//! same invariants for the same reasons, and it is also reachable as
//! `inillucent migrate --kind postgres` from the command line and from MCP -
//! that path is the one to prefer, and this one exists because a migration is
//! the sort of thing somebody runs from a script against a server that is not
//! their own.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

use std::path::{Path, PathBuf};
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
    let takes_a_value = ["--sqlite-file", "--from", "--batch"];
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
    // A running server, named by `--from` and read over its own wire protocol.
    if let Some(source) = text_flag(&arguments, "--from") {
        let Some(destination) = positional.first() else {
            eprintln!(
                "usage: inillucent-migrate --from <postgres://…|mysql://…> <destination.rdb> \
                 [--batch N]"
            );
            return ExitCode::FAILURE;
        };
        let batch = text_flag(&arguments, "--batch").and_then(|text| text.parse::<u64>().ok());
        return migrate_server(&source, &PathBuf::from(destination.as_str()), batch);
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
"
        );
        return ExitCode::FAILURE;
    };
    let mut plan = Plan::new(source, destination);
    plan.publish = !arguments.iter().any(|argument| argument == "--no-publish");
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
fn migrate_sqlite_file(source: &Path, destination: &Path) -> ExitCode {
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

/// Migrates one running PostgreSQL or MySQL server into the new engine.
///
/// Every check is printed whether it passed or not, for the same reason the
/// SQLite path prints them all: knowing that the counts are right and one
/// table's digest is not is a different problem from knowing that nothing
/// arrived. **The URL printed is the redacted one** - the source line of a
/// migration ends up in a terminal scrollback and in a bug report.
///
/// @param source - the connection URL
/// @param destination - where the verified database is published
/// @param batch - rows per destination transaction, when the caller chose one
fn migrate_server(source: &str, destination: &PathBuf, batch: Option<u64>) -> ExitCode {
    let url = match inillucent_remote::ConnectionUrl::parse(source) {
        Ok(url) => url,
        Err(error) => {
            eprintln!("{}", error.detail().unwrap_or_else(|| error.message()));
            return ExitCode::FAILURE;
        }
    };
    let mut plan = inillucent_remote::Plan::new(url, destination);
    if let Some(batch) = batch {
        plan.batch = batch.max(1);
    }
    match inillucent_remote::migrate::migrate(&plan) {
        Ok(report) => {
            println!(
                "{} -> {}\n{}, {} tables, {} rows",
                report.source,
                destination.display(),
                report.server,
                report.tables.len(),
                report.rows()
            );
            for check in &report.checks {
                println!("  {}", check.line());
            }
            for (kind, name) in &report.not_carried {
                println!("  not carried: {kind} {name}");
            }
            if report.passed() {
                println!("published: {}", destination.display());
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
            eprintln!("{}", error.detail().unwrap_or_else(|| error.message()));
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--flag value` argument.
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    text_flag(arguments, name).map(PathBuf::from)
}

/// Returns the value of a `--flag value` argument as it was written.
///
/// A connection URL is not a path, so reading one through `PathBuf` and back
/// would put it through the platform's own separator rules on the way.
///
/// @param arguments - the command line
/// @param name - the flag
fn text_flag(arguments: &[String], name: &str) -> Option<String> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).cloned()
}
