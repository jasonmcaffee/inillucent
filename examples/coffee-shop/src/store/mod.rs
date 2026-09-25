//! The database: every SQL statement the service runs.
//!
//! Keeping all the SQL under `store/` means a reader can see the whole
//! contract between the service and inillucent in one folder. The HTTP
//! handlers in `routes/` call the methods here and never write SQL.
//!
//! | File | What it holds |
//! |---|---|
//! | `menu.rs` | categories, items, prices by size, recipes, modifiers, and each item's margin |
//! | `orders.rs` | opening an order, adding lines, promotions and points, pricing, the receipt, the queue |
//! | `payments.rs` | paying, fulfilling, cancelling and refunding an order |
//! | `people.rs` | customers and their points, staff and their shifts |
//! | `inventory.rs` | ingredients, purchases, supplier payments, waste, stock counts, reconciliation |
//! | `ledger.rs` | manual journal entries, the account ledger, closing a day and its tip pool |
//! | `reports.rs` | the day's Z report, sales by day and hour, item rankings, trial balance, income statement, balance sheet |
//!
//! ## One statement at a time
//!
//! [`SharedDatabase`] opens the database on a thread of its own and runs one
//! statement at a time, from any number of threads. The HTTP server runs each
//! request on a tokio worker thread, and every one of them shares this one
//! handle. A transaction holds the database for its whole length, so the
//! transactions here are short: they read what they need, write, check, and
//! commit.
//!
//! ## Reports are the SQL's own rows
//!
//! A report's JSON is the query's result, one object per row with one field per
//! column, made by [`Sql::objects`]. So the README can show a query and the JSON
//! a client gets for it side by side, and the two cannot drift apart. The
//! objects the service builds itself, such as a receipt, are Rust structs.

mod inventory;
mod ledger;
mod menu;
mod orders;
mod payments;
mod people;
mod reports;

use std::path::Path;

use inillucent::{Rows, SharedDatabase, SharedTransaction, Value};
use serde_json::{Map, Value as Json};

use crate::error::{ApiError, ApiResult};
use crate::schema::SCHEMA;

pub use inventory::{Count, NewIngredient, NewPurchase, Waste};
pub use ledger::{CloseDay, ManualEntry};
pub use menu::{NewItem, NewModifier, NewPromotion, PriceChange};
pub use orders::{LineRequest, NewOrder};
pub use payments::PayRequest;
pub use people::{NewCustomer, NewStaff};

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
            .query_all("SELECT count(*) AS n FROM sqlite_schema WHERE type = 'table' AND name = 'orders'", &[])
            .map_err(|error| error.to_string())?;
        if Record::first(&found).map(|row| row.int("n")) == Some(0) {
            self.db.execute_batch(SCHEMA).map_err(|error| format!("cannot create the schema: {error}"))?;
        }
        Ok(())
    }

    /// Opens a transaction. Dropping it without calling `commit` rolls it back,
    /// so an early return with `?` never leaves half a change behind.
    fn begin(&self) -> ApiResult<SharedTransaction<'_>> {
        Ok(self.db.begin()?)
    }

    /// Answers the time of an event: the one the caller sent, or now.
    ///
    /// Every write takes an optional `at`, so the seed can make up last week's
    /// trading and the tests can fix the clock. `strftime` both normalises the
    /// text and checks it: `2026-09-25 8:00` becomes `2026-09-25T08:00:00Z`,
    /// and `yesterday` becomes NULL, which is answered with `400`.
    ///
    /// @param at - a time the caller sent, or nothing for now
    pub fn timestamp(&self, at: Option<&str>) -> ApiResult<String> {
        let rows = self.rows("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', coalesce(?1, 'now')) AS at", &[opt_text(at)])?;
        match Record::first(&rows).and_then(|row| row.opt_text("at")) {
            Some(at) => Ok(at),
            None => Err(ApiError::bad_request(format!("`{}` is not a time; use YYYY-MM-DDTHH:MM:SSZ", at.unwrap_or_default()))),
        }
    }

    /// Answers a business day: the one the caller sent, or today.
    ///
    /// @param day - a `YYYY-MM-DD` day the caller sent, or nothing for today
    pub fn day(&self, day: Option<&str>) -> ApiResult<String> {
        let rows = self.rows("SELECT date(coalesce(?1, 'now')) AS day", &[opt_text(day)])?;
        match Record::first(&rows).and_then(|row| row.opt_text("day")) {
            Some(day) => Ok(day),
            None => Err(ApiError::bad_request(format!("`{}` is not a day; use YYYY-MM-DD", day.unwrap_or_default()))),
        }
    }
}

impl Store {
    /// Answers the day `offset` days after `day`, as the database's `date()` computes it.
    ///
    /// @param day - a `YYYY-MM-DD` day
    /// @param offset - days to add; negative for days before
    pub fn day_after(&self, day: &str, offset: i64) -> ApiResult<String> {
        let rows = self.rows("SELECT date(?1, printf('%+d days', ?2)) AS day", &[text(day), int(offset)])?;
        Record::first(&rows).and_then(|row| row.opt_text("day")).ok_or_else(|| ApiError::bad_request(format!("`{day}` is not a day")))
    }

    /// Answers a time some minutes after another.
    ///
    /// @param at - a time
    /// @param minutes - how many minutes later
    pub fn minutes_after(&self, at: &str, minutes: i64) -> ApiResult<String> {
        let rows = self.rows("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1, printf('%+d minutes', ?2)) AS at", &[text(at), int(minutes)])?;
        Record::first(&rows).and_then(|row| row.opt_text("at")).ok_or_else(|| ApiError::bad_request(format!("`{at}` is not a time")))
    }
}

/// What the store runs SQL through: the database itself, or an open transaction.
///
/// The helpers that several operations share, such as pricing an order or
/// checking the books balance, take `&impl Sql`, so they run inside whatever
/// transaction the caller opened.
pub trait Sql {
    /// Runs one query and returns every row.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2` and so on
    fn rows(&self, sql: &str, params: &[Value]) -> ApiResult<Rows>;

    /// Runs one statement for its effect and returns how many rows it changed.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2` and so on
    fn run(&self, sql: &str, params: &[Value]) -> ApiResult<u64>;

    /// Runs a query and returns its rows as JSON objects, one field per column.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2` and so on
    /// @param json_columns - columns that hold JSON text, such as the output of
    ///   `json_group_object`, to be nested as JSON and not as a string
    fn objects(&self, sql: &str, params: &[Value], json_columns: &[&str]) -> ApiResult<Vec<Json>> {
        Ok(objects(&self.rows(sql, params)?, json_columns))
    }

    /// Runs a query that returns one row and answers it as a JSON object, or
    /// `404` naming `what` when there is no row.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2` and so on
    /// @param what - what the row is, such as `order 7`, for the `404`
    fn object(&self, sql: &str, params: &[Value], what: &str) -> ApiResult<Json> {
        self.objects(sql, params, &[])?.into_iter().next().ok_or_else(|| ApiError::not_found(what))
    }

    /// Runs a query that returns one integer, such as a `count(*)`.
    ///
    /// @param sql - the statement; its first column is read
    /// @param params - the values bound to `?1`, `?2` and so on
    fn integer(&self, sql: &str, params: &[Value]) -> ApiResult<i64> {
        let rows = self.rows(sql, params)?;
        Ok(rows.rows.first().and_then(|row| row.first()).and_then(Value::as_i64).unwrap_or(0))
    }

    /// Reads one row of the `setting` table.
    ///
    /// @param key - the setting, such as `tax_rate_bp`
    fn setting(&self, key: &str) -> ApiResult<i64> {
        self.integer("SELECT value FROM setting WHERE key = ?1", &[text(key)])
    }
}

impl Sql for Store {
    fn rows(&self, sql: &str, params: &[Value]) -> ApiResult<Rows> {
        Ok(self.db.query_all(sql, params)?)
    }

    fn run(&self, sql: &str, params: &[Value]) -> ApiResult<u64> {
        Ok(self.db.execute(sql, params)?)
    }
}

impl Sql for SharedTransaction<'_> {
    fn rows(&self, sql: &str, params: &[Value]) -> ApiResult<Rows> {
        Ok(self.query(sql, params, usize::MAX)?)
    }

    fn run(&self, sql: &str, params: &[Value]) -> ApiResult<u64> {
        Ok(self.execute(sql, params)?)
    }
}

/// Refuses to commit a transaction that left the books out of balance.
///
/// Every journal entry must have debits equal to its credits. The triggers and
/// the service are written so that they always do, and this reads the
/// `unbalanced_entry` view before commit to prove it. A failure here means a
/// defect in this program, so the answer is `500` and the transaction is
/// dropped, which rolls every write back.
///
/// @param sql - the open transaction
pub fn ensure_balanced(sql: &impl Sql) -> ApiResult<()> {
    let bad = sql.objects("SELECT id, source, memo, debit_cents, credit_cents FROM unbalanced_entry", &[], &[])?;
    match bad.first() {
        None => Ok(()),
        Some(entry) => Err(ApiError::internal(format!("a journal entry does not balance, so nothing was saved: {entry}"))),
    }
}

/// Turns a result into JSON objects, one per row and one field per column.
///
/// @param rows - the result
/// @param json_columns - columns holding JSON text, parsed and nested
pub fn objects(rows: &Rows, json_columns: &[&str]) -> Vec<Json> {
    rows.rows
        .iter()
        .map(|row| {
            let mut object = Map::new();
            for (column, value) in rows.columns.iter().zip(row) {
                let nested = json_columns.contains(&column.name.as_str());
                object.insert(column.name.clone(), json_value(value, nested));
            }
            Json::Object(object)
        })
        .collect()
}

/// Turns one cell into JSON.
///
/// @param value - the cell
/// @param nested - whether the cell holds JSON text to parse
fn json_value(value: &Value, nested: bool) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Integer(number) => Json::from(*number),
        Value::Real(number) => Json::from(*number),
        Value::Text(text) if nested => serde_json::from_str(text).unwrap_or_else(|_| Json::from(text.as_str())),
        Value::Text(text) => Json::from(text.as_str()),
        Value::Blob(bytes) => Json::from(bytes.len()),
    }
}

/// One row of a result, read by column name.
///
/// Reading `row.int("total_cents")` rather than `row[5]` keeps the Rust code
/// right when a column is added to a `SELECT`.
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
        self.value(name).as_i64()
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
}

/// Reads an integer out of a value, counting a whole `REAL` as one.
trait AsInteger {
    /// The integer, or nothing for NULL, text and blobs.
    fn as_i64(&self) -> Option<i64>;
}

impl AsInteger for Value {
    fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(value) => Some(*value),
            Value::Real(value) => Some(*value as i64),
            _ => None,
        }
    }
}

/// Binds an integer.
///
/// @param value - the value
fn int(value: i64) -> Value {
    Value::Integer(value)
}

/// Binds an optional integer: `Some` as an integer, `None` as NULL.
///
/// @param value - the value
fn opt_int(value: Option<i64>) -> Value {
    value.map(Value::Integer).unwrap_or(Value::Null)
}

/// Binds a string as text.
///
/// @param value - the value
fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

/// Binds an optional string: `Some` as text, `None` as NULL.
///
/// @param value - the value
fn opt_text(value: Option<&str>) -> Value {
    value.map(text).unwrap_or(Value::Null)
}

/// Binds a Rust value as JSON text, for a statement that reads it with `json_each`.
///
/// @param value - anything serde can write
fn json_text<T: serde::Serialize>(value: &T) -> ApiResult<Value> {
    serde_json::to_string(value).map(Value::Text).map_err(|error| ApiError::bad_request(error.to_string()))
}
