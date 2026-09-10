//! What went wrong, in a form an application can act on.
//!
//! Invariant: **"this engine cannot do that yet" is a status of its own, and it
//! is read off the error rather than guessed from its wording.**
//!
//! The engine is deliberately incomplete - it cannot enforce a foreign key,
//! answer an outer join, or run a recursive CTE - and it refuses those rather
//! than answering them wrongly, which is correct. But a refusal is only useful
//! to an application that can recognise one, and before this driver existed
//! every refusal left the engine as the same `SQLITE_MISUSE` a typo produces. `SELECT * FROM
//! peple` and `SELECT * FROM people LEFT JOIN teams ON ...` differed only in
//! prose, so an application that wanted to say "this engine cannot do that yet"
//! rather than "check your spelling" had to match on a sentence.
//!
//! `DbError::unsupported` now carries the fact, set at the two places the
//! sentence is written - `inillucent-exec`'s physical pass and
//! `inillucent-sql`'s binder - so the classification here reads it instead of
//! re-deriving it. That is the difference between a driver that knows and a
//! driver that is usually right.

use inillucent_engine::{DbError, PrimaryCode};

/// What kind of failure this is.
///
/// The list is ADBC 1.1.0's status codes with the members this engine cannot
/// produce removed. ADBC was worth copying because it makes
/// `ADBC_STATUS_NOT_IMPLEMENTED` - *"the operation is not implemented or
/// supported"* - a first-class member sitting beside `INVALID_ARGUMENT` rather
/// than folding it in, which is exactly the distinction this engine needs.
///
/// The three ADBC has that are absent are `UNAUTHENTICATED`, `UNAUTHORIZED` and
/// `TIMEOUT`: this engine has no authentication, no authorization and no
/// statement timeout, and a status nothing can return is a branch no input can
/// take.
///
/// The numbers are frozen. A binding compiled against them keeps working when a
/// status is added, because an addition takes the next number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Status {
    /// The engine has not implemented the construct. [`Error::feature`] names
    /// it.
    Unsupported = 1,
    /// The statement is not valid SQL.
    Syntax = 2,
    /// No such table, column or index.
    NotFound = 3,
    /// A constraint refused the write.
    Constraint = 4,
    /// The database, or the connection, is read only.
    ReadOnly = 5,
    /// Another writer holds the database.
    Busy = 6,
    /// The statement was interrupted.
    Interrupted = 7,
    /// The file is not a database, or is damaged.
    Corrupt = 8,
    /// The file system refused a read or a write.
    Io = 9,
    /// The database or the disk is full.
    Full = 10,
    /// A value or a result is past a hard limit.
    TooBig = 11,
    /// A rule of this driver's own contract was broken by the caller.
    InvalidState = 12,
    /// A defect. The only status that means "report this".
    Internal = 13,
}

impl Status {
    /// Returns the name a binding prints, which is also the name the
    /// conformance suite writes.
    pub fn name(self) -> &'static str {
        match self {
            Status::Unsupported => "unsupported",
            Status::Syntax => "syntax",
            Status::NotFound => "not_found",
            Status::Constraint => "constraint",
            Status::ReadOnly => "readonly",
            Status::Busy => "busy",
            Status::Interrupted => "interrupted",
            Status::Corrupt => "corrupt",
            Status::Io => "io",
            Status::Full => "full",
            Status::TooBig => "too_big",
            Status::InvalidState => "invalid_state",
            Status::Internal => "internal",
        }
    }
}

/// One failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What kind of failure it is.
    pub status: Status,
    /// The engine's own words, safe to show a person.
    ///
    /// `inillucent-base` guarantees this never holds a file-system path, a
    /// bound value or page bytes, and has a test named
    /// `display_hides_internal_detail` that keeps it so.
    pub message: String,
    /// Diagnostic text, present only when the database was opened with
    /// diagnostics on.
    ///
    /// **It may hold a path or a value**, which is why it is off by default and
    /// why it is a separate field rather than being appended to the message. A
    /// caller that logs errors to a shared sink should log [`Error::message`].
    pub detail: Option<String>,
    /// The construct the engine has not implemented.
    ///
    /// `Some` if and only if the status is [`Status::Unsupported`], and it
    /// carries the engine's own words for what it will not run - "an outer
    /// join", "a recursive CTE". It is finer-grained than the capability table
    /// on purpose: "a rowid range as an inner join term" is a real refusal and
    /// is not a row anybody would put in a table.
    pub feature: Option<String>,
    /// The byte offset into the statement, when the engine knows one.
    pub offset: Option<u32>,
    /// The engine's extended result code, for diagnosis.
    ///
    /// Reported rather than interpreted. A caller that needs a decision uses
    /// [`Error::status`]; this is here so a bug report can name the code.
    pub engine_code: i32,
}

impl Error {
    /// Returns a failure this driver raised on its own account.
    ///
    /// @param status - what kind it is
    /// @param message - what to tell the caller
    pub fn said(status: Status, message: impl Into<String>) -> Error {
        Error {
            status,
            message: message.into(),
            detail: None,
            feature: None,
            offset: None,
            engine_code: 0,
        }
    }

    /// Classifies an engine error.
    ///
    /// The order of the arms is the whole of the classification and it matters:
    /// the marked refusal is asked about first, because a construct the engine
    /// has not implemented is reported as a misuse and would otherwise be
    /// classified as a syntax error - which is the exact confusion this driver
    /// exists to end.
    ///
    /// @param error - the engine's error
    /// @param diagnostics - whether the caller asked for internal detail
    pub fn from_engine(error: &DbError, diagnostics: bool) -> Error {
        let status = match (error.unsupported(), error.code()) {
            (Some(_), _) => Status::Unsupported,
            (None, PrimaryCode::Constraint) => Status::Constraint,
            (None, PrimaryCode::ReadOnly) => Status::ReadOnly,
            (None, PrimaryCode::Busy) | (None, PrimaryCode::Locked) => Status::Busy,
            (None, PrimaryCode::Interrupt) => Status::Interrupted,
            (None, PrimaryCode::Corrupt) | (None, PrimaryCode::NotADb) => Status::Corrupt,
            (None, PrimaryCode::IoErr) | (None, PrimaryCode::CantOpen) => Status::Io,
            (None, PrimaryCode::Full) => Status::Full,
            (None, PrimaryCode::TooBig) => Status::TooBig,
            (None, PrimaryCode::Internal) => Status::Internal,
            // A missing object and a malformed statement are both reported by
            // the binder as a misuse, and SQLite's wording for the first is
            // stable and deliberate: `inillucent-sql`'s `no_such_table` exists
            // to reproduce it exactly. This is the driver's one comparison
            // against a message, it is recorded in the TDD as such, and the
            // moment `NotFound` has to be exact the mechanism that carries
            // `unsupported` carries this too, in one line.
            (None, _) if error.message().starts_with("no such ") => Status::NotFound,
            (None, _) => Status::Syntax,
        };
        Error {
            status,
            message: error.message().to_owned(),
            detail: match diagnostics {
                true => error.detail().map(str::to_owned),
                false => None,
            },
            feature: error.unsupported().map(str::to_owned),
            offset: error.sql_offset(),
            engine_code: error.extended().value(),
        }
    }
}

impl std::fmt::Display for Error {
    /// Writes what the engine said, then what kind of failure it is, then where.
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{} [{}]", self.message, self.status.name())?;
        if let Some(offset) = self.offset {
            write!(out, " at byte {offset}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

/// Everything in this crate answers with this.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_engine::DbError;

    /// A construct the engine has not implemented is classified from the marker
    /// the engine set, not from its wording.
    #[test]
    fn an_unimplemented_construct_is_its_own_status_and_names_itself() {
        let engine = inillucent_engine::DbError::primary(PrimaryCode::Misuse)
            .with_message("the new engine's physical pass does not handle an outer join yet")
            .with_unsupported("an outer join");
        let error = Error::from_engine(&engine, false);
        assert_eq!(error.status, Status::Unsupported);
        assert_eq!(error.feature.as_deref(), Some("an outer join"));
    }

    /// The same primary code with no marker is a statement that is simply
    /// wrong, which is the distinction the whole classification exists for.
    #[test]
    fn an_ordinary_misuse_is_a_syntax_error_and_carries_no_feature() {
        let engine = DbError::primary(PrimaryCode::Misuse).with_message("unexpected token `form`");
        let error = Error::from_engine(&engine, false);
        assert_eq!(error.status, Status::Syntax);
        assert_eq!(error.feature, None);
    }

    /// A missing object is neither of those two.
    #[test]
    fn a_missing_table_is_not_found() {
        let engine = DbError::primary(PrimaryCode::Misuse).with_message("no such table: peple");
        assert_eq!(
            Error::from_engine(&engine, false).status,
            Status::NotFound,
            "a typo in a table name is not a syntax error"
        );
    }

    /// The internal detail does not cross the boundary unless it was asked for,
    /// because `inillucent-base` promises it holds paths and bound values.
    #[test]
    fn the_diagnostic_detail_is_withheld_by_default() {
        let engine = DbError::primary(PrimaryCode::CantOpen).with_detail("C:/secret/path/app.db");
        assert_eq!(Error::from_engine(&engine, false).detail, None);
        assert_eq!(
            Error::from_engine(&engine, true).detail.as_deref(),
            Some("C:/secret/path/app.db")
        );
        assert!(!Error::from_engine(&engine, false)
            .message
            .contains("secret"));
    }

    /// Every status prints a name, and no two share one - the conformance suite
    /// writes these strings, so a duplicate would make two outcomes
    /// indistinguishable in the file.
    #[test]
    fn every_status_name_is_distinct() {
        let all = [
            Status::Unsupported,
            Status::Syntax,
            Status::NotFound,
            Status::Constraint,
            Status::ReadOnly,
            Status::Busy,
            Status::Interrupted,
            Status::Corrupt,
            Status::Io,
            Status::Full,
            Status::TooBig,
            Status::InvalidState,
            Status::Internal,
        ];
        let mut names: Vec<&str> = all.iter().map(|status| status.name()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two statuses share a name");
    }
}
