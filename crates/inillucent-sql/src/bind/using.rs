//! Join constraints: what `ON`, `USING` and `NATURAL` become, and what a
//! column that a `USING` join repeats resolves to.
//!
//! Invariant: **a `USING` column resolves the way SQLite's `lookupName` and
//! `selectExpander` resolve it.** Under an `INNER` or `LEFT` join it is the
//! left copy, under a `RIGHT` join the right copy, and under a `FULL` join
//! `coalesce()` of every copy, for an unqualified name and for `*` alike.
//!
//! ## Why this is its own module
//!
//! `bind.rs` was at its recorded ceiling, and resolving a `USING` column
//! under `RIGHT` and `FULL` joins added about a hundred and fifty lines to
//! it. The ratchet in `policy.rs` asks for an extraction rather than a raised
//! number, and every item here answers the one question of what a join's
//! constraint means. The items that already existed moved unchanged.

use super::*;

impl Binder<'_> {
    /// Turns `ON`, `USING` and `NATURAL` into ordinary predicates.
    ///
    /// The output-column rules survive the rewrite: a `USING` or `NATURAL`
    /// column is suppressed from the right-hand term's contribution to `*`,
    /// which is the only visible difference between a `USING` join and the
    /// equality predicate it means.
    ///
    /// The terms are addressed by their position in *this block's* FROM list,
    /// which the scope turns into the statement-wide source id. A parenthesised
    /// join has already flattened itself into the same list by the time this
    /// runs, so a position is always a real term.
    pub(crate) fn desugar_join_constraints(
        &mut self,
        terms: &[ast::FromTermId],
    ) -> Result<(), ParseError> {
        let base = self
            .scope()
            .len()
            .saturating_sub(terms.iter().map(|_| 1usize).sum::<usize>());
        for (offset, id) in terms.iter().enumerate() {
            let Some(term) = self.ast.from_term(*id) else {
                continue;
            };
            if matches!(term.source, FromSource::Join(_)) {
                // Its own constraints were desugared when it was flattened.
                continue;
            }
            let position = base.saturating_add(offset);
            let constraint = term.constraint.clone();
            let natural = term.natural;
            let span = term.span;
            if natural {
                let names = self.natural_columns(position);
                let predicate = self.equality_over(position, &names)?;
                self.set_constraint(position, predicate);
                continue;
            }
            match constraint {
                JoinConstraint::None => {}
                JoinConstraint::On(expr) => {
                    let bound = self.bind_expr(expr)?;
                    self.set_constraint(position, Some(bound));
                }
                JoinConstraint::Using(names) => {
                    let folded: Vec<Vec<u8>> = names
                        .iter()
                        .map(|name| self.ast.folded(*name).to_vec())
                        .collect();
                    for name in &folded {
                        if self.find_column_in(position, name).is_none() {
                            return Err(no_such_column(name, span));
                        }
                    }
                    let predicate = self.equality_over(position, &folded)?;
                    if predicate.is_none() {
                        return Err(unsupported("empty USING list", span));
                    }
                    self.set_constraint(position, predicate);
                }
            }
        }
        Ok(())
    }

    /// Stores a join constraint on a source of the current block.
    fn set_constraint(&mut self, position: usize, constraint: Option<BoundExpr>) {
        let Some(id) = self.scope_id(position) else {
            return;
        };
        if let Some(source) = self.sources.get_mut(id) {
            source.constraint = constraint;
        }
    }

    /// Returns the column names a NATURAL join equates: every name the right
    /// term shares with any term to its left in the same block.
    fn natural_columns(&self, position: usize) -> Vec<Vec<u8>> {
        let Some(right) = self.source_at(position) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for column in &right.table.columns {
            if column.hidden {
                continue;
            }
            let shared = (0..position).any(|earlier| {
                self.source_at(earlier)
                    .is_some_and(|left| left.table.column_position(&column.folded).is_some())
            });
            if shared {
                names.push(column.folded.clone());
            }
        }
        names
    }

    /// Returns one source of the current block by its position in the block.
    fn source_at(&self, position: usize) -> Option<&BoundSource> {
        let id = self.scope_id(position)?;
        self.sources.get(id)
    }

    /// Builds `left.name = right.name AND ...` for a USING or NATURAL join,
    /// and suppresses the right-hand columns from star expansion.
    fn equality_over(
        &mut self,
        position: usize,
        names: &[Vec<u8>],
    ) -> Result<Option<BoundExpr>, ParseError> {
        let mut predicate: Option<BoundExpr> = None;
        for name in names {
            let Some(left) = self.using_left_operand(position, name)? else {
                continue;
            };
            let Some((right_source, right_column)) = self.find_column_in(position, name) else {
                continue;
            };
            if let Some(id) = self.scope_id(position) {
                if let Some(source) = self.sources.get_mut(id) {
                    source.suppressed.push(right_column);
                }
            }
            let right = self.column_expr(right_source, right_column)?;
            let (affinity, collation) = comparison_rules(&left, &right);
            let equality = BoundExpr::Compare {
                op: BinaryOp::Equal,
                left: Box::new(left),
                right: Box::new(right),
                affinity,
                collation,
            };
            predicate = Some(match predicate {
                Some(existing) => BoundExpr::And(Box::new(existing), Box::new(equality)),
                None => equality,
            });
        }
        Ok(predicate)
    }

    /// Finds a column by folded name in one source, returning its source id.
    fn find_column_in(&self, position: usize, folded: &[u8]) -> Option<(usize, u16)> {
        let id = self.scope_id(position)?;
        let source = self.sources.get(id)?;
        source.table.column_position(folded).map(|c| (id, c))
    }

    /// Returns the left side of one `USING` equality, as SQLite builds it.
    ///
    /// SQLite equates the right term's column with the *leftmost* term that
    /// has the name, not the nearest one. The two differ in a chain of `LEFT`
    /// joins: `a LEFT JOIN b USING (k) LEFT JOIN c USING (k)` matches `c`
    /// against `a.k`, which is set on every row, where `b.k` is NULL on a row
    /// `b` did not match. When the block has a `RIGHT` or `FULL` join, any
    /// term on the left may be the one holding the value, so the operand is
    /// `coalesce()` over every left copy, and a copy that is not itself a
    /// `USING` column is ambiguous. This is `sqlite3ProcessJoin` in SQLite's
    /// `select.c`.
    ///
    /// @param position - the right term's position in the block
    /// @param folded - the column name, folded
    fn using_left_operand(
        &mut self,
        position: usize,
        folded: &[u8],
    ) -> Result<Option<BoundExpr>, ParseError> {
        let copies: Vec<(usize, u16)> = (0..position)
            .filter_map(|index| self.find_column_in(index, folded))
            .collect();
        let Some(&(first_source, first_column)) = copies.first() else {
            return Ok(None);
        };
        let outer_right = self.scope().iter().any(|id| {
            self.sources
                .get(*id)
                .is_some_and(|source| matches!(source.join, JoinKind::Right | JoinKind::Full))
        });
        if !outer_right || copies.len() == 1 {
            return self.column_expr(first_source, first_column).map(Some);
        }
        for &(source, column) in copies.iter().skip(1) {
            let joined = self
                .sources
                .get(source)
                .is_some_and(|held| held.suppressed.contains(&column));
            if !joined {
                return Err(refused(
                    format!(
                        "ambiguous reference to {} in USING()",
                        String::from_utf8_lossy(folded)
                    ),
                    Span::default(),
                ));
            }
        }
        self.coalesce_using_copies(&copies, Span::default())
            .map(Some)
    }

    /// Returns what `*` shows for one column, given the `USING` joins after it.
    ///
    /// SQLite expands a column that a later `USING` names as the bare name,
    /// so it resolves by the rule an unqualified reference follows: under a
    /// `RIGHT` join that is the right copy, under a `FULL` join `coalesce()`
    /// of every copy. Without those joins the answer is this column itself.
    ///
    /// @param scope - the block's source ids, in FROM order
    /// @param id - the source being expanded
    /// @param index - the column being expanded
    /// @param span - where the `*` is, for an error
    pub(super) fn star_using_column(
        &mut self,
        scope: &[usize],
        id: usize,
        index: u16,
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        let Some(folded) = self
            .sources
            .get(id)
            .and_then(|source| source.table.column(index))
            .map(|column| column.folded.clone())
        else {
            return self.column_expr(id, index);
        };
        let mut base: Option<(usize, u16)> = None;
        let mut found: Option<(usize, u16)> = None;
        let mut coalesced: Vec<(usize, u16)> = Vec::new();
        for candidate in scope {
            let Some(source) = self.sources.get(*candidate) else {
                continue;
            };
            let Some(position) = source.table.column_position(&folded) else {
                continue;
            };
            if source.suppressed.contains(&position) {
                step_using_match(
                    source.join,
                    (*candidate, position),
                    &mut found,
                    &mut coalesced,
                );
            } else if base.is_none() {
                base = Some((*candidate, position));
                found = base;
            }
        }
        // Only the leftmost copy is expanded as the bare name. A right copy
        // shows itself when `r.*` asks for it, and so does a column no `USING`
        // names.
        if base != Some((id, index)) {
            return self.column_expr(id, index);
        }
        if coalesced.len() > 1 {
            return self.coalesce_using_copies(&coalesced, span);
        }
        match found {
            Some((source, column)) => self.column_expr(source, column),
            None => self.column_expr(id, index),
        }
    }

    /// Returns `coalesce()` over the copies of one `USING` column.
    ///
    /// @param copies - each copy's source id and column, leftmost first
    /// @param span - where the reference is, for an error
    pub(super) fn coalesce_using_copies(
        &mut self,
        copies: &[(usize, u16)],
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        let Some(func) = function::lookup_scalar(b"coalesce") else {
            return Err(no_such_function(b"coalesce", span));
        };
        let mut arguments = Vec::with_capacity(copies.len());
        for &(source, column) in copies {
            arguments.push(self.authorized_column(source, column, span)?);
        }
        let collation = arguments
            .first()
            .and_then(BoundExpr::collation)
            .unwrap_or(Collation::Binary);
        Ok(BoundExpr::Function {
            func,
            arguments,
            collation,
        })
    }
}

/// Applies SQLite's rule for an unqualified name that a `USING` join repeats.
///
/// Called for each right copy of the name, in FROM order, after the leftmost
/// copy has set `found`. An `INNER` or `LEFT` join keeps the left copy, since
/// the left side is set on every row it produces. A `RIGHT` join makes the
/// right copy the answer, since only it is set on every row. A `FULL` join
/// can leave either side NULL, so the answer is `coalesce()` of every copy,
/// collected in `coalesced`. This is `lookupName` in SQLite's `resolve.c`.
///
/// @param join - the join that attaches the right copy's term
/// @param copy - the right copy's source id and column
/// @param found - the copy the name resolves to so far
/// @param coalesced - the copies a `FULL` join has collected, or empty
pub(super) fn step_using_match(
    join: JoinKind,
    copy: (usize, u16),
    found: &mut Option<(usize, u16)>,
    coalesced: &mut Vec<(usize, u16)>,
) {
    match join {
        JoinKind::Right => {
            coalesced.clear();
            *found = Some(copy);
        }
        JoinKind::Full => {
            if coalesced.is_empty() {
                if let Some(previous) = *found {
                    coalesced.push(previous);
                }
            }
            coalesced.push(copy);
            *found = Some(copy);
        }
        JoinKind::Left | JoinKind::Inner | JoinKind::Comma | JoinKind::Cross => {}
    }
}
