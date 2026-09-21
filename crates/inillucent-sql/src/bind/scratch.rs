//! The binder's working vectors: the set it borrows from the connection, and
//! the subset it saves while a nested block is bound.
//!
//! Invariant: **a vector in here is scratch, never part of an answer.** What
//! [`Binder::bind_statement`] returns owns its own vectors and leaves with
//! them; everything this module moves about is state the binder keeps while it
//! works and has no use for afterwards. That is what makes it safe to hand back
//! to the connection and fill again.
//!
//! ## Why this is its own module
//!
//! The same reason [`super::cte`] is: `bind.rs` was at the size `policy.rs`
//! records for it and task-2026 added to it, and that check asks for an
//! extraction rather than a raised number. The two halves here are one
//! question - which of the binder's vectors are saved, and who they are given
//! back to - asked at two scales.
//!
//! [`BinderScratch`] is the outer one: the vectors a *connection* keeps
//! between statements, so a compile pushes into capacity the last compile
//! took. [`BlockFrame`] is the inner one: the vectors a *query block* borrows
//! from the block around it, so an aggregate written inside a subquery is
//! finalised at its own level rather than the enclosing one. Nothing moved
//! changed in the move.

use super::{
    Binder, BoundAggregate, BoundExpr, BoundSource, BoundWindow, CteBinding, RecursiveTarget,
};
use crate::ast;

/// The vectors a binder works in, kept by the connection rather than made for
/// every statement.
///
/// **The parse arena's counterpart, for the stage after the parse
/// (task-2026).** `Compiled::scratch_ast` exists because every vector in an
/// `Ast` is empty at construction and grows on its first push, so a parse that
/// is thrown away a microsecond later pays the allocator for capacity it
/// already had last time. The binder has exactly that shape and was not getting
/// that treatment: binding `SELECT 1` took the scope stack's buffer and the
/// result-alias buffer - 96 and 320 bytes - out of the allocator on every
/// compile, and `prepare.trivial` is a workload the gate compiles on every
/// iteration.
///
/// What is here is the binder's own working state. What the binder *returns* is
/// not: a `BoundStatement`'s vectors leave with it and belong to whoever asked
/// for the bind, so recycling them would mean handing back memory something
/// else is still reading.
///
/// Like the arena, it is cleared on the way in rather than on the way out, and
/// it keeps whatever capacity the largest statement so far needed - a
/// connection that once bound a statement with ten thousand FROM terms holds
/// that much `sources` until it closes. That is the trade [`crate::ast::Ast`]
/// already makes, made once more here rather than differently.
#[derive(Default)]
pub struct BinderScratch {
    sources: Vec<BoundSource>,
    scopes: Vec<Vec<usize>>,
    aggregates: Vec<BoundAggregate>,
    result_aliases: Vec<(Vec<u8>, BoundExpr)>,
    schemas: Vec<(usize, u32)>,
    ctes: Vec<Vec<CteBinding>>,
    recursing: Vec<RecursiveTarget>,
    binding_ctes: Vec<ast::SelectId>,
    correlations: Vec<usize>,
    windows: Vec<BoundWindow>,
    named_windows: Vec<(Vec<u8>, ast::WindowId)>,
    firing: Vec<Vec<u8>>,
    firing_foreign_keys: Vec<Vec<u8>>,
    pending_constraints: Vec<BoundExpr>,
}

impl BinderScratch {
    /// Returns a scratch with nothing in it and nothing allocated.
    pub fn new() -> BinderScratch {
        BinderScratch::default()
    }

    /// Empties every vector, keeping the memory each has already taken.
    ///
    /// A `clear` rather than a `new` for the reason [`crate::ast::Ast::clear`]
    /// gives: the capacity is the point, and dropping it would leave a scratch
    /// that costs an allocation to refill.
    fn clear(&mut self) {
        self.sources.clear();
        self.scopes.clear();
        self.aggregates.clear();
        self.result_aliases.clear();
        self.schemas.clear();
        self.ctes.clear();
        self.recursing.clear();
        self.binding_ctes.clear();
        self.correlations.clear();
        self.windows.clear();
        self.named_windows.clear();
        self.firing.clear();
        self.firing_foreign_keys.clear();
        self.pending_constraints.clear();
    }
}

/// The per-block binder state saved while a nested block is bound.
///
/// Aggregates, result aliases and the correlation list all belong to one query
/// block. Without a frame, an aggregate written inside a subquery would be
/// added to the enclosing block's accumulator list and finalised at the wrong
/// level - which is a wrong answer rather than an error.
pub(super) struct BlockFrame {
    windows: Vec<BoundWindow>,
    named_windows: Vec<(Vec<u8>, ast::WindowId)>,
    aggregates: Vec<BoundAggregate>,
    result_aliases: Vec<(Vec<u8>, BoundExpr)>,
    allow_aggregates: bool,
    inside_aggregate: bool,
    correlations: Vec<usize>,
    tail_may_name_an_alias: bool,
}

impl<'a> Binder<'a> {
    /// Binds into vectors the caller keeps, rather than into fresh ones.
    ///
    /// **For a caller that binds one statement after another**, which is every
    /// connection: the scratch is cleared on the way in, so the second bind
    /// pushes into capacity the first one took. See [`BinderScratch`] for what
    /// is in it and what is deliberately not.
    ///
    /// @param scratch - the vectors to fill, cleared first
    pub fn with_scratch(mut self, mut scratch: BinderScratch) -> Binder<'a> {
        scratch.clear();
        self.sources = scratch.sources;
        self.scopes = scratch.scopes;
        self.aggregates = scratch.aggregates;
        self.result_aliases = scratch.result_aliases;
        self.dependencies.schemas = scratch.schemas;
        self.ctes = scratch.ctes;
        self.recursing = scratch.recursing;
        self.binding_ctes = scratch.binding_ctes;
        self.correlations = scratch.correlations;
        self.windows = scratch.windows;
        self.named_windows = scratch.named_windows;
        self.firing = scratch.firing;
        self.firing_foreign_keys = scratch.firing_foreign_keys;
        self.pending_constraints = scratch.pending_constraints;
        self
    }

    /// Returns the vectors this bind filled, for the next bind to reuse.
    ///
    /// The binder is consumed, so nothing can still be reading what is handed
    /// back. The bound statement is not in here - it was returned by
    /// [`Binder::bind_statement`] and owns its own vectors.
    pub fn into_scratch(self) -> BinderScratch {
        BinderScratch {
            sources: self.sources,
            scopes: self.scopes,
            aggregates: self.aggregates,
            result_aliases: self.result_aliases,
            schemas: self.dependencies.schemas,
            ctes: self.ctes,
            recursing: self.recursing,
            binding_ctes: self.binding_ctes,
            correlations: self.correlations,
            windows: self.windows,
            named_windows: self.named_windows,
            firing: self.firing,
            firing_foreign_keys: self.firing_foreign_keys,
            pending_constraints: self.pending_constraints,
        }
    }

    /// Opens a query block: a fresh scope, and fresh per-block state.
    pub(super) fn enter_block(&mut self) -> BlockFrame {
        self.scopes.push(Vec::new());
        BlockFrame {
            windows: core::mem::take(&mut self.windows),
            named_windows: core::mem::take(&mut self.named_windows),
            aggregates: core::mem::take(&mut self.aggregates),
            result_aliases: core::mem::take(&mut self.result_aliases),
            allow_aggregates: core::mem::replace(&mut self.allow_aggregates, false),
            inside_aggregate: core::mem::replace(&mut self.inside_aggregate, false),
            correlations: core::mem::take(&mut self.correlations),
            tail_may_name_an_alias: self.tail_may_name_an_alias,
        }
    }

    /// Closes a query block, returning the FROM terms it owned.
    ///
    /// A correlation the closing block recorded is passed outward when the
    /// block that is now innermost does not own the term either, which is what
    /// makes correlation transitive through two levels of nesting.
    pub(super) fn leave_block(&mut self, frame: BlockFrame) -> Vec<usize> {
        let ids = self.scopes.pop().unwrap_or_default();
        let inner = core::mem::replace(&mut self.correlations, frame.correlations);
        for id in inner {
            if ids.contains(&id) {
                continue;
            }
            let owned = self.scopes.last().is_some_and(|scope| scope.contains(&id));
            if !owned && !self.correlations.contains(&id) {
                self.correlations.push(id);
            }
        }
        self.windows = frame.windows;
        self.named_windows = frame.named_windows;
        self.aggregates = frame.aggregates;
        self.result_aliases = frame.result_aliases;
        self.allow_aggregates = frame.allow_aggregates;
        self.inside_aggregate = frame.inside_aggregate;
        self.tail_may_name_an_alias = frame.tail_may_name_an_alias;
        ids
    }
}
