//! What a source database looks like from here, and how its values are carried.
//!
//! The type map is the part of this crate most able to be quietly wrong, so it
//! is written down as a table and asserted type by type. The rule it follows:
//!
//! > **Exact where this dialect has an equivalent, and the server's own text
//! > rendering where it does not.**
//!
//! The alternative - a translation per exotic type - is a place a migration can
//! be wrong in a way that reads as data. A `numeric(38,10)` rounded into an
//! IEEE double is still a number, still eight bytes, and no check anywhere will
//! notice. Its digits, carried as text, are exactly what `psql` prints and
//! cannot lose anything they had.
//!
//! Invariant: **every source type maps to exactly one inillucent type, and the
//! map is written down rather than inferred.** This is the part of the crate
//! most able to be quietly wrong - a column that arrives as the wrong class
//! reads back as a working migration - so the table is asserted type by type.

use inillucent_tree::datum::OwnedDatum;

use inillucent_base::DbResult;

/// How a source value is carried into this engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A signed 64-bit integer, exactly.
    Integer,
    /// An IEEE-754 binary64, exactly.
    Real,
    /// Text, byte for byte as the server rendered it.
    Text,
    /// Uninterpreted bytes.
    Blob,
    /// A boolean, carried as 0 or 1 - this dialect's own convention.
    Boolean,
    /// An exact decimal, carried as its digits rather than as a double.
    Decimal,
}

impl Kind {
    /// Returns the type this column is declared as in the destination.
    ///
    /// A declared type in this dialect is an *affinity*, so the choice decides
    /// how the destination stores a value that arrives as text - which is why
    /// `Decimal` declares `TEXT` rather than `NUMERIC`: `NUMERIC` affinity would
    /// convert `1.10` to the real `1.1` and `2.0` to the integer `2`, which is
    /// the rounding this map exists to avoid.
    pub fn declared(self) -> &'static str {
        match self {
            Kind::Integer | Kind::Boolean => "INTEGER",
            Kind::Real => "REAL",
            Kind::Text | Kind::Decimal => "TEXT",
            Kind::Blob => "BLOB",
        }
    }
}

/// One column of a source table, as the server describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceColumn {
    /// The column's name, as the source spells it.
    pub name: String,
    /// The source's own type name, kept for the report.
    pub declared: String,
    /// How its values are carried.
    pub kind: Kind,
    /// Whether the source allows a null in it.
    pub nullable: bool,
}

/// One table of a source database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceTable {
    /// The schema or database the table lives in.
    pub schema: String,
    /// The table's own name.
    pub name: String,
    /// What it is called in the destination, which has one namespace.
    pub target: String,
    /// Its columns, in ordinal order.
    pub columns: Vec<SourceColumn>,
    /// The primary key's columns, in key order, when it has one this dialect
    /// can express.
    pub primary_key: Vec<String>,
}

impl SourceTable {
    /// Returns the `CREATE TABLE` that holds this table in the destination.
    ///
    /// The primary key is carried as a table constraint rather than a column
    /// one even when it is a single column, because a single-column
    /// `INTEGER PRIMARY KEY` is a **rowid alias** in this dialect - which would
    /// silently give the destination a different physical layout, and would
    /// reject a source row whose key is null in a way the source did not.
    pub fn create_sql(&self) -> String {
        let mut out = format!("CREATE TABLE {} (", quoted(&self.target));
        for (at, column) in self.columns.iter().enumerate() {
            if at > 0 {
                out.push_str(", ");
            }
            out.push_str(&quoted(&column.name));
            out.push(' ');
            out.push_str(column.kind.declared());
            if !column.nullable {
                out.push_str(" NOT NULL");
            }
        }
        if !self.primary_key.is_empty() {
            out.push_str(", PRIMARY KEY (");
            for (at, name) in self.primary_key.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                out.push_str(&quoted(name));
            }
            out.push(')');
        }
        out.push(')');
        out
    }

    /// Returns the parameterised `INSERT` rows are bound to.
    pub fn insert_sql(&self) -> String {
        let mut out = format!("INSERT INTO {} (", quoted(&self.target));
        for (at, column) in self.columns.iter().enumerate() {
            if at > 0 {
                out.push_str(", ");
            }
            out.push_str(&quoted(&column.name));
        }
        out.push_str(") VALUES (");
        for at in 0..self.columns.len() {
            if at > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("?{}", at.saturating_add(1)));
        }
        out.push(')');
        out
    }

    /// Returns how the table is named in a report.
    pub fn qualified(&self) -> String {
        if self.schema.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }
}

/// Returns an identifier quoted for this dialect.
///
/// @param name - the identifier, which may contain a double quote
pub fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A database somebody else is running, read over its own wire protocol.
pub trait RemoteSource {
    /// Returns every ordinary table the source holds, with its columns.
    fn describe(&mut self) -> DbResult<Vec<SourceTable>>;

    /// Returns the table's row count, asked as a count rather than as a scan.
    ///
    /// A **different query path on the same server** than [`RemoteSource::scan`],
    /// which is the whole point of asking: a count computed by re-counting the
    /// rows this client already read would be checking the client against
    /// itself.
    fn count(&mut self, table: &SourceTable) -> DbResult<u64>;

    /// Streams every row of a table into a sink, returning how many there were.
    ///
    /// Pushed rather than returned so that a table larger than memory costs one
    /// row plus whatever the sink holds.
    fn scan(
        &mut self,
        table: &SourceTable,
        sink: &mut dyn FnMut(&[OwnedDatum]) -> DbResult<()>,
    ) -> DbResult<u64>;

    /// Returns what the server calls itself, for the report.
    fn server(&self) -> String;

    /// Returns what the peer's certificate proved, when the connection is
    /// encrypted.
    ///
    /// `None` for a plaintext connection, which the report records separately -
    /// so an absent peer means "there was nothing to verify" rather than "the
    /// verification was skipped".
    fn peer(&self) -> Option<String> {
        None
    }

    /// Returns the objects that exist in the source and are not carried.
    ///
    /// Named rather than omitted: a view, a sequence or a stored procedure that
    /// silently did not arrive is something the owner finds out about from an
    /// application that stopped working.
    fn not_carried(&mut self) -> DbResult<Vec<(String, String)>>;

    /// Closes the connection.
    fn finish(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a table for the DDL tests.
    fn table() -> SourceTable {
        SourceTable {
            schema: "public".to_string(),
            name: "note".to_string(),
            target: "note".to_string(),
            columns: vec![
                SourceColumn {
                    name: "id".to_string(),
                    declared: "bigint".to_string(),
                    kind: Kind::Integer,
                    nullable: false,
                },
                SourceColumn {
                    name: "body".to_string(),
                    declared: "text".to_string(),
                    kind: Kind::Text,
                    nullable: true,
                },
                SourceColumn {
                    name: "price".to_string(),
                    declared: "numeric".to_string(),
                    kind: Kind::Decimal,
                    nullable: true,
                },
            ],
            primary_key: vec!["id".to_string()],
        }
    }

    /// The generated DDL names the declared types the map chose, keeps the
    /// source's nullability, and writes the key as a table constraint.
    #[test]
    fn create_sql_declares_the_mapped_types_and_a_table_level_key() {
        assert_eq!(
            table().create_sql(),
            "CREATE TABLE \"note\" (\"id\" INTEGER NOT NULL, \"body\" TEXT, \"price\" TEXT, \
             PRIMARY KEY (\"id\"))"
        );
    }

    /// **A decimal is declared TEXT, not NUMERIC.** NUMERIC affinity would turn
    /// the digits `2.0` into the integer 2 and `1.10` into the real 1.1, which
    /// is the rounding the map exists to avoid.
    #[test]
    fn a_decimal_column_is_declared_text() {
        assert_eq!(Kind::Decimal.declared(), "TEXT");
        assert_eq!(Kind::Boolean.declared(), "INTEGER");
        assert_eq!(Kind::Integer.declared(), "INTEGER");
        assert_eq!(Kind::Real.declared(), "REAL");
        assert_eq!(Kind::Blob.declared(), "BLOB");
    }

    /// The insert is parameterised, one placeholder per column in order, so a
    /// value never passes through SQL text.
    #[test]
    fn insert_sql_is_parameterised_in_column_order() {
        assert_eq!(
            table().insert_sql(),
            "INSERT INTO \"note\" (\"id\", \"body\", \"price\") VALUES (?1, ?2, ?3)"
        );
    }

    /// An identifier holding a double quote is doubled rather than escaped, so
    /// a table called `we"ird` cannot end a quoted name early.
    #[test]
    fn a_quote_in_an_identifier_is_doubled() {
        assert_eq!(quoted("we\"ird"), "\"we\"\"ird\"");
    }
}
