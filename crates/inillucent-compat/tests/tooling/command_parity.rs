//! The CLI and the MCP server offer the same commands, and both are described.
//!
//! Invariant: **`inillucent <verb>` and `inillucent_<verb>` are the same set,
//! and every difference is a stated one.** The whole design is that the
//! command table is read by three front ends rather than copied into them, and
//! a design like that is worth exactly as much as the test that keeps it true.
//! `drivers/README.md` makes the same argument about the capability table:
//! `java.sql.DatabaseMetaData` has had `supportsFullOuterJoins()` since 1997
//! and its answers are famously unreliable, because every driver hand-writes
//! them and nothing runs them.
//!
//! So this asserts, in both directions:
//!
//! 1. every command has a summary, a detail, and a description on every
//!    parameter - because those sentences are what a model reads, and an empty
//!    one is a tool nobody can call correctly;
//! 2. the MCP tool names are exactly the commands that are not `cli_only`;
//! 3. every `cli_only` command says *why*, because an exclusion nobody has to
//!    justify is an exclusion that grows;
//! 4. every schema declares the command's required parameters and no others;
//! 5. `capabilities` answers the same rows the driver's own checked table
//!    holds, so the command cannot drift from the thing it reports.

use inillucent_cli::command::{self, Context, OpenMode};
use inillucent_cli::json::Json;
use inillucent_cli::mcp;

/// Returns a scratch context for a command that has to run.
fn context() -> Context {
    Context::open(":memory:", OpenMode::ReadWrite, None).expect("an in-memory database opens")
}

/// Every command carries the sentences a caller needs to use it.
#[test]
fn every_command_and_parameter_is_described() {
    for command in command::COMMANDS {
        assert!(
            !command.summary.trim().is_empty(),
            "{} has no summary, and the summary is the MCP tool description",
            command.name
        );
        assert!(
            !command.detail.trim().is_empty(),
            "{} has no detail",
            command.name
        );
        assert!(
            command.summary.len() < 120,
            "{}'s summary is a paragraph; the list has to stay readable",
            command.name
        );
        for param in command.params {
            assert!(
                !param.description.trim().is_empty(),
                "{}.{} has no description",
                command.name,
                param.name
            );
            assert!(
                param.description.len() > 20,
                "{}.{}'s description restates its name and tells a caller nothing",
                command.name,
                param.name
            );
        }
    }
}

/// The tools are the commands, and the commands are the tools.
#[test]
fn the_two_surfaces_name_the_same_set() {
    let served: Vec<String> = mcp::tools()
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Json::text).map(str::to_string))
        .collect();
    let expected: Vec<String> = command::COMMANDS
        .iter()
        .filter(|command| command.cli_only.is_none())
        .map(mcp::tool_name)
        .collect();
    assert_eq!(
        served, expected,
        "the MCP surface has drifted from the table"
    );

    // And the other direction: every served tool resolves back to a command,
    // which is what would fail if a name were ever built rather than taken.
    for name in &served {
        assert!(
            command::find(name).is_some(),
            "{name} is served but is not a command"
        );
    }
}

/// An excluded command says why it is excluded.
#[test]
fn every_exclusion_is_argued_for() {
    let excluded: Vec<&str> = command::COMMANDS
        .iter()
        .filter(|command| command.cli_only.is_some())
        .map(|command| command.name)
        .collect();
    assert_eq!(
        excluded,
        vec!["shell", "mcp"],
        "a command was hidden from MCP. If that is right, this list is what says so."
    );
    for command in command::COMMANDS {
        if let Some(reason) = command.cli_only {
            assert!(
                reason.len() > 40,
                "{}'s cli_only reason is too short to be a reason",
                command.name
            );
        }
    }
}

/// A schema declares what the command actually requires.
#[test]
fn a_schema_matches_its_command() {
    for command in command::COMMANDS {
        let schema = mcp::schema_of(command);
        let properties = schema.get("properties").cloned().unwrap_or(Json::Null);
        for param in command.params {
            let declared = properties.get(param.name);
            assert!(
                declared.is_some(),
                "{}'s schema is missing {}",
                command.name,
                param.name
            );
            assert_eq!(
                declared
                    .and_then(|member| member.get("type"))
                    .and_then(Json::text),
                Some(param.kind.schema_type()),
                "{}.{} is declared as the wrong type",
                command.name,
                param.name
            );
        }
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
            "{} declares the wrong required parameters",
            command.name
        );
    }
}

/// The `capabilities` command answers the driver's own checked table.
///
/// The point is that there is one table. `drivers/inillucent-driver/tests/
/// capability.rs` probes every row against a running engine and fails in both
/// directions; this asserts the command reports what that test protects, rather
/// than a list of its own that nothing probes.
#[test]
fn capabilities_reports_the_checked_table() {
    let command = command::find("capabilities").expect("the command exists");
    let produced = command::run(command, &mut context(), &command::Arguments::default())
        .expect("the capability table needs no database");
    let reported: Vec<String> = produced
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(Json::text).map(str::to_string))
        .collect();
    let held: Vec<String> = inillucent_driver::CAPABILITIES
        .iter()
        .map(|entry| entry.name.to_string())
        .collect();
    assert_eq!(reported, held);
    assert!(
        !held.is_empty(),
        "an empty capability table would pass every other assertion here"
    );
}

/// Every command that is served runs, or refuses for a reason a caller can read.
///
/// It calls each tool with no arguments at all, which is the least valid call a
/// client can make. Nothing may panic, and nothing may come back with an empty
/// message: an agent that receives a blank refusal has been told only that
/// something went wrong.
#[test]
fn every_command_answers_an_empty_call() {
    let mut held = context();
    for command in command::COMMANDS {
        if command.cli_only.is_some() {
            continue;
        }
        match command::run(command, &mut held, &command::Arguments::default()) {
            Ok(produced) => assert_eq!(
                produced.command, command.name,
                "{} reported itself as {}",
                command.name, produced.command
            ),
            Err(failure) => {
                assert!(
                    !failure.message.trim().is_empty(),
                    "{} refused with no message",
                    command.name
                );
                assert!(
                    failure.message.len() > 10,
                    "{} refused with '{}', which tells a caller nothing",
                    command.name,
                    failure.message
                );
            }
        }
    }
}

/// `inillucent help` lists every command in the table.
#[test]
fn help_lists_everything() {
    let command = command::find("help").expect("the command exists");
    let produced = command::run(command, &mut context(), &command::Arguments::default())
        .expect("help needs no database");
    let listed: Vec<String> = produced
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(Json::text).map(str::to_string))
        .collect();
    let expected: Vec<String> = command::COMMANDS
        .iter()
        .map(|command| command.name.to_string())
        .collect();
    assert_eq!(listed, expected);
}

/// The MCP server answers a real session: initialize, list, call, in order.
#[test]
fn a_session_runs_end_to_end() {
    let mut held = context();
    let mut session = mcp::Session::default();
    let opening = held_answer(
        &mut held,
        &mut session,
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\
         \"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\
         \"clientInfo\":{\"name\":\"command-parity\"}}}",
    );
    assert!(opening.contains("\"serverInfo\""));
    assert!(opening.contains("\"protocolVersion\":\"2025-06-18\""));
    assert!(
        opening.contains("\"tools\""),
        "a server that declares no tools capability is one a client will not call"
    );
    assert!(mcp::handle_with_session(
        &mut held,
        &mut session,
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}",
    )
    .is_none());
    let listed = held_answer(
        &mut held,
        &mut session,
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}",
    );
    for command in command::COMMANDS.iter().filter(|c| c.cli_only.is_none()) {
        assert!(
            listed.contains(&mcp::tool_name(command)),
            "{} is missing from tools/list",
            command.name
        );
    }
    assert!(mcp::handle_with_session(
        &mut held,
        &mut session,
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}",
    )
    .is_none());
    let made = held_answer(
        &mut held,
        &mut session,
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\
         \"name\":\"inillucent_batch\",\"arguments\":{\"sql\":\
         \"CREATE TABLE t (a INTEGER, b TEXT); INSERT INTO t VALUES (1,'one'),(2,'two')\"}}}",
    );
    assert!(made.contains("\"isError\":false"), "{made}");
    let read = held_answer(
        &mut held,
        &mut session,
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\
         \"name\":\"inillucent_query\",\"arguments\":{\"sql\":\
         \"SELECT b FROM t WHERE a = ?1\",\"params\":[2]}}}",
    );
    assert!(read.contains("two"), "{read}");
    assert!(
        !read.contains("one"),
        "the bound parameter was ignored: {read}"
    );
}

/// Sends one request to the server and returns its answer.
///
/// @param context - the open database
/// @param session - state retained for the client connection
/// @param request - the JSON-RPC request
fn held_answer(context: &mut Context, session: &mut mcp::Session, request: &str) -> String {
    mcp::handle_with_session(context, session, request)
        .unwrap_or_else(|| panic!("no answer to {request}"))
}
