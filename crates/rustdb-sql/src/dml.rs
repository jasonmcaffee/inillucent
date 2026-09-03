//! Binding INSERT, UPDATE and DELETE.
//!
//! Invariant: a bound DML statement names every value it will write, in table
//! column order, before anything is compiled. A column the statement did not
//! mention is not left to be filled in later by whoever runs it - it carries
//! its `DEFAULT`, or a NULL, as an expression like any other. That is what
//! makes `INSERT INTO t(b) VALUES(1)` and `INSERT INTO t VALUES(NULL, 1)`
//! compile to the same shape, and it is why the constraint checks can be
//! written once against a row image rather than twice against two.
//!
//! Constraints are bound here too, out of the `CREATE TABLE` text the file
//! stores. The catalog keeps them as source, because the catalog sits below
//! the binder and cannot bind anything; the binder parses that source against
//! the table it belongs to and gets an ordinary expression back. A CHECK is
//! therefore evaluated by exactly the machinery that evaluates a WHERE clause,
//! which is the only way to be sure the two agree about what `x > 0` means
//! when `x` is text.

use rustdb_base::limits::Limits;
use rustdb_value::{Affinity, Collation};

use crate::ast::{self, ConflictAction};
use crate::bind::{
    no_such_column, refused, unsupported, Binder, BoundExpr, BoundResultColumn, BoundSelect,
    BoundSource,
};
use crate::catalog_view::{TableInfo, TableKind};
use crate::diagnostic::ParseError;
use crate::lexer::Span;
use crate::parser::parse_expression;

/// Where one column's value comes from in an INSERT.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnSource {
    /// The value at this position of the source row.
    Row(usize),
    /// An expression evaluated once per row, which is what a `DEFAULT` is.
    Expr(BoundExpr),
}

/// What an INSERT inserts.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundInsertSource {
    /// Literal rows, each already bound.
    Values(Vec<Vec<BoundExpr>>),
    /// A query, whose result columns feed the target columns in order.
    Select(Box<BoundSelect>),
}

/// One `CHECK` constraint, bound against its table.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCheck {
    /// The constraint's name, when it was written with one.
    pub name: Option<Vec<u8>>,
    /// The predicate.
    pub expr: BoundExpr,
}

/// A bound `INSERT`.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundInsert {
    /// The table being written.
    pub table: TableInfo,
    /// Where each table column's value comes from, in column order.
    pub columns: Vec<ColumnSource>,
    /// Where the rowid comes from, when the statement supplies one.
    pub rowid: Option<ColumnSource>,
    /// The rows.
    pub source: BoundInsertSource,
    /// How many values each source row supplies.
    pub arity: usize,
    /// The statement's conflict algorithm, when it wrote one.
    pub on_conflict: Option<ConflictAction>,
    /// The table's `CHECK` constraints.
    pub checks: Vec<BoundCheck>,
    /// The `ON CONFLICT ... DO UPDATE` clause, when there is one.
    pub upsert: Option<BoundUpsert>,
    /// The `RETURNING` columns.
    pub returning: Vec<BoundResultColumn>,
}

/// A bound `ON CONFLICT ... DO UPDATE` clause.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundUpsert {
    /// The conflict target columns, when written; empty means any constraint.
    pub target: Vec<u16>,
    /// The assignments, or empty for `DO NOTHING`.
    pub assignments: Vec<BoundAssignment>,
    /// Whether the action is `DO UPDATE`.
    pub do_update: bool,
    /// The `WHERE` on the `DO UPDATE`.
    pub filter: Option<BoundExpr>,
}

/// One `SET` assignment.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundAssignment {
    /// The column being assigned.
    pub column: u16,
    /// The new value.
    pub value: BoundExpr,
}

/// A bound `UPDATE`.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundUpdate {
    /// The table being written.
    pub table: TableInfo,
    /// The assignments, in table column order with duplicates already refused.
    pub assignments: Vec<BoundAssignment>,
    /// The `WHERE` clause.
    pub filter: Option<BoundExpr>,
    /// The statement's conflict algorithm, when it wrote one.
    pub on_conflict: Option<ConflictAction>,
    /// The table's `CHECK` constraints.
    pub checks: Vec<BoundCheck>,
    /// The `RETURNING` columns.
    pub returning: Vec<BoundResultColumn>,
    /// The `LIMIT`.
    pub limit: Option<BoundExpr>,
    /// The `OFFSET`.
    pub offset: Option<BoundExpr>,
}

/// A bound `DELETE`.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundDelete {
    /// The table being written.
    pub table: TableInfo,
    /// The `WHERE` clause.
    pub filter: Option<BoundExpr>,
    /// The `RETURNING` columns.
    pub returning: Vec<BoundResultColumn>,
    /// The `LIMIT`.
    pub limit: Option<BoundExpr>,
    /// The `OFFSET`.
    pub offset: Option<BoundExpr>,
}

impl<'a> Binder<'a> {
    /// Binds an `INSERT` or `REPLACE`.
    pub fn bind_insert(&mut self, insert: &ast::Insert) -> Result<BoundInsert, ParseError> {
        if !insert.with.ctes.is_empty() {
            return Err(unsupported("WITH on INSERT", Span::default()));
        }
        let table = self.writable_target(insert.database, insert.table, Span::default())?;
        let alias = match insert.alias {
            Some(alias) => self.ast.text(alias).to_vec(),
            None => table.name.clone(),
        };
        self.push_write_source(table.clone(), alias);
        // `DEFAULT VALUES` supplies nothing, so every column takes its default
        // - which is what an empty target list means here. The grammar does
        // not allow a column list with it, so there is none to honour.
        let targets = match insert.source {
            ast::InsertSource::DefaultValues => Vec::new(),
            ast::InsertSource::Select(_) => self.insert_targets(&table, &insert.columns)?,
        };
        let (source, arity) = self.bind_insert_source(&insert.source, &table, &targets)?;
        if arity != targets.len() {
            return Err(refused(
                format!("{} values for {} columns", arity, targets.len()),
                Span::default(),
            ));
        }
        let (columns, rowid) = self.column_sources(&table, &targets)?;
        let checks = self.bind_checks(&table)?;
        let upsert = self.bind_upsert(&table, insert)?;
        let returning = self.bind_returning(&insert.returning)?;
        Ok(BoundInsert {
            table,
            columns,
            rowid,
            source,
            arity,
            on_conflict: insert.on_conflict,
            checks,
            upsert,
            returning,
        })
    }

    /// Binds an `UPDATE`.
    pub fn bind_update(&mut self, update: &ast::Update) -> Result<BoundUpdate, ParseError> {
        if !update.with.ctes.is_empty() {
            return Err(unsupported("WITH on UPDATE", Span::default()));
        }
        if !update.from.is_empty() {
            return Err(unsupported("UPDATE ... FROM", Span::default()));
        }
        if !update.order_by.is_empty() {
            return Err(unsupported("ORDER BY on UPDATE", Span::default()));
        }
        let table = self.write_target_from_term(update.target)?;
        let mut assignments = Vec::new();
        for (names, value) in &update.assignments {
            let bound = self.bind_expr(*value)?;
            for name in names {
                let folded = self.ast.folded(*name).to_vec();
                let Some(position) = table.column_position(&folded) else {
                    return Err(no_such_column(self.ast.text(*name), Span::default()));
                };
                if assignments
                    .iter()
                    .any(|existing: &BoundAssignment| existing.column == position)
                {
                    return Err(refused(
                        format!(
                            "column {} is assigned twice",
                            String::from_utf8_lossy(self.ast.text(*name))
                        ),
                        Span::default(),
                    ));
                }
                assignments.push(BoundAssignment {
                    column: position,
                    value: bound.clone(),
                });
            }
        }
        assignments.sort_by_key(|assignment| assignment.column);
        let filter = match update.filter {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let checks = self.bind_checks(&table)?;
        let returning = self.bind_returning(&update.returning)?;
        let limit = match update.limit {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let offset = match update.offset {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        Ok(BoundUpdate {
            table,
            assignments,
            filter,
            on_conflict: update.on_conflict,
            checks,
            returning,
            limit,
            offset,
        })
    }

    /// Binds a `DELETE`.
    pub fn bind_delete(&mut self, delete: &ast::Delete) -> Result<BoundDelete, ParseError> {
        if !delete.with.ctes.is_empty() {
            return Err(unsupported("WITH on DELETE", Span::default()));
        }
        if !delete.order_by.is_empty() {
            return Err(unsupported("ORDER BY on DELETE", Span::default()));
        }
        let table = self.write_target_from_term(delete.target)?;
        let filter = match delete.filter {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let returning = self.bind_returning(&delete.returning)?;
        let limit = match delete.limit {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let offset = match delete.offset {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        Ok(BoundDelete {
            table,
            filter,
            returning,
            limit,
            offset,
        })
    }

    /// Resolves a write target and refuses the things that cannot be written.
    fn writable_target(
        &mut self,
        database: Option<ast::NameId>,
        name: ast::NameId,
        span: Span,
    ) -> Result<TableInfo, ParseError> {
        let qualifier = database.map(|id| self.ast.folded(id).to_vec());
        let folded = self.ast.folded(name).to_vec();
        let Some(table) = self
            .catalog
            .find_table(qualifier.as_deref(), &folded)
            .cloned()
        else {
            return Err(crate::bind::no_such_table(self.ast.text(name), span));
        };
        match table.kind {
            TableKind::View => return Err(unsupported("writing to a view", span)),
            TableKind::Virtual => return Err(unsupported("writing to a virtual table", span)),
            TableKind::Table => {}
        }
        if table.without_rowid {
            return Err(unsupported("WITHOUT ROWID tables", span));
        }
        if table.folded.starts_with(b"sqlite_") {
            return Err(unsupported(
                "writing to a table whose name begins with sqlite_",
                span,
            ));
        }
        self.record_write_dependency(table.database);
        Ok(table)
    }

    /// Resolves the target of an UPDATE or DELETE, which is a FROM term.
    fn write_target_from_term(&mut self, id: ast::FromTermId) -> Result<TableInfo, ParseError> {
        let Some(term) = self.ast.from_term(id) else {
            return Err(unsupported("missing target", Span::default()));
        };
        let ast::FromSource::Table { database, name, .. } = term.source else {
            return Err(unsupported("a target that is not a table", term.span));
        };
        let table = self.writable_target(database, name, term.span)?;
        let alias = match term.alias {
            Some(alias) => self.ast.text(alias).to_vec(),
            None => table.name.clone(),
        };
        self.push_write_source(table.clone(), alias);
        Ok(table)
    }

    /// Makes the target table the statement's one visible source.
    fn push_write_source(&mut self, table: TableInfo, alias: Vec<u8>) {
        self.sources.clear();
        self.sources.push(BoundSource {
            table,
            alias,
            join: ast::JoinKind::Comma,
            constraint: None,
            suppressed: Vec::new(),
        });
    }

    /// Returns the target column positions an INSERT writes, in source order.
    ///
    /// With no column list the targets are every column in declaration order,
    /// which is why adding a column to a table changes what a positional
    /// INSERT means - SQLite's behaviour, and the reason the column list is
    /// worth writing.
    fn insert_targets(
        &self,
        table: &TableInfo,
        columns: &[ast::NameId],
    ) -> Result<Vec<u16>, ParseError> {
        if columns.is_empty() {
            return Ok((0..table.columns.len() as u16).collect());
        }
        let mut targets = Vec::with_capacity(columns.len());
        for name in columns {
            let folded = self.ast.folded(*name).to_vec();
            let Some(position) = table.column_position(&folded) else {
                return Err(no_such_column(self.ast.text(*name), Span::default()));
            };
            if targets.contains(&position) {
                return Err(refused(
                    format!(
                        "column {} is named twice",
                        String::from_utf8_lossy(self.ast.text(*name))
                    ),
                    Span::default(),
                ));
            }
            targets.push(position);
        }
        Ok(targets)
    }

    /// Binds the rows an INSERT supplies.
    fn bind_insert_source(
        &mut self,
        source: &ast::InsertSource,
        table: &TableInfo,
        targets: &[u16],
    ) -> Result<(BoundInsertSource, usize), ParseError> {
        match source {
            ast::InsertSource::DefaultValues => {
                let _ = (table, targets);
                Ok((BoundInsertSource::Values(vec![Vec::new()]), 0))
            }
            ast::InsertSource::Select(id) => {
                // The target table is source zero while the rows are bound, so
                // that `INSERT INTO t SELECT ... FROM u` resolves `u`'s columns
                // and not `t`'s. Binding a SELECT replaces the source list, and
                // the target is pushed back afterwards.
                let saved = core::mem::take(&mut self.sources);
                let select = self.bind_select(*id);
                let bound = match select {
                    Ok(bound) => bound,
                    Err(error) => {
                        self.sources = saved;
                        return Err(error);
                    }
                };
                self.sources = saved;
                if bound.values.is_empty() {
                    let arity = bound.columns.len();
                    return Ok((BoundInsertSource::Select(Box::new(bound)), arity));
                }
                let arity = bound.values.first().map_or(0, Vec::len);
                for row in &bound.values {
                    if row.len() != arity {
                        return Err(unsupported(
                            "all VALUES rows must have the same number of columns",
                            Span::default(),
                        ));
                    }
                }
                Ok((BoundInsertSource::Values(bound.values), arity))
            }
        }
    }

    /// Works out where every table column's value comes from.
    ///
    /// A column the statement named takes its value from the source row; a
    /// column it did not takes its `DEFAULT`, and a column with no default
    /// takes NULL. The rowid is separated out here rather than in the
    /// compiler, because an `INTEGER PRIMARY KEY` column *is* the rowid and
    /// writing it into the record as well would store a duplicate that SQLite
    /// does not.
    fn column_sources(
        &mut self,
        table: &TableInfo,
        targets: &[u16],
    ) -> Result<(Vec<ColumnSource>, Option<ColumnSource>), ParseError> {
        let mut columns = Vec::with_capacity(table.columns.len());
        for position in 0..table.columns.len() as u16 {
            let source = match targets.iter().position(|target| *target == position) {
                Some(index) => ColumnSource::Row(index),
                None => ColumnSource::Expr(self.default_expr(table, position)?),
            };
            columns.push(source);
        }
        let rowid = match table.rowid_alias {
            Some(position) => columns.get(position as usize).cloned(),
            None => None,
        };
        Ok((columns, rowid))
    }

    /// Binds a column's `DEFAULT`, or NULL when it has none.
    fn default_expr(&mut self, table: &TableInfo, position: u16) -> Result<BoundExpr, ParseError> {
        let Some(column) = table.column(position) else {
            return Ok(BoundExpr::Null);
        };
        let Some(sql) = column.default_sql.as_ref() else {
            return Ok(BoundExpr::Null);
        };
        if sql.is_empty() {
            return Ok(BoundExpr::Null);
        }
        self.bind_schema_expr(sql)
    }

    /// Binds every `CHECK` the table declares.
    fn bind_checks(&mut self, table: &TableInfo) -> Result<Vec<BoundCheck>, ParseError> {
        let mut checks = Vec::with_capacity(table.checks.len());
        for check in &table.checks {
            checks.push(BoundCheck {
                name: check.name.clone(),
                expr: self.bind_schema_expr(&check.expr_sql)?,
            });
        }
        Ok(checks)
    }

    /// Parses and binds an expression that was written in the schema.
    ///
    /// It is parsed into its own arena and bound against the statement's
    /// current sources, so the result is an ordinary `BoundExpr` that refers to
    /// the target table by position and carries no reference to the schema
    /// text it came from.
    pub fn bind_schema_expr(&mut self, sql: &[u8]) -> Result<BoundExpr, ParseError> {
        let limits = Limits::default();
        let (ast, expr) = parse_expression(sql, &limits)?;
        let mut nested = Binder::new(self.catalog, &ast, self.authorizer);
        nested.sources = self.sources.clone();
        let bound = nested.bind_expr(expr)?;
        Ok(bound)
    }

    /// Binds an `ON CONFLICT` clause.
    fn bind_upsert(
        &mut self,
        table: &TableInfo,
        insert: &ast::Insert,
    ) -> Result<Option<BoundUpsert>, ParseError> {
        let Some(upsert) = insert.upserts.first() else {
            return Ok(None);
        };
        if insert.upserts.len() > 1 {
            return Err(unsupported(
                "more than one ON CONFLICT clause",
                Span::default(),
            ));
        }
        if upsert.target_filter.is_some() {
            return Err(unsupported(
                "a partial-index conflict target",
                Span::default(),
            ));
        }
        // `excluded` is in scope for the assignments and the WHERE, and only
        // there. Setting it around the binding rather than pushing a second
        // FROM term keeps unqualified names resolving to the target row, which
        // is what SQLite does and what a second source would have made
        // ambiguous - every column of the target is also a column of
        // `excluded`.
        self.excluded = Some(table.clone());
        let bound_upsert = self.bind_upsert_body(table, upsert);
        self.excluded = None;
        bound_upsert
    }

    /// Binds an upsert's target, assignments and filter.
    fn bind_upsert_body(
        &mut self,
        table: &TableInfo,
        upsert: &ast::Upsert,
    ) -> Result<Option<BoundUpsert>, ParseError> {
        let mut target = Vec::new();
        for column in &upsert.target {
            let Some(name) = bare_indexed_column(self.ast, column) else {
                return Err(unsupported(
                    "an expression in a conflict target",
                    Span::default(),
                ));
            };
            let Some(position) = table.column_position(&name) else {
                return Err(no_such_column(&name, Span::default()));
            };
            target.push(position);
        }
        let mut assignments = Vec::new();
        for (names, value) in &upsert.assignments {
            let bound = self.bind_expr(*value)?;
            for name in names {
                let folded = self.ast.folded(*name).to_vec();
                let Some(position) = table.column_position(&folded) else {
                    return Err(no_such_column(self.ast.text(*name), Span::default()));
                };
                assignments.push(BoundAssignment {
                    column: position,
                    value: bound.clone(),
                });
            }
        }
        assignments.sort_by_key(|assignment| assignment.column);
        let filter = match upsert.filter {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        Ok(Some(BoundUpsert {
            target,
            assignments,
            do_update: upsert.do_update,
            filter,
        }))
    }

    /// Binds a `RETURNING` list, which is a result-column list over the row
    /// that was written.
    fn bind_returning(
        &mut self,
        columns: &[ast::ResultColumn],
    ) -> Result<Vec<BoundResultColumn>, ParseError> {
        if columns.is_empty() {
            return Ok(Vec::new());
        }
        self.bind_result_columns_public(columns)
    }
}

/// Returns an indexed column's bare folded name, when it names a column.
fn bare_indexed_column(ast: &crate::Ast, column: &ast::IndexedColumn) -> Option<Vec<u8>> {
    match ast.expr(column.expr) {
        Some(ast::Expr::Column {
            table: None,
            column: name,
            ..
        }) => Some(ast.folded(*name).to_vec()),
        _ => None,
    }
}

/// Returns the affinity and collation a table column compares with.
pub fn column_rules(table: &TableInfo, position: u16) -> (Affinity, Collation) {
    let Some(column) = table.column(position) else {
        return (Affinity::Blob, Collation::Binary);
    };
    let collation =
        Collation::from_name(core::str::from_utf8(&column.collation).unwrap_or("BINARY"))
            .unwrap_or(Collation::Binary);
    (column.affinity, collation)
}
