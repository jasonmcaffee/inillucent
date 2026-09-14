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
