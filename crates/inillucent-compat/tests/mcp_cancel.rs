//! Stopping a tool call that is already running.
//!
//! Invariant: **a client can stop a call it has started, and the server says so
//! rather than answering late.** MCP defines `notifications/cancelled` for
//! exactly this, and a server that cannot read a message while it is answering
//! one cannot act on it: the notification sits in the pipe until the call it is
//! about has finished, at which point cancelling it means nothing.
//!
//! ## The defect (task-1932, H11)
//!
//! `mcp::serve` read one line, answered it, and only then read again. Nothing
//! in the process held the `AtomicBool` the executor polls - `command::run`
//! armed a fresh one per call and dropped it - so there was no flag for a
//! cancellation to set even if one had been read. A `tools/call` running a scan
//! of a large table held the server for its whole sixty second deadline, and
//! the driver's own `Connection::cancel` was correct and unreachable from every
//! front end.
//!
//! Both halves are asserted here, because either alone passes for the wrong
//! reason: a server that reads the notification and has nothing to set answers
//! late, and a server with a shared flag nobody can reach answers late too.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use inillucent_compat::workspace_root;

/// Where this suite's scratch databases live.
fn area() -> PathBuf {
    let path = workspace_root().join("_agent_output/mcp-cancel");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns one of the shipped binaries, building them first.
///
/// @param name - which binary
fn binary(name: &str) -> Option<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args(["build", "-p", "inillucent-cli"])
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let mut directory = std::env::current_exe().unwrap_or_default();
    directory.pop();
    directory.pop();
    let path = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// How many rows the table holds.
///
/// The slow statement is a three way cross join over it, so the work is the
/// cube of this: 150 rows is 3.4 million combinations, which takes this engine
/// a few seconds and is far short of the sixty second deadline a served request
/// runs under. The gap between "a few seconds" and "sixty" is what makes the
/// timing assertion below mean something, and the number is kept small because
/// the uncancelled arm has to run the whole thing to measure it.
const ROWS: usize = 150;

/// A database with enough rows that a statement over it takes a while.
///
/// @param name - the file's name
fn database(name: &str) -> Option<PathBuf> {
    let program = binary("inillucent")?;
    let path = area().join(name);
    for suffix in ["", "-wal", "-journal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let named = path.to_string_lossy().into_owned();
    let made = Command::new(&program)
        .args(["--db", &named, "exec", "CREATE TABLE t (n INTEGER)"])
        .output()
        .ok()?;
    if !made.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&made.stderr));
        return None;
    }
    let values: Vec<String> = (0..ROWS).map(|nth| format!("({nth})")).collect();
    let filled = Command::new(&program)
        .args(["--db", &named, "exec"])
        .arg(format!("INSERT INTO t (n) VALUES {}", values.join(",")))
        .output()
        .ok()?;
    if !filled.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&filled.stderr));
        return None;
    }
    Some(path)
}

/// The statement the cases cancel.
///
/// Counted rather than selected, so the answer is one row: this is about how
/// long the server is *held*, and a statement that also produced a large answer
/// would be stopped by the row ceiling instead and prove nothing about
/// cancellation.
const SLOW: &str = "SELECT count(*) FROM t AS a, t AS b, t AS c";

/// An MCP server on a database, and the two pipes to it.
struct Server {
    /// The process.
    child: Child,
    /// Its answers, one JSON object per line.
    answers: BufReader<std::process::ChildStdout>,
}

impl Server {
    /// Starts a server on a database and completes the handshake.
    ///
    /// @param database - the file to serve
    fn start(database: &PathBuf) -> Option<Server> {
        let program = binary("inillucent-mcp")?;
        let mut child = Command::new(program)
            .arg("--db")
            .arg(database)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let answers = BufReader::new(child.stdout.take()?);
        let mut server = Server { child, answers };
        let _ = server.ask(concat!(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":"#,
            r#"{"protocolVersion":"2025-06-18","capabilities":{},"#,
            r#""clientInfo":{"name":"mcp_cancel"}}}"#
        ));
        server.tell(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        Some(server)
    }

    /// Writes one line and does not wait for an answer.
    ///
    /// @param line - the JSON-RPC message, without its newline
    fn tell(&mut self, line: &str) {
        if let Some(stdin) = self.child.stdin.as_mut() {
            let _ = writeln!(stdin, "{line}");
            let _ = stdin.flush();
        }
    }

    /// Sends one request and returns the line that came back.
    ///
    /// @param request - the JSON-RPC request, without its newline
    fn ask(&mut self, request: &str) -> String {
        self.tell(request);
        let mut line = String::new();
        let _ = self.answers.read_line(&mut line);
        line
    }

    /// Returns the next answer without sending anything.
    fn listen(&mut self) -> String {
        let mut line = String::new();
        let _ = self.answers.read_line(&mut line);
        line
    }

    /// Stops the server.
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Returns a `tools/call` for the slow query, under one id.
///
/// @param id - the request id
fn slow_call(id: u32) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":\
         {{\"name\":\"inillucent_query\",\"arguments\":{{\"sql\":\"{SLOW}\"}}}}}}"
    )
}

/// A cancellation notification naming one request id.
///
/// @param id - the request to cancel
fn cancellation(id: u32) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":\
         {{\"requestId\":{id},\"reason\":\"the client changed its mind\"}}}}"
    )
}

/// How long the slow statement takes when nobody stops it.
///
/// Measured rather than assumed: the timing assertions below compare against
/// this, so a faster machine tightens them rather than making them vacuous.
fn uncancelled_seconds(server: &mut Server) -> f64 {
    let started = Instant::now();
    let answered = server.ask(&slow_call(1));
    let elapsed = started.elapsed().as_secs_f64();
    assert!(
        !answered.contains("cancelled"),
        "the uncancelled run reported a cancellation: {answered}"
    );
    elapsed
}

/// A call that is already running is stopped by a cancellation.
///
/// The two lines are written in one go, which is how a client that changes its
/// mind sends them and is the ordering that used to be lost entirely: the
/// server could not read the second while it was answering the first.
#[test]
fn a_running_tool_call_is_stopped_by_a_cancellation() {
    let Some(database) = database("running.rdb") else {
        inillucent_compat::differential::skipping("mcp_cancel: the command line did not build");
        return;
    };
    let Some(mut server) = Server::start(&database) else {
        inillucent_compat::differential::skipping("mcp_cancel: the MCP server did not start");
        return;
    };

    let whole = uncancelled_seconds(&mut server);
    assert!(
        whole > 0.5,
        "the slow statement finished in {whole:.2}s, which is too fast for a cancellation to \
         be observably earlier - raise ROWS"
    );

    let started = Instant::now();
    server.tell(&slow_call(2));
    server.tell(&cancellation(2));
    let answered = server.listen();
    let elapsed = started.elapsed().as_secs_f64();
    server.stop();

    assert!(
        answered.contains("cancelled"),
        "a cancelled call answered with something else after {elapsed:.2}s: {answered}"
    );
    assert!(
        elapsed < whole,
        "the cancelled call took {elapsed:.2}s and the uncancelled one took {whole:.2}s, so \
         nothing was stopped early"
    );
}

/// A cancellation that arrives before its request runs still cancels it.
///
/// **The ordering the flag alone cannot handle.** `budget::arm` clears the
/// cancellation flag when a call starts, which is what stops a cancellation for
/// a finished call from killing the next one - and it means a cancellation that
/// overtook its own request would be cleared away by it. The server records the
/// id instead, and answers the request as cancelled without running it.
#[test]
fn a_cancellation_that_arrives_first_is_not_lost() {
    let Some(database) = database("early.rdb") else {
        inillucent_compat::differential::skipping("mcp_cancel: the command line did not build");
        return;
    };
    let Some(mut server) = Server::start(&database) else {
        inillucent_compat::differential::skipping("mcp_cancel: the MCP server did not start");
        return;
    };

    // Cancel a request that has not been sent, then send it.
    server.tell(&cancellation(7));
    let started = Instant::now();
    let answered = server.ask(&slow_call(7));
    let elapsed = started.elapsed().as_secs_f64();

    // And the next call, which nothing cancelled, still runs.
    let after = server.ask(&slow_call(8));
    server.stop();

    assert!(
        answered.contains("cancelled"),
        "a request whose cancellation arrived first was answered with something else after \
         {elapsed:.2}s: {answered}"
    );
    assert!(
        elapsed < 0.5,
        "a request whose cancellation had already arrived was still run for {elapsed:.2}s"
    );
    assert!(
        !after.contains("cancelled"),
        "the cancellation of request 7 also cancelled request 8: {after}"
    );
}
