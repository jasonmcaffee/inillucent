//! What `inillucent batch` promises, driven through the real binary.
//!
//! Invariant: **a script `batch` refused left nothing behind.** The command's
//! own description - which the MCP tool `inillucent_batch` inherits verbatim -
//! says "either all of them take effect or none of them do, which is what you
//! want when creating a schema or loading related rows". Until task-1932 that
//! was not true of the code: `Connection::execute_batch` is a loop of
//! `execute_any` with nothing around it, nothing opened a transaction, and each
//! statement committed as it succeeded. `inillucent batch "INSERT ...;
//! INSERT ...; GARBAGE"` reported failure with two rows committed.
//!
//! These cases drive the shipped `inillucent` binary rather than the engine,
//! because the promise is the command's and not the engine's: `execute_batch`
//! is still a loop, and it is `verbs::batch` that wraps a transaction round it.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::cliproc;
use inillucent_compat::workspace_root;

/// Where this suite's scratch databases live.
fn area() -> PathBuf {
    let path = workspace_root().join("_agent_output/cli-batch");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns a fresh database path with no files left over from a previous run.
///
/// @param name - the file's name, so no two cases share one
fn scratch(name: &str) -> PathBuf {
    let path = area().join(name);
    inillucent_base::testing::remove_database(&path);
    path
}

/// `(exit code, standard output, standard error)` from one invocation.
type Ran = (i32, String, String);

/// Runs the `inillucent` binary against one database.
///
/// @param program - the binary
/// @param database - the file
/// @param arguments - everything after `--db <file>`
fn run(program: &Path, database: &Path, arguments: &[&str]) -> Ran {
    let named = database.to_string_lossy().into_owned();
    let output = Command::new(program)
        .args(["--db", &named])
        .args(arguments)
        .output()
        .expect("the binary runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
    )
}

/// Returns the single integer a one-row, one-column `query` produced.
///
/// **Read out of the rendered table rather than by matching on the JSON.** The
/// JSON object carries `changes`, `last_insert_rowid`, `row_count`, `total` and
/// `elapsed_ms` alongside the row, so a substring check for a zero somewhere in
/// it passes on a database holding any number of rows at all - which is the
/// test that cannot fail that `tests/inillucent-testing-tdd.md` rule 1.5 is
/// about. The `text` field is the aligned table the command prints, and its
/// last line is the value.
///
/// @param stdout - what the command printed
fn only_value(stdout: &str) -> String {
    let rows = stdout
        .split("\"rows\": [")
        .nth(1)
        .unwrap_or_else(|| panic!("no rows in the output: {stdout}"));
    let inner = rows
        .split(']')
        .next()
        .unwrap_or_else(|| panic!("no row in the output: {stdout}"));
    inner
        .trim()
        .trim_start_matches('[')
        .trim()
        .trim_end_matches(',')
        .trim()
        .to_string()
}

/// H2 (task-1920): a `batch` whose last statement fails commits none of them.
///
/// The middle of the script is what makes this worth asserting: the two inserts
/// before the failure are ordinary, they succeed, and before the fix they were
/// each their own transaction and were each committed on the spot. So the
/// command reported failure and the table held two rows - the half-applied
/// script its own description names as the reason to use `batch` at all.
///
/// The read that checks it is a separate invocation of the binary, so what is
/// asserted is what reached the file rather than what one process believed.
#[test]
fn a_batch_whose_script_fails_commits_nothing() {
    let program = cliproc::program("inillucent");
    let database = scratch("failing-batch.rdb");
    let (code, _, stderr) = run(
        &program,
        &database,
        &[
            "exec",
            "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)",
        ],
    );
    assert_eq!(code, 0, "the schema is created: {stderr}");

    let (code, _, _) = run(
        &program,
        &database,
        &[
            "batch",
            "INSERT INTO note (body) VALUES ('one'); \
             INSERT INTO note (body) VALUES ('two'); \
             GARBAGE",
        ],
    );
    assert_ne!(code, 0, "a batch with a syntax error must report failure");

    let (code, stdout, stderr) = run(
        &program,
        &database,
        &["query", "SELECT count(*) FROM note", "--output", "json"],
    );
    assert_eq!(code, 0, "the count reads: {stderr}");
    assert_eq!(
        only_value(&stdout),
        "0",
        "the failed batch left rows behind: {stdout}"
    );

    // The same again with a failure the *engine* raises rather than the parser,
    // so the rollback is exercised on a statement that got as far as running.
    let (code, _, _) = run(
        &program,
        &database,
        &[
            "batch",
            "INSERT INTO note (id, body) VALUES (1, 'one'); \
             INSERT INTO note (id, body) VALUES (2, 'two'); \
             INSERT INTO note (id, body) VALUES (1, 'again')",
        ],
    );
    assert_ne!(
        code, 0,
        "a batch whose last statement violates a key must report failure"
    );
    let (_, stdout, _) = run(
        &program,
        &database,
        &["query", "SELECT count(*) FROM note", "--output", "json"],
    );
    assert_eq!(
        only_value(&stdout),
        "0",
        "the failed batch left rows behind: {stdout}"
    );
}

/// A `batch` that succeeds commits every statement in it.
///
/// The other half of the promise, and the one a fix that simply refused
/// everything would break. The read is a separate invocation for the same
/// reason as above.
#[test]
fn a_batch_that_succeeds_commits_every_statement() {
    let program = cliproc::program("inillucent");
    let database = scratch("succeeding-batch.rdb");
    let (code, _, stderr) = run(
        &program,
        &database,
        &[
            "batch",
            "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT); \
             CREATE INDEX note_body ON note (body); \
             INSERT INTO note (body) VALUES ('one'); \
             INSERT INTO note (body) VALUES ('two')",
        ],
    );
    assert_eq!(code, 0, "the batch runs: {stderr}");

    let (code, stdout, stderr) = run(
        &program,
        &database,
        &[
            "query",
            "SELECT body FROM note ORDER BY id",
            "--output",
            "json",
        ],
    );
    assert_eq!(code, 0, "the rows read: {stderr}");
    assert!(stdout.contains("one"), "{stdout}");
    assert!(stdout.contains("two"), "{stdout}");
    let (code, _, stderr) = run(&program, &database, &["integrity-check"]);
    assert_eq!(code, 0, "the database checks out: {stderr}");
}

/// A failure inside `batch` reports the engine's status, as `exec` does.
///
/// **`batch` reported every failure as `syntax` with exit code 1.** It read the
/// error's text and dropped the error, so a statement the engine has not built
/// came back `syntax` from `batch` and `unsupported`, exit code 3, from `exec`.
/// A script could not tell "not built yet" from "wrong" by running it in a
/// batch, which is the distinction the exit codes exist for. Each statement
/// below is run both ways and the two must agree on the exit code and on the
/// status name.
#[test]
fn a_batch_reports_the_same_status_as_exec() {
    let program = cliproc::program("inillucent");
    let database = scratch("status-batch.rdb");
    let (code, _, stderr) = run(
        &program,
        &database,
        &[
            "exec",
            "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)",
        ],
    );
    assert_eq!(code, 0, "the schema is created: {stderr}");
    let (code, _, stderr) = run(
        &program,
        &database,
        &["exec", "INSERT INTO note (id, body) VALUES (1, 'one')"],
    );
    assert_eq!(code, 0, "the row is written: {stderr}");

    let cases: [(&str, i32, &str); 3] = [
        ("SELECT id FROM note LIMIT 1 + 1", 3, "\"unsupported\""),
        (
            "INSERT INTO note (id, body) VALUES (1, 'again')",
            1,
            "\"constraint\"",
        ),
        ("SELECT no_such_function(1)", 1, "\"not_found\""),
    ];
    for (sql, expected, status) in cases {
        for verb in ["exec", "batch"] {
            let (code, stdout, stderr) = run(&program, &database, &[verb, sql, "--output", "json"]);
            let said = format!("{stdout}{stderr}");
            assert_eq!(code, expected, "{verb} {sql} exited {code}: {said}");
            assert!(
                said.contains(status),
                "{verb} {sql} did not report {status}: {said}"
            );
        }
    }
}
