//! The database: every SQL statement the service runs.
//!
//! Keeping all the SQL under `store/` means a reader can see the whole
//! contract between the service and inillucent in one folder. The HTTP
//! handlers in `routes.rs` call the methods here and never write SQL.
//!
//! | File | What it holds |
//! |---|---|
//! | `people.rs` | people, and each person's workload ranked with window functions |
//! | `lists.rs` | lists, the overview of every list, reordering, and the TodoMVC "toggle all" and "clear completed" |
//! | `todos.rs` | one todo: create, read with its subtask tree, update, move, delete, and the search index |
//! | `tags.rs` | tags, comments, and a todo's timeline |
//! | `reports.rs` | keyword search, the agenda, and completions per day |
//!
//! ## One statement at a time
//!
//! [`SharedDatabase`] opens the database on a thread of its own and runs one
//! statement at a time, from any number of threads. The HTTP server runs each
//! request on a tokio worker thread, and every one of them shares this one
//! handle. A transaction holds the database for its whole length, so the
//! transactions here are short: they read what they need, write, and commit.

mod lists;
mod people;
mod reports;
mod tags;
mod todos;

use std::path::Path;

use inillucent::{Rows, SharedDatabase, SharedTransaction, Value};

use crate::error::{ApiError, ApiResult};
use crate::schema::SCHEMA;

pub use lists::{ListPatch, NewList, TodoFilter};
pub use people::NewPerson;
pub use tags::NewComment;
pub use todos::{NewTodo, TodoCard, TodoPatch};

/// The database the service reads and writes.
///
/// Cloning it is cheap: every clone is the same database and the same thread.
#[derive(Clone)]
pub struct Store {
    db: SharedDatabase,
}

impl Store {
    /// Opens the database, creating the file and the schema when they are missing.
    ///
    /// `PRAGMA foreign_keys = ON` is run on every open, because it is a
    /// setting of the connection and not of the file. Without it the
    /// `REFERENCES` clauses in the schema are parsed and never enforced.
    ///
    /// @param path - the `.rdb` file
    pub fn open(path: &Path) -> Result<Store, String> {
        if let Some(folder) = path.parent().filter(|folder| !folder.as_os_str().is_empty()) {
            std::fs::create_dir_all(folder).map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
        }
        let db = SharedDatabase::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        db.execute("PRAGMA foreign_keys = ON", &[]).map_err(|error| format!("cannot turn on foreign keys: {error}"))?;
        let store = Store { db };
        store.ensure_schema()?;
        Ok(store)
    }

    /// Creates the tables the first time the database is opened.
    fn ensure_schema(&self) -> Result<(), String> {
        let found = self
            .db
            .query_all("SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'todo'", &[])
            .map_err(|error| error.to_string())?;
        if Record::first(&found).map(|row| row.int_at(0)) == Some(0) {
            self.db.execute_batch(SCHEMA).map_err(|error| format!("cannot create the schema: {error}"))?;
        }
        Ok(())
    }

    /// Runs one query and returns every row.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2` and so on
    fn query(&self, sql: &str, params: &[Value]) -> ApiResult<Rows> {
        Ok(self.db.query_all(sql, params)?)
    }

    /// Runs one statement for its effect and returns how many rows it changed.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2` and so on
    fn execute(&self, sql: &str, params: &[Value]) -> ApiResult<u64> {
        Ok(self.db.execute(sql, params)?)
    }

    /// Opens a transaction. Dropping it without calling `commit` rolls it back,
    /// so an early return with `?` never leaves half a change behind.
    fn begin(&self) -> ApiResult<SharedTransaction<'_>> {
        Ok(self.db.begin()?)
    }

    /// Answers `today` when the caller passed one, and the database's own date otherwise.
    ///
    /// Every question about "overdue" or "due this week" takes the day as a
    /// parameter, so the tests can ask about a fixed day and get the same
    /// answer every time they run.
    ///
    /// @param today - a `YYYY-MM-DD` day, or nothing for today
    pub fn today(&self, today: Option<&str>) -> ApiResult<String> {
        let rows = self.query("SELECT date(coalesce(?1, 'now'))", &[optional_text(today)])?;
        match Record::first(&rows).and_then(|row| row.opt_text_at(0)) {
            Some(day) => Ok(day),
            None => Err(ApiError::bad_request(format!("`{}` is not a date; use YYYY-MM-DD", today.unwrap_or_default()))),
        }
    }

    /// Returns the day `offset` days after `day`, as the database's `date()` computes it.
    ///
    /// @param day - a `YYYY-MM-DD` day
    /// @param offset - days to add; negative for days before
    pub fn day_after(&self, day: &str, offset: i64) -> ApiResult<String> {
        let rows = self.query("SELECT date(?1, printf('%+d days', ?2))", &[text(day), Value::Integer(offset)])?;
        Record::first(&rows).and_then(|row| row.opt_text_at(0)).ok_or_else(|| ApiError::bad_request(format!("`{day}` is not a date")))
    }
}

/// Runs a query inside a transaction and returns every row.
///
/// @param tx - the open transaction
/// @param sql - the statement
/// @param params - the values bound to `?1`, `?2` and so on
fn tx_query(tx: &SharedTransaction, sql: &str, params: &[Value]) -> ApiResult<Rows> {
    Ok(tx.query(sql, params, usize::MAX)?)
}

/// Runs a statement inside a transaction and returns how many rows it changed.
///
/// @param tx - the open transaction
/// @param sql - the statement
/// @param params - the values bound to `?1`, `?2` and so on
fn tx_execute(tx: &SharedTransaction, sql: &str, params: &[Value]) -> ApiResult<u64> {
    Ok(tx.execute(sql, params)?)
}

/// One row of a result, read by column name.
///
/// Reading `row.text("title")` rather than `row[3]` keeps the Rust code right
/// when a column is added to a `SELECT`. A name that is not in the result is a
/// mistake in the SQL next to it, and the end to end tests would catch it: the
/// reader returns NULL and the field comes back empty.
pub struct Record<'a> {
    rows: &'a Rows,
    row: usize,
}

impl<'a> Record<'a> {
    /// Returns every row of a result as a record.
    ///
    /// @param rows - the result
    pub fn all(rows: &'a Rows) -> impl Iterator<Item = Record<'a>> + 'a {
        (0..rows.rows.len()).map(move |row| Record { rows, row })
    }

    /// Returns the first row, or nothing when the result is empty.
    ///
    /// @param rows - the result
    pub fn first(rows: &'a Rows) -> Option<Record<'a>> {
        Record::all(rows).next()
    }

    /// Returns the cell in the named column, or NULL when there is no such column.
    ///
    /// @param name - the column's name, as the `SELECT` wrote it
    fn value(&self, name: &str) -> &'a Value {
        debug_assert!(self.rows.column(name).is_some(), "the result has no column named `{name}`");
        self.rows.column(name).and_then(|column| self.rows.value(self.row, column)).unwrap_or(&Value::Null)
    }

    /// Reads an integer column, or 0 when it is NULL.
    ///
    /// @param name - the column's name
    pub fn int(&self, name: &str) -> i64 {
        self.opt_int(name).unwrap_or(0)
    }

    /// Reads an integer column that may be NULL.
    ///
    /// @param name - the column's name
    pub fn opt_int(&self, name: &str) -> Option<i64> {
        match self.value(name) {
            Value::Integer(value) => Some(*value),
            Value::Real(value) => Some(*value as i64),
            _ => None,
        }
    }

    /// Reads a number column as a float, or 0.0 when it is NULL.
    ///
    /// @param name - the column's name
    pub fn real(&self, name: &str) -> f64 {
        match self.value(name) {
            Value::Real(value) => *value,
            Value::Integer(value) => *value as f64,
            _ => 0.0,
        }
    }

    /// Reads a 0 or 1 column as a bool.
    ///
    /// @param name - the column's name
    pub fn bool(&self, name: &str) -> bool {
        self.int(name) != 0
    }

    /// Reads a text column, or an empty string when it is NULL.
    ///
    /// @param name - the column's name
    pub fn text(&self, name: &str) -> String {
        self.opt_text(name).unwrap_or_default()
    }

    /// Reads a text column that may be NULL.
    ///
    /// @param name - the column's name
    pub fn opt_text(&self, name: &str) -> Option<String> {
        self.value(name).text().map(str::to_string)
    }

    /// Reads a column that holds JSON text, such as the output of `json_group_array`.
    ///
    /// @param name - the column's name
    pub fn json(&self, name: &str) -> serde_json::Value {
        self.value(name).text().and_then(|text| serde_json::from_str(text).ok()).unwrap_or(serde_json::Value::Null)
    }

    /// Reads a column that holds a JSON array of strings, such as a todo's tags.
    ///
    /// @param name - the column's name
    pub fn strings(&self, name: &str) -> Vec<String> {
        self.value(name).text().and_then(|text| serde_json::from_str(text).ok()).unwrap_or_default()
    }

    /// Reads the integer in a column by position. Used only for `SELECT count(*)`.
    ///
    /// @param column - the column, from zero
    fn int_at(&self, column: usize) -> i64 {
        match self.rows.value(self.row, column) {
            Some(Value::Integer(value)) => *value,
            _ => 0,
        }
    }

    /// Reads the text in a column by position. Used only for a one column `SELECT`.
    ///
    /// @param column - the column, from zero
    fn opt_text_at(&self, column: usize) -> Option<String> {
        self.rows.value(self.row, column).and_then(Value::text).map(str::to_string)
    }
}

/// Binds an optional integer: `Some` as an integer, `None` as NULL.
///
/// @param value - the value
fn optional_int(value: Option<i64>) -> Value {
    value.map(Value::Integer).unwrap_or(Value::Null)
}

/// Binds an optional string: `Some` as text, `None` as NULL.
///
/// @param value - the value
fn optional_text(value: Option<&str>) -> Value {
    value.map(|text| Value::Text(text.to_string())).unwrap_or(Value::Null)
}

/// Binds a string as text.
///
/// @param value - the value
fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}
