//! The JSON built-ins.
//!
//! Invariant: a value that *is* JSON and a value that merely *looks* like JSON
//! are different things, and the difference is carried beside the value rather
//! than guessed from it. `json_object('a', json('[1]'))` is `{"a":[1]}` and
//! `json_object('a', '[1]')` is `{"a":"[1]"}`; the two calls hand the same
//! bytes to the same function, and only the mark says which was meant. SQLite
//! calls this the value's subtype. Guessing instead - "this text parses as
//! JSON, so it must be JSON" - would make a column of user-supplied strings
//! change meaning the day one of them happened to start with a bracket.
//!
//! Everything here is pure: it takes values and returns values, and reaches no
//! database. The table-valued forms `json_each` and `json_tree` are built on
//! the same tree in `crate::vtab::json`.

pub mod binary;
pub mod node;
pub mod parse;
pub mod path;
pub mod render;

use inillucent_base::DbResult;
use inillucent_sql::function::JsonFunc;
use inillucent_value::{numeric, Value};

pub use node::Node;

/// One argument, with the mark that says whether it is already JSON.
#[derive(Clone, Copy, Debug)]
pub struct Argument<'value> {
    /// The value.
    pub value: &'value Value<'static>,
    /// Whether the value carries the JSON subtype.
    pub json: bool,
}

impl<'value> Argument<'value> {
    /// Builds an argument that is not marked as JSON.
    pub fn plain(value: &'value Value<'static>) -> Argument<'value> {
        Argument { value, json: false }
    }
}

/// One result, with the mark the caller has to carry on.
#[derive(Clone, Debug)]
pub struct Answer {
    /// The value.
    pub value: Value<'static>,
    /// Whether the value carries the JSON subtype.
    pub json: bool,
}

impl Answer {
    /// An answer that is not JSON.
    fn plain(value: Value<'static>) -> Answer {
        Answer { value, json: false }
    }

    /// An answer that is JSON.
    fn marked(value: Value<'static>) -> Answer {
        Answer { value, json: true }
    }

    /// The NULL every JSON function returns for a NULL document.
    fn null() -> Answer {
        Answer::plain(Value::Null)
    }
}

/// Returns the error a document that will not parse reports.
pub fn malformed() -> inillucent_base::DbError {
    failure("malformed JSON")
}

/// Returns a statement error, which is what every JSON refusal is.
pub fn failure(detail: impl Into<String>) -> inillucent_base::DbError {
    inillucent_base::DbError::primary(inillucent_base::PrimaryCode::Error).with_detail(detail)
}

/// Calls one JSON built-in.
pub fn call(func: JsonFunc, arguments: &[Argument<'_>]) -> DbResult<Answer> {
    match func {
        JsonFunc::Json => render_document(arguments, false),
        JsonFunc::Jsonb => render_document(arguments, true),
        JsonFunc::Array => build_array(arguments, false),
        JsonFunc::ArrayB => build_array(arguments, true),
        JsonFunc::Object => build_object(arguments, false),
        JsonFunc::ObjectB => build_object(arguments, true),
        JsonFunc::ArrayLength => array_length(arguments),
        JsonFunc::ErrorPosition => error_position(arguments),
        JsonFunc::Extract => extract(arguments, false),
        JsonFunc::ExtractB => extract(arguments, true),
        JsonFunc::Arrow => arrow(arguments, false),
        JsonFunc::ArrowShift => arrow(arguments, true),
        JsonFunc::Insert => edit(arguments, path::Edit::Insert, false),
        JsonFunc::InsertB => edit(arguments, path::Edit::Insert, true),
        JsonFunc::Replace => edit(arguments, path::Edit::Replace, false),
        JsonFunc::ReplaceB => edit(arguments, path::Edit::Replace, true),
        JsonFunc::Set => edit(arguments, path::Edit::Set, false),
        JsonFunc::SetB => edit(arguments, path::Edit::Set, true),
        JsonFunc::Remove => remove(arguments, false),
        JsonFunc::RemoveB => remove(arguments, true),
        JsonFunc::Patch => merge_patch(arguments, false),
        JsonFunc::PatchB => merge_patch(arguments, true),
        JsonFunc::Pretty => pretty(arguments),
        JsonFunc::Type => type_of(arguments),
        JsonFunc::Valid => valid(arguments),
        JsonFunc::Quote => quote(arguments),
        JsonFunc::ArrayInsert => array_insert(arguments, false),
        JsonFunc::ArrayInsertB => array_insert(arguments, true),
    }
}

/// Reads one argument as a JSON document.
///
/// A blob is JSONB whether or not it is marked, because that is how a document
/// read back out of a column arrives: the subtype does not survive storage, and
/// a blob that decodes as JSONB is one. Text is parsed. A number is its own
/// document, which is what makes `json(1)` answer `1` rather than fail.
pub fn document(argument: &Argument<'_>) -> DbResult<Option<Node>> {
    match argument.value {
        Value::Null => Ok(None),
        Value::Integer(value) => Ok(Some(Node::Int(value.to_string()))),
        Value::Real(value) => Ok(Some(real_node(*value))),
        Value::Blob(blob) => binary::from_blob(blob.raw()).map(Some),
        Value::Text(text) => {
            let bytes = text.utf8_bytes();
            let source = std::str::from_utf8(&bytes).map_err(|_| malformed())?;
            parse::parse(source)
                .map(|parsed| Some(parsed.node))
                .map_err(|_| malformed())
        }
    }
}

/// Returns the node a real number becomes.
fn real_node(value: f64) -> Node {
    let text =
        String::from_utf8(numeric::real_to_text(value)).unwrap_or_else(|_| "0.0".to_string());
    if value.is_nan() {
        return Node::Null;
    }
    if value.is_infinite() {
        return Node::Float(if value < 0.0 {
            "-9e999".to_string()
        } else {
            "9e999".to_string()
        });
    }
    Node::Float(text)
}

/// Reads one argument as a value to be stored into a document.
///
/// This is the other half of the subtype rule. Unmarked text becomes a JSON
/// string; marked text is a document and is embedded. An unmarked blob has no
/// JSON spelling at all and is an error, which is the message applications see
/// when they pass a `zeroblob` to `json_array` by accident.
fn stored(argument: &Argument<'_>, raw: bool) -> DbResult<Node> {
    match argument.value {
        Value::Null => Ok(Node::Null),
        Value::Integer(value) => Ok(Node::Int(value.to_string())),
        Value::Real(value) => Ok(real_node(*value)),
        Value::Blob(blob) => {
            if !argument.json {
                return Err(failure("JSON cannot hold BLOB values"));
            }
            binary::from_blob(blob.raw())
        }
        Value::Text(text) => {
            let bytes = text.utf8_bytes();
            let source = std::str::from_utf8(&bytes).map_err(|_| malformed())?;
            if argument.json {
                return parse::parse(source)
                    .map(|parsed| parsed.node)
                    .map_err(|_| malformed());
            }
            Ok(if raw {
                Node::text_raw(source)
            } else {
                Node::text_escaped(source)
            })
        }
    }
}

/// Renders a node as the answer a JSON function returns.
fn answer(node: &Node, binary: bool) -> DbResult<Answer> {
    if binary {
        return Ok(Answer::marked(Value::owned_blob(&binary::to_blob(node))?));
    }
    Ok(Answer::marked(Value::owned_text(
        render::to_text(node).as_bytes(),
    )?))
}

/// `json(X)` and `jsonb(X)`.
fn render_document(arguments: &[Argument<'_>], binary: bool) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    let Some(node) = document(first)? else {
        return Ok(Answer::null());
    };
    answer(&node, binary)
}

/// `json_array(...)` and `jsonb_array(...)`.
fn build_array(arguments: &[Argument<'_>], binary: bool) -> DbResult<Answer> {
    let mut items = Vec::with_capacity(arguments.len());
    for argument in arguments {
        items.push(stored(argument, false)?);
    }
    answer(&Node::Array(items), binary)
}

/// `json_object(...)` and `jsonb_object(...)`.
fn build_object(arguments: &[Argument<'_>], binary: bool) -> DbResult<Answer> {
    if !arguments.len().is_multiple_of(2) {
        return Err(failure(
            "json_object() requires an even number of arguments",
        ));
    }
    let mut members = Vec::with_capacity(arguments.len() / 2);
    for pair in arguments.chunks(2) {
        let (Some(label), Some(value)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let Value::Text(text) = label.value else {
            return Err(failure("json_object() labels must be TEXT"));
        };
        let bytes = text.utf8_bytes();
        let name = std::str::from_utf8(&bytes).map_err(|_| malformed())?;
        members.push((Node::text_escaped(name), stored(value, false)?));
    }
    answer(&Node::Object(members), binary)
}

/// Reads one argument as a path.
fn path_of(argument: &Argument<'_>) -> DbResult<Vec<path::Step>> {
    let Value::Text(text) = argument.value else {
        let rendered = match argument.value {
            Value::Integer(value) => value.to_string(),
            Value::Real(value) => String::from_utf8(numeric::real_to_text(*value))
                .unwrap_or_else(|_| String::from("?")),
            Value::Null => String::from("NULL"),
            _ => String::from("?"),
        };
        return Err(path::bad_path(&rendered));
    };
    let bytes = text.utf8_bytes();
    let source = std::str::from_utf8(&bytes).map_err(|_| malformed())?;
    path::parse(source)
}

/// `json_array_length(X)` and `json_array_length(X, P)`.
fn array_length(arguments: &[Argument<'_>]) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    let Some(node) = document(first)? else {
        return Ok(Answer::null());
    };
    let target = match arguments.get(1) {
        None => Some(&node),
        Some(argument) => {
            if argument.value.is_null() {
                return Ok(Answer::null());
            }
            path::lookup(&node, &path_of(argument)?)
        }
    };
    Ok(match target {
        Some(Node::Array(items)) => Answer::plain(Value::Integer(items.len() as i64)),
        Some(_) => Answer::plain(Value::Integer(0)),
        None => Answer::null(),
    })
}

/// `json_error_position(X)`.
fn error_position(arguments: &[Argument<'_>]) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    match first.value {
        Value::Null => Ok(Answer::null()),
        Value::Text(text) => {
            let bytes = text.utf8_bytes();
            let Ok(source) = std::str::from_utf8(&bytes) else {
                return Ok(Answer::plain(Value::Integer(1)));
            };
            Ok(Answer::plain(Value::Integer(match parse::parse(source) {
                Ok(_) => 0,
                Err(failure) => failure.position as i64,
            })))
        }
        _ => Ok(Answer::plain(Value::Integer(0))),
    }
}

/// Answers a single-path `json_extract` from a document and a path that are
/// already parsed.
///
/// **The whole point is what it does not do.** `json_extract('{...}', '$.b.c')`
/// with two literal arguments used to re-parse the document into a `Node` tree
/// and re-parse the path into steps on every one of the gate's four thousand
/// calls, because the entry point takes SQL values and values carry no memory.
/// A caller that can prove its arguments have not changed - which a compiled
/// expression can, by remembering the bytes it last saw - keeps both and calls
/// this instead.
///
/// @param node - the document, already parsed
/// @param steps - the path, already parsed
/// @param binary - true for `jsonb_extract`, false for `json_extract`
pub fn extract_parsed(node: &Node, steps: &[path::Step], binary: bool) -> DbResult<Answer> {
    let Some(found) = path::lookup(node, steps) else {
        return Ok(Answer::null());
    };
    if binary {
        return as_jsonb(found);
    }
    as_sql(found)
}

/// Turns one element into the SQL value `json_extract` hands back.
pub fn value_of(node: &Node) -> DbResult<Answer> {
    as_sql(node)
}

/// Turns one element into what `jsonb_extract` hands back.
///
/// **A container comes back as JSONB and a primitive comes back as itself.**
/// SQLite's `jsonb_extract` "works just like `json_extract()` except the
/// returned value is JSONB rather than JSON text" - and a number was never JSON
/// text to begin with, so `jsonb_extract(jsonb('{"a":2}'), '$.a')` is the
/// integer 2 in SQLite and was the raw JSONB byte `0x13 '2'` here: a blob where
/// an application expected a number, on the function it would reach for to read
/// one out of a document.
///
/// @param node - the element the path found
fn as_jsonb(node: &Node) -> DbResult<Answer> {
    match node {
        Node::Array(_) | Node::Object(_) => answer(node, true),
        _ => as_sql(node),
    }
}

/// Turns one element into the SQL value `json_extract` hands back.
fn as_sql(node: &Node) -> DbResult<Answer> {
    Ok(match node {
        Node::Null => Answer::plain(Value::Null),
        Node::True => Answer::plain(Value::Integer(1)),
        Node::False => Answer::plain(Value::Integer(0)),
        Node::Int(text) => match text.parse::<i64>() {
            Ok(value) => Answer::plain(Value::Integer(value)),
            Err(_) => Answer::plain(Value::Real(text.parse::<f64>().unwrap_or(0.0))),
        },
        Node::Int5(text) => match render::integer5_value(text) {
            Some(value) => Answer::plain(Value::Integer(value)),
            None => Answer::plain(Value::Real(0.0)),
        },
        Node::Float(text) => Answer::plain(Value::Real(parse_real(text))),
        Node::Float5(text) => Answer::plain(Value::Real(parse_real(&render::float5_to_json(text)))),
        Node::Text(_) | Node::TextJ(_) | Node::Text5(_) | Node::TextRaw(_) => {
            Answer::plain(Value::owned_text(render::unescape(node).as_bytes())?)
        }
        Node::Array(_) | Node::Object(_) => {
            Answer::marked(Value::owned_text(render::to_text(node).as_bytes())?)
        }
    })
}

/// Parses a JSON float, which may name an infinity the format spells `9e999`.
fn parse_real(text: &str) -> f64 {
    text.parse::<f64>().unwrap_or_else(|_| {
        if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        }
    })
}

/// `json_extract(X, P, ...)` and `jsonb_extract(X, P, ...)`.
///
/// One path answers the element; several answer an array of them. The single
/// path form is also the only place a JSON function returns SQL text for a
/// string and JSON text for a container, which is the asymmetry `->>` exists
/// to remove.
fn extract(arguments: &[Argument<'_>], binary: bool) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    let Some(node) = document(first)? else {
        return Ok(Answer::null());
    };
    let paths = arguments.get(1..).unwrap_or_default();
    if paths.is_empty() {
        return Err(failure("json_extract() needs at least two arguments"));
    }
    if paths.len() == 1 {
        let Some(argument) = paths.first() else {
            return Ok(Answer::null());
        };
        if argument.value.is_null() {
            return Ok(Answer::null());
        }
        let steps = path_of(argument)?;
        let Some(found) = path::lookup(&node, &steps) else {
            return Ok(Answer::null());
        };
        if binary {
            return as_jsonb(found);
        }
        return as_sql(found);
    }
    let mut items = Vec::with_capacity(paths.len());
    for argument in paths {
        let steps = path_of(argument)?;
        items.push(path::lookup(&node, &steps).cloned().unwrap_or(Node::Null));
    }
    answer(&Node::Array(items), binary)
}

/// The `->` and `->>` operators.
///
/// They are `json_extract` with two differences: the right operand may be a
/// bare name or a bare integer rather than a path, and `->` always answers JSON
/// text while `->>` always answers a SQL value.
fn arrow(arguments: &[Argument<'_>], sql: bool) -> DbResult<Answer> {
    let (Some(first), Some(second)) = (arguments.first(), arguments.get(1)) else {
        return Ok(Answer::null());
    };
    let Some(node) = document(first)? else {
        return Ok(Answer::null());
    };
    let steps = match second.value {
        Value::Null => return Ok(Answer::null()),
        Value::Integer(index) => {
            if *index < 0 {
                vec![path::Step::FromEnd(index.unsigned_abs() as usize)]
            } else {
                vec![path::Step::Index(*index as usize)]
            }
        }
        Value::Text(text) => {
            let bytes = text.utf8_bytes();
            let source = std::str::from_utf8(&bytes).map_err(|_| malformed())?;
            if source.starts_with('$') {
                path::parse(source)?
            } else {
                vec![path::Step::Key(source.to_string())]
            }
        }
        _ => return Err(path::bad_path("?")),
    };
    let Some(found) = path::lookup(&node, &steps) else {
        return Ok(Answer::null());
    };
    if sql {
        return as_sql(found);
    }
    answer(found, false)
}

/// `json_insert`, `json_replace`, `json_set` and their binary forms.
fn edit(arguments: &[Argument<'_>], edit: path::Edit, binary: bool) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    let rest = arguments.get(1..).unwrap_or_default();
    if rest.len() % 2 != 0 {
        return Err(failure("json_insert() needs an odd number of arguments"));
    }
    let Some(mut node) = document(first)? else {
        return Ok(Answer::null());
    };
    for pair in rest.chunks(2) {
        let (Some(target), Some(value)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let steps = path_of(target)?;
        path::apply(&mut node, &steps, stored(value, true)?, edit)?;
    }
    answer(&node, binary)
}

/// `json_array_insert(X, P, V, ...)` and its `jsonb_` spelling.
///
/// The pairs are applied left to right, so an insert can be made in front of
/// one the same call just made - which is why `('$[0]',9,'$[0]',8)` answers
/// `[8,9,...]` and not `[9,8,...]`.
fn array_insert(arguments: &[Argument<'_>], binary: bool) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    let rest = arguments.get(1..).unwrap_or_default();
    if rest.len() % 2 != 0 {
        return Err(failure(
            "json_array_insert() needs an odd number of arguments",
        ));
    }
    let Some(mut node) = document(first)? else {
        return Ok(Answer::null());
    };
    for pair in rest.chunks(2) {
        let (Some(target), Some(value)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let steps = path_of(target)?;
        let written = match target.value {
            Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
            _ => String::from("$"),
        };
        path::insert_into_array(&mut node, &steps, stored(value, true)?, &written)?;
    }
    answer(&node, binary)
}

/// `json_remove(X, P, ...)` and `jsonb_remove(X, P, ...)`.
fn remove(arguments: &[Argument<'_>], binary: bool) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    let Some(mut node) = document(first)? else {
        return Ok(Answer::null());
    };
    for argument in arguments.get(1..).unwrap_or_default() {
        if argument.value.is_null() {
            return Ok(Answer::null());
        }
        let steps = path_of(argument)?;
        // Removing the root removes the document, which is a NULL answer
        // rather than an empty one.
        if steps.is_empty() {
            return Ok(Answer::null());
        }
        path::remove(&mut node, &steps);
    }
    answer(&node, binary)
}

/// `json_patch(T, P)` and `jsonb_patch(T, P)`.
fn merge_patch(arguments: &[Argument<'_>], binary: bool) -> DbResult<Answer> {
    let (Some(first), Some(second)) = (arguments.first(), arguments.get(1)) else {
        return Ok(Answer::null());
    };
    let (Some(mut target), Some(updates)) = (document(first)?, document(second)?) else {
        return Ok(Answer::null());
    };
    path::patch(&mut target, &updates);
    answer(&target, binary)
}

/// `json_pretty(X)` and `json_pretty(X, indent)`.
fn pretty(arguments: &[Argument<'_>]) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    let Some(node) = document(first)? else {
        return Ok(Answer::null());
    };
    let indent = match arguments.get(1) {
        Some(Argument {
            value: Value::Text(text),
            ..
        }) => String::from_utf8(text.utf8_bytes().to_vec()).unwrap_or_else(|_| "    ".to_string()),
        _ => "    ".to_string(),
    };
    Ok(Answer::marked(Value::owned_text(
        render::to_pretty(&node, &indent).as_bytes(),
    )?))
}

/// `json_type(X)` and `json_type(X, P)`.
fn type_of(arguments: &[Argument<'_>]) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    let Some(node) = document(first)? else {
        return Ok(Answer::null());
    };
    let target = match arguments.get(1) {
        None => Some(&node),
        Some(argument) => {
            if argument.value.is_null() {
                return Ok(Answer::null());
            }
            path::lookup(&node, &path_of(argument)?)
        }
    };
    Ok(match target {
        Some(node) => Answer::plain(Value::owned_text(node.type_name().as_bytes())?),
        None => Answer::null(),
    })
}

/// `json_valid(X)` and `json_valid(X, flags)`.
///
/// The flags are a set rather than a choice: 1 asks whether the text is
/// RFC-8259, 2 whether it is JSON5, 4 whether a blob's header looks like JSONB,
/// and 8 whether the whole blob is JSONB. Asking for 6 asks both text
/// questions, which is what makes `json_valid(x, 6)` the useful one.
fn valid(arguments: &[Argument<'_>]) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    if first.value.is_null() {
        return Ok(Answer::null());
    }
    let flags = match arguments.get(1) {
        None => 1,
        Some(argument) => match argument.value {
            Value::Null => return Ok(Answer::null()),
            Value::Integer(value) => *value,
            Value::Real(value) => *value as i64,
            _ => 1,
        },
    };
    let answer = match first.value {
        Value::Blob(blob) => {
            let bytes = blob.raw();
            let strict = flags & 8 != 0 && binary::is_valid(bytes);
            let shallow = flags & 4 != 0 && binary::read_header(bytes, 0).is_ok();
            strict || shallow
        }
        Value::Text(text) => {
            let bytes = text.utf8_bytes();
            match std::str::from_utf8(&bytes) {
                Err(_) => false,
                Ok(source) => match parse::parse(source) {
                    Err(_) => false,
                    Ok(parsed) => {
                        if parsed.used_json5 {
                            flags & 2 != 0
                        } else {
                            flags & 3 != 0
                        }
                    }
                },
            }
        }
        // A number is a document in either dialect.
        Value::Integer(_) | Value::Real(_) => flags & 3 != 0,
        Value::Null => false,
    };
    Ok(Answer::plain(Value::Integer(i64::from(answer))))
}

/// `json_quote(X)`.
///
/// It is the one function that does not read a document: a number comes back
/// as itself, text comes back as a JSON string, and a value already marked as
/// JSON comes back unchanged.
fn quote(arguments: &[Argument<'_>]) -> DbResult<Answer> {
    let Some(first) = arguments.first() else {
        return Ok(Answer::null());
    };
    if first.json {
        return match document(first)? {
            Some(node) => answer(&node, false),
            None => Ok(Answer::null()),
        };
    }
    let node = match first.value {
        Value::Null => Node::Null,
        Value::Integer(value) => Node::Int(value.to_string()),
        Value::Real(value) => real_node(*value),
        Value::Blob(_) => return Err(failure("JSON cannot hold BLOB values")),
        Value::Text(text) => {
            let bytes = text.utf8_bytes();
            Node::text_escaped(std::str::from_utf8(&bytes).map_err(|_| malformed())?)
        }
    };
    answer(&node, false)
}

/// Folds one value into a `json_group_array` accumulator.
pub fn group_array_step(items: &mut Vec<Node>, argument: &Argument<'_>) -> DbResult<()> {
    items.push(stored(argument, false)?);
    Ok(())
}

/// Finishes a `json_group_array` accumulator.
pub fn group_array_final(items: Vec<Node>, binary: bool) -> DbResult<Answer> {
    answer(&Node::Array(items), binary)
}

/// Folds one label and value into a `json_group_object` accumulator.
pub fn group_object_step(
    members: &mut Vec<(Node, Node)>,
    label: &Argument<'_>,
    value: &Argument<'_>,
) -> DbResult<()> {
    let name = match label.value {
        Value::Text(text) => {
            String::from_utf8(text.utf8_bytes().to_vec()).map_err(|_| malformed())?
        }
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => {
            String::from_utf8(numeric::real_to_text(*number)).map_err(|_| malformed())?
        }
        _ => return Err(failure("json_group_object() labels must be TEXT")),
    };
    members.push((Node::text_escaped(&name), stored(value, false)?));
    Ok(())
}

/// Finishes a `json_group_object` accumulator.
pub fn group_object_final(members: Vec<(Node, Node)>, binary: bool) -> DbResult<Answer> {
    answer(&Node::Object(members), binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calls a function over owned values, none of them marked as JSON.
    fn run(func: JsonFunc, values: &[Value<'static>]) -> DbResult<Answer> {
        let arguments: Vec<Argument<'_>> = values.iter().map(Argument::plain).collect();
        call(func, &arguments)
    }

    /// Returns an owned text value for a test.
    fn text(source: &str) -> Value<'static> {
        Value::owned_text(source.as_bytes()).expect("a text value")
    }

    /// Renders an answer as text for a comparison.
    fn rendered(answer: &Answer) -> String {
        match &answer.value {
            Value::Text(value) => String::from_utf8_lossy(&value.utf8_bytes()).into_owned(),
            Value::Integer(value) => value.to_string(),
            Value::Null => "NULL".to_string(),
            other => format!("{other:?}"),
        }
    }

    /// `json()` minifies and preserves the spelling it was given.
    #[test]
    fn json_minifies() {
        let answer = run(JsonFunc::Json, &[text(" { \"a\" : 1.50 } ")]).expect("succeeds");
        assert_eq!(rendered(&answer), "{\"a\":1.50}");
        assert!(answer.json);
    }

    /// A document that will not parse is an error, not a NULL.
    #[test]
    fn a_malformed_document_is_an_error() {
        assert!(run(JsonFunc::Json, &[text("x")]).is_err());
        assert!(run(JsonFunc::Json, &[text("")]).is_err());
    }

    /// A NULL document answers NULL everywhere.
    #[test]
    fn a_null_document_answers_null() {
        for func in [
            JsonFunc::Json,
            JsonFunc::ArrayLength,
            JsonFunc::Type,
            JsonFunc::Pretty,
        ] {
            let answer = run(func, &[Value::Null]).expect("succeeds");
            assert!(answer.value.is_null(), "{func:?}");
        }
    }

    /// Unmarked text is a JSON string; marked text is a document.
    #[test]
    fn the_subtype_decides_whether_text_is_a_document() {
        let plain = run(JsonFunc::Array, &[text("[1]")]).expect("succeeds");
        assert_eq!(rendered(&plain), "[\"[1]\"]");

        let inner = text("[1]");
        let marked = call(
            JsonFunc::Array,
            &[Argument {
                value: &inner,
                json: true,
            }],
        )
        .expect("succeeds");
        assert_eq!(rendered(&marked), "[[1]]");
    }

    /// An unmarked blob has no JSON spelling and is refused.
    #[test]
    fn an_unmarked_blob_is_refused() {
        let blob = Value::owned_blob(b"ab").expect("a blob");
        assert!(run(JsonFunc::Array, &[blob]).is_err());
    }

    /// Several paths answer an array, and one answers the element.
    #[test]
    fn extract_answers_one_element_or_an_array() {
        let document = text("{\"a\":1,\"b\":2}");
        assert_eq!(
            rendered(&run(JsonFunc::Extract, &[document.clone(), text("$.a")]).expect("succeeds")),
            "1"
        );
        assert_eq!(
            rendered(
                &run(JsonFunc::Extract, &[document, text("$.a"), text("$.b")]).expect("succeeds")
            ),
            "[1,2]"
        );
    }

    /// `json_valid` answers the dialect question its flags name.
    #[test]
    fn valid_answers_per_dialect() {
        let document = text("{a:1}");
        for (flags, expected) in [(1, "0"), (2, "1"), (4, "0"), (6, "1")] {
            let answer =
                run(JsonFunc::Valid, &[document.clone(), Value::Integer(flags)]).expect("succeeds");
            assert_eq!(rendered(&answer), expected, "flags {flags}");
        }
    }

    /// The error position is one-based, and zero means the document parsed.
    #[test]
    fn error_position_is_one_based() {
        assert_eq!(
            rendered(&run(JsonFunc::ErrorPosition, &[text("[1,2")]).expect("succeeds")),
            "5"
        );
        assert_eq!(
            rendered(&run(JsonFunc::ErrorPosition, &[text("{a:1}")]).expect("succeeds")),
            "0"
        );
    }
}
