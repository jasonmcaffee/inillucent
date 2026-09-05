//! The file that makes a commit across several databases one event.
//!
//! Invariant: a transaction over several databases is decided by the existence
//! of one file. Each database's own journal carries that file's name, and a
//! journal that names a super-journal is replayed only while the super-journal
//! is still there. So the deletion of that one file is the moment every
//! database in the transaction committed, and until it happens every one of
//! them rolls back. There is no window where some have and some have not,
//! because there is nothing to observe between "the file is there" and "the
//! file is gone" - a deletion is one operation.
//!
//! The super-journal itself is a list of the journals that were part of the
//! transaction, each a path followed by a zero byte. It is read by recovery to
//! answer the other question: whether a super-journal left behind by a crash
//! is still needed by any of the databases it names, or is safe to remove.
//!
//! Reference: <https://sqlite.org/atomiccommit.html#_multi_file_commit>.

use std::sync::Arc;

use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_vfs::{AccessMode, DbPath, FileKind, OpenOptions, SyncMode, Vfs, VfsFile};

/// The suffix a super-journal's name is built with.
pub const SUPER_JOURNAL_PREFIX: &str = "-mj";

/// The file that decides a multi-database transaction.
#[derive(Debug)]
pub struct SuperJournal {
    vfs: Arc<dyn Vfs>,
    path: DbPath,
    file: Box<dyn VfsFile>,
    offset: u64,
}

impl SuperJournal {
    /// Creates a super-journal beside the main database.
    ///
    /// The name carries randomness because two transactions on the same
    /// database may be in flight in two processes, and a journal that named
    /// the wrong super-journal would be decided by somebody else's commit.
    pub fn create(vfs: Arc<dyn Vfs>, near: &DbPath) -> DbResult<SuperJournal> {
        let mut seed = [0u8; 4];
        vfs.randomness(&mut seed)?;
        let suffix = format!("{SUPER_JOURNAL_PREFIX}{:08x}", u32::from_be_bytes(seed));
        let path = near.with_suffix(&suffix);
        let mut options = OpenOptions::of_kind(FileKind::MasterJournal);
        options.exclusive = true;
        let file = vfs.open(&path, options)?;
        file.truncate(0)?;
        Ok(SuperJournal {
            vfs,
            path,
            file,
            offset: 0,
        })
    }

    /// Returns the path a journal has to name to be decided by this one.
    pub fn path(&self) -> &DbPath {
        &self.path
    }

    /// Adds one database's journal to the list.
    pub fn add(&mut self, journal: &DbPath) -> DbResult<()> {
        let Some(text) = journal.to_str() else {
            return Err(misuse(
                "a journal whose name is not valid UTF-8 cannot join a multi-database commit",
            ));
        };
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        self.file.write_all_at(self.offset, &bytes)?;
        self.offset = self.offset.saturating_add(bytes.len() as u64);
        Ok(())
    }

    /// Makes the list durable, which has to happen before any journal names it.
    ///
    /// The order is the whole argument. A journal that names a super-journal
    /// nobody wrote would be a journal that never gets replayed, so the list
    /// exists first and the names point at it second.
    pub fn sync(&mut self) -> DbResult<()> {
        self.file
            .sync(SyncMode::Full)
            .map_err(|error| error.into_db_error())
    }

    /// Deletes the file, which is the commit point.
    ///
    /// Every database is durable by now and every journal is still hot. This
    /// one operation makes all of them non-hot at once, because a journal that
    /// names a file that is not there is a journal describing a transaction
    /// that finished.
    pub fn commit(&mut self) -> DbResult<()> {
        self.vfs.delete(&self.path, true)?;
        Ok(())
    }

    /// Removes the file after a failure, before any database was written.
    pub fn abandon(&mut self) {
        let _ = self.vfs.delete(&self.path, false);
    }
}

/// Reads the journals a super-journal names.
pub fn children_of(vfs: &dyn Vfs, path: &DbPath) -> DbResult<Vec<DbPath>> {
    if !vfs.access(path, AccessMode::Exists)? {
        return Ok(Vec::new());
    }
    let file = vfs.open(
        path,
        OpenOptions::of_kind(FileKind::MasterJournal).read_only(),
    )?;
    let size = file.file_size()?;
    let mut raw = vec![0u8; usize::try_from(size).unwrap_or(0)];
    if !raw.is_empty() {
        file.read_exact_at(0, &mut raw)?;
    }
    Ok(raw
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .filter_map(|name| core::str::from_utf8(name).ok())
        .map(|name| DbPath::new(std::path::PathBuf::from(name)))
        .collect())
}

/// Deletes a super-journal no journal needs any more.
///
/// A super-journal is needed while any journal it names still exists *and*
/// still names it. Once every one of them has been finalised there is nothing
/// left to decide, and leaving the file behind would make the next connection
/// on any of those databases read a list of journals that are not there.
pub fn remove_if_unused(
    vfs: &dyn Vfs,
    path: &DbPath,
    names_it: impl Fn(&DbPath) -> DbResult<bool>,
) -> DbResult<bool> {
    for child in children_of(vfs, path)? {
        if !vfs.access(&child, AccessMode::Exists)? {
            continue;
        }
        if names_it(&child)? {
            return Ok(false);
        }
    }
    vfs.delete(path, true)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_vfs::MemoryVfs;

    /// The list round-trips, and the name is beside the database it belongs to.
    #[test]
    fn the_list_round_trips() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let main = DbPath::new(std::path::PathBuf::from("/app.db"));
        let mut super_journal = SuperJournal::create(Arc::clone(&vfs), &main).unwrap();
        assert!(super_journal
            .path()
            .to_str()
            .is_some_and(|name| name.starts_with("/app.db-mj")));
        super_journal
            .add(&DbPath::new(std::path::PathBuf::from("/app.db-journal")))
            .unwrap();
        super_journal
            .add(&DbPath::new(std::path::PathBuf::from("/other.db-journal")))
            .unwrap();
        super_journal.sync().unwrap();
        let children = children_of(vfs.as_ref(), super_journal.path()).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(
            children.first().and_then(|p| p.to_str()),
            Some("/app.db-journal")
        );
        assert_eq!(
            children.get(1).and_then(|p| p.to_str()),
            Some("/other.db-journal")
        );
    }

    /// Committing removes the file, which is what every journal that names it
    /// is waiting to observe.
    #[test]
    fn committing_removes_the_file() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let main = DbPath::new(std::path::PathBuf::from("/app.db"));
        let mut super_journal = SuperJournal::create(Arc::clone(&vfs), &main).unwrap();
        let path = super_journal.path().clone();
        assert!(vfs.access(&path, AccessMode::Exists).unwrap());
        super_journal.commit().unwrap();
        assert!(!vfs.access(&path, AccessMode::Exists).unwrap());
        assert!(children_of(vfs.as_ref(), &path).unwrap().is_empty());
    }

    /// A super-journal one of its journals still names is kept; one nobody
    /// names is removed.
    #[test]
    fn it_is_removed_only_when_nobody_needs_it() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let main = DbPath::new(std::path::PathBuf::from("/app.db"));
        let mut super_journal = SuperJournal::create(Arc::clone(&vfs), &main).unwrap();
        let child = DbPath::new(std::path::PathBuf::from("/app.db-journal"));
        super_journal.add(&child).unwrap();
        super_journal.sync().unwrap();
        let path = super_journal.path().clone();

        // The journal exists and still names it: the file stays.
        let journal = vfs
            .open(&child, OpenOptions::of_kind(FileKind::MainJournal))
            .unwrap();
        journal.write_all_at(0, b"records").unwrap();
        drop(journal);
        assert!(!remove_if_unused(vfs.as_ref(), &path, |_| Ok(true)).unwrap());
        assert!(vfs.access(&path, AccessMode::Exists).unwrap());

        // It no longer names it: the file goes.
        assert!(remove_if_unused(vfs.as_ref(), &path, |_| Ok(false)).unwrap());
        assert!(!vfs.access(&path, AccessMode::Exists).unwrap());
    }
}
