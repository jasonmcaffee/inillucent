//! A handle on one value, read and written a range at a time.
//!
//! Invariant: a blob handle never changes the length of the value it is open
//! on. That is what makes it incremental: the record's shape is fixed, so the
//! bytes of the value keep their places, and writing one of them is writing one
//! page rather than reading the row, patching it, and writing it back. A write
//! that would run past the end is refused instead of growing anything.
//!
//! The handle holds no cursor between calls. It remembers where the value is -
//! which page holds the cell, where in it the local part starts, and where the
//! overflow chain begins - and finds the range again on each call. A cursor
//! kept open across calls would have to survive every write any other statement
//! made in between, and the position it would be restored to is exactly what is
//! recomputed here.
//!
//! Reference: <https://sqlite.org/c3ref/blob_open.html>.

use inillucent_base::error::{misuse, DbError};
use inillucent_base::ids::PageId;
use inillucent_base::{DbResult, PrimaryCode};
use inillucent_storage::overflow::{self, PayloadPlace};
use inillucent_storage::{BTreeCursor, SeekBias};
use inillucent_value::record;

use crate::connection::{Access, Connection, Outcome};

/// An open handle on one value of one row.
pub struct Blob<'a> {
    connection: &'a Connection,
    database: usize,
    root: PageId,
    /// The record slot the value occupies, which is not always the column's
    /// declared position: a `WITHOUT ROWID` table permutes its record.
    slot: usize,
    rowid: i64,
    writable: bool,
    len: u32,
}

impl<'a> Blob<'a> {
    /// Opens a handle on one column of one row.
    ///
    /// The row is looked up once, here, and its length is fixed from that
    /// moment: a handle whose row is later deleted or whose value is later
    /// replaced by a shorter one reports `SQLITE_ABORT` rather than reading
    /// whatever is now in those bytes.
    pub fn open(
        connection: &'a Connection,
        database: &[u8],
        table: &[u8],
        column: &[u8],
        rowid: i64,
        writable: bool,
    ) -> DbResult<Blob<'a>> {
        use inillucent_sql::catalog_view::CatalogView;
        let catalog = connection.catalog()?;
        let folded = table.to_ascii_lowercase();
        let scope = (!database.is_empty()).then(|| database.to_ascii_lowercase());
        let Some(info) = catalog.find_table(scope.as_deref(), &folded) else {
            return Err(misuse(format!(
                "no such table: {}",
                String::from_utf8_lossy(table)
            )));
        };
        if info.without_rowid {
            return Err(misuse(
                "a blob handle needs a rowid, and this table has none",
            ));
        }
        let Some(position) = info.column_position(&column.to_ascii_lowercase()) else {
            return Err(misuse(format!(
                "no such column: {}",
                String::from_utf8_lossy(column)
            )));
        };
        let root = PageId::from_persisted(info.root)?;
        let slot = info.record_slot(position).unwrap_or(usize::from(position));
        let mut blob = Blob {
            connection,
            database: info.database,
            root,
            slot,
            rowid,
            writable,
            len: 0,
        };
        blob.len = blob.measure()?;
        Ok(blob)
    }

    /// Returns the value's length in bytes.
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Reports whether the value is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Points the handle at the same column of a different row.
    ///
    /// It is the operation the interface exists for: walking a table's blobs
    /// costs one lookup each rather than one handle each.
    pub fn reopen(&mut self, rowid: i64) -> DbResult<()> {
        self.rowid = rowid;
        self.len = self.measure()?;
        Ok(())
    }

    /// Copies bytes out of the value.
    pub fn read_at(&self, offset: u32, output: &mut [u8]) -> DbResult<()> {
        self.check_range(offset, output.len())?;
        self.connection.begin_statement(Access::Read)?;
        let outcome = self.with_place(|pager, value| {
            overflow::read_range(
                pager,
                value,
                value.start().saturating_add(u64::from(offset)),
                output,
            )
        });
        let closed = self.connection.end_statement(
            Access::Read,
            if outcome.is_ok() {
                Outcome::Done
            } else {
                Outcome::Abort
            },
        );
        outcome?;
        closed
    }

    /// Writes bytes over the value, which cannot change its length.
    pub fn write_at(&self, offset: u32, input: &[u8]) -> DbResult<()> {
        if !self.writable {
            return Err(DbError::primary(PrimaryCode::ReadOnly)
                .with_message("attempt to write a readonly database")
                .with_detail("this blob handle was opened for reading"));
        }
        self.check_range(offset, input.len())?;
        self.connection
            .begin_statement_on(Access::Schema, &[self.database])?;
        let outcome = self.with_place(|pager, value| {
            overflow::write_range(
                pager,
                value,
                value.start().saturating_add(u64::from(offset)),
                input,
            )
        });
        let closed = self.connection.end_statement(
            Access::Schema,
            if outcome.is_ok() {
                Outcome::Done
            } else {
                Outcome::Abort
            },
        );
        outcome?;
        closed
    }

    /// Refuses a range that runs past the value.
    ///
    /// SQLite reports `SQLITE_ERROR` for this rather than growing the value or
    /// returning a short read, and so does this: a blob handle is a window on
    /// bytes that exist.
    fn check_range(&self, offset: u32, len: usize) -> DbResult<()> {
        let end = u64::from(offset).saturating_add(len as u64);
        if end > u64::from(self.len) {
            return Err(misuse(format!(
                "bytes {offset}..{end} of a value of {} bytes",
                self.len
            )));
        }
        Ok(())
    }

    /// Reads the value's length by finding the row.
    fn measure(&self) -> DbResult<u32> {
        self.connection.begin_statement(Access::Read)?;
        let outcome = self.with_place(|_, place| Ok(place.value_len));
        let closed = self.connection.end_statement(
            Access::Read,
            if outcome.is_ok() {
                Outcome::Done
            } else {
                Outcome::Abort
            },
        );
        let len = outcome?;
        closed?;
        Ok(len)
    }

    /// Runs a closure with the value's place in the file.
    ///
    /// The row is found again on every call. That is the cost of not holding a
    /// cursor open, and it is the same cost SQLite pays when it restores one:
    /// a seek by rowid, which is a descent of the tree.
    fn with_place<T>(
        &self,
        body: impl FnOnce(&mut inillucent_storage::Pager, &ValuePlace) -> DbResult<T>,
    ) -> DbResult<T> {
        self.connection.with_database(self.database, |pager| {
            let mut cursor = BTreeCursor::table(self.root);
            if !cursor.seek_rowid(pager, self.rowid, SeekBias::AtOrAfter)?
                || cursor.rowid()? != self.rowid
            {
                return Err(DbError::primary(PrimaryCode::Abort)
                    .with_message("no such rowid")
                    .with_detail("the row this blob handle was opened on is gone"));
            }
            let place = cursor.payload_place()?;
            let value = value_place(pager, &place, self.slot)?;
            body(pager, &value)
        })?
    }
}

/// Where one value of a record lives, and how long it is.
pub struct ValuePlace {
    /// The record's place in the file.
    place: PayloadPlace,
    /// Where the value starts within the record.
    start: u64,
    /// How long it is.
    value_len: u32,
}

impl core::ops::Deref for ValuePlace {
    type Target = PayloadPlace;

    /// A value's place is its record's place, narrowed to one range.
    fn deref(&self) -> &PayloadPlace {
        &self.place
    }
}

/// Returns where one slot of a record starts and how long it is.
///
/// Only the record's *header* is read, which is the point: the header is a few
/// bytes at the front of the payload and says how long every value is, so the
/// value itself is never touched to find out where it begins.
fn value_place(
    pager: &mut inillucent_storage::Pager,
    place: &PayloadPlace,
    slot: usize,
) -> DbResult<ValuePlace> {
    // The header's own length is a varint at the very front, so the first nine
    // bytes are always enough to learn how much of the header to read.
    let prefix = u64::from(9u32).min(place.total);
    let mut lead = vec![0u8; usize::try_from(prefix).unwrap_or(0)];
    overflow::read_range(pager, place, 0, &mut lead)?;
    let header_len = record::header_length(&lead)?.min(place.total);
    let mut header = vec![0u8; usize::try_from(header_len).unwrap_or(0)];
    overflow::read_range(pager, place, 0, &mut header)?;
    let (start, len) = record::field_extent(&header, slot)?;
    Ok(ValuePlace {
        place: *place,
        start,
        value_len: u32::try_from(len).unwrap_or(u32::MAX),
    })
}

impl ValuePlace {
    /// Returns where the value starts within the record.
    pub fn start(&self) -> u64 {
        self.start
    }
}
