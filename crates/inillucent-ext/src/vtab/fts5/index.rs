//! The pending buffer, the doclists in it, and the index's own totals.
//!
//! Invariant: **one doclist per term, and it is rewritten rather than
//! segmented.** There are no segments to merge, which is why the merge
//! commands are accepted and do nothing: a term's postings are one blob, and
//! an append rewrites it under one lock.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use inillucent_base::DbResult;
use inillucent_value::Value;

use super::super::Context;
use crate::shadow::ShadowTables;

use super::doclist::{
    append_doclist_entry, last_doclist_rowid, read_varint, write_varint, DocEntry,
};
use super::*;

/// Where an FTS5 index build spends its time, in nanoseconds.
///
/// **Measured rather than reasoned about**, which is the rule Phase 3's Part E
/// states for the read families and which applies here: the build path could be
/// tokenisation, the per-row shadow write, the doclist merge or the same
/// allocation-per-row shape found earlier in the index build, and guessing
/// which has been wrong twice on this project.
///
/// It is kept the way `create_index`'s stage timing is kept - permanently, and
/// read by the gate beside the ratio - because a workload under a bar needs its
/// breakdown printed beside the number, not reconstructed by a later ticket
/// from a profile nobody kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildStages {
    /// How many rows were indexed.
    pub rows: u64,
    /// Writing the row into `%_content`.
    pub content: u128,
    /// Turning the row's text into tokens.
    pub tokenize: u128,
    /// Encoding the per-column token counts and writing `%_docsize`.
    pub docsize: u128,
    /// Sorting the postings and grouping them by term, without the merge.
    pub group: u128,
    /// Adding this document to a term's doclist, for a term this transaction
    /// has already seen.
    pub terms: u128,
    /// The same, for a term this transaction has not seen before: finding or
    /// creating its `%_idx` row, and reading its doclist back.
    pub new_terms: u128,
    /// How many terms took that path.
    pub new_term_count: u64,
    /// Looking a term up in the `%_idx` dictionary.
    pub dictionary_read: u128,
    /// Writing a new term's `%_idx` row.
    pub dictionary_write: u128,
    /// Reading, updating and staging the corpus totals `bm25` needs.
    pub totals: u128,
    /// Writing the staged doclists out to `%_data`.
    pub flush: u128,
}
/// Adds one measurement to this thread's tally.
///
/// @param edit - what to add
pub(crate) fn record_stage(edit: impl FnOnce(&mut BuildStages)) {
    BUILD_STAGES.with(|held| {
        let mut stages = held.get();
        edit(&mut stages);
        held.set(stages);
    });
}
/// Returns where the FTS5 builds on this thread have spent their time.
pub fn build_stages() -> BuildStages {
    BUILD_STAGES.with(|held| held.get())
}
/// Forgets what this thread's FTS5 builds have spent, so the next is on its own.
pub fn reset_build_stages() {
    BUILD_STAGES.with(|held| held.set(BuildStages::default()));
}
/// The totals `bm25` needs: how many rows, and how many tokens per column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Totals {
    /// How many rows the index holds.
    pub rows: i64,
    /// How many tokens each column holds across every row.
    pub tokens: Vec<i64>,
}
impl Totals {
    /// Returns the totals of an empty index.
    pub(crate) fn empty(columns: usize) -> Totals {
        Totals {
            rows: 0,
            tokens: vec![0; columns],
        }
    }

    /// Returns the totals as the bytes `%_data` row one holds.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint(&mut out, self.rows as u64);
        write_varint(&mut out, self.tokens.len() as u64);
        for count in &self.tokens {
            write_varint(&mut out, *count as u64);
        }
        out
    }

    /// Reads the totals back.
    pub(crate) fn decode(bytes: &[u8]) -> Totals {
        let mut at = 0usize;
        let rows = read_varint(bytes, &mut at) as i64;
        let columns = read_varint(bytes, &mut at) as usize;
        let mut tokens = Vec::with_capacity(columns);
        for _ in 0..columns.min(1024) {
            tokens.push(read_varint(bytes, &mut at) as i64);
        }
        Totals { rows, tokens }
    }
}
/// Reads the totals row.
pub(crate) fn get_totals(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    columns: usize,
) -> Totals {
    let Ok(Some(row)) = shadows.read_row(context, b"data", TOTALS) else {
        return Totals::empty(columns);
    };
    let Some(blob) = row.get(1).and_then(Value::as_blob) else {
        return Totals::empty(columns);
    };
    Totals::decode(blob.raw())
}
/// Returns the totals, from the buffer when this transaction has them.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged rows
/// @param columns - how many columns the table has
pub(crate) fn buffered_totals(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    columns: usize,
) -> Totals {
    if let Ok(held) = buffer.lock() {
        if let Some(totals) = held.totals.as_ref() {
            return totals.clone();
        }
    }
    let totals = get_totals(context, shadows, columns);
    if let Ok(mut held) = buffer.lock() {
        held.totals = Some(totals.clone());
    }
    totals
}
/// Stages the totals, to be written when the transaction flushes.
///
/// @param buffer - the staged rows
/// @param totals - the totals as they now stand
pub(crate) fn stage_totals(buffer: &Buffer, totals: Totals) {
    if let Ok(mut held) = buffer.lock() {
        held.totals = Some(totals);
        held.totals_dirty = true;
    }
}
/// Writes the totals row.
pub(crate) fn put_totals(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    totals: &Totals,
) -> DbResult<()> {
    shadows.write_row(
        context,
        b"data",
        TOTALS,
        &[Value::Null, Value::owned_blob(&totals.encode())?],
    )
}
/// The doclists this transaction has changed but has not written yet.
///
/// **A doclist is rewritten once per transaction, not once per document.**
/// `merge_term` reads a term's whole doclist, appends one entry and writes the
/// whole thing back, so a term that appears in every document of a bulk load is
/// read and written once per document and the bytes moved grow with the load:
/// five hundred documents over a ten-word vocabulary moved about ten megabytes
/// to store thirty kilobytes. The entries are the same entries and the row is
/// the same row; only the number of times it is written changes.
///
/// It is shared with the cursors the table opens, so a query inside the same
/// transaction reads what the transaction has written - which is what makes
/// this a buffer rather than a delayed write.
#[derive(Default)]
pub struct Pending {
    /// A term's doclist, keyed by the term itself.
    ///
    /// **The dictionary row and the doclist are the same row now, so the
    /// cache of one is the cache of the other.** A term this transaction has
    /// looked up - to append to it or merely to answer a query - is staged
    /// here whether or not its bytes have changed, which is what lets a
    /// document that repeats a common word skip a second read of `%_idx` for
    /// it: the map holds the one row `%_idx` would otherwise be asked for
    /// again.
    ///
    /// **A `BTreeMap` because the order it iterates in *is* the key order the
    /// flush wants.** `%_idx` is an index tree keyed by the term, and the
    /// terms of a document arrive in whatever order the text put them in - so
    /// writing each as it appears is a descent and a possible split per term,
    /// into a tree that is growing under it. Held here and written in key
    /// order at the flush, the same run of terms is a walk to the right of the
    /// tree instead. It was measured before it was done: `extension.fts.build`
    /// spent 1.8 of its 9.4 ms writing 507 dictionary rows one at a time,
    /// against 0.5 ms reading them and 1.2 ms writing every doclist - and
    /// those two writes are now one.
    ///
    /// **Every reader that scans `%_idx` flushes first.** A staged row is
    /// invisible to a scan, and the scans are `expr.rs`'s prefix search,
    /// `integrity`, `rebuild` and `remove`.
    pub(crate) doclists: BTreeMap<Vec<u8>, Staged>,
    /// How many bytes the doclists hold, so the buffer can be bounded.
    pub(crate) bytes: usize,
    /// The totals row, read once and written once per transaction.
    ///
    /// **One `%_data` row, and it was being rewritten per document.** `add`
    /// finishes by reading row 1, adding this document's counts and writing it
    /// back, so a bulk load of five hundred documents read and wrote the same
    /// row five hundred times - and each of those is a descent, a log record
    /// and a page image on a row that nothing reads until a query asks for a
    /// score. `fts.build` measured 0.10x against SQLite with 22.6 of its 23.7
    /// ms inside the module, and this is the part of it that is pure repetition.
    ///
    /// `None` means "not read yet this transaction", which is different from
    /// "there are no totals": an empty index has a totals row of zeroes.
    totals: Option<Totals>,
    /// Whether the buffered totals differ from what `%_data` holds.
    totals_dirty: bool,
    /// The highest `%_content` rowid this transaction has handed out.
    ///
    /// **Two tree descents per document, for a number the module already
    /// knows.** An insert that names no rowid asked `max_rowid` for one - a
    /// descent to the rightmost leaf - and then asked `read_row` whether that
    /// rowid was taken, which is a second descent for a question whose answer
    /// is no by construction. Together they were half the descents a document
    /// costs, and `fts.build` is five hundred documents in one transaction.
    ///
    /// It is cleared at `sync`, which is the transaction boundary, so a rowid
    /// freed by a delete in a *later* transaction is reused exactly as SQLite
    /// reuses it. Within one transaction the number only rises, which is the
    /// same rule a rowid table follows for rows it has just written.
    pub(crate) content_highest: Option<i64>,
}
/// One staged doclist, and the rowid it currently ends at.
///
/// **The rowid is remembered because finding it costs a walk of the whole
/// list.** A doclist is a run of varints with no length prefix and no index, so
/// the only way to read the last entry is to decode every entry before it -
/// and an append has to know it, because entries are stored as deltas.
///
/// That made appending O(the list so far), and a bulk load appends to the same
/// term once per document: five hundred documents over a ten-word vocabulary
/// walked about a hundred and twenty thousand entries to add five thousand.
/// `merge_term`'s own doc comment records the identical shape being fixed for
/// the *unbuffered* path, by appending instead of decoding; the buffered path
/// it introduced brought the walk back one level down.
#[derive(Default)]
pub(crate) struct Staged {
    /// The doclist as `%_idx` will hold it.
    pub(crate) bytes: Vec<u8>,
    /// The rowid the last entry names, when there is one.
    last: Option<i64>,
}
/// A handle on the buffer, shared by a table and the cursors it opens.
pub type Buffer = Arc<Mutex<Pending>>;
/// How many bytes of doclists are held before the buffer is written out.
///
/// A bound rather than a tuning knob: without one, a bulk load of a large
/// corpus would hold the whole index in memory. Flushing early costs a rewrite
/// of what is held, which is what the unbuffered path paid per document.
pub(crate) const PENDING_BUDGET: usize = 8 * 1024 * 1024;
/// Returns the rowid an insert that named none is given.
///
/// The table is asked once per transaction and the mark rises from there.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged rows
/// @param suffix - the shadow the rows are reached under
pub(crate) fn next_content_rowid(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    suffix: &[u8],
) -> DbResult<i64> {
    if let Ok(mut held) = buffer.lock() {
        if let Some(highest) = held.content_highest {
            let next = highest.saturating_add(1);
            held.content_highest = Some(next);
            return Ok(next);
        }
    }
    let next = shadows.max_rowid(context, suffix)?.saturating_add(1);
    if let Ok(mut held) = buffer.lock() {
        held.content_highest = Some(next);
    }
    Ok(next)
}
/// Records that a rowid the caller named is now in the table.
///
/// @param buffer - the staged rows
/// @param rowid - the rowid that was written
pub(crate) fn note_content_rowid(buffer: &Buffer, rowid: i64) {
    if let Ok(mut held) = buffer.lock() {
        held.content_highest = Some(held.content_highest.unwrap_or(0).max(rowid));
    }
}
/// What a `%_idx` row's third column holds.
///
/// Self-describing rather than versioned - see the module's own doc comment
/// for the argument. A row this build wrote, or has rewritten since a reopen,
/// carries its doclist inline; a row an older build wrote and this build has
/// not touched yet still names a `%_data` page.
pub(crate) enum TermValue {
    /// The doclist, exactly as `%_idx` holds it.
    Inline(Vec<u8>),
    /// The `%_data` row an older build left the doclist in.
    Page(i64),
}
/// Reads a `%_idx` row's third column, telling the two layouts apart by the
/// value's own type: an `Integer` is a page an older build wrote, a `Blob` or
/// `Text` is this build's doclist.
///
/// @param values - one `%_idx` row, as `scan_keyed` or `read_keyed` hands it
pub(crate) fn term_value(values: &[Value<'static>]) -> Option<TermValue> {
    match values.get(2) {
        Some(Value::Integer(page)) => Some(TermValue::Page(*page)),
        Some(Value::Blob(blob)) => Some(TermValue::Inline(blob.raw().to_vec())),
        Some(Value::Text(text)) => Some(TermValue::Inline(text.utf8_bytes().into_owned())),
        _ => None,
    }
}
/// Returns a `%_idx` row's doclist, wherever it actually lives.
///
/// One read for a row this build wrote; one read plus the `%_data` row an
/// older build's indirection still names, for a row it has not touched yet.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param values - the `%_idx` row
pub(crate) fn resolve_doclist(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    values: &[Value<'static>],
) -> DbResult<Option<Vec<u8>>> {
    match term_value(values) {
        Some(TermValue::Inline(bytes)) => Ok(Some(bytes)),
        Some(TermValue::Page(page)) => {
            Ok(shadows.read_row(context, b"data", page)?.and_then(|row| {
                row.get(1)
                    .and_then(Value::as_blob)
                    .map(|blob| blob.raw().to_vec())
            }))
        }
        None => Ok(None),
    }
}
/// Returns a term's doclist, from the buffer when this transaction has
/// staged it, or read from `%_idx` otherwise.
///
/// **A read-only accessor.** Unlike [`term_row`], it never stages what it
/// reads - the query path this serves does not necessarily hold a write
/// transaction to stage into, and does not need to: it asks for a term once
/// per query rather than once per document.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged doclists
/// @param term - the term
pub(crate) fn read_doclist(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    term: &[u8],
) -> DbResult<Option<Vec<u8>>> {
    if let Ok(held) = buffer.lock() {
        if let Some(staged) = held.doclists.get(term) {
            return Ok(Some(staged.bytes.clone()));
        }
    }
    let key = [Value::Integer(SEGMENT), Value::owned_blob(term)?];
    match shadows.read_keyed(context, b"idx", &key, 3)? {
        Some(row) => resolve_doclist(context, shadows, &row),
        None => Ok(None),
    }
}
/// What [`append_in_one_lock`] managed.
pub(crate) enum Appended {
    /// The entry is in the buffer, and whether the buffer is now over budget.
    Done {
        /// Whether the caller should flush.
        full: bool,
    },
    /// Nothing was written; the caller takes the general path.
    No,
}
/// Appends one document's occurrences of one term, under a single lock.
///
/// **The whole of the bulk-load path, in one critical section.** It answers
/// [`Appended::No`] and writes nothing whenever any of its three conditions
/// does not hold - the term has not been looked up in this transaction, its
/// doclist is not staged, or the document does not sort after everything
/// already in it - and the caller then does the general thing, which is the
/// same code that ran before this existed.
///
/// The postings are written straight out of the sorted run, so nothing is
/// allocated to describe them: they arrive sorted by column and then by
/// position, which is exactly the order a doclist entry holds.
///
/// @param buffer - the staged doclists
/// @param term - the term
/// @param rowid - the document
/// @param run - this term's postings in this document
pub(crate) fn append_in_one_lock(
    buffer: &Buffer,
    term: &[u8],
    rowid: i64,
    run: &[(Vec<u8>, usize, u32)],
) -> Appended {
    let Ok(mut held) = buffer.lock() else {
        return Appended::No;
    };
    let Some(staged) = held.doclists.get_mut(term) else {
        return Appended::No;
    };
    let Some(last) = staged.last else {
        return Appended::No;
    };
    if rowid <= last {
        return Appended::No;
    }
    let was = staged.bytes.len();
    append_run(&mut staged.bytes, rowid.wrapping_sub(last), run);
    staged.last = Some(rowid);
    let grew = staged.bytes.len().saturating_sub(was);
    held.bytes = held.bytes.saturating_add(grew);
    Appended::Done {
        full: held.bytes > PENDING_BUDGET,
    }
}
/// Writes one doclist entry straight out of a sorted run of postings.
///
/// The bytes are the ones [`append_doclist_entry`] writes for the same
/// occurrences - the two are checked against each other by
/// `a_run_appends_the_bytes_the_entry_would`, because a doclist written two
/// ways is a format with two definitions.
///
/// @param out - the doclist being appended to
/// @param delta - this document's rowid minus the previous entry's
/// @param run - the postings, sorted by column then position
pub(crate) fn append_run(out: &mut Vec<u8>, delta: i64, run: &[(Vec<u8>, usize, u32)]) {
    write_varint(out, delta as u64);
    // How many distinct columns the run touches, counted without a collection:
    // it is sorted, so a column starts wherever it differs from the one before.
    let mut columns = 0u64;
    let mut previous: Option<usize> = None;
    for (_, column, _) in run {
        if previous != Some(*column) {
            columns = columns.saturating_add(1);
            previous = Some(*column);
        }
    }
    write_varint(out, columns);
    let mut at = 0usize;
    while at < run.len() {
        let Some((_, column, _)) = run.get(at) else {
            break;
        };
        let start = at;
        while matches!(run.get(at), Some((_, held, _)) if held == column) {
            at = at.saturating_add(1);
        }
        write_varint(out, *column as u64);
        write_varint(out, at.saturating_sub(start) as u64);
        let mut last = 0u32;
        for (_, _, position) in run.get(start..at).unwrap_or_default() {
            write_varint(out, u64::from(position.wrapping_sub(last)));
            last = *position;
        }
    }
}
/// Groups a sorted run of postings into the per-column form a `DocEntry` holds.
///
/// Only for the paths that need a whole entry - the first document to hold a
/// term, and one that does not sort last - so the allocation it costs is off
/// the bulk-load path.
///
/// @param run - the postings, sorted by column then position
pub(crate) fn columns_of(run: &[(Vec<u8>, usize, u32)]) -> Vec<(usize, Vec<u32>)> {
    let mut columns: Vec<(usize, Vec<u32>)> = Vec::new();
    for (_, column, position) in run {
        match columns.last_mut() {
            Some((held, positions)) if held == column => positions.push(*position),
            _ => columns.push((*column, vec![*position])),
        }
    }
    columns
}
/// Appends one entry to a staged doclist without copying it.
///
/// **The buffer is written into, not read out of and put back.** Reading a
/// staged doclist hands back a copy, and appending through that copy moves the
/// whole doclist per occurrence - which is the same quadratic the buffer was
/// added to remove, relocated from the file into memory. A term in five hundred
/// documents grew a two-kilobyte list, so the copies were half a megabyte per
/// term.
///
/// Answers false when the term is not staged, or when the entry does not sort
/// after everything already there; the caller then takes the general path.
///
/// @param buffer - the staged doclists
/// @param term - the term whose doclist this is
/// @param entry - the entry to append
pub(crate) fn append_staged(buffer: &Buffer, term: &[u8], entry: &DocEntry) -> bool {
    let Ok(mut held) = buffer.lock() else {
        return false;
    };
    let Some(staged) = held.doclists.get_mut(term) else {
        return false;
    };
    // The remembered end, rather than a walk to find it. See `Staged`.
    let Some(last) = staged.last else {
        return false;
    };
    if entry.rowid <= last {
        return false;
    }
    let was = staged.bytes.len();
    append_doclist_entry(&mut staged.bytes, entry.rowid.wrapping_sub(last), entry);
    staged.last = Some(entry.rowid);
    let grew = staged.bytes.len().saturating_sub(was);
    held.bytes = held.bytes.saturating_add(grew);
    true
}
/// Stages a doclist to be written when the buffer is next flushed.
///
/// @param buffer - the staged doclists
/// @param term - the term it belongs to
/// @param bytes - the whole doclist
pub(crate) fn stage_doclist(buffer: &Buffer, term: &[u8], bytes: Vec<u8>) {
    // The walk happens here, once per whole-list rewrite, rather than on every
    // append: this is the path a list reaches when it is read back or built
    // from scratch, and it is the only place the end is not already known.
    let last = last_doclist_rowid(&bytes);
    if let Ok(mut held) = buffer.lock() {
        let was = held
            .doclists
            .get(term)
            .map(|staged| staged.bytes.len())
            .unwrap_or(0);
        held.bytes = held.bytes.saturating_sub(was).saturating_add(bytes.len());
        held.doclists.insert(term.to_vec(), Staged { bytes, last });
    }
}
/// Forgets a staged doclist, for one whose row is being deleted.
///
/// @param buffer - the staged doclists
/// @param term - the term
pub(crate) fn forget_doclist(buffer: &Buffer, term: &[u8]) {
    if let Ok(mut held) = buffer.lock() {
        if let Some(staged) = held.doclists.remove(term) {
            held.bytes = held.bytes.saturating_sub(staged.bytes.len());
        }
    }
}
/// Reports whether the buffer is holding more than it should.
///
/// @param buffer - the staged doclists
pub(crate) fn buffer_is_full(buffer: &Buffer) -> bool {
    buffer
        .lock()
        .map(|held| held.bytes > PENDING_BUDGET)
        .unwrap_or(false)
}
/// Writes every staged doclist and empties the buffer.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged doclists
pub(crate) fn flush_doclists(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
) -> DbResult<()> {
    let started = std::time::Instant::now();
    let answer = flush_doclists_timed(context, shadows, buffer);
    let spent = started.elapsed().as_nanos();
    record_stage(|stages| stages.flush = stages.flush.saturating_add(spent));
    answer
}
/// Writes every staged doclist and empties the buffer, without the timing.
///
/// **One write per term, not two.** Every entry the buffer holds is a term's
/// whole `%_idx` row - the dictionary key and the doclist together - so
/// writing it out in key order is the same walk to the right of the tree this
/// used to spend on the dictionary alone, and there is no second pass over a
/// separate `%_data` table behind it.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged doclists
pub(crate) fn flush_doclists_timed(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
) -> DbResult<()> {
    // **In term order.** `%_idx` is keyed by the term, so taking the
    // `BTreeMap` in its own order writes the run as a walk to the right of the
    // tree rather than a descent per term into a growing one.
    let staged: Vec<(Vec<u8>, Staged)> = match buffer.lock() {
        Ok(mut held) => {
            held.bytes = 0;
            core::mem::take(&mut held.doclists).into_iter().collect()
        }
        Err(_) => return Ok(()),
    };
    let started = std::time::Instant::now();
    for (term, doclist) in staged {
        shadows.write_keyed(
            context,
            b"idx",
            2,
            &[
                Value::Integer(SEGMENT),
                Value::owned_blob(&term)?,
                Value::owned_blob(&doclist.bytes)?,
            ],
        )?;
    }
    let spent = started.elapsed().as_nanos();
    record_stage(|stages| stages.dictionary_write = stages.dictionary_write.saturating_add(spent));
    // The totals go out with them, once, rather than once per document. The
    // buffered copy is kept: a reader inside the same transaction has to see
    // what the transaction wrote, which is what makes this a buffer.
    let totals = match buffer.lock() {
        Ok(mut held) if held.totals_dirty => {
            held.totals_dirty = false;
            held.totals.clone()
        }
        _ => None,
    };
    if let Some(totals) = totals {
        put_totals(context, shadows, &totals)?;
    }
    Ok(())
}
/// Resolves a term's dictionary entry, staging its doclist for reuse.
///
/// **One table now, so finding a term and reading its doclist are the same
/// row.** They used to be two: `%_idx` named a `%_data` page and a second
/// descent read it, so every term this transaction had not already staged
/// cost two tree reads whether or not the caller was about to change it. The
/// row `%_idx` returns now carries the doclist already - inline if this build
/// wrote it, or through the `%_data` indirection an older build left, which
/// [`resolve_doclist`] follows - so it is staged here the moment it is found
/// and a caller never reads it twice.
///
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the staged doclists
/// @param term - the term
/// @param create - whether a term with no row yet is reported as fresh rather
///   than absent
pub(crate) fn term_row(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &Buffer,
    term: &[u8],
    create: bool,
) -> DbResult<Option<Term>> {
    if let Ok(held) = buffer.lock() {
        if held.doclists.contains_key(term) {
            return Ok(Some(Term { fresh: false }));
        }
    }
    let probed = std::time::Instant::now();
    let key = [Value::Integer(SEGMENT), Value::owned_blob(term)?];
    let found = shadows.read_keyed(context, b"idx", &key, 3)?;
    let probe_ns = probed.elapsed().as_nanos();
    record_stage(|stages| stages.dictionary_read = stages.dictionary_read.saturating_add(probe_ns));
    let Some(row) = found else {
        return Ok(create.then_some(Term { fresh: true }));
    };
    let bytes = resolve_doclist(context, shadows, &row)?.unwrap_or_default();
    stage_doclist(buffer, term, bytes);
    Ok(Some(Term { fresh: false }))
}
/// Whether a term's dictionary row was found, and staged, by [`term_row`].
///
/// **`fresh` is what lets the caller skip a decode it knows will miss.** A
/// term with no row yet has no doclist to merge into, so building its first
/// entry from scratch is the whole job; one `term_row` found has already been
/// staged, and the caller's own retry of the fast append path is what uses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Term {
    /// Whether the row was created by the call that returned this, rather
    /// than found on disk or already staged.
    pub(crate) fresh: bool,
}
/// Returns the error a command FTS5 does not know reports.
///
/// SQLite says nothing more than `SQL logic error` here - there is no message
/// naming the command - so neither does this.
pub(crate) fn unrecognized() -> inillucent_base::DbError {
    inillucent_base::DbError::primary(inillucent_base::PrimaryCode::Error)
}
/// Reads a `%_docsize` blob back into one count per column.
pub fn decode_sizes(bytes: &[u8], columns: usize) -> Vec<i64> {
    let mut at = 0usize;
    (0..columns)
        .map(|_| read_varint(bytes, &mut at) as i64)
        .collect()
}
/// Returns a value as the text the tokenizer reads.
pub(crate) fn text_of(value: &Value<'static>) -> Option<Vec<u8>> {
    match value {
        Value::Text(text) => Some(text.utf8_bytes().into_owned()),
        Value::Blob(blob) => Some(blob.raw().to_vec()),
        Value::Integer(number) => Some(number.to_string().into_bytes()),
        Value::Real(number) => Some(inillucent_value::numeric::real_to_text(*number)),
        Value::Null => None,
    }
}
