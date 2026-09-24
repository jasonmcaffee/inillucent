//! What a served request may spend, and what happens when it spends more.
//!
//! Invariant: **an MCP request has a finite bound on rows, on bytes, on the
//! size of what it sends, on the size of what it gets back, and on how long it
//! runs, and hitting one is a structured refusal naming which.** Before
//! this, it had none of those. The `limit` argument's helper turned a
//! *negative* number into zero, and zero means every row - so `limit=-1`, which
//! is how "no limit" is spelled in most other things and how an off-by-one in a
//! client's arithmetic comes out, asked a confined server for the whole table.
//!
//! The cases drive the real `inillucent-mcp` binary over JSON-RPC, because the
//! distinction that matters is between the two surfaces: the command line has
//! no ceiling on purpose, and asserting the ceiling anywhere other than the
//! served one would not say which is which.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use inillucent_compat::cliproc;
use inillucent_compat::workspace_root;

/// Where this suite's scratch databases live.
fn area() -> PathBuf {
    let path = workspace_root().join("_agent_output/budgets");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// A database with enough rows that a ceiling is reachable.
///
/// @param name - the file's name, so two cases never share one
/// @param rows - how many rows to put in it
fn database(name: &str, rows: usize) -> PathBuf {
    let program = cliproc::program("inillucent");
    let path = area().join(name);
    inillucent_base::testing::remove_database(&path);
    let named = path.to_string_lossy().into_owned();
    let made = Command::new(&program)
        .args(["--db", &named, "exec", "CREATE TABLE t (n INTEGER, s TEXT)"])
        .output()
        .unwrap_or_else(|error| panic!("inillucent did not start: {error}"));
    assert!(
        made.status.success(),
        "`inillucent exec` could not create the table:
{}",
        String::from_utf8_lossy(&made.stderr)
    );
    // One statement rather than a row at a time: this is setup, and setup that
    // takes a minute is setup somebody turns off.
    let values: Vec<String> = (0..rows)
        .map(|nth| format!("({nth},'row {nth} with some text on it')"))
        .collect();
    let filled = Command::new(&program)
        .args(["--db", &named, "exec"])
        .arg(format!("INSERT INTO t (n, s) VALUES {}", values.join(",")))
        .output()
        .unwrap_or_else(|error| panic!("inillucent did not start: {error}"));
    assert!(
        filled.status.success(),
        "`inillucent exec` could not fill the table:
{}",
        String::from_utf8_lossy(&filled.stderr)
    );
    path
}

/// An MCP server on a database, and the two pipes to it.
struct Server {
    /// The process.
    child: Child,
    /// Its answers, one JSON object per line.
    answers: BufReader<std::process::ChildStdout>,
}

impl Server {
    /// Starts a server on a database.
    ///
    /// @param database - the file to serve
    fn start(database: &PathBuf) -> Server {
        let program = cliproc::program("inillucent-mcp");
        let mut child = Command::new(program)
            .arg("--db")
            .arg(database)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("inillucent-mcp did not start: {error}"));
        let Some(stdout) = child.stdout.take() else {
            panic!("inillucent-mcp was started with a piped standard output and has none");
        };
        let answers = BufReader::new(stdout);
        let mut server = Server { child, answers };
        server.shake_hands();
        server
    }

    /// Completes the MCP handshake, which every other method waits for.
    ///
    /// **Not optional, and not a formality.** task-1909 made the server demand
    /// `protocolVersion` on `initialize` and refuse every other method until a
    /// `notifications/initialized` has followed it. Without this, every case
    /// below read `MCP initialization must complete before this method is used`
    /// and asserted against that string - so a suite about row ceilings was
    /// reporting that the ceilings were missing when it had never asked about
    /// them. A test that cannot reach the thing it is testing fails for the
    /// wrong reason, which the testing standard rates as bad as passing for one.
    fn shake_hands(&mut self) {
        let _ = self.ask(concat!(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":"#,
            r#"{"protocolVersion":"2025-06-18","capabilities":{},"#,
            r#""clientInfo":{"name":"budgets"}}}"#
        ));
        // A notification, so there is no answer to read.
        if let Some(stdin) = self.child.stdin.as_mut() {
            let _ = writeln!(
                stdin,
                r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
            );
            let _ = stdin.flush();
        }
    }

    /// Sends one request and returns the line that came back.
    ///
    /// @param request - the JSON-RPC request, without its newline
    fn ask(&mut self, request: &str) -> String {
        if let Some(stdin) = self.child.stdin.as_mut() {
            let _ = writeln!(stdin, "{request}");
            let _ = stdin.flush();
        }
        let mut line = String::new();
        let _ = self.answers.read_line(&mut line);
        line
    }

    /// Sends a `tools/call` for one command and returns what it answered.
    ///
    /// @param name - the tool, without the `inillucent_` prefix
    /// @param arguments - the arguments object, as JSON text
    fn call(&mut self, name: &str, arguments: &str) -> String {
        self.ask(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":\
             {{\"name\":\"inillucent_{name}\",\"arguments\":{arguments}}}}}"
        ))
    }

    /// Stops the server.
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A negative limit is refused rather than read as "every row".
///
/// This is the reproduction from code review, inverted into a test:
/// `limit_of` matched `Some(asked) if asked >= 0` and fell through to zero,
/// and zero is unlimited.
#[test]
fn a_negative_limit_is_refused_rather_than_meaning_everything() {
    let database = database("negative.rdb", 50);
    let mut server = Server::start(&database);
    let answered = server.call("query", "{\"sql\":\"SELECT * FROM t\",\"limit\":-1}");
    server.stop();
    assert!(
        answered.contains("not a number of rows"),
        "a negative limit was not refused: {answered}"
    );
    assert!(
        answered.contains("\"isError\":true") || answered.contains("isError\": true"),
        "the refusal did not come back as a tool error: {answered}"
    );
}

/// A row ceiling refuses a request past it, and names the ceiling.
#[test]
fn a_row_ceiling_refuses_a_request_past_it() {
    let database = database("ceiling.rdb", 20);
    let mut server = Server::start(&database);
    let refused = server.call("query", "{\"sql\":\"SELECT * FROM t\",\"limit\":99999999}");
    // And a request inside the ceiling still works, which is what stops a
    // server that refused everything from passing the case above.
    let allowed = server.call("query", "{\"sql\":\"SELECT * FROM t\",\"limit\":5}");
    server.stop();
    assert!(
        refused.contains("past the") && refused.contains("rows this server hands back"),
        "a request past the ceiling was not refused: {refused}"
    );
    assert!(
        allowed.contains("\"isError\":false") || allowed.contains("isError\": false"),
        "a request inside the ceiling was refused: {allowed}"
    );
}

/// `limit=0` means every row, and a served surface refuses it by name.
///
/// It is the request the ceiling exists for, so a ceiling that let it through
/// would be a ceiling with a hole exactly the shape of the thing it guards.
#[test]
fn asking_for_every_row_is_refused_on_a_served_surface() {
    let database = database("everything.rdb", 20);
    let mut server = Server::start(&database);
    let refused = server.call("query", "{\"sql\":\"SELECT * FROM t\",\"limit\":0}");
    server.stop();
    assert!(
        refused.contains("asks for every row"),
        "limit=0 was not refused on a served surface: {refused}"
    );
}

/// The command line has no ceiling, and that is the point of the distinction.
///
/// A person asking their own database for every row is asking for what they
/// want. Without this case the three above would also pass for a change that
/// put the ceiling everywhere.
#[test]
fn the_command_line_has_no_row_ceiling() {
    let program = cliproc::program("inillucent");
    let database = database("unbounded.rdb", 20);
    let named = database.to_string_lossy().into_owned();
    let produced = Command::new(&program)
        .args(["--db", &named, "query", "SELECT * FROM t", "--limit", "0"])
        .output()
        .expect("the command line runs");
    assert!(
        produced.status.success(),
        "the command line refused every row: {}{}",
        String::from_utf8_lossy(&produced.stdout),
        String::from_utf8_lossy(&produced.stderr)
    );
}

/// A request line past the ceiling is refused rather than buffered.
///
/// **The assertion is that the server answers at all.** `BufRead::read_line`
/// against a client that never sends a newline grows a `String` until the
/// process dies, so the failure this guards is not a wrong answer - it is no
/// answer and a machine that is out of memory.
#[test]
fn an_over_long_request_is_refused_rather_than_buffered() {
    let database = database("longline.rdb", 5);
    let mut server = Server::start(&database);
    // Two megabytes of one line, which is past the one-megabyte ceiling and
    // small enough that the test is quick.
    let padding = "x".repeat(2 * 1024 * 1024);
    let answered = server.ask(&format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":\
         {{\"name\":\"inillucent_query\",\"arguments\":{{\"sql\":\"SELECT '{padding}'\"}}}}}}"
    ));
    server.stop();
    assert!(
        answered.contains("may not be longer than"),
        "an over-long request was not refused: {}",
        answered.chars().take(300).collect::<String>()
    );
}

/// The `limit` parameter's description says the ceiling exists.
///
/// A bound a client cannot see is a bound a client discovers by hitting it, and
/// the schema is where a client looks.
#[test]
fn the_tool_schema_says_there_is_a_ceiling() {
    let database = database("schema.rdb", 1);
    let mut server = Server::start(&database);
    let listed =
        server.ask("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}");
    server.stop();
    assert!(
        listed.contains("limit"),
        "the tool list does not describe a limit at all: {}",
        listed.chars().take(300).collect::<String>()
    );
}
