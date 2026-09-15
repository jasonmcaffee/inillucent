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

use crate::command::{self, Arguments, Command, Context, Failed, OpenMode};
use crate::json::{self, Json};

/// The protocol version this server speaks.
///
/// Sent to every client, including one that asks for a revision this server does
/// not support. MCP requires a server to select a revision it supports.
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

/// The initialization stage of one MCP stdio session.
#[derive(Default)]
enum Lifecycle {
    /// The client has not sent its initialize request.
    #[default]
    AwaitingInitialize,
    /// The server answered initialize and waits for notifications/initialized.
    AwaitingInitializedNotification,
    /// The client completed initialization and may use normal methods.
    Ready,
}

/// State held for the lifetime of one MCP stdio session.
#[derive(Default)]
pub struct Session {
    lifecycle: Lifecycle,
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
            if let Some(allowed) = command.allowed_values(param.name) {
                member.push((
                    "enum",
                    Json::Array(allowed.iter().map(|value| json::text(*value)).collect()),
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
        ("additionalProperties", Json::Bool(false)),
    ])
}

/// Reads requests from a reader and writes answers to a writer until it ends.
///
/// **The reading happens on a second thread (task-1932, H11).** A server that
/// reads, answers, and only then reads again cannot see a message that arrives
/// *while* it is answering - which is every message worth acting on
/// immediately, and `notifications/cancelled` is the one the protocol defines
/// for it. A `tools/call` running a scan of a large table held this server for
/// its whole sixty second deadline and the client's cancellation sat unread in
/// the pipe behind it.
///
/// The reader thread does two things: it sets this session's cancellation flag
/// the moment it sees a cancellation notification, and it hands every line to
/// the main thread. Nothing else is interpreted there - the protocol lives in
/// `handle_with_session`, and a second reader of it would be a second server.
///
/// @param settings - what the server was started with
/// @param input - where requests arrive, owned so it can be read from a thread
/// @param output - where answers go
pub fn serve<R: BufRead + Send + 'static>(
    settings: Settings,
    mut input: R,
    output: &mut impl Write,
) -> Result<(), String> {
    let mut context = Context::open(
        &settings.database,
        OpenMode::of(settings.readonly),
        settings.root.clone(),
    )
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
        inillucent_driver::StatementLimits::served().with_time(Some(settings.max_time)),
    );
    let mut session = Session::default();

    // The reader thread. It owns the input for the life of the server, sends
    // each line here, and stops when the input ends or the main thread is gone.
    let cancel = context.cancel_flag();
    // This server clears the flag itself, at the boundary below, so `arm` must
    // not clear it again - see `budget::arm_as_it_stands`.
    context.preserve_cancellation();
    // **What is running, and what has been cancelled, under one lock
    // (task-1932, H11).** A call and its cancellation arrive as two lines in
    // one write, and the two threads can interleave in either order:
    //
    // - the cancellation is read *before* the main thread takes the call off
    //   the queue, in which case `running` is not yet its id and the id is
    //   recorded - the main thread finds it and answers cancelled without
    //   running anything;
    // - the cancellation is read *after*, in which case `running` is its id and
    //   the flag is set - the main thread has already cleared the flag and
    //   armed the budget, so the statement stops at its next batch.
    //
    // Both decisions are made holding this lock, which is what makes the pair
    // exhaustive. Before it the second case lost about one run in three: the
    // id check had already passed and `budget::arm`'s clear wiped the flag.
    let state: std::sync::Arc<std::sync::Mutex<Cancellation>> =
        std::sync::Arc::new(std::sync::Mutex::new(Cancellation::default()));
    let noted = std::sync::Arc::clone(&state);
    let (lines, arriving) = std::sync::mpsc::channel::<Arrival>();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            let arrival = match read_request(&mut input, &mut line) {
                Ok(0) => Arrival::Ended,
                Ok(_) => {
                    if let Some(id) = cancellation_target(&line) {
                        if let Ok(mut held) = noted.lock() {
                            if held.running.as_deref() == Some(id.as_str()) {
                                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            } else {
                                held.cancelled.push(id);
                            }
                        }
                    }
                    Arrival::Line(line.clone())
                }
                Err(TooLong) => Arrival::TooLong,
            };
            let ended = matches!(arrival, Arrival::Ended | Arrival::TooLong);
            if lines.send(arrival).is_err() || ended {
                return;
            }
        }
    });

    let mut line = String::new();
    loop {
        line.clear();
        match arriving.recv() {
            // The reader ended, or went away with it.
            Ok(Arrival::Ended) | Err(_) => return Ok(()),
            Ok(Arrival::Line(arrived)) => line.push_str(&arrived),
            Ok(Arrival::TooLong) => {
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
        // A request whose cancellation arrived first is answered as cancelled
        // rather than run. The id is taken out of the list, so a client that
        // reuses an id is not cancelled twice by one notification; and the flag
        // is cleared and `running` published in the same critical section, so
        // the reader thread's next decision is made against this request rather
        // than the one before it.
        match claim(&line, &state, &context) {
            Claim::Cancelled(id) => {
                let answer = error_response(id, -32800, "this request was cancelled.");
                writeln!(output, "{answer}").map_err(|error| error.to_string())?;
                output.flush().map_err(|error| error.to_string())?;
                continue;
            }
            Claim::Running => {}
        }
        let answered = handle_with_session(&mut context, &mut session, &line);
        if let Ok(mut held) = state.lock() {
            held.running = None;
        }
        if let Some(answer) = answered {
            let answer = enforce_response_budget(answer);
            writeln!(output, "{answer}").map_err(|error| error.to_string())?;
            output.flush().map_err(|error| error.to_string())?;
        }
    }
}

/// Replaces an oversized response with a JSON RPC error for the same request.
///
/// @param answer - the completed response before it is written to the client
fn enforce_response_budget(answer: String) -> String {
    if answer.len() <= MAX_RESPONSE_BYTES {
        return answer;
    }
    let id = response_id(&answer);
    error_response(
        id,
        -32603,
        &format!(
            "this answer would have been {} bytes, past the {MAX_RESPONSE_BYTES} a \
             reply may hold. Ask for fewer rows or fewer columns.",
            answer.len()
        ),
    )
}

/// What the reader thread found.
enum Arrival {
    /// One request line, whole.
    Line(String),
    /// The input ended.
    Ended,
    /// A line ran past [`MAX_REQUEST_BYTES`].
    TooLong,
}

/// Returns the request id a cancellation notification names.
///
/// `Some("")` for a cancellation with no `requestId`, which is not a shape the
/// protocol defines but is one a hand-written client sends: the flag is still
/// set for it, and no future request matches an empty id.
///
/// @param line - the request line as it arrived
fn cancellation_target(line: &str) -> Option<String> {
    let request = json::parse(line).ok()?;
    if request.get("method").and_then(Json::text) != Some("notifications/cancelled") {
        return None;
    }
    Some(
        request
            .get("params")
            .and_then(|params| params.get("requestId"))
            .map(id_text)
            .unwrap_or_default(),
    )
}

/// Returns a request id as the text two ids are compared by.
///
/// @param id - the id, as it arrived
fn id_text(id: &Json) -> String {
    match id {
        Json::Text(text) => text.clone(),
        Json::Int(number) => number.to_string(),
        Json::Real(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

/// What the two threads agree about, under one lock.
#[derive(Default)]
struct Cancellation {
    /// The id of the request the main thread is answering, if any.
    running: Option<String>,
    /// The ids of requests a cancellation named before they started.
    cancelled: Vec<String>,
}

/// What claiming a request decided.
enum Claim {
    /// A cancellation for it had already arrived; this is its id.
    Cancelled(Json),
    /// It is now the running request.
    Running,
}

/// Takes a request as the running one, or reports that it was cancelled first.
///
/// **The one critical section (task-1932, H11).** Clearing the flag, checking
/// the recorded ids and publishing `running` all happen here, so the reader
/// thread's next decision is made against this request. Splitting them is the
/// window a cancellation used to be lost in.
///
/// @param line - the request line as it arrived
/// @param state - what the two threads agree about
/// @param context - the session whose cancellation flag is being cleared
fn claim(line: &str, state: &std::sync::Mutex<Cancellation>, context: &Context) -> Claim {
    let Ok(request) = json::parse(line) else {
        return Claim::Running;
    };
    let Some(id) = request.get("id") else {
        // A notification has no id, so nothing can cancel it and nothing has
        // to be published about it.
        return Claim::Running;
    };
    let text = id_text(id);
    let Ok(mut held) = state.lock() else {
        return Claim::Running;
    };
    if let Some(at) = held.cancelled.iter().position(|named| *named == text) {
        held.cancelled.remove(at);
        return Claim::Cancelled(id.clone());
    }
    context
        .cancel_flag()
        .store(false, std::sync::atomic::Ordering::Relaxed);
    held.running = Some(text);
    Claim::Running
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
/// @param session - lifecycle state held for this client connection
/// @param line - the request, as it arrived
pub fn handle_with_session(
    context: &mut Context,
    session: &mut Session,
    line: &str,
) -> Option<String> {
    let request = match json::parse(line) {
        Ok(request) => request,
        // -32700 is JSON-RPC's parse error, and it is answered with a null id
        // because the id is exactly what could not be read.
        Err(why) => return Some(error_response(Json::Null, -32700, &why)),
    };
    let Json::Object(_) = request else {
        return Some(error_response(
            Json::Null,
            -32600,
            "a request must be an object.",
        ));
    };
    let id = request.get("id").cloned().unwrap_or(Json::Null);
    if !valid_request_id(&id) {
        return Some(error_response(
            Json::Null,
            -32600,
            "a request id must be a string, number, or null.",
        ));
    }
    if request.get("jsonrpc").and_then(Json::text) != Some("2.0") {
        return Some(error_response(id, -32600, "'jsonrpc' must be '2.0'."));
    }
    let Some(method) = request.get("method").and_then(Json::text) else {
        return Some(error_response(id, -32600, "a request needs a 'method'."));
    };
    // A notification has no id and takes no answer. Writing one anyway is the
    // most common way a hand-written server breaks a strict client.
    let is_notification = request.get("id").is_none();
    let params = request.get("params").cloned().unwrap_or(Json::Null);
    if request.get("params").is_some() && !matches!(params, Json::Object(_) | Json::Array(_)) {
        return Some(error_response(
            Json::Null,
            -32600,
            "request params must be an object or array.",
        ));
    }
    if method == "initialize" {
        if !matches!(session.lifecycle, Lifecycle::AwaitingInitialize) {
            return Some(error_response(
                id,
                -32600,
                "initialize was already completed.",
            ));
        }
        let result = initialize(&params);
        if result.is_ok() {
            session.lifecycle = Lifecycle::AwaitingInitializedNotification;
        }
        return response_for(id, is_notification, result);
    }
    if method == "notifications/initialized" {
        if matches!(
            session.lifecycle,
            Lifecycle::AwaitingInitializedNotification
        ) {
            session.lifecycle = Lifecycle::Ready;
            return None;
        }
        return response_for(
            id,
            is_notification,
            Err(Failed::misuse(
                "notifications/initialized must follow initialize.",
            )),
        );
    }
    if !matches!(session.lifecycle, Lifecycle::Ready) {
        return Some(error_response(
            id,
            -32002,
            "MCP initialization must complete before this method is used.",
        ));
    }
    let result = match method {
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
    response_for(id, is_notification, result)
}

/// Answers one request for callers that do not maintain a stdio session.
///
/// @param context - the open database
/// @param line - the request, as it arrived
pub fn handle(context: &mut Context, line: &str) -> Option<String> {
    let mut session = Session {
        lifecycle: Lifecycle::Ready,
    };
    handle_with_session(context, &mut session, line)
}

/// Returns whether a JSON-RPC request id has one of the permitted types.
///
/// @param id - the request id the client supplied or the null default
fn valid_request_id(id: &Json) -> bool {
    matches!(
        id,
        Json::Null | Json::Int(_) | Json::Real(_) | Json::Text(_)
    )
}

/// Renders a method result unless the request was a notification.
///
/// @param id - the request id to include in a response
/// @param is_notification - whether the request omitted its id member
/// @param result - the method result or parameter refusal
fn response_for(id: Json, is_notification: bool, result: Result<Json, Failed>) -> Option<String> {
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
fn initialize(params: &Json) -> Result<Json, Failed> {
    let Json::Object(_) = params else {
        return Err(Failed::misuse("initialize params must be an object."));
    };
    for (name, required) in [
        ("protocolVersion", true),
        ("capabilities", true),
        ("clientInfo", true),
    ] {
        if required && params.get(name).is_none() {
            return Err(Failed::misuse(format!("initialize needs '{name}'.")));
        }
    }
    if params.get("protocolVersion").and_then(Json::text).is_none() {
        return Err(Failed::misuse("'protocolVersion' has to be text."));
    }
    if !matches!(params.get("capabilities"), Some(Json::Object(_))) {
        return Err(Failed::misuse("'capabilities' has to be an object."));
    }
    if !matches!(params.get("clientInfo"), Some(Json::Object(_))) {
        return Err(Failed::misuse("'clientInfo' has to be an object."));
    }
    Ok(json::object(vec![
        ("protocolVersion", json::text(PROTOCOL)),
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
    ]))
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
    let arguments = Arguments::from_json(
        command,
        &params.get("arguments").cloned().unwrap_or(Json::Null),
    )?;
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

/// Extracts the JSON RPC id from a completed response.
///
/// @param response - the response that may need replacing because it is too large
fn response_id(response: &str) -> Json {
    json::parse(response)
        .ok()
        .and_then(|value| value.get("id").cloned())
        .unwrap_or(Json::Null)
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
        Context::open(":memory:", OpenMode::ReadWrite, None).expect("an in-memory database opens")
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

    /// Initialize selects the revision the server supports.
    #[test]
    fn initialize_selects_the_supported_version() {
        let mut session = Session::default();
        let answer = handle_with_session(
            &mut context(),
            &mut session,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\
             \"params\":{\"protocolVersion\":\"2099-01-01\",\"capabilities\":{},\
             \"clientInfo\":{\"name\":\"test\"}}}",
        )
        .unwrap_or_default();
        assert!(answer.contains(&format!("\"protocolVersion\":\"{PROTOCOL}\"")));
        assert!(answer.contains("\"name\":\"inillucent\""));
    }

    /// Invalid JSON RPC envelopes and initialize payloads return protocol errors.
    #[test]
    fn invalid_requests_and_initialize_payloads_are_refused() {
        for request in [
            "{\"id\":41,\"method\":\"ping\"}",
            "{\"jsonrpc\":\"1.0\",\"id\":42,\"method\":\"ping\"}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}",
        ] {
            let answer = handle(&mut context(), request).unwrap_or_default();
            assert!(answer.contains("\"error\""), "{answer}");
        }
    }

    /// JSON-RPC rejects scalar params and non scalar request ids with a null id.
    #[test]
    fn invalid_json_rpc_member_types_are_refused() {
        for request in [
            "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"ping\",\"params\":\"bad\"}",
            "{\"jsonrpc\":\"2.0\",\"id\":true,\"method\":\"ping\"}",
            "{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"ping\"}",
        ] {
            let answer = handle(&mut context(), request).unwrap_or_default();
            assert!(answer.contains("\"code\":-32600"), "{answer}");
            assert!(answer.contains("\"id\":null"), "{answer}");
        }
    }

    /// Normal methods wait for initialize and notifications/initialized.
    #[test]
    fn initialization_must_complete_before_normal_methods() {
        let mut held = context();
        let mut session = Session::default();
        let before = handle_with_session(
            &mut held,
            &mut session,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}",
        )
        .unwrap_or_default();
        assert!(before.contains("\"code\":-32002"), "{before}");
        let initialized = handle_with_session(
            &mut held,
            &mut session,
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\"}}}",
        )
        .unwrap_or_default();
        assert!(initialized.contains("\"result\""), "{initialized}");
        let waiting = handle_with_session(
            &mut held,
            &mut session,
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\"}",
        )
        .unwrap_or_default();
        assert!(waiting.contains("\"code\":-32002"), "{waiting}");
        assert!(handle_with_session(
            &mut held,
            &mut session,
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}",
        )
        .is_none());
        let listed = handle_with_session(
            &mut held,
            &mut session,
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/list\"}",
        )
        .unwrap_or_default();
        assert!(listed.contains("\"result\""), "{listed}");
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
            assert_eq!(schema.get("additionalProperties"), Some(&Json::Bool(false)));
        }
    }

    /// Tool arguments reject unknown names, wrong types, and disallowed text values.
    #[test]
    fn tool_arguments_are_checked_against_the_command_schema() {
        for arguments in [
            "{\"sql\":\"SELECT 1\",\"limit\":\"one\"}",
            "{\"sql\":\"SELECT 1\",\"limti\":1}",
            "{\"sql\":\"SELECT 1\",\"output\":\"yaml\"}",
        ] {
            let answer = handle(
                &mut context(),
                &format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"inillucent_query\",\"arguments\":{arguments}}}}}"),
            )
            .unwrap_or_default();
            assert!(answer.contains("\"code\":-32602"), "{answer}");
        }
        let query_schema = schema_of(command::find("query").unwrap_or(&command::COMMANDS[0]));
        let output = query_schema
            .get("properties")
            .and_then(|value| value.get("output"))
            .unwrap_or(&Json::Null)
            .write();
        assert!(output.contains("\"enum\":[\"text\",\"json\"]"), "{output}");
    }

    /// An oversized replacement response keeps the original JSON RPC id.
    #[test]
    fn response_budget_errors_keep_the_request_id() {
        let response = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":77,\"result\":\"{}\"}}",
            "x".repeat(MAX_RESPONSE_BYTES)
        );
        let replacement = enforce_response_budget(response);
        assert!(replacement.contains("\"id\":77"), "{replacement}");
        assert!(replacement.contains("\"code\":-32603"), "{replacement}");
    }

    /// A read-only server refuses a write and says why.
    #[test]
    fn read_only_refuses_a_write() {
        let mut held = Context::open(":memory:", OpenMode::ReadOnly, None).expect("opens");
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
