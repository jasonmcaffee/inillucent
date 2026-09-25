//! End to end tests: the real server, the real database, the real embedding model.
//!
//! Each test starts `rag-server serve` as a child process and talks to it over
//! stdin and stdout, exactly as Claude Code or opencode does. The corpus is a
//! handful of short articles copied out of `../corpus/greek-philosophy.jsonl`,
//! so a run embeds about 40 chunks and takes seconds.
//!
//! The embedding model has to be installed (`inillucent setup-embeddings all`).
//! Without it the first sync fails and the tests fail with the server's
//! message, which names the command to run.
//!
//! `cargo test -- --ignored` also runs the evaluation over the whole corpus,
//! which embeds about 3,700 chunks and takes several minutes.

mod support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};
use support::{corpus_lines, finish, scratch, titles, write_lines, Server};

/// The articles the sync test starts with. All five are short.
const FIRST_CORPUS: [&str; 5] = ["Protagoras", "Anaxarchus", "Diogenes of Sinope", "Socrates", "Zeno of Elea"];

/// How long the first sync of the small corpus may take, including loading the model.
const FIRST_SYNC: Duration = Duration::from_secs(300);

/// The protocol itself: the handshake, the tool list, and every kind of error.
///
/// The corpus is an empty folder and the timer is off, so this test never
/// loads the model. Every failure here is decided before a search runs.
#[test]
fn the_server_speaks_mcp_and_reports_errors() {
    let folder = scratch("protocol");
    let corpus = folder.join("empty");
    std::fs::create_dir_all(&corpus).expect("the empty corpus folder is created");
    let mut server = Server::start(&folder.join("protocol.rdb"), &corpus, "off");

    let init = server.initialize();
    assert_eq!(init["protocolVersion"], json!("2025-06-18"), "the server answers with the version the client asked for");
    assert_eq!(init["serverInfo"]["name"], json!("rag-server"));
    assert!(init["capabilities"]["tools"].is_object(), "the server offers tools: {init}");
    assert!(init["instructions"].as_str().is_some_and(|text| text.contains("search")));

    let newer = server.request("initialize", json!({ "protocolVersion": "1999-01-01", "capabilities": {} }));
    assert_eq!(newer["result"]["protocolVersion"], json!("2025-11-25"), "an unknown version gets the newest one");

    assert_eq!(server.request("ping", json!({}))["result"], json!({}));

    let listed = server.request("tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"].as_array().expect("a tools array").iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, vec!["search", "get_passage", "list_documents", "sync_status", "sync_now"]);
    for tool in listed["result"]["tools"].as_array().expect("a tools array") {
        assert_eq!(tool["inputSchema"]["type"], json!("object"), "{} has an object schema", tool["name"]);
    }

    let unknown_tool = server.request("tools/call", json!({ "name": "delete_everything", "arguments": {} }));
    assert_eq!(unknown_tool["error"]["code"], json!(-32602));
    assert_eq!(server.request("resources/list", json!({}))["error"]["code"], json!(-32601));

    server.send_line("{this is not json");
    let broken = server.next_reply();
    assert_eq!(broken["error"]["code"], json!(-32700));
    assert_eq!(broken["id"], Value::Null);

    assert_tool_error(&mut server, "search", json!({ "query": "   " }), "empty");
    assert_tool_error(&mut server, "search", json!({ "query": "Plato", "mode": "semantic" }), "unknown variant `semantic`");
    assert_tool_error(&mut server, "search", json!({ "query": "Plato", "k": 50 }), "from 1 to 20");
    assert_tool_error(&mut server, "search", json!({ "question": "Plato" }), "unknown field `question`");
    assert_tool_error(&mut server, "get_passage", json!({ "chunk_id": 999 }), "no chunk 999");

    // A notification gets no reply, so the next line the server writes answers the ping.
    server.notify("notifications/cancelled");
    assert_eq!(server.request("ping", json!({}))["result"], json!({}));
    finish(Some(server), &folder);
}

/// The whole life of an index: the first sync, every search mode, the overlap,
/// and a periodic sync that picks up an edit, a deletion and an addition.
#[test]
fn the_server_searches_the_corpus_and_keeps_it_in_sync() {
    let folder = scratch("sync");
    let corpus = folder.join("corpus.jsonl");
    write_lines(&corpus, &corpus_lines(&FIRST_CORPUS));
    let mut server = Server::start(&folder.join("sync.rdb"), &corpus, "3s");
    server.initialize();

    let status = server.wait_for_sync("the first sync", FIRST_SYNC, |s| report_count(s, "added") == 5);
    assert_tables_agree(&status);
    assert_eq!(status["last_report"]["errors"], json!([]));

    every_mode_finds_the_article(&mut server);
    a_rare_name_is_found_by_keyword(&mut server);
    a_title_limits_the_search(&mut server);
    punctuation_does_not_break_a_keyword_search(&mut server);
    overlap_is_stored_and_not_repeated(&mut server);
    distance_is_larger_for_a_question_on_another_subject(&mut server);
    a_periodic_sync_writes_only_what_changed(&mut server, &corpus);
    finish(Some(server), &folder);
}

/// Every mode puts Protagoras first for his best known saying.
fn every_mode_finds_the_article(server: &mut Server) {
    for mode in ["hybrid", "vector", "keyword", "rrf"] {
        let result = server.call_ok("search", json!({ "query": "man is the measure of all things", "mode": mode, "k": 3 }));
        assert_eq!(titles(&result).first().map(String::as_str), Some("Protagoras"), "mode {mode}: {result:#}");
        assert_eq!(result["mode"], json!(mode));
        assert_eq!(result["index"]["documents"], json!(5));
        let hit = &result["hits"][0];
        assert!(hit["url"].as_str().is_some_and(|url| url.starts_with("https://en.wikipedia.org/")));
        assert!(hit["chunk_id"].as_i64().is_some_and(|id| id > 0));
        if mode != "keyword" {
            assert!(hit["distance"].as_f64().is_some_and(|d| d < 0.4), "{mode} reports a small distance: {hit}");
        }
        match mode {
            "hybrid" | "keyword" => assert!(hit["confidence"].as_f64().is_some(), "{mode} reports confidence"),
            "rrf" => assert!(hit["rrf_score"].as_f64().is_some() && hit.get("vector_rank").is_some(), "rrf reports its ranks"),
            _ => {}
        }
    }
}

/// `Metrodorus` appears in the Anaxarchus article, and keyword mode finds it.
fn a_rare_name_is_found_by_keyword(server: &mut Server) {
    let result = server.call_ok("search", json!({ "query": "Metrodorus", "mode": "keyword", "k": 3 }));
    assert_eq!(titles(&result).first().map(String::as_str), Some("Anaxarchus"), "{result:#}");
    assert_eq!(result["hits"][0]["origin"], json!("lexical"));
}

/// A search limited to one title returns only that article's chunks.
fn a_title_limits_the_search(server: &mut Server) {
    for mode in ["hybrid", "vector", "keyword", "rrf"] {
        let result = server.call_ok("search", json!({ "query": "a philosopher in Athens", "mode": mode, "title": "Socrates" }));
        let found = titles(&result);
        assert!(!found.is_empty() && found.iter().all(|t| t == "Socrates"), "mode {mode}: {found:?}");
    }
}

/// A question with an apostrophe and a question mark is a valid keyword search.
fn punctuation_does_not_break_a_keyword_search(server: &mut Server) {
    let result = server.call_ok("search", json!({ "query": "what was Socrates' method?", "mode": "keyword" }));
    assert_eq!(result["keywords"], json!("\"socrates\" OR \"method\""));
    assert!(titles(&result).contains(&"Socrates".to_string()), "{result:#}");
}

/// Neighbouring chunks share text, and `get_passage` prints the shared text once.
fn overlap_is_stored_and_not_repeated(server: &mut Server) {
    let hit = server.call_ok("search", json!({ "query": "Protagoras agnosticism about the gods", "title": "Protagoras", "k": 1 }));
    let id = hit["hits"][0]["chunk_id"].as_i64().expect("a hit");
    let alone = server.call_ok("get_passage", json!({ "chunk_id": id, "neighbors": 0 }));
    assert_eq!(alone["text"], hit["hits"][0]["text"], "a passage with no neighbours is the chunk itself");

    let span = server.call_ok("get_passage", json!({ "chunk_id": id, "neighbors": 1 }));
    let ids: Vec<i64> = span["chunk_ids"].as_array().expect("chunk ids").iter().filter_map(Value::as_i64).collect();
    assert!(ids.len() >= 2, "Protagoras has more than one chunk: {span:#}");
    let span_text = span["text"].as_str().expect("text");
    let mut total = 0;
    for chunk in &ids {
        let piece = server.call_ok("get_passage", json!({ "chunk_id": chunk, "neighbors": 0 }));
        let piece_text = piece["text"].as_str().expect("text");
        assert!(span_text.contains(piece_text), "chunk {chunk} is inside the span");
        total += piece_text.len();
    }
    assert!(
        total > span_text.len(),
        "the chunks overlap ({total} bytes of chunks, {} of span), and the span has no repeats",
        span_text.len()
    );
}

/// The best hit's distance separates a covered question from one on another subject.
///
/// The default mode is `rrf`, and its hits carry the distance too.
fn distance_is_larger_for_a_question_on_another_subject(server: &mut Server) {
    let covered = server.call_ok("search", json!({ "query": "who was Protagoras" }));
    let unrelated = server.call_ok("search", json!({ "query": "how do I configure a Kubernetes ingress controller" }));
    assert_eq!(covered["mode"], json!("rrf"), "rrf is the default mode");
    let nearest =
        |result: &Value| result["hits"].as_array().into_iter().flatten().filter_map(|h| h["distance"].as_f64()).fold(f64::MAX, f64::min);
    let (covered_distance, unrelated_distance) = (nearest(&covered), nearest(&unrelated));
    assert!(covered_distance < 0.3, "covered {covered_distance}");
    assert!(unrelated_distance > 0.4, "unrelated {unrelated_distance}");
}

/// Edits the source, waits for the timer, and checks the sync touched only what changed.
///
/// Diogenes gains a sentence with a made up word, Zeno of Elea is removed and
/// Xenophanes is added. The other three articles must be skipped.
fn a_periodic_sync_writes_only_what_changed(server: &mut Server, corpus: &Path) {
    let mut lines = corpus_lines(&["Protagoras", "Anaxarchus", "Diogenes of Sinope", "Socrates", "Xenophanes"]);
    for line in lines.iter_mut() {
        let mut article: Value = serde_json::from_str(line).expect("a corpus line");
        if article["title"] == json!("Diogenes of Sinope") {
            let text = article["text"].as_str().unwrap_or("").to_string();
            article["text"] = json!(format!("{text} Diogenes also kept a tortoise named Zanzibarquux."));
            *line = article.to_string();
        }
    }
    // Write beside the corpus and rename, so a sync never reads half a file.
    let staged = corpus.with_extension("next");
    write_lines(&staged, &lines);
    std::fs::rename(&staged, corpus).expect("the corpus is replaced");

    let status = server.wait_for_sync("the edit to be synced", Duration::from_secs(120), |s| {
        report_count(s, "added") == 1 && report_count(s, "updated") == 1 && report_count(s, "removed") == 1
    });
    let report = &status["last_report"];
    assert_eq!(report["added"], json!(["Xenophanes"]));
    assert_eq!(report["updated"], json!(["Diogenes of Sinope"]));
    assert_eq!(report["removed"], json!(["Zeno of Elea"]));
    assert_eq!(report["unchanged"], json!(3), "the three untouched articles are skipped");
    let written = chunk_count(server, "Diogenes of Sinope") + chunk_count(server, "Xenophanes");
    assert_eq!(report["chunks_written"], json!(written), "only the changed and the added article are embedded");
    assert_tables_agree(&status);

    let found = server.call_ok("search", json!({ "query": "Zanzibarquux", "mode": "keyword" }));
    assert_eq!(titles(&found).first().map(String::as_str), Some("Diogenes of Sinope"), "{found:#}");
    let gone = server.call_ok("search", json!({ "query": "Zeno paradoxes", "title": "Zeno of Elea" }));
    assert_eq!(gone["hits"], json!([]), "the removed article has no chunks left");
    assert_eq!(server.call_ok("list_documents", json!({ "filter": "zeno" }))["count"], json!(0));

    let asked = server.call_ok("sync_now", json!({}));
    assert!(asked["started"].is_boolean(), "{asked}");
}

/// The whole corpus, every mode, against `questions.json`.
///
/// Builds a full index in a scratch folder with `rag-server sync`, then runs
/// `rag-server evaluate` and checks every mode finds most answers.
#[test]
#[ignore = "embeds the whole corpus, about 3,700 chunks; run with cargo test -- --ignored"]
fn the_whole_corpus_answers_the_evaluation_questions() {
    let folder = scratch("evaluate");
    let db = folder.join("full.rdb");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let binary = env!("CARGO_BIN_EXE_rag-server");
    let db_text = db.to_string_lossy().to_string();
    let corpus = manifest.join("../corpus/greek-philosophy.jsonl").to_string_lossy().to_string();
    let synced = Command::new(binary).args(["sync", "--db", &db_text, "--corpus", &corpus]).output().expect("sync runs");
    assert!(synced.status.success(), "sync failed: {}", String::from_utf8_lossy(&synced.stderr));
    let questions = manifest.join("questions.json").to_string_lossy().to_string();
    let evaluated = Command::new(binary).args(["evaluate", "--db", &db_text, "--questions", &questions]).output().expect("evaluate runs");
    let table = String::from_utf8_lossy(&evaluated.stdout).to_string();
    println!("{table}");
    for (mode, least) in [("hybrid", 17), ("vector", 17), ("keyword", 16), ("rrf", 18)] {
        let row = table.lines().find(|line| line.starts_with(&format!("| {mode} "))).expect("a row per mode");
        let found: usize = row.split('|').nth(2).and_then(|cell| cell.split_whitespace().next()).and_then(|n| n.parse().ok()).unwrap_or(0);
        assert!(found >= least, "{mode} found {found}, fewer than {least}: {table}");
    }
    finish(None, &folder);
}

/// Checks that a tool call fails as a tool error whose message contains some text.
///
/// @param server - the server
/// @param tool - the tool
/// @param arguments - arguments that are wrong
/// @param expected - text the message has to contain
fn assert_tool_error(server: &mut Server, tool: &str, arguments: Value, expected: &str) {
    let result = server.call(tool, arguments.clone());
    assert_eq!(result["isError"], json!(true), "{tool} {arguments} should fail: {result}");
    let message = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(message.contains(expected), "{tool} {arguments}: `{message}` does not say `{expected}`");
}

/// Returns how many titles the last sync report lists under a key.
///
/// @param status - the `sync_status` result
/// @param key - `added`, `updated` or `removed`
fn report_count(status: &Value, key: &str) -> usize {
    status["last_report"][key].as_array().map(Vec::len).unwrap_or(0)
}

/// Checks that both tables hold every chunk, each with a 768 number vector.
///
/// @param status - the `sync_status` result
fn assert_tables_agree(status: &Value) {
    let tables = &status["tables"];
    assert!(tables["chunk_rows"].as_i64().is_some_and(|n| n > 0), "{tables}");
    assert_eq!(tables["chunk_rows"], tables["chunk_vectors"], "every chunk has a vector: {tables}");
    assert_eq!(tables["chunk_rows"], tables["chunk_search_rows"], "chunk and chunk_search agree: {tables}");
    assert_eq!(tables["narrowest_vector"], json!(768));
    assert_eq!(tables["widest_vector"], json!(768));
}

/// Returns how many chunks `list_documents` reports for one title.
///
/// @param server - the server
/// @param title - the article
fn chunk_count(server: &mut Server, title: &str) -> i64 {
    let listed = server.call_ok("list_documents", json!({ "filter": title }));
    listed["documents"][0]["chunks"].as_i64().expect("the document is listed")
}
