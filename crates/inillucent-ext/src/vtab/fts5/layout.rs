//! Which index layout wrote this table, and what a reader does when it is one
//! this build has not got.
//!
//! Invariant: **a build that cannot read an index refuses by name rather than
//! answering no rows.** An empty result set is a legitimate answer to a search,
//! so an index the reader does not understand is the one failure an application
//! cannot tell from a correct answer - nothing in the response says the index
//! was written by a build that knew more than this one.
//!
//! That is what happened, and it is why this module exists. The FTS5 index
//! layout changed in 0.1.2: `%_idx`'s third column used to hold an integer
//! naming the `%_data` row a term's doclist lived in, and now holds the doclist
//! itself. 0.1.1 reads a file a later build wrote and answers `count(*)` over
//! `note_fts` as five, `SELECT rowid, title` as all five rows, and
//! `WHERE note_fts MATCH 'segment'` as **no rows at all** - because it read the
//! blob as a page number, found no such page, and a term with no doclist is a
//! term in no documents. 0.1.1 is published and its answer can never be fixed.
//! What can be fixed is the next one, and this record is how: from this build
//! on, an index says which layout wrote it and which release wrote that.
//!
//! ## What is written, and where
//!
//! One `%_data` row, at `LAYOUT_ROW`, holding `MAGIC`, the layout number, and
//! the version string of the build that stamped it. `%_data` rather than
//! `%_config`, because `%_config` is a table an application reads and SQLite
//! writes exactly one row into - `version` - so a second key there would be a
//! row a reader would find and SQLite would not have written. `%_data` is
//! already this engine's own and says so in [`super`]'s doc comment.
//!
//! ## When it is written, and why not more often
//!
//! At `CREATE VIRTUAL TABLE`, and again whenever `rebuild` or `delete-all`
//! empties the index - the two places the whole index is written from scratch.
//! An ordinary insert into an index that has no record does **not** stamp one,
//! and that is deliberate: a file written by 0.1.2 through 0.1.7 has no record
//! and is read by the per-row rule in [`super::term_value`], which is correct
//! for it. Stamping one on the next write would claim the whole index is in
//! this build's layout when half of its rows might still be in the older one,
//! which is a claim the file cannot support.
//!
//! ## `rebuild` is refused too, and that is not an oversight
//!
//! The check is in `Fts5Table::update`, and an FTS5 command is written as an
//! insert into the table's own hidden column, so `rebuild` on an index in a
//! later layout is refused along with everything else - even though
//! `%_content` still holds the documents and a rebuild could in principle
//! derive a readable index from them. That is the conservative answer on
//! purpose: a rebuild here would silently throw away an index a newer build
//! wrote, on a file the two builds may be sharing. An application that
//! genuinely wants this build's index drops the table and creates it again,
//! which says so.
//!
//! ## What a missing record means
//!
//! "Some layout up to and including this build's." It is not an error and
//! never becomes one: every file published before this change has no record,
//! and a reader that refused them would refuse every database in existence.
//! The record only ever answers the other question - whether the index is
//! **newer** than the reader.

use inillucent_base::{DbError, DbResult};
use inillucent_value::Value;

use super::super::Context;
use crate::shadow::ShadowTables;
use crate::vtab::failure;

use super::index::Buffer;

/// The eight bytes a layout record begins with.
///
/// **A magic rather than a bare number, because the row can be overwritten by
/// a build that does not know it is there.** An index this build created is
/// handed to 0.1.1, which allocates `%_data` rows for doclists from the low
/// numbers up; one of them can land on `LAYOUT_ROW` and replace the record with
/// postings. A reader that decoded the first four bytes of that as a layout
/// number would refuse a file it can read perfectly well. With the magic, a row
/// that is not a layout record reads as no record at all, which is the right
/// answer for an index an older build has written to.
const MAGIC: [u8; 8] = *b"RDBFTS5\0";

/// The `%_data` row the layout record lives in.
///
/// Row 1 is the totals. Row 2 is free in every index this build creates,
/// because this build writes no other `%_data` row at all - a term's doclist
/// is in its `%_idx` row. It is not free in a file 0.1.1 wrote, where it may
/// hold a doclist; that is what `MAGIC` is for.
const LAYOUT_ROW: i64 = 2;

/// The index layout this build writes.
///
/// 1 was the two-table form: `%_idx` named a `%_data` row and that row held the
/// doclist. 2 is the merged form task-1911 introduced, where the `%_idx` row
/// carries the doclist itself. This build writes 2 and reads both.
pub(crate) const LAYOUT: u32 = 2;

/// What a layout record says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    /// The layout the index is in.
    pub(crate) layout: u32,
    /// The version of the build that stamped it, as it named itself.
    pub(crate) writer: String,
}

/// Returns the bytes a layout record for this build holds.
///
/// The version string is the writing crate's own, so the refusal an older build
/// prints names a release a reader can go and install rather than a number only
/// this repository can interpret.
fn encode() -> Vec<u8> {
    let writer = env!("CARGO_PKG_VERSION").as_bytes();
    let mut out = Vec::with_capacity(MAGIC.len().saturating_add(6).saturating_add(writer.len()));
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&LAYOUT.to_le_bytes());
    // A length rather than "the rest of the row", so a field added after the
    // version string is readable by a build written before it was added.
    let width = u16::try_from(writer.len()).unwrap_or(0);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(writer.get(..usize::from(width)).unwrap_or_default());
    out
}

/// Reads a layout record back, or `None` when the bytes are not one.
///
/// **Every failure answers `None` rather than an error**, because the only way
/// bytes that are not a record get here is that something else is using the
/// row - an older build's doclist - and that is a file this build reads
/// correctly. An error would turn a readable index into an unopenable one.
///
/// @param bytes - the `%_data` row's block
fn decode(bytes: &[u8]) -> Option<Stamp> {
    if bytes.get(..MAGIC.len())? != MAGIC {
        return None;
    }
    let at = MAGIC.len();
    let mut four = [0u8; 4];
    four.copy_from_slice(bytes.get(at..at.checked_add(4)?)?);
    let layout = u32::from_le_bytes(four);
    let at = at.checked_add(4)?;
    let mut two = [0u8; 2];
    two.copy_from_slice(bytes.get(at..at.checked_add(2)?)?);
    let width = usize::from(u16::from_le_bytes(two));
    let at = at.checked_add(2)?;
    let writer = bytes.get(at..at.checked_add(width)?)?;
    Some(Stamp {
        layout,
        writer: String::from_utf8_lossy(writer).into_owned(),
    })
}

/// Writes the layout record for this build.
///
/// Called where the whole index is written from scratch and nowhere else; see
/// this module's own doc comment for why an ordinary write does not stamp one.
///
/// @param context - the running statement
/// @param shadows - the table's shadow tables
pub(crate) fn stamp(context: &mut Context<'_>, shadows: &ShadowTables) -> DbResult<()> {
    shadows.write_row(
        context,
        b"data",
        LAYOUT_ROW,
        &[Value::Null, Value::owned_blob(&encode())?],
    )
}

/// Returns whether a `%_data` rowid is the layout record's.
///
/// `rebuild` and `delete-all` throw every `%_data` row away except the totals,
/// and the layout record is the second row that has to survive that.
///
/// @param rowid - the `%_data` row being considered
pub(crate) fn is_layout_row(rowid: i64) -> bool {
    rowid == LAYOUT_ROW
}

/// Reads the layout record, when the index carries one.
///
/// @param context - the running statement
/// @param shadows - the table's shadow tables
fn read(context: &mut Context<'_>, shadows: &ShadowTables) -> DbResult<Option<Stamp>> {
    let Some(row) = shadows.read_row(context, b"data", LAYOUT_ROW)? else {
        return Ok(None);
    };
    let Some(block) = row.get(1).and_then(Value::as_blob) else {
        return Ok(None);
    };
    Ok(decode(block.raw()))
}

/// Returns the refusal an index this build cannot read reports.
///
/// **`unsupported`, not corruption.** An index a later build wrote is perfectly
/// well formed; it is this build that is behind. The status is the one every
/// other "this engine has not built that" gives, so the command line exits 3
/// and a driver reports `unsupported` - which is what lets an application tell
/// "upgrade and try again" from "your query is wrong", and either of those from
/// "there are no matching documents".
///
/// @param table - the virtual table's name, as the caller wrote it
/// @param stamp - what the index's own record says
fn refuse(table: &str, stamp: &Stamp) -> DbError {
    let Stamp { layout, writer } = stamp;
    let said = format!(
        "the full-text index on {table} is in layout {layout}, written by inillucent {writer}, \
         and this build reads layouts up to {LAYOUT}"
    );
    failure(format!("{said}; upgrade inillucent to search it"))
        .with_message(said)
        .with_unsupported(format!("a full-text index in layout {layout}"))
}

/// Refuses when the index is in a layout this build does not read.
///
/// **The file is read once per connection, not once per query.** The answer
/// cannot change while this connection holds the file open without another
/// build writing to it at the same time, and asking costs a tree descent -
/// which a plain scan of `%_content` does not otherwise pay. The answer is kept
/// in the buffer the table shares with its cursors, which is the one piece of
/// state both sides already hold. The write path keeps a flag of its own on the
/// table as well, because it calls this per row and even the cached answer
/// costs the buffer's lock.
///
/// @param context - the running statement
/// @param shadows - the table's shadow tables
/// @param buffer - the doclists and totals this connection has staged
/// @param table - the virtual table's name, for the refusal to name
pub(crate) fn readable(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    table: &[u8],
) -> DbResult<()> {
    if let Ok(held) = buffer.lock() {
        if let Some(known) = held.layout {
            // **The readable case allocates nothing.** This runs once per row
            // of a bulk insert, and building a `Stamp` to answer it would copy
            // the release string per row for a question whose answer is a
            // comparison of two numbers. The stamp is built only to refuse.
            if known <= LAYOUT {
                return Ok(());
            }
            let stamp = Stamp {
                layout: known,
                writer: held.layout_writer.clone(),
            };
            drop(held);
            return verdict(&stamp, table);
        }
    }
    let stamp = stamp_of(context, shadows)?;
    if let Ok(mut held) = buffer.lock() {
        held.layout = Some(stamp.layout);
        held.layout_writer.clone_from(&stamp.writer);
    }
    verdict(&stamp, table)
}

/// The same check, for a reader that has no buffer to cache the answer in.
///
/// `fts5vocab` reaches into another table's shadows and holds none of its
/// state, so it pays the read per scan. A scan of the whole dictionary is what
/// it does anyway, and one more row is not what that costs.
///
/// @param context - the running statement
/// @param shadows - the target index's shadow tables
/// @param table - the index's name, for the refusal to name
pub(crate) fn readable_once(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    table: &[u8],
) -> DbResult<()> {
    let stamp = stamp_of(context, shadows)?;
    verdict(&stamp, table)
}

/// Returns what the index says about itself, or what a file with no record
/// means.
///
/// A missing record is every file published before this change, and it is read
/// by the per-row rule rather than refused - so it reads back as this build's
/// own layout, with no release named because none was recorded.
///
/// @param context - the running statement
/// @param shadows - the table's shadow tables
fn stamp_of(context: &mut Context<'_>, shadows: &ShadowTables) -> DbResult<Stamp> {
    Ok(read(context, shadows)?.unwrap_or(Stamp {
        layout: LAYOUT,
        writer: String::new(),
    }))
}

/// Returns whether an index carrying this record may be read.
///
/// @param stamp - what the index's own record says
/// @param table - the virtual table's name, for the refusal to name
fn verdict(stamp: &Stamp, table: &[u8]) -> DbResult<()> {
    match stamp.layout > LAYOUT {
        true => Err(refuse(&String::from_utf8_lossy(table), stamp)),
        false => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record this build writes reads back as this build's layout.
    #[test]
    fn a_record_round_trips() {
        let read = decode(&encode()).expect("the bytes this build writes are a layout record");
        assert_eq!(
            read.layout, LAYOUT,
            "the layout is the one this build writes"
        );
        assert_eq!(
            read.writer,
            env!("CARGO_PKG_VERSION"),
            "the record names the release that wrote it, so a refusal can too"
        );
    }

    /// Bytes that are not a record read as no record, rather than as a layout.
    ///
    /// The case this exists for is an older build allocating the layout row for
    /// a doclist: the row then holds postings, and a reader that took the first
    /// four bytes of those as a layout number would refuse an index it reads
    /// perfectly well. A doclist is varints, so its first byte is small.
    #[test]
    fn postings_in_the_layout_row_are_not_a_layout() {
        assert_eq!(decode(&[1, 2, 0, 3, 4, 0, 5, 6, 7, 8]), None);
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&MAGIC), None, "a magic with no layout after it");
    }

    /// A record from a later build reads back the layout and the release.
    ///
    /// Written by hand rather than by `encode`, because `encode` can only
    /// produce this build's own number and the case is about a number it
    /// cannot produce.
    #[test]
    fn a_later_layout_reads_back_with_the_release_that_wrote_it() {
        let read = decode(&later_record(99, "9.9.9")).expect("a later record still decodes");
        assert_eq!(read.layout, 99);
        assert_eq!(read.writer, "9.9.9");
    }

    /// Returns a layout record naming a build that does not exist.
    ///
    /// @param layout - the layout number to claim
    /// @param writer - the release to claim wrote it
    fn later_record(layout: u32, writer: &str) -> Vec<u8> {
        let mut bytes = Vec::from(MAGIC);
        bytes.extend_from_slice(&layout.to_le_bytes());
        bytes.extend_from_slice(&(writer.len() as u16).to_le_bytes());
        bytes.extend_from_slice(writer.as_bytes());
        bytes
    }

    /// The refusal names the layout, the release and the status.
    #[test]
    fn the_refusal_says_what_to_do_about_it() {
        let error = refuse(
            "note_fts",
            &Stamp {
                layout: 99,
                writer: "9.9.9".to_string(),
            },
        );
        assert_eq!(
            error.unsupported(),
            Some("a full-text index in layout 99"),
            "a caller asking the status gets `unsupported`, not a parse of the message"
        );
        let message = error.message();
        assert!(message.contains("note_fts"), "{message}");
        assert!(message.contains("layout 99"), "{message}");
        assert!(message.contains("9.9.9"), "{message}");
    }
}
