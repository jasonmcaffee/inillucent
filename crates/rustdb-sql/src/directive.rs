//! Statements the session carries out itself rather than compiling.
//!
//! Invariant: a directive is a decision, already resolved, with nothing left
//! to look up. Binding a `DROP TABLE` resolves the name and refuses a missing
//! one here; what reaches the session is "free this root page and remove this
//! `sqlite_schema` row", not a name it has to resolve again.
//!
//! Transaction control and DDL are here rather than in the bytecode for a
//! reason the TDD's own DDL protocol describes: their steps are catalog
//! publication, cookie invalidation and lock transitions, none of which the
//! machine's register-and-cursor model expresses. They still run inside the
//! same transaction machinery as DML - the statement savepoint, the journal
//! and the commit are identical - which is what the protocol actually
//! requires. The row-touching part of DDL is ordinary storage work and goes
//! through the same pager as everything else.

use crate::ast::{self, ObjectKind, TransactionBehaviour};
use crate::bind::{no_such_table, refused, unsupported, Binder, BoundExpr, BoundStatement};
use crate::diagnostic::ParseError;
use crate::lexer::Span;

/// How an explicit `BEGIN` acquires its rights.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeginKind {
    /// Take nothing until the first read or write needs it.
    Deferred,
    /// Take the writer's reservation now.
    Immediate,
    /// Take the write lock now, excluding readers too.
    Exclusive,
}

impl BeginKind {
    /// Returns the kind a `BEGIN` clause names, defaulting to DEFERRED.
    pub fn of(behaviour: Option<TransactionBehaviour>) -> BeginKind {
        match behaviour {
            None | Some(TransactionBehaviour::Deferred) => BeginKind::Deferred,
            Some(TransactionBehaviour::Immediate) => BeginKind::Immediate,
            Some(TransactionBehaviour::Exclusive) => BeginKind::Exclusive,
        }
    }
}

/// One key column of an index being created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexKeyColumn {
    /// The table column, when the key is a bare column.
    pub column: u16,
    /// The folded collation name.
    pub collation: Vec<u8>,
    /// Whether the key is stored descending.
    pub descending: bool,
}

/// A statement the session carries out.
#[derive(Clone, Debug, PartialEq)]
pub enum Directive {
    /// `BEGIN`.
    Begin(BeginKind),
    /// `COMMIT` or `END`.
    Commit,
    /// `ROLLBACK`, or `ROLLBACK TO savepoint`.
    Rollback {
        /// The savepoint to roll back to, when one was named.
        savepoint: Option<Vec<u8>>,
    },
    /// `SAVEPOINT name`.
    Savepoint(Vec<u8>),
    /// `RELEASE name`.
    Release(Vec<u8>),
    /// `CREATE TABLE`.
    CreateTable {
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// Which attached database.
        database: usize,
        /// The table name as written.
        name: Vec<u8>,
        /// The byte the name starts at in the statement's source.
        name_offset: u32,
        /// Whether the table already exists.
        exists: bool,
    },
    /// `CREATE INDEX`.
    CreateIndex {
        /// Whether `UNIQUE` was written.
        unique: bool,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// Which attached database.
        database: usize,
        /// The index name as written.
        name: Vec<u8>,
        /// The byte the name starts at in the statement's source.
        name_offset: u32,
        /// The table it indexes.
        table: Vec<u8>,
        /// The root page of that table.
        table_root: u32,
        /// The key columns.
        columns: Vec<IndexKeyColumn>,
        /// Whether the index already exists.
        exists: bool,
    },
    /// `DROP TABLE` or `DROP INDEX`.
    Drop {
        /// Which kind of object.
        kind: ObjectKind,
        /// Whether `IF EXISTS` was written.
        if_exists: bool,
        /// Which attached database.
        database: usize,
        /// The object name.
        name: Vec<u8>,
        /// The root page to free, or zero when the object has none.
        root: u32,
        /// The root pages of the indexes a `DROP TABLE` takes with it.
        index_roots: Vec<u32>,
        /// Whether the object exists.
        exists: bool,
    },
    /// `PRAGMA`.
    Pragma {
        /// The pragma name, folded.
        name: Vec<u8>,
        /// The argument, when one was written.
        argument: Option<PragmaArgument>,
    },
}

/// What a `PRAGMA` was given.
#[derive(Clone, Debug, PartialEq)]
pub enum PragmaArgument {
    /// A bare word, such as `PRAGMA journal_mode = WAL`.
    Name(Vec<u8>),
    /// An expression, such as `PRAGMA user_version = 4`.
    Value(BoundExpr),
}

impl<'a> Binder<'a> {
    /// Binds a statement the session carries out itself.
    pub fn bind_directive(&mut self, statement: &ast::Statement) -> Result<Directive, ParseError> {
        match statement {
            ast::Statement::Begin { behaviour } => Ok(Directive::Begin(BeginKind::of(*behaviour))),
            ast::Statement::Commit => Ok(Directive::Commit),
            ast::Statement::Rollback { savepoint } => Ok(Directive::Rollback {
                savepoint: savepoint.map(|id| self.ast.text(id).to_vec()),
            }),
            ast::Statement::Savepoint(name) => {
                Ok(Directive::Savepoint(self.ast.text(*name).to_vec()))
            }
            ast::Statement::Release(name) => Ok(Directive::Release(self.ast.text(*name).to_vec())),
            ast::Statement::CreateTable {
                temporary,
                if_not_exists,
                database,
                name,
                body,
            } => self.bind_create_table(*temporary, *if_not_exists, *database, *name, body),
            ast::Statement::CreateIndex {
                unique,
                if_not_exists,
                database,
                name,
                table,
                columns,
                filter,
            } => self.bind_create_index(
                *unique,
                *if_not_exists,
                *database,
                *name,
                *table,
                columns,
                *filter,
            ),
            ast::Statement::Drop {
                kind,
                if_exists,
                database,
                name,
            } => self.bind_drop(*kind, *if_exists, *database, *name),
            ast::Statement::Pragma {
                database,
                name,
                value,
            } => self.bind_pragma(*database, *name, value),
            _ => Err(unsupported(
                "this statement is not implemented yet",
                Span::default(),
            )),
        }
    }

    /// Binds a `CREATE TABLE`.
    fn bind_create_table(
        &mut self,
        temporary: bool,
        if_not_exists: bool,
        database: Option<ast::NameId>,
        name: ast::NameId,
        body: &ast::CreateTableBody,
    ) -> Result<Directive, ParseError> {
        if temporary {
            return Err(unsupported("TEMP tables", Span::default()));
        }
        let ast::CreateTableBody::Columns {
            columns,
            without_rowid,
            ..
        } = body
        else {
            return Err(unsupported("CREATE TABLE ... AS SELECT", Span::default()));
        };
        if *without_rowid {
            return Err(unsupported("WITHOUT ROWID tables", Span::default()));
        }
        if columns.is_empty() {
            return Err(refused(
                "a table must have at least one column",
                Span::default(),
            ));
        }
        let index = self.resolve_database(database)?;
        let written = self.ast.text(name).to_vec();
        if written.to_ascii_lowercase().starts_with(b"sqlite_") {
            return Err(refused(
                format!(
                    "object name reserved for internal use: {}",
                    String::from_utf8_lossy(&written)
                ),
                Span::default(),
            ));
        }
        let folded = self.ast.folded(name).to_vec();
        let database_name = self.catalog.database_name(index).to_vec();
        let exists = self
            .catalog
            .find_table(Some(database_name.as_slice()), &folded)
            .is_some();
        if exists && !if_not_exists {
            return Err(refused(
                format!("table {} already exists", String::from_utf8_lossy(&written)),
                Span::default(),
            ));
        }
        self.record_write_dependency(index);
        Ok(Directive::CreateTable {
            if_not_exists,
            database: index,
            name: written,
            name_offset: self.name_offset(name),
            exists,
        })
    }

    /// Binds a `CREATE INDEX`.
    ///
    /// The parameters are the grammar's own fields, passed straight through
    /// from the statement rather than bundled into a struct that would exist
    /// only to have fewer of them.
    #[allow(clippy::too_many_arguments)]
    fn bind_create_index(
        &mut self,
        unique: bool,
        if_not_exists: bool,
        database: Option<ast::NameId>,
        name: ast::NameId,
        table: ast::NameId,
        columns: &[ast::IndexedColumn],
        filter: Option<ast::ExprId>,
    ) -> Result<Directive, ParseError> {
        if filter.is_some() {
            return Err(unsupported("partial indexes", Span::default()));
        }
        let index = self.resolve_database(database)?;
        let database_name = self.catalog.database_name(index).to_vec();
        let table_folded = self.ast.folded(table).to_vec();
        let Some(target) = self
            .catalog
            .find_table(Some(database_name.as_slice()), &table_folded)
            .cloned()
        else {
            return Err(no_such_table(self.ast.text(table), Span::default()));
        };
        let written = self.ast.text(name).to_vec();
        let folded = self.ast.folded(name).to_vec();
        let exists = self
            .catalog
            .find_index(Some(database_name.as_slice()), &folded)
            .is_some();
        if exists && !if_not_exists {
            return Err(refused(
                format!("index {} already exists", String::from_utf8_lossy(&written)),
                Span::default(),
            ));
        }
        let mut keys = Vec::with_capacity(columns.len());
        for column in columns {
            let Some(ast::Expr::Column {
                table: None,
                column: name,
                ..
            }) = self.ast.expr(column.expr)
            else {
                return Err(unsupported("indexes on expressions", Span::default()));
            };
            let folded = self.ast.folded(*name).to_vec();
            let Some(position) = target.column_position(&folded) else {
                return Err(crate::bind::no_such_column(
                    self.ast.text(*name),
                    Span::default(),
                ));
            };
            let collation = match column.collation {
                Some(collation) => self.ast.folded(collation).to_vec(),
                None => target
                    .column(position)
                    .map(|column| column.collation.clone())
                    .unwrap_or_else(|| b"binary".to_vec()),
            };
            keys.push(IndexKeyColumn {
                column: position,
                collation,
                descending: column.order == ast::SortOrder::Descending,
            });
        }
        self.record_write_dependency(index);
        Ok(Directive::CreateIndex {
            unique,
            if_not_exists,
            database: index,
            name: written,
            name_offset: self.name_offset(name),
            table: target.name.clone(),
            table_root: target.root,
            columns: keys,
            exists,
        })
    }

    /// Binds a `DROP TABLE` or `DROP INDEX`.
    fn bind_drop(
        &mut self,
        kind: ObjectKind,
        if_exists: bool,
        database: Option<ast::NameId>,
        name: ast::NameId,
    ) -> Result<Directive, ParseError> {
        if matches!(kind, ObjectKind::View | ObjectKind::Trigger) {
            return Err(unsupported("DROP VIEW and DROP TRIGGER", Span::default()));
        }
        let index = self.resolve_database(database)?;
        let database_name = self.catalog.database_name(index).to_vec();
        let written = self.ast.text(name).to_vec();
        let folded = self.ast.folded(name).to_vec();
        self.record_write_dependency(index);
        if kind == ObjectKind::Table {
            let found = self
                .catalog
                .find_table(Some(database_name.as_slice()), &folded)
                .cloned();
            let Some(table) = found else {
                if if_exists {
                    return Ok(Directive::Drop {
                        kind,
                        if_exists,
                        database: index,
                        name: written,
                        root: 0,
                        index_roots: Vec::new(),
                        exists: false,
                    });
                }
                return Err(no_such_table(&written, Span::default()));
            };
            let index_roots = table
                .indexes
                .iter()
                .map(|index| index.root)
                .filter(|root| *root != 0)
                .collect();
            return Ok(Directive::Drop {
                kind,
                if_exists,
                database: index,
                name: written,
                root: table.root,
                index_roots,
                exists: true,
            });
        }
        let found = self.find_index_root(index, &folded);
        let Some(root) = found else {
            if if_exists {
                return Ok(Directive::Drop {
                    kind,
                    if_exists,
                    database: index,
                    name: written,
                    root: 0,
                    index_roots: Vec::new(),
                    exists: false,
                });
            }
            return Err(refused(
                format!("no such index: {}", String::from_utf8_lossy(&written)),
                Span::default(),
            ));
        };
        Ok(Directive::Drop {
            kind,
            if_exists,
            database: index,
            name: written,
            root,
            index_roots: Vec::new(),
            exists: true,
        })
    }

    /// Binds a `PRAGMA`.
    fn bind_pragma(
        &mut self,
        _database: Option<ast::NameId>,
        name: ast::NameId,
        value: &ast::PragmaValue,
    ) -> Result<Directive, ParseError> {
        let argument = match value {
            ast::PragmaValue::None => None,
            ast::PragmaValue::Name(name) => {
                Some(PragmaArgument::Name(self.ast.text(*name).to_vec()))
            }
            ast::PragmaValue::Value(expr) => Some(PragmaArgument::Value(self.bind_expr(*expr)?)),
        };
        Ok(Directive::Pragma {
            name: self.ast.folded(name).to_vec(),
            argument,
        })
    }

    /// Resolves a schema qualifier to an attached database index.
    fn resolve_database(&self, database: Option<ast::NameId>) -> Result<usize, ParseError> {
        let Some(id) = database else {
            return Ok(0);
        };
        let folded = self.ast.folded(id);
        self.catalog.database_index(folded).ok_or_else(|| {
            refused(
                format!(
                    "unknown database {}",
                    String::from_utf8_lossy(self.ast.text(id))
                ),
                Span::default(),
            )
        })
    }

    /// Returns the byte an identifier starts at in the statement's source.
    ///
    /// The canonical `sqlite_schema` text is the statement from its object
    /// name onward, which is how `IF NOT EXISTS` and the schema qualifier come
    /// to be missing from what SQLite stores. Slicing the source is the only
    /// way to reproduce that exactly; rendering the tree back would normalise
    /// whitespace and quoting the user chose.
    fn name_offset(&self, name: ast::NameId) -> u32 {
        self.ast.name(name).map_or(0, |name| name.span.start)
    }

    /// Returns an index's root page, searching every table of a database.
    fn find_index_root(&self, database: usize, folded: &[u8]) -> Option<u32> {
        let name = self.catalog.database_name(database).to_vec();
        self.catalog
            .find_index(Some(name.as_slice()), folded)
            .map(|(_, index)| index.root)
    }
}

/// Returns whether a bound statement is one the session carries out itself.
pub fn is_directive(statement: &BoundStatement) -> bool {
    matches!(statement, BoundStatement::Directive(_))
}
