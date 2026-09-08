//! The Pratt expression parser.
//!
//! Invariant: precedence comes from `crate::precedence`, never from the shape
//! of these functions, and every special form is its own node. `NOT BETWEEN`,
//! `NOT IN`, `IS NOT DISTINCT FROM` and `NOTNULL` are parsed as what they are
//! rather than reconstructed from a generic `NOT` applied to something else,
//! because the reconstruction loses the span the user wrote and, for `IS NOT
//! DISTINCT FROM`, is not even equivalent.

use super::Parser;
use crate::ast::BinaryOp;
use crate::ast::{Expr, ExprId, InRhs, Literal, PatternOp, RaiseAction, SortOrder, UnaryOp};
use crate::diagnostic::{ParseError, ParseErrorKind};
use crate::keyword::Keyword;
use crate::lexer::{self, Punctuator, Span, TokenKind};
use crate::precedence::{self, Power};
use inillucent_base::limits::Limit;

impl Parser<'_> {
    /// Parses a complete expression.
    pub(super) fn parse_expr(&mut self) -> Result<ExprId, ParseError> {
        self.parse_expr_bp(precedence::LOWEST)
    }

    /// Parses an expression that binds at least as tightly as `minimum`.
    fn parse_expr_bp(&mut self, minimum: Power) -> Result<ExprId, ParseError> {
        self.enter()?;
        let parsed = self.parse_expr_loop(minimum);
        self.leave();
        parsed
    }

    /// The Pratt loop: one prefix, then as many suffixes as bind tightly enough.
    fn parse_expr_loop(&mut self, minimum: Power) -> Result<ExprId, ParseError> {
        let mut left = self.parse_prefix()?;
        loop {
            let Some(next) = self.parse_suffix(left, minimum)? else {
                return Ok(left);
            };
            left = next;
        }
    }

    /// Parses one prefix form: a literal, a name, a call, a parenthesis, or a
    /// unary operator.
    pub(crate) fn parse_prefix(&mut self) -> Result<ExprId, ParseError> {
        let token = self.peek()?;
        match token.kind {
            TokenKind::EndOfInput => Err(self.unexpected(&["an expression"])?),
            TokenKind::Integer => {
                self.bump()?;
                let text = token.text(self.source()).to_vec();
                Ok(self
                    .ast
                    .add_expr(Expr::Literal(Literal::Integer(text)), token.span))
            }
            TokenKind::Float => {
                self.bump()?;
                let text = token.text(self.source()).to_vec();
                Ok(self
                    .ast
                    .add_expr(Expr::Literal(Literal::Float(text)), token.span))
            }
            TokenKind::String => {
                self.bump()?;
                let text = lexer::string_text(self.source(), token).into_owned();
                self.charge_literal(text.len(), token.span)?;
                Ok(self
                    .ast
                    .add_expr(Expr::Literal(Literal::String(text)), token.span))
            }
            TokenKind::Blob => {
                self.bump()?;
                let bytes = lexer::blob_bytes(self.source(), token);
                self.charge_literal(bytes.len(), token.span)?;
                Ok(self
                    .ast
                    .add_expr(Expr::Literal(Literal::Blob(bytes)), token.span))
            }
            TokenKind::Parameter => {
                self.bump()?;
                let (index, name) = self.assign_parameter(token)?;
                Ok(self
                    .ast
                    .add_expr(Expr::Parameter { index, name }, token.span))
            }
            TokenKind::Punctuator(Punctuator::Minus) => self.parse_unary(UnaryOp::Negate),
            TokenKind::Punctuator(Punctuator::Plus) => self.parse_unary(UnaryOp::Identity),
            TokenKind::Punctuator(Punctuator::BitNot) => self.parse_unary(UnaryOp::BitNot),
            TokenKind::Punctuator(Punctuator::LeftParen) => self.parse_parenthesised(),
            TokenKind::Punctuator(Punctuator::Star) => {
                self.bump()?;
                Ok(self.ast.add_expr(Expr::Star { table: None }, token.span))
            }
            TokenKind::Identifier { .. } => self.parse_word_prefix(),
            TokenKind::Punctuator(_) => Err(self.unexpected(&["an expression"])?),
        }
    }

    /// Refuses a literal longer than the length limit before it is stored.
    fn charge_literal(&mut self, length: usize, span: Span) -> Result<(), ParseError> {
        if length as i64 > self.limits.get(Limit::Length) {
            return Err(ParseError::new(
                ParseErrorKind::LimitExceeded("string or blob too big"),
                span,
            ));
        }
        Ok(())
    }

    /// Parses a prefix operator and its operand.
    fn parse_unary(&mut self, op: UnaryOp) -> Result<ExprId, ParseError> {
        let token = self.bump()?;
        let operand = self.parse_expr_bp(precedence::UNARY)?;
        let span = token.span.to(self.ast.expr_span(operand));
        Ok(self.ast.add_expr(Expr::Unary { op, operand }, span))
    }

    /// Parses `( ... )`, which is a scalar subquery, a row value, or grouping.
    fn parse_parenthesised(&mut self) -> Result<ExprId, ParseError> {
        let open = self.expect(Punctuator::LeftParen)?;
        if self.at_keyword(Keyword::SELECT)?
            || self.at_keyword(Keyword::WITH)?
            || self.at_keyword(Keyword::VALUES)?
        {
            let select = self.parse_select()?;
            let close = self.expect(Punctuator::RightParen)?;
            return Ok(self
                .ast
                .add_expr(Expr::Subquery(select), open.span.to(close.span)));
        }
        let first = self.parse_expr()?;
        if !self.at(Punctuator::Comma)? {
            let close = self.expect(Punctuator::RightParen)?;
            // Parentheses are grouping, not a node: the span widens so a
            // diagnostic can point at the whole group, and nothing else changes.
            let span = open.span.to(close.span);
            let inner = self.ast.expr(first).cloned();
            return match inner {
                Some(expr) => Ok(self.ast.add_expr(expr, span)),
                None => Ok(first),
            };
        }
        let mut values = vec![first];
        while self.eat(Punctuator::Comma)? {
            values.push(self.parse_expr()?);
        }
        let close = self.expect(Punctuator::RightParen)?;
        Ok(self
            .ast
            .add_expr(Expr::RowValue(values), open.span.to(close.span)))
    }

    /// Parses a prefix form that begins with a word: a keyword operator, a
    /// literal keyword, a function call, or a column reference.
    fn parse_word_prefix(&mut self) -> Result<ExprId, ParseError> {
        let token = self.peek()?;
        match token.keyword() {
            Some(Keyword::NOT) => {
                self.bump()?;
                let operand = self.parse_expr_bp(precedence::NOT)?;
                let span = token.span.to(self.ast.expr_span(operand));
                return Ok(self.ast.add_expr(
                    Expr::Unary {
                        op: UnaryOp::Not,
                        operand,
                    },
                    span,
                ));
            }
            Some(Keyword::NULL) => {
                self.bump()?;
                return Ok(self.ast.add_expr(Expr::Literal(Literal::Null), token.span));
            }
            Some(Keyword::CASE) => return self.parse_case(),
            Some(Keyword::CAST) => return self.parse_cast(),
            Some(Keyword::EXISTS) => {
                self.bump()?;
                self.expect(Punctuator::LeftParen)?;
                let select = self.parse_select()?;
                let close = self.expect(Punctuator::RightParen)?;
                return Ok(self.ast.add_expr(
                    Expr::Exists {
                        negated: false,
                        select,
                    },
                    token.span.to(close.span),
                ));
            }
            Some(Keyword::RAISE) => return self.parse_raise(),
            Some(Keyword::CURRENT_DATE) => {
                self.bump()?;
                return Ok(self
                    .ast
                    .add_expr(Expr::Literal(Literal::CurrentDate), token.span));
            }
            Some(Keyword::CURRENT_TIME) => {
                self.bump()?;
                return Ok(self
                    .ast
                    .add_expr(Expr::Literal(Literal::CurrentTime), token.span));
            }
            Some(Keyword::CURRENT_TIMESTAMP) => {
                self.bump()?;
                return Ok(self
                    .ast
                    .add_expr(Expr::Literal(Literal::CurrentTimestamp), token.span));
            }
            _ => {}
        }
        // `TRUE` and `FALSE` are not keywords in SQLite; they are identifiers
        // the expression layer recognises, which is why `SELECT true` works and
        // `CREATE TABLE t(true)` also works.
        let folded = token.text(self.source()).to_ascii_lowercase();
        if matches!(
            token.kind,
            TokenKind::Identifier {
                quote: crate::lexer::QuoteForm::Bare,
                ..
            }
        ) && (folded == b"true" || folded == b"false")
            && !self.peek_at(1)?.is(Punctuator::Dot)
            && !self.peek_at(1)?.is(Punctuator::LeftParen)
        {
            self.bump()?;
            return Ok(self.ast.add_expr(
                Expr::Literal(Literal::Boolean(folded == b"true")),
                token.span,
            ));
        }
        // `like(X, Y)` is a function call even though LIKE is a hard keyword,
        // and so are `glob`, `regexp` and `match`. SQLite's grammar has a rule
        // for exactly this set - they are its `LIKE_KW` token - because the
        // functions are how an application overrides the operators.
        let pattern_function = matches!(
            token.keyword(),
            Some(Keyword::LIKE)
                | Some(Keyword::GLOB)
                | Some(Keyword::REGEXP)
                | Some(Keyword::MATCH)
        ) && self.peek_at(1)?.is(Punctuator::LeftParen);
        if !Parser::token_is_name(token) && !pattern_function {
            return Err(self.unexpected(&["an expression"])?);
        }
        if self.peek_at(1)?.is(Punctuator::LeftParen) {
            return self.parse_function_call();
        }
        self.parse_column_reference()
    }

    /// Parses `a`, `a.b`, `a.b.c`, `a.*` and `a.b.*`.
    fn parse_column_reference(&mut self) -> Result<ExprId, ParseError> {
        let (first, start) = self.parse_name_spanned()?;
        if !self.at(Punctuator::Dot)? {
            return Ok(self.ast.add_expr(
                Expr::Column {
                    database: None,
                    table: None,
                    column: first,
                },
                start,
            ));
        }
        self.bump()?;
        if self.at(Punctuator::Star)? {
            let star = self.bump()?;
            return Ok(self
                .ast
                .add_expr(Expr::Star { table: Some(first) }, start.to(star.span)));
        }
        let (second, second_span) = self.parse_name_spanned()?;
        if !self.at(Punctuator::Dot)? {
            return Ok(self.ast.add_expr(
                Expr::Column {
                    database: None,
                    table: Some(first),
                    column: second,
                },
                start.to(second_span),
            ));
        }
        self.bump()?;
        if self.at(Punctuator::Star)? {
            let star = self.bump()?;
            return Ok(self.ast.add_expr(
                Expr::Star {
                    table: Some(second),
                },
                start.to(star.span),
            ));
        }
        let (third, third_span) = self.parse_name_spanned()?;
        Ok(self.ast.add_expr(
            Expr::Column {
                database: Some(first),
                table: Some(second),
                column: third,
            },
            start.to(third_span),
        ))
    }

    /// Parses a function call, including `count(*)`, `DISTINCT`, an argument
    /// `ORDER BY`, `FILTER` and `OVER`.
    fn parse_function_call(&mut self) -> Result<ExprId, ParseError> {
        // The span is taken before the name is interned. Interning reuses
        // an equal entry, so the interned span of `hex` in
        // `SELECT hex(a), hex(b)` is the first one's - and a call that
        // reported that as its own start would cover both columns.
        let start = self.peek()?.span;
        let name = self.parse_function_name()?;
        self.expect(Punctuator::LeftParen)?;
        let mut distinct = false;
        let mut arguments = Some(Vec::new());
        let mut order_by = Vec::new();
        if self.at(Punctuator::Star)? && self.peek_at(1)?.is(Punctuator::RightParen) {
            self.bump()?;
            arguments = None;
        } else if !self.at(Punctuator::RightParen)? {
            distinct = self.eat_keyword(Keyword::DISTINCT)?;
            if !distinct {
                self.eat_keyword(Keyword::ALL)?;
            }
            let mut list = Vec::new();
            loop {
                list.push(self.parse_expr()?);
                if !self.eat(Punctuator::Comma)? {
                    break;
                }
            }
            if list.len() as i64 > self.limits.get(Limit::FunctionArg) {
                return Err(ParseError::new(
                    ParseErrorKind::LimitExceeded("too many function arguments"),
                    start,
                ));
            }
            if self.at_keyword(Keyword::ORDER)? {
                self.bump()?;
                self.expect_keyword(Keyword::BY)?;
                order_by = self.parse_order_terms()?;
            }
            arguments = Some(list);
        }
        let mut end = self.expect(Punctuator::RightParen)?.span;
        let mut filter = None;
        if self.at_keyword(Keyword::FILTER)? && self.peek_at(1)?.is(Punctuator::LeftParen) {
            self.bump()?;
            self.expect(Punctuator::LeftParen)?;
            self.expect_keyword(Keyword::WHERE)?;
            filter = Some(self.parse_expr()?);
            end = self.expect(Punctuator::RightParen)?.span;
        }
        let mut over = None;
        if self.at_keyword(Keyword::OVER)? {
            self.bump()?;
            let (window, span) = self.parse_over_clause()?;
            over = Some(window);
            end = span;
        }
        Ok(self.ast.add_expr(
            Expr::Function {
                name,
                distinct,
                arguments,
                order_by,
                filter,
                over,
            },
            start.to(end),
        ))
    }

    /// Parses a function's name, which may be one of the pattern keywords.
    fn parse_function_name(&mut self) -> Result<crate::ast::NameId, ParseError> {
        let token = self.peek()?;
        if Parser::token_is_name(token) {
            return self.parse_name();
        }
        if matches!(
            token.keyword(),
            Some(Keyword::LIKE)
                | Some(Keyword::GLOB)
                | Some(Keyword::REGEXP)
                | Some(Keyword::MATCH)
        ) {
            self.bump()?;
            return Ok(self.intern_token(token));
        }
        Err(self.unexpected(&["a function name"])?)
    }

    /// Parses `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
    fn parse_case(&mut self) -> Result<ExprId, ParseError> {
        let start = self.expect_keyword(Keyword::CASE)?.span;
        let operand = if self.at_keyword(Keyword::WHEN)? {
            None
        } else {
            Some(self.parse_expr()?)
        };
        let mut branches = Vec::new();
        while self.eat_keyword(Keyword::WHEN)? {
            let when = self.parse_expr()?;
            self.expect_keyword(Keyword::THEN)?;
            let then = self.parse_expr()?;
            branches.push((when, then));
        }
        if branches.is_empty() {
            return Err(self.unexpected(&["WHEN"])?);
        }
        let otherwise = if self.eat_keyword(Keyword::ELSE)? {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let end = self.expect_keyword(Keyword::END)?.span;
        Ok(self.ast.add_expr(
            Expr::Case {
                operand,
                branches,
                otherwise,
            },
            start.to(end),
        ))
    }

    /// Parses `CAST(expr AS type)`.
    fn parse_cast(&mut self) -> Result<ExprId, ParseError> {
        let start = self.expect_keyword(Keyword::CAST)?.span;
        self.expect(Punctuator::LeftParen)?;
        let operand = self.parse_expr()?;
        self.expect_keyword(Keyword::AS)?;
        // SQLite's `typetoken` production is allowed to be empty, so
        // `CAST(1 AS)` is a legal cast to no affinity at all. It looks like a
        // typo and is not one; the differential run against 3.53.4 is what
        // established that the pinned release accepts it.
        let declared = if self.at(Punctuator::RightParen)? {
            let span = crate::lexer::Span::at(self.cursor());
            self.ast
                .intern(Vec::new(), crate::lexer::QuoteForm::Bare, span)
        } else {
            self.parse_type_name()?
        };
        let end = self.expect(Punctuator::RightParen)?.span;
        Ok(self
            .ast
            .add_expr(Expr::Cast { operand, declared }, start.to(end)))
    }

    /// Parses `RAISE(IGNORE)` or `RAISE(ROLLBACK|ABORT|FAIL, message)`.
    fn parse_raise(&mut self) -> Result<ExprId, ParseError> {
        let start = self.expect_keyword(Keyword::RAISE)?.span;
        self.expect(Punctuator::LeftParen)?;
        let action = if self.eat_keyword(Keyword::IGNORE)? {
            RaiseAction::Ignore
        } else if self.eat_keyword(Keyword::ROLLBACK)? {
            RaiseAction::Rollback
        } else if self.eat_keyword(Keyword::ABORT)? {
            RaiseAction::Abort
        } else if self.eat_keyword(Keyword::FAIL)? {
            RaiseAction::Fail
        } else {
            return Err(self.unexpected(&["IGNORE", "ROLLBACK", "ABORT", "FAIL"])?);
        };
        let message = if action == RaiseAction::Ignore {
            None
        } else {
            self.expect(Punctuator::Comma)?;
            let token = self.peek()?;
            if token.kind != TokenKind::String {
                return Err(self.unexpected(&["a string"])?);
            }
            self.bump()?;
            Some(lexer::string_text(self.source(), token).into_owned())
        };
        let end = self.expect(Punctuator::RightParen)?.span;
        Ok(self
            .ast
            .add_expr(Expr::Raise { action, message }, start.to(end)))
    }

    /// Parses a type name, which is a run of words with an optional size.
    ///
    /// SQLite accepts `UNSIGNED BIG INT` and `VARCHAR(255)` and keeps the whole
    /// written text, because affinity is decided by reading that text as
    /// characters rather than by recognising a type.
    ///
    /// The words are `ids`, not `idj`: `typename ::= ids` and
    /// `typename ::= typename ids`, so a join keyword or `INDEXED` is **not** a
    /// type name even though either is a column *name*. Measured against the
    /// pinned release: `CREATE TABLE t (a key)` parses and `CREATE TABLE t (a
    /// left)` does not, while `CREATE TABLE t (left TEXT)` parses because the
    /// two positions take different classes.
    pub(super) fn parse_type_name(&mut self) -> Result<crate::ast::NameId, ParseError> {
        let first = self.peek()?;
        if !Parser::token_is_plain_name(first) {
            return Err(self.unexpected(&["a type name"])?);
        }
        let mut end = first.span;
        self.bump()?;
        // A type name is one or more words, and it stops at the first word that
        // begins a column constraint. Without that stop, `d BLOB GENERATED
        // ALWAYS AS (...)` had the declared type `BLOB GENERATED ALWAYS` -
        // which `PRAGMA table_xinfo` then reported and which decides the
        // column's affinity.
        while self.at_plain_name()? && !self.at_constraint_keyword()? {
            end = self.bump()?.span;
        }
        if self.at(Punctuator::LeftParen)? {
            self.bump()?;
            loop {
                let token = self.peek()?;
                match token.kind {
                    TokenKind::Integer | TokenKind::Float => {
                        self.bump()?;
                    }
                    TokenKind::Punctuator(Punctuator::Plus)
                    | TokenKind::Punctuator(Punctuator::Minus) => {
                        self.bump()?;
                        continue;
                    }
                    _ => return Err(self.unexpected(&["a number"])?),
                }
                if !self.eat(Punctuator::Comma)? {
                    break;
                }
            }
            end = self.expect(Punctuator::RightParen)?.span;
        }
        let span = first.span.to(end);
        let text = span.slice(self.source()).to_vec();
        Ok(self.ast.intern(text, crate::lexer::QuoteForm::Bare, span))
    }

    /// Returns whether the next word begins a column constraint.
    fn at_constraint_keyword(&mut self) -> Result<bool, ParseError> {
        let Some(keyword) = self.peek()?.keyword() else {
            return Ok(false);
        };
        Ok(matches!(
            keyword,
            crate::keyword::Keyword::CONSTRAINT
                | crate::keyword::Keyword::PRIMARY
                | crate::keyword::Keyword::NOT
                | crate::keyword::Keyword::NULL
                | crate::keyword::Keyword::UNIQUE
                | crate::keyword::Keyword::CHECK
                | crate::keyword::Keyword::DEFAULT
                | crate::keyword::Keyword::COLLATE
                | crate::keyword::Keyword::REFERENCES
                | crate::keyword::Keyword::GENERATED
                | crate::keyword::Keyword::AS
        ))
    }

    /// Parses one infix or postfix form, if the next token binds tightly
    /// enough. Returns `None` when the expression is finished.
    fn parse_suffix(&mut self, left: ExprId, minimum: Power) -> Result<Option<ExprId>, ParseError> {
        let token = self.peek()?;
        if let TokenKind::Punctuator(punctuator) = token.kind {
            let Some(power) = precedence::infix_power(punctuator) else {
                return Ok(None);
            };
            if power < minimum {
                return Ok(None);
            }
            self.bump()?;
            let op = binary_op(punctuator);
            let right = self.parse_expr_bp(Power(power.0.saturating_add(1)))?;
            let span = self.ast.expr_span(left).to(self.ast.expr_span(right));
            return Ok(Some(
                self.ast.add_expr(Expr::Binary { op, left, right }, span),
            ));
        }
        let Some(keyword) = token.keyword() else {
            return Ok(None);
        };
        match keyword {
            Keyword::OR if precedence::OR >= minimum => {
                self.bump()?;
                self.parse_binary_keyword(left, BinaryOp::Or, precedence::OR)
            }
            Keyword::AND if precedence::AND >= minimum => {
                self.bump()?;
                self.parse_binary_keyword(left, BinaryOp::And, precedence::AND)
            }
            Keyword::COLLATE if precedence::COLLATE >= minimum => {
                self.bump()?;
                let collation = self.parse_name()?;
                let span = self.ast.expr_span(left).to(self
                    .ast
                    .name(collation)
                    .map(|n| n.span)
                    .unwrap_or_default());
                Ok(Some(self.ast.add_expr(
                    Expr::Collate {
                        operand: left,
                        collation,
                    },
                    span,
                )))
            }
            Keyword::ISNULL | Keyword::NOTNULL if precedence::COMPARISON >= minimum => {
                let token = self.bump()?;
                let span = self.ast.expr_span(left).to(token.span);
                Ok(Some(self.ast.add_expr(
                    Expr::IsNull {
                        negated: keyword == Keyword::NOTNULL,
                        operand: left,
                    },
                    span,
                )))
            }
            Keyword::IS if precedence::COMPARISON >= minimum => self.parse_is(left),
            Keyword::IN if precedence::COMPARISON >= minimum => self.parse_in(left, false),
            Keyword::BETWEEN if precedence::COMPARISON >= minimum => {
                self.parse_between(left, false)
            }
            Keyword::LIKE | Keyword::GLOB | Keyword::REGEXP | Keyword::MATCH
                if precedence::COMPARISON >= minimum =>
            {
                self.parse_pattern(left, false)
            }
            Keyword::NOT if precedence::COMPARISON >= minimum => self.parse_negated_suffix(left),
            _ => Ok(None),
        }
    }

    /// Parses the right-hand side of a keyword-spelled binary operator.
    fn parse_binary_keyword(
        &mut self,
        left: ExprId,
        op: BinaryOp,
        power: Power,
    ) -> Result<Option<ExprId>, ParseError> {
        let right = self.parse_expr_bp(Power(power.0.saturating_add(1)))?;
        let span = self.ast.expr_span(left).to(self.ast.expr_span(right));
        Ok(Some(
            self.ast.add_expr(Expr::Binary { op, left, right }, span),
        ))
    }

    /// Parses the four `NOT`-prefixed suffix forms.
    fn parse_negated_suffix(&mut self, left: ExprId) -> Result<Option<ExprId>, ParseError> {
        let after = self.peek_at(1)?.keyword();
        match after {
            Some(Keyword::IN) => {
                self.bump()?;
                self.parse_in(left, true)
            }
            Some(Keyword::BETWEEN) => {
                self.bump()?;
                self.parse_between(left, true)
            }
            Some(Keyword::LIKE)
            | Some(Keyword::GLOB)
            | Some(Keyword::REGEXP)
            | Some(Keyword::MATCH) => {
                self.bump()?;
                self.parse_pattern(left, true)
            }
            Some(Keyword::NULL) => {
                self.bump()?;
                let end = self.bump()?.span;
                let span = self.ast.expr_span(left).to(end);
                Ok(Some(self.ast.add_expr(
                    Expr::IsNull {
                        negated: true,
                        operand: left,
                    },
                    span,
                )))
            }
            // A bare `NOT` after an expression is not a suffix at all; the
            // expression has ended and the caller decides what follows.
            _ => Ok(None),
        }
    }

    /// Parses `IS [NOT] [DISTINCT FROM] right` and `IS [NOT] NULL`.
    fn parse_is(&mut self, left: ExprId) -> Result<Option<ExprId>, ParseError> {
        self.expect_keyword(Keyword::IS)?;
        let negated = self.eat_keyword(Keyword::NOT)?;
        if self.at_keyword(Keyword::NULL)? {
            let end = self.bump()?.span;
            let span = self.ast.expr_span(left).to(end);
            return Ok(Some(self.ast.add_expr(
                Expr::IsNull {
                    negated,
                    operand: left,
                },
                span,
            )));
        }
        let distinct_from = if self.at_keyword(Keyword::DISTINCT)? {
            self.bump()?;
            self.expect_keyword(Keyword::FROM)?;
            true
        } else {
            false
        };
        let right = self.parse_expr_bp(Power(precedence::COMPARISON.0.saturating_add(1)))?;
        let span = self.ast.expr_span(left).to(self.ast.expr_span(right));
        Ok(Some(self.ast.add_expr(
            Expr::Is {
                negated,
                distinct_from,
                left,
                right,
            },
            span,
        )))
    }

    /// Parses `[NOT] IN (list | select | table)`.
    fn parse_in(&mut self, left: ExprId, negated: bool) -> Result<Option<ExprId>, ParseError> {
        self.expect_keyword(Keyword::IN)?;
        // Every arm below sets this before it is read; the table form sets it
        // twice, because the arguments move the end past the name.
        let mut end;
        let rhs = if self.at(Punctuator::LeftParen)? {
            self.bump()?;
            if self.at(Punctuator::RightParen)? {
                end = self.bump()?.span;
                InRhs::List(Vec::new())
            } else if self.at_keyword(Keyword::SELECT)?
                || self.at_keyword(Keyword::WITH)?
                || self.at_keyword(Keyword::VALUES)?
            {
                let select = self.parse_select()?;
                end = self.expect(Punctuator::RightParen)?.span;
                InRhs::Select(select)
            } else {
                let mut values = Vec::new();
                loop {
                    values.push(self.parse_expr()?);
                    if !self.eat(Punctuator::Comma)? {
                        break;
                    }
                }
                end = self.expect(Punctuator::RightParen)?.span;
                InRhs::List(values)
            }
        } else {
            let (database, table) = self.parse_qualified_name()?;
            end = self.ast.name(table).map(|n| n.span).unwrap_or_default();
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
                end = self.expect(Punctuator::RightParen)?.span;
                Some(list)
            } else {
                None
            };
            InRhs::Table {
                database,
                table,
                arguments,
            }
        };
        let span = self.ast.expr_span(left).to(end);
        Ok(Some(self.ast.add_expr(
            Expr::In {
                negated,
                operand: left,
                rhs,
            },
            span,
        )))
    }

    /// Parses `[NOT] BETWEEN low AND high`.
    ///
    /// The bounds are parsed above comparison precedence so that the `AND` that
    /// separates them is not read as the boolean operator, which is the one
    /// place SQL's grammar is genuinely ambiguous without this rule.
    fn parse_between(&mut self, left: ExprId, negated: bool) -> Result<Option<ExprId>, ParseError> {
        self.expect_keyword(Keyword::BETWEEN)?;
        let low = self.parse_expr_bp(Power(precedence::COMPARISON.0.saturating_add(1)))?;
        self.expect_keyword(Keyword::AND)?;
        let high = self.parse_expr_bp(Power(precedence::COMPARISON.0.saturating_add(1)))?;
        let span = self.ast.expr_span(left).to(self.ast.expr_span(high));
        Ok(Some(self.ast.add_expr(
            Expr::Between {
                negated,
                operand: left,
                low,
                high,
            },
            span,
        )))
    }

    /// Parses `[NOT] LIKE|GLOB|REGEXP|MATCH pattern [ESCAPE expr]`.
    fn parse_pattern(&mut self, left: ExprId, negated: bool) -> Result<Option<ExprId>, ParseError> {
        let token = self.bump()?;
        let op = match token.keyword() {
            Some(Keyword::LIKE) => PatternOp::Like,
            Some(Keyword::GLOB) => PatternOp::Glob,
            Some(Keyword::REGEXP) => PatternOp::Regexp,
            _ => PatternOp::Match,
        };
        let pattern = self.parse_expr_bp(Power(precedence::COMPARISON.0.saturating_add(1)))?;
        let mut end = self.ast.expr_span(pattern);
        let escape = if self.at_keyword(Keyword::ESCAPE)? {
            self.bump()?;
            let escape = self.parse_expr_bp(Power(precedence::COMPARISON.0.saturating_add(1)))?;
            end = self.ast.expr_span(escape);
            Some(escape)
        } else {
            None
        };
        let span = self.ast.expr_span(left).to(end);
        Ok(Some(self.ast.add_expr(
            Expr::Pattern {
                negated,
                op,
                operand: left,
                pattern,
                escape,
            },
            span,
        )))
    }

    /// Parses a comma-separated `ORDER BY` term list.
    pub(super) fn parse_order_terms(&mut self) -> Result<Vec<crate::ast::OrderTerm>, ParseError> {
        use crate::ast::{NullOrder, OrderTerm};
        let mut terms = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            let order = if self.eat_keyword(Keyword::ASC)? {
                SortOrder::Ascending
            } else if self.eat_keyword(Keyword::DESC)? {
                SortOrder::Descending
            } else {
                SortOrder::Ascending
            };
            let nulls = if self.eat_keyword(Keyword::NULLS)? {
                if self.eat_keyword(Keyword::FIRST)? {
                    Some(NullOrder::First)
                } else {
                    self.expect_keyword(Keyword::LAST)?;
                    Some(NullOrder::Last)
                }
            } else {
                None
            };
            terms.push(OrderTerm { expr, order, nulls });
            if !self.eat(Punctuator::Comma)? {
                return Ok(terms);
            }
        }
    }
}

/// Maps a punctuator to the binary operator it spells.
fn binary_op(punctuator: Punctuator) -> BinaryOp {
    match punctuator {
        Punctuator::Equal => BinaryOp::Equal,
        Punctuator::NotEqual => BinaryOp::NotEqual,
        Punctuator::Less => BinaryOp::Less,
        Punctuator::LessEqual => BinaryOp::LessEqual,
        Punctuator::Greater => BinaryOp::Greater,
        Punctuator::GreaterEqual => BinaryOp::GreaterEqual,
        Punctuator::Plus => BinaryOp::Add,
        Punctuator::Minus => BinaryOp::Subtract,
        Punctuator::Star => BinaryOp::Multiply,
        Punctuator::Slash => BinaryOp::Divide,
        Punctuator::Percent => BinaryOp::Modulo,
        Punctuator::Concat => BinaryOp::Concat,
        Punctuator::BitAnd => BinaryOp::BitAnd,
        Punctuator::BitOr => BinaryOp::BitOr,
        Punctuator::ShiftLeft => BinaryOp::ShiftLeft,
        Punctuator::ShiftRight => BinaryOp::ShiftRight,
        Punctuator::Arrow => BinaryOp::Extract,
        Punctuator::DoubleArrow => BinaryOp::ExtractText,
        Punctuator::L2Distance => BinaryOp::L2Distance,
        Punctuator::CosineDistance => BinaryOp::CosineDistance,
        Punctuator::NegativeInnerProduct => BinaryOp::NegativeInnerProduct,
        Punctuator::L1Distance => BinaryOp::L1Distance,
        Punctuator::HammingDistance => BinaryOp::HammingDistance,
        Punctuator::JaccardDistance => BinaryOp::JaccardDistance,
        // Every punctuator that reaches here has an infix power, and every
        // punctuator with an infix power is listed above.
        _ => BinaryOp::Equal,
    }
}
