//! A client for `inillucent-mcp`, over the pipes a real one uses.
//!
//! Invariant: **everything here goes through the child's standard input and
//! output, one line per message, and nothing calls the server in process.** A
//! tool that works in process and a server that cannot frame its own replies
//! are the same defect from a client's point of view, and 0.1.2 shipped with
//! the second of them: the release scripts sent a handshake the server refused,
//! and the release's own smoke test was what found it.
//!
//! ## Why this is a module rather than a helper in one test file
//!
//! `crates/inillucent-compat/tests/e2e/mcp_wire.rs` has a `Session` of its own,
//! written when it was the only suite that spoke the protocol. There are three
//! now - `mcp_wire`, `mcp_session` and `mcp_replay` - and the parts that are
//! easy to get subtly different are the parts that matter: reading a reply by
//! its id rather than by position, flattening a request onto one line, and
//! ending the child by closing its input rather than by killing it.
//!
//! `mcp_wire.rs` is deliberately not repointed at this. It works, its cases are
//! the twenty-eight tools, and a rewrite of a working suite to share a helper
//! is a rewrite of twenty-eight assertions for no assertion.

// **This module may panic**, for the reason `cliproc.rs` beside it may: it is a
// helper every MCP suite uses, it lives in `src/` so the crate's test-only
// relaxation does not reach it, and what it panics on is a broken environment -
// a server that will not start, a pipe that will not accept a write, a reply
// that never comes.
#![allow(clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// A live server, and the two pipes a client talks to it through.
pub struct Session {
    /// The child process.
    child: Child,
    /// Its standard input, which is where requests go. Taken when the client
    /// closes its end.
    input: Option<ChildStdin>,
    /// Its standard output, which is where replies come from. Taken when the
    /// reading moves to another thread.
    output: Option<BufReader<ChildStdout>>,
    /// The next request id, so every call is answered by its own reply.
    next: u64,
    /// Replies that arrived while a different id was being waited for.
    ///
    /// **Without this, a client that pipelines loses an answer.** A server may
    /// answer two in-flight requests in either order; the first version of
    /// `reply_to` read lines until it found its id and *discarded* the rest, so
    /// waiting for the second request's answer threw the first one away and
    /// then waited for it until the pipe closed. That is a defect in the
    /// client, and `two_requests_in_flight_are_both_answered` failed on it with
    /// a message blaming the server.
    pending: Vec<String>,
}

impl Session {
    /// Starts the server on a database and completes the handshake.
    ///
    /// @param server - the built `inillucent-mcp`
    /// @param database - the file to open
    pub fn start(server: &Path, database: &Path) -> Session {
        Session::start_with(server, database, &[])
    }

    /// Starts the server with extra arguments and completes the handshake.
    ///
    /// @param server - the built `inillucent-mcp`
    /// @param database - the file to open
    /// @param extra - the flags to start it with
    pub fn start_with(server: &Path, database: &Path, extra: &[&str]) -> Session {
        let mut session = Session::start_silent(server, database, extra);
        let hello = session.call(
            "initialize",
            r#"{"protocolVersion":"2024-11-05","capabilities":{},
                "clientInfo":{"name":"inillucent-compat","version":"1"}}"#,
        );
        assert!(
            hello.contains("protocolVersion") && hello.contains("serverInfo"),
            "the server did not answer `initialize`:\n{hello}"
        );
        session.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        session
    }

    /// Starts the server and sends nothing.
    ///
    /// For the one case that is about the handshake itself: a `tools/call`
    /// before `initialize` has to be refused, and a session that has already
    /// shaken hands cannot ask that question.
    ///
    /// @param server - the built `inillucent-mcp`
    /// @param database - the file to open
    /// @param extra - the flags to start it with
    pub fn start_silent(server: &Path, database: &Path, extra: &[&str]) -> Session {
        let mut child = Command::new(server)
            .args(["--db", &database.to_string_lossy()])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("inillucent-mcp did not start: {error}"));
        let input = child.stdin.take();
        let output = child.stdout.take().map(BufReader::new);
        Session {
            child,
            input,
            output,
            next: 1,
            pending: Vec::new(),
        }
    }

    /// Writes one request and returns the line that answered it.
    ///
    /// The reply is matched by id rather than by position, because a server is
    /// allowed to interleave a notification of its own, and a client that read
    /// the next line would then attribute one tool's answer to another.
    ///
    /// @param method - the JSON-RPC method
    /// @param params - its parameters, as a JSON object
    pub fn call(&mut self, method: &str, params: &str) -> String {
        let id = self.send(method, params);
        self.reply_to(id, method)
    }

    /// Writes one request and does not wait for its answer.
    ///
    /// The half of [`Session::call`] the two-in-flight case needs: both
    /// requests go out before either answer is read.
    ///
    /// @param method - the JSON-RPC method
    /// @param params - its parameters, as a JSON object
    /// @returns the id the request went out under
    pub fn send(&mut self, method: &str, params: &str) -> u64 {
        let id = self.next;
        self.next = self.next.saturating_add(1);
        self.notify(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}"
        ));
        id
    }

    /// Reads lines until one carries this id.
    ///
    /// @param id - the request's id
    /// @param about - what was asked, for the failure message
    pub fn reply_to(&mut self, id: u64, about: &str) -> String {
        let marker = format!("\"id\":{id}");
        if let Some(at) = self.pending.iter().position(|line| line.contains(&marker)) {
            return self.pending.remove(at);
        }
        loop {
            let Some(output) = self.output.as_mut() else {
                panic!("the session's output has been taken; {about} cannot be read");
            };
            let mut line = String::new();
            let read = output
                .read_line(&mut line)
                .unwrap_or_else(|error| panic!("reading the server's reply to {about}: {error}"));
            assert!(
                read > 0,
                "the server closed its output before answering {about} (id {id}); {} other \
                 lines were waiting",
                self.pending.len()
            );
            if line.contains(&marker) {
                return line;
            }
            // Somebody else's answer, or a notification of the server's own.
            // Kept rather than dropped: see `pending`.
            self.pending.push(line);
        }
    }

    /// Reads the next line the server writes, whatever it carries.
    ///
    /// For the replies that have no id to match on: the server's refusal of a
    /// request it could not parse carries `"id":null`, because it never got as
    /// far as reading one.
    ///
    /// @param about - what was sent, for the failure message
    pub fn next_line(&mut self, about: &str) -> String {
        if !self.pending.is_empty() {
            return self.pending.remove(0);
        }
        let Some(output) = self.output.as_mut() else {
            panic!("the session's output has been taken; {about} cannot be read");
        };
        let mut line = String::new();
        let read = output
            .read_line(&mut line)
            .unwrap_or_else(|error| panic!("reading the server's answer to {about}: {error}"));
        assert!(read > 0, "the server said nothing at all about {about}");
        line
    }

    /// Writes one line to the server without waiting for a reply.
    ///
    /// @param line - the JSON-RPC message
    pub fn notify(&mut self, line: &str) {
        let flattened: String = line.split_whitespace().collect::<Vec<&str>>().join(" ");
        self.write_line(&flattened);
    }

    /// Writes a line exactly as given, whether or not it is JSON.
    ///
    /// The protocol edges need this: a line that is not JSON at all, a JSON
    /// object with no `id`, a request of a megabyte and a byte.
    ///
    /// @param line - the bytes to send, without its newline
    pub fn write_line(&mut self, line: &str) {
        let Some(input) = self.input.as_mut() else {
            panic!("the session's input has already been closed");
        };
        writeln!(input, "{line}").unwrap_or_else(|error| {
            panic!(
                "the server would not take a {} byte line: {error}",
                line.len()
            )
        });
        input
            .flush()
            .unwrap_or_else(|error| panic!("flushing a {} byte line: {error}", line.len()));
    }

    /// Closes the client's end and waits for the server to finish.
    ///
    /// **The draining has to be on another thread, and the first version of
    /// this was not.** Reading the child's output on the same thread that waits
    /// for it to exit looks right and hangs: `read` blocks until a byte
    /// arrives, so a deadline checked *between* reads is never reached while
    /// the child is busy - and the one case this exists for, a statement still
    /// streaming when the pipe closes, is exactly the case where no byte
    /// arrives for a long time. The first run of
    /// `mcp_session::closing_the_pipe_mid_statement_leaves_no_stale_lock` sat
    /// there until it was stopped by hand.
    ///
    /// Not draining at all is the other way to hang: a child whose output pipe
    /// has filled blocks on its next write and never reaches the end of its
    /// input.
    ///
    /// @param within - how long to wait for the child to exit
    /// @returns the exit status, and how many bytes it wrote on the way out
    pub fn close_and_wait(&mut self, within: Duration) -> (Option<i32>, usize) {
        self.input = None;
        let reader = self.output.take().map(|mut output| {
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = output.read_to_end(&mut sink);
                sink.len()
            })
        });
        let deadline = Instant::now() + within;
        let mut status = None;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(finished)) => {
                    status = Some(finished.code().unwrap_or(-1));
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        if status.is_none() {
            // Still going, and `Drop` would end it anyway. Ending it here is
            // what closes the pipe the reading thread is waiting on, so the
            // join below returns instead of waiting with it.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let wrote = reader.and_then(|handle| handle.join().ok()).unwrap_or(0);
        (status, wrote)
    }

    /// Calls one tool and returns the line that answered it.
    ///
    /// @param tool - the tool's name
    /// @param arguments - its arguments, as a JSON object
    pub fn tool(&mut self, tool: &str, arguments: &str) -> String {
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
    /// kill would leave a case unable to tell a clean exit from a crash. The
    /// kill is the backstop for a server that does not end.
    fn drop(&mut self) {
        self.input = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether a reply is a refusal.
///
/// **Two spellings, and only one of them is JSON-RPC's.** A protocol level
/// refusal - a method before the handshake, a request past the size limit -
/// carries an `error` object. A *tool* that refuses answers a normal result
/// with `isError` set, because the call reached the tool and the tool declined;
/// that is MCP's own distinction and a client that only looked for the first
/// would read a refused export as a successful one.
///
/// @param line - the reply
pub fn is_an_error(line: &str) -> bool {
    line.contains("\"error\"") || line.contains("\"isError\":true")
}

/// The value of a `"status"` field in a reply, or an empty string.
///
/// The envelope every tool answers carries one, and it is the field a client
/// branches on: `unsupported` is the engine's own "not yet" and is a different
/// thing from a statement that is wrong.
///
/// @param line - the reply
pub fn status_in(line: &str) -> String {
    let escaped = "\\\"status\\\":\\\"";
    if let Some(at) = line.find(escaped) {
        return line
            .get(at + escaped.len()..)
            .and_then(|rest| rest.split('\\').next())
            .unwrap_or_default()
            .to_string();
    }
    let plain = "\"status\":\"";
    match line.find(plain) {
        Some(at) => line
            .get(at + plain.len()..)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_default()
            .to_string(),
        None => String::new(),
    }
}

/// Masks the fields a replay may not compare on.
///
/// A recorded reply carries the id the recording client happened to use and
/// whatever the server's clock said, and neither is a property of the protocol.
/// Comparing them would make a transcript expire, which is the one thing a
/// fixture must not do.
///
/// @param line - one line of a transcript, or one reply
pub fn masked(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        // `"id":<digits>` becomes `"id":*`.
        let Some(at) = rest.find("\"id\":") else {
            out.push_str(rest);
            break;
        };
        let (before, after) = rest.split_at(at + "\"id\":".len());
        out.push_str(before);
        let digits = after
            .chars()
            .take_while(|letter| letter.is_ascii_digit())
            .count();
        if digits > 0 {
            out.push('*');
            rest = after.get(digits..).unwrap_or_default();
        } else {
            rest = after;
        }
    }
    let mut masked = out;
    for field in [
        "elapsed_ms",
        "elapsed_us",
        "duration_ms",
        "milliseconds",
        "microseconds",
    ] {
        masked = mask_number(&masked, field);
    }
    masked
}

/// Replaces the number after a named field with a star.
///
/// @param line - the text
/// @param field - the field's name, without its quotes
fn mask_number(line: &str, field: &str) -> String {
    let escaped = format!("\\\"{field}\\\":");
    let plain = format!("\"{field}\":");
    let mut out = line.to_string();
    for marker in [escaped, plain] {
        let mut result = String::with_capacity(out.len());
        let mut rest = out.as_str();
        loop {
            let Some(at) = rest.find(&marker) else {
                result.push_str(rest);
                break;
            };
            let (before, after) = rest.split_at(at + marker.len());
            result.push_str(before);
            let digits = after
                .chars()
                .take_while(|letter| letter.is_ascii_digit() || *letter == '.')
                .count();
            if digits > 0 {
                result.push('*');
                rest = after.get(digits..).unwrap_or_default();
            } else {
                rest = after;
            }
        }
        out = result;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A status reads out of both the escaped and the plain spelling.
    ///
    /// The envelope arrives inside a JSON string, so its own quotes are
    /// escaped; a reply that carries the object directly is not. Both are read
    /// because which one a tool answers with is the server's choice and not
    /// this module's.
    #[test]
    fn a_status_reads_out_of_either_spelling() {
        assert_eq!(
            status_in(r#"{"result":{"content":[{"text":"{\"status\":\"unsupported\"}"}]}}"#),
            "unsupported"
        );
        assert_eq!(status_in(r#"{"result":{"status":"ok"}}"#), "ok");
        assert_eq!(status_in(r#"{"result":{}}"#), "");
    }

    /// A refusal is told apart from a result, in both its spellings.
    #[test]
    fn a_refusal_is_told_apart_from_a_result() {
        assert!(is_an_error(r#"{"id":1,"error":{"code":-32600}}"#));
        assert!(is_an_error(
            r#"{"id":1,"result":{"content":[],"isError":true}}"#
        ));
        assert!(!is_an_error(
            r#"{"id":1,"result":{"content":[],"isError":false}}"#
        ));
        assert!(!is_an_error(r#"{"id":1,"result":{}}"#));
    }

    /// The mask takes the id and the timings and leaves everything else.
    #[test]
    fn the_mask_takes_the_id_and_the_timings() {
        assert_eq!(
            masked(r#"{"jsonrpc":"2.0","id":17,"result":{"rows":3,"elapsed_ms":42}}"#),
            r#"{"jsonrpc":"2.0","id":*,"result":{"rows":3,"elapsed_ms":*}}"#
        );
        // Two ids in one line, which a batched reply has.
        assert_eq!(masked(r#"[{"id":1},{"id":200}]"#), r#"[{"id":*},{"id":*}]"#);
        // And nothing else moves.
        assert_eq!(
            masked(r#"{"result":{"total":10001,"name":"note"}}"#),
            r#"{"result":{"total":10001,"name":"note"}}"#
        );
    }
}
