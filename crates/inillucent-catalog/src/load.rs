//! Reading `sqlite_schema` and building a snapshot from it.
//!
//! Invariant: everything the catalog knows about a table is derived from the
//! `CREATE` text SQLite stored, parsed with the first-party parser. There is no
//! second source of truth — no side table of affinities, no cached column list
//! — so a schema written by SQLite and a schema written by inillucent are read the
//! same way, and a schema that cannot be read is reported by name rather than
//! silently producing a table with no columns.
//!
//! **A statement this engine cannot parse is not a damaged file, and reporting
//! it as one cost somebody an afternoon.** Every failure here used to be built
//! with [`inillucent_base::error::corrupt`], which means *malformed
//! persistent bytes* and attaches what it is given as `detail` rather than as
//! `message` — so a caller reading `message()`, the field it is told to read,
//! got the primary code's canned text, `database disk image is malformed`, for
//! a file SQLite reads perfectly. The reader then goes looking for corruption,
//! runs `PRAGMA integrity_check` on a healthy database and finds nothing.
//!
//! The two failures are therefore separated here and reported differently:
//!
//! - The bytes were read and the **statement** could not be understood —
//!   [`unparseable_schema`]. That is a refusal carrying the parser's own words
//!   and offset, and it keeps the `Unsupported` marker when the parser set one,
//!   so a construct this engine has not implemented reaches a caller as exactly
//!   that rather than as damage.
//! - The row is **structurally** wrong — a `table` row holding a `CREATE
//!   INDEX`, a `CREATE TABLE` with no column list — [`corrupt_schema`]. That
//!   really is corruption, and it now says so in the message as well as the
//!   detail.
//!
//! An automatic index has no SQL of its own: SQLite writes
//! `sqlite_autoindex_<table>_<n>` with a NULL statement and expects the reader
//! to reconstruct its key from the table's own `PRIMARY KEY` and `UNIQUE`
//! constraints, in declaration order. That reconstruction is here, and getting
//! its order wrong would make an index seek return the wrong rows rather than
//! fail, which is why it has its own test.

use inillucent_base::limits::Limits;
use inillucent_base::{error, DbError, DbResult};
use inillucent_sql::ast::{
    ColumnConstraint, CreateTableBody, Expr, IndexedColumn, ReferentialAction, Statement,
    TableConstraint,
};
use inillucent_sql::catalog_view::{
    CheckInfo, ColumnInfo, ForeignKeyInfo, IndexColumnInfo, IndexInfo, IndexOrigin, TableInfo,
    TableKind, TriggerEventInfo, TriggerInfo,
};
use inillucent_sql::parser::parse_next_statement;
use inillucent_sql::Ast;
use inillucent_storage::pager::Pager;
use inillucent_storage::schema::{load_schema, SchemaKind, SchemaObject};
use inillucent_value::affinity;

use crate::snapshot::DatabaseCatalog;

/// Reads one database's schema and returns its catalog.
///
/// The pager must already be in a read transaction, so every row read here and
/// every page a later cursor reads come from the same snapshot of the file.
pub fn load_database_catalog(
    pager: &mut Pager,
    name: &[u8],
    database: usize,
) -> DbResult<DatabaseCatalog> {
    Ok(load_database_catalog_with_strays(pager, name, database)?.0)
}

/// Reads one database's schema, and hands back the triggers it could not
/// attach.
///
/// A temporary trigger on a permanent table is the case: the trigger is stored
/// in the temporary database and the table it fires for is in another one, so
/// the table cannot be found while this database is being read. The row is
/// returned rather than dropped, and the caller - which has every database -
/// puts it where it belongs.
pub fn load_database_catalog_with_strays(
    pager: &mut Pager,
    name: &[u8],
    database: usize,
) -> DbResult<(DatabaseCatalog, Vec<SchemaObject>)> {
    let schema_cookie = pager.header().schema_cookie;
    let rows = load_schema(pager)?;
    let mut tables = Vec::new();
    for row in &rows {
        if row.kind != SchemaKind::Table && row.kind != SchemaKind::View {
            continue;
        }
        tables.push(table_from_row(row, database)?);
    }
    for row in &rows {
        if row.kind != SchemaKind::Index {
            continue;
        }
        attach_index(&mut tables, row)?;
    }
    let mut strays = Vec::new();
    for row in &rows {
        if row.kind != SchemaKind::Trigger {
            continue;
        }
        if !attach_trigger_if_present(&mut tables, row)? {
            strays.push(row.clone());
        }
    }
    load_statistics(pager, &mut tables)?;
    // The keys are planned once the whole database is in hand: a key records
    // only the child's side, so the parent's side is found by asking every
    // table what it points at, and that cannot be answered one table at a time.
    inillucent_sql::foreign_key::plan_schema(&mut tables, name, &Limits::default());
    tables.extend(schema_table_aliases(database));
    Ok((
        DatabaseCatalog {
            name: name.to_vec(),
            schema_cookie,
            tables,
        },
        strays,
    ))
}

/// Reads `sqlite_stat1`, when there is one, onto the tables it describes.
///
/// A missing, empty or unreadable statistics table is not an error: statistics
/// are a hint, and a planner that refused to run without them - or refused to
/// run on a stale one - would turn `ANALYZE` from an optimisation into a
/// dependency. Anything it cannot read leaves the defaults in place.
fn load_statistics(pager: &mut Pager, tables: &mut [TableInfo]) -> DbResult<()> {
    let root = tables
        .iter()
        .find(|table| table.folded == crate::analyze::STAT1.as_bytes())
        .map(|table| table.root)
        .unwrap_or(0);
    let Some(root) = inillucent_base::ids::PageId::new(root) else {
        return Ok(());
    };
    let limits = Limits::default();
    let encoding = pager.text_encoding();
    let mut cursor = inillucent_storage::cursor::BTreeCursor::table(root);
    let mut present = cursor.first(pager)?;
    while present {
        let payload = cursor.payload(pager, &limits)?;
        if let Ok(record) =
            inillucent_value::record::RecordRef::parse_with_limits(&payload, encoding, &limits)
        {
            let table = text_of(&record, 0);
            let index = text_of(&record, 1);
            let stat = text_of(&record, 2);
            if let (Some(table), Some(stat)) = (table, stat) {
                apply_statistic(tables, &table, index.as_deref(), &stat);
            }
        }
        present = cursor.next(pager)?;
    }
    Ok(())
}

/// Returns one column of a statistics row as text, when it is text.
fn text_of(record: &inillucent_value::record::RecordRef<'_>, column: usize) -> Option<Vec<u8>> {
    let value = record.value(column).ok()?;
    match value {
        inillucent_value::Value::Text(text) => Some(text.utf8_bytes().to_vec()),
        _ => None,
    }
}

/// Attaches one `sqlite_stat1` row to the object it is about.
///
/// Public because the new engine holds its schema in its own catalog tree and
/// reads `sqlite_stat1` out of a PAX tree rather than a SQLite b-tree - a
/// different way of *finding* the three strings, over the same rule for what
/// they mean. Two copies of that rule would be two answers to
/// `ANALYZE`-changed-the-plan.
pub fn apply_statistic(tables: &mut [TableInfo], table: &[u8], index: Option<&[u8]>, stat: &[u8]) {
    let folded = table.to_ascii_lowercase();
    let Some(info) = tables
        .iter_mut()
        .find(|candidate| candidate.folded == folded)
    else {
        return;
    };
    let (rows, prefixes) = crate::analyze::parse_stat(stat);
    let Some(index) = index else {
        info.analysed_rows = Some(rows);
        return;
    };
    let index_folded = index.to_ascii_lowercase();
    let Some(entry) = info
        .indexes
        .iter_mut()
        .find(|candidate| candidate.folded == index_folded)
    else {
        return;
    };
    let partial = entry.partial_sql.is_some();
    entry.prefix_rows = prefixes;
    entry.analysed_rows = Some(rows);
    // **A partial index's count is not the table's.** Every
    // `sqlite_stat1` row used to set the table's row count from its own leading
    // number, and a partial index's leading number is how many rows its
    // predicate *accepted*. So a table of 6,000 documents with a partial index
    // over the 120 that are not indexed yet came out as a 6,000-row table or a
    // 120-row one depending on which statistic was applied last - and a table
    // the planner believes holds 120 rows is a table it will always scan.
    if !partial {
        info.analysed_rows = Some(rows);
    }
}

/// The `CREATE` text SQLite reports for the schema table itself.
const SCHEMA_TABLE_SQL: &[u8] =
    b"CREATE TABLE sqlite_schema(type text,name text,tbl_name text,rootpage integer,sql text)";

/// Returns the schema table under each of the names it answers to.
///
/// `sqlite_schema` is the one table that has no row in `sqlite_schema`: it is
/// rooted at page one by definition, and a file that had to describe it would
/// have nowhere to put the description. So it is synthesised here, and under
/// every alias SQLite accepts - `sqlite_master` is what almost every tool
/// actually types, and a database that could not answer it would be one no
/// existing tool could inspect.
///
/// The entries are deliberately not part of what `SELECT ... FROM
/// sqlite_schema` returns, because they are not rows in the file; they exist
/// only so a name resolves.
pub fn schema_table_aliases(database: usize) -> Vec<TableInfo> {
    let mut aliases = Vec::new();
    // The temporary database answers to the temporary names and the others to
    // the plain ones. An unqualified name is searched in `temp` first, so a
    // temporary database that also answered to `sqlite_schema` would make
    // `SELECT name FROM sqlite_schema` list the temporary objects - which is
    // not what it means anywhere else.
    let names: &[&[u8]] = if database == inillucent_storage::TEMP_DATABASE {
        &[
            b"sqlite_temp_schema".as_slice(),
            b"sqlite_temp_master".as_slice(),
        ]
    } else {
        &[b"sqlite_schema".as_slice(), b"sqlite_master".as_slice()]
    };
    for name in names {
        let Ok(mut table) = table_from_create_sql(
            SCHEMA_TABLE_SQL,
            database,
            inillucent_storage::schema::SCHEMA_ROOT,
        ) else {
            continue;
        };
        table.name = name.to_vec();
        table.folded = name.to_ascii_lowercase();
        aliases.push(table);
    }
    aliases
}

/// Builds a table entry from one `sqlite_schema` row.
fn table_from_row(row: &SchemaObject, database: usize) -> DbResult<TableInfo> {
    let root = row.root_page.map_or(0, |page| page.get());
    let sql = row.sql.clone().unwrap_or_default();
    if row.kind == SchemaKind::View {
        let create_sql = sql.into_bytes();
        // The body is parsed here, once, and kept. A view referenced twice in
        // one statement is then two binds of one arena rather than two parses,
        // and - the reason it has to be here rather than in the binder - the
        // arena outlives every statement bound against this snapshot.
        let view = view_from_create_sql(&create_sql)
            .map_err(|error| in_object("view", &row.name, error))?;
        return Ok(TableInfo {
            name: row.name.clone().into_bytes(),
            folded: row.name.to_ascii_lowercase().into_bytes(),
            database,
            root: 0,
            columns: Vec::new(),
            rowid_alias: None,
            without_rowid: false,
            strict: false,
            autoincrement: false,
            kind: TableKind::View,
            create_sql,
            view: Some(Box::new(view)),
            triggers: Vec::new(),
            analysed_rows: None,
            indexes: Vec::new(),
            checks: Vec::new(),
            foreign_keys: Vec::new(),
            foreign_key_triggers: Vec::new(),
            module: None,
        });
    }
    if sql.is_empty() {
        // A table with no SQL is either a virtual table SQLite could not record
        // or a corrupt row. Either way there is nothing to bind against.
        return Ok(TableInfo {
            name: row.name.clone().into_bytes(),
            folded: row.name.to_ascii_lowercase().into_bytes(),
            database,
            root,
            columns: Vec::new(),
            rowid_alias: None,
            without_rowid: false,
            strict: false,
            autoincrement: false,
            kind: TableKind::Virtual,
            create_sql: Vec::new(),
            view: None,
            triggers: Vec::new(),
            analysed_rows: None,
            indexes: Vec::new(),
            checks: Vec::new(),
            foreign_keys: Vec::new(),
            foreign_key_triggers: Vec::new(),
            module: None,
        });
    }
    let mut table = table_from_create_sql(sql.as_bytes(), database, root)
        .map_err(|error| in_object("object", &row.name, error))?;
    table.name = row.name.clone().into_bytes();
    table.folded = row.name.to_ascii_lowercase().into_bytes();
    Ok(table)
}

/// Parses a `CREATE VIEW` statement into the body a reference binds.
///
/// The arena is kept whole rather than the `SELECT` being lifted out of it,
/// because every node the select refers to - names, expressions, nested
/// selects - lives in the arena and is addressed by an index into it.
pub fn view_from_create_sql(sql: &[u8]) -> DbResult<inillucent_sql::catalog_view::ViewBody> {
    let limits = Limits::default();
    let parsed = parse_next_statement(sql, 0, &limits)
        .map_err(|error| unparseable_schema("CREATE VIEW", error))?;
    let Statement::CreateView {
        columns, select, ..
    } = &parsed.statement
    else {
        return Err(corrupt_schema(
            "the schema SQL for this view is not a CREATE VIEW",
        ));
    };
    let names = columns
        .iter()
        .map(|name| parsed.ast.text(*name).to_vec())
        .collect();
    Ok(inillucent_sql::catalog_view::ViewBody {
        select: *select,
        columns: names,
        ast: parsed.ast,
    })
}

/// Puts one trigger on the table or view it fires for.
///
/// The body is parsed once, here, for the reason a view body is: the arena
/// belongs to the snapshot, so the binder can bind the body in place instead of
/// re-parsing it on every write to the table.
pub fn attach_trigger(tables: &mut [TableInfo], row: &SchemaObject) -> DbResult<()> {
    attach_trigger_if_present(tables, row).map(|_| ())
}

/// Attaches a trigger to its table, reporting whether the table was there.
///
/// A missing table is not a failure to report: in one database it means a
/// corrupt schema, and refusing to open the file would be a worse answer than
/// opening it without the trigger; across databases it means the trigger fires
/// for a table somewhere else, which is what a temporary trigger on a
/// permanent table is.
fn attach_trigger_if_present(tables: &mut [TableInfo], row: &SchemaObject) -> DbResult<bool> {
    let table_folded = row.table_name.to_ascii_lowercase().into_bytes();
    let Some(table) = tables.iter_mut().find(|table| table.folded == table_folded) else {
        return Ok(false);
    };
    let Some(sql) = row.sql.as_ref() else {
        return Ok(true);
    };
    let trigger = trigger_from_create_sql(sql.as_bytes())
        .map_err(|error| in_object("trigger", &row.name, error))?;
    // Newest first. SQLite pushes each trigger onto the front of the table's
    // list as it reads the schema, and fires them in that order, so the most
    // recently created one runs first. Measured against 3.53.4: three AFTER
    // INSERT triggers created as t1, t2, t3 log three, two, one.
    table.triggers.insert(0, trigger);
    Ok(true)
}

/// Parses a `CREATE TRIGGER` statement into the definition a write fires.
pub fn trigger_from_create_sql(sql: &[u8]) -> DbResult<TriggerInfo> {
    let limits = Limits::default();
    let parsed = parse_next_statement(sql, 0, &limits)
        .map_err(|error| unparseable_schema("CREATE TRIGGER", error))?;
    let Statement::CreateTrigger {
        name,
        time,
        event,
        when,
        body,
        ..
    } = &parsed.statement
    else {
        return Err(corrupt_schema(
            "the schema SQL for this trigger is not a CREATE TRIGGER",
        ));
    };
    let text = parsed.ast.text(*name).to_vec();
    let event = match event {
        inillucent_sql::ast::TriggerEvent::Insert => TriggerEventInfo::Insert,
        inillucent_sql::ast::TriggerEvent::Delete => TriggerEventInfo::Delete,
        inillucent_sql::ast::TriggerEvent::Update(columns) => TriggerEventInfo::Update(
            columns
                .iter()
                .map(|column| parsed.ast.folded(*column).to_vec())
                .collect(),
        ),
    };
    Ok(TriggerInfo {
        folded: text.to_ascii_lowercase(),
        name: text,
        // `CREATE TRIGGER` with no time written is a BEFORE trigger.
        time: time.unwrap_or(inillucent_sql::ast::TriggerTime::Before),
        event,
        when: *when,
        body: body.clone(),
        ast: parsed.ast,
    })
}

/// Parses a `CREATE TABLE` statement into a table entry.
///
/// This is public because it is how a test builds a catalog without a file, and
/// because the DDL path will build a candidate table the same way.
pub fn table_from_create_sql(sql: &[u8], database: usize, root: u32) -> DbResult<TableInfo> {
    let limits = Limits::default();
    let parsed = parse_next_statement(sql, 0, &limits)
        .map_err(|error| unparseable_schema("CREATE TABLE", error))?;
    let Statement::CreateTable { name, body, .. } = &parsed.statement else {
        if let Statement::CreateVirtualTable {
            name,
            module,
            arguments,
            ..
        } = &parsed.statement
        {
            let text = parsed.ast.text(*name).to_vec();
            let module_name = parsed.ast.text(*module).to_vec();
            return Ok(TableInfo {
                folded: text.to_ascii_lowercase(),
                name: text,
                database,
                root,
                columns: Vec::new(),
                rowid_alias: None,
                without_rowid: false,
                strict: false,
                autoincrement: false,
                kind: TableKind::Virtual,
                create_sql: sql.to_vec(),
                view: None,
                triggers: Vec::new(),
                analysed_rows: None,
                indexes: Vec::new(),
                checks: Vec::new(),
                foreign_keys: Vec::new(),
                foreign_key_triggers: Vec::new(),
                // The columns stay empty here on purpose: only the module can
                // say what they are, and the catalog is below the module
                // registry. The connection fills them in when it loads the
                // schema, which is the same moment SQLite calls `xConnect`.
                module: Some(inillucent_sql::vtab::ModuleRef {
                    folded: module_name.to_ascii_lowercase(),
                    name: module_name,
                    arguments: arguments.clone(),
                }),
            });
        }
        return Err(corrupt_schema(
            "the schema SQL for this table is not a CREATE TABLE",
        ));
    };
    let text = parsed.ast.text(*name).to_vec();
    let CreateTableBody::Columns {
        columns,
        constraints,
        without_rowid,
        strict,
    } = body
    else {
        return Err(corrupt_schema("a stored CREATE TABLE has no column list"));
    };
    let mut info = TableInfo {
        folded: text.to_ascii_lowercase(),
        name: text,
        database,
        root,
        columns: Vec::new(),
        rowid_alias: None,
        without_rowid: *without_rowid,
        strict: *strict,
        autoincrement: false,
        kind: TableKind::Table,
        create_sql: sql.to_vec(),
        view: None,
        triggers: Vec::new(),
        analysed_rows: None,
        indexes: Vec::new(),
        checks: Vec::new(),
        foreign_keys: Vec::new(),
        foreign_key_triggers: Vec::new(),
        module: None,
    };
    for column in columns {
        info.columns.push(column_info(sql, &parsed.ast, column));
    }
    info.checks = collect_checks(sql, &parsed.ast, columns, constraints);
    info.foreign_keys = collect_foreign_keys(&info, &parsed.ast, columns, constraints);
    apply_table_constraints(&mut info, &parsed.ast, constraints);
    if info.without_rowid {
        // Every primary-key column of a WITHOUT ROWID table is implicitly NOT
        // NULL, however the key was written. `apply_table_constraints` says so
        // for a table-level `PRIMARY KEY(...)`; a column-level `id INTEGER
        // PRIMARY KEY` is recorded while the column itself is built, and used
        // to arrive here without it - so an INSERT that named no key was
        // accepted where the reference reports NOT NULL on the key column.
        for column in &mut info.columns {
            if column.primary_key_position.is_some() {
                column.not_null = true;
            }
        }
    }
    info.rowid_alias = rowid_alias(&info, &parsed.ast, columns, constraints);
    info.autoincrement = info.rowid_alias.is_some() && declares_autoincrement(columns, constraints);
    let (automatic, rowid_key_conflict) =
        automatic_indexes(&info, &parsed.ast, columns, constraints);
    info.indexes = automatic;
    // A table-level `PRIMARY KEY(id) ON CONFLICT REPLACE` over a rowid alias
    // has no index of its own, so its clause is recorded on the column - the
    // same place a column-level one lands.
    if let Some(column) = info
        .rowid_alias
        .map(usize::from)
        .and_then(|at| info.columns.get_mut(at))
    {
        if column.primary_key_conflict.is_none() {
            column.primary_key_conflict = rowid_key_conflict;
        }
    }
    if info.without_rowid {
        // A WITHOUT ROWID table *is* its primary-key index: SQLite writes no
        // `sqlite_autoindex` row for it, so nothing would ever fill the root in
        // and every seek on the key would open page zero. Pointing the entry at
        // the table's own root is what makes the planner able to use the key,
        // and is the truth about the file - the b-tree at that root is an index
        // b-tree whose record is the whole row.
        let root = info.root;
        let keys = info.primary_key();
        for index in &mut info.indexes {
            if index.origin != IndexOrigin::PrimaryKey {
                continue;
            }
            if index
                .columns
                .iter()
                .map(|key| key.column)
                .eq(keys.iter().copied().map(Some))
            {
                index.root = root;
            }
        }
    }
    Ok(info)
}

/// Builds one column entry from its declaration.
fn column_info(source: &[u8], ast: &Ast, column: &inillucent_sql::ast::ColumnDef) -> ColumnInfo {
    let name = ast.text(column.name).to_vec();
    let declared = column.declared_type.clone().unwrap_or_default();
    let mut info = ColumnInfo {
        folded: name.to_ascii_lowercase(),
        name,
        affinity: affinity::for_column(&declared),
        declared_type: declared,
        collation: b"binary".to_vec(),
        not_null: false,
        not_null_conflict: None,
        primary_key_conflict: None,
        default_sql: None,
        primary_key_position: None,
        hidden: false,
        generated: false,
        stored: false,
        generated_sql: None,
    };
    for (_, constraint) in &column.constraints {
        match constraint {
            ColumnConstraint::NotNull(action) => {
                info.not_null = true;
                info.not_null_conflict = *action;
            }
            ColumnConstraint::Collate(name) => {
                info.collation = ast.folded(*name).to_vec();
            }
            ColumnConstraint::Default(expr) => {
                info.default_sql = Some(source_of(source, ast, *expr));
            }
            ColumnConstraint::PrimaryKey { on_conflict, .. } => {
                info.primary_key_position = Some(1);
                info.primary_key_conflict = *on_conflict;
            }
            ColumnConstraint::Generated { expr, stored } => {
                info.generated = true;
                // A VIRTUAL generated column is not stored in the record, and a
                // STORED one is. Neither is hidden from `SELECT *`.
                info.stored = *stored;
                info.generated_sql = Some(source_of(source, ast, *expr));
            }
            _ => {}
        }
    }
    info
}

/// Returns the source text an expression was written as.
///
/// It is sliced out of the stored `CREATE` statement by the span the parser
/// recorded, rather than rendered back from the tree. Rendering loses whatever
/// the printer does not know how to write - `DEFAULT -1` is a unary expression,
/// not a literal, and reconstructing it as the empty string turned a default
/// into no default at all - and the stored text is the definition, so slicing
/// it cannot disagree with it.
fn source_of(source: &[u8], ast: &Ast, expr: inillucent_sql::ast::ExprId) -> Vec<u8> {
    ast.expr_span(expr).slice(source).to_vec()
}

/// Collects every `CHECK` constraint a table declares, in written order.
///
/// Column-level checks come first in SQLite's own evaluation order, which is
/// the order they are declared in, and table-level ones follow.
fn collect_checks(
    source: &[u8],
    ast: &Ast,
    columns: &[inillucent_sql::ast::ColumnDef],
    constraints: &[(Option<inillucent_sql::ast::NameId>, TableConstraint)],
) -> Vec<CheckInfo> {
    let mut checks = Vec::new();
    for column in columns {
        for (name, constraint) in &column.constraints {
            if let ColumnConstraint::Check(expr) = constraint {
                checks.push(CheckInfo {
                    name: name.map(|name| ast.text(name).to_vec()),
                    expr_sql: source_of(source, ast, *expr),
                    // A column-level `CHECK` has no conflict clause to carry.
                    conflict: None,
                });
            }
        }
    }
    for (name, constraint) in constraints {
        if let TableConstraint::Check { expr, on_conflict } = constraint {
            checks.push(CheckInfo {
                name: name.map(|name| ast.text(name).to_vec()),
                expr_sql: source_of(source, ast, *expr),
                conflict: *on_conflict,
            });
        }
    }
    checks
}

/// Collects the foreign keys a table declares, column clauses first.
///
/// The order matters and is SQLite's: `PRAGMA foreign_key_list` numbers them,
/// and a constraint with no name of its own is identified by that number in
/// every diagnostic about it.
///
/// A clause naming no parent columns keeps naming none. Resolving it to the
/// parent's primary key needs the parent, and the catalog reads one table at a
/// time - the parent may come later in the file, or may not exist at all,
/// which stays legal until something writes a row.
fn collect_foreign_keys(
    info: &TableInfo,
    ast: &Ast,
    columns: &[inillucent_sql::ast::ColumnDef],
    constraints: &[(Option<inillucent_sql::ast::NameId>, TableConstraint)],
) -> Vec<ForeignKeyInfo> {
    let mut keys = Vec::new();
    for (position, column) in columns.iter().enumerate() {
        for (_, constraint) in &column.constraints {
            let ColumnConstraint::References(clause) = constraint else {
                continue;
            };
            let Ok(position) = u16::try_from(position) else {
                continue;
            };
            if let Some(key) = foreign_key(ast, clause, vec![position], keys.len()) {
                keys.push(key);
            }
        }
    }
    for (_, constraint) in constraints {
        let TableConstraint::ForeignKey {
            columns: names,
            clause,
        } = constraint
        else {
            continue;
        };
        let mut positions = Vec::with_capacity(names.len());
        for name in names {
            let Some(position) = info.column_position(ast.folded(*name)) else {
                positions.clear();
                break;
            };
            positions.push(position);
        }
        if positions.is_empty() {
            continue;
        }
        if let Some(key) = foreign_key(ast, clause, positions, keys.len()) {
            keys.push(key);
        }
    }
    keys
}

/// Builds one foreign key from its clause.
fn foreign_key(
    ast: &Ast,
    clause: &inillucent_sql::ast::ForeignKeyClause,
    columns: Vec<u16>,
    id: usize,
) -> Option<ForeignKeyInfo> {
    let parent = ast.text(clause.table).to_vec();
    let mut on_delete = ReferentialAction::NoAction;
    let mut on_update = ReferentialAction::NoAction;
    let mut match_clause = Vec::new();
    for action in &clause.actions {
        match action {
            inillucent_sql::ast::ForeignKeyAction::OnDelete(action) => on_delete = *action,
            inillucent_sql::ast::ForeignKeyAction::OnUpdate(action) => on_update = *action,
            inillucent_sql::ast::ForeignKeyAction::Match(name) => {
                match_clause = ast.text(*name).to_vec()
            }
        }
    }
    Some(ForeignKeyInfo {
        id: u32::try_from(id).ok()?,
        columns,
        parent_folded: parent.to_ascii_lowercase(),
        parent,
        parent_columns: clause
            .columns
            .iter()
            .map(|name| ast.text(*name).to_vec())
            .collect(),
        on_delete,
        on_update,
        match_clause,
        deferrable: clause.deferrable.unwrap_or(false),
        initially_deferred: clause.initially_deferred,
        // Filled in once the whole schema is in hand, by plan_schema.
        cyclic: false,
    })
}

/// Applies table-level `PRIMARY KEY` and `NOT NULL` implications.
fn apply_table_constraints(
    info: &mut TableInfo,
    ast: &Ast,
    constraints: &[(Option<inillucent_sql::ast::NameId>, TableConstraint)],
) {
    for (_, constraint) in constraints {
        let TableConstraint::PrimaryKey { columns, .. } = constraint else {
            continue;
        };
        for (position, key) in columns.iter().enumerate() {
            let Some(name) = bare_column_name(ast, key) else {
                continue;
            };
            let Some(index) = info.column_position(&name) else {
                continue;
            };
            if let Some(column) = info.columns.get_mut(index as usize) {
                column.primary_key_position = Some(position.saturating_add(1) as u16);
                // A WITHOUT ROWID table's primary key columns are implicitly
                // NOT NULL; a rowid table's are not, which is one of SQLite's
                // longest-standing documented quirks.
                if info.without_rowid {
                    column.not_null = true;
                }
            }
        }
    }
}

/// Returns whether the primary key was declared `AUTOINCREMENT`.
///
/// Only meaningful next to a rowid alias: SQLite refuses `AUTOINCREMENT` on
/// anything else, so a table that has it and no alias is one this reader is
/// looking at wrongly rather than one to guess about.
fn declares_autoincrement(
    columns: &[inillucent_sql::ast::ColumnDef],
    constraints: &[(Option<inillucent_sql::ast::NameId>, TableConstraint)],
) -> bool {
    let on_column = columns.iter().any(|column| {
        column.constraints.iter().any(|(_, constraint)| {
            matches!(
                constraint,
                ColumnConstraint::PrimaryKey {
                    autoincrement: true,
                    ..
                }
            )
        })
    });
    let on_table = constraints.iter().any(|(_, constraint)| {
        matches!(
            constraint,
            TableConstraint::PrimaryKey {
                autoincrement: true,
                ..
            }
        )
    });
    on_column || on_table
}

/// Returns the folded name of an indexed column when it is a bare column.
fn bare_column_name(ast: &Ast, key: &IndexedColumn) -> Option<Vec<u8>> {
    match ast.expr(key.expr) {
        Some(Expr::Column { column, .. }) => Some(ast.folded(*column).to_vec()),
        _ => None,
    }
}

/// Returns the column that is an alias for the rowid, when there is one.
///
/// The rule is narrow on purpose: only a rowid table, only a single-column
/// `INTEGER PRIMARY KEY`, and only when the declared type is exactly `INTEGER`.
/// `INT PRIMARY KEY` is *not* a rowid alias, which surprises people every time.
fn rowid_alias(
    info: &TableInfo,
    ast: &Ast,
    columns: &[inillucent_sql::ast::ColumnDef],
    constraints: &[(Option<inillucent_sql::ast::NameId>, TableConstraint)],
) -> Option<u16> {
    if info.without_rowid {
        return None;
    }
    for (position, column) in columns.iter().enumerate() {
        // **`INTEGER PRIMARY KEY DESC` is not a rowid alias.** SQLite's own
        // rule, and not a quirk: a rowid table's key is the rowid and the rowid
        // ascends, so a key that was asked to descend cannot be it - SQLite
        // builds a real index instead, and `PRAGMA index_list` shows it. Taking
        // it as an alias made the descending declaration disappear and left the
        // table with no index where SQLite has one.
        let is_primary = column.constraints.iter().any(|(_, constraint)| {
            matches!(
                constraint,
                ColumnConstraint::PrimaryKey {
                    order: inillucent_sql::ast::SortOrder::Ascending,
                    ..
                }
            )
        });
        if !is_primary {
            // A descending primary key over this column is still a primary key;
            // it simply is not the rowid. Nothing here claims it, and
            // `automatic_indexes` builds its index.
            if column
                .constraints
                .iter()
                .any(|(_, constraint)| matches!(constraint, ColumnConstraint::PrimaryKey { .. }))
            {
                return None;
            }
            continue;
        }
        let declared = column.declared_type.clone().unwrap_or_default();
        if declared.eq_ignore_ascii_case(b"integer") {
            return Some(position as u16);
        }
        return None;
    }
    for (_, constraint) in constraints {
        let TableConstraint::PrimaryKey { columns: keys, .. } = constraint else {
            continue;
        };
        if keys.len() != 1 {
            continue;
        }
        let Some(name) = keys.first().and_then(|key| bare_column_name(ast, key)) else {
            continue;
        };
        let Some(index) = info.column_position(&name) else {
            continue;
        };
        let declared = info
            .column(index)
            .map(|column| column.declared_type.clone())
            .unwrap_or_default();
        if declared.eq_ignore_ascii_case(b"integer") {
            return Some(index);
        }
    }
    None
}

/// Reconstructs the automatic indexes SQLite creates for constraints.
///
/// SQLite numbers them from one in declaration order, counting a table-level
/// constraint where it was written and a column-level one where its column was.
/// A rowid table's `INTEGER PRIMARY KEY` gets no index because the table itself
/// is the index.
fn automatic_indexes(
    info: &TableInfo,
    ast: &Ast,
    columns: &[inillucent_sql::ast::ColumnDef],
    constraints: &[(Option<inillucent_sql::ast::NameId>, TableConstraint)],
) -> (Vec<IndexInfo>, Option<inillucent_sql::ast::ConflictAction>) {
    let mut indexes = Vec::new();
    let mut ordinal = 0u32;
    // The clause on a table-level `PRIMARY KEY` over a rowid alias, which makes
    // no index to carry it.
    let mut rowid_key_conflict = None;
    for (position, column) in columns.iter().enumerate() {
        for (_, constraint) in &column.constraints {
            let (unique, origin, conflict) = match constraint {
                ColumnConstraint::PrimaryKey { on_conflict, .. } => {
                    if info.rowid_alias == Some(position as u16) {
                        continue;
                    }
                    (true, IndexOrigin::PrimaryKey, *on_conflict)
                }
                ColumnConstraint::Unique(action) => (true, IndexOrigin::Unique, *action),
                _ => continue,
            };
            ordinal = ordinal.saturating_add(1);
            let collation = info
                .column(position as u16)
                .map(|column| column.collation.clone())
                .unwrap_or_else(|| b"binary".to_vec());
            indexes.push(IndexInfo {
                name: automatic_name(&info.name, ordinal),
                folded: automatic_name(&info.name, ordinal).to_ascii_lowercase(),
                root: 0,
                unique,
                columns: vec![IndexColumnInfo {
                    column: Some(position as u16),
                    expr_sql: None,
                    collation,
                    descending: false,
                    declared_descending: false,
                }],
                partial_sql: None,
                origin,
                conflict,
                prefix_rows: Vec::new(),
                analysed_rows: None,
            });
        }
    }
    for (_, constraint) in constraints {
        let (keys, unique, origin, conflict) = match constraint {
            TableConstraint::PrimaryKey {
                columns,
                on_conflict,
                ..
            } => {
                let single_rowid = columns.len() == 1
                    && columns
                        .first()
                        .and_then(|key| bare_column_name(ast, key))
                        .and_then(|name| info.column_position(&name))
                        .is_some_and(|index| info.rowid_alias == Some(index));
                if single_rowid {
                    // **No index is made, so the clause is handed back
                    // instead.** A rowid alias named by a table-level
                    // `PRIMARY KEY` is the same constraint as one named on the
                    // column, and SQLite resolves a rowid collision by it
                    // either way - so the caller puts it on the column, which
                    // is where the write path looks.
                    rowid_key_conflict = *on_conflict;
                    continue;
                }
                (columns, true, IndexOrigin::PrimaryKey, *on_conflict)
            }
            TableConstraint::Unique {
                columns,
                on_conflict,
            } => (columns, true, IndexOrigin::Unique, *on_conflict),
            _ => continue,
        };
        ordinal = ordinal.saturating_add(1);
        let mut key_columns = Vec::with_capacity(keys.len());
        for key in keys {
            let column = bare_column_name(ast, key).and_then(|name| info.column_position(&name));
            let collation = match key.collation {
                Some(name) => ast.folded(name).to_vec(),
                None => column
                    .and_then(|index| info.column(index))
                    .map(|column| column.collation.clone())
                    .unwrap_or_else(|| b"binary".to_vec()),
            };
            key_columns.push(IndexColumnInfo {
                column,
                expr_sql: None,
                collation,
                descending: key.order == inillucent_sql::ast::SortOrder::Descending,
                declared_descending: key.order == inillucent_sql::ast::SortOrder::Descending,
            });
        }
        indexes.push(IndexInfo {
            name: automatic_name(&info.name, ordinal),
            folded: automatic_name(&info.name, ordinal).to_ascii_lowercase(),
            root: 0,
            unique,
            columns: key_columns,
            partial_sql: None,
            origin,
            conflict,
            prefix_rows: Vec::new(),
            analysed_rows: None,
        });
    }
    (indexes, rowid_key_conflict)
}

/// Returns the name SQLite gives an automatic index.
fn automatic_name(table: &[u8], ordinal: u32) -> Vec<u8> {
    let mut name = b"sqlite_autoindex_".to_vec();
    name.extend_from_slice(table);
    name.push(b'_');
    name.extend_from_slice(ordinal.to_string().as_bytes());
    name
}

/// Attaches an index row to the table it indexes.
fn attach_index(tables: &mut [TableInfo], row: &SchemaObject) -> DbResult<()> {
    let table_folded = row.table_name.to_ascii_lowercase().into_bytes();
    let Some(table) = tables.iter_mut().find(|table| table.folded == table_folded) else {
        // An index whose table is missing is a corrupt schema, but a reader
        // that refuses to open the file cannot report which object is wrong.
        return Ok(());
    };
    let root = row.root_page.map_or(0, |page| page.get());
    let folded = row.name.to_ascii_lowercase().into_bytes();
    let Some(sql) = row.sql.as_ref() else {
        // An automatic index: the entry was reconstructed from the table's own
        // constraints and only the root page is missing from it.
        if let Some(existing) = table
            .indexes
            .iter_mut()
            .find(|index| index.folded == folded)
        {
            existing.root = root;
        }
        return Ok(());
    };
    let index = index_from_create_sql(sql.as_bytes(), table, root)
        .map_err(|error| in_object("index", &row.name, error))?;
    table.indexes.push(index);
    Ok(())
}

/// Parses a `CREATE INDEX` statement into an index entry.
pub fn index_from_create_sql(sql: &[u8], table: &TableInfo, root: u32) -> DbResult<IndexInfo> {
    let limits = Limits::default();
    let parsed = parse_next_statement(sql, 0, &limits)
        .map_err(|error| unparseable_schema("CREATE INDEX", error))?;
    let Statement::CreateIndex {
        unique,
        name,
        columns,
        filter,
        ..
    } = &parsed.statement
    else {
        return Err(corrupt_schema(
            "the schema SQL for this index is not a CREATE INDEX",
        ));
    };
    let text = parsed.ast.text(*name).to_vec();
    let mut key_columns = Vec::with_capacity(columns.len());
    for key in columns {
        // `ON t(b COLLATE NOCASE)` parses the collation into the expression,
        // because that is where the grammar puts a `COLLATE` that follows a
        // value. It is still an index on a bare column; reading it as an
        // expression would make every write to the table refuse, because a
        // key the engine cannot compute is a key it cannot maintain.
        let (expr, written_collation) = match parsed.ast.expr(key.expr) {
            Some(Expr::Collate { operand, collation }) => {
                (parsed.ast.expr(*operand), Some(*collation))
            }
            other => (other, key.collation),
        };
        let column = match expr {
            Some(Expr::Column { column, .. }) => table.column_position(parsed.ast.folded(*column)),
            _ => None,
        };
        let expr_sql = if column.is_none() {
            Some(parsed.ast.expr_span(key.expr).slice(sql).to_vec())
        } else {
            None
        };
        let collation = match written_collation {
            Some(name) => parsed.ast.folded(name).to_vec(),
            None => column
                .and_then(|index| table.column(index))
                .map(|column| column.collation.clone())
                .unwrap_or_else(|| b"binary".to_vec()),
        };
        key_columns.push(IndexColumnInfo {
            column,
            expr_sql,
            collation,
            descending: key.order == inillucent_sql::ast::SortOrder::Descending,
            declared_descending: key.order == inillucent_sql::ast::SortOrder::Descending,
        });
    }
    let partial_sql = filter.map(|expr| parsed.ast.expr_span(expr).slice(sql).to_vec());
    Ok(IndexInfo {
        folded: text.to_ascii_lowercase(),
        name: text,
        root,
        unique: *unique,
        columns: key_columns,
        partial_sql,
        origin: IndexOrigin::Created,
        // `CREATE UNIQUE INDEX` has no `ON CONFLICT` clause in the grammar, so
        // a violation of one always resolves as ABORT unless the statement
        // overrides it.
        conflict: None,
        prefix_rows: Vec::new(),
        analysed_rows: None,
    })
}

/// Returns the error a caller sees when a schema row is structurally wrong.
///
/// This is for a row that cannot be what it claims to be — a `table` row whose
/// SQL is a `CREATE INDEX`, a `CREATE TABLE` with no column list. Those are
/// corruption and are reported as corruption.
///
/// The sentence goes in the **message** as well as the detail. `error::corrupt`
/// attaches its argument as detail only, so an error built with it alone
/// answers `message()` with "database disk image is malformed" and the
/// explanation never leaves the process. A sentence about a schema object's own
/// shape names no path, no bound value and no page bytes, so it is safe where
/// `DbError::with_message` requires safety.
///
/// @param detail - what is wrong with the row, safe for a caller to read
pub fn corrupt_schema(detail: impl Into<String>) -> DbError {
    let detail = detail.into();
    error::corrupt(detail.clone()).with_message(detail)
}

/// Returns the error a caller sees when stored schema SQL will not parse.
///
/// **Not corruption.** The row's bytes were read; it is the statement that
/// could not be understood, and the two are different facts about a file. A
/// `CREATE TABLE` SQLite wrote and this parser refuses is a gap in this engine,
/// not damage to the disk — `CREATE TABLE pairs (left TEXT, right TEXT)` used
/// to be exactly that, and it reported the file as malformed.
///
/// So the failure keeps the parser's own code, words and offset, and keeps the
/// `Unsupported` marker when the parser set one. `inillucent-driver` reads that
/// marker rather than the wording, so a construct this engine has not
/// implemented arrives as `Status::Unsupported` naming the construct, and
/// anything else as `Status::Syntax` naming the word — which is what a person
/// can act on.
///
/// @param what - the statement being read, as `CREATE TABLE`
/// @param error - the parser's failure
pub(crate) fn unparseable_schema(
    what: &str,
    error: inillucent_sql::diagnostic::ParseError,
) -> DbError {
    let said = format!(
        "cannot parse the {what} stored in the schema: {}",
        error.message()
    );
    let built = DbError::primary(error.code())
        .with_message(said.clone())
        .with_detail(said)
        .with_sql_offset(error.offset());
    match error.kind {
        inillucent_sql::diagnostic::ParseErrorKind::Unsupported(construct) => {
            built.with_unsupported(construct)
        }
        _ => built,
    }
}

/// Names the schema object a failure was reading, in the message.
///
/// The name used to be attached with `with_detail`, which **replaced** the
/// detail the parse site had just written — so the object was named and the
/// reason was destroyed, and neither of them was in `message()`. Composing them
/// keeps both, in the field a caller reads.
///
/// A schema object's own name is the caller's own word and carries no path or
/// bound value, so it is safe in a message.
///
/// @param kind - what the object is, as `table` or `index`
/// @param name - the object's name, as `sqlite_schema` records it
/// @param error - the failure to name
fn in_object(kind: &str, name: &str, error: DbError) -> DbError {
    let said = format!("in {kind} {name}: {}", error.message());
    error.with_message(said.clone()).with_detail(said)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_value::Affinity;

    /// Affinity comes from reading the declared type as characters, so the
    /// surprising cases have to work: `INT` is integer, `VARCHAR` is text, and
    /// a type with no rule at all is numeric.
    #[test]
    fn affinity_is_derived_from_the_declared_type() {
        let table = table_from_create_sql(
            b"CREATE TABLE t(a INTEGER, b VARCHAR(20), c BLOB, d REAL, e WHATEVER, f)",
            0,
            2,
        )
        .expect("it parses");
        let affinities: Vec<Affinity> =
            table.columns.iter().map(|column| column.affinity).collect();
        assert_eq!(
            affinities,
            vec![
                Affinity::Integer,
                Affinity::Text,
                Affinity::Blob,
                Affinity::Real,
                Affinity::Numeric,
                Affinity::Blob,
            ]
        );
    }

    /// `INTEGER PRIMARY KEY` is the rowid; `INT PRIMARY KEY` is not, and a
    /// `WITHOUT ROWID` table has no alias at all.
    #[test]
    fn only_integer_primary_key_aliases_the_rowid() {
        let integer = table_from_create_sql(b"CREATE TABLE t(a INTEGER PRIMARY KEY, b)", 0, 2)
            .expect("it parses");
        assert_eq!(integer.rowid_alias, Some(0));

        let int = table_from_create_sql(b"CREATE TABLE t(a INT PRIMARY KEY, b)", 0, 2)
            .expect("it parses");
        assert_eq!(int.rowid_alias, None);

        let table_level =
            table_from_create_sql(b"CREATE TABLE t(a INTEGER, b, PRIMARY KEY(a))", 0, 2)
                .expect("it parses");
        assert_eq!(table_level.rowid_alias, Some(0));

        let without = table_from_create_sql(
            b"CREATE TABLE t(a INTEGER PRIMARY KEY, b) WITHOUT ROWID",
            0,
            2,
        )
        .expect("it parses");
        assert_eq!(without.rowid_alias, None);
        assert!(without.without_rowid);
    }

    /// Automatic indexes are numbered in declaration order, and a rowid alias
    /// gets none because the table is already the index.
    #[test]
    fn automatic_indexes_are_numbered_in_declaration_order() {
        let table = table_from_create_sql(
            b"CREATE TABLE t(a INTEGER PRIMARY KEY, b UNIQUE, c, UNIQUE(c, b))",
            0,
            2,
        )
        .expect("it parses");
        let names: Vec<String> = table
            .indexes
            .iter()
            .map(|index| String::from_utf8_lossy(&index.name).into_owned())
            .collect();
        assert_eq!(names, vec!["sqlite_autoindex_t_1", "sqlite_autoindex_t_2"]);
        assert_eq!(
            table.indexes.get(1).map(|index| index.columns.len()),
            Some(2)
        );
    }

    /// A `WITHOUT ROWID` primary key makes its columns NOT NULL; a rowid
    /// table's primary key famously does not.
    #[test]
    fn without_rowid_primary_keys_are_not_null() {
        let without = table_from_create_sql(
            b"CREATE TABLE t(a TEXT, b TEXT, PRIMARY KEY(a)) WITHOUT ROWID",
            0,
            2,
        )
        .expect("it parses");
        assert!(without
            .columns
            .first()
            .is_some_and(|column| column.not_null));

        let rowid = table_from_create_sql(b"CREATE TABLE t(a TEXT, b TEXT, PRIMARY KEY(a))", 0, 2)
            .expect("it parses");
        assert!(rowid.columns.first().is_some_and(|column| !column.not_null));
    }

    /// A declared collation reaches the column and its automatic index, which
    /// is what lets the planner refuse an index whose collation disagrees.
    #[test]
    fn a_declared_collation_reaches_the_column_and_its_index() {
        let table = table_from_create_sql(
            b"CREATE TABLE t(a TEXT COLLATE NOCASE UNIQUE, b TEXT)",
            0,
            2,
        )
        .expect("it parses");
        assert_eq!(
            table.columns.first().map(|column| column.collation.clone()),
            Some(b"nocase".to_vec())
        );
        assert_eq!(
            table
                .indexes
                .first()
                .and_then(|index| index.columns.first())
                .map(|column| column.collation.clone()),
            Some(b"nocase".to_vec())
        );
    }

    /// Schema SQL that does not parse says what it could not parse and why -
    /// it does not claim the file is damaged.
    ///
    /// The message is asserted, not the detail: `message()` is the field
    /// `inillucent-driver` shows a caller, and putting the sentence only in
    /// `detail()` is the defect this replaces. A reader who is told "database
    /// disk image is malformed" runs `PRAGMA integrity_check` on a healthy
    /// database and learns nothing.
    #[test]
    fn unparseable_schema_sql_names_the_statement_and_the_reason() {
        let error = table_from_create_sql(b"CREATE TABLE t(", 0, 2).expect_err("it must not parse");
        assert_eq!(error.code(), inillucent_base::PrimaryCode::Error);
        assert!(
            error.message().contains("cannot parse the CREATE TABLE"),
            "{error:?}"
        );
        assert!(
            !error.message().contains("disk image is malformed"),
            "{error:?}"
        );
        assert_eq!(error.detail(), Some(error.message()));
    }

    /// A row that cannot be what it claims to be is still corruption, and now
    /// says so where a caller can read it.
    #[test]
    fn a_row_whose_sql_is_the_wrong_statement_is_corruption() {
        let error = table_from_create_sql(b"CREATE INDEX i ON t (a)", 0, 2)
            .expect_err("an index is not a table");
        assert_eq!(error.code(), inillucent_base::PrimaryCode::Corrupt);
        assert!(
            error.message().contains("is not a CREATE TABLE"),
            "{error:?}"
        );
    }

    /// A construct the parser marks as one it has not implemented keeps that
    /// mark, so a caller can tell "this engine cannot do that yet" from "your
    /// schema is wrong" without matching on a sentence.
    ///
    /// `inillucent-driver` reads exactly this marker to answer
    /// `Status::Unsupported` with the construct named, which is the shape
    /// schema failures are meant to have.
    #[test]
    fn an_unimplemented_construct_in_schema_sql_keeps_its_marker() {
        let error = trigger_from_create_sql(
            b"CREATE TRIGGER r AFTER INSERT ON t BEGIN INSERT INTO u VALUES (1) RETURNING 1; END",
        )
        .expect_err("RETURNING is refused inside a trigger");
        assert_eq!(
            error.unsupported(),
            Some("RETURNING is not available in triggers"),
            "{error:?}"
        );
        assert_eq!(error.code(), inillucent_base::PrimaryCode::Error);
        assert!(
            error.message().contains("cannot parse the CREATE TRIGGER"),
            "{error:?}"
        );
    }

    /// Reserved-word column names: `left` and `right` are ordinary names in
    /// SQLite - a diff table, a tree, a stereo channel - and a schema SQLite
    /// writes has to load here.
    #[test]
    fn a_column_named_left_loads() {
        let table = table_from_create_sql(b"CREATE TABLE pairs (left TEXT, right TEXT)", 0, 2)
            .expect("it parses");
        let names: Vec<String> = table
            .columns
            .iter()
            .map(|column| String::from_utf8_lossy(&column.name).into_owned())
            .collect();
        assert_eq!(names, vec!["left".to_string(), "right".to_string()]);
    }
}
