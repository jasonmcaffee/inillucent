//! `--root` against the file system, not against path text.
//!
//! Invariant: **a process started with `--root DIR` cannot read, create,
//! replace, attach, back up, restore, import, export or migrate a file that
//! resolves outside `DIR`.** The check this suite guards used to compare
//! normalised path text with `starts_with`, and there were two ways
//! past it: a Windows junction placed below the root, whose text is inside and
//! whose target is not, and `ATTACH DATABASE` with an absolute path, which
//! never reached the check at all because it arrives inside a SQL statement
//! rather than as a command argument.
//!
//! Every case here drives the real `inillucent` binary rather than calling
//! `Context::confine`, because the defect was not in that function's arithmetic
//! - it was in which file operations reached it. A unit test of the check
//! would have passed on the day the junction worked.
//!
//! The link cases need a platform that makes links and, on Windows, a
//! directory junction, which `mklink /J` makes without a privilege. Where the
//! link cannot be created the case says so and returns; `--strict` in the
//! runner is what turns a suite that could not run into a visible one, and the
//! non-link cases below still run everywhere.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::workspace_root;

/// Where this suite's scratch directories live.
///
/// @param name - the case's own directory, so two cases never share a root
fn area(name: &str) -> PathBuf {
    // One component per `join`, so the path has no forward slash in it: this
    // file hands paths to `cmd`, which reads one as a switch (task-1913).
    let path = workspace_root()
        .join("_agent_output")
        .join("confinement")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the verb-shaped command line, building it first.
///
/// **`None` is announced as a skip rather than returned quietly (task-1913).**
/// Ten cases in this file opened with `let Some(program) = binary(...) else {
/// return; };`, so a build that did not produce the binary made all ten pass
/// without running anything - and these are the cases that check a confined
/// server cannot be talked into opening a file outside its root, which is the
/// last place a silent pass belongs. `--strict` turns the announced skip into
/// a failure; the two link cases in this file already announced theirs.
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

/// What one run of the command line printed and what it returned.
struct Ran {
    /// Standard output.
    out: String,
    /// Standard error.
    err: String,
    /// Whether the process reported success.
    ok: bool,
}

impl Ran {
    /// Returns both streams together, which is where a refusal lands.
    fn text(&self) -> String {
        format!("{}{}", self.out, self.err)
    }
}

/// Runs the command line with a root and returns what it printed.
///
/// @param program - the binary to run
/// @param root - the directory to confine to
/// @param arguments - the rest of the command line
fn run(program: &Path, root: &Path, arguments: &[&str]) -> Ran {
    let mut command = Command::new(program);
    command.arg("--root").arg(root);
    command.args(arguments);
    let produced = match command.output() {
        Ok(produced) => produced,
        Err(error) => {
            return Ran {
                out: String::new(),
                err: format!("could not run: {error}"),
                ok: false,
            }
        }
    };
    Ran {
        out: String::from_utf8_lossy(&produced.stdout).into_owned(),
        err: String::from_utf8_lossy(&produced.stderr).into_owned(),
        ok: produced.status.success(),
    }
}

/// Makes a directory link at `link` pointing at `target`.
///
/// Windows gets a junction, which needs no privilege; Unix gets a symbolic
/// link. Returns false when the platform refused to make one, which is a case
/// that cannot run rather than a case that passed.
///
/// @param link - where the link goes
/// @param target - what it points at
fn link_directory(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    {
        // **Every separator is a backslash before `cmd` sees it
        // (task-1913).** `mklink` is a `cmd` builtin, and `cmd` reads a
        // forward slash as the start of a switch: with one anywhere in the
        // path it answered `Invalid switch - "confinement\\link\\root\\escape"`
        // and exited 1. `area()` built its path from the single component
        // `_agent_output/confinement`, so every junction this file ever tried
        // to make failed, and the two cases that check a junction cannot widen
        // a confinement root have never run on Windows. They announced the
        // skip, so nothing lied - but `cargo test` captures the output of a
        // passing test, so the announcement was only ever visible under
        // `--strict`, where it became the failure it should have been.
        let backslashed = |path: &Path| path.to_string_lossy().replace('/', "\\");
        Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(backslashed(link))
            .arg(backslashed(target))
            .output()
            .map(|produced| produced.status.success())
            .unwrap_or(false)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

/// A junction below the root that points outside it does not widen the root.
///
/// This is the junction reproduction above, inverted into a test. The read
/// succeeded before this change: `root/escape/secret.rdb` normalises to a path
/// under `root`, and `starts_with` said yes while the file system opened a
/// database in a completely different directory.
#[test]
fn a_link_below_the_root_does_not_reach_outside_it() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("link");
    let root = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside);

    // A real database outside the root, with a row in it that a successful
    // escape would print. Created without a root, which is the unconfined use
    // of the same binary.
    let secret = outside.join("secret.rdb");
    let secret_name = secret.to_string_lossy().into_owned();
    let created = Command::new(&program)
        .args(["--db", &secret_name, "exec"])
        .arg("CREATE TABLE hidden (word TEXT)")
        .output()
        .expect("the command line runs");
    assert!(
        created.status.success(),
        "could not create the database outside the root: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    if !link_directory(&root.join("escape"), &outside) {
        inillucent_compat::differential::skipping(
            "confinement: this platform would not make a directory link",
        );
        return;
    }

    // **The escape route is real, and this proves it before asserting it is
    // shut.** Without the root, the same path text opens the same database
    // outside it - so the refusal below is the confinement working rather than
    // a junction the platform silently did not make.
    let through = root
        .join("escape/secret.rdb")
        .to_string_lossy()
        .into_owned();
    let reached = Command::new(&program)
        .args(["--db", &through, "query"])
        .arg("SELECT count(*) FROM hidden")
        .output()
        .expect("the command line runs");
    assert!(
        reached.status.success(),
        "the junction does not reach the database, so this case proves nothing: {}",
        String::from_utf8_lossy(&reached.stderr)
    );

    let refused = run(&program, &root, &["--db", "escape/secret.rdb", "query"])
        .text()
        .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "a junction below the root reached a database outside it: {refused}"
    );
}

/// A path whose text climbs out of the root is still refused.
///
/// The behaviour the lexical check already had, kept: replacing it with a
/// resolving one must not lose the case it was right about.
#[test]
fn a_path_that_climbs_out_of_the_root_is_refused() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("climb");
    let root = base.join("root");
    let _ = std::fs::create_dir_all(&root);
    let refused = run(&program, &root, &["--db", "../outside.rdb", "query"])
        .text()
        .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "a path that climbs out of the root was accepted: {refused}"
    );
}

/// An absolute path outside the root is refused.
#[test]
fn an_absolute_path_outside_the_root_is_refused() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("absolute");
    let root = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside);
    let named = outside.join("app.rdb").to_string_lossy().into_owned();
    let refused = run(&program, &root, &["--db", &named, "query"])
        .text()
        .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "an absolute path outside the root was accepted: {refused}"
    );
}

/// A path inside the root still works, which is what makes the refusals mean
/// something: a confinement that refused everything would pass every case
/// above and be useless.
#[test]
fn a_path_inside_the_root_is_admitted() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("inside");
    let root = base.join("root");
    let _ = std::fs::create_dir_all(root.join("nested"));
    let made = run(
        &program,
        &root,
        &[
            "--db",
            "nested/app.rdb",
            "exec",
            "CREATE TABLE t (a INTEGER)",
        ],
    );
    assert!(
        made.ok,
        "a path inside the root was refused: {}",
        made.text()
    );
    assert!(
        root.join("nested/app.rdb").is_file(),
        "the database was not created where the root says it should be"
    );
}

/// `ATTACH DATABASE` with an absolute path outside the root is refused.
///
/// The second of the two reproductions above. The SQL path never reached the
/// command surface's check, so a confined server could attach anything on the
/// disk and read it with a qualified name.
#[test]
fn attaching_a_database_outside_the_root_is_refused() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("attach");
    let root = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside);
    let secret = outside.join("secret.rdb");
    let secret_name = secret.to_string_lossy().into_owned();
    let created = Command::new(&program)
        .args(["--db", &secret_name, "exec"])
        .arg("CREATE TABLE hidden (word TEXT)")
        .output()
        .expect("the command line runs");
    assert!(created.status.success());

    let statement = format!(
        "ATTACH DATABASE '{}' AS stolen",
        secret_name.replace('\\', "/")
    );
    let refused = run(&program, &root, &["--db", "app.rdb", "exec", &statement])
        .text()
        .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "ATTACH reached a database outside the root: {refused}"
    );
    assert!(
        !secret.with_file_name("secret.rdb-wal").exists() || std::fs::metadata(&secret).is_ok(),
        "the refused attachment still touched the file"
    );
}

/// `ATTACH DATABASE` through a junction below the root is refused.
///
/// The two holes combined: a path whose text is inside the root, reached
/// through the statement that never saw the check.
#[test]
fn attaching_through_a_link_is_refused() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("attach-link");
    let root = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside);
    let secret = outside.join("secret.rdb");
    let secret_name = secret.to_string_lossy().into_owned();
    let created = Command::new(&program)
        .args(["--db", &secret_name, "exec"])
        .arg("CREATE TABLE hidden (word TEXT)")
        .output()
        .expect("the command line runs");
    assert!(created.status.success());

    if !link_directory(&root.join("escape"), &outside) {
        inillucent_compat::differential::skipping(
            "confinement: this platform would not make a directory link",
        );
        return;
    }
    let refused = run(
        &program,
        &root,
        &[
            "--db",
            "app.rdb",
            "exec",
            "ATTACH DATABASE 'escape/secret.rdb' AS stolen",
        ],
    )
    .text()
    .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "ATTACH through a junction reached a database outside the root: {refused}"
    );
}

/// `VACUUM INTO` cannot write outside the root.
///
/// The other statement that names a file of its own. It creates one rather
/// than reading one, which is the direction a confinement is usually written
/// to stop.
#[test]
fn vacuuming_into_a_path_outside_the_root_is_refused() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("vacuum");
    let root = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside);
    let target = outside.join("copy.rdb");
    let statement = format!(
        "VACUUM INTO '{}'",
        target.to_string_lossy().replace('\\', "/")
    );
    let made = run(
        &program,
        &root,
        &["--db", "app.rdb", "exec", "CREATE TABLE t (a INTEGER)"],
    );
    assert!(made.ok, "setup failed: {}", made.text());
    let refused = run(&program, &root, &["--db", "app.rdb", "exec", &statement]).text();
    assert!(
        refused.to_lowercase().contains("confined") || refused.to_lowercase().contains("outside"),
        "VACUUM INTO wrote outside the root: {refused}"
    );
    assert!(
        !target.exists(),
        "the refused VACUUM INTO still created the file"
    );
}

/// The `backup` command cannot write outside the root, and `restore` cannot
/// read from outside it.
#[test]
fn backup_and_restore_stay_inside_the_root() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("backup");
    let root = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside);
    let made = run(
        &program,
        &root,
        &["--db", "app.rdb", "exec", "CREATE TABLE t (a INTEGER)"],
    );
    assert!(made.ok, "setup failed: {}", made.text());

    let away = outside.join("copy.rdb").to_string_lossy().into_owned();
    let refused = run(&program, &root, &["--db", "app.rdb", "backup", &away])
        .text()
        .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "backup wrote outside the root: {refused}"
    );

    let refused = run(&program, &root, &["--db", "app.rdb", "restore", &away])
        .text()
        .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "restore read from outside the root: {refused}"
    );
}

/// `export` cannot write outside the root and `import` cannot read from
/// outside it.
#[test]
fn import_and_export_stay_inside_the_root() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("transfer");
    let root = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside);
    let made = run(
        &program,
        &root,
        &["--db", "app.rdb", "exec", "CREATE TABLE t (a INTEGER)"],
    );
    assert!(made.ok, "setup failed: {}", made.text());

    let away = outside.join("rows.csv");
    let away_name = away.to_string_lossy().into_owned();
    let refused = run(
        &program,
        &root,
        &["--db", "app.rdb", "export", "t", "--out", &away_name],
    )
    .text()
    .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "export wrote outside the root: {refused}"
    );
    assert!(!away.exists(), "the refused export still created the file");

    let _ = std::fs::write(&away, "a\n1\n");
    let refused = run(
        &program,
        &root,
        &["--db", "app.rdb", "import", &away_name, "--table", "t"],
    )
    .text()
    .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "import read from outside the root: {refused}"
    );
}

/// The root is applied before the database named on the command line is
/// opened.
///
/// The order used to be the other way round: the surface opened `--db` and
/// then stored the root, so the one path an operator is most likely to have
/// got wrong was the one path that was never checked.
#[test]
fn the_startup_database_is_confined_too() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("startup");
    let root = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside);
    let named = outside.join("startup.rdb").to_string_lossy().into_owned();
    let refused = run(
        &program,
        &root,
        &["--db", &named, "exec", "CREATE TABLE t (a INTEGER)"],
    )
    .text()
    .to_lowercase();
    assert!(
        refused.contains("confined") || refused.contains("outside"),
        "the startup database escaped the root: {refused}"
    );
    assert!(
        !outside.join("startup.rdb").exists(),
        "the refused startup database was created anyway"
    );
}

/// A root that names nothing is refused rather than confining to nothing.
#[test]
fn a_root_that_is_not_a_directory_is_refused() {
    let Some(program) = binary("inillucent") else {
        return;
    };
    let base = area("absent");
    let missing = base.join("not-there");
    let refused = run(&program, &missing, &["--db", ":memory:", "query"])
        .text()
        .to_lowercase();
    assert!(
        refused.contains("root"),
        "a root that is not a directory was accepted: {refused}"
    );
}

/// The MCP server refuses the same paths the command line does.
///
/// The two surfaces run one command table over one `Context`, and this is the
/// case that says so for the confinement rather than assuming it: `--root` is
/// documented on `inillucent-mcp` and that is the program an agent is handed.
#[test]
fn the_mcp_server_refuses_a_path_outside_the_root() {
    let Some(program) = binary("inillucent-mcp") else {
        return;
    };
    let base = area("mcp");
    let root = base.join("root");
    let outside = base.join("outside");
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(&outside);
    let named = outside
        .join("secret.rdb")
        .to_string_lossy()
        .replace('\\', "/");

    // **The handshake, in full.** task-1909 made the server demand
    // `protocolVersion` on `initialize` and refuse every other method until
    // `notifications/initialized` has followed it. A test that sends the old
    // two-line shape gets `initialization must complete` back, which contains
    // neither "confined" nor "outside" - so it reported that the server had
    // read a database outside its root when the server had not read one at
    // all. A confinement check that fails for the wrong reason is the same
    // defect as one that passes for the wrong reason.
    let request = format!(
        "{}\n{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"confinement"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        format_args!(
            "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"inillucent_query\",\"arguments\":{{\"db\":\"{named}\",\"sql\":\"SELECT 1\"}}}}}}"
        )
    );
    let mut child = match Command::new(&program)
        .arg("--root")
        .arg(&root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => panic!("could not start the MCP server: {error}"),
    };
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("the server takes standard input");
        let _ = stdin.write_all(request.as_bytes());
    }
    let produced = child.wait_with_output().expect("the server ends");
    let answered = String::from_utf8_lossy(&produced.stdout).to_lowercase();
    assert!(
        answered.contains("confined") || answered.contains("outside"),
        "the MCP server read a database outside the root: {answered}"
    );
}
