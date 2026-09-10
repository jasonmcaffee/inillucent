//! Running the statements the session carries out itself.
//!
//! Invariant: DDL runs inside exactly the machinery DML runs inside. It opens
//! a statement level, writes ordinary rows into `sqlite_schema` through the
//! same pager, moves the schema cookie in the same header write, and commits
//! or rolls back with the same code - so a `CREATE TABLE` that fails half-way
//! leaves the file the way an `INSERT` that failed half-way would, and there is
//! no second recovery path that could be wrong.
//!
//! Transaction control is the exception that proves it: `BEGIN`, `COMMIT`,
//! `ROLLBACK`, `SAVEPOINT` and `RELEASE` are the statements that *change* which
//! transaction is open, so they cannot run inside one. They go straight to the
//! connection.

use crate::pragma::argument_text;
use crate::settings::Setting;
use inillucent_base::error::misuse;
use inillucent_base::DbResult;
use inillucent_catalog::ddl::{self, SchemaRow};
use inillucent_catalog::{analyze, rename};
use inillucent_sql::ast::ObjectKind;
use inillucent_sql::directive::{AlterKind, BeginKind, Directive, PragmaArgument};
use inillucent_storage::schema::SchemaKind;
use inillucent_storage::wal::CheckpointMode;
use inillucent_transaction::journal::{JournalMode, Synchronous};
use inillucent_transaction::state::BeginMode;
use inillucent_value::Value;

use crate::connection::{Access, Connection, Outcome};

/// The rows a directive reports, if any.
pub type DirectiveRows = Vec<Vec<Value<'static>>>;

/// Runs one directive and returns whatever it reports.
///
/// `source` is the statement's own SQL, which the DDL cases slice the
/// canonical `sqlite_schema` text out of.
pub fn run_directive(
    connection: &Connection,
    directive: &Directive,
    source: &[u8],
) -> DbResult<DirectiveRows> {
    match directive {
        Directive::Begin(kind) => {
            connection.begin_transaction(begin_mode(*kind))?;
            Ok(Vec::new())
        }
        Directive::Commit => {
            connection.commit_transaction()?;
            Ok(Vec::new())
        }
        Directive::Rollback { savepoint: None } => {
            connection.rollback_transaction()?;
            Ok(Vec::new())
        }
        Directive::Rollback {
            savepoint: Some(name),
        } => {
            connection.rollback_to_savepoint(name)?;
            Ok(Vec::new())
        }
        Directive::Savepoint(name) => {
            connection.open_savepoint(name)?;
            Ok(Vec::new())
        }
        Directive::Release(name) => {
            connection.release_savepoint(name)?;
            Ok(Vec::new())
        }
        Directive::CreateTable { .. } => run_write(connection, directive, |connection| {
            create_table(connection, directive, source)
        }),
        // `CREATE TABLE ... AS SELECT` is answered by the new engine
        // and not by this one, which is on its way out with the
        // rest of the crates the rearchitecture is deleting. A refusal here is
        // what it always was; what changed is only that the shape now has a
        // directive of its own rather than being refused in the binder.
        Directive::CreateTableAsSelect { .. } => Err(inillucent_base::DbError::primary(
            inillucent_base::PrimaryCode::Misuse,
        )
        .with_message("unsupported: CREATE TABLE ... AS SELECT")),
        Directive::CreateVirtualTable { .. } => run_write(connection, directive, |connection| {
            create_virtual_table(connection, directive, source)
        }),
        Directive::Analyze { .. } => run_write(connection, directive, |connection| {
            analyze(connection, directive)
        }),
        Directive::Alter { .. } => run_write(connection, directive, |connection| {
            alter_table(connection, directive, source)
        }),
        Directive::Reindex { .. } => run_write(connection, directive, |connection| {
            reindex(connection, directive)
        }),
        Directive::Vacuum { .. } => vacuum(connection, directive),
        Directive::CreateView { .. } => run_write(connection, directive, |connection| {
            create_view(connection, directive, source)
        }),
        Directive::CreateIndex { .. } => run_write(connection, directive, |connection| {
            create_index(connection, directive, source)
        }),
        Directive::CreateTrigger { .. } => run_write(connection, directive, |connection| {
            create_trigger(connection, directive, source)
        }),
        Directive::Drop { .. } => run_write(connection, directive, |connection| {
            drop_object(connection, directive)
        }),
        Directive::Attach { file, schema } => {
            connection.attach(file, schema)?;
            Ok(Vec::new())
        }
        Directive::Detach { schema } => {
            connection.detach(schema)?;
            Ok(Vec::new())
        }
        Directive::Pragma {
            database,
            name,
            argument,
        } => pragma(connection, *database, name, argument.as_ref()),
    }
}

/// Creates a virtual table: its shadow tables, its row, and its connection.
///
/// The order matters and is SQLite's. The shadow tables are made first, because
/// the module is about to be connected with `creating` set and may write into
/// them - FTS5 writes its configuration row there and then reads it back on
/// every later open. The virtual table's own row is written next, with a root
/// page of zero: it has no b-tree of its own, and the zero is what says so.
///
/// A module that refuses leaves nothing behind, because the whole directive
/// runs inside the statement's own transaction and a failure rolls it back.
fn create_virtual_table(
    connection: &Connection,
    directive: &Directive,
    source: &[u8],
) -> DbResult<DirectiveRows> {
    let Directive::CreateVirtualTable {
        database,
        name,
        module,
        arguments,
        name_offset,
        exists,
        ..
    } = directive
    else {
        return Err(misuse("not a CREATE VIRTUAL TABLE"));
    };
    if *exists {
        return Ok(Vec::new());
    }
    let registry = connection.with_state(|state| std::sync::Arc::clone(&state.registry))?;
    let Some(found) = registry.module(module) else {
        // A statement error rather than a misuse: the caller's API use was
        // correct and the *statement* named something that is not there, which
        // is the same class of failure as naming a table that does not exist.
        return Err(
            inillucent_base::DbError::primary(inillucent_base::PrimaryCode::Error).with_detail(
                format!("no such module: {}", String::from_utf8_lossy(module)),
            ),
        );
    };
    if !found.constructible() {
        return Err(
            inillucent_base::DbError::primary(inillucent_base::PrimaryCode::Error).with_detail(
                format!(
                    "{} is an eponymous-only module and cannot be created",
                    String::from_utf8_lossy(module)
                ),
            ),
        );
    }
    let reference = inillucent_vm::program::VirtualRef {
        database: *database,
        table: name.clone(),
        module: inillucent_sql::vtab::ModuleRef {
            folded: module.to_ascii_lowercase(),
            name: module.clone(),
            arguments: arguments.clone(),
        },
    };
    let shadow_tables =
        found.shadow_tables(&crate::vtab::arguments_of(&reference, b"main", Vec::new()))?;
    for shadow in &shadow_tables {
        let sql = shadow
            .create_sql
            .replace('%', &String::from_utf8_lossy(name));
        crate::statement::execute_batch(connection, sql.as_bytes())?;
    }
    let sql = ddl::canonical_sql(
        "CREATE VIRTUAL TABLE",
        source,
        *name_offset,
        source.len() as u32,
    );
    connection.with_database(*database, |pager| {
        ddl::insert_schema_row(
            pager,
            &SchemaRow {
                kind: SchemaKind::Table,
                name: name.clone(),
                table: name.clone(),
                // A virtual table has no b-tree, and zero is how the file says
                // so. Every reader - this engine's catalog and SQLite's - tells
                // a virtual table from an ordinary one by exactly this.
                root: 0,
                sql: Some(sql.clone()),
            },
        )
    })??;
    connection.with_database(*database, ddl::bump_schema_cookie)??;
    // The schema is re-read before the module is connected, because the module
    // is about to be handed the roots of its shadow tables and those roots are
    // in the schema this statement has just written.
    connection.refresh_catalog()?;
    // The module is connected with `creating` set once the shadow tables and
    // the row are both there, so that anything it writes lands in a schema that
    // already describes it.
    let shadows = {
        let catalog = connection.catalog()?;
        crate::vtab::shadow_roots(&catalog, *database, name)
    };
    connection.with_state(|state| -> DbResult<()> {
        let registry = std::sync::Arc::clone(&state.registry);
        let limits = state.limits.clone();
        let (mut table, _, _) =
            crate::vtab::connect(&registry, &reference, b"main", shadows, true)?;
        {
            let mut services = crate::connection::ConnectionServices {
                state,
                database: *database,
            };
            let mut context = inillucent_ext::vtab::Context {
                host: &mut services,
                database: *database,
                limits: &limits,
                catalog: None,
            };
            table.begin(&mut context)?;
            table.sync(&mut context)?;
            table.commit(&mut context)?;
        }
        let handle = std::rc::Rc::clone(&state.virtual_tables);
        if let Ok(mut tables) = handle.try_borrow_mut() {
            tables.insert(crate::vtab::key_of(&reference), table);
        }
        Ok(())
    })??;
    Ok(Vec::new())
}

/// Returns the transaction mode a `BEGIN` keyword names.
fn begin_mode(kind: BeginKind) -> BeginMode {
    match kind {
        BeginKind::Deferred => BeginMode::Deferred,
        BeginKind::Immediate => BeginMode::Immediate,
        BeginKind::Exclusive => BeginMode::Exclusive,
    }
}

/// Runs a write inside a statement level, undoing it if it fails.
fn run_write(
    connection: &Connection,
    directive: &Directive,
    body: impl FnOnce(&Connection) -> DbResult<DirectiveRows>,
) -> DbResult<DirectiveRows> {
    connection.begin_statement_on(Access::Schema, &[target_database(directive)])?;
    let outcome = body(connection);
    let ending = if outcome.is_ok() {
        Outcome::Done
    } else {
        Outcome::Abort
    };
    let closed = connection.end_statement(Access::Schema, ending);
    let rows = outcome?;
    closed?;
    Ok(rows)
}

/// Returns the database a directive writes.
///
/// A schema statement names it - `CREATE TABLE aux.t` - and the writer has to
/// be taken on that file rather than on `main`. A directive that names none
/// writes `main`, which is what an unqualified statement means.
fn target_database(directive: &Directive) -> usize {
    match directive {
        Directive::CreateTable { database, .. }
        | Directive::CreateTableAsSelect { database, .. }
        | Directive::CreateVirtualTable { database, .. }
        | Directive::Alter { database, .. }
        | Directive::Reindex { database, .. }
        | Directive::Vacuum { database, .. }
        | Directive::Analyze { database, .. }
        | Directive::CreateView { database, .. }
        | Directive::CreateIndex { database, .. }
        | Directive::CreateTrigger { database, .. }
        | Directive::Drop { database, .. } => *database,
        _ => inillucent_storage::MAIN_DATABASE,
    }
}

/// Creates a table, its automatic indexes, and its `sqlite_schema` rows.
fn create_table(
    connection: &Connection,
    directive: &Directive,
    source: &[u8],
) -> DbResult<DirectiveRows> {
    let Directive::CreateTable {
        database,
        name,
        name_offset,
        exists,
        ..
    } = directive
    else {
        return Err(misuse("not a CREATE TABLE"));
    };
    if *exists {
        // `IF NOT EXISTS` on an existing table: nothing happens, and nothing
        // is written, so the cookie does not move either.
        return Ok(Vec::new());
    }
    let sql = ddl::canonical_sql("CREATE TABLE", source, *name_offset, source.len() as u32);
    // A WITHOUT ROWID table's b-tree is an *index* b-tree - its cells carry a
    // key and no rowid - so its root page has to be created as one. A table
    // page here would be read back with the wrong cell format.
    let without_rowid = inillucent_catalog::table_from_create_sql(&sql, 0, 0)
        .map(|table| table.without_rowid)
        .unwrap_or(false);
    let root = connection.with_database(*database, |pager| {
        if without_rowid {
            ddl::allocate_index_root(pager)
        } else {
            ddl::allocate_table_root(pager)
        }
    })??;
    connection.with_database(*database, |pager| {
        ddl::insert_schema_row(
            pager,
            &SchemaRow {
                kind: SchemaKind::Table,
                name: name.clone(),
                table: name.clone(),
                root,
                sql: Some(sql.clone()),
            },
        )
    })??;
    // The automatic indexes a UNIQUE or PRIMARY KEY constraint implies are
    // separate objects with their own root pages and their own rows, and
    // SQLite writes them with a NULL `sql` because the table's own CREATE text
    // is their definition. Reconstructing them here from the same parse the
    // catalog uses is what keeps the two in step.
    let table = inillucent_catalog::table_from_create_sql(&sql, 0, root)?;
    for (ordinal, index) in table.indexes.iter().enumerate() {
        if index.root == root {
            // The primary key of a WITHOUT ROWID table *is* the table's own
            // b-tree. SQLite writes it no `sqlite_autoindex` row, and writing
            // one would leave a second object claiming the same root page.
            continue;
        }
        let root = connection.with_database(*database, ddl::allocate_index_root)??;
        let index_name = ddl::automatic_index_name(name, ordinal.saturating_add(1) as u32);
        let _ = index;
        connection.with_database(*database, |pager| {
            ddl::insert_schema_row(
                pager,
                &SchemaRow {
                    kind: SchemaKind::Index,
                    name: index_name.clone(),
                    table: name.clone(),
                    root,
                    sql: None,
                },
            )
        })??;
    }
    if table.autoincrement {
        // The sequence table is created with the first AUTOINCREMENT table, as
        // SQLite creates it, so that every later INSERT can be compiled against
        // a root that already exists.
        sequence_root(connection)?;
    }
    connection.with_database(*database, ddl::bump_schema_cookie)??;
    connection.refresh_catalog()?;
    Ok(Vec::new())
}

/// The name of the table an `AUTOINCREMENT` key remembers its counter in.
const SEQUENCE: &[u8] = b"sqlite_sequence";

/// The text SQLite stores for `sqlite_sequence`, byte for byte.
const SEQUENCE_SQL: &str = "CREATE TABLE sqlite_sequence(name,seq)";

/// Returns `sqlite_sequence`'s root, creating the table when it is first owed.
///
/// The same shape `sqlite_stat1` gets: SQLite creates neither until something
/// needs it, and both are ordinary tables with a `sqlite_` name, so they are
/// written here rather than through the binder - which refuses that prefix, and
/// should.
fn sequence_root(connection: &Connection) -> DbResult<u32> {
    let catalog = connection.catalog()?;
    let existing = {
        use inillucent_sql::catalog_view::CatalogView;
        catalog.find_table(None, SEQUENCE).map(|table| table.root)
    };
    if let Some(root) = existing {
        if root != 0 {
            return Ok(root);
        }
    }
    let root = connection.with_state(|state| ddl::allocate_table_root(&mut state.pager))??;
    connection.with_state(|state| {
        ddl::insert_schema_row(
            &mut state.pager,
            &SchemaRow {
                kind: SchemaKind::Table,
                name: SEQUENCE.to_vec(),
                table: SEQUENCE.to_vec(),
                root,
                sql: Some(SEQUENCE_SQL.as_bytes().to_vec()),
            },
        )
    })??;
    Ok(root)
}

/// Runs `ALTER TABLE`.
///
/// Every form is the same three steps: rewrite the stored `CREATE` text of the
/// table and of everything that names it, check each rewrite still parses, and
/// only then write them all. The check before the write is what makes it safe:
/// an `ALTER` that produced text the parser cannot read would leave a database
/// whose schema fails to load, which is not recoverable from inside the engine.
fn alter_table(
    connection: &Connection,
    directive: &Directive,
    source: &[u8],
) -> DbResult<DirectiveRows> {
    let Directive::Alter {
        database,
        table,
        action,
    } = directive
    else {
        return Err(misuse("not an ALTER TABLE"));
    };
    let folded = table.to_ascii_lowercase();
    let rows = connection.with_database(*database, ddl::read_schema_rows)??;

    // Every rewrite is computed first, and one that fails aborts the whole
    // statement before anything is written.
    let mut updates: Vec<(i64, SchemaRow)> = Vec::new();
    for (rowid, row) in &rows {
        let Some(sql) = row.sql.as_ref() else {
            // An automatic index has no SQL of its own - the table's own text is
            // its definition - so there is nothing to rewrite, but its
            // `tbl_name` still follows a rename.
            if row.table.to_ascii_lowercase() == folded {
                if let AlterKind::RenameTable { to } = action {
                    let mut moved = row.clone();
                    moved.table = to.clone();
                    moved.name = renamed_automatic(&row.name, table, to);
                    updates.push((*rowid, moved));
                }
            }
            continue;
        };
        let owns = row.table.to_ascii_lowercase() == folded;
        let itself = row.name.to_ascii_lowercase() == folded && row.kind == SchemaKind::Table;
        let rewritten = match action {
            AlterKind::RenameTable { to } => {
                // A view or trigger anywhere in the schema may name the table in
                // its body, so every row is offered the rewrite; one that does
                // not mention it comes back unchanged.
                let next = rename::rewrite(sql, rename::Rename::Table, table, to)?;
                if next == *sql && !owns {
                    continue;
                }
                let mut moved = row.clone();
                moved.sql = Some(rename::reparsed(next)?);
                if itself {
                    moved.name = to.clone();
                    moved.table = to.clone();
                } else if owns {
                    moved.table = to.clone();
                }
                moved
            }
            AlterKind::RenameColumn { from, to } => {
                // The table's own text, its indexes and its triggers all name
                // its columns unambiguously. So does a view or trigger that
                // reads only this table; one that reads two is ambiguous - a
                // bare column name in it could belong to either - and is
                // refused rather than rewritten on a guess.
                if !owns {
                    let reads = rename::referenced_tables(sql);
                    if !reads.contains(&folded) {
                        continue;
                    }
                    if reads.len() > 1 {
                        return Err(misuse(format!(
                            "error in {}: cannot rename a column it reads alongside another table",
                            String::from_utf8_lossy(&row.name)
                        )));
                    }
                }
                let next = rename::rewrite(sql, rename::Rename::Column, from, to)?;
                if next == *sql {
                    continue;
                }
                let mut moved = row.clone();
                moved.sql = Some(rename::reparsed(next)?);
                moved
            }
            AlterKind::AddColumn { start, end, .. } => {
                if !itself {
                    continue;
                }
                let definition = source
                    .get(*start as usize..*end as usize)
                    .unwrap_or_default()
                    .to_vec();
                let mut moved = row.clone();
                moved.sql = Some(rename::reparsed(rename::add_column(sql, &definition)?)?);
                moved
            }
            AlterKind::DropColumn { position, .. } => {
                if !itself {
                    continue;
                }
                let mut moved = row.clone();
                moved.sql = Some(rename::reparsed(rename::drop_column(
                    sql,
                    usize::from(*position),
                )?)?);
                moved
            }
        };
        updates.push((*rowid, rewritten));
    }

    if let AlterKind::DropColumn { position, .. } = action {
        let root = rows
            .iter()
            .find(|(_, row)| {
                row.kind == SchemaKind::Table && row.name.to_ascii_lowercase() == folded
            })
            .map(|(_, row)| row.root)
            .unwrap_or(0);
        drop_column_values(connection, root, usize::from(*position))?;
    }

    for (rowid, row) in &updates {
        connection.with_database(*database, |pager| {
            ddl::update_schema_row(pager, *rowid, row)
        })??;
    }
    connection.with_database(*database, ddl::bump_schema_cookie)??;
    connection.refresh_catalog()?;
    Ok(Vec::new())
}

/// Returns the new name of an automatic index when its table is renamed.
///
/// SQLite names them `sqlite_autoindex_<table>_<n>`, so the name has to follow
/// the table or the next `CREATE TABLE` of the old name would collide with it.
fn renamed_automatic(name: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let prefix = b"sqlite_autoindex_";
    let Some(rest) = name.strip_prefix(prefix.as_slice()) else {
        return name.to_vec();
    };
    let Some(suffix) = rest.strip_prefix(from) else {
        return name.to_vec();
    };
    let mut out = prefix.to_vec();
    out.extend_from_slice(to);
    out.extend_from_slice(suffix);
    out
}

/// Rewrites every row of a table with one record slot removed.
///
/// The rows are read whole before any is written: rewriting under the cursor
/// that is reading them would have the scan walk over pages the write had just
/// rebalanced.
fn drop_column_values(connection: &Connection, root: u32, position: usize) -> DbResult<()> {
    let Some(root) = inillucent_base::ids::PageId::new(root) else {
        return Ok(());
    };
    let rewritten = connection.with_state(|state| -> DbResult<Vec<(i64, Vec<u8>)>> {
        let limits = inillucent_base::limits::Limits::default();
        let encoding = state.pager.text_encoding();
        let format = state.pager.header().schema_format.max(1);
        let mut cursor = inillucent_storage::cursor::BTreeCursor::table(root);
        let mut out = Vec::new();
        let mut more = cursor.first(&mut state.pager)?;
        while more {
            let rowid = cursor.rowid()?;
            let payload = cursor.payload(&mut state.pager, &limits)?;
            let record = inillucent_value::record::RecordRef::parse_with_limits(
                &payload, encoding, &limits,
            )?;
            let mut values = Vec::with_capacity(record.field_count());
            for column in 0..record.field_count() {
                if column == position {
                    continue;
                }
                values.push(record.value(column)?.into_owned()?);
            }
            out.push((
                rowid,
                inillucent_value::record::encode_record(&values, encoding, format)?,
            ));
            more = cursor.next(&mut state.pager)?;
        }
        Ok(out)
    })??;
    for (rowid, payload) in &rewritten {
        connection.with_state(|state| {
            inillucent_storage::mutate::insert_row(&mut state.pager, root, *rowid, payload)
        })??;
    }
    Ok(())
}

/// Runs `REINDEX`: empties each index and fills it from its table again.
///
/// The refill goes through the same `build_index` a `CREATE INDEX` uses rather
/// than through a second implementation, which is the point of the statement:
/// an index rebuilt by different code from the one that built it could be
/// rebuilt *wrongly* and nothing would notice, because the thing that checks an
/// index is the index.
fn reindex(connection: &Connection, directive: &Directive) -> DbResult<DirectiveRows> {
    let Directive::Reindex { database, indexes } = directive else {
        return Err(misuse("not a REINDEX"));
    };
    for name in indexes {
        let found = {
            use inillucent_sql::catalog_view::CatalogView;
            let catalog = connection.catalog()?;
            let folded = name.to_ascii_lowercase();
            catalog
                .find_index(None, &folded)
                .map(|(table, index)| (table.clone(), index.clone()))
        };
        let Some((table, index)) = found else {
            continue;
        };
        let Some(root) = inillucent_base::ids::PageId::new(index.root) else {
            continue;
        };
        connection.with_database(*database, |pager| {
            inillucent_storage::mutate::clear_tree(pager, root)
        })??;
        backfill_index(connection, *database, name)?;
        let _ = table;
    }
    Ok(Vec::new())
}

/// Runs `VACUUM`: moves every free page off the end of the file and truncates.
///
/// On an auto-vacuum database this is the incremental vacuum run to completion,
/// which is exactly what SQLite's own `PRAGMA incremental_vacuum` does with no
/// limit, and it reaches the file the format specifies. The page-relocation and
/// pointer-map bookkeeping then has one implementation rather than two.
///
/// On a database that is *not* in auto-vacuum mode there are no pointer maps,
/// so a page cannot be moved without finding every reference to it - and SQLite
/// does not try either: it rebuilds the whole database into a fresh file and
/// swaps it. That rebuild needs a second pager and the file-swap protocol, both
/// of which belong to the backup service in phase 9, so it is refused here
/// rather than reported as having done something it did not do. A `VACUUM` that
/// silently no-ops is worse than one that says it cannot: the first leaves a
/// fragmented file and a person who believes otherwise.
fn vacuum(connection: &Connection, directive: &Directive) -> DbResult<DirectiveRows> {
    let Directive::Vacuum { into, .. } = directive else {
        return Err(misuse("not a VACUUM"));
    };
    if !connection.autocommit() {
        // `SQLITE_ERROR`, not misuse: the reference reports this as an ordinary
        // statement error, and an application that switches on the primary code
        // would see a different one.
        return Err(
            inillucent_base::DbError::primary(inillucent_base::PrimaryCode::Error)
                .with_message("cannot VACUUM from within a transaction"),
        );
    }
    // One statement level for the whole thing. The rebuild reads every page of
    // this database, so it needs the read transaction held across the copy; and
    // holding the write lock for the duration is what stops another connection
    // changing the file between the rebuild and the copy back.
    connection.begin_statement(Access::Schema)?;
    let outcome = match into {
        Some(path) => vacuum_into(connection, path),
        None => vacuum_in_place(connection),
    };
    let ending = if outcome.is_ok() {
        Outcome::Done
    } else {
        Outcome::Abort
    };
    let closed = connection.end_statement(Access::Schema, ending);
    outcome?;
    closed?;
    Ok(Vec::new())
}

/// Runs `VACUUM INTO`: writes a rebuilt copy and leaves this database alone.
fn vacuum_into(connection: &Connection, path: &[u8]) -> DbResult<()> {
    let name = String::from_utf8_lossy(path).into_owned();
    let target = std::path::PathBuf::from(&name);
    if target.exists() {
        // SQLite refuses rather than overwriting, and so must this: the whole
        // point of INTO is that nothing existing is touched.
        return Err(misuse(format!("output file already exists: {name}")));
    }
    build_rebuild(connection, &target)
}

/// Runs `VACUUM`: rebuilds the database into a temporary file and copies it
/// back.
///
/// The copy back happens inside this database's own write transaction, so it is
/// journalled like any other write and a crash half way through leaves the
/// database it started with. Building the new file first and then copying is
/// what makes that possible: a rebuild in place would have to move every root
/// page while statements were still compiled against them.
fn vacuum_in_place(connection: &Connection) -> DbResult<()> {
    let scratch = scratch_path(connection)?;
    let _ = std::fs::remove_file(&scratch);
    let outcome = (|| -> DbResult<()> {
        build_rebuild(connection, &scratch)?;
        copy_back(connection, &scratch)?;
        // Inside the statement, while the pager can still be read: every root
        // page in the file is new, and loading the catalog needs a read
        // transaction. Refreshing after the statement closed left the snapshot
        // pointing at the roots the rebuild had just replaced.
        connection.refresh_catalog()
    })();
    // The temporary file is this statement's own, so it goes whether the
    // rebuild worked or not.
    let _ = std::fs::remove_file(&scratch);
    let mut journal = scratch.clone().into_os_string();
    journal.push("-journal");
    let _ = std::fs::remove_file(std::path::PathBuf::from(journal));
    outcome
}

/// Returns the path the rebuilt copy is built at, beside the database itself.
///
/// Beside it rather than in the system temporary directory, because the rebuild
/// is as large as the database and the directory holding one is the only place
/// known to have room for the other.
fn scratch_path(connection: &Connection) -> DbResult<std::path::PathBuf> {
    let path = connection.with_state(|state| state.pager.path().as_path().to_path_buf())?;
    let mut name = path.as_os_str().to_os_string();
    name.push(format!("-vacuum-{}", std::process::id()));
    Ok(std::path::PathBuf::from(name))
}

/// Builds the rebuilt database at a path, with this database's geometry.
fn build_rebuild(connection: &Connection, target: &std::path::Path) -> DbResult<()> {
    let header = connection.with_state(|state| *state.pager.header())?;
    let vfs = connection.vfs();
    let path = inillucent_vfs::DbPath::new(target.to_path_buf());
    let mut fresh = inillucent_storage::pager::Pager::create(
        vfs.as_ref(),
        &path,
        inillucent_storage::pager::PagerOptions {
            cache_bytes: 2 * 1024 * 1024,
            database: inillucent_base::ids::DatabaseId(1),
            ..inillucent_storage::pager::PagerOptions::default()
        },
        inillucent_storage::pager::NewDatabase {
            page_size: header.page_size,
            reserved_bytes: header.reserved_bytes,
            text_encoding: header.text_encoding,
            vacuum_mode: header.vacuum_mode,
        },
    )?;
    fresh.attach_journal(Box::new(
        inillucent_transaction::journal::RollbackJournal::new(
            std::sync::Arc::clone(&vfs),
            &path,
            inillucent_transaction::journal::JournalOptions::default(),
        ),
    ));
    // A read transaction first, then the write: a pager straight out of
    // `create` is in the Open state, and `begin_write` alone leaves it there as
    // far as reads are concerned - the first page it was asked for came back as
    // "get_page from the Open state".
    fresh.begin_read()?;
    fresh.begin_write()?;
    let outcome = connection.with_state(|state| {
        inillucent_catalog::rebuild::rebuild_into(&mut state.pager, &mut fresh)
    })?;
    match outcome {
        Ok(published) => {
            fresh.commit()?;
            fresh.close()?;
            if let Some(rowid) = published {
                connection.with_state(|state| state.transaction.record_insert_rowid(rowid))?;
            }
            Ok(())
        }
        Err(failure) => {
            let _ = fresh.rollback();
            let _ = fresh.close();
            Err(failure)
        }
    }
}

/// Copies a rebuilt file over this database, page for page.
fn copy_back(connection: &Connection, scratch: &std::path::Path) -> DbResult<()> {
    let vfs = connection.vfs();
    let path = inillucent_vfs::DbPath::new(scratch.to_path_buf());
    let mut rebuilt = inillucent_storage::pager::Pager::open_read_only(
        vfs.as_ref(),
        &path,
        inillucent_storage::pager::PagerOptions {
            cache_bytes: 2 * 1024 * 1024,
            database: inillucent_base::ids::DatabaseId(2),
            ..inillucent_storage::pager::PagerOptions::default()
        },
    )?;
    rebuilt.begin_read()?;
    let count = rebuilt.page_count();
    let header = *rebuilt.header();
    let mut pages: Vec<Vec<u8>> = Vec::with_capacity(count as usize);
    for number in 1..=count {
        let Some(page) = inillucent_base::ids::PageId::new(number) else {
            continue;
        };
        pages.push(rebuilt.get_page(page)?.bytes().to_vec());
    }
    rebuilt.end_read()?;
    rebuilt.close()?;

    connection.with_state(|state| -> DbResult<()> {
        // Grown first, so page 1's header is written over a file that is
        // already the right length: shrinking afterwards is what frees the
        // space the vacuum reclaimed.
        let existing = state.pager.page_count();
        if count > existing {
            state.pager.set_page_count(count)?;
        }
        for (index, bytes) in pages.iter().enumerate() {
            let number = index.saturating_add(1) as u32;
            let Some(page) = inillucent_base::ids::PageId::new(number) else {
                continue;
            };
            state.pager.edit_page(page, |raw| {
                let take = raw.len().min(bytes.len());
                if let (Some(destination), Some(rest)) = (raw.get_mut(..take), bytes.get(..take)) {
                    destination.copy_from_slice(rest);
                }
                Ok(())
            })?;
        }
        if count < existing {
            state.pager.set_page_count(count)?;
        }
        // The header the copy just wrote onto page 1 describes the rebuilt
        // file. Writing it through the pager is what keeps its own idea of the
        // page count, the free list and the cookie in step with the bytes.
        state.pager.set_header(header)
    })?
}

/// Runs `ANALYZE`: measures the schema, and writes what it measured.
///
/// The statistics table is created on first use, exactly as SQLite creates it,
/// and rewritten whole rather than updated in place - a stale row for an index
/// that has since been dropped would be read back as a statistic about
/// something that no longer exists.
fn analyze(connection: &Connection, directive: &Directive) -> DbResult<DirectiveRows> {
    let Directive::Analyze { database, table } = directive else {
        return Err(misuse("not an ANALYZE"));
    };
    let root = statistics_root(connection)?;
    connection.with_database(*database, |pager| analyze::clear_stats(pager, root))??;

    let catalog = connection.catalog()?;
    let wanted = table.as_ref().map(|name| name.to_ascii_lowercase());
    let targets: Vec<inillucent_sql::catalog_view::TableInfo> = {
        use inillucent_sql::catalog_view::{CatalogView, TableKind};
        catalog
            .tables_of(0)
            .into_iter()
            .filter(|candidate| candidate.kind == TableKind::Table)
            // The statistics table is not measured. It is written by this
            // statement, so any figure taken from it would describe the state
            // before the write and be wrong the moment it landed.
            .filter(|candidate| !candidate.folded.starts_with(b"sqlite_"))
            .filter(|candidate| {
                wanted
                    .as_ref()
                    .is_none_or(|name| candidate.folded == name.as_slice())
            })
            .cloned()
            .collect()
    };

    let mut rowid = 1i64;
    for target in &targets {
        let stats =
            connection.with_database(*database, |pager| analyze::measure(pager, target))??;
        for stat in &stats {
            connection.with_database(*database, |pager| {
                analyze::write_stat(pager, root, rowid, stat)
            })??;
            rowid = rowid.saturating_add(1);
        }
    }
    connection.with_database(*database, ddl::bump_schema_cookie)??;
    connection.refresh_catalog()?;
    Ok(Vec::new())
}

/// Returns the root page of `sqlite_stat1`, creating the table if it is absent.
fn statistics_root(connection: &Connection) -> DbResult<u32> {
    let catalog = connection.catalog()?;
    let existing = {
        use inillucent_sql::catalog_view::CatalogView;
        catalog
            .find_table(None, analyze::STAT1.as_bytes())
            .map(|table| table.root)
    };
    if let Some(root) = existing {
        if root != 0 {
            return Ok(root);
        }
    }
    let root = connection.with_state(|state| ddl::allocate_table_root(&mut state.pager))??;
    connection.with_state(|state| {
        ddl::insert_schema_row(
            &mut state.pager,
            &SchemaRow {
                kind: SchemaKind::Table,
                name: analyze::STAT1.as_bytes().to_vec(),
                table: analyze::STAT1.as_bytes().to_vec(),
                root,
                sql: Some(analyze::STAT1_SQL.as_bytes().to_vec()),
            },
        )
    })??;
    connection.with_state(|state| ddl::bump_schema_cookie(&mut state.pager))??;
    connection.refresh_catalog()?;
    Ok(root)
}

/// Creates a view: one `sqlite_schema` row, and no B-tree at all.
///
/// A view's root page is zero, which is how every reader tells it from a table
/// without consulting the row's kind twice. Nothing is allocated and nothing is
/// backfilled, so the whole of `CREATE VIEW` is the row and the cookie.
fn create_view(
    connection: &Connection,
    directive: &Directive,
    source: &[u8],
) -> DbResult<DirectiveRows> {
    let Directive::CreateView {
        database,
        name,
        name_offset,
        exists,
        ..
    } = directive
    else {
        return Err(misuse("not a CREATE VIEW"));
    };
    if *exists {
        return Ok(Vec::new());
    }
    let sql = ddl::canonical_sql("CREATE VIEW", source, *name_offset, source.len() as u32);
    connection.with_database(*database, |pager| {
        ddl::insert_schema_row(
            pager,
            &SchemaRow {
                kind: SchemaKind::View,
                name: name.clone(),
                table: name.clone(),
                root: 0,
                sql: Some(sql.clone()),
            },
        )
    })??;
    connection.with_database(*database, ddl::bump_schema_cookie)??;
    connection.refresh_catalog()?;
    Ok(Vec::new())
}

/// Writes a `CREATE TRIGGER`'s schema row.
///
/// A trigger owns no B-tree - it is the stored text and nothing else, like a
/// view - so creating one is a schema row and a cookie bump. Its `tbl_name` is
/// the table it fires for rather than its own name, which is what makes
/// `DROP TABLE` take its triggers with it and what lets the catalog attach it
/// to the right object on the next load.
fn create_trigger(
    connection: &Connection,
    directive: &Directive,
    source: &[u8],
) -> DbResult<DirectiveRows> {
    let Directive::CreateTrigger {
        database,
        name,
        name_offset,
        table,
        exists,
        ..
    } = directive
    else {
        return Err(misuse("not a CREATE TRIGGER"));
    };
    if *exists {
        return Ok(Vec::new());
    }
    let sql = ddl::canonical_sql("CREATE TRIGGER", source, *name_offset, source.len() as u32);
    connection.with_database(*database, |pager| {
        ddl::insert_schema_row(
            pager,
            &SchemaRow {
                kind: SchemaKind::Trigger,
                name: name.clone(),
                table: table.clone(),
                root: 0,
                sql: Some(sql.clone()),
            },
        )
    })??;
    connection.with_database(*database, ddl::bump_schema_cookie)??;
    connection.refresh_catalog()?;
    Ok(Vec::new())
}

/// Creates an index and its `sqlite_schema` row, then fills it in.
fn create_index(
    connection: &Connection,
    directive: &Directive,
    source: &[u8],
) -> DbResult<DirectiveRows> {
    let Directive::CreateIndex {
        database,
        unique,
        name,
        name_offset,
        table,
        exists,
        ..
    } = directive
    else {
        return Err(misuse("not a CREATE INDEX"));
    };
    if *exists {
        return Ok(Vec::new());
    }
    let keywords = if *unique {
        "CREATE UNIQUE INDEX"
    } else {
        "CREATE INDEX"
    };
    let sql = ddl::canonical_sql(keywords, source, *name_offset, source.len() as u32);
    let root = connection.with_database(*database, ddl::allocate_index_root)??;
    connection.with_database(*database, |pager| {
        ddl::insert_schema_row(
            pager,
            &SchemaRow {
                kind: SchemaKind::Index,
                name: name.clone(),
                table: table.clone(),
                root,
                sql: Some(sql.clone()),
            },
        )
    })??;
    connection.with_database(*database, ddl::bump_schema_cookie)??;
    connection.refresh_catalog()?;
    backfill_index(connection, *database, name)?;
    Ok(Vec::new())
}

/// Fills a newly created index with an entry for every existing row.
///
/// It runs as an ordinary statement against the catalog the `CREATE INDEX` has
/// just published, which is what the TDD's DDL protocol asks for: the backfill
/// is a VM program like any other rather than a second implementation of index
/// maintenance that could disagree with the first.
fn backfill_index(connection: &Connection, database: usize, name: &[u8]) -> DbResult<()> {
    let catalog = connection.catalog()?;
    let folded = name.to_ascii_lowercase();
    let found = {
        use inillucent_sql::catalog_view::CatalogView;
        let name = catalog.database_name(database).to_vec();
        catalog
            .find_index(Some(&name), &folded)
            .map(|(table, index)| (table.clone(), index.clone()))
    };
    let Some((table, index)) = found else {
        return Err(misuse("the index that was just created cannot be found"));
    };
    let columns: Vec<u16> = if table.without_rowid {
        // The scan reads *record* slots, and a WITHOUT ROWID table's record is
        // permuted: its primary key comes first. Handing the declared positions
        // through would index the wrong columns.
        index
            .columns
            .iter()
            .filter_map(|key| key.column)
            .map(|column| table.record_slot(column).unwrap_or(usize::from(column)) as u16)
            .collect()
    } else {
        index.columns.iter().filter_map(|key| key.column).collect()
    };
    let trailing: Vec<u16> = if table.without_rowid {
        table
            .primary_key()
            .into_iter()
            .map(|column| table.record_slot(column).unwrap_or(usize::from(column)) as u16)
            .collect()
    } else {
        Vec::new()
    };
    // The scan's own ordering, which for a WITHOUT ROWID table is the primary
    // key its root b-tree is sorted by.
    let table_key = table
        .without_rowid
        .then(|| inillucent_value::record::KeyInfo {
            columns: table
                .primary_key()
                .into_iter()
                .map(|position| inillucent_value::record::KeyColumn {
                    collation: table
                        .column(position)
                        .map(|column| {
                            inillucent_value::Collation::from_name(
                                core::str::from_utf8(&column.collation).unwrap_or("BINARY"),
                            )
                            .unwrap_or(inillucent_value::Collation::Binary)
                        })
                        .unwrap_or(inillucent_value::Collation::Binary),
                    descending: false,
                })
                .collect(),
        });
    connection.with_database(database, |pager| {
        inillucent_storage::mutate::build_index(
            pager,
            table.root,
            index.root,
            &columns,
            &index_key_info(&index),
            table.rowid_alias,
            &trailing,
            table_key.as_ref(),
        )
    })?
}

/// Returns the ordering an index's entries are compared with.
pub(crate) fn index_key_info(
    index: &inillucent_sql::catalog_view::IndexInfo,
) -> inillucent_value::record::KeyInfo {
    inillucent_value::record::KeyInfo {
        columns: index
            .columns
            .iter()
            .map(|key| inillucent_value::record::KeyColumn {
                collation: inillucent_value::Collation::from_name(
                    core::str::from_utf8(&key.collation).unwrap_or("BINARY"),
                )
                .unwrap_or(inillucent_value::Collation::Binary),
                descending: key.descending,
            })
            .collect(),
    }
}

/// Drops a table or an index, freeing its pages and removing its rows.
fn drop_object(connection: &Connection, directive: &Directive) -> DbResult<DirectiveRows> {
    let Directive::Drop {
        database,
        kind,
        name,
        root,
        index_roots,
        exists,
        ..
    } = directive
    else {
        return Err(misuse("not a DROP"));
    };
    if !*exists {
        return Ok(Vec::new());
    }
    let folded = name.to_ascii_lowercase();
    let table_drop = *kind == ObjectKind::Table;
    let wanted: &[u8] = match kind {
        ObjectKind::View => b"view",
        ObjectKind::Trigger => b"trigger",
        _ => b"index",
    };
    connection.with_database(*database, |pager| {
        ddl::delete_schema_rows(pager, |row| {
            if table_drop {
                // A table takes its indexes and triggers with it, which is why
                // the match is on the row's *table* rather than on its name.
                row.table.to_ascii_lowercase() == folded
            } else {
                row.name.to_ascii_lowercase() == folded && row.kind == wanted
            }
        })
    })??;
    // The roots come from the catalog rather than from the rows just deleted,
    // because a table's automatic indexes are named in it and a row that was
    // already missing would silently leak its pages.
    for root in index_roots.iter().chain(core::iter::once(root)) {
        connection.with_database(*database, |pager| ddl::free_root(pager, *root))??;
    }
    if table_drop {
        // A dropped table's `AUTOINCREMENT` counter goes with it. Leaving the
        // row behind would have a table of the same name created later carry on
        // from the dead one's numbers.
        forget_sequence(connection, &folded)?;
    }
    connection.with_database(*database, ddl::bump_schema_cookie)??;
    connection.refresh_catalog()?;
    Ok(Vec::new())
}

/// Removes a table's row from `sqlite_sequence`, if it has one.
fn forget_sequence(connection: &Connection, folded: &[u8]) -> DbResult<()> {
    let catalog = connection.catalog()?;
    let root = {
        use inillucent_sql::catalog_view::CatalogView;
        catalog
            .find_table(None, SEQUENCE)
            .map_or(0, |table| table.root)
    };
    let Some(root) = inillucent_base::ids::PageId::new(root) else {
        return Ok(());
    };
    let doomed = connection.with_state(|state| -> DbResult<Vec<i64>> {
        let limits = inillucent_base::limits::Limits::default();
        let encoding = state.pager.text_encoding();
        let mut cursor = inillucent_storage::cursor::BTreeCursor::table(root);
        let mut out = Vec::new();
        let mut more = cursor.first(&mut state.pager)?;
        while more {
            let rowid = cursor.rowid()?;
            let payload = cursor.payload(&mut state.pager, &limits)?;
            let record = inillucent_value::record::RecordRef::parse(&payload, encoding)?;
            if let Ok(Value::Text(text)) = record.value(0) {
                if text.utf8_bytes().to_ascii_lowercase() == folded {
                    out.push(rowid);
                }
            }
            more = cursor.next(&mut state.pager)?;
        }
        Ok(out)
    })??;
    for rowid in doomed {
        connection.with_state(|state| {
            inillucent_storage::mutate::delete_row(&mut state.pager, root, rowid)
        })??;
    }
    Ok(())
}

/// Answers or applies a `PRAGMA`.
///
/// The writes are here, because a write needs the transaction machinery and
/// only the connection has it. The reads are in the register, because the
/// `pragma_*` table-valued functions need exactly the same answers at a moment
/// when the connection is already borrowed and only its state can be reached -
/// and two implementations of the same answer would be two answers.
///
/// An unrecognised pragma returns no rows and changes nothing, which is
/// SQLite's behaviour and the reason a typo in one is so easy to miss.
fn pragma(
    connection: &Connection,
    database: Option<usize>,
    name: &[u8],
    argument: Option<&PragmaArgument>,
) -> DbResult<DirectiveRows> {
    if crate::pragma::spec(name).is_none() {
        return Ok(Vec::new());
    }
    // The write forms first: each one either does its work and returns, or
    // falls through to the read below so that the pragma answers with the value
    // it now holds.
    if let Some(argument) = argument {
        match name {
            b"user_version" => {
                return header_write(connection, database, argument, HeaderField::UserVersion)
            }
            b"application_id" => {
                return header_write(connection, database, argument, HeaderField::ApplicationId)
            }
            b"schema_version" => {
                return header_write(connection, database, argument, HeaderField::SchemaCookie)
            }
            b"journal_mode" => {
                let text = crate::pragma::argument_text(argument);
                if let Some(mode) = JournalMode::parse(&text) {
                    connection.set_journal_mode(mode)?;
                }
            }
            b"synchronous" => {
                let text = crate::pragma::argument_text(argument);
                if let Some(level) = Synchronous::parse(&text) {
                    connection.set_synchronous(level)?;
                }
            }
            b"wal_autocheckpoint" => {
                let frames =
                    crate::pragma::argument_integer(argument).clamp(0, i64::from(u32::MAX)) as u32;
                connection.set_wal_auto_checkpoint(frames)?;
            }
            b"foreign_keys" => {
                connection.set_foreign_keys(crate::pragma::argument_boolean(argument))?;
                return Ok(Vec::new());
            }
            b"defer_foreign_keys" => {
                connection.set_defer_foreign_keys(crate::pragma::argument_boolean(argument))?;
                return Ok(Vec::new());
            }
            b"defensive" | b"trusted_schema" | b"writable_schema" => {
                let value = crate::pragma::argument_boolean(argument);
                connection.with_state(|state| {
                    let registry = std::sync::Arc::make_mut(&mut state.registry);
                    let policy = registry.policy_mut();
                    match name {
                        b"defensive" => policy.defensive = value,
                        b"trusted_schema" => policy.trusted_schema = value,
                        _ => policy.writable_schema = value,
                    }
                })?;
                return Ok(Vec::new());
            }
            b"locking_mode" => {
                let text = crate::pragma::argument_text(argument).to_ascii_lowercase();
                connection
                    .set_setting(Setting::ExclusiveLocking, i64::from(text == "exclusive"))?;
            }
            b"wal_checkpoint" => return wal_checkpoint(connection, Some(argument)),
            other => {
                if let Some(setting) = Setting::named(other) {
                    let value = if setting.is_boolean() {
                        i64::from(crate::pragma::argument_boolean(argument))
                    } else {
                        crate::pragma::argument_integer(argument)
                    };
                    connection.set_setting(setting, value)?;
                    if !setting.answers_after_a_write() {
                        return Ok(Vec::new());
                    }
                }
            }
        }
    }
    if name == b"wal_checkpoint" {
        return wal_checkpoint(connection, argument);
    }
    if name == b"foreign_key_check" {
        return foreign_key_check(connection, argument);
    }
    // Everything that reads pages needs a read transaction, and a pragma is a
    // directive rather than a program, so nothing has opened one for it. The
    // schema pragmas need it too: the catalog they read was loaded through the
    // pager and a DDL statement in this same transaction may have replaced it.
    connection.begin_statement(Access::Read)?;
    let rows = connection.with_state(|state| crate::pragma::read(state, database, name, argument));
    let ending = if rows.is_ok() {
        Outcome::Done
    } else {
        Outcome::Abort
    };
    let closed = connection.end_statement(Access::Read, ending);
    let rows = rows??;
    closed?;
    Ok(rows.unwrap_or_default())
}

/// Which header field a pragma writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderField {
    /// `PRAGMA user_version`.
    UserVersion,
    /// `PRAGMA application_id`.
    ApplicationId,
    /// `PRAGMA schema_version`.
    SchemaCookie,
}

/// Writes one of the three header fields an application owns.
///
/// It goes through the ordinary write path rather than editing the page: the
/// header is part of the database, so changing it has to be journalled,
/// committed and rolled back like anything else.
fn header_write(
    connection: &Connection,
    database: Option<usize>,
    argument: &PragmaArgument,
    field: HeaderField,
) -> DbResult<DirectiveRows> {
    let database = target(database);
    let value = crate::pragma::argument_integer(argument);
    run_write(connection, &Directive::Commit, |connection| {
        connection.with_database(database, |pager| {
            let mut header = *pager.header();
            match field {
                HeaderField::UserVersion => header.user_version = value as i32,
                HeaderField::ApplicationId => header.application_id = value as i32,
                HeaderField::SchemaCookie => header.schema_cookie = value as u32,
            }
            pager.set_header(header)
        })??;
        Ok(Vec::new())
    })
}

/// Which database a pragma with no qualifier reads.
fn target(database: Option<usize>) -> usize {
    database.unwrap_or(inillucent_storage::MAIN_DATABASE)
}

// The policy flags live on the module registry rather than beside the other
// settings, because the registry is what consults them: whether a schema may
// name a function, whether a shadow table may be written, whether the schema
// table itself may be. A copy beside the other settings would be a second
// answer to the same question.

/// Runs `PRAGMA wal_checkpoint` and reports what it managed.
///
/// The three numbers are SQLite's: whether it was blocked, how long the log
/// is, and how much of it is now in the database. A blocked checkpoint is not
/// an error - it means a reader is still using frames it would have copied -
/// so it reports one in the first column rather than failing the statement.
fn wal_checkpoint(
    connection: &Connection,
    argument: Option<&PragmaArgument>,
) -> DbResult<DirectiveRows> {
    let mode = argument
        .map(argument_text)
        .and_then(|text| CheckpointMode::parse(&text))
        .unwrap_or(CheckpointMode::Passive);
    if !connection.is_wal() {
        return Ok(vec![vec![
            Value::Integer(0),
            Value::Integer(-1),
            Value::Integer(-1),
        ]]);
    }
    let outcome = connection.checkpoint(mode)?;
    Ok(vec![vec![
        Value::Integer(i64::from(outcome.busy)),
        Value::Integer(i64::from(outcome.log_frames)),
        Value::Integer(i64::from(outcome.checkpointed_frames)),
    ]])
}

/// Reports every child row whose foreign key has no parent.
///
/// The check is written as a query and run through the ordinary planner, so it
/// uses whatever index the child has on its key rather than a second scan
/// written by hand - and so that what it reports is what a `SELECT` would say.
fn foreign_key_check(
    connection: &Connection,
    argument: Option<&PragmaArgument>,
) -> DbResult<DirectiveRows> {
    let wanted = argument.map(|argument| argument_text(argument).to_ascii_lowercase());
    let catalog = connection.catalog()?;
    let mut rows = Vec::new();
    for query in violation_queries(connection, wanted.as_deref())? {
        for row in internal_query(connection, &query.sql)? {
            rows.push(vec![
                Value::owned_text(&query.child)?,
                row.first().cloned().unwrap_or(Value::Null),
                Value::owned_text(&query.parent)?,
                Value::Integer(i64::from(query.key)),
            ]);
        }
    }
    let _ = catalog;
    Ok(rows)
}

/// One foreign key's check, and what to report about the rows it finds.
pub struct ViolationQuery {
    /// The query that finds the offending child rows.
    pub sql: String,
    /// The child table's name.
    pub child: Vec<u8>,
    /// The parent table's name.
    pub parent: Vec<u8>,
    /// The key's position in the child table.
    pub key: u32,
}

/// Builds the checks for one table, or for every table when none is named.
///
/// `deferred_only` is what a commit asks for: an immediate key was checked when
/// the row was written and re-checking it would be work with a known answer.
pub fn violation_queries(
    connection: &Connection,
    only: Option<&str>,
) -> DbResult<Vec<ViolationQuery>> {
    use inillucent_sql::catalog_view::{CatalogView, TableKind};
    let catalog = connection.catalog()?;
    let database = catalog.database_name(0).to_vec();
    let mut queries = Vec::new();
    for child in catalog.tables_of(0) {
        if child.kind != TableKind::Table || child.folded.starts_with(b"sqlite_") {
            continue;
        }
        if only.is_some_and(|name| child.folded != name.as_bytes()) {
            continue;
        }
        for key in &child.foreign_keys {
            let Some(parent) = catalog.find_table(Some(&database), &key.parent_folded) else {
                continue;
            };
            let Some(sql) =
                inillucent_sql::foreign_key::violation_query(child, parent, key, &database)
            else {
                continue;
            };
            queries.push(ViolationQuery {
                sql,
                child: child.name.clone(),
                parent: parent.name.clone(),
                key: key.id,
            });
        }
    }
    Ok(queries)
}

/// Applies the actions of every key that can lead back to its own table.
///
/// A cyclic action - a tree with `ON DELETE CASCADE` on its parent column is
/// the case - cannot be inlined into the statement that fires it, because the
/// body would have to appear once per level the data happens to be deep and
/// that is not known when the statement is compiled. The trigger takes the
/// first level; this takes what it leaves, by repeating the action until
/// nothing changes.
///
/// It terminates because every pass either changes a row or stops, and a pass
/// only ever removes a row or clears a key. The bound is there for the case
/// nobody has thought of rather than for one anybody has seen.
pub fn sweep_cyclic_foreign_keys(connection: &Connection) -> DbResult<()> {
    use inillucent_sql::catalog_view::{CatalogView, TableKind};
    let catalog = connection.catalog()?;
    let database = catalog.database_name(0).to_vec();
    let mut statements = Vec::new();
    for child in catalog.tables_of(0) {
        if child.kind != TableKind::Table {
            continue;
        }
        for key in &child.foreign_keys {
            if !key.cyclic {
                continue;
            }
            let Some(parent) = catalog.find_table(Some(&database), &key.parent_folded) else {
                continue;
            };
            if let Some(sql) =
                inillucent_sql::foreign_key::sweep_statement(child, parent, key, &database)
            {
                statements.push(sql);
            }
        }
    }
    if statements.is_empty() {
        return Ok(());
    }
    for _ in 0..MAX_SWEEP_PASSES {
        // The running total is what says whether a pass did anything: it moves
        // as each statement finishes, so comparing it across a pass asks
        // exactly "did any of these change a row" without the sweep having to
        // count them itself.
        let before = connection.counters().total_changes;
        for sql in &statements {
            internal_query(connection, sql)?;
        }
        if connection.counters().total_changes == before {
            return Ok(());
        }
    }
    Err(misuse(
        "a foreign key's action did not settle; the schema may have a cycle that cannot resolve",
    ))
}

/// How many times the cyclic sweep repeats before it gives up.
///
/// One pass per level of the deepest chain in the data. A tree deeper than this
/// is a tree with a million levels, which is a different problem.
const MAX_SWEEP_PASSES: usize = 1_000_000;

/// Runs one internal query and returns its rows.
///
/// The engine asking itself a question. A foreign-key check *is* a query, and
/// writing it as one means the planner, the indexes and the collations are the
/// ones a user's query would get rather than a second implementation of them.
pub fn internal_query(connection: &Connection, sql: &str) -> DbResult<Vec<Vec<Value<'static>>>> {
    let (mut statement, _) = crate::statement::Statement::prepare(connection, sql.as_bytes())?;
    let mut rows = Vec::new();
    while statement.step()? {
        rows.push(statement.row().to_vec());
    }
    Ok(rows)
}

/// Returns the column names a pragma reports.
pub fn pragma_columns(name: &[u8]) -> Vec<Vec<u8>> {
    match name {
        b"journal_mode" => vec![b"journal_mode".to_vec()],
        b"synchronous" => vec![b"synchronous".to_vec()],
        b"user_version" => vec![b"user_version".to_vec()],
        b"schema_version" => vec![b"schema_version".to_vec()],
        b"page_size" => vec![b"page_size".to_vec()],
        b"page_count" => vec![b"page_count".to_vec()],
        b"wal_checkpoint" => vec![b"busy".to_vec(), b"log".to_vec(), b"checkpointed".to_vec()],
        b"wal_autocheckpoint" => vec![b"wal_autocheckpoint".to_vec()],
        b"foreign_keys" => vec![b"foreign_keys".to_vec()],
        b"defer_foreign_keys" => vec![b"defer_foreign_keys".to_vec()],
        b"foreign_key_list" => vec![
            b"id".to_vec(),
            b"seq".to_vec(),
            b"table".to_vec(),
            b"from".to_vec(),
            b"to".to_vec(),
            b"on_update".to_vec(),
            b"on_delete".to_vec(),
            b"match".to_vec(),
        ],
        b"foreign_key_check" => vec![
            b"table".to_vec(),
            b"rowid".to_vec(),
            b"parent".to_vec(),
            b"fkid".to_vec(),
        ],
        _ => Vec::new(),
    }
}
