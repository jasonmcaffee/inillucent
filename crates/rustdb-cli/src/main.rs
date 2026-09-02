//! SQLite-like shell used for compatibility testing and manual diagnosis.
//!
//! Invariant: the shell is an adapter. It parses dot commands and formats
//! output; every statement it runs goes through the public `rustdb` facade.
//!
//! Status: the interactive shell lands in phase 12. task-1782 creates the
//! binary so the dependency-direction contract covers it from the start.

#![forbid(unsafe_code)]

/// Reports which phase fills the shell in, then exits non-zero so no caller
/// mistakes the placeholder for a working shell.
fn main() {
    eprintln!(
        "rustdb-shell is not implemented yet; it lands in {}.",
        rustdb::IMPLEMENTATION_PHASE
    );
    std::process::exit(2);
}
