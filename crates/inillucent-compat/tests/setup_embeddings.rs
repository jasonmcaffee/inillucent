//! `inillucent setup-embeddings`, driven as a program.
//!
//! Invariant: **this suite never downloads anything.** Everything it checks is
//! about the command's behaviour before a byte moves - what a bare invocation
//! does, what a machine with nothing installed reports, what an unknown
//! component or an unknown profile is refused with - and each of those is a case
//! that has already been wrong once.
//!
//! The one that matters most is the first. The command table's parity suite
//! calls every command with no arguments, and the first version of this command
//! answered that by installing: `cargo test` pulled 535 MB into the machine's
//! install directory, and nothing in the suite said so. A test that asserts the
//! bare call installs nothing is the one that would have caught it, so it is
//! here, driving the real binary rather than the function.
//!
//! The download path itself is checked where the downloading is: the network
//! test in `inillucent_remote::http` fetches from both hosts, and refuses a
//! wrong digest.

use std::path::PathBuf;
use std::process::Command;

use inillucent_compat::workspace_root;

/// Where this suite's install roots go.
///
/// Under `_agent_output`, which is not tracked, and one directory per test so a
/// failure leaves evidence rather than a shared directory two tests fought over.
///
/// @param name - the test's own name
fn area(name: &str) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/setup-embeddings")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the `inillucent` binary, building it first.
///
/// `None` when it will not build, which is what a machine missing the MSVC
/// environment produces - and `cliproc::program` announces that through the one
/// skip helper, so `--strict` counts it rather than reading the six cases as
/// passes.
///
/// **This suite used to look for the binary at a path nothing writes to**
/// (task-2066 §4.4.16). It ran `cargo build -p inillucent-cli`, which honours
/// whatever `CARGO_TARGET_DIR` says, and then looked at the hardcoded
/// `<workspace>/target/debug`. Every `git worktree` in this repository
/// redirects that variable to a directory of its own, so the build succeeded,
/// the lookup found nothing, and all six cases returned having asserted
/// nothing - and returned without a message, so neither `--strict` nor the skip
/// guard could see it. `cliproc::program` reads the profile and the target
/// directory back off the calling test's own path, which is the same problem
/// task-1962 solved for twelve other suites.
fn binary() -> PathBuf {
    inillucent_compat::cliproc::program("inillucent")
}

/// Runs the command against an install root, and returns its output.
///
/// The two override variables are cleared, because they are the ones a
/// developer's shell has set and they would make this suite report on that
/// machine's install rather than on the empty directory it just made.
///
/// @param root - the install root
/// @param arguments - what to pass after the verb
fn run(root: &PathBuf, arguments: &[&str]) -> (bool, String, String) {
    let binary = binary();
    let output = Command::new(binary)
        .current_dir(workspace_root())
        .arg("setup-embeddings")
        .args(arguments)
        .env("INILLUCENT_HOME", root)
        .env_remove("INILLUCENT_ONNX_DIR")
        .env_remove("ORT_DYLIB_PATH")
        .env_remove("INILLUCENT_EMBED_RESIDENCY")
        .output()
        .unwrap_or_else(|error| panic!("inillucent did not start: {error}"));
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// How many bytes a directory holds, counting everything under it.
///
/// @param path - the directory
fn bytes_under(path: &PathBuf) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            total = total.saturating_add(bytes_under(&entry.path()));
        } else if let Ok(metadata) = entry.metadata() {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

/// A bare `setup-embeddings` reports and downloads nothing.
///
/// This is the case that was wrong: the command used to install on a bare call,
/// so the parity suite's "answer an empty call" check fetched the whole model.
/// The assertion is on bytes under the install root rather than on the message,
/// because a message can be right while the command is still writing half a
/// gigabyte beside it.
#[test]
fn a_bare_invocation_installs_nothing() {
    let root = area("bare");
    let (ok, stdout, _) = run(&root, &[]);
    assert!(ok, "a bare invocation succeeds: {stdout}");
    assert!(
        stdout.contains("setup-embeddings all"),
        "it says how to install: {stdout}"
    );
    assert_eq!(bytes_under(&root), 0, "a bare invocation writes nothing");
}

/// `--status` on a machine with nothing installed says so, names the command
/// that fixes it, and succeeds.
///
/// Succeeds rather than fails, because "nothing is installed" is an answer to
/// the question rather than a failure to answer it, and a script checking
/// whether to install should not have to read a message to find out.
#[test]
fn status_on_an_empty_machine_says_what_is_missing() {
    let root = area("status");
    let (ok, stdout, _) = run(&root, &["--status"]);
    assert!(ok, "status succeeds even with nothing installed: {stdout}");
    assert!(stdout.contains("ONNX Runtime: not installed"), "{stdout}");
    assert!(stdout.contains("setup-embeddings runtime"), "{stdout}");
    assert!(stdout.contains("not installed"), "{stdout}");
    assert_eq!(bytes_under(&root), 0, "status writes nothing");
}

/// `--status --output json` carries the same answer as structured fields.
///
/// `ready` is the field a script branches on, so it is checked by name: a status
/// that printed the right sentence and reported `ready: true` on an empty
/// machine would be worse than no status at all.
#[test]
fn status_reports_readiness_as_a_field() {
    let root = area("status-json");
    let (ok, stdout, _) = run(&root, &["--status", "--output", "json"]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("\"ready\": false"), "{stdout}");
    assert!(stdout.contains("\"runtime\": null"), "{stdout}");
    assert!(stdout.contains("\"residency\": \"idle:300s\""), "{stdout}");
}

/// A component this command does not know is refused, by name, before anything
/// is fetched.
#[test]
fn an_unknown_component_is_refused_before_anything_is_fetched() {
    let root = area("component");
    let (ok, stdout, stderr) = run(&root, &["everything"]);
    assert!(!ok, "an unknown component fails: {stdout}");
    let said = format!("{stdout}{stderr}");
    assert!(said.contains("everything"), "the refusal names it: {said}");
    assert!(said.contains("all, runtime or model"), "{said}");
    assert_eq!(bytes_under(&root), 0);
}

/// A residency profile this build does not know is refused before anything is
/// fetched, rather than silently falling back to the default.
///
/// The order matters and is what this asserts: the profile is parsed before the
/// download starts, so a typo costs a message rather than 620 MB and then a
/// message.
#[test]
fn an_unknown_residency_profile_is_refused_before_anything_is_fetched() {
    let root = area("residency");
    let (ok, stdout, stderr) = run(&root, &["all", "--residency", "sometimes"]);
    assert!(!ok, "an unknown profile fails: {stdout}");
    let said = format!("{stdout}{stderr}");
    assert!(said.contains("sometimes"), "the refusal names it: {said}");
    assert!(
        said.contains("resident"),
        "and names the ones that work: {said}"
    );
    assert_eq!(
        bytes_under(&root),
        0,
        "nothing was fetched before the profile was read"
    );
}

/// `--residency` alone records the profile and downloads nothing.
///
/// A person changing their mind about when the model is in memory should not
/// re-fetch half a gigabyte to do it.
#[test]
fn setting_only_the_profile_records_it_and_downloads_nothing() {
    let root = area("profile-only");
    let (ok, stdout, _) = run(&root, &["--residency", "resident"]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("Residency profile: resident"), "{stdout}");

    // Recorded, so the next process reads it back rather than the default.
    let (_, again, _) = run(&root, &["--status"]);
    assert!(again.contains("Residency profile: resident"), "{again}");

    // The state file is the only thing written, and it is under a kilobyte.
    assert!(
        bytes_under(&root) < 4096,
        "a profile change writes the state file and nothing else, and this wrote {} bytes",
        bytes_under(&root)
    );
}
