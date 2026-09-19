//! What a `CREATE VIRTUAL TABLE ... USING fts5(...)` said, and what it means.
//!
//! Invariant: **an option is honoured or refused, never recorded and ignored.**
//! Accepting `detail='none'` and then answering a phrase query from positions
//! the option says are not stored is the one outcome that is worse than saying
//! no, because the caller has no way to find out (task-1979, R5 and R15).
//!
//! It is its own module because `mod.rs` had grown past the size
//! `crates/inillucent-compat/tests/policy.rs` records for it, and the argument
//! grammar is the part of it that has nothing to do with the table.

use inillucent_base::DbResult;

use super::tokenize;
use crate::vtab::failure;

/// One column of an FTS5 table, and what was written about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ColumnSpec {
    /// The column name.
    pub(super) name: Vec<u8>,
    /// Whether `UNINDEXED` was written, so the column is stored and not indexed.
    pub(super) unindexed: bool,
}

/// What a `CREATE VIRTUAL TABLE ... USING fts5(...)` said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Options {
    /// The indexed and stored columns, in order.
    pub(super) columns: Vec<ColumnSpec>,
    /// The tokenizer's name and its arguments.
    pub(super) tokenizer: Vec<Vec<u8>>,
    /// The table the rows live in, when they are not this table's own.
    ///
    /// `content='c'` makes an **external content** table: the index is built
    /// over rows that belong to `c`, and `%_content` is not created at all. It
    /// is what an application uses when the documents already exist and a
    /// second copy of them would double the file.
    pub(super) content: Option<Vec<u8>>,
    /// The option this build cannot honour, when the table declares one.
    ///
    /// **Recorded rather than ignored (task-1979, R15).** `detail=` and
    /// `columnsize=` each change what the index stores and what a query may ask
    /// of it, and both fell through to the catch-all that records an option and
    /// changes nothing - so `detail='none'`, under which SQLite refuses a
    /// phrase query because it holds no positions, answered the phrase query
    /// here from positions it had stored anyway.
    ///
    /// **`content_rowid=` is not on the list, because it works.** The review
    /// counted it with the other two; measured against the pinned SQLite on an
    /// external content table whose key is called `key` rather than `id`, both
    /// engines answer the same rows. This module reads an external table's
    /// columns by name, so the name of its rowid needs no separate handling.
    ///
    /// It is a refusal at `CREATE VIRTUAL TABLE` and a refusal on every query
    /// of a table an older build or SQLite wrote, rather than a refusal to
    /// open: a database has to open before the table in it can be dropped.
    pub(super) unsupported: Option<String>,
    /// Whether `content=''` made the table contentless.
    ///
    /// **Accepted and ignored until task-1979, R5.** `content=''` fell through
    /// to the catch-all that records an option and changes nothing, so
    /// `%_content` was created anyway and every column read back the document
    /// text where SQLite answers NULL. That is the difference an application
    /// chooses the option for: the source text is not supposed to be in the
    /// database at all, and it was, silently.
    pub(super) contentless: bool,
}

/// Reads the arguments of a `CREATE VIRTUAL TABLE ... USING fts5(...)`.
///
/// An argument is either a column - a bare name, optionally followed by
/// `UNINDEXED` - or an option, written `name = value`. That is FTS5's own
/// grammar, and the reason a column cannot be called `tokenize`.
pub(super) fn parse_options(arguments: &[Vec<u8>]) -> DbResult<Options> {
    let mut columns = Vec::new();
    let mut tokenizer = vec![b"unicode61".to_vec()];
    let mut content: Option<Vec<u8>> = None;
    let mut contentless = false;
    let mut unsupported: Option<String> = None;
    for argument in arguments {
        let text = String::from_utf8_lossy(argument).trim().to_string();
        if let Some((name, value)) = split_option(&text) {
            match name.to_ascii_lowercase().as_str() {
                "tokenize" => tokenizer = tokenize::parse_specification(&value),
                // **An external content table names its rows' owner**, and
                // reaching them is the one thing the module contract does not
                // give a module for free. It is asked for explicitly, by name,
                // through `ShadowTable::owner` - the same grant `fts5vocab`
                // uses - so the reach stays a grant rather than a hole.
                //
                // `content=''` is a *contentless* table, which is a different
                // thing: it stores no rows on purpose.
                "content" if !unquote_option(&value).is_empty() => {
                    content = Some(unquote_option(&value).as_bytes().to_vec())
                }
                "content" => contentless = true,
                // **Three options that change what the index holds.** Each is
                // accepted at the value this build does store - which is
                // SQLite's own default - and refused at every other, because
                // answering a query against a `detail='none'` index from
                // positions the option says are not there is the one outcome
                // that is worse than saying no.
                "detail" if !unquote_option(&value).eq_ignore_ascii_case("full") => {
                    unsupported = Some(format!("fts5 detail={}", unquote_option(&value)))
                }
                "columnsize" if unquote_option(&value).trim() != "1" => {
                    unsupported = Some(format!("fts5 columnsize={}", unquote_option(&value)))
                }

                // The options this build understands and the ones it does not
                // are both accepted, because refusing one would make a schema
                // SQLite wrote unreadable. What is not understood is recorded
                // in `%_config` and changes nothing.
                _ => {}
            }
            continue;
        }
        // A column may be written `"a b" UNINDEXED`, so the words are split
        // with the quotes honoured rather than on whitespace - or the column
        // would be called `"a`.
        let words = tokenize::split_words(&text);
        let Some(name) = words.first() else {
            continue;
        };
        let unindexed = words
            .iter()
            .skip(1)
            .any(|word| word.eq_ignore_ascii_case(b"UNINDEXED"));
        columns.push(ColumnSpec {
            name: name.clone(),
            unindexed,
        });
    }
    if columns.is_empty() {
        return Err(failure("an fts5 table needs at least one column"));
    }
    Ok(Options {
        columns,
        tokenizer,
        content,
        contentless,
        unsupported,
    })
}

/// Strips the quotes an option's value is written inside.
///
/// FTS5 takes `content='c'`, `content="c"` and a bare `content=c` as the same
/// thing, and the difference between the empty value and a name is what decides
/// whether a table is contentless or external.
pub(super) fn unquote_option(value: &str) -> &str {
    let text = value.trim();
    for quote in ['\'', '"', '`'] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return &text[1..text.len() - 1];
        }
    }
    text
}

/// Splits `name = value`, which is how an option is written.
fn split_option(text: &str) -> Option<(String, String)> {
    let (name, value) = text.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some((name.to_string(), value.trim().to_string()))
}

/// Returns the refusal an option this build cannot honour reports.
///
/// Carried as `unsupported` so the command line exits 3 and the driver reports
/// the status `unsupported`, which is how a caller tells "this engine has not
/// built that" apart from "your statement is wrong".
///
/// @param what - the option and the value it was given
pub(super) fn unsupported_option(what: &str) -> inillucent_base::DbError {
    failure(format!(
        "{what} is not built; this index stores full positions and one size per column"
    ))
    .with_message(format!("{what} is not built"))
    .with_unsupported(what.to_string())
}
