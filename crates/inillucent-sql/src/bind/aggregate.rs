//! How a call to an aggregate becomes a reference to one accumulator, and how
//! a call to a function the application registered is bound.
//!
//! Invariant: **every `BoundExpr::Aggregate` the binder makes comes from
//! [`Binder::aggregate_slot`], so the same aggregate written twice names one
//! slot and every reference carries its arguments' explicit collation.** The
//! reference holds no arguments, because they live in the binder's aggregate
//! list, and `max(s COLLATE NOCASE) = 'C'` compared with BINARY until the
//! reference carried the collation (task-2094).
//!
//! ## Why this is its own module
//!
//! The same reason [`super::cte`] and [`super::scratch`] are: `bind.rs` was at
//! the size `policy.rs` records for it and task-2094 added to it, and that
//! check asks for an extraction rather than a raised number. A registered
//! function is the other half of this module because a registered aggregate is
//! the second place a call becomes an accumulator. Nothing changed in the move.

use inillucent_value::Collation;

use super::collation::explicit_argument_collation;
use super::{refused, unsupported, Binder, BoundAggregate, BoundExpr};
use crate::ast::ExprId;
use crate::diagnostic::ParseError;
use crate::function::{self, AggregateFunc};
use crate::lexer::Span;

impl Binder<'_> {
    /// Binds a call to a function an application registered, if there is one.
    ///
    /// Registered functions are consulted before the built-ins, which is what
    /// makes `sqlite3_create_function("upper", 1, ...)` replace `upper` rather
    /// than collide with it - the same order SQLite resolves in.
    pub(super) fn bind_external_call(
        &mut self,
        folded: &[u8],
        arguments: &[ExprId],
        distinct: bool,
        span: Span,
    ) -> Result<Option<BoundExpr>, ParseError> {
        let Some(found) = function::lookup_external(self.externals, folded, arguments.len()) else {
            return Ok(None);
        };
        // **Where a schema is stopped from choosing what code runs
        // (task-1972).** The rule is `inillucent-sql`'s own, and
        // `Registry::authorize_function` reads the same one over the same
        // flags, so an application that asks the registry directly and a
        // statement the binder compiles get the same answer.
        if let Some(why) =
            function::schema_refusal(found.flags, self.call_site, self.trusted_schema)
        {
            return Err(refused(
                format!("{} {why}", String::from_utf8_lossy(folded)),
                span,
            ));
        }
        let aggregate = found.aggregate;
        if !aggregate {
            // `DISTINCT` means nothing to a function that sees one row, and
            // SQLite ignores it, so a registered scalar function ignores it too.
            let mut bound = Vec::with_capacity(arguments.len());
            for argument in arguments {
                bound.push(self.bind_expr(*argument)?);
            }
            return Ok(Some(BoundExpr::External {
                name: folded.to_vec(),
                arguments: bound,
            }));
        }
        if !self.allow_aggregates || self.inside_aggregate {
            return Err(unsupported("misuse of aggregate function", span));
        }
        self.inside_aggregate = true;
        let mut bound = Vec::with_capacity(arguments.len());
        for argument in arguments {
            bound.push(self.bind_expr(*argument)?);
        }
        self.inside_aggregate = false;
        let collation = bound
            .first()
            .and_then(BoundExpr::collation)
            .unwrap_or(Collation::Binary);
        let candidate = BoundAggregate {
            func: AggregateFunc::External,
            external: Some(folded.to_vec()),
            distinct,
            arguments: bound,
            star: false,
            collation,
            // A registered aggregate reaches this path without a `FILTER` or an
            // `ORDER BY`; both are read where a built-in is bound.
            filter: None,
            order_by: Vec::new(),
        };
        Ok(Some(self.aggregate_slot(candidate)))
    }

    /// Returns the reference to an aggregate's accumulator, adding the
    /// accumulator when the block does not have it yet.
    ///
    /// The same aggregate written twice is one accumulator. It is not only
    /// cheaper: `... ORDER BY count(*)` has to name the *same* slot the result
    /// column named, or the two are different values that happen to be spelt
    /// alike. The reference also carries the explicit collation of the
    /// arguments, which is the only part of them a comparison around the call
    /// reads (task-2094).
    ///
    /// @param candidate - the bound aggregate call
    pub(super) fn aggregate_slot(&mut self, candidate: BoundAggregate) -> BoundExpr {
        let collation = explicit_argument_collation(&candidate.arguments);
        let slot = match self
            .aggregates
            .iter()
            .position(|existing| existing == &candidate)
        {
            Some(slot) => slot,
            None => {
                self.aggregates.push(candidate);
                self.aggregates.len().saturating_sub(1)
            }
        };
        BoundExpr::Aggregate { slot, collation }
    }
}
