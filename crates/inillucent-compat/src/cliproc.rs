//! Running the shipped programs as processes, and reading what they printed.
//!
//! Invariant: **what is under test here is the program, not the library it
//! links.** Every helper spawns a built binary, waits for it to exit, and reads
//! its exit code and its standard streams. Nothing here calls into the engine
//! in process, because the whole reason these suites exist is that argument
//! parsing, output rendering and the exit code are the layer a user touches and
//! the layer nothing was exercising (task-1969, 5.1 to 5.6).
//!
//! **Why the binaries are looked for beside the calling test rather than in
//! `target/debug` (task-1962).** A test binary under an ordinary `cargo test`
//! lives in `target/debug/deps/`, under a release run in `target/release/deps/`,
//! and under `cargo llvm-cov` in a target directory of its own. Building into
//! the default directory and then looking in the caller's found nothing in the
//! last case, and twelve suites failed with "the shell is not built" while a
//! perfectly good shell sat somewhere else. So the profile and the target
//! directory are both read back off the calling test's own path.

// **This module may panic, and the crate-level deny does not reach it.** It is
// the same case `differential.rs` makes: a helper every `tests/*.rs` target
// uses has to live in `src/`, which means it is compiled without `cfg(test)`
// and the crate's test-only relaxation does not apply.
//
// What it panics on is a broken environment rather than a result - a binary
// that will not start, a process that will not finish, `--output json` that
// produced something that is not a JSON document, a field of that document
// that is absent or of the wrong type. Returning an `Option` for those would
// put the caller back where this ticket started: a test that reads `None` and
// passes. The one absence that is *not* a broken environment - the binary is
// not built on this machine - is the one thing here that returns `None`, and
// `program` announces it through `differential::skipping` before it does.
#![allow(clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use inillucent_scalar::json::node::Node;
use inillucent_scalar::json::{parse, render};

/// What a run of a program produced.
pub struct Ran {
    /// Its exit code, or 130 for a run a signal ended.
    pub code: i32,
    /// Everything it wrote to standard output.
    pub stdout: String,
    /// Everything it wrote to standard error.
    pub stderr: String,
}

impl Ran {
    /// Returns both streams, for a message that has to show what happened.
    pub fn said(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Returns one of the command surface's binaries, building them first.
///
/// **`None` is announced as a skip rather than returned quietly (task-1913).**
/// A build that did not produce the binary used to make a whole file's worth of
/// cases return without asserting anything - green under `--strict`, which
/// exists to turn exactly that into a failure. Announcing here rather than at
/// each call site means the next case added cannot forget it.
///
/// @param name - the binary's name, without the platform's suffix
pub fn program(name: &str) -> Option<PathBuf> {
    let mut directory = std::env::current_exe().ok()?;
    directory.pop();
    directory.pop();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut build = Command::new(cargo);
    build
        .current_dir(crate::workspace_root())
        .args(["build", "-p", "inillucent-cli"]);
    let profile = directory
        .file_name()
        .map(|part| part.to_string_lossy().into_owned())
        .unwrap_or_else(|| "debug".to_string());
    if profile != "debug" {
        build.args(["--profile", &profile]);
    }
    if let Some(target) = directory.parent() {
        build.arg("--target-dir").arg(target);
    }
    let built = build.status().ok().is_some_and(|status| status.success());
    let path = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let found = (built && path.is_file()).then_some(path);
    if found.is_none() {
        crate::differential::skipping(&format!("{name} did not build"));
    }
    found
}

/// Runs a program with its standard input closed, and returns what it produced.
///
/// Standard input is closed rather than inherited because the shell reads it
/// when no statement was given, and a test that waits on a terminal never ends.
///
/// @param program - the binary to run
/// @param arguments - the command line
pub fn run(program: &Path, arguments: &[&str]) -> Ran {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("{} did not start: {error}", program.display()));
    from_output(&output)
}

/// Runs a program, writing `input` to its standard input.
///
/// @param program - the binary to run
/// @param arguments - the command line
/// @param input - what to write to its standard input
pub fn run_with_input(program: &Path, arguments: &[&str], input: &str) -> Ran {
    use std::io::Write;
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{} did not start: {error}", program.display()));
    if let Some(pipe) = child.stdin.as_mut() {
        let _ = pipe.write_all(input.as_bytes());
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("{} did not finish: {error}", program.display()));
    from_output(&output)
}

/// Runs statements in a child process and kills it before it can tidy up.
///
/// **What a crash is, done with a process rather than with a pragma.** A test
/// that needs a log nothing has folded into the file used to ask for it with
/// `PRAGMA locking_mode = EXCLUSIVE`, the statements, and `PRAGMA locking_mode
/// = NORMAL` - the last of which released the file without checkpointing, which
/// is what the operating system does for a process that has died. That release
/// is gone (task-1980): a connection that let the file go with pages still
/// dirty left the file describing a database without the statement that had
/// just succeeded, which is a lost write rather than a stale read, and two
/// writer processes lost 43% of their acknowledged commits to it.
///
/// So the crash is a real one now. The shell is spawned with its standard input
/// held open, the statements are written to it, and the process is killed while
/// it waits for the next line: nothing is checkpointed because `exclusive`
/// checkpoints nothing, no destructor runs because the process is gone, and the
/// locks are released by the operating system, which is the thing being
/// simulated.
///
/// The statements are sent under `PRAGMA locking_mode = EXCLUSIVE`, which the
/// caller does not write itself, and a `SELECT` follows them so the caller can
/// see they ran before the kill.
///
/// @param shell - the `inillucent-shell` binary
/// @param database - the file to open
/// @param sql - the statements, each ending in a semicolon
/// @returns what the shell printed before it was killed
pub fn write_and_crash(shell: &Path, database: &Path, sql: &str) -> String {
    use std::io::{Read, Write};
    let mut child = Command::new(shell)
        .arg(database.to_string_lossy().replace('\\', "/"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{} did not start: {error}", shell.display()));
    let text = format!("PRAGMA locking_mode = EXCLUSIVE;\n{sql}\nSELECT 'written';\n");
    if let Some(pipe) = child.stdin.as_mut() {
        let _ = pipe.write_all(text.as_bytes());
        let _ = pipe.flush();
    }
    // **Read until the sentinel rather than sleeping.** The kill has to land
    // after the statements have run and before the shell is asked to exit, and
    // a fixed wait is either too short on a loaded machine or wasted on an idle
    // one. The shell prints a row per statement, so the sentinel arriving is
    // the statements having finished.
    //
    // Bounded, because a shell that buffered its output would never print the
    // sentinel and this would wait for ever. The bound is generous - the work
    // is a handful of statements - and the kill happens either way, so a run
    // that reaches it fails on the caller's assertion about the sentinel rather
    // than by hanging.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut said = String::new();
    if let Some(pipe) = child.stdout.as_mut() {
        let mut byte = [0u8; 1];
        while !said.contains("written") && std::time::Instant::now() < deadline {
            match pipe.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => said.push(char::from(byte[0])),
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    said.replace("\r\n", "\n")
}

/// Turns a finished process into a `Ran`, normalising line endings.
///
/// The endings are normalised because every assertion below compares against a
/// sentence written in this source file, and a Windows run would otherwise
/// differ from a Linux one in a way that is not about the engine.
///
/// @param output - what the run produced
fn from_output(output: &Output) -> Ran {
    Ran {
        code: output.status.code().unwrap_or(130),
        stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
    }
}

/// Parses a program's `--output json` and returns the document.
///
/// It panics rather than returning an error: a command that was asked for JSON
/// and printed something else has failed the thing under test, and a caller
/// that handled the failure would be deciding what to do about it one case at a
/// time.
///
/// @param text - what the program printed on standard output
pub fn document(text: &str) -> Node {
    parse::parse(text.trim())
        .unwrap_or_else(|failure| {
            panic!("`--output json` did not produce a JSON document ({failure:?}):\n{text}")
        })
        .node
}

/// Returns one member of a JSON object, or nothing.
///
/// @param node - the object
/// @param name - the member's label
pub fn field<'tree>(node: &'tree Node, name: &str) -> Option<&'tree Node> {
    match node {
        Node::Object(members) => members
            .iter()
            .find(|(label, _)| render::unescape(label) == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

/// Returns a JSON string's content, with its escapes resolved.
///
/// @param node - the node, which may be any kind
pub fn text_of(node: &Node) -> Option<String> {
    match node {
        Node::Text(_) | Node::TextJ(_) | Node::Text5(_) | Node::TextRaw(_) => {
            Some(render::unescape(node))
        }
        _ => None,
    }
}

/// Returns a JSON number's value.
///
/// @param node - the node, which may be any kind
pub fn number_of(node: &Node) -> Option<f64> {
    match node {
        Node::Int(text) | Node::Int5(text) | Node::Float(text) | Node::Float5(text) => {
            text.parse().ok()
        }
        _ => None,
    }
}

/// Returns a JSON array's items.
///
/// @param node - the node, which may be any kind
pub fn items(node: &Node) -> Option<&[Node]> {
    match node {
        Node::Array(members) => Some(members),
        _ => None,
    }
}

/// Returns a named field of the result envelope as text, or panics saying which
/// field and what the whole document was.
///
/// The panic carries the document because a failure here is almost always a
/// command whose envelope changed shape, and the next question is always "what
/// did it print instead".
///
/// @param stdout - what the program printed
/// @param name - the field to read
pub fn text_field(stdout: &str, name: &str) -> String {
    let node = document(stdout);
    let found = field(&node, name)
        .unwrap_or_else(|| panic!("the result object has no `{name}`:\n{stdout}"));
    text_of(found).unwrap_or_else(|| panic!("`{name}` is not a string:\n{stdout}"))
}

/// Returns a named field of the result envelope as a number.
///
/// @param stdout - what the program printed
/// @param name - the field to read
pub fn number_field(stdout: &str, name: &str) -> f64 {
    let node = document(stdout);
    let found = field(&node, name)
        .unwrap_or_else(|| panic!("the result object has no `{name}`:\n{stdout}"));
    number_of(found).unwrap_or_else(|| panic!("`{name}` is not a number:\n{stdout}"))
}

/// Returns the `rows` of a result envelope, each row rendered as text.
///
/// Rendered rather than typed, because what every caller here asks is "is this
/// value in the answer", and a row of mixed integers and strings would
/// otherwise need a match per cell at every call site.
///
/// @param stdout - what the program printed
pub fn rows(stdout: &str) -> Vec<Vec<String>> {
    let node = document(stdout);
    let Some(rows) = field(&node, "rows").and_then(items) else {
        panic!("the result object has no `rows` array:\n{stdout}");
    };
    rows.iter()
        .map(|row| {
            items(row)
                .unwrap_or_default()
                .iter()
                .map(|cell| text_of(cell).unwrap_or_else(|| render::to_text(cell)))
                .collect()
        })
        .collect()
}

/// Returns the column names of a result envelope.
///
/// @param stdout - what the program printed
pub fn column_names(stdout: &str) -> Vec<String> {
    let node = document(stdout);
    let Some(columns) = field(&node, "columns").and_then(items) else {
        panic!("the result object has no `columns` array:\n{stdout}");
    };
    columns
        .iter()
        .filter_map(|column| field(column, "name").and_then(text_of))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The envelope's fields come back typed.
    #[test]
    fn a_result_object_reads_back_by_name() {
        let printed = r#"{"ok":true,"command":"tables","total":2,
            "columns":[{"name":"name","type":"text"}],
            "rows":[["note"],["docs"]],"text":"note\ndocs"}"#;
        assert_eq!(text_field(printed, "command"), "tables");
        assert_eq!(number_field(printed, "total"), 2.0);
        assert_eq!(column_names(printed), vec!["name".to_string()]);
        assert_eq!(
            rows(printed),
            vec![vec!["note".to_string()], vec!["docs".to_string()]]
        );
    }

    /// A string's escapes are resolved, which is what a Windows path needs.
    #[test]
    fn an_escaped_path_reads_back_whole() {
        let printed = r#"{"path":"C:\\a\\b.rdb"}"#;
        assert_eq!(text_field(printed, "path"), "C:\\a\\b.rdb");
    }
}
