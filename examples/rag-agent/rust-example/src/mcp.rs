//! The Model Context Protocol over stdio: JSON-RPC 2.0, one message per line.
//!
//! An MCP client such as Claude Code or opencode starts this program and
//! writes requests to its standard input. Each request is one line of JSON.
//! The server writes each response as one line to standard output. Anything
//! else the server prints goes to standard error, because a stray line on
//! standard output would be read as a broken message.
//!
//! The exchange for one question looks like this:
//!
//! ```text
//! client -> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18",...}}
//! server <- {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},...}}
//! client -> {"jsonrpc":"2.0","method":"notifications/initialized"}
//! client -> {"jsonrpc":"2.0","id":2,"method":"tools/list"}
//! client -> {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"who was Seneca"}}}
//! ```
//!
//! Five methods are handled. That is all a server offering tools needs, and
//! it is small enough to write here instead of taking an SDK, so every byte
//! the agent sees is visible in this file and `tools.rs`.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::tools::Tools;

/// The protocol versions this server speaks, newest first.
///
/// The client names the version it wants in `initialize`. The server answers
/// with the same version when it knows it, and otherwise with its newest, and
/// the client then decides whether it can continue.
pub const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// A JSON-RPC error: a code from the specification and a message.
#[derive(Debug)]
pub struct RpcError {
    /// `-32700` a line that is not JSON, `-32600` not a request, `-32601` no such method, `-32602` bad parameters.
    pub code: i64,
    /// What went wrong, for a person reading the client's log.
    pub message: String,
}

impl RpcError {
    /// Makes an error.
    ///
    /// @param code - the JSON-RPC error code
    /// @param message - what went wrong
    pub fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError { code, message: message.into() }
    }
}

/// Reads requests from standard input and answers them until the input closes.
///
/// The client closes standard input when it is done with the server, and the
/// server then returns and exits.
///
/// @param tools - the tools the server offers
pub fn serve(tools: &Tools) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(&line, tools) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Handles one line and returns the line to answer with, if any.
///
/// A notification has no `id` and gets no answer, as JSON-RPC requires.
///
/// @param line - one line from standard input
/// @param tools - the tools the server offers
pub fn handle_line(line: &str, tools: &Tools) -> Option<String> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => return Some(reply(&Value::Null, Err(RpcError::new(-32700, format!("not JSON: {error}"))))),
    };
    let id = message.get("id").cloned();
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        let id = id.unwrap_or(Value::Null);
        return Some(reply(&id, Err(RpcError::new(-32600, "a request needs a `method`"))));
    };
    let id = id?;
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": Tools::definitions() })),
        "tools/call" => tools.call(&params),
        other => Err(RpcError::new(-32601, format!("no method `{other}`"))),
    };
    Some(reply(&id, result))
}

/// Answers `initialize`: the protocol version, what the server offers, and its name.
///
/// `instructions` is text a client may add to the agent's context. It tells
/// the agent what the server is for and how to answer from it.
///
/// @param params - the client's `initialize` parameters
fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
    let version = PROTOCOL_VERSIONS.iter().find(|known| **known == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "rag-server", "title": "Greek philosophy search", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Searches 80 Wikipedia articles on Greek and Roman philosophy. Call `search` before answering a \
                         question about the subject, answer only from the passages it returns, and cite each article's \
                         title and URL. Use `get_passage` when a passage is cut off. If the passages do not answer the \
                         question, say that the articles do not cover it."
    })
}

/// Wraps a result or an error in a JSON-RPC response line.
///
/// @param id - the request's id
/// @param result - the method's result
fn reply(id: &Value, result: Result<Value, RpcError>) -> String {
    let message = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": error.code, "message": error.message } }),
    };
    message.to_string()
}
