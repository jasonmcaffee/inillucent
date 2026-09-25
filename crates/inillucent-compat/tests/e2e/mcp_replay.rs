//! Two recorded clients, replayed against the server that is built now.
//!
//! Invariant: **what a real client sent once, the server still answers the same
//! way.** The transcripts are recorded by `tools/record-mcp-transcript.py`
//! sitting between a client and a live server, so what they hold is bytes a
//! client actually produced rather than this project's idea of what one
//! produces.
//!
//! ## The escape
//!
//! 0.1.2. The release scripts sent a handshake the server refused, and the
//! release's own smoke test was what found it - after the release. Everything
//! in the suite that spoke MCP spoke it the way the suite wrote it, so a change
//! to what the server required broke the one client nobody had written down.
//! `crates/inillucent-compat/tests/fixtures/mcp/release-smoke.transcript` is
//! now that client, line for line from `packaging/release.sh`.
//!
//! ## What is compared, and what is not
//!
//! Every reply is compared after masking: the request id, because the recording
//! client chose it, and the timing fields, because a fixture that carried a
//! duration would expire the first time the machine was busy.
//!
//! Beyond the mask, the comparison is by what a client acts on:
//!
//! - **the handshake**, byte for byte on `protocolVersion` and on the presence
//!   of `serverInfo`. This is the field 0.1.2 broke.
//! - **the tool names**, as a set that may grow and may not shrink. A tool
//!   added is a release; a tool that stops being listed is every client that
//!   called it. The *descriptions* are prose and are deliberately not compared:
//!   a fixture that went red when somebody improved a sentence would be
//!   re-recorded without being read, which is worse than not having it.
//! - **every `tools/call` answer, in full**, after masking. That is the part a
//!   client reads, and prose churn does not reach it.
//! - **whether each reply was a refusal**, which is the one field that turns a
//!   working client into a broken one without changing anything it prints.
//!
//! ## When a transcript goes stale
//!
//! Re-record it: `sh tools/record-mcp-fixtures.sh`. The failure message says
//! so and names the file. What it must not be is deleted - a client nobody has
//! written down is how 0.1.2 happened.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use inillucent_compat::cliproc::{program, run};
use inillucent_compat::mcpclient::{is_an_error, masked, Session};
use inillucent_compat::workspace_root;

/// One line of a transcript.
struct Crossed {
    /// `true` when the client sent it, `false` when the server did.
    from_the_client: bool,
    /// The JSON, as it crossed.
    line: String,
}

/// Reads a transcript, dropping its comments.
///
/// @param name - the fixture's file name
fn transcript(name: &str) -> Vec<Crossed> {
    let path = workspace_root()
        .join("crates/inillucent-compat/tests/fixtures/mcp")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|why| {
        panic!(
            "{}: {why}. Record it with `sh tools/record-mcp-fixtures.sh`.",
            path.display()
        )
    });
    let mut crossed = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let (tag, rest) = line.split_at(1);
        match tag {
            "C" => crossed.push(Crossed {
                from_the_client: true,
                line: rest.trim().to_string(),
            }),
            "S" => crossed.push(Crossed {
                from_the_client: false,
                line: rest.trim().to_string(),
            }),
            other => panic!(
                "{}: a line begins `{other}`, which is neither C nor S: {line}",
                path.display()
            ),
        }
    }
    assert!(
        crossed.len() >= 4,
        "{} holds {} lines, which is not a session",
        path.display(),
        crossed.len()
    );
    crossed
}

/// The id a line carries, or nothing for a notification.
///
/// @param line - the JSON
fn id_of(line: &str) -> Option<u64> {
    let at = line.find("\"id\":")? + "\"id\":".len();
    let rest = line.get(at..)?;
    let digits: String = rest
        .chars()
        .take_while(|one| one.is_ascii_digit())
        .collect();
    digits.parse::<u64>().ok()
}

/// The method a request names, or nothing for a reply.
///
/// @param line - the JSON
fn method_of(line: &str) -> Option<String> {
    let at = line.find("\"method\":\"")? + "\"method\":\"".len();
    let rest = line.get(at..)?;
    rest.split('"').next().map(str::to_string)
}

/// Every `"name":"..."` in a `tools/list` reply.
///
/// @param line - the reply
fn tool_names(line: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut rest = line;
    while let Some(at) = rest.find("\"name\":\"") {
        let after = rest.split_at(at + "\"name\":\"".len()).1;
        if let Some(name) = after.split('"').next() {
            if name.starts_with("inillucent_") {
                names.insert(name.to_string());
            }
        }
        rest = after;
    }
    names
}

/// The `protocolVersion` a handshake reply reports.
///
/// @param line - the reply
fn protocol_version(line: &str) -> Option<String> {
    let at = line.find("\"protocolVersion\":\"")? + "\"protocolVersion\":\"".len();
    line.get(at..)?.split('"').next().map(str::to_string)
}

/// Builds the database both transcripts were recorded against.
///
/// The same statements `tools/record-mcp-fixtures.sh` runs, because the replay
/// has to ask the same questions of the same rows - a `SELECT count(*) FROM t`
/// answering 2 in the recording and 3 here would be a difference in the fixture
/// rather than in the server.
///
/// @param binary - the built `inillucent`
/// @param directory - where to put it
fn recorded_database(binary: &Path, directory: &Path) -> PathBuf {
    let database = directory.join("smoke.rdb");
    let path = database.to_string_lossy().to_string();
    let made = run(binary, &["create", path.as_str()]);
    assert_eq!(made.code, 0, "`create` failed:\n{}", made.said());
    for statement in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)",
        "INSERT INTO t (body) VALUES ('one')",
        "INSERT INTO t (body) VALUES ('two')",
    ] {
        let ran = run(binary, &["--db", path.as_str(), "exec", statement]);
        assert_eq!(ran.code, 0, "`{statement}` failed:\n{}", ran.said());
    }
    database
}

/// Replays one transcript and compares the server's half.
///
/// @param name - the fixture's file name
fn replay(name: &str) {
    let (server, binary) = (program("inillucent-mcp"), program("inillucent"));
    let directory = workspace_root()
        .join("_agent_output/mcp-replay")
        .join(name.replace('.', "-"));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let database = recorded_database(&binary, &directory);

    let crossed = transcript(name);
    let mut session = Session::start_silent(&server, &database, &[]);

    // The client's half, in the order it was sent.
    let mut asked: Vec<(u64, String)> = Vec::new();
    for one in crossed.iter().filter(|one| one.from_the_client) {
        session.write_line(&one.line);
        if let (Some(id), Some(method)) = (id_of(&one.line), method_of(&one.line)) {
            asked.push((id, method));
        }
    }
    assert!(
        !asked.is_empty(),
        "{name} holds no request with an id, so this replay sent nothing that can be matched"
    );

    let recorded: Vec<&Crossed> = crossed.iter().filter(|one| !one.from_the_client).collect();
    assert_eq!(
        recorded.len(),
        asked.len(),
        "{name} recorded {} answers to {} requests. Re-record it: `sh \
         tools/record-mcp-fixtures.sh`.",
        recorded.len(),
        asked.len()
    );

    let mut compared = 0usize;
    for (id, method) in &asked {
        let answered = session.reply_to(*id, method);
        let Some(was) = recorded.iter().find(|one| id_of(&one.line) == Some(*id)) else {
            panic!("{name} has no recorded answer for {method} (id {id})");
        };
        compared += 1;

        assert_eq!(
            is_an_error(&answered),
            is_an_error(&was.line),
            "{name}: {method} is now {} where the recording has it {}. A client that worked \
             against the recording does not work against this build.\n  now: {}\n  was: {}",
            if is_an_error(&answered) {
                "refused"
            } else {
                "answered"
            },
            if is_an_error(&was.line) {
                "refused"
            } else {
                "answered"
            },
            &answered[..answered.len().min(300)],
            &was.line[..was.line.len().min(300)]
        );

        match method.as_str() {
            // The field 0.1.2 broke.
            "initialize" => {
                assert_eq!(
                    protocol_version(&answered),
                    protocol_version(&was.line),
                    "{name}: the server answers a different protocolVersion than it did when \
                     this client was recorded, which is what 0.1.2 shipped"
                );
                assert!(
                    answered.contains("serverInfo"),
                    "{name}: the handshake carries no serverInfo:\n{answered}"
                );
            }
            // A tool may be added and may not vanish.
            "tools/list" => {
                let then = tool_names(&was.line);
                let now = tool_names(&answered);
                assert!(
                    then.len() >= 20,
                    "{name}: the recording lists {} tools, so the names are not being read",
                    then.len()
                );
                let gone: Vec<&String> = then.difference(&now).collect();
                assert!(
                    gone.is_empty(),
                    "{name}: these tools were listed when this client was recorded and are not \
                     listed now, so every client that calls one is broken:\n  {gone:?}"
                );
            }
            // The part a client reads, in full.
            "tools/call" => {
                assert_eq!(
                    masked(answered.trim()),
                    masked(was.line.trim()),
                    "{name}: {method} (id {id}) answers differently than it did when this \
                     client was recorded. If the change is intended, re-record: `sh \
                     tools/record-mcp-fixtures.sh`."
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        compared,
        asked.len(),
        "{name}: {compared} of {} replies were compared",
        asked.len()
    );
}

/// The release smoke test's client still gets what it got.
///
/// **This is the test that would have stopped 0.1.2.** The four lines are
/// `packaging/release.sh`'s own, and a handshake the server stops accepting
/// fails here rather than at the end of a release.
#[test]
fn the_release_smoke_tests_client_still_works() {
    replay("release-smoke.transcript");
}

/// An agent client's session still gets what it got.
#[test]
fn an_agent_clients_session_still_works() {
    replay("claude-code.transcript");
}

/// The release script's handshake is the one the fixture holds.
///
/// **The fixture is a copy, and a copy goes stale.** If somebody changes the
/// handshake `packaging/release.sh` sends and does not re-record, the replay
/// goes on checking a client that no longer exists - which is the shape of the
/// gap 0.1.2 fell through, one level up.
#[test]
fn the_fixture_is_the_handshake_the_release_script_sends() {
    let script = workspace_root().join("packaging/release.sh");
    let text = std::fs::read_to_string(&script)
        .unwrap_or_else(|why| panic!("{}: {why}", script.display()));
    let Some(sent) = text
        .lines()
        .map(str::trim)
        .find(|line| line.contains("\"method\":\"initialize\""))
    else {
        panic!(
            "{} no longer sends an `initialize`, so the fixture recorded from it describes \
             nothing",
            script.display()
        );
    };
    let handshake = sent
        .trim_start_matches('\'')
        .trim_end_matches("' \\")
        .trim();

    let recorded = transcript("release-smoke.transcript");
    let first = recorded
        .iter()
        .find(|one| one.from_the_client)
        .map(|one| one.line.as_str())
        .unwrap_or_default();
    assert_eq!(
        first, handshake,
        "crates/inillucent-compat/tests/fixtures/mcp/release-smoke.transcript was recorded from \
         a handshake packaging/release.sh no longer sends, so the replay is checking a client \
         that does not exist. Re-record it: `sh tools/record-mcp-fixtures.sh`."
    );
}
