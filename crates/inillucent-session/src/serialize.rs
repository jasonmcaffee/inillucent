//! A database as a byte string, and a byte string as a database.
//!
//! Invariant: what comes out is the file. Not a rebuilt copy, not a dump of
//! rows - the pages, in order, exactly as they are on disk, so that writing
//! the bytes to a path produces a database any engine can open and so that a
//! serialize-deserialize round trip is the identity.
//!
//! The pair exists for the same reason SQLite's does: a database that lives
//! somewhere that is not a file system - inside another database, in a message,
//! in a test fixture - still wants to be a database while it is being used.

use std::sync::Arc;

use inillucent_base::error::misuse;
use inillucent_base::ids::PageId;
use inillucent_base::{page, DbResult};
use inillucent_vfs::{DbPath, FileKind, MemoryVfs, OpenOptions, Vfs};

use crate::connection::{
    Access, Connection, OpenOptions as SessionOptions, Outcome, SessionDatabase,
};

/// Returns one database's pages, in order, as the file holds them.
///
/// It is read inside one transaction, so the bytes are one snapshot rather
/// than a walk that a writer moved underneath.
pub fn serialize(connection: &Connection, database: usize) -> DbResult<Vec<u8>> {
    connection.begin_statement(Access::Read)?;
    let outcome = serialize_inside(connection, database);
    let ending = if outcome.is_ok() {
        Outcome::Done
    } else {
        Outcome::Abort
    };
    let closed = connection.end_statement(Access::Read, ending);
    let bytes = outcome?;
    closed?;
    Ok(bytes)
}

/// Reads every page with the transaction already open.
fn serialize_inside(connection: &Connection, database: usize) -> DbResult<Vec<u8>> {
    let (page_count, page_size) = connection.with_database(database, |pager| {
        (pager.page_count(), pager.page_size().as_usize())
    })?;
    let mut bytes = Vec::with_capacity(page_count as usize * page_size);
    for number in 1..=page_count {
        connection.with_database(database, |pager| {
            let id = PageId::from_persisted(number)?;
            let pin = pager.get_page(id)?;
            bytes.extend_from_slice(pin.bytes());
            Ok::<(), inillucent_base::DbError>(())
        })??;
    }
    Ok(bytes)
}

/// A database opened from bytes rather than from a path.
///
/// It owns the memory the pages live in, so dropping it is what frees them -
/// which is why it is a value the caller keeps rather than a connection that
/// borrows from somewhere.
pub struct Deserialized {
    database: SessionDatabase,
}

impl Deserialized {
    /// Opens a database from the bytes of one.
    ///
    /// The bytes are checked by being opened: a length that is not a whole
    /// number of pages, or a header that does not decode, is refused here
    /// rather than at the first query.
    pub fn open(bytes: &[u8], options: SessionOptions) -> DbResult<Deserialized> {
        if bytes.len() < page::HEADER_SIZE as usize {
            return Err(misuse(
                "a database is at least one page and this is shorter than its header",
            ));
        }
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let path = DbPath::new(std::path::PathBuf::from("/deserialized.db"));
        {
            let file = vfs.open(&path, OpenOptions::of_kind(FileKind::MainDb))?;
            file.write_all_at(0, bytes)?;
        }
        let database = SessionDatabase::open_with(path.as_path(), vfs, options)?;
        // Connecting is what validates the header and runs recovery, so it is
        // done here: a `Deserialized` that cannot be connected to is a failure
        // the caller wants now rather than later.
        let connection = database.connect()?;
        drop(connection);
        Ok(Deserialized { database })
    }

    /// Opens a connection onto the deserialized database.
    pub fn connect(&self) -> DbResult<Connection> {
        self.database.connect()
    }

    /// Returns the database, for a caller that wants to hold it.
    pub fn database(&self) -> &SessionDatabase {
        &self.database
    }

    /// Hands the database over, for a caller that is going to keep it.
    ///
    /// The memory the pages live in is owned by the file system inside it, so
    /// moving the database moves the bytes with it.
    pub fn into_database(self) -> SessionDatabase {
        self.database
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a small database and returns its bytes.
    fn built() -> Vec<u8> {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let database = SessionDatabase::open_with(
            std::path::PathBuf::from("/source.db"),
            vfs,
            SessionOptions::default(),
        )
        .expect("the database opens");
        let connection = database.connect().expect("the connection opens");
        crate::statement::execute_batch(
            &connection,
            b"CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); INSERT INTO t VALUES (1,'one'),(2,'two')",
        )
        .expect("the rows are written");
        serialize(&connection, 0).expect("the database serialises")
    }

    /// The bytes are a whole number of pages and open again as a database with
    /// the same rows in it.
    #[test]
    fn a_database_round_trips_through_its_bytes() {
        let bytes = built();
        assert!(!bytes.is_empty());
        assert_eq!(bytes.len() % 4096, 0, "the bytes are not whole pages");
        let opened = Deserialized::open(&bytes, SessionOptions::default())
            .expect("the bytes open as a database");
        let connection = opened.connect().expect("the connection opens");
        let (mut statement, _) =
            crate::statement::Statement::prepare(&connection, b"SELECT b FROM t ORDER BY a")
                .expect("the query prepares");
        let mut rows = Vec::new();
        while statement.step().expect("the query steps") {
            let row = statement.row();
            rows.push(
                row.first()
                    .and_then(|value| value.as_text().map(|text| text.utf8_bytes().into_owned()))
                    .unwrap_or_default(),
            );
        }
        assert_eq!(rows, vec![b"one".to_vec(), b"two".to_vec()]);
    }

    /// Serialising twice with nothing in between produces the same bytes, and
    /// a write moves them.
    #[test]
    fn the_bytes_are_the_file() {
        let first = built();
        let second = built();
        assert_eq!(
            first, second,
            "two identical databases serialised differently"
        );
        let opened = Deserialized::open(&first, SessionOptions::default()).expect("the bytes open");
        let connection = opened.connect().expect("the connection opens");
        crate::statement::execute_batch(&connection, b"INSERT INTO t VALUES (3,'three')")
            .expect("the row is written");
        let after = serialize(&connection, 0).expect("it serialises again");
        assert_ne!(first, after, "a write did not change the bytes");
    }

    /// Bytes that are not a database are refused rather than opened.
    #[test]
    fn a_string_that_is_not_a_database_is_refused() {
        assert!(Deserialized::open(b"", SessionOptions::default()).is_err());
        assert!(Deserialized::open(b"not a database", SessionOptions::default()).is_err());
        let mut damaged = built();
        if let Some(byte) = damaged.get_mut(0) {
            *byte ^= 0xff;
        }
        assert!(Deserialized::open(&damaged, SessionOptions::default()).is_err());
    }
}
