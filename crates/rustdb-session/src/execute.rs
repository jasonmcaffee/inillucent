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

use rustdb_base::error::misuse;
use rustdb_base::DbResult;
use rustdb_catalog::analyze;
use rustdb_catalog::ddl::{self, SchemaRow};
use rustdb_sql::ast::ObjectKind;
use rustdb_sql::directive::{BeginKind, Directive, PragmaArgument};
use rustdb_storage::schema::SchemaKind;
use rustdb_transaction::journal::{JournalMode, Synchronous};
use rustdb_transaction::state::BeginMode;
use rustdb_value::Value;

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
        Directive::CreateTable { .. } => run_write(connection, |connection| {
            create_table(connection, directive, source)
        }),
        Directive::Analyze { .. } => {
            run_write(connection, |connection| analyze(connection, directive))
        }
        Directive::Reindex { .. } => {
            run_write(connection, |connection| reindex(connection, directive))
        }
        Directive::Vacuum { .. } => vacuum(connection),
        Directive::CreateView { .. } => run_write(connection, |connection| {
            create_view(connection, directive, source)
        }),
        Directive::CreateIndex { .. } => run_write(connection, |connection| {
            create_index(connection, directive, source)
        }),
        Directive::Drop { .. } => {
            run_write(connection, |connection| drop_object(connection, directive))
        }
        Directive::Pragma { name, argument } => pragma(connection, name, argument.as_ref()),
    }
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
    body: impl FnOnce(&Connection) -> DbResult<DirectiveRows>,
) -> DbResult<DirectiveRows> {
    connection.begin_statement(Access::Schema)?;
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

/// Creates a table, its automatic indexes, and its `sqlite_schema` rows.
fn create_table(
    connection: &Connection,
    directive: &Directive,
    source: &[u8],
) -> DbResult<DirectiveRows> {
    let Directive::CreateTable {
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
    let root = connection.with_state(|state| ddl::allocate_table_root(&mut state.pager))??;
    connection.with_state(|state| {
        ddl::insert_schema_row(
            &mut state.pager,
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
    let table = rustdb_catalog::table_from_create_sql(&sql, 0, root)?;
    for (ordinal, index) in table.indexes.iter().enumerate() {
        let root = connection.with_state(|state| ddl::allocate_index_root(&mut state.pager))??;
        let index_name = ddl::automatic_index_name(name, ordinal.saturating_add(1) as u32);
        let _ = index;
        connection.with_state(|state| {
            ddl::insert_schema_row(
                &mut state.pager,
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
    connection.with_state(|state| ddl::bump_schema_cookie(&mut state.pager))??;
    connection.refresh_catalog()?;
    Ok(Vec::new())
}

/// Runs `REINDEX`: empties each index and fills it from its table again.
///
/// The refill goes through the same `build_index` a `CREATE INDEX` uses rather
/// than through a second implementation, which is the point of the statement:
/// an index rebuilt by different code from the one that built it could be
/// rebuilt *wrongly* and nothing would notice, because the thing that checks an
/// index is the index.
fn reindex(connection: &Connection, directive: &Directive) -> DbResult<DirectiveRows> {
    let Directive::Reindex { indexes, .. } = directive else {
        return Err(misuse("not a REINDEX"));
    };
    for name in indexes {
        let found = {
            use rustdb_sql::catalog_view::CatalogView;
            let catalog = connection.catalog()?;
            let folded = name.to_ascii_lowercase();
            catalog
                .find_index(None, &folded)
                .map(|(table, index)| (table.clone(), index.clone()))
        };
        let Some((table, index)) = found else {
            continue;
        };
        let Some(root) = rustdb_base::ids::PageId::new(index.root) else {
            continue;
        };
        connection
            .with_state(|state| rustdb_storage::mutate::clear_tree(&mut state.pager, root))??;
        backfill_index(connection, name)?;
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
fn vacuum(connection: &Connection) -> DbResult<DirectiveRows> {
    let auto = connection.with_state(|state| state.pager.header().vacuum_mode)?;
    if auto != rustdb_storage::header::VacuumMode::Auto {
        return Err(misuse(
            "VACUUM on a database that is not in auto_vacuum mode is not implemented yet",
        ));
    }
    connection.begin_statement(Access::Schema)?;
    let outcome = connection.with_state(|state| {
        let pages = state.pager.page_count().saturating_add(1);
        rustdb_storage::vacuum::incremental_vacuum(&mut state.pager, pages)
    });
    let ending = if matches!(outcome, Ok(Ok(_))) {
        Outcome::Done
    } else {
        Outcome::Abort
    };
    let closed = connection.end_statement(Access::Schema, ending);
    outcome??;
    closed?;
    Ok(Vec::new())
}

/// Runs `ANALYZE`: measures the schema, and writes what it measured.
///
/// The statistics table is created on first use, exactly as SQLite creates it,
/// and rewritten whole rather than updated in place - a stale row for an index
/// that has since been dropped would be read back as a statistic about
/// something that no longer exists.
fn analyze(connection: &Connection, directive: &Directive) -> DbResult<DirectiveRows> {
    let Directive::Analyze { table, .. } = directive else {
        return Err(misuse("not an ANALYZE"));
    };
    let root = statistics_root(connection)?;
    connection.with_state(|state| analyze::clear_stats(&mut state.pager, root))??;

    let catalog = connection.catalog()?;
    let wanted = table.as_ref().map(|name| name.to_ascii_lowercase());
    let targets: Vec<rustdb_sql::catalog_view::TableInfo> = {
        use rustdb_sql::catalog_view::{CatalogView, TableKind};
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
        let stats = connection.with_state(|state| analyze::measure(&mut state.pager, target))??;
        for stat in &stats {
            connection
                .with_state(|state| analyze::write_stat(&mut state.pager, root, rowid, stat))??;
            rowid = rowid.saturating_add(1);
        }
    }
    connection.with_state(|state| ddl::bump_schema_cookie(&mut state.pager))??;
    connection.refresh_catalog()?;
    Ok(Vec::new())
}

/// Returns the root page of `sqlite_stat1`, creating the table if it is absent.
fn statistics_root(connection: &Connection) -> DbResult<u32> {
    let catalog = connection.catalog()?;
    let existing = {
        use rustdb_sql::catalog_view::CatalogView;
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
    connection.with_state(|state| {
        ddl::insert_schema_row(
            &mut state.pager,
            &SchemaRow {
                kind: SchemaKind::View,
                name: name.clone(),
                table: name.clone(),
                root: 0,
                sql: Some(sql.clone()),
            },
        )
    })??;
    connection.with_state(|state| ddl::bump_schema_cookie(&mut state.pager))??;
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
    let root = connection.with_state(|state| ddl::allocate_index_root(&mut state.pager))??;
    connection.with_state(|state| {
        ddl::insert_schema_row(
            &mut state.pager,
            &SchemaRow {
                kind: SchemaKind::Index,
                name: name.clone(),
                table: table.clone(),
                root,
                sql: Some(sql.clone()),
            },
        )
    })??;
    connection.with_state(|state| ddl::bump_schema_cookie(&mut state.pager))??;
    connection.refresh_catalog()?;
    backfill_index(connection, name)?;
    Ok(Vec::new())
}

/// Fills a newly created index with an entry for every existing row.
///
/// It runs as an ordinary statement against the catalog the `CREATE INDEX` has
/// just published, which is what the TDD's DDL protocol asks for: the backfill
/// is a VM program like any other rather than a second implementation of index
/// maintenance that could disagree with the first.
fn backfill_index(connection: &Connection, name: &[u8]) -> DbResult<()> {
    let catalog = connection.catalog()?;
    let folded = name.to_ascii_lowercase();
    let found = {
        use rustdb_sql::catalog_view::CatalogView;
        catalog
            .find_index(None, &folded)
            .map(|(table, index)| (table.clone(), index.clone()))
    };
    let Some((table, index)) = found else {
        return Err(misuse("the index that was just created cannot be found"));
    };
    connection.with_state(|state| {
        rustdb_storage::mutate::build_index(
            &mut state.pager,
            table.root,
            index.root,
            &index
                .columns
                .iter()
                .filter_map(|key| key.column)
                .collect::<Vec<u16>>(),
            &index_key_info(&index),
            table.rowid_alias,
        )
    })?
}

/// Returns the ordering an index's entries are compared with.
fn index_key_info(index: &rustdb_sql::catalog_view::IndexInfo) -> rustdb_value::record::KeyInfo {
    rustdb_value::record::KeyInfo {
        columns: index
            .columns
            .iter()
            .map(|key| rustdb_value::record::KeyColumn {
                collation: rustdb_value::Collation::from_name(
                    core::str::from_utf8(&key.collation).unwrap_or("BINARY"),
                )
                .unwrap_or(rustdb_value::Collation::Binary),
                descending: key.descending,
            })
            .collect(),
    }
}

/// Drops a table or an index, freeing its pages and removing its rows.
fn drop_object(connection: &Connection, directive: &Directive) -> DbResult<DirectiveRows> {
    let Directive::Drop {
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
    connection.with_state(|state| {
        ddl::delete_schema_rows(&mut state.pager, |row| {
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
        connection.with_state(|state| ddl::free_root(&mut state.pager, *root))??;
    }
    connection.with_state(|state| ddl::bump_schema_cookie(&mut state.pager))??;
    connection.refresh_catalog()?;
    Ok(Vec::new())
}

/// Answers or applies a `PRAGMA`.
///
/// An unrecognised pragma returns no rows and changes nothing, which is
/// SQLite's behaviour and the reason a typo in one is so easy to miss.
fn pragma(
    connection: &Connection,
    name: &[u8],
    argument: Option<&PragmaArgument>,
) -> DbResult<DirectiveRows> {
    match name {
        b"journal_mode" => {
            if let Some(argument) = argument {
                let text = argument_text(argument);
                let Some(mode) = JournalMode::parse(&text) else {
                    return Ok(vec![vec![text_value(
                        connection.journal_options().mode.as_str(),
                    )?]]);
                };
                connection.set_journal_mode(mode)?;
            }
            Ok(vec![vec![text_value(
                connection.journal_options().mode.as_str(),
            )?]])
        }
        b"synchronous" => {
            if let Some(argument) = argument {
                let text = argument_text(argument);
                if let Some(level) = Synchronous::parse(&text) {
                    connection.set_synchronous(level)?;
                }
            }
            Ok(vec![vec![Value::Integer(
                connection.journal_options().synchronous.as_number(),
            )]])
        }
        b"user_version" => {
            if let Some(argument) = argument {
                let value = argument_integer(argument);
                return run_write(connection, |connection| {
                    connection.with_state(|state| {
                        let mut header = *state.pager.header();
                        header.user_version = value as i32;
                        state.pager.set_header(header)
                    })??;
                    Ok(Vec::new())
                });
            }
            let version = connection.with_state(|state| state.pager.header().user_version)?;
            Ok(vec![vec![Value::Integer(i64::from(version))]])
        }
        b"schema_version" => {
            let cookie = connection.with_state(|state| state.pager.header().schema_cookie)?;
            Ok(vec![vec![Value::Integer(i64::from(cookie))]])
        }
        b"page_size" => {
            let size = connection.with_state(|state| state.pager.page_size().bytes())?;
            Ok(vec![vec![Value::Integer(i64::from(size))]])
        }
        b"page_count" => {
            let pages = connection.with_state(|state| state.pager.page_count())?;
            Ok(vec![vec![Value::Integer(i64::from(pages))]])
        }
        _ => Ok(Vec::new()),
    }
}

/// Returns a pragma argument as text.
fn argument_text(argument: &PragmaArgument) -> String {
    match argument {
        PragmaArgument::Name(name) => String::from_utf8_lossy(name).into_owned(),
        PragmaArgument::Value(expr) => match expr {
            rustdb_sql::bind::BoundExpr::Text(text) => String::from_utf8_lossy(text).into_owned(),
            rustdb_sql::bind::BoundExpr::Integer(value) => value.to_string(),
            _ => String::new(),
        },
    }
}

/// Returns a pragma argument as an integer.
fn argument_integer(argument: &PragmaArgument) -> i64 {
    match argument {
        PragmaArgument::Name(name) => String::from_utf8_lossy(name).parse().unwrap_or(0),
        PragmaArgument::Value(rustdb_sql::bind::BoundExpr::Integer(value)) => *value,
        PragmaArgument::Value(rustdb_sql::bind::BoundExpr::Real(value)) => *value as i64,
        _ => 0,
    }
}

/// Returns a text value, for a pragma that answers with a word.
fn text_value(text: &str) -> DbResult<Value<'static>> {
    Value::owned_text(text.as_bytes())
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
        _ => Vec::new(),
    }
}
