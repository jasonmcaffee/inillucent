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
//! **The example below runs (task-1969, 4.11).** It was marked as compile-only
//! for the first year of this crate's life, so the `assert_eq!` at the bottom
//! was checked by the compiler and never executed, and
//! `cargo test --doc -p inillucent` - a stage of both validate scripts - was
//! proving that the facade's only example parses. task-1961's first criterion
//! asked for "one runnable example"; this is it.
//!
//! The hidden lines build a database under the system's temporary directory and
//! remove it afterwards, which is the pattern
//! `drivers/inillucent-driver/src/lib.rs` already uses. The `Database::open`
//! line a reader sees names `app.rdb` because that is what an application
//! writes; the line that actually runs opens the temporary file, and the shown
//! one is compiled out. Two lines rather than one so that the example reads as
//! the thing to copy rather than as a test fixture.
//!
//! ```
//! use inillucent::{Database, Value};
//!
//! # fn main() -> inillucent::Result<()> {
//! # let directory = std::env::temp_dir()
//! #     .join(format!("inillucent-facade-doc-{}", std::process::id()));
//! # std::fs::create_dir_all(&directory).ok();
//! # let path = directory.join("app.rdb");
//! # let database = Database::open(&path)?;
//! # #[cfg(any())]
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
//! # drop(statement);
//! # drop(connection);
//! # drop(database);
//! # std::fs::remove_dir_all(&directory).ok();
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
