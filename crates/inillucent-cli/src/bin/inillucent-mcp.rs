//! `inillucent-mcp`: the command table served to an agent over stdio.
//!
//! Invariant: **this binary is a command line and nothing else.** Every
//! decision the server makes - what the tools are, what their schemas say,
//! what a refusal looks like - lives in `inillucent_cli::mcp`, so that the
//! `inillucent mcp` subcommand and this program cannot answer a client
//! differently. Two entry points to one server is a convenience; two servers
//! would be a defect nobody would find until a client used the other one.
//!
//! The same thing `inillucent mcp` does, as its own binary. It has one because
//! an MCP client's configuration names a program and arguments, and a
//! `"command": ["inillucent", "mcp"]` entry is one more place for a
//! configuration to be subtly wrong than `"command": ["inillucent-mcp"]` is.
//! Every decision the server makes lives in `inillucent_cli::mcp`; this file is
//! the command line and nothing else.
//!
//! ```text
//! inillucent-mcp --db app.rdb                  # read and write app.rdb
//! inillucent-mcp --db app.rdb --readonly       # refuse every write
//! inillucent-mcp --root C:\data --db app.rdb   # refuse every path outside C:\data
//! ```
//!
//! With no `--db` it serves `$INILLUCENT_DB`, and with neither it serves an
//! in-memory database - which is a real database that is discarded when the
//! process ends, and is the right default for an agent that was pointed at this
//! server to try something out.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

use std::path::PathBuf;
use std::process::ExitCode;

use inillucent_cli::mcp::{self, Settings};

/// The engine's own allocator, installed for this program.
///
/// The same reasoning as the shell's, and the same one every binary here uses.
#[global_allocator]
static ALLOCATOR: inillucent_alloc::Pooled = inillucent_alloc::Pooled;

/// Reads the command line and serves until standard input ends.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments
        .iter()
        .any(|word| word == "--help" || word == "-h")
    {
        usage();
        return ExitCode::SUCCESS;
    }
    if arguments
        .iter()
        .any(|word| word == "--version" || word == "-V")
    {
        println!("inillucent-mcp {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let settings = match parse(&arguments) {
        Ok(settings) => settings,
        Err(message) => {
            eprintln!("inillucent-mcp: {message}");
            return ExitCode::from(2);
        }
    };
    // **Standard output belongs to the protocol.** Every diagnostic this
    // program has goes to standard error, because a stray line on standard
    // output is a JSON-RPC frame the client cannot parse, and the failure it
    // reports is "the server is broken" rather than whatever was printed.
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

/// Reads the settings off the command line.
///
/// @param arguments - the command line, program name removed
fn parse(arguments: &[String]) -> Result<Settings, String> {
    let mut settings = Settings {
        database: std::env::var("INILLUCENT_DB").unwrap_or_else(|_| ":memory:".to_string()),
        ..Settings::default()
    };
    let mut walk = arguments.iter();
    while let Some(argument) = walk.next() {
        match argument.as_str() {
            "--db" | "-d" => {
                settings.database = walk
                    .next()
                    .cloned()
                    .ok_or_else(|| "--db needs a path.".to_string())?;
            }
            "--readonly" => settings.readonly = true,
            "--root" => {
                let named = walk
                    .next()
                    .cloned()
                    .ok_or_else(|| "--root needs a directory.".to_string())?;
                settings.root = Some(PathBuf::from(named));
            }
            "--limit" => {
                let value = walk
                    .next()
                    .cloned()
                    .ok_or_else(|| "--limit needs a number.".to_string())?;
                settings.limit = value
                    .parse()
                    .map_err(|_| format!("--limit wants a number, not '{value}'."))?;
            }
            other => return Err(format!("'{other}' is not an option this server takes.")),
        }
    }
    Ok(settings)
}

/// Prints what the command line accepts.
fn usage() {
    println!("inillucent-mcp - serve inillucent's commands to an agent over MCP (stdio)");
    println!();
    println!("Usage: inillucent-mcp [options]");
    println!();
    for line in [
        "  -d, --db PATH   the database to serve (or $INILLUCENT_DB; :memory: by default)",
        "      --readonly  refuse every statement that would change something",
        "      --root DIR  refuse every path that resolves outside DIR (links followed)",
        "      --limit N   how many rows a call gets back when it does not say (default 200)",
        "  -V, --version   print the version and stop",
        "  -h, --help      print this",
    ] {
        println!("{line}");
    }
    println!();
    println!("It speaks JSON-RPC 2.0 over standard input and output, one object per line.");
    println!("Configure it in an MCP client as:");
    println!("  {{\"command\": [\"inillucent-mcp\", \"--db\", \"app.rdb\"]}}");
}
