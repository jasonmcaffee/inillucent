//! Writing a catalog row through `INSERT INTO sqlite_schema`.
//!
//! Invariant: **a row written here goes through the same `record` a `CREATE`
//! uses.** The catalog tree has ten columns and `sqlite_schema` declares five,
//! so a row written down the ordinary insert path would name no tree and carry
//! no statistics - which is a schema the next open cannot read. Everything this
//! module does is turn the five values a dump supplies into a `SchemaEntry` and
//! hand it to the catalog.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_catalog::paged::{ObjectKind, SchemaEntry};
use inillucent_value::Value;

use crate::{ImportedDatabase, Outcome};

impl ImportedDatabase {
    /// Records a catalog row an `INSERT INTO sqlite_schema` supplied.
    ///
    /// **What makes a `.dump` of a virtual table replayable (task-1979, R2).**
    /// A dump restores a virtual table by writing its shadow tables as ordinary
    /// `CREATE TABLE` statements and then inserting the `sqlite_schema` row for
    /// the table itself, because running its `CREATE VIRTUAL TABLE` would build
    /// a second, empty set of shadow tables over the ones just restored. SQLite
    /// accepts that insert under `PRAGMA writable_schema` and this engine
    /// refused it, so its own dump output stopped at the first virtual table
    /// with `unsupported: writing to a table whose name begins with sqlite_`.
    ///
    /// The module is connected afterwards, from the shadow tables the dump has
    /// already restored, which is the same path a reopen takes.
    ///
    /// @param statement - the bound insert, whose target is `sqlite_schema`
    /// @param params - the values bound to `?1`, `?2`, ...
    pub(crate) fn insert_into_schema(
        &mut self,
        statement: &inillucent_sql::dml::BoundInsert,
        params: &inillucent_exec::physical::Params,
    ) -> DbResult<Outcome> {
        let inillucent_sql::dml::BoundInsertSource::Values(values) = &statement.source else {
            return Err(
                refusal("an INSERT ... SELECT into sqlite_schema is not built")
                    .with_unsupported("an INSERT ... SELECT into sqlite_schema"),
            );
        };
        let mut changed = 0usize;
        for row in values {
            let mut supplied: Vec<Value<'static>> = Vec::with_capacity(row.len());
            for expr in row {
                supplied.push(
                    Value::from(&inillucent_exec::physical::literal_value(expr, params)?.borrow())
                        .into_owned()?,
                );
            }
            let cells = cells_of(statement, &supplied);
            self.record(0, entry_of(&cells)?)?;
            changed = changed.saturating_add(1);
        }
        self.rebuild_tables()?;
        self.refresh_catalog();
        self.reconnect_modules()?;
        self.refresh_vector_indexes();
        self.seal()?;
        self.record_changes(changed as i64, changed as i64);
        Ok(Outcome::empty())
    }
}

/// Places each supplied value at the column the statement named.
///
/// The five are `sqlite_schema`'s own declaration, in its order: `type`,
/// `name`, `tbl_name`, `rootpage`, `sql`. A column the statement did not name
/// is NULL, which `entry_of` then refuses where it matters.
///
/// @param statement - the bound insert
/// @param supplied - the row's values, in the order they were written
fn cells_of(
    statement: &inillucent_sql::dml::BoundInsert,
    supplied: &[Value<'static>],
) -> Vec<Value<'static>> {
    let mut cells = vec![Value::Null; 5];
    for (position, column) in statement.columns.iter().enumerate() {
        let inillucent_sql::dml::ColumnSource::Row(at) = column else {
            continue;
        };
        if let (Some(slot), Some(value)) = (cells.get_mut(position), supplied.get(*at)) {
            *slot = value.clone();
        }
    }
    cells
}

/// Turns one `sqlite_schema` row into the catalog entry it describes.
///
/// **`rootpage` is read and discarded.** A dump writes `0` for a virtual table
/// and the number in a file is a page in *that* file, so carrying it across
/// would name a page of the database being restored into. `record` fills in the
/// tree the row belongs to, which for a virtual table is none.
///
/// @param cells - the five declared columns, in `sqlite_schema`'s order
fn entry_of(cells: &[Value<'static>]) -> DbResult<SchemaEntry> {
    let kind = match text_of(cells.first()).to_ascii_lowercase().as_slice() {
        b"table" => ObjectKind::Table,
        b"index" => ObjectKind::Index,
        b"view" => ObjectKind::View,
        b"trigger" => ObjectKind::Trigger,
        other => {
            return Err(refusal(format!(
                "sqlite_schema.type has to be table, index, view or trigger, not {}",
                String::from_utf8_lossy(other)
            )))
        }
    };
    let name = text_of(cells.get(1));
    if name.is_empty() {
        return Err(refusal("a row of sqlite_schema needs a name"));
    }
    let sql = text_of(cells.get(4));
    // **Only a virtual table, which is the one shape a dump writes this way.**
    // Every other kind of object has a `CREATE` statement that rebuilds it, and
    // running that statement is how a dump restores it; accepting an arbitrary
    // row here would let a caller record a table with no tree behind it, which
    // reads back as a corrupt database rather than as an error.
    if kind != ObjectKind::Table || !starts_with_create_virtual_table(&sql) {
        return Err(refusal(
            "only a CREATE VIRTUAL TABLE row can be written into sqlite_schema; every other \
             object is restored by running its own CREATE statement",
        )
        .with_unsupported("writing an arbitrary row into sqlite_schema"));
    }
    let table = match text_of(cells.get(2)) {
        held if held.is_empty() => name.clone(),
        held => held,
    };
    Ok(SchemaEntry {
        kind,
        name,
        table,
        root: inillucent_pool::PageId::NONE,
        sql,
        stats: Default::default(),
        // Filled by `record` from the identifier it is given.
        tree_id: 0,
    })
}

/// Returns a cell's text, and the empty string for anything else.
///
/// @param value - the cell
fn text_of(value: Option<&Value<'static>>) -> Vec<u8> {
    match value {
        Some(Value::Text(text)) => text.raw().to_vec(),
        _ => Vec::new(),
    }
}

/// Returns whether a statement's first three words are `CREATE VIRTUAL TABLE`.
///
/// @param sql - the statement text the row carries
fn starts_with_create_virtual_table(sql: &[u8]) -> bool {
    let words: Vec<String> = String::from_utf8_lossy(sql)
        .split_whitespace()
        .take(3)
        .map(str::to_ascii_lowercase)
        .collect();
    words == ["create", "virtual", "table"]
}
