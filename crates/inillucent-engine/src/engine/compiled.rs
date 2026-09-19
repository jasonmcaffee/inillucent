//! The write path: compiling a statement to a plan, and applying what it decided.
//!
//! Invariant: **a write is compiled once and the compiled form is what runs.**
//! `compile` turns a bound statement into a `Cached`, and `apply_compiled` is
//! what every insert, update and delete goes through. The rows a write touches
//! are found by `keys_plan`, which is a read, and changed by `write`, which is
//! the only thing here that writes.

use inillucent_base::error::{refusal, Unwind};
use inillucent_base::DbResult;
use inillucent_exec::dml::{self, Changes};
use inillucent_exec::physical::{self, Params};
use inillucent_sql::bind::{AllowAll, Binder, BoundStatement};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::plan::{plan_select_with, Levers, PhysicalPlan};

use crate::*;

impl crate::ImportedDatabase {
    /// Runs one already-compiled statement.
    ///
    /// @param cached - the compiled statement
    /// @param params - the bound parameters
    pub(crate) fn execute_compiled(
        &mut self,
        cached: &std::rc::Rc<Cached>,
        params: &Params,
    ) -> DbResult<Outcome> {
        // **The connection's counters, handed to the statement before it is
        // compiled.** `changes()`, `total_changes()`, `last_insert_rowid()` and
        // the random built-ins' seed are questions about the connection rather
        // than about a row, and the executor has no connection - so they are
        // read once here and travel on the parameter set, which is the same
        // route a folded subquery takes and for the same reason: a plan is
        // cached by its text, and a value baked into the plan would answer with
        // whatever was true when it was first compiled.
        params.set_context(self.scalar_context());
        params.set_recursive_triggers(self.pragmas.recursive_triggers());
        // **The file lock, taken here and released here.** Both entry points -
        // `execute_any` and `execute_statement` - come through this function, so
        // no statement can run without it. Under `exclusive`, which is the
        // default, both calls compare two integers. Under `normal` this is what
        // lets a second process have the file between statements, and what makes
        // this connection notice when one has written to it.
        self.enter(Self::writes_of(cached))?;
        let outcome = self.apply_compiled(cached, params);
        self.leave()?;
        let outcome = outcome?;
        // **The cyclic half of a foreign key's action happens here**, after the
        // statement rather than inside it, because a cascade that can reach
        // itself cannot be inlined to a depth the data decides: the body would
        // have to appear once per level the data happens to be deep, and that
        // is not known when the statement is compiled.
        //
        // It sits on this function rather than on `execute_any` because this is
        // the funnel *both* callers reach - a statement run by text and a
        // statement prepared and stepped - and a settle that only one of them
        // performed would leave the tree half-repaired depending on which API
        // the application happened to use.
        if !self.writing.settling() {
            self.writing.set_settling(true);
            let settled = self.settle_foreign_keys();
            self.writing.set_settling(false);
            settled?;
        }
        Ok(outcome)
    }

    /// Tells a module to remove each row a query found.
    ///
    /// Its own function because `apply_compiled` has a recorded length in
    /// `crates/inillucent-compat/tests/policy.rs` and this arm is the one that
    /// grew when the change count moved onto the refusal path.
    ///
    /// **The count is recorded on the way out of a refusal too**, for the
    /// reason `insert_into_module` gives: `changes()` reads the connection's
    /// counters, and a statement that errored before touching them left the
    /// *previous* statement's number there. A module that refuses a delete - a
    /// contentless fts5 table refuses every one - reported the last insert's 1
    /// where SQLite reports 0.
    ///
    /// @param statement - the bound delete
    /// @param keys - the rowids the query answered, one per row
    fn delete_from_module(
        &mut self,
        statement: &inillucent_sql::dml::BoundDelete,
        keys: &[Vec<OwnedDatum>],
    ) -> DbResult<Outcome> {
        let mut changed = 0usize;
        for key in keys {
            let Some(rowid) = key.first() else { continue };
            if let Err(error) = self.change_module(
                &statement.table.name,
                &inillucent_sql::vtab::Change::Delete(
                    inillucent_value::Value::from(&rowid.borrow()).into_owned()?,
                ),
            ) {
                self.record_changes(changed as i64, changed as i64);
                return Err(error);
            }
            changed = changed.saturating_add(1);
        }
        if self.writing.batch().is_none() {
            self.sync_modules()?;
            self.seal()?;
        }
        // See the matching comment in `vtab::insert_into_module`.
        self.record_changes(changed as i64, changed as i64);
        Ok(Outcome {
            rows: Vec::new(),
            names: Vec::new(),
            changes: Changes {
                rows: changed,
                ..Default::default()
            },
        })
    }

    /// Runs one already-compiled statement, without settling anything after it.
    ///
    /// @param cached - the compiled statement
    /// @param params - the bound parameters
    fn apply_compiled(
        &mut self,
        cached: &std::rc::Rc<Cached>,
        params: &Params,
    ) -> DbResult<Outcome> {
        // **`PRAGMA query_only` is enforced here, at the one place every
        // compiled statement passes through.** A caller sets it to make a
        // mistake impossible, so recording it and writing anyway would be worse
        // than not having the pragma at all. The message is SQLite's own, which
        // is what an application's error handling is written against.
        if self.pragmas.query_only() && writes_something(cached) {
            return Err(inillucent_base::error::DbError::primary(
                inillucent_base::error::PrimaryCode::ReadOnly,
            )
            .with_message("attempt to write a readonly database")
            .with_detail("attempt to write a readonly database"));
        }
        match &**cached {
            // **Borrowed, not cloned.** `cached` is an `Rc` the caller already
            // holds, so the statement outlives anything this does to `self` -
            // including the DDL path emptying the plan cache. Cloning it was a
            // whole bound statement copied per execution, and for a module
            // insert that is once per row.
            Cached::Nothing => Ok(Outcome::empty()),
            Cached::Ddl(sql) => self.execute_ddl(sql),
            Cached::QueryPlan(lines) => Ok(query_plan_rows(lines)),
            Cached::Program(rows) => Ok(program_rows(rows)),
            Cached::VirtualInsert(statement) => self.insert_into_module(statement, params),
            Cached::SchemaInsert(statement) => self.insert_into_schema(statement, params),
            Cached::Select(plan, prepared, slot) => {
                self.execute_select_cached(plan, prepared, slot, params)
            }
            Cached::Insert(statement, source, values_hold_subquery) => {
                let rows = match source {
                    Some(query) => {
                        self.run_cached_query(&query.plan, &query.prepared, &query.slot, params)?
                    }
                    None => Vec::new(),
                };
                // A `VALUES` list has expressions and no plan, so the
                // plan-shaped fold never sees it. Folded here instead, or a
                // subquery in a value would be refused as though it were
                // correlated - which is what an unfilled slot looks like from
                // inside the physical pass. The flag was decided when the
                // statement was compiled: an insert that holds no subquery is
                // the common case and pays nothing for this.
                let folded = if *values_hold_subquery {
                    self.fold_values(statement, params)?
                } else {
                    None
                };
                let params = folded.as_ref().unwrap_or(params);
                self.write(
                    params,
                    returning_names(&statement.returning),
                    |target, params| dml::insert(statement, target, params, &rows),
                )
            }
            Cached::Update(statement, query, assignments_hold_subquery, setup) => {
                let keys = self.keys_of(query, params)?;
                // The same for an `UPDATE`'s assignments: the plan above finds
                // the rows, and the values written into them are evaluated by
                // the write path from expressions the plan never carried.
                let folded = if *assignments_hold_subquery || !statement.returning.is_empty() {
                    let assigned: Vec<&inillucent_sql::bind::BoundExpr> = statement
                        .assignments
                        .iter()
                        .map(|assignment| &assignment.value)
                        .chain(statement.returning.iter().map(|column| &column.expr))
                        .collect();
                    inillucent_exec::subquery::fold_expressions(&assigned, self, params)?
                } else {
                    None
                };
                let params = folded.as_ref().unwrap_or(params);
                self.write(
                    params,
                    returning_names(&statement.returning),
                    |target, params| dml::update_cached(statement, target, params, &keys, setup),
                )
            }
            Cached::VirtualUpdate(statement, query) => {
                let keys =
                    self.run_cached_query(&query.plan, &query.prepared, &query.slot, params)?;
                let changed = self.update_module(statement, &keys, params)?;
                if self.writing.batch().is_none() {
                    self.sync_modules()?;
                    self.seal()?;
                }
                // See the matching comment in `vtab::insert_into_module`: a
                // module's own write has to reach `changes()`/`total_changes()`
                // the same way an ordinary one does, and this path never
                // touched either counter.
                self.record_changes(changed as i64, changed as i64);
                Ok(Outcome {
                    rows: Vec::new(),
                    names: Vec::new(),
                    changes: Changes {
                        rows: changed,
                        ..Default::default()
                    },
                })
            }
            Cached::VirtualDelete(statement, query) => {
                let keys =
                    self.run_cached_query(&query.plan, &query.prepared, &query.slot, params)?;
                self.delete_from_module(statement, &keys)
            }
            Cached::Delete(statement, query) => {
                let keys = self.keys_of(query, params)?;
                // A `RETURNING` clause is a result-column list the write path
                // evaluates directly, so the plan-shaped fold never sees its
                // subqueries. `DELETE ... RETURNING id, (SELECT count(*) FROM
                // b)` came back as "a correlated subquery" - which is what an
                // unfilled slot looks like from inside `translate`, and a true
                // sentence about the slot rather than about the query.
                let returned: Vec<&inillucent_sql::bind::BoundExpr> = statement
                    .returning
                    .iter()
                    .map(|column| &column.expr)
                    .collect();
                let folded = inillucent_exec::subquery::fold_expressions(&returned, self, params)?;
                let params = folded.as_ref().unwrap_or(params);
                self.write(
                    params,
                    returning_names(&statement.returning),
                    |target, params| dml::delete(statement, target, params, &keys),
                )
            }
        }
    }

    /// Folds the subqueries in an insert's `VALUES` list, when it has one.
    ///
    /// An insert whose source is a `SELECT` is planned, so its subqueries are
    /// folded by the plan-shaped path along with everything else in that plan.
    /// A `VALUES` list is not planned at all - the write path evaluates its
    /// expressions directly - so it is folded here.
    ///
    /// @param statement - the bound insert
    /// @param params - the values bound for this execution
    fn fold_values(
        &self,
        statement: &inillucent_sql::dml::BoundInsert,
        params: &Params,
    ) -> DbResult<Option<Params>> {
        let inillucent_sql::dml::BoundInsertSource::Values(rows) = &statement.source else {
            return Ok(None);
        };
        let values: Vec<&inillucent_sql::bind::BoundExpr> = rows.iter().flatten().collect();
        inillucent_exec::subquery::fold_expressions(&values, self, params)
    }

    /// Compiles an `EXPLAIN`, which the two forms of do different things.
    ///
    /// **`EXPLAIN QUERY PLAN` is answerable and plain `EXPLAIN` is not**, and
    /// the reason is structural rather than unfinished. SQLite's `EXPLAIN`
    /// lists the opcodes of the bytecode program it compiled; this engine
    /// compiles no bytecode - it builds an operator chain - so there is no
    /// opcode listing to print, and printing the operator chain under that name
    /// would be answering a different question with the same word.
    ///
    /// `EXPLAIN QUERY PLAN` asks what the plan *is*, which this engine can
    /// answer: `Prepared::describe` already renders the chain, and the
    /// benchmark harness has been printing it beside SQLite's since Phase 1 so
    /// a reader can see whether the two chose the same structure.
    ///
    /// @param sql - the whole statement text, for a refusal to quote
    /// @param query_plan - whether `QUERY PLAN` was written
    /// @param inner - the statement being explained
    /// @param parsed - the parse the statement came out of
    fn compile_explain(
        &self,
        sql: &str,
        query_plan: bool,
        inner: &inillucent_sql::ast::Statement,
        parsed: &inillucent_sql::parser::ParsedStatement,
    ) -> DbResult<Cached> {
        let fallback = AllowAll;
        let authorizer: &dyn inillucent_sql::bind::Authorizer = match &self.session_state.authorizer
        {
            Some(held) => held.as_ref(),
            None => &fallback,
        };
        let externals = self.external_functions();
        let mut binder = Binder::new(&self.schema.catalog, &parsed.ast, authorizer)
            .with_source(sql.as_bytes())
            .with_functions(&externals)
            .with_collations(&self.session_state.collations)
            .with_trusted_schema(self.session_state.registry.policy().trusted_schema)
            .with_limits(&self.pragmas.limits().borrow())
            .with_foreign_keys(
                self.pragmas.foreign_keys(),
                self.pragmas.defer_foreign_keys(),
            );
        let bound = binder.bind_statement(inner).map_err(refused)?;
        let lines = match bound {
            BoundStatement::Select(select) => {
                plan_select_with(*select, self.pragmas.levers()).describe()
            }
            // A write's plan is the query that finds the rows it changes, and
            // that is the thing a reader is asking about - "did my DELETE use
            // the index" is the same question as "did the search use it".
            // Answering "a delete" would be answering that it is a delete,
            // which the reader wrote.
            BoundStatement::Update(statement) => self
                .keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?
                .0
                .describe(),
            BoundStatement::Delete(statement) => self
                .keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?
                .0
                .describe(),
            other => vec![describe_statement(&other).to_string()],
        };
        if query_plan {
            return Ok(Cached::QueryPlan(lines));
        }
        Ok(Cached::Program(program_of(&lines)))
    }

    /// Compiles one statement as far as its parameters allow.
    ///
    /// @param sql - the statement text
    pub(crate) fn compile(&self, sql: &str) -> DbResult<Cached> {
        // Counted here rather than at the three call sites, so a fourth path to
        // a compilation cannot be added without moving this number with it.
        self.compiled
            .compiles
            .set(self.compiled.compiles.get().saturating_add(1));
        // `EXPLAIN` is decided before binding, because the binder's job is the
        // statement being explained and not the explaining. The old engine did
        // this a level up, where a VDBE program was available to render; here
        // there is no program, and that difference is the whole of the
        // `query_plan` split below.
        let parsed = self.parse_once(sql)?;
        if let inillucent_sql::ast::Statement::Explain { query_plan, inner } = &parsed.statement {
            return self.compile_explain(sql, *query_plan, inner, &parsed);
        }
        let bound = self.bind_parsed(sql, &parsed);
        self.compiled.recycle(parsed);
        match bound? {
            BoundStatement::Select(select) => {
                let plan = plan_select_with(*select, self.pragmas.levers());
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::Select(
                    Box::new(plan),
                    Box::new(prepared),
                    std::cell::RefCell::new(physical::Slot::default()),
                ))
            }
            BoundStatement::Insert(statement)
                if crate::engine::statements::is_the_schema_table(&statement.table.folded) =>
            {
                // The catalog tree is wider than `sqlite_schema` declares, so
                // the row goes through `record` - see `insert_into_schema`.
                Ok(Cached::SchemaInsert(statement))
            }
            BoundStatement::Insert(statement)
                if statement.table.kind == inillucent_sql::catalog_view::TableKind::Virtual =>
            {
                // A write to a virtual table is the *module's* to make. The
                // engine evaluates the row and hands it over; what happens to it
                // is the module's business, which is what makes a module a
                // module rather than a table with a funny name.
                Ok(Cached::VirtualInsert(statement))
            }
            BoundStatement::Insert(statement) => {
                let source = match &statement.source {
                    inillucent_sql::dml::BoundInsertSource::Select(select) => {
                        let plan = plan_select_with((**select).clone(), self.pragmas.levers());
                        let prepared = physical::prepare_any(&plan, self)?;
                        Some(CachedQuery::new(plan, prepared))
                    }
                    inillucent_sql::dml::BoundInsertSource::Values(_) => None,
                };
                let values_hold_subquery = match &statement.source {
                    inillucent_sql::dml::BoundInsertSource::Values(rows) => rows
                        .iter()
                        .flatten()
                        .any(inillucent_sql::plan::expression_holds_subquery),
                    inillucent_sql::dml::BoundInsertSource::Select(_) => false,
                };
                Ok(Cached::Insert(statement, source, values_hold_subquery))
            }
            // **A write to a virtual table is the module's to make**, the same
            // way an insert and a delete already are. `UPDATE f SET body=...`
            // reached the ordinary path, asked for the layout of a table with
            // no tree, and answered "no layout imported for the table being
            // written" - so an fts5 table could be inserted into and deleted
            // from and never corrected.
            BoundStatement::Update(statement)
                if statement.table.kind == inillucent_sql::catalog_view::TableKind::Virtual =>
            {
                let select = inillucent_exec::dml::module_keys_query(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                );
                let plan = plan_select_with(select, self.pragmas.levers());
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::VirtualUpdate(
                    statement,
                    CachedQuery::new(plan, prepared),
                ))
            }
            BoundStatement::Update(statement) => {
                let (plan, prepared) = self.update_keys_plan(&statement)?;
                let assignments_hold_subquery = statement.assignments.iter().any(|assignment| {
                    inillucent_sql::plan::expression_holds_subquery(&assignment.value)
                });
                Ok(Cached::Update(
                    statement,
                    CachedQuery::new(plan, prepared),
                    assignments_hold_subquery,
                    dml::UpdateCache::default(),
                ))
            }
            BoundStatement::Delete(statement)
                if statement.table.kind == inillucent_sql::catalog_view::TableKind::Virtual =>
            {
                let select = inillucent_exec::dml::module_keys_query(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                );
                let plan = plan_select_with(select, self.pragmas.levers());
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::VirtualDelete(
                    statement,
                    CachedQuery::new(plan, prepared),
                ))
            }
            BoundStatement::Delete(statement) if statement.view_rows.is_some() => {
                let rows = statement
                    .view_rows
                    .as_ref()
                    .ok_or_else(|| refusal("a view delete with no query"))?;
                let plan = plan_select_with((**rows).clone(), self.pragmas.levers());
                let prepared = physical::prepare_any(&plan, self)?;
                Ok(Cached::Delete(statement, CachedQuery::new(plan, prepared)))
            }
            BoundStatement::Delete(statement) => {
                let (plan, prepared) = self.keys_plan(
                    &statement.table,
                    statement.source,
                    statement.filter.as_ref(),
                    statement.limit.as_ref(),
                    statement.offset.as_ref(),
                )?;
                Ok(Cached::Delete(statement, CachedQuery::new(plan, prepared)))
            }
            // A directive is *not* cached as a compiled thing: it changes the
            // catalog the next statement will be bound against, and the whole
            // point of `refresh_catalog` is that what was compiled before a DDL
            // statement is not run after it. So the entry holds the text, and
            // the execution re-binds against the schema as it is at that
            // moment.
            BoundStatement::Directive(_) => Ok(Cached::Ddl(sql.to_string())),
            // **Text that is only a comment is a statement that does nothing,
            // not a statement that cannot be run.** A `-- comment` after the
            // last `;` of a script is the ordinary way to end a file, and it
            // was refused with "`-- trailing` binds to nothing, which the new
            // engine does not run yet" - which stops the script rather than the
            // comment. SQLite runs it as a no-op and so does this.
            BoundStatement::Empty => Ok(Cached::Nothing),
        }
    }

    /// Plans and prepares the query that finds the rows a write will change.
    ///
    /// @param table - the table being written
    /// @param source - the statement-wide number of its FROM term
    /// @param filter - the statement's `WHERE`
    /// @param limit - the statement's `LIMIT`
    /// @param offset - the statement's `OFFSET`
    fn keys_plan(
        &self,
        table: &TableInfo,
        source: usize,
        filter: Option<&inillucent_sql::bind::BoundExpr>,
        limit: Option<&inillucent_sql::bind::BoundExpr>,
        offset: Option<&inillucent_sql::bind::BoundExpr>,
    ) -> DbResult<(PhysicalPlan, physical::Prepared)> {
        let layout = self
            .schema
            .layouts
            .get(&table.root)
            .ok_or_else(|| refusal("no layout imported for the table being written"))?;
        let select = dml::keys_query(table, source, filter, limit, offset, layout)?;
        let mut plan = plan_select_with(select, self.pragmas.levers());
        // **The one place `Levers::INDEXED_WRITE` has to be applied by hand.**
        // `plan_select_with` is the ordinary read planner, shared with every
        // `SELECT`, so nothing in it ever consulted this lever -
        // `inillucent_sql::plan::write_path_with` does, but nothing calls it
        // any more since the rearchitecture moved a write's row-finding onto
        // this bound `SELECT`. Forcing the sole source to a table scan below
        // is `write_path_with`'s own answer for "the lever is off".
        if !self.pragmas.levers().has(Levers::INDEXED_WRITE) {
            for planned in &mut plan.sources {
                planned.path = inillucent_sql::plan::AccessPath::TableScan { root: table.root };
            }
            // **A `TableScan` consumes no predicate.** The residual above was
            // distributed against the path just discarded, so a predicate
            // that path answered by seeking - `key BETWEEN 10 AND 40` against
            // an index on `key` - is missing from it, and the forced scan
            // then filtered on nothing: `levers.rs`'s
            // `..._changes_the_plan_and_not_the_outcome` measured its "off"
            // arm deleting every row instead of twenty. The whole filter,
            // unconsumed, is what a table scan needs tested instead.
            if let Some(residual) = plan.residuals.first_mut() {
                *residual = filter.cloned();
            }
        }
        let prepared = physical::prepare_any(&plan, self)?;
        Ok((plan, prepared))
    }

    /// Returns the query that finds an `UPDATE`'s rows, and its shape.
    ///
    /// For an ordinary `UPDATE` this is [`ImportedDatabase::keys_plan`]. For an
    /// `UPDATE ... FROM` the query also carries the joined terms and projects
    /// the assigned values beside the key, because those values read a row the
    /// write path never sees.
    ///
    /// @param statement - the bound update
    fn update_keys_plan(
        &self,
        statement: &inillucent_sql::dml::BoundUpdate,
    ) -> DbResult<(PhysicalPlan, physical::Prepared)> {
        // **A view's `keys` are its rows.** There is no tree to find a key in:
        // an `INSTEAD OF UPDATE` fires with `OLD` taken from running the view,
        // which is exactly the query the binder left on `view_rows`.
        if let Some(rows) = &statement.view_rows {
            let plan = plan_select_with((**rows).clone(), self.pragmas.levers());
            let prepared = physical::prepare_any(&plan, self)?;
            return Ok((plan, prepared));
        }
        if statement.from.is_empty() {
            return self.keys_plan(
                &statement.table,
                statement.source,
                statement.filter.as_ref(),
                statement.limit.as_ref(),
                statement.offset.as_ref(),
            );
        }
        let layout = self
            .schema
            .layouts
            .get(&statement.table.root)
            .ok_or_else(|| refusal("no layout imported for the table being written"))?;
        let assigned: Vec<inillucent_sql::bind::BoundExpr> = statement
            .assignments
            .iter()
            .map(|assignment| assignment.value.clone())
            .collect();
        let select = dml::keys_query_joined(
            &statement.table,
            statement.source,
            statement.filter.as_ref(),
            statement.limit.as_ref(),
            statement.offset.as_ref(),
            layout,
            &statement.from,
            &assigned,
        )?;
        let plan = plan_select_with(select, self.pragmas.levers());
        let prepared = physical::prepare_any(&plan, self)?;
        Ok((plan, prepared))
    }

    /// Runs one write as its own transaction, logged and committed.
    ///
    /// The commit record is appended and awaited *after* the change, which is
    /// what makes the change atomic: recovery replays a transaction only if it
    /// found the commit, so a crash anywhere inside `apply` leaves a log that
    /// describes nothing that happened.
    ///
    /// @param params - the bound parameters
    /// @param apply - what to change
    pub(crate) fn write(
        &mut self,
        params: &Params,
        names: Vec<String>,
        apply: impl FnOnce(&mut dyn WriteTarget, &Params) -> DbResult<Changes>,
    ) -> DbResult<Outcome> {
        // A statement inside an open batch joins it and does not commit; a
        // statement outside one is its own transaction and does.
        let (txn, autocommit) = match self.writing.batch() {
            Some(held) => (held, false),
            None => {
                let txn = self.writing.next_txn();
                self.writing.set_next_txn(txn.saturating_add(1));
                // **What `current_txn()` answers for the rest of this
                // statement.** `next_txn` has just moved past this number, so
                // anything the statement reaches that asks for "the current
                // transaction" - a module write out of
                // `follow_vector_indexes`, most of all - would otherwise be
                // told the next one and log into a transaction nothing
                // commits. Cleared on every exit below, including the failure
                // one.
                self.writing.set_statement_txn(Some(txn));
                (txn, true)
            }
        };
        // **Collected whether or not a transaction is open**, because a
        // statement is abandoned by more than a rollback. SQLite's default
        // algorithm is `ABORT`, which undoes *the statement* and keeps the
        // transaction, and an autocommit statement gets it too: this buffer
        // used to be `None` outside a transaction on the reasoning that
        // "an autocommit statement cannot be abandoned", and that was the bug -
        // a four-row `INSERT` failing on its third row kept the first two and
        // committed them.
        //
        // Outside a transaction it holds at most one statement: `write` clears
        // it when the statement ends, either way. Every schema's log appends to
        // the one buffer, because a rollback undoes one *transaction* rather
        // than one file - and each record carries the schema it came out of.
        let undo = Some(self.writing.undo());
        // **Where this statement's writes begin.** The success path does
        // nothing with it; the failure path rolls back to it. That asymmetry is
        // the whole cost of statement atomicity inside a transaction - one
        // integer read off a `Vec`'s length - which is why there is no
        // per-statement savepoint here and `txn.large`'s two thousand
        // statements do not pay for two thousand of them.
        let mark = self.writing.undo().borrow().len();
        let main_log = WalLog {
            wal: std::rc::Rc::clone(&self.storage.wal),
            txn,
            schema: MAIN,
            wrote: false,
            undo,
            uncommitted: self.storage.database.pool().uncommitted_handle(),
        };
        let logs = if self.session_state.attached.is_empty() && self.session_state.temps.is_empty()
        {
            Logs::One(main_log)
        } else {
            // One per schema *number*, so that `logs[at]` is the log of the file
            // schema `at` names. The temporary slot is filled with `main`'s log
            // when this session has no temporary database, and nothing can reach
            // it: a handle that resolved to `TEMP` could only have come from a
            // temporary tree, which only exists when the schema does.
            let mut held: Vec<WalLog<'_>> =
                Vec::with_capacity(self.session_state.attached.len().saturating_add(2));
            held.push(main_log);
            held.push(match self.session_state.schema_at(TEMP) {
                Some(temp) => WalLog {
                    wal: std::rc::Rc::clone(&temp.wal),
                    txn,
                    schema: TEMP,
                    wrote: false,
                    undo,
                    uncommitted: temp.database.pool().uncommitted_handle(),
                },
                None => WalLog {
                    wal: std::rc::Rc::clone(&self.storage.wal),
                    txn,
                    schema: MAIN,
                    wrote: false,
                    undo,
                    uncommitted: self.storage.database.pool().uncommitted_handle(),
                },
            });
            for (nth, attached) in self.session_state.attached.iter().enumerate() {
                held.push(WalLog {
                    wal: std::rc::Rc::clone(&attached.wal),
                    txn,
                    schema: FIRST_ATTACHED.saturating_add(nth),
                    wrote: false,
                    undo,
                    uncommitted: attached.database.pool().uncommitted_handle(),
                });
            }
            Logs::Many(held)
        };
        let session = self.session_state.session.get();
        let (applied, wrote, counted) = {
            let mut view = WriteView {
                database: &mut self.storage.database,
                attached: &mut self.session_state.attached,
                temps: &mut self.session_state.temps,
                session,
                logs,
                owner: &self.session_state.owner,
                trees: &mut self.schema.trees,
                layouts: &self.schema.layouts,
                covering: &self.schema.covering,
                indexed: &self.session_state.vector_indexes,
                counted: std::cell::Cell::new((0, 0, None)),
                registry: &self.session_state.registry,
            };
            // **Not `?`.** A failed statement has writes of its own to put
            // back, and the borrow of the trees has to end before anything can.
            let applied = apply(&mut view, params);
            // **The participant set, read off the logs that were used.** A
            // transaction that wrote one file commits the way it always has; one
            // that wrote two is decided by a super-journal, and this is the only
            // place that can tell them apart without asking every tree. Read on
            // the failure path too, because a statement that failed partway
            // still wrote, and `OR FAIL` keeps what it wrote.
            let wrote = view.logs.wrote();
            // Read on the failure path too, and for the same reason `wrote` is:
            // a statement that failed partway still wrote, and `OR FAIL` keeps
            // it - so the counters have to see it.
            let counted = view.rows_written();
            (applied, wrote, counted)
        };
        let changes = match applied {
            Ok(changes) => changes,
            Err(error) => {
                // **The rowid moves even though the row does not.** SQLite
                // documents `last_insert_rowid()` as the last rowid
                // *attempted*, and measures out that way: an `INSERT` of two
                // rows that fails on the second answers the first row's rowid,
                // with the table holding neither.
                self.remember_rowid(counted.2);
                // What the statement kept, which is what the counters count.
                // `FAIL` keeps the rows it wrote and everything else puts them
                // back, so the tally is taken only for `FAIL` - which is what
                // makes `UPDATE OR FAIL` report `1 | 4` and a plain `UPDATE`
                // that aborts report `0` and no movement at all.
                if error.unwind() == Unwind::Nothing {
                    self.record_changes(counted.0, counted.1);
                } else {
                    self.counters.last_changes.set(0);
                }
                return Err(self.abandon(error, mark, autocommit, wrote, txn));
            }
        };
        self.writing.set_touched(self.writing.touched() | wrote);
        // **Inside the same transaction, and after the trees rather than
        // during them.** The module is registered on the connection and the
        // write borrowed the connection apart, so this is the first moment both
        // halves exist at once. Doing it before the commit below is what makes
        // the table and the index it carries one change rather than two.
        //
        // **It fails the way the statement fails**, not past it: the index it
        // maintains is part of the write, so a statement that could not
        // maintain it is a statement that did not happen. Reached with the
        // trees no longer borrowed, which is what lets it undo.
        // **And flushed, in the same transaction.** A module holds a delta log
        // and folds it into a published generation on `sync`; nothing called
        // `sync` on this path, so a table with a vector index on it accumulated
        // delta entries for ever and never published a generation. The index
        // answered correctly - the read path replays the log over the
        // generation - and it answered by replaying every entry, which made
        // having the index **slower than not having one**: 2.34 s against
        // 0.66 s for the same top-5 over the 2,661 passages of
        // `examples/rag-agent`. A fold is bounded by what this transaction
        // wrote and returns at once when the log is short, so an ordinary small
        // write pays a `sync` that does nothing.
        match self.follow_vector_indexes(&changes) {
            Err(error) => return Err(self.abandon(error, mark, autocommit, wrote, txn)),
            // **Only outside a batch**, which is the same guard the
            // `VirtualUpdate` and `VirtualDelete` arms take. `sync_modules`
            // ends with `commit` on every connected module, and telling a
            // module its transaction is over while the batch is still open
            // would throw away what the rest of the batch is still adding to.
            // Inside a batch, `commit_batch` syncs them once at the end, which
            // is also cheaper: a thousand-row transaction folds once.
            Ok(true) if autocommit => {
                if let Err(error) = self.sync_modules() {
                    return Err(self.abandon(error, mark, autocommit, wrote, txn));
                }
            }
            Ok(true) => {}
            Ok(false) => {}
        }
        if autocommit {
            // **Nothing else can abandon what an autocommit statement wrote**,
            // so the before-images stop being useful here rather than growing
            // for the life of the connection. Held until now so that everything
            // above can still be undone, and cleared before the commit so that
            // a commit which fails leaves nothing behind for the next
            // statement's mark to sit on top of.
            self.writing.undo().borrow_mut().clear();
        }
        self.remember_rowid(changes.last_rowid);
        // **Read off the view rather than off `Changes`**, so the success path
        // and the failure path count the same way and a trigger's rows land in
        // `total_changes()` where SQLite puts them.
        self.record_changes(counted.0, counted.1);
        let committed = if autocommit {
            let participants = self.writing.replace_touched(0);
            self.commit_across(txn, participants)
        } else {
            Ok(())
        };
        // **Cleared whether the commit worked or not**, because what comes next
        // is a different statement either way, and a number left behind here
        // would be handed to it by `current_txn()`.
        self.writing.set_statement_txn(None);
        committed?;
        Ok(Outcome {
            rows: changes.returned.clone(),
            names,
            changes,
        })
    }

    /// Puts back what a failed statement wrote, as its algorithm says.
    ///
    /// **The three raising algorithms differ only here.** `ABORT` - which is
    /// what an error carrying no algorithm at all reads as, and so what a
    /// `STRICT` type failure, a foreign key and a trigger's `RAISE` get -
    /// undoes back to the mark `write` took and leaves the transaction open.
    /// `ROLLBACK` continues up to the transaction, which is the existing
    /// `rollback` in full: floor zero, the savepoints gone, the batch closed.
    /// `FAIL` undoes nothing, and is the only one this engine already matched.
    ///
    /// The error it returns is the statement's own unless the undo itself
    /// failed, in which case that failure is the one worth reporting: a
    /// constraint message describing a database that is now in a state nobody
    /// intended is worse than saying so.
    ///
    /// @param error - what the statement failed with
    /// @param mark - the undo buffer's length before the statement wrote
    /// @param autocommit - whether the statement was its own transaction
    /// @param wrote - the schemas the statement wrote, as a participant set
    /// @param txn - the transaction the statement wrote under
    fn abandon(
        &mut self,
        error: DbError,
        mark: usize,
        autocommit: bool,
        wrote: u16,
        txn: u64,
    ) -> DbError {
        let unwind = error.unwind();
        let undone = match unwind {
            Unwind::Nothing => Ok(()),
            Unwind::Statement => self.undo_to_floor(mark, false, txn),
            // **In autocommit the two are the same thing**: the statement is
            // the transaction, so `ROLLBACK` is `ABORT` with a floor of zero,
            // and there is no batch to close. Inside one it is the existing
            // `rollback` in full - the savepoints gone, the batch closed, the
            // schema refreshed.
            Unwind::Transaction if autocommit => self.undo_to_floor(0, false, txn),
            Unwind::Transaction => self.rollback(),
        };
        if autocommit {
            self.writing.undo().borrow_mut().clear();
            self.writing.marks().borrow_mut().clear();
            if matches!(unwind, Unwind::Nothing) && undone.is_ok() {
                // **`OR FAIL` outside a transaction commits.** The rows written
                // before the failure are kept, and keeping them only in the
                // page cache would make them a fact this process believes and
                // the file does not. The statement failed; its transaction did
                // not.
                self.writing.set_touched(self.writing.touched() | wrote);
                let participants = self.writing.replace_touched(0);
                if let Err(failure) = self.commit_across(txn, participants) {
                    return failure;
                }
            } else {
                // Undone, so there is nothing to commit and nothing to name as
                // a participant. The restores are logged like any other write
                // and no `Commit` follows them, so a recovery replays neither
                // the statement nor its undo.
                //
                // And no-steal has nothing left to hold back either: in
                // autocommit this statement was the whole transaction, so
                // `wrote` is every schema it armed a watermark on.
                for at in schemas_in(wrote) {
                    if let Some(database) = self.schema_file(at) {
                        database.pool().set_uncommitted_lsn(u64::MAX);
                    }
                }
                self.writing.set_touched(0);
            }
        }
        // The statement is over, so its transaction number stops being the
        // answer - cleared here rather than at the two call sites, so that
        // every way out of `write` clears it.
        self.writing.set_statement_txn(None);
        undone.err().unwrap_or(error)
    }
}
