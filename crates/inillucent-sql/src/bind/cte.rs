//! Common table expressions: what a `WITH` binds, and how a recursive one is
//! filled.
//!
//! Invariant: **a CTE is bound once per reference and never bound inside
//! itself.** Two references to one CTE are two independent scans with their
//! own FROM-term numbers, which is why a binding holds an AST id rather than a
//! bound block; and a definition already being bound is a cycle, which is
//! answered rather than followed.
//!
//! ## Why this is its own module
//!
//! `bind.rs` was at its recorded ceiling and task-1913 added ninety-nine lines
//! to it, so the ratchet in `policy.rs` asked for an extraction rather than a
//! raised number. This is one question - what a name in a `WITH` stands for -
//! and the ten items here were the only ones asking it. Nothing moved changed
//! in the move.

use super::{subquery_table, unsupported, Binder, BoundSource, RecursiveBody, SourceRows};
use crate::ast::{self, CompoundOp, JoinKind, SelectId};
use crate::catalog_view::TableInfo;
use crate::diagnostic::{ParseError, ParseErrorKind};
use crate::lexer::Span;

/// One common table expression visible to a block.
///
/// The definition is kept as an AST id rather than a bound block because two
/// references to the same CTE are two independent scans: each gets its own
/// FROM-term numbers and its own materialisation. Binding once and cloning
/// would give both references the same source ids, and the second scan would
/// then read the first one's cursors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CteBinding {
    /// The folded name a FROM term matches against.
    pub folded: Vec<u8>,
    /// The name as written, which the expansion is aliased to.
    pub name: Vec<u8>,
    /// The explicit column list, when the `WITH` wrote one.
    pub columns: Vec<Vec<u8>>,
    /// The query the name stands for.
    pub select: SelectId,
    /// Whether the `WITH` said `RECURSIVE`.
    pub recursive: bool,
}

/// One recursive CTE whose definition is being bound.
#[derive(Clone, Debug)]
pub(super) struct RecursiveTarget {
    /// The CTE's folded name.
    pub(super) folded: Vec<u8>,
    /// The statement-wide number of the FROM term that will hold its store.
    id: usize,
    /// The columns a reference to it exposes, taken from the seed arm.
    table: TableInfo,
    /// Whether any arm bound so far referred to it.
    referenced: bool,
}

impl Binder<'_> {
    /// Pushes the CTEs of a `WITH` prefix, returning whether it pushed any.
    pub(crate) fn push_ctes(&mut self, with: &ast::With) -> Result<bool, ParseError> {
        if with.ctes.is_empty() {
            return Ok(false);
        }
        let mut bindings = Vec::with_capacity(with.ctes.len());
        for cte in &with.ctes {
            bindings.push(CteBinding {
                folded: self.ast.folded(cte.name).to_vec(),
                name: self.ast.text(cte.name).to_vec(),
                columns: cte
                    .columns
                    .iter()
                    .map(|name| self.ast.text(*name).to_vec())
                    .collect(),
                select: cte.select,
                recursive: with.recursive,
            });
        }
        self.ctes.push(bindings);
        Ok(true)
    }

    /// Drops the innermost level of CTE bindings.
    pub(crate) fn pop_ctes(&mut self) {
        self.ctes.pop();
    }

    /// Returns the innermost CTE a folded name matches.
    pub(super) fn find_cte(&self, folded: &[u8]) -> Option<CteBinding> {
        for level in self.ctes.iter().rev() {
            if let Some(found) = level.iter().find(|cte| cte.folded == folded) {
                return Some(found.clone());
            }
        }
        None
    }

    /// Reports whether a CTE's own query names it in a FROM clause.
    ///
    /// **What makes a CTE recursive is the self-reference, not the keyword.**
    /// SQLite accepts `WITH c AS (SELECT 1 UNION ALL SELECT ... FROM c)` with
    /// no `RECURSIVE` written and answers it; this binder read only the
    /// keyword, so the same query bound `c`'s definition inside `c`'s
    /// definition until the process ran out of stack (task-1913).
    ///
    /// An inner `WITH` that binds the same name shadows the outer one, so
    /// nothing under it can be the recursion - which is why this stops there
    /// rather than reporting every mention of the name.
    ///
    /// @param select - the CTE's query
    /// @param folded - the CTE's folded name
    pub(super) fn select_names_itself(&self, select: ast::SelectId, folded: &[u8]) -> bool {
        let Some(query) = self.ast.select(select) else {
            return false;
        };
        if query
            .with
            .ctes
            .iter()
            .any(|inner| self.ast.folded(inner.name) == folded)
        {
            return false;
        }
        if self.core_names_cte(query.first, folded) {
            return true;
        }
        query
            .compounds
            .iter()
            .any(|(_, arm)| self.core_names_cte(*arm, folded))
    }

    /// Reports whether one arm of a compound names a CTE in its FROM clause.
    ///
    /// @param core - the arm
    /// @param folded - the CTE's folded name
    pub(super) fn core_names_cte(&self, core: ast::SelectCoreId, folded: &[u8]) -> bool {
        let Some(arm) = self.ast.core(core) else {
            return false;
        };
        let ast::SelectBody::Select { from, .. } = &arm.body else {
            return false;
        };
        self.terms_name_cte(from, folded)
    }

    /// Reports whether any FROM term names a CTE.
    ///
    /// @param terms - the FROM terms
    /// @param folded - the CTE's folded name
    pub(super) fn terms_name_cte(&self, terms: &[ast::FromTermId], folded: &[u8]) -> bool {
        terms.iter().any(|id| match self.ast.from_term(*id) {
            Some(term) => match &term.source {
                ast::FromSource::Table { database, name, .. } => {
                    database.is_none() && self.ast.folded(*name) == folded
                }
                ast::FromSource::Subquery(select) => self.select_names_itself(*select, folded),
                ast::FromSource::Join(inner) => self.terms_name_cte(inner, folded),
            },
            None => false,
        })
    }

    /// Registers a reference to the recursive CTE currently being bound.
    pub(super) fn push_recursive_self(
        &mut self,
        position: usize,
        alias: Option<ast::NameId>,
        join: JoinKind,
    ) -> Result<(), ParseError> {
        let Some(target) = self.recursing.get_mut(position) else {
            return Err(unsupported("unknown recursive reference", Span::default()));
        };
        target.referenced = true;
        let cte = target.id;
        let table = target.table.clone();
        let alias = match alias {
            Some(alias) => self.ast.text(alias).to_vec(),
            None => table.name.clone(),
        };
        let id = self.sources.len();
        self.sources.push(BoundSource {
            index_hint: crate::bind::IndexChoice::Any,
            id,
            rows: SourceRows::RecursiveSelf { cte },
            table: std::rc::Rc::new(table),
            alias,
            join,
            constraint: None,
            suppressed: Vec::new(),
            index_exprs: Vec::new(),
        });
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(id);
        }
        Ok(())
    }

    /// Binds a `WITH RECURSIVE` CTE reference.
    ///
    /// The seed arm is bound first, alone, because until it is bound nothing
    /// knows what columns the CTE has - and the step arm cannot be bound until
    /// a reference to the CTE has columns to resolve against. A CTE declared
    /// `RECURSIVE` that turns out not to reference itself is an ordinary
    /// compound, and is rebuilt as one rather than run through a queue that
    /// would never be fed.
    pub(super) fn bind_recursive_cte(
        &mut self,
        cte: &CteBinding,
        alias: Vec<u8>,
        join: JoinKind,
        span: Span,
    ) -> Result<(), ParseError> {
        let Some(select) = self.ast.select(cte.select) else {
            return Err(unsupported("missing select", span));
        };
        if select.compounds.is_empty() {
            return self.bind_subquery_term(
                cte.select,
                Some(alias),
                cte.columns.clone(),
                join,
                span,
            );
        }
        let arms: Vec<(CompoundOp, ast::SelectCoreId)> = select.compounds.clone();
        let order_by = select.order_by.clone();
        let limit = select.limit;
        let offset = select.offset;
        let first = select.first;
        if !order_by.is_empty() || limit.is_some() || offset.is_some() {
            return Err(ParseError::new(
                ParseErrorKind::Unsupported(
                    "ORDER BY and LIMIT are not allowed on a recursive CTE",
                ),
                span,
            ));
        }

        let id = self.sources.len();
        // The store's FROM-term number is reserved before anything is bound, so
        // that a self-reference inside the step arm can name the store it will
        // read without the two being bound in an impossible order.
        self.sources.push(BoundSource {
            index_hint: crate::bind::IndexChoice::Any,
            id,
            rows: SourceRows::Table,
            table: std::rc::Rc::new(TableInfo::subquery(alias.clone(), 0, Vec::new())),
            alias: alias.clone(),
            join,
            constraint: None,
            suppressed: Vec::new(),
            index_exprs: Vec::new(),
        });

        let seed = self.bind_isolated_arm(first)?;
        let table = subquery_table(&alias, &cte.columns, &seed);
        if !cte.columns.is_empty() && cte.columns.len() != seed.columns.len() {
            return Err(ParseError::new(
                ParseErrorKind::Unsupported("the named column list does not match the query"),
                span,
            ));
        }
        self.recursing.push(RecursiveTarget {
            folded: cte.folded.clone(),
            id,
            table: table.clone(),
            referenced: false,
        });
        let mut seeds = vec![(CompoundOp::UnionAll, seed)];
        let mut steps = Vec::new();
        let mut outcome = Ok(());
        for (op, arm) in &arms {
            if !matches!(op, CompoundOp::Union | CompoundOp::UnionAll) {
                outcome = Err(ParseError::new(
                    ParseErrorKind::Unsupported("recursive query does not use UNION or UNION ALL"),
                    span,
                ));
                break;
            }
            if let Some(target) = self.recursing.last_mut() {
                target.referenced = false;
            }
            let bound = match self.bind_isolated_arm(*arm) {
                Ok(bound) => bound,
                Err(reason) => {
                    outcome = Err(reason);
                    break;
                }
            };
            let referenced = self
                .recursing
                .last()
                .is_some_and(|target| target.referenced);
            if referenced {
                steps.push((*op, bound));
            } else {
                seeds.push((*op, bound));
            }
        }
        self.recursing.pop();
        outcome?;

        let mut source = BoundSource {
            index_hint: crate::bind::IndexChoice::Any,
            id,
            rows: SourceRows::Recursive(Box::new(RecursiveBody { seeds, steps })),
            table: std::rc::Rc::new(table),
            alias,
            join,
            constraint: None,
            suppressed: Vec::new(),
            index_exprs: Vec::new(),
        };
        if let SourceRows::Recursive(body) = &mut source.rows {
            if body.steps.is_empty() {
                // Declared recursive, never refers to itself: an ordinary
                // compound wearing the keyword.
                let mut arms = core::mem::take(&mut body.seeds);
                if arms.is_empty() {
                    return Err(unsupported("missing select core", span));
                }
                let mut head = arms.remove(0).1;
                head.compounds = arms;
                source.rows = SourceRows::Subquery(Box::new(head));
            }
        }
        if let Some(slot) = self.sources.get_mut(id) {
            *slot = source;
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(id);
        }
        Ok(())
    }
}
