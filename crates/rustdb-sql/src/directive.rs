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
use crate::catalog_view::CatalogView;
use crate::catalog_view::TableKind;
use crate::diagnostic::ParseError;
use crate::lexer::Span;
use rustdb_value::Collation;

/// Returns the direct children of an expression node.
///
/// The arena has no walker of its own, and the only caller that needs one is
/// the generated-column check, so it lives beside it rather than becoming a
/// method every other reader would have to ignore.
fn expression_children(ast: &crate::ast::Ast, expr: ast::ExprId) -> Vec<ast::ExprId> {
    let mut out = Vec::new();
    let Some(node) = ast.expr(expr) else {
        return out;
    };
    match node {
        ast::Expr::Unary { operand, .. } => out.push(*operand),
        ast::Expr::Binary { left, right, .. } => {
            out.push(*left);
            out.push(*right);
        }
        ast::Expr::Collate { operand, .. } | ast::Expr::Cast { operand, .. } => out.push(*operand),
        ast::Expr::IsNull { operand, .. } => out.push(*operand),
        ast::Expr::Is { left, right, .. } => {
            out.push(*left);
            out.push(*right);
        }
        ast::Expr::Between {
            operand, low, high, ..
        } => {
            out.push(*operand);
            out.push(*low);
            out.push(*high);
        }
        ast::Expr::In { operand, rhs, .. } => {
            out.push(*operand);
            if let ast::InRhs::List(items) = rhs {
                out.extend(items.iter().copied());
            }
        }
        ast::Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            if let Some(operand) = operand {
                out.push(*operand);
            }
            for (when, then) in branches {
                out.push(*when);
                out.push(*then);
            }
            if let Some(otherwise) = otherwise {
                out.push(*otherwise);
            }
        }
        ast::Expr::Pattern {
            operand,
            pattern,
            escape,
            ..
        } => {
            out.push(*operand);
            out.push(*pattern);
            if let Some(escape) = escape {
                out.push(*escape);
            }
        }
        ast::Expr::Function { arguments, .. } => {
            if let Some(arguments) = arguments {
                out.extend(arguments.iter().copied());
            }
        }
        _ => {}
    }
    out
}

/// Returns whether a stored expression names an identifier.
///
/// It lexes rather than searches, so a column called `a` is not found inside
/// `abc` or inside the text of a string literal.
fn mentions_name(sql: &[u8], folded: &[u8]) -> bool {
    let mut lexer = crate::lexer::Lexer::at(sql, 0);
    loop {
        let Ok(token) = lexer.next_token() else {
            return false;
        };
        match token.kind {
            crate::lexer::TokenKind::EndOfInput => return false,
            crate::lexer::TokenKind::Identifier { keyword: None, .. } => {
                if token.span.slice(sql).to_ascii_lowercase() == folded {
                    return true;
                }
            }
            _ => {}
        }
    }
}

/// Returns the failure `REINDEX` gives for a name that is nothing it knows.
fn no_such_collation_sequence(name: &[u8], span: Span) -> ParseError {
    ParseError::new(
        crate::diagnostic::ParseErrorKind::Unexpected {
            found: format!(
                "unable to identify the object to be reindexed: {}",
                String::from_utf8_lossy(name)
            ),
            expected: Vec::new(),
        },
        span,
    )
}

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

/// What an `ALTER TABLE` does, with every name already resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlterKind {
    /// `RENAME TO`.
    RenameTable {
        /// The new name, as written.
        to: Vec<u8>,
    },
    /// `RENAME COLUMN a TO b`.
    RenameColumn {
        /// The column's current name, as stored.
        from: Vec<u8>,
        /// Its new name, as written.
        to: Vec<u8>,
    },
    /// `ADD COLUMN`.
    AddColumn {
        /// Where the definition starts in the statement's own source.
        ///
        /// The offsets rather than the text, for the same reason `CREATE TABLE`
        /// carries an offset: the executor has the statement's source and
        /// slicing it there keeps the *written* definition - its spacing, its
        /// case and its comments - rather than something re-rendered from the
        /// parse.
        start: u32,
        /// Where it ends.
        end: u32,
    },
    /// `DROP COLUMN`.
    DropColumn {
        /// The column's name, as stored.
        name: Vec<u8>,
        /// Its declared position, which is the record slot to remove.
        position: u16,
    },
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
    /// `ALTER TABLE`.
    Alter {
        /// Which attached database.
        database: usize,
        /// The table being altered, by its stored name.
        table: Vec<u8>,
        /// What to do to it.
        action: AlterKind,
    },
    /// `REINDEX`, over one index, one table's indexes, or everything.
    Reindex {
        /// Which attached database.
        database: usize,
        /// The indexes to rebuild, by name.
        indexes: Vec<Vec<u8>>,
    },
    /// `VACUUM`, which rebuilds the database into a fresh file.
    Vacuum {
        /// Which attached database.
        database: usize,
        /// The file `VACUUM INTO` writes the rebuilt copy to.
        ///
        /// A string literal, as SQLite's grammar has it. `INTO` leaves the
        /// database it was run on completely alone, which is the difference
        /// between the two forms and the reason the path is carried rather
        /// than resolved here.
        into: Option<Vec<u8>>,
    },
    /// `ANALYZE`, over one object or the whole schema.
    Analyze {
        /// Which attached database.
        database: usize,
        /// The one table to measure, or nothing for all of them.
        table: Option<Vec<u8>>,
    },
    /// `CREATE VIEW`.
    CreateView {
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// Which attached database.
        database: usize,
        /// The view name as written.
        name: Vec<u8>,
        /// The byte the name starts at in the statement's source.
        name_offset: u32,
        /// Whether the view already exists.
        exists: bool,
    },
    /// `CREATE TRIGGER`.
    CreateTrigger {
        /// Which attached database.
        database: usize,
        /// The trigger name as written.
        name: Vec<u8>,
        /// The byte the name starts at in the statement's source.
        name_offset: u32,
        /// The table or view the trigger is attached to.
        table: Vec<u8>,
        /// Whether the trigger already exists.
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

/// The fields of a `CREATE TRIGGER`, passed as one argument.
///
/// Ten parameters is past the point where their order is checkable by reading,
/// and every one of them is a field of the statement rather than something
/// computed here.
pub(crate) struct CreateTriggerParts<'p> {
    /// Whether `TEMP` was written.
    pub temporary: bool,
    /// Whether `IF NOT EXISTS` was written.
    pub if_not_exists: bool,
    /// The schema qualifier.
    pub database: Option<ast::NameId>,
    /// The trigger name.
    pub name: ast::NameId,
    /// When it fires.
    pub time: Option<ast::TriggerTime>,
    /// The table it is attached to.
    pub table: ast::NameId,
    /// Whether `FOR EACH ROW` was written.
    pub for_each_row: bool,
    /// The `WHEN` guard.
    pub when: Option<ast::ExprId>,
    /// The body statements.
    pub body: &'p [ast::Statement],
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
            ast::Statement::Analyze { database, name } => self.bind_analyze(*database, *name),
            ast::Statement::AlterTable {
                database,
                table,
                action,
            } => self.bind_alter(*database, *table, action),
            ast::Statement::Reindex { database, name } => self.bind_reindex(*database, *name),
            ast::Statement::Vacuum { database, into } => self.bind_vacuum(*database, *into),
            ast::Statement::CreateView {
                temporary,
                if_not_exists,
                database,
                name,
                columns,
                select,
            } => self.bind_create_view(
                *temporary,
                *if_not_exists,
                *database,
                *name,
                columns,
                *select,
            ),
            ast::Statement::CreateTrigger {
                temporary,
                if_not_exists,
                database,
                name,
                time,
                event: _,
                table,
                for_each_row,
                when,
                body,
            } => self.bind_create_trigger(CreateTriggerParts {
                temporary: *temporary,
                if_not_exists: *if_not_exists,
                database: *database,
                name: *name,
                time: *time,
                table: *table,
                for_each_row: *for_each_row,
                when: *when,
                body,
            }),
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
            constraints,
            without_rowid,
            strict,
        } = body
        else {
            return Err(unsupported("CREATE TABLE ... AS SELECT", Span::default()));
        };
        if *without_rowid && !self.declares_primary_key(columns, constraints) {
            return Err(refused("PRIMARY KEY missing on table", Span::default()));
        }
        self.check_autoincrement(columns, *without_rowid)?;
        if *strict {
            self.check_strict(columns)?;
        }
        self.check_generated(columns)?;
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

    /// Returns whether a `CREATE TABLE` declares a primary key anywhere.
    fn declares_primary_key(
        &self,
        columns: &[ast::ColumnDef],
        constraints: &[(Option<ast::NameId>, ast::TableConstraint)],
    ) -> bool {
        let on_column = columns.iter().any(|column| {
            column.constraints.iter().any(|(_, constraint)| {
                matches!(constraint, ast::ColumnConstraint::PrimaryKey { .. })
            })
        });
        on_column
            || constraints.iter().any(|(_, constraint)| {
                matches!(constraint, ast::TableConstraint::PrimaryKey { .. })
            })
    }

    /// Checks the rules a generated column has to obey.
    ///
    /// A generated column may not carry a `DEFAULT` - it has no value of its
    /// own to fall back to - may not be part of a rowid table's `PRIMARY KEY`,
    /// and may not refer to a column that does not exist or to itself. The
    /// cycle check is the one that matters: without it a `CREATE TABLE` that
    /// describes one is accepted and every later insert recurses.
    fn check_generated(&self, columns: &[ast::ColumnDef]) -> Result<(), ParseError> {
        let names: Vec<Vec<u8>> = columns
            .iter()
            .map(|column| self.ast.folded(column.name).to_vec())
            .collect();
        let mut generated: Vec<(usize, Vec<usize>)> = Vec::new();
        for (position, column) in columns.iter().enumerate() {
            let mut expr = None;
            let mut has_default = false;
            let mut in_primary_key = false;
            for (_, constraint) in &column.constraints {
                match constraint {
                    ast::ColumnConstraint::Generated { expr: body, .. } => expr = Some(*body),
                    ast::ColumnConstraint::Default(_) => has_default = true,
                    ast::ColumnConstraint::PrimaryKey { .. } => in_primary_key = true,
                    _ => {}
                }
            }
            let Some(expr) = expr else {
                continue;
            };
            let written = String::from_utf8_lossy(self.ast.text(column.name)).into_owned();
            if has_default {
                return Err(refused(
                    format!("cannot use DEFAULT on a generated column: {written}"),
                    Span::default(),
                ));
            }
            if in_primary_key {
                return Err(refused(
                    format!("generated columns cannot be part of the PRIMARY KEY: {written}"),
                    Span::default(),
                ));
            }
            let mut reads = Vec::new();
            self.expression_names(expr, &mut reads);
            let mut resolved = Vec::new();
            for name in &reads {
                let Some(found) = names.iter().position(|candidate| candidate == name) else {
                    return Err(crate::bind::no_such_column(name, Span::default()));
                };
                resolved.push(found);
            }
            generated.push((position, resolved));
        }
        // A cycle is anything that never becomes computable: repeat the "every
        // dependency is settled" pass until it stops making progress, and if
        // anything is left it depends on itself, directly or through others.
        let mut settled: Vec<usize> = (0..columns.len())
            .filter(|position| !generated.iter().any(|(owner, _)| owner == position))
            .collect();
        let mut pending = generated;
        loop {
            let before = pending.len();
            let mut still = Vec::new();
            for (position, reads) in pending {
                if reads.iter().all(|read| settled.contains(read)) {
                    settled.push(position);
                } else {
                    still.push((position, reads));
                }
            }
            pending = still;
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        if let Some((position, _)) = pending.first() {
            let written = columns
                .get(*position)
                .map(|column| String::from_utf8_lossy(self.ast.text(column.name)).into_owned())
                .unwrap_or_default();
            return Err(refused(
                format!("generated column loop on {written}"),
                Span::default(),
            ));
        }
        Ok(())
    }

    /// Collects the folded column names an expression mentions.
    fn expression_names(&self, expr: ast::ExprId, into: &mut Vec<Vec<u8>>) {
        let Some(node) = self.ast.expr(expr) else {
            return;
        };
        if let ast::Expr::Column { column, .. } = node {
            let name = self.ast.folded(*column).to_vec();
            if !into.contains(&name) {
                into.push(name);
            }
        }
        for child in expression_children(self.ast, expr) {
            self.expression_names(child, into);
        }
    }

    /// Checks the rules a `STRICT` table adds to its column list.
    ///
    /// Every column must name one of six types, and the check is on the
    /// declared text rather than on the affinity it maps to: `VARCHAR(10)` has
    /// TEXT affinity and is still refused, because STRICT is about what was
    /// written and not about what it means.
    fn check_strict(&self, columns: &[ast::ColumnDef]) -> Result<(), ParseError> {
        for column in columns {
            let Some(declared) = column.declared_type.as_ref() else {
                return Err(refused(
                    format!(
                        "missing datatype for {}",
                        String::from_utf8_lossy(self.ast.text(column.name))
                    ),
                    Span::default(),
                ));
            };
            let folded = declared.to_ascii_uppercase();
            let allowed = matches!(
                folded.as_slice(),
                b"INT" | b"INTEGER" | b"REAL" | b"TEXT" | b"BLOB" | b"ANY"
            );
            if !allowed {
                return Err(refused(
                    format!(
                        "unknown datatype for {}: \"{}\"",
                        String::from_utf8_lossy(self.ast.text(column.name)),
                        String::from_utf8_lossy(declared)
                    ),
                    Span::default(),
                ));
            }
        }
        Ok(())
    }

    /// Binds an `ANALYZE`.
    ///
    /// A bare `ANALYZE` measures everything; one with a name measures that
    /// object. SQLite accepts a database name, an index name or a table name in
    /// the same position and works out which it is, and so does this: the name
    /// is resolved against the tables, then the indexes, and only then refused.
    fn bind_analyze(
        &mut self,
        database: Option<ast::NameId>,
        name: Option<ast::NameId>,
    ) -> Result<Directive, ParseError> {
        let index = self.resolve_database(database)?;
        self.record_write_dependency(index);
        let Some(name) = name else {
            return Ok(Directive::Analyze {
                database: index,
                table: None,
            });
        };
        let folded = self.ast.folded(name).to_vec();
        let database_name = self.catalog.database_name(index).to_vec();
        if self
            .catalog
            .database_index(&folded)
            .is_some_and(|found| found == index)
        {
            // The name was the database's, which means everything in it.
            return Ok(Directive::Analyze {
                database: index,
                table: None,
            });
        }
        if let Some(table) = self
            .catalog
            .find_table(Some(database_name.as_slice()), &folded)
        {
            return Ok(Directive::Analyze {
                database: index,
                table: Some(table.name.clone()),
            });
        }
        if let Some((table, _)) = self
            .catalog
            .find_index(Some(database_name.as_slice()), &folded)
        {
            return Ok(Directive::Analyze {
                database: index,
                table: Some(table.name.clone()),
            });
        }
        Err(no_such_table(self.ast.text(name), Span::default()))
    }

    /// Binds an `ALTER TABLE`.
    ///
    /// Every refusal SQLite makes is made here, where the catalog is available,
    /// rather than half-way through rewriting the schema: a rename that is
    /// going to fail must fail before anything has been written.
    fn bind_alter(
        &mut self,
        database: Option<ast::NameId>,
        table: ast::NameId,
        action: &ast::AlterAction,
    ) -> Result<Directive, ParseError> {
        let index = self.resolve_database(database)?;
        let database_name = self.catalog.database_name(index).to_vec();
        let folded = self.ast.folded(table).to_vec();
        let Some(target) = self
            .catalog
            .find_table(Some(database_name.as_slice()), &folded)
            .cloned()
        else {
            return Err(no_such_table(self.ast.text(table), Span::default()));
        };
        if target.kind != crate::catalog_view::TableKind::Table {
            return Err(refused(
                format!(
                    "cannot alter {}: not a table",
                    String::from_utf8_lossy(&target.name)
                ),
                Span::default(),
            ));
        }
        if target.folded.starts_with(b"sqlite_") {
            return Err(refused(
                format!(
                    "table {} may not be altered",
                    String::from_utf8_lossy(&target.name)
                ),
                Span::default(),
            ));
        }
        self.record_write_dependency(index);
        let kind = match action {
            ast::AlterAction::RenameTo(name) => {
                let to = self.ast.text(*name).to_vec();
                let to_folded = self.ast.folded(*name).to_vec();
                if self
                    .catalog
                    .find_table(Some(database_name.as_slice()), &to_folded)
                    .is_some()
                {
                    return Err(refused(
                        format!(
                            "there is already another table or index with this name: {}",
                            String::from_utf8_lossy(&to)
                        ),
                        Span::default(),
                    ));
                }
                AlterKind::RenameTable { to }
            }
            ast::AlterAction::RenameColumn { from, to } => {
                let from_folded = self.ast.folded(*from).to_vec();
                let Some(position) = target.column_position(&from_folded) else {
                    return Err(crate::bind::no_such_column(
                        self.ast.text(*from),
                        Span::default(),
                    ));
                };
                let to_folded = self.ast.folded(*to).to_vec();
                if target.column_position(&to_folded).is_some() {
                    return Err(refused(
                        format!(
                            "duplicate column name: {}",
                            String::from_utf8_lossy(self.ast.text(*to))
                        ),
                        Span::default(),
                    ));
                }
                let stored = target
                    .column(position)
                    .map(|column| column.name.clone())
                    .unwrap_or_default();
                AlterKind::RenameColumn {
                    from: stored,
                    to: self.ast.text(*to).to_vec(),
                }
            }
            ast::AlterAction::AddColumn(definition) => {
                self.check_added_column(&target, definition)?;
                AlterKind::AddColumn {
                    start: definition.span.start,
                    end: definition.span.end,
                }
            }
            ast::AlterAction::DropColumn(name) => {
                let folded = self.ast.folded(*name).to_vec();
                let Some(position) = target.column_position(&folded) else {
                    return Err(crate::bind::no_such_column(
                        self.ast.text(*name),
                        Span::default(),
                    ));
                };
                self.check_dropped_column(&target, position)?;
                let stored = target
                    .column(position)
                    .map(|column| column.name.clone())
                    .unwrap_or_default();
                AlterKind::DropColumn {
                    name: stored,
                    position,
                }
            }
        };
        Ok(Directive::Alter {
            database: index,
            table: target.name.clone(),
            action: kind,
        })
    }

    /// Checks what `ADD COLUMN` may not add.
    ///
    /// Every one of these is refused because the existing rows have no value
    /// for the new column and cannot be given one: a `PRIMARY KEY` or `UNIQUE`
    /// column would need an index built over values that are all the same
    /// default, and a `NOT NULL` column with no default would make every
    /// existing row violate its own table.
    fn check_added_column(
        &self,
        table: &crate::catalog_view::TableInfo,
        definition: &ast::ColumnDef,
    ) -> Result<(), ParseError> {
        let folded = self.ast.folded(definition.name).to_vec();
        if table.column_position(&folded).is_some() {
            return Err(refused(
                format!(
                    "duplicate column name: {}",
                    String::from_utf8_lossy(self.ast.text(definition.name))
                ),
                Span::default(),
            ));
        }
        let mut not_null = false;
        let mut has_default = false;
        for (_, constraint) in &definition.constraints {
            match constraint {
                ast::ColumnConstraint::PrimaryKey { .. } => {
                    return Err(refused("cannot add a PRIMARY KEY column", Span::default()))
                }
                ast::ColumnConstraint::Unique(_) => {
                    return Err(refused("cannot add a UNIQUE column", Span::default()))
                }
                ast::ColumnConstraint::NotNull(_) => not_null = true,
                ast::ColumnConstraint::Default(expr) => {
                    has_default = true;
                    if !self.constant_default(*expr) {
                        return Err(refused(
                            "cannot add a column with a non-constant default",
                            Span::default(),
                        ));
                    }
                }
                ast::ColumnConstraint::Generated { stored, .. } => {
                    if *stored {
                        return Err(refused("cannot add a STORED column", Span::default()));
                    }
                }
                _ => {}
            }
        }
        if not_null && !has_default {
            return Err(refused(
                "cannot add a NOT NULL column with default value NULL",
                Span::default(),
            ));
        }
        Ok(())
    }

    /// Returns whether a `DEFAULT` is a constant an existing row can be given.
    fn constant_default(&self, expr: ast::ExprId) -> bool {
        match self.ast.expr(expr) {
            Some(ast::Expr::Literal(_)) => true,
            Some(ast::Expr::Unary { operand, .. }) => self.constant_default(*operand),
            _ => false,
        }
    }

    /// Checks what `DROP COLUMN` may not drop.
    fn check_dropped_column(
        &self,
        table: &crate::catalog_view::TableInfo,
        position: u16,
    ) -> Result<(), ParseError> {
        let named = table
            .column(position)
            .map(|column| String::from_utf8_lossy(&column.name).into_owned())
            .unwrap_or_default();
        if table.columns.len() <= 1 {
            return Err(refused(
                format!("cannot drop column \"{named}\": no other columns exist"),
                Span::default(),
            ));
        }
        if table.rowid_alias == Some(position)
            || table
                .column(position)
                .is_some_and(|column| column.primary_key_position.is_some())
        {
            return Err(refused(
                format!("cannot drop column \"{named}\": PRIMARY KEY"),
                Span::default(),
            ));
        }
        let indexed = table
            .indexes
            .iter()
            .any(|index| index.columns.iter().any(|key| key.column == Some(position)));
        if indexed {
            return Err(refused(
                format!("cannot drop column \"{named}\": indexed"),
                Span::default(),
            ));
        }
        // A CHECK or a generated column that reads it would be left naming a
        // column that is gone, and the table would stop loading.
        let folded = table
            .column(position)
            .map(|column| column.folded.clone())
            .unwrap_or_default();
        let referenced = table
            .checks
            .iter()
            .any(|check| mentions_name(&check.expr_sql, &folded))
            || table.columns.iter().enumerate().any(|(other, column)| {
                other != usize::from(position)
                    && column
                        .generated_sql
                        .as_ref()
                        .is_some_and(|sql| mentions_name(sql, &folded))
            });
        if referenced {
            return Err(refused(
                format!(
                    "error in table {}: cannot drop column \"{named}\"",
                    String::from_utf8_lossy(&table.name)
                ),
                Span::default(),
            ));
        }
        Ok(())
    }

    /// Binds a `REINDEX`.
    ///
    /// The name is a collation, a table or an index, and SQLite works out which
    /// from what it finds - so the resolution order is the same here. A bare
    /// `REINDEX` rebuilds everything, which is the form that matters: it is what
    /// a person runs after a collation's definition has changed underneath an
    /// index that was built with the old one.
    fn bind_reindex(
        &mut self,
        database: Option<ast::NameId>,
        name: Option<ast::NameId>,
    ) -> Result<Directive, ParseError> {
        let index = self.resolve_database(database)?;
        self.record_write_dependency(index);
        let database_name = self.catalog.database_name(index).to_vec();
        let everything = |catalog: &dyn CatalogView| -> Vec<Vec<u8>> {
            catalog
                .tables_of(index)
                .into_iter()
                .flat_map(|table| table.indexes.iter().map(|entry| entry.name.clone()))
                .filter(|name| !name.is_empty())
                .collect()
        };
        let Some(name) = name else {
            return Ok(Directive::Reindex {
                database: index,
                indexes: everything(self.catalog),
            });
        };
        let folded = self.ast.folded(name).to_vec();
        if let Some(table) = self
            .catalog
            .find_table(Some(database_name.as_slice()), &folded)
        {
            return Ok(Directive::Reindex {
                database: index,
                indexes: table
                    .indexes
                    .iter()
                    .map(|entry| entry.name.clone())
                    .collect(),
            });
        }
        if let Some((_, entry)) = self
            .catalog
            .find_index(Some(database_name.as_slice()), &folded)
        {
            return Ok(Directive::Reindex {
                database: index,
                indexes: vec![entry.name.clone()],
            });
        }
        // A collation name rebuilds every index ordered by it. An unknown name
        // is an error, and SQLite reports it against the collation because that
        // is the last thing it tried.
        if Collation::from_name(core::str::from_utf8(&folded).unwrap_or("")).is_some() {
            let wanted = folded.clone();
            let indexes = self
                .catalog
                .tables_of(index)
                .into_iter()
                .flat_map(|table| table.indexes.iter())
                .filter(|entry| {
                    entry
                        .columns
                        .iter()
                        .any(|key| key.collation.eq_ignore_ascii_case(&wanted))
                })
                .map(|entry| entry.name.clone())
                .collect();
            return Ok(Directive::Reindex {
                database: index,
                indexes,
            });
        }
        Err(no_such_collation_sequence(
            self.ast.text(name),
            Span::default(),
        ))
    }

    /// Binds a `VACUUM`.
    fn bind_vacuum(
        &mut self,
        database: Option<ast::NameId>,
        into: Option<ast::ExprId>,
    ) -> Result<Directive, ParseError> {
        let target = match into {
            Some(expr) => Some(self.literal_path(expr)?),
            None => None,
        };
        let index = self.resolve_database(database)?;
        self.record_write_dependency(index);
        Ok(Directive::Vacuum {
            database: index,
            into: target,
        })
    }

    /// Reads the file name a `VACUUM INTO` was given.
    ///
    /// A literal only. SQLite evaluates the expression, but every other value
    /// it could produce is a file name computed at run time, and a statement
    /// that decides where to write a copy of the database from arithmetic is
    /// not a shape worth supporting before it is asked for.
    fn literal_path(&mut self, expr: ast::ExprId) -> Result<Vec<u8>, ParseError> {
        match self.ast.expr(expr) {
            Some(ast::Expr::Literal(ast::Literal::String(text))) => Ok(text.clone()),
            _ => Err(unsupported(
                "VACUUM INTO with a name that is not a literal",
                Span::default(),
            )),
        }
    }

    /// Binds a `CREATE VIEW`.
    ///
    /// The body is bound here, and thrown away, purely to refuse a view whose
    /// query does not resolve. SQLite does the same: the definition is checked
    /// when the view is created rather than when it is first read, so a typo
    /// fails at `CREATE VIEW` rather than in whatever statement happens to
    /// select from it next.
    fn bind_create_view(
        &mut self,
        temporary: bool,
        if_not_exists: bool,
        database: Option<ast::NameId>,
        name: ast::NameId,
        columns: &[ast::NameId],
        select: ast::SelectId,
    ) -> Result<Directive, ParseError> {
        if temporary {
            return Err(unsupported("TEMP views", Span::default()));
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
        if !exists {
            let saved = core::mem::take(&mut self.scopes);
            let bound = self.bind_select(select);
            self.scopes = saved;
            let bound = bound?;
            if !columns.is_empty() && columns.len() != bound.columns.len() {
                return Err(refused(
                    format!(
                        "expected {} columns for {} but got {}",
                        columns.len(),
                        String::from_utf8_lossy(&written),
                        bound.columns.len()
                    ),
                    Span::default(),
                ));
            }
        }
        self.record_write_dependency(index);
        Ok(Directive::CreateView {
            if_not_exists,
            database: index,
            name: written,
            name_offset: self.name_offset(name),
            exists,
        })
    }

    /// Refuses the two places `AUTOINCREMENT` may not be written.
    ///
    /// It counts the rowid the table has handed out, so it needs a rowid to
    /// count: only an `INTEGER PRIMARY KEY` column, and never on a table that
    /// has no rowid at all. Both messages are the reference's own, because an
    /// application that reads them is reading SQLite's.
    fn check_autoincrement(
        &mut self,
        columns: &[ast::ColumnDef],
        without_rowid: bool,
    ) -> Result<(), ParseError> {
        for column in columns {
            let declared = column.declared_type.clone().unwrap_or_default();
            for (_, constraint) in &column.constraints {
                let ast::ColumnConstraint::PrimaryKey {
                    autoincrement: true,
                    ..
                } = constraint
                else {
                    continue;
                };
                if without_rowid {
                    return Err(refused(
                        "AUTOINCREMENT not allowed on WITHOUT ROWID tables",
                        Span::default(),
                    ));
                }
                if !declared.eq_ignore_ascii_case(b"integer") {
                    return Err(refused(
                        "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY",
                        Span::default(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Binds a `CREATE TRIGGER`.
    ///
    /// The body is bound here, against the table the trigger is attached to, so
    /// a trigger that reads a column that does not exist is refused when it is
    /// written rather than the first time somebody writes the table. SQLite
    /// makes the same promise, and the alternative is a schema that loads and
    /// then fails on an unrelated INSERT.
    fn bind_create_trigger(
        &mut self,
        parts: CreateTriggerParts<'_>,
    ) -> Result<Directive, ParseError> {
        if parts.temporary {
            return Err(unsupported("TEMP triggers", Span::default()));
        }
        // `for_each_row` records whether the words were written, not whether
        // the trigger is one: SQLite has only row triggers, an omitted clause
        // means FOR EACH ROW, and FOR EACH STATEMENT is a syntax error in the
        // parser. There is nothing to refuse here.
        let _ = parts.for_each_row;
        let index = self.resolve_database(parts.database)?;
        let written = self.ast.text(parts.name).to_vec();
        if written.to_ascii_lowercase().starts_with(b"sqlite_") {
            return Err(refused(
                format!(
                    "object name reserved for internal use: {}",
                    String::from_utf8_lossy(&written)
                ),
                Span::default(),
            ));
        }
        let folded = self.ast.folded(parts.name).to_vec();
        let database_name = self.catalog.database_name(index).to_vec();
        let table_folded = self.ast.folded(parts.table).to_vec();
        let Some(target) = self
            .catalog
            .find_table(Some(database_name.as_slice()), &table_folded)
            .cloned()
        else {
            return Err(crate::bind::no_such_table(
                self.ast.text(parts.table),
                Span::default(),
            ));
        };
        let exists = self
            .catalog
            .find_trigger(Some(database_name.as_slice()), &folded)
            .is_some();
        if exists && !parts.if_not_exists {
            return Err(refused(
                format!(
                    "trigger {} already exists",
                    String::from_utf8_lossy(&written)
                ),
                Span::default(),
            ));
        }
        let instead_of = parts.time == Some(ast::TriggerTime::InsteadOf);
        match target.kind {
            TableKind::View if !instead_of => {
                return Err(refused(
                    format!(
                        "cannot create {} trigger on view: {}",
                        if parts.time == Some(ast::TriggerTime::After) {
                            "AFTER"
                        } else {
                            "BEFORE"
                        },
                        String::from_utf8_lossy(&target.name)
                    ),
                    Span::default(),
                ));
            }
            TableKind::Table if instead_of => {
                return Err(refused(
                    format!(
                        "cannot create INSTEAD OF trigger on table: {}",
                        String::from_utf8_lossy(&target.name)
                    ),
                    Span::default(),
                ));
            }
            TableKind::Virtual | TableKind::Subquery => {
                return Err(unsupported("a trigger on that object", Span::default()));
            }
            _ => {}
        }
        // `UPDATE OF a, b` is deliberately *not* checked against the table's
        // columns. The pinned build accepts `UPDATE OF nosuchcolumn` and simply
        // never fires the trigger, and refusing it here would make rust-db's
        // language smaller than the reference's - a schema SQLite wrote that
        // rust-db could not load.
        // The body is deliberately *not* bound here. SQLite stores a trigger
        // whose body names a column that does not exist and reports it on the
        // first write that fires it - measured against the pinned build, which
        // accepts both `UPDATE OF nosuchcolumn` and a body reading a column the
        // table has not got. Refusing either here would leave rust-db unable to
        // load a schema SQLite had written.
        let _ = (parts.time, parts.when, parts.body);
        self.record_write_dependency(index);
        Ok(Directive::CreateTrigger {
            database: index,
            name: written,
            name_offset: self.name_offset(parts.name),
            table: target.name.clone(),
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
        let index = self.resolve_database(database)?;
        let database_name = self.catalog.database_name(index).to_vec();
        let written = self.ast.text(name).to_vec();
        let folded = self.ast.folded(name).to_vec();
        self.record_write_dependency(index);
        if kind == ObjectKind::Trigger {
            // A trigger owns no B-tree either, so dropping one is its schema row
            // and nothing else.
            let exists = self
                .catalog
                .find_trigger(Some(database_name.as_slice()), &folded)
                .is_some();
            if !exists && !if_exists {
                return Err(refused(
                    format!("no such trigger: {}", String::from_utf8_lossy(&written)),
                    Span::default(),
                ));
            }
            return Ok(Directive::Drop {
                kind,
                if_exists,
                database: index,
                name: written,
                root: 0,
                index_roots: Vec::new(),
                exists,
            });
        }
        if kind == ObjectKind::View {
            // A view owns no B-tree, so dropping one is the schema row and
            // nothing else - and it must refuse a table, because `DROP VIEW t`
            // on a table is an error rather than a drop.
            let found = self
                .catalog
                .find_table(Some(database_name.as_slice()), &folded)
                .cloned();
            let exists = found
                .as_ref()
                .is_some_and(|table| table.kind == crate::catalog_view::TableKind::View);
            if !exists && !if_exists {
                return Err(refused(
                    format!("no such view: {}", String::from_utf8_lossy(&written)),
                    Span::default(),
                ));
            }
            return Ok(Directive::Drop {
                kind,
                if_exists,
                database: index,
                name: written,
                root: 0,
                index_roots: Vec::new(),
                exists,
            });
        }
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
            if table.kind == crate::catalog_view::TableKind::View {
                return Err(refused(
                    format!(
                        "use DROP VIEW to delete view {}",
                        String::from_utf8_lossy(&written)
                    ),
                    Span::default(),
                ));
            }
            // A WITHOUT ROWID table's primary key *is* the table's own b-tree,
            // so its entry names the same root. Freeing it twice frees a page
            // that is already on the free list, which reads back as a malformed
            // database.
            let index_roots = table
                .indexes
                .iter()
                .map(|index| index.root)
                .filter(|root| *root != 0 && *root != table.root)
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
