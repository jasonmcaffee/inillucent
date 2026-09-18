//! inillucent's command surface: the shell, the command table, and what reads them.
//!
//! Invariant: **there is one command table and every front end reads it.**
//! A verb-shaped `inillucent` binary and an MCP server were added beside
//! the `sqlite3`-shaped shell that was already here, and the reason all three
//! live in one crate is that the alternative - a second list of commands, kept
//! in step by whoever remembers - is the failure mode `drivers/README.md`
//! describes for `java.sql.DatabaseMetaData`: a list nobody runs decays into a
//! list of claims that were true once.
//!
//! So [`command::COMMANDS`] is the table, [`command::run`] executes an entry,
//! and the three binaries are adapters over it:
//!
//! | binary | what it is |
//! |---|---|
//! | `inillucent-shell` | the `sqlite3`-shaped REPL. Unchanged by this ticket. |
//! | `inillucent` | the verbs, for a script and for a person who is not in a REPL. |
//! | `inillucent-mcp` | the same verbs as MCP tools, for an agent. |
//!
//! The layer below all of them is [`shell::Shell`], which is itself an adapter:
//! every statement it runs goes through the public `inillucent` facade and it
//! never reaches past it. The command table does not reach past the shell for
//! the same reason - a third path to the same data is a third set of answers,
//! and the difference is only ever found by somebody who trusted one of them.

// **`deny` rather than `forbid`, for one file (task-1932, H11).**
// `interrupt.rs` installs a console control handler so that Ctrl+C stops a
// statement rather than the process, and there is no way to be told about
// Ctrl+C in the standard library: both platforms offer one FFI call. Every
// other file in this crate is still refused the word, `interrupt.rs` is
// named in `policy.rs`'s `UNSAFE_ALLOWED`, and both of its `unsafe` blocks
// carry their own SAFETY note.
#![deny(unsafe_code)]
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

/// How much stack a statement is given, in bytes.
///
/// **A statement the parser accepts must not overflow any later stage
/// (task-1979, section 5.3).** `SELECT abs(abs(...(1)...))` 300 deep ended the
/// process with `thread 'main' has overflowed its stack` and exit code
/// 0xC00000FD, in debug and in release, on the command line and on the MCP
/// server - where it ends the server for every client. The declared limits are
/// SQLite's own, `ExprDepth` 1000 and `ParserDepth` 2500, and the binaries
/// carry a 1 MiB stack reserve read from the PE header, so the limit could
/// never be the thing that fired.
///
/// **Measured on this build rather than guessed.** Nesting `abs()` on the
/// default 1 MiB main thread overflows between 34 and 38 levels in a debug
/// build, so one level of parse, bind, plan and execute costs about 29 KiB
/// there - the deepest and most expensive of the two builds, which is the one
/// to size against. `ExprDepth` at 1000 therefore needs about 29 MiB, and 64
/// MiB is that with a margin of more than two.
///
/// **Reserved, not spent.** A thread's stack is reserved address space that
/// the operating system commits a page at a time as it is used, on Windows and
/// on Linux alike, so a program that never writes a deep expression pays for
/// none of this. Raising the *limits* instead would move the crash rather than
/// remove it, which is why the stack is sized to the limit and not the other
/// way round.
pub const STATEMENT_STACK: usize = 64 << 20;

/// Runs a program's whole body on a thread with [`STATEMENT_STACK`] bytes of
/// stack, and returns what it produced.
///
/// **The whole body rather than one request**, because the engine is `!Send`:
/// a `Database` holds `Rc`s and cannot be moved to a thread once it exists, so
/// the thread is taken first and the database is opened on it. What that buys
/// is the same thing per-request threads would: the depth limits are against a
/// stack this crate chose rather than against whatever the linker's default
/// reserve happened to be.
///
/// **A plain function rather than a closure**, so that a machine which cannot
/// start a thread can still run the body where it is - `Builder::spawn`
/// consumes a closure whether or not it succeeds, and a function pointer is
/// `Copy`. The three programs that call this each hand it their own `run`.
///
/// @param body - what to run
pub fn on_a_sized_stack<R: Send + 'static>(body: fn() -> R) -> R {
    match std::thread::Builder::new()
        .stack_size(STATEMENT_STACK)
        .spawn(body)
    {
        Ok(running) => match running.join() {
            Ok(produced) => produced,
            // A panic on the worker is the panic the caller would have had, so
            // it is re-raised here rather than turned into a value nobody
            // expects.
            Err(panicked) => std::panic::resume_unwind(panicked),
        },
        // A machine that cannot start a thread runs the body where it is. The
        // limits are then against the default reserve, which is what they were
        // against before this existed.
        Err(_) => body(),
    }
}

pub mod archive;
pub mod command;
pub mod commands;
pub mod dbconfig;
pub mod diagnose;
pub mod dot;
pub mod dump;
pub mod help;
pub mod import;
// The one module allowed the word, and only for the two calls that install a
// console control handler. See its own header.
#[allow(unsafe_code)]
pub mod interrupt;
pub mod json;
pub mod mcp;
pub mod render;
pub mod setup;
pub mod shell;
