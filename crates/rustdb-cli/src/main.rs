//! A SQLite-like shell: `rustdb-shell`.
//!
//! Invariant: the shell is an adapter. It parses dot commands and formats
//! output; every statement it runs goes through the public `rustdb` facade, and
//! it never reaches past it. A shell that starts reading the schema directly
//! becomes a second, slightly different database, and the difference is only
//! ever found by somebody who trusted it.
//!
//! The command line is SQLite's, because a script that drives `sqlite3` has to
//! drive this: `rustdb-shell FILE "SELECT ..."` runs one statement and exits,
//! `rustdb-shell FILE` reads standard input, and the option flags before the
//! file name set the same things the dot commands do.

#![forbid(unsafe_code)]
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

mod dot;
mod dump;
mod import;
mod render;
mod shell;

use shell::{drive, Shell};

/// What the command line asked for.
struct Invocation {
    /// The database to open.
    path: String,
    /// Statements given on the command line, run in order.
    statements: Vec<String>,
    /// Settings to apply before anything runs.
    commands: Vec<String>,
    /// Whether to print the version and stop.
    version: bool,
    /// Whether to print usage and stop.
    help: bool,
}

/// Parses the command line the way `sqlite3` does.
///
/// Options come first, then the file, then any statements. An unknown option is
/// not an error: `sqlite3` treats it as the file name, and a script that passes
/// one through is better served by opening a strangely-named file than by an
/// argument parser with opinions.
fn parse(arguments: impl Iterator<Item = String>) -> Invocation {
    let mut invocation = Invocation {
        path: ":memory:".to_string(),
        statements: Vec::new(),
        commands: Vec::new(),
        version: false,
        help: false,
    };
    let mut named = false;
    let mut arguments = arguments.peekable();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-version" | "--version" => invocation.version = true,
            "-help" | "--help" => invocation.help = true,
            "-header" | "--header" => invocation.commands.push(".headers on".to_string()),
            "-noheader" | "--noheader" => invocation.commands.push(".headers off".to_string()),
            "-echo" | "--echo" => invocation.commands.push(".echo on".to_string()),
            "-bail" | "--bail" => invocation.commands.push(".bail on".to_string()),
            "-csv" | "--csv" => invocation.commands.push(".mode csv".to_string()),
            "-json" | "--json" => invocation.commands.push(".mode json".to_string()),
            "-line" | "--line" => invocation.commands.push(".mode line".to_string()),
            "-list" | "--list" => invocation.commands.push(".mode list".to_string()),
            "-html" | "--html" => invocation.commands.push(".mode html".to_string()),
            "-box" | "--box" => invocation.commands.push(".mode box".to_string()),
            "-table" | "--table" => invocation.commands.push(".mode table".to_string()),
            "-markdown" | "--markdown" => invocation.commands.push(".mode markdown".to_string()),
            "-quote" | "--quote" => invocation.commands.push(".mode quote".to_string()),
            "-column" | "--column" => invocation.commands.push(".mode column".to_string()),
            "-separator" | "--separator" => {
                if let Some(value) = arguments.next() {
                    invocation.commands.push(format!(".separator \"{value}\""));
                }
            }
            "-nullvalue" | "--nullvalue" => {
                if let Some(value) = arguments.next() {
                    invocation.commands.push(format!(".nullvalue \"{value}\""));
                }
            }
            "-cmd" | "--cmd" => {
                if let Some(value) = arguments.next() {
                    invocation.commands.push(value);
                }
            }
            _ if !named => {
                invocation.path = argument;
                named = true;
            }
            _ => invocation.statements.push(argument),
        }
    }
    invocation
}

/// Opens the database, applies the settings, and runs whatever was asked for.
fn main() {
    let invocation = parse(std::env::args().skip(1));
    if invocation.help {
        usage();
        return;
    }
    let mut shell = match Shell::open(&invocation.path) {
        Ok(shell) => shell,
        Err(message) => {
            eprintln!(
                "Error: unable to open database \"{}\": {message}",
                invocation.path
            );
            std::process::exit(1);
        }
    };
    if invocation.version {
        dot::run(&mut shell, ".version");
        return;
    }
    for command in &invocation.commands {
        drive(&mut shell, std::iter::once(command.clone()));
    }
    if invocation.statements.is_empty() {
        let lines = std::io::stdin().lines().map_while(Result::ok);
        drive(&mut shell, lines);
    } else {
        let statements = invocation.statements.clone();
        drive(&mut shell, statements.into_iter());
    }
    if shell.failed {
        std::process::exit(1);
    }
}

/// Prints what the command line accepts.
fn usage() {
    println!("Usage: rustdb-shell [OPTIONS] FILENAME [SQL...]");
    println!("Options:");
    for line in [
        "   -bail                stop after hitting an error",
        "   -box                 set output mode to 'box'",
        "   -column              set output mode to 'column'",
        "   -cmd COMMAND         run COMMAND before reading stdin",
        "   -csv                 set output mode to 'csv'",
        "   -echo                print commands before execution",
        "   -header              turn headers on",
        "   -help                show this message",
        "   -html                set output mode to HTML",
        "   -json                set output mode to 'json'",
        "   -line                set output mode to 'line'",
        "   -list                set output mode to 'list'",
        "   -markdown            set output mode to 'markdown'",
        "   -noheader            turn headers off",
        "   -nullvalue TEXT      set text string for NULL values",
        "   -quote               set output mode to 'quote'",
        "   -separator SEP       set output column separator",
        "   -table               set output mode to 'table'",
        "   -version             show the version and exit",
    ] {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first bare word is the file and the rest are statements.
    #[test]
    fn the_first_word_is_the_file() {
        let parsed = parse(
            ["db.sqlite", "SELECT 1", "SELECT 2"]
                .iter()
                .map(|word| (*word).to_string()),
        );
        assert_eq!(parsed.path, "db.sqlite");
        assert_eq!(parsed.statements, vec!["SELECT 1", "SELECT 2"]);
    }

    /// An option before the file sets a dot command.
    #[test]
    fn options_become_dot_commands() {
        let parsed = parse(
            ["-csv", "-header", "db.sqlite"]
                .iter()
                .map(|word| (*word).to_string()),
        );
        assert_eq!(parsed.commands, vec![".mode csv", ".headers on"]);
        assert_eq!(parsed.path, "db.sqlite");
    }

    /// With no file at all, the database is in memory.
    #[test]
    fn no_file_means_memory() {
        let parsed = parse(std::iter::empty());
        assert_eq!(parsed.path, ":memory:");
    }

    /// An option that takes a value consumes the next word.
    #[test]
    fn an_option_can_take_a_value() {
        let parsed = parse(
            ["-separator", ",", "db.sqlite"]
                .iter()
                .map(|word| (*word).to_string()),
        );
        assert_eq!(parsed.commands, vec![".separator \",\""]);
        assert_eq!(parsed.path, "db.sqlite");
    }
}
