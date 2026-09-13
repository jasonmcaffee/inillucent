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

use crate::{Cached, ImportedDatabase, Outcome};

impl ImportedDatabase {
    /// Returns the key a compiled statement is held under.
    ///
    /// The lever mask and the session, packed. Two connections' plans are kept
    /// apart because a temporary table makes the same text mean two different
    /// tables; two lever settings' plans are kept apart because a plan built
    /// with the covering-index rule on is that rule's answer.
    fn plan_key(&self) -> u64 {
        (self.session.get() << 32) | u64::from(self.levers.disabled())
    }

    /// Returns how many statements are compiled and held.
    ///
    /// The plan cache's size, which is what a test asserting that a
    /// registration invalidated it asks about.
    pub fn cached_plan_count(&self) -> usize {
        self.statements.borrow().values().map(HashMap::len).sum()
    }

    /// Returns how many statements this connection has compiled since it opened.
    ///
    /// The cache's size says how many plans are *held*; this says how many were
    /// *built*. They answer different questions, and the second is the one a
    /// guard on the plan cache needs: a cache that holds one entry and rebuilds
    /// it on every prepare has the size the first number reports and none of
    /// the behaviour it is being trusted for.
    pub fn compiled_statement_count(&self) -> u64 {
        self.compiles.get()
    }

    /// Reports whether a compiled plan may be reused.
    ///
    /// Only when nothing can refuse a statement: an authorizer that could
    /// answer differently this time has to be asked this time.
    fn cacheable(&self) -> bool {
        match &self.authorizer {
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
        if !self.levers.has(Levers::PLAN_CACHE) {
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
        let held = self.statements.borrow();
        if let Some(found) = held.get(&key).and_then(|under| under.get(sql)) {
            return Ok(std::rc::Rc::clone(found));
        }
        drop(held);
        let compiled = std::rc::Rc::new(self.compile(sql)?);
        self.statements
            .borrow_mut()
            .entry(key)
            .or_default()
            .insert(sql.to_string(), std::rc::Rc::clone(&compiled));
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
