//! The catalog snapshot and the view the binder resolves names against.
//!
//! Invariant: a snapshot is immutable once built, and every object in it came
//! from the same read of the same file, so no two objects in one snapshot can
//! disagree about the schema they describe. A statement holds its snapshot for
//! as long as it is running; a schema change makes a new one.

use inillucent_sql::catalog_view::{CatalogView, IndexInfo, TableInfo};

/// One attached database's objects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DatabaseCatalog {
    /// The name the database is attached under.
    pub name: Vec<u8>,
    /// The schema cookie the objects were read at.
    pub schema_cookie: u32,
    /// Every table, view and virtual table, with their indexes attached.
    pub tables: Vec<TableInfo>,
}

impl DatabaseCatalog {
    /// Returns a table by folded name.
    pub fn table(&self, folded: &[u8]) -> Option<&TableInfo> {
        self.tables.iter().find(|table| table.folded == folded)
    }
}

/// Every attached database, as of one generation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogSnapshot {
    /// The attached databases, in attachment order.
    pub databases: Vec<DatabaseCatalog>,
    /// The generation, which increases whenever a new snapshot is built.
    pub generation: u64,
    /// The eponymous virtual tables the connection's modules provide.
    ///
    /// They belong to no database and have no `sqlite_schema` row: the name
    /// *is* the table. They are resolved last, so a real table called
    /// `generate_series` shadows the module rather than the other way round -
    /// which is SQLite's order and the only safe one, because the file was
    /// there first.
    pub eponymous: Vec<TableInfo>,
}

impl CatalogSnapshot {
    /// Returns a snapshot holding one database.
    pub fn single(database: DatabaseCatalog, generation: u64) -> CatalogSnapshot {
        CatalogSnapshot {
            databases: vec![database],
            generation,
            eponymous: Vec::new(),
        }
    }

    /// Returns the databases in the order an unqualified name searches them.
    ///
    /// SQLite looks in `temp` first, then `main`, then the rest in attachment
    /// order, so a temporary table shadows a permanent one of the same name.
    fn search_order(&self) -> Vec<usize> {
        let mut order = Vec::with_capacity(self.databases.len());
        if let Some(temp) = self
            .databases
            .iter()
            .position(|database| database.name.eq_ignore_ascii_case(b"temp"))
        {
            order.push(temp);
        }
        for index in 0..self.databases.len() {
            if !order.contains(&index) {
                order.push(index);
            }
        }
        order
    }
}

impl CatalogView for CatalogSnapshot {
    /// Returns the number of attached databases.
    fn database_count(&self) -> usize {
        self.databases.len()
    }

    /// Returns the name of an attached database.
    fn database_name(&self, index: usize) -> &[u8] {
        self.databases
            .get(index)
            .map_or(&[], |database| database.name.as_slice())
    }

    /// Returns the index of an attached database by folded name.
    fn database_index(&self, folded: &[u8]) -> Option<usize> {
        self.databases
            .iter()
            .position(|database| database.name.eq_ignore_ascii_case(folded))
    }

    /// Returns a table by name, qualified or not.
    fn find_table(&self, database: Option<&[u8]>, folded: &[u8]) -> Option<&TableInfo> {
        if let Some(database) = database {
            let index = self.database_index(database)?;
            return self.databases.get(index)?.table(folded);
        }
        for index in self.search_order() {
            if let Some(found) = self.databases.get(index).and_then(|db| db.table(folded)) {
                return Some(found);
            }
        }
        self.eponymous.iter().find(|table| table.folded == folded)
    }

    /// Returns the table an index belongs to, and the index.
    fn find_index(
        &self,
        database: Option<&[u8]>,
        folded: &[u8],
    ) -> Option<(&TableInfo, &IndexInfo)> {
        let order = match database {
            Some(name) => vec![self.database_index(name)?],
            None => self.search_order(),
        };
        for position in order {
            let Some(catalog) = self.databases.get(position) else {
                continue;
            };
            for table in &catalog.tables {
                if let Some(index) = table.indexes.iter().find(|index| index.folded == folded) {
                    return Some((table, index));
                }
            }
        }
        None
    }

    /// Returns every table of every attached database.
    fn every_table(&self) -> Vec<&TableInfo> {
        self.databases
            .iter()
            .flat_map(|catalog| catalog.tables.iter())
            .collect()
    }

    /// Returns every table of one attached database.
    fn tables_of(&self, database: usize) -> Vec<&TableInfo> {
        self.databases
            .get(database)
            .map(|catalog| catalog.tables.iter().collect())
            .unwrap_or_default()
    }

    /// Returns the schema cookie an attached database was read at.
    fn schema_cookie(&self, database: usize) -> u32 {
        self.databases
            .get(database)
            .map_or(0, |database| database.schema_cookie)
    }

    /// Returns the generation of the snapshot.
    fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_sql::catalog_view::TableKind;

    /// Builds an empty table entry for the tests.
    fn table(name: &[u8], database: usize) -> TableInfo {
        TableInfo {
            name: name.to_vec(),
            folded: name.to_ascii_lowercase(),
            database,
            root: 2,
            columns: Vec::new(),
            rowid_alias: None,
            without_rowid: false,
            strict: false,
            autoincrement: false,
            kind: TableKind::Table,
            create_sql: Vec::new(),
            indexes: Vec::new(),
            view: None,
            triggers: Vec::new(),
            analysed_rows: None,
            checks: Vec::new(),
            foreign_keys: Vec::new(),
            foreign_key_triggers: Vec::new(),
            module: None,
        }
    }

    /// The unqualified search order puts `temp` first, so a temp table shadows
    /// a permanent one; a qualified name ignores the order entirely.
    #[test]
    fn temp_shadows_main_unless_the_name_is_qualified() {
        let snapshot = CatalogSnapshot {
            databases: vec![
                DatabaseCatalog {
                    name: b"main".to_vec(),
                    schema_cookie: 3,
                    tables: vec![table(b"t", 0)],
                },
                DatabaseCatalog {
                    name: b"temp".to_vec(),
                    schema_cookie: 4,
                    tables: vec![table(b"t", 1)],
                },
            ],
            generation: 1,
            eponymous: Vec::new(),
        };
        assert_eq!(
            snapshot.find_table(None, b"t").map(|table| table.database),
            Some(1)
        );
        assert_eq!(
            snapshot
                .find_table(Some(b"main"), b"t")
                .map(|table| table.database),
            Some(0)
        );
        assert_eq!(snapshot.schema_cookie(0), 3);
        assert_eq!(snapshot.schema_cookie(1), 4);
    }

    /// A name nobody attached is `None`, and an out-of-range index is empty
    /// rather than a panic.
    #[test]
    fn unknown_names_and_indexes_are_empty() {
        let snapshot = CatalogSnapshot::default();
        assert!(snapshot.find_table(None, b"t").is_none());
        assert_eq!(snapshot.database_name(4), b"");
        assert_eq!(snapshot.schema_cookie(4), 0);
        assert_eq!(snapshot.database_count(), 0);
    }
}
