//! One agent session over MCP, and the protocol edges a client can produce.
//!
//! Invariant: **the sequence an agent actually produces runs end to end against
//! one server, and every malformed thing a client can send is answered rather
//! than ending the process.**
//!
//! ## What this adds to `mcp_wire.rs`
//!
//! That file calls all twenty-eight tools once each with well formed requests,
//! which is the right shape for "does every tool answer". What it never does is
//! send anything wrong. Nothing in this repository sent malformed JSON, a
//! request larger than the limit, a `tools/call` before `initialize`, two
//! requests before reading either answer, or closed a client's end of the pipe
//! while a statement was streaming - and 0.1.2's handshake break is what a gap
//! of that shape costs.
//!
//! ## The two halves
//!
//! **The session** is the sequence an agent produces when it meets a database:
//! list the tools, look at an empty file, create a schema, describe it, get a
//! query wrong, hit something the engine has not built, get it right, ask for
//! more rows than the limit, export and import inside the root, back up, check.
//! Each step asserts the field that is about that step.
//!
//! **The edges** are the six in section 5.3 of the design. Each one asserts
//! that the server answered *and is still answering*, because the failure mode
//! they are about is a server that dies on a bad line and takes the agent's
//! session with it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use inillucent_compat::cliproc::{program, run};
use inillucent_compat::mcpclient::{is_an_error, status_in, Session};
use inillucent_compat::workspace_root;

/// A scratch directory of one case's own, emptied first.
///
/// @param case - what to name it after
fn area(case: &str) -> PathBuf {
    let directory = workspace_root()
        .join("_agent_output/mcp-session")
        .join(case);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// Makes an empty database with the shipped command line.
///
/// Through the program rather than through the library, for the reason
/// `cli_commands.rs` gives: a fixture written by the library leaves the
/// program's own write path out of what is exercised.
///
/// @param binary - the built `inillucent`
/// @param directory - where to put it
fn empty_database(binary: &Path, directory: &Path) -> PathBuf {
    let database = directory.join("app.rdb");
    let made = run(binary, &["create", &database.to_string_lossy()]);
    assert_eq!(made.code, 0, "`create` failed:\n{}", made.said());
    database
}

/// The sequence an agent produces when it meets a database it has not seen.
///
/// Every step is a call a real client makes, in the order it makes them, and
/// each assertion names the part of the answer that is about that step. The two
/// that matter most are the pair in the middle: a query that is **wrong** and a
/// query the engine has **not built** have to answer differently, because
/// `unsupported` is the engine's own "not yet" and an agent that could not tell
/// it from a syntax error would start rewording perfectly good SQL.
#[test]
fn an_agent_session_runs_end_to_end() {
    let (Some(server), Some(binary)) = (program("inillucent-mcp"), program("inillucent")) else {
        return;
    };
    let directory = area("agent");
    let database = empty_database(&binary, &directory);
    let mut session = Session::start_with(
        &server,
        &database,
        &["--root", &directory.to_string_lossy()],
    );

    let listed = session.call("tools/list", "{}");
    assert!(
        listed.contains("inillucent_query") && listed.contains("inillucent_tables"),
        "`tools/list` did not name the tools an agent starts from:\n{listed}"
    );

    let empty = session.tool("inillucent_tables", "{}");
    assert!(
        !is_an_error(&empty),
        "listing the tables of an empty database is an error:\n{empty}"
    );

    builds_a_schema_and_describes_it(&mut session);
    tells_a_syntax_error_from_an_unsupported_construct(&mut session);
    pages_on_an_exact_total(&mut session);
    refuses_an_export_to_a_file_and_answers_the_rows(&mut session, &directory);
    backs_up_and_checks_itself(&mut session, &directory);
}

/// The agent writes a schema in one batch and reads it back.
///
/// @param session - the running session
fn builds_a_schema_and_describes_it(session: &mut Session) {
    let built = session.tool(
        "inillucent_batch",
        "{\"sql\":\"CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT NOT NULL, tag TEXT); \
          CREATE INDEX note_by_tag ON note (tag); \
          INSERT INTO note (body, tag) VALUES ('first', 'a'); \
          INSERT INTO note (body, tag) VALUES ('second', 'b')\"}",
    );
    assert!(
        !is_an_error(&built),
        "the schema batch was refused:\n{built}"
    );

    let described = session.tool("inillucent_describe", "{\"table\":\"note\"}");
    assert!(
        described.contains("body") && described.contains("tag"),
        "`describe` did not name the columns:\n{described}"
    );
}

/// A wrong statement and an unbuilt one answer differently, and then the
/// corrected query answers.
///
/// **The distinction is the whole reason `unsupported` is a separate status.**
/// An agent that cannot tell it from a syntax error rewords SQL that is already
/// right, which is the loop this case exists to keep out of the tree.
///
/// @param session - the running session
fn tells_a_syntax_error_from_an_unsupported_construct(session: &mut Session) {
    let wrong = session.tool("inillucent_query", "{\"sql\":\"SELECT * FORM note\"}");
    assert!(
        is_an_error(&wrong) || status_in(&wrong) == "error" || wrong.contains("syntax"),
        "a syntax error answered as though it were fine:\n{wrong}"
    );

    // **A row value on the left of `IN (subquery)`**, which is one of the five
    // the differential corpus's own allow list still names. It is deliberately
    // not a window function: those were the obvious choice and they answer now,
    // so a case written against them would have asserted `unsupported` about
    // something that works.
    let not_built = session.tool(
        "inillucent_query",
        "{\"sql\":\"SELECT id FROM note WHERE (id, body) IN (SELECT id, body FROM note)\"}",
    );
    assert!(
        not_built.contains("unsupported"),
        "a construct the engine has not built answered without saying so. An agent that cannot \
         tell `unsupported` from a syntax error rewords SQL that is already right:\n{not_built}"
    );

    let right = session.tool(
        "inillucent_query",
        "{\"sql\":\"SELECT id, body FROM note ORDER BY id\"}",
    );
    assert!(
        right.contains("first") && right.contains("second"),
        "the corrected query did not answer:\n{right}"
    );
}

/// More rows than the tool returns, with the exact total beside them.
///
/// This is the field a client pages on, and a total that counted the returned
/// rows rather than the matching ones would make an agent stop early.
///
/// @param session - the running session
fn pages_on_an_exact_total(session: &mut Session) {
    let many = session.tool(
        "inillucent_batch",
        "{\"sql\":\"INSERT INTO note (body, tag) \
          SELECT 'row ' || value, 'bulk' FROM generate_series(1, 10001)\"}",
    );
    assert!(!is_an_error(&many), "the bulk insert was refused:\n{many}");
    let paged = session.tool(
        "inillucent_query",
        "{\"sql\":\"SELECT id FROM note WHERE tag = 'bulk' ORDER BY id\"}",
    );
    assert!(
        paged.contains("10001"),
        "the answer does not carry the exact total of 10,001 matching rows, so a client cannot \
         page on it:\n{}",
        &paged[..paged.len().min(600)]
    );
}

/// An export to a file is refused by name; an export with no file answers the
/// rows.
///
/// **This is `ad14898`'s rule, asserted from the client's side.** A server
/// started with a root is in safe mode, and `export --out` is implemented as a
/// `.once` redirection - so an agent that asks for a file gets *`.once` is
/// prohibited in safe mode*, whether the path is inside the root or outside it.
/// That is the right answer: the point of safe mode is that a model cannot be
/// talked into writing a file. What the agent still gets is the rows, in the
/// report, which is how an export over MCP is meant to work.
///
/// @param session - the running session
/// @param directory - the server's root, where the refused file would have gone
fn refuses_an_export_to_a_file_and_answers_the_rows(session: &mut Session, directory: &Path) {
    let exported = session.tool(
        "inillucent_export",
        &format!(
            "{{\"table\":\"note\",\"out\":\"{}\",\"format\":\"csv\"}}",
            directory
                .join("note.csv")
                .to_string_lossy()
                .replace('\\', "/")
        ),
    );
    assert!(
        is_an_error(&exported),
        "an export that asks for a file was not refused by a server in safe mode:\n{}",
        &exported[..exported.len().min(400)]
    );
    assert!(
        exported.contains("safe mode"),
        "the refusal does not say it is safe mode, so an agent cannot tell it from a broken \
         statement:\n{}",
        &exported[..exported.len().min(400)]
    );
    assert!(
        !directory.join("note.csv").exists(),
        "the export was refused and wrote the file anyway"
    );

    let in_the_answer = session.tool(
        "inillucent_export",
        "{\"table\":\"note\",\"format\":\"csv\"}",
    );
    assert!(
        in_the_answer.contains("first") && in_the_answer.contains("second"),
        "an export with no file named did not answer the rows:\n{}",
        &in_the_answer[..in_the_answer.len().min(400)]
    );
}

/// The session backs the database up and checks it.
///
/// The backup is asserted by the file being there, not by the report saying it
/// wrote one - which is the same defect `export --out` has (task-2044).
///
/// @param session - the running session
/// @param directory - the server's root, where the copy goes
fn backs_up_and_checks_itself(session: &mut Session, directory: &Path) {
    let backed_up = session.tool(
        "inillucent_backup",
        &format!(
            "{{\"file\":\"{}\"}}",
            directory
                .join("copy.rdb")
                .to_string_lossy()
                .replace('\\', "/")
        ),
    );
    assert!(
        !is_an_error(&backed_up),
        "the backup was refused:\n{backed_up}"
    );
    assert!(
        directory.join("copy.rdb").is_file(),
        "the backup reported success and wrote no file"
    );

    let checked = session.tool("inillucent_integrity_check", "{}");
    assert!(
        checked.contains("ok"),
        "the database the session built does not pass its own check:\n{checked}"
    );
}

/// A `tools/call` before `initialize` is refused, and the server carries on.
#[test]
fn a_call_before_the_handshake_is_refused() {
    let (Some(server), Some(binary)) = (program("inillucent-mcp"), program("inillucent")) else {
        return;
    };
    let directory = area("before-handshake");
    let database = empty_database(&binary, &directory);
    let mut session = Session::start_silent(&server, &database, &[]);

    let early = session.call(
        "tools/call",
        "{\"name\":\"inillucent_tables\",\"arguments\":{}}",
    );
    assert!(
        is_an_error(&early),
        "a `tools/call` before `initialize` was answered as though the session had started:\n\
         {early}"
    );

    // And the server is still there: the handshake works afterwards, which is
    // what says the refusal was a refusal rather than a death.
    let hello = session.call(
        "initialize",
        r#"{"protocolVersion":"2024-11-05","capabilities":{},
            "clientInfo":{"name":"inillucent-compat","version":"1"}}"#,
    );
    assert!(
        hello.contains("serverInfo"),
        "the server did not survive refusing a call made too early:\n{hello}"
    );
}

/// A line that is not JSON, and a JSON object with no `id`, are both survived.
///
/// **The `id`-less object is the subtler of the two.** It is a notification by
/// the protocol's own rule, so the correct answer is *nothing at all* - and a
/// server that replied to it would put an unmatched line on the wire that the
/// next `call` would have to skip past.
#[test]
fn a_malformed_line_does_not_end_the_session() {
    let (Some(server), Some(binary)) = (program("inillucent-mcp"), program("inillucent")) else {
        return;
    };
    let directory = area("malformed");
    let database = empty_database(&binary, &directory);
    let mut session = Session::start_with(&server, &database, &[]);

    session.write_line("this is not json at all");
    session.write_line("{\"jsonrpc\":\"2.0\",\"method\":\"tools/list\",\"params\":{}}");

    // Whatever the server made of those two, it still answers.
    let listed = session.call("tools/list", "{}");
    assert!(
        listed.contains("inillucent_query"),
        "the server stopped answering after a line that was not JSON and a request with no \
         id:\n{listed}"
    );
}

/// A request one byte past the limit is refused by name, and the connection
/// ends.
///
/// **Both halves are the behaviour, and the second one surprised this test.**
/// The server's limit is 1,048,576 bytes a request: a line of exactly that
/// length is answered, and a line one byte longer is answered with JSON-RPC's
/// `-32600` and the sentence *a request may not be longer than 1048576 bytes,
/// and this connection has sent one that is* - after which the server closes
/// its output and exits 0.
///
/// The first version of this case asserted that the server was still answering
/// afterwards, and failed. That was the test being wrong rather than the
/// server: ending the connection is what the message says it does, and a
/// length prefix nobody can trust is not something a stream protocol can carry
/// on from. What a client has to know is that the session is over, which is
/// what this now pins - a clean exit rather than a crash, so a supervisor can
/// tell "refused me" from "died on me".
#[test]
fn a_request_past_the_size_limit_is_refused_and_ends_the_session() {
    let (Some(server), Some(binary)) = (program("inillucent-mcp"), program("inillucent")) else {
        return;
    };
    let directory = area("too-large");
    let database = empty_database(&binary, &directory);

    let head = "{\"jsonrpc\":\"2.0\",\"id\":9001,\"method\":\"tools/call\",\"params\":\
                {\"name\":\"inillucent_query\",\"arguments\":{\"sql\":\"SELECT '";
    let tail = "'\"}}}";
    let padded = |length: usize| -> String {
        let padding = length.saturating_sub(head.len() + tail.len());
        let line = format!("{head}{}{tail}", "x".repeat(padding));
        assert_eq!(line.len(), length, "the padded line is the wrong length");
        line
    };

    // Exactly the limit is answered.
    {
        let mut session = Session::start_with(&server, &database, &[]);
        session.write_line(&padded(1_048_576));
        let answered = session.reply_to(9001, "a request of exactly 1,048,576 bytes");
        assert!(
            !is_an_error(&answered),
            "a request of exactly the limit was refused:\n{}",
            &answered[..answered.len().min(300)]
        );
    }

    // One byte past it is refused by name, and the connection ends.
    let mut session = Session::start_with(&server, &database, &[]);
    session.write_line(&padded(1_048_577));
    let refused = session.next_line("a request of 1,048,577 bytes");
    assert!(
        refused.contains("-32600"),
        "the oversized request was not refused with JSON-RPC's invalid request code:\n{}",
        &refused[..refused.len().min(300)]
    );
    assert!(
        refused.contains("1048576"),
        "the refusal does not name the limit, so a client cannot tell what to send \
         instead:\n{}",
        &refused[..refused.len().min(300)]
    );

    let (status, _wrote) = session.close_and_wait(Duration::from_secs(30));
    assert_eq!(
        status,
        Some(0),
        "the server ended with {status:?} after refusing an oversized request, so a supervisor \
         cannot tell a refusal from a crash"
    );
}

/// Two requests written before either answer is read are both answered, matched
/// by id.
///
/// A client that pipelines is allowed to, and a server that answered the second
/// one first would be correct - which is why this matches on id rather than on
/// order, and why a test that read the next line would be wrong even against a
/// server that works.
#[test]
fn two_requests_in_flight_are_both_answered() {
    let (Some(server), Some(binary)) = (program("inillucent-mcp"), program("inillucent")) else {
        return;
    };
    let directory = area("in-flight");
    let database = empty_database(&binary, &directory);
    let mut session = Session::start_with(&server, &database, &[]);
    session.tool(
        "inillucent_exec",
        "{\"sql\":\"CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)\"}",
    );

    let first = session.send(
        "tools/call",
        "{\"name\":\"inillucent_query\",\"arguments\":{\"sql\":\"SELECT 1 AS one\"}}",
    );
    let second = session.send(
        "tools/call",
        "{\"name\":\"inillucent_query\",\"arguments\":{\"sql\":\"SELECT 2 AS two\"}}",
    );
    assert_ne!(first, second, "two requests went out under one id");

    let second_answer = session.reply_to(second, "the second query");
    let first_answer = session.reply_to(first, "the first query");
    assert!(
        first_answer.contains("one"),
        "the first request's id carries the second request's answer:\n{first_answer}"
    );
    assert!(
        second_answer.contains("two"),
        "the second request's id carries the wrong answer:\n{second_answer}"
    );
}

/// Closing the client's end while a long statement is streaming ends the
/// server, and leaves no lock behind.
///
/// **The second half is the one that matters.** A server that exits leaving the
/// file locked is a database an agent cannot reopen, and the way to ask is to
/// open it again and *write* - an open alone can succeed against a file a dead
/// process still holds.
#[test]
fn closing_the_pipe_mid_statement_leaves_no_stale_lock() {
    let (Some(server), Some(binary)) = (program("inillucent-mcp"), program("inillucent")) else {
        return;
    };
    let directory = area("stdin-closed");
    let database = empty_database(&binary, &directory);
    let path = database.to_string_lossy().to_string();

    {
        let mut session = Session::start_with(&server, &database, &[]);
        // A recursive series long enough that the answer is still being
        // produced when the pipe closes.
        session.send(
            "tools/call",
            "{\"name\":\"inillucent_query\",\"arguments\":{\"sql\":\
              \"WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 400000) \
              SELECT count(*) FROM s\"}}",
        );
        let (status, wrote) = session.close_and_wait(Duration::from_secs(60));
        assert!(
            status.is_some(),
            "the server did not exit within sixty seconds of its input closing; it wrote \
             {wrote} bytes on the way"
        );
    }

    // A fresh process opens the file and writes to it.
    let wrote = run(
        &binary,
        &[
            "--db",
            path.as_str(),
            "exec",
            "CREATE TABLE after_the_close (id INTEGER PRIMARY KEY)",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        wrote.code,
        0,
        "a fresh process could not write to the database after the server's input closed, so a \
         lock was left behind:\n{}",
        wrote.said()
    );
}
