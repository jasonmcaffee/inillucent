//! `ATTACH`, `DETACH`, and the temporary database a connection makes for
//! itself.
//!
//! Invariant: **a schema is a file, and a handle is this connection's name for
//! one of its trees.** The file numbers its own trees, writes those numbers into
//! its catalog rows and into every log record, and knows nothing about any other
//! file. The connection numbers the trees of *every* file it holds, so that a
//! plan can say "read tree 3,221,225,472" and mean exactly one tree. `main`'s
//! two numbers are the same number, which is what makes a connection that never
//! attached anything the connection it was before this module existed.
//!
//! ## What `ATTACH` may not do
//!
//! `main` and `temp` are not names another database can take, a name in use is
//! in use, and a file may not be attached past the limit. Each of those is a
//! refusal by name rather than a silent second binding, because every one of
//! them would otherwise be a query that reads a different table from the one it
//! names - the worst failure this engine has.
//!
//! ## Why `DETACH` is refused inside a transaction
//!
//! SQLite refuses it, and the reason is the same here: the schemas are numbered
//! in attachment order, an open transaction's participant set is recorded by
//! those numbers, and removing one from the middle would renumber a set that is
//! already being counted. Refusing is not a limitation being worked around; it
//! is the rule that makes the participant set mean something.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_pool::{Database, Options};
use inillucent_vfs::memory::MemoryVfs;
use inillucent_vfs::os::OsVfs;
use inillucent_vfs::{DbPath, Vfs};

use super::{
    load_schema, open_file, write_catalog, Attached, ImportedDatabase, LoadedSchema, OpenedFile,
    FIRST_ATTACHED, FIRST_CREATED_ROOT, MAIN, MAX_ATTACHED,
};

/// The name a caller writes to ask for a database with no file behind it.
pub const IN_MEMORY: &[u8] = b":memory:";

/// The name of the connection's own temporary database.
pub const TEMP: &[u8] = b"temp";

impl ImportedDatabase {
    /// Returns the schema one name refers to, when this connection holds it.
    ///
    /// @param name - the name as a statement wrote it
    pub(crate) fn schema_named(&self, name: &[u8]) -> Option<usize> {
        if name.eq_ignore_ascii_case(b"main") {
            return Some(MAIN);
        }
        if name.eq_ignore_ascii_case(TEMP) {
            return Some(super::TEMP);
        }
        self.attached
            .iter()
            .position(|held| held.name.eq_ignore_ascii_case(name))
            .map(|nth| nth.saturating_add(FIRST_ATTACHED))
    }

    /// Adds a database file to this connection under a name.
    ///
    /// The file is created when it is not there, which is what `ATTACH` does:
    /// naming a database that does not exist yet is how one is made.
    ///
    /// @param file - the path, as the statement's literal
    /// @param name - the name it will be known by
    pub(crate) fn attach(&mut self, file: &[u8], name: &[u8]) -> DbResult<()> {
        if name.eq_ignore_ascii_case(b"main") || name.eq_ignore_ascii_case(TEMP) {
            return Err(refusal(format!(
                "database {} is already in use",
                String::from_utf8_lossy(name)
            )));
        }
        if self
            .attached
            .iter()
            .any(|held| held.name.eq_ignore_ascii_case(name))
        {
            return Err(refusal(format!(
                "database {} is already in use",
                String::from_utf8_lossy(name)
            )));
        }
        if self.attached.len() >= MAX_ATTACHED {
            return Err(refusal(format!(
                "too many attached databases - max {MAX_ATTACHED}"
            )));
        }
        let (vfs, path, held) = if file == IN_MEMORY || file.is_empty() {
            let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
            (
                vfs,
                DbPath::from(format!("/memory/{}.db", String::from_utf8_lossy(name)).as_str()),
                None,
            )
        } else {
            let text = String::from_utf8_lossy(file).into_owned();
            let vfs: Arc<dyn Vfs> = Arc::new(OsVfs::new());
            // **A confined process refuses the path here, by name.** The VFS
            // refuses it too, and that is the guarantee - but the VFS has only
            // an extended result code to answer with, and an agent told
            // "access permission denied" cannot tell a confinement from a file
            // it lacks rights to. `ATTACH` is the statement that can reach a
            // database outside `--root`, so it is the one that says what
            // happened.
            let path = match inillucent_vfs::confine::process_root() {
                None => DbPath::from(text.as_str()),
                Some(root) => match root.admit(&text) {
                    Ok(inside) => DbPath::new(inside),
                    Err(refused) => return Err(refusal(refused.message())),
                },
            };
            let held = path.as_path().to_path_buf();
            (vfs, path, Some(held))
        };
        self.attach_file(vfs, path, held, name.to_vec(), None)
    }

    /// Opens or creates one file and registers it as a schema of this
    /// connection.
    ///
    /// Shared by `ATTACH` and by the temporary database, which differs from an
    /// attachment in two ways and only two: it has no file, and it belongs to
    /// one connection rather than to the database.
    ///
    /// @param vfs - the file system the file and its log live on
    /// @param path - where the file is, on that file system
    /// @param held - the path to report, or `None` when there is no file
    /// @param name - the name a statement qualifies with
    /// @param session - the connection this schema belongs to, for a `temp`
    pub(crate) fn attach_file(
        &mut self,
        vfs: Arc<dyn Vfs>,
        path: DbPath,
        held: Option<PathBuf>,
        name: Vec<u8>,
        session: Option<u64>,
    ) -> DbResult<()> {
        // **Created when it is not there.** `ATTACH` on a path that holds
        // nothing makes the database, which is what SQLite does and what makes
        // `ATTACH ':memory:'` mean anything at all.
        if !vfs.access(&path, inillucent_vfs::AccessMode::Exists)? {
            let mut fresh = Database::create(
                vfs.as_ref(),
                &path,
                Options::default()
                    .with_page_size(self.page_size)
                    .with_frames(self.frames.max(64)),
            )?;
            // An empty catalog is a catalog tree with no rows, not the absence
            // of one: every later `CREATE TABLE` inserts into it.
            let _ = write_catalog(&mut fresh, &[])?;
            fresh.checkpoint()?;
            drop(fresh);
        }
        let OpenedFile {
            database,
            wal,
            catalog_tree,
            highest_txn,
        } = open_file(&vfs, &path, self.frames, &self.doubt_for(&path)?)?;
        // **The connection has one transaction counter and now two logs.** A
        // file attached mid-session may hold higher numbers than anything this
        // connection has issued, and a number reused across the two would make
        // a crashed run's records replay under a live transaction's commit.
        self.raise_transactions_past(highest_txn);

        // A temporary database is schema one whoever it belongs to; an
        // attachment takes the next number after the ones already there.
        let index = match session {
            Some(_) => super::TEMP,
            None => self.attached.len().saturating_add(FIRST_ATTACHED),
        };
        let mut next = self.next_handle;
        let mut take = |count: u32| -> u32 {
            let handle = next;
            next = next.saturating_add(count);
            handle
        };
        let catalog_handle = take(1);
        let loaded = load_schema(
            &database,
            catalog_tree,
            index,
            &name,
            catalog_handle,
            &mut |_local| take(1),
        )?;
        if next <= self.next_handle {
            return Err(refusal(
                "this connection holds too many attached objects to name them all",
            ));
        }
        self.next_handle = next;

        let LoadedSchema {
            trees,
            layouts,
            covering,
            entries,
            tables: _,
            schema_info,
            handles,
            skipped,
            highest_identifier,
        } = loaded;
        for root in trees.keys() {
            self.owner.insert(*root, index);
        }
        self.trees.extend(trees);
        self.layouts.extend(layouts);
        self.covering.extend(covering);
        self.skipped.extend(skipped);
        let held = Attached {
            name,
            path: held,
            vfs,
            database,
            wal,
            entries,
            next_root: highest_identifier.saturating_add(1).max(FIRST_CREATED_ROOT),
            handles,
            catalog_handle,
            schema_info,
            session,
        };
        match session {
            Some(_) => self.temps.push(held),
            None => self.attached.push(held),
        }
        self.rebuild_tables()?;
        self.refresh_catalog();
        Ok(())
    }

    /// Removes a database this connection attached.
    ///
    /// @param name - the name it was attached under
    pub(crate) fn detach(&mut self, name: &[u8]) -> DbResult<()> {
        if name.eq_ignore_ascii_case(b"main") || name.eq_ignore_ascii_case(TEMP) {
            return Err(refusal(format!(
                "cannot detach database {}",
                String::from_utf8_lossy(name)
            )));
        }
        let Some(at) = self.schema_named(name) else {
            return Err(refusal(format!(
                "no such database: {}",
                String::from_utf8_lossy(name)
            )));
        };
        if self.batch.get().is_some() {
            return Err(refusal("cannot DETACH database within transaction"));
        }
        let Some(nth) = at.checked_sub(FIRST_ATTACHED) else {
            return Err(refusal("cannot detach database main"));
        };
        let Some(mut held) = (nth < self.attached.len()).then(|| self.attached.remove(nth)) else {
            return Err(refusal(format!(
                "no such database: {}",
                String::from_utf8_lossy(name)
            )));
        };
        // **Its handles go with it.** A plan built while it was attached names
        // trees this connection no longer holds; the statement cache is emptied
        // by `refresh_catalog` below, and these maps are emptied here so that a
        // handle cannot be answered by a tree that was detached.
        let gone: Vec<u32> = self
            .owner
            .iter()
            .filter(|(_, owner)| **owner == at)
            .map(|(root, _)| *root)
            .collect();
        for root in gone {
            self.owner.remove(&root);
            self.trees.remove(&root);
            self.layouts.remove(&root);
            self.covering.remove(&root);
            for roots in self.covering.values_mut() {
                roots.retain(|kept| *kept != root);
            }
        }
        // **Every schema after it moves down one.** The binder numbers schemas
        // by their position, so a gap would make `aux2` answer to the number
        // `aux3` was bound under - which is a statement reading a different
        // file from the one it names.
        let moved: Vec<(u32, usize)> = self
            .owner
            .iter()
            .filter(|(_, owner)| **owner > at)
            .map(|(root, owner)| (*root, owner.saturating_sub(1)))
            .collect();
        for (root, owner) in moved {
            self.owner.insert(root, owner);
        }
        // The log is folded into the file before the file goes, so that what is
        // left on disk is a database rather than a database and a log nobody
        // will open again.
        held.database.checkpoint()?;
        drop(held);
        self.rebuild_tables()?;
        self.refresh_catalog();
        Ok(())
    }
}
