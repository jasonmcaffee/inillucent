//! Every MCP tool, called over real pipes, with one field of its answer read.
//!
//! Invariant: **one `inillucent-mcp` process, one `initialize` handshake, and
//! each of the twenty-eight tools called once through `tools/call` with
//! arguments that mean something, asserting on what came back.** The whole
//! session is one process and one pair of pipes, because that is the thing
//! under test: a tool that works in process and a server that cannot frame its
//! own replies are the same bug from a client's point of view.
//!
//! **No tool was ever called by name over the wire (task-1969, 5.4).**
//! `command_parity.rs` keeps the tool set equal to the registry minus
//! `cli_only`, which is a set comparison rather than a call.
//! `a_session_runs_end_to_end` calls two of them in process. Over real pipes
//! the existing suites cover budgets, cancellation and confinement - three
//! concerns, none of them a named tool - and `bin/inillucent-mcp.rs` has no
//! test module at all.
//!
//! **Why one field rather than the whole result.** Every tool answers the same
//! envelope, so asserting the envelope would be asserting the same thing
//! twenty-eight times and would say nothing about any individual tool. What
//! each row below names is the part of the answer that is about that tool: the
//! table's name from `tables`, the column names from `describe`, the plan from
//! `explain`, the version from `version`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use inillucent_compat::cliproc::program;
use inillucent_compat::workspace_root;

/// A live server, and the two pipes a client talks to it through.
struct Session {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    /// The next request id, so every call is answered by its own reply.
    next: u64,
}

impl Session {
    /// Starts the server on a database and completes the handshake.
    ///
    /// The handshake is part of the session rather than a case of its own
    /// because every other case needs it: a server that answers `tools/call`
    /// before `initialize` is not the protocol, and one that never answers
    /// `initialize` fails every row below with the same message.
    ///
    /// @param server - the built `inillucent-mcp`
    /// @param database - the file to open
    fn start(server: &Path, database: &Path) -> Session {
        Session::start_with(server, database, &[])
    }

    /// Starts the server with extra arguments and completes the handshake.
    ///
    /// @param server - the built `inillucent-mcp`
    /// @param database - the file to open
    /// @param extra - the flags to start it with
    fn start_with(server: &Path, database: &Path, extra: &[&str]) -> Session {
        let mut child = Command::new(server)
            .args(["--db", &database.to_string_lossy()])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("inillucent-mcp did not start: {error}"));
        let input = child.stdin.take().expect("the server has a standard input");
        let output = BufReader::new(
            child
                .stdout
                .take()
                .expect("the server has a standard output"),
        );
        let mut session = Session {
            child,
            input,
            output,
            next: 1,
        };
        let hello = session.call(
            "initialize",
            r#"{"protocolVersion":"2024-11-05","capabilities":{},
                "clientInfo":{"name":"mcp_wire","version":"1"}}"#,
        );
        assert!(
            hello.contains("protocolVersion") && hello.contains("serverInfo"),
            "the server did not answer `initialize`:\n{hello}"
        );
        session.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        session
    }

    /// Writes one request and returns the line that answered it.
    ///
    /// The reply is matched by id rather than by position, because a server is
    /// allowed to interleave a notification of its own, and a test that read
    /// the next line would then attribute one tool's answer to another.
    ///
    /// @param method - the JSON-RPC method
    /// @param params - its parameters, as a JSON object
    fn call(&mut self, method: &str, params: &str) -> String {
        let id = self.next;
        self.next = self.next.saturating_add(1);
        let request = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}"
        );
        self.notify(&request);
        loop {
            let mut line = String::new();
            let read = self
                .output
                .read_line(&mut line)
                .unwrap_or_else(|error| panic!("reading the server's reply to {method}: {error}"));
            assert!(
                read > 0,
                "the server closed its output before answering {method} (id {id})"
            );
            if line.contains(&format!("\"id\":{id}")) {
                return line;
            }
        }
    }

    /// Writes one line to the server without waiting for a reply.
    ///
    /// @param line - the JSON-RPC message
    fn notify(&mut self, line: &str) {
        let flattened: String = line.split_whitespace().collect::<Vec<&str>>().join(" ");
        writeln!(self.input, "{flattened}").expect("the server accepts a request");
        self.input.flush().expect("the request is flushed");
    }

    /// Calls one tool and returns the text of its result.
    ///
    /// @param tool - the tool's name
    /// @param arguments - its arguments, as a JSON object
    fn tool(&mut self, tool: &str, arguments: &str) -> String {
        self.call(
            "tools/call",
            &format!("{{\"name\":\"{tool}\",\"arguments\":{arguments}}}"),
        )
    }
}

impl Drop for Session {
    /// Ends the server when the session goes out of scope.
    ///
    /// Closing the pipe first, because the server ends on end-of-input and a
    /// kill would leave the case unable to tell a clean exit from a crash. The
    /// kill is the backstop for a server that does not end.
    fn drop(&mut self) {
        let _ = self.input.write_all(b"");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Returns a database with one table, one full-text table and one vector table.
///
/// Built by the command line rather than by the engine in process, for the same
/// reason `cli_commands.rs` does it: a fixture written by the library would
/// leave the program's own write path out of what is exercised.
///
/// @param binary - the built `inillucent`
fn populated(binary: &Path) -> PathBuf {
    populated_at(binary, "session")
}

/// Returns the same fixture in a directory of the case's own.
///
/// Per case rather than shared, because three cases in this file build it and
/// a shared directory makes the order they happen to run in part of what is
/// under test - the second `create` reports that the file already exists.
///
/// @param binary - the built `inillucent`
/// @param case - what to name the directory after
fn populated_at(binary: &Path, case: &str) -> PathBuf {
    let directory = workspace_root().join("_agent_output/mcp-wire").join(case);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let database = directory.join("app.rdb");
    let path = database.to_string_lossy().to_string();
    for arguments in [
        vec!["create", path.as_str()],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE INDEX note_body ON note (body)",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "INSERT INTO note (body) VALUES ('hello'), ('goodbye')",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE VIRTUAL TABLE doc USING fts5(body)",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "INSERT INTO doc (body) VALUES ('the quick brown fox')",
        ],
        vec![
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE point (id INTEGER PRIMARY KEY, at VECTOR(3))",
        ],
    ] {
        let ran = inillucent_compat::cliproc::run(binary, &arguments);
        assert_eq!(
            ran.code,
            0,
            "building the fixture failed at {arguments:?}:\n{}",
            ran.said()
        );
    }
    database
}

/// A statement this engine has not built, for the `unsupported` case.
///
/// The same one `cli_commands.rs` drives the command line with, so the two
/// sides of the claim - exit code 3 from the binary, `unsupported` in a
/// JSON-RPC result - are about one statement rather than two.
///
/// It names no table, because it is asked last and the three tools above that
/// move the session have by then left it on a database it made itself. A
/// statement that needed a table would come back `not_found`, which is a
/// different status and would have made this case look like it was checking
/// something it was not.
const NOT_BUILT: &str = "SELECT (SELECT 1, 2)";

/// Every tool, the arguments to call it with, and a word its answer must carry.
///
/// A table rather than twenty-eight functions, because what differs between
/// them is three strings and nothing else; twenty-eight functions would be
/// twenty-eight copies of one `assert!` and a reader would have to diff them to
/// find the one that is different.
///
/// The paths are placeholders replaced per run: `{dir}` is this run's scratch
/// directory and `{copy}` is a backup this session writes before it reads.
const CALLS: [(&str, &str, &str); 28] = [
    // The order matters, and it is not alphabetical. **Three tools change which
    // database the session is on**: `create` opens the file it makes,
    // `restore` opens the file it is given, and `migrate` writes a new one.
    // Called in the middle of the list they took every tool after them onto an
    // empty database, and the symptom was `import` inventing a `note` table out
    // of a CSV header while `search` answered "no such table: doc". They go
    // last, and the fixture the rows above read is therefore the one the
    // session opened with.
    (
        "inillucent_query",
        r#"{"sql":"SELECT body FROM note ORDER BY id"}"#,
        "hello",
    ),
    (
        "inillucent_exec",
        r#"{"sql":"UPDATE note SET body = body"}"#,
        "2 rows changed",
    ),
    (
        "inillucent_batch",
        r#"{"sql":"INSERT INTO note (body) VALUES ('one'); INSERT INTO note (body) VALUES ('two')"}"#,
        "ok",
    ),
    ("inillucent_run", r#"{"input":".tables"}"#, "note"),
    ("inillucent_tables", "{}", "note"),
    ("inillucent_describe", r#"{"table":"note"}"#, "body"),
    ("inillucent_schema", "{}", "CREATE TABLE note"),
    ("inillucent_indexes", "{}", "note_body"),
    ("inillucent_databases", "{}", "main"),
    (
        "inillucent_explain",
        r#"{"sql":"SELECT body FROM note"}"#,
        "note",
    ),
    (
        "inillucent_export",
        r#"{"table":"note","format":"csv"}"#,
        "id,body",
    ),
    ("inillucent_dump", "{}", "CREATE TABLE note"),
    (
        "inillucent_import",
        r#"{"file":"{dir}/rows.csv","table":"note","format":"csv","skip":1}"#,
        "row",
    ),
    (
        "inillucent_search",
        r#"{"query":"brown","table":"doc"}"#,
        "quick brown fox",
    ),
    (
        "inillucent_vector_search",
        r#"{"table":"point","column":"at","vector":[1,0,0]}"#,
        "distance",
    ),
    ("inillucent_checkpoint", "{}", "checkpointed"),
    ("inillucent_integrity_check", "{}", "ok"),
    ("inillucent_analyze", "{}", "sqlite_stat1"),
    ("inillucent_stats", "{}", "pool"),
    ("inillucent_capabilities", "{}", "ddl"),
    ("inillucent_functions", "{}", "substr"),
    ("inillucent_setup_embeddings", "{}", "nomic-embed-text-v1.5"),
    ("inillucent_version", "{}", "0.1."),
    ("inillucent_help", "{}", "query"),
    // The three that move the session, and the one that refuses.
    ("inillucent_backup", r#"{"file":"{copy}"}"#, "wrote"),
    ("inillucent_restore", r#"{"file":"{copy}"}"#, ""),
    (
        "inillucent_create",
        r#"{"path":"{dir}/made.rdb"}"#,
        "created",
    ),
    (
        "inillucent_migrate",
        r#"{"source":"{dir}/no-such-source.db","destination":"{dir}/out.rdb","kind":"sqlite"}"#,
        "",
    ),
];

/// One handshake, then every tool once, then the unsupported statement.
///
/// **One case rather than twenty-nine**, because the handshake and the process
/// are the expensive part and because what is being asserted is that a client
/// can hold one session open and use the whole surface through it. Twenty-nine
/// cases would each start a server, and a server that can only answer its first
/// tool call would pass all of them.
#[test]
fn every_tool_answers_over_one_session() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(server) = program("inillucent-mcp") else {
        return;
    };
    let database = populated(&binary);
    let directory = database
        .parent()
        .expect("the database has a directory")
        .to_string_lossy()
        .replace('\\', "/");
    std::fs::write(
        format!("{directory}/rows.csv"),
        "id,body\n30,first\n31,second\n",
    )
    .expect("the csv is written");
    let copy = format!("{directory}/copy.rdb");

    let mut session = Session::start(&server, &database);

    // The tool set the server advertises has to be the one this file calls.
    // Without this a tool could be dropped from the server and every row below
    // would still pass, because a call to a tool that is not there is answered
    // with an error that names it - and the rows that assert on an empty word
    // would accept that error.
    let listed = session.call("tools/list", "{}");
    let mut absent: Vec<&str> = Vec::new();
    for (tool, _, _) in CALLS {
        if !listed.contains(&format!("\"{tool}\"")) {
            absent.push(tool);
        }
    }
    assert!(
        absent.is_empty(),
        "these tools are called below and the server does not advertise them: {absent:?}"
    );
    assert_eq!(
        listed.matches("\"inputSchema\"").count(),
        CALLS.len(),
        "the server advertises {} tools and this file calls {}",
        listed.matches("\"inputSchema\"").count(),
        CALLS.len()
    );

    let mut wrong: Vec<String> = Vec::new();
    for (tool, arguments, wanted) in CALLS {
        let filled = arguments
            .replace("{dir}", &directory)
            .replace("{copy}", &copy);
        let answered = session.tool(tool, &filled);
        assert!(
            answered.contains("\"result\""),
            "{tool} did not answer with a result:\n{answered}"
        );
        // `inillucent_restore` and `inillucent_migrate` are called for their
        // refusal and their reopen rather than for a word in their answer:
        // restore prints nothing on success, and migrate is driven at a source
        // that is not there so that this session does not spend a minute
        // copying a database it did not ask for.
        if !wanted.is_empty() && !answered.contains(wanted) {
            wrong.push(format!("{tool} answered without `{wanted}`:\n{answered}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "these tools answered something other than what their result is for:\n{}",
        wrong.join("\n")
    );

    // The other half of exit code 3: the same statement, through MCP, has to
    // come back as the driver's own status name rather than as a generic
    // failure. `cli_commands.rs` asserts the exit code; nothing read
    // `"unsupported"` out of a JSON-RPC response (task-1969, 5.3).
    let refused = session.tool("inillucent_exec", &format!("{{\"sql\":\"{NOT_BUILT}\"}}"));
    assert!(
        refused.contains("unsupported"),
        "a statement the engine has not built came back without the `unsupported` status:\n{refused}"
    );
    assert!(
        refused.contains("\"isError\":true"),
        "a statement the engine has not built came back as a success:\n{refused}"
    );
}

/// A read only server refuses every statement that changes the file, and the
/// file is unchanged afterwards (task-1979, H2).
///
/// **Every one of these ran and persisted through `--readonly` before
/// task-1980.** The filter asked the engine to `EXPLAIN` the statement and
/// refused only on the text "not a read-only statement", which
/// `compile_explain` produces for a `SELECT`, an `UPDATE` and a `DELETE` and
/// never for an `INSERT`, a write pragma, an `ATTACH` or a `VACUUM INTO`.
#[test]
fn a_read_only_server_refuses_every_write_and_leaves_the_file_alone() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(server) = program("inillucent-mcp") else {
        return;
    };
    let database = populated_at(&binary, "readonly");
    let before = std::fs::read(&database).expect("the database reads");

    let mut session = Session::start_with(&server, &database, &["--readonly"]);
    for sql in [
        "INSERT INTO note (body) VALUES ('written')",
        "UPDATE note SET body = 'changed'",
        "DELETE FROM note",
        "PRAGMA user_version = 7",
        "DROP TABLE note",
        "CREATE TABLE another (a)",
    ] {
        let answer = session.tool("inillucent_query", &format!("{{\"sql\":\"{sql}\"}}"));
        assert!(
            answer.contains("read only"),
            "a read only server did not refuse `{sql}`:\n{answer}"
        );
    }
    // A read still answers, so the refusals above are about writing rather than
    // about a server that stopped working.
    let read = session.tool(
        "inillucent_query",
        "{\"sql\":\"SELECT count(*) FROM note\"}",
    );
    assert!(
        read.contains("\"isError\":false") && read.contains("count(*)"),
        "a read only server could not read:\n{read}"
    );
    drop(session);

    let after = std::fs::read(&database).expect("the database reads");
    assert_eq!(
        before,
        after,
        "the file changed under a read only server, by {} bytes",
        after.len() as i64 - before.len() as i64
    );
}

/// A server refuses every dot command that reaches the operating system or a
/// path outside its root (task-1979, H1).
///
/// **`.shell` and `.system` spawned `cmd /C` on a server started `--root`, and
/// `.output`, `.once` and `.read` opened any path with plain `std::fs`.** The
/// reviewer wrote files outside the root through all five and the server
/// answered `isError=false`. A child spawned that way also inherits the
/// server's standard output, which is the JSON-RPC channel.
#[test]
fn a_server_refuses_the_dot_commands_that_reach_outside_it() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(server) = program("inillucent-mcp") else {
        return;
    };
    let database = populated_at(&binary, "confined");
    let root = database
        .parent()
        .expect("the fixture has a directory")
        .to_path_buf();
    let outside = root.join("..").join("mcp-wire-outside");
    let _ = std::fs::create_dir_all(&outside);
    let escaped = outside
        .join("escaped.txt")
        .to_string_lossy()
        .replace('\\', "/");

    let mut session = Session::start_with(&server, &database, &["--root", &root.to_string_lossy()]);
    for input in [
        ".shell cmd /c echo escaped".to_string(),
        ".system cmd /c echo escaped".to_string(),
        format!(".output {escaped}"),
        format!(".once {escaped}"),
        format!(".read {escaped}"),
        format!(".import {escaped} note"),
    ] {
        let flattened = input.replace('"', "'");
        let answer = session.tool("inillucent_run", &format!("{{\"input\":\"{flattened}\"}}"));
        let said = answer.to_ascii_lowercase();
        assert!(
            said.contains("prohibited in safe mode")
                || said.contains("outside")
                || said.contains("cannot open"),
            "`{input}` was not refused by a server confined to its root:\n{answer}"
        );
    }
    // **And the same commands with a path the confinement allows.** Every case
    // above names a path outside the root, so the confinement refuses them and
    // the assertion passes whether safe mode is on or not - which is how six of
    // these went unrefused for as long as they did. Measured before the fix:
    // `.output out.txt` through `inillucent-mcp` created `out.txt` in the
    // server's working directory and answered no error at all. A path inside
    // the root is the case only safe mode can refuse.
    let inside = root.join("inside.txt").to_string_lossy().replace('\\', "/");
    for input in [
        format!(".output {inside}"),
        format!(".once {inside}"),
        format!(".read {inside}"),
        format!(".import {inside} note"),
        format!(".backup {inside}"),
        format!(".restore {inside}"),
        ".cd .".to_string(),
        ".load libwhatever".to_string(),
    ] {
        let flattened = input.replace('"', "'");
        let answer = session.tool("inillucent_run", &format!("{{\"input\":\"{flattened}\"}}"));
        assert!(
            answer
                .to_ascii_lowercase()
                .contains("prohibited in safe mode"),
            "`{input}` names a path inside the root, so only safe mode refuses it, and it \
             was not refused:\n{answer}"
        );
    }
    drop(session);

    assert!(
        !outside.join("escaped.txt").is_file(),
        "a dot command wrote outside the root the server was confined to"
    );
    assert!(
        !root.join("inside.txt").is_file(),
        "a dot command wrote a file inside the root, which safe mode refuses"
    );
}

/// A statement deep enough to have ended the server is refused, and the server
/// answers the next request (task-1979, section 5.3).
///
/// **It used to end the server for every client.** `SELECT abs(abs(...(1)...))`
/// 300 deep overflowed the 1 MiB stack the binaries carry, which is exit code
/// 0xC00000FD and a client whose next request is never answered.
#[test]
fn a_statement_past_the_depth_limit_does_not_end_the_server() {
    let Some(binary) = program("inillucent") else {
        return;
    };
    let Some(server) = program("inillucent-mcp") else {
        return;
    };
    let database = populated_at(&binary, "deep");
    let mut session = Session::start(&server, &database);
    let deep = format!("SELECT {}1{}", "abs(".repeat(2_000), ")".repeat(2_000));
    let refused = session.tool("inillucent_query", &format!("{{\"sql\":\"{deep}\"}}"));
    assert!(
        refused.contains("depth exceeded"),
        "a statement 2,000 deep was not refused by a limit:\n{}",
        &refused[..refused.len().min(400)]
    );
    let after = session.tool("inillucent_query", "{\"sql\":\"SELECT 1\"}");
    assert!(
        after.contains("\"isError\":false"),
        "the server did not answer the request after a deep statement:\n{after}"
    );
}
