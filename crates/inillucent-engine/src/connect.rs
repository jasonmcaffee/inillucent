//! Opening a database and running statements against it.
//!
//! Invariant: **this is the shape a caller moves onto, and it holds no engine
//! state of its own.** Everything here delegates to [`crate::ImportedDatabase`],
//! which is the engine; what this adds is the `open` / `connect` / statement
//! shape that `inillucent-session` exposes today, so that re-rooting
//! `inillucent::Database`, the CLI and `inillucent-migrate` is a change of import
//! rather than a change of design.
//!
//! It is deliberately small. The old connection carries pragmas, savepoints,
//! hooks, collation and function registries, an authorizer, a busy timeout and a
//! VFS choice, and every one of those is a decision about behaviour rather than
//! a wrapper. Writing them all here before anything used them would be inventing
//! a second connection nobody had asked a question of. What is here is what the
//! first callers need, and the rest arrives as they are moved.
//!
//! ## `open` creates
//!
//! `Database::open` on a path that does not exist **creates** the database, and
//! on one that does it **opens and recovers** it. That is what a caller means by
//! `open`, it is what `inillucent-session` does today, and it is the reason
//! `ImportedDatabase::create` had to exist before this file could.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use inillucent_base::DbResult;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

use crate::{ImportedDatabase, DEFAULT_FRAMES};

/// The page size a database is created at.
///
/// The engine's own default, which Phase 1 fixed after measuring 16/32/64. A
/// connection that chose its own would be making files unlike the ones every
/// measurement was taken on.
pub const PAGE_SIZE: usize = 32_768;

/// An open database file.
pub struct Database {
    /// The engine, behind a cell because a statement takes `&mut` and a caller
    /// holds the database by shared reference - which is the same arrangement
    /// `inillucent-session` uses and for the same reason.
    engine: RefCell<ImportedDatabase>,
    /// The file, kept so it can be reported.
    path: PathBuf,
}

impl Database {
    /// Opens a database, creating it when the path holds nothing.
    ///
    /// @param path - the database file
    pub fn open(path: impl AsRef<Path>) -> DbResult<Database> {
        Database::open_with(path, DEFAULT_FRAMES)
    }

    /// Opens a database with the pool size stated.
    ///
    /// @param path - the database file
    /// @param frames - how many frames the buffer pool holds
    pub fn open_with(path: impl AsRef<Path>, frames: usize) -> DbResult<Database> {
        let path = path.as_ref().to_path_buf();
        let engine = if path.is_file() {
            ImportedDatabase::open(path.clone(), PAGE_SIZE, frames)?
        } else {
            ImportedDatabase::create(path.clone(), PAGE_SIZE, frames)?
        };
        Ok(Database {
            engine: RefCell::new(engine),
            path,
        })
    }

    /// Returns a connection to this database.
    pub fn connect(&self) -> Connection<'_> {
        Connection { database: self }
    }

    /// Returns the file this database is in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Makes everything written so far durable in the file.
    ///
    /// A database that is dropped without this is not lost - `open` replays the
    /// log - but a checkpoint is what makes the next open cheap.
    pub fn checkpoint(&self) -> DbResult<()> {
        self.engine.borrow_mut().checkpoint()
    }

    /// Checks every tree's structure.
    pub fn check(&self) -> DbResult<()> {
        self.engine.borrow().check_trees()
    }
}

/// A connection to a database.
///
/// It borrows the database rather than owning a handle of its own, because this
/// engine is single threaded and one file is one pool: two connections that each
/// held a pool over one file would be two page caches over one set of bytes.
pub struct Connection<'d> {
    database: &'d Database,
}

impl Connection<'_> {
    /// Runs one or more statements for their effect.
    ///
    /// @param sql - the statements, separated by semicolons
    pub fn execute_batch(&self, sql: &str) -> DbResult<()> {
        for statement in split_statements(sql) {
            self.database
                .engine
                .borrow_mut()
                .execute_any(&statement, &Params::new())?;
        }
        Ok(())
    }

    /// Runs one statement and returns its rows.
    ///
    /// @param sql - the statement
    pub fn query(&self, sql: &str) -> DbResult<Vec<Vec<OwnedDatum>>> {
        Ok(self
            .database
            .engine
            .borrow_mut()
            .execute_any(sql, &Params::new())?
            .rows)
    }

    /// Runs one statement with bound parameters and returns its rows.
    ///
    /// @param sql - the statement
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn query_with(&self, sql: &str, params: &Params) -> DbResult<Vec<Vec<OwnedDatum>>> {
        Ok(self
            .database
            .engine
            .borrow_mut()
            .execute_any(sql, params)?
            .rows)
    }
}

/// Splits a batch into statements on semicolons outside quotes.
///
/// **Small on purpose, and not a parser.** The engine has one, and a batch that
/// needs it should go through `parse_next_statement` rather than through this.
/// What this handles is the shape a caller writes in a test or a schema file:
/// statements separated by semicolons, with semicolons inside string literals
/// and identifiers left alone. A statement it splits wrongly fails to parse and
/// says so, rather than doing something unintended.
///
/// @param sql - the batch
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for character in sql.chars() {
        match quote {
            Some(open) => {
                current.push(character);
                if character == open {
                    quote = None;
                }
            }
            None => match character {
                '\'' | '"' | '`' => {
                    quote = Some(character);
                    current.push(character);
                }
                ';' => {
                    if !current.trim().is_empty() {
                        out.push(current.trim().to_string());
                    }
                    current.clear();
                }
                _ => current.push(character),
            },
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batch splits on the semicolons that separate statements and no others.
    #[test]
    fn a_batch_splits_on_statement_semicolons() {
        let split = split_statements(
            "CREATE TABLE t (a TEXT); INSERT INTO t VALUES ('one;two'); SELECT * FROM t",
        );
        assert_eq!(
            split,
            vec![
                "CREATE TABLE t (a TEXT)".to_string(),
                "INSERT INTO t VALUES ('one;two')".to_string(),
                "SELECT * FROM t".to_string(),
            ],
            "a semicolon inside a literal is not a separator"
        );
    }

    /// A trailing semicolon and blank statements produce nothing extra.
    #[test]
    fn a_trailing_semicolon_is_not_a_statement() {
        assert_eq!(
            split_statements("SELECT 1;;  ;"),
            vec!["SELECT 1".to_string()],
            "empty statements are dropped"
        );
    }
}
