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
use crate::bind::{no_such_table, refused, schema_refused, unsupported, Binder, BoundExpr};
use crate::catalog_view::CatalogView;
use crate::catalog_view::TableKind;
use crate::diagnostic::ParseError;
use crate::lexer::Span;
use inillucent_value::Collation;

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
        ast::Expr::Function {
            arguments: Some(arguments),
            ..
        } => out.extend(arguments.iter().copied()),
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
            crate::lexer::TokenKind::Identifier { keyword: None, .. }
                if token.span.slice(sql).to_ascii_lowercase() == folded =>
            {
                return true;
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

/// What an added column would do to rows that already exist.
///
/// SQLite refuses `PRIMARY KEY` and `UNIQUE` while it is still compiling,
/// because no table can take them however empty it is. The other three it
/// defers: a `NOT NULL` column with no default, a non-constant default and a
/// `STORED` generated column are refused *only when there is a row to break*,
/// and are accepted on an empty table. That is not a quirk worth smoothing
/// over - it is the difference between a migration that runs on a fresh
/// database and one that runs on a populated one - so the binder records what
/// it saw and the executor, which knows the row count, decides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AddedColumnRisk {
    /// `NOT NULL` with nothing to fill the existing rows with.
    pub null_without_default: bool,
    /// A `DEFAULT` the existing rows cannot all be given one answer from.
    pub non_constant_default: bool,
    /// `GENERATED ALWAYS AS (...) STORED`, which needs a value in every record.
    pub generated_stored: bool,
}

impl AddedColumnRisk {
    /// Returns the refusal a table with rows in it owes, in SQLite's wording.
    ///
    /// The capitalisation is the reference's own and is inconsistent between
    /// the three; it is reproduced rather than tidied, because a caller
    /// matching on the message is matching on what SQLite prints.
    pub fn refusal(&self) -> Option<&'static str> {
        if self.null_without_default {
            return Some("Cannot add a NOT NULL column with default value NULL");
        }
        if self.non_constant_default {
            return Some("Cannot add a column with non-constant default");
        }
        if self.generated_stored {
            return Some("cannot add a STORED column");
        }
        None
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
        /// What it would do to rows that already exist.
        risk: AddedColumnRisk,
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
    ///
    /// `None` for a key that is an expression. It was a bare `u16` while
    /// `CREATE INDEX ix ON t(lower(a))` was refused in the binder; the field is
    /// an `Option` now so that a reader which needs a column - a module-backed
    /// index, say - has to say what it does when there is not one, rather than
    /// reading a position that was invented to fill the slot.
    pub column: Option<u16>,
    /// The key expression, as written, when the key is one.
    pub expr_sql: Option<Vec<u8>>,
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
    /// `CREATE TABLE ... AS SELECT`.
    ///
    /// A `CREATE` whose column list comes from a plan, which is why it is a
    /// directive of its own rather than a flag on the one above: everything
    /// about the table - its column names, and the declared types it inherits
    /// from the query's origin columns - is decided by binding the query, and
    /// the `CREATE` text that is stored is *synthesised* rather than being a
    /// slice of what was typed.
    CreateTableAsSelect {
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// Which attached database.
        database: usize,
        /// The table name as written.
        name: Vec<u8>,
        /// Whether the table already exists.
        exists: bool,
        /// The `CREATE TABLE name(...)` text to store, built from the query.
        create_sql: Vec<u8>,
        /// The `SELECT` that fills it, as the source text it was written as.
        ///
        /// The text rather than the bound query, because the rows are inserted
        /// by an ordinary `INSERT INTO name <select>` compiled against the
        /// schema *after* the table exists - which is one implementation of
        /// what an insert means rather than a second one written here.
        select_sql: Vec<u8>,
    },
    /// `CREATE VIRTUAL TABLE`.
    CreateVirtualTable {
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// Which attached database.
        database: usize,
        /// The table name as written.
        name: Vec<u8>,
        /// The module name as written.
        module: Vec<u8>,
        /// The arguments inside the parentheses, as written.
        arguments: Vec<Vec<u8>>,
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
    /// `ATTACH`, which adds a database file to this connection.
    Attach {
        /// The file to open, as the literal it was written as.
        file: Vec<u8>,
        /// The name it will be known by.
        schema: Vec<u8>,
    },
    /// `DETACH`, which removes one.
    Detach {
        /// The name it was attached under.
        schema: Vec<u8>,
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
        /// The module named by `USING`, folded, when one was.
        using: Option<Vec<u8>>,
        /// The key columns.
        columns: Vec<IndexKeyColumn>,
        /// The storage parameters `WITH ( ... )` named, checked against the
        /// module that will read them.
        settings: Vec<(Vec<u8>, Vec<u8>)>,
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
        /// The schema the pragma was qualified with, when one was written.
        ///
        /// `PRAGMA aux.table_info(t)` asks about the attached database rather
        /// than about `main`, and a pragma that dropped the qualifier would
        /// answer confidently about the wrong file.
        database: Option<usize>,
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

/// Whether a `CREATE INDEX` declared `UNIQUE`.
///
/// **An enum rather than a `bool` beside another `bool` (task-1962, A9).**
/// `bind_create_index` took `unique` and `if_not_exists` adjacent and
/// positional; swapping them compiles and declares a unique index where the
/// statement asked for `IF NOT EXISTS`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Uniqueness {
    /// `CREATE UNIQUE INDEX`: two rows may not share a key.
    Unique,
    /// `CREATE INDEX`: a key may repeat.
    Duplicates,
}

/// Whether a `CREATE` declared `IF NOT EXISTS`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IfNotExists {
    /// The statement is a no-op when the object is already there.
    Skip,
    /// The statement fails when the object is already there.
    Refuse,
}

/// Everything a `CREATE INDEX` statement names.
///
/// The grammar's own fields, gathered rather than passed as nine positional
/// arguments of which two were adjacent booleans.
pub struct CreateIndexSpec<'a> {
    /// Whether the index refuses a repeated key.
    pub unique: Uniqueness,
    /// What to do when the index is already there.
    pub if_not_exists: IfNotExists,
    /// The schema the index is created in, when one was written.
    pub database: Option<ast::NameId>,
    /// The index's name.
    pub name: ast::NameId,
    /// The table it is over.
    pub table: ast::NameId,
    /// The module named by `USING`, for the extension index forms.
    pub using: Option<ast::NameId>,
    /// The indexed columns, in key order.
    pub columns: &'a [ast::IndexedColumn],
    /// The `WITH` settings, as written.
    pub settings: &'a [Vec<u8>],
    /// The `WHERE` of a partial index, as an expression of the statement.
    pub filter: Option<ast::ExprId>,
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
            ast::Statement::CreateVirtualTable {
                if_not_exists,
                database,
                name,
                module,
                arguments,
            } => {
                self.bind_create_virtual_table(*if_not_exists, *database, *name, *module, arguments)
            }
            ast::Statement::CreateIndex {
                unique,
                if_not_exists,
                database,
                name,
                table,
                using,
                columns,
                settings,
                filter,
            } => self.bind_create_index(&CreateIndexSpec {
                unique: if *unique {
                    Uniqueness::Unique
                } else {
                    Uniqueness::Duplicates
                },
                if_not_exists: if *if_not_exists {
                    IfNotExists::Skip
                } else {
                    IfNotExists::Refuse
                },
                database: *database,
                name: *name,
                table: *table,
                using: *using,
                columns,
                settings,
                filter: *filter,
            }),
            ast::Statement::Analyze { database, name } => self.bind_analyze(*database, *name),
            ast::Statement::AlterTable {
                database,
                table,
                action,
            } => self.bind_alter(*database, *table, action),
            ast::Statement::Reindex { database, name } => self.bind_reindex(*database, *name),
            ast::Statement::Vacuum { database, into } => self.bind_vacuum(*database, *into),
            ast::Statement::Attach { file, schema, key } => self.bind_attach(*file, *schema, *key),
            ast::Statement::Detach { schema } => self.bind_detach(*schema),
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
    fn bind_create_virtual_table(
        &mut self,
        if_not_exists: bool,
        database: Option<ast::NameId>,
        name: ast::NameId,
        module: ast::NameId,
        arguments: &[Vec<u8>],
    ) -> Result<Directive, ParseError> {
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
        Ok(Directive::CreateVirtualTable {
            if_not_exists,
            database: index,
            name: written,
            module: self.ast.text(module).to_vec(),
            arguments: arguments.to_vec(),
            name_offset: self
                .ast
                .name(name)
                .map(|entry| entry.span.start)
                .unwrap_or_default(),
            exists,
        })
    }

    /// Binds `CREATE TABLE`, refusing what the file format cannot hold.
    fn bind_create_table(
        &mut self,
        temporary: bool,
        if_not_exists: bool,
        database: Option<ast::NameId>,
        name: ast::NameId,
        body: &ast::CreateTableBody,
    ) -> Result<Directive, ParseError> {
        let temp = self.temporary_database(temporary, database)?;
        // **Two bodies, and the second one is built.** This used to be written
        // as two `let ... else` bindings, the inner one answering
        // `unsupported("CREATE TABLE ... AS SELECT")` - an arm no statement
        // could reach, because `CreateTableBody` has exactly these two
        // variants, so a feature that works was described by a refusal
        // (task-1979, section 8.3). A match over both says the same thing with
        // nothing left over.
        let (columns, constraints, without_rowid, strict) = match body {
            ast::CreateTableBody::AsSelect(select) => {
                return self.bind_create_table_as_select(
                    temp,
                    if_not_exists,
                    database,
                    name,
                    *select,
                )
            }
            ast::CreateTableBody::Columns {
                columns,
                constraints,
                without_rowid,
                strict,
            } => (columns, constraints, without_rowid, strict),
        };
        if *without_rowid && !self.declares_primary_key(columns, constraints) {
            return Err(schema_refused(
                format!(
                    "PRIMARY KEY missing on table {}",
                    String::from_utf8_lossy(self.ast.text(name))
                ),
                Span::default(),
            ));
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
        let index = match temp {
            Some(index) => index,
            None => self.resolve_database(database)?,
        };
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

    /// Binds `CREATE TABLE ... AS SELECT`.
    ///
    /// **The column list comes from a plan**, which is the whole of why this is
    /// a shape of its own. SQLite takes the table's columns from the query's
    /// result columns: the name each one reports, and the declared type it
    /// carries when it is a plain reference to a column that has one. So
    /// `CREATE TABLE u AS SELECT a*2 AS d, b, c FROM t` on `t(a INTEGER, b TEXT,
    /// c REAL)` stores `CREATE TABLE u(d,b TEXT,c REAL)` - `d` is an expression
    /// and inherits nothing, and the other two inherit their origin's type.
    ///
    /// The rows are inserted afterwards by an ordinary `INSERT INTO name
    /// <select>`, compiled against the schema once the table is in it. That is
    /// one implementation of what an insert means rather than a second one
    /// written into the DDL path, and it is what makes the affinity Part B4
    /// applies reach these rows too.
    ///
    /// @param temp - the temporary database's index, when `TEMP` was written
    /// @param if_not_exists - whether `IF NOT EXISTS` was written
    /// @param database - the schema qualifier, when one was written
    /// @param name - the table's name
    /// @param select - the query the table is built from
    fn bind_create_table_as_select(
        &mut self,
        temp: Option<usize>,
        if_not_exists: bool,
        database: Option<ast::NameId>,
        name: ast::NameId,
        select: ast::SelectId,
    ) -> Result<Directive, ParseError> {
        let index = match temp {
            Some(index) => index,
            None => self.resolve_database(database)?,
        };
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
        let span = self
            .ast
            .select(select)
            .map(|held| held.span)
            .ok_or_else(|| refused("the query could not be read", Span::default()))?;
        let select_sql = self
            .source
            .get(span.start as usize..span.end as usize)
            .ok_or_else(|| refused("the query could not be read", span))?
            .to_vec();
        // Bound rather than merely parsed, because binding is what resolves the
        // result columns' names and origins - and because a query that does not
        // bind has to be refused here rather than after the table exists.
        let bound = self.bind_select(select)?;
        if bound.columns.is_empty() {
            return Err(refused(
                "a table must have at least one column",
                Span::default(),
            ));
        }
        // **The declaration a `CREATE TABLE ... AS SELECT` stores is the
        // *affinity*, not the source column's declared type.** SQLite writes
        // `a INT` for a source column declared `INTEGER` and `b TEXT` for one
        // declared `VARCHAR(3)`, because what survives a query is the affinity
        // and nothing else - the width, the precision and the spelling are
        // properties of the source table that the copy does not have. Storing
        // `VARCHAR(3)` here claimed a constraint the new table does not
        // enforce, and made the two schemas differ for every CTAS.
        //
        // The line break is SQLite's own rule too, so the stored text matches
        // byte for byte: the name lengths are added up first, and a wide
        // declaration is written one column per line.
        let mut width = identifier_width(&written);
        for column in &bound.columns {
            width = width
                .saturating_add(identifier_width(&column.name))
                .saturating_add(5);
        }
        let (open, between, close): (&[u8], &[u8], &[u8]) = if width < 50 {
            (b"", b",", b")")
        } else {
            (b"\n  ", b",\n  ", b"\n)")
        };
        let mut create_sql = Vec::new();
        create_sql.extend_from_slice(b"CREATE TABLE ");
        create_sql.extend_from_slice(&written);
        create_sql.push(b'(');
        let mut seen: Vec<Vec<u8>> = Vec::with_capacity(bound.columns.len());
        for (position, column) in bound.columns.iter().enumerate() {
            create_sql.extend_from_slice(if position > 0 { between } else { open });
            let folded = column.name.to_ascii_lowercase();
            if seen.contains(&folded) {
                return Err(refused(
                    format!(
                        "duplicate column name: {}",
                        String::from_utf8_lossy(&column.name)
                    ),
                    Span::default(),
                ));
            }
            seen.push(folded);
            create_sql.extend_from_slice(&quoted_name(&column.name));
            create_sql.extend_from_slice(affinity_type(&column.declared_type));
        }
        create_sql.extend_from_slice(close);
        self.record_write_dependency(index);
        Ok(Directive::CreateTableAsSelect {
            if_not_exists,
            database: index,
            name: written,
            exists,
            create_sql,
            select_sql,
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
        // **An unqualified `ALTER TABLE` searches `temp` before `main`
        // (task-2061).** This resolved every unqualified name through
        // `resolve_database(None)`, which answers `main` and nothing else, and
        // then looked the table up in `main` alone - so
        // `CREATE TEMP TABLE t (a, b); ALTER TABLE t ADD COLUMN c` was
        // `no such table: t` when nothing called `t` was in `main`, and altered
        // `main.t` when something was. SQLite searches `temp` first for an
        // unqualified name in `ALTER TABLE` exactly as it does in a `SELECT`,
        // and `find_table(None, ...)` is already that search - the same one
        // every query goes through - so the schema comes back from the table
        // that was found rather than being decided before the search.
        let written = match database {
            // A qualifier still has to name a database that exists, and it
            // still restricts the search to that one.
            Some(_) => Some(
                self.catalog
                    .database_name(self.resolve_database(database)?)
                    .to_vec(),
            ),
            None => None,
        };
        let folded = self.ast.folded(table).to_vec();
        let Some(target) = self
            .catalog
            .find_table(written.as_deref(), &folded)
            .cloned()
        else {
            return Err(no_such_table(self.ast.text(table), Span::default()));
        };
        let index = target.database;
        let database_name = self.catalog.database_name(index).to_vec();
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
                let risk = self.check_added_column(&target, definition)?;
                AlterKind::AddColumn {
                    start: definition.span.start,
                    end: definition.span.end,
                    risk,
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
    ) -> Result<AddedColumnRisk, ParseError> {
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
        let mut constant = true;
        let mut generated_stored = false;
        for (_, constraint) in &definition.constraints {
            match constraint {
                ast::ColumnConstraint::PrimaryKey { .. } => {
                    return Err(schema_refused(
                        "Cannot add a PRIMARY KEY column",
                        Span::default(),
                    ))
                }
                ast::ColumnConstraint::Unique(_) => {
                    return Err(schema_refused(
                        "Cannot add a UNIQUE column",
                        Span::default(),
                    ))
                }
                ast::ColumnConstraint::NotNull(_) => not_null = true,
                ast::ColumnConstraint::Default(expr) => {
                    has_default = true;
                    if !self.constant_default(*expr) {
                        constant = false;
                    }
                }
                ast::ColumnConstraint::Generated { stored, .. } if *stored => {
                    generated_stored = true;
                }
                _ => {}
            }
        }
        Ok(AddedColumnRisk {
            null_without_default: not_null && !has_default,
            non_constant_default: !constant,
            generated_stored,
        })
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

    /// Binds an `ATTACH`.
    ///
    /// Both operands are literals. SQLite evaluates them, and every other
    /// value they could produce is a file name computed at run time - a
    /// statement that decides which database to open from arithmetic is not a
    /// shape worth supporting before it is asked for, and it is one an
    /// authorizer could not check.
    pub(crate) fn bind_attach(
        &mut self,
        file: ast::ExprId,
        schema: ast::ExprId,
        key: Option<ast::ExprId>,
    ) -> Result<Directive, ParseError> {
        if key.is_some() {
            return Err(unsupported("ATTACH ... KEY", Span::default()));
        }
        Ok(Directive::Attach {
            file: self.literal_path(file)?,
            schema: self.literal_or_name(schema)?,
        })
    }

    /// Binds a `DETACH`.
    pub(crate) fn bind_detach(&mut self, schema: ast::ExprId) -> Result<Directive, ParseError> {
        Ok(Directive::Detach {
            schema: self.literal_or_name(schema)?,
        })
    }

    /// Reads a name written either as a word or as a string.
    ///
    /// `ATTACH 'file.db' AS aux` and `ATTACH 'file.db' AS 'aux'` name the same
    /// schema. The grammar parses that position as an expression, so a bare
    /// word arrives as a reference to a column that does not exist - and what
    /// the statement meant is the word.
    fn literal_or_name(&mut self, expr: ast::ExprId) -> Result<Vec<u8>, ParseError> {
        match self.ast.expr(expr) {
            Some(ast::Expr::Literal(ast::Literal::String(text))) => Ok(text.clone()),
            Some(ast::Expr::Column {
                table: None,
                column,
                ..
            }) => Ok(self.ast.text(*column).to_vec()),
            _ => Err(unsupported(
                "a schema name that is not a word or a string",
                Span::default(),
            )),
        }
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
        let temp = self.temporary_database(temporary, database)?;
        let index = match temp {
            Some(index) => index,
            None => self.resolve_database(database)?,
        };
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
        let temp = self.temporary_database(parts.temporary, parts.database)?;
        // `for_each_row` records whether the words were written, not whether
        // the trigger is one: SQLite has only row triggers, an omitted clause
        // means FOR EACH ROW, and FOR EACH STATEMENT is a syntax error in the
        // parser. There is nothing to refuse here.
        let _ = parts.for_each_row;
        let index = match temp {
            Some(index) => index,
            None => self.resolve_database(parts.database)?,
        };
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
        // A trigger created in a named database fires for a table in that
        // database. A temporary one fires for whatever the name finds, which
        // is the whole point of `CREATE TEMP TRIGGER ... ON t`: the trigger is
        // the connection's and the table is everybody's.
        let scope = temp.map_or(Some(database_name.as_slice()), |_| None);
        let Some(target) = self.catalog.find_table(scope, &table_folded).cloned() else {
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
        // never fires the trigger, and refusing it here would make inillucent's
        // language smaller than the reference's - a schema SQLite wrote that
        // inillucent could not load.
        // The body is deliberately *not* bound here. SQLite stores a trigger
        // whose body names a column that does not exist and reports it on the
        // first write that fires it - measured against the pinned build, which
        // accepts both `UPDATE OF nosuchcolumn` and a body reading a column the
        // table has not got. Refusing either here would leave inillucent unable to
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
    /// @param spec - what the statement named
    fn bind_create_index(&mut self, spec: &CreateIndexSpec<'_>) -> Result<Directive, ParseError> {
        let CreateIndexSpec {
            database,
            name,
            table,
            using,
            columns,
            settings,
            ..
        } = *spec;
        let unique = spec.unique == Uniqueness::Unique;
        let if_not_exists = spec.if_not_exists == IfNotExists::Skip;
        // **A `WHERE` is carried in the statement text, not in this
        // directive.** The engine re-parses the canonical SQL it stores -
        // `index_from_create_sql` already puts the predicate on
        // `IndexInfo::partial_sql` - so a field here would be a second copy to
        // keep in step. A predicate that names a column the table has not got
        // is refused when the index is built, by the query that fills it.
        // Only one module can back an index, and naming another is refused here
        // rather than accepted and ignored - an index that silently was not the
        // structure it asked for is the shape of wrong answer this ticket keeps
        // finding.
        let using = match using {
            None => None,
            Some(named) => {
                let folded = self.ast.folded(named).to_vec();
                // Two structures, and both are real: `inillucent_hnsw` is the
                // graph the retrieval engine builds, and `ivfflat` is the
                // inverted file pgvector's other index type is - k-means
                // centroids and a list per centroid, probed `probes` deep.
                // Anything else is refused rather than accepted and ignored:
                // an index that silently was not the structure it asked for is
                // the shape of wrong answer this ticket keeps finding.
                if folded != b"inillucent_hnsw" && folded != b"ivfflat" {
                    return Err(unsupported(
                        "an index USING a module other than inillucent_hnsw or ivfflat",
                        Span::default(),
                    ));
                }
                Some(folded)
            }
        };
        let parsed_settings = index_settings(&using, settings)?;
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
            // `CREATE INDEX x ON t(b COLLATE NOCASE DESC)` parses the collation
            // into the *expression*, because that is where the grammar puts a
            // `COLLATE` that follows a value. It is still an index on a bare
            // column, and treating it as one is the difference between
            // supporting the everyday form and refusing it as an expression.
            let (expr, written_collation) = match self.ast.expr(column.expr) {
                Some(ast::Expr::Collate { operand, collation }) => {
                    (self.ast.expr(*operand), Some(*collation))
                }
                other => (other, column.collation),
            };
            // A key that is not a bare column is an expression, and is carried
            // as the source text the engine re-parses. Its collation is BINARY
            // unless the statement named one: there is no column to inherit
            // from.
            let named = match expr {
                Some(ast::Expr::Column {
                    table: None,
                    column: name,
                    ..
                }) => Some(*name),
                _ => None,
            };
            let Some(name) = named else {
                let collation = match written_collation {
                    Some(collation) => self.ast.folded(collation).to_vec(),
                    None => b"binary".to_vec(),
                };
                keys.push(IndexKeyColumn {
                    column: None,
                    expr_sql: Some(self.ast.expr_span(column.expr).slice(self.source).to_vec()),
                    collation,
                    descending: column.order == ast::SortOrder::Descending,
                });
                continue;
            };
            let folded = self.ast.folded(name).to_vec();
            let Some(position) = target.column_position(&folded) else {
                return Err(crate::bind::no_such_column(
                    self.ast.text(name),
                    Span::default(),
                ));
            };
            let collation = match written_collation {
                Some(collation) => self.ast.folded(collation).to_vec(),
                None => target
                    .column(position)
                    .map(|column| column.collation.clone())
                    .unwrap_or_else(|| b"binary".to_vec()),
            };
            keys.push(IndexKeyColumn {
                column: Some(position),
                expr_sql: None,
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
            using,
            columns: keys,
            settings: parsed_settings,
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
        database: Option<ast::NameId>,
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
        let database = match database {
            Some(id) => Some(self.resolve_database(Some(id))?),
            None => None,
        };
        Ok(Directive::Pragma {
            database,
            name: self.ast.folded(name).to_vec(),
            argument,
        })
    }

    /// Returns the temporary database's number when `TEMP` was written.
    ///
    /// A temporary object's name may not be qualified: `CREATE TEMP TABLE
    /// main.t` says two different things about where the table goes, and
    /// SQLite refuses it rather than picking one.
    fn temporary_database(
        &self,
        temporary: bool,
        database: Option<ast::NameId>,
    ) -> Result<Option<usize>, ParseError> {
        if !temporary {
            return Ok(None);
        }
        if database.is_some() {
            return Err(refused(
                "temporary table name must be unqualified",
                Span::default(),
            ));
        }
        self.catalog
            .database_index(b"temp")
            .map(Some)
            .ok_or_else(|| refused("no temporary database", Span::default()))
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

/// Returns a column name as it can be written back into a `CREATE` statement.
///
/// A name a query invented - `SELECT 1` reports the column as `1` - is not an
/// identifier, so it is quoted the way SQLite quotes it: `CREATE TABLE w("1")`.
///
/// @param name - the column's name as the query reports it
fn quoted_name(name: &[u8]) -> Vec<u8> {
    let plain = !name.is_empty()
        && !name.first().is_some_and(u8::is_ascii_digit)
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
    if plain {
        return name.to_vec();
    }
    let mut out = Vec::with_capacity(name.len().saturating_add(2));
    out.push(b'"');
    for byte in name {
        if *byte == b'"' {
            out.push(b'"');
        }
        out.push(*byte);
    }
    out.push(b'"');
    out
}

/// Returns the type name a `CREATE TABLE ... AS SELECT` writes for a column.
///
/// The affinity's own name, with the leading space, exactly as SQLite writes
/// it: BLOB affinity - which is what a column with no declared type has -
/// writes nothing at all, so the copy of an untyped column is untyped.
///
/// @param declared - the source column's declared type, as written
fn affinity_type(declared: &[u8]) -> &'static [u8] {
    match inillucent_value::affinity::for_column(declared) {
        inillucent_value::affinity::Affinity::Blob => b"",
        inillucent_value::affinity::Affinity::Text => b" TEXT",
        inillucent_value::affinity::Affinity::Integer => b" INT",
        inillucent_value::affinity::Affinity::Real => b" REAL",
        inillucent_value::affinity::Affinity::Numeric
        | inillucent_value::affinity::Affinity::FlexNum => b" NUM",
    }
}

/// Returns the width SQLite counts an identifier as when it decides whether to
/// write a `CREATE TABLE ... AS SELECT`'s columns one per line.
///
/// Its own `identLength`: the name plus the two quotes it might need, plus one
/// for each quote inside it that would have to be doubled. The rule that reads
/// it is "under fifty, one line", and reproducing both is what makes the stored
/// declaration byte-identical rather than merely equivalent.
///
/// @param name - the identifier
fn identifier_width(name: &[u8]) -> usize {
    name.len()
        .saturating_add(2)
        .saturating_add(name.iter().filter(|byte| **byte == b'"').count())
}

/// The storage parameters `CREATE INDEX ... WITH ( ... )` accepts.
///
/// One entry per name the vector index understands, with the store option it
/// becomes. **A name that is not here is refused rather than ignored**, which is
/// the same rule `USING` follows a few lines above and for the same reason: an
/// index that quietly was not built the way it was asked to be is a wrong answer
/// nobody can see.
const INDEX_SETTINGS: [(&str, &str); 10] = [
    // The graph's own three, spelled as pgvector spells them.
    ("m", "m"),
    ("ef_construction", "ef_construction"),
    ("ef_search", "ef_search"),
    // Whether a query walks the graph (`approximate`, the default for an
    // `inillucent_hnsw` index) or compares every vector (`exact`). The store
    // validates the value, so `mode = 'fast'` is refused by name.
    ("mode", "mode"),
    // The distance the index is built for. pgvector puts this in an operator
    // class - `USING hnsw (v vector_l2_ops)` - and names it here as well.
    ("metric", "metric"),
    ("distance", "metric"),
    // How many threads the build uses, and how far behind the table the index
    // may fall before it is rebuilt.
    ("threads", "threads"),
    ("compact", "compact"),
    // The two an `ivfflat` has: how many centroids it clusters into, and how
    // many of those lists a query reads.
    ("lists", "lists"),
    ("probes", "probes"),
];

/// Checks `WITH ( ... )` against the structure that will read it.
///
/// Returns the settings as folded `(name, value)` pairs, in the order written.
/// A plain `CREATE INDEX` may not carry any: a b-tree has no parameters, and
/// accepting them would mean accepting a setting nothing reads.
///
/// @param using - the module the index named, when it named one
/// @param settings - the raw `name = value` slices
fn index_settings(
    using: &Option<Vec<u8>>,
    settings: &[Vec<u8>],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ParseError> {
    if settings.is_empty() {
        return Ok(Vec::new());
    }
    if using.is_none() {
        return Err(unsupported(
            "WITH ( ... ) on an index that is not USING a module",
            Span::default(),
        ));
    }
    let mut held = Vec::with_capacity(settings.len());
    for setting in settings {
        let text = String::from_utf8_lossy(setting).to_string();
        let Some((name, value)) = text.split_once('=') else {
            return Err(refused(
                format!("index setting {} is not name = value", text.trim()),
                Span::default(),
            ));
        };
        let folded = name.trim().to_ascii_lowercase();
        let Some((_, option)) = INDEX_SETTINGS
            .iter()
            .find(|(known, _)| *known == folded.as_str())
        else {
            return Err(refused(
                format!("no such index setting: {folded}"),
                Span::default(),
            ));
        };
        let value = value
            .trim()
            .trim_matches(|held| held == '\'' || held == '"');
        held.push((option.as_bytes().to_vec(), value.as_bytes().to_vec()));
    }
    Ok(held)
}
