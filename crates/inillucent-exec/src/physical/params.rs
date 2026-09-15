//! The bound values a statement runs with, and what reading one costs.
//!
//! Invariant: **a parameter is read through the set rather than copied out of
//! it.** A plan holds an index and the set holds the value, so the same
//! compiled statement runs against a second set of values without being
//! rebuilt - which is the whole of why `prepare` and `execute` are separate
//! calls.

// `literal_value` is named by path from a dozen call sites in
// `inillucent-engine`, so it stays reachable here after the move to `constant`.
// `Compiled`, `Slot` and `try_compile` moved to `crate::compiled` to keep this
// file under its recorded ceiling; re-exported here so every existing
// `physical::Slot` / `physical::Compiled` / `physical::try_compile` reference
// - `inillucent-engine`'s `Cached::Select` among them - did not have to move
// with them.
use inillucent_tree::datum::OwnedDatum;

/// A catalog that also answers one recursive CTE's queue.
///
/// Everything else is delegated, so the step arm sees exactly the trees, the
/// layouts and the modules the statement sees. Wrapping rather than threading a
/// parameter through every builder is what keeps a recursive query from
/// changing the shape of a signature nothing else uses.
use super::*;

/// The values bound to `?1`, `?2`, ... for one execution.
///
/// **`Clone` is written out rather than derived, and the reason is the cell.**
/// `Correlation::answer` clones the statement's parameters and writes an outer
/// row's columns into slots above the declared count, once per row. A derived
/// `Clone` would share the `Rc`, so those writes would land in the *outer*
/// statement's bindings and every row of the outer query would see the last
/// inner row's values. A copied set gets a cell of its own.
#[derive(Debug, Default)]
pub struct Params {
    values: Bindings,
    /// How many parameters the statement has, once it has been compiled.
    ///
    /// `None` until the binder says, because a caller may bind before the
    /// statement exists. See [`Params::try_set`].
    declared: Option<u32>,
    /// How many times a parameter has been read out of this set.
    ///
    /// The counter is what makes [`Statement`] safe. A statement may only be
    /// re-run against new parameters if nothing but its *source* looked at the
    /// old ones - a `LIMIT ?1`, a projected `?2` or a residual filter over a
    /// parameter is baked into the operator chain when the chain is built, and
    /// re-running that chain against different values would answer the previous
    /// question with the new question's parameters.
    ///
    /// Deciding that by inspecting the plan means a second, separate opinion
    /// about which constructs can carry a parameter, which is exactly the kind
    /// of duplicated judgement that goes stale when a construct is added.
    /// Counting the reads asks the builder instead: every path that consumes a
    /// parameter goes through [`Params::get`], so if the count does not move
    /// while everything except the source is built, nothing except the source
    /// read one.
    reads: std::cell::Cell<u64>,
    /// What each uncorrelated subquery in this statement answered.
    ///
    /// Indexed by the statement-wide number the binder gave the subquery, and
    /// empty when the statement has none. It rides here rather than in the plan
    /// because a folded subquery is true only of the data it was read from, and
    /// plans are cached by their text: a value baked into the plan would answer
    /// `SELECT (SELECT count(*) FROM t)` with the count from whenever the
    /// statement was first compiled.
    ///
    /// An entry is `None` when the subquery is correlated, which is the one
    /// case that has no single value. The physical pass refuses those by name.
    subqueries: Vec<Option<crate::subquery::Subvalue>>,
    /// What the connection's counters said when this statement began.
    ///
    /// `changes()`, `total_changes()`, `last_insert_rowid()` and the seed the
    /// random built-ins draw from. They ride here for the same reason a folded
    /// subquery does: a plan is cached by its text, so a value baked into the
    /// plan would answer `SELECT changes()` with the count from whenever the
    /// statement was first compiled.
    ///
    /// They are constants for the length of one statement, which is SQLite's
    /// own rule - the counters move when a statement *finishes* - so reading
    /// them once here is not an approximation.
    ///
    /// A `Cell` because the engine fills it on a `Params` the caller owns and
    /// lends: an application binds its values and hands over `&Params`, and the
    /// connection state is not the application's to supply.
    context: std::cell::Cell<crate::scalar::Context>,
    /// Whether a trigger's own writes fire triggers.
    ///
    /// `PRAGMA recursive_triggers`. It rides here rather than being compiled in
    /// because it is read where a body statement is *run* - see
    /// `crate::trigger::run_body` - and not where one is translated.
    recursive_triggers: std::cell::Cell<bool>,
}
/// Scrambles a seed into the next one.
///
/// `splitmix64`, which is the function library's own scrambler, so adjacent
/// seeds give unrelated streams rather than correlated ones.
///
/// @param seed - the seed to move on from
fn split_mix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
impl Clone for Params {
    fn clone(&self) -> Params {
        Params {
            values: Bindings::new(std::sync::Mutex::new(self.held().clone())),
            declared: self.declared,
            reads: std::cell::Cell::new(self.reads.get()),
            context: self.context.clone(),
            recursive_triggers: self.recursive_triggers.clone(),
            subqueries: self.subqueries.clone(),
        }
    }
}
impl Params {
    /// Returns an empty parameter set.
    pub fn new() -> Params {
        Params {
            values: Bindings::default(),
            declared: None,
            reads: std::cell::Cell::new(0),
            context: std::cell::Cell::new(crate::scalar::Context::default()),
            recursive_triggers: std::cell::Cell::new(false),
            subqueries: Vec::new(),
        }
    }

    /// Returns a parameter set over a list of values, `?1` first.
    ///
    /// @param values - the values, in parameter order
    pub fn from_values(values: Vec<OwnedDatum>) -> Params {
        Params {
            values: Bindings::new(std::sync::Mutex::new(values)),
            declared: None,
            reads: std::cell::Cell::new(0),
            context: std::cell::Cell::new(crate::scalar::Context::default()),
            recursive_triggers: std::cell::Cell::new(false),
            subqueries: Vec::new(),
        }
    }

    /// Returns this set with room for a statement's folded subqueries.
    ///
    /// The bound values are carried over, because a subquery's own block may
    /// read `?1` and has to see the same binding the outer statement did.
    ///
    /// @param subqueries - one slot per subquery, by the binder's numbering
    pub fn with_subqueries(&self, subqueries: Vec<Option<crate::subquery::Subvalue>>) -> Params {
        Params {
            values: Bindings::new(std::sync::Mutex::new(self.held().clone())),
            declared: self.declared,
            reads: std::cell::Cell::new(self.reads.get()),
            context: self.context.clone(),
            recursive_triggers: self.recursive_triggers.clone(),
            subqueries,
        }
    }

    /// Returns this set with the folded-subquery table emptied.
    ///
    /// **For a statement run from inside another one's row.** A correlated
    /// block is a statement of its own and folds its own uncorrelated
    /// subqueries; carrying the outer table in would tell it the fold had
    /// already happened - `has_subqueries` is how `crate::subquery::fold`
    /// decides - and leave its slots unfilled, which reads from inside
    /// `translate` as "a correlated subquery": a true sentence about the slot
    /// and a false one about the query.
    ///
    /// The bound values are kept, because a nested block may read `?1` and has
    /// to see the same binding the outer statement did.
    pub fn without_subqueries(&self) -> Params {
        Params {
            values: Bindings::new(std::sync::Mutex::new(self.held().clone())),
            declared: self.declared,
            reads: std::cell::Cell::new(self.reads.get()),
            context: self.context.clone(),
            recursive_triggers: self.recursive_triggers.clone(),
            subqueries: Vec::new(),
        }
    }

    /// Records what one subquery answered.
    ///
    /// @param id - the binder's number for the subquery
    /// @param value - the rows it produced, read as one column
    pub fn set_subquery(&mut self, id: usize, value: crate::subquery::Subvalue) {
        if let Some(slot) = self.subqueries.get_mut(id) {
            *slot = Some(value);
        }
    }

    /// Returns whether this execution's subqueries have been folded already.
    pub fn has_subqueries(&self) -> bool {
        !self.subqueries.is_empty()
    }

    /// Returns what one subquery answered, or `None` when it is correlated.
    ///
    /// Counted as a parameter read for the same reason a `?1` is: a chain that
    /// baked this value in must not be re-run against a later state of the
    /// table, and the read counter is what already decides that.
    ///
    /// @param id - the binder's number for the subquery
    pub fn subquery(&self, id: usize) -> Option<&crate::subquery::Subvalue> {
        self.reads.set(self.reads.get().saturating_add(1));
        self.subqueries.get(id).and_then(|slot| slot.as_ref())
    }

    /// Returns a copy of the bound values, `?1` first.
    pub fn values(&self) -> Vec<OwnedDatum> {
        self.held()
    }

    /// Returns how many parameter reads this set has answered.
    pub fn reads(&self) -> u64 {
        self.reads.get()
    }

    /// Tells this set what the connection's counters say.
    ///
    /// Called once per statement, by the engine, before anything is compiled.
    ///
    /// @param context - the counters and the statement's random seed
    pub fn set_context(&self, context: crate::scalar::Context) {
        self.context.set(context);
    }

    /// Tells this set whether a trigger's own writes fire triggers.
    ///
    /// @param recursive - what `PRAGMA recursive_triggers` is set to
    pub fn set_recursive_triggers(&self, recursive: bool) {
        self.recursive_triggers.set(recursive);
    }

    /// Returns whether a trigger's own writes fire triggers.
    ///
    /// Not counted as a parameter read: nothing is compiled from it, so a chain
    /// built while it was one way is not stale when it is the other.
    pub fn recursive_triggers(&self) -> bool {
        self.recursive_triggers.get()
    }

    /// Returns what the connection's counters said.
    ///
    /// Counted as a parameter read for the same reason a `?1` is: a chain that
    /// baked these in must not be re-run against a later state of the
    /// connection, and the read counter is what already decides that.
    ///
    /// **The seed moves on every read, so two call sites in one statement do
    /// not share a stream.** `SELECT random(), random()` is two nodes, each
    /// with its own stream advanced per row; started from the same number they
    /// would answer the same pair, which is what SQLite does not do. The
    /// counters themselves are unchanged by the read - every `changes()` in one
    /// statement is the same number.
    pub fn context(&self) -> crate::scalar::Context {
        self.reads.set(self.reads.get().saturating_add(1));
        let held = self.context.get();
        self.context.set(crate::scalar::Context {
            seed: split_mix(held.seed),
            ..held
        });
        held
    }

    /// Replaces every bound value, reusing the buffer.
    ///
    /// A benchmark that re-binds a prepared statement per iteration should not
    /// allocate to do it - `sqlite3_bind_int64` does not - and building a fresh
    /// `Params` per execution was one `Vec` per execution on the arm being
    /// timed.
    ///
    /// @param values - the new values, `?1` first
    pub fn refill(&mut self, values: impl IntoIterator<Item = OwnedDatum>) {
        let Ok(mut held) = self.values.lock() else {
            return;
        };
        held.clear();
        held.extend(values);
    }

    /// Returns the value bound to a parameter.
    ///
    /// An unbound parameter is NULL, which is what SQLite does.
    ///
    /// **Zero is NULL rather than the first parameter (task-1962, T3).** The
    /// subtraction below saturates, so `get(0)` used to read `?1`'s value - the
    /// wrong parameter's, silently. The binder numbers from one and
    /// [`Params::try_set`] refuses zero, so nothing reaches this today; the
    /// guard is what keeps it an answer rather than a neighbour's value if
    /// something ever does.
    ///
    /// @param index - the one-based parameter number
    pub fn get(&self, index: u32) -> OwnedDatum {
        self.reads.set(self.reads.get().saturating_add(1));
        if index == 0 {
            return OwnedDatum::Null;
        }
        self.held()
            .get(index.saturating_sub(1) as usize)
            .cloned()
            .unwrap_or(OwnedDatum::Null)
    }

    /// Returns the cell an `Expr::Parameter` reads when it is evaluated.
    ///
    /// **Not counted as a read.** Handing over the cell is the opposite of
    /// folding a value into the chain: the chain that holds this answers
    /// whatever is in it at the moment it runs, which is the property
    /// [`Statement::rebindable`] exists to establish.
    pub fn bindings(&self) -> Bindings {
        std::sync::Arc::clone(&self.values)
    }

    /// Returns a copy of the bound values, for the paths that need them all.
    ///
    /// A lock that cannot be taken reads as no values bound, which is what an
    /// unbound set is - a poisoned mutex here would otherwise turn a parameter
    /// read into a panic on a path that is not allowed to have one.
    fn held(&self) -> Vec<OwnedDatum> {
        self.values
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    /// Records that the chain being built folded in a value that is only true
    /// of this execution.
    ///
    /// **`now` is the one that made this necessary.** Every `now` in one
    /// statement is the same instant, which is SQLite's rule, so `translate`
    /// reads the clock once and puts the reading in the node - and a chain kept
    /// across executions would then answer `datetime('now')` with the instant it
    /// was built. Counting it as a read is what makes
    /// [`Statement::rebindable`] refuse to reuse such a chain, using the
    /// mechanism already there for a folded subquery and a folded `changes()`.
    pub fn note_execution_constant(&self) {
        self.reads.set(self.reads.get().saturating_add(1));
    }

    /// Binds one parameter by its one-based number.
    ///
    /// Parameters between the highest bound so far and this one become NULL,
    /// which is what an unbound parameter already is - so binding `?3` before
    /// `?1` leaves `?1` NULL rather than shifting it.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value to bind
    /// Binds one parameter with no range check, for the engine's own use.
    ///
    /// **Deliberately unchecked, and not what a caller's `bind` goes through.**
    /// The binder allocates parameter slots of its own above the ones the SQL
    /// wrote - `correlate::Correlation::answer` feeds an outer row's columns
    /// into a correlated block through exactly this method, at numbers past the
    /// statement's declared count. Routing this through the checked form made
    /// those writes vanish and every correlated `EXISTS` answered against an
    /// unbound slot, which is a wrong answer rather than an error.
    ///
    /// [`Params::try_set`] is the caller-facing one, and the split is the
    /// point: the engine may write any slot it invented, and an application may
    /// only write the ones its statement declared.
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    pub fn set(&mut self, index: u32, value: OwnedDatum) {
        let at = index.saturating_sub(1) as usize;
        let Ok(mut held) = self.values.lock() else {
            return;
        };
        if held.len() <= at {
            held.resize(at.saturating_add(1), OwnedDatum::Null);
        }
        if let Some(slot) = held.get_mut(at) {
            *slot = value;
        }
    }

    /// Binds one parameter, reporting an index the statement does not have.
    ///
    /// **`index` is one-based, and zero is out of range rather than the first
    /// slot.** This used to be `index.saturating_sub(1)` into a vector that was
    /// resized to fit whatever it was given, which made every index legal: a
    /// bind of 9 on a one-parameter statement grew the set to nine slots and
    /// answered `Ok`, and a bind of **0 silently wrote over `?1`** - so a caller
    /// who believed index 0 was a no-op had replaced its first parameter and
    /// had nothing in the result to say so. SQLite answers `SQLITE_RANGE` to
    /// both, and now so does this.
    ///
    /// The set still grows, because it has to: a caller binds `?1` before the
    /// statement it belongs to has been compiled, so at bind time the number of
    /// parameters may not be known. What it will not do is grow past the count
    /// once one has been declared with [`Params::expect`].
    ///
    /// @param index - the one-based parameter number
    /// @param value - the value
    ///
    /// **The error type is `()` on purpose.** There is exactly one way this
    /// fails - the index is zero or past a declared count - and the caller
    /// turns it into the engine's own refusal with the parameter number it
    /// already has. An error type carrying that number would be the same number
    /// twice.
    #[allow(clippy::result_unit_err)]
    pub fn try_set(&mut self, index: u32, value: OwnedDatum) -> Result<(), ()> {
        if index == 0 {
            return Err(());
        }
        if let Some(declared) = self.declared {
            if index > declared {
                return Err(());
            }
        }
        let at = (index - 1) as usize;
        let Ok(mut held) = self.values.lock() else {
            return Err(());
        };
        if held.len() <= at {
            held.resize(at.saturating_add(1), OwnedDatum::Null);
        }
        if let Some(slot) = held.get_mut(at) {
            *slot = value;
        }
        Ok(())
    }

    /// Tells this set how many parameters the statement it belongs to has.
    ///
    /// Called once the statement is compiled and the binder knows the answer.
    /// Until then the set has no upper bound to check against and only index
    /// zero is refused.
    ///
    /// @param count - the highest parameter number the statement uses
    pub fn expect(&mut self, count: u32) {
        self.declared = Some(count);
    }

    /// Returns how many parameters the statement was said to have.
    pub fn declared(&self) -> Option<u32> {
        self.declared
    }

    /// Unbinds every parameter.
    pub fn clear(&mut self) {
        if let Ok(mut held) = self.values.lock() {
            held.clear();
        }
    }

    /// Returns how many parameters are bound.
    pub fn len(&self) -> usize {
        self.values.lock().map(|held| held.len()).unwrap_or(0)
    }

    /// Reports whether nothing is bound.
    pub fn is_empty(&self) -> bool {
        self.values
            .lock()
            .map(|held| held.is_empty())
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unbound parameter reads as NULL rather than as an error.
    ///
    /// **SQLite's own rule, and the reason it is not a refusal (T3,
    /// task-1962).** `SELECT ?1` with nothing bound answers NULL, and a
    /// statement prepared before its parameters are bound is the ordinary case
    /// rather than a mistake.
    #[test]
    fn an_unbound_parameter_reads_as_null() {
        let params = Params::new();
        assert_eq!(params.get(1), OwnedDatum::Null);
        assert_eq!(params.get(9), OwnedDatum::Null);
    }

    /// Parameters are numbered from one, and `?0` does not exist.
    #[test]
    fn the_first_parameter_is_one() {
        let params = Params::from_values(vec![OwnedDatum::Int(11), OwnedDatum::Int(22)]);
        assert_eq!(params.get(1), OwnedDatum::Int(11));
        assert_eq!(params.get(2), OwnedDatum::Int(22));
        assert_eq!(
            params.get(0),
            OwnedDatum::Null,
            "there is no `?0`, and asking for one is not a panic"
        );
    }

    /// Every read is counted, and handing over the cell is not a read.
    ///
    /// **The counter is what makes a prepared statement safe to re-run.** A
    /// `LIMIT ?1`, a projected `?2` or a residual filter over a parameter is
    /// baked into the operator chain when the chain is built, so a chain may
    /// only be re-run against new values if nothing except its source looked at
    /// the old ones. Counting the reads asks the builder rather than forming a
    /// second opinion about which constructs can carry a parameter.
    #[test]
    fn every_read_is_counted() {
        let params = Params::from_values(vec![OwnedDatum::Int(1)]);
        assert_eq!(params.reads(), 0, "nothing has read one yet");
        let _ = params.get(1);
        let _ = params.get(1);
        assert_eq!(params.reads(), 2);
        let _ = params.bindings();
        assert_eq!(
            params.reads(),
            2,
            "handing the cell to a compiled expression is the opposite of \
             reading the value now, so it is not counted"
        );
    }

    /// A binding past what the statement declared is refused.
    ///
    /// A caller binding `?3` on a statement with two parameters has made a
    /// mistake, and accepting it silently would leave the value somewhere
    /// nothing reads.
    #[test]
    fn a_binding_past_the_declaration_is_refused() {
        let mut params = Params::new();
        assert!(params.try_set(1, OwnedDatum::Int(1)).is_ok());
        assert!(
            params.try_set(0, OwnedDatum::Int(1)).is_err(),
            "there is no `?0` to bind"
        );
        params.expect(2);
        assert_eq!(params.declared(), Some(2));
        assert!(params.try_set(2, OwnedDatum::Int(2)).is_ok());
        assert!(
            params.try_set(3, OwnedDatum::Int(3)).is_err(),
            "the statement declared two parameters"
        );
    }

    /// Clearing unbinds every parameter, which is `sqlite3_clear_bindings`.
    #[test]
    fn clearing_unbinds_everything() {
        let mut params = Params::from_values(vec![OwnedDatum::Int(1), OwnedDatum::Int(2)]);
        assert_eq!(params.get(2), OwnedDatum::Int(2));
        params.clear();
        assert_eq!(params.get(1), OwnedDatum::Null);
        assert_eq!(params.get(2), OwnedDatum::Null);
    }
}
