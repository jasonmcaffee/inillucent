//! What is in a database.
//!
//! Invariant: **every answer here is SQL the engine already runs**, so there is
//! one schema reader in the tree rather than two that can disagree. The engine
//! registers `sqlite_schema` as a readable table and answers `PRAGMA
//! table_info`, `index_list`, `index_info`, `table_list` and `database_list`
//! from its honoured set; this module composes those and nothing else.
//!
//! It stops where a decision starts. `Table::key` is the primary key as the
//! schema declared it, and `Table::without_rowid` is a fact about the table -
//! but whether a table with no declared key is nonetheless *addressable*
//! through SQLite's implicit `rowid` is the consumer's rule, and it depends on
//! whether that consumer is willing to write a statement against a hidden
//! column. `unluminous-db` has that rule and it is a careful one, checking all
//! three of `rowid`, `_rowid_` and `oid` for shadowing. Reimplementing it here
//! would be a second copy of a rule that exists to prevent an `UPDATE` from
//! changing two rows, so this reports the facts and leaves the rule where it is.

use crate::value::Column;

/// What kind of thing a schema entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A table.
    Table,
    /// A view.
    View,
    /// An index.
    Index,
    /// A trigger. Stored by an older file; this engine refuses to create one.
    Trigger,
}

impl Kind {
    /// Returns the word `sqlite_schema` uses, which is what a caller matches on.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Table => "table",
            Kind::View => "view",
            Kind::Index => "index",
            Kind::Trigger => "trigger",
        }
    }

    /// Reads the kind out of `sqlite_schema`'s own `type` column.
    ///
    /// @param word - the value of the `type` column
    pub fn from_name(word: &str) -> Option<Kind> {
        match word {
            "table" => Some(Kind::Table),
            "view" => Some(Kind::View),
            "index" => Some(Kind::Index),
            "trigger" => Some(Kind::Trigger),
            _ => None,
        }
    }

    /// Reports whether rows can be read out of it.
    pub fn holds_rows(self) -> bool {
        matches!(self, Kind::Table | Kind::View)
    }
}

/// One entry in the schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// What it is called.
    pub name: String,
    /// What kind of thing it is.
    pub kind: Kind,
}

/// One table or view, once its columns have been asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    /// What it is called.
    pub name: String,
    /// Its columns, in declared order.
    pub columns: Vec<Column>,
    /// Whether each column refuses NULL, in the same order as `columns`.
    pub not_null: Vec<bool>,
    /// The primary key's columns, in key order.
    ///
    /// **A list rather than a flag**, because a compound key needs every part of
    /// itself in a `WHERE` clause and matching on one of two would change the
    /// wrong rows.
    pub key: Vec<String>,
    /// Whether the table was declared `WITHOUT ROWID`, and so has no implicit
    /// key a caller could address a row by.
    pub without_rowid: bool,
}

impl Table {}

/// Puts quotes round an identifier, doubling any quote already inside it.
///
/// **Needed because an identifier cannot be a bound parameter.** `PRAGMA
/// table_info(?)` is not a thing SQL has: the name is part of the statement's
/// own grammar. So it is quoted, and the doubling is what keeps a name holding a
/// quote from ending the identifier early.
///
/// @param name - the identifier
pub fn quoted(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for character in name.chars() {
        if character == '"' {
            out.push('"');
        }
        out.push(character);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quote inside a name is doubled, so a name cannot end its own
    /// identifier and turn the rest of it into syntax.
    #[test]
    fn an_identifier_with_a_quote_in_it_is_still_one_identifier() {
        assert_eq!(quoted("people"), "\"people\"");
        assert_eq!(quoted("odd\"name"), "\"odd\"\"name\"");
        assert_eq!(quoted("drop\" ; --"), "\"drop\"\" ; --\"");
    }

    /// The four words `sqlite_schema` writes round-trip, and anything else is
    /// reported as unknown rather than guessed at.
    #[test]
    fn a_schema_kind_round_trips_and_an_unknown_one_is_refused() {
        for kind in [Kind::Table, Kind::View, Kind::Index, Kind::Trigger] {
            assert_eq!(Kind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(Kind::from_name("sequence"), None);
        assert!(Kind::Table.holds_rows());
        assert!(!Kind::Index.holds_rows());
    }
}
