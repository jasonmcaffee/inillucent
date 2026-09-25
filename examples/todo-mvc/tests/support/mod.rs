//! Starts the built `todo-server` and calls it over HTTP, the way a client does.
//!
//! Nothing here reaches into the server's code. A test sees only what an HTTP
//! client sees: a status code and a JSON body. That makes these tests end to
//! end: the real binary, the real database file, the real socket.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::channel;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// How long the server may take to open the database and bind its socket.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// A running server.
pub struct Server {
    child: Child,
    /// `http://127.0.0.1:<port>`.
    pub base: String,
}

impl Server {
    /// Starts `todo-server serve` on a database, on a free port.
    ///
    /// The server prints `listening on http://<address>` once its socket is
    /// bound, and this waits for that line, so the first request never races
    /// the start.
    ///
    /// @param db - the database file; created when it does not exist
    pub fn start(db: &Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_todo-server"))
            .args(["serve", "--db", &db.to_string_lossy(), "--addr", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("todo-server starts");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (send, receive) = channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            if let Some(Ok(line)) = lines.next() {
                let _ = send.send(line);
            }
            for _ in lines {}
        });
        let line = receive.recv_timeout(START_TIMEOUT).unwrap_or_else(|_| panic!("the server printed nothing in {START_TIMEOUT:?}"));
        let base = line.strip_prefix("listening on ").unwrap_or_else(|| panic!("unexpected first line: {line}")).to_string();
        Server { child, base }
    }

    /// Sends a request and returns the status and the JSON body, whatever the status.
    ///
    /// @param method - `GET`, `POST`, `PATCH`, `PUT` or `DELETE`
    /// @param path - the path and query string, such as `/lists/1/todos?status=active`
    /// @param body - the JSON body, or nothing
    pub fn send(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let request = ureq::request(method, &format!("{}{path}", self.base));
        let outcome = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        };
        let response = match outcome {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(error) => panic!("{method} {path} did not get an answer: {error}"),
        };
        let status = response.status();
        let text = response.into_string().expect("the body is text");
        let json = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{method} {path} answered {status} with a body that is not JSON ({error}): {text}"));
        (status, json)
    }

    /// Sends a request that has to succeed, and returns its body.
    ///
    /// @param method - the HTTP method
    /// @param path - the path and query string
    /// @param body - the JSON body, or nothing
    pub fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, json) = self.send(method, path, body.clone());
        assert!((200..300).contains(&status), "{method} {path} {body:?} answered {status}: {json:#}");
        json
    }

    /// `GET` that has to succeed.
    ///
    /// @param path - the path and query string
    pub fn get(&self, path: &str) -> Value {
        self.ok("GET", path, None)
    }

    /// `POST` that has to succeed.
    ///
    /// @param path - the path
    /// @param body - the JSON body
    pub fn post(&self, path: &str, body: Value) -> Value {
        self.ok("POST", path, Some(body))
    }

    /// `PATCH` that has to succeed.
    ///
    /// @param path - the path
    /// @param body - the JSON body
    pub fn patch(&self, path: &str, body: Value) -> Value {
        self.ok("PATCH", path, Some(body))
    }

    /// `PUT` that has to succeed.
    ///
    /// @param path - the path
    /// @param body - the JSON body
    pub fn put(&self, path: &str, body: Value) -> Value {
        self.ok("PUT", path, Some(body))
    }

    /// `DELETE` that has to succeed.
    ///
    /// @param path - the path
    pub fn delete(&self, path: &str) -> Value {
        self.ok("DELETE", path, None)
    }

    /// Sends a request that has to fail with a given status, and returns the error body.
    ///
    /// @param status - the status expected
    /// @param method - the HTTP method
    /// @param path - the path and query string
    /// @param body - the JSON body, or nothing
    pub fn fails(&self, status: u16, method: &str, path: &str, body: Option<Value>) -> Value {
        let (got, json) = self.send(method, path, body.clone());
        assert_eq!(got, status, "{method} {path} {body:?} answered {got}, expected {status}: {json:#}");
        assert!(json["error"].is_string() && json["message"].is_string(), "the error body has `error` and `message`: {json}");
        json
    }

    /// Adds a person and returns their id.
    ///
    /// @param name - the name, also used for the email address
    pub fn person(&self, name: &str) -> i64 {
        id(&self.post("/people", json!({ "name": name, "email": format!("{}@example.com", name.to_lowercase()) })))
    }

    /// Adds a list and returns its id.
    ///
    /// @param owner - the owner's id
    /// @param name - the list's name
    pub fn list(&self, owner: i64, name: &str) -> i64 {
        id(&self.post("/lists", json!({ "owner_id": owner, "name": name })))
    }

    /// Adds a todo and returns its id.
    ///
    /// @param list - the list's id
    /// @param body - the todo, at least `{"title": ...}`
    pub fn todo(&self, list: i64, body: Value) -> i64 {
        id(&self.post(&format!("/lists/{list}/todos"), body))
    }
}

impl Drop for Server {
    /// Stops the server, so a failed test does not leave a process holding the database open.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads the `id` field of a body.
///
/// @param body - a created row
pub fn id(body: &Value) -> i64 {
    body["id"].as_i64().unwrap_or_else(|| panic!("no id in {body}"))
}

/// Reads the titles of the todos in a body that has a `todos` array.
///
/// @param body - a `GET /lists/{id}/todos` answer
pub fn titles(body: &Value) -> Vec<String> {
    body["todos"].as_array().expect("a todos array").iter().map(|todo| todo["title"].as_str().unwrap_or("").to_string()).collect()
}

/// Returns a new, empty folder for one test's files.
///
/// @param name - the test's name, to tell the folders apart
pub fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let folder = std::env::temp_dir().join("todo-server-tests").join(format!("{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&folder).expect("the scratch folder is created");
    folder
}

/// Stops the server and removes a test's folder.
///
/// Called as the last line of a test, so it runs only when every assertion
/// passed. A failed test leaves its folder and its database for whoever reads
/// the failure. The server is stopped first because Windows will not delete a
/// file another process holds open.
///
/// @param server - the server that used the folder
/// @param folder - the folder `scratch` returned
pub fn finish(server: Server, folder: &Path) {
    drop(server);
    let _ = std::fs::remove_dir_all(folder);
}
