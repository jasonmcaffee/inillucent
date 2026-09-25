//! Starts the built `coffee-server` and calls it over HTTP, the way a client does.
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

/// The built program.
pub const PROGRAM: &str = env!("CARGO_BIN_EXE_coffee-server");

/// A running server.
pub struct Server {
    child: Child,
    /// `http://127.0.0.1:<port>`.
    pub base: String,
}

impl Server {
    /// Starts `coffee-server serve` on a database, on a free port.
    ///
    /// The server prints `listening on http://<address>` once its socket is
    /// bound, and this waits for that line, so the first request never races
    /// the start.
    ///
    /// @param db - the database file; created when it does not exist
    pub fn start(db: &Path) -> Server {
        let mut child = Command::new(PROGRAM)
            .args(["serve", "--db", &db.to_string_lossy(), "--addr", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("coffee-server starts");
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
    /// @param path - the path and query string
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

    /// `POST` with no body that has to succeed, for the actions that take `?at=`.
    ///
    /// @param path - the path and query string
    pub fn act(&self, path: &str) -> Value {
        self.ok("POST", path, None)
    }

    /// Sends a request that has to fail with a given status, and returns the
    /// error's message.
    ///
    /// @param status - the status expected
    /// @param method - the HTTP method
    /// @param path - the path and query string
    /// @param body - the JSON body, or nothing
    pub fn fails(&self, status: u16, method: &str, path: &str, body: Option<Value>) -> String {
        let (got, json) = self.send(method, path, body.clone());
        assert_eq!(got, status, "{method} {path} {body:?} answered {got}, expected {status}: {json:#}");
        assert!(json["error"].is_string(), "the error body has `error`: {json}");
        json["message"].as_str().expect("the error body has `message`").to_string()
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

/// Reads an integer field, failing the test when it is missing.
///
/// @param body - the object
/// @param field - the field
pub fn int(body: &Value, field: &str) -> i64 {
    body[field].as_i64().unwrap_or_else(|| panic!("no integer `{field}` in {body}"))
}

/// Returns a new, empty folder for one test's files.
///
/// @param name - the test's name, to tell the folders apart
pub fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let folder = std::env::temp_dir().join("coffee-server-tests").join(format!("{name}-{}-{nanos}", std::process::id()));
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

/// The ids of the small shop [`shop`] sets up.
pub struct Shop {
    /// A latte, sold small and large.
    pub latte: i64,
    /// A croissant, sold in one size and not taxed.
    pub croissant: i64,
    /// Oat milk, which replaces whole milk.
    pub oat: i64,
    /// An extra shot, which adds 18 g of beans.
    pub shot: i64,
    /// The manager.
    pub ana: i64,
    /// A barista.
    pub ben: i64,
    /// A loyalty customer, with 50 welcome points.
    pub maya: i64,
}

/// The day every test trades on. Years away, so no test depends on today.
pub const DAY: &str = "2030-03-04";

/// A time on [`DAY`].
///
/// @param time - `HH:MM`
pub fn at(time: &str) -> String {
    format!("{DAY}T{time}:00Z")
}

/// Sets up a small shop through the API: the books opened, five ingredients
/// bought, a latte and a croissant on the menu, two modifiers, two staff, a
/// customer, and two promotions.
///
/// The purchase prices make the average costs round numbers: beans 22,000
/// millionths of a dollar a gram, whole milk 1,100 a millilitre, oat milk
/// 3,300, a cup 90,000, a croissant 1,150,000.
///
/// @param server - the running server
pub fn shop(server: &Server) -> Shop {
    server.post(
        "/journal",
        json!({ "memo": "opening", "at": at("05:00"), "lines": [
            { "account": "1020", "debit_cents": 100000 },
            { "account": "1000", "debit_cents": 20000 },
            { "account": "3000", "credit_cents": 120000 }
        ]}),
    );
    for (name, unit, reorder) in [
        ("Espresso beans", "g", 500),
        ("Whole milk", "ml", 2000),
        ("Oat milk", "ml", 1000),
        ("Cups", "each", 20),
        ("Croissants", "each", 0),
    ] {
        server.post("/ingredients", json!({ "name": name, "unit": unit, "reorder_level": reorder }));
    }
    server.post(
        "/purchases",
        json!({ "supplier": "Roaster", "invoice": "R-1", "at": at("05:30"), "lines": [
            { "ingredient_id": 1, "quantity": 1000, "cost_cents": 2200 },
            { "ingredient_id": 2, "quantity": 10000, "cost_cents": 1100 },
            { "ingredient_id": 3, "quantity": 5000, "cost_cents": 1650 },
            { "ingredient_id": 4, "quantity": 100, "cost_cents": 900 },
            { "ingredient_id": 5, "quantity": 10, "cost_cents": 1150 }
        ]}),
    );
    let latte = id(&server.post(
        "/menu/items",
        json!({ "category": "Coffee", "sku": "latte", "name": "Latte", "prices": { "small": 425, "large": 525 },
                "recipe": { "small": [{ "ingredient_id": 1, "quantity": 18 }, { "ingredient_id": 2, "quantity": 240 }, { "ingredient_id": 4, "quantity": 1 }],
                            "large": [{ "ingredient_id": 1, "quantity": 36 }, { "ingredient_id": 2, "quantity": 360 }, { "ingredient_id": 4, "quantity": 1 }] } }),
    ));
    let croissant = id(&server.post(
        "/menu/items",
        json!({ "category": "Bakery", "sku": "CROISSANT", "name": "Croissant", "taxable": false, "prices": { "regular": 375 },
                "recipe": { "regular": [{ "ingredient_id": 5, "quantity": 1 }] } }),
    ));
    let oat =
        id(&server
            .post("/menu/modifiers", json!({ "name": "Oat milk", "price_cents": 70, "ingredient_id": 3, "replaces_ingredient_id": 2 })));
    let shot = id(&server.post("/menu/modifiers", json!({ "name": "Extra shot", "price_cents": 90, "ingredient_id": 1, "quantity": 18 })));
    let ana = id(&server.post("/staff", json!({ "name": "Ana", "role": "manager" })));
    let ben = id(&server.post("/staff", json!({ "name": "Ben" })));
    let maya = id(&server.post("/customers", json!({ "name": "Maya", "email": "Maya@Example.com", "at": at("05:00") })));
    server.post("/promotions", json!({ "code": "welcome10", "kind": "percent", "value": 10, "starts_on": DAY, "ends_on": DAY }));
    server.post(
        "/promotions",
        json!({ "code": "MORNING", "kind": "amount", "value": 100, "min_subtotal_cents": 800, "starts_on": DAY, "ends_on": DAY }),
    );
    Shop { latte, croissant, oat, shot, ana, ben, maya }
}

/// Opens an order and answers its receipt.
///
/// @param server - the running server
/// @param body - the order
pub fn order(server: &Server, body: Value) -> Value {
    server.post("/orders", body)
}

/// Pays an order by card, the whole total, and answers the receipt.
///
/// @param server - the running server
/// @param order - the order's id
/// @param time - `HH:MM` on [`DAY`]
/// @param tip - the tip in cents
pub fn pay_by_card(server: &Server, order: i64, time: &str, tip: i64) -> Value {
    server.post(&format!("/orders/{order}/pay"), json!({ "at": at(time), "payments": [{ "method": "card", "tip_cents": tip }] }))
}

/// Answers the lines of one journal entry as `(account, debit, credit)`.
///
/// @param entry - a `GET /journal/{id}` body
pub fn lines(entry: &Value) -> Vec<(String, i64, i64)> {
    entry["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| (line["account_code"].as_str().unwrap_or("").to_string(), int(line, "debit_cents"), int(line, "credit_cents")))
        .collect()
}

/// Answers the journal entry with a given source for an order or a day.
///
/// @param server - the running server
/// @param day - the business day to look in
/// @param source - such as `sale` or `refund`
pub fn entries(server: &Server, day: &str, source: &str) -> Vec<Value> {
    server
        .get(&format!("/journal?day={day}"))
        .as_array()
        .expect("entries")
        .iter()
        .filter(|entry| entry["source"] == source)
        .cloned()
        .collect()
}
