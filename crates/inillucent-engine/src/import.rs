//! Building a database from a SQLite file, one phase at a time.
//!
//! Invariant: **the import writes a file, closes it, opens it again, and
//! compares what came back against what it wrote.** Everything below the
//! reopen reads a file it did not write, which is what makes the format
//! round-trip a checked claim rather than an assumption - and it is why the
//! phases are in this order rather than a more convenient one.
//!
//! ## Why it is seven functions (M4)
//!
//! `ImportedDatabase::import_into` was 493 lines and did seven things:
//! validation, catalog creation, schema import, row movement, index work,
//! verification and reporting. The review that split it put the cost plainly -
//! a function that long cannot be reviewed locally, so a change to one of its
//! seven jobs is reviewed by somebody holding the other six in their head.
//!
//! The phases are named for what they produce rather than for when they run,
//! and each takes what it needs and returns what it made. [`Carried`] is the
//! state the middle phases share; it exists because four of them genuinely
//! build one thing between them - the schema of the file being written - and
//! passing eight vectors between four functions would be the same coupling
//! with more punctuation.

use std::path::Path;

use super::*;

/// What the source file is, read once.
///
/// The declarations are the `CREATE` text SQLite stored, keyed by folded name.
/// The catalog view derives columns, collations and key order from that text
/// but does not keep an index's copy of it, and the new file's catalog has to
/// store the real declaration rather than one reconstructed from the
/// derivation - a reconstruction that round-trips today is one that stops
/// round-tripping at the first syntax the renderer forgets.
pub(crate) struct Source {
    /// The SQLite file, open for reading.
    file: SqliteFile,
    /// Its schema, parsed by the one loader this workspace has.
    loaded: inillucent_catalog::snapshot::DatabaseCatalog,
    /// The `CREATE` text, by `(kind, folded name)`.
    declarations: HashMap<(String, String), String>,
}

/// The schema the import is building, as it is built.
///
/// Four phases add to it and one reads it. It is one struct rather than eight
/// arguments because they genuinely describe one thing - the schema of the
/// file being written - and because `entries` and `identifiers` are parallel:
/// splitting them across signatures is how they get out of step.
pub(crate) struct Carried {
    /// The binder's view of the schema.
    pub(crate) catalog: StaticCatalog,
    /// The `TableInfo`s the catalog is built from, kept so DDL can rebuild it.
    pub(crate) tables: Vec<TableInfo>,
    /// How each tree's columns map onto a record, by source root.
    pub(crate) layouts: HashMap<u32, std::rc::Rc<SourceLayout>>,
    /// The indexes that cover every row of a table, by the table's source root.
    pub(crate) covering: HashMap<u32, Vec<u32>>,
    /// Each written tree's shape, by source root.
    pub(crate) shapes: HashMap<u32, TreeShape>,
    /// What the import could not take, named rather than silently absent.
    ///
    /// "The query returned nothing" and "the table was never imported" are
    /// different failures and only one of them is a bug in the engine.
    pub(crate) skipped: Vec<String>,
    /// What goes into the file's own catalog tree.
    ///
    /// Built as the import runs, because a table's root page in *this* file is
    /// only known once it has been written - which is the whole difference
    /// between this catalog and the one it was imported from.
    pub(crate) entries: Vec<SchemaEntry>,
    /// The identifier each entry's tree is registered under, in `entries` order.
    ///
    /// For an imported object that is the fixture's SQLite root page, which is
    /// not the page the row's `rootpage` column names.
    pub(crate) identifiers: Vec<u32>,
}

impl Carried {
    /// Returns the state an import starts from.
    ///
    /// **Not `Default`, and the difference is not cosmetic.**
    /// `StaticCatalog::default()` has an empty database list;
    /// `StaticCatalog::empty()` registers `main`. Deriving `Default` here made
    /// every imported database answer `no such table` for every table it had
    /// just written, because nothing resolved in a catalog with no databases
    /// in it. That is the whole reason this is a named constructor.
    pub(crate) fn new() -> Carried {
        Carried {
            catalog: StaticCatalog::empty(),
            tables: Vec::new(),
            layouts: HashMap::new(),
            covering: HashMap::new(),
            shapes: HashMap::new(),
            skipped: Vec::new(),
            entries: Vec::new(),
            identifiers: Vec::new(),
        }
    }
}

/// Opens the source file and reads its schema.
///
/// @param path - the SQLite database to read
pub(crate) fn read_source(path: PathBuf) -> DbResult<Source> {
    let mut file = SqliteFile::open(path.clone())?;
    // One schema reader, not two: `inillucent-catalog`'s loader parses every
    // CREATE TABLE and CREATE INDEX and attaches each index to its table
    // with its key columns, collations and descending flags. Re-deriving
    // any of that here would be a second implementation that could
    // disagree with the binder about what the schema says - and the binder
    // is the thing whose plans this import has to satisfy.
    let loaded = file.catalog(b"main")?;
    // The `CREATE` text as SQLite stored it, keyed by folded name. The
    // catalog view derives columns, collations and key order from this text
    // but does not keep an index's copy of it, and the new file's catalog
    // has to store the real declaration rather than one reconstructed from
    // the derivation - a reconstruction that round-trips today is a
    // reconstruction that stops round-tripping at the first syntax the
    // renderer forgets.
    let declarations: HashMap<(String, String), String> = file
        .schema()?
        .into_iter()
        .map(|object| {
            (
                (object.kind.clone(), object.name.to_ascii_lowercase()),
                object.sql,
            )
        })
        .collect();
    Ok(Source {
        file,
        loaded,
        declarations,
    })
}

/// Creates the destination file, replacing whatever was at the path.
///
/// The target is removed first, so a caller that stages into a fresh name gets
/// a fresh file and one that reuses a name gets a rebuild rather than a merge.
///
/// @param target - the file to build
/// @param page_size - the page size to build the new trees at
/// @param frames - how many frames the buffer pool holds
pub(crate) fn create_destination(
    target: &Path,
    page_size: usize,
    frames: usize,
) -> DbResult<(std::sync::Arc<dyn inillucent_vfs::Vfs>, DbPath, Database)> {
    let vfs: std::sync::Arc<dyn inillucent_vfs::Vfs> = std::sync::Arc::new(OsVfs::new());
    let _ = std::fs::remove_file(target);
    let db_path = DbPath::new(target.to_string_lossy().as_ref());
    let database = Database::create(
        vfs.as_ref(),
        &db_path,
        Options::default()
            .with_page_size(page_size)
            .with_frames(frames.max(64)),
    )?;
    Ok((vfs, db_path, database))
}

/// Carries every table the source holds, and each one's indexes.
///
/// @param database - the file being built
/// @param source - the file being read
/// @param carried - the schema being built
pub(crate) fn carry_tables(
    database: &mut Database,
    source: &mut Source,
    carried: &mut Carried,
) -> DbResult<()> {
    // The virtual tables the source declares, so the storage they own can be
    // told from an application's own tables. A module's shadow tables are in
    // SQLite's format and mean nothing to this engine's module of the same
    // name; `inillucent-migrate` rebuilds a full-text table from its content
    // instead. Importing them would also collide with the shadow tables that
    // rebuild then tries to create.
    let module_owners: Vec<Vec<u8>> = source
        .loaded
        .tables
        .iter()
        .filter(|held| held.kind == inillucent_sql::catalog_view::TableKind::Virtual)
        .map(|held| held.folded.clone())
        .collect();
    for info in &source.loaded.tables {
        if info.root == 0 {
            continue;
        }
        if is_shadow_of(&module_owners, &info.folded) {
            continue;
        }
        // **`sqlite_sequence` is carried; the rest of the reserved prefix is
        // not.** It is not bookkeeping this engine can rebuild: it holds
        // the AUTOINCREMENT high-water mark, and a table whose high rows
        // were deleted reuses their keys without it - which is the one
        // thing AUTOINCREMENT exists to prevent, failing silently on the
        // first insert after a migration. This engine keeps the same table
        // under the same name (`inillucent_exec::sequence`), so importing
        // it is importing a table rather than translating a concept.
        //
        // **`sqlite_stat1` is carried too**, because reading the other
        // engine's statistics is a stated invariant of this one rather
        // than a convenience - see `inillucent_catalog::analyze`. Dropping
        // it on import made that invariant hold in one direction only: a
        // file SQLite had `ANALYZE`d arrived here with no statistics at
        // all, and the first join over it was planned by the estimates the
        // measurements exist to replace. It is the same three-column table
        // under the same name, so carrying it is carrying a table.
        //
        // The rest of the reserved prefix is indexes and shadow state this
        // engine derives for itself, where a stale copy would be worse
        // than a fresh derivation.
        if info.name.starts_with(b"sqlite_")
            && !info
                .name
                .eq_ignore_ascii_case(inillucent_exec::sequence::SEQUENCE_TABLE)
            && !info
                .name
                .eq_ignore_ascii_case(inillucent_catalog::analyze::STAT1.as_bytes())
        {
            continue;
        }
        // A `WITHOUT ROWID` table is keyed by its primary key rather than
        // by a rowid, so its pages are index pages and its rows are index
        // entries. Phase 2 skipped it and named it in `skipped()`; Phase 3
        // takes it, because thirteen of the read-only SLT corpus's
        // thirty-seven refusals were the `teams` table and every query that
        // joined it.
        //
        // The new format needs no special case at all - it is a tree whose
        // key is more than one column, which every index already is. What
        // it needs is the *reader* to know the record's field order, and
        // SQLite records that in `primary_key_position`.
        let imported = if info.without_rowid {
            import_keyed_table(database, &mut source.file, info)
        } else {
            import_table(database, &mut source.file, info)
        };
        let (shape, layout) = imported.map_err(|error| unreadable(error, &info.name))?;
        carried.entries.push(SchemaEntry {
            kind: ObjectKind::Table,
            name: info.name.clone(),
            table: info.name.clone(),
            root: shape.root,
            sql: info.create_sql.clone(),
            stats: stats_of(&shape),
            // The identifier this tree is registered and logged under. The
            // import numbers by the *source* file's root pages, which is
            // arbitrary but stable, and putting it in the catalog is what
            // makes it the number a later open derives rather than invents.
            tree_id: u64::from(info.root),
        });
        carried.identifiers.push(info.root);
        carried.shapes.insert(info.root, shape);
        carried.layouts.insert(info.root, std::rc::Rc::new(layout));
        // **A descending index is imported descending**, which is how
        // SQLite stores it and what makes the two engines read one in the
        // same order.
        //
        // It used to be dropped and named in `skipped`, then imported
        // *flattened* to ascending - both were workarounds for trees that
        // could only be built one way round. The direction was then made
        // real (see `ColumnSpec::descending`), so the flattening became the
        // lie: the schema text this file carries says `DESC`, a reopen
        // parses it and the planner believes it, and a tree built ascending
        // under a catalog that says descending is sorted by neither. It
        // showed up as `SELECT k FROM t WHERE k > 1000` returning every row
        // in the table. `in_key_order` sorts into the tree's own order,
        // which now includes the direction.
        for index in &info.indexes {
            if index.root == 0 {
                continue;
            }
            // A `WITHOUT ROWID` table's primary-key index *is* the table:
            // SQLite reports it at the table's own root page, because there
            // is only one b-tree. Importing it as an index would overwrite
            // the table's layout with one that carries the key columns and
            // nothing else - which is what happened, and the symptom was
            // `SELECT * FROM teams` refusing with "the tree read for FROM
            // term 0 does not carry record slot 1".
            if info.without_rowid && index.root == info.root {
                continue;
            }
            let (shape, layout) =
                import_index(database, &mut source.file, info, index, index.root)?;
            carried.entries.push(SchemaEntry {
                kind: ObjectKind::Index,
                name: index.name.clone(),
                table: info.name.clone(),
                root: shape.root,
                tree_id: u64::from(index.root),
                // An automatic index - one a UNIQUE or PRIMARY KEY
                // constraint produced - has no CREATE text of its own in
                // SQLite either, and is reconstructed from the table's
                // declaration when the catalog is read back.
                sql: source
                    .declarations
                    .get(&(
                        "index".to_string(),
                        String::from_utf8_lossy(&index.name).to_ascii_lowercase(),
                    ))
                    .map(|sql| sql.as_bytes().to_vec())
                    .unwrap_or_default(),
                stats: stats_of(&shape),
            });
            carried.identifiers.push(index.root);
            carried.shapes.insert(index.root, shape);
            carried.layouts.insert(index.root, std::rc::Rc::new(layout));
            if covers_every_row(index) {
                carried
                    .covering
                    .entry(info.root)
                    .or_default()
                    .push(index.root);
            }
        }
        carried.tables.push(info.clone());
        // Taken and put back, because `with_table` consumes the catalog and
        // the field is behind a `&mut`.
        carried.catalog = std::mem::take(&mut carried.catalog).with_table(info.clone());
    }
    Ok(())
}

/// Carries the objects that have no tree: views and triggers.
///
/// They are rows in the schema and nothing else - a view is a query the binder
/// resolves when a statement reads through it, and a trigger is a body the
/// write path fires. Dropping them made an imported database answer
/// `no such table` for a view the fixture declared, which reads as a schema the
/// source never had rather than as a refusal.
///
/// @param source - the file being read
/// @param carried - the schema being built
pub(crate) fn carry_treeless(source: &Source, carried: &mut Carried) {
    // **The objects that have no tree**, which the loop above skipped
    // because it skipped `info.root == 0`. A view and a trigger are rows in
    // the schema and nothing else: a view is a query the binder resolves
    // when a statement reads through it, and a trigger is a body the write
    // path fires. Dropping them made an imported database answer
    // `no such table: blue` for a view the fixture declared - which reads
    // as a schema the source never had rather than as a refusal.
    for info in &source.loaded.tables {
        if info.kind != inillucent_sql::catalog_view::TableKind::View {
            continue;
        }
        carried.entries.push(SchemaEntry {
            kind: ObjectKind::View,
            name: info.name.clone(),
            table: info.name.clone(),
            root: PageId::NONE,
            sql: info.create_sql.clone(),
            stats: Default::default(),
            // A view has no tree, so it has no identifier either. Zero is
            // what `Recorded.root` documents for an object with none, and
            // it is what a virtual table's row already carries.
            tree_id: 0,
        });
        carried.identifiers.push(0);
        carried.tables.push((*info).clone());
        carried.catalog = std::mem::take(&mut carried.catalog).with_table((*info).clone());
    }
    // A trigger belongs to a table rather than to itself, so it is written
    // out of the table it is attached to. The declaration comes from the
    // source's own `sqlite_schema` text rather than from the parsed body:
    // the text is the definition, and rendering one back would lose
    // whatever the printer does not know how to write.
    for info in &source.loaded.tables {
        for trigger in &info.triggers {
            let Some(sql) = source.declarations.get(&(
                "trigger".to_string(),
                String::from_utf8_lossy(&trigger.name).to_ascii_lowercase(),
            )) else {
                carried
                    .skipped
                    .push(String::from_utf8_lossy(&trigger.name).into_owned());
                continue;
            };
            carried.entries.push(SchemaEntry {
                kind: ObjectKind::Trigger,
                name: trigger.name.clone(),
                table: info.name.clone(),
                root: PageId::NONE,
                sql: sql.as_bytes().to_vec(),
                stats: Default::default(),
                tree_id: 0,
            });
            carried.identifiers.push(0);
        }
    }
}

/// Writes the catalog tree, which names every root the import allocated.
///
/// @param database - the file being built
/// @param carried - the schema that was built
pub(crate) fn write_schema(database: &mut Database, carried: &Carried) -> DbResult<TreeShape> {
    // The catalog tree goes in last, because it names every root page the
    // import allocated.
    //
    // It holds no row for itself, which is not an omission - SQLite's
    // `sqlite_schema` has never listed `sqlite_schema`, because the reader
    // has to be able to find the catalog before it can read anything, so
    // the catalog's own location lives where the reader looks first. Here
    // that is the meta page's `catalog_root`; in SQLite it is page 1. The
    // table is still fully queryable: it is registered in the binder's
    // catalog below from a declaration this crate holds, and the scan that
    // answers a query against it is the ordinary one.
    let catalog_tree = write_catalog(database, &carried.entries)?;
    let catalog_shape = TreeShape {
        root: catalog_tree.root(),
        columns: schema_layout(),
        key_columns: 1,
        first_leaf: catalog_tree.first_leaf(),
        leaf_count: catalog_tree.leaf_count(),
        row_count: catalog_tree.row_count(),
    };
    Ok(catalog_shape)
}

/// Closes the file, opens it again, and checks what came back.
///
/// **This is the phase the order exists for.** Everything after it reads a file
/// it did not write, and the catalog is read back out of the file and compared
/// against what was written - which is what makes the catalog tree load bearing
/// rather than decorative: a file whose own account of itself disagreed would
/// say so here rather than in a query's wrong answer much later.
///
/// @param database - the file that was built, consumed by the close
/// @param vfs - the file system it lives on
/// @param db_path - where it is
/// @param frames - how many frames the reopened pool holds
/// @param carried - the schema that was written, to compare against
pub(crate) fn reopen_and_verify(
    database: Database,
    vfs: &std::sync::Arc<dyn inillucent_vfs::Vfs>,
    db_path: &DbPath,
    frames: usize,
    carried: &Carried,
) -> DbResult<Database> {
    let mut database = database;
    // Load, checkpoint, close - the TDD's Phase 2 lifecycle, and the only way
    // to know the format round-trips.
    database.checkpoint()?;
    drop(database);
    let database = Database::open(vfs.as_ref(), db_path, frames.max(64))?;
    let stored = read_catalog(
        database.pool(),
        &attach_catalog(database.pool(), database.catalog_root())?,
    )?;
    if stored != carried.entries {
        return Err(inillucent_base::error::corrupt(
            "the catalog read back from the file is not the one written to it",
        ));
    }
    Ok(database)
}

/// Returns the refusal a table whose rows could not be read produces.
///
/// **A refusal, not a skip (task-1979, M1).** A table the reader could not walk
/// used to be dropped and named in a `skipped` list nothing on the migration
/// path reads - so one flipped bit in a leaf page of a 500 row table produced a
/// published, integrity-clean database with the table gone entirely and exit
/// code 0, while real SQLite still read all 500 rows from the same file.
/// Reading fewer tables than the source has is the one outcome a migration must
/// never report as success.
///
/// @param error - what the read failed with
/// @param name - the table it was reading
fn unreadable(error: inillucent_base::DbError, name: &[u8]) -> inillucent_base::DbError {
    let said = error.detail().unwrap_or_default().to_string();
    let named = String::from_utf8_lossy(name).into_owned();
    error
        .with_message(format!(
            "the rows of {named} could not be read from the source database"
        ))
        .with_detail(format!("the rows of {named} could not be read: {said}"))
}
