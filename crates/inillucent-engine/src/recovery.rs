//! Opening one database file and replaying its log into it.
//!
//! Invariant: **a file is recovered before anything reads a page out of it.**
//! Every path that opens a database of this engine - the file a connection is
//! opened on, and every file it `ATTACH`es - comes through `open_file`, so
//! there is one set of rules about what a torn tail means rather than one per
//! caller.
//!
//! Extracted from `lib.rs`, whose recorded size this pushed past. Nothing here
//! changed in the move; the recovery order and the reasoning for it are in the
//! comments on `open_file` itself.

use inillucent_base::DbResult;
use inillucent_catalog::paged::SchemaEntry;
use inillucent_pool::Database;
use inillucent_tree::PagedTree;
use inillucent_vfs::DbPath;
use inillucent_wal::{Wal, WalOptions, FIRST_LSN};

use crate::{attach_catalog, let_the_pool_ask_the_log, read_catalog};
use crate::{index_shape, keyed_table_shape, schema_layout, table_from_create_sql, table_shape};
use inillucent_base::error::refusal;
use inillucent_catalog::paged::ObjectKind;
use inillucent_pool::PageId;
use inillucent_tree::datum::Datum;
use inillucent_txn::redo::{RowRedo, TreeRows};

/// One database file, opened, recovered, and ready to be read.
pub(crate) struct OpenedFile {
    /// The pool, the meta page and the free map.
    pub(crate) database: Database,
    /// The log, positioned where recovery ended.
    pub(crate) wal: std::rc::Rc<Wal>,
    /// The catalog tree, attached from the meta page's root.
    pub(crate) catalog_tree: PagedTree,
    /// The highest transaction number any record recovery scanned carried.
    ///
    /// **A reopened database must not reuse a number the log still holds**, and
    /// this is what the engine's counter is started above. Recovery decides
    /// which records to replay by transaction number, so a number used twice in
    /// one log makes two different transactions into one - a run that wrote as
    /// transaction 3 and crashed leaves records the *next* run resurrects the
    /// moment its own transaction 3 commits, permanently.
    ///
    /// `inillucent-wal` has reported this since it was written and nothing read
    /// it; the cross-file commit is what made it load-bearing, because a marker
    /// naming transaction 7 in a file whose next run also calls something
    /// transaction 7 would suppress a commit that had nothing to do with it.
    pub(crate) highest_txn: u64,
}

/// Returns where the log resumes, raising it above every stamp the file carries.
///
/// **A page's LSN has to be a position in the stream currently beside the file,
/// and after a recovery whose chain was short of what the pages reflect it is
/// not.** Recovery applies a record to a page only when the page's
/// stamp is below the record's, so a page stamped by a stream that no longer
/// exists silently swallows every later write to it - the record is skipped,
/// the file stays structurally intact, and nothing anywhere says a committed row
/// was lost. It is how Nikaya's mail database ended up with page 3 stamped
/// 21,939,058,496 beside a log ending at 21,075,008,440, after 24 segments were
/// moved aside to recover it.
///
/// So the log resumes at `max(recovered.next_lsn, high_water + 1)`. In every
/// healthy file the first term already wins and this changes nothing: the
/// write-ahead rule puts every stamp below the log's durable end, and the
/// durable end is at or below where recovery stopped. It fires only on a file
/// whose log is short of what its pages carry.
///
/// Two things follow from an LSN being a **byte offset inside a segment**:
///
/// 1. The jump takes the *next* sequence. The write offset of a record is
///    `header + (lsn - segment.first_lsn)`, so resuming 864 million positions
///    into the segment recovery stopped in would ask for an 864 MB file.
/// 2. The meta page is checkpointed before a record is written at the new
///    position. That leaves a gap between the old segment's last byte and the
///    new one's first, and `read_chain` stops a chain at a gap - correctly,
///    since a gap is otherwise a lost segment - so the next recovery has to
///    start *inside* the new segment rather than walk up to it. The claim the
///    checkpoint makes is true at that moment: the replay's pages have just been
///    flushed, and there are no records between the chain's end and the new
///    position.
///
/// @param database - the recovered file, whose pool carries the high water
/// @param outcome - what recovery found
fn resume_above_every_stamp(
    database: &mut Database,
    outcome: &inillucent_wal::Recovered,
) -> DbResult<(u64, u64)> {
    let next_lsn = outcome.next_lsn.max(FIRST_LSN);
    let sequence = outcome.sequence.max(1);
    // Read off the pool rather than off the meta record. `Database::open` seeds
    // it with what the meta page carried and `Pool::writeback` has raised it for
    // every page this recovery has already evicted, so it is the higher of the
    // two and never the lower.
    let high_water = database.pool().high_water_lsn();
    if high_water < next_lsn {
        return Ok((next_lsn, sequence));
    }
    let resumed = high_water.saturating_add(1);
    let rolled = sequence.saturating_add(1);
    database.set_log_position(resumed, outcome.latest_cts, rolled);
    database.checkpoint()?;
    Ok((resumed, rolled))
}

/// Reads the catalog as the file's own pages currently show it, before redo.
///
/// A plain, checksummed page fetch - see `open_file`'s own comment on why a
/// caller of this may need to run a repair pass and ask again rather than
/// treat a failure here as final.
///
/// @param database - the file being opened
fn read_checkpointed_catalog(database: &Database) -> DbResult<Vec<SchemaEntry>> {
    let before = attach_catalog(database.pool(), database.catalog_root())?;
    read_catalog(database.pool(), &before)
}

/// Opens one database file, replays its log into it, and opens that log.
///
/// **The one recovery path, for the file a connection is opened on and for
/// every file it attaches.** An `ATTACH`ed database is an ordinary database of
/// this engine - it may have been written by a process that crashed, and a
/// second recovery path would be a second set of rules about what a torn tail
/// means. There is one, and both callers take it.
///
/// @param vfs - the file system the file and its log live on
/// @param db_path - the database file
/// @param frames - how many frames the buffer pool holds
/// @param doubtful - transactions whose `Commit` record is not the decision
pub(crate) fn open_file(
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    db_path: &DbPath,
    frames: usize,
    doubtful: &std::collections::BTreeSet<u64>,
) -> DbResult<OpenedFile> {
    let database = Database::open_before_recovery(vfs.as_ref(), db_path, frames.max(64))?;

    // **Recovery.** The log is replayed into the file before anything is read
    // out of it, which is what makes this an open rather than a reader of
    // whatever the last checkpoint happened to leave behind.
    //
    // It could not be done before a tree's identifier was stored in the
    // catalog. `TreeRows` is keyed by that identifier, every logical row record
    // carries it, and until then the writer's numbering and a reader's were
    // different - so a replay would have put rows into the wrong tree, which is
    // a wrong answer rather than a refusal.
    //
    // From the file's own checkpoint, not from the start of the log:
    // `RecoveryStart::fresh` scans from `FIRST_LSN` and would replay everything
    // the last checkpoint already applied.
    //
    // `doubtful` is how a cross-file commit reaches this. A transaction that
    // wrote two databases votes in each file's log and is *decided* by a
    // super-journal outside both, so a `Commit` record for one of those
    // transactions is a vote rather than the decision - see
    // `super_journal_doubt`.
    let meta = database.meta();
    let start = if meta.checkpoint_lsn == 0 {
        inillucent_wal::RecoveryStart {
            doubtful: doubtful.clone(),
            ..inillucent_wal::RecoveryStart::fresh(database.uuid())
        }
    } else {
        inillucent_wal::RecoveryStart {
            uuid: database.uuid(),
            checkpoint_lsn: meta.checkpoint_lsn,
            sequence: meta.wal_sequence,
            cts_watermark: meta.cts_watermark,
            doubtful: doubtful.clone(),
        }
    };
    let mut database = database;
    // The shapes come from the catalog as it stood at the last checkpoint, plus
    // the catalog tree itself, whose own rows are what a `CREATE TABLE` writes.
    // A record naming a tree that is in none of them - a table created *after*
    // the checkpoint, whose rows were then written - makes `TreeRows` refuse,
    // which fails this open with a named error rather than replaying into a
    // tree that is not the one meant.
    //
    // **The catalog root can itself be the page a crash tore, and reading it
    // here is what makes that unrepairable.** This read is an ordinary
    // checksummed page fetch, done before redo has run a single record, so a
    // checkpoint interrupted while rewriting the catalog's own page fails
    // exactly the way redo exists to fix - except redo cannot run first
    // either, because its row decoder needs the catalog's shapes to replay a
    // row record. Neither side can go first, which is what makes it a circle
    // rather than an ordering bug.
    //
    // It breaks like this: when this first read fails, a **repair pass** runs
    // ahead of the real one, tolerant of a record naming a tree it has not
    // been told the shape of - every other record it applies exactly as
    // normal, including the schema tree's own rows, whose shape
    // (`schema_layout()`) is fixed and needs no catalog at all. That is enough
    // whenever the log holds a record for the torn page, which it does for
    // the one case measured: `ddl.rs`'s `refresh_statistics` rewrites a
    // table's catalog row on every checkpoint whose shape changed, and that
    // rewrite is an ordinary schema-tree row record - the very thing this pass
    // can already replay without the catalog. The catalog is read again after
    // it; if it is still unreadable, the file is refused the way it always
    // was, unchanged - a corruption no record in the log describes is not
    // this pass's to fix, and `crates/inillucent-compat/tests/corruption.rs`
    // is what proves that stays true. Reached only on the error path, so an
    // ordinary open pays nothing extra: one read, one pass, exactly as before.
    let mut repaired = false;
    let checkpointed = match read_checkpointed_catalog(&database) {
        Ok(checkpointed) => checkpointed,
        Err(_) => {
            repaired = true;
            let mut repair =
                inillucent_txn::redo::Applier::new(&mut database, LearningRows::new_tolerant(&[]));
            inillucent_wal::recover(vfs.as_ref(), db_path, start.clone(), &mut repair)?;
            read_checkpointed_catalog(&database)?
        }
    };
    let (outcome, free_map) = {
        // **The second pass is tolerant exactly when the first one had to run
        // (task-1932, found by `reindex_crash.rs`).** The catalog it is seeded
        // with was read *after* the repair pass replayed the whole window, so
        // it is the catalog as at the END of the log, while the records it is
        // about to replay run from the start of it. A tree that was superseded
        // inside that window - which is what `REINDEX` and `CREATE INDEX` do,
        // every rebuild allocating a fresh tree - is therefore named by no row
        // this pass will ever see, and refusing its records failed the open
        // outright:
        //
        // ```text
        // bad parameter or other API misuse: the log names tree 2147483649,
        // which this recovery was not told the shape of
        // ```
        //
        // A database that had survived a crash during a `REINDEX` would not
        // open at all. Skipping those records is right rather than merely
        // convenient: the tree they name has been dropped by the end of the
        // window, so replaying them would write pages nothing will ever read.
        // When the checkpointed catalog was readable this stays strict, which
        // is every ordinary open.
        let mut applier = inillucent_txn::redo::Applier::new(
            &mut database,
            LearningRows::new_with_tolerance(&checkpointed, repaired),
        );
        let outcome = inillucent_wal::recover(vfs.as_ref(), db_path, start, &mut applier)?;
        (outcome, applier.free_map_changes().to_vec())
    };
    database.load_free_map()?;
    // The free map is rebuilt after the scan rather than inside it: the map and
    // every page write are both behind `&mut Database`, and one record cannot
    // hold two mutable borrows of the same object.
    //
    // **In log order.** Claiming every allocation and then
    // releasing every free gave the frees the last word, so a page freed and
    // allocated again inside the replayed range came back free while it was
    // live, and the next allocation handed it to a second owner. See
    // `Applier::free_map_changes`.
    for change in &free_map {
        match change.allocated {
            true => database.claim(change.page)?,
            false => database.release(change.page, 1)?,
        }
    }
    inillucent_wal::truncate_after(vfs.as_ref(), db_path, &outcome)?;

    // **The log resumes where recovery ended, not at the beginning.** Opening it
    // at `FIRST_LSN` with sequence 1 starts a second stream over the same
    // segments: the session writes records the *next* open cannot find, because
    // the meta page's checkpoint points into the first stream. A test caught it
    // as a table created after an open vanishing on the one after that -
    // `no such table: second` from a file that had just been told to make it.
    //
    // **And above every stamp the file carries.** See
    // `resume_above_every_stamp`.
    let (next_lsn, sequence) = resume_above_every_stamp(&mut database, &outcome)?;
    let wal = std::rc::Rc::new(Wal::open(
        std::sync::Arc::clone(vfs),
        db_path,
        database.uuid(),
        next_lsn,
        sequence,
        WalOptions::default(),
    )?);
    database.pool().set_durable_lsn(wal.write_ahead_point());
    // **Seeds `Pool::note_dirty_from`'s floor from this file's own last
    // checkpoint**, so a page whose stamp predates it cannot repeat, across a
    // reopen, the bug `Pool::retained_lsn`'s doc comment describes within one
    // session. `database.meta()` reflects `resume_above_every_stamp`'s own
    // corrective checkpoint above when it ran, so this always reads the
    // recovery point actually in force for this file right now, not the one
    // this open started from.
    database
        .pool()
        .set_retained_lsn(database.meta().checkpoint_lsn);
    let_the_pool_ask_the_log(database.pool(), &wal);

    // The catalog is read again, because recovery may have changed it: a
    // `CREATE TABLE` after the checkpoint is a row in this very tree.
    let catalog_tree = attach_catalog(database.pool(), database.catalog_root())?;
    Ok(OpenedFile {
        database,
        wal,
        catalog_tree,
        highest_txn: outcome.highest_txn,
    })
}

// **The replay applier lives here rather than in `lib.rs` (task-1932).** It is
// the half of recovery that decides what a log record means, and `open_file`
// above is the half that decides which records to replay; they were seven
// hundred lines apart in a file of nearly eight thousand. Nothing in them
// changed in the move.

/// A [`RowRedo`] that learns a tree's shape from the catalog rows it replays.
///
/// **The problem it solves.** `TreeRows` has to be told every tree's column
/// directory before the replay starts, and the only place to get one is the
/// catalog. But the catalog a reader can read before the replay is the catalog
/// as at the *last checkpoint* - so a table created after it, and then written
/// to, names a tree the applier has never heard of, and the replay refuses.
/// That is not a corner: a database that is created, given a schema and filled
/// without ever being checkpointed is the ordinary shape of a crash, and it is
/// exactly what `ImportedDatabase::create` followed by DDL produces.
///
/// **Why it works.** A `CREATE TABLE` is a row inserted into the catalog tree,
/// and the log is replayed in LSN order - so that row goes past *before* any row
/// of the tree it describes. Watching the catalog tree go by is therefore enough
/// to know every shape by the time it is needed, and it needs no second pass.
///
/// The catalog row is decoded by `inillucent-catalog`'s own decoder rather than
/// here, because a second decoder is a second opinion about which column is
/// which, and the columns are what the format is.
///
/// ## `seen` is a catalog, not a list of the rows that went past
///
/// It used to be the second, and that made a database unopenable after an
/// ordinary migration. `ALTER TABLE chunk ADD COLUMN embedded_at INTEGER`
/// rewrites the table's catalog row - a `DeleteRow` and an `InsertRow` - so a
/// list that only appends held **both** definitions, and `shape_of` reads the
/// owner table with `find`, which answers with the first. Every shape derived
/// after that ALTER therefore came from the definition before it.
///
/// What that costs is not a missing column in a report. `CREATE INDEX
/// chunk_embedded_at_idx ON chunk (embedded_at)` against a table with no
/// `embedded_at` resolves the key to no column at all, and `index_shape` gives
/// an unresolved key column `PhysicalType::Any` where the writer used
/// `PhysicalType::Int64`. An `Any` mini-column is wider, so recovery repacks the
/// index's leaves less densely than the process that wrote them - and the first
/// logical `CompactLeaf` replayed against such a leaf cannot fit rows that
/// demonstrably fitted when they were written. The open fails with
/// `database disk image is malformed`, and the whole database is unreachable
/// while the log is beside it.
///
/// It was measured on Nikaya's real 5.8 GB corpus: `chunk`'s catalog row was
/// rewritten at LSN 21,934,260,592 and the index's row written at
/// 21,936,659,792, both after the last checkpoint at 21,074,969,552; recovery
/// then refused a compaction of index leaf 237505 that held 2,031 live rows.
/// Replaying that page's records out of the parked log with the shape read off
/// the page fits every one of them, and with the first key column forced to
/// `Any` it fails at the same LSN with the same 2,031 rows.
///
/// So a row is *remembered* rather than appended: an entry replaces the one it
/// supersedes, by name and by tree identifier, and every shape is derived again
/// from the catalog as it now stands. Deriving them all again rather than only
/// the one that changed is what makes an ALTER reach the indexes on the table -
/// their own rows may have gone past already, and their shapes come from the
/// table's text rather than from their own.
struct LearningRows {
    /// The applier this delegates to, gaining trees as it goes.
    rows: TreeRows,
    /// The catalog as it now stands: at most one entry per object.
    seen: Vec<SchemaEntry>,
    /// Whether a record naming a tree this pass has no shape for is skipped
    /// rather than refused.
    ///
    /// Set for the repair pass `open_file` runs when the checkpointed catalog
    /// itself could not be read - see the module-level note above `open_file`
    /// on the catalog/redo circularity - **and for the second pass that
    /// follows one** (task-1932). That pass cannot yet know the shape of a
    /// tree whose `CREATE TABLE` predates the redo window, so a record naming
    /// one is not damage, it is a shape this pass was never going to have;
    /// refusing it would fail an open a second, catalog-aware pass is about to
    /// repair. What this pass exists to fix - the schema tree's own pages -
    /// has no such gap: `TreeRows::new` always knows `schema_layout()`,
    /// tolerant or not.
    ///
    /// The second pass needs it for the opposite reason: the catalog it is
    /// seeded with was read after the repair pass replayed the whole window,
    /// so it describes the END of the log while the records run from the start
    /// of it, and a tree superseded inside the window - every `REINDEX` and
    /// every `CREATE INDEX` allocates a fresh one - is named by no row it will
    /// ever see. An ordinary open, where the checkpointed catalog read, stays
    /// strict.
    tolerant: bool,
}

impl LearningRows {
    /// Returns an applier for the repair pass, told nothing but the schema
    /// tree's own fixed shape and asked to skip what it cannot yet decode.
    ///
    /// @param checkpointed - the catalog as at the last checkpoint, empty when
    ///   even that could not be read
    fn new_tolerant(checkpointed: &[SchemaEntry]) -> LearningRows {
        LearningRows::new_with_tolerance(checkpointed, true)
    }

    /// Returns an applier that already knows a catalog, tolerant or not.
    ///
    /// @param checkpointed - the catalog the pass starts from
    /// @param tolerant - whether an unknown tree is skipped rather than refused
    pub(crate) fn new_with_tolerance(checkpointed: &[SchemaEntry], tolerant: bool) -> LearningRows {
        let mut learning = LearningRows {
            rows: TreeRows::new().with_tree(
                inillucent_catalog::paged::SCHEMA_TREE_ID,
                schema_layout(),
                1,
            ),
            seen: checkpointed.to_vec(),
            tolerant,
        };
        learning.derive_every_shape();
        learning
    }

    /// Learns a tree's shape from a catalog row the replay is about to apply.
    ///
    /// Silent about a row it cannot make a shape of - a view, a trigger, an
    /// index whose table has not gone past yet - because the applier refuses by
    /// name if a record then needs it, and refusing there says which tree.
    ///
    /// @param row - the catalog row's encoded values
    fn learn(&mut self, row: &[u8]) {
        let Ok(values) = decode_row(row) else {
            return;
        };
        let Ok(entry) = inillucent_catalog::paged::entry_from_row(&values) else {
            return;
        };
        self.remember(entry);
        self.derive_every_shape();
    }

    /// Puts one catalog entry in place of the one it supersedes.
    ///
    /// Matched on the folded name **and** on the tree identifier: an
    /// `ALTER TABLE ... ADD COLUMN` rewrites the row under the same name, and an
    /// `ALTER TABLE ... RENAME TO` rewrites it under a new name with the same
    /// identifier. Both leave one entry behind, which is what the rest of this
    /// type assumes.
    ///
    /// @param entry - the entry the replay just read
    fn remember(&mut self, entry: SchemaEntry) {
        let name = entry.name.to_ascii_lowercase();
        let kind = entry.kind;
        let identifier = entry.tree_id;
        self.seen.retain(|held| {
            let same_name = held.kind == kind && held.name.to_ascii_lowercase() == name;
            let same_tree = identifier != 0 && held.tree_id == identifier;
            !same_name && !same_tree
        });
        self.seen.push(entry);
    }

    /// Derives every tree's shape again from the catalog as it now stands.
    ///
    /// Every one of them, not only the entry that changed: an index's columns
    /// come from its *table's* declaration, so a table whose row was just
    /// rewritten changes the shape of indexes whose own rows went past earlier.
    fn derive_every_shape(&mut self) {
        let mut rows = std::mem::take(&mut self.rows);
        for entry in &self.seen {
            let Ok(identifier) = identifier_of(entry) else {
                continue;
            };
            let Some((columns, key_columns)) = shape_of(entry, &self.seen, identifier) else {
                continue;
            };
            rows = rows.with_tree(u64::from(identifier), columns, key_columns);
        }
        self.rows = rows;
    }
}

/// Decodes a run of tagged values, which is how a row record carries a row.
///
/// @param row - the record's row bytes
fn decode_row(row: &[u8]) -> DbResult<Vec<Datum<'_>>> {
    let mut values = Vec::new();
    let mut at = 0usize;
    while at < row.len() {
        let (value, width) = Datum::decode_tagged(row.get(at..).unwrap_or(&[]))?;
        values.push(value);
        at = at.saturating_add(width);
    }
    Ok(values)
}

impl RowRedo for LearningRows {
    fn insert_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        row: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        if tree == inillucent_catalog::paged::SCHEMA_TREE_ID {
            self.learn(row);
        }
        let result = self.rows.insert_row(database, tree, page, row, lsn);
        self.tolerate_unknown_tree(tree, result)
    }

    fn delete_row(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        let result = self.rows.delete_row(database, tree, page, key, lsn);
        self.tolerate_unknown_tree(tree, result)
    }

    fn update_in_place(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        key: &[u8],
        column: u32,
        value: &[u8],
        lsn: u64,
    ) -> DbResult<()> {
        let result = self
            .rows
            .update_in_place(database, tree, page, key, column, value, lsn);
        self.tolerate_unknown_tree(tree, result)
    }

    fn compact_leaf(
        &mut self,
        database: &mut Database,
        tree: u64,
        page: PageId,
        lsn: u64,
        from_lsn: u64,
    ) -> DbResult<()> {
        let result = self.rows.compact_leaf(database, tree, page, lsn, from_lsn);
        self.tolerate_unknown_tree(tree, result)
    }
}

impl LearningRows {
    /// Turns "this pass was never told the shape of that tree" into success,
    /// for a tree the catalog does not name and on the repair pass.
    ///
    /// **A tree no catalog row names has been dropped, and its records are for
    /// pages that belong to something else now (task-1932).** Every rebuild
    /// allocates a fresh tree and moves the catalog row onto it - `REINDEX`
    /// does, and so does `CREATE INDEX` - so a replay window that spans one
    /// holds records naming a tree the catalog at the end of it has no row for.
    /// Refusing those failed the open outright, which meant a database that had
    /// survived a crash during a `REINDEX` **could not be opened at all**:
    ///
    /// ```text
    /// bad parameter or other API misuse: the log names tree 2147483649,
    /// which this recovery was not told the shape of
    /// ```
    ///
    /// `reindex_crash.rs` found it, deterministically, at the fifty-fifth cut
    /// of both journal modes.
    ///
    /// **What makes skipping right rather than merely convenient** is that the
    /// log names a tree before it describes any of its pages: `create_index`
    /// and `rebuild_index` both write the catalog row naming the new tree
    /// before they fill it, in the same transaction and superseded by the row
    /// at the end of the statement. So a record for a tree with no row is never
    /// one whose row has not gone past yet - it is a tree that has been
    /// dropped, and writing its pages back would overwrite whatever owns them
    /// now.
    ///
    /// **Never for the schema tree.** Its shape is fixed
    /// (`with_tree(SCHEMA_TREE_ID, schema_layout(), 1)` in every constructor),
    /// so a refusal naming it is never this gap - it is a genuinely damaged
    /// catalog row, and has to be refused the way it always was. And never for
    /// any other failure a row's own redo can raise - a bad key, a page that is
    /// not a leaf - which name a real defect this pass must not hide. A tree
    /// the catalog *does* name and whose shape could not be derived is still
    /// refused, which is the gap `autoindex_reopen.rs` and `analyze_reopen.rs`
    /// were written for.
    ///
    /// @param tree - the tree the record named
    /// @param result - what the delegated redo answered
    fn tolerate_unknown_tree(&self, tree: u64, result: DbResult<()>) -> DbResult<()> {
        let named = self.seen.iter().any(|entry| entry.tree_id == tree);
        if (!self.tolerant && named) || tree == inillucent_catalog::paged::SCHEMA_TREE_ID {
            return result;
        }
        match result {
            Err(error)
                if error.detail().is_some_and(|detail| {
                    detail.ends_with("which this recovery was not told the shape of")
                }) =>
            {
                Ok(())
            }
            other => other,
        }
    }
}

/// Returns a catalog entry's tree shape, for the recovery applier.
///
/// The same derivations the open uses below, in the one form the applier wants:
/// the column directory and how many leading columns form the key. An entry
/// whose declaration will not parse - or an index whose table is not in the
/// catalog - answers `None`, and the applier then refuses any record naming it
/// rather than replaying into a shape it guessed.
///
/// @param entry - the catalog row
/// @param catalog - every row, so an index can find its table
/// @param identifier - the tree's identifier
fn shape_of(
    entry: &SchemaEntry,
    catalog: &[SchemaEntry],
    identifier: u32,
) -> Option<(Vec<crate::ColumnSpec>, usize)> {
    match entry.kind {
        ObjectKind::Table => {
            let mut info = table_from_create_sql(&entry.sql, 0, identifier).ok()?;
            info.root = identifier;
            if info.without_rowid {
                let (columns, key_columns, _) = keyed_table_shape(&info).ok()?;
                Some((columns, key_columns))
            } else {
                let (columns, _) = table_shape(&info);
                Some((columns, 1))
            }
        }
        ObjectKind::Index => {
            let folded = entry.table.to_ascii_lowercase();
            // **The newest matching entry, not the first.** `LearningRows`
            // keeps one entry per object, so there is only ever one - but a
            // caller that hands this a catalog holding a superseded definition
            // as well should get the definition that superseded it, because the
            // shape derived from the older one is what made a database
            // unopenable after an `ALTER TABLE`.
            let owner = catalog.iter().rev().find(|held| {
                held.kind == ObjectKind::Table && held.name.to_ascii_lowercase() == folded
            })?;
            let mut table = table_from_create_sql(&owner.sql, 0, owner.tree_id as u32).ok()?;
            table.root = owner.tree_id as u32;
            // **An automatic index is declared by the *table's* text.** The
            // catalog stores an empty `sql` for one - which is what SQLite
            // writes for `sqlite_autoindex_t_1` - so parsing that empty text as
            // a `CREATE INDEX` answers nothing, the applier is told no shape,
            // and every log record naming the index is refused. That is a
            // database with a `TEXT PRIMARY KEY` that cannot be reopened after
            // a write, and it is what this branch exists to prevent; the same
            // rule is applied by `ImportedDatabase::shape_of` when the schema
            // is loaded.
            let index = if entry.sql.is_empty() {
                let folded = entry.name.to_ascii_lowercase();
                table
                    .indexes
                    .iter()
                    .find(|index| index.folded == folded)?
                    .clone()
            } else {
                inillucent_catalog::load::index_from_create_sql(&entry.sql, &table, identifier)
                    .ok()?
            };
            let (columns, _) = index_shape(&table, &index, identifier);
            let key_columns = columns.len();
            Some((columns, key_columns))
        }
        _ => None,
    }
}

/// Returns the identifier a catalog row registers its tree under.
///
/// **Refused rather than defaulted.** A zero here is a row written before the
/// identifier was persisted, and guessing one would put the tree back in the
/// state this change exists to leave: a number the writer did not use, which
/// recovery would follow to the wrong tree. A file that does not say is a file
/// this engine will not open.
///
/// @param entry - the catalog row
pub(crate) fn identifier_of(entry: &SchemaEntry) -> DbResult<u32> {
    if entry.tree_id == 0 {
        return Err(refusal(format!(
            "the catalog row for {} carries no tree identifier; the database was created before catalog rows carried one and has to be rebuilt",
            String::from_utf8_lossy(&entry.name)
        )));
    }
    u32::try_from(entry.tree_id).map_err(|_| {
        refusal(format!(
            "the catalog row for {} carries a tree identifier that does not fit",
            String::from_utf8_lossy(&entry.name)
        ))
    })
}
