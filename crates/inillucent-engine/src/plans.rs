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
use inillucent_sql::plan::Levers;

use crate::{Cached, ImportedDatabase};

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
}
