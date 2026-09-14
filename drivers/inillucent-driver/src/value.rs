//! A cell, and what a column is called.
//!
//! **Values are typed, not text**, and that is a decision with a reason rather
//! than an accident of what was easy. `unluminous-db`'s PostgreSQL client takes
//! every value as the text the server printed, deliberately: asking PostgreSQL
//! for binary means one decoder per type OID, and then a `numeric` that renders
//! differently from the way `psql` renders it.
//!
//! That argument is about a wire protocol and it does not survive the trip
//! in-process. The engine hands this driver a value that is already
//! `Null | Int | Real | Text | Blob`; flattening it to a string would mean the
//! driver choosing a float's formatting on the application's behalf, and the
//! application parsing it back if it wanted the number. So the five arrive as
//! five, which is also what `rusqlite` gives the same consumer today.
//!
//! Invariant: **a value keeps the type the engine gave it, all the way to the
//! caller.** Rendering it as text and parsing it back is where a storage class
//! is lost, and the loss is silent: an integer that arrives as a string still
//! prints correctly and no longer compares correctly.

use inillucent_engine::Value as EngineValue;

/// One cell.
///
/// NULL is a variant rather than an empty string. They are different values,
/// and every layer above that draws them the same is a layer nobody can trust:
/// a column never filled in and one filled in with nothing are not the same
/// fact about a row.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    /// SQL NULL.
    #[default]
    Null,
    /// A signed 64-bit integer.
    Integer(i64),
    /// An IEEE-754 binary64.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Uninterpreted bytes.
    Blob(Vec<u8>),
}

/// What kind of value a cell holds, as a number the C ABI can report.
///
/// The numbers are frozen: a binding written against them keeps working when a
/// variant is added, because a variant would take the next number rather than
/// renumbering the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
#[non_exhaustive]
pub enum ValueKind {
    /// [`Value::Null`].
    Null = 0,
    /// [`Value::Integer`].
    Integer = 1,
    /// [`Value::Real`].
    Real = 2,
    /// [`Value::Text`].
    Text = 3,
    /// [`Value::Blob`].
    Blob = 4,
}

impl Value {
    /// Returns which variant this is.
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Null => ValueKind::Null,
            Value::Integer(_) => ValueKind::Integer,
            Value::Real(_) => ValueKind::Real,
            Value::Text(_) => ValueKind::Text,
            Value::Blob(_) => ValueKind::Blob,
        }
    }

    /// Returns whether this cell is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Returns the text of this cell, when it has one.
    ///
    /// A number does **not** answer here. A caller that wants a number rendered
    /// asks for the number and renders it, because the rendering is a decision
    /// about how it looks and this driver does not take it.
    pub fn text(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    /// Returns the bytes of this cell, when it holds bytes.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(bytes) => Some(bytes),
            Value::Text(text) => Some(text.as_bytes()),
            _ => None,
        }
    }

    /// Copies a value coming out of the engine.
    ///
    /// **Invalid UTF-8 in a text column becomes a blob, not a replacement
    /// character.** The engine's text is bytes; a lossy conversion would hand a
    /// caller a string that is not what is stored, which is a wrong answer with
    /// no error attached to it. A caller that sees `Blob` where it expected
    /// `Text` has been told something true.
    ///
    /// @param value - the engine's value
    pub fn from_engine(value: &EngineValue) -> Value {
        match value {
            EngineValue::Null => Value::Null,
            EngineValue::Int(number) => Value::Integer(*number),
            EngineValue::Real(number) => Value::Real(*number),
            EngineValue::Text(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => Value::Text(text.to_owned()),
                Err(_) => Value::Blob(bytes.clone()),
            },
            EngineValue::Blob(bytes) => Value::Blob(bytes.clone()),
        }
    }

    /// Copies a value going into the engine.
    pub fn to_engine(&self) -> EngineValue {
        match self {
            Value::Null => EngineValue::Null,
            Value::Integer(number) => EngineValue::Int(*number),
            Value::Real(number) => EngineValue::Real(*number),
            Value::Text(text) => EngineValue::Text(text.as_bytes().to_vec()),
            Value::Blob(bytes) => EngineValue::Blob(bytes.clone()),
        }
    }
}

impl Value {
    /// Copies a value an application-defined function was handed.
    ///
    /// The engine's expression value is a different type from the one a row
    /// carries: it borrows, it records a text encoding, and it is what runs
    /// inside a statement. Converting once here is what lets a caller write a
    /// function against the same five variants it reads rows in.
    ///
    /// @param value - the engine's expression value
    pub fn from_expr(value: &inillucent_engine::ExprValue<'_>) -> Value {
        match value {
            inillucent_engine::ExprValue::Null => Value::Null,
            inillucent_engine::ExprValue::Integer(number) => Value::Integer(*number),
            inillucent_engine::ExprValue::Real(number) => Value::Real(*number),
            inillucent_engine::ExprValue::Text(text) => match std::str::from_utf8(text.raw()) {
                Ok(said) => Value::Text(said.to_owned()),
                Err(_) => Value::Blob(text.raw().to_vec()),
            },
            inillucent_engine::ExprValue::Blob(bytes) => Value::Blob(bytes.raw().to_vec()),
        }
    }

    /// Copies a value an application-defined function is answering with.
    ///
    /// Owned rather than borrowed, because the caller's function has returned
    /// by the time the engine reads it and anything borrowed would be borrowed
    /// from a frame that is gone.
    pub fn to_expr(&self) -> inillucent_engine::DbResult<inillucent_engine::ExprValue<'static>> {
        match self {
            Value::Null => Ok(inillucent_engine::ExprValue::Null),
            Value::Integer(number) => Ok(inillucent_engine::ExprValue::Integer(*number)),
            Value::Real(number) => Ok(inillucent_engine::ExprValue::Real(*number)),
            Value::Text(text) => inillucent_engine::ExprValue::owned_text(text.as_bytes()),
            Value::Blob(bytes) => inillucent_engine::ExprValue::owned_blob(bytes),
        }
    }
}

/// One column of a result.
///
/// Two fields, because two are all the engine can honestly answer for an
/// arbitrary statement. A column that is an expression has no declared type -
/// there is none to have - and the empty string is the right answer for it
/// rather than a guess taken from the first row's value, which would be a
/// different answer on a different page of the same query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// What the statement called it.
    pub name: String,
    /// The type the schema declared, or empty for an expression.
    pub declared_type: String,
}

impl Column {
    /// Returns a column with a name and a declared type.
    ///
    /// @param name - the result column's name
    /// @param declared_type - the schema's type name, or empty
    pub fn new(name: impl Into<String>, declared_type: impl Into<String>) -> Column {
        Column {
            name: name.into(),
            declared_type: declared_type.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NULL and the empty string are different values, which is the whole
    /// reason NULL is a variant.
    #[test]
    fn null_and_empty_text_are_different() {
        assert!(Value::Null.is_null());
        assert!(!Value::Text(String::new()).is_null());
        assert_eq!(Value::Null.text(), None);
        assert_eq!(Value::Text(String::new()).text(), Some(""));
    }

    /// Every variant survives a round trip through the engine's own value.
    #[test]
    fn every_variant_round_trips_through_the_engine() {
        let all = [
            Value::Null,
            Value::Integer(-9_223_372_036_854_775_808),
            Value::Real(0.1),
            Value::Text("héllo\u{0}world".to_owned()),
            Value::Blob(vec![0, 255, 0, 1]),
        ];
        for value in all {
            let back = Value::from_engine(&value.to_engine());
            assert_eq!(back, value, "{value:?} did not survive the round trip");
        }
    }

    /// Bytes that are not UTF-8 come back as a blob rather than as a string
    /// with the bad bytes replaced, because the replacement would be a value
    /// nobody stored.
    #[test]
    fn invalid_utf8_text_arrives_as_bytes_rather_than_as_a_lie() {
        let stored = EngineValue::Text(vec![0xff, 0xfe]);
        assert_eq!(Value::from_engine(&stored), Value::Blob(vec![0xff, 0xfe]));
    }

    /// A number does not answer `text()`, because rendering it is a decision
    /// this driver does not take on a caller's behalf.
    #[test]
    fn a_number_is_not_answered_as_text() {
        assert_eq!(Value::Integer(7).text(), None);
        assert_eq!(Value::Real(7.0).text(), None);
        assert_eq!(Value::Integer(7).kind(), ValueKind::Integer);
    }
}
