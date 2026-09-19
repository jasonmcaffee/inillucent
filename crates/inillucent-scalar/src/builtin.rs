//! The built-in scalar functions.
//!
//! Invariant: a function returns what the pinned release returns, including
//! where that is surprising. `length()` of a blob is its byte count and of text
//! is its *character* count; `substr()` counts from one and accepts a negative
//! start; `round()` rounds half away from zero rather than to even; `max()` of
//! any NULL argument is NULL. Each of those differs from the obvious
//! implementation, and each has a case in the tests below.

use inillucent_sql::function::ScalarFunc;
use inillucent_value::{cast, compare, numeric, Collation, TextEncoding, Value};

use crate::eval;

/// What a scalar function may need to know about the statement around it.
///
/// Four built-ins answer a question about the connection rather than about
/// their arguments, and one of them is not even deterministic. Passing the
/// answers in keeps `call` a pure function of what it is given, which is what
/// lets the whole of this module be tested without a database.
#[derive(Clone, Copy, Debug, Default)]
pub struct Context {
    /// What `changes()` returns.
    pub changes: i64,
    /// What `total_changes()` returns.
    pub total_changes: i64,
    /// What `last_insert_rowid()` returns.
    pub last_insert_rowid: i64,
    /// The seed the random built-ins draw from.
    pub seed: u64,
    /// Whether `LIKE` compares ASCII letters exactly.
    ///
    /// What `PRAGMA case_sensitive_like` sets. It rides with the counters
    /// because it is the same kind of thing: a fact about the connection that
    /// an expression has to know and cannot ask for itself.
    pub like_case_sensitive: bool,
    /// The largest string or blob this connection admits, in bytes.
    ///
    /// **`Limit::Length` was enforced on the write path and nowhere else
    /// (task-1979, section 5.4).** `SELECT length(zeroblob(1073741824))`
    /// answered 1,073,741,824 from a served MCP server with a 256 MiB budget,
    /// after taking the process's working set to 2,873 MB;
    /// `SELECT length(printf('%2000000000d', 1))` reached 5,734 MB. A value
    /// that is only read never reached `record.rs`, which is where the bound
    /// was being applied.
    ///
    /// It rides here for the same reason `like_case_sensitive` does: it is a
    /// fact about the connection that an expression has to know and cannot ask
    /// for itself. Zero means unbounded, which is what a `Default` context -
    /// every unit test of this module - gets.
    pub length_limit: i64,
}

impl Context {
    /// Returns whether a value of `bytes` fits this connection's value bound.
    ///
    /// @param bytes - the size in question
    pub fn permits_length(&self, bytes: u64) -> bool {
        self.length_limit <= 0 || bytes <= self.length_limit as u64
    }
}

/// Calls a scalar function.
pub fn call(
    func: ScalarFunc,
    arguments: &[Value<'static>],
    collation: Collation,
    encoding: TextEncoding,
) -> Value<'static> {
    call_with(func, arguments, collation, encoding, Context::default())
}

/// Calls a scalar function, with what it may need to know about the statement.
pub fn call_with(
    func: ScalarFunc,
    arguments: &[Value<'static>],
    collation: Collation,
    encoding: TextEncoding,
    context: Context,
) -> Value<'static> {
    match func {
        ScalarFunc::VectorDistanceCos => vector_pair(arguments, cosine_distance),
        ScalarFunc::VectorDistanceL2 => vector_pair(arguments, euclidean_distance),
        ScalarFunc::VectorDot => vector_pair(arguments, dot_product),
        ScalarFunc::VectorDistanceL1 => vector_pair(arguments, taxicab_distance),
        ScalarFunc::VectorDistanceHamming => bit_pair(arguments, hamming_distance),
        ScalarFunc::VectorDistanceJaccard => bit_pair(arguments, jaccard_distance),
        ScalarFunc::VectorDims => vector_dims(arguments.first()),
        ScalarFunc::VectorNorm => vector_norm(arguments.first()),
        ScalarFunc::VectorNormalize => vector_normalize(arguments.first()),
        ScalarFunc::VectorQuantize => binary_quantize(arguments.first()),
        ScalarFunc::VectorSlice => subvector(arguments),
        ScalarFunc::VectorAdd => vector_zip(arguments, |one, two| one + two),
        ScalarFunc::VectorSubtract => vector_zip(arguments, |one, two| one - two),
        ScalarFunc::VectorMultiply => vector_zip(arguments, |one, two| one * two),
        ScalarFunc::VectorConcat => vector_concat(arguments),
        ScalarFunc::GeopolyArea => geopoly_measure(arguments, |shape| Value::Real(shape.area())),
        ScalarFunc::GeopolyBlob => geopoly_shape(arguments, Some),
        ScalarFunc::GeopolyJson => geopoly_measure(arguments, |shape| {
            Value::owned_text(shape.to_json().as_bytes()).unwrap_or(Value::Null)
        }),
        ScalarFunc::GeopolySvg => geopoly_svg(arguments, encoding),
        ScalarFunc::GeopolyWithin => geopoly_pair(arguments, |first, second| {
            match crate::geopoly::overlap(first, second) {
                2 => 1,
                4 => 2,
                _ => 0,
            }
        }),
        ScalarFunc::GeopolyOverlap => geopoly_pair(arguments, |first, second| {
            crate::geopoly::overlap(first, second)
        }),
        ScalarFunc::GeopolyContainsPoint => geopoly_measure(arguments, |shape| {
            let x = arguments.get(1).map_or(0.0, cast::real_value);
            let y = arguments.get(2).map_or(0.0, cast::real_value);
            Value::Integer(shape.contains_point(x, y))
        }),
        // Tracing this build does not have, read and discarded, which is what
        // a build without `GEOPOLY_ENABLE_DEBUG` does with it.
        ScalarFunc::GeopolyDebug => Value::Null,
        ScalarFunc::GeopolyBbox => geopoly_shape(arguments, |shape| {
            Some(crate::geopoly::box_polygon(shape.bounds()))
        }),
        ScalarFunc::GeopolyCcw => geopoly_shape(arguments, |shape| Some(shape.counter_clockwise())),
        ScalarFunc::GeopolyXform => geopoly_shape(arguments, |shape| {
            let mut matrix = [0.0f64; 6];
            for (at, slot) in matrix.iter_mut().enumerate() {
                *slot = arguments.get(at + 1).map_or(0.0, cast::real_value);
            }
            Some(shape.transformed(matrix))
        }),
        ScalarFunc::GeopolyRegular => geopoly_regular(arguments),
        ScalarFunc::RTreeDepth => rtree_depth(arguments.first()),
        ScalarFunc::RTreeNode => rtree_node(arguments),
        // Folded to its answer by the physical pass, which is the only place
        // the table it names is reachable. Reaching here at all means the
        // statement was compiled without a catalog.
        ScalarFunc::RTreeCheck => Value::Null,
        // Folded to its answer by the physical pass, which is the only place
        // the tree it asks about is reachable. Reaching here means the
        // statement was compiled without one.
        ScalarFunc::Offset => Value::Null,
        ScalarFunc::SqlarCompress => sqlar_compress(arguments.first()),
        ScalarFunc::SqlarUncompress => sqlar_uncompress(arguments),
        ScalarFunc::Printf => crate::printf::format(arguments, encoding),
        ScalarFunc::OctetLength => octet_length(arguments.first()),
        ScalarFunc::Random => Value::Integer(scramble(context.seed)),
        ScalarFunc::RandomBlob => random_blob(arguments.first(), context.seed),
        ScalarFunc::Changes => Value::Integer(context.changes),
        ScalarFunc::TotalChanges => Value::Integer(context.total_changes),
        ScalarFunc::LastInsertRowid => Value::Integer(context.last_insert_rowid),
        ScalarFunc::SourceId => Value::owned_text(SOURCE_ID.as_bytes()).unwrap_or(Value::Null),
        ScalarFunc::Fts5SourceId => {
            Value::owned_text(FTS5_SOURCE_ID.as_bytes()).unwrap_or(Value::Null)
        }
        ScalarFunc::Abs => unary(arguments, absolute),
        ScalarFunc::Char => char_of(arguments),
        ScalarFunc::Coalesce => coalesce(arguments),
        ScalarFunc::Concat => concat(arguments, None, encoding),
        ScalarFunc::ConcatWs => concat_with_separator(arguments, encoding),
        ScalarFunc::Glob => pattern_call(arguments, false, encoding, false),
        // `hex`, `quote`, `typeof` and `zeroblob` are the four built-ins that
        // answer a question *about* their argument rather than computing with
        // it, so a NULL argument has an answer instead of poisoning the result.
        ScalarFunc::Hex => hex(
            &arguments.first().cloned().unwrap_or(Value::Null),
            TextEncoding::Utf8,
        ),
        ScalarFunc::IfNull => coalesce(arguments),
        ScalarFunc::Iif => iif(arguments),
        ScalarFunc::Unknown => Value::Null,
        // `subtype` is answered by the binder, which is the only place that
        // knows which function produced the argument; a value here carries no
        // tag, so an argument that reached this far has none.
        // **Only reached where the answer depends on the value.** The binder
        // answers `subtype` outright wherever the producing call decides it;
        // what is left is `json_extract`, which carries the JSON subtype when
        // what it extracted was itself an array or an object.
        ScalarFunc::Subtype => Value::Integer(json_shaped(arguments.first())),
        ScalarFunc::Unistr => unistr(arguments.first()).unwrap_or(Value::Null),
        ScalarFunc::UnistrQuote => {
            unistr_quote(&arguments.first().cloned().unwrap_or(Value::Null), encoding)
        }
        ScalarFunc::CompileOptionUsed => compile_option_used(arguments.first()),
        ScalarFunc::CompileOptionGet => compile_option_get(arguments.first()),
        // The log is written by the connection, which a scalar cannot reach.
        // The answer is the reference's: NULL, whatever it was given.
        ScalarFunc::Log => Value::Null,
        // Both of these refuse before they are called, in `refusal_for`.
        ScalarFunc::LoadExtension => Value::Null,
        ScalarFunc::Regexp => regexp(arguments, encoding),
        ScalarFunc::Instr => instr(arguments, encoding),
        ScalarFunc::Length => unary(arguments, length),
        ScalarFunc::Like => pattern_call(arguments, true, encoding, !context.like_case_sensitive),
        ScalarFunc::Likelihood => arguments.first().cloned().unwrap_or(Value::Null),
        ScalarFunc::Lower => unary(arguments, |value| change_case(&value, false, encoding)),
        ScalarFunc::LTrim => trim(arguments, true, false, encoding),
        ScalarFunc::Max => extreme(arguments, collation, true),
        ScalarFunc::Min => extreme(arguments, collation, false),
        ScalarFunc::NullIf => null_if(arguments, collation),
        ScalarFunc::Quote => quote(
            &arguments.first().cloned().unwrap_or(Value::Null),
            TextEncoding::Utf8,
        ),
        ScalarFunc::Replace => replace(arguments, encoding),
        ScalarFunc::Round => round(arguments),
        ScalarFunc::RTrim => trim(arguments, false, true, encoding),
        ScalarFunc::Sign => unary(arguments, sign),
        ScalarFunc::Substr => substring(arguments, encoding),
        ScalarFunc::Trim => trim(arguments, true, true, encoding),
        ScalarFunc::TypeOf => Value::owned_text(
            arguments
                .first()
                .cloned()
                .unwrap_or(Value::Null)
                .storage_class()
                .typeof_name()
                .as_bytes(),
        )
        .unwrap_or(Value::Null),
        ScalarFunc::Unhex => unhex(arguments, encoding),
        ScalarFunc::Unicode => unary(arguments, |value| unicode(&value, encoding)),
        ScalarFunc::Upper => unary(arguments, |value| change_case(&value, true, encoding)),
        ScalarFunc::ZeroBlob => zero_blob(arguments.first().cloned().unwrap_or(Value::Null)),
        ScalarFunc::Version => {
            Value::owned_text(inillucent_base::REFERENCE_SQLITE_VERSION.as_bytes())
                .unwrap_or(Value::Null)
        }
    }
}

/// Applies a one-argument function, propagating NULL.
fn unary(
    arguments: &[Value<'static>],
    body: impl Fn(Value<'static>) -> Value<'static>,
) -> Value<'static> {
    let Some(value) = arguments.first() else {
        return Value::Null;
    };
    if value.is_null() {
        return Value::Null;
    }
    body(value.clone())
}

/// What `sqlite_source_id()` reports for the pinned release.
///
/// It is the pinned build's own string rather than something about inillucent,
/// because an application that reads it is asking which SQLite it is talking
/// to, and answering with a different shape would break the parse rather than
/// inform anyone.
const SOURCE_ID: &str =
    "2025-08-13 12:00:00 0000000000000000000000000000000000000000000000000000000000000000";

/// What `fts5_source_id()` reports.
///
/// **`fts5:` and then a build stamp, which is the shape SQLite answers in** -
/// an application that reads it is checking which FTS5 it is talking to, and a
/// differently shaped string breaks the parse rather than informing anybody.
/// The stamp itself is this engine's, because claiming a particular SQLite
/// build's hash would be a false statement about what is running. It was
/// added as the one FTS name of the six the audit found absent that has
/// a faithful answer here.
const FTS5_SOURCE_ID: &str =
    "fts5: 2026-09-08 00:00:00 inillucent000000000000000000000000000000000000000000000000000000";

/// `octet_length(x)`: the bytes a value occupies, whatever its class.
fn octet_length(value: Option<&Value<'static>>) -> Value<'static> {
    match value {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Text(text)) => Value::Integer(text.raw().len() as i64),
        Some(Value::Blob(blob)) => Value::Integer(blob.raw().len() as i64),
        // A number's octet length is the length of its text rendering, which is
        // what SQLite reports: the question is about the value, not its storage.
        Some(Value::Integer(integer)) => {
            Value::Integer(numeric::integer_to_text(*integer).len() as i64)
        }
        Some(Value::Real(real)) => Value::Integer(numeric::real_to_text(*real).len() as i64),
    }
}

/// `randomblob(n)`: n pseudo-random bytes, at least one.
fn random_blob(value: Option<&Value<'static>>, seed: u64) -> Value<'static> {
    let wanted = value.map_or(1, cast::integer_value).clamp(1, 1_000_000) as usize;
    let mut bytes = Vec::with_capacity(wanted);
    let mut state = seed;
    while bytes.len() < wanted {
        state = scramble(state) as u64;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(wanted);
    Value::owned_blob(&bytes).unwrap_or(Value::Null)
}

/// Mixes a seed into a value spread over the whole 64-bit range.
///
/// SQLite's `random()` returns a signed 64-bit integer from its own generator;
/// nothing observable depends on which generator, only that the values are
/// spread and that two calls in one statement differ. This is splitmix64.
fn scramble(seed: u64) -> i64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as i64
}

/// `abs(x)`.
fn absolute(value: Value<'static>) -> Value<'static> {
    // Text and blobs answer as a real whatever they hold: `abs('3')` is 3.0 and
    // `abs('x')` is 0.0, both real. Only a value that arrives as an integer
    // leaves as one.
    let textual = matches!(value, Value::Text(_) | Value::Blob(_));
    if textual {
        return Value::Real(cast::real_value(&value).abs());
    }
    match cast::numerify(value) {
        Value::Integer(integer) => match integer.checked_abs() {
            Some(absolute) => Value::Integer(absolute),
            // Unreachable through the executor: `refusal_for` raises `integer
            // overflow` for exactly this argument before the call is made,
            // which is what SQLite answers. The arm stays because this
            // function is callable on its own and must not wrap a negative.
            None => Value::Real(-(integer as f64)),
        },
        Value::Real(real) => Value::Real(real.abs()),
        _ => Value::Integer(0),
    }
}

/// `sign(x)`.
///
/// NULL for anything that is not a number, including text that does not look
/// like one and any blob. Casting first would make `sign('x')` zero, which
/// reads as "this value is zero" rather than "this is not a number".
fn sign(value: Value<'static>) -> Value<'static> {
    let numeric = match &value {
        Value::Integer(_) | Value::Real(_) => true,
        Value::Text(text) => numeric::looks_numeric(text.raw(), text.encoding()),
        _ => false,
    };
    if !numeric {
        return Value::Null;
    }
    match cast::numerify(value) {
        Value::Integer(integer) => Value::Integer(integer.signum()),
        Value::Real(real) if real > 0.0 => Value::Integer(1),
        Value::Real(real) if real < 0.0 => Value::Integer(-1),
        Value::Real(0.0) => Value::Integer(0),
        _ => Value::Null,
    }
}

/// `length(x)`: characters for text, bytes for a blob.
fn length(value: Value<'static>) -> Value<'static> {
    match &value {
        Value::Blob(blob) => Value::Integer(blob.len() as i64),
        Value::Text(text) => Value::Integer(numeric::character_count(
            before_nul(text.raw(), text.encoding()),
            text.encoding(),
        ) as i64),
        _ => {
            let rendered = eval::text_bytes(&value, TextEncoding::Utf8);
            Value::Integer(numeric::character_count(
                before_nul(&rendered, TextEncoding::Utf8),
                TextEncoding::Utf8,
            ) as i64)
        }
    }
}

/// Returns the bytes of a string up to its first NUL character.
///
/// **`length()` counts characters "prior to the first NUL character"**, which
/// is SQLite's documented definition and not an implementation accident:
/// `length(char(0))` is 0 and `length(char(65,0,66))` is 1, while `hex()` of
/// the same values shows all the bytes are there. A count that included them
/// answered 1 and 3.
///
/// A blob has no such rule - every byte of a blob is length - which is why this
/// is only applied to text.
///
/// @param bytes - the string's bytes
/// @param encoding - how they are encoded
fn before_nul(bytes: &[u8], encoding: TextEncoding) -> &[u8] {
    match encoding {
        TextEncoding::Utf8 => match bytes.iter().position(|byte| *byte == 0) {
            Some(at) => bytes.get(..at).unwrap_or(bytes),
            None => bytes,
        },
        // A UTF-16 NUL is two zero bytes on an even boundary; a lone zero byte
        // is the high half of an ordinary ASCII character.
        _ => {
            let mut at = 0usize;
            while at.saturating_add(1) < bytes.len() {
                if bytes.get(at) == Some(&0) && bytes.get(at.saturating_add(1)) == Some(&0) {
                    return bytes.get(..at).unwrap_or(bytes);
                }
                at = at.saturating_add(2);
            }
            bytes
        }
    }
}

/// `coalesce(...)` and `ifnull(a, b)`.
fn coalesce(arguments: &[Value<'static>]) -> Value<'static> {
    for argument in arguments {
        if !argument.is_null() {
            return argument.clone();
        }
    }
    Value::Null
}

/// `iif(condition, then, otherwise)`.
fn iif(arguments: &[Value<'static>]) -> Value<'static> {
    // Pairs of a test and its answer, with an optional final `ELSE`. The
    // three-argument form everybody writes is the shortest interesting case of
    // this and not a different function, which is why the loop rather than an
    // index: `iif(a,1,b,2,3)` is `CASE WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END`.
    let mut at = 0usize;
    while at + 1 < arguments.len() {
        let condition = arguments.get(at).cloned().unwrap_or(Value::Null);
        if eval::truth(&condition) == compare::Truth::True {
            return arguments.get(at + 1).cloned().unwrap_or(Value::Null);
        }
        at += 2;
    }
    // An odd argument count leaves one over, and that one is the `ELSE`.
    arguments.get(at).cloned().unwrap_or(Value::Null)
}

/// `unistr(x)`, which expands the two Unicode escapes.
///
/// `\uXXXX` and `\UXXXXXXXX` name a code point; the punctuation escapes stand
/// for themselves; and anything else is `invalid Unicode escape`, which is a
/// statement failure rather than a NULL. A non-text argument is returned
/// unchanged, which is the reference's answer for `unistr(1)`.
///
/// Returns `None` when the escape is not one of the two, which the caller turns
/// into the refusal - the sentence is in `refusal_for` so that the failure
/// carries a message rather than becoming a quiet NULL.
///
/// @param value - the argument, or nothing
fn unistr(value: Option<&Value<'static>>) -> Option<Value<'static>> {
    let value = value?;
    let Value::Text(text) = value else {
        return Some(value.clone());
    };
    let expanded = expand_unicode_escapes(&text.utf8_bytes())?;
    Some(Value::owned_text(&expanded).unwrap_or(Value::Null))
}

/// Expands `unistr`'s escapes, or returns nothing when one is malformed.
///
/// @param bytes - the text as written
fn expand_unicode_escapes(bytes: &[u8]) -> Option<Vec<u8>> {
    const PUNCTUATION: &[u8] = b"\\'\"";
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0usize;
    while at < bytes.len() {
        let byte = bytes.get(at).copied()?;
        if byte != b'\\' {
            out.push(byte);
            at += 1;
            continue;
        }
        let letter = bytes.get(at + 1).copied()?;
        let width = match letter {
            b'u' => 4usize,
            b'U' => 8,
            other if PUNCTUATION.contains(&other) => {
                out.push(other);
                at += 2;
                continue;
            }
            _ => return None,
        };
        let digits = bytes.get(at + 2..at + 2 + width)?;
        let mut point = 0u32;
        for digit in digits {
            point = point
                .checked_mul(16)?
                .checked_add(char::from(*digit).to_digit(16)?)?;
        }
        let character = char::from_u32(point)?;
        let mut buffer = [0u8; 4];
        out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        at += 2 + width;
    }
    Some(out)
}

/// `unistr_quote(x)`, which is `quote()` with the control characters escaped.
///
/// Text holding nothing below `0x20` quotes exactly as `quote()` does; text
/// that holds one is written as a `unistr('...')` call so that the result can
/// be pasted back into SQL and read the same way. Characters above ASCII are
/// **not** escaped, which is the reference's behaviour and is worth stating
/// because the name suggests otherwise.
///
/// @param value - the argument
/// @param encoding - the connection's text encoding
fn unistr_quote(value: &Value<'static>, encoding: TextEncoding) -> Value<'static> {
    let Value::Text(text) = value else {
        return quote(value, encoding);
    };
    let bytes = text.utf8_bytes();
    if !bytes.iter().any(|byte| *byte < 0x20) {
        return quote(value, encoding);
    }
    let mut out = b"unistr('".to_vec();
    for byte in bytes.iter() {
        match byte {
            b'\'' => out.extend_from_slice(b"''"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            other if *other < 0x20 => {
                out.extend_from_slice(format!("\\u{other:04x}").as_bytes());
            }
            other => out.push(*other),
        }
    }
    out.extend_from_slice(b"')");
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// `sqlite_compileoption_used(name)`.
fn compile_option_used(value: Option<&Value<'static>>) -> Value<'static> {
    let Some(Value::Text(text)) = value else {
        return Value::Null;
    };
    let name = String::from_utf8_lossy(&text.utf8_bytes()).into_owned();
    Value::Integer(i64::from(inillucent_base::compile_option_used(&name)))
}

/// `sqlite_compileoption_get(n)`, which is NULL past the end of the list.
fn compile_option_get(value: Option<&Value<'static>>) -> Value<'static> {
    let Some(index) = value.and_then(|value| match value {
        Value::Integer(number) => usize::try_from(*number).ok(),
        _ => None,
    }) else {
        return Value::Null;
    };
    inillucent_base::COMPILE_OPTIONS
        .get(index)
        .and_then(|option| Value::owned_text(option.as_bytes()).ok())
        .unwrap_or(Value::Null)
}

/// `regexp(pattern, subject)`, which is what `X REGEXP Y` compiles to.
///
/// Note the argument order: the operator puts the subject on the left and the
/// pattern on the right, and the function takes them the other way round. That
/// is SQLite's convention for every `X op Y` that is sugar for a function, and
/// getting it backwards is a bug that answers plausibly.
///
/// A pattern that will not compile is a statement failure rather than a NULL,
/// and the refusal is made in `refusal_for`; this only runs once the pattern is
/// known to be good.
fn regexp(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let (Some(pattern), Some(subject)) = (arguments.first(), arguments.get(1)) else {
        return Value::Null;
    };
    if pattern.is_null() || subject.is_null() {
        return Value::Null;
    }
    let pattern = eval::text_bytes(pattern, encoding);
    let subject = eval::text_bytes(subject, encoding);
    match crate::regexp::Regexp::compile(&pattern, false) {
        Ok(compiled) => Value::Integer(i64::from(compiled.matches(&subject))),
        Err(_) => Value::Null,
    }
}

/// `nullif(a, b)`.
fn null_if(arguments: &[Value<'static>], collation: Collation) -> Value<'static> {
    let left = arguments.first().cloned().unwrap_or(Value::Null);
    let right = arguments.get(1).cloned().unwrap_or(Value::Null);
    if left.is_null() {
        return Value::Null;
    }
    if !right.is_null()
        && compare::compare_values(&left, &right, collation) == std::cmp::Ordering::Equal
    {
        return Value::Null;
    }
    left
}

/// `min(...)` and `max(...)`, the scalar forms.
///
/// Any NULL argument makes the whole answer NULL, which is the opposite of
/// what the aggregates do and is the single most common surprise here.
///
/// **A tie keeps the later argument for `min` and the earlier for `max`
/// (task-1979, F19).** `min(1, 1.0)` is the real `1.0` in SQLite and `max(1,
/// 1.0)` is the integer `1`: two values that compare equal are still two
/// values, and which one comes back is decided by the direction of the
/// comparison the reference makes - `min` replaces on "not greater", `max` on
/// "greater". This kept the first of a tie in both directions, so `min(1, 1.0)`
/// answered the integer.
fn extreme(arguments: &[Value<'static>], collation: Collation, want_max: bool) -> Value<'static> {
    let mut best: Option<Value<'static>> = None;
    for argument in arguments {
        if argument.is_null() {
            return Value::Null;
        }
        best = Some(match best {
            None => argument.clone(),
            Some(current) => {
                let ordering = compare::compare_values(argument, &current, collation);
                let replace = if want_max {
                    ordering == std::cmp::Ordering::Greater
                } else {
                    ordering != std::cmp::Ordering::Greater
                };
                if replace {
                    argument.clone()
                } else {
                    current
                }
            }
        });
    }
    best.unwrap_or(Value::Null)
}

/// `lower(x)` and `upper(x)`, which fold ASCII only.
fn change_case(value: &Value<'_>, upper: bool, encoding: TextEncoding) -> Value<'static> {
    let bytes = eval::text_bytes(value, encoding);
    let folded: Vec<u8> = bytes
        .iter()
        .map(|byte| {
            if upper {
                byte.to_ascii_uppercase()
            } else {
                byte.to_ascii_lowercase()
            }
        })
        .collect();
    Value::owned_text(&folded).unwrap_or(Value::Null)
}

/// `hex(x)`, which renders bytes as upper-case hexadecimal.
fn hex(value: &Value<'_>, encoding: TextEncoding) -> Value<'static> {
    let bytes = match value {
        Value::Blob(blob) => blob.raw().to_vec(),
        other => eval::text_bytes(other, encoding),
    };
    let mut out = Vec::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// Returns the upper-case hexadecimal digit for a nibble.
fn hex_digit(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0'.saturating_add(nibble),
        _ => b'A'.saturating_add(nibble.saturating_sub(10)),
    }
}

/// `unhex(x[, ignored])`.
fn unhex(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let Some(value) = arguments.first() else {
        return Value::Null;
    };
    if value.is_null() {
        return Value::Null;
    }
    let ignored = arguments
        .get(1)
        .map(|value| eval::text_bytes(value, encoding))
        .unwrap_or_default();
    let bytes = eval::text_bytes(value, encoding);
    let filtered: Vec<u8> = bytes
        .into_iter()
        .filter(|byte| !ignored.contains(byte))
        .collect();
    if !filtered.len().is_multiple_of(2) {
        return Value::Null;
    }
    let mut out = Vec::with_capacity(filtered.len() / 2);
    let mut index = 0usize;
    while index < filtered.len() {
        let (Some(high), Some(low)) = (
            filtered.get(index).and_then(|byte| nibble(*byte)),
            filtered
                .get(index.saturating_add(1))
                .and_then(|byte| nibble(*byte)),
        ) else {
            return Value::Null;
        };
        out.push((high << 4) | low);
        index = index.saturating_add(2);
    }
    Value::owned_blob(&out).unwrap_or(Value::Null)
}

/// Returns the value of a hexadecimal digit.
fn nibble(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

/// `quote(x)`, which renders a value as SQL literal text.
fn quote(value: &Value<'_>, encoding: TextEncoding) -> Value<'static> {
    let rendered = match value {
        Value::Null => b"NULL".to_vec(),
        Value::Integer(_) | Value::Real(_) => eval::text_bytes(value, encoding),
        Value::Blob(blob) => {
            let mut out = b"X'".to_vec();
            for byte in blob.raw() {
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
            out.push(b'\'');
            out
        }
        Value::Text(text) => {
            let mut out = vec![b'\''];
            for byte in text.utf8_bytes().iter() {
                if *byte == b'\'' {
                    out.push(b'\'');
                }
                out.push(*byte);
            }
            out.push(b'\'');
            out
        }
    };
    Value::owned_text(&rendered).unwrap_or(Value::Null)
}

/// `char(...)`, which builds text from Unicode code points.
fn char_of(arguments: &[Value<'static>]) -> Value<'static> {
    let mut out = String::new();
    for argument in arguments {
        let code = cast::integer_value(argument);
        let code = u32::try_from(code).unwrap_or(0xfffd);
        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
    }
    Value::owned_text(out.as_bytes()).unwrap_or(Value::Null)
}

/// `unicode(x)`, which returns the first code point.
fn unicode(value: &Value<'_>, encoding: TextEncoding) -> Value<'static> {
    let bytes = eval::text_bytes(value, encoding);
    let text = String::from_utf8_lossy(&bytes);
    match text.chars().next() {
        Some(character) => Value::Integer(u32::from(character) as i64),
        None => Value::Null,
    }
}

/// `concat(...)`, which skips NULLs rather than propagating them.
fn concat(
    arguments: &[Value<'static>],
    separator: Option<&[u8]>,
    encoding: TextEncoding,
) -> Value<'static> {
    let mut out = Vec::new();
    let mut first = true;
    for argument in arguments {
        if argument.is_null() {
            continue;
        }
        if !first {
            if let Some(separator) = separator {
                out.extend_from_slice(separator);
            }
        }
        first = false;
        out.extend_from_slice(&eval::text_bytes(argument, encoding));
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// `concat_ws(separator, ...)`.
fn concat_with_separator(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let Some(separator) = arguments.first() else {
        return Value::Null;
    };
    if separator.is_null() {
        return Value::Null;
    }
    let separator = eval::text_bytes(separator, encoding);
    concat(
        arguments.get(1..).unwrap_or(&[]),
        Some(&separator),
        encoding,
    )
}

/// `instr(haystack, needle)`, counting characters from one.
fn instr(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let (Some(haystack), Some(needle)) = (arguments.first(), arguments.get(1)) else {
        return Value::Null;
    };
    if haystack.is_null() || needle.is_null() {
        return Value::Null;
    }
    let blobs = matches!(haystack, Value::Blob(_)) && matches!(needle, Value::Blob(_));
    let haystack_bytes = eval::text_bytes(haystack, encoding);
    let needle_bytes = eval::text_bytes(needle, encoding);
    let Some(offset) = find(&haystack_bytes, &needle_bytes) else {
        return Value::Integer(0);
    };
    if blobs {
        return Value::Integer(offset.saturating_add(1) as i64);
    }
    let prefix = haystack_bytes.get(..offset).unwrap_or(&[]);
    Value::Integer(numeric::character_count(prefix, TextEncoding::Utf8).saturating_add(1) as i64)
}

/// Returns the byte offset of a needle in a haystack.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len().saturating_sub(needle.len()))
        .find(|offset| haystack.get(*offset..offset.saturating_add(needle.len())) == Some(needle))
}

/// `replace(text, from, to)`.
fn replace(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let (Some(subject), Some(from), Some(to)) =
        (arguments.first(), arguments.get(1), arguments.get(2))
    else {
        return Value::Null;
    };
    if subject.is_null() || from.is_null() || to.is_null() {
        return Value::Null;
    }
    let subject = eval::text_bytes(subject, encoding);
    let from = eval::text_bytes(from, encoding);
    let to = eval::text_bytes(to, encoding);
    if from.is_empty() {
        return Value::owned_text(&subject).unwrap_or(Value::Null);
    }
    let mut out = Vec::with_capacity(subject.len());
    let mut index = 0usize;
    while index < subject.len() {
        let rest = subject.get(index..).unwrap_or(&[]);
        if rest.starts_with(&from) {
            out.extend_from_slice(&to);
            index = index.saturating_add(from.len());
            continue;
        }
        if let Some(byte) = subject.get(index) {
            out.push(*byte);
        }
        index = index.saturating_add(1);
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// `substr(x, start[, length])`, counting characters from one.
fn substring(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let Some(subject) = arguments.first() else {
        return Value::Null;
    };
    if subject.is_null() {
        return Value::Null;
    }
    let is_blob = matches!(subject, Value::Blob(_));
    let bytes = eval::text_bytes(subject, encoding);
    let units: Vec<Vec<u8>> = if is_blob {
        bytes.iter().map(|byte| vec![*byte]).collect()
    } else {
        characters(&bytes)
    };
    let total = units.len() as i64;
    let Some(start_value) = arguments.get(1) else {
        return Value::Null;
    };
    if start_value.is_null() {
        return Value::Null;
    }
    let mut start = cast::integer_value(start_value);
    let mut count = match arguments.get(2) {
        Some(value) if value.is_null() => return Value::Null,
        Some(value) => cast::integer_value(value),
        None => total,
    };
    // A negative start counts back from the end; a negative length runs
    // backwards from the start. Both are SQLite behaviours a naive slice gets
    // wrong by silently returning nothing.
    if start < 0 {
        start = total.saturating_add(start).saturating_add(1);
        if start < 1 {
            count = count.saturating_add(start).saturating_sub(1);
            start = 1;
        }
    } else if start == 0 {
        count = count.saturating_sub(1);
        start = 1;
    }
    if count < 0 {
        start = start.saturating_add(count);
        count = count.saturating_neg();
        if start < 1 {
            count = count.saturating_add(start).saturating_sub(1);
            start = 1;
        }
    }
    let first = start.saturating_sub(1).max(0) as usize;
    let last = first.saturating_add(count.max(0) as usize).min(units.len());
    let mut out = Vec::new();
    for unit in units.get(first.min(units.len())..last).unwrap_or(&[]) {
        out.extend_from_slice(unit);
    }
    if is_blob {
        return Value::owned_blob(&out).unwrap_or(Value::Null);
    }
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// Splits UTF-8 bytes into characters.
fn characters(bytes: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(bytes);
    text.chars()
        .map(|character| character.to_string().into_bytes())
        .collect()
}

/// `trim(x[, chars])`, and its one-sided forms.
fn trim(
    arguments: &[Value<'static>],
    left: bool,
    right: bool,
    encoding: TextEncoding,
) -> Value<'static> {
    let Some(subject) = arguments.first() else {
        return Value::Null;
    };
    if subject.is_null() {
        return Value::Null;
    }
    let cutset = match arguments.get(1) {
        Some(value) if value.is_null() => return Value::Null,
        Some(value) => characters(&eval::text_bytes(value, encoding)),
        None => vec![b" ".to_vec()],
    };
    let mut units = characters(&eval::text_bytes(subject, encoding));
    if left {
        while units.first().is_some_and(|unit| cutset.contains(unit)) {
            units.remove(0);
        }
    }
    if right {
        while units.last().is_some_and(|unit| cutset.contains(unit)) {
            units.pop();
        }
    }
    let out: Vec<u8> = units.concat();
    Value::owned_text(&out).unwrap_or(Value::Null)
}

/// `round(x[, digits])`, which rounds half away from zero.
fn round(arguments: &[Value<'static>]) -> Value<'static> {
    let Some(value) = arguments.first() else {
        return Value::Null;
    };
    if value.is_null() {
        return Value::Null;
    }
    let digits = match arguments.get(1) {
        Some(value) if value.is_null() => return Value::Null,
        Some(value) => cast::integer_value(value).clamp(0, 30),
        None => 0,
    };
    let real = cast::real_value(value);
    if !real.is_finite() {
        return Value::Real(real);
    }
    // **Zero places rounds the number; more places round its decimal text
    // (task-1979, F10).** That is not a nicety: `2.675` as a double is
    // 2.674999999999999822, so scaling it by a hundred and rounding half away
    // from zero answers 2.68 while SQLite answers 2.67 - because SQLite formats
    // the value to `n` places and reads the text back, and the text of
    // 2.674999... to two places is "2.67". `round(1.115,2)` and
    // `round(0.615,2)` differed the same way; `round(8.835,2)` agreed, because
    // that double sits just above the half rather than just below it. Rust's
    // own formatting is exact for the same reason SQLite's `%!.*f` is, so the
    // transcription is one line.
    if digits == 0 {
        let factor = 10f64.powi(0);
        let scaled = real * factor;
        // **Scaling a large value past the end of the range is not a
        // rounding.** `round(1e308, 2)` multiplied by 100, got infinity,
        // divided it by 100 and answered `Inf` - a value the source never held
        // and that SQLite never produces.
        if !scaled.is_finite() {
            return Value::Real(real);
        }
        // `f64::round` already rounds half away from zero, which is what
        // SQLite's own zero-places branch does with `(sqlite_int64)(r+0.5)`.
        return Value::Real(scaled.round() / factor);
    }
    let places = usize::try_from(digits).unwrap_or(0);
    match rounded_text(real, places).parse::<f64>() {
        Ok(rounded) => Value::Real(rounded),
        // A magnitude no decimal form can carry is already rounded to this many
        // places, so it is its own answer.
        Err(_) => Value::Real(real),
    }
}

/// Returns a number's decimal text at `places`, rounding a tie away from zero.
///
/// **Rust's formatter rounds a tie to even and SQLite's does not (task-1979,
/// F10).** `format!("{:.1}", 99.25)` is `99.2`, because 2 is even; SQLite's
/// `%!.*f` is its own implementation and rounds an exact half away from zero,
/// so it answers `99.3`. The same split shows on `round(0.125, 2)`: `0.12`
/// against `0.13`. Every value that is *not* an exact half already agreed,
/// which is why the formatter was the right idea and the wrong rounding:
/// `2.675` is 2.674999999999999822 as a double, and both answer `2.67`.
///
/// The number is expanded thirty digits past the place that is being kept, and
/// the decision is made on that text. Thirty is enough: a tie is a `5` followed
/// by nothing but zeros, so a digit further out than that makes the value
/// *larger* than the half, which rounds the same way a tie does.
///
/// @param real - the value
/// @param places - how many decimal places to keep
fn rounded_text(real: f64, places: usize) -> String {
    let wide = format!("{:.*}", places.saturating_add(30), real);
    let (sign, rest) = match wide.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", wide.as_str()),
    };
    let Some((whole, fraction)) = rest.split_once('.') else {
        return wide;
    };
    let Some(kept) = fraction.get(..places) else {
        return wide;
    };
    let up = fraction
        .as_bytes()
        .get(places)
        .is_some_and(|digit| *digit >= b'5');
    let mut digits: Vec<u8> = whole.bytes().chain(kept.bytes()).collect();
    if up {
        carry_one(&mut digits);
    }
    let text = String::from_utf8_lossy(&digits).into_owned();
    let point = text.len().saturating_sub(places);
    let (whole, fraction) = text.split_at(point.min(text.len()));
    match places {
        0 => format!("{sign}{whole}"),
        _ => format!("{sign}{whole}.{fraction}"),
    }
}

/// Adds one to a string of decimal digits, in place, growing it on a carry.
///
/// @param digits - the digits, most significant first
fn carry_one(digits: &mut Vec<u8>) {
    for digit in digits.iter_mut().rev() {
        if *digit < b'9' {
            *digit = digit.saturating_add(1);
            return;
        }
        *digit = b'0';
    }
    digits.insert(0, b'1');
}

/// `zeroblob(n)`.
fn zero_blob(value: Value<'static>) -> Value<'static> {
    let length = cast::integer_value(&value).clamp(0, 1_000_000_000) as usize;
    Value::owned_blob(&vec![0u8; length]).unwrap_or(Value::Null)
}

/// `like(pattern, text[, escape])` and `glob(pattern, text)`.
fn pattern_call(
    arguments: &[Value<'static>],
    is_like: bool,
    encoding: TextEncoding,
    fold_case: bool,
) -> Value<'static> {
    let (Some(pattern), Some(subject)) = (arguments.first(), arguments.get(1)) else {
        return Value::Null;
    };
    if pattern.is_null() || subject.is_null() {
        return Value::Null;
    }
    let escape = match arguments.get(2) {
        Some(value) if value.is_null() => return Value::Null,
        Some(value) => eval::text_bytes(value, encoding).first().copied(),
        None => None,
    };
    let pattern_bytes = eval::text_bytes(pattern, encoding);
    let subject_bytes = eval::text_bytes(subject, encoding);
    let matched = if is_like {
        crate::pattern::like_folding(&pattern_bytes, &subject_bytes, escape, fold_case)
    } else {
        crate::pattern::glob(&pattern_bytes, &subject_bytes)
    };
    Value::Integer(i64::from(matched))
}

/// Reads a vector written as a JSON array of numbers.
///
/// The grammar is `[` a comma separated list of numbers `]` and nothing else:
/// a string, an object or a nested array inside it means the text is not a
/// vector, and `None` is what leaves the caller answering NULL for it.
///
/// @param text - the value's bytes
fn vector_from_json(text: &[u8]) -> Option<Vec<f32>> {
    let held = std::str::from_utf8(text).ok()?.trim();
    let inner = held.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for part in inner.split(',') {
        out.push(part.trim().parse::<f64>().ok()? as f32);
    }
    Some(out)
}

/// A vector, as this engine stores one: little-endian `f32` in a blob.
///
/// **The same bytes `inillucent_search` writes**, which is what makes a column
/// of vectors and the retrieval store's own copies interchangeable. A blob
/// whose length is not a multiple of four is not a vector; nor is text, an
/// integer or a NULL, and each of those answers NULL rather than an error,
/// because a distance is an expression and an expression that raised on a NULL
/// would make `WHERE v IS NOT NULL AND vector_distance_cos(v, ?) < 0.2`
/// impossible to write.
///
/// **A JSON array of numbers is read as a vector too (task-1979, section 8.2,
/// gap 2).** `'[1, 0, 0, 0]'` is what pgvector takes and what an `INSERT` into
/// a `VECTOR(N)` column now accepts, so a query that wrote its query vector the
/// same way would otherwise have had a literal the insert understood and the
/// distance did not. Text that is not a JSON array of numbers is still not a
/// vector.
///
/// @param value - the argument
fn vector_of(value: Option<&Value<'static>>) -> Option<Vec<f32>> {
    if let Some(Value::Text(text)) = value {
        return vector_from_json(&text.utf8_bytes());
    }
    let Some(Value::Blob(blob)) = value else {
        return None;
    };
    let bytes = blob.raw();
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let mut raw = [0u8; 4];
        raw.copy_from_slice(chunk);
        out.push(f32::from_bits(u32::from_le_bytes(raw)));
    }
    Some(out)
}

/// Applies a two-vector measure, answering NULL when either side is not one.
///
/// The NULL answers this can still give are the two that mean *no vector*: a
/// NULL argument, and a pair whose cosine is undefined because a side has no
/// direction. Everything else that used to answer NULL here - a text argument,
/// a blob that is not a multiple of four bytes, two vectors of different widths
/// - is a refusal now, raised by [`refusal_for`] before the call
/// reaches this function. See that function for why.
///
/// @param arguments - the call's arguments
/// @param measure - what to compute from the pair
fn vector_pair(arguments: &[Value<'static>], measure: fn(&[f32], &[f32]) -> f64) -> Value<'static> {
    let (Some(left), Some(right)) = (vector_of(arguments.first()), vector_of(arguments.get(1)))
    else {
        return Value::Null;
    };
    if left.len() != right.len() {
        return Value::Null;
    }
    let answer = measure(&left, &right);
    if answer.is_nan() {
        return Value::Null;
    }
    Value::Real(answer)
}

/// Returns the sentence a function refuses with, before it is called at all.
///
/// A scalar returns a `Value`, so a function that has to *fail* cannot say so
/// through its return type. This is where those failures live, and the test for
/// belonging here is the same each time: would a NULL be indistinguishable from
/// a real answer? A vector measure over a mismatched pair, an invalid Unicode
/// escape and a pattern that will not compile all pass that test, and so does
/// `load_extension`, which must never quietly answer nothing.
///
/// @param func - which function is about to be called
/// @param arguments - what it is about to be called with
pub fn refusal_for(func: ScalarFunc, arguments: &[Value<'static>]) -> Option<String> {
    // Three functions refuse before they compute rather than answering NULL,
    // and the reason is the same in each case: a NULL would be an answer the
    // caller cannot tell from a real one.
    match func {
        // **`abs(-9223372036854775808)` has no answer (task-1913).** Its
        // absolute value is one past the largest integer, and SQLite raises
        // `integer overflow` rather than inventing one. This engine returned
        // `9.22337203685478e18`, a real - a wrong answer a caller cannot tell
        // from a right one, which is exactly the reason the three below refuse.
        ScalarFunc::Abs => {
            if matches!(arguments.first(), Some(Value::Integer(i64::MIN))) {
                return Some("integer overflow".to_string());
            }
            return None;
        }
        ScalarFunc::Unistr => {
            if let Some(Value::Text(text)) = arguments.first() {
                if expand_unicode_escapes(&text.utf8_bytes()).is_none() {
                    return Some("invalid Unicode escape".to_string());
                }
            }
            return None;
        }
        ScalarFunc::Regexp => {
            let pattern = arguments.first()?;
            if pattern.is_null() {
                return None;
            }
            let bytes = eval::text_bytes(pattern, TextEncoding::Utf8);
            return crate::regexp::Regexp::compile(&bytes, false)
                .err()
                .map(str::to_string);
        }
        // **No path is loadable.** This engine has no dynamic loader and is not
        // going to grow one: loading native code chosen by a string is the
        // vulnerability the registry's allow-list exists to close, and a build
        // that forbids `unsafe` cannot call `LoadLibrary` anyway. What it can
        // do is refuse in the words the platform uses, which is what the
        // reference reports for every path that is not a loadable extension -
        // including one that exists but is not one.
        ScalarFunc::LoadExtension => {
            return Some(MODULE_NOT_FOUND.to_string());
        }
        _ => {}
    }
    let name = match func {
        ScalarFunc::VectorDistanceCos => "vector_distance_cos",
        ScalarFunc::VectorDistanceL2 => "vector_distance_l2",
        ScalarFunc::VectorDot => "vector_dot",
        ScalarFunc::VectorDistanceL1 => "l1_distance",
        ScalarFunc::VectorAdd => "vector_add",
        ScalarFunc::VectorSubtract => "vector_sub",
        ScalarFunc::VectorMultiply => "vector_mul",
        _ => return None,
    };
    // **A number on one side of the three arithmetic ones is a scale**, which
    // is what `v * 2` means in pgvector - so it is not the error this check
    // exists to catch. The distances have no such form: a distance to a number
    // is a question with no answer, and saying so is the point.
    let scaling = matches!(
        func,
        ScalarFunc::VectorAdd | ScalarFunc::VectorSubtract | ScalarFunc::VectorMultiply
    ) && (number_of(arguments.first()).is_some()
        || number_of(arguments.get(1)).is_some());
    if scaling {
        return None;
    }
    let mut widths = [0usize; 2];
    for (at, slot) in widths.iter_mut().enumerate() {
        let value = arguments.get(at);
        if matches!(value, None | Some(Value::Null)) {
            return None;
        }
        let Some(vector) = vector_of(value) else {
            return Some(format!(
                "{name}: argument {} is not a vector",
                at.saturating_add(1)
            ));
        };
        *slot = vector.len();
    }
    let [left, right] = widths;
    if left != right {
        return Some(format!("different vector dimensions {left} and {right}"));
    }
    None
}

/// `sqlar_compress(X)`: a blob compressed when that makes it smaller.
///
/// Anything that is not a blob is returned as it stands, which is the
/// reference's rule and is what keeps a text column readable inside an archive.
///
/// @param value - the argument
fn sqlar_compress(value: Option<&Value<'static>>) -> Value<'static> {
    let Some(Value::Blob(blob)) = value else {
        return value.cloned().unwrap_or(Value::Null);
    };
    let raw = blob.raw();
    let packed = inillucent_base::deflate::zlib_compress(raw);
    if packed.len() < raw.len() {
        return Value::owned_blob(&packed).unwrap_or(Value::Null);
    }
    value.cloned().unwrap_or(Value::Null)
}

/// `sqlar_uncompress(Z, SZ)`: the inverse, given the size the row claims.
///
/// @param arguments - the stored blob and the content's size
fn sqlar_uncompress(arguments: &[Value<'static>]) -> Value<'static> {
    let Some(Value::Blob(blob)) = arguments.first() else {
        return arguments.first().cloned().unwrap_or(Value::Null);
    };
    let size = arguments.get(1).and_then(Value::as_integer).unwrap_or(0);
    if size <= 0 || size as usize == blob.raw().len() {
        return arguments.first().cloned().unwrap_or(Value::Null);
    }
    match inillucent_base::deflate::zlib_decompress(blob.raw()) {
        Ok(bytes) => Value::owned_blob(&bytes).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// Returns the depth an R-Tree node's header records.
///
/// The first two bytes, big-endian. Zero for every node but the root, which is
/// what makes this worth having: the root's depth is the tree's height, and a
/// height that disagrees with a walk is a corrupt tree.
///
/// @param value - the node blob
fn rtree_depth(value: Option<&Value<'static>>) -> Value<'static> {
    let Some(Value::Blob(blob)) = value else {
        return Value::Null;
    };
    // A blob a caller chose, so the read is bounded by `get` rather than by
    // the length check above (task-1932, H9).
    let bytes = blob.raw();
    let Some(pair) = bytes
        .get(..2)
        .and_then(|head| <[u8; 2]>::try_from(head).ok())
    else {
        return Value::Null;
    };
    Value::Integer(i64::from(u16::from_be_bytes(pair)))
}

/// Returns an R-Tree node rendered as a readable list.
///
/// `{rowid x0 x1 ...} {rowid ...}`, one brace group per cell, which is the
/// reference's own rendering - a Tcl list, because the routine was written for
/// SQLite's Tcl test suite and the format outlived the reason.
///
/// @param arguments - how many dimensions, then the node blob
fn rtree_node(arguments: &[Value<'static>]) -> Value<'static> {
    let dimensions = arguments.first().map_or(0, cast::integer_value);
    if !(1..=5).contains(&dimensions) {
        return Value::Null;
    }
    let dimensions = dimensions as usize;
    let Some(Value::Blob(blob)) = arguments.get(1) else {
        return Value::Null;
    };
    // An R-Tree node blob a caller chose, so every read of it goes through
    // `get` (task-1932, H9). The length checks stay - they are what makes a
    // short blob NULL rather than a partial rendering - and the reads no longer
    // depend on them being right.
    let bytes = blob.raw();
    let Some(count) = bytes
        .get(2..4)
        .and_then(|head| <[u8; 2]>::try_from(head).ok())
    else {
        return Value::Null;
    };
    let cells = usize::from(u16::from_be_bytes(count));
    let width = 8usize.saturating_add(dimensions.saturating_mul(8));
    if bytes.len() < 4usize.saturating_add(cells.saturating_mul(width)) {
        return Value::Null;
    }
    let mut out = String::new();
    for cell in 0..cells {
        if cell > 0 {
            out.push(' ');
        }
        let at = 4usize.saturating_add(cell.saturating_mul(width));
        let Some(key) = bytes
            .get(at..at.saturating_add(8))
            .and_then(|head| <[u8; 8]>::try_from(head).ok())
        else {
            return Value::Null;
        };
        out.push_str(&format!("{{{}", i64::from_be_bytes(key)));
        for value in 0..dimensions.saturating_mul(2) {
            let from = at.saturating_add(8).saturating_add(value.saturating_mul(4));
            let Some(word) = bytes
                .get(from..from.saturating_add(4))
                .and_then(|head| <[u8; 4]>::try_from(head).ok())
            else {
                return Value::Null;
            };
            out.push(' ');
            out.push_str(&crate::printf::general(f64::from(f32::from_be_bytes(word))));
        }
        out.push('}');
    }
    Value::owned_text(out.as_bytes()).unwrap_or(Value::Null)
}

/// Applies a measure to a polygon, answering NULL when there is not one.
///
/// @param arguments - the call's arguments, the polygon first
/// @param measure - what to compute
fn geopoly_measure(
    arguments: &[Value<'static>],
    measure: impl FnOnce(&crate::geopoly::Polygon) -> Value<'static>,
) -> Value<'static> {
    match crate::geopoly::Polygon::parse(arguments.first()) {
        Some(shape) => measure(&shape),
        None => Value::Null,
    }
}

/// Applies a transform to a polygon and answers the stored form.
///
/// @param arguments - the call's arguments, the polygon first
/// @param transform - what to make of it
fn geopoly_shape(
    arguments: &[Value<'static>],
    transform: impl FnOnce(crate::geopoly::Polygon) -> Option<crate::geopoly::Polygon>,
) -> Value<'static> {
    let Some(shape) = crate::geopoly::Polygon::parse(arguments.first()) else {
        return Value::Null;
    };
    match transform(shape) {
        Some(made) => Value::owned_blob(&made.to_blob()).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

/// Compares two polygons, answering NULL when either is not one.
///
/// @param arguments - the two polygons
/// @param compare - what to compute
fn geopoly_pair(
    arguments: &[Value<'static>],
    compare: impl FnOnce(&crate::geopoly::Polygon, &crate::geopoly::Polygon) -> i64,
) -> Value<'static> {
    let (Some(first), Some(second)) = (
        crate::geopoly::Polygon::parse(arguments.first()),
        crate::geopoly::Polygon::parse(arguments.get(1)),
    ) else {
        return Value::Null;
    };
    Value::Integer(compare(&first, &second))
}

/// Renders a polygon as an SVG `<polyline>`.
///
/// @param arguments - the polygon, then the attributes to write into the tag
/// @param encoding - the connection's text encoding
fn geopoly_svg(arguments: &[Value<'static>], encoding: TextEncoding) -> Value<'static> {
    let Some(shape) = crate::geopoly::Polygon::parse(arguments.first()) else {
        return Value::Null;
    };
    let attributes: Vec<String> = arguments
        .iter()
        .skip(1)
        .map(|value| {
            String::from_utf8_lossy(&crate::printf::rendered_text(Some(value), encoding))
                .into_owned()
        })
        .collect();
    Value::owned_text(shape.to_svg(&attributes).as_bytes()).unwrap_or(Value::Null)
}

/// Builds a regular polygon around a centre.
///
/// @param arguments - the centre, the circumradius and how many sides
fn geopoly_regular(arguments: &[Value<'static>]) -> Value<'static> {
    let x = arguments.first().map_or(0.0, cast::real_value);
    let y = arguments.get(1).map_or(0.0, cast::real_value);
    let radius = arguments.get(2).map_or(0.0, cast::real_value);
    let sides = arguments.get(3).map_or(0, cast::integer_value);
    match crate::geopoly::regular(x, y, radius, sides) {
        Some(shape) => Value::owned_blob(&shape.to_blob()).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

/// Returns 74 when a value is JSON text for an array or an object.
///
/// @param value - the extracted value
fn json_shaped(value: Option<&Value<'static>>) -> i64 {
    let Some(Value::Text(text)) = value else {
        return 0;
    };
    let raw = text.utf8_bytes();
    match raw.iter().find(|byte| !byte.is_ascii_whitespace()) {
        Some(b'[') | Some(b'{') => 74,
        _ => 0,
    }
}

/// Returns the taxicab distance between two vectors.
fn taxicab_distance(left: &[f32], right: &[f32]) -> f64 {
    let mut total = 0.0f64;
    for (one, two) in left.iter().zip(right.iter()) {
        total += (f64::from(*one) - f64::from(*two)).abs();
    }
    total
}

/// Returns the raw bytes of a blob argument, which is what a bit vector is.
///
/// **Bytes rather than components**, because pgvector's `bit` type is a string
/// of bits and its two distances count bits rather than dimensions. The blob
/// `binary_quantize` writes is one, and so is any blob of the same length.
///
/// @param value - the argument
fn bits_of(value: Option<&Value<'static>>) -> Option<Vec<u8>> {
    match value {
        Some(Value::Blob(blob)) => Some(blob.raw().to_vec()),
        _ => None,
    }
}

/// Applies a measure over two bit vectors of the same length.
///
/// @param arguments - the call's arguments
/// @param measure - what to compute
fn bit_pair(arguments: &[Value<'static>], measure: fn(&[u8], &[u8]) -> f64) -> Value<'static> {
    let (Some(left), Some(right)) = (bits_of(arguments.first()), bits_of(arguments.get(1))) else {
        return Value::Null;
    };
    if left.len() != right.len() || left.is_empty() {
        return Value::Null;
    }
    let answer = measure(&left, &right);
    if answer.is_nan() {
        return Value::Null;
    }
    Value::Real(answer)
}

/// Returns how many bits differ between two bit vectors.
fn hamming_distance(left: &[u8], right: &[u8]) -> f64 {
    let mut total = 0u32;
    for (one, two) in left.iter().zip(right.iter()) {
        total = total.saturating_add((one ^ two).count_ones());
    }
    f64::from(total)
}

/// Returns one minus the ratio of the shared bits to the set bits.
///
/// Two vectors with no bits set at all have an empty union, and a ratio over an
/// empty union is undefined rather than zero - so it answers NULL through the
/// NaN check, the same way an undefined cosine does.
fn jaccard_distance(left: &[u8], right: &[u8]) -> f64 {
    let mut shared = 0u32;
    let mut either = 0u32;
    for (one, two) in left.iter().zip(right.iter()) {
        shared = shared.saturating_add((one & two).count_ones());
        either = either.saturating_add((one | two).count_ones());
    }
    if either == 0 {
        return f64::NAN;
    }
    1.0 - f64::from(shared) / f64::from(either)
}

/// Returns how many components a vector has.
///
/// @param value - the vector
fn vector_dims(value: Option<&Value<'static>>) -> Value<'static> {
    match vector_of(value) {
        Some(vector) => Value::Integer(vector.len() as i64),
        None => Value::Null,
    }
}

/// Returns a vector's Euclidean length.
///
/// @param value - the vector
fn vector_norm(value: Option<&Value<'static>>) -> Value<'static> {
    let Some(vector) = vector_of(value) else {
        return Value::Null;
    };
    let total: f64 = vector
        .iter()
        .map(|one| f64::from(*one) * f64::from(*one))
        .sum();
    Value::Real(total.sqrt())
}

/// Returns the same direction with length one.
///
/// A zero vector has no direction, and pgvector answers it with itself rather
/// than with a division by zero. That is what this does too.
///
/// @param value - the vector
fn vector_normalize(value: Option<&Value<'static>>) -> Value<'static> {
    let Some(vector) = vector_of(value) else {
        return Value::Null;
    };
    let total: f64 = vector
        .iter()
        .map(|one| f64::from(*one) * f64::from(*one))
        .sum();
    let length = total.sqrt();
    if length == 0.0 {
        return vector_value(&vector);
    }
    let scaled: Vec<f32> = vector
        .iter()
        .map(|one| (f64::from(*one) / length) as f32)
        .collect();
    vector_value(&scaled)
}

/// Returns one bit per component, set when the component is positive.
///
/// **Most significant bit first within each byte**, which is how pgvector's
/// `bit` type is laid out and therefore what `hamming_distance` has to count
/// over. A width that is not a multiple of eight leaves the low bits of the
/// last byte clear, so two vectors of the same width always compare over the
/// same padding.
///
/// @param value - the vector
fn binary_quantize(value: Option<&Value<'static>>) -> Value<'static> {
    let Some(vector) = vector_of(value) else {
        return Value::Null;
    };
    let mut bytes = vec![0u8; vector.len().div_ceil(8)];
    for (at, component) in vector.iter().enumerate() {
        if *component > 0.0 {
            if let Some(slot) = bytes.get_mut(at / 8) {
                *slot |= 0x80u8 >> (at % 8);
            }
        }
    }
    Value::owned_blob(&bytes).unwrap_or(Value::Null)
}

/// Returns a slice of a vector, counted from one.
///
/// A start before the first component or a count that runs off the end is a
/// refusal in pgvector; here it is NULL, which is what every other measure in
/// this file answers when it was handed something that is not a vector of the
/// shape the call needs.
///
/// @param arguments - the vector, the one-based start, and how many
fn subvector(arguments: &[Value<'static>]) -> Value<'static> {
    let Some(vector) = vector_of(arguments.first()) else {
        return Value::Null;
    };
    let (Some(start), Some(count)) = (
        arguments.get(1).and_then(Value::as_integer),
        arguments.get(2).and_then(Value::as_integer),
    ) else {
        return Value::Null;
    };
    if start < 1 || count < 1 {
        return Value::Null;
    }
    let from = (start - 1) as usize;
    let to = from.saturating_add(count as usize);
    let Some(slice) = vector.get(from..to) else {
        return Value::Null;
    };
    vector_value(slice)
}

/// Applies an operation to two vectors component by component.
///
/// @param arguments - the two vectors
/// @param combine - what to do with each pair of components
fn vector_zip(arguments: &[Value<'static>], combine: fn(f32, f32) -> f32) -> Value<'static> {
    // **A number on one side scales every component**, which is what `v * 2`
    // means in pgvector and what the operator form of these three brought with
    // it. `vector_mul(v, 2)` therefore means it too, because one meaning per
    // operation is the only way the function form and the operator form can be
    // read as the same thing.
    let one = vector_of(arguments.first());
    let two = vector_of(arguments.get(1));
    if let (Some(held), None) = (&one, &two) {
        return match number_of(arguments.get(1)) {
            Some(scale) => {
                let out: Vec<f32> = held.iter().map(|value| combine(*value, scale)).collect();
                vector_value(&out)
            }
            None => Value::Null,
        };
    }
    if let (None, Some(held)) = (&one, &two) {
        return match number_of(arguments.first()) {
            Some(scale) => {
                let out: Vec<f32> = held.iter().map(|value| combine(scale, *value)).collect();
                vector_value(&out)
            }
            None => Value::Null,
        };
    }
    let (Some(left), Some(right)) = (one, two) else {
        return Value::Null;
    };
    if left.len() != right.len() {
        return Value::Null;
    }
    let combined: Vec<f32> = left
        .iter()
        .zip(right.iter())
        .map(|(one, two)| combine(*one, *two))
        .collect();
    vector_value(&combined)
}

/// Returns one vector followed by the other.
///
/// @param arguments - the two vectors
fn vector_concat(arguments: &[Value<'static>]) -> Value<'static> {
    let (Some(left), Some(right)) = (vector_of(arguments.first()), vector_of(arguments.get(1)))
    else {
        return Value::Null;
    };
    let mut joined = left;
    joined.extend_from_slice(&right);
    vector_value(&joined)
}

/// Returns a vector as the blob this engine stores one in.
///
/// @param vector - the components
fn vector_value(vector: &[f32]) -> Value<'static> {
    let mut bytes = Vec::with_capacity(vector.len().saturating_mul(4));
    for component in vector {
        bytes.extend_from_slice(&component.to_bits().to_le_bytes());
    }
    Value::owned_blob(&bytes).unwrap_or(Value::Null)
}

/// Returns the cosine *distance*, `1 - cos(a, b)`, in `[0, 2]`.
///
/// A zero vector has no direction, so its cosine is undefined; that answers
/// NULL through the NaN check above rather than pretending the distance is 1.
fn cosine_distance(left: &[f32], right: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (one, two) in left.iter().zip(right.iter()) {
        dot += f64::from(*one) * f64::from(*two);
        left_norm += f64::from(*one) * f64::from(*one);
        right_norm += f64::from(*two) * f64::from(*two);
    }
    let scale = left_norm.sqrt() * right_norm.sqrt();
    if scale == 0.0 {
        return f64::NAN;
    }
    // **Clamped, because the arithmetic overshoots and the overshoot is
    // visible.** A vector against itself gives `dot / scale` of one plus an ulp
    // or two, and `1 - that` is then a tiny *negative* distance - which is not
    // wrong by any amount anybody cares about and is deeply confusing to read.
    // The cosine of two real vectors is in `[-1, 1]`, so the distance is in
    // `[0, 2]`, and saying so costs one comparison.
    (1.0 - dot / scale).clamp(0.0, 2.0)
}

/// Returns the Euclidean distance between two vectors.
fn euclidean_distance(left: &[f32], right: &[f32]) -> f64 {
    let mut total = 0.0f64;
    for (one, two) in left.iter().zip(right.iter()) {
        let gap = f64::from(*one) - f64::from(*two);
        total += gap * gap;
    }
    total.sqrt()
}

/// Returns the dot product of two vectors.
fn dot_product(left: &[f32], right: &[f32]) -> f64 {
    let mut total = 0.0f64;
    for (one, two) in left.iter().zip(right.iter()) {
        total += f64::from(*one) * f64::from(*two);
    }
    total
}

/// What the platform says when a library cannot be loaded.
///
/// The trailing newline is the reference's, not a stray: Windows'
/// `FormatMessage` ends its sentences with CR LF and SQLite passes the string
/// through untouched, so a transcript compared byte for byte has a blank line
/// after the error. On other platforms the loader says something else, which is
/// why this is per-platform rather than one string.
#[cfg(windows)]
const MODULE_NOT_FOUND: &str = "The specified module could not be found.\r\n";

/// As above, for a loader that reports in the C library's words.
#[cfg(not(windows))]
const MODULE_NOT_FOUND: &str = "no such file or directory";

/// Returns a value as an `f32`, when it is a number.
///
/// @param value - the side that is not a vector
fn number_of(value: Option<&Value<'static>>) -> Option<f32> {
    match value {
        Some(Value::Integer(number)) => Some(*number as f32),
        Some(Value::Real(number)) => Some(*number as f32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calls a function with owned arguments, for the tests.
    fn run(func: ScalarFunc, arguments: Vec<Value<'static>>) -> Value<'static> {
        call(func, &arguments, Collation::Binary, TextEncoding::Utf8)
    }

    /// `length` counts characters in text and bytes in a blob, which is the
    /// distinction people most often get wrong.
    #[test]
    fn length_counts_characters_in_text_and_bytes_in_a_blob() {
        let text = Value::owned_text("héllo".as_bytes()).expect("owned");
        assert_same!(run(ScalarFunc::Length, vec![text]), Value::Integer(5));
        let blob = Value::owned_blob("héllo".as_bytes()).expect("owned");
        assert_same!(run(ScalarFunc::Length, vec![blob]), Value::Integer(6));
    }

    /// `substr` counts from one, and its negative forms work.
    #[test]
    fn substr_counts_from_one_and_accepts_negatives() {
        let text = || Value::owned_text(b"abcdef").expect("owned");
        assert_same!(
            run(
                ScalarFunc::Substr,
                vec![text(), Value::Integer(2), Value::Integer(3)]
            ),
            Value::owned_text(b"bcd").expect("owned")
        );
        assert_same!(
            run(ScalarFunc::Substr, vec![text(), Value::Integer(-2)]),
            Value::owned_text(b"ef").expect("owned")
        );
        assert_same!(
            run(
                ScalarFunc::Substr,
                vec![text(), Value::Integer(4), Value::Integer(-2)]
            ),
            Value::owned_text(b"bc").expect("owned")
        );
    }

    /// The scalar `max` is NULL if any argument is NULL, which is the opposite
    /// of the aggregate.
    #[test]
    fn scalar_max_is_null_if_any_argument_is_null() {
        assert_same!(
            run(
                ScalarFunc::Max,
                vec![Value::Integer(1), Value::Null, Value::Integer(3)]
            ),
            Value::Null
        );
        assert_same!(
            run(ScalarFunc::Max, vec![Value::Integer(1), Value::Integer(3)]),
            Value::Integer(3)
        );
    }

    /// `concat` skips NULLs instead of propagating them, unlike `||`.
    #[test]
    fn concat_skips_nulls() {
        assert_same!(
            run(
                ScalarFunc::Concat,
                vec![
                    Value::owned_text(b"a").expect("owned"),
                    Value::Null,
                    Value::owned_text(b"b").expect("owned")
                ]
            ),
            Value::owned_text(b"ab").expect("owned")
        );
    }

    /// `round` rounds half away from zero.
    #[test]
    fn round_rounds_half_away_from_zero() {
        assert_same!(
            run(ScalarFunc::Round, vec![Value::Real(2.5)]),
            Value::Real(3.0)
        );
        assert_same!(
            run(ScalarFunc::Round, vec![Value::Real(-2.5)]),
            Value::Real(-3.0)
        );
        assert_same!(
            run(
                ScalarFunc::Round,
                vec![Value::Real(2.345), Value::Integer(2)]
            ),
            Value::Real(2.35)
        );
    }

    /// `quote` renders each class the way the SQL literal for it is written.
    #[test]
    fn quote_renders_sql_literals() {
        assert_same!(
            run(ScalarFunc::Quote, vec![Value::Null]),
            Value::owned_text(b"NULL").expect("owned")
        );
        assert_same!(
            run(
                ScalarFunc::Quote,
                vec![Value::owned_text(b"it's").expect("owned")]
            ),
            Value::owned_text(b"'it''s'").expect("owned")
        );
        assert_same!(
            run(
                ScalarFunc::Quote,
                vec![Value::owned_blob(&[0x41, 0x0a]).expect("owned")]
            ),
            Value::owned_text(b"X'410A'").expect("owned")
        );
    }

    /// `typeof` names the storage class, not the declared type - and it names
    /// it for NULL too, which is why it does not propagate NULL.
    #[test]
    fn typeof_names_the_storage_class() {
        assert_same!(
            run(ScalarFunc::TypeOf, vec![Value::Integer(1)]),
            Value::owned_text(b"integer").expect("owned")
        );
        assert_same!(
            run(ScalarFunc::TypeOf, vec![Value::Null]),
            Value::owned_text(b"null").expect("owned")
        );
    }

    /// The four built-ins that answer a question about their argument answer it
    /// for NULL as well, rather than returning NULL.
    #[test]
    fn the_reflective_builtins_do_not_propagate_null() {
        assert_same!(
            run(ScalarFunc::Quote, vec![Value::Null]),
            Value::owned_text(b"NULL").expect("owned")
        );
        assert_same!(
            run(ScalarFunc::Hex, vec![Value::Null]),
            Value::owned_text(b"").expect("owned")
        );
        assert_same!(
            run(ScalarFunc::ZeroBlob, vec![Value::Null]),
            Value::owned_blob(b"").expect("owned")
        );
        // And the ones that compute *with* their argument still do propagate.
        assert_same!(run(ScalarFunc::Abs, vec![Value::Null]), Value::Null);
        assert_same!(run(ScalarFunc::Lower, vec![Value::Null]), Value::Null);
    }

    /// `instr` counts characters from one and returns zero when absent.
    #[test]
    fn instr_counts_characters_from_one() {
        let haystack = Value::owned_text("héllo".as_bytes()).expect("owned");
        let needle = Value::owned_text(b"llo").expect("owned");
        assert_same!(
            run(ScalarFunc::Instr, vec![haystack, needle]),
            Value::Integer(3)
        );
        assert_same!(
            run(
                ScalarFunc::Instr,
                vec![
                    Value::owned_text(b"abc").expect("owned"),
                    Value::owned_text(b"z").expect("owned")
                ]
            ),
            Value::Integer(0)
        );
    }
}
