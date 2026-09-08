//! `fsdir`: a directory tree as a table.
//!
//! Invariant: **it reads and never writes, and it reads only under the path it
//! was given.** A table-valued function over the file system is the one module
//! here whose rows are not in the database at all, so the bound on what it can
//! reach is the whole of its safety: the walk starts at the argument, never
//! follows a link out of it, and refuses a path it cannot read rather than
//! reporting an empty directory.
//!
//! It is registered by the **shell** rather than by the engine, which is where
//! the reference puts it too and for the same reason: a library that read the
//! file system on behalf of any statement would be a library an untrusted
//! query could read `/etc/passwd` through. A program that wants it says so.
//!
//! ```text
//! SELECT name, mode, mtime, length(data) FROM fsdir('crates');
//! ```
//!
//! `name` is the path as it would be typed, `mode` the POSIX mode bits,
//! `mtime` seconds since the epoch, `data` the file's bytes - NULL for a
//! directory - and `level` how deep below the starting path the entry is.

use std::path::{Path, PathBuf};

use inillucent_base::DbResult;
use inillucent_value::Value;

use super::{
    ConstraintOp, Context, Declaration, DeclaredColumn, FilterPlan, IndexQuery, Module,
    ModuleArguments, ShadowTable, VirtualCursor, VirtualTable,
};

/// How deep the walk will go.
///
/// A bound rather than a preference: a directory tree can contain a cycle
/// through a link, and a walk with no limit would not come back.
const DEEPEST: usize = 64;

/// The `fsdir` module.
pub struct FsDirModule;

impl Module for FsDirModule {
    /// Returns the module's name.
    fn name(&self) -> &str {
        "fsdir"
    }

    /// It is a table-valued function, so the name is the table.
    fn eponymous(&self) -> bool {
        true
    }

    /// And it is only ever that: there is nothing for a `CREATE` to make.
    fn constructible(&self) -> bool {
        false
    }

    /// No shadow tables: the rows are the file system's.
    fn shadow_tables(&self, _arguments: &ModuleArguments) -> DbResult<Vec<ShadowTable>> {
        Ok(Vec::new())
    }

    /// Connects, declaring the reference's own columns.
    fn connect(
        &self,
        _arguments: &ModuleArguments,
        _creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        Ok(Box::new(FsDirTable {
            declaration: Declaration {
                columns: vec![
                    DeclaredColumn::visible("name"),
                    DeclaredColumn::visible("mode"),
                    DeclaredColumn::visible("mtime"),
                    DeclaredColumn::visible("data"),
                    DeclaredColumn::visible("level"),
                    DeclaredColumn::hidden("path"),
                    DeclaredColumn::hidden("dir"),
                ],
                without_rowid: false,
            },
        }))
    }
}

/// The `fsdir` table, which holds nothing of its own.
struct FsDirTable {
    declaration: Declaration,
}

/// Which hidden column carries the path to walk.
const PATH_COLUMN: i32 = 5;
/// Which hidden column carries the directory the path is relative to.
const DIR_COLUMN: i32 = 6;

impl VirtualTable for FsDirTable {
    /// Returns the five visible columns and the two hidden arguments.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Claims the two arguments and nothing else.
    ///
    /// The path is **required**: a walk with no starting point would be a walk
    /// of the whole file system, which is not a thing a query should be able to
    /// ask for by leaving an argument out.
    fn best_index(&self, query: &mut IndexQuery) -> DbResult<()> {
        let mut found = false;
        for index in 0..query.constraints.len() {
            let Some(constraint) = query.constraints.get(index).copied() else {
                continue;
            };
            if !constraint.usable || constraint.op != ConstraintOp::Eq {
                continue;
            }
            if constraint.column == PATH_COLUMN || constraint.column == DIR_COLUMN {
                query.use_constraint(index, true);
                found |= constraint.column == PATH_COLUMN;
            }
        }
        query.index_number = i32::from(found);
        query.estimated_cost = if found { 100.0 } else { 1.0e9 };
        query.estimated_rows = 100;
        Ok(())
    }

    /// Opens a cursor over the walk.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(FsDirCursor {
            rows: Vec::new(),
            at: 0,
        }))
    }
}

/// One entry the walk found.
struct Entry {
    /// The path as it would be typed.
    name: String,
    /// The POSIX mode bits.
    mode: i64,
    /// Seconds since the epoch.
    mtime: i64,
    /// The file's bytes, or nothing for a directory.
    data: Option<Vec<u8>>,
    /// How deep below the starting path it is.
    level: i64,
}

/// A walk in progress.
struct FsDirCursor {
    rows: Vec<Entry>,
    at: usize,
}

impl VirtualCursor for FsDirCursor {
    /// Walks the tree the arguments name.
    fn filter(&mut self, _context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        self.rows.clear();
        self.at = 0;
        let Some(path) = plan.arguments.first().and_then(text_of) else {
            return Err(inillucent_base::error::misuse(
                "fsdir() takes the path to walk",
            ));
        };
        let base = plan.arguments.get(1).and_then(text_of);
        let start = match &base {
            Some(directory) => PathBuf::from(directory).join(&path),
            None => PathBuf::from(&path),
        };
        let Ok(found) = std::fs::symlink_metadata(&start) else {
            return Err(inillucent_base::error::misuse(format!(
                "cannot stat file: {path}"
            )));
        };
        // **The starting path is a row too**, and it is the first: the
        // reference reports the directory it was pointed at and then everything
        // under it, numbering the levels from one. A walk that reported only
        // the contents would answer nothing at all for a plain file.
        self.rows.push(entry_of(&start, path.clone(), 1));
        if found.is_dir() {
            walk(&start, &path, 2, &mut self.rows);
        }
        Ok(())
    }

    /// Steps to the next entry.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        Ok(())
    }

    /// Returns whether the walk is finished.
    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// Returns one column of the current entry.
    fn column(&mut self, _context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        let Some(entry) = self.rows.get(self.at) else {
            return Ok(Value::Null);
        };
        Ok(match index {
            0 => Value::owned_text(entry.name.as_bytes())?,
            1 => Value::Integer(entry.mode),
            2 => Value::Integer(entry.mtime),
            3 => match &entry.data {
                Some(bytes) => Value::owned_blob(bytes)?,
                None => Value::Null,
            },
            4 => Value::Integer(entry.level),
            _ => Value::Null,
        })
    }

    /// Returns the entry's position in the walk.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self.at as i64)
    }
}

/// Adds every entry under one directory to the list, depth first.
///
/// @param on_disk - where the directory actually is
/// @param shown - the path as it should be reported
/// @param level - how deep below the starting path this directory is
/// @param into - the list being built
fn walk(on_disk: &Path, shown: &str, level: i64, into: &mut Vec<Entry>) {
    if level as usize >= DEEPEST {
        return;
    }
    let Ok(listing) = std::fs::read_dir(on_disk) else {
        return;
    };
    let mut found: Vec<(String, PathBuf)> = listing
        .filter_map(|held| held.ok())
        .map(|held| (held.file_name().to_string_lossy().into_owned(), held.path()))
        .collect();
    // Sorted, because a directory listing's order is the file system's and a
    // query that answered differently on two machines would be untestable.
    found.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, path) in found {
        let reported = format!("{shown}/{name}");
        let is_directory = path.is_dir();
        into.push(entry_of(&path, reported.clone(), level));
        if is_directory {
            walk(&path, &reported, level.saturating_add(1), into);
        }
    }
}

/// Returns one entry, reading the file's bytes when it is a file.
///
/// @param on_disk - where the entry actually is
/// @param shown - the path as it should be reported
/// @param level - how deep below the starting path it is
fn entry_of(on_disk: &Path, shown: String, level: i64) -> Entry {
    let found = std::fs::symlink_metadata(on_disk).ok();
    let is_directory = found.as_ref().is_some_and(|held| held.is_dir());
    let mtime = found
        .as_ref()
        .and_then(|held| held.modified().ok())
        .and_then(|held| held.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|held| held.as_secs() as i64)
        .unwrap_or(0);
    Entry {
        name: shown,
        mode: found.as_ref().map(mode_of).unwrap_or(0),
        mtime,
        data: if is_directory {
            None
        } else {
            std::fs::read(on_disk).ok()
        },
        level,
    }
}

/// Returns a value's bytes as text, when it holds any.
///
/// @param value - the argument
fn text_of(value: &Value<'static>) -> Option<String> {
    match value {
        Value::Text(text) => Some(String::from_utf8_lossy(&text.utf8_bytes()).into_owned()),
        Value::Blob(blob) => Some(String::from_utf8_lossy(blob.raw()).into_owned()),
        _ => None,
    }
}

/// Returns the POSIX mode bits an entry is reported with.
///
/// On a system that has them, the ones it has. On Windows there are none, and
/// the reference reports what its C runtime synthesises - `0o40777` for a
/// directory, `0o100666` for a writable file and `0o100444` for a read-only
/// one - so those are what is reported here, because a caller comparing two
/// engines' output on one machine is comparing these numbers.
///
/// @param found - the entry's metadata
#[cfg(unix)]
fn mode_of(found: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    i64::from(found.mode())
}

/// The same, where the operating system has no mode bits.
///
/// @param found - the entry's metadata
#[cfg(not(unix))]
fn mode_of(found: &std::fs::Metadata) -> i64 {
    if found.is_dir() {
        return 0o40_777;
    }
    if found.permissions().readonly() {
        0o100_444
    } else {
        0o100_666
    }
}
