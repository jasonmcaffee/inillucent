//! `VACUUM`: rebuilding a database into a file with nothing spare in it.
//!
//! Invariant: a rebuild is a **logical** copy, not a page copy. Every table is
//! read back as rows and written into a fresh database through the ordinary
//! write path, so the result is a database this engine would have produced had
//! the rows arrived in key order and nothing had ever been deleted. That is the
//! only version of `VACUUM` that can honestly claim the space back: a byte copy
//! of the file reproduces its free pages, its half-empty leaves and its
//! interleaved trees exactly, and the file it writes is the same size as the
//! one it read.
//!
//! It is also what SQLite does. `sqlite3RunVacuum` opens a second database,
//! copies the schema into it by *running the `CREATE` statements*, copies the
//! rows with `INSERT ... SELECT`, and then swaps the two files' contents. The
//! shape here is the same, and for the same reason: the rebuild then goes
//! through every check the ordinary write path makes, so a `VACUUM` cannot
//! produce a database a `CREATE TABLE` could not have produced.
//!
//! # Why the two forms share this
//!
//! `VACUUM` and `VACUUM INTO 'file'` differ in one line: where the rebuilt
//! database ends up. They used to share nothing - `VACUUM`
//! checkpointed and `VACUUM INTO` called `std::fs::copy` - and neither of them
//! reclaimed a byte. A reader who asked "is the file smaller afterwards?" got
//! "no" from both, which is not what either statement means.

use std::path::{Path, PathBuf};

use inillucent_base::{error::refusal, DbResult};
use inillucent_catalog::paged::ObjectKind;

use crate::ImportedDatabase;

/// One object to recreate, with the rows it holds.
///
/// The schema is captured before anything is written, because the rebuild reads
/// from a database it is about to replace and a half-read schema is worse than
/// no schema.
struct Captured {
    /// What kind of object it is.
    kind: ObjectKind,
    /// Its name, as written.
    name: Vec<u8>,
    /// The `CREATE` text, empty for an object that has none.
    sql: Vec<u8>,
}

/// Rebuilds a database into a fresh file, and reports what it wrote.
///
/// The order is the one a schema can be replayed in: tables first - a virtual
/// table is one of these, with `VIRTUAL` in its `CREATE` text - because
/// everything else names one; then the rows, while no secondary index exists to
/// maintain; then the indexes, which build over rows that are already there;
/// then views and triggers, which may name any of the above.
///
/// An automatic index - the one a `UNIQUE` or `PRIMARY KEY` declaration
/// implies - has no `CREATE` text and is skipped: recreating the table recreates
/// it, and replaying it would be a second index under the same name.
///
/// @param source - the database being rebuilt
/// @param destination - the file to write, which must not exist
/// @param page_size - the page size the new file is laid out with
/// @param frames - how many frames its pool is given
pub(crate) fn rebuild_into(
    source: &ImportedDatabase,
    destination: &Path,
    page_size: usize,
    frames: usize,
) -> DbResult<()> {
    let captured = capture_schema(source);
    let mut fresh = ImportedDatabase::create(destination.to_path_buf(), page_size, frames)?;
    replay_schema(&mut fresh, &captured, |kind| kind == ObjectKind::Table)?;
    for entry in &captured {
        if entry.kind != ObjectKind::Table {
            continue;
        }
        copy_rows(source, &mut fresh, &entry.name)?;
    }
    replay_schema(&mut fresh, &captured, |kind| kind == ObjectKind::Index)?;
    replay_schema(&mut fresh, &captured, |kind| {
        matches!(kind, ObjectKind::View | ObjectKind::Trigger)
    })?;
    carry_header(source, &mut fresh)?;
    fresh.checkpoint()?;
    Ok(())
}

/// Reads the schema of `main` into something that outlives the source.
///
/// @param source - the database being rebuilt
fn capture_schema(source: &ImportedDatabase) -> Vec<Captured> {
    source
        .main_entries()
        .iter()
        .filter(|entry| !entry.sql.is_empty())
        .filter(|entry| !entry.name.starts_with(b"sqlite_"))
        .map(|entry| Captured {
            kind: entry.kind,
            name: entry.name.clone(),
            sql: entry.sql.clone(),
        })
        .collect()
}

/// Runs the `CREATE` statements of every object a predicate selects.
///
/// @param fresh - the database being built
/// @param captured - the whole schema, in catalog order
/// @param wanted - which kinds to replay on this pass
fn replay_schema(
    fresh: &mut ImportedDatabase,
    captured: &[Captured],
    wanted: impl Fn(ObjectKind) -> bool,
) -> DbResult<()> {
    for entry in captured {
        if !wanted(entry.kind) {
            continue;
        }
        let sql = String::from_utf8_lossy(&entry.sql).into_owned();
        let empty = inillucent_exec::physical::Params::new();
        fresh.execute_any(&sql, &empty).map_err(|error| {
            refusal(format!(
                "cannot rebuild {}: {}",
                String::from_utf8_lossy(&entry.name),
                error.detail().unwrap_or_else(|| error.message())
            ))
        })?;
    }
    Ok(())
}

/// Copies one table's rows, key order first, in batches.
///
/// **Read in `rowid` order and written in `rowid` order**, which is what makes
/// the rebuilt tree dense: a leaf filled by ascending appends is full, and one
/// filled by arriving keys is half full on average. It is also what makes the
/// copy reproducible - two `VACUUM`s of one database produce the same file.
///
/// A virtual table's rows are copied through its module the same way, because
/// `INSERT` into one is the module's business and a rebuild has no business
/// reaching around it.
///
/// @param source - the database being rebuilt
/// @param fresh - the database being built
/// @param table - the table's name, as written
fn copy_rows(
    source: &ImportedDatabase,
    fresh: &mut ImportedDatabase,
    table: &[u8],
) -> DbResult<()> {
    let name = quoted(table);
    let (rows, columns) = source.run(&format!("SELECT * FROM {name}"))?;
    if rows.is_empty() {
        return Ok(());
    }
    let placeholders = (1..=columns.len())
        .map(|at| format!("?{at}"))
        .collect::<Vec<_>>()
        .join(", ");
    let names = columns
        .iter()
        .map(|column| quoted(column.as_bytes()))
        .collect::<Vec<_>>()
        .join(", ");
    let insert = format!("INSERT INTO {name} ({names}) VALUES ({placeholders})");
    for row in rows {
        let mut params = inillucent_exec::physical::Params::new();
        for (at, value) in row.iter().enumerate() {
            params.set(at.saturating_add(1) as u32, value.clone());
        }
        fresh.execute_any(&insert, &params)?;
    }
    Ok(())
}

/// Carries the header fields a rebuild must not invent.
///
/// `user_version` and `application_id` belong to the application rather than to
/// the file's layout, so a rebuild that reset them would silently break the
/// migration scheme of every application that uses one. SQLite carries both
/// across a `VACUUM` for the same reason.
///
/// @param source - the database being rebuilt
/// @param fresh - the database being built
fn carry_header(source: &ImportedDatabase, fresh: &mut ImportedDatabase) -> DbResult<()> {
    let empty = inillucent_exec::physical::Params::new();
    fresh.execute_any(
        &format!("PRAGMA user_version={}", source.user_version()),
        &empty,
    )?;
    fresh.execute_any(
        &format!("PRAGMA application_id={}", source.application_id()),
        &empty,
    )?;
    Ok(())
}

/// Returns an identifier quoted so that any name can be written into SQL.
///
/// Double quotes with the internal ones doubled, which is the form every
/// identifier takes regardless of what it holds - a rebuild reads names out of a
/// file it did not write, and a table called `select` or `a"b` is legal.
///
/// @param name - the identifier's bytes
fn quoted(name: &[u8]) -> String {
    let mut out = String::from("\"");
    for byte in name {
        if *byte == b'"' {
            out.push('"');
        }
        out.push(char::from(*byte));
    }
    out.push('"');
    out
}

/// Returns a path beside `target` that nothing else is using.
///
/// The rebuild is written next to the database rather than into a temporary
/// directory so that the swap at the end is a rename within one filesystem,
/// which is atomic; a rename across filesystems is a copy, and a copy is what
/// `VACUUM` exists to avoid doing twice.
///
/// @param target - the database being rebuilt
/// @param stamp - something that makes the name unique
pub(crate) fn scratch_beside(target: &Path, stamp: u64) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(format!(".vacuum-{stamp}"));
    PathBuf::from(name)
}

/// Removes every write-ahead log segment beside a database file.
///
/// A segment describes the *pages* of the database it was written for, so one
/// left beside a file whose bytes have been replaced is not stale bookkeeping -
/// it is a recipe for undoing the replacement, applied silently by recovery on
/// the next open. This is called at exactly the two moments a file's bytes are
/// about to change identity, and at no other time.
///
/// Failures are ignored: the segments are this engine's own scratch, and a
/// segment that cannot be removed is a warning rather than a reason to abandon
/// a rebuild that has already succeeded.
///
/// @param database - the database file the segments belong to
pub(crate) fn remove_log_segments(database: &Path) {
    let Some(directory) = database.parent() else {
        return;
    };
    let Some(base) = database.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!("{base}-wal.");
    let Ok(listing) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in listing.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
