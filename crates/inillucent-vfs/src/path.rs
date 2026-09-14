//! Database paths and the files derived from them.
//!
//! Invariant: the journal, WAL, and shared-memory names for a database are
//! derived in exactly one place. SQLite's recovery depends on a second process
//! deriving the same names from the same database path, so a suffix that is
//! spelled at two call sites is a recovery bug waiting to happen.
//!
//! A `DbPath` is a thin wrapper rather than a re-implementation of `PathBuf`.
//! It exists so that "a string a caller handed us" and "a path the VFS has
//! resolved" are different types, and so that the suffix rules cannot be
//! bypassed by string concatenation somewhere else.

use std::path::{Path, PathBuf};

/// The suffix SQLite appends to a database name for its rollback journal.
pub const JOURNAL_SUFFIX: &str = "-journal";

/// The suffix SQLite appends to a database name for its write-ahead log.
pub const WAL_SUFFIX: &str = "-wal";

/// The suffix SQLite appends to a database name for its shared-memory file.
pub const SHM_SUFFIX: &str = "-shm";

/// A path to a database or one of its companion files.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DbPath {
    inner: PathBuf,
}

impl DbPath {
    /// Wraps a path as given, without resolving it.
    pub fn new(path: impl Into<PathBuf>) -> DbPath {
        DbPath { inner: path.into() }
    }

    /// Returns the underlying path.
    pub fn as_path(&self) -> &Path {
        &self.inner
    }

    /// Returns the path as a string when it is valid UTF-8.
    ///
    /// Windows paths are UTF-16 and POSIX paths are bytes, so a path that is
    /// not valid UTF-8 is possible; callers that need to display one use this
    /// and fall back rather than assuming.
    pub fn to_str(&self) -> Option<&str> {
        self.inner.to_str()
    }

    /// Returns the path in a form that is always displayable, replacing any
    /// invalid sequence rather than failing.
    pub fn display(&self) -> String {
        self.inner.display().to_string()
    }

    /// Returns the rollback journal path for this database.
    pub fn journal(&self) -> DbPath {
        self.with_suffix(JOURNAL_SUFFIX)
    }

    /// Returns the write-ahead log path for this database.
    pub fn wal(&self) -> DbPath {
        self.with_suffix(WAL_SUFFIX)
    }

    /// Returns the shared-memory path for this database.
    pub fn shm(&self) -> DbPath {
        self.with_suffix(SHM_SUFFIX)
    }

    /// Returns this path with `suffix` appended to the whole file name.
    ///
    /// The suffix follows the extension rather than replacing it: the journal
    /// for `app.db` is `app.db-journal`, not `app-journal`.
    pub fn with_suffix(&self, suffix: &str) -> DbPath {
        let mut text = self.inner.clone().into_os_string();
        text.push(suffix);
        DbPath::new(PathBuf::from(text))
    }

    /// Returns the directory that contains this path, when it has one.
    pub fn parent(&self) -> Option<DbPath> {
        self.inner.parent().map(DbPath::new)
    }

    /// Returns the directory this path's file sits in, as a directory that can
    /// be opened.
    ///
    /// **`Path::parent` answers `Some("")` for a bare file name, and `""` is not
    /// a directory anything can open (task-1946, M5).** `OsVfs` flushes the
    /// containing directory after a delete and after a rename so that the
    /// directory entry survives a power loss, and on Unix that means opening it
    /// and calling `fsync`. Opening `""` answers `ENOENT`, so `ATTACH 'app.rdb'`
    /// - a name with no directory in it, which is what a relative name from a
    /// shell looks like - failed on Linux with `DirSync: No such file or
    /// directory`. It did not fail on Windows because `sync_directory` there is
    /// a no-op: the file system journals its own metadata.
    ///
    /// The containing directory of a bare file name is the process's current
    /// directory, which is what `.` names.
    pub fn containing_directory(&self) -> Option<DbPath> {
        let parent = self.inner.parent()?;
        Some(if parent.as_os_str().is_empty() {
            DbPath::new(".")
        } else {
            DbPath::new(parent)
        })
    }

    /// Reports whether this path is the special in-memory database name.
    pub fn is_memory(&self) -> bool {
        self.to_str() == Some(":memory:")
    }

    /// Reports whether this path is the empty name that asks for a private
    /// temporary database.
    pub fn is_anonymous(&self) -> bool {
        self.inner.as_os_str().is_empty()
    }
}

impl From<&str> for DbPath {
    /// Wraps a string path.
    fn from(value: &str) -> DbPath {
        DbPath::new(value)
    }
}

impl From<PathBuf> for DbPath {
    /// Wraps an owned path.
    fn from(value: PathBuf) -> DbPath {
        DbPath::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The companion names must append to the whole file name, because a
    /// second process derives them from the database path and has to agree.
    #[test]
    fn companion_files_append_to_the_whole_name() {
        let database = DbPath::from("C:/data/app.db");
        assert_eq!(database.journal().to_str(), Some("C:/data/app.db-journal"));
        assert_eq!(database.wal().to_str(), Some("C:/data/app.db-wal"));
        assert_eq!(database.shm().to_str(), Some("C:/data/app.db-shm"));
    }

    /// A database with no extension still gets suffixed names.
    #[test]
    fn a_name_without_an_extension_still_gets_companions() {
        let database = DbPath::from("app");
        assert_eq!(database.journal().to_str(), Some("app-journal"));
        assert_eq!(database.wal().to_str(), Some("app-wal"));
    }

    /// The two special names the API accepts must be recognisable.
    #[test]
    fn the_special_names_are_recognised() {
        assert!(DbPath::from(":memory:").is_memory());
        assert!(DbPath::from("").is_anonymous());
        assert!(!DbPath::from("app.db").is_memory());
        assert!(!DbPath::from("app.db").is_anonymous());
    }

    /// Deriving a companion twice must be idempotent in the sense that it is a
    /// pure function of the input, so recovery in another process agrees.
    #[test]
    fn deriving_a_companion_is_a_pure_function_of_the_path() {
        let database = DbPath::from("/var/lib/app.db");
        assert_eq!(database.journal(), database.journal());
        assert_eq!(database.wal().parent(), Some(DbPath::from("/var/lib")));
    }

    /// A bare file name's containing directory is one that can be opened.
    ///
    /// `Path::parent` answers `Some("")` here, and `""` opens as nothing. On
    /// Unix that is the difference between a durable delete and
    /// `DirSync: No such file or directory`, which is what `ATTACH 'app.rdb'`
    /// answered on Linux until task-1946.
    #[test]
    fn a_name_with_no_directory_in_it_is_contained_by_the_current_one() {
        assert_eq!(
            DbPath::from("app.rdb").containing_directory(),
            Some(DbPath::from("."))
        );
        assert_eq!(
            DbPath::from("/var/lib/app.rdb").containing_directory(),
            Some(DbPath::from("/var/lib"))
        );
        // The root has no containing directory, and asking for one answers so
        // rather than answering `.`, which would be a different directory.
        assert_eq!(DbPath::from("/").containing_directory(), None);
    }
}
