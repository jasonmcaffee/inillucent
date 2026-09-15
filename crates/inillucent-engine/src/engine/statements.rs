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
            BoundStatement::Select(select) => {
                Ok(plan_select_with(*select, self.session_state.levers))
            }
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
            .with_limits(&self.session_state.limits)
            .with_foreign_keys(
                self.session_state.foreign_keys,
                self.session_state.defer_foreign_keys,
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
            Cached::VirtualUpdate(..) => Ok(vec!["an update of a module".to_string()]),
            Cached::VirtualDelete(..) => Ok(vec!["a delete from a module".to_string()]),
            Cached::Update(..) => Ok(vec!["an update".to_string()]),
            Cached::Delete(..) => Ok(vec!["a delete".to_string()]),
        }
    }
}
