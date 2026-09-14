//! The SELECT grammar: `WITH`, compound arms, FROM terms, and windows.
//!
//! Invariant: nothing is normalised here. A comma join and a `CROSS JOIN` are
//! different nodes even though both are cross joins, because SQLite refuses to
//! reorder one and will reorder the other; `VALUES` is its own arm rather than
//! a `SELECT` over nothing; and an `ORDER BY` after a compound belongs to the
//! compound, not to its last arm.

use super::Parser;
use crate::ast::{
    CommonTableExpr, CompoundOp, FromSource, FromTerm, FromTermId, IndexHint, JoinConstraint,
    JoinKind, NameId, ResultColumn, Select, SelectBody, SelectCore, SelectCoreId, SelectId,
    Statement, With,
};
use crate::diagnostic::{ParseError, ParseErrorKind};
use crate::keyword::Keyword;
use crate::lexer::{Punctuator, Span};
use inillucent_base::limits::Limit;

impl Parser<'_> {
    /// Parses a SELECT statement, including any `WITH` prefix.
    pub(super) fn parse_select_statement(&mut self) -> Result<Statement, ParseError> {
        Ok(Statement::Select(self.parse_select()?))
    }

    /// Parses a complete SELECT: `WITH`, arms, `ORDER BY`, `LIMIT`.
    pub(super) fn parse_select(&mut self) -> Result<SelectId, ParseError> {
        // Counted so `CHECK` can tell whether the expression it just read
        // contained a subquery. See `Parser::no_subquery_in_check`.
        self.selects = self.selects.saturating_add(1);
        self.enter()?;
        let parsed = self.parse_select_inner();
        self.leave();
        parsed
    }

    /// The body of [`Parser::parse_select`], with the depth charge applied.
    fn parse_select_inner(&mut self) -> Result<SelectId, ParseError> {
        let with = self.parse_with_prefix()?;
        let start = self.cursor();
        let first = self.parse_select_core()?;
        let mut compounds = Vec::new();
        while let Some(op) = self.parse_compound_operator()? {
            if compounds.len() as i64 >= self.limits.get(Limit::CompoundSelect) {
                return Err(ParseError::new(
                    ParseErrorKind::LimitExceeded("too many terms in compound SELECT"),
                    Span::at(self.cursor()),
                ));
            }
            compounds.push((op, self.parse_select_core()?));
        }
        let order_by = if self.at_keyword(Keyword::ORDER)? {
            self.bump()?;
            self.expect_keyword(Keyword::BY)?;
            self.parse_order_terms()?
        } else {
            Vec::new()
        };
        let (limit, offset) = self.parse_limit_clause()?;
        let end = self.cursor();
        Ok(self.ast.add_select(Select {
            with,
            first,
            compounds,
            order_by,
            limit,
            offset,
            span: Span::new(start, end),
        }))
    }

    /// Parses `LIMIT expr [OFFSET expr | , expr]`.
    ///
    /// The comma form reverses the operands: `LIMIT a, b` means offset `a`,
    /// limit `b`, which is the opposite of what it reads like.
    pub(super) fn parse_limit_clause(
        &mut self,
    ) -> Result<(Option<crate::ast::ExprId>, Option<crate::ast::ExprId>), ParseError> {
        if !self.eat_keyword(Keyword::LIMIT)? {
            return Ok((None, None));
        }
        let first = self.parse_expr()?;
        if self.eat_keyword(Keyword::OFFSET)? {
            return Ok((Some(first), Some(self.parse_expr()?)));
        }
        if self.eat(Punctuator::Comma)? {
            let second = self.parse_expr()?;
            return Ok((Some(second), Some(first)));
        }
        Ok((Some(first), None))
    }

    /// Parses a `WITH [RECURSIVE] name AS (...)` prefix, if there is one.
    pub(super) fn parse_with_prefix(&mut self) -> Result<With, ParseError> {
        if !self.eat_keyword(Keyword::WITH)? {
            return Ok(With::default());
        }
        let recursive = self.eat_keyword(Keyword::RECURSIVE)?;
        let mut ctes = Vec::new();
        loop {
            let name = self.parse_name()?;
            let mut columns = Vec::new();
            if self.at(Punctuator::LeftParen)? && !self.peek_at(1)?.is(Punctuator::RightParen) {
                // A CTE's column list and its body are both parenthesised; the
                // column list is the one that is not followed by SELECT.
                let is_column_list = !matches!(
                    self.peek_at(1)?.keyword(),
                    Some(Keyword::SELECT) | Some(Keyword::VALUES) | Some(Keyword::WITH)
                );
                if is_column_list {
                    self.bump()?;
                    loop {
                        columns.push(self.parse_name()?);
                        if !self.eat(Punctuator::Comma)? {
                            break;
                        }
                    }
                    self.expect(Punctuator::RightParen)?;
                }
            }
            self.expect_keyword(Keyword::AS)?;
            let materialized = if self.eat_keyword(Keyword::MATERIALIZED)? {
                Some(true)
            } else if self.at_keyword(Keyword::NOT)?
                && self.at_keyword_ahead(1, Keyword::MATERIALIZED)?
            {
                self.bump()?;
                self.bump()?;
                Some(false)
            } else {
                None
            };
            self.expect(Punctuator::LeftParen)?;
            let select = self.parse_select()?;
            self.expect(Punctuator::RightParen)?;
            ctes.push(CommonTableExpr {
                name,
                columns,
                materialized,
                select,
            });
            if !self.eat(Punctuator::Comma)? {
                break;
            }
        }
        Ok(With { recursive, ctes })
    }

    /// Parses a compound operator, if the next tokens spell one.
    fn parse_compound_operator(&mut self) -> Result<Option<CompoundOp>, ParseError> {
        if self.at_keyword(Keyword::UNION)? {
            self.bump()?;
            if self.eat_keyword(Keyword::ALL)? {
                return Ok(Some(CompoundOp::UnionAll));
            }
            return Ok(Some(CompoundOp::Union));
        }
        if self.eat_keyword(Keyword::INTERSECT)? {
            return Ok(Some(CompoundOp::Intersect));
        }
        if self.eat_keyword(Keyword::EXCEPT)? {
            return Ok(Some(CompoundOp::Except));
        }
        Ok(None)
    }

    /// Parses one arm: a `SELECT ...` or a `VALUES ...`.
    fn parse_select_core(&mut self) -> Result<SelectCoreId, ParseError> {
        let start = self.cursor();
        if self.at_keyword(Keyword::VALUES)? {
            self.bump()?;
            let mut rows = Vec::new();
            loop {
                self.expect(Punctuator::LeftParen)?;
                let mut row = Vec::new();
                if !self.at(Punctuator::RightParen)? {
                    loop {
                        row.push(self.parse_expr()?);
                        if !self.eat(Punctuator::Comma)? {
                            break;
                        }
                    }
                }
                self.expect(Punctuator::RightParen)?;
                rows.push(row);
                if !self.eat(Punctuator::Comma)? {
                    break;
                }
            }
            let end = self.cursor();
            return Ok(self.ast.add_core(SelectCore {
                body: SelectBody::Values(rows),
                span: Span::new(start, end),
            }));
        }
        self.expect_keyword(Keyword::SELECT)?;
        let distinct = self.eat_keyword(Keyword::DISTINCT)?;
        let all = if distinct {
            false
        } else {
            self.eat_keyword(Keyword::ALL)?
        };
        let columns = self.parse_result_columns()?;
        let from = if self.eat_keyword(Keyword::FROM)? {
            self.parse_from_clause()?
        } else {
            Vec::new()
        };
        let filter = if self.eat_keyword(Keyword::WHERE)? {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        let mut having = None;
        if self.at_keyword(Keyword::GROUP)? {
            self.bump()?;
            self.expect_keyword(Keyword::BY)?;
            loop {
                group_by.push(self.parse_expr()?);
                if !self.eat(Punctuator::Comma)? {
                    break;
                }
            }
            if self.eat_keyword(Keyword::HAVING)? {
                having = Some(self.parse_expr()?);
            }
        }
        let windows = self.parse_window_clause()?;
        let end = self.cursor();
        Ok(self.ast.add_core(SelectCore {
            body: SelectBody::Select {
                distinct,
                all,
                columns,
                from,
                filter,
                group_by,
                having,
                windows,
            },
            span: Span::new(start, end),
        }))
    }

    /// Parses the result column list.
    fn parse_result_columns(&mut self) -> Result<Vec<ResultColumn>, ParseError> {
        let mut columns = Vec::new();
        loop {
            let start = self.cursor();
            let expr = self.parse_expr()?;
            let (alias, alias_was_explicit) = match self.ast.expr(expr) {
                // `*` and `t.*` take no alias; a word after them is a syntax
                // error rather than an alias, which is what SQLite reports.
                Some(crate::ast::Expr::Star { .. }) => (None, false),
                _ => self.parse_alias()?,
            };
            let end = self.cursor();
            columns.push(ResultColumn {
                expr,
                alias,
                alias_was_explicit,
                span: Span::new(start, end),
            });
            if columns.len() as i64 > self.limits.get(Limit::Column) {
                return Err(ParseError::new(
                    ParseErrorKind::LimitExceeded("too many columns in result set"),
                    Span::at(start),
                ));
            }
            if !self.eat(Punctuator::Comma)? {
                return Ok(columns);
            }
        }
    }

    /// Parses a FROM clause: a first term followed by joined terms.
    fn parse_from_clause(&mut self) -> Result<Vec<FromTermId>, ParseError> {
        let mut terms = Vec::new();
        let first = self.parse_from_term(JoinKind::Comma, false, JoinConstraint::None)?;
        terms.push(first);
        loop {
            let Some((join, natural)) = self.parse_join_operator()? else {
                return Ok(terms);
            };
            let start = self.cursor();
            let term = self.parse_from_term(join, natural, JoinConstraint::None)?;
            let constraint = self.parse_join_constraint()?;
            if natural && constraint != JoinConstraint::None {
                return Err(ParseError::new(
                    ParseErrorKind::Unsupported("a NATURAL join may not have ON or USING"),
                    Span::at(start),
                ));
            }
            if let Some(stored) = self.ast_from_term_mut(term) {
                stored.constraint = constraint;
            }
            terms.push(term);
        }
    }

    /// Returns a mutable handle to a stored FROM term.
    ///
    /// The constraint is parsed after the term it belongs to, because `ON` and
    /// `USING` follow the table rather than precede it, so the term is patched
    /// once rather than being built out of order.
    fn ast_from_term_mut(&mut self, id: FromTermId) -> Option<&mut FromTerm> {
        self.ast.from_term_mut(id)
    }

    /// Parses a join operator, returning the kind and whether it was natural.
    fn parse_join_operator(&mut self) -> Result<Option<(JoinKind, bool)>, ParseError> {
        if self.eat(Punctuator::Comma)? {
            return Ok(Some((JoinKind::Comma, false)));
        }
        let natural = self.at_keyword(Keyword::NATURAL)?;
        let offset = usize::from(natural);
        let kind = match self.peek_at(offset)?.keyword() {
            Some(Keyword::JOIN) => JoinKind::Inner,
            Some(Keyword::INNER) => JoinKind::Inner,
            Some(Keyword::CROSS) => JoinKind::Cross,
            Some(Keyword::LEFT) => JoinKind::Left,
            Some(Keyword::RIGHT) => JoinKind::Right,
            Some(Keyword::FULL) => JoinKind::Full,
            _ => return Ok(None),
        };
        if natural {
            self.bump()?;
        }
        if kind != JoinKind::Inner || self.at_keyword(Keyword::INNER)? {
            self.bump()?;
            if matches!(kind, JoinKind::Left | JoinKind::Right | JoinKind::Full) {
                self.eat_keyword(Keyword::OUTER)?;
            }
            self.expect_keyword(Keyword::JOIN)?;
        } else {
            self.expect_keyword(Keyword::JOIN)?;
        }
        Ok(Some((kind, natural)))
    }

    /// Parses `ON expr` or `USING (a, b)`.
    fn parse_join_constraint(&mut self) -> Result<JoinConstraint, ParseError> {
        if self.eat_keyword(Keyword::ON)? {
            return Ok(JoinConstraint::On(self.parse_expr()?));
        }
        if self.eat_keyword(Keyword::USING)? {
            self.expect(Punctuator::LeftParen)?;
            let mut columns = Vec::new();
            loop {
                columns.push(self.parse_name()?);
                if !self.eat(Punctuator::Comma)? {
                    break;
                }
            }
            self.expect(Punctuator::RightParen)?;
            return Ok(JoinConstraint::Using(columns));
        }
        Ok(JoinConstraint::None)
    }

    /// Parses one FROM term: a table, a subquery, or a parenthesised join.
    pub(super) fn parse_from_term(
        &mut self,
        join: JoinKind,
        natural: bool,
        constraint: JoinConstraint,
    ) -> Result<FromTermId, ParseError> {
        let start = self.cursor();
        if self.at(Punctuator::LeftParen)? {
            self.bump()?;
            let source = if self.at_keyword(Keyword::SELECT)?
                || self.at_keyword(Keyword::WITH)?
                || self.at_keyword(Keyword::VALUES)?
            {
                FromSource::Subquery(self.parse_select()?)
            } else {
                FromSource::Join(self.parse_from_clause()?)
            };
            self.expect(Punctuator::RightParen)?;
            let (alias, _) = self.parse_alias()?;
            let end = self.cursor();
            return Ok(self.ast.add_from_term(FromTerm {
                source,
                alias,
                join,
                natural,
                constraint,
                span: Span::new(start, end),
            }));
        }
        let (database, name) = self.parse_qualified_name()?;
        let arguments = if self.at(Punctuator::LeftParen)? {
            self.bump()?;
            let mut list = Vec::new();
            if !self.at(Punctuator::RightParen)? {
                loop {
                    list.push(self.parse_expr()?);
                    if !self.eat(Punctuator::Comma)? {
                        break;
                    }
                }
            }
            self.expect(Punctuator::RightParen)?;
            Some(list)
        } else {
            None
        };
        let (alias, _) = self.parse_alias()?;
        let indexed_by = self.parse_index_hint()?;
        let end = self.cursor();
        Ok(self.ast.add_from_term(FromTerm {
            source: FromSource::Table {
                database,
                name,
                arguments,
                indexed_by,
            },
            alias,
            join,
            natural,
            constraint,
            span: Span::new(start, end),
        }))
    }

    /// Parses `INDEXED BY name` or `NOT INDEXED`.
    fn parse_index_hint(&mut self) -> Result<IndexHint, ParseError> {
        if self.at_keyword(Keyword::INDEXED)? {
            self.bump()?;
            self.expect_keyword(Keyword::BY)?;
            return Ok(IndexHint::IndexedBy(self.parse_name()?));
        }
        if self.at_keyword(Keyword::NOT)? && self.at_keyword_ahead(1, Keyword::INDEXED)? {
            self.bump()?;
            self.bump()?;
            return Ok(IndexHint::NotIndexed);
        }
        Ok(IndexHint::None)
    }

    /// Parses a `WINDOW name AS (...)` clause.
    fn parse_window_clause(&mut self) -> Result<Vec<(NameId, crate::ast::WindowId)>, ParseError> {
        if !self.at_keyword(Keyword::WINDOW)? {
            return Ok(Vec::new());
        }
        self.bump()?;
        let mut windows = Vec::new();
        loop {
            let name = self.parse_name()?;
            self.expect_keyword(Keyword::AS)?;
            let (window, _) = self.parse_over_clause()?;
            windows.push((name, window));
            if !self.eat(Punctuator::Comma)? {
                return Ok(windows);
            }
        }
    }

    /// Parses the body of an `OVER` clause, which is either a window name or a
    /// parenthesised definition.
    pub(super) fn parse_over_clause(&mut self) -> Result<(crate::ast::WindowId, Span), ParseError> {
        use crate::ast::{FrameBound, FrameExclude, FrameUnit, Window};
        let start = self.cursor();
        if !self.at(Punctuator::LeftParen)? {
            // **The token's span, not the interned name's (task-1913).**
            // Interning deduplicates, so the second `w` in
            // `SELECT first_value(n) OVER w, last_value(n) OVER w` carried the
            // first one's position, and the second column took its name from a
            // slice running backwards through the query: `w, last_value`.
            let (base, span) = self.parse_name_spanned()?;
            let id = self.ast.add_window(Window {
                base: Some(base),
                partition_by: Vec::new(),
                order_by: Vec::new(),
                unit: None,
                start: None,
                end: None,
                exclude: FrameExclude::NoOthers,
                span,
            });
            return Ok((id, span));
        }
        self.expect(Punctuator::LeftParen)?;
        let base = if self.at_name()?
            && !self.at_keyword(Keyword::PARTITION)?
            && !self.at_keyword(Keyword::ORDER)?
            && !self.at_keyword(Keyword::ROWS)?
            && !self.at_keyword(Keyword::RANGE)?
            && !self.at_keyword(Keyword::GROUPS)?
        {
            Some(self.parse_name()?)
        } else {
            None
        };
        let mut partition_by = Vec::new();
        if self.at_keyword(Keyword::PARTITION)? {
            self.bump()?;
            self.expect_keyword(Keyword::BY)?;
            loop {
                partition_by.push(self.parse_expr()?);
                if !self.eat(Punctuator::Comma)? {
                    break;
                }
            }
        }
        let order_by = if self.at_keyword(Keyword::ORDER)? {
            self.bump()?;
            self.expect_keyword(Keyword::BY)?;
            self.parse_order_terms()?
        } else {
            Vec::new()
        };
        let unit = if self.eat_keyword(Keyword::ROWS)? {
            Some(FrameUnit::Rows)
        } else if self.eat_keyword(Keyword::RANGE)? {
            Some(FrameUnit::Range)
        } else if self.eat_keyword(Keyword::GROUPS)? {
            Some(FrameUnit::Groups)
        } else {
            None
        };
        let (frame_start, frame_end) = if unit.is_some() {
            if self.eat_keyword(Keyword::BETWEEN)? {
                let low = self.parse_frame_bound()?;
                self.expect_keyword(Keyword::AND)?;
                let high = self.parse_frame_bound()?;
                (Some(low), Some(high))
            } else {
                (
                    Some(self.parse_frame_bound()?),
                    Some(FrameBound::CurrentRow),
                )
            }
        } else {
            (None, None)
        };
        let exclude = if self.eat_keyword(Keyword::EXCLUDE)? {
            if self.eat_keyword(Keyword::NO)? {
                self.expect_keyword(Keyword::OTHERS)?;
                FrameExclude::NoOthers
            } else if self.eat_keyword(Keyword::CURRENT)? {
                self.expect_keyword(Keyword::ROW)?;
                FrameExclude::CurrentRow
            } else if self.eat_keyword(Keyword::GROUP)? {
                FrameExclude::Group
            } else {
                self.expect_keyword(Keyword::TIES)?;
                FrameExclude::Ties
            }
        } else {
            FrameExclude::NoOthers
        };
        let close = self.expect(Punctuator::RightParen)?;
        let span = Span::new(start, close.span.end as usize);
        let id = self.ast.add_window(Window {
            base,
            partition_by,
            order_by,
            unit,
            start: frame_start,
            end: frame_end,
            exclude,
            span,
        });
        Ok((id, span))
    }

    /// Parses one end of a window frame.
    fn parse_frame_bound(&mut self) -> Result<crate::ast::FrameBound, ParseError> {
        use crate::ast::FrameBound;
        if self.eat_keyword(Keyword::UNBOUNDED)? {
            if self.eat_keyword(Keyword::PRECEDING)? {
                return Ok(FrameBound::UnboundedPreceding);
            }
            self.expect_keyword(Keyword::FOLLOWING)?;
            return Ok(FrameBound::UnboundedFollowing);
        }
        if self.at_keyword(Keyword::CURRENT)? {
            self.bump()?;
            self.expect_keyword(Keyword::ROW)?;
            return Ok(FrameBound::CurrentRow);
        }
        let expr = self.parse_expr()?;
        if self.eat_keyword(Keyword::PRECEDING)? {
            return Ok(FrameBound::Preceding(expr));
        }
        self.expect_keyword(Keyword::FOLLOWING)?;
        Ok(FrameBound::Following(expr))
    }

    /// Parses a `RETURNING` clause.
    pub(super) fn parse_returning(&mut self) -> Result<Vec<ResultColumn>, ParseError> {
        if !self.eat_keyword(Keyword::RETURNING)? {
            return Ok(Vec::new());
        }
        self.parse_result_columns()
    }
}
