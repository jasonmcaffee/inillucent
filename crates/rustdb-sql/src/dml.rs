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
use crate::catalog_view::{TableInfo, TableKind, TriggerEventInfo, TriggerInfo};
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
    /// A generated column, computed from the rest of the row rather than from
    /// anything the statement supplied.
    ///
    /// It is its own variant because it is evaluated at a different *time*: a
    /// `DEFAULT` is a value like any other, while a generated column reads the
    /// row it is part of and so cannot be computed until the rest of it is.
    Generated(BoundExpr),
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

/// One statement of a trigger body, bound.
///
/// The four the grammar allows and no more. A trigger body is not a general
/// statement list: it cannot create objects, cannot open transactions, and
/// cannot return rows to the caller, so a variant for anything else would be a
/// shape the binder is required to refuse.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundTriggerStatement {
    /// `INSERT`.
    Insert(Box<BoundInsert>),
    /// `UPDATE`.
    Update(Box<BoundUpdate>),
    /// `DELETE`.
    Delete(Box<BoundDelete>),
    /// `SELECT`, which a body runs for its side effects - in practice for the
    /// `RAISE()` inside it.
    Select(Box<BoundSelect>),
}

/// A trigger, bound against the write that fires it.
///
/// It is bound per statement rather than once per schema because the body's
/// FROM terms take statement-wide source numbers, and those only exist relative
/// to the statement they are inlined into.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundTrigger {
    /// The trigger's name, for the diagnostic when its body fails.
    pub name: Vec<u8>,
    /// Whether it fires before or after the row is written.
    pub time: ast::TriggerTime,
    /// The `WHEN` guard, when one was written.
    pub when: Option<BoundExpr>,
    /// The body statements, in written order.
    pub body: Vec<BoundTriggerStatement>,
}

/// A bound `INSERT`.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundInsert {
    /// The table being written.
    pub table: TableInfo,
    /// The statement-wide number of the FROM term being written.
    ///
    /// It used to be implicitly zero, because a DML statement had exactly one
    /// source. A trigger body is compiled into the statement that fires it, so
    /// its target takes the next number after the firing statement's - and a
    /// compiler that assumed zero read the wrong cursor for every fire after
    /// the first.
    pub target_source: usize,
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
    /// `sqlite_sequence`'s root page, when the target is `AUTOINCREMENT`.
    ///
    /// Resolved here rather than in the compiler because it is a fact about the
    /// catalog, and the catalog is what the binder holds. It is zero for every
    /// other table, which is also what it reads as before the first
    /// `AUTOINCREMENT` table in a database is created.
    pub sequence_root: u32,
    /// The `RETURNING` columns.
    pub returning: Vec<BoundResultColumn>,
    /// The triggers this write fires, in schema order.
    pub triggers: Vec<BoundTrigger>,
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
    /// The statement-wide number of the FROM term being written.
    ///
    /// It used to be implicitly zero, because a DML statement had exactly one
    /// source. A trigger body is compiled into the statement that fires it, so
    /// its target takes the next number after the firing statement's - and a
    /// compiler that assumed zero read the wrong cursor for every fire after
    /// the first.
    pub source: usize,
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
    /// The triggers this write fires, in schema order.
    pub triggers: Vec<BoundTrigger>,
    /// The rows to fire an `INSTEAD OF` trigger for, when the target is a view.
    ///
    /// A view has no rows of its own, so `OLD` has to come from running the
    /// view. This is that query, with the statement's `WHERE` on it and one
    /// result column per view column.
    pub view_rows: Option<Box<BoundSelect>>,
}

/// A bound `DELETE`.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundDelete {
    /// The table being written.
    pub table: TableInfo,
    /// The statement-wide number of the FROM term being written.
    ///
    /// It used to be implicitly zero, because a DML statement had exactly one
    /// source. A trigger body is compiled into the statement that fires it, so
    /// its target takes the next number after the firing statement's - and a
    /// compiler that assumed zero read the wrong cursor for every fire after
    /// the first.
    pub source: usize,
    /// The `WHERE` clause.
    pub filter: Option<BoundExpr>,
    /// The `RETURNING` columns.
    pub returning: Vec<BoundResultColumn>,
    /// The `LIMIT`.
    pub limit: Option<BoundExpr>,
    /// The `OFFSET`.
    pub offset: Option<BoundExpr>,
    /// The triggers this write fires, in schema order.
    pub triggers: Vec<BoundTrigger>,
    /// The rows to fire an `INSTEAD OF` trigger for, when the target is a view.
    pub view_rows: Option<Box<BoundSelect>>,
}

/// Returns whether a view has an `INSTEAD OF` trigger for one event.
fn has_instead_of(table: &TableInfo, event: &TriggerEventInfo) -> bool {
    table
        .triggers
        .iter()
        .any(|trigger| trigger.time == ast::TriggerTime::InsteadOf && trigger.fires_for(event, &[]))
}

impl<'a> Binder<'a> {
    /// Binds an `INSERT` or `REPLACE`.
    pub fn bind_insert(&mut self, insert: &ast::Insert) -> Result<BoundInsert, ParseError> {
        if !insert.with.ctes.is_empty() {
            return Err(unsupported("WITH on INSERT", Span::default()));
        }
        let table = self.writable_target(
            insert.database,
            insert.table,
            Span::default(),
            &TriggerEventInfo::Insert,
        )?;
        let alias = match insert.alias {
            Some(alias) => self.ast.text(alias).to_vec(),
            None => table.name.clone(),
        };
        let target_source = self.push_write_source(table.clone(), alias);
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
        let triggers = self.bind_triggers(&table, TriggerEventInfo::Insert, &[])?;
        let sequence_root = if table.autoincrement {
            self.catalog
                .find_table(None, b"sqlite_sequence")
                .map_or(0, |sequence| sequence.root)
        } else {
            0
        };
        Ok(BoundInsert {
            table,
            target_source,
            columns,
            rowid,
            source,
            arity,
            on_conflict: insert.on_conflict,
            checks,
            upsert,
            sequence_root,
            returning,
            triggers,
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
        let (table, source) =
            self.write_target_from_term(update.target, &TriggerEventInfo::Update(Vec::new()))?;
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
        let changed: Vec<Vec<u8>> = assignments
            .iter()
            .filter_map(|assignment| table.column(assignment.column))
            .map(|column| column.folded.clone())
            .collect();
        let triggers =
            self.bind_triggers(&table, TriggerEventInfo::Update(Vec::new()), &changed)?;
        let view_rows = self.view_rows(&table, filter.clone());
        Ok(BoundUpdate {
            table,
            source,
            assignments,
            filter,
            on_conflict: update.on_conflict,
            checks,
            returning,
            limit,
            offset,
            triggers,
            view_rows,
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
        let (table, source) =
            self.write_target_from_term(delete.target, &TriggerEventInfo::Delete)?;
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
        let triggers = self.bind_triggers(&table, TriggerEventInfo::Delete, &[])?;
        let view_rows = self.view_rows(&table, filter.clone());
        Ok(BoundDelete {
            table,
            source,
            filter,
            returning,
            limit,
            offset,
            triggers,
            view_rows,
        })
    }

    /// Binds the triggers one write fires, bodies and all.
    ///
    /// The bodies are bound here, into the same binder, so their FROM terms take
    /// statement-wide source numbers alongside the write's own. That is what
    /// lets the compiler inline them: a trigger body is not a separate program
    /// with a separate cursor space, it is more of this statement.
    ///
    /// A trigger already being bound is skipped rather than bound again, which
    /// is SQLite's behaviour with its default `recursive_triggers = off` and is
    /// also the only reason inlining terminates.
    fn bind_triggers(
        &mut self,
        table: &TableInfo,
        event: TriggerEventInfo,
        changed: &[Vec<u8>],
    ) -> Result<Vec<BoundTrigger>, ParseError> {
        // The catalog reference is copied out of `self` first: the trigger's
        // arena has to outlive the binder for the body to be bound in place,
        // and a borrow taken through `&self` would end at the first `&mut self`.
        let catalog = self.catalog;
        let database = catalog.database_name(table.database).to_vec();
        let Some(live) = catalog.find_table(Some(database.as_slice()), &table.folded) else {
            return Ok(Vec::new());
        };
        let (old, new) = match event {
            TriggerEventInfo::Insert => (false, true),
            TriggerEventInfo::Delete => (true, false),
            TriggerEventInfo::Update(_) => (true, true),
        };
        let mut bound = Vec::new();
        for trigger in &live.triggers {
            if !trigger.fires_for(&event, changed) {
                continue;
            }
            if self.firing.iter().any(|name| *name == trigger.folded) {
                continue;
            }
            if self.firing.len() >= crate::bind::MAX_TRIGGER_DEPTH {
                return Err(refused(
                    "too many levels of trigger recursion",
                    Span::default(),
                ));
            }
            self.firing.push(trigger.folded.clone());
            let saved_ast = self.ast;
            let saved_scopes = core::mem::take(&mut self.scopes);
            let saved_aliases = self.row_aliases.take();
            let saved_target = self.view_target.take();
            self.ast = &trigger.ast;
            self.row_aliases = Some(crate::bind::RowAliases {
                table: table.clone(),
                old,
                new,
            });
            let result = self.bind_trigger_body(trigger);
            self.ast = saved_ast;
            self.scopes = saved_scopes;
            self.row_aliases = saved_aliases;
            self.view_target = saved_target;
            self.firing.pop();
            bound.push(result?);
        }
        Ok(bound)
    }

    /// Binds one trigger's guard and body statements.
    fn bind_trigger_body(&mut self, trigger: &TriggerInfo) -> Result<BoundTrigger, ParseError> {
        let when = match trigger.when {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let mut body = Vec::new();
        for statement in &trigger.body {
            // Each statement gets a fresh scope stack. A body statement's names
            // resolve against its own tables and against OLD and NEW, never
            // outward into the statement that fired it.
            let saved = core::mem::take(&mut self.scopes);
            let one = self.bind_trigger_statement(statement);
            self.scopes = saved;
            body.push(one?);
        }
        Ok(BoundTrigger {
            name: trigger.name.clone(),
            time: trigger.time,
            when,
            body,
        })
    }

    /// Binds one statement of a trigger body.
    pub(crate) fn bind_trigger_statement(
        &mut self,
        statement: &ast::Statement,
    ) -> Result<BoundTriggerStatement, ParseError> {
        match statement {
            ast::Statement::Insert(insert) => {
                if !insert.returning.is_empty() {
                    return Err(refused(
                        "RETURNING is not allowed on a trigger body statement",
                        Span::default(),
                    ));
                }
                Ok(BoundTriggerStatement::Insert(Box::new(
                    self.bind_insert(insert)?,
                )))
            }
            ast::Statement::Update(update) => {
                if !update.returning.is_empty() {
                    return Err(refused(
                        "RETURNING is not allowed on a trigger body statement",
                        Span::default(),
                    ));
                }
                Ok(BoundTriggerStatement::Update(Box::new(
                    self.bind_update(update)?,
                )))
            }
            ast::Statement::Delete(delete) => {
                if !delete.returning.is_empty() {
                    return Err(refused(
                        "RETURNING is not allowed on a trigger body statement",
                        Span::default(),
                    ));
                }
                Ok(BoundTriggerStatement::Delete(Box::new(
                    self.bind_delete(delete)?,
                )))
            }
            ast::Statement::Select(select) => Ok(BoundTriggerStatement::Select(Box::new(
                self.bind_select(*select)?,
            ))),
            _ => Err(unsupported(
                "that statement in a trigger body",
                Span::default(),
            )),
        }
    }

    /// Resolves a write target and refuses the things that cannot be written.
    fn writable_target(
        &mut self,
        database: Option<ast::NameId>,
        name: ast::NameId,
        span: Span,
        event: &TriggerEventInfo,
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
            TableKind::View => {
                // A view is writable exactly when it has an `INSTEAD OF`
                // trigger for this event: the trigger *is* the write, and the
                // view itself is never touched.
                if !has_instead_of(&table, event) {
                    return Err(unsupported("writing to a view", span));
                }
                let expanded = self.expanded_view(&table, span)?;
                self.record_write_dependency(table.database);
                return Ok(expanded);
            }
            TableKind::Virtual => return Err(unsupported("writing to a virtual table", span)),
            TableKind::Subquery => return Err(unsupported("writing to a subquery", span)),
            TableKind::Table => {}
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
    fn write_target_from_term(
        &mut self,
        id: ast::FromTermId,
        event: &TriggerEventInfo,
    ) -> Result<(TableInfo, usize), ParseError> {
        let Some(term) = self.ast.from_term(id) else {
            return Err(unsupported("missing target", Span::default()));
        };
        let ast::FromSource::Table { database, name, .. } = term.source else {
            return Err(unsupported("a target that is not a table", term.span));
        };
        let table = self.writable_target(database, name, term.span, event)?;
        let alias = match term.alias {
            Some(alias) => self.ast.text(alias).to_vec(),
            None => table.name.clone(),
        };
        if table.kind == TableKind::View {
            // The view goes in as an ordinary nested query, so the statement's
            // WHERE and SET bind against the view's own columns and against the
            // term the block producing OLD will iterate. Binding first and
            // re-pointing afterwards would be two chances to disagree.
            let inner = self.view_query(&table, term.span)?;
            let source = BoundSource {
                id: self.sources.len(),
                rows: crate::bind::SourceRows::Subquery(Box::new(inner)),
                table: table.clone(),
                alias,
                join: ast::JoinKind::Comma,
                constraint: None,
                suppressed: Vec::new(),
            };
            self.view_target = Some(source.id);
            let scope = source.id;
            self.sources.push(source);
            self.scopes.push(vec![scope]);
            return Ok((table, scope));
        }
        let scope = self.push_write_source(table.clone(), alias);
        Ok((table, scope))
    }

    /// Returns a view's `TableInfo` with the columns its body produces.
    ///
    /// A view's catalog entry carries no column list - its columns are whatever
    /// binding its `SELECT` says they are - so a statement that writes one needs
    /// the body bound before `new.column` can resolve to anything at all.
    pub(crate) fn expanded_view(
        &mut self,
        table: &TableInfo,
        span: Span,
    ) -> Result<TableInfo, ParseError> {
        let bound = self.view_query(table, span)?;
        let mut expanded = table.clone();
        expanded.columns = crate::bind::subquery_columns(&bound, &[]);
        Ok(expanded)
    }

    /// Binds a view's body, out of the arena the catalog snapshot holds.
    fn view_query(&mut self, table: &TableInfo, span: Span) -> Result<BoundSelect, ParseError> {
        let catalog = self.catalog;
        let database = catalog.database_name(table.database).to_vec();
        let Some(live) = catalog.find_table(Some(database.as_slice()), &table.folded) else {
            return Err(crate::bind::no_such_table(&table.name, span));
        };
        let Some(body) = live.view.as_ref() else {
            return Err(unsupported(
                "a view whose definition could not be parsed",
                span,
            ));
        };
        let names = body.columns.clone();
        let saved_ast = self.ast;
        let saved_scopes = core::mem::take(&mut self.scopes);
        self.ast = &body.ast;
        let bound = self.bind_select(body.select);
        self.ast = saved_ast;
        self.scopes = saved_scopes;
        let mut bound = bound?;
        // `CREATE VIEW v (a, b)` renames the body's columns, and those are the
        // names `new.a` resolves against.
        for (position, name) in names.iter().enumerate() {
            if let Some(column) = bound.columns.get_mut(position) {
                column.name = name.clone();
            }
        }
        Ok(bound)
    }

    /// Builds the block whose rows an `INSTEAD OF UPDATE` or `DELETE` fires for.
    ///
    /// It reads the term `write_target_from_term` already pushed, so the filter
    /// handed in here - bound against that same term - needs no adjustment.
    fn view_rows(
        &mut self,
        table: &TableInfo,
        filter: Option<BoundExpr>,
    ) -> Option<Box<BoundSelect>> {
        // The kind is checked before the target is taken. A trigger body's own
        // UPDATE binds through here too, and taking first meant the body's
        // statement - whose target is an ordinary table - consumed the view
        // target belonging to the statement that fired it, which then compiled
        // as a write to a view's root page of zero.
        if table.kind != TableKind::View {
            return None;
        }
        let id = self.view_target.take()?;
        let source = self.sources.get(id)?.clone();
        let columns = table
            .columns
            .iter()
            .enumerate()
            .map(|(position, column)| BoundResultColumn {
                expr: BoundExpr::Column {
                    source: id,
                    column: position as u16,
                    slot: position as u16,
                    affinity: column.affinity,
                    collation: Collation::from_name(
                        core::str::from_utf8(&column.collation).unwrap_or("BINARY"),
                    )
                    .unwrap_or(Collation::Binary),
                },
                name: column.name.clone(),
                origin: None,
                declared_type: column.declared_type.clone(),
            })
            .collect();
        Some(Box::new(crate::bind::block_over(source, filter, columns)))
    }

    /// Makes the target table the statement's one visible source.
    ///
    /// It opens a scope holding just the target, so every name in the
    /// statement's `SET`, `WHERE` and `RETURNING` resolves against the table
    /// being written and nothing else.
    fn push_write_source(&mut self, table: TableInfo, alias: Vec<u8>) -> usize {
        let id = self.sources.len();
        self.sources.push(BoundSource {
            id,
            rows: crate::bind::SourceRows::Table,
            table,
            alias,
            join: ast::JoinKind::Comma,
            constraint: None,
            suppressed: Vec::new(),
        });
        self.scopes.push(vec![id]);
        id
    }

    /// Refuses an attempt to write a generated column.
    ///
    /// SQLite's message names the column, because the usual cause is a script
    /// that inserts every column of a table one of whose columns has since been
    /// made generated.
    fn refuse_generated(
        &self,
        table: &TableInfo,
        position: u16,
        span: Span,
    ) -> Result<(), ParseError> {
        let Some(column) = table.column(position) else {
            return Ok(());
        };
        if !column.generated {
            return Ok(());
        }
        Err(refused(
            format!(
                "cannot INSERT into generated column \"{}\"",
                String::from_utf8_lossy(&column.name)
            ),
            span,
        ))
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
            // A bare `INSERT INTO t VALUES (...)` supplies the columns a person
            // can write, which is every column that is not generated - so a
            // table with a generated column takes fewer values than it has
            // columns, exactly as SQLite counts them.
            return Ok((0..table.columns.len() as u16)
                .filter(|position| {
                    table
                        .column(*position)
                        .is_some_and(|column| !column.generated)
                })
                .collect());
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
            self.refuse_generated(table, position, Span::default())?;
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
                // The scope stack is emptied rather than pushed to, because a
                // pushed scope would still be searched *outward* into the
                // target's, and `INSERT INTO t SELECT a FROM u` would then
                // resolve `a` against `t` when `u` has no such column.
                let saved = core::mem::take(&mut self.scopes);
                let select = self.bind_select(*id);
                let bound = match select {
                    Ok(bound) => bound,
                    Err(error) => {
                        self.scopes = saved;
                        return Err(error);
                    }
                };
                self.scopes = saved;
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
            if let Some(expr) = self.generated_expr(table, position)? {
                columns.push(ColumnSource::Generated(expr));
                continue;
            }
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

    /// Binds a generated column's expression, when the column is one.
    fn generated_expr(
        &mut self,
        table: &TableInfo,
        position: u16,
    ) -> Result<Option<BoundExpr>, ParseError> {
        let Some(column) = table.column(position) else {
            return Ok(None);
        };
        if !column.generated {
            return Ok(None);
        }
        let Some(sql) = column.generated_sql.clone() else {
            return Ok(Some(BoundExpr::Null));
        };
        Ok(Some(self.bind_schema_expr(&sql)?))
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
        nested.scopes = self.scopes.clone();
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
