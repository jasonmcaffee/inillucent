//! What each command line does before it opens anything.
//!
//! Invariant: **a command line the program cannot act on leaves the directory
//! exactly as it found it.** Both binaries end in the same shell, and that
//! shell creates a database that is not there rather than refusing it, so every
//! word either of them reads as a file name is a file that gets written. A
//! mistyped command therefore used to exit 0, print nothing, and leave a
//! 128 KiB database and a log segment named after the typo - `inillucent
//! bogusverb` produced `bogusverb` and `bogusverb-wal.0000000001` - and a
//! mistyped option did the same thing one word later, which is where the files
//! called `--`, `-d` and `--db` in an unrelated repository came from.
//!
//! The pinned reference is the argument for the second half of this. `sqlite3`
//! 3.53.4 answers `sqlite3 --db app.db "SELECT 1"` with `Error: unknown option:
//! -db`, creates nothing, and exits non-zero; the shell here used to open a
//! file called `--db` instead, so refusing it is the compatible answer rather
//! than a departure from one.
//!
//! Each case runs a real binary in a directory of its own and then lists that
//! directory, because the defect is a file on disk and nothing else reports it.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use inillucent_compat::workspace_root;

/// What a run produced.
struct Ran {
    /// The exit status, or `None` if a signal ended it.
    code: Option<i32>,
    /// Everything the run printed, both streams together.
    printed: String,
    /// The names left in the directory it ran in, sorted.
    left_behind: Vec<String>,
}

/// Returns an empty directory of its own for one case.
///
/// @param name - the case, which names the directory
fn area(name: &str) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/cli-arguments")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns one of the command surface's binaries, building them first.
///
/// **`None` is announced as a skip rather than returned quietly (task-1913).**
/// Every case in this file opened with `let Some(program) = binary(...) else {
/// return; };`, so a build that did not produce the binary made eight tests
/// pass without running anything - green under `--strict`, which exists to
/// turn exactly that into a failure. Announcing here rather than at each call
/// site means the next case added to this file cannot forget it. It is the
/// marker `differential::skipping` writes that `testrun` classifies, and the
/// same one `mcp_cancel.rs` and `budgets.rs` already used.
///
/// @param name - the binary's name, without the platform's suffix
fn binary(name: &str) -> Option<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let built = Command::new(cargo)
        .current_dir(workspace_root())
        .args(["build", "-p", "inillucent-cli"])
        .status();
    let found = match built {
        Ok(status) if status.success() => {
            let mut directory = std::env::current_exe().unwrap_or_default();
            directory.pop();
            directory.pop();
            let path = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            path.is_file().then_some(path)
        }
        _ => None,
    };
    if found.is_none() {
        inillucent_compat::differential::skipping(&format!("{name} did not build"));
    }
    found
}

/// Runs a binary in a directory of its own and reports what it left there.
///
/// Standard input is closed rather than inherited, because the shell reads it
/// when no statement was given and a test that waits on a terminal never ends.
///
/// @param program - the binary to run
/// @param case - the directory to run in
/// @param arguments - the command line to give it
fn run(program: &PathBuf, case: &str, arguments: &[&str]) -> Ran {
    let directory = area(case);
    let output = Command::new(program)
        .args(arguments)
        .current_dir(&directory)
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    let mut printed = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    printed.push_str(&String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"));
    let mut left_behind: Vec<String> = std::fs::read_dir(&directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    left_behind.sort();
    Ran {
        code: output.status.code(),
        printed,
        left_behind,
    }
}

/// A mistyped command is refused, and writes nothing.
#[test]
fn a_mistyped_command_creates_no_database() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let ran = run(&program, "mistyped-command", &["bogusverb"]);
    assert_eq!(ran.code, Some(2), "printed: {}", ran.printed);
    assert!(
        ran.left_behind.is_empty(),
        "a mistyped command left {:?} behind",
        ran.left_behind
    );
    assert!(
        ran.printed.contains("is not a command"),
        "printed: {}",
        ran.printed
    );
}

/// A mistyped command is answered with the one it is closest to.
#[test]
fn a_mistyped_command_names_the_command_it_is_closest_to() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let ran = run(&program, "nearest-command", &["qeury"]);
    assert_eq!(ran.code, Some(2), "printed: {}", ran.printed);
    assert!(ran.printed.contains("query"), "printed: {}", ran.printed);
    assert!(ran.left_behind.is_empty(), "left {:?}", ran.left_behind);
}

/// The documented shell form still opens a database that is not there yet.
#[test]
fn a_bare_file_name_is_still_the_shell() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let ran = run(&program, "bare-file-name", &["app.rdb", "SELECT 1"]);
    assert_eq!(ran.code, Some(0), "printed: {}", ran.printed);
    assert!(ran.printed.contains('1'), "printed: {}", ran.printed);
    assert!(
        ran.left_behind.iter().any(|name| name == "app.rdb"),
        "the shell did not create app.rdb; it left {:?}",
        ran.left_behind
    );
}

/// A file with no extension is still reachable, by writing it as a path.
#[test]
fn a_path_opens_a_file_with_no_extension() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let ran = run(&program, "extensionless-path", &["./ledger", "SELECT 1"]);
    assert_eq!(ran.code, Some(0), "printed: {}", ran.printed);
    assert!(
        ran.left_behind.iter().any(|name| name == "ledger"),
        "left {:?}",
        ran.left_behind
    );
}

/// A mistyped option is refused by the shell instead of becoming the file name.
#[test]
fn a_mistyped_option_creates_no_database() {
    let Some(program) = binary("inillucent-shell") else {
        return;
    };
    let ran = run(
        &program,
        "mistyped-option",
        &["--db", "app.rdb", "SELECT 1"],
    );
    assert_eq!(ran.code, Some(2), "printed: {}", ran.printed);
    assert!(
        ran.left_behind.is_empty(),
        "a mistyped option left {:?} behind",
        ran.left_behind
    );
    assert!(
        ran.printed.contains("unknown option: --db"),
        "printed: {}",
        ran.printed
    );
}

/// The same mistyped option through the verb-shaped binary is refused too.
#[test]
fn a_mistyped_option_is_refused_through_the_verb_binary() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let ran = run(&program, "mistyped-option-verb", &["-x", "app.rdb"]);
    assert_ne!(ran.code, Some(0), "printed: {}", ran.printed);
    assert!(
        ran.left_behind.is_empty(),
        "a mistyped option left {:?} behind",
        ran.left_behind
    );
}

/// After `--`, a file whose name begins with a dash still opens.
#[test]
fn the_separator_still_opens_a_dashed_file_name() {
    let Some(program) = binary("inillucent-shell") else {
        return;
    };
    let ran = run(
        &program,
        "dashed-file-name",
        &["--", "-ledger.rdb", "SELECT 1"],
    );
    assert_eq!(ran.code, Some(0), "printed: {}", ran.printed);
    assert!(
        ran.left_behind.iter().any(|name| name == "-ledger.rdb"),
        "left {:?}",
        ran.left_behind
    );
}

/// An option the engine refuses by name still says why, and still writes nothing.
#[test]
fn a_refused_option_still_says_why() {
    let Some(program) = binary("inillucent-shell") else {
        return;
    };
    let ran = run(
        &program,
        "refused-option",
        &["-mmap", "268435456", "app.rdb"],
    );
    assert_eq!(ran.code, Some(1), "printed: {}", ran.printed);
    assert!(
        ran.printed.contains("-mmap is not supported"),
        "printed: {}",
        ran.printed
    );
    assert!(ran.left_behind.is_empty(), "left {:?}", ran.left_behind);
}
