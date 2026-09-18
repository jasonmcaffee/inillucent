//! The stable error model every inillucent layer reports through.
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
            None => PrimaryCode::from_code(self.0 & 0xff).unwrap_or(PrimaryCode::Error),
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

    /// Resolves a numeric primary code, returning `None` for a number the
    /// manifest does not list.
    ///
    /// Named for what it takes. It was `from_value` until task-1961, which is
    /// the name of the datum-to-SQL-value conversion everywhere else in this
    /// workspace, and this function has nothing to do with that one: its
    /// argument is an error number out of `compat/errors.toml`.
    pub fn from_code(number: i32) -> Option<PrimaryCode> {
        PRIMARY_ROWS
            .iter()
            .find(|row| row.value == number)
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

/// What a failing statement leaves behind.
///
/// **SQLite's five conflict algorithms differ in three ways, and only this one
/// belongs to the layer above the write path.** `IGNORE` and `REPLACE` resolve
/// the row and carry on, which is a decision the write path makes for itself;
/// the other three all report the failure and differ solely in how much of what
/// has already been written goes back. The write path cannot make *that*
/// decision - it owns neither the undo buffer nor the transaction - so it says
/// what it wants and the engine does it.
///
/// An error that was never tagged reads as [`Unwind::Statement`], which is
/// SQLite's default `ABORT`. That is deliberate, and it is why a `STRICT` type
/// failure, a foreign-key violation and a trigger's `RAISE` all undo the
/// statement without any of their raise sites having heard of this type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unwind {
    /// The statement's own writes go back and the transaction is kept -
    /// SQLite's `ABORT`, and what every unqualified statement gets.
    Statement,
    /// Nothing goes back: the rows written before the failure stay - SQLite's
    /// `FAIL`.
    Nothing,
    /// The statement's writes and the whole open transaction go back - SQLite's
    /// `ROLLBACK`.
    Transaction,
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
    /// The construct the engine has not implemented, when that is why it
    /// refused.
    ///
    /// **A fact the engine already knows, carried rather than re-derived.**
    /// `inillucent-exec`'s physical pass and `inillucent-sql`'s binder each
    /// have a single `unsupported` helper, and both used to be flattened into
    /// an ordinary `SQLITE_MISUSE` whose only distinguishing mark was the
    /// wording of its sentence. A caller that wanted to tell "this engine
    /// cannot do that yet" from "you typed it wrong" therefore had to match on
    /// prose, which works until somebody improves the prose. This field is that
    /// caller's answer, and it is set at the same two places the sentence is
    /// written so the two cannot disagree.
    ///
    /// It changes no code and no message: an error carrying it reports the same
    /// `SQLITE_MISUSE` and the same text it always did.
    unsupported: Option<String>,
    /// What this installation has not got, when that is why it refused.
    ///
    /// **The same mechanism as `unsupported`, for the other reason a call that
    /// is written correctly cannot be answered.** `unsupported` means the
    /// engine never built the construct, and nothing done on this machine will
    /// change that. This field means the engine built it and the machine is
    /// missing something the caller can go and install - the embedding model
    /// `embed(TEXT)` runs, and the ONNX Runtime under it.
    ///
    /// The two are kept apart because the answer a caller needs is different:
    /// one says stop asking, the other says run this command. Before this
    /// field, a refusal of the second kind left the engine as a bare
    /// `SQLITE_MISUSE` and reached `inillucent-driver` as the status `syntax`,
    /// so `SELECT length(embed('hello'))` on a machine that had never run
    /// `inillucent setup-embeddings` told a person their SQL was malformed.
    ///
    /// Like `unsupported` it is safe to show a caller: it names a component,
    /// never a path and never a bound value.
    requirement: Option<String>,
    /// How much of what has been written this failure undoes, when a conflict
    /// algorithm said.
    ///
    /// `None` reads as [`Unwind::Statement`]. It is set at the innermost site
    /// that knows - a `RAISE`, then a constraint carrying its own clause, then
    /// the statement's own `OR` - and only when it is not already set, so the
    /// precedence is the order those sites run in rather than a rule written
    /// down anywhere.
    unwind: Option<Unwind>,
    /// Whether the unwind was written out by a `RAISE`, which nothing overrides.
    ///
    /// **The one place SQLite's precedence is not innermost-first.** A trigger
    /// body's `RAISE(ROLLBACK)` beats the statement that fired it, and the
    /// statement that fired it beats a nested statement's own `OR` clause and
    /// any clause written on a constraint - so "set if absent" gets two of the
    /// three right and this flag gets the third.
    unwind_explicit: bool,
}

/// A inillucent error: a stable code plus the context a caller may safely see.
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
    ///
    /// **`unwind` is deliberately not one of them.** Equality here is over what
    /// the error *says*; the unwind is about what the engine does with it, and
    /// folding it in would make an error tagged at a raise site unequal to the
    /// same error written out in a test - a comparison that would start failing
    /// for a reason nothing in the message could show.
    fn eq(&self, other: &DbError) -> bool {
        self.extended == other.extended
            && self.message() == other.message()
            && self.sql_offset() == other.sql_offset()
            && self.database() == other.database()
            && self.detail() == other.detail()
            && self.unsupported() == other.unsupported()
            && self.requirement() == other.requirement()
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

    /// Records that this refusal is a construct the engine has not implemented.
    ///
    /// Unlike a message, this is safe to show a caller and is meant to be: it
    /// names a SQL construct and never a path or a bound value.
    ///
    /// @param what - the construct, in the words the refusal already uses
    pub fn with_unsupported(mut self, what: impl Into<String>) -> DbError {
        self.context_mut().unsupported = Some(what.into());
        self
    }

    /// Records that this refusal is about something this installation has not
    /// got, which the caller can install.
    ///
    /// Safe to show a caller, and meant to be: it names a component, such as
    /// "an embedding model", and never a path or a bound value.
    ///
    /// @param what - the missing component, in the words the refusal already uses
    pub fn with_requirement(mut self, what: impl Into<String>) -> DbError {
        self.context_mut().requirement = Some(what.into());
        self
    }

    /// Records how much of what has been written this failure undoes, unless
    /// something closer to the failure has already said.
    ///
    /// **Set if absent, never overwritten**, because the sites that know run
    /// innermost first and SQLite's precedence is exactly that order: an
    /// explicit `RAISE(ROLLBACK)` beats the enclosing statement's `OR FAIL`,
    /// and a statement's `OR` beats the clause written on the constraint. A
    /// setter that overwrote would invert it, and the inversion is invisible -
    /// the error text is the same either way and only the rows differ.
    ///
    /// @param unwind - what this failure undoes
    pub fn or_unwind(mut self, unwind: Unwind) -> DbError {
        let context = self.context_mut();
        if context.unwind.is_none() {
            context.unwind = Some(unwind);
        }
        self
    }

    /// Records an unwind a `RAISE` wrote out, which nothing overrides.
    ///
    /// `RAISE(ROLLBACK, ...)` in a trigger body rolls the transaction back
    /// whatever the statement that fired the trigger asked for, which is the
    /// one direction [`DbError::or_unwind`]'s innermost-first rule gets wrong.
    ///
    /// @param unwind - what the `RAISE` undoes
    pub fn with_raised_unwind(mut self, unwind: Unwind) -> DbError {
        let context = self.context_mut();
        context.unwind = Some(unwind);
        context.unwind_explicit = true;
        self
    }

    /// Records the unwind of the statement a trigger's statements are nested
    /// in, which beats theirs and beats a constraint's own clause.
    ///
    /// SQLite's rule: "if an `ON CONFLICT` clause is specified as part of the
    /// statement causing the trigger to fire, then conflict handling policy of
    /// the outer statement is used instead". So this is the one setter that
    /// overwrites - and it still yields to a `RAISE`.
    ///
    /// @param unwind - what the outermost statement's `OR` clause undoes
    pub fn with_outer_unwind(mut self, unwind: Unwind) -> DbError {
        let context = self.context_mut();
        if !context.unwind_explicit {
            context.unwind = Some(unwind);
        }
        self
    }

    /// Returns how much of what has been written this failure undoes.
    ///
    /// An untagged error undoes the statement, which is SQLite's `ABORT`.
    pub fn unwind(&self) -> Unwind {
        self.context
            .as_ref()
            .and_then(|context| context.unwind)
            .unwrap_or(Unwind::Statement)
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

    /// Returns the construct the engine has not implemented, when that is why
    /// it refused.
    ///
    /// `None` for every other failure, including a statement that is simply
    /// wrong - which is the distinction it exists to make.
    pub fn unsupported(&self) -> Option<&str> {
        self.context
            .as_ref()
            .and_then(|context| context.unsupported.as_deref())
    }

    /// Returns what this installation is missing, when that is why it refused.
    ///
    /// `None` for every other failure, including a construct the engine has
    /// never built - that is [`DbError::unsupported`], and it is a different
    /// answer to give a caller.
    pub fn requirement(&self) -> Option<&str> {
        self.context
            .as_ref()
            .and_then(|context| context.requirement.as_deref())
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
///
/// **What it is given becomes the internal detail and not the message**, so an
/// error built this way answers `message()` with its primary code's own text:
/// "bad parameter or other API misuse". That is right for the thing the name
/// says - a contract the *caller* broke, whose explanation may name a file or a
/// value - and wrong for a refusal about a statement, which is what most of the
/// engine uses it for. [`refusal`] is the one to use for those.
pub fn misuse(detail: impl Into<String>) -> DbError {
    DbError::primary(PrimaryCode::Misuse).with_detail(detail)
}

/// Builds a `SQLITE_MISUSE` error whose sentence is about the caller's own
/// statement, so it is the message **and** the detail.
///
/// The distinction from [`misuse`] is which field the sentence lands in, and it
/// was a real defect for as long as only one of them existed. `no such table:
/// peple`, `table t already exists` and `UNIQUE constraint failed: t.a` are all
/// things a person needs to read, and they were all going into the field this
/// module documents as staying inside the process - so a caller reading
/// `message()`, which is the field it is *told* to read, got "bad parameter or
/// other API misuse" for every one of them. `inillucent-cli::shell::reason` and
/// `inillucent-compat`'s `readgate::why` had each independently worked around
/// it with `detail().unwrap_or(message())`.
///
/// A refusal built this way must stay free of paths, bound values and page
/// bytes, exactly as [`DbError::with_message`] requires - which a sentence about
/// a statement's own tables, columns and constructs is. Anything naming a file
/// belongs in [`misuse`].
///
/// The detail is set as well as the message so that every existing reader of
/// `detail()` sees exactly what it saw before.
///
/// @param said - the sentence, safe for a caller to read
pub fn refusal(said: impl Into<String>) -> DbError {
    let said = said.into();
    DbError::primary(PrimaryCode::Misuse)
        .with_message(said.clone())
        .with_detail(said)
}

/// Builds a `SQLITE_ERROR` refusal about the caller's own statement - the
/// same shape as [`refusal`], but for the far more common case where SQLite's
/// real answer is code 1 rather than `SQLITE_MISUSE`'s 21.
///
/// **Most engine-level statement refusals are `SQLITE_ERROR`, and `refusal`
/// answers `SQLITE_MISUSE` unconditionally.** `bind.rs`'s own `refused`
/// function found and fixed this for the parser and binder - `PrimaryCode::
/// from `ParseError::code`, not from `refusal`'s hardcoded `Misuse` - because
/// a parse or bind refusal is `SQLITE_ERROR` in SQLite, measured through
/// `dml_differential.rs`. Refusals raised directly by the engine's execution
/// code (`CREATE VIRTUAL TABLE` naming no such module, `VACUUM` from inside a
/// transaction) go through `refusal` directly rather than through a
/// `ParseError`, so they did not get that fix and still answer 21 where the
/// pinned reference answers 1. This is the same fix, for that path: use it in
/// place of `refusal` once the reference has been checked and answers 1 - do
/// not switch a call over on the strength of this doc comment alone.
///
/// @param said - the sentence, safe for a caller to read
pub fn statement_refusal(said: impl Into<String>) -> DbError {
    let said = said.into();
    DbError::primary(PrimaryCode::Error)
        .with_message(said.clone())
        .with_detail(said)
}

/// Builds a `SQLITE_BUSY` refusal about another process holding the file.
///
/// **The sentence is the message as well as the detail, because the caller can
/// act on it.** What a contended open used to answer was the VFS's own
/// "a writer holds PENDING", which names an internal lock level, is wrong
/// whenever the holder is a reader, and says nothing about how long the wait
/// was or what would change the outcome (task-1979, C6). A sentence built here
/// names who holds the file, what they hold it for, and the pragma that governs
/// the wait.
///
/// It must stay free of paths and bound values, exactly as
/// [`DbError::with_message`] requires; the path is added by the caller that
/// knows it, in the detail.
///
/// @param said - the sentence, safe for a caller to read
pub fn busy(said: impl Into<String>) -> DbError {
    let said = said.into();
    DbError::primary(PrimaryCode::Busy)
        .with_message(said.clone())
        .with_detail(said)
}

/// Builds a refusal about a component this installation has not got, which the
/// caller can install.
///
/// **The third member of the family, and the one `embed(TEXT)` needed.**
/// [`misuse`] hides its sentence in the detail, [`refusal`] shows it, and both
/// reach a driver as the status `syntax` - which is the right reading of a
/// statement that is wrong and the wrong reading of a statement that is fine on
/// a machine that is not ready. This one shows the sentence *and* marks why,
/// so `inillucent-driver` answers `invalid_state` instead.
///
/// `what` is the component, in the words the sentence already uses, and `said`
/// is the sentence. Both are shown to the caller, so both must stay free of
/// paths and bound values exactly as [`DbError::with_message`] requires. A
/// sentence that has to name the directory it looked in puts that in
/// `with_detail`.
///
/// @param what - the missing component, such as "an embedding model"
/// @param said - the sentence, naming what installs it
pub fn unmet_requirement(what: impl Into<String>, said: impl Into<String>) -> DbError {
    let said = said.into();
    DbError::primary(PrimaryCode::Misuse)
        .with_message(said.clone())
        .with_detail(said)
        .with_requirement(what)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every generated row must round-trip through its numeric value, which is
    /// what the C surface and the oracle protocol both depend on.
    #[test]
    fn primary_codes_round_trip_through_their_numeric_value() {
        for row in PRIMARY_ROWS.iter() {
            assert_eq!(PrimaryCode::from_code(row.value), Some(row.code));
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

    /// The three `SQLITE_MISUSE` builders differ in exactly one thing - whether
    /// the sentence they are given is the one a caller reads - and that is the
    /// difference `embed(TEXT)` was on the wrong side of.
    ///
    /// `misuse` is the one that hides it, and the test says so rather than
    /// leaving a reader to infer it from the doc comment: a caller reading
    /// `message()` gets the primary code's manifest text and has to open the
    /// diagnostic detail to find out anything at all.
    #[test]
    fn only_two_of_the_three_misuse_builders_show_their_sentence() {
        let said = "embed: no embedding model is installed";
        assert_eq!(misuse(said).message(), PrimaryCode::Misuse.message());
        assert_eq!(misuse(said).detail(), Some(said));
        assert_eq!(refusal(said).message(), said);
        assert_eq!(
            unmet_requirement("an embedding model", said).message(),
            said
        );
    }

    /// A refusal about a missing component says which component, and says it
    /// through a field rather than through its wording.
    ///
    /// The marker is what `inillucent-driver` reads to answer `invalid_state`
    /// instead of `syntax`. Deriving it from the sentence instead would work
    /// until somebody improved the sentence, which is the argument the
    /// `unsupported` marker beside it was added on.
    #[test]
    fn a_missing_component_is_named_by_a_marker_and_is_not_an_unimplemented_one() {
        let error = unmet_requirement(
            "an embedding model",
            "embed: no embedding model is installed. Run `inillucent setup-embeddings`",
        );
        assert_eq!(error.requirement(), Some("an embedding model"));
        assert_eq!(
            error.unsupported(),
            None,
            "it is built, it is not installed"
        );
        assert_eq!(error.code(), PrimaryCode::Misuse);
        assert!(error.to_string().contains("setup-embeddings"), "{error}");
        assert_eq!(
            refusal("no such table: peple").requirement(),
            None,
            "an ordinary statement refusal is missing nothing"
        );
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
