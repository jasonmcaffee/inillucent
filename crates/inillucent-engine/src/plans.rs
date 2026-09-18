//! The plan cache: what a compiled statement is held under, and when it is
//! reused rather than compiled again.
//!
//! Invariant: **a plan is only reused when nothing about the connection could
//! make it answer differently.** Two things can: an authorizer that is entitled
//! to refuse a statement has to be asked about this execution rather than the
//! first one, and a planner lever changes what the right plan *is*. The first is
//! a reason not to cache at all; the second is folded into the key, so the same
//! text under two lever settings is two entries rather than one the second
//! setting silently inherits.
//!
//! ## Why this is its own module
//!
//! It came out of `lib.rs` in task-1886, which added
//! [`ImportedDatabase::compiled_statement_count`] and found the crate root at
//! its recorded ceiling. The five functions here are one idea and they were the
//! cohesive thing to lift: everything below is about *whether* to compile.
//! `ImportedDatabase::compile`, which is about *how*, stayed behind with the
//! parser and binder plumbing it is written in terms of.
//!
//! ## The two counts, which are not the same count
//!
//! [`ImportedDatabase::cached_plan_count`] is how many plans are held and
//! [`ImportedDatabase::compiled_statement_count`] is how many were built. A
//! cache that holds one entry and rebuilds it on every prepare has the size the
//! first reports and none of the behaviour it is trusted for, so
//! `crates/inillucent/tests/budget.rs` asserts on the second - and asserts on it
//! as a count, because the wall-clock ratio it used to assert on was decided by
//! whatever else the machine was running.

use std::collections::HashMap;

use inillucent_base::DbResult;
use inillucent_exec::dml::Changes;
use inillucent_exec::physical::{self, Params};
use inillucent_sql::plan::{Levers, PhysicalPlan};
use inillucent_tree::datum::OwnedDatum;

use crate::{ImportedDatabase, Outcome};

/// How many compiled statements one session holds before the cache is emptied.
///
/// **A thousand, and the number is a ceiling rather than a working set.** An
/// application re-issues a handful of statements; a query builder or a reporting
/// tool issues generated SQL and never repeats a string, and before this ticket
/// each one of those was compiled once and kept for the life of the process
/// (task-1932, M1). A thousand is far past the first case and far short of
/// unbounded.
pub const DEFAULT_STATEMENT_CACHE: usize = 1_000;

/// One statement, compiled as far as it can be before its parameters arrive.
///
/// A `SELECT` is a plan and the structural choice over it. A write is the bound
/// statement plus, for an `UPDATE` or a `DELETE`, the plan that finds the rows
/// it will change - which is an ordinary query and is prepared like one, so
/// `WHERE id = ?1` reaches the same point probe on the second execution as on
/// the first.
pub(crate) enum Cached {
    /// Text that carries no statement at all.
    ///
    /// A `-- comment` after the last `;`, an empty string, whitespace. Running
    /// it produces no rows and changes nothing, which is what SQLite does with
    /// the same text.
    Nothing,
    /// A statement the session carries out itself, held as its own text.
    ///
    /// Re-bound on every execution, because binding a `DROP TABLE` resolves
    /// whether the table is there and the answer changes when it runs.
    Ddl(String),
    /// `EXPLAIN QUERY PLAN`, rendered when the statement was compiled.
    ///
    /// The lines describe the plan and the plan depends on the schema, so this
    /// is cached and invalidated exactly like the query it describes - which is
    /// the point of holding it here rather than rendering it per execution.
    QueryPlan(Vec<String>),
    /// A plain `EXPLAIN`, rendered when the statement was compiled.
    ///
    /// One entry per step: the opcode's name, its first two operands, its
    /// argument, and the comment. See `program_of` for what those mean here.
    Program(Vec<(String, i64, i64, String, String)>),
    /// An insert into a virtual table, which the module applies.
    VirtualInsert(Box<inillucent_sql::dml::BoundInsert>),
    /// An insert into `sqlite_schema` under `PRAGMA writable_schema`.
    ///
    /// It writes a catalog row through the same `record` every `CREATE` uses,
    /// rather than through the ordinary insert path: the catalog tree has four
    /// columns `sqlite_schema` does not declare, and a row written without them
    /// names no tree. See `insert_into_schema` for what a dump needs it for.
    SchemaInsert(Box<inillucent_sql::dml::BoundInsert>),
    /// A delete from a virtual table, with the query that finds its rowids.
    ///
    /// A module owns its storage, so the only handle on one of its rows is the
    /// rowid it answers with: the plan asks which rowids match and the module
    /// is told about each. That is what SQLite does, and the reason `xUpdate`
    /// takes a rowid rather than a predicate.
    VirtualDelete(Box<inillucent_sql::dml::BoundDelete>, CachedQuery),
    /// An update of a virtual table, with the query that finds its rowids.
    ///
    /// The same shape as [`Cached::VirtualDelete`] and for the same reason: a
    /// module owns its storage, so the only handle on one of its rows is the
    /// rowid it answers with.
    VirtualUpdate(Box<inillucent_sql::dml::BoundUpdate>, CachedQuery),
    /// A query, with a slot for a compiled chain reused across executions.
    ///
    /// The slot is a `RefCell` beside the plan, the same reason
    /// `dml::UpdateCache` sits inside `Cached::Update`: `cached` here is a
    /// shared `&Rc<Cached>`, and interior mutability is what lets one
    /// execution build the chain and a later one, through the same `Rc`,
    /// find it already there.
    Select(
        Box<PhysicalPlan>,
        Box<physical::Prepared>,
        std::cell::RefCell<physical::Slot>,
    ),
    /// An insert, with the query for its `SELECT` source when it has one.
    ///
    /// The flag says whether a `VALUES` list holds a subquery. It is decided
    /// once, here, because the alternative is walking the value expressions on
    /// every execution of every insert - and `BoundExpr::children` allocates a
    /// vector per node, which is the cost this project already measured on the
    /// read path at about 0.07 us per execution.
    Insert(
        Box<inillucent_sql::dml::BoundInsert>,
        Option<CachedQuery>,
        bool,
    ),
    /// An update, with the query that finds the rows it changes.
    ///
    /// The flag says whether an assignment holds a subquery, for the reason
    /// above.
    Update(
        Box<inillucent_sql::dml::BoundUpdate>,
        CachedQuery,
        bool,
        /// Everything the statement builds before it looks at a row, kept
        /// between executions. See `dml::UpdateSetup`: it was more than half of
        /// what `txn.large` cost, and none of it depends on the row.
        crate::dml::UpdateCache,
    ),
    /// A delete, with the query that finds the rows it removes.
    Delete(Box<inillucent_sql::dml::BoundDelete>, CachedQuery),
}

impl ImportedDatabase {
    /// Returns how many compiled statements this database is holding.
    ///
    /// **Nothing could ask before this (task-1932, M1).** A cache with no
    /// accessor and no bound is a cache nobody can say anything about: the
    /// question "is this connection holding a plan per generated statement" had
    /// no answer that did not involve a debugger.
    ///
    /// Counted across every session, because the cache is keyed by session and
    /// a caller asking how much it is holding means all of it.
    pub fn cached_statements(&self) -> usize {
        self.compiled.held()
    }

    /// Returns the ceiling one session's cache is emptied at.
    pub fn statement_cache_limit(&self) -> usize {
        self.compiled.limit()
    }

    /// Returns what one run-time limit is set to on this connection.
    ///
    /// @param limit - which limit
    pub fn limit(&self, limit: inillucent_base::limits::Limit) -> i64 {
        self.pragmas.limits().borrow().get(limit)
    }

    /// Sets one run-time limit, and returns what it was before.
    ///
    /// **The register was unreachable until task-1946's H3.** `limits` was set
    /// to `Limits::default()` when a connection opened and never touched again,
    /// so `.limit trigger_depth 10` printed 1000 and changed nothing, and the
    /// binder's own trigger cap could only ever be the default. The clamping is
    /// `Limits::set`'s: a request above the manifest's `hard_max` is clamped to
    /// it and one below `minimum` is refused, which is how `sqlite3_limit`
    /// behaves.
    ///
    /// @param limit - which limit
    /// @param requested - the value asked for
    /// @returns the value that was in force before this call
    pub fn set_limit(&mut self, limit: inillucent_base::limits::Limit, requested: i64) -> i64 {
        self.pragmas.limits().borrow_mut().set(limit, requested)
    }

    /// Sets the ceiling one session's cache is emptied at.
    ///
    /// Zero means every statement is compiled fresh, which is what a caller
    /// diagnosing a plan wants and what nothing else should ask for.
    ///
    /// @param most - how many compiled statements one session may hold
    pub fn set_statement_cache_limit(&self, most: usize) {
        self.compiled.set_limit(most);
    }

    /// Forgets every compiled statement.
    ///
    /// The same thing a schema change does, said out loud. A caller that has
    /// just issued a hundred thousand generated statements and wants the memory
    /// back has no other way to ask for it.
    pub fn clear_statement_cache(&self) {
        self.compiled.statements.borrow_mut().clear();
    }

    /// Returns the key a compiled statement is held under.
    ///
    /// The lever mask and the session, packed. Two connections' plans are kept
    /// apart because a temporary table makes the same text mean two different
    /// tables; two lever settings' plans are kept apart because a plan built
    /// with the covering-index rule on is that rule's answer.
    fn plan_key(&self) -> u64 {
        (self.session_state.session.get() << 32) | u64::from(self.pragmas.levers().disabled())
    }

    /// Returns how many statements are compiled and held.
    ///
    /// The plan cache's size, which is what a test asserting that a
    /// registration invalidated it asks about.
    pub fn cached_plan_count(&self) -> usize {
        self.compiled
            .statements
            .borrow()
            .values()
            .map(HashMap::len)
            .sum()
    }

    /// Returns how many statements this connection has compiled since it opened.
    ///
    /// The cache's size says how many plans are *held*; this says how many were
    /// *built*. They answer different questions, and the second is the one a
    /// guard on the plan cache needs: a cache that holds one entry and rebuilds
    /// it on every prepare has the size the first number reports and none of
    /// the behaviour it is being trusted for.
    pub fn compiled_statement_count(&self) -> u64 {
        self.compiled.compiles()
    }

    /// Reports whether a compiled plan may be reused.
    ///
    /// Only when nothing can refuse a statement: an authorizer that could
    /// answer differently this time has to be asked this time.
    fn cacheable(&self) -> bool {
        match &self.session_state.authorizer {
            Some(held) => held.allows_everything(),
            None => true,
        }
    }

    /// Returns one statement compiled, from the cache or by compiling it.
    ///
    /// Everything that does not depend on the bound parameters happens here and
    /// happens once: the parse, the bind, the plan and the structural choice.
    /// What is left per execution is the parameters and the work.
    ///
    /// @param sql - the statement text
    pub(crate) fn compiled(&self, sql: &str) -> DbResult<std::rc::Rc<Cached>> {
        // **An authorizer that can refuse is asked every time.** A cached plan
        // is a plan whose authorizer already said yes once, and reusing it
        // would skip the callback on every later execution - so a connection
        // with a real authorizer compiles per statement, which is what SQLite
        // does for the same reason.
        if !self.cacheable() {
            return Ok(std::rc::Rc::new(self.compile(sql)?));
        }
        if !self.pragmas.levers().has(Levers::PLAN_CACHE) {
            // The lever is off, so nothing is held and every execution
            // compiles. It exists so a measurement can price the compile.
            return Ok(std::rc::Rc::new(self.compile(sql)?));
        }
        // **Keyed by the levers as well as the text, and nested rather than
        // paired.** A plan built with the covering-index rule on is that rule's
        // answer, so the same SQL under two settings is two entries rather than
        // one the second setting silently inherits - which is what makes an A/B
        // measurement of a lever trustworthy on a connection that has already
        // run the other arm.
        //
        // The nesting is what keeps the *hit* free. A `(String, u32)` key has
        // to be built before the map can be asked, so every lookup allocated a
        // copy of the SQL - about 90 ns on a 1,163 ns compile, and paid again
        // on every execution of an already-cached statement, which is the one
        // path a plan cache exists to make cheap.
        // **And by the session**, because `SELECT * FROM t` binds to a
        // different table in a connection that has shadowed `t` with a `TEMP`
        // one. Packed into one `u64` so the lookup stays a single hash of the
        // SQL text: a paired key would have to be built before the map could be
        // asked, which is an allocation on the one path a plan cache exists to
        // make free.
        let key = self.plan_key();
        let held = self.compiled.statements.borrow();
        if let Some(found) = held.get(&key).and_then(|under| under.get(sql)) {
            return Ok(std::rc::Rc::clone(found));
        }
        drop(held);
        let compiled = std::rc::Rc::new(self.compile(sql)?);
        // **The cache has a ceiling (task-1932, M1).** It used to be cleared
        // only by a schema change or a function registration, and keyed by the
        // statement's text - so a long-lived connection issuing generated SQL,
        // which is what an application with a query builder or a reporting tool
        // issues, grew one compiled plan per distinct string for the life of the
        // process. Nothing measured it and nothing bounded it.
        //
        // Emptying the whole session's entries rather than evicting the least
        // recently used one, because the cost of getting it wrong is a
        // recompile and the cost of tracking recency is a write on every *hit* -
        // which is the one path this cache exists to keep free. A ceiling of a
        // thousand statements is far past what an application re-issues and far
        // short of what an unbounded cache reaches.
        //
        // A ceiling of zero caches nothing at all, which is what a caller
        // diagnosing a plan asks for: every statement is compiled fresh.
        let ceiling = self.compiled.statement_cache_limit.get();
        if ceiling > 0 {
            let mut held = self.compiled.statements.borrow_mut();
            let under = held.entry(key).or_default();
            if under.len() >= ceiling {
                under.clear();
            }
            under.insert(sql.to_string(), std::rc::Rc::clone(&compiled));
        }
        Ok(compiled)
    }

    /// Runs a `SELECT` through its compiled chain when one is available,
    /// falling back to a fresh build otherwise.
    ///
    /// A thin wrapper over [`ImportedDatabase::run_cached_query`], which does
    /// the actual slot dispatch and is shared with the write path - an
    /// `UPDATE`/`DELETE`'s keys query, an `INSERT ... SELECT`'s source, and a
    /// virtual table write's rowid query all go through the same function.
    /// What only a `SELECT` needs is the column names, which come straight
    /// off the plan rather than off whichever arm answered: `Shape::names` is
    /// never anything but `plan.select.columns`'s own names, copied at build
    /// time, so reading them from the plan directly is one fewer thing the
    /// reused arm and the fresh-build arm could disagree about.
    ///
    /// @param plan - the planner's output
    /// @param prepared - the structural choice `prepare` made
    /// @param slot - this statement's compiled-chain cache
    /// @param params - the bound parameters
    pub(crate) fn execute_select_cached(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
        slot: &std::cell::RefCell<physical::Slot>,
        params: &Params,
    ) -> DbResult<Outcome> {
        let rows = self.run_cached_query(plan, prepared, slot, params)?;
        Ok(Outcome {
            rows,
            names: column_names(plan),
            changes: Changes::default(),
        })
    }

    /// Runs a plan through its compiled chain when one is available, falling
    /// back to a fresh, uncached build otherwise.
    ///
    /// **The one place every cached plan - read or write - decides whether to
    /// reuse.** [`ImportedDatabase::execute_select_cached`] calls this for a
    /// `SELECT`; [`ImportedDatabase::keys_of`] calls it for an `UPDATE` or
    /// `DELETE`'s keys query; `apply_compiled` calls it directly for an
    /// `INSERT ... SELECT`'s source and for `VirtualUpdate`/`VirtualDelete`'s
    /// rowid query. One function deciding *whether* to reuse is what keeps
    /// the write path from growing a second opinion about the question
    /// `physical::Slot` already answers.
    ///
    /// **The slot is tried once and remembered - see [`physical::Slot`].**
    /// [`physical::Slot::Untried`] attempts [`physical::try_compile`] and
    /// stores whatever it decided, `Reusable` or `Never`, so every later
    /// execution of the same query answers instantly without asking the
    /// builder again. A slot already borrowed - the same statement
    /// re-entering its own chain, through a registered function, a trigger,
    /// or (on the write side) a statement whose own `WHERE` reads the table a
    /// trigger it fires also writes - runs a fresh, uncached build for that
    /// one call rather than panicking or refusing: `RefCell::try_borrow_mut`
    /// is exactly the tool for "reusable, except while it is already in use".
    ///
    /// **Returns before the caller's write ever takes `&mut self`.** This
    /// method takes `&self`, borrows the slot, runs the chain, and returns an
    /// owned `Vec` - nothing about the slot or the chain is still borrowed
    /// once it returns, which is what lets [`ImportedDatabase::keys_of`] be
    /// called before `self.write` needs `&mut self`.
    ///
    /// @param plan - the planner's output
    /// @param prepared - the structural choice `prepare` made
    /// @param slot - this query's compiled-chain cache
    /// @param params - the bound parameters
    pub(crate) fn run_cached_query(
        &self,
        plan: &PhysicalPlan,
        prepared: &physical::Prepared,
        slot: &std::cell::RefCell<physical::Slot>,
        params: &Params,
    ) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let Ok(mut held) = slot.try_borrow_mut() else {
            return Ok(physical::run_any_prepared(plan, self, prepared, params)?.0);
        };
        match &mut *held {
            physical::Slot::Reusable(compiled) => {
                compiled.run(plan, self, params)?;
                return Ok(compiled.take_rows());
            }
            physical::Slot::Never => {}
            // `try_compile` returns `Some` whenever it actually built
            // something, whether or not that build turns out to be
            // reusable - see its own doc comment for why. So this always
            // runs the build it was handed, exactly once, and only *then*
            // decides whether to keep it: a build that read a parameter it
            // should not have already paid for whatever reading it cost
            // (evaluating a deterministic function, folding a subquery), and
            // asking `run_any_prepared` to build it again would pay that
            // cost a second time for the same first execution.
            physical::Slot::Untried => match physical::try_compile(plan, self, prepared, params)? {
                Some(mut compiled) => {
                    compiled.run(plan, self, params)?;
                    let rows = compiled.take_rows();
                    *held = if compiled.rebindable() {
                        physical::Slot::Reusable(Box::new(compiled))
                    } else {
                        physical::Slot::Never
                    };
                    return Ok(rows);
                }
                None => *held = physical::Slot::Never,
            },
        }
        drop(held);
        Ok(physical::run_any_prepared(plan, self, prepared, params)?.0)
    }
}

/// A plan and structural choice compiled once and reused through a slot - the
/// write path's equivalent of `Cached::Select`, for whichever rows a write
/// has to read first: the keys an `UPDATE`/`DELETE` touches, the rowids
/// `VirtualUpdate`/`VirtualDelete` hand a module, or the rows an `INSERT ...
/// SELECT` reads. Replaces the bare `(Box<PhysicalPlan>, Box<Prepared>)` pair
/// every one of those used to carry, which had no way to remember that a
/// `Compiled` chain had already been tried.
///
/// Lives beside [`ImportedDatabase::run_cached_query`] rather than in
/// `lib.rs`, where the rest of `Cached`'s variants are declared: the slot
/// this carries is exactly what that method reads and writes, and `lib.rs`
/// was at its recorded ceiling when the write path gained one of these per
/// statement kind.
pub(crate) struct CachedQuery {
    /// The planner's output.
    pub(crate) plan: Box<PhysicalPlan>,
    /// The structural choice `prepare` made.
    pub(crate) prepared: Box<physical::Prepared>,
    /// This query's compiled-chain cache. A `RefCell` for the same reason
    /// `Cached::Select`'s is: `cached` is a shared `&Rc<Cached>`, and interior
    /// mutability is what lets one execution build the chain and a later one,
    /// through the same `Rc`, find it already there.
    pub(crate) slot: std::cell::RefCell<physical::Slot>,
}

impl CachedQuery {
    /// Returns a query with an untried slot.
    ///
    /// @param plan - the planner's output
    /// @param prepared - the structural choice `prepare` made
    pub(crate) fn new(plan: PhysicalPlan, prepared: physical::Prepared) -> CachedQuery {
        CachedQuery {
            plan: Box::new(plan),
            prepared: Box::new(prepared),
            slot: std::cell::RefCell::new(physical::Slot::default()),
        }
    }
}

/// Returns a plan's result column names, decoded from UTF-8 lossily.
///
/// @param plan - the planner's output
fn column_names(plan: &PhysicalPlan) -> Vec<String> {
    plan.select
        .columns
        .iter()
        .map(|column| String::from_utf8_lossy(&column.name).into_owned())
        .collect()
}
