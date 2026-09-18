//! A SQLite-like shell: `inillucent-shell`.
//!
//! Invariant: the shell is an adapter. It parses dot commands and formats
//! output; every statement it runs goes through the public `inillucent` facade, and
//! it never reaches past it. A shell that starts reading the schema directly
//! becomes a second, slightly different database, and the difference is only
//! ever found by somebody who trusted it.
//!
//! The command line is SQLite's, because a script that drives `sqlite3` has to
//! drive this: `inillucent-shell FILE "SELECT ..."` runs one statement and exits,
//! `inillucent-shell FILE` reads standard input, and the option flags before the
//! file name set the same things the dot commands do.
//!
//! The shell itself lives in the crate's library, because the
//! `inillucent` and `inillucent-mcp` binaries drive the same one. This file is
//! what is left: the `sqlite3`-shaped command line, and nothing else.

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

/// The engine's own allocator, installed for this program.
///
/// **Part of the build, not of a workload.** SQLite ships its own memory
/// subsystem and is compiled as one translation unit; a Rust workspace measured
/// on the platform allocator is being measured on a build configuration rather
/// than on an engine, which is the same reasoning that fixed fat LTO and one
/// codegen unit in the release profile. The Windows C runtime heap was
/// measured at 59% of a trivial compile, and this size-classed free list at
/// 17% overall, which is why Phase 3's Part E names it the cheapest first move.
#[global_allocator]
static ALLOCATOR: inillucent_alloc::Pooled = inillucent_alloc::Pooled;

use inillucent_cli::shell::{drive, Shell};
use inillucent_cli::{commands, dot};

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
    /// Whether every statement that changes something is refused.
    readonly: bool,
    /// Whether the commands that reach outside the database are refused.
    safe: bool,
    /// Whether the file has to exist already.
    if_exists: bool,
    /// Whether a symbolic link is refused rather than followed.
    no_follow: bool,
    /// The options this engine has no equivalent for, with the reason for each.
    refused: Vec<(String, &'static str)>,
    /// The words that are shaped like an option and are not one of them.
    unknown: Vec<String>,
}

/// The options that name a SQLite internal this engine does not have.
///
/// **Refused by name rather than ignored, and that is the decision.** A shell
/// that swallows `-mmap 268435456` and carries on has told its caller the
/// setting took effect. Every one of these names a mechanism the reference
/// shell exposes because SQLite has it - a lookaside allocator, a page cache
/// handed a caller's buffer, a memory-mapped read path, a VFS shim, the
/// serialize/deserialize interface, the appendvfs, the ZIP reader. This engine
/// has none of them, and the driver already carries a whole status for exactly
/// this distinction - `unsupported`, which `drivers/README.md` argues at length
/// must not be folded into "you mistyped something". Saying so out loud is the
/// same answer in a different medium.
///
/// The second field says whether the option consumes the word after it, so a
/// value is not then read as the file name.
const REFUSED: &[(&str, bool, &str)] = &[
    (
        "-append",
        false,
        "this engine does not write into the end of another file; there is no appendvfs.",
    ),
    (
        "-deserialize",
        false,
        "there is no sqlite3_deserialize: a database is a file, or it is :memory:.",
    ),
    (
        "-maxsize",
        true,
        "it sizes a deserialized database, and there are none.",
    ),
    ("-zip", false, "there is no ZIP virtual file system."),
    (
        "-vfs",
        true,
        "the VFS is not selectable at run time. .vfslist names the one there is.",
    ),
    (
        "-vfstrace",
        false,
        "there is no VFS shim layer to trace through.",
    ),
    (
        "-lookaside",
        true,
        "there is no lookaside allocator to budget. The allocator is inillucent-alloc, a \
         size-classed free list, and it takes no per-connection reservation.",
    ),
    (
        "-pagecache",
        true,
        "the buffer pool is not handed a caller's buffer. PRAGMA cache_size sizes it.",
    ),
    (
        "-pcachetrace",
        false,
        "there is no pluggable page cache to trace.",
    ),
    ("-memtrace", false, "there is no memory tracer to install."),
    (
        "-mmap",
        true,
        "the pool reads through the VFS. There is no memory-mapped read path to size.",
    ),
    (
        "-unsafe-testing",
        false,
        "there is no testing back door to open.",
    ),
    (
        "-no-rowid-in-view",
        false,
        "this engine has no legacy rowid-in-view behaviour to switch off.",
    ),
    (
        "-escape",
        true,
        "the output escaping modes are not implemented: .mode has no --escape.",
    ),
    (
        "-screenwidth",
        true,
        "the columnar modes do not wrap to a screen width: .mode has no --sw.",
    ),
    (
        "-interactive",
        false,
        "this shell never prompts, so there is nothing to force on. It is always -batch.",
    ),
    // These two are refused for one reason, and it is a real limitation rather
    // than a missing switch. The output path writes a line and then a newline;
    // the row separator is honoured only where it *ends* in one, which is why
    // `-csv` works and is byte-identical to the reference. `-ascii` wants a
    // record separator with no newline after it at all, and `-newline` wants
    // an arbitrary one. Accepting either would set the column separator, drop
    // the row separator, and produce output that differs from `sqlite3` in a
    // way a caller would find in their own parser rather than here.
    (
        "-ascii",
        false,
        "the row separator is honoured only when it ends in a newline, and the ASCII record \
         separator does not. -separator sets the column separator on its own.",
    ),
    (
        "-newline",
        true,
        "the row separator is honoured only when it ends in a newline. -separator sets the \
         column separator on its own.",
    ),
];

/// Parses the command line the way `sqlite3` does.
///
/// Options come first, then the file, then any statements. An unrecognised word
/// that begins with a dash is an error, which is what `sqlite3` itself does:
/// version 3.53.4 answers `sqlite3 --db app.db "SELECT 1"` with `Error: unknown
/// option: -db`, opens nothing and exits non-zero. This shell used to take such
/// a word as the file name instead, so a mistyped option created a database
/// called `--db` in whatever directory the caller was standing in, and files
/// named `--`, `-d` and `--db` turned up in a repository from exactly that.
/// `--` still ends the options, so a file whose name begins with a dash can be
/// opened by writing it after one.
fn parse(arguments: impl Iterator<Item = String>) -> Invocation {
    let mut invocation = Invocation {
        path: ":memory:".to_string(),
        statements: Vec::new(),
        commands: Vec::new(),
        version: false,
        help: false,
        readonly: false,
        safe: false,
        if_exists: false,
        no_follow: false,
        refused: Vec::new(),
        unknown: Vec::new(),
    };
    let mut named = false;
    let mut only_positional = false;
    let mut arguments = arguments.peekable();
    while let Some(argument) = arguments.next() {
        if only_positional {
            take_positional(&mut invocation, argument, &mut named);
            continue;
        }
        if let Some((option, takes_value, why)) = REFUSED
            .iter()
            .find(|(option, _, _)| is_option(&argument, option))
        {
            if *takes_value {
                let _ = arguments.next();
            }
            invocation.refused.push(((*option).to_string(), *why));
            continue;
        }
        match argument.as_str() {
            "--" => only_positional = true,
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
            "-tabs" | "--tabs" => invocation.commands.push(".mode tabs".to_string()),
            "-stats" | "--stats" => invocation.commands.push(".stats on".to_string()),
            "-batch" | "--batch" | "-noinit" | "--noinit" => {}
            "-readonly" | "--readonly" => invocation.readonly = true,
            "-safe" | "--safe" => invocation.safe = true,
            "-ifexists" | "--ifexists" => invocation.if_exists = true,
            "-nofollow" | "--nofollow" => invocation.no_follow = true,
            "-init" | "--init" => {
                if let Some(value) = arguments.next() {
                    invocation.commands.push(format!(".read \"{value}\""));
                }
            }
            "-nonce" | "--nonce" => {
                if let Some(value) = arguments.next() {
                    invocation.commands.push(format!(".nonce {value}"));
                }
            }
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
            // Reaching here means no option matched, so a word still carrying
            // a leading dash is a mistyped option rather than a file name.
            // Everything after `--` skips this arm entirely, which is how a
            // file whose name begins with a dash is still opened.
            _ if argument.starts_with('-') => invocation.unknown.push(argument),
            _ => take_positional(&mut invocation, argument, &mut named),
        }
    }
    invocation
}

/// Records a word that is not an option: the first is the file, the rest SQL.
///
/// @param invocation - what is being built
/// @param argument - the word
/// @param named - whether the file has already been named
fn take_positional(invocation: &mut Invocation, argument: String, named: &mut bool) {
    if *named {
        invocation.statements.push(argument);
    } else {
        invocation.path = argument;
        *named = true;
    }
}

/// Returns whether a command-line word is a given option, in either spelling.
///
/// @param argument - the word
/// @param option - the option, with one leading dash
fn is_option(argument: &str, option: &str) -> bool {
    argument == option
        || argument
            .strip_prefix("--")
            .is_some_and(|rest| Some(rest) == option.strip_prefix('-'))
}

/// Refuses to open a file that does not satisfy `-ifexists` or `-nofollow`.
///
/// @param invocation - what the command line asked for
fn openable(invocation: &Invocation) -> Result<(), String> {
    if invocation.path == ":memory:" {
        return Ok(());
    }
    let path = std::path::Path::new(&invocation.path);
    if invocation.if_exists && !path.exists() {
        return Err(format!(
            "Error: cannot open \"{}\": it does not exist, and -ifexists was given",
            invocation.path
        ));
    }
    // Asked of the link itself rather than of what it points at, which is the
    // whole of the check: `exists()` follows a link, so a check written that way
    // would answer about the target every time and never refuse anything.
    if invocation.no_follow {
        if let Ok(about) = std::fs::symlink_metadata(path) {
            if about.file_type().is_symlink() {
                return Err(format!(
                    "Error: cannot open \"{}\": it is a symbolic link, and -nofollow was given",
                    invocation.path
                ));
            }
        }
    }
    Ok(())
}

/// Opens the database, applies the settings, and runs whatever was asked for.
fn main() {
    // See `inillucent_cli::STATEMENT_STACK`.
    inillucent_cli::on_a_sized_stack(run)
}

/// Everything `main` does, on the sized thread.
fn run() {
    let invocation = parse(std::env::args().skip(1));
    if invocation.help {
        usage();
        return;
    }
    // Before anything is opened, and before the refusals below: a word that is
    // shaped like an option and is not one means the rest of the line was
    // probably misread too. The file name is the word after it, so carrying on
    // would create a database under the typo's name and run the real file name
    // as a statement.
    if !invocation.unknown.is_empty() {
        for option in &invocation.unknown {
            eprintln!("Error: unknown option: {option}");
        }
        eprintln!("Use -help for a list of options.");
        std::process::exit(2);
    }
    // Before anything is opened: an option nobody can honour means the command
    // line was misunderstood, and running most of it would be worse than
    // running none of it.
    if !invocation.refused.is_empty() {
        for (option, why) in &invocation.refused {
            eprintln!("Error: {option} is not supported by this engine.");
            eprintln!("  {why}");
        }
        std::process::exit(1);
    }
    if let Err(message) = openable(&invocation) {
        eprintln!("{message}");
        std::process::exit(1);
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
    // Ctrl+C ends the statement, not the shell (task-1932, H11). A second
    // press still ends the process: the operating system's default handler is
    // back once ours has fired.
    inillucent_cli::interrupt::stop_on_ctrl_c(shell.cancel_flag());
    if invocation.version {
        dot::run(&mut shell, ".version");
        return;
    }
    // The settings run before safe mode is armed, because `-init` names a
    // script the caller chose and `-safe` is about what that script may then
    // do. Arming first would refuse the caller's own `.read`.
    for command in &invocation.commands {
        drive(&mut shell, std::iter::once(command.clone()));
    }
    shell.readonly = invocation.readonly;
    shell.safe = invocation.safe;
    if invocation.statements.is_empty() {
        let lines = std::io::stdin().lines().map_while(Result::ok);
        drive(&mut shell, lines);
    } else {
        let statements = invocation.statements.clone();
        drive(&mut shell, statements.into_iter());
    }
    // `.testcase`/`.check` report their tally once, at the end, and only when
    // some ran - which is what makes the line invisible to an ordinary script.
    commands::report_tests(&mut shell);
    if shell.failed {
        std::process::exit(1);
    }
}

/// Prints what the command line accepts.
///
/// Every option the reference shell has appears here, including the sixteen
/// this engine refuses: an option missing from the usage text reads as an
/// option that was forgotten, and a caller who reads "not supported, and here
/// is why" has learned something a silent omission would not have told them.
fn usage() {
    println!("Usage: inillucent-shell [OPTIONS] FILENAME [SQL...]");
    println!("Options:");
    for line in [
        "   --                   treat every later argument as a file or a statement",
        "   -bail                stop after hitting an error",
        "   -batch               force batch input (this shell is always batch)",
        "   -box                 set output mode to 'box'",
        "   -column              set output mode to 'column'",
        "   -cmd COMMAND         run COMMAND before reading stdin",
        "   -csv                 set output mode to 'csv'",
        "   -echo                print commands before execution",
        "   -header              turn headers on",
        "   -help                show this message",
        "   -html                set output mode to HTML",
        "   -ifexists            only open FILENAME if it already exists",
        "   -init FILENAME       read and run FILENAME before anything else",
        "   -json                set output mode to 'json'",
        "   -line                set output mode to 'line'",
        "   -list                set output mode to 'list'",
        "   -markdown            set output mode to 'markdown'",
        "   -nofollow            refuse to open FILENAME if it is a symbolic link",
        "   -noheader            turn headers off",
        "   -noinit              do not read a start-up file (none is read anyway)",
        "   -nonce STRING        suspend safe mode for a command that matches STRING",
        "   -nullvalue TEXT      set text string for NULL values",
        "   -quote               set output mode to 'quote'",
        "   -readonly            refuse every statement that changes the database",
        "   -safe                refuse .cd, .load, .shell, .system, .excel and .www",
        "   -separator SEP       set output column separator",
        "   -stats               print memory and page-cache statistics",
        "   -table               set output mode to 'table'",
        "   -tabs                set output mode to 'tabs'",
        "   -version             show the version and exit",
    ] {
        println!("{line}");
    }
    println!();
    println!("Refused, because this engine has no equivalent - each says why when used:");
    let width = REFUSED
        .iter()
        .map(|(option, _, _)| option.len())
        .max()
        .unwrap_or(0);
    for (option, takes_value, _) in REFUSED {
        let value = if *takes_value { " N" } else { "" };
        let padding = " ".repeat(width.saturating_sub(option.len()));
        println!("   {option}{padding}{value}");
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

    /// A word shaped like an option and matching none of them is not the file.
    #[test]
    fn an_unknown_option_is_not_the_file_name() {
        let parsed = parse(
            ["--db", "app.rdb", "SELECT 1"]
                .iter()
                .map(|word| (*word).to_string()),
        );
        assert_eq!(parsed.unknown, vec!["--db"]);
        assert_eq!(parsed.path, "app.rdb");
        assert_eq!(parsed.statements, vec!["SELECT 1"]);
    }

    /// The short spellings that turned up on disk are caught the same way.
    #[test]
    fn every_dashed_word_that_is_not_an_option_is_reported() {
        let parsed = parse(
            ["-d", "app.rdb", "--nonsense"]
                .iter()
                .map(|word| (*word).to_string()),
        );
        assert_eq!(parsed.unknown, vec!["-d", "--nonsense"]);
    }

    /// After `--`, a name that begins with a dash is a file again.
    #[test]
    fn the_separator_still_opens_a_dashed_file() {
        let parsed = parse(["--", "-weird.rdb"].iter().map(|word| (*word).to_string()));
        assert!(parsed.unknown.is_empty());
        assert_eq!(parsed.path, "-weird.rdb");
    }

    /// A refused option is still a refusal rather than an unknown word.
    #[test]
    fn a_refused_option_is_not_reported_as_unknown() {
        let parsed = parse(
            ["-mmap", "268435456", "app.rdb"]
                .iter()
                .map(|word| (*word).to_string()),
        );
        assert!(parsed.unknown.is_empty());
        assert_eq!(parsed.refused.len(), 1);
        assert_eq!(parsed.path, "app.rdb");
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
