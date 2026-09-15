//! Writes the obligation registers to `compat/api`, and the pragma page to `docs/`.
//!
//! Invariant: this binary decides *where* the registers go and nothing else.
//! What is in them is `inillucent_compat::obligations`, so the harness test can
//! generate the same bytes without shelling out to a program.
//!
//! Usage: `cargo run -p inillucent-compat --bin inillucent-obligations -- [--out <dir>]`

use std::path::PathBuf;
use std::process::ExitCode;

use inillucent_compat::obligations::{pragma_page, registers};
use inillucent_compat::workspace_root;

/// Writes the three registers and the pragma page, and reports where they went.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let out = flag(&arguments, "--out").unwrap_or_else(|| workspace_root().join("compat/api"));
    if let Err(reason) = std::fs::create_dir_all(&out) {
        eprintln!("cannot create {}: {reason}", out.display());
        return ExitCode::FAILURE;
    }
    for (name, body) in registers() {
        let path = out.join(name);
        if let Err(reason) = std::fs::write(&path, body) {
            eprintln!("cannot write {}: {reason}", path.display());
            return ExitCode::FAILURE;
        }
        println!("wrote {}", path.display());
    }
    // The pragma page goes to `docs/` rather than to `--out`, because it is
    // documentation rather than a register a program reads, and `docs/README.md`
    // lists it. `--out` stays what it was so a caller generating the registers
    // into a scratch directory is unaffected.
    let page = workspace_root().join("docs/pragmas.md");
    if let Err(reason) = std::fs::write(&page, pragma_page()) {
        eprintln!("cannot write {}: {reason}", page.display());
        return ExitCode::FAILURE;
    }
    println!("wrote {}", page.display());
    ExitCode::SUCCESS
}

/// Returns the value of a `--flag value` argument.
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}
