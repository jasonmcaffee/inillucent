//! The conformance suite, run against the Rust driver.
//!
//! `drivers/conformance/suite.json` is the driver's behaviour written as data,
//! and this file is one of its two runners; `drivers/bindings/python` is the
//! other. Running the same file from both is what stops the reference
//! implementation and the specification drifting apart, because there is only
//! one specification and neither implementation is it.
//!
//! ## Why the JSON reader is written here
//!
//! `docs/invariants/layering.toml` approves `serde_json` for `inillucent-core`
//! and `inillucent-bench` and nothing else, and widening a dependency policy so
//! that a test can read a file is the wrong trade - the policy is the thing
//! keeping the workspace's edges honest. `inillucent-compat/src/toml_lite.rs`
//! already set the precedent for exactly this: *"a deliberately small reader
//! for the shapes the workspace uses. Anything else is reported rather than
//! ignored."*
//!
//! So [`json`] reads the shapes `suite.json` uses and **refuses** anything
//! else. A reader that silently accepted what it did not understand would make
//! a malformed suite look like a passing one, which is the failure mode a test
//! harness must not have.
//!
//! Invariant: **the driver answers what the engine answers.** It is the one
//! surface every language binding reaches the engine through, so a difference
//! between the two is a difference every binding inherits.

use std::path::PathBuf;

use inillucent_driver::{Database, Rows, Status, Value};

mod json {
    //! A small JSON reader, for one file.
    //!
    //! It handles objects, arrays, strings with the escapes `suite.json` uses,
    //! numbers, `true`, `false` and `null`, and it refuses everything else by
    //! name rather than guessing - because a suite this misread would report
    //! passes it had not run.

    /// One JSON value.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Json {
        /// `null`.
        Null,
        /// `true` or `false`.
        Bool(bool),
        /// A number, kept as text so an i64 and an f64 can both be read out of
        /// it exactly. `-9223372036854775808` does not survive a trip through
        /// `f64`, and the suite binds it on purpose.
        Number(String),
        /// A string.
        Text(String),
        /// An array.
        List(Vec<Json>),
        /// An object, in the order its keys were written.
        Map(Vec<(String, Json)>),
    }

    impl Json {
        /// Returns the value at a key, for an object.
        ///
        /// @param key - the key
        pub fn get(&self, key: &str) -> Option<&Json> {
            match self {
                Json::Map(pairs) => pairs
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value),
                _ => None,
            }
        }

        /// Returns the items, for an array.
        pub fn items(&self) -> &[Json] {
            match self {
                Json::List(items) => items,
                _ => &[],
            }
        }

        /// Returns the text, for a string.
        pub fn text(&self) -> Option<&str> {
            match self {
                Json::Text(text) => Some(text),
                _ => None,
            }
        }

        /// Returns the value as an `i64`, for a number.
        pub fn integer(&self) -> Option<i64> {
            match self {
                Json::Number(digits) => digits.parse().ok(),
                _ => None,
            }
        }

        /// Returns the value as an `f64`, for a number.
        pub fn real(&self) -> Option<f64> {
            match self {
                Json::Number(digits) => digits.parse().ok(),
                _ => None,
            }
        }

        /// Returns the value as a `usize`, for a number.
        pub fn count(&self) -> Option<usize> {
            self.integer()
                .and_then(|number| usize::try_from(number).ok())
        }

        /// Returns the value as a `bool`.
        pub fn boolean(&self) -> Option<bool> {
            match self {
                Json::Bool(held) => Some(*held),
                _ => None,
            }
        }
    }

    /// Reads one JSON document.
    ///
    /// @param text - the document
    pub fn parse(text: &str) -> Result<Json, String> {
        let characters: Vec<char> = text.chars().collect();
        let mut at = 0usize;
        let value = read(&characters, &mut at)?;
        skip_space(&characters, &mut at);
        if at < characters.len() {
            return Err(format!("trailing text at character {at}"));
        }
        Ok(value)
    }

    /// Reads one value.
    ///
    /// @param text - the document's characters
    /// @param at - where to read from, advanced past what was read
    fn read(text: &[char], at: &mut usize) -> Result<Json, String> {
        skip_space(text, at);
        match text.get(*at) {
            None => Err("the document ended early".to_owned()),
            Some('{') => read_map(text, at),
            Some('[') => read_list(text, at),
            Some('"') => read_text(text, at).map(Json::Text),
            Some('t') => word(text, at, "true").map(|()| Json::Bool(true)),
            Some('f') => word(text, at, "false").map(|()| Json::Bool(false)),
            Some('n') => word(text, at, "null").map(|()| Json::Null),
            Some(character) if character.is_ascii_digit() || *character == '-' => {
                read_number(text, at)
            }
            Some(character) => Err(format!("`{character}` at character {at} begins no value")),
        }
    }

    /// Skips whitespace.
    ///
    /// @param text - the document's characters
    /// @param at - where to read from
    fn skip_space(text: &[char], at: &mut usize) {
        while matches!(text.get(*at), Some(c) if c.is_whitespace()) {
            *at = at.saturating_add(1);
        }
    }

    /// Reads a fixed word.
    ///
    /// @param text - the document's characters
    /// @param at - where to read from
    /// @param expected - the word
    fn word(text: &[char], at: &mut usize, expected: &str) -> Result<(), String> {
        for wanted in expected.chars() {
            if text.get(*at) != Some(&wanted) {
                return Err(format!("expected `{expected}` at character {at}"));
            }
            *at = at.saturating_add(1);
        }
        Ok(())
    }

    /// Reads a number, keeping its text.
    ///
    /// @param text - the document's characters
    /// @param at - where to read from
    fn read_number(text: &[char], at: &mut usize) -> Result<Json, String> {
        let start = *at;
        while matches!(text.get(*at), Some(c) if c.is_ascii_digit()
            || *c == '-' || *c == '+' || *c == '.' || *c == 'e' || *c == 'E')
        {
            *at = at.saturating_add(1);
        }
        let digits: String = text.get(start..*at).unwrap_or_default().iter().collect();
        match digits.is_empty() {
            true => Err(format!("no number at character {start}")),
            false => Ok(Json::Number(digits)),
        }
    }

    /// Reads a string, handling the escapes this suite uses.
    ///
    /// @param text - the document's characters
    /// @param at - where to read from
    fn read_text(text: &[char], at: &mut usize) -> Result<String, String> {
        word(text, at, "\"")?;
        let mut out = String::new();
        loop {
            match text.get(*at) {
                None => return Err("a string was not closed".to_owned()),
                Some('"') => {
                    *at = at.saturating_add(1);
                    return Ok(out);
                }
                Some('\\') => {
                    *at = at.saturating_add(1);
                    let escaped = *text.get(*at).ok_or("an escape was not finished")?;
                    *at = at.saturating_add(1);
                    match escaped {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' => {
                            let hex: String = text
                                .get(*at..at.saturating_add(4))
                                .unwrap_or_default()
                                .iter()
                                .collect();
                            let code = u32::from_str_radix(&hex, 16)
                                .map_err(|_| format!("`\\u{hex}` is not a code point"))?;
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                            *at = at.saturating_add(4);
                        }
                        other => return Err(format!("`\\{other}` is not an escape this reads")),
                    }
                }
                Some(character) => {
                    out.push(*character);
                    *at = at.saturating_add(1);
                }
            }
        }
    }

    /// Reads an array.
    ///
    /// @param text - the document's characters
    /// @param at - where to read from
    fn read_list(text: &[char], at: &mut usize) -> Result<Json, String> {
        word(text, at, "[")?;
        let mut items = Vec::new();
        skip_space(text, at);
        if text.get(*at) == Some(&']') {
            *at = at.saturating_add(1);
            return Ok(Json::List(items));
        }
        loop {
            items.push(read(text, at)?);
            skip_space(text, at);
            match text.get(*at) {
                Some(',') => *at = at.saturating_add(1),
                Some(']') => {
                    *at = at.saturating_add(1);
                    return Ok(Json::List(items));
                }
                _ => return Err(format!("an array is not closed at character {at}")),
            }
        }
    }

    /// Reads an object.
    ///
    /// @param text - the document's characters
    /// @param at - where to read from
    fn read_map(text: &[char], at: &mut usize) -> Result<Json, String> {
        word(text, at, "{")?;
        let mut pairs = Vec::new();
        skip_space(text, at);
        if text.get(*at) == Some(&'}') {
            *at = at.saturating_add(1);
            return Ok(Json::Map(pairs));
        }
        loop {
            skip_space(text, at);
            let key = read_text(text, at)?;
            skip_space(text, at);
            word(text, at, ":")?;
            pairs.push((key, read(text, at)?));
            skip_space(text, at);
            match text.get(*at) {
                Some(',') => *at = at.saturating_add(1),
                Some('}') => {
                    *at = at.saturating_add(1);
                    return Ok(Json::Map(pairs));
                }
                _ => return Err(format!("an object is not closed at character {at}")),
            }
        }
    }
}

use json::Json;

/// Returns a scratch database path nothing else is using.
///
/// @param name - the case's name
fn scratch(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inillucent-conformance-{name}-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    path
}

/// Reads a value out of the suite's one-key object form.
///
/// **One key rather than a bare literal**, so that NULL and the empty string
/// can never be confused by the file itself - the same reason the driver has a
/// `Null` variant.
///
/// @param value - the suite's value
fn value_from_json(value: &Json) -> Result<Value, String> {
    if value.get("null").is_some() {
        return Ok(Value::Null);
    }
    if let Some(number) = value.get("int") {
        return number
            .integer()
            .map(Value::Integer)
            .ok_or_else(|| format!("{number:?} is not an integer"));
    }
    if let Some(number) = value.get("real") {
        return number
            .real()
            .map(Value::Real)
            .ok_or_else(|| format!("{number:?} is not a number"));
    }
    if let Some(text) = value.get("text") {
        return text
            .text()
            .map(|said| Value::Text(said.to_owned()))
            .ok_or_else(|| format!("{text:?} is not a string"));
    }
    if let Some(bytes) = value.get("blob") {
        let mut out = Vec::new();
        for byte in bytes.items() {
            let number = byte.integer().ok_or("a blob holds numbers")?;
            out.push(u8::try_from(number).map_err(|_| format!("{number} is not a byte"))?);
        }
        return Ok(Value::Blob(out));
    }
    Err(format!("{value:?} names no value kind"))
}

/// Renders a value for a failure message.
///
/// @param value - the value
fn shown(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => number.to_string(),
        Value::Text(text) => format!("{text:?}"),
        Value::Blob(bytes) => format!("{} bytes {:?}", bytes.len(), bytes),
    }
}

/// Checks one step's assertions against what the driver answered.
///
/// Only the keys the step carries are checked, so a case can pin a row count
/// without pinning the rows.
///
/// @param step - the step
/// @param outcome - what the driver answered
/// @param wrong - where to record a mismatch
fn check_success(step: &Json, outcome: &Rows, wrong: &mut Vec<String>) {
    if let Some(status) = step.get("status").and_then(Json::text) {
        wrong.push(format!(
            "expected it to fail with `{status}` and it succeeded"
        ));
        return;
    }
    if let Some(columns) = step.get("columns") {
        let want: Vec<&str> = columns.items().iter().filter_map(Json::text).collect();
        let got: Vec<&str> = outcome
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        if want != got {
            wrong.push(format!("columns are {got:?} and should be {want:?}"));
        }
    }
    if let Some(rows) = step.get("rows") {
        let want = rows.items();
        if want.len() != outcome.rows.len() {
            wrong.push(format!(
                "there are {} rows and there should be {}",
                outcome.rows.len(),
                want.len()
            ));
        } else {
            for (nth, row) in want.iter().enumerate() {
                let cells = row.items();
                let Some(got) = outcome.rows.get(nth) else {
                    continue;
                };
                if cells.len() != got.len() {
                    wrong.push(format!(
                        "row {nth} has {} cells and should have {}",
                        got.len(),
                        cells.len()
                    ));
                    continue;
                }
                for (column, cell) in cells.iter().enumerate() {
                    let Ok(want_value) = value_from_json(cell) else {
                        wrong.push(format!("row {nth} column {column} names no value kind"));
                        continue;
                    };
                    let Some(got_value) = got.get(column) else {
                        continue;
                    };
                    if &want_value != got_value {
                        wrong.push(format!(
                            "row {nth} column {column} is {} and should be {}",
                            shown(got_value),
                            shown(&want_value)
                        ));
                    }
                }
            }
        }
    }
    if let Some(affected) = step.get("affected") {
        let want = match affected {
            Json::Null => None,
            other => other.integer().map(|number| number as u64),
        };
        if outcome.affected != want {
            wrong.push(format!(
                "affected is {:?} and should be {want:?}",
                outcome.affected
            ));
        }
    }
    if let Some(total) = step.get("total").and_then(Json::count) {
        if outcome.total != total {
            wrong.push(format!(
                "total is {} and should be {total} - and total is exact, so this is a real \
                 disagreement rather than an estimate being off",
                outcome.total
            ));
        }
    }
    if let Some(more) = step.get("more").and_then(Json::boolean) {
        if outcome.more != more {
            wrong.push(format!("more is {} and should be {more}", outcome.more));
        }
    }
}

/// Checks a step that was expected to fail.
///
/// @param step - the step
/// @param failure - what the driver refused with
/// @param wrong - where to record a mismatch
fn check_failure(step: &Json, failure: &inillucent_driver::Error, wrong: &mut Vec<String>) {
    let Some(status) = step.get("status").and_then(Json::text) else {
        wrong.push(format!(
            "it was expected to succeed and it failed: {failure}"
        ));
        return;
    };
    if failure.status.name() != status {
        wrong.push(format!(
            "it failed with `{}` and should have failed with `{status}` - {failure}",
            failure.status.name()
        ));
    }
    if let Some(fragment) = step.get("message_contains").and_then(Json::text) {
        if !failure.message.contains(fragment) {
            wrong.push(format!(
                "the message is {:?} and should hold {fragment:?}",
                failure.message
            ));
        }
    }
    if let Some(fragment) = step.get("feature_contains").and_then(Json::text) {
        match failure.feature.as_deref() {
            None => wrong.push(
                "it named no construct, and a refusal that is `unsupported` has to name one \
                 or an application cannot say what it hit"
                    .to_string(),
            ),
            Some(named) if !named.contains(fragment) => wrong.push(format!(
                "it named {named:?} and should have named something holding {fragment:?}"
            )),
            Some(_) => {}
        }
    }
    if failure.status == Status::Unsupported && failure.feature.is_none() {
        wrong.push("an `unsupported` refusal must carry a feature".to_owned());
    }
}

/// Runs the whole suite and reports every case that did not behave.
#[test]
fn the_conformance_suite_passes() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../conformance/suite.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let suite = json::parse(&text).unwrap_or_else(|error| panic!("suite.json: {error}"));
    let cases = suite.get("cases").map(Json::items).unwrap_or_default();
    assert!(!cases.is_empty(), "the suite has no cases");

    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0usize;
    for case in cases {
        let name = case.get("name").and_then(Json::text).unwrap_or("(unnamed)");
        let file = scratch(name);
        let database = Database::open(&file).expect("the scratch database opens");
        // **A case may ask to be run a connection per call**, which is the shape
        // a driver in another language is forced into: `Connection` borrows the
        // `Database`, so a garbage-collected caller - or a C handle - keeps the
        // database and connects per call. Every one of those is a new session
        // unless the session number is carried across, and a session is what
        // `temp.`, `ATTACH` and the connection pragmas are scoped to. Running
        // such a case on one long-lived connection would pass without testing
        // anything.
        let per_call = case
            .get("connection")
            .and_then(Json::text)
            .is_some_and(|how| how == "per_call");
        let session = database.session().session();
        let hold = (!per_call).then(|| database.session_as(session));
        let mut wrong: Vec<String> = Vec::new();

        for statement in case.get("setup").map(Json::items).unwrap_or_default() {
            let Some(sql) = statement.text() else {
                wrong.push("a setup statement is not a string".to_owned());
                continue;
            };
            let held;
            let connection = match &hold {
                Some(open) => open,
                None => {
                    held = database.session_as(session);
                    &held
                }
            };
            if let Err(why) = connection.query(sql, &[], usize::MAX) {
                wrong.push(format!("the setup statement `{sql}` was refused: {why}"));
            }
        }

        if wrong.is_empty() {
            for step in case.get("steps").map(Json::items).unwrap_or_default() {
                let Some(sql) = step.get("sql").and_then(Json::text) else {
                    wrong.push("a step has no sql".to_owned());
                    continue;
                };
                let mut params: Vec<Value> = Vec::new();
                for value in step.get("params").map(Json::items).unwrap_or_default() {
                    match value_from_json(value) {
                        Ok(value) => params.push(value),
                        Err(why) => wrong.push(format!("`{sql}`: {why}")),
                    }
                }
                let limit = step
                    .get("limit")
                    .and_then(Json::count)
                    .unwrap_or(usize::MAX);
                let mut said: Vec<String> = Vec::new();
                let held;
                let connection = match &hold {
                    Some(open) => open,
                    None => {
                        held = database.session_as(session);
                        &held
                    }
                };
                match connection.query(sql, &params, limit) {
                    Ok(outcome) => check_success(step, &outcome, &mut said),
                    Err(failure) => check_failure(step, &failure, &mut said),
                }
                ran = ran.saturating_add(1);
                for problem in said {
                    wrong.push(format!("`{sql}`: {problem}"));
                }
            }
        }

        let _ = hold;
        drop(database);
        let _ = std::fs::remove_file(&file);
        for problem in wrong {
            failures.push(format!("{name}: {problem}"));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of the conformance suite's assertions did not hold:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
    assert!(
        ran >= 40,
        "only {ran} steps ran, which means the suite was misread rather than being small"
    );
}

#[cfg(test)]
mod reader_tests {
    use super::json::{parse, Json};

    /// The reader handles the shapes the suite uses.
    #[test]
    fn it_reads_the_shapes_the_suite_uses() {
        let document = parse(r#"{"a": [1, -2.5, true, false, null, "x\ny"], "b": {"c": "héllo"}}"#)
            .expect("reads");
        let list = document.get("a").expect("a").items().to_vec();
        assert_eq!(list.len(), 6);
        assert_eq!(list.first().and_then(Json::integer), Some(1));
        assert_eq!(list.get(1).and_then(Json::real), Some(-2.5));
        assert_eq!(list.get(2).and_then(Json::boolean), Some(true));
        assert_eq!(list.get(4), Some(&Json::Null));
        assert_eq!(list.get(5).and_then(Json::text), Some("x\ny"));
        assert_eq!(
            document
                .get("b")
                .and_then(|b| b.get("c"))
                .and_then(Json::text),
            Some("héllo")
        );
    }

    /// A number keeps its text, so an i64 at the edge of the range survives.
    ///
    /// `-9223372036854775808` is not representable in an `f64`, and the suite
    /// binds it deliberately. A reader that went through `f64` would hand the
    /// test a different number than the file holds and the test would still
    /// pass, which is the worst kind of wrong.
    #[test]
    fn the_smallest_integer_survives_being_read() {
        let document = parse(r#"{"int": -9223372036854775808}"#).expect("reads");
        assert_eq!(
            document.get("int").and_then(Json::integer),
            Some(i64::MIN),
            "a number must not be read through an f64"
        );
    }

    /// Anything it does not understand is refused rather than skipped, because
    /// a suite half-read would report passes it never ran.
    #[test]
    fn what_it_cannot_read_is_refused_rather_than_ignored() {
        assert!(parse("{").is_err());
        assert!(parse(r#"{"a": }"#).is_err());
        assert!(parse(r#"{"a": 1} trailing"#).is_err());
        assert!(parse(r#""a\qb""#).is_err(), "an unknown escape is refused");
    }
}
