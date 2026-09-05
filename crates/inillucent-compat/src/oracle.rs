//! The black-box oracle protocol.
//!
//! Invariant: values cross the protocol as tagged bytes, never as JSON numbers
//! or bare strings. A double that went through a decimal literal, or text that
//! went through a JSON string, would be compared after a conversion neither
//! engine performed, and the comparison would be of the harness rather than of
//! the engines.
//!
//! Two drivers speak this protocol as child processes: one built from the
//! pinned SQLite amalgamation, and one built from inillucent. `inillucent-compat`
//! sends both the same command sequence and compares the observations. SQLite
//! is a test oracle here and never a runtime component; no production crate
//! depends on this one.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::report::json_string;

/// A value as it crosses the protocol.
#[derive(Clone, Debug, PartialEq)]
pub enum TaggedValue {
    /// SQL NULL.
    Null,
    /// A signed 64-bit integer, carried as its big-endian bytes.
    Integer(i64),
    /// An IEEE-754 binary64, carried as its exact bit pattern.
    Real(f64),
    /// Text, carried as its UTF-8 bytes.
    Text(Vec<u8>),
    /// A blob, carried as its bytes.
    Blob(Vec<u8>),
}

impl TaggedValue {
    /// Renders the value as the protocol's JSON object.
    pub fn to_json(&self) -> String {
        match self {
            TaggedValue::Null => "{\"class\":\"null\"}".to_string(),
            TaggedValue::Integer(value) => format!(
                "{{\"class\":\"integer\",\"be_hex\":\"{}\"}}",
                hex(&value.to_be_bytes())
            ),
            TaggedValue::Real(value) => format!(
                "{{\"class\":\"real\",\"ieee754_hex\":\"{}\"}}",
                hex(&value.to_bits().to_be_bytes())
            ),
            TaggedValue::Text(bytes) => {
                format!("{{\"class\":\"text\",\"utf8_hex\":\"{}\"}}", hex(bytes))
            }
            TaggedValue::Blob(bytes) => {
                format!("{{\"class\":\"blob\",\"hex\":\"{}\"}}", hex(bytes))
            }
        }
    }

    /// Parses one value object.
    pub fn parse(text: &str) -> Result<TaggedValue, String> {
        let class = field(text, "class").ok_or_else(|| format!("no class in {text}"))?;
        match class.as_str() {
            "null" => Ok(TaggedValue::Null),
            "integer" => {
                let bytes = unhex(&field(text, "be_hex").ok_or("integer has no be_hex")?)?;
                let mut value = [0u8; 8];
                if bytes.len() != 8 {
                    return Err("integer is not eight bytes".to_string());
                }
                for (slot, byte) in value.iter_mut().zip(bytes.iter()) {
                    *slot = *byte;
                }
                Ok(TaggedValue::Integer(i64::from_be_bytes(value)))
            }
            "real" => {
                let bytes = unhex(&field(text, "ieee754_hex").ok_or("real has no ieee754_hex")?)?;
                let mut value = [0u8; 8];
                if bytes.len() != 8 {
                    return Err("real is not eight bytes".to_string());
                }
                for (slot, byte) in value.iter_mut().zip(bytes.iter()) {
                    *slot = *byte;
                }
                Ok(TaggedValue::Real(f64::from_bits(u64::from_be_bytes(value))))
            }
            "text" => Ok(TaggedValue::Text(unhex(
                &field(text, "utf8_hex").ok_or("text has no utf8_hex")?,
            )?)),
            "blob" => Ok(TaggedValue::Blob(unhex(
                &field(text, "hex").ok_or("blob has no hex")?,
            )?)),
            other => Err(format!("unknown value class `{other}`")),
        }
    }

    /// Compares two values exactly, including the bit pattern of a double.
    ///
    /// `f64` equality says `NaN != NaN` and `0.0 == -0.0`, and both are wrong
    /// for a parity comparison: the engines must agree on the bits they stored,
    /// not on what those bits mean arithmetically.
    pub fn identical(&self, other: &TaggedValue) -> bool {
        match (self, other) {
            (TaggedValue::Real(left), TaggedValue::Real(right)) => {
                left.to_bits() == right.to_bits()
            }
            (left, right) => left == right,
        }
    }
}

/// What a driver reported for one command.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    /// Whether the command succeeded.
    pub ok: bool,
    /// The primary result code, when the command failed.
    pub code: i32,
    /// The extended result code, when the command failed.
    pub extended: i32,
    /// The message, when the command failed.
    pub message: String,
    /// The column names a query returned.
    pub columns: Vec<String>,
    /// The rows a query returned, as tagged values.
    pub rows: Vec<Vec<TaggedValue>>,
    /// `changes` after the command.
    pub changes: i64,
    /// `total_changes` after the command.
    pub total_changes: i64,
    /// `last_insert_rowid` after the command.
    pub last_insert_rowid: i64,
    /// Whether the connection is in autocommit mode after the command.
    pub autocommit: bool,
}

impl Default for Observation {
    /// An empty successful observation.
    fn default() -> Observation {
        Observation {
            ok: true,
            code: 0,
            extended: 0,
            message: String::new(),
            columns: Vec::new(),
            rows: Vec::new(),
            changes: 0,
            total_changes: 0,
            last_insert_rowid: 0,
            autocommit: true,
        }
    }
}

impl Observation {
    /// Parses a driver's reply line.
    pub fn parse(line: &str) -> Result<Observation, String> {
        let mut observation = Observation {
            ok: field_raw(line, "ok").is_some_and(|raw| raw.starts_with("true")),
            code: number(line, "code").unwrap_or(0) as i32,
            extended: number(line, "extended").unwrap_or(0) as i32,
            message: field(line, "message").unwrap_or_default(),
            columns: string_list(line, "columns"),
            rows: Vec::new(),
            changes: number(line, "changes").unwrap_or(0),
            total_changes: number(line, "total_changes").unwrap_or(0),
            last_insert_rowid: number(line, "last_insert_rowid").unwrap_or(0),
            autocommit: field_raw(line, "autocommit").is_none_or(|raw| raw.starts_with("true")),
        };
        observation.rows = parse_rows(line)?;
        Ok(observation)
    }
}

/// Extracts every `{"class": ...}` object, grouped into rows by the `[[` and
/// `]]` nesting of the `rows` array.
fn parse_rows(line: &str) -> Result<Vec<Vec<TaggedValue>>, String> {
    let Some(start) = line.find("\"rows\":") else {
        return Ok(Vec::new());
    };
    let body = line.get(start..).unwrap_or("");
    let mut rows = Vec::new();
    let mut current: Option<Vec<TaggedValue>> = None;
    let mut depth = 0usize;
    let mut index = 0usize;
    let bytes = body.as_bytes();
    while index < bytes.len() {
        let byte = bytes.get(index).copied().unwrap_or(0);
        match byte {
            b'[' => {
                depth = depth.saturating_add(1);
                if depth == 2 {
                    current = Some(Vec::new());
                }
                index = index.saturating_add(1);
            }
            b']' => {
                if depth == 2 {
                    if let Some(row) = current.take() {
                        rows.push(row);
                    }
                }
                if depth == 1 {
                    return Ok(rows);
                }
                depth = depth.saturating_sub(1);
                index = index.saturating_add(1);
            }
            b'{' => {
                let end = body
                    .get(index..)
                    .and_then(|rest| rest.find('}'))
                    .ok_or_else(|| "unterminated value object".to_string())?;
                let object = body.get(index..=index.saturating_add(end)).unwrap_or("");
                let value = TaggedValue::parse(object)?;
                if let Some(row) = current.as_mut() {
                    row.push(value);
                }
                index = index.saturating_add(end).saturating_add(1);
            }
            _ => index = index.saturating_add(1),
        }
    }
    Ok(rows)
}

/// A command sent to a driver.
#[derive(Clone, Debug)]
pub enum Op {
    /// Ask the driver to identify itself.
    Hello,
    /// Open a database file, or `:memory:`.
    Open(String),
    /// Run SQL that returns no rows.
    Exec(String),
    /// Run SQL and return its rows.
    Query(String),
    /// Bind values into `SELECT ?1, ?2, ...` and return them, which is how the
    /// protocol proves it carries every storage class without loss.
    Echo(Vec<TaggedValue>),
    /// Run SQL with values bound to `?1, ?2, ...` and return its first row.
    ///
    /// Embedding a value in SQL text would compare the engines' *parsers*
    /// rather than their value systems - there is no literal syntax for the
    /// exact bits of a double, and a blob written as `x'..'` has already been
    /// through a conversion. Binding is the only way to ask both engines the
    /// same question about the same value.
    Bind {
        /// The statement, with `?1`-style parameters.
        sql: String,
        /// The values to bind, in parameter order.
        values: Vec<TaggedValue>,
    },
    /// Report every run-time limit at its current value.
    Limits,
    /// Close the database.
    Close,
    /// Ask the driver to exit.
    Bye,
}

impl Op {
    /// Renders the command as one line of JSON.
    pub fn to_json(&self) -> String {
        match self {
            Op::Hello => "{\"op\":\"hello\"}".to_string(),
            Op::Open(path) => format!("{{\"op\":\"open\",\"path\":{}}}", json_string(path)),
            Op::Exec(sql) => format!("{{\"op\":\"exec\",\"sql\":{}}}", json_string(sql)),
            Op::Query(sql) => format!("{{\"op\":\"query\",\"sql\":{}}}", json_string(sql)),
            Op::Echo(values) => {
                let rendered: Vec<String> = values.iter().map(TaggedValue::to_json).collect();
                format!("{{\"op\":\"echo\",\"values\":[{}]}}", rendered.join(","))
            }
            Op::Bind { sql, values } => {
                let rendered: Vec<String> = values.iter().map(TaggedValue::to_json).collect();
                format!(
                    "{{\"op\":\"bind\",\"sql\":{},\"values\":[{}]}}",
                    json_string(sql),
                    rendered.join(",")
                )
            }
            Op::Limits => "{\"op\":\"limits\"}".to_string(),
            Op::Close => "{\"op\":\"close\"}".to_string(),
            Op::Bye => "{\"op\":\"bye\"}".to_string(),
        }
    }
}

/// A driver process speaking the protocol.
#[derive(Debug)]
pub struct Driver {
    name: String,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Driver {
    /// Starts a driver process.
    pub fn start(name: &str, program: &std::path::Path) -> Result<Driver, String> {
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", program.display()))?;
        let input = child.stdin.take().ok_or("the driver has no stdin")?;
        let output = child.stdout.take().ok_or("the driver has no stdout")?;
        Ok(Driver {
            name: name.to_string(),
            child,
            input,
            output: BufReader::new(output),
        })
    }

    /// Returns the driver's name, used in comparison messages.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Sends one command and returns what the driver reported.
    pub fn send(&mut self, op: &Op) -> Result<Observation, String> {
        writeln!(self.input, "{}", op.to_json())
            .map_err(|error| format!("{}: {error}", self.name))?;
        self.input
            .flush()
            .map_err(|error| format!("{}: {error}", self.name))?;
        let mut line = String::new();
        let read = self
            .output
            .read_line(&mut line)
            .map_err(|error| format!("{}: {error}", self.name))?;
        if read == 0 {
            return Err(format!("{} closed its output", self.name));
        }
        Observation::parse(line.trim())
            .map_err(|reason| format!("{}: {reason} in `{}`", self.name, line.trim()))
    }
}

impl Drop for Driver {
    /// Asks the driver to exit and reaps it, so a failing comparison cannot
    /// leave a child process behind.
    fn drop(&mut self) {
        let _ = writeln!(self.input, "{}", Op::Bye.to_json());
        let _ = self.input.flush();
        let _ = self.child.wait();
    }
}

/// How two drivers differed on one command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Difference {
    /// What was compared.
    pub field: &'static str,
    /// What the reference reported.
    pub reference: String,
    /// What inillucent reported.
    pub candidate: String,
}

/// Compares two observations, returning every field they disagree on.
pub fn compare(reference: &Observation, candidate: &Observation) -> Vec<Difference> {
    let mut differences = Vec::new();
    let mut check = |field: &'static str, left: String, right: String| {
        if left != right {
            differences.push(Difference {
                field,
                reference: left,
                candidate: right,
            });
        }
    };
    check("ok", reference.ok.to_string(), candidate.ok.to_string());
    check(
        "code",
        reference.code.to_string(),
        candidate.code.to_string(),
    );
    check(
        "extended",
        reference.extended.to_string(),
        candidate.extended.to_string(),
    );
    check(
        "columns",
        reference.columns.join(","),
        candidate.columns.join(","),
    );
    check(
        "changes",
        reference.changes.to_string(),
        candidate.changes.to_string(),
    );
    check(
        "last_insert_rowid",
        reference.last_insert_rowid.to_string(),
        candidate.last_insert_rowid.to_string(),
    );
    check(
        "autocommit",
        reference.autocommit.to_string(),
        candidate.autocommit.to_string(),
    );
    if reference.rows.len() != candidate.rows.len() {
        differences.push(Difference {
            field: "row-count",
            reference: reference.rows.len().to_string(),
            candidate: candidate.rows.len().to_string(),
        });
        return differences;
    }
    for (index, (left, right)) in reference.rows.iter().zip(candidate.rows.iter()).enumerate() {
        if left.len() != right.len() || !left.iter().zip(right.iter()).all(|(a, b)| a.identical(b))
        {
            differences.push(Difference {
                field: "row",
                reference: format!("{index}: {left:?}"),
                candidate: format!("{index}: {right:?}"),
            });
        }
    }
    differences
}

/// Renders bytes as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    crate::hash::to_hex(bytes)
}

/// Parses lowercase or uppercase hex into bytes.
fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err(format!("hex string `{text}` has an odd length"));
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut index = 0usize;
    while index < bytes.len() {
        let pair = text
            .get(index..index.saturating_add(2))
            .ok_or_else(|| "hex string ended early".to_string())?;
        out.push(u8::from_str_radix(pair, 16).map_err(|_| format!("`{pair}` is not hex"))?);
        index = index.saturating_add(2);
    }
    Ok(out)
}

/// Returns the string body of a JSON field, if the line has one.
fn field(line: &str, key: &str) -> Option<String> {
    let raw = field_raw(line, key)?;
    let body = raw.strip_prefix('"')?;
    let mut out = String::new();
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Some(out),
            '\\' => match characters.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other),
                None => return None,
            },
            other => out.push(other),
        }
    }
    None
}

/// Returns the text following a key, trimmed of leading whitespace.
fn field_raw<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)?.saturating_add(needle.len());
    Some(line.get(start..).unwrap_or("").trim_start())
}

/// Returns a signed number field.
fn number(line: &str, key: &str) -> Option<i64> {
    let raw = field_raw(line, key)?;
    let mut digits = String::new();
    for character in raw.chars() {
        if character == '-' && digits.is_empty() {
            digits.push(character);
            continue;
        }
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        break;
    }
    digits.parse().ok()
}

/// Returns an array-of-strings field.
fn string_list(line: &str, key: &str) -> Vec<String> {
    let Some(raw) = field_raw(line, key) else {
        return Vec::new();
    };
    let Some(body) = raw.strip_prefix('[') else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for character in body.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => {
                if in_string {
                    items.push(std::mem::take(&mut current));
                }
                in_string = !in_string;
            }
            other if in_string => current.push(other),
            // The list ends at the first bracket that is not inside a string.
            // Stopping at the first bracket of any kind is what made a column
            // called `json('[1,2,3]')` parse as no columns at all.
            ']' => break,
            _ => {}
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every storage class must survive the protocol's encoding unchanged,
    /// including the values a decimal or a JSON string would damage.
    #[test]
    fn every_storage_class_round_trips_through_the_protocol() {
        let cases = vec![
            TaggedValue::Null,
            TaggedValue::Integer(0),
            TaggedValue::Integer(-1),
            TaggedValue::Integer(i64::MIN),
            TaggedValue::Integer(i64::MAX),
            TaggedValue::Real(0.0),
            TaggedValue::Real(-0.0),
            TaggedValue::Real(f64::MIN_POSITIVE),
            TaggedValue::Real(f64::MAX),
            TaggedValue::Real(f64::INFINITY),
            TaggedValue::Real(f64::from_bits(0x7ff8_0000_0000_0001)),
            TaggedValue::Real(0.1),
            TaggedValue::Text(Vec::new()),
            TaggedValue::Text(b"hello".to_vec()),
            TaggedValue::Text(vec![0x61, 0x00, 0x62]),
            TaggedValue::Text("héllo ☃".as_bytes().to_vec()),
            TaggedValue::Blob(Vec::new()),
            TaggedValue::Blob(vec![0x00, 0xff, 0x7f, 0x80]),
        ];
        for case in cases {
            let parsed = TaggedValue::parse(&case.to_json()).expect("the value parses");
            assert!(parsed.identical(&case), "{case:?} became {parsed:?}");
        }
    }

    /// Signed zero and NaN must compare by bits, not by arithmetic equality, or
    /// two engines that stored different bytes would look identical.
    #[test]
    fn doubles_are_compared_by_their_bits() {
        let zero = TaggedValue::Real(0.0);
        let minus_zero = TaggedValue::Real(-0.0);
        assert!(!zero.identical(&minus_zero));
        let nan = TaggedValue::Real(f64::NAN);
        assert!(nan.identical(&TaggedValue::Real(f64::NAN)));
    }

    /// A reply line must parse back into the observation it describes.
    #[test]
    fn an_observation_parses() {
        let line = "{\"ok\":true,\"columns\":[\"a\",\"b\"],\"rows\":[[{\"class\":\"integer\",\"be_hex\":\"0000000000000001\"},{\"class\":\"null\"}],[{\"class\":\"text\",\"utf8_hex\":\"6869\"},{\"class\":\"blob\",\"hex\":\"00ff\"}]],\"changes\":2,\"total_changes\":5,\"last_insert_rowid\":9,\"autocommit\":false}";
        let observation = Observation::parse(line).expect("the line parses");
        assert!(observation.ok);
        assert_eq!(observation.columns, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(observation.rows.len(), 2);
        assert_eq!(observation.rows[0][0], TaggedValue::Integer(1));
        assert_eq!(observation.rows[1][1], TaggedValue::Blob(vec![0x00, 0xff]));
        assert_eq!(observation.changes, 2);
        assert_eq!(observation.last_insert_rowid, 9);
        assert!(!observation.autocommit);
    }

    /// An error reply must carry both codes and the message.
    #[test]
    fn an_error_observation_parses() {
        let line = "{\"ok\":false,\"code\":1,\"extended\":1,\"message\":\"no such table: t\"}";
        let observation = Observation::parse(line).expect("the line parses");
        assert!(!observation.ok);
        assert_eq!(observation.code, 1);
        assert_eq!(observation.message, "no such table: t");
    }

    /// The comparator must name every field two drivers disagreed on, not just
    /// the first, so one run reports the whole difference.
    #[test]
    fn the_comparator_names_every_difference() {
        let reference = Observation {
            rows: vec![vec![TaggedValue::Integer(1)]],
            changes: 1,
            ..Observation::default()
        };
        let candidate = Observation {
            rows: vec![vec![TaggedValue::Integer(2)]],
            changes: 2,
            ..Observation::default()
        };
        let differences = compare(&reference, &candidate);
        assert_eq!(differences.len(), 2);
        assert!(differences
            .iter()
            .any(|difference| difference.field == "changes"));
        assert!(differences
            .iter()
            .any(|difference| difference.field == "row"));
        assert!(compare(&reference, &reference).is_empty());
    }
}
