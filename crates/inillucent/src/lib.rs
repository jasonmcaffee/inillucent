//! The public Rust API: every type is [`inillucent_driver`]'s, re-exported.
//!
//! Invariant: **there is one Rust API and this is a name for it (task-1961,
//! A2).** This crate used to define a second surface over the same engine, with
//! its own `Value`, its own `Statement` and no transaction type at all, and
//! nothing anywhere said which of the two an application should depend on. The
//! driver won because it has the transaction, the [`Rows`] type, the cancel
//! flag and the checked capability table, and because the C ABI and the four
//! language packages already go through it. `cargo add inillucent` keeps
//! working and now gives that.
//!
//! ```no_run
//! use inillucent::{Database, Value};
//!
//! # fn main() -> inillucent::Result<()> {
//! let database = Database::open("app.rdb")?;
//! let connection = database.session();
//! connection.execute("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
//! let transaction = connection.begin()?;
//! transaction.execute(
//!     "INSERT INTO note (body) VALUES (?1)",
//!     &[Value::Text("hello".to_string())],
//! )?;
//! transaction.commit()?;
//! let mut statement = connection.prepare("SELECT body FROM note WHERE id = ?1")?;
//! let rows = statement.query(&[Value::Integer(1)], 1)?;
//! assert_eq!(rows.value(0, 0).and_then(Value::text), Some("hello"));
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

pub use inillucent_driver::*;
