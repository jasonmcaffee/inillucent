//! What a statement answered.
//!
//! One value for every kind of statement, because a consumer draws one thing: a
//! `SELECT` fills [`Rows::columns`] and [`Rows::rows`], a write fills
//! [`Rows::affected`] and leaves the grid empty, and an `INSERT ... RETURNING`
//! fills all three.
//!
//! ## `more` is counted, not guessed
//!
//! A grid that says `1-200 of 200+` is claiming somebody looked. `unluminous-db`
//! makes that true against a server by asking for `limit + 1` rows and cutting
//! back - nobody counted the rest, and a count that claimed to be exact would be
//! a lie told on every page.
//!
//! This driver cannot do that without rewriting the caller's statement, and
//! rewriting a caller's statement is the failure four instruments on this
//! project have already had. It does not need to: the engine materialises, so by
//! the time a result reaches here every row exists and the count is a
//! `Vec::len()`. [`Rows::total`] is therefore **exact** and [`Rows::more`] is a
//! fact rather than an inference - which is stronger than what a server-backed
//! driver can offer.
//!
//! The cost is real and is stated rather than hidden: a query over a large table
//! costs what the whole result costs, because that is what the engine
//! underneath does. A caller that cannot afford it puts a `LIMIT` in its own SQL,
//! where the planner can act on it.
//!
//! Invariant: **one value describes every kind of statement, and the fields a
//! statement did not fill are empty rather than absent.** A consumer draws one
//! thing whether it ran a `SELECT`, a write or an `INSERT ... RETURNING`, so
//! there is no shape to branch on before reading an answer.

use std::time::Duration;

use crate::value::{Column, Value};

/// The result of one statement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    /// The result columns, empty for a statement that returns none.
    pub columns: Vec<Column>,
    /// The rows, cut to the caller's limit.
    pub rows: Vec<Vec<Value>>,
    /// How many rows the statement changed. `None` for a query.
    pub affected: Option<u64>,
    /// How many rows the statement produced, exactly, before the limit.
    pub total: usize,
    /// Whether the limit cut anything off.
    pub more: bool,
    /// How long the statement took, measured around the call.
    pub elapsed: Duration,
    /// A one-line summary.
    ///
    /// **The driver's own words, not the engine's.** A PostgreSQL client quotes
    /// the server's completion tag because the server sends one; this engine
    /// sends none, so the tag is composed from the statement's leading keyword
    /// and the count. It is for a status bar, and a caller that needs a fact
    /// reads [`Rows::affected`] or [`Rows::total`].
    pub tag: String,
}

impl Rows {
    /// Returns which column has this name.
    ///
    /// Compared exactly, because two SQL identifiers that differ in case are
    /// different names once they have been quoted.
    ///
    /// @param name - the column's name
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    /// Returns one cell, or `None` if the row or column is not there.
    ///
    /// @param row - the row, from zero
    /// @param column - the column, from zero
    pub fn value(&self, row: usize, column: usize) -> Option<&Value> {
        self.rows.get(row).and_then(|row| row.get(column))
    }

    /// Returns one line for a status bar.
    pub fn summary(&self) -> String {
        format!("{} in {} ms", self.tag, self.elapsed.as_millis())
    }
}

/// Composes a completion tag from a statement and what it did.
///
/// The leading keyword is the caller's own word, uppercased, so `select 1`
/// answers `SELECT 1` the way `psql` would. A statement with no leading word -
/// which the parser would have refused before reaching here - answers `OK`.
///
/// @param sql - the statement the caller wrote
/// @param count - the rows produced, or changed
pub fn tag_for(sql: &str, count: usize) -> String {
    let verb: String = sql
        .trim_start()
        .chars()
        .take_while(|character| character.is_ascii_alphabetic())
        .collect();
    match verb.is_empty() {
        true => "OK".to_owned(),
        false => format!("{} {count}", verb.to_uppercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tag is the caller's own verb and the count, which is what a console
    /// reader expects to see.
    #[test]
    fn a_tag_quotes_the_statements_own_leading_word() {
        assert_eq!(tag_for("SELECT a FROM t", 27), "SELECT 27");
        assert_eq!(tag_for("  update t set a = 1", 1), "UPDATE 1");
        assert_eq!(tag_for("\n\tINSERT INTO t VALUES (1)", 1), "INSERT 1");
        assert_eq!(tag_for("", 0), "OK");
        assert_eq!(
            tag_for("(SELECT 1)", 1),
            "OK",
            "a leading paren has no verb"
        );
    }

    /// A column is found by an exact name, because quoting makes case
    /// significant.
    #[test]
    fn a_column_is_found_by_its_exact_name() {
        let rows = Rows {
            columns: vec![Column::new("Name", "TEXT"), Column::new("name", "TEXT")],
            ..Rows::default()
        };
        assert_eq!(rows.column("Name"), Some(0));
        assert_eq!(rows.column("name"), Some(1));
        assert_eq!(rows.column("NAME"), None);
    }

    /// Reading past the end answers `None` rather than panicking, because a
    /// grid asks for cells it has not checked the existence of.
    #[test]
    fn a_cell_that_is_not_there_is_none() {
        let rows = Rows {
            columns: vec![Column::new("a", "INTEGER")],
            rows: vec![vec![Value::Integer(1)]],
            ..Rows::default()
        };
        assert_eq!(rows.value(0, 0), Some(&Value::Integer(1)));
        assert_eq!(rows.value(0, 1), None);
        assert_eq!(rows.value(9, 0), None);
    }
}
