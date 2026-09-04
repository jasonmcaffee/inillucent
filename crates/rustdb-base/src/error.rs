//! The stable error model every rust-db layer reports through.
//!
//! Invariant: an error's numeric code, its effect on the connection, and its
//! effect on the transaction are decided by `compat/errors.toml` and nothing
//! else. Code that wants a new failure mode adds a manifest row; it cannot
//! invent a code at the call site.
//!
//! A `DbError` carries two messages. `message` is safe to hand to a caller and
//! never contains a file-system path, a bound value, or page bytes. `detail` is
//! for diagnostics that stay inside the process unless an explicit diagnostic
//! callback asks for them.

use core::fmt;

mod generated {
    //! The table generated from `compat/errors.toml` at build time.
    #![allow(missing_docs)]
    use super::ExtendedCode;
    include!(concat!(env!("OUT_DIR"), "/errors_generated.rs"));
}

pub use generated::{
    ExtendedRow, PrimaryCode, PrimaryRow, ERROR_TABLE_REFERENCE, ERROR_TABLE_SOURCE, EXTENDED_ROWS,
    PRIMARY_ROWS,
};

/// The result type used everywhere below the public facade.
pub type DbResult<T> = Result<T, DbError>;

/// The row returned for a primary code the generated table does not list.
///
/// Every `PrimaryCode` variant is generated from the manifest, so this is
/// unreachable in practice. It exists so that `PrimaryCode::row` is total
/// without an `unwrap` on a hot path.
static UNRECOGNISED_PRIMARY_ROW: PrimaryRow = PrimaryRow {
    code: PrimaryCode::Error,
    value: 1,
    c_name: "SQLITE_ERROR",
    message: "SQL logic error",
    connection_usable: true,
    statement_resettable: true,
    transaction_rolled_back: false,
};

/// A SQLite extended result code.
///
/// This is a newtype over the numeric value rather than an enum because the
/// extended space is open: an unrecognised code from an extension or a future
/// SQLite release still has to round-trip through the C surface unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ExtendedCode(pub i32);

impl ExtendedCode {
    /// Returns the extended code for a primary code with no refinement, which
    /// is the primary code's own numeric value.
    pub fn from_primary(code: PrimaryCode) -> ExtendedCode {
        ExtendedCode(code.value())
    }

    /// Returns the numeric value a C caller sees.
    pub fn value(self) -> i32 {
        self.0
    }

    /// Returns the generated row for this code, if the manifest knows it.
    pub fn row(self) -> Option<&'static ExtendedRow> {
        EXTENDED_ROWS.iter().find(|row| row.value == self.0)
    }

    /// Returns the primary code this extended code refines.
    ///
    /// SQLite derives the primary code from the low eight bits, so an unknown
    /// extended code still resolves to a usable primary code rather than an
    /// error of its own.
    pub fn primary(self) -> PrimaryCode {
        match self.row() {
            Some(row) => row.primary,
            None => PrimaryCode::from_value(self.0 & 0xff).unwrap_or(PrimaryCode::Error),
        }
    }

    /// Returns the C macro name when the manifest knows this code.
    pub fn c_name(self) -> Option<&'static str> {
        self.row().map(|row| row.c_name)
    }

    /// Returns the default English message for this code.
    pub fn message(self) -> &'static str {
        match self.row() {
            Some(row) => row.message,
            None => self.primary().message(),
        }
    }
}

impl PrimaryCode {
    /// Returns the numeric value a C caller sees.
    pub fn value(self) -> i32 {
        self.row().value
    }

    /// Returns the generated row for this code.
    ///
    /// Every variant comes from the manifest, so the lookup always succeeds;
    /// the fallback keeps the function total without an `unwrap`.
    pub fn row(self) -> &'static PrimaryRow {
        match PRIMARY_ROWS.iter().find(|row| row.code == self) {
            Some(row) => row,
            None => &UNRECOGNISED_PRIMARY_ROW,
        }
    }

    /// Resolves a numeric primary code, returning `None` for a value the
    /// manifest does not list.
    pub fn from_value(value: i32) -> Option<PrimaryCode> {
        PRIMARY_ROWS
            .iter()
            .find(|row| row.value == value)
            .map(|row| row.code)
    }

    /// Returns the C macro name.
    pub fn c_name(self) -> &'static str {
        self.row().c_name
    }

    /// Returns the default English message for this code.
    pub fn message(self) -> &'static str {
        self.row().message
    }

    /// Reports whether a result code means the operation succeeded.
    pub fn is_success(self) -> bool {
        matches!(self, PrimaryCode::Ok | PrimaryCode::Row | PrimaryCode::Done)
    }
}

/// The name of an attached database, used to attribute an error to one file.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DatabaseName(String);

impl DatabaseName {
    /// Wraps a schema name such as `main`, `temp`, or an attached alias.
    pub fn new(name: impl Into<String>) -> DatabaseName {
        DatabaseName(name.into())
    }

    /// Returns the schema name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DatabaseName {
    /// Writes the schema name.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Everything an error carries beyond its code.
///
/// Held behind a pointer because almost every error has none of it: the code's
/// own manifest message is the message, and there is no offset, database or
/// detail. Keeping the four fields inline made `DbError` 88 bytes, and a
/// `DbResult` is returned once per bytecode instruction - so every instruction
/// in the machine, and every `?` on every path in the engine, was moving 88
/// bytes to describe a failure that had not happened. Boxing it makes the error
/// half of a `Result` one pointer, and costs an allocation only on the paths
/// that are already failing or already building a message.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
struct ErrorContext {
    /// A message that replaces the code's own, when one was attached.
    message: Option<String>,
    /// The byte offset in the SQL text, when the error has one.
    sql_offset: Option<u32>,
    /// The schema the error was attributed to, when it was attributed.
    database: Option<DatabaseName>,
    /// Diagnostic text that never leaves the process.
    detail: Option<String>,
}

/// A rust-db error: a stable code plus the context a caller may safely see.
///
/// Equality is over what the error *says*, not over how it is stored: an error
/// carrying no context and one carrying a message equal to its code's own
/// message are the same error, and were the same error before the context was
/// boxed. `PartialEq` is written out below for that reason rather than derived.
#[derive(Clone, Debug)]
pub struct DbError {
    extended: ExtendedCode,
    context: Option<Box<ErrorContext>>,
}

impl PartialEq for DbError {
    /// Compares the code and every effective field, so the boxing is invisible.
    fn eq(&self, other: &DbError) -> bool {
        self.extended == other.extended
            && self.message() == other.message()
            && self.sql_offset() == other.sql_offset()
            && self.database() == other.database()
            && self.detail() == other.detail()
    }
}

impl Eq for DbError {}

impl DbError {
    /// Returns the context, creating an empty one to write into.
    fn context_mut(&mut self) -> &mut ErrorContext {
        self.context
            .get_or_insert_with(Box::<ErrorContext>::default)
    }

    /// Builds an error from an extended code, taking the manifest's message.
    pub fn new(extended: ExtendedCode) -> DbError {
        DbError {
            extended,
            context: None,
        }
    }

    /// Builds an error from a primary code with no extended refinement.
    pub fn primary(code: PrimaryCode) -> DbError {
        DbError::new(ExtendedCode::from_primary(code))
    }

    /// Replaces the caller-visible message.
    ///
    /// The replacement must stay free of paths, bound values, and page bytes;
    /// anything sensitive belongs in `with_detail` instead.
    pub fn with_message(mut self, message: impl Into<String>) -> DbError {
        self.context_mut().message = Some(message.into());
        self
    }

    /// Attaches diagnostic text that stays inside the process.
    pub fn with_detail(mut self, detail: impl Into<String>) -> DbError {
        self.context_mut().detail = Some(detail.into());
        self
    }

    /// Attaches the byte offset in the SQL text that produced the error.
    pub fn with_sql_offset(mut self, offset: u32) -> DbError {
        self.context_mut().sql_offset = Some(offset);
        self
    }

    /// Attaches the schema name of the database the error came from.
    pub fn with_database(mut self, database: DatabaseName) -> DbError {
        self.context_mut().database = Some(database);
        self
    }

    /// Returns the primary result code.
    pub fn code(&self) -> PrimaryCode {
        self.extended.primary()
    }

    /// Returns the extended result code.
    pub fn extended(&self) -> ExtendedCode {
        self.extended
    }

    /// Returns the caller-visible message.
    ///
    /// An error that was never given one answers with its code's own message,
    /// which is what it was constructed with before the context was boxed.
    pub fn message(&self) -> &str {
        match self
            .context
            .as_ref()
            .and_then(|context| context.message.as_deref())
        {
            Some(message) => message,
            None => self.extended.message(),
        }
    }

    /// Returns the internal diagnostic text, if any was attached.
    pub fn detail(&self) -> Option<&str> {
        self.context
            .as_ref()
            .and_then(|context| context.detail.as_deref())
    }

    /// Returns the SQL byte offset, if the error has one.
    pub fn sql_offset(&self) -> Option<u32> {
        self.context.as_ref().and_then(|context| context.sql_offset)
    }

    /// Returns the schema name, if the error was attributed to one.
    pub fn database(&self) -> Option<&DatabaseName> {
        self.context
            .as_ref()
            .and_then(|context| context.database.as_ref())
    }

    /// Reports whether the connection may still be used after this error.
    pub fn connection_usable(&self) -> bool {
        match self.extended.row() {
            Some(row) => row.connection_usable,
            None => self.code().row().connection_usable,
        }
    }

    /// Reports whether the statement may be reset and stepped again.
    pub fn statement_resettable(&self) -> bool {
        match self.extended.row() {
            Some(row) => row.statement_resettable,
            None => self.code().row().statement_resettable,
        }
    }

    /// Reports whether the error implicitly rolled the transaction back.
    pub fn transaction_rolled_back(&self) -> bool {
        match self.extended.row() {
            Some(row) => row.transaction_rolled_back,
            None => self.code().row().transaction_rolled_back,
        }
    }
}

impl fmt::Display for DbError {
    /// Writes the safe message, and the SQL offset when one is known. The
    /// internal detail is deliberately absent so that logging an error cannot
    /// leak a path or a bound value.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.sql_offset() {
            Some(offset) => write!(formatter, "{} (at SQL byte {offset})", self.message()),
            None => formatter.write_str(self.message()),
        }
    }
}

impl std::error::Error for DbError {}

/// Builds a `SQLITE_CORRUPT` error for malformed persistent bytes.
pub fn corrupt(detail: impl Into<String>) -> DbError {
    DbError::primary(PrimaryCode::Corrupt).with_detail(detail)
}

/// Builds a `SQLITE_TOOBIG` error for a value or arithmetic result that does
/// not fit the format's range.
pub fn too_big(detail: impl Into<String>) -> DbError {
    DbError::primary(PrimaryCode::TooBig).with_detail(detail)
}

/// Builds a `SQLITE_NOMEM` error for a fallible allocation that failed.
pub fn no_mem(detail: impl Into<String>) -> DbError {
    DbError::primary(PrimaryCode::NoMem).with_detail(detail)
}

/// Builds a `SQLITE_MISUSE` error for an API contract the caller broke.
pub fn misuse(detail: impl Into<String>) -> DbError {
    DbError::primary(PrimaryCode::Misuse).with_detail(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every generated row must round-trip through its numeric value, which is
    /// what the C surface and the oracle protocol both depend on.
    #[test]
    fn primary_codes_round_trip_through_their_numeric_value() {
        for row in PRIMARY_ROWS.iter() {
            assert_eq!(PrimaryCode::from_value(row.value), Some(row.code));
            assert_eq!(row.code.value(), row.value);
            assert_eq!(row.code.c_name(), row.c_name);
        }
    }

    /// An extended code resolves to the primary code the manifest names, and
    /// its low byte agrees with that primary code, which is the rule SQLite
    /// documents for callers that mask the value themselves.
    #[test]
    fn extended_codes_agree_with_their_primary_code() {
        for row in EXTENDED_ROWS.iter() {
            let extended = ExtendedCode(row.value);
            assert_eq!(extended.primary(), row.primary);
            assert_eq!(row.value & 0xff, row.primary.value());
        }
    }

    /// An extended code the manifest has never seen still resolves to a usable
    /// primary code instead of failing, because extensions may invent codes.
    #[test]
    fn unknown_extended_codes_fall_back_to_their_low_byte() {
        let invented = ExtendedCode((99 << 8) | PrimaryCode::Constraint.value());
        assert!(invented.row().is_none());
        assert_eq!(invented.primary(), PrimaryCode::Constraint);
        assert_eq!(invented.message(), PrimaryCode::Constraint.message());
    }

    /// The manifest must not contain two rows with the same numeric value; a
    /// duplicate would make the C surface ambiguous.
    #[test]
    fn numeric_values_are_unique() {
        let mut values: Vec<i32> = PRIMARY_ROWS.iter().map(|row| row.value).collect();
        values.extend(EXTENDED_ROWS.iter().map(|row| row.value));
        let count = values.len();
        values.sort_unstable();
        values.dedup();
        assert_eq!(
            values.len(),
            count,
            "duplicate result code in compat/errors.toml"
        );
    }

    /// Display must never carry the internal detail, because callers log it.
    #[test]
    fn display_hides_internal_detail() {
        let error = DbError::primary(PrimaryCode::CantOpen)
            .with_detail("C:/secret/path/app.db")
            .with_sql_offset(12);
        let rendered = error.to_string();
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("at SQL byte 12"), "{rendered}");
        assert_eq!(error.detail(), Some("C:/secret/path/app.db"));
    }

    /// The recovery contract is what callers branch on, so spot-check the rows
    /// where it differs from the default.
    #[test]
    fn recovery_contract_matches_the_manifest() {
        assert!(!DbError::primary(PrimaryCode::Misuse).connection_usable());
        assert!(DbError::primary(PrimaryCode::Busy).connection_usable());
        assert!(DbError::new(ExtendedCode::ABORT_ROLLBACK).transaction_rolled_back());
        assert!(!DbError::new(ExtendedCode::CONSTRAINT_UNIQUE).transaction_rolled_back());
    }
}
