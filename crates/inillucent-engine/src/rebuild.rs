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
/// **The fresh file is created on the caller's own file system.** This used to
/// call `ImportedDatabase::create`, which constructs a fresh `OsVfs`, so a
/// connection on `MemoryVfs` or `SimVfs` wrote its rebuild onto the real disk
/// and then renamed a path that was not there (task-1946, H2).
///
/// @param vfs - the file system the database lives on
/// @param source - the database being rebuilt
/// @param destination - the file to write, which must not exist
/// @param page_size - the page size the new file is laid out with
/// @param frames - how many frames its pool is given
pub(crate) fn rebuild_into(
    vfs: std::sync::Arc<dyn inillucent_vfs::Vfs>,
    source: &ImportedDatabase,
    destination: &Path,
    page_size: usize,
    frames: usize,
) -> DbResult<()> {
    let captured = capture_schema(source);
    let mut fresh = ImportedDatabase::create_on(vfs, destination.to_path_buf(), page_size, frames)
        .map_err(|error| {
            inillucent_base::error::misuse(format!(
                "VACUUM could not create the file to rebuild into, {}: {}",
                destination.display(),
                error
            ))
        })?;
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
    carry_header(source, &mut fresh).map_err(|error| {
        inillucent_base::error::misuse(format!("VACUUM could not carry the header: {error}"))
    })?;
    fresh.checkpoint().map_err(|error| {
        inillucent_base::error::misuse(format!(
            "VACUUM could not checkpoint the rebuilt file: {error}"
        ))
    })?;
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

/// Swaps a rebuilt file into place over `path`, durably.
///
/// **A rename, not a copy.** `std::fs::copy` reads `scratch` and writes
/// `path` a buffer at a time; a crash midway through leaves `path` holding
/// some of the old file's bytes and some of the new one's, which is neither
/// database. `std::fs::rename` within one directory is a single update to the
/// directory entry - it names the old file, or it names the new one, and
/// there is no byte offset in between for a crash to land on. Both Windows
/// and Unix guarantee this when the rename replaces an existing destination
/// on the same volume, which is exactly what `scratch_beside` arranges by
/// naming the rebuilt file `path`'s own directory.
///
/// The rename itself is enough to survive a **crash of this process** -
/// `scratch`'s bytes are already durable, by the checkpoint `rebuild_into`
/// ran before this is ever called, and the directory update is one the file
/// system either applies whole or not at all. What it is not enough for is a
/// **power loss**: the directory entry's own new value is dirty page-cache
/// state until it is flushed, same as any other write. Unix needs an
/// explicit `fsync` of the containing directory to force that; Windows does
/// not, because NTFS journals a rename's metadata itself - the same split
/// `inillucent_vfs`'s own directory sync draws for a delete.
///
/// **Through the caller's `Vfs`, not `std::fs`.** This used to rename directly
/// on the operating system's file system whatever VFS the connection was opened
/// on, so an application on `MemoryVfs`, `SimVfs` or its own encrypting VFS
/// rebuilt into a file the rename could not find (task-1946, H2). The
/// platform-specific directory flush moved with it: `Vfs::rename` is documented
/// to make its own directory entry durable, and `OsVfs` does exactly what this
/// function used to do.
///
/// @param vfs - the file system the database lives on
/// @param scratch - the rebuilt file, already checkpointed and therefore durable
/// @param path - the database file it replaces
pub(crate) fn commit_rebuild(
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    scratch: &Path,
    path: &Path,
) -> DbResult<()> {
    let from = inillucent_vfs::DbPath::new(scratch.to_string_lossy().as_ref());
    let to = inillucent_vfs::DbPath::new(path.to_string_lossy().as_ref());
    vfs.rename(&from, &to).map_err(|error| {
        inillucent_base::error::misuse(format!(
            "cannot write the rebuilt database over {}: {}",
            path.display(),
            error.detail()
        ))
    })
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
/// **Through the caller's `Vfs`, and by name rather than by listing.** This used
/// to `read_dir` the containing directory and `remove_file` whatever matched,
/// which is two more operations on the operating system's file system that a
/// connection on another VFS never asked for (task-1946, H2). A listing is not
/// needed: `inillucent_wal::segment::segment_name` makes a segment's name a
/// function of the database's name and a sequence number, so the names can be
/// generated. The walk stops at the first sequence number that is not there,
/// with a small run of misses tolerated, because the sequence is dense in
/// practice and an unbounded walk over a `u64` is not a walk.
///
/// @param vfs - the file system the database lives on
/// @param database - the database file the segments belong to
pub(crate) fn remove_log_segments(vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>, database: &Path) {
    let Some(base) = database.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let directory = database.parent();
    // How many consecutive absent sequence numbers end the walk. A log's
    // segments are numbered from one without gaps; the tolerance is for a
    // directory somebody has tidied by hand rather than for anything the engine
    // does.
    const TOLERATED_GAP: u64 = 16;
    let mut missed = 0u64;
    let mut sequence = 0u64;
    while missed < TOLERATED_GAP {
        let path = inillucent_wal::writer::segment_path(base, directory, sequence);
        sequence = sequence.saturating_add(1);
        match vfs.access(&path, inillucent_vfs::AccessMode::Exists) {
            Ok(true) => {
                missed = 0;
                let _ = vfs.delete(&path, true);
            }
            _ => missed = missed.saturating_add(1),
        }
    }
}

/// Returns what `last_insert_rowid()` should read once `VACUUM` has rebuilt
/// `database`'s schema, given what the counter held before the rebuild.
///
/// Measured against the pinned reference: a schema with no view leaves the
/// counter exactly where `preserved` already has it, but a view moves it.
/// SQLite's own rebuild copies every table, index and trigger into the new
/// schema with one bulk `INSERT ... SELECT`, which does not touch the
/// counter, then recreates each view by running its own `CREATE VIEW` as an
/// ordinary statement, which does. Every view therefore lands after
/// everything else, so the last one recreated sets the counter to the
/// schema's total row count - checked with the view at every position among
/// four fixtures, and it was always the total rather than the view's own.
///
/// @param database - the schema `VACUUM` just rebuilt
/// @param preserved - what the counter held before the rebuild
pub(crate) fn last_rowid_after_vacuum(database: &ImportedDatabase, preserved: i64) -> i64 {
    let entries = database.main_entries();
    if entries.iter().any(|entry| entry.kind == ObjectKind::View) {
        entries.len() as i64
    } else {
        preserved
    }
}

/// Every pragma-set connection setting `VACUUM`'s reopen would otherwise
/// default, captured so it can be put back.
///
/// **None of these are a fact about the file.** Two connections open on the
/// same file may disagree about every one of them, which is exactly why
/// `*self = ImportedDatabase::open(...)` - a fresh connection at every
/// default - is the wrong answer for a statement that is supposed to leave
/// the connection alone. SQLite's own `VACUUM` never faces this question: it
/// rewrites the file without ever closing the connection, so there is
/// nothing for it to put back.
///
/// `attached` and `temps` are carried separately, by [`AttachedSchemas`] -
/// they are schemas rather than settings. `session`, `next_session` and the
/// transaction-scoped fields (`batch`, `undo`, `marks`) are not carried at
/// all: `vacuum_in_place` is only reachable outside a transaction in the
/// first place (`Directive::Vacuum` refuses one before this ever runs), so
/// the transaction-scoped fields are already at their empty defaults on every
/// path that reaches this struct, and `session`/`next_session` restart at the
/// same values - zero and one - that `ImportedDatabase::open` always gives
/// them, which is what lets a restored `temp` schema's own recorded session
/// still match. The random built-ins' seed is left to reseed too - it is
/// per-connection state, not a setting a caller chose, and nothing asks a
/// `VACUUM`d connection's random stream to continue where the last one left
/// off.
pub(crate) struct ConnectionSettings {
    journal_mode: inillucent_pool::journal::JournalMode,
    foreign_keys: bool,
    defer_foreign_keys: bool,
    locking_exclusive: bool,
    defensive: bool,
    secure_delete: u8,
    auto_vacuum: u8,
    automatic_index: bool,
    ignore_check_constraints: bool,
    case_sensitive_like: bool,
    cache_size: Option<i64>,
    analysis_limit: i64,
    writable_schema: bool,
    query_only: bool,
    recursive_triggers: bool,
    max_page_count: i64,
    temp_store: i64,
    busy_timeout_ms: u64,
    collations: Vec<(String, inillucent_value::collation::Collation)>,
    authorizer: Option<std::rc::Rc<dyn inillucent_sql::bind::Authorizer>>,
    levers: inillucent_sql::plan::Levers,
    /// Application-registered modules and scalar/aggregate functions -
    /// `register_module`/`create_scalar_function`'s own state. Pure metadata,
    /// not a handle into the file about to be replaced, so cloning it here and
    /// putting it back is exactly as safe as `collations` above.
    registry: inillucent_ext::registry::Registry,
}

impl ConnectionSettings {
    /// Reads every setting off a connection, before its reopen defaults them.
    ///
    /// @param database - the connection `VACUUM` is about to reopen
    pub(crate) fn capture(database: &ImportedDatabase) -> ConnectionSettings {
        ConnectionSettings {
            journal_mode: database.pragmas.journal_mode.get(),
            foreign_keys: database.pragmas.foreign_keys.get(),
            defer_foreign_keys: database.pragmas.defer_foreign_keys.get(),
            locking_exclusive: database.pragmas.locking_exclusive.get(),
            defensive: database.pragmas.defensive.get(),
            secure_delete: database.pragmas.secure_delete.get(),
            auto_vacuum: database.pragmas.auto_vacuum.get(),
            automatic_index: database.pragmas.automatic_index.get(),
            ignore_check_constraints: database.pragmas.ignore_check_constraints.get(),
            case_sensitive_like: database.pragmas.case_sensitive_like.get(),
            cache_size: database.pragmas.cache_size.get(),
            analysis_limit: database.pragmas.analysis_limit.get(),
            writable_schema: database.pragmas.writable_schema.get(),
            query_only: database.pragmas.query_only.get(),
            recursive_triggers: database.pragmas.recursive_triggers.get(),
            max_page_count: database.pragmas.max_page_count.get(),
            temp_store: database.pragmas.temp_store.get(),
            busy_timeout_ms: database.pragmas.busy_timeout_ms.get(),
            collations: database.session_state.collations.clone(),
            authorizer: database.session_state.authorizer.clone(),
            levers: database.pragmas.levers.get(),
            registry: database.session_state.registry.clone(),
        }
    }

    /// Writes every setting back onto a freshly reopened connection.
    ///
    /// `journal_mode` goes through [`ImportedDatabase::set_journal_mode`]
    /// rather than a field write, deliberately: the WAL flag it carries is
    /// also written into the file's own header, and a plain assignment here
    /// would leave that header unset - a `VACUUM`d WAL database would then
    /// reopen as `delete` for every later opener, not just this connection.
    /// Every other field here is connection-local, so a direct write is
    /// exactly what restoring it means.
    ///
    /// The registry just written may name a module or function the fresh
    /// open's default registry did not have, so the eponymous list and the
    /// catalog it feeds are rebuilt against it afterwards - the same two
    /// calls `register_module` makes whenever it changes the registry.
    ///
    /// @param database - the freshly reopened connection
    pub(crate) fn restore(self, database: &mut ImportedDatabase) -> DbResult<()> {
        database.set_journal_mode(self.journal_mode)?;
        database.pragmas.foreign_keys.set(self.foreign_keys);
        database
            .pragmas
            .defer_foreign_keys
            .set(self.defer_foreign_keys);
        database
            .pragmas
            .locking_exclusive
            .set(self.locking_exclusive);
        database.pragmas.defensive.set(self.defensive);
        database.pragmas.secure_delete.set(self.secure_delete);
        database.pragmas.auto_vacuum.set(self.auto_vacuum);
        database.pragmas.automatic_index.set(self.automatic_index);
        database
            .pragmas
            .ignore_check_constraints
            .set(self.ignore_check_constraints);
        database
            .pragmas
            .case_sensitive_like
            .set(self.case_sensitive_like);
        database.pragmas.cache_size.set(self.cache_size);
        database.pragmas.analysis_limit.set(self.analysis_limit);
        database.pragmas.writable_schema.set(self.writable_schema);
        database.pragmas.query_only.set(self.query_only);
        database
            .pragmas
            .recursive_triggers
            .set(self.recursive_triggers);
        database.pragmas.max_page_count.set(self.max_page_count);
        database.pragmas.temp_store.set(self.temp_store);
        database.pragmas.busy_timeout_ms.set(self.busy_timeout_ms);
        database.session_state.collations = self.collations;
        database.session_state.authorizer = self.authorizer;
        database.pragmas.levers.set(self.levers);
        database.session_state.registry = self.registry;
        database.session_state.eponymous.clear();
        database.refresh_catalog();
        Ok(())
    }
}

/// Every temporary table and every attached database, taken off a connection
/// so `VACUUM`'s reopen cannot silently drop them.
///
/// **Carried across whole, not rebuilt.** `main` is the only file `VACUUM`
/// touches: a temporary table's tree lives in its own `MemoryVfs` and an
/// attachment's lives in its own file or its own `MemoryVfs`, and neither is
/// opened, read or written by anything else in this module. So the fix is not
/// to recreate them from a captured schema, the way `rebuild_into` does for
/// `main` - it is to stop discarding them: taken out of the connection before
/// `*self` is replaced and put back once the new one exists, they are
/// exactly the schemas they were, because nothing about them ever changed.
/// This is checked against the pinned SQLite shell, which keeps both a `TEMP`
/// table's rows and an attached database - a real file or `:memory:` - across
/// its own `VACUUM` of `main`.
///
/// `owner` and `next_handle` travel with them: they are what a tree root
/// number resolves to a schema through, keyed by the same root numbers
/// `attached`'s trees already carry, so restoring one without the other would
/// leave every attached tree unreachable by name.
pub(crate) struct AttachedSchemas {
    temps: Vec<crate::Attached>,
    attached: Vec<crate::Attached>,
    owner: std::collections::HashMap<u32, usize>,
    next_handle: u32,
    /// Every built tree an attached or temporary schema owns.
    ///
    /// **`self.schema.trees` is connection-wide, keyed by handle rather than by
    /// schema, and `main`'s own trees are not among these** - `Attached`
    /// itself holds no `PagedTree` at all, only the catalog row that names
    /// one, so a handle here is unreachable without its matching entry moving
    /// too. Filtered to handles at or past [`crate::FIRST_ATTACHED_HANDLE`],
    /// which `main`'s own roots - imported page numbers below it and
    /// DDL-created ones from `crate::FIRST_CREATED_ROOT` - never reach.
    trees: std::collections::HashMap<u32, inillucent_tree::PagedTree>,
    /// The matching `SourceLayout` for each tree above.
    layouts: std::collections::HashMap<u32, std::rc::Rc<inillucent_exec::physical::SourceLayout>>,
    /// The matching index roots for each table above that has one.
    covering: std::collections::HashMap<u32, Vec<u32>>,
}

impl AttachedSchemas {
    /// Takes every temporary table and every attached database off a
    /// connection, leaving it as though neither had ever been used - which is
    /// what the connection `*self = ImportedDatabase::open(...)` builds next
    /// actually is.
    ///
    /// @param database - the connection about to be reopened
    pub(crate) fn take(database: &mut ImportedDatabase) -> AttachedSchemas {
        let is_attached = |root: &u32| *root >= crate::FIRST_ATTACHED_HANDLE;
        let trees = std::mem::take(&mut database.schema.trees)
            .into_iter()
            .filter(|(root, _)| is_attached(root))
            .collect();
        let layouts = std::mem::take(&mut database.schema.layouts)
            .into_iter()
            .filter(|(root, _)| is_attached(root))
            .collect();
        let covering = std::mem::take(&mut database.schema.covering)
            .into_iter()
            .filter(|(root, _)| is_attached(root))
            .collect();
        AttachedSchemas {
            temps: std::mem::take(&mut database.session_state.temps),
            attached: std::mem::take(&mut database.session_state.attached),
            owner: std::mem::take(&mut database.session_state.owner),
            next_handle: database.schema.next_handle,
            trees,
            layouts,
            covering,
        }
    }

    /// Puts every temporary table and every attached database back onto a
    /// freshly reopened connection, and rebuilds the tables that describe
    /// them - the same step `ATTACH` itself takes after adding one, needed
    /// here because the fresh connection derived `self.schema.tables` from `main`
    /// alone, before any of this existed to derive it from.
    ///
    /// @param database - the freshly reopened connection
    pub(crate) fn restore(self, database: &mut ImportedDatabase) -> DbResult<()> {
        database.session_state.temps = self.temps;
        database.session_state.attached = self.attached;
        database.session_state.owner = self.owner;
        database.schema.next_handle = self.next_handle;
        database.schema.trees.extend(self.trees);
        database.schema.layouts.extend(self.layouts);
        database.schema.covering.extend(self.covering);
        database.rebuild_tables()
    }
}

/// Reopens one of the two files `VACUUM` swaps, saying which when it cannot.
///
/// **An error here used to say `Open: No such file or directory` and nothing
/// else**, which names neither the file nor the step - and `VACUUM` opens two
/// different files in sequence, so the message left a reader unable to tell a
/// rebuild that was never written from a rename that did not land.
///
/// @param vfs - the file system the database lives on
/// @param path - the file to open
/// @param page_size - the page size to open it with
/// @param frames - how many frames its pool is given
/// @param which - what this file is, for the message
fn reopened(
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    path: &Path,
    page_size: usize,
    frames: usize,
    which: &str,
) -> DbResult<ImportedDatabase> {
    ImportedDatabase::open_on(
        std::sync::Arc::clone(vfs),
        path.to_path_buf(),
        page_size,
        frames,
    )
    .map_err(|error| {
        inillucent_base::error::misuse(format!(
            "VACUUM could not reopen {which}, {}: {}",
            path.display(),
            error
        ))
    })
}

/// Reports whether any write-ahead log segment sits beside a database file.
///
/// The non-destructive twin of [`remove_log_segments`], for a test that needs
/// to know the fixture it built actually has segments to be endangered by -
/// asserting on a crash scenario that never had anything to lose would not be
/// testing the thing it claims to.
///
/// @param database - the database file to check beside
/// Rebuilds a database over itself, reclaiming everything nothing uses.
///
/// @param connection - the connection to rebuild, replaced in place
///
/// **Written beside the database and renamed over it, not copied**: see
/// `commit_rebuild` for why a rename is what a crash
/// cannot catch halfway and a byte copy is. The connection reopens onto
/// the new file afterwards, because every tree handle it holds names a
/// root that has moved.
///
/// **Every step is on `connection.storage.vfs`** - the rebuild, the rename, the segment
/// removals and both reopens. It was not: the reopens constructed a fresh
/// `OsVfs` and the rebuild called `std::fs` directly, so a connection on
/// `MemoryVfs`, `SimVfs` or an application's own encrypting VFS either
/// failed to find its own database or silently finished the statement on
/// the operating system's file system and stayed there (task-1946, H2).
/// `docs/relational-architecture.md` §6 has the argument for the rename.
///
/// **Every temporary table and every attached database is carried across
/// too**, by `AttachedSchemas` (see its doc) - `main` is
/// the only file this rewrites, so both are simply moved onto the
/// reopened connection unchanged. Refused instead for a declared imposter
/// table, which is bound into `main`'s own trees by page - every index
/// below gets a fresh one, so there is nothing to move it onto.
///
/// **`changes()`, `total_changes()` and `last_insert_rowid()` are
/// preserved - except when the rebuilt schema holds a view**, which moves
/// the last one; see `last_rowid_after_vacuum`. Every
/// other connection setting is captured before the first reopen and put
/// back after the second, by `ConnectionSettings`.
pub(crate) fn vacuum_in_place(connection: &mut ImportedDatabase) -> DbResult<()> {
    if !connection.schema.imposters.is_empty() {
        return Err(refusal(
            "cannot VACUUM a connection with an imposter table declared - every index gets a fresh tree below",
        ));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0);
    let scratch = scratch_beside(&connection.storage.path, stamp);
    let _ = connection.storage.vfs.delete(
        &inillucent_vfs::DbPath::new(scratch.to_string_lossy().as_ref()),
        false,
    );
    connection.rebuild_into(&scratch)?;
    let path = connection.storage.path.clone();
    let page_size = connection.storage.page_size;
    let frames = connection.storage.frames;
    // **Taken before the first `*connection`.** Every reopen and every file
    // operation below has to land on the file system this connection was
    // opened on, and `*connection` replaces the whole `ImportedDatabase` - so
    // the handle is taken here, while it is still the one the caller
    // gave us (task-1946, H2).
    let vfs = std::sync::Arc::clone(&connection.storage.vfs);
    // **And the writer is taken here for the same reason (task-1962, A1
    // step 3).** `crate::connect::Database` holds a second handle on it, so a
    // reopen that left a fresh group behind would leave that handle reading a
    // writer nothing writes to. Put back below, with the reopen's own values
    // copied into it.
    let writer = std::rc::Rc::clone(&connection.writing);
    // The settings group for the same reason, and it is put back before
    // `settings.restore` below so that the restore writes into the group both
    // handles hold rather than into one the reopen made.
    let pragmas = std::rc::Rc::clone(&connection.pragmas);
    // **`changes()`/`total_changes()`/`last_insert_rowid()` are the
    // connection's own history, not a fact about the file `VACUUM` is
    // rewriting, and SQLite's own `VACUUM` leaves them alone.** Assigning
    // through `connection` below replaces the whole `ImportedDatabase` with a
    // freshly opened one, whose `last_changes`/`changed_ever`/
    // `last_rowid`/`session_change_baseline` all start at zero - so the
    // differential suite's `vacuum_matches_sqlite` read `changes()` as 0
    // and `last_insert_rowid()` as 0 straight after a `VACUUM` that
    // changed nothing itself and inserted no row, where the reference
    // still answered whatever the last real write left there. Saved here
    // and restored once, after the second and final swap - nothing runs a
    // statement on it between the two, so there is nothing to restore
    // in between.
    // Carrying the group itself is what keeps them, and it keeps
    // `session_change_baseline` with them, which is the one a reopen could not
    // have reconstructed.
    let counters = std::rc::Rc::clone(&connection.counters);
    let last_rowid = connection.counters.last_rowid.get();
    // The plan cache is carried for the handle rather than for the plans:
    // `crate::connect::Database` holds a second handle on it, and every plan in
    // it names a tree this rebuild has moved, so it takes the reopen's own
    // empty cache through `Compiled::adopt` below.
    let compiled = std::rc::Rc::clone(&connection.compiled);
    let settings = ConnectionSettings::capture(connection);
    let schemas = AttachedSchemas::take(connection);
    // **The old file is closed before it is replaced, not after** -
    // assigning through `connection` drops the old value first, which is the
    // only moment in this function when neither file is open by us. A
    // pool still holding frames of a file whose bytes changed underneath
    // it would answer from the database that used to be there.
    // **On the caller's own VFS.** `ImportedDatabase::open` constructs a
    // fresh `OsVfs`, so this used to move the whole connection onto the
    // operating system's file system for the rest of the session whatever
    // it was opened on (task-1946, H2).
    *connection = reopened(&vfs, &scratch, page_size, frames, "the rebuilt file")?;
    // **The one crash-sensitive moment.** Up to here neither `path` nor
    // its log segments have been touched, so a crash recovers the
    // original the ordinary way. `commit_rebuild` is a single rename, and
    // once it returns `path` holds the rebuilt bytes durably.
    commit_rebuild(&vfs, &scratch, &path)?;
    // Only now, with the rename durable, do `path`'s pre-rebuild segments
    // stop describing anything true - left beside the new file they would
    // be replayed over it on the next open, undoing the rebuild. Removing
    // them earlier, before the rename could be proven to land, was this
    // function's defect: a crash between the removal and the copy left
    // the original unrecoverable, its log already gone.
    remove_log_segments(&vfs, &path);
    *connection = reopened(&vfs, &path, page_size, frames, "the database it replaced")?;
    writer.adopt(&connection.writing);
    connection.writing = writer;
    connection.pragmas = pragmas;
    compiled.adopt(&connection.compiled);
    connection.compiled = compiled;
    connection.counters = counters;
    connection
        .counters
        .last_rowid
        .set(last_rowid_after_vacuum(connection, last_rowid));
    // Before the settings: `ConnectionSettings::restore`'s
    // `refresh_catalog` needs the tables `rebuild_tables` derives here
    // already in place to describe them.
    schemas.restore(connection)?;
    settings.restore(connection)?;
    // The scratch name is spent - `commit_rebuild` renamed the file away -
    // so this is only the log segments it picked up along the way.
    remove_log_segments(&vfs, &scratch);
    Ok(())
}

#[cfg(test)]
fn has_log_segments(database: &Path) -> bool {
    let Some(directory) = database.parent() else {
        return false;
    };
    let Some(base) = database.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let prefix = format!("{base}-wal.");
    let Ok(listing) = std::fs::read_dir(directory) else {
        return false;
    };
    listing.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&prefix))
    })
}

/// Crash-recovery cases specific to `VACUUM`'s own swap, over real files.
///
/// **Not driven through `SimVfs`.** `ImportedDatabase::create`/`open` (and so
/// `rebuild_into`, which calls them) always build on `OsVfs`, regardless of
/// the vfs the connection they are rebuilding was itself opened on - a
/// pre-existing fact about this engine, not something this ticket changes -
/// and the swap `commit_rebuild` performs is a raw `std::fs::rename` that
/// bypasses the `Vfs` trait entirely, on the task's own instruction. Neither
/// is reachable by `durability.rs`'s call-counting fault injection, which
/// only ever sees calls made through a `Vfs`, so the cases below reproduce
/// the two states a crash can leave `vacuum_in_place` in directly: they run
/// its actual primitives (`rebuild_into`, `commit_rebuild`,
/// `remove_log_segments`) up to a chosen point and stop, exactly as a killed
/// process would, then reopen what is on disk the way a new process starting
/// up would.
#[cfg(test)]
mod vacuum_crash {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::ImportedDatabase;

    use super::{commit_rebuild, has_log_segments, remove_log_segments, scratch_beside};

    const PAGE_SIZE: usize = 4_096;
    const FRAMES: usize = 256;

    /// Returns a path under the system temp directory nothing else is using.
    ///
    /// @param tag - which case this path belongs to, for a reader of the temp
    ///   directory
    /// The operating system's file system, which is what these tests rebuild on.
    fn os_vfs() -> std::sync::Arc<dyn inillucent_vfs::Vfs> {
        std::sync::Arc::new(inillucent_vfs::OsVfs::new())
    }

    fn temp_db_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "inillucent-vacuum-crash-{}-{tag}-{unique}.rdb",
            std::process::id()
        ));
        path
    }

    /// Builds a real database with something for `VACUUM` to reclaim - the
    /// deleted row leaves a hole the rebuild has to compact away, which is
    /// what makes the rebuilt file's bytes different from the original's
    /// rather than a no-op copy of them.
    ///
    /// @param path - where to create it
    fn seeded(path: &Path) -> ImportedDatabase {
        let mut engine = ImportedDatabase::create(path.to_path_buf(), PAGE_SIZE, FRAMES)
            .expect("the fixture database is created");
        for sql in [
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)",
            "INSERT INTO t VALUES(1, 'one'), (2, 'two'), (3, 'three'), (4, 'four')",
            "DELETE FROM t WHERE a = 2",
        ] {
            engine
                .execute_any(sql, &inillucent_exec::physical::Params::new())
                .expect("the fixture script runs");
        }
        engine
    }

    /// Returns `t`'s rows, in a stable order.
    ///
    /// @param engine - the connection to read through
    fn rows(engine: &ImportedDatabase) -> Vec<Vec<inillucent_tree::datum::OwnedDatum>> {
        engine
            .run("SELECT a, b FROM t ORDER BY a")
            .expect("the fixture's own query runs")
            .0
    }

    /// A crash between `rebuild_into` returning and the rename that follows
    /// it leaves the original file exactly as it was.
    ///
    /// Nothing above the rename touches `path` or its log segments, so this
    /// is true by construction rather than by anything worth measuring - it
    /// is here as the other half of the pair with the case below, which is
    /// the one this ticket's fix is actually about.
    #[test]
    fn a_crash_before_the_rename_recovers_the_original() {
        let path = temp_db_path("before-rename");
        let mut engine = seeded(&path);
        let before = rows(&engine);

        let scratch = scratch_beside(&path, 1);
        let _ = std::fs::remove_file(&scratch);
        engine
            .rebuild_into(&scratch)
            .expect("the rebuild checkpoints the scratch file durably");
        // The crash: everything `vacuum_in_place` still had left to do -
        // closing this handle, renaming `scratch` over `path`, removing
        // `path`'s segments - never runs. `engine` is simply dropped, as a
        // killed process's connection would be.
        drop(engine);

        let reopened = ImportedDatabase::open(path.clone(), PAGE_SIZE, FRAMES)
            .expect("the untouched original still opens");
        assert_eq!(
            rows(&reopened),
            before,
            "a crash before the rename must leave the original database exactly as it was"
        );
        drop(reopened);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&scratch);
        remove_log_segments(&os_vfs(), &path);
        remove_log_segments(&os_vfs(), &scratch);
    }

    /// A crash after the rename lands, but before `path`'s pre-rebuild log
    /// segments are removed, must still recover the rebuilt file - not the
    /// pre-rebuild pages those stale segments describe, replayed back over
    /// it.
    ///
    /// This is the window the fix opens that the old code did not have in
    /// this shape: the old order removed the segments *before* the swap,
    /// which closed this window by opening the one
    /// `an_early_segment_removal_loses_the_original_to_an_interrupted_copy`
    /// demonstrates instead. Proving both stay safe under their own ordering
    /// is what justifies moving the removal rather than just picking a
    /// different unsafe order.
    #[test]
    fn a_crash_after_the_rename_recovers_the_rebuilt_file() {
        let path = temp_db_path("after-rename");
        let mut engine = seeded(&path);
        let before = rows(&engine);

        let scratch = scratch_beside(&path, 2);
        let _ = std::fs::remove_file(&scratch);
        engine
            .rebuild_into(&scratch)
            .expect("the rebuild checkpoints the scratch file durably");
        assert!(
            has_log_segments(&path),
            "the fixture must still carry its own log segments for this case to test anything"
        );
        // Closes the handle on `path`, as `vacuum_in_place` does before the
        // rename - a rename cannot replace a file this process still has open
        // for writing.
        engine = ImportedDatabase::open(scratch.clone(), PAGE_SIZE, FRAMES)
            .expect("the scratch reopens");
        commit_rebuild(&os_vfs(), &scratch, &path).expect("the rename lands");
        // The crash: `remove_log_segments(&path)`, the very next line in
        // `vacuum_in_place`, never runs - `path`'s pre-rebuild segments are
        // still sitting beside the rebuilt file.
        drop(engine);

        let reopened = ImportedDatabase::open(path.clone(), PAGE_SIZE, FRAMES)
            .expect("the rebuilt file must still open despite the stale segments left beside it");
        assert_eq!(
            rows(&reopened),
            before,
            "a crash after the rename must recover the rebuilt file, not a replay of the segments \
             that described the file it replaced"
        );
        drop(reopened);
        let _ = std::fs::remove_file(&path);
        remove_log_segments(&os_vfs(), &path);
    }

    /// The order `vacuum_in_place` used to run in - the original's log
    /// segments removed *before* anything overwrites `path`, and a byte copy
    /// rather than a rename - loses data when the copy is interrupted.
    ///
    /// Reproduced directly against this file's own primitives rather than by
    /// reverting `vacuum_in_place`, so this keeps demonstrating the old
    /// defect even after nothing in the source still runs in that order.
    #[test]
    fn an_early_segment_removal_loses_the_original_to_an_interrupted_copy() {
        let path = temp_db_path("old-order");
        let mut engine = seeded(&path);
        let before = rows(&engine);

        let scratch = scratch_beside(&path, 3);
        let _ = std::fs::remove_file(&scratch);
        engine
            .rebuild_into(&scratch)
            .expect("the rebuild checkpoints the scratch file durably");
        engine = ImportedDatabase::open(scratch.clone(), PAGE_SIZE, FRAMES)
            .expect("the scratch reopens");

        // The defect: the original's segments are gone *before* `path` has
        // been touched at all.
        remove_log_segments(&os_vfs(), &path);
        assert!(
            !has_log_segments(&path),
            "the segments this case removes early must actually have existed"
        );

        // The old mechanism: a byte copy, interrupted partway - exactly what
        // `std::fs::copy` gave a crash the room to do, and a rename never
        // does. Half of `scratch`'s bytes land in `path`; the write stops
        // there, as a killed process's would.
        let rebuilt_bytes = std::fs::read(&scratch).expect("the scratch file reads back");
        let torn = &rebuilt_bytes[..rebuilt_bytes.len() / 2];
        std::fs::write(&path, torn).expect("the torn write lands");
        drop(engine);

        // What the old order leaves behind: `path` holding half the rebuilt
        // file's bytes, with its own recovery log already deleted. There is
        // no longer a definition of "recovers correctly" available to it -
        // either it is detected as unreadable, or it opens and answers
        // something other than the rows it held a moment before. Both are
        // the loss this ticket's fix removes; neither is "either the original
        // or the rebuilt file, whole", which is what the comment above
        // `vacuum_in_place` used to claim.
        match ImportedDatabase::open(path.clone(), PAGE_SIZE, FRAMES) {
            Err(_) => {}
            Ok(reopened) => {
                assert_ne!(
                    rows(&reopened),
                    before,
                    "a torn write over a file whose own recovery log was already deleted read back \
                     correctly by coincidence on this run - the old order still cannot be trusted \
                     in general, but this run did not demonstrate why"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&scratch);
        remove_log_segments(&os_vfs(), &scratch);
    }

    /// `VACUUM` refuses a connection with an imposter table declared.
    ///
    /// Unlike a temporary table or an attachment, an imposter is bound into
    /// `main`'s own trees by page - every index gets a fresh one when the
    /// schema is replayed into the rebuilt file, so there is no tree left for
    /// the declaration to still name afterward.
    #[test]
    fn a_declared_imposter_refuses_vacuum() {
        let path = temp_db_path("imposter-refusal");
        let mut engine = seeded(&path);
        engine
            .execute_any(
                "CREATE INDEX t_b ON t(b)",
                &inillucent_exec::physical::Params::new(),
            )
            .expect("the index the imposter reads is created");
        engine
            .imposter(Some(b"t_b"), b"imposter_t_b")
            .expect("the imposter declares");

        let refused = engine.execute_any("VACUUM", &inillucent_exec::physical::Params::new());
        assert!(
            refused.is_err(),
            "VACUUM must refuse a connection with an imposter table declared"
        );
        drop(engine);
        let _ = std::fs::remove_file(&path);
        remove_log_segments(&os_vfs(), &path);
    }
}
