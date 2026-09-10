//! The MCP server: the command table, served to an agent.
//!
//! Invariant: **every tool is a row of [`crate::command::COMMANDS`], and the
//! server invents nothing.** `tools/list` is generated from the table, each
//! schema is generated from that command's parameters, and each description is
//! the summary and detail a person reads in `inillucent help`. A tool the table
//! does not have cannot be served, and a command the table has cannot be
//! forgotten - `command_parity.rs` asserts both directions.
//!
//! The transport is stdio JSON-RPC 2.0, one object per line, which is what the
//! Model Context Protocol specifies for a local server. There is no HTTP here
//! and there should not be: a database server that listens on a socket is a
//! different product with a different threat model, and this one is started by
//! the agent that talks to it and dies with it.
//!
//! ## Why the default answer is a table rather than JSON
//!
//! Every tool takes `output`, and it defaults to `text`. It is `output` rather
//! than `format` because `inillucent_export` has a `format` of its own, naming
//! CSV against JSON against Markdown - a different question, and one word
//! cannot answer both. A structured result is
//! strictly more informative, and the local 27B model this was verified against
//! is measurably worse at reading one: it drops columns out of an array of
//! objects and answers from the first row. An aligned table is what it reads
//! correctly, so that is the default and `output: "json"` is one parameter away
//! for a client that would rather parse.
//!
//! ## What it does not do
//!
//! No resources, no prompts, no sampling, no subscriptions. Each is a real part
//! of the protocol and none of them is a database operation; declaring a
//! capability this server does not implement would be the same kind of untrue
//! claim the capability table exists to prevent.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::command::{self, Arguments, Command, Context, Failed};
use crate::json::{self, Json};

/// The protocol version this server speaks.
///
/// Echoed back to a client that asks for it, and sent as-is to one that asks
/// for something else - which is what the specification says to do, because a
/// client that understands a later revision still understands this one.
pub const PROTOCOL: &str = "2025-06-18";

/// The most bytes one request line may hold.
///
/// **A megabyte, and the bound is on the line rather than on the parsed
/// object**, because a reader that parsed first would have allocated whatever
/// it was sent before it could decide. `BufRead::read_line` on a client that
/// never sends a newline grows a `String` until the process dies, and this is
/// the only place that can stop it.
///
/// A megabyte is far above any tool call - the largest thing one carries is a
/// SQL statement and a parameter array - and far below a size that costs
/// anything to refuse.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// The most bytes one answer may hold before it is refused rather than sent.
///
/// **Refused, not truncated.** A truncated JSON-RPC frame is not a smaller
/// answer, it is an unparseable one, and a client that received one would
/// report a broken server. So an answer that would be too large is replaced by
/// a refusal that says which budget it was and what to do about it.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The most rows one MCP call hands back.
///
/// **Ten thousand, against the command line's absence of a ceiling**, which is
/// the distinction the whole budget rests on: a person asking their own
/// database for every row is asking for what they want, and an agent asking a
/// served database for the same thing is what `--root` and `--readonly` already
/// exist for. It is also well above what an agent can read: a model that is
/// handed ten thousand rows is going to summarise the first fifty.
pub const MAX_ROWS: usize = 10_000;

/// What the server was started with.
pub struct Settings {
    /// The database opened when a call does not name one.
    pub database: String,
    /// Whether every statement that changes something is refused.
    pub readonly: bool,
    /// The directory outside which no path may be named.
    pub root: Option<PathBuf>,
    /// How many rows a call gets back when it does not say.
    pub limit: usize,
    /// The most rows one call may hand back.
    pub max_rows: usize,
    /// How long one call may run before it is stopped.
    pub max_time: std::time::Duration,
}

impl Default for Settings {
    /// The settings an agent gets when it starts the server with no arguments.
    fn default() -> Settings {
        Settings {
            database: ":memory:".to_string(),
            readonly: false,
            root: None,
            limit: 200,
            max_rows: MAX_ROWS,
            max_time: std::time::Duration::from_secs(60),
        }
    }
}

/// Returns the tools an MCP client is offered.
///
/// One per command that is not `cli_only`, named `inillucent_<command>` with
/// dashes folded to underscores, because a tool name is an identifier in every
/// client that has ever been written and a dash is not.
pub fn tools() -> Vec<Json> {
    command::COMMANDS
        .iter()
        .filter(|command| command.cli_only.is_none())
        .map(tool_of)
        .collect()
}

/// Returns one command as an MCP tool declaration.
///
/// @param command - the table row
fn tool_of(command: &'static Command) -> Json {
    json::object(vec![
        ("name", json::text(tool_name(command))),
        (
            "description",
            json::text(format!("{}\n\n{}", command.summary, command.detail)),
        ),
        ("inputSchema", schema_of(command)),
    ])
}

/// Returns the tool name a command is served under.
///
/// @param command - the table row
pub fn tool_name(command: &Command) -> String {
    format!("inillucent_{}", command.name.replace('-', "_"))
}

/// Returns the JSON Schema for a command's arguments.
///
/// @param command - the table row
pub fn schema_of(command: &Command) -> Json {
    let properties: Vec<(String, Json)> = command
        .params
        .iter()
        .map(|param| {
            let mut member = vec![
                ("type", json::text(param.kind.schema_type())),
                ("description", json::text(param.description)),
            ];
            // An array's element type has to be declared or a client cannot
            // validate what it is about to send. These arrays hold SQL values,
            // which are exactly the five JSON scalars, so the schema says so
            // rather than leaving `items` open.
            if param.kind == command::Kind::Values {
                member.push((
                    "items",
                    json::object(vec![(
                        "type",
                        Json::Array(vec![
                            json::text("string"),
                            json::text("number"),
                            json::text("boolean"),
                            json::text("null"),
                        ]),
                    )]),
                ));
            }
            (param.name.to_string(), json::object(member))
        })
        .collect();
    let required: Vec<Json> = command
        .params
        .iter()
        .filter(|param| param.required)
        .map(|param| json::text(param.name))
        .collect();
    json::object(vec![
        ("type", json::text("object")),
        ("properties", Json::Object(properties)),
        ("required", Json::Array(required)),
    ])
}

/// Reads requests from a reader and writes answers to a writer until it ends.
///
/// @param settings - what the server was started with
/// @param input - where requests arrive
/// @param output - where answers go
pub fn serve(
    settings: Settings,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<(), String> {
    let mut context = Context::open(&settings.database, settings.readonly, settings.root.clone())
        .map_err(|failure| failure.message)?;
    context.limit = settings.limit.min(settings.max_rows);
    context.set_max_rows(Some(settings.max_rows));
    // **The engine's own budget, armed for the life of the server rather than
    // per call.** A row ceiling on the command surface bounds what a *tool
    // call* hands back; this bounds what the engine does on the way there, so a
    // `SELECT` whose `WHERE` rejects everything after scanning a hundred
    // million rows still stops. The two are different questions and both need
    // an answer.
    context.set_limits(
        inillucent_engine::base::budget::Limits::served().with_time(Some(settings.max_time)),
    );
    let mut line = String::new();
    loop {
        line.clear();
        match read_request(input, &mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(TooLong) => {
                // The connection is not recoverable: the rest of an over-long
                // line is still in the stream and would be read as the next
                // request. Saying so and stopping is the honest end.
                let _ = writeln!(
                    output,
                    "{}",
                    error_response(
                        Json::Null,
                        -32600,
                        &format!(
                            "a request may not be longer than {MAX_REQUEST_BYTES} bytes, and this \
                             connection has sent one that is."
                        )
                    )
                );
                let _ = output.flush();
                return Ok(());
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Some(answer) = handle(&mut context, &line) {
            let answer = match answer.len() > MAX_RESPONSE_BYTES {
                false => answer,
                true => error_response(
                    Json::Null,
                    -32603,
                    &format!(
                        "this answer would have been {} bytes, past the {MAX_RESPONSE_BYTES} a \
                         reply may hold. Ask for fewer rows or fewer columns.",
                        answer.len()
                    ),
                ),
            };
            writeln!(output, "{answer}").map_err(|error| error.to_string())?;
            output.flush().map_err(|error| error.to_string())?;
        }
    }
}

/// A request line that ran past [`MAX_REQUEST_BYTES`].
struct TooLong;

/// Reads one request line, refusing one that is too long.
///
/// **Byte by byte up to the ceiling, rather than `read_line` and a check
/// afterwards.** `read_line` on a client that never sends a newline grows the
/// string until the process dies, so a check after it never runs. This is the
/// same argument `inillucent-remote`'s `MAX_MESSAGE` makes about a length a
/// server announces: a bound that is applied after the allocation is not a
/// bound.
///
/// @param input - where requests arrive
/// @param line - the buffer to fill
fn read_request(input: &mut impl BufRead, line: &mut String) -> Result<usize, TooLong> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let mut one = [0u8; 1];
        match input.read(&mut one) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let byte = one.first().copied().unwrap_or(b'\n');
        if byte == b'\n' {
            break;
        }
        if bytes.len() >= MAX_REQUEST_BYTES {
            return Err(TooLong);
        }
        bytes.push(byte);
    }
    if bytes.is_empty() {
        return Ok(0);
    }
    line.push_str(&String::from_utf8_lossy(&bytes));
    Ok(line.len())
}

/// Answers one request, or returns nothing for a notification.
///
/// @param context - the open database
/// @param line - the request, as it arrived
pub fn handle(context: &mut Context, line: &str) -> Option<String> {
    let request = match json::parse(line) {
        Ok(request) => request,
        // -32700 is JSON-RPC's parse error, and it is answered with a null id
        // because the id is exactly what could not be read.
        Err(why) => return Some(error_response(Json::Null, -32700, &why)),
    };
    let id = request.get("id").cloned().unwrap_or(Json::Null);
    let Some(method) = request.get("method").and_then(Json::text) else {
        return Some(error_response(id, -32600, "a request needs a 'method'."));
    };
    // A notification has no id and takes no answer. Writing one anyway is the
    // most common way a hand-written server breaks a strict client.
    let is_notification = request.get("id").is_none();
    let params = request.get("params").cloned().unwrap_or(Json::Null);
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "tools/list" => Ok(json::object(vec![("tools", Json::Array(tools()))])),
        "tools/call" => call(context, &params),
        "ping" => Ok(json::object(vec![])),
        _ if is_notification => return None,
        other => {
            return Some(error_response(
                id,
                -32601,
                &format!("this server has no '{other}' method."),
            ))
        }
    };
    if is_notification {
        return None;
    }
    Some(match result {
        Ok(value) => json::object(vec![
            ("jsonrpc", json::text("2.0")),
            ("id", id),
            ("result", value),
        ])
        .write(),
        Err(failure) => error_response(id, -32602, &failure.message),
    })
}

/// Returns what a client is told when it connects.
///
/// @param params - what the client sent, whose `protocolVersion` is echoed
fn initialize(params: &Json) -> Json {
    let asked = params
        .get("protocolVersion")
        .and_then(Json::text)
        .unwrap_or(PROTOCOL);
    json::object(vec![
        ("protocolVersion", json::text(asked)),
        (
            "capabilities",
            json::object(vec![(
                "tools",
                json::object(vec![("listChanged", Json::Bool(false))]),
            )]),
        ),
        (
            "serverInfo",
            json::object(vec![
                ("name", json::text("inillucent")),
                ("version", json::text(env!("CARGO_PKG_VERSION"))),
            ]),
        ),
        (
            "instructions",
            json::text(
                "inillucent is an embedded SQL database that speaks SQLite's dialect, with \
                 full-text and vector search built in. Call inillucent_tables to see what is \
                 there, inillucent_describe before writing SQL against a table you did not \
                 create, inillucent_query to read and inillucent_exec to write. If a call comes \
                 back with status 'unsupported', that construct is not built yet - it is not a \
                 mistake in your SQL, and rewording it will not help.",
            ),
        ),
    ])
}

/// Runs one tool call.
///
/// @param context - the open database
/// @param params - the `name` and `arguments` the client sent
fn call(context: &mut Context, params: &Json) -> Result<Json, Failed> {
    let Some(name) = params.get("name").and_then(Json::text) else {
        return Err(Failed::misuse("a tools/call needs a 'name'."));
    };
    let Some(command) = command::find(name) else {
        return Ok(tool_error(&format!(
            "there is no tool called '{name}'. Call tools/list to see what there is."
        )));
    };
    if command.cli_only.is_some() {
        return Ok(tool_error(&format!(
            "'{name}' is not served over MCP: {}",
            command.cli_only.unwrap_or_default()
        )));
    }
    let arguments = Arguments::from_json(&params.get("arguments").cloned().unwrap_or(Json::Null));
    let wants_json = arguments.text("output") == Some("json");
    match command::run(command, context, &arguments) {
        Ok(produced) => {
            let body = match wants_json {
                true => produced.to_json().pretty(0),
                false => produced.text.clone(),
            };
            Ok(content(&body, false))
        }
        // A refusal is a *result* with `isError`, not a JSON-RPC error: the
        // request was well formed and the server answered it. A client that saw
        // a protocol error here would report a broken server rather than
        // showing the model a sentence it can act on.
        Err(failure) => {
            let body = match wants_json {
                true => failure.to_json(command.name).pretty(0),
                false => failure.to_text(),
            };
            Ok(content(&body, true))
        }
    }
}

/// Wraps text as an MCP tool result.
///
/// @param text - what to show
/// @param failed - whether the tool refused
fn content(text: &str, failed: bool) -> Json {
    json::object(vec![
        (
            "content",
            Json::Array(vec![json::object(vec![
                ("type", json::text("text")),
                ("text", json::text(text)),
            ])]),
        ),
        ("isError", Json::Bool(failed)),
    ])
}

/// Wraps a message the server itself is refusing with.
///
/// @param message - what to say
fn tool_error(message: &str) -> Json {
    content(message, true)
}

/// Renders a JSON-RPC error response.
///
/// @param id - the request's id, or null when it could not be read
/// @param code - the JSON-RPC error code
/// @param message - what went wrong
fn error_response(id: Json, code: i64, message: &str) -> String {
    json::object(vec![
        ("jsonrpc", json::text("2.0")),
        ("id", id),
        (
            "error",
            json::object(vec![
                ("code", Json::Int(code)),
                ("message", json::text(message)),
            ]),
        ),
    ])
    .write()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens a scratch context for a test.
    fn context() -> Context {
        Context::open(":memory:", false, None).expect("an in-memory database opens")
    }

    /// Every served tool is a command, and every command that is not cli-only
    /// is served.
    #[test]
    fn the_tools_are_the_commands() {
        let served: Vec<String> = tools()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Json::text).map(str::to_string))
            .collect();
        let expected: Vec<String> = command::COMMANDS
            .iter()
            .filter(|command| command.cli_only.is_none())
            .map(tool_name)
            .collect();
        assert_eq!(served, expected);
        assert!(served.contains(&"inillucent_query".to_string()));
        assert!(!served.contains(&"inillucent_shell".to_string()));
    }

    /// A tool name is a valid identifier in every client.
    #[test]
    fn tool_names_have_no_dashes() {
        for tool in tools() {
            let name = tool
                .get("name")
                .and_then(Json::text)
                .unwrap_or_default()
                .to_string();
            assert!(!name.contains('-'), "{name} has a dash in it");
        }
    }

    /// Initialize echoes the version the client asked for.
    #[test]
    fn initialize_echoes_the_clients_version() {
        let answer = handle(
            &mut context(),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\
             \"params\":{\"protocolVersion\":\"2024-11-05\"}}",
        )
        .unwrap_or_default();
        assert!(answer.contains("\"protocolVersion\":\"2024-11-05\""));
        assert!(answer.contains("\"name\":\"inillucent\""));
    }

    /// A notification is not answered.
    #[test]
    fn a_notification_gets_no_answer() {
        assert!(handle(
            &mut context(),
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}"
        )
        .is_none());
    }

    /// A round trip through the tools creates a table and reads it back.
    #[test]
    fn a_call_creates_and_reads() {
        let mut held = context();
        let made = handle(
            &mut held,
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\
             \"name\":\"inillucent_exec\",\"arguments\":{\
             \"sql\":\"CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)\"}}}",
        )
        .unwrap_or_default();
        assert!(made.contains("\"isError\":false"), "{made}");
        handle(
            &mut held,
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\
             \"name\":\"inillucent_exec\",\"arguments\":{\
             \"sql\":\"INSERT INTO people VALUES (?1, ?2)\",\"params\":[1,\"Ada\"]}}}",
        );
        let read = handle(
            &mut held,
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\
             \"name\":\"inillucent_query\",\"arguments\":{\
             \"sql\":\"SELECT name FROM people\"}}}",
        )
        .unwrap_or_default();
        assert!(read.contains("Ada"), "{read}");
    }

    /// A tool that does not exist is a result, not a protocol error.
    #[test]
    fn an_unknown_tool_is_a_tool_error() {
        let answer = handle(
            &mut context(),
            "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\
             \"params\":{\"name\":\"inillucent_nonsense\",\"arguments\":{}}}",
        )
        .unwrap_or_default();
        assert!(answer.contains("\"isError\":true"));
        assert!(answer.contains("\"result\""));
        assert!(!answer.contains("\"error\""));
    }

    /// A document that is not JSON gets the parse error and a null id.
    #[test]
    fn a_broken_request_is_refused() {
        let answer = handle(&mut context(), "{not json").unwrap_or_default();
        assert!(answer.contains("-32700"));
        assert!(answer.contains("\"id\":null"));
    }

    /// A method nobody implements is refused by name.
    #[test]
    fn an_unknown_method_is_refused() {
        let answer = handle(
            &mut context(),
            "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"resources/list\"}",
        )
        .unwrap_or_default();
        assert!(answer.contains("-32601"));
    }

    /// Every schema declares its required parameters and nothing else.
    #[test]
    fn schemas_declare_what_is_required() {
        for command in command::COMMANDS {
            let schema = schema_of(command);
            let required: Vec<String> = schema
                .get("required")
                .and_then(Json::array)
                .unwrap_or_default()
                .iter()
                .filter_map(|name| name.text().map(str::to_string))
                .collect();
            let expected: Vec<String> = command
                .params
                .iter()
                .filter(|param| param.required)
                .map(|param| param.name.to_string())
                .collect();
            assert_eq!(
                required, expected,
                "{} declares the wrong required set",
                command.name
            );
        }
    }

    /// A read-only server refuses a write and says why.
    #[test]
    fn read_only_refuses_a_write() {
        let mut held = Context::open(":memory:", true, None).expect("opens");
        let answer = handle(
            &mut held,
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\
             \"name\":\"inillucent_exec\",\"arguments\":{\"sql\":\"CREATE TABLE t (a)\"}}}",
        )
        .unwrap_or_default();
        assert!(answer.contains("\"isError\":true"), "{answer}");
        assert!(answer.contains("read only"), "{answer}");
    }
}
