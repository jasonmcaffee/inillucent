//! `INSERT`, `UPDATE` and `DELETE`.
//!
//! Invariant: the ambiguity SQLite's own grammar has here is preserved rather
//! than resolved early. `INSERT INTO t SELECT ...` and `INSERT INTO t VALUES
//! ...` are the same production over a SELECT, `ON CONFLICT` keeps every clause
//! written, and an `UPSERT` records whether the action was `DO UPDATE` even
//! when its assignment list is empty, because the diagnostics differ.

use super::Parser;
use crate::ast::{
    ConflictAction, Delete, Insert, InsertSource, JoinConstraint, JoinKind, NameId, Statement,
    Update, Upsert,
};
use crate::diagnostic::ParseError;
use crate::keyword::Keyword;
use crate::lexer::Punctuator;

impl Parser<'_> {
    /// Parses `INSERT [OR action] INTO ...` and `REPLACE INTO ...`.
    pub(super) fn parse_insert(&mut self) -> Result<Statement, ParseError> {
        let with = self.parse_with_prefix()?;
        let token = self.bump()?;
        let on_conflict = if token.keyword() == Some(Keyword::REPLACE) {
            Some(ConflictAction::Replace)
        } else if self.eat_keyword(Keyword::OR)? {
            Some(self.parse_conflict_action()?)
        } else {
            None
        };
        self.expect_keyword(Keyword::INTO)?;
        let (database, table) = self.parse_qualified_name()?;
        let alias = if self.eat_keyword(Keyword::AS)? {
            Some(self.parse_name()?)
        } else {
            None
        };
        let mut columns = Vec::new();
        if self.at(Punctuator::LeftParen)? {
            self.bump()?;
            loop {
                columns.push(self.parse_name()?);
                if !self.eat(Punctuator::Comma)? {
                    break;
                }
            }
            self.expect(Punctuator::RightParen)?;
        }
        let source = if self.at_keyword(Keyword::DEFAULT)? {
            self.bump()?;
            self.expect_keyword(Keyword::VALUES)?;
            InsertSource::DefaultValues
        } else {
            InsertSource::Select(self.parse_select()?)
        };
        let mut upserts = Vec::new();
        while self.at_keyword(Keyword::ON)? && self.at_keyword_ahead(1, Keyword::CONFLICT)? {
            self.bump()?;
            self.bump()?;
            upserts.push(self.parse_upsert()?);
        }
        let returning = self.parse_returning()?;
        Ok(Statement::Insert(Box::new(Insert {
            with,
            on_conflict,
            database,
            table,
            alias,
            columns,
            source,
            upserts,
            returning,
        })))
    }

    /// Parses the body of one `ON CONFLICT` clause.
    fn parse_upsert(&mut self) -> Result<Upsert, ParseError> {
        let mut target = Vec::new();
        let mut target_filter = None;
        if self.at(Punctuator::LeftParen)? {
            self.bump()?;
            loop {
                target.push(self.parse_indexed_column()?);
                if !self.eat(Punctuator::Comma)? {
                    break;
                }
            }
            self.expect(Punctuator::RightParen)?;
            if self.eat_keyword(Keyword::WHERE)? {
                target_filter = Some(self.parse_expr()?);
            }
        }
        self.expect_keyword(Keyword::DO)?;
        if self.eat_keyword(Keyword::NOTHING)? {
            return Ok(Upsert {
                target,
                target_filter,
                assignments: Vec::new(),
                do_update: false,
                filter: None,
            });
        }
        self.expect_keyword(Keyword::UPDATE)?;
        self.expect_keyword(Keyword::SET)?;
        let assignments = self.parse_assignments()?;
        let filter = if self.eat_keyword(Keyword::WHERE)? {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Upsert {
            target,
            target_filter,
            assignments,
            do_update: true,
            filter,
        })
    }

    /// Parses `UPDATE [OR action] target SET ... [FROM ...] [WHERE ...]`.
    pub(super) fn parse_update(&mut self) -> Result<Statement, ParseError> {
        let with = self.parse_with_prefix()?;
        self.expect_keyword(Keyword::UPDATE)?;
        let on_conflict = if self.eat_keyword(Keyword::OR)? {
            Some(self.parse_conflict_action()?)
        } else {
            None
        };
        let target = self.parse_from_term(JoinKind::Comma, false, JoinConstraint::None)?;
        self.expect_keyword(Keyword::SET)?;
        let assignments = self.parse_assignments()?;
        // The whole FROM grammar a SELECT takes, joins and all: SQLite
        // accepts `UPDATE t SET ... FROM a JOIN b ON ...`, and this parsed a
        // comma list only, so `JOIN` was a syntax error.
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
        let returning = self.parse_returning()?;
        let mut limited_at = None;
        let order_by = if self.at_keyword(Keyword::ORDER)? {
            limited_at = Some((crate::ast::Limited::OrderBy, self.peek()?.span));
            self.bump()?;
            self.expect_keyword(Keyword::BY)?;
            self.parse_order_terms()?
        } else {
            Vec::new()
        };
        if limited_at.is_none() && self.at_keyword(Keyword::LIMIT)? {
            limited_at = Some((crate::ast::Limited::Limit, self.peek()?.span));
        }
        let (limit, offset) = self.parse_limit_clause()?;
        Ok(Statement::Update(Box::new(Update {
            with,
            on_conflict,
            target,
            assignments,
            from,
            filter,
            returning,
            order_by,
            limit,
            offset,
            limited_at,
        })))
    }

    /// Parses `DELETE FROM target [WHERE ...]`.
    pub(super) fn parse_delete(&mut self) -> Result<Statement, ParseError> {
        let with = self.parse_with_prefix()?;
        self.expect_keyword(Keyword::DELETE)?;
        self.expect_keyword(Keyword::FROM)?;
        let target = self.parse_from_term(JoinKind::Comma, false, JoinConstraint::None)?;
        let filter = if self.eat_keyword(Keyword::WHERE)? {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        let mut limited_at = None;
        let order_by = if self.at_keyword(Keyword::ORDER)? {
            limited_at = Some((crate::ast::Limited::OrderBy, self.peek()?.span));
            self.bump()?;
            self.expect_keyword(Keyword::BY)?;
            self.parse_order_terms()?
        } else {
            Vec::new()
        };
        if limited_at.is_none() && self.at_keyword(Keyword::LIMIT)? {
            limited_at = Some((crate::ast::Limited::Limit, self.peek()?.span));
        }
        let (limit, offset) = self.parse_limit_clause()?;
        Ok(Statement::Delete(Box::new(Delete {
            with,
            target,
            filter,
            returning,
            order_by,
            limit,
            offset,
            limited_at,
        })))
    }

    /// Parses a `SET` list, where a group of names is the row-value form.
    fn parse_assignments(&mut self) -> Result<Vec<(Vec<NameId>, crate::ast::ExprId)>, ParseError> {
        let mut assignments = Vec::new();
        loop {
            let mut names = Vec::new();
            if self.at(Punctuator::LeftParen)? {
                self.bump()?;
                loop {
                    names.push(self.parse_name()?);
                    if !self.eat(Punctuator::Comma)? {
                        break;
                    }
                }
                self.expect(Punctuator::RightParen)?;
            } else {
                names.push(self.parse_name()?);
            }
            self.expect(Punctuator::Equal)?;
            let value = self.parse_expr()?;
            assignments.push((names, value));
            if !self.eat(Punctuator::Comma)? {
                return Ok(assignments);
            }
        }
    }

    /// Parses a conflict algorithm after `OR`.
    pub(super) fn parse_conflict_action(&mut self) -> Result<ConflictAction, ParseError> {
        if self.eat_keyword(Keyword::ROLLBACK)? {
            return Ok(ConflictAction::Rollback);
        }
        if self.eat_keyword(Keyword::ABORT)? {
            return Ok(ConflictAction::Abort);
        }
        if self.eat_keyword(Keyword::FAIL)? {
            return Ok(ConflictAction::Fail);
        }
        if self.eat_keyword(Keyword::IGNORE)? {
            return Ok(ConflictAction::Ignore);
        }
        if self.eat_keyword(Keyword::REPLACE)? {
            return Ok(ConflictAction::Replace);
        }
        Err(self.unexpected(&["ROLLBACK", "ABORT", "FAIL", "IGNORE", "REPLACE"])?)
    }
}
