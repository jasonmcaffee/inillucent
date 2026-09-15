//! Turning one value into the text a suite compares.
//!
//! Invariant: **each of these is one rendering, used by every suite that wants
//! it.** A suite that kept its own copy of a renderer and a suite that used
//! this one would disagree the day either changed, and the disagreement would
//! read as a difference between two engines rather than as a difference
//! between two `format!` strings.
//!
//! **Why there are three rather than one (task-1962, A12).** Thirteen test
//! files carried a private `fn render` and there were three distinct ones among
//! them, duplicated four, four and five times. They are not variants of one
//! renderer: [`shell_text`] prints what the SQLite shell prints, so it has to
//! render NULL as nothing at all; [`tagged`] names the type in the text, which
//! is the only way a suite can tell `1` from `'1'`; and [`datum_text`] rounds a
//! real to six places and prints a blob's length, which is what the search
//! suites compare. Merging them would lose a distinction each was making.

use inillucent_tree::datum::OwnedDatum;
use inillucent_value::Value;

/// Renders a value the way the SQLite shell's default output does.
///
/// NULL is the empty string, a real is `{}` rather than `{:?}`, and text and
/// blobs are their bytes read lossily - which is what `.mode list` writes and
/// therefore what a comparison against the shell's stdout has to produce.
///
/// @param value - the value to render
pub fn shell_text(value: &Value<'_>) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => format!("{number}"),
        Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).to_string(),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes.raw()).to_string(),
    }
}

/// Renders a value with its type named in the text.
///
/// `int:1` and `text:1` are different strings, which is what makes this the
/// rendering a differential suite compares: an engine that answered the right
/// number with the wrong affinity would pass a comparison of the digits alone.
/// The real uses `{:?}` so that `1.0` does not print as `1`, and the blob is
/// hex so that a byte difference is visible.
///
/// @param value - the value to render
pub fn tagged(value: &Value<'_>) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(integer) => format!("int:{integer}"),
        Value::Real(real) => format!("real:{real:?}"),
        Value::Text(text) => format!("text:{}", String::from_utf8_lossy(&text.utf8_bytes())),
        Value::Blob(blob) => format!(
            "blob:{}",
            blob.raw()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

/// Renders an owned datum the way the retrieval suites compare one.
///
/// A real is rounded to six places, because a score is compared against a
/// number a person wrote down; a blob prints its length rather than its bytes,
/// because the blobs here are embeddings and their bytes are not what any of
/// those suites is asserting about.
///
/// @param value - the value to render
pub fn datum_text(value: &OwnedDatum) -> String {
    match value {
        OwnedDatum::Null => "NULL".to_string(),
        OwnedDatum::Int(number) => number.to_string(),
        OwnedDatum::Real(number) => format!("{number:.6}"),
        OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        OwnedDatum::Blob(bytes) => format!("blob:{}", bytes.len()),
    }
}
