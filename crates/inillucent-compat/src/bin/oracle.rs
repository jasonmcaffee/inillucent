//! The inillucent side of the oracle protocol.
//!
//! Invariant: this driver answers with what inillucent really does, including
//! "not implemented yet". A driver that quietly returned the right answer for a
//! feature the engine does not have would make the differential harness agree
//! with SQLite about an engine that cannot do the work.
//!
//! At phase 1 the engine has no SQL front end, so `exec` and `query` report
//! `SQLITE_ERROR` and say so. What it does have is the binary layer, and the
//! `echo` command routes every value through it - integers through the checked
//! big-endian codec, doubles through the exact bit codec, text and blobs
//! through the fallible buffers - which is what makes the round-trip evidence
//! about inillucent rather than about the harness.

use std::io::{BufRead, Write};

use inillucent_base::buffer;
use inillucent_base::bytes;
use inillucent_base::error::PrimaryCode;
use inillucent_compat::oracle::TaggedValue;
use inillucent_compat::report::json_string;
use inillucent_vfs::contract::{OpenOptions, Vfs};
use inillucent_vfs::os::OsVfs;
use inillucent_vfs::path::DbPath;

/// Reads commands until stdin closes or `bye` arrives.
fn main() {
    let mut session = Session::default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let command = line.trim();
        if command.is_empty() {
            continue;
        }
        if operation(command).as_deref() == Some("bye") {
            break;
        }
        let reply = session.dispatch(command);
        if writeln!(stdout, "{reply}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

/// What the driver is holding open.
#[derive(Default)]
struct Session {
    open_path: Option<DbPath>,
}

impl Session {
    /// Runs one command and returns the reply line.
    fn dispatch(&mut self, command: &str) -> String {
        match operation(command).as_deref() {
            Some("hello") => format!(
                "{{\"ok\":true,\"driver\":\"inillucent\",\"version\":{},\"phase\":{}}}",
                json_string(env!("CARGO_PKG_VERSION")),
                json_string(inillucent_base::IMPLEMENTATION_PHASE)
            ),
            Some("open") => self.open(command),
            Some("close") => {
                self.open_path = None;
                "{\"ok\":true}".to_string()
            }
            Some("echo") => echo(command),
            Some("exec") | Some("query") => not_implemented(),
            other => error_reply(
                PrimaryCode::Misuse,
                &format!("unknown operation {}", other.unwrap_or("<none>")),
            ),
        }
    }

    /// Opens a database file through the VFS, which is as much of `open` as the
    /// engine can honestly do before the pager exists.
    fn open(&mut self, command: &str) -> String {
        let Some(path) = string_field(command, "path") else {
            return error_reply(PrimaryCode::Misuse, "open needs a path");
        };
        let path = DbPath::from(path.as_str());
        if path.is_memory() || path.is_anonymous() {
            self.open_path = Some(path);
            return "{\"ok\":true}".to_string();
        }
        let vfs = OsVfs::new();
        match vfs.open(&path, OpenOptions::main_db()) {
            Ok(_) => {
                self.open_path = Some(path);
                "{\"ok\":true}".to_string()
            }
            Err(error) => format!(
                "{{\"ok\":false,\"code\":{},\"extended\":{},\"message\":{}}}",
                error.code().value(),
                error.extended().value(),
                json_string(error.extended().message())
            ),
        }
    }
}

/// Echoes tagged values back after routing them through the binary layer.
fn echo(command: &str) -> String {
    let values = match parse_values(command) {
        Ok(values) => values,
        Err(reason) => return error_reply(PrimaryCode::Misuse, &reason),
    };
    let mut rendered = Vec::with_capacity(values.len());
    for value in values {
        match round_trip(&value) {
            Ok(returned) => rendered.push(returned.to_json()),
            Err(reason) => return error_reply(PrimaryCode::Internal, &reason),
        }
    }
    format!("{{\"ok\":true,\"rows\":[[{}]]}}", rendered.join(","))
}

/// Routes one value through inillucent's own codecs and back.
///
/// This is the whole point of the command: an integer that survives
/// `write_i64`/`read_i64`, a double that survives its exact bit codec, and text
/// or a blob that survives a fallible buffer copy are evidence about the
/// engine's binary layer, not about the JSON in between.
fn round_trip(value: &TaggedValue) -> Result<TaggedValue, String> {
    match value {
        TaggedValue::Null => Ok(TaggedValue::Null),
        TaggedValue::Integer(number) => {
            let mut page = [0u8; 8];
            bytes::write_i64(&mut page, 0, *number).map_err(|error| error.message().to_string())?;
            let returned =
                bytes::read_i64(&page, 0).map_err(|error| error.message().to_string())?;
            Ok(TaggedValue::Integer(returned))
        }
        TaggedValue::Real(number) => {
            let mut page = [0u8; 8];
            bytes::write_f64(&mut page, 0, *number).map_err(|error| error.message().to_string())?;
            let returned =
                bytes::read_f64(&page, 0).map_err(|error| error.message().to_string())?;
            Ok(TaggedValue::Real(returned))
        }
        TaggedValue::Text(body) => {
            let copy = buffer::try_copy_of(body).map_err(|error| error.message().to_string())?;
            Ok(TaggedValue::Text(copy.to_vec()))
        }
        TaggedValue::Blob(body) => {
            let copy = buffer::try_copy_of(body).map_err(|error| error.message().to_string())?;
            Ok(TaggedValue::Blob(copy.to_vec()))
        }
    }
}

/// Reports that the engine cannot run SQL yet, with the code SQLite uses for a
/// statement it cannot prepare.
fn not_implemented() -> String {
    error_reply(
        PrimaryCode::Error,
        "inillucent has no SQL front end yet; it lands in phase 5",
    )
}

/// Renders an error reply.
fn error_reply(code: PrimaryCode, message: &str) -> String {
    format!(
        "{{\"ok\":false,\"code\":{},\"extended\":{},\"message\":{}}}",
        code.value(),
        code.value(),
        json_string(message)
    )
}

/// Returns the `op` field of a command line.
fn operation(command: &str) -> Option<String> {
    string_field(command, "op")
}

/// Returns a string field of a command line.
fn string_field(command: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let start = command.find(&needle)?.saturating_add(needle.len());
    let rest = command.get(start..)?.trim_start();
    let body = rest.strip_prefix('"')?;
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

/// Parses the `values` array of an echo command.
fn parse_values(command: &str) -> Result<Vec<TaggedValue>, String> {
    let Some(start) = command.find("\"values\":") else {
        return Err("echo needs a values array".to_string());
    };
    let body = command.get(start..).unwrap_or("");
    let mut values = Vec::new();
    let mut index = 0usize;
    let bytes = body.as_bytes();
    while index < bytes.len() {
        match bytes.get(index).copied() {
            Some(b'{') => {
                let end = body
                    .get(index..)
                    .and_then(|rest| rest.find('}'))
                    .ok_or_else(|| "unterminated value object".to_string())?;
                let object = body.get(index..=index.saturating_add(end)).unwrap_or("");
                values.push(TaggedValue::parse(object)?);
                index = index.saturating_add(end).saturating_add(1);
            }
            Some(b']') => break,
            _ => index = index.saturating_add(1),
        }
    }
    Ok(values)
}
