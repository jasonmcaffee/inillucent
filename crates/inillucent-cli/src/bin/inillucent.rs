//! `inillucent`: the verb-shaped command line.
//!
//! Invariant: **this binary knows no commands.** It parses a command line,
//! looks the verb up in `inillucent_cli::command::COMMANDS`, coerces the
//! arguments to the kinds that command declared, and prints what came back. A
//! verb added to the table works here without this file changing, which is the
//! property the MCP server has for the same reason.
//!
//! ```text
//! inillucent query "SELECT * FROM people" --db app.rdb
//! inillucent describe people --db app.rdb --json
//! inillucent import data.csv --table people --db app.rdb
//! inillucent shell app.rdb              # the sqlite3-shaped REPL
//! inillucent mcp --db app.rdb           # serve the same commands to an agent
//! inillucent app.rdb "SELECT 1"         # a bare file is the shell, as sqlite3 is
//! ```
//!
//! Exit codes are part of the interface: `0` success, `1` failure, `2` a
//! command line nobody could act on, and `3` a construct the engine has not
//! built - so a script can branch on "not yet" without matching on a message.

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

use std::path::PathBuf;
use std::process::ExitCode;

use inillucent_cli::command::{self, Arguments, Command, Context, Failed, Kind};
use inillucent_cli::json::{self, Json};
use inillucent_cli::mcp;

/// The engine's own allocator, installed for this program.
///
/// The same reasoning as the shell's: a Rust workspace measured on the platform
/// allocator is being measured on a build configuration rather than on an
/// engine, and every binary this repository ships is built the same way.
#[global_allocator]
static ALLOCATOR: inillucent_alloc::Pooled = inillucent_alloc::Pooled;

/// What the command line asked for, once the shared options are out of it.
struct Invocation {
    /// The verb, if one was named.
    verb: Option<String>,
    /// Everything after it.
    rest: Vec<String>,
    /// The database to open.
    database: String,
    /// Whether the command line explicitly named a database.
    database_was_named: bool,
    /// Whether the result is printed as JSON.
    json: bool,
    /// Whether writes are refused.
    readonly: bool,
    /// The directory paths are confined to.
    root: Option<PathBuf>,
    /// How many rows a command hands back by default.
    limit: usize,
    /// What to print where a value is null.
    null: String,
}

/// Runs whatever the command line named.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let Some(topic) = help_topic(&arguments) {
        return dispatch_help(topic);
    }
    match arguments.first().map(String::as_str) {
        None | Some("--help") | Some("-h") | Some("help") if arguments.len() <= 1 => {
            print_overview();
            return ExitCode::SUCCESS;
        }
        Some("--version") | Some("-V") => {
            println!("inillucent {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let invocation = match split(&arguments) {
        Ok(invocation) => invocation,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let Some(verb) = invocation.verb.clone() else {
        print_overview();
        return ExitCode::from(2);
    };
    // A bare file name is the shell, the way `sqlite3 app.db` is. It is the one
    // piece of guessing this parser does, and `names_a_database` is the guard on
    // it: a word that could not be a file name is a mistyped command, and
    // handing one to the shell creates a database named after the typo.
    let Some(command) = command::find(&verb) else {
        if names_a_database(&verb) {
            return shell_like(&arguments);
        }
        return unknown_verb(&verb);
    };
    match command.name {
        "shell" => shell_like(&invocation.rest),
        "mcp" => serve(&invocation),
        "create" if invocation.database_was_named => {
            eprintln!("create takes its database path as its argument and does not accept --db.");
            ExitCode::from(2)
        }
        _ => dispatch(command, &invocation),
    }
}

/// Returns whether a word the command table does not know could name a database.
///
/// **This is the guard on the `sqlite3`-shaped fallback, and the fallback is
/// why it has to exist.** `inillucent <file> [SQL...]` runs the shell, so a
/// first word that is not a command used to be handed straight to it - and the
/// shell opens a database that is not there by creating it. A mistyped command
/// therefore exited 0, printed nothing, and left a 128 KiB file and a log
/// segment named after the typo in whatever directory the caller was standing
/// in.
///
/// A word is read as a database when it is the in-memory spelling, a `file:`
/// URI, something that is already there, or something written the way a path is
/// written: a separator inside it, a drive letter in front of it, or an
/// extension on the end. A mistyped command has none of those. A caller who
/// does want a new file with no extension in the current directory writes
/// `./name`, which has a separator, and the refusal says so.
///
/// @param word - the first word of the command line
fn names_a_database(word: &str) -> bool {
    if word == ":memory:" || word.starts_with("file:") {
        return true;
    }
    if std::path::Path::new(word).exists() {
        return true;
    }
    if word.contains('/') || word.contains('\\') {
        return true;
    }
    // `C:app.rdb` is drive-relative: it names a file on Windows while carrying
    // no separator at all.
    let mut letters = word.chars();
    if letters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && letters.next() == Some(':')
    {
        return true;
    }
    std::path::Path::new(word).extension().is_some()
}

/// Refuses a first word that is neither a command nor a possible file name.
///
/// Exit code 2, "a command line nobody could act on", rather than 1: nothing
/// ran, so there is no statement that could have failed.
///
/// @param word - what was written where a command was expected
fn unknown_verb(word: &str) -> ExitCode {
    eprintln!("inillucent: '{word}' is not a command, and it does not name a database file.");
    let nearest = nearest_commands(word);
    if !nearest.is_empty() {
        eprintln!("  Did you mean: {}?", nearest.join(", "));
    }
    eprintln!(
        "  Run 'inillucent help' for the {} commands there are.",
        command::COMMANDS.len()
    );
    eprintln!(
        "  To open a file of that name as a database, write it as a path: inillucent ./{word}"
    );
    ExitCode::from(2)
}

/// Returns the command names closest to a word somebody mistyped.
///
/// Up to three, nearest first: a command the word is the start of, or one
/// within two single-character edits of it. Past two edits a suggestion stops
/// being a suggestion and becomes the command list, which the next line of the
/// refusal points at anyway.
///
/// @param word - what was written
fn nearest_commands(word: &str) -> Vec<&'static str> {
    let lowered = word.to_ascii_lowercase();
    let mut scored: Vec<(usize, &'static str)> = command::COMMANDS
        .iter()
        .filter_map(|candidate| {
            if !lowered.is_empty() && candidate.name.starts_with(&lowered) {
                return Some((0, candidate.name));
            }
            let gap = distance(&lowered, candidate.name);
            (gap <= 2).then_some((gap, candidate.name))
        })
        .collect();
    scored.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(right.1)));
    scored.truncate(3);
    scored.into_iter().map(|(_, name)| name).collect()
}

/// Returns how many single-character edits separate two words.
///
/// The ordinary two-row edit distance, written over `get` rather than indexes
/// because this binary denies `clippy::indexing_slicing`.
///
/// @param from - the word somebody wrote
/// @param to - the command it is being compared against
fn distance(from: &str, to: &str) -> usize {
    let target: Vec<char> = to.chars().collect();
    let mut previous: Vec<usize> = (0..=target.len()).collect();
    for (row, wrote) in from.chars().enumerate() {
        let mut current: Vec<usize> = Vec::with_capacity(target.len().saturating_add(1));
        current.push(row.saturating_add(1));
        for (column, expected) in target.iter().enumerate() {
            let substitution = previous
                .get(column)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(usize::from(wrote != *expected));
            let deletion = previous
                .get(column.saturating_add(1))
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            let insertion = current
                .get(column)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            current.push(substitution.min(deletion).min(insertion));
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(0)
}

/// Returns the command help topic requested with a shared help flag.
///
/// @param arguments - the complete command line after the program name
fn help_topic(arguments: &[String]) -> Option<&str> {
    let topic = arguments
        .iter()
        .find(|argument| command::find(argument).is_some())?;
    arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
        .then_some(topic)
}

/// Prints the detailed help text for one known command.
///
/// @param topic - the command the caller asked about
fn dispatch_help(topic: &str) -> ExitCode {
    let Some(command) = command::find(topic) else {
        eprintln!("there is no '{topic}' command. Run 'inillucent help' for the list.");
        return ExitCode::from(2);
    };
    let invocation = Invocation {
        verb: Some("help".to_string()),
        rest: vec![command.name.to_string()],
        database: ":memory:".to_string(),
        database_was_named: false,
        json: false,
        readonly: false,
        root: None,
        limit: 200,
        null: String::new(),
    };
    let Some(help) = command::find("help") else {
        return ExitCode::from(2);
    };
    dispatch(help, &invocation)
}

/// Splits the shared options out of the command line.
///
/// Shared options may appear anywhere, before the verb or after it, because a
/// person writing `inillucent query "..." --db app.rdb` should not have to know
/// that `--db` belongs to the program rather than to the verb.
///
/// @param arguments - the whole command line, program name removed
fn split(arguments: &[String]) -> Result<Invocation, String> {
    let mut invocation = Invocation {
        verb: None,
        rest: Vec::new(),
        database: std::env::var("INILLUCENT_DB").unwrap_or_else(|_| ":memory:".to_string()),
        database_was_named: false,
        json: false,
        readonly: false,
        root: None,
        limit: 200,
        null: String::new(),
    };
    let mut walk = arguments.iter();
    while let Some(argument) = walk.next() {
        match argument.as_str() {
            "--db" | "-d" => {
                invocation.database_was_named = true;
                invocation.database = walk
                    .next()
                    .cloned()
                    .ok_or_else(|| "--db needs a path.".to_string())?;
            }
            "--json" => invocation.json = true,
            "--readonly" => invocation.readonly = true,
            "--root" => {
                let named = walk
                    .next()
                    .cloned()
                    .ok_or_else(|| "--root needs a directory.".to_string())?;
                invocation.root = Some(PathBuf::from(named));
            }
            "--limit" => {
                let value = walk
                    .next()
                    .cloned()
                    .ok_or_else(|| "--limit needs a number.".to_string())?;
                invocation.limit = value
                    .parse()
                    .map_err(|_| format!("--limit wants a number, not '{value}'."))?;
            }
            "--null" => {
                invocation.null = walk
                    .next()
                    .cloned()
                    .ok_or_else(|| "--null needs a string.".to_string())?;
            }
            // `--output`, not `--format`: `export` has a `--format` of its own that
            // names CSV against JSON against Markdown, and one word cannot mean
            // both without one of the two meanings losing quietly.
            "--output" => {
                let value = walk
                    .next()
                    .cloned()
                    .ok_or_else(|| "--output needs 'text' or 'json'.".to_string())?;
                invocation.json = match value.as_str() {
                    "json" => true,
                    "text" => false,
                    other => return Err(format!("--output wants text or json, not '{other}'.")),
                };
            }
            _ if invocation.verb.is_none() && !argument.starts_with('-') => {
                invocation.verb = Some(argument.clone());
            }
            other => invocation.rest.push(other.to_string()),
        }
    }
    Ok(invocation)
}

/// Runs a command from the table and prints what it produced.
///
/// @param command - the verb that was named
/// @param invocation - the rest of the command line
fn dispatch(command: &'static Command, invocation: &Invocation) -> ExitCode {
    let arguments = match collect(command, &invocation.rest) {
        Ok(arguments) => arguments,
        Err(message) => {
            eprintln!("{message}\nUsage: {}", command.usage());
            return ExitCode::from(2);
        }
    };
    let database = if command.name == "create" {
        ":memory:"
    } else {
        &invocation.database
    };
    let mut context = match Context::open(database, invocation.readonly, invocation.root.clone()) {
        Ok(context) => context,
        Err(failure) => return report(&failure, invocation.json, command.name),
    };
    context.limit = invocation.limit;
    context.null = invocation.null.clone();
    // Ctrl+C stops the command rather than the process, so a long `query` or a
    // `migrate` can be given up on without losing what it has already reported
    // (task-1932, H11).
    inillucent_cli::interrupt::stop_on_ctrl_c(context.cancel_flag());
    match command::run(command, &mut context, &arguments) {
        Ok(produced) => {
            let shown = match invocation.json {
                true => produced.to_json().pretty(0),
                false => produced.text.clone(),
            };
            if !shown.is_empty() {
                println!("{shown}");
            }
            ExitCode::SUCCESS
        }
        Err(failure) => report(&failure, invocation.json, command.name),
    }
}

/// Prints a failure in whichever form was asked for, and returns its code.
///
/// @param failure - what went wrong
/// @param as_json - whether the caller asked for JSON
/// @param command - the verb that failed
fn report(failure: &Failed, as_json: bool, command: &str) -> ExitCode {
    match as_json {
        true => println!("{}", failure.to_json(command).pretty(0)),
        false => eprintln!("{}", failure.to_text()),
    }
    ExitCode::from(failure.exit_code() as u8)
}

/// Reads a command's own arguments off the command line.
///
/// @param command - the verb, whose parameters say what to expect
/// @param rest - the words after the shared options were removed
fn collect(command: &'static Command, rest: &[String]) -> Result<Arguments, String> {
    let mut arguments = Arguments::default();
    let mut positional_taken = false;
    let mut walk = rest.iter().peekable();
    while let Some(word) = walk.next() {
        let Some(name) = word.strip_prefix("--") else {
            let Some(param) = command.positional() else {
                return Err(format!(
                    "'{}' takes no unnamed argument, and got '{word}'.",
                    command.name
                ));
            };
            if positional_taken {
                return Err(format!("'{}' takes one unnamed argument.", command.name));
            }
            positional_taken = true;
            arguments.set(param.name, coerce(param.kind, word)?);
            continue;
        };
        let Some(param) = command
            .param(&name.replace('-', "_"))
            .or(command.param(name))
        else {
            return Err(format!("'{}' has no --{name} option.", command.name));
        };
        // A boolean is a flag: `--indent` rather than `--indent true`, unless
        // the next word really is a boolean, which keeps a scripted caller that
        // writes it out from being told it is wrong.
        if param.kind == Kind::Boolean {
            let explicit = walk
                .peek()
                .and_then(|next| parse_boolean(next))
                .inspect(|_value| {
                    walk.next();
                })
                .unwrap_or(true);
            arguments.set(param.name, Json::Bool(explicit));
            continue;
        }
        let Some(value) = walk.next() else {
            return Err(format!("--{name} needs a value."));
        };
        arguments.set(param.name, coerce(param.kind, value)?);
    }
    Ok(arguments)
}

/// Turns a command-line word into the kind its parameter declared.
///
/// @param kind - what the parameter takes
/// @param word - what was written
fn coerce(kind: Kind, word: &str) -> Result<Json, String> {
    match kind {
        Kind::Text => Ok(json::text(word)),
        Kind::Integer => word
            .parse::<i64>()
            .map(Json::Int)
            .map_err(|_| format!("'{word}' is not a whole number.")),
        Kind::Boolean => parse_boolean(word)
            .map(Json::Bool)
            .ok_or_else(|| format!("'{word}' is not true or false.")),
        // An array arrives as JSON, because a shell has no array and every
        // other spelling would need an escaping rule of its own.
        Kind::Values => json::parse(word)
            .map_err(|why| format!("'{word}' is not a JSON array: {why}"))
            .and_then(|value| match value {
                Json::Array(_) => Ok(value),
                _ => Err(format!("'{word}' is not a JSON array.")),
            }),
    }
}

/// Reads the spellings of true and false a command line uses.
///
/// @param word - the candidate
fn parse_boolean(word: &str) -> Option<bool> {
    match word.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Starts the MCP server on standard input and output.
///
/// @param invocation - what the command line asked for
fn serve(invocation: &Invocation) -> ExitCode {
    let settings = mcp::Settings {
        database: invocation.database.clone(),
        readonly: invocation.readonly,
        root: invocation.root.clone(),
        limit: invocation.limit,
        ..mcp::Settings::default()
    };
    let input = std::io::BufReader::new(std::io::stdin());
    let mut output = std::io::stdout();
    match mcp::serve(settings, input, &mut output) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("inillucent-mcp: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Hands the command line to the `sqlite3`-shaped shell.
///
/// **By running it, not by re-implementing it.** `inillucent-shell` is the
/// binary a script that drives `sqlite3` points at, its argument grammar is
/// SQLite's, and a second parser for the same grammar here would be a second
/// set of answers to the same command line.
///
/// @param arguments - what to pass through
fn shell_like(arguments: &[String]) -> ExitCode {
    let mut path = std::env::current_exe().unwrap_or_default();
    path.set_file_name(match cfg!(windows) {
        true => "inillucent-shell.exe",
        false => "inillucent-shell",
    });
    if !path.is_file() {
        eprintln!(
            "inillucent: the interactive shell lives in a separate binary and it is not beside \
             this one.\n  looked for: {}\n  Everything the shell does is also reachable with: \
             inillucent run \"<input>\"",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    match std::process::Command::new(&path).args(arguments).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("inillucent: could not start {}: {error}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// Prints the command list and the shared options.
fn print_overview() {
    println!(
        "inillucent {} - an embedded SQL database with search built in",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("Usage: inillucent <command> [arguments] [options]");
    println!();
    println!("Commands:");
    let width = command::COMMANDS
        .iter()
        .map(|command| command.name.len())
        .max()
        .unwrap_or(0);
    for command in command::COMMANDS {
        let padding = " ".repeat(width.saturating_sub(command.name.len()));
        println!("  {}{padding}  {}", command.name, command.summary);
    }
    println!();
    println!("Options, which may go anywhere on the line:");
    for line in [
        "  -d, --db PATH      the database to open (or $INILLUCENT_DB; :memory: by default)",
        "      --json         print the whole result object instead of a table",
        "      --output WHICH text or json (--json means --output json)",
        "      --readonly     refuse every statement that would change something",
        "      --root DIR     refuse every path that resolves outside DIR (links followed)",
        "      --limit N      how many rows to hand back (default 200; 0 for all)",
        "      --null TEXT    what to print where a value is null",
        "  -V, --version      print the version and stop",
        "  -h, --help         print this",
    ] {
        println!("{line}");
    }
    println!();
    println!("'inillucent help <command>' explains one command and every option it takes.");
    println!(
        "'inillucent <file> [SQL...]' runs the sqlite3-shaped shell, as 'sqlite3 <file>' does."
    );
    println!();
    println!("Exit codes: 0 ok, 1 failed, 2 bad command line, 3 the engine has not built that.");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mistyped command is not taken for the name of a database to create.
    #[test]
    fn a_mistyped_command_does_not_name_a_database() {
        for word in ["bogusverb", "qeury", "descrbe", "quer", ""] {
            assert!(!names_a_database(word), "{word} was read as a file name");
        }
    }

    /// Every spelling a database is actually written in is still read as one.
    #[test]
    fn a_database_is_recognised_by_how_it_is_written() {
        for word in [
            ":memory:",
            "app.rdb",
            "./app",
            "data/app",
            "C:\\tmp\\app",
            "C:app",
            "file:app.rdb?mode=ro",
        ] {
            assert!(names_a_database(word), "{word} was not read as a file name");
        }
    }

    /// A typo is answered with the command it is closest to.
    #[test]
    fn the_nearest_command_is_suggested() {
        assert!(nearest_commands("qeury").contains(&"query"));
        assert!(nearest_commands("descr").contains(&"describe"));
        assert!(nearest_commands("expor").contains(&"export"));
    }

    /// A word close to nothing is answered with no suggestion at all.
    #[test]
    fn a_word_close_to_nothing_suggests_nothing() {
        assert!(nearest_commands("zzzzzzzzzzzz").is_empty());
    }

    /// At most three suggestions, so the refusal stays readable.
    #[test]
    fn there_are_never_more_than_three_suggestions() {
        assert!(nearest_commands("e").len() <= 3);
    }

    /// The edit distance is the ordinary one.
    #[test]
    fn the_distance_counts_single_character_edits() {
        assert_eq!(distance("query", "query"), 0);
        assert_eq!(distance("quer", "query"), 1);
        assert_eq!(distance("qeury", "query"), 2);
        assert_eq!(distance("", "query"), 5);
    }
}
