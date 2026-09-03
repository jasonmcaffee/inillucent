//! What the binder is allowed to know about a schema.
//!
//! Invariant: this is a read-only view over an immutable snapshot. Nothing here
//! can open a page, and nothing here changes while a statement is being bound,
//! so a bound statement is a pure function of its SQL and one generation of one
//! catalog. That is what makes prepared-statement invalidation a comparison of
//! two numbers rather than a re-derivation.
//!
//! The types are defined here, below the catalog that fills them in, so the
//! binder can be compiled and tested against a hand-built schema with no file
//! anywhere near it.

use crate::ast::ConflictAction;
use rustdb_value::Affinity;

/// Where an index came from, which decides whether it can be dropped and how
/// it is named in `sqlite_schema`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexOrigin {
    /// `CREATE INDEX`.
    Created,
    /// A `UNIQUE` constraint.
    Unique,
    /// A `PRIMARY KEY` constraint on a rowid table.
    PrimaryKey,
}

/// One column of a table or view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    /// The name as declared.
    pub name: Vec<u8>,
    /// The ASCII-folded lookup key.
    pub folded: Vec<u8>,
    /// The declared type, exactly as written, empty when none was given.
    pub declared_type: Vec<u8>,
    /// The affinity derived from the declared type.
    pub affinity: Affinity,
    /// The folded name of the column's declared collation.
    pub collation: Vec<u8>,
    /// Whether the column is `NOT NULL`.
    pub not_null: bool,
    /// The `ON CONFLICT` clause written on the `NOT NULL`, when there was one.
    ///
    /// A constraint carries its own algorithm and the statement may override
    /// it: `INSERT OR IGNORE` beats `NOT NULL ON CONFLICT ABORT`. Recording it
    /// per constraint rather than per table is what makes that override a
    /// choice between two known values instead of a guess.
    pub not_null_conflict: Option<ConflictAction>,
    /// The `DEFAULT` expression, as written.
    pub default_sql: Option<Vec<u8>>,
    /// The one-based position in the primary key, when it is in one.
    pub primary_key_position: Option<u16>,
    /// Whether the column is hidden from `SELECT *`.
    pub hidden: bool,
    /// Whether the column is generated.
    pub generated: bool,
}

/// One key column of an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexColumnInfo {
    /// The table column this key indexes, when it indexes a bare column.
    pub column: Option<u16>,
    /// The key expression, as written, when the key is an expression.
    pub expr_sql: Option<Vec<u8>>,
    /// The folded collation name the key is ordered by.
    pub collation: Vec<u8>,
    /// Whether the key is stored descending.
    pub descending: bool,
}

/// An index over a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    /// The index name.
    pub name: Vec<u8>,
    /// The ASCII-folded lookup key.
    pub folded: Vec<u8>,
    /// The root page of the index B-tree.
    pub root: u32,
    /// Whether the index enforces uniqueness.
    pub unique: bool,
    /// The key columns, in order.
    pub columns: Vec<IndexColumnInfo>,
    /// The partial-index predicate, as written.
    pub partial_sql: Option<Vec<u8>>,
    /// Where the index came from.
    pub origin: IndexOrigin,
    /// The `ON CONFLICT` clause the constraint that created it carried.
    pub conflict: Option<ConflictAction>,
}

/// What kind of schema object a name resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableKind {
    /// An ordinary table.
    Table,
    /// A view.
    View,
    /// A virtual table.
    Virtual,
}

/// A table, view or virtual table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    /// The name as declared.
    pub name: Vec<u8>,
    /// The ASCII-folded lookup key.
    pub folded: Vec<u8>,
    /// Which attached database it belongs to.
    pub database: usize,
    /// The root page of the table B-tree, or zero for a view.
    pub root: u32,
    /// The columns, in declaration order.
    pub columns: Vec<ColumnInfo>,
    /// The column that is an alias for the rowid, when there is one.
    pub rowid_alias: Option<u16>,
    /// Whether the table is `WITHOUT ROWID`.
    pub without_rowid: bool,
    /// Whether the table is `STRICT`.
    pub strict: bool,
    /// What kind of object this is.
    pub kind: TableKind,
    /// The `CREATE` text as stored in `sqlite_schema`.
    pub create_sql: Vec<u8>,
    /// The indexes over this table.
    pub indexes: Vec<IndexInfo>,
    /// Every `CHECK` constraint, as the source text it was written as.
    ///
    /// The text rather than a bound expression, for the same reason
    /// `default_sql` is text: the catalog is below the binder, so it cannot
    /// bind anything, and a constraint that had been half-interpreted on the
    /// way through would be a second source of truth beside the `CREATE`
    /// statement the file actually stores.
    pub checks: Vec<CheckInfo>,
}

/// One `CHECK` constraint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckInfo {
    /// The constraint's name, when one was written.
    pub name: Option<Vec<u8>>,
    /// The predicate, as the source text between its parentheses.
    pub expr_sql: Vec<u8>,
}

impl TableInfo {
    /// Returns the position of a column by its folded name.
    pub fn column_position(&self, folded: &[u8]) -> Option<u16> {
        self.columns
            .iter()
            .position(|column| column.folded == folded)
            .map(|index| index as u16)
    }

    /// Returns a column by position.
    pub fn column(&self, position: u16) -> Option<&ColumnInfo> {
        self.columns.get(position as usize)
    }

    /// Returns whether the table has a rowid a query may refer to.
    pub fn has_rowid(&self) -> bool {
        self.kind == TableKind::Table && !self.without_rowid
    }

    /// Returns whether a name is one of the rowid's three spellings and is not
    /// shadowed by a real column.
    ///
    /// SQLite's rule is exactly this: `rowid`, `_rowid_` and `oid` name the
    /// rowid *unless* the table declares a column with that name, in which case
    /// the column wins. A table without a rowid has none of the three.
    pub fn is_rowid_name(&self, folded: &[u8]) -> bool {
        if !self.has_rowid() {
            return false;
        }
        let spelled = folded == b"rowid" || folded == b"_rowid_" || folded == b"oid";
        spelled && self.column_position(folded).is_none()
    }
}

/// The read-only schema the binder resolves names against.
pub trait CatalogView {
    /// Returns the number of attached databases.
    fn database_count(&self) -> usize;

    /// Returns the name of an attached database by index.
    fn database_name(&self, index: usize) -> &[u8];

    /// Returns the index of an attached database by folded name.
    fn database_index(&self, folded: &[u8]) -> Option<usize>;

    /// Returns a table, view or virtual table by name.
    ///
    /// With no qualifier the search follows SQLite's order: `temp`, then
    /// `main`, then every other attached database in attachment order.
    fn find_table(&self, database: Option<&[u8]>, folded: &[u8]) -> Option<&TableInfo>;

    /// Returns the table an index belongs to, together with the index.
    ///
    /// Index names live in the same namespace as table names in SQLite, but
    /// the catalog stores an index inside the table it indexes - which is
    /// where every reader of one wants it. `DROP INDEX` is the caller that
    /// has only the name, so the search lives here rather than being written
    /// out again wherever a name has to be resolved.
    fn find_index(
        &self,
        database: Option<&[u8]>,
        folded: &[u8],
    ) -> Option<(&TableInfo, &IndexInfo)>;

    /// Returns every table of one attached database, in no particular order.
    fn tables_of(&self, database: usize) -> Vec<&TableInfo>;

    /// Returns the schema cookie of an attached database, which a prepared
    /// statement records so it can tell whether the schema moved under it.
    fn schema_cookie(&self, database: usize) -> u32;

    /// Returns the generation of the whole snapshot.
    fn generation(&self) -> u64;
}

/// A catalog held in memory, which is what a test binds against and what the
/// loader produces once it has read `sqlite_schema`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StaticCatalog {
    /// The attached databases, in attachment order, with their cookies.
    pub databases: Vec<(Vec<u8>, u32)>,
    /// Every table, in no particular order.
    pub tables: Vec<TableInfo>,
    /// The generation of this snapshot.
    pub generation: u64,
}

impl StaticCatalog {
    /// Returns a catalog with one `main` database and no objects.
    pub fn empty() -> StaticCatalog {
        StaticCatalog {
            databases: vec![(b"main".to_vec(), 0)],
            tables: Vec::new(),
            generation: 0,
        }
    }

    /// Adds a table, returning the catalog, for building fixtures.
    pub fn with_table(mut self, table: TableInfo) -> StaticCatalog {
        self.tables.push(table);
        self
    }
}

impl CatalogView for StaticCatalog {
    /// Returns the number of attached databases.
    fn database_count(&self) -> usize {
        self.databases.len()
    }

    /// Returns the name of an attached database by index.
    fn database_name(&self, index: usize) -> &[u8] {
        self.databases.get(index).map_or(&[], |(name, _)| name)
    }

    /// Returns the index of an attached database by folded name.
    fn database_index(&self, folded: &[u8]) -> Option<usize> {
        self.databases
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(folded))
    }

    /// Returns a table by name, searching in SQLite's own order.
    fn find_table(&self, database: Option<&[u8]>, folded: &[u8]) -> Option<&TableInfo> {
        if let Some(database) = database {
            let index = self.database_index(database)?;
            return self
                .tables
                .iter()
                .find(|table| table.database == index && table.folded == folded);
        }
        for index in self.search_order() {
            if let Some(found) = self
                .tables
                .iter()
                .find(|table| table.database == index && table.folded == folded)
            {
                return Some(found);
            }
        }
        None
    }

    /// Returns the table an index belongs to, and the index.
    fn find_index(
        &self,
        database: Option<&[u8]>,
        folded: &[u8],
    ) -> Option<(&TableInfo, &IndexInfo)> {
        let wanted = database.and_then(|name| self.database_index(name));
        for table in &self.tables {
            if wanted.is_some_and(|index| index != table.database) {
                continue;
            }
            if let Some(index) = table.indexes.iter().find(|index| index.folded == folded) {
                return Some((table, index));
            }
        }
        None
    }

    /// Returns every table of one attached database.
    fn tables_of(&self, database: usize) -> Vec<&TableInfo> {
        self.tables
            .iter()
            .filter(|table| table.database == database)
            .collect()
    }

    /// Returns the schema cookie of an attached database.
    fn schema_cookie(&self, database: usize) -> u32 {
        self.databases
            .get(database)
            .map_or(0, |(_, cookie)| *cookie)
    }

    /// Returns the generation of the snapshot.
    fn generation(&self) -> u64 {
        self.generation
    }
}

impl StaticCatalog {
    /// Returns the database indexes in the order an unqualified name searches.
    fn search_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = Vec::with_capacity(self.databases.len());
        if let Some(temp) = self
            .databases
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(b"temp"))
        {
            order.push(temp);
        }
        for (index, _) in self.databases.iter().enumerate() {
            if !order.contains(&index) {
                order.push(index);
            }
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a one-column table for the tests below.
    fn table(name: &[u8], database: usize) -> TableInfo {
        TableInfo {
            name: name.to_vec(),
            folded: name.to_ascii_lowercase(),
            database,
            root: 2,
            columns: vec![ColumnInfo {
                name: b"a".to_vec(),
                folded: b"a".to_vec(),
                declared_type: Vec::new(),
                affinity: Affinity::Blob,
                collation: b"binary".to_vec(),
                not_null: false,
                not_null_conflict: None,
                default_sql: None,
                primary_key_position: None,
                hidden: false,
                generated: false,
            }],
            rowid_alias: None,
            without_rowid: false,
            strict: false,
            kind: TableKind::Table,
            create_sql: Vec::new(),
            indexes: Vec::new(),
            checks: Vec::new(),
        }
    }

    /// An unqualified name finds `temp` before `main`, which is the rule that
    /// lets a temp table shadow a real one.
    #[test]
    fn temp_is_searched_before_main() {
        let catalog = StaticCatalog {
            databases: vec![(b"main".to_vec(), 1), (b"temp".to_vec(), 2)],
            tables: vec![table(b"t", 0), table(b"t", 1)],
            generation: 7,
        };
        let found = catalog.find_table(None, b"t").expect("it resolves");
        assert_eq!(found.database, 1);
        let qualified = catalog
            .find_table(Some(b"main"), b"t")
            .expect("it resolves");
        assert_eq!(qualified.database, 0);
    }

    /// The three rowid spellings resolve, and a real column of that name wins.
    #[test]
    fn the_rowid_spellings_resolve_unless_shadowed() {
        let mut plain = table(b"t", 0);
        assert!(plain.is_rowid_name(b"rowid"));
        assert!(plain.is_rowid_name(b"_rowid_"));
        assert!(plain.is_rowid_name(b"oid"));
        assert!(!plain.is_rowid_name(b"id"));

        if let Some(column) = plain.columns.first_mut() {
            column.name = b"oid".to_vec();
            column.folded = b"oid".to_vec();
        }
        assert!(!plain.is_rowid_name(b"oid"));
        assert!(plain.is_rowid_name(b"rowid"));

        let mut without = table(b"t", 0);
        without.without_rowid = true;
        assert!(!without.is_rowid_name(b"rowid"));
    }

    /// A missing database or table is `None`, never a panic.
    #[test]
    fn a_missing_name_is_none() {
        let catalog = StaticCatalog::empty();
        assert!(catalog.find_table(None, b"nope").is_none());
        assert!(catalog.find_table(Some(b"nodb"), b"t").is_none());
        assert_eq!(catalog.database_name(99), b"");
        assert_eq!(catalog.schema_cookie(99), 0);
    }
}
