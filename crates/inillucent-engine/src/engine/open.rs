//! Opening a database: importing one, creating one, and recovering one.
//!
//! Invariant: **every way into an `ImportedDatabase` is here, and each of them
//! leaves the file recovered.** `open` on a file a crash left behind replays the
//! log before it answers, `import` reads a SQLite file into a new one, and
//! `create` writes the meta page. A caller that had opened a file some other way
//! would be holding a database whose log had not been applied.

use std::collections::HashMap;
use std::path::PathBuf;

use inillucent_base::DbResult;
use inillucent_catalog::load::table_from_create_sql;
use inillucent_catalog::paged::{schema_create_sql, write_catalog};
use inillucent_exec::physical::SourceLayout;
use inillucent_exec::StaticType;
use inillucent_pool::{Database, Options};
use inillucent_sql::catalog_view::StaticCatalog;
use inillucent_tree::PagedTree;
use inillucent_vfs::{DbPath, OsVfs};
use inillucent_wal::{Wal, WalOptions, FIRST_LSN};

use crate::*;

impl crate::ImportedDatabase {
    /// Imports a SQLite fixture into PAX trees in a fresh database file.
    ///
    /// @param path - the SQLite database to read
    /// @param page_size - the page size to build the new trees at
    pub fn import(path: PathBuf, page_size: usize) -> DbResult<ImportedDatabase> {
        ImportedDatabase::import_with(path, page_size, DEFAULT_FRAMES)
    }

    /// Imports a SQLite fixture, with the pool size stated.
    ///
    /// The file is written beside the fixture with a `.rdb` suffix, built,
    /// checkpointed, closed and reopened - so what the measurement reads is a
    /// database that came off a disk, through a pool of the size named here,
    /// rather than a tree that never left memory. That distinction is the whole
    /// of the Phase 1 report's outstanding fairness question.
    ///
    /// @param path - the SQLite database to read
    /// @param page_size - the page size to build the new trees at
    /// @param frames - how many frames the buffer pool holds
    pub fn import_with(
        path: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        let target = target_path(&path, page_size, frames);
        ImportedDatabase::import_into(path, target, page_size, frames)
    }

    /// Imports a SQLite file into a target the caller names.
    ///
    /// The same import, with the destination stated rather than derived. A
    /// measurement wants the derived name - the page size and frame count are
    /// in it so a sweep does not overwrite a file it still has open - and a
    /// **migration** wants to choose, because it stages into a uniquely named
    /// file beside the destination and publishes by renaming. A half-written
    /// database must never sit at the path an application opens, and that is a
    /// property of *where* it is written.
    ///
    /// The target is removed first if it exists, so a caller that stages into a
    /// fresh name gets a fresh file and one that reuses a name gets a rebuild
    /// rather than a merge.
    ///
    /// @param path - the SQLite database to read
    /// @param target - the file to build
    /// @param page_size - the page size to build the new trees at
    /// @param frames - how many frames the buffer pool holds
    pub fn import_into(
        path: PathBuf,
        target: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        // **The seven phases, in the one order that works.** The reopen is what
        // the order exists for: everything after it reads a file this process
        // did not write, so the format round-trip is checked rather than
        // assumed. Each phase is in `crate::import` with its own argument.
        let mut source = crate::import::read_source(path)?;
        let (vfs, db_path, mut database) =
            crate::import::create_destination(&target, page_size, frames)?;
        let mut carried = crate::import::Carried::new();
        crate::import::carry_tables(&mut database, &mut source, &mut carried)?;
        crate::import::carry_treeless(&source, &mut carried);
        let catalog_shape = crate::import::write_schema(&mut database, &carried)?;
        let database =
            crate::import::reopen_and_verify(database, &vfs, &db_path, frames, &carried)?;

        let mut trees = HashMap::new();
        for (root, shape) in &carried.shapes {
            let tree = PagedTree::attach(
                database.pool(),
                u64::from(*root),
                shape.root,
                shape.columns.clone(),
                shape.key_columns,
                shape.leaf_count,
                shape.row_count,
            )?;
            trees.insert(*root, tree);
        }

        // `sqlite_schema` joins the catalog as a table like any other, keyed by
        // a root number no imported table can have. `SELECT ... FROM
        // sqlite_schema` is then planned, scanned and projected by the ordinary
        // path - there is no view machinery, because there does not need to be.
        let schema_root = SCHEMA_VIEW_ROOT;
        let schema_info = table_from_create_sql(schema_create_sql(), 0, schema_root)?;
        carried.layouts.insert(
            schema_root,
            std::rc::Rc::new(SourceLayout {
                tree_key: schema_root,
                // The rowid is the tree's key column and is not one of the five
                // declared columns, so the record slots start at tree column 1.
                slots: (1..=5).map(Some).collect(),
                rowid: Some(0),
                identity: vec![0],
                types: vec![
                    StaticType::Int,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Text,
                    StaticType::Int,
                    StaticType::Text,
                ],
                width: 6,
                key_columns: vec![0],
            }),
        );
        trees.insert(
            schema_root,
            PagedTree::attach(
                database.pool(),
                u64::from(schema_root),
                catalog_shape.root,
                catalog_shape.columns.clone(),
                catalog_shape.key_columns,
                catalog_shape.leaf_count,
                catalog_shape.row_count,
            )?,
        );
        carried.catalog = carried.catalog.with_table(schema_info.clone());
        carried.catalog = carried.catalog.with_table(schema_alias_of(&schema_info));

        // The log the write path describes every change in, opened on a file
        // that has just been checkpointed - so it starts empty, at the first
        // stream position, and every record in it is one this process wrote.
        let wal = std::rc::Rc::new(Wal::open(
            std::sync::Arc::clone(&vfs),
            &db_path,
            database.uuid(),
            FIRST_LSN,
            1,
            WalOptions::default(),
        )?);
        database.pool().set_durable_lsn(wal.write_ahead_point());
        let_the_pool_ask_the_log(database.pool(), &wal);

        // Smallest tree first, so the physical pass takes the cheapest
        // structure that covers the query. Sorting by bytes rather than by
        // column count is what makes it the right order: an index with more
        // columns but shorter values can still be the smaller scan.
        for roots in carried.covering.values_mut() {
            roots.sort_by_key(|root| {
                trees
                    .get(root)
                    .map(PagedTree::byte_size)
                    .unwrap_or(usize::MAX)
            });
        }

        let mut opened = ImportedDatabase {
            // **SQLite's own two defaults, and each one is a measurement.**
            //
            // `delete` because it costs nothing: the medium gate reads 3.78x
            // weighted with `wal` and 3.70x with `delete`, lower bounds 3.45x
            // and 3.44x - the same number twice, over 30 paired rounds on one
            // machine. The write-ahead log is still here and
            // `PRAGMA journal_mode = WAL` still switches to it; what changed is
            // which one a caller gets without asking, and the answer is now the
            // one every other SQLite gives.
            //
            // **The second half of this comment argued for `exclusive` as the
            // default, and the default is `normal` - it has been since task-1980
            // and the comment was never corrected** (task-2000, design 1d). It
            // said `locking_mode = normal` took the weighted headline to 3.03x
            // with `write` at 1.19x, `transaction` at 0.37x and `schema` at
            // 0.66x, and concluded that a program which never opens a second
            // connection should not pay for one that does. What settled it the
            // other way is that a default a second process cannot share is not a
            // default an embedded database can ship, and the multi-process
            // protocol task-1979 and task-1980 built is what the roadmap
            // advertises.
            //
            // Those numbers were also a measurement of one particular commit
            // path, not of the mode. What made `normal` expensive was that a
            // statement's release *folded the log into the file*: six to eight
            // fsync class calls a statement, with a rollback journal protecting
            // the fold's in place page writes. Design 1 of task-2000 took the
            // fold off the release path - a commit is one log append and one
            // sync, and the fold runs every four mebibytes of log - so the mode
            // costs the two cheap staleness checks `enter_within` makes and
            // nothing else.
            counters: std::rc::Rc::new(Counters {
                last_rowid: std::cell::Cell::new(0),
                last_changes: std::cell::Cell::new(0),
                seed: std::cell::Cell::new(fresh_seed()),
                changed_ever: std::cell::Cell::new(0),
                session_change_baseline: session_changes::SessionChanges::default(),
            }),
            compiled: std::rc::Rc::new(Compiled {
                statements: std::cell::RefCell::new(HashMap::new()),
                statement_cache_limit: std::cell::Cell::new(plans::DEFAULT_STATEMENT_CACHE),
                compiles: std::cell::Cell::new(0),
                scratch_ast: std::cell::RefCell::new(None),
                scratch_binder: std::cell::RefCell::new(None),
                index_stages: std::cell::Cell::new(StageTimings::default()),
            }),
            writing: std::rc::Rc::new(Writing::starting_at(1)),
            storage: Storage {
                database,
                page_size,
                frames,
                path: target,
                wal,
                // A file this call just created has no log to recover.
                recovery: crate::recovery::RecoveryReport::default(),
                in_doubt: false,
                read_only: false,
                vfs: std::sync::Arc::clone(&vfs),
            },
            schema: Schema {
                catalog: carried.catalog,
                trees,
                layouts: carried.layouts,
                covering: carried.covering,
                skipped: carried.skipped,
                entries: carried
                    .entries
                    .into_iter()
                    .zip(carried.identifiers)
                    .enumerate()
                    .map(|(nth, (entry, root))| Recorded {
                        rowid: nth.saturating_add(1) as i64,
                        root,
                        entry,
                    })
                    .collect(),
                tables: carried.tables,
                schema_info,
                next_root: FIRST_CREATED_ROOT,
                imposters: Vec::new(),
                catalog_generation: 0,
                next_handle: FIRST_ATTACHED_HANDLE,
                ddl_schema: 0,
            },
            pragmas: std::rc::Rc::new(Pragmas::fresh()),
            session_state: SessionState {
                modules_begun: std::cell::Cell::new(false),
                authorizer: None,
                collations: Vec::new(),
                registry: modules(),
                eponymous: Vec::new(),
                virtual_tables: HashMap::new(),
                vector_indexes: HashMap::new(),
                attached: Vec::new(),
                temps: Vec::new(),
                session: std::cell::Cell::new(0),
                tables_session: 0,
                owner: HashMap::new(),
            },
        };
        opened.settle_journal()?;
        Ok(opened)
    }

    /// Reconciles the journal mode with what the file says, and installs it.
    ///
    /// **Two things a constructor cannot do for itself.** The mode a connection
    /// starts in is not a constant: a database left in WAL comes back in WAL,
    /// because the alternative is one connection writing pre-images beside a
    /// log another is appending frames to. And a rollback mode needs its
    /// `Journal` attached to the pool - without it the mode is a word the
    /// pragma reports and nothing writes a pre-image, which is a durability
    /// hole rather than a cosmetic one.
    fn settle_journal(&mut self) -> DbResult<()> {
        let mode = if self.storage.database.wal_mode() {
            inillucent_pool::journal::JournalMode::Wal
        } else {
            self.pragmas.journal_mode()
        };
        self.pragmas.set_journal_mode(mode);
        let held: std::sync::Arc<dyn inillucent_vfs::Vfs> =
            std::sync::Arc::clone(&self.storage.vfs);
        let journal = journal_for(mode).map(|protection| {
            inillucent_pool::journal::Journal::new(
                held,
                &DbPath::new(self.storage.path.to_string_lossy().as_ref()),
                protection,
                self.storage.page_size,
            )
        });
        self.storage.database.pool().set_journal(journal);
        // **And the fold's protection follows the mode** (task-2000, design 1a).
        // Under `wal` the fold appends an after image of every page it is about
        // to write to the log it already has, so it asks the journal for
        // nothing; the journal stays in place for an eviction, which is undo and
        // needs a pre image. See `Pool::fold_protected_by_log`.
        self.storage
            .database
            .pool()
            .set_fold_protected_by_log(mode == inillucent_pool::journal::JournalMode::Wal);
        // **And the owner replays whenever this cache is thrown away**
        // (task-2000, design 1b). With the fold lazy, a connection holds dirty
        // pages between statements, so a take where another process has folded
        // finds a cache that is stale and dirty at once. `resync_from_file`
        // replays the log from the file's own checkpoint on every such take,
        // which is what makes dropping those frames rather than refusing them
        // lose nothing. See `Database::replayed_by_its_owner`.
        self.storage.database.set_replayed_by_its_owner(true);
        Ok(())
    }

    /// Creates a fresh, empty database.
    ///
    /// **The primitive the re-rooting needs, and the one the engine did not
    /// have.** It could `import` a SQLite file and, since this ticket, `open` a
    /// file it had written; it could not make one. `Database::open` on a path
    /// that does not exist has to create it, so a connection cannot be re-rooted
    /// onto this engine without it.
    ///
    /// What it writes is the smallest legal database: the file, its meta page,
    /// and a catalog tree with no rows in it. Everything else - tables, indexes,
    /// virtual tables - arrives through DDL afterwards, which is the path that
    /// already exists and is already tested.
    ///
    /// It goes through the same close-and-reopen the import does, for the same
    /// reason: what the caller gets back has been read off a disk rather than
    /// kept in the pool that wrote it, so a format that does not round-trip
    /// fails here rather than in a query much later.
    ///
    /// @param path - where to create the database
    /// @param page_size - the page size to build at
    /// @param frames - how many frames the buffer pool holds
    pub fn create(path: PathBuf, page_size: usize, frames: usize) -> DbResult<ImportedDatabase> {
        ImportedDatabase::create_on(std::sync::Arc::new(OsVfs::new()), path, page_size, frames)
    }

    /// Creates a fresh, empty database on a file system of the caller's.
    ///
    /// The general form of [`ImportedDatabase::create`], and what `:memory:`
    /// goes through: a `MemoryVfs` given here is the file system the database,
    /// its log and its journal all live on, and it disappears with the last
    /// handle to it.
    ///
    /// @param vfs - the file system to build on
    /// @param path - where to create the database
    /// @param page_size - the page size to build at
    /// @param frames - how many frames the buffer pool holds
    pub fn create_on(
        vfs: std::sync::Arc<dyn inillucent_vfs::Vfs>,
        path: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        let _ = vfs.delete(&db_path, false);
        let mut database = Database::create(
            vfs.as_ref(),
            &db_path,
            Options::default()
                .with_page_size(page_size)
                .with_frames(frames.max(64)),
        )?;
        // An empty catalog is a catalog tree with no entries, not the absence of
        // one: every later DDL statement inserts into it, and a database whose
        // catalog root pointed nowhere would be one no `CREATE TABLE` could
        // start from.
        let _ = write_catalog(&mut database, &[])?;
        database.checkpoint()?;
        drop(database);
        ImportedDatabase::open_on(vfs, path, page_size, frames)
    }

    /// Opens a database this engine wrote, reading its schema from the file.
    ///
    /// **This is the engine's own open path, and it is a different thing from
    /// [`ImportedDatabase::reopen`].** `reopen` closes and reopens a handle this
    /// process already has, and it carries that handle's column specifications
    /// and tree numbering across - which is correct for what it is for, proving
    /// that a checkpointed file reads back, but it cannot open a file this
    /// process did not write. Its own comment says so: "a genuine open would
    /// number them itself" and "the engine's own open path is Phase 5's consumer
    /// story".
    ///
    /// This is that path. Nothing comes from memory: the catalog tree is
    /// attached from the meta page's root, every row is read out of it, and each
    /// object's shape is derived from the `CREATE` text the row carries, by the
    /// same `table_shape` / `keyed_table_shape` / `index_shape` the import uses.
    /// One derivation, so a file that opens differently from the way it was
    /// built is a bug in one function rather than a disagreement between two.
    ///
    /// The per-tree statistics come from the catalog row rather than from a walk
    /// - which is what `TreeStats` is in the file for, and what makes opening a
    /// large database cost a catalog read instead of a scan of every leaf.
    ///
    /// @param path - the database file to open
    /// @param page_size - the page size the file was built at
    /// @param frames - how many frames the buffer pool holds
    pub fn open(path: PathBuf, page_size: usize, frames: usize) -> DbResult<ImportedDatabase> {
        ImportedDatabase::open_on(std::sync::Arc::new(OsVfs::new()), path, page_size, frames)
    }

    /// Opens a database this connection will never write.
    ///
    /// See `crate::recovery::open_file_as`: the file handle is read only, no
    /// step of the open touches the media, and the connection's own log is a
    /// scratch one in memory. A statement that would write is refused by the
    /// pool with `ReadOnly` (task-1979, section 5.2).
    ///
    /// @param path - the database file to open
    /// @param page_size - the page size the file was built at
    /// @param frames - how many frames the buffer pool holds
    pub fn open_read_only(
        path: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        ImportedDatabase::open_as(
            std::sync::Arc::new(OsVfs::new()),
            path,
            page_size,
            frames,
            true,
        )
    }

    /// Opens a database on a file system of the caller's.
    ///
    /// The general form of [`ImportedDatabase::open`]; see
    /// [`ImportedDatabase::create_on`] for why the file system is held rather
    /// than made where it is used.
    ///
    /// @param vfs - the file system the database lives on
    /// @param path - the database file to open
    /// @param page_size - the page size the file was built at
    /// @param frames - how many frames the buffer pool holds
    pub fn open_on(
        vfs: std::sync::Arc<dyn inillucent_vfs::Vfs>,
        path: PathBuf,
        page_size: usize,
        frames: usize,
    ) -> DbResult<ImportedDatabase> {
        ImportedDatabase::open_as(vfs, path, page_size, frames, false)
    }

    /// [`ImportedDatabase::open_on`], with the caller saying whether this
    /// connection may write.
    ///
    /// @param vfs - the file system the database lives on
    /// @param path - the database file to open
    /// @param page_size - the page size the file was built at
    /// @param frames - how many frames the buffer pool holds
    /// @param read_only - whether this connection may write the file
    pub fn open_as(
        vfs: std::sync::Arc<dyn inillucent_vfs::Vfs>,
        path: PathBuf,
        page_size: usize,
        frames: usize,
        read_only: bool,
    ) -> DbResult<ImportedDatabase> {
        let db_path = DbPath::new(path.to_string_lossy().as_ref());
        // **A file opened as `main` asks the same question an attached one
        // does.** A database this connection is opened on may have been the
        // participant of a cross-file commit that a crash caught undecided, and
        // there is nothing about being `main` that settles it.
        // **A hot rollback journal is replayed before anything reads a page.**
        // It describes a file that is halfway through a transaction, and every
        // page it names has to go back before the meta record is even read -
        // the meta page itself may be one of them. A journal whose header is
        // absent or zeroed describes nothing and is removed, which is what a
        // finished one looks like. See `inillucent_pool::journal`.
        //
        // A read only connection cannot do it and refuses instead - see
        // `settle_a_hot_journal`.
        settle_a_hot_journal(vfs.as_ref(), &db_path, read_only)?;
        let doubtful = multi::doubtful_transactions(&path)?;
        let opened_file =
            crate::recovery::open_file_as(&vfs, &db_path, frames, &doubtful, read_only)?;
        let OpenedFile {
            database,
            wal,
            catalog_tree,
            recovery,
            highest_txn,
        } = opened_file;
        // **One derivation for every file this connection can name.** The
        // shapes are read out of the catalog tree by `load_schema`, which is
        // the same function `ATTACH` uses - so a file opened as `main` and the
        // same file attached as `aux` are planned against identical
        // declarations rather than against two derivations that agree today.
        //
        // `main`'s handles are its own local identifiers, unchanged: the
        // database a connection was opened on keeps the numbering it has always
        // had, which is what makes a one-file connection the code it was.
        let loaded = load_schema(
            &database,
            catalog_tree,
            0,
            b"main",
            SCHEMA_VIEW_ROOT,
            &mut |local| local,
        )?;
        let LoadedSchema {
            trees,
            layouts,
            covering,
            entries,
            tables: loaded_tables,
            schema_info,
            handles: _main_handles,
            skipped,
            highest_identifier,
        } = loaded;
        let mut catalog = StaticCatalog::empty();
        for info in &loaded_tables {
            catalog = catalog.with_table(info.clone());
        }
        catalog = catalog.with_table(schema_info.clone());
        catalog = catalog.with_table(schema_alias_of(&schema_info));

        let mut opened = ImportedDatabase {
            // Above every number the log still holds, so that this run cannot
            // call something by a name a crashed one already used.
            counters: std::rc::Rc::new(Counters {
                last_rowid: std::cell::Cell::new(0),
                last_changes: std::cell::Cell::new(0),
                seed: std::cell::Cell::new(fresh_seed()),
                changed_ever: std::cell::Cell::new(0),
                session_change_baseline: session_changes::SessionChanges::default(),
            }),
            compiled: std::rc::Rc::new(Compiled {
                statements: std::cell::RefCell::new(HashMap::new()),
                statement_cache_limit: std::cell::Cell::new(plans::DEFAULT_STATEMENT_CACHE),
                compiles: std::cell::Cell::new(0),
                scratch_ast: std::cell::RefCell::new(None),
                scratch_binder: std::cell::RefCell::new(None),
                index_stages: std::cell::Cell::new(StageTimings::default()),
            }),
            writing: std::rc::Rc::new(Writing::starting_at(highest_txn.saturating_add(1))),
            storage: Storage {
                database,
                page_size,
                frames,
                path,
                wal,
                recovery,
                in_doubt: !doubtful.is_empty(),
                read_only,
                vfs: std::sync::Arc::clone(&vfs),
            },
            schema: Schema {
                catalog,
                trees,
                layouts,
                covering,
                skipped,
                entries,
                tables: Vec::new(),
                schema_info,
                next_root: highest_identifier.saturating_add(1).max(FIRST_CREATED_ROOT),
                next_handle: FIRST_ATTACHED_HANDLE,
                ddl_schema: 0,
                imposters: Vec::new(),
                catalog_generation: 0,
            },
            pragmas: std::rc::Rc::new(Pragmas::fresh()),
            session_state: SessionState {
                modules_begun: std::cell::Cell::new(false),
                attached: Vec::new(),
                temps: Vec::new(),
                session: std::cell::Cell::new(0),
                tables_session: 0,
                owner: HashMap::new(),
                authorizer: None,
                collations: Vec::new(),
                registry: modules(),
                eponymous: Vec::new(),
                virtual_tables: HashMap::new(),
                vector_indexes: HashMap::new(),
            },
        };
        opened.settle_journal()?;
        opened.rebuild_tables()?;
        opened.refresh_catalog();
        // The modules are connected after the tables are loaded, because a
        // module's shadow tables have to exist before it can be connected to
        // them. Nothing did this before, so a reopened database holding a
        // search table answered "no such table" for it.
        opened.reconnect_modules()?;
        opened.rebuild_tables()?;
        opened.refresh_catalog();
        // **The tail is given back only now, because every step above can
        // refuse** (task-2070). See `header_accounts_for_every_object`.
        if !read_only
            && header_accounts_for_every_object(&opened.schema.entries, &opened.storage.database)
        {
            opened.storage.database.give_back_the_unclaimed_tail()?;
        }
        Ok(opened)
    }
}

/// Reports whether every page the catalog names lies inside the page count the
/// meta record carries.
///
/// **What makes giving the file's tail back safe** (task-2070).
/// `Database::give_back_the_unclaimed_tail` cuts the file to
/// `page_count * page_size`, and `page_count` comes from the meta record - so a
/// meta record that is behind the file turns the trim from reclaiming a tail
/// nothing owns into deleting pages the catalog is pointing at. Measured: a
/// database of 360,448 bytes carrying the meta record its *creation* wrote came
/// back as 131,072 bytes, the four pages that record describes, with its
/// catalog still naming a table rooted in the part that had just been deleted.
///
/// Leaving the tail in place instead costs a file longer than it needs to be,
/// which the next checkpoint's own count reclaims. That is the cheaper of the
/// two wrong answers by a wide margin, and it is why this skips rather than
/// refuses: a database that opens and reads correctly is not one to turn away
/// over its length.
///
/// @param entries - the catalog rows this open read
/// @param database - the file they were read from
pub(crate) fn header_accounts_for_every_object(
    entries: &[Recorded],
    database: &inillucent_pool::Database,
) -> bool {
    let count = database.pool().page_count();
    entries.iter().all(|held| held.entry.root.0 < count)
}

/// Returns a built tree's shape as the catalog records it.
///
/// @param shape - what the build produced
pub(crate) fn stats_of(shape: &TreeShape) -> inillucent_catalog::paged::TreeStats {
    inillucent_catalog::paged::TreeStats {
        first_leaf: shape.first_leaf,
        leaf_count: shape.leaf_count,
        row_count: shape.row_count,
    }
}

/// One catalog row, with the identifier of the tree it describes.
///
/// The row is what the file holds; the identifier is what the `trees` and
/// `layouts` maps are keyed by. They are different numbers - see the module
/// documentation on `newengine::ddl` - and carrying them together is what lets a
/// rename change the row without the tree it names moving.
#[derive(Clone, Debug)]
pub(crate) struct Recorded {
    /// The rowid the catalog tree stores it under.
    pub(crate) rowid: i64,
    /// The identifier its tree is registered under, zero when it has no tree.
    pub(crate) root: u32,
    /// The row itself.
    pub(crate) entry: SchemaEntry,
}

/// The shape of one built tree, kept so it can be re-attached after the file is
/// closed and reopened.
///
/// It is what the catalog will hold in Phase 3. Carrying it explicitly rather
/// than rediscovering it on open is deliberate: rediscovering a root's height
/// by reading the root is fine, but rediscovering its *row count* means walking
/// it, and a harness that walked every tree on open would be measuring its own
/// startup.
pub(crate) struct TreeShape {
    pub(crate) root: PageId,
    pub(crate) columns: Vec<ColumnSpec>,
    pub(crate) key_columns: usize,
    pub(crate) first_leaf: PageId,
    pub(crate) leaf_count: u64,
    pub(crate) row_count: u64,
}

/// The root number `sqlite_schema` is registered under.
///
/// A root number is only an identifier here - the physical page comes from the
/// meta record - so the catalog takes one no imported table can be given. SQLite
/// roots start at 1 and count pages, so a number at the top of the range is
/// free by construction.
pub(crate) const SCHEMA_VIEW_ROOT: u32 = u32::MAX;

/// The identifier the first DDL-created tree is registered under.
///
/// Imported trees are keyed by the fixture's SQLite root *page*, which counts
/// pages from one, so a fixture would have to be eight terabytes at the default
/// page size before it reached this. Counting up from here keeps every created
/// tree's identifier distinct from every imported one without a search.
pub(crate) const FIRST_CREATED_ROOT: u32 = 0x8000_0000;

/// The first handle a tree of an attached database is registered under.
///
/// **A handle is the connection's name for a tree; a tree identifier is the
/// file's.** They are the same number for `main` and they cannot be for anything
/// else: two files number their own trees from one, so a connection holding both
/// would have two trees under one key. So `main` keeps identity and every other
/// schema's trees are re-numbered into the range above this.
///
/// The two ranges cannot meet by growth, because `allocate_root` refuses at this
/// number: a `main` holding 2^30 created objects is refused by name rather than
/// silently handed a handle an attached database already answers to.
pub(crate) const FIRST_ATTACHED_HANDLE: u32 = 0xC000_0000;

/// How many databases a connection may hold beside `main` and `temp`.
///
/// SQLite's `SQLITE_MAX_ATTACHED` default, and the number `attach.rs` grades
/// against.
pub(crate) const MAX_ATTACHED: usize = 10;

/// Returns the path the imported database is written to.
///
/// The page size and the frame count are in the name so that a sweep over
/// either does not overwrite the previous run's file while it is still open.
///
/// @param fixture - the SQLite fixture being imported
/// @param page_size - the page size the trees are built at
/// @param frames - how many frames the pool holds
pub(crate) fn target_path(fixture: &std::path::Path, page_size: usize, frames: usize) -> PathBuf {
    let stem = fixture
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "fixture".to_string());
    let directory = fixture
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    directory.join(format!("{stem}-p{page_size}-f{frames}.rdb"))
}

/// Tells a pool how to make the log catch up when it is behind.
///
/// **Registered wherever a database and a log come together**, which is the
/// import, the open and the reopen. Without it the pool can only refuse a page
/// whose LSN is past the durable point, and a statement that dirties more pages
/// than the pool holds has no way to satisfy it: `CREATE INDEX` on the large
/// fixture failed on every one of thirty qualification rounds for exactly that
/// reason.
///
/// The sync is real. `write_ahead_point` is `durable_end` under NORMAL and
/// FULL, so what is handed back is a point the log has reached rather than one
/// it has merely been given bytes for, and the pool's guard still refuses if it
/// is not far enough.
///
/// @param pool - the pool that will do the asking
/// @param wal - the log it should ask
pub(crate) fn let_the_pool_ask_the_log(pool: &Pool, wal: &std::rc::Rc<Wal>) {
    let held = std::rc::Rc::clone(wal);
    pool.on_log_behind(std::rc::Rc::new(move || {
        held.sync()?;
        Ok(held.write_ahead_point())
    }));
}

/// Replays a rollback journal left by an interrupted write, or refuses.
///
/// **A read only connection cannot replay one, so it refuses (task-1979,
/// section 5.2).** The replay is a write, and a file with a hot journal is one
/// halfway through a transaction - reading it as it stands would hand a caller
/// pages from the middle of somebody else's write. Naming what is wrong and
/// what would fix it is better than answering rows nobody should act on.
///
/// @param vfs - the file system the database lives on
/// @param db_path - the database file
/// @param read_only - whether this connection may write the file
fn settle_a_hot_journal(
    vfs: &dyn inillucent_vfs::Vfs,
    db_path: &DbPath,
    read_only: bool,
) -> DbResult<()> {
    if !read_only {
        inillucent_pool::journal::replay_hot_journal(vfs, db_path)?;
        return Ok(());
    }
    if vfs
        .access(&db_path.journal(), inillucent_vfs::AccessMode::Exists)
        .unwrap_or(false)
    {
        return Err(inillucent_base::error::refusal(
            "this database has a rollback journal from an interrupted write, and a read only \
             connection cannot replay it; open it for writing once to recover it",
        ));
    }
    Ok(())
}
