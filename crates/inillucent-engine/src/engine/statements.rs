//! Compiling a statement, and running one.
//!
//! Invariant: **the plan and the execution are separate calls, and that is what
//! the profiling harnesses measure between.** `plan` answers what the planner
//! chose, `prepare` builds the operator chain, and `execute` runs it; a harness
//! that wanted to plan once and execute many could not do it through one
//! combined call.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_exec::physical::{self, ForcePlan, Params};
use inillucent_sql::bind::{AllowAll, Binder, BoundStatement};
use inillucent_sql::plan::{plan_select_with, PhysicalPlan};
use inillucent_tree::datum::OwnedDatum;

use crate::*;

impl crate::ImportedDatabase {
    /// Parses, binds and plans one statement.
    ///
    /// Separated from [`ImportedDatabase::run`] so the gate harness can plan
    /// once and execute many times, which is what `prepare_each: false` means
    /// in a scorecard plan.
    ///
    /// @param sql - the statement text
    ///
    /// **A refusal says what SQLite's says.** These used to wrap the parse or
    /// bind failure with `{error:?}`, so `SELECT * FROM nope` reported
    /// `SELECT * FROM nope;: ParseError { kind: Refused("no such table: nope"),
    /// span: Span { start: 0, end: 0 } }` where SQLite reports `no such table:
    /// nope`. The one-line message was there all along - `ParseError::message`
    /// - and printing the struct around it made every refusal look like a bug
    /// report about the engine rather than a sentence about the statement.
    pub fn plan(&self, sql: &str) -> DbResult<PhysicalPlan> {
        let parsed = self.parse_once(sql)?;
        let bound = self.bind_parsed(sql, &parsed);
        self.compiled.recycle(parsed);
        match bound? {
            BoundStatement::Select(select) => Ok(plan_select_with(*select, self.pragmas.levers())),
            _ => Err(refusal(format!("{sql} is not a read-only statement"))),
        }
    }

    /// Runs a planned statement and returns its rows and column names.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute(
        &self,
        plan: &PhysicalPlan,
        params: &Params,
    ) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        // A compound is several plans and one answer, and a windowed query is a
        // plan with a pass on top of it; neither is a single prepared
        // statement, so both are dispatched by `run_any` rather than inside
        // `prepare` - which returns the structural choice for *one* pipeline.
        let (rows, shape) = physical::run_any(plan, self, params)?;
        Ok((rows, names_of(&shape)))
    }

    /// Chooses a statement's physical plan, once.
    ///
    /// Separated from execution because the choice depends on the statement and
    /// the schema and not on the data, and because making it per execution made
    /// a query answering 64 rows spend more time choosing a tree than reading
    /// one. `prepare once` in a scorecard plan means the same thing on both
    /// sides.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    pub fn prepare(&self, plan: &PhysicalPlan) -> DbResult<physical::Prepared> {
        physical::prepare(plan, self, ForcePlan::default())
    }

    /// Builds a pipeline over an already-prepared statement.
    ///
    /// The measurement path: a caller hands in the sink it wants and drives the
    /// pipeline itself, so the timed region is the pipeline rather than a
    /// `Vec<Vec<OwnedDatum>>` neither engine's caller asked for.
    ///
    /// @param plan - a plan from [`ImportedDatabase::plan`]
    /// @param prepared - the choices [`ImportedDatabase::prepare`] made
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param sink - the end of the pipeline
    pub fn pipeline(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
        sink: Box<dyn inillucent_exec::Sink>,
    ) -> DbResult<(physical::Pipeline<'_>, physical::Shape)> {
        physical::build_prepared(plan, self, prepared, params, sink)
    }

    /// Builds a statement whose operator chain is reused across executions.
    ///
    /// The difference from [`ImportedDatabase::pipeline`] is the difference
    /// between preparing a *plan* and preparing a *statement*. A workload that
    /// binds new parameters and runs again is answered by SQLite from a VDBE
    /// program compiled once; `pipeline` rebuilt the operator chain each time,
    /// which `inillucent-probeprofile` measured at 42% of `point.rowid` and 71%
    /// of `point.miss`. This builds the chain once and rebuilds only the source
    /// whose key the parameters decide.
    ///
    /// @param plan - the planner's output
    /// @param prepared - the structural choices `prepare` made
    /// @param params - the values the first execution binds
    /// @param sink - the end of the pipeline, which the statement keeps
    pub fn statement<'a>(
        &'a self,
        plan: &'a PhysicalPlan,
        prepared: &physical::Prepared,
        params: &Params,
        sink: Box<dyn inillucent_exec::Sink>,
    ) -> DbResult<physical::Statement<'a>> {
        physical::build_statement(plan, self, prepared, params, sink)
    }

    /// Parses, plans and runs one statement.
    ///
    /// @param sql - the statement text
    pub fn run(&self, sql: &str) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let plan = self.plan(sql)?;
        self.execute(&plan, &Params::new())
    }

    /// Parses, plans and runs one statement with parameters bound.
    ///
    /// @param sql - the statement text
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn run_with(
        &self,
        sql: &str,
        params: &Params,
    ) -> DbResult<(Vec<Vec<OwnedDatum>>, Vec<String>)> {
        let plan = self.plan(sql)?;
        self.execute(&plan, params)
    }

    /// Returns the `EXPLAIN QUERY PLAN` lines a statement's plan renders as.
    ///
    /// The harness prints these beside SQLite's so a reader can see whether the
    /// two engines chose the same structure. A ratio measured against a
    /// different structure is not a ratio between engines.
    ///
    /// @param sql - the statement text
    pub fn describe(&self, sql: &str) -> DbResult<Vec<String>> {
        Ok(self.plan(sql)?.describe())
    }

    /// Returns the step listing a plain `EXPLAIN` of a statement would print.
    ///
    /// Compiled through the same cache the statement itself uses, because the
    /// listing is *of* the compiled statement: a listing built from a fresh
    /// plan could describe a plan the next execution would not get.
    ///
    /// @param sql - the statement to list
    pub(crate) fn program_listing(
        &self,
        sql: &str,
    ) -> DbResult<Vec<(String, i64, i64, String, String)>> {
        match &*self.compiled(&format!("EXPLAIN {sql}"))? {
            Cached::Program(listing) => Ok(listing.clone()),
            _ => Ok(Vec::new()),
        }
    }

    /// Returns the objects a statement names, and which one it writes.
    ///
    /// The plan's sources rather than the parse's, so a name that resolved to a
    /// view is reported as the view and the tables behind it are reported too.
    ///
    /// @param sql - the statement to describe
    pub(crate) fn statement_tables(&self, sql: &str) -> DbResult<StatementTables> {
        let parsed = self.parse_once(sql)?;
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
        let bound = binder.bind_statement(&parsed.statement).map_err(refused)?;
        let mut names: Vec<(&'static str, Vec<u8>)> = Vec::new();
        let mut written = None;
        match &bound {
            BoundStatement::Select(select) => collect_sources(select, &mut names),
            BoundStatement::Insert(statement) => written = Some(statement.table.name.clone()),
            BoundStatement::Update(statement) => written = Some(statement.table.name.clone()),
            BoundStatement::Delete(statement) => written = Some(statement.table.name.clone()),
            _ => {}
        }
        if let Some(target) = &written {
            names.insert(0, ("table", target.clone()));
        }
        names.dedup();
        Ok((names, written))
    }

    /// Returns how many columns a cached statement answers with, and whether
    /// it only reads.
    ///
    /// @param sql - the statement text
    pub(crate) fn statement_shape(&self, sql: &str) -> (i64, bool) {
        let columns = match self.compiled(sql) {
            Ok(cached) => match &*cached {
                Cached::Select(plan, _, _) => plan.select.columns.len() as i64,
                _ => 0,
            },
            Err(_) => 0,
        };
        let reads = self
            .statement_tables(sql)
            .map(|(_, written)| written.is_none())
            .unwrap_or(true);
        (columns, reads)
    }

    /// Returns the physical operators a statement runs **through the cache**.
    ///
    /// The difference from [`ImportedDatabase::describe`] is the whole point of
    /// it: that one plans afresh every time, so it could not tell a live cache
    /// from an invalidated one. This asks for the compiled statement the next
    /// execution would get, which is the object a DDL statement has to throw
    /// away.
    ///
    /// @param sql - the statement text
    pub fn describe_cached(&self, sql: &str) -> DbResult<Vec<String>> {
        match &*self.compiled(sql)? {
            Cached::Nothing => Ok(vec!["nothing".to_string()]),
            Cached::Select(_, prepared, _) => Ok(prepared.describe()),
            Cached::Ddl(_) => Ok(vec!["a directive".to_string()]),
            Cached::QueryPlan(_) => Ok(vec!["a query plan".to_string()]),
            Cached::Program(_) => Ok(vec!["a program listing".to_string()]),
            Cached::Insert(..) => Ok(vec!["an insert".to_string()]),
            Cached::VirtualInsert(_) => Ok(vec!["an insert into a module".to_string()]),
            Cached::SchemaInsert(_) => Ok(vec!["an insert into sqlite_schema".to_string()]),
            Cached::VirtualUpdate(..) => Ok(vec!["an update of a module".to_string()]),
            Cached::VirtualDelete(..) => Ok(vec!["a delete from a module".to_string()]),
            Cached::Update(..) => Ok(vec!["an update".to_string()]),
            Cached::Delete(..) => Ok(vec!["a delete".to_string()]),
        }
    }
}

impl ImportedDatabase {
    /// Binds one statement against the imported schema.
    ///
    /// @param sql - the statement text
    pub fn bind(&self, sql: &str) -> DbResult<BoundStatement> {
        let parsed = self.parse_once(sql)?;
        let bound = self.bind_parsed(sql, &parsed);
        self.compiled.recycle(parsed);
        bound
    }

    /// Binds a statement somebody has already parsed.
    ///
    /// **So that a compile parses once.** `compile` has to look at the parse to
    /// decide whether the statement is an `EXPLAIN` - the binder's job is the
    /// statement being explained, not the explaining - and it then called
    /// `bind`, which parsed the same text a second time. On `SELECT 1` that was
    /// 270 ns of a 1,145 ns compile spent producing an arena that was thrown
    /// away, and `prepare.trivial` pays a compile every iteration.
    ///
    /// @param sql - the statement text, for diagnostics and spans
    /// @param parsed - the parse to bind
    pub(crate) fn bind_parsed(
        &self,
        sql: &str,
        parsed: &inillucent_sql::parser::ParsedStatement,
    ) -> DbResult<BoundStatement> {
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
        let mut bound = binder.bind_statement(&parsed.statement).map_err(refused)?;
        // **A correlated `IN` becomes `EXISTS` before anything plans it.** The
        // physical pass computes a correlated block once per outer row and
        // hands the operator one column, which is not a list, so it refused one
        // by name; `EXISTS` over the same block is a path the engine already
        // runs. See `inillucent_sql::correlated_in` for the NULL rules the
        // lowering has to keep.
        inillucent_sql::correlated_in::lower(&mut bound);
        self.refuse_shadow_write(&bound)?;
        self.refuse_schema_write(&bound)?;
        Ok(bound)
    }

    /// Refuses a write of `sqlite_schema` the connection has not asked for.
    ///
    /// **The binder used to refuse every `sqlite_` name outright, and that made
    /// a dump of a virtual table unreplayable (task-1979, R2).** `.dump` writes
    /// `PRAGMA writable_schema=ON` and then
    /// `INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES(...)`
    /// for a virtual table, because running its `CREATE VIRTUAL TABLE` would
    /// build empty shadow tables over the ones the dump restores a few lines
    /// earlier. SQLite accepts that statement under the pragma and this engine
    /// answered `unsupported`, so its own `dump` output stopped at the first
    /// virtual table.
    ///
    /// The pragma is read here rather than in the binder for the reason
    /// `refuse_shadow_write` gives: the check belongs to the connection, and
    /// the binder is bound against a catalog rather than against a connection's
    /// settings.
    ///
    /// **Only an insert.** `UPDATE sqlite_schema SET sql = ...` is how SQLite
    /// repairs a corrupt schema by hand and nothing in this tree produces one,
    /// so it stays refused with the code that says "not built" rather than
    /// being half implemented.
    ///
    /// @param bound - the statement that was just bound
    fn refuse_schema_write(&self, bound: &BoundStatement) -> DbResult<()> {
        let (written, inserting) = match bound {
            BoundStatement::Insert(statement) => (&statement.table.folded, true),
            BoundStatement::Update(statement) => (&statement.table.folded, false),
            BoundStatement::Delete(statement) => (&statement.table.folded, false),
            _ => return Ok(()),
        };
        if !is_the_schema_table(written) {
            return Ok(());
        }
        if !self.pragmas.writable_schema() {
            return Err(refusal(
                "writing to sqlite_schema needs PRAGMA writable_schema = ON",
            ));
        }
        if !inserting {
            return Err(refusal(
                "only an INSERT into sqlite_schema is built, not an UPDATE or a DELETE",
            )
            .with_unsupported("changing a row of sqlite_schema"));
        }
        Ok(())
    }

    /// Refuses a write of a module's private storage on a defensive connection.
    ///
    /// **The second half of task-1972.** `Registry::authorize_shadow_write` had
    /// the same defect `authorize_function` had: it was the whole of what
    /// `PRAGMA defensive` promises about shadow tables, it read a flag nothing
    /// set, and nothing called it. So `.dbconfig defensive on` - which the
    /// shell turns on for every connection it opens - refused a
    /// `journal_mode=off` and nothing else, while an `INSERT` into `docs_data`
    /// put rows into an FTS5 index that the module would later read back as
    /// its own.
    ///
    /// It is checked after the bind rather than inside it because what a shadow
    /// table *is* is a question only a module can answer, and the binder sits
    /// below the crate the modules live in.
    ///
    /// A module writes its own storage through `ShadowStore` and root pages
    /// rather than through SQL, so nothing a module does reaches this.
    ///
    /// @param bound - the statement that was just bound
    fn refuse_shadow_write(&self, bound: &BoundStatement) -> DbResult<()> {
        // The registry's policy is asked rather than the pragma record, because
        // the registry is what `authorize_shadow_write` reads and one setting
        // read from two places is how the two stopped agreeing in the first
        // place. `ImportedDatabase::set_defensive` writes both.
        if !self.session_state.registry.policy().defensive {
            return Ok(());
        }
        let written = match bound {
            BoundStatement::Insert(statement) => &statement.table.name,
            BoundStatement::Update(statement) => &statement.table.name,
            BoundStatement::Delete(statement) => &statement.table.name,
            _ => return Ok(()),
        };
        if !self.is_shadow_table(written) {
            return Ok(());
        }
        self.session_state.registry.authorize_shadow_write(written)
    }

    /// Binds a statement whose expressions came out of the schema.
    ///
    /// **What `CREATE INDEX` on an expression checks itself with
    /// (task-1972).** Such an index is filled by a `SELECT` this engine builds
    /// out of the index's own expression and partial predicate, and a `SELECT`
    /// is a statement - so that one query was the place a schema expression
    /// reached the machine with a statement's permissions. Without this,
    /// `CREATE INDEX i ON t (embed(body))` ran `embed` once per row of `t`
    /// while it built the index, and only the *next* write of `t` was refused
    /// for naming a function a schema may not name: the index was built, the
    /// model was loaded, and the table could no longer be written.
    ///
    /// The result is thrown away. It is a check, and the statement is bound
    /// again by the ordinary path when it runs, which costs one bind per
    /// `CREATE INDEX` and nothing per row.
    ///
    /// @param sql - the query the index build will run
    pub(crate) fn refuse_untrusted_schema_query(&self, sql: &str) -> DbResult<()> {
        let parsed = self.parse_once(sql)?;
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
            )
            .in_schema();
        let bound = binder.bind_statement(&parsed.statement).map_err(refused);
        self.compiled.recycle(parsed);
        bound.map(|_| ())
    }

    /// Parses, plans and runs one statement of any kind.
    ///
    /// A `SELECT` answers with rows; an `INSERT`, `UPDATE` or `DELETE` answers
    /// with a count and whatever `RETURNING` asked for. One entry point rather
    /// than two, because a corpus record does not say which it is and a harness
    /// that had to guess would be guessing from the SQL text.
    ///
    /// @param sql - the statement text
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_any(&mut self, sql: &str, params: &Params) -> DbResult<Outcome> {
        let cached = self.compiled(sql)?;
        self.execute_compiled(&cached, params)
    }

    /// Compiles one statement and hands back the handle, without running it.
    ///
    /// **So that a caller can take the compile out of a timed region**, which is
    /// where SQLite's already is: `sqlite_bench.c` calls `sqlite3_prepare_v2`
    /// before it reads the clock and then resets and re-binds inside the loop.
    /// A harness that looked its statement up per iteration would be timing a
    /// hash of the SQL text that the other arm does not pay.
    ///
    /// @param sql - the statement text
    pub fn prepare_statement(&self, sql: &str) -> DbResult<Statement> {
        Ok(Statement {
            cached: std::cell::RefCell::new(self.compiled(sql)?),
            sql: sql.to_string(),
            generation: std::cell::Cell::new(self.schema_generation()),
        })
    }

    /// Runs a statement [`ImportedDatabase::prepare_statement`] compiled.
    ///
    /// **The plan is compiled again when the schema has moved under it
    /// (task-1932).** A plan is built against a snapshot of the catalog, and a
    /// statement held across a `CREATE TABLE`, a `DROP`, an `ALTER` or a
    /// `REINDEX` is holding one that describes trees that are not there any
    /// more. `Connection::step` has checked this since it existed; this
    /// entry point, which the gates and the profiles run their statements
    /// through, did not - so the two halves of the same public API disagreed
    /// about whether an already-prepared statement follows a schema change.
    /// SQLite's own `sqlite3_step` reprepares, and so does this.
    ///
    /// @param statement - the handle
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_statement(
        &mut self,
        statement: &Statement,
        params: &Params,
    ) -> DbResult<Outcome> {
        let held = self.current_plan(statement)?;
        self.execute_compiled(&held, params)
    }

    /// Returns a statement's plan, compiling it again if the schema has moved.
    ///
    /// @param statement - the handle
    fn current_plan(&self, statement: &Statement) -> DbResult<std::rc::Rc<Cached>> {
        let generation = self.schema_generation();
        if statement.generation.get() != generation {
            let fresh = self.compiled(&statement.sql)?;
            *statement.cached.borrow_mut() = fresh;
            statement.generation.set(generation);
        }
        Ok(std::rc::Rc::clone(&statement.cached.borrow()))
    }

    /// Runs one statement and reports where its time went.
    ///
    /// **Two numbers, because there are two halves and they are fixed in
    /// different places.** `find` is the query that decides which rows change -
    /// an ordinary planned query, whose cost is the operator chain and the
    /// descent. `apply` is everything after: compiling the assignments, reading
    /// the rows, maintaining the indexes and writing the tree.
    ///
    /// This exists because the write gate misses and a guess about which half is
    /// expensive is a guess this project has been wrong about before. It is on
    /// the harness's own type, in a test-only crate, and nothing in the engine
    /// consults it.
    ///
    /// @param statement - a handle from `prepare_statement`
    /// @param params - the values bound to `?1`, `?2`, ...
    pub fn execute_timed(
        &mut self,
        statement: &Statement,
        params: &Params,
    ) -> DbResult<(u128, u128)> {
        let cached = self.current_plan(statement)?;
        let found = std::time::Instant::now();
        let rows = match &*cached {
            Cached::Nothing
            | Cached::Ddl(_)
            | Cached::QueryPlan(_)
            | Cached::Program(_)
            | Cached::VirtualUpdate(..)
            | Cached::VirtualDelete(..)
            | Cached::VirtualInsert(_)
            | Cached::SchemaInsert(_)
            | Cached::Select(..)
            | Cached::Insert(_, None, _) => Vec::new(),
            // This harness measures a fresh build on purpose - see the doc
            // comment - so it keeps calling `run_any_prepared` directly
            // rather than `query`'s slot, exactly as it did before Stage 3.
            Cached::Insert(_, Some(query), _) => {
                physical::run_any_prepared(&query.plan, self, &query.prepared, params)?.0
            }
            Cached::Update(_, query, _, _) | Cached::Delete(_, query) => {
                if let Some(key) = physical::rowid_seek_key(&query.plan, params)? {
                    vec![vec![key]]
                } else {
                    physical::run_any_prepared(&query.plan, self, &query.prepared, params)?.0
                }
            }
        };
        let find = found.elapsed().as_nanos();
        let applied = std::time::Instant::now();
        match &*cached {
            Cached::Nothing => {}
            Cached::Ddl(sql) => {
                self.execute_ddl(sql)?;
            }
            // Rendered when it was compiled, so there is nothing to apply and
            // nothing to time. It is here to be exhaustive rather than to be
            // measured: a plan description is not a workload.
            Cached::QueryPlan(_) | Cached::Program(_) => {}
            // A module's own write, which this harness does not time: what it
            // costs is the module's business and not the engine's.
            Cached::VirtualDelete(..) | Cached::VirtualUpdate(..) => {}
            Cached::VirtualInsert(statement) => {
                self.insert_into_module(statement, params)?;
            }
            Cached::SchemaInsert(statement) => {
                self.insert_into_schema(statement, params)?;
            }
            Cached::Select(plan, prepared, _) => {
                physical::run_any_prepared(plan, self, prepared, params)?;
            }
            Cached::Insert(statement, ..) => {
                self.write(params, Vec::new(), |target, params| {
                    dml::insert(statement, target, params, &rows)
                })?;
            }
            Cached::Update(statement, _, _, setup) => {
                self.write(params, Vec::new(), |target, params| {
                    dml::update_cached(statement, target, params, &rows, setup)
                })?;
            }
            Cached::Delete(statement, ..) => {
                self.write(params, Vec::new(), |target, params| {
                    dml::delete(statement, target, params, &rows)
                })?;
            }
        }
        Ok((find, applied.elapsed().as_nanos()))
    }
}

impl ImportedDatabase {
    /// Returns the named parameters one statement declares, with their indexes.
    ///
    /// **A question about the text, answered by parsing it.** The compiled form
    /// does not carry the names - a plan is cached by its SQL and the values
    /// arrive later - and the caller that needs them is a *shell*, binding what
    /// a person typed into `.parameter set`. Parsing again costs one parse of
    /// one statement and keeps the name table out of every cached plan.
    ///
    /// @param sql - the statement text
    pub fn parameter_names(&self, sql: &str) -> DbResult<Vec<(Vec<u8>, u32)>> {
        let parsed = self.parse_once(sql)?;
        let names = parsed.parameters.names.clone();
        self.compiled.recycle(parsed);
        Ok(names)
    }

    /// Returns the highest parameter number a statement uses.
    ///
    /// What `sqlite3_bind_parameter_count` answers, and what a bind has to be
    /// checked against: an index above it is `SQLITE_RANGE` rather than a slot
    /// nobody will ever read.
    ///
    /// It is a parse rather than a lookup because the compiled plan does not
    /// carry the number - `Cached` has thirteen variants and none of them has a
    /// place to put it. The parse reuses the recycled arena, which is most of
    /// what a parse costs, and it happens once per `prepare` rather than once
    /// per execution: a statement prepared once and stepped a million times
    /// pays it once.
    ///
    /// @param sql - the statement text
    pub fn parameter_count(&self, sql: &str) -> DbResult<u32> {
        let parsed = self.parse_once(sql)?;
        let count = parsed.parameters.count;
        self.compiled.recycle(parsed);
        Ok(count)
    }

    /// Returns whether a compiled statement changes the database.
    ///
    /// A read takes a shared lock and a write an exclusive one, so the answer
    /// decides which. A directive is counted as a write: `CREATE TABLE` and
    /// `PRAGMA user_version = 1` both change the file, and the ones that do not
    /// pay a lock they did not need rather than skip one they did.
    pub(crate) fn writes_of(cached: &Cached) -> bool {
        !matches!(
            cached,
            Cached::Select(..) | Cached::QueryPlan(_) | Cached::Program(_) | Cached::Nothing
        )
    }
}

/// A statement compiled once and run many times.
///
/// Opaque on purpose: what is inside is the engine's business, and a caller that
/// could see it would be a caller that could be broken by a plan shape changing.
pub struct Statement {
    /// The compiled plan, replaced when the schema moves under it.
    ///
    /// A `RefCell` because [`ImportedDatabase::execute_statement`] takes the
    /// statement by shared reference and every caller holds it across many
    /// executions; the reprepare has to happen in place or the signature would
    /// have to change under all of them.
    pub(crate) cached: std::cell::RefCell<std::rc::Rc<Cached>>,
    /// The statement's text, so it can be compiled again.
    pub(crate) sql: String,
    /// The schema generation `cached` was compiled against.
    pub(crate) generation: std::cell::Cell<u64>,
}

/// What running one statement produced.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// The rows a `SELECT` answered, or the rows `RETURNING` named.
    pub rows: Vec<Vec<OwnedDatum>>,
    /// The result column names, for a `SELECT`.
    pub names: Vec<String>,
    /// What a write changed.
    pub changes: Changes,
}

/// Turns a parse or bind failure into a database error, keeping its kind.
///
/// **The message is exactly what it was**; what this adds is that a refusal the
/// binder marked `Unsupported` - "a construct the grammar has but this phase
/// does not implement" - arrives carrying that fact, where every conversion
/// site used to flatten it into an ordinary `SQLITE_MISUSE`.
///
/// It matters because this engine is deliberately incomplete, and a caller in
/// front of it has to tell "this engine cannot do that yet" from "you typed it
/// wrong" without matching on the wording of a sentence. `inillucent-driver`
/// is that caller; `ParseErrorKind::Refused` stays a plain misuse, because it
/// is the reference's own wording for a statement the schema will not have and
/// is not a gap in this engine.
///
/// @param error - the parser's or binder's failure
pub(crate) fn refused(
    error: inillucent_sql::diagnostic::ParseError,
) -> inillucent_base::error::DbError {
    // **The sentence goes in the message as well as the detail**, and that is a
    // fix rather than a flourish. `misuse` attaches what it is given as
    // *detail*, so every refusal this engine produced answered `message()` with
    // its primary code's own text - "bad parameter or other API misuse" - and
    // the sentence a person can act on was in the field `inillucent-base`
    // documents as never leaving the process. `inillucent-cli::shell::reason`
    // and `readgate::why` had each worked around it separately, which is what a
    // defect looks like when it has been met twice and fixed neither time.
    //
    // **The primary code comes from `error.code()`, not from `refusal`'s own
    // `SQLITE_MISUSE`.** `ParseError::code` already answers this correctly -
    // `PrimaryCode::Error` for an ordinary compile-time refusal, `TooBig` for
    // the one limit SQLite reports as a parse error - because a parse or bind
    // refusal is `SQLITE_ERROR` in SQLite, not `SQLITE_MISUSE`: `SELECT
    // nosuchcolumn FROM a`, `PRIMARY KEY missing on table x`, `ambiguous
    // column name: v`, `AUTOINCREMENT is only allowed on an INTEGER PRIMARY
    // KEY` and `RAISE() may only be used within a trigger-program` are every
    // one of them code 1 at the reference, measured through
    // `dml_differential.rs`. Routing them all through `refusal` here answered
    // 21 for every one of them - right message, wrong code - which is
    // invisible to a suite that only compares rows and text, and exactly what
    // `dml_differential`'s own primary-code assertions exist to catch.
    //
    // A parse or bind refusal is caller-safe by construction: it names tables,
    // columns and constructs, which are the caller's own words, and never a
    // path, a bound value or page bytes. The detail is left in place so that
    // everything reading it - the shell, the gate, the surface inventory -
    // sees exactly what it saw before.
    let mut built = inillucent_base::error::DbError::primary(error.code())
        .with_message(error.message())
        .with_detail(error.message());
    // **And the position, which used to be dropped here.** A refusal carries the
    // span of the token it is about, and the shell draws the reference's two
    // lines of caret art from it - so losing it here turned every parse failure
    // into a bare sentence where the reference points at the word. A refusal
    // that is deliberately positionless says so with a default span, which is
    // what `no_such_table` and the `ALTER TABLE` refusals use, and those stay
    // positionless because the reference points at nothing for them either.
    if error.span != inillucent_sql::lexer::Span::default() {
        built = built.with_sql_offset(error.offset());
    }
    match error.kind {
        inillucent_sql::diagnostic::ParseErrorKind::Unsupported(what) => {
            built.with_unsupported(what)
        }
        _ => built,
    }
}

/// Names the kind of statement a refusal is about.
///
/// @param statement - the bound statement
pub(crate) fn describe_statement(statement: &BoundStatement) -> &'static str {
    match statement {
        BoundStatement::Select(_) => "a query",
        BoundStatement::Insert(_) => "an insert",
        BoundStatement::Update(_) => "an update",
        BoundStatement::Delete(_) => "a delete",
        BoundStatement::Directive(_) => "a directive",
        BoundStatement::Empty => "nothing",
    }
}

/// Returns whether a folded name is one of the schema table's two spellings.
///
/// @param name - the table's name, folded
pub(crate) fn is_the_schema_table(name: &[u8]) -> bool {
    name == b"sqlite_schema" || name == b"sqlite_master"
}
