//! The pragmas that describe the schema: what tables, columns and indexes exist.
//!
//! Invariant: **these answer about the catalog rather than about the file.**
//! `table_info` reads the declaration a `CREATE TABLE` was parsed into, so it
//! reports what the schema says a column is, which is what SQLite's own answer
//! means and is not always what a row happens to hold.

use inillucent_base::DbResult;
use inillucent_sql::declare::argument_text;
use inillucent_sql::directive::PragmaArgument;
use inillucent_tree::datum::OwnedDatum;

use crate::Outcome;

use super::*;

impl crate::ImportedDatabase {
    /// Returns one row per database this connection holds, in schema order.
    ///
    /// `main` first, then the connection's temporary database when it has made
    /// one, then the attachments in the order they arrived - which is the order
    /// an unqualified name is resolved in, minus `temp`'s place at the front of
    /// it, and the cheapest end-to-end check that the schema set is what the
    /// connection thinks it is.
    ///
    /// A database with no file - `temp`, and `ATTACH ':memory:'` - reports an
    /// empty path, which is what SQLite reports for the same thing.
    pub(crate) fn database_list(&self) -> Vec<Vec<OwnedDatum>> {
        let mut rows = Vec::new();
        // **`temp` is listed once the session has made something temporary and
        // not before, which is what the reference does** (task-2066 section
        // 4.2, item 27). The audit read this as a missing row; the pinned
        // oracle disagrees. `pragma.rs`'s `the_schema_pragmas_describe_the_schema`
        // runs `SELECT seq, name FROM pragma_database_list` against SQLite
        // 3.53.4 on a connection that has made no temporary object, and SQLite
        // answers one row. Emitting `temp` unconditionally was tried here and
        // that differential caught it.
        for (seq, at) in self.schema_numbers().into_iter().enumerate() {
            let (name, file) = match at {
                crate::MAIN => (
                    b"main".to_vec(),
                    self.storage.path.to_string_lossy().as_bytes().to_vec(),
                ),
                _ => match self.session_state.schema_at(at) {
                    Some(held) => (
                        held.name.clone(),
                        held.path
                            .as_ref()
                            .map(|path| path.to_string_lossy().as_bytes().to_vec())
                            .unwrap_or_default(),
                    ),
                    None => continue,
                },
            };
            rows.push(vec![
                OwnedDatum::Int(seq as i64),
                OwnedDatum::Text(name),
                OwnedDatum::Text(file),
            ]);
        }
        rows
    }
    /// Reports the foreign keys one table declares, in SQLite's own columns.
    ///
    /// One row per key column rather than per key: a composite key reports its
    /// columns in `seq` order under one `id`, which is how an application
    /// reconstructs the pair.
    ///
    /// @param argument - the table named in the pragma
    /// @param at - the attached database the pragma was qualified with
    pub(crate) fn pragma_foreign_key_list(
        &self,
        argument: Option<&PragmaArgument>,
        at: Option<usize>,
    ) -> DbResult<Outcome> {
        let names = vec![
            "id".into(),
            "seq".into(),
            "table".into(),
            "from".into(),
            "to".into(),
            "on_update".into(),
            "on_delete".into(),
            "match".into(),
        ];
        let Some(table) = self.named_table(argument, at) else {
            return Ok(Outcome {
                rows: Vec::new(),
                names: std::rc::Rc::new(names),
                changes: Default::default(),
            });
        };
        let mut rows = Vec::new();
        for key in &table.foreign_keys {
            let parent = self
                .schema
                .tables
                .iter()
                .find(|candidate| candidate.folded == key.parent_folded);
            let targets = parent
                .and_then(|parent| inillucent_sql::foreign_key::parent_columns(key, parent))
                .unwrap_or_default();
            for (position, column) in key.columns.iter().enumerate() {
                let from = table
                    .columns
                    .get(usize::from(*column))
                    .map(|info| info.name.clone())
                    .unwrap_or_default();
                rows.push(vec![
                    OwnedDatum::Int(i64::from(key.id)),
                    OwnedDatum::Int(position as i64),
                    OwnedDatum::Text(key.parent.clone()),
                    OwnedDatum::Text(from),
                    match targets.get(position) {
                        Some(name) => OwnedDatum::Text(name.clone()),
                        None => OwnedDatum::Null,
                    },
                    OwnedDatum::Text(action_name(key.on_update).as_bytes().to_vec()),
                    OwnedDatum::Text(action_name(key.on_delete).as_bytes().to_vec()),
                    OwnedDatum::Text(if key.match_clause.is_empty() {
                        b"NONE".to_vec()
                    } else {
                        key.match_clause.clone()
                    }),
                ]);
            }
        }
        Ok(Outcome {
            rows,
            names: std::rc::Rc::new(names),
            changes: Default::default(),
        })
    }
    /// Describes one table's columns.
    ///
    /// `extended` is `table_xinfo`, which differs in two ways: it shows the
    /// columns `table_info` hides - a virtual table's arguments and a generated
    /// column - and it carries a seventh column saying which kind of hidden
    /// each one is. That seventh column is why an ORM can tell a generated
    /// column from an ordinary one, and it was missing.
    ///
    /// @param argument - the table named in the pragma
    /// @param extended - whether this is the `xinfo` spelling
    /// @param at - the attached database the pragma was qualified with
    pub(crate) fn pragma_table_info(
        &self,
        argument: Option<&PragmaArgument>,
        extended: bool,
        at: Option<usize>,
    ) -> DbResult<Outcome> {
        // **The names come first and are answered even when nothing matched.**
        // A statement's result columns are a fact about the *pragma*, not about
        // whether the table it was asked about exists - and the table-valued
        // form reads them at catalog time, with no argument at all, to learn
        // what columns `pragma_table_info` declares.
        let mut names: Vec<String> = ["cid", "name", "type", "notnull", "dflt_value", "pk"]
            .iter()
            .map(|held| (*held).to_string())
            .collect();
        if extended {
            names.push("hidden".to_string());
        }
        let Some(table) = self.named_table(argument, at) else {
            return Ok(Outcome {
                rows: Vec::new(),
                names: std::rc::Rc::new(names),
                changes: Default::default(),
            });
        };
        // **A view has columns, and they are the columns its `SELECT`
        // produces.** Nothing in the file records them, so they are bound here
        // - which is why `PRAGMA table_info(v)` answered nothing at all, and
        // why every ORM that reads this pragma could not see a view.
        let bound;
        let columns: &[inillucent_sql::catalog_view::ColumnInfo] =
            if table.kind == inillucent_sql::catalog_view::TableKind::View {
                bound = self.view_columns(table);
                &bound
            } else {
                &table.columns
            };
        let mut rows = Vec::new();
        // **`cid` counts the columns the pragma reports, not the columns the
        // table declares.** They are the same number until a table has a
        // generated column, which the plain form hides - and SQLite then
        // numbers what is left 0, 1, 2 rather than leaving a gap where the
        // hidden one was. The extended form shows every column, so its `cid`
        // is the declared position.
        let mut cid = 0i64;
        for (position, column) in columns.iter().enumerate() {
            if !extended && (column.hidden || column.generated) {
                continue;
            }
            let reported = if extended { position as i64 } else { cid };
            cid = cid.saturating_add(1);
            // 1 is a virtual table's hidden column; 2 is a VIRTUAL generated
            // column and 3 a STORED one. The two generated codes are the way
            // round SQLite has them, which is not the way round the keywords
            // suggest.
            let hidden = if column.generated {
                if column.stored {
                    3
                } else {
                    2
                }
            } else {
                i64::from(column.hidden)
            };
            let mut row = vec![
                OwnedDatum::Int(reported),
                OwnedDatum::Text(column.name.clone()),
                OwnedDatum::Text(column.declared_type.clone()),
                OwnedDatum::Int(i64::from(column.not_null)),
                match &column.default_sql {
                    Some(text) => OwnedDatum::Text(text.clone()),
                    None => OwnedDatum::Null,
                },
                // `primary_key_position` is already one-based, which is what
                // SQLite's `pk` column holds, and zero for a column that is not
                // in the key.
                OwnedDatum::Int(column.primary_key_position.map(i64::from).unwrap_or(0)),
            ];
            if extended {
                row.push(OwnedDatum::Int(hidden));
            }
            rows.push(row);
        }
        Ok(Outcome {
            rows,
            names: std::rc::Rc::new(names),
            changes: Default::default(),
        })
    }
    /// Returns the columns a view's `SELECT` produces.
    ///
    /// Bound rather than stored, because the file records a view's text and not
    /// its shape. A view whose body no longer binds - a table it reads was
    /// dropped - answers no columns rather than failing the pragma, which is
    /// what SQLite does with the same thing.
    ///
    /// @param table - the view
    pub(crate) fn view_columns(
        &self,
        table: &inillucent_sql::catalog_view::TableInfo,
    ) -> Vec<inillucent_sql::catalog_view::ColumnInfo> {
        let Some(body) = table.view.as_ref() else {
            return Vec::new();
        };
        let authorizer = inillucent_sql::bind::AllowAll;
        // **The body is read as what it is: schema (task-1972).** A view over a
        // registered function reports the columns it would produce only if the
        // binder can resolve the name, which needs the connection's
        // registrations; and a view naming a function a schema may not name
        // reports no columns, which is the same answer selecting from it gives.
        let externals = self.external_functions();
        let mut binder =
            inillucent_sql::bind::Binder::new(&self.schema.catalog, &body.ast, &authorizer)
                .with_functions(&externals)
                .with_collations(&self.session_state.collations)
                .with_trusted_schema(self.session_state.registry.policy().trusted_schema)
                .in_schema();
        let Ok(bound) = binder.bind_select(body.select) else {
            return Vec::new();
        };
        let declared = body.columns.clone();
        bound
            .columns
            .iter()
            .enumerate()
            .map(|(position, column)| {
                let name = declared
                    .get(position)
                    .cloned()
                    .unwrap_or_else(|| column.name.clone());
                inillucent_sql::catalog_view::ColumnInfo {
                    folded: name.to_ascii_lowercase(),
                    name,
                    declared_type: column.declared_type.clone(),
                    affinity: inillucent_value::affinity::for_column(&column.declared_type),
                    collation: b"binary".to_vec(),
                    not_null: false,
                    not_null_conflict: None,
                    primary_key_conflict: None,
                    default_sql: None,
                    primary_key_position: None,
                    hidden: false,
                    generated: false,
                    stored: true,
                    generated_sql: None,
                }
            })
            .collect()
    }
    /// Lists one table's indexes, newest first, as SQLite does.
    ///
    /// @param argument - the table named in the pragma
    /// @param at - the attached database the pragma was qualified with
    pub(crate) fn pragma_index_list(
        &self,
        argument: Option<&PragmaArgument>,
        at: Option<usize>,
    ) -> DbResult<Outcome> {
        let names: Vec<String> = ["seq", "name", "unique", "origin", "partial"]
            .iter()
            .map(|held| (*held).to_string())
            .collect();
        let Some(table) = self.named_table(argument, at) else {
            return Ok(Outcome {
                rows: Vec::new(),
                names: std::rc::Rc::new(names),
                changes: Default::default(),
            });
        };
        // **A `WITHOUT ROWID` table's own primary key has no tree of its own
        // - and `PRAGMA index_list` reports it anyway.** SQLite writes no
        // `sqlite_autoindex` row to `sqlite_schema` for it and builds no
        // second b-tree (`convertToWithoutRowidTable` repoints the in-memory
        // `Index` at the table's own root and skips the schema write), but
        // that `Index` stays on the table's index chain, and `index_list`
        // walks the chain rather than the schema table. So the reference
        // answers `[0, "sqlite_autoindex_u_1", 1, "pk", 0]` for a `WITHOUT
        // ROWID` table's composite key, which this row used to filter out on
        // the theory that SQLite never lists it - checked against the pinned
        // `sqlite3.c`'s own `PragTyp_INDEX_LIST` case, which has no such
        // filter.
        let rows: Vec<Vec<OwnedDatum>> = table
            .indexes
            .iter()
            .rev()
            .enumerate()
            .map(|(seq, index)| {
                let automatic = index.name.starts_with(b"sqlite_autoindex_");
                // **`v` for an index a module owns, which SQLite has no value
                // for because it has no such index (task-1979, R19).** It used
                // to report `c`, the value for an index a `CREATE INDEX`
                // statement made, and nothing else distinguished the two - so
                // `inillucent indexes`, which reads `sqlite_master` and finds a
                // vector index recorded there as a virtual table, had no second
                // place to look. SQLite's three values keep their meanings.
                let origin = match index.origin {
                    inillucent_sql::catalog_view::IndexOrigin::Module => b"v".to_vec(),
                    _ if automatic => b"pk".to_vec(),
                    _ => b"c".to_vec(),
                };
                vec![
                    OwnedDatum::Int(seq as i64),
                    OwnedDatum::Text(index.name.clone()),
                    OwnedDatum::Int(i64::from(index.unique)),
                    OwnedDatum::Text(origin),
                    // **The `partial` column, which was a hard zero while a
                    // partial index could not be created.** It can now, and an
                    // application asks this column precisely to find out
                    // whether an index answers every row - so answering `0` for
                    // one that does not is the kind of difference that only
                    // shows up in somebody's data.
                    OwnedDatum::Int(i64::from(index.partial_sql.is_some())),
                ]
            })
            .collect();
        Ok(Outcome {
            rows,
            names: std::rc::Rc::new(names),
            changes: Default::default(),
        })
    }
    /// Describes one index's key columns.
    ///
    /// `extended` is `index_xinfo`, which answered the same three columns as
    /// `index_info` and so said nothing the plain form did not. It has six:
    /// the key's sort direction, the collation it is ordered by, and whether
    /// the entry is a *key* column or one of the row-identifying columns the
    /// index carries after them - plus the trailing row for the rowid itself,
    /// which is what makes an index's real key visible.
    ///
    /// @param argument - the index named in the pragma
    /// @param extended - whether this is the `xinfo` spelling
    /// @param at - the attached database the pragma was qualified with
    pub(crate) fn pragma_index_info(
        &self,
        argument: Option<&PragmaArgument>,
        extended: bool,
        at: Option<usize>,
    ) -> DbResult<Outcome> {
        let mut names: Vec<String> = ["seqno", "cid", "name"]
            .iter()
            .map(|held| (*held).to_string())
            .collect();
        if extended {
            for held in ["desc", "coll", "key"] {
                names.push(held.to_string());
            }
        }
        let empty = Outcome {
            rows: Vec::new(),
            names: std::rc::Rc::new(names.clone()),
            changes: Default::default(),
        };
        let Some(argument) = argument else {
            return Ok(empty);
        };
        let wanted = argument_text(argument).to_ascii_lowercase().into_bytes();
        let found = self.schema.tables.iter().find_map(|table| {
            if !at.is_none_or(|named| table.database == named) {
                return None;
            }
            table
                .indexes
                .iter()
                .find(|index| index.folded == wanted)
                .map(|index| (table, index))
        });
        let Some((table, index)) = found else {
            return Ok(empty);
        };
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::new();
        for (seq, key) in index.columns.iter().enumerate() {
            let mut row = vec![
                OwnedDatum::Int(seq as i64),
                OwnedDatum::Int(key.column.map(i64::from).unwrap_or(-2)),
                match key.column.and_then(|at| table.column(at)) {
                    Some(column) => OwnedDatum::Text(column.name.clone()),
                    None => OwnedDatum::Null,
                },
            ];
            if extended {
                row.push(OwnedDatum::Int(i64::from(key.declared_descending)));
                row.push(OwnedDatum::Text(collation_name(&key.collation)));
                row.push(OwnedDatum::Int(1));
            }
            rows.push(row);
        }
        if extended && table.has_rowid() {
            // **The row every index has and none of them declares.** An index
            // over a rowid table carries the rowid after its key columns, which
            // is how a lookup finds the table row; SQLite reports it as cid -1
            // with a NULL name and `key` 0, and an application reading this to
            // work out an index's real width needs it.
            rows.push(vec![
                OwnedDatum::Int(rows.len() as i64),
                OwnedDatum::Int(-1),
                OwnedDatum::Null,
                OwnedDatum::Int(0),
                OwnedDatum::Text(b"BINARY".to_vec()),
                OwnedDatum::Int(0),
            ]);
        }
        Ok(Outcome {
            rows,
            names: std::rc::Rc::new(names),
            changes: Default::default(),
        })
    }
    /// Lists every table in the schema, or one of them by name.
    ///
    /// **A view is reported as a view, and the schema tables are reported.**
    /// Every row used to say `table`, so a caller reading this to decide what
    /// it could write to was told a view was writable; and `sqlite_schema` and
    /// `sqlite_temp_schema` were missing, which SQLite lists last and a tool
    /// walking the catalog expects to find.
    ///
    /// **`PRAGMA table_list(t)` narrows to one table.** The pinned reference's
    /// own `PragTyp_TABLE_LIST` case skips every row whose name does not match
    /// the argument case-insensitively; this used to answer the full list
    /// regardless, which told a caller asking about one table what every
    /// table looked like.
    ///
    /// @param argument - the table to narrow to, when one was given
    /// @param at - the attached database the pragma was qualified with
    pub(crate) fn pragma_table_list(
        &self,
        argument: Option<&PragmaArgument>,
        at: Option<usize>,
    ) -> DbResult<Outcome> {
        let wanted = argument.map(|argument| argument_text(argument).to_ascii_lowercase());
        let matches = |folded: &[u8]| {
            wanted
                .as_deref()
                .is_none_or(|wanted| folded == wanted.as_bytes())
        };
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::new();
        // Newest first, which is the order SQLite reports and the order
        // `index_list` already uses for the same reason.
        for table in self.schema.tables.iter().rev() {
            if table.folded == b"sqlite_schema" || table.folded == b"sqlite_temp_schema" {
                continue;
            }
            if !matches(&table.folded) {
                continue;
            }
            // A qualified `PRAGMA aux.table_list` lists that database's tables
            // and no others, which is what the qualifier is for.
            if !at.is_none_or(|named| table.database == named) {
                continue;
            }
            let kind: &[u8] = match table.kind {
                inillucent_sql::catalog_view::TableKind::View => b"view",
                inillucent_sql::catalog_view::TableKind::Virtual => b"virtual",
                _ => b"table",
            };
            let ncol = if table.kind == inillucent_sql::catalog_view::TableKind::View {
                self.view_columns(table).len() as i64
            } else {
                table.columns.len() as i64
            };
            rows.push(vec![
                OwnedDatum::Text(b"main".to_vec()),
                OwnedDatum::Text(table.name.clone()),
                OwnedDatum::Text(kind.to_vec()),
                OwnedDatum::Int(ncol),
                // **`wr` reported the table's own flag, not a constant.** Every
                // row said 0 regardless of how the table was declared, so a
                // `CREATE TABLE ... WITHOUT ROWID` table was told apart from an
                // ordinary one by nothing this pragma answers - `table_info`
                // still named its columns correctly, but a caller that reads
                // `table_list` to decide whether a table has a rowid before
                // choosing how to reference a row was told every table did.
                OwnedDatum::Int(i64::from(table.without_rowid)),
                OwnedDatum::Int(i64::from(table.strict)),
            ]);
        }
        for (schema, name) in [
            (b"main".as_slice(), b"sqlite_schema".as_slice()),
            (b"temp".as_slice(), b"sqlite_temp_schema".as_slice()),
        ] {
            if !matches(name) {
                continue;
            }
            rows.push(vec![
                OwnedDatum::Text(schema.to_vec()),
                OwnedDatum::Text(name.to_vec()),
                OwnedDatum::Text(b"table".to_vec()),
                OwnedDatum::Int(5),
                OwnedDatum::Int(0),
                OwnedDatum::Int(0),
            ]);
        }
        let rows: Vec<Vec<OwnedDatum>> = rows;
        Ok(Outcome {
            rows,
            names: std::rc::Rc::new(vec![
                "schema".into(),
                "name".into(),
                "type".into(),
                "ncol".into(),
                "wr".into(),
                "strict".into(),
            ]),
            changes: Default::default(),
        })
    }
    /// Lists the collations this connection can order by.
    ///
    /// The three built in, then whatever the application registered, which is
    /// what SQLite reports and in the same shape.
    pub(crate) fn pragma_collation_list(&self) -> Outcome {
        // The order is the reference's own, which is neither alphabetical nor
        // registration order - it is the order its hash table happens to walk
        // in. Neither engine's order carries meaning, so matching the pinned
        // build's costs nothing and makes the two transcripts comparable.
        let mut names: Vec<String> = ["decimal", "BINARY", "NOCASE", "RTRIM", "uint"]
            .iter()
            .map(|held| (*held).to_string())
            .collect();
        for (name, _) in &self.session_state.collations {
            if !names.iter().any(|held| held.eq_ignore_ascii_case(name)) {
                names.push(name.clone());
            }
        }
        Outcome {
            rows: names
                .iter()
                .enumerate()
                .map(|(seq, name)| {
                    vec![
                        OwnedDatum::Int(seq as i64),
                        OwnedDatum::Text(name.as_bytes().to_vec()),
                    ]
                })
                .collect(),
            names: std::rc::Rc::new(vec!["seq".into(), "name".into()]),
            changes: Default::default(),
        }
    }
    /// Returns the table a pragma's argument names.
    pub(crate) fn named_table(
        &self,
        argument: Option<&PragmaArgument>,
        at: Option<usize>,
    ) -> Option<&inillucent_sql::catalog_view::TableInfo> {
        let argument = argument?;
        let wanted = argument_text(argument).to_ascii_lowercase().into_bytes();
        self.schema
            .tables
            .iter()
            .find(|table| table.folded == wanted && at.is_none_or(|named| table.database == named))
            .or(
                if wanted == b"sqlite_schema" || wanted == b"sqlite_master" {
                    Some(&self.schema.schema_info)
                } else {
                    None
                },
            )
    }
}
