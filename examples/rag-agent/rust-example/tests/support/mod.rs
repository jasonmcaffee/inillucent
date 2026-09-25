//! Starts the built `rag-server` and speaks MCP to it, the way an agent's client does.
//!
//! Nothing here reaches into the server's code. A test sees only what an MCP
//! client sees: lines of JSON on the server's stdout, in answer to lines it
//! wrote to the server's stdin. That makes these tests end to end: the real
//! binary, the real database file, and the real embedding model.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// How long a single request may take before the test fails.
///
/// The first search in a process loads the model, which takes about a second,
/// and a search that waits behind a sync's write can take a few more.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// A running server and the pipes to it.
pub struct Server {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<String>,
    stderr: Arc<Mutex<Vec<String>>>,
    next_id: i64,
}

impl Server {
    /// Starts `rag-server serve` on a database and a corpus.
    ///
    /// @param db - the database file; created when it does not exist
    /// @param corpus - the JSONL file or folder to sync from
    /// @param sync_every - the sync interval, such as `3s`
    pub fn start(db: &Path, corpus: &Path, sync_every: &str) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rag-server"))
            .args(["serve", "--db", &path_text(db), "--corpus", &path_text(corpus), "--sync-every", sync_every])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("rag-server starts");
        let stdin = child.stdin.take().expect("stdin is piped");
        let replies = read_lines(child.stdout.take().expect("stdout is piped"));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        collect_lines(child.stderr.take().expect("stderr is piped"), Arc::clone(&stderr));
        Server { child, stdin, replies, stderr, next_id: 1 }
    }

    /// Sends a request and returns the whole response message.
    ///
    /// @param method - the JSON-RPC method
    /// @param params - its parameters
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send_line(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string());
        let reply = self.next_reply();
        assert_eq!(reply["id"], json!(id), "the reply answers a different request: {reply}");
        reply
    }

    /// Sends a notification, which gets no reply.
    ///
    /// @param method - the JSON-RPC method
    pub fn notify(&mut self, method: &str) {
        self.send_line(&json!({ "jsonrpc": "2.0", "method": method }).to_string());
    }

    /// Writes one raw line to the server's stdin.
    ///
    /// @param line - the line, without its newline
    pub fn send_line(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("the server reads stdin");
        self.stdin.flush().expect("stdin flushes");
    }

    /// Waits for the next line the server writes, and parses it.
    pub fn next_reply(&mut self) -> Value {
        let line = self
            .replies
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|_| panic!("no reply in {REPLY_TIMEOUT:?}. The server said:\n{}", self.log()));
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("the server wrote a line that is not JSON ({error}): {line}"))
    }

    /// Runs the MCP handshake and returns the `initialize` result.
    pub fn initialize(&mut self) -> Value {
        let reply = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "rag-server-tests", "version": "1" }
            }),
        );
        self.notify("notifications/initialized");
        reply["result"].clone()
    }

    /// Calls a tool and returns its result: `content`, `structuredContent` and `isError`.
    ///
    /// @param tool - the tool's name
    /// @param arguments - its arguments
    pub fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let reply = self.request("tools/call", json!({ "name": tool, "arguments": arguments }));
        assert!(reply.get("error").is_none(), "tools/call {tool} failed: {reply}");
        reply["result"].clone()
    }

    /// Calls a tool that has to succeed and returns its structured result.
    ///
    /// @param tool - the tool's name
    /// @param arguments - its arguments
    pub fn call_ok(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments.clone());
        assert_eq!(result["isError"], json!(false), "{tool} {arguments} returned an error: {result}");
        result["structuredContent"].clone()
    }

    /// Polls `sync_status` until a condition holds, and returns the status that met it.
    ///
    /// @param what - what is being waited for, for the failure message
    /// @param timeout - how long to wait
    /// @param done - the condition, given the status
    pub fn wait_for_sync(&mut self, what: &str, timeout: Duration, done: impl Fn(&Value) -> bool) -> Value {
        let started = Instant::now();
        loop {
            let status = self.call_ok("sync_status", json!({}));
            if done(&status) {
                return status;
            }
            if started.elapsed() > timeout {
                panic!("waited {timeout:?} for {what}. Last status: {status:#}\nThe server said:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Returns everything the server wrote to stderr so far.
    pub fn log(&self) -> String {
        self.stderr.lock().map(|lines| lines.join("\n")).unwrap_or_default()
    }
}

impl Drop for Server {
    /// Stops the server. Closing stdin would stop it too, but a failed test
    /// must not leave a process holding the database open.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Returns a new, empty folder for one test's files.
///
/// @param name - the test's name, to tell the folders apart
pub fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let folder = std::env::temp_dir().join("rag-server-tests").join(format!("{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&folder).expect("the scratch folder is created");
    folder
}

/// Returns the lines of the shared corpus for the given article titles, in the corpus's order.
///
/// The tests use real articles from `../corpus/greek-philosophy.jsonl`, so a
/// search that passes here passes on the text the example ships.
///
/// @param titles - the articles to take
pub fn corpus_lines(titles: &[&str]) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus/greek-philosophy.jsonl");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let lines: Vec<String> = text
        .lines()
        .filter(|line| serde_json::from_str::<Value>(line).is_ok_and(|v| titles.contains(&v["title"].as_str().unwrap_or(""))))
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), titles.len(), "the corpus is missing one of {titles:?}");
    lines
}

/// Writes lines to a file, one per line.
///
/// @param path - the file
/// @param lines - the lines
pub fn write_lines(path: &Path, lines: &[String]) {
    std::fs::write(path, lines.join("\n") + "\n").expect("the corpus file is written");
}

/// Returns the titles of a search result's hits, in order.
///
/// @param result - the `search` tool's structured result
pub fn titles(result: &Value) -> Vec<String> {
    result["hits"].as_array().map(|hits| hits.iter().map(|h| h["title"].as_str().unwrap_or("").to_string()).collect()).unwrap_or_default()
}

/// Reads a child's stdout on a thread and sends each line to a channel.
///
/// @param stdout - the child's stdout
fn read_lines(stdout: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (send, receive) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    receive
}

/// Reads a child's stderr on a thread into a shared list.
///
/// @param stderr - the child's stderr
/// @param into - the list
fn collect_lines(stderr: impl std::io::Read + Send + 'static, into: Arc<Mutex<Vec<String>>>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(mut lines) = into.lock() {
                lines.push(line);
            }
        }
    });
}

/// Writes a path with forward slashes.
///
/// @param path - the path
fn path_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
