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
#[non_exhaustive]
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
    /// A rule of this driver's own contract was broken by the caller, or this
    /// machine is missing a component the call needs.
    ///
    /// The second reading is what `embed(TEXT)` answers on a machine that has
    /// never run `inillucent setup-embeddings`: the statement is valid, the
    /// engine built the function, and the weights are not there yet. The
    /// message names the command that installs them.
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
///
/// ```
/// # use inillucent_driver::{Database, Status, Value};
/// # let directory = std::env::temp_dir().join(format!("inillucent-doc-error-{}", std::process::id()));
/// # std::fs::create_dir_all(&directory).ok();
/// let database = Database::open(directory.join("app.rdb")).expect("it opens");
/// let connection = database.session();
///
/// // A statement about a table that is not there is a refusal a caller can
/// // read, and the connection is still usable afterwards.
/// let failure = connection
///     .query("SELECT * FROM nothing_of_the_sort", &[], 1)
///     .expect_err("a missing table is an error");
/// assert!(!failure.message.is_empty());
/// assert!(connection.query("SELECT 1", &[], 1).is_ok());
///
/// // `Unsupported` is a different answer from `Syntax`, which is the whole
/// // point of the driver: one says "this engine has not built that", the
/// // other says "you typed it wrong".
/// assert_ne!(Status::Unsupported, Status::Syntax);
/// # drop(connection);
/// # drop(database);
/// # std::fs::remove_dir_all(&directory).ok();
/// ```
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
    /// the two marked refusals are asked about first, because both are reported
    /// as a misuse and would otherwise be classified as a syntax error - which
    /// is the exact confusion this driver exists to end.
    ///
    /// **They are two markers and not one because they are two different
    /// answers.** `unsupported` means this engine never built the construct, so
    /// the caller writes different SQL; `requirement` means it did build it and
    /// this machine has not got what it needs, so the caller installs something
    /// and the same SQL then works. `SELECT length(embed('hello'))` on a
    /// machine that has never run `inillucent setup-embeddings` is the second,
    /// and reporting it as `unsupported` would print "not built yet" at a
    /// person whose fix is one command.
    ///
    /// @param error - the engine's error
    /// @param diagnostics - whether the caller asked for internal detail
    pub fn from_engine(error: &DbError, diagnostics: bool) -> Error {
        let status = match (error.unsupported(), error.requirement(), error.code()) {
            (Some(_), _, _) => Status::Unsupported,
            (None, Some(_), _) => Status::InvalidState,
            (None, None, PrimaryCode::Constraint) => Status::Constraint,
            (None, None, PrimaryCode::ReadOnly) => Status::ReadOnly,
            (None, None, PrimaryCode::Busy) | (None, None, PrimaryCode::Locked) => Status::Busy,
            (None, None, PrimaryCode::Interrupt) => Status::Interrupted,
            (None, None, PrimaryCode::Corrupt) | (None, None, PrimaryCode::NotADb) => {
                Status::Corrupt
            }
            (None, None, PrimaryCode::IoErr) | (None, None, PrimaryCode::CantOpen) => Status::Io,
            (None, None, PrimaryCode::Full) => Status::Full,
            (None, None, PrimaryCode::TooBig) => Status::TooBig,
            (None, None, PrimaryCode::Internal) => Status::Internal,
            // **A path a confined process may not reach** (task-2066 section
            // 4.2, item 27). `inillucent-vfs`'s `confine` refuses with
            // `PrimaryCode::Perm`, and with no arm here it fell through to
            // `Status::Syntax` - so an agent told a server started with
            // `--root` that its `ATTACH` was a syntax error, while
            // `agent-skills/inillucent-mcp/SKILL.md` tells it `invalid_state`
            // is what a refused path looks like. `InvalidState` rather than a
            // status of its own: the statement is fine and the state it asked
            // about is not this process's to reach, which is the same shape
            // every other `InvalidState` here has.
            (None, None, PrimaryCode::Perm) => Status::InvalidState,
            // A missing object and a malformed statement are both reported by
            // the binder as a misuse, and SQLite's wording for the first is
            // stable and deliberate: `inillucent-sql`'s `no_such_table` exists
            // to reproduce it exactly. This is the driver's one comparison
            // against a message, it is recorded in the TDD as such, and the
            // moment `NotFound` has to be exact the mechanism that carries
            // `unsupported` carries this too, in one line.
            (None, None, _) if error.message().starts_with("no such ") => Status::NotFound,
            (None, None, _) => Status::Syntax,
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

    /// **A path a confined process may not reach is `invalid_state`, and it
    /// used to be `syntax`.**
    ///
    /// `inillucent-vfs`'s `confine` refuses with `PrimaryCode::Perm` and there
    /// was no arm for it, so the classification fell through to the default -
    /// and an agent on a server started with `--root` was told its `ATTACH`
    /// was a syntax error, while `agent-skills/inillucent-mcp/SKILL.md` tells
    /// it `invalid_state` is what a refused path looks like (task-2066 section
    /// 4.2, item 27).
    #[test]
    fn a_path_outside_the_root_is_invalid_state() {
        let said = "\"C:/keys/id_rsa\" is outside the root this process is confined to";
        let engine = DbError::primary(PrimaryCode::Perm).with_message(said);
        let error = Error::from_engine(&engine, false);
        assert_eq!(error.status, Status::InvalidState);
        assert_eq!(error.message, said);
        assert_eq!(error.feature, None, "it is refused, it is not unbuilt");
    }

    /// A component this machine has not got is its own status, and it is not
    /// the one for a construct the engine never built.
    ///
    /// The two markers sit next to each other and the wrong one would print
    /// "not built yet" at a person whose whole fix is `inillucent
    /// setup-embeddings`.
    #[test]
    fn a_missing_component_is_invalid_state_and_keeps_its_own_sentence() {
        let said = "embed: no embedding model is installed. Run `inillucent setup-embeddings`";
        let engine = inillucent_engine::base::error::unmet_requirement("an embedding model", said);
        let error = Error::from_engine(&engine, false);
        assert_eq!(error.status, Status::InvalidState);
        assert_eq!(error.message, said);
        assert_eq!(error.feature, None, "it is built, it is not installed");
    }

    /// A construct the engine never built still wins when an error somehow
    /// carries both marks, because "write different SQL" is the stronger claim.
    #[test]
    fn an_unimplemented_construct_outranks_a_missing_component() {
        let engine = DbError::primary(PrimaryCode::Misuse)
            .with_message("the physical pass does not handle an outer join yet")
            .with_unsupported("an outer join")
            .with_requirement("an embedding model");
        assert_eq!(
            Error::from_engine(&engine, false).status,
            Status::Unsupported
        );
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
