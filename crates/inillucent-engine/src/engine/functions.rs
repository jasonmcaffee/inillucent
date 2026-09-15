//! The function registry, the collations, the authorizer and the levers.
//!
//! Invariant: **every one of these empties the plan cache.** A plan compiled
//! under one authorizer is that authorizer's answer and reusing it would skip
//! the callback the caller installed it to receive; a plan compiled under one
//! lever is that lever's answer and re-running it would measure the old choice
//! while reporting the new one.

use inillucent_base::DbResult;
use inillucent_sql::plan::Levers;

impl crate::ImportedDatabase {
    /// Returns what the binder needs to know about the registered functions.
    ///
    /// The name, the arity and whether it reduces a group - and nothing else.
    /// A binder that held the *body* would be a bound tree that depends on who
    /// was holding it, which is why the machinery looks the body up when it
    /// runs rather than carrying it.
    pub(crate) fn external_functions(&self) -> Vec<inillucent_sql::function::ExternalFunction> {
        self.session_state
            .registry
            .functions()
            .iter()
            .map(|held| inillucent_sql::function::ExternalFunction {
                name: held.name.to_ascii_lowercase().into_bytes(),
                arity: held.arity,
                aggregate: held.is_aggregate(),
            })
            .collect()
    }

    /// Registers a scalar an application defined, replacing one of the same
    /// name and arity.
    ///
    /// @param name - the name SQL calls it by
    /// @param arity - how many arguments it takes, or -1 for any number
    /// @param flags - what the function promises about itself
    /// @param body - what it does
    pub fn create_scalar_function(
        &mut self,
        name: &str,
        arity: i32,
        flags: inillucent_ext::registry::FunctionFlags,
        body: inillucent_ext::registry::ScalarBody,
    ) -> DbResult<()> {
        self.register_function(inillucent_ext::registry::UserFunction {
            name: name.to_string(),
            arity,
            flags,
            body: inillucent_ext::registry::UserBody::Scalar(body),
        })
    }

    /// Registers an aggregate an application defined.
    ///
    /// @param name - the name SQL calls it by
    /// @param arity - how many arguments it takes, or -1 for any number
    /// @param flags - what the function promises about itself
    /// @param body - what it does with a whole group
    pub fn create_aggregate_function(
        &mut self,
        name: &str,
        arity: i32,
        flags: inillucent_ext::registry::FunctionFlags,
        body: inillucent_ext::registry::AggregateBody,
    ) -> DbResult<()> {
        self.register_function(inillucent_ext::registry::UserFunction {
            name: name.to_string(),
            arity,
            flags,
            body: inillucent_ext::registry::UserBody::Aggregate(body),
        })
    }

    /// Puts one function into the registry and forgets the compiled statements.
    ///
    /// **The cache has to go.** Which function a name resolves to is decided
    /// when a statement is bound - a registration can shadow a built-in - so a
    /// statement compiled before the registration would keep calling the
    /// built-in, and one compiled before a *removal* would keep calling code
    /// the application has taken back.
    ///
    /// @param function - the registration
    fn register_function(
        &mut self,
        function: inillucent_ext::registry::UserFunction,
    ) -> DbResult<()> {
        self.session_state.registry.register_function(function);
        self.forget_compiled_statements();
        Ok(())
    }

    /// Removes a function by name and arity, reporting whether one went.
    ///
    /// @param name - the name it was registered under
    /// @param arity - the arity it was registered for
    pub fn remove_function(&mut self, name: &str, arity: i32) -> bool {
        let removed = self.session_state.registry.unregister_function(name, arity);
        if removed {
            self.forget_compiled_statements();
        }
        removed
    }

    /// Registers a collating sequence an application defined.
    ///
    /// **The comparator is process-wide and the name is not.** A `Collation` is
    /// a `Copy` handle carried through every key and every comparison, so the
    /// body lives in `inillucent-value`'s table; what this connection holds is
    /// the name it resolves to that handle by.
    ///
    /// @param name - the name `COLLATE` calls it by
    /// @param comparator - how it orders two values
    pub fn create_collation(
        &mut self,
        name: &str,
        comparator: inillucent_value::collation::Comparator,
    ) -> DbResult<()> {
        let collation = inillucent_value::collation::register_custom(name, comparator);
        let folded = name.to_ascii_uppercase();
        self.session_state
            .collations
            .retain(|(existing, _)| *existing != folded);
        self.session_state.collations.push((folded, collation));
        // A comparison compiled under BINARY would keep comparing under BINARY.
        self.forget_compiled_statements();
        Ok(())
    }

    /// Puts the connection into or out of defensive mode.
    ///
    /// @param on - whether the flag is in force
    pub fn set_defensive(&mut self, on: bool) {
        self.pragmas.defensive.set(on);
    }

    /// Installs the authorizer every later statement is bound under.
    ///
    /// **The plan cache is emptied with it**, for the same reason it is emptied
    /// when a lever changes: a plan compiled under one authorizer is that
    /// authorizer's answer, and reusing it would skip the callback the caller
    /// installed the authorizer to receive.
    ///
    /// @param authorizer - the callback, or nothing to allow everything again
    pub fn set_authorizer(
        &mut self,
        authorizer: Option<std::rc::Rc<dyn inillucent_sql::bind::Authorizer>>,
    ) {
        self.session_state.authorizer = authorizer;
        self.compiled.statements.borrow_mut().clear();
    }

    /// Sets exactly which planner optimizations are off for this connection.
    ///
    /// **An absolute mask, not an accumulating one** - this mirrors SQLite's own
    /// `SQLITE_TESTCTRL_OPTIMIZATIONS`, which assigns the disabled set rather
    /// than folding a new one into whatever was there. A caller measuring a
    /// lever moves back and forth between two masks on the same connection -
    /// `disable_optimizations(SOME_LEVER)` and then `disable_optimizations(0)`
    /// to return to "everything on" - and an OR-based mask cannot answer that
    /// second call: once any lever had ever been disabled, `mask = 0` folded in
    /// nothing new and left it disabled, so a connection could turn levers off
    /// but never back on. Three of the optimisation-arm tests in
    /// `crates/inillucent-compat/tests/levers.rs` reuse one connection across a
    /// `for` loop of statements for exactly this reason, and every statement
    /// after the first ran its "with" arm under the previous statement's
    /// "without" arm's mask - so the two arms silently compared the same plan
    /// against itself from the second statement onward. `plan_cache.rs`'s
    /// `a_lever_change_is_cached_separately` already asserts the corrected
    /// contract: a `disable_optimizations(0)` after a lever was disabled has to
    /// land back on the plan compiled before it, not stay on the disabled one.
    ///
    /// **Every compiled statement goes with it.** A plan built under a lever is
    /// that lever's answer, and re-running it after the lever changed would
    /// measure the old choice while reporting the new one - which is the whole
    /// thing a lever exists to compare.
    ///
    /// **It takes `Levers`, the type this workspace already has for exactly
    /// this (task-1961, A10).** It used to take a bare `u32`, so a caller had
    /// to know that the number meant "disabled" and not "enabled", and nothing
    /// stopped a lever constant from one build being passed to another. The
    /// bare mask is now built once, by [`Levers::without`], where the meaning
    /// is written down.
    ///
    /// @param levers - exactly the configuration this connection should have
    pub fn disable_optimizations(&mut self, levers: Levers) {
        // **The cache is keyed by the levers rather than cleared by them.** A
        // plan built under a lever is that lever's answer, so the same SQL under
        // two settings is two entries; clearing would make the second arm's
        // first execution pay a compile the first arm's did not, and that
        // difference is the size of the thing such a measurement looks for.
        self.pragmas.levers.set(levers);
    }

    /// Returns which planner optimizations this connection has on.
    pub fn levers(&self) -> Levers {
        self.pragmas.levers.get()
    }
}
