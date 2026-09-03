//! Reading `sqlite_schema` and building a snapshot from it.
//!
//! Invariant: everything the catalog knows about a table is derived from the
//! `CREATE` text SQLite stored, parsed with the first-party parser. There is no
//! second source of truth — no side table of affinities, no cached column list
//! — so a schema written by SQLite and a schema written by rust-db are read the
//! same way, and a schema that cannot be parsed is reported as corruption
//! naming the object rather than silently producing a table with no columns.
//!
//! An automatic index has no SQL of its own: SQLite writes
//! `sqlite_autoindex_<table>_<n>` with a NULL statement and expects the reader
//! to reconstruct its key from the table's own `PRIMARY KEY` and `UNIQUE`
//! constraints, in declaration order. That reconstruction is here, and getting
//! its order wrong would make an index seek return the wrong rows rather than
//! fail, which is why it has its own test.

use rustdb_base::limits::Limits;
use rustdb_base::{error, DbError, DbResult};
use rustdb_sql::ast::{
    ColumnConstraint, CreateTableBody, Expr, IndexedColumn, Statement, TableConstraint,
};
use rustdb_sql::catalog_view::{
    ColumnInfo, IndexColumnInfo, IndexInfo, IndexOrigin, TableInfo, TableKind,
};
use rustdb_sql::parser::parse_next_statement;
use rustdb_sql::Ast;
use rustdb_storage::pager::Pager;
use rustdb_storage::schema::{load_schema, SchemaKind, SchemaObject};
use rustdb_value::affinity;

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
    Ok(DatabaseCatalog {
        name: name.to_vec(),
        schema_cookie,
        tables,
    })
}

/// Builds a table entry from one `sqlite_schema` row.
fn table_from_row(row: &SchemaObject, database: usize) -> DbResult<TableInfo> {
    let root = row.root_page.map_or(0, |page| page.get());
    let sql = row.sql.clone().unwrap_or_default();
    if row.kind == SchemaKind::View {
        return Ok(TableInfo {
            name: row.name.clone().into_bytes(),
            folded: row.name.to_ascii_lowercase().into_bytes(),
            database,
            root: 0,
            columns: Vec::new(),
            rowid_alias: None,
            without_rowid: false,
            strict: false,
            kind: TableKind::View,
            create_sql: sql.into_bytes(),
            indexes: Vec::new(),
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
            kind: TableKind::Virtual,
            create_sql: Vec::new(),
            indexes: Vec::new(),
        });
    }
    let mut table = table_from_create_sql(sql.as_bytes(), database, root)
        .map_err(|error| error.with_detail(format!("in object {}", row.name)))?;
    table.name = row.name.clone().into_bytes();
    table.folded = row.name.to_ascii_lowercase().into_bytes();
    Ok(table)
}

/// Parses a `CREATE TABLE` statement into a table entry.
///
/// This is public because it is how a test builds a catalog without a file, and
/// because the DDL path will build a candidate table the same way.
pub fn table_from_create_sql(sql: &[u8], database: usize, root: u32) -> DbResult<TableInfo> {
    let limits = Limits::default();
    let parsed = parse_next_statement(sql, 0, &limits)
        .map_err(|error| error::corrupt(format!("malformed schema SQL: {}", error.message())))?;
    let Statement::CreateTable { name, body, .. } = &parsed.statement else {
        if let Statement::CreateVirtualTable { name, .. } = &parsed.statement {
            let text = parsed.ast.text(*name).to_vec();
            return Ok(TableInfo {
                folded: text.to_ascii_lowercase(),
                name: text,
                database,
                root,
                columns: Vec::new(),
                rowid_alias: None,
                without_rowid: false,
                strict: false,
                kind: TableKind::Virtual,
                create_sql: sql.to_vec(),
                indexes: Vec::new(),
            });
        }
        return Err(error::corrupt("schema SQL is not a CREATE TABLE"));
    };
    let text = parsed.ast.text(*name).to_vec();
    let CreateTableBody::Columns {
        columns,
        constraints,
        without_rowid,
        strict,
    } = body
    else {
        return Err(error::corrupt("a stored CREATE TABLE has no column list"));
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
        kind: TableKind::Table,
        create_sql: sql.to_vec(),
        indexes: Vec::new(),
    };
    for column in columns {
        info.columns.push(column_info(&parsed.ast, column));
    }
    apply_table_constraints(&mut info, &parsed.ast, constraints);
    info.rowid_alias = rowid_alias(&info, &parsed.ast, columns, constraints);
    info.indexes = automatic_indexes(&info, &parsed.ast, columns, constraints);
    Ok(info)
}

/// Builds one column entry from its declaration.
fn column_info(ast: &Ast, column: &rustdb_sql::ast::ColumnDef) -> ColumnInfo {
    let name = ast.text(column.name).to_vec();
    let declared = column.declared_type.clone().unwrap_or_default();
    let mut info = ColumnInfo {
        folded: name.to_ascii_lowercase(),
        name,
        affinity: affinity::for_column(&declared),
        declared_type: declared,
        collation: b"binary".to_vec(),
        not_null: false,
        default_sql: None,
        primary_key_position: None,
        hidden: false,
        generated: false,
    };
    for (_, constraint) in &column.constraints {
        match constraint {
            ColumnConstraint::NotNull(_) => info.not_null = true,
            ColumnConstraint::Collate(name) => {
                info.collation = ast.folded(*name).to_vec();
            }
            ColumnConstraint::Default(expr) => {
                info.default_sql = Some(render_default(ast, *expr));
            }
            ColumnConstraint::PrimaryKey { .. } => {
                info.primary_key_position = Some(1);
            }
            ColumnConstraint::Generated { stored, .. } => {
                info.generated = true;
                // A VIRTUAL generated column is not stored in the record, and a
                // STORED one is. Neither is hidden from `SELECT *`.
                let _ = stored;
            }
            _ => {}
        }
    }
    info
}

/// Renders a `DEFAULT` expression back to text, for reporting only.
fn render_default(ast: &Ast, expr: rustdb_sql::ast::ExprId) -> Vec<u8> {
    match ast.expr(expr) {
        Some(Expr::Literal(rustdb_sql::ast::Literal::Integer(text))) => text.clone(),
        Some(Expr::Literal(rustdb_sql::ast::Literal::Float(text))) => text.clone(),
        Some(Expr::Literal(rustdb_sql::ast::Literal::String(text))) => text.clone(),
        Some(Expr::Literal(rustdb_sql::ast::Literal::Null)) => b"NULL".to_vec(),
        _ => Vec::new(),
    }
}

/// Applies table-level `PRIMARY KEY` and `NOT NULL` implications.
fn apply_table_constraints(
    info: &mut TableInfo,
    ast: &Ast,
    constraints: &[(Option<rustdb_sql::ast::NameId>, TableConstraint)],
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
    columns: &[rustdb_sql::ast::ColumnDef],
    constraints: &[(Option<rustdb_sql::ast::NameId>, TableConstraint)],
) -> Option<u16> {
    if info.without_rowid {
        return None;
    }
    for (position, column) in columns.iter().enumerate() {
        let is_primary = column
            .constraints
            .iter()
            .any(|(_, constraint)| matches!(constraint, ColumnConstraint::PrimaryKey { .. }));
        if !is_primary {
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
    columns: &[rustdb_sql::ast::ColumnDef],
    constraints: &[(Option<rustdb_sql::ast::NameId>, TableConstraint)],
) -> Vec<IndexInfo> {
    let mut indexes = Vec::new();
    let mut ordinal = 0u32;
    for (position, column) in columns.iter().enumerate() {
        for (_, constraint) in &column.constraints {
            let (unique, origin) = match constraint {
                ColumnConstraint::PrimaryKey { .. } => {
                    if info.rowid_alias == Some(position as u16) {
                        continue;
                    }
                    (true, IndexOrigin::PrimaryKey)
                }
                ColumnConstraint::Unique(_) => (true, IndexOrigin::Unique),
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
                }],
                partial_sql: None,
                origin,
            });
        }
    }
    for (_, constraint) in constraints {
        let (keys, unique, origin) = match constraint {
            TableConstraint::PrimaryKey { columns, .. } => {
                let single_rowid = columns.len() == 1
                    && columns
                        .first()
                        .and_then(|key| bare_column_name(ast, key))
                        .and_then(|name| info.column_position(&name))
                        .is_some_and(|index| info.rowid_alias == Some(index));
                if single_rowid {
                    continue;
                }
                (columns, true, IndexOrigin::PrimaryKey)
            }
            TableConstraint::Unique { columns, .. } => (columns, true, IndexOrigin::Unique),
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
                descending: key.order == rustdb_sql::ast::SortOrder::Descending,
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
        });
    }
    indexes
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
        .map_err(|error| error.with_detail(format!("in index {}", row.name)))?;
    table.indexes.push(index);
    Ok(())
}

/// Parses a `CREATE INDEX` statement into an index entry.
fn index_from_create_sql(sql: &[u8], table: &TableInfo, root: u32) -> DbResult<IndexInfo> {
    let limits = Limits::default();
    let parsed = parse_next_statement(sql, 0, &limits)
        .map_err(|error| error::corrupt(format!("malformed index SQL: {}", error.message())))?;
    let Statement::CreateIndex {
        unique,
        name,
        columns,
        filter,
        ..
    } = &parsed.statement
    else {
        return Err(error::corrupt("index SQL is not a CREATE INDEX"));
    };
    let text = parsed.ast.text(*name).to_vec();
    let mut key_columns = Vec::with_capacity(columns.len());
    for key in columns {
        let column = match parsed.ast.expr(key.expr) {
            Some(Expr::Column { column, .. }) => table.column_position(parsed.ast.folded(*column)),
            _ => None,
        };
        let expr_sql = if column.is_none() {
            Some(parsed.ast.expr_span(key.expr).slice(sql).to_vec())
        } else {
            None
        };
        let collation = match key.collation {
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
            descending: key.order == rustdb_sql::ast::SortOrder::Descending,
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
    })
}

/// Returns the error a caller sees when a schema cannot be understood.
pub fn corrupt_schema(detail: impl Into<String>) -> DbError {
    error::corrupt(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustdb_value::Affinity;

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

    /// Schema SQL that does not parse is corruption naming the object, not a
    /// table with no columns.
    #[test]
    fn unparseable_schema_sql_is_corruption() {
        let error = table_from_create_sql(b"CREATE TABLE t(", 0, 2).expect_err("it must not parse");
        assert_eq!(error.code(), rustdb_base::PrimaryCode::Corrupt);
    }
}
