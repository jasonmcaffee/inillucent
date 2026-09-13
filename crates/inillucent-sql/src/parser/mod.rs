//! The recursive-descent statement parser.
//!
//! Invariant: the parser performs no I/O and consults no catalog. It turns
//! bytes into an arena tree and nothing else, which is what lets a syntax error
//! be reported before a file is opened and lets the same parser run inside the
//! catalog loader on the CREATE text stored in `sqlite_schema`.
//!
//! Depth is bounded before allocation rather than after: every recursive entry
//! charges the expression-depth limit, so an adversarial `((((((...` fails with
//! a limit error at a known offset instead of growing the arena until something
//! else notices.
//!
//! One call parses one statement and reports how many bytes it consumed, which
//! is SQLite's prepare contract: the caller gets a statement and the unused
//! tail, and an empty statement succeeds with no program.

mod ddl;
mod dml;
mod expr;
mod select;

use inillucent_base::limits::{Limit, Limits};

use crate::ast::{Ast, NameId, Statement};
use crate::diagnostic::{ParseError, ParseErrorKind};
use crate::keyword::Keyword;
use crate::lexer::{self, Lexer, Punctuator, QuoteForm, Span, Token, TokenKind};

/// Where a statement's parameters ended up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParameterMap {
    /// The highest parameter index the statement used.
    pub count: u32,
    /// Named parameters and the index each was assigned.
    pub names: Vec<(Vec<u8>, u32)>,
}

impl ParameterMap {
    /// Returns the index a named parameter was assigned.
    pub fn index_of(&self, name: &[u8]) -> Option<u32> {
        self.names
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, index)| *index)
    }
}

/// One parsed statement and everything the caller needs to continue.
#[derive(Clone, Debug)]
pub struct ParsedStatement {
    /// The arena holding every node.
    pub ast: Ast,
    /// The statement itself.
    pub statement: Statement,
    /// How many bytes of the source this statement consumed, including its
    /// terminating semicolon and any trivia before the next statement.
    pub consumed: usize,
    /// The parameters the statement declared.
    pub parameters: ParameterMap,
    /// The span of the statement text itself, without the trailing trivia.
    pub span: Span,
}

/// What a statement is, decided without a full parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementClass {
    /// The statement reads and does not write.
    ReadOnly,
    /// The statement writes.
    Write,
    /// The statement changes the schema.
    SchemaChange,
    /// The statement controls a transaction.
    TransactionControl,
    /// The statement is a PRAGMA, which may do either.
    Pragma,
    /// There is no statement here.
    Empty,
    /// The text does not begin a statement at all.
    Unknown,
}

/// The parser: a lexer, a token buffer, an arena, and a depth charge.
pub struct Parser<'a> {
    source: &'a [u8],
    lexer: Lexer<'a>,
    buffer: Vec<Token>,
    ast: Ast,
    limits: &'a Limits,
    depth: i64,
    parameters: ParameterMap,
    /// How many `SELECT`s this parse has read.
    ///
    /// **A counter rather than a walk over what was parsed.** `CHECK` is the
    /// one place the grammar has to know whether an expression contained a
    /// subquery, and comparing this before and after the expression answers it
    /// exactly - where a recursive scan of the arena would be a second
    /// enumeration of every expression node, to be kept in step with the first
    /// for ever. See `no_subquery_in_check`.
    selects: u64,
}

impl<'a> Parser<'a> {
    /// Returns a parser positioned at an offset in the source.
    pub fn new(source: &'a [u8], offset: usize, limits: &'a Limits) -> Parser<'a> {
        Parser::with_arena(source, offset, limits, Ast::new())
    }

    /// Returns a parser that fills an arena the caller supplies.
    ///
    /// **For a caller that parses one statement after another.** The arena is
    /// cleared rather than dropped, so the second parse pushes into capacity
    /// the first one took - see [`Ast::clear`]. Nothing else differs: the arena
    /// is filled and handed back exactly as `new`'s own is.
    ///
    /// @param source - the SQL text
    /// @param offset - where in it this statement starts
    /// @param limits - the limits to enforce
    /// @param arena - the arena to fill, cleared first
    pub fn with_arena(
        source: &'a [u8],
        offset: usize,
        limits: &'a Limits,
        mut arena: Ast,
    ) -> Parser<'a> {
        arena.clear();
        Parser {
            source,
            lexer: Lexer::at(source, offset),
            buffer: Vec::new(),
            ast: arena,
            limits,
            depth: 0,
            parameters: ParameterMap::default(),
            selects: 0,
        }
    }

    /// Returns the arena, for a caller that owns the parse.
    pub fn into_ast(self) -> Ast {
        self.ast
    }

    /// Returns the source being parsed.
    pub fn source(&self) -> &'a [u8] {
        self.source
    }

    /// Fills the lookahead buffer to at least `wanted` tokens.
    fn fill(&mut self, wanted: usize) -> Result<(), ParseError> {
        while self.buffer.len() < wanted {
            let token = self.lexer.next_token()?;
            let end = token.kind == TokenKind::EndOfInput;
            self.buffer.push(token);
            if end {
                break;
            }
        }
        Ok(())
    }

    /// Returns the token `ahead` positions from the cursor.
    fn peek_at(&mut self, ahead: usize) -> Result<Token, ParseError> {
        self.fill(ahead.saturating_add(1))?;
        Ok(self.buffer.get(ahead).copied().unwrap_or(Token {
            kind: TokenKind::EndOfInput,
            span: Span::at(self.source.len()),
        }))
    }

    /// Returns the next token without consuming it.
    fn peek(&mut self) -> Result<Token, ParseError> {
        self.peek_at(0)
    }

    /// Consumes and returns the next token.
    fn bump(&mut self) -> Result<Token, ParseError> {
        let token = self.peek()?;
        if token.kind != TokenKind::EndOfInput && !self.buffer.is_empty() {
            self.buffer.remove(0);
        }
        Ok(token)
    }

    /// Returns the byte offset the cursor sits at.
    fn cursor(&mut self) -> usize {
        match self.buffer.first() {
            Some(token) => token.span.start as usize,
            None => self.lexer.offset(),
        }
    }

    /// Returns whether the next token spells a keyword.
    fn at_keyword(&mut self, keyword: Keyword) -> Result<bool, ParseError> {
        Ok(self.peek()?.keyword() == Some(keyword))
    }

    /// Returns whether the token `ahead` positions away spells a keyword.
    fn at_keyword_ahead(&mut self, ahead: usize, keyword: Keyword) -> Result<bool, ParseError> {
        Ok(self.peek_at(ahead)?.keyword() == Some(keyword))
    }

    /// Consumes a keyword if it is next, reporting whether it was.
    fn eat_keyword(&mut self, keyword: Keyword) -> Result<bool, ParseError> {
        if self.at_keyword(keyword)? {
            self.bump()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Consumes a keyword, failing with the keyword as the expected set.
    fn expect_keyword(&mut self, keyword: Keyword) -> Result<Token, ParseError> {
        if self.at_keyword(keyword)? {
            return self.bump();
        }
        Err(self.unexpected(&[keyword.as_str()])?)
    }

    /// Returns whether the next token is a punctuator.
    fn at(&mut self, punctuator: Punctuator) -> Result<bool, ParseError> {
        Ok(self.peek()?.is(punctuator))
    }

    /// Consumes a punctuator if it is next, reporting whether it was.
    fn eat(&mut self, punctuator: Punctuator) -> Result<bool, ParseError> {
        if self.at(punctuator)? {
            self.bump()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Consumes a punctuator, failing with it as the expected set.
    fn expect(&mut self, punctuator: Punctuator) -> Result<Token, ParseError> {
        if self.at(punctuator)? {
            return self.bump();
        }
        Err(self.unexpected(&[punctuator.as_str()])?)
    }

    /// Builds the failure for whatever token is next.
    fn unexpected(&mut self, expected: &[&'static str]) -> Result<ParseError, ParseError> {
        let token = self.peek()?;
        let kind = if token.kind == TokenKind::EndOfInput {
            ParseErrorKind::UnexpectedEnd {
                expected: expected.to_vec(),
            }
        } else {
            ParseErrorKind::Unexpected {
                found: String::from_utf8_lossy(token.text(self.source)).into_owned(),
                expected: expected.to_vec(),
            }
        };
        Ok(ParseError::new(kind, token.span))
    }

    /// Charges one level of recursion against the parser's own depth limit.
    ///
    /// **Not `ExprDepth`, which is a different measurement.** This counts how
    /// deep the recursive descent has gone; `ExprDepth` counts how deep the
    /// expression *tree* is, and the two differ by every redundant
    /// parenthesis - `((((1))))` is four of one and one of the other. Charging
    /// the parser's recursion against the tree's limit refused
    /// `SELECT ((( ... 1 ... )))` at a thousand parentheses, which the
    /// reference accepts because its parser stack is allowed 2500.
    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > self.limits.get(Limit::ParserDepth) {
            let span = Span::at(self.cursor());
            return Err(ParseError::new(
                ParseErrorKind::LimitExceeded("parser stack depth"),
                span,
            ));
        }
        Ok(())
    }

    /// Releases one level of recursion.
    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Charges the expression tree's own depth against `Limit::ExprDepth`.
    ///
    /// **`ExprDepth` was declared in `compat/limits.toml` and enforced nowhere
    /// (task-1932, H8).** `enter`/`leave` above charge `ParserDepth`, which
    /// counts recursion, and that is a different measurement: a flat chain
    /// `a1 = 1 AND a2 = 2 AND ...` enters and leaves `parse_expr_bp` once per
    /// term, so the recursion counter never accumulates, while the tree grows
    /// one level per term with nothing counting it. Under the 1 GiB
    /// `SqlLength` default that is a tree tens of millions of levels deep,
    /// accepted here and then walked recursively by the binder, the planner and
    /// the executor - each of which overflows the stack somewhere nobody
    /// measured. SQLite refuses at depth 1000.
    ///
    /// It is charged here rather than inside `Ast::add_expr` because
    /// `add_expr` is infallible and called from about a hundred places; this is
    /// one call in the Pratt loop, which every expression node passes through,
    /// so a chain is refused after the term that crossed the limit rather than
    /// after the whole statement is built.
    fn charge_expr_depth(&mut self) -> Result<(), ParseError> {
        if i64::from(self.ast.max_expr_depth()) > self.limits.get(Limit::ExprDepth) {
            let span = Span::at(self.cursor());
            return Err(ParseError::new(
                ParseErrorKind::LimitExceeded("expression tree depth"),
                span,
            ));
        }
        // **The identifier count, charged at the same place and for the same
        // reason.** `Ast::intern` is a hash lookup as of task-1932 and no
        // longer quadratic, but a statement can still name arbitrarily many
        // distinct identifiers under the `SqlLength` default, and every one of
        // them is a `Name` holding two copies of its text. `Limit::Column` is
        // the closest declared bound and this is deliberately generous against
        // it - a name is a column, a table, an alias, a function or a
        // collation, so one honest statement interns several times as many
        // names as any one table has columns.
        let names = i64::try_from(self.ast.name_count()).unwrap_or(i64::MAX);
        if names > self.limits.get(Limit::Column).saturating_mul(64) {
            let span = Span::at(self.cursor());
            return Err(ParseError::new(
                ParseErrorKind::LimitExceeded("distinct identifiers"),
                span,
            ));
        }
        Ok(())
    }

    /// Returns whether a token may be read as a name here.
    ///
    /// A quoted word is always a name. A bare word is a name unless it is a
    /// hard keyword, where "hard" is [`Keyword::may_be_name`] - SQLite's
    /// `nm ::= idj | STRING` with `idj ::= ID|INDEXED|JOIN_KW`, so the fallback
    /// set plus the seven join keywords plus `INDEXED`. This is the
    /// per-position question SQLite's grammar asks, asked in the one place that
    /// can answer it.
    ///
    /// It used to ask [`Keyword::may_fall_back`], which is the
    /// narrower of the two sets and made `CREATE TABLE pairs (left TEXT)` - a
    /// schema SQLite itself writes - a syntax error.
    fn token_is_name(token: Token) -> bool {
        match token.kind {
            TokenKind::Identifier { keyword, quote } => match quote {
                QuoteForm::Bare => keyword.is_none_or(Keyword::may_be_name),
                _ => true,
            },
            _ => false,
        }
    }

    /// Returns whether a token may be read where SQLite's grammar writes `ids`.
    ///
    /// `%token_class ids ID|STRING` - deliberately narrower than
    /// [`Parser::token_is_name`], which is the `idj` class and takes the join
    /// keywords and `INDEXED` as well. **Two** positions in the grammar take
    /// the narrow class and both were measured against the pinned release:
    ///
    /// - a bare alias, `as ::= ids`. This is what stops a join keyword being
    ///   eaten as the alias of the table before it: `SELECT * FROM t LEFT JOIN
    ///   u` is a join and `SELECT a left FROM t` is a syntax error, in SQLite
    ///   and here. The same rule leaves `INDEXED` for `INDEXED BY` to claim.
    /// - a declared type, `typename ::= ids`. `CREATE TABLE t (a left)` is a
    ///   syntax error in SQLite even though `CREATE TABLE t (left a)` is not,
    ///   because the name position and the type position take different
    ///   classes. `CREATE TABLE t (a key)` parses, because `KEY` is in the
    ///   fallback set and so lexes as `ID`.
    fn token_is_plain_name(token: Token) -> bool {
        match token.kind {
            TokenKind::Identifier { keyword, quote } => match quote {
                QuoteForm::Bare => keyword.is_none_or(Keyword::may_fall_back),
                _ => true,
            },
            _ => false,
        }
    }

    /// Reports whether a token is a word, keyword or not.
    ///
    /// It is deliberately weaker than [`Parser::token_is_name`], which asks
    /// whether a word may stand where an identifier is expected. Some
    /// positions - a pragma's value is the one - accept the spelling of a
    /// reserved word because nothing else can appear there.
    fn token_is_word(token: Token) -> bool {
        matches!(token.kind, TokenKind::Identifier { .. })
    }

    /// Returns whether the next token may be read as a name.
    fn at_name(&mut self) -> Result<bool, ParseError> {
        Ok(Parser::token_is_name(self.peek()?))
    }

    /// Returns whether the next token may be read where the grammar writes
    /// `ids` - a bare alias, or a declared type name.
    fn at_plain_name(&mut self) -> Result<bool, ParseError> {
        Ok(Parser::token_is_plain_name(self.peek()?))
    }

    /// Refuses a subquery where SQLite refuses one.
    ///
    /// `CHECK (a IN (SELECT ...))` is `subqueries prohibited in CHECK
    /// constraints` in SQLite and was **accepted** here - a constraint that
    /// would be evaluated per row against a query, which this engine has no
    /// intention of doing, so the declaration was being stored and not
    /// enforced. That is the shape of failure the whole `CHECK` work exists to
    /// avoid: a declaration the application trusts, doing nothing.
    ///
    /// @param before - the `SELECT` count taken before the expression
    /// @param span - where the constraint was written
    fn no_subquery_in_check(&mut self, before: u64, span: Span) -> Result<(), ParseError> {
        if self.selects == before {
            return Ok(());
        }
        Err(ParseError::new(
            ParseErrorKind::Refused("subqueries prohibited in CHECK constraints".to_string()),
            span,
        ))
    }

    /// Consumes an identifier, interning it.
    fn parse_name(&mut self) -> Result<NameId, ParseError> {
        Ok(self.parse_name_spanned()?.0)
    }

    /// Parses an identifier and returns where it was written.
    ///
    /// The written position and the interned name's position are not the same
    /// thing, and confusing them is a real bug rather than a cosmetic one.
    /// Interning deduplicates, so the name `b` in `CHECK (b > 0)` resolves to
    /// the entry the *column declaration* `b INTEGER` created, and that entry
    /// carries the declaration's span. Building the expression's span from it
    /// made `CHECK (b > 0)` claim to span `b INTEGER CHECK (b > 0`, which the
    /// catalog then stored as the constraint's source and could not reparse.
    /// The token's own span is the only one that describes this occurrence.
    fn parse_name_spanned(&mut self) -> Result<(NameId, Span), ParseError> {
        let token = self.peek()?;
        // **A string literal where a name is required is a name.** SQLite's own
        // documented misfeature, and not an academic one: SQLite *writes*
        // `CREATE TABLE 'f_data'(id INTEGER PRIMARY KEY, block BLOB)` into
        // `sqlite_schema` for an FTS5 table's shadow storage, so a migration
        // that could not read it reported "the declaration of f_data did not
        // parse: database disk image is malformed" about a perfectly good file.
        //
        // Only here, where the grammar *requires* a name - the lookahead
        // `at_name` is deliberately left alone, so nothing about which
        // alternative the parser takes changes. Accepting a string in a
        // required position can only turn a parse error into a parse.
        if !Parser::token_is_name(token) && token.kind != TokenKind::String {
            return Err(self.unexpected(&["a name"])?);
        }
        self.bump()?;
        Ok((self.intern_token(token), token.span))
    }

    /// Interns an identifier token into the arena.
    fn intern_token(&mut self, token: Token) -> NameId {
        // A string standing in for a name is interned as the name it spells,
        // with its own quoting undone - and remembered as double-quoted, which
        // is how it is written back out when the declaration is rendered.
        if token.kind == TokenKind::String {
            let text = lexer::string_text(self.source, token).into_owned();
            return self.ast.intern(text, QuoteForm::Double, token.span);
        }
        let quote = match token.kind {
            TokenKind::Identifier { quote, .. } => quote,
            _ => QuoteForm::Bare,
        };
        let text = lexer::identifier_text(self.source, token).into_owned();
        self.ast.intern(text, quote, token.span)
    }

    /// Parses an optional `schema.` qualifier followed by a name.
    ///
    /// Returns the qualifier and the name. The lookahead is what distinguishes
    /// `main.t` from a column reference; the dot has to be there *and* be
    /// followed by a name for the first word to be a qualifier.
    fn parse_qualified_name(&mut self) -> Result<(Option<NameId>, NameId), ParseError> {
        let first = self.parse_name()?;
        if self.at(Punctuator::Dot)? && Parser::token_is_name(self.peek_at(1)?) {
            self.bump()?;
            let second = self.parse_name()?;
            return Ok((Some(first), second));
        }
        Ok((None, first))
    }

    /// Parses an optional `AS alias` or bare alias.
    fn parse_alias(&mut self) -> Result<(Option<NameId>, bool), ParseError> {
        if self.eat_keyword(Keyword::AS)? {
            let name = self.parse_name()?;
            return Ok((Some(name), true));
        }
        // A bare alias is any word in the *fallback* set - not the wider name
        // set, which would read the `LEFT` of `FROM t LEFT JOIN u` as an alias
        // and the `INDEXED` of `FROM t INDEXED BY i` as one too. SQLite draws
        // the same line, in the same place, for the same reason.
        //
        // `WINDOW` needs one more exception on top of that: it *is* in the
        // fallback set, so `FROM t WINDOW w AS (...)` would read `WINDOW` as
        // the table's alias and then choke on `w`. SQLite's own grammar gives
        // it the same special treatment.
        if self.at_keyword(Keyword::WINDOW)? {
            return Ok((None, false));
        }
        if Parser::token_is_plain_name(self.peek()?) {
            let name = self.parse_name()?;
            return Ok((Some(name), false));
        }
        Ok((None, false))
    }

    /// Records a parameter and returns the index it was assigned.
    ///
    /// SQLite's rule is that a bare `?` takes one past the highest index used
    /// so far, an explicit `?NNN` takes exactly NNN and raises the high-water
    /// mark, and a repeated `:name` reuses the index the first occurrence got.
    fn assign_parameter(&mut self, token: Token) -> Result<(u32, Option<NameId>), ParseError> {
        let text = token.text(self.source);
        let sigil = text.first().copied().unwrap_or(b'?');
        let limit = self.limits.get(Limit::VariableNumber).max(0) as u32;
        if sigil == b'?' && text.len() > 1 {
            let digits = text.get(1..).unwrap_or(&[]);
            let mut index: u32 = 0;
            for byte in digits {
                index = index
                    .saturating_mul(10)
                    .saturating_add(u32::from(byte.saturating_sub(b'0')));
            }
            if index == 0 || index > limit {
                return Err(ParseError::new(
                    ParseErrorKind::LimitExceeded("variable number"),
                    token.span,
                ));
            }
            self.parameters.count = self.parameters.count.max(index);
            return Ok((index, None));
        }
        if sigil == b'?' {
            let index = self.parameters.count.saturating_add(1);
            if index > limit {
                return Err(ParseError::new(
                    ParseErrorKind::LimitExceeded("variable number"),
                    token.span,
                ));
            }
            self.parameters.count = index;
            return Ok((index, None));
        }
        let name = text.to_vec();
        if let Some(index) = self.parameters.index_of(&name) {
            let id = self.ast.intern(name, QuoteForm::Bare, token.span);
            return Ok((index, Some(id)));
        }
        let index = self.parameters.count.saturating_add(1);
        if index > limit {
            return Err(ParseError::new(
                ParseErrorKind::LimitExceeded("variable number"),
                token.span,
            ));
        }
        self.parameters.count = index;
        self.parameters.names.push((name.clone(), index));
        let id = self.ast.intern(name, QuoteForm::Bare, token.span);
        Ok((index, Some(id)))
    }

    /// Parses one statement, without the `EXPLAIN` prefix or the terminator.
    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        self.enter()?;
        let parsed = self.parse_statement_inner();
        self.leave();
        parsed
    }

    /// Dispatches on the leading keyword.
    fn parse_statement_inner(&mut self) -> Result<Statement, ParseError> {
        let token = self.peek()?;
        if token.kind == TokenKind::EndOfInput {
            return Ok(Statement::Empty);
        }
        if token.is(Punctuator::Semicolon) {
            return Ok(Statement::Empty);
        }
        let Some(keyword) = token.keyword() else {
            return Err(self.unexpected(&["a statement"])?);
        };
        match keyword {
            Keyword::EXPLAIN => self.parse_explain(),
            Keyword::WITH => self.parse_after_with(),
            Keyword::SELECT | Keyword::VALUES => self.parse_select_statement(),
            Keyword::INSERT | Keyword::REPLACE => self.parse_insert(),
            Keyword::UPDATE => self.parse_update(),
            Keyword::DELETE => self.parse_delete(),
            Keyword::CREATE => self.parse_create(),
            Keyword::DROP => self.parse_drop(),
            Keyword::ALTER => self.parse_alter(),
            Keyword::BEGIN => self.parse_begin(),
            Keyword::COMMIT | Keyword::END => self.parse_commit(),
            Keyword::ROLLBACK => self.parse_rollback(),
            Keyword::SAVEPOINT => self.parse_savepoint(),
            Keyword::RELEASE => self.parse_release(),
            Keyword::PRAGMA => self.parse_pragma(),
            Keyword::ATTACH => self.parse_attach(),
            Keyword::DETACH => self.parse_detach(),
            Keyword::VACUUM => self.parse_vacuum(),
            Keyword::ANALYZE => self.parse_analyze(),
            Keyword::REINDEX => self.parse_reindex(),
            _ => Err(self.unexpected(&["a statement"])?),
        }
    }

    /// Dispatches a statement that begins with a `WITH` prefix.
    ///
    /// The prefix does not say what follows it: `WITH c AS (...)` may lead to a
    /// SELECT, an INSERT, an UPDATE or a DELETE, and the CTE bodies in between
    /// contain SELECTs of their own. The scan therefore counts parentheses and
    /// takes the first statement keyword at depth zero; taking the first one at
    /// any depth reads `WITH c AS (SELECT 1) DELETE FROM t` as a query.
    fn parse_after_with(&mut self) -> Result<Statement, ParseError> {
        let mut ahead = 1usize;
        let mut depth = 0usize;
        loop {
            let token = self.peek_at(ahead)?;
            match token.kind {
                TokenKind::EndOfInput => return Err(self.unexpected(&["a statement"])?),
                TokenKind::Punctuator(Punctuator::LeftParen) => depth = depth.saturating_add(1),
                TokenKind::Punctuator(Punctuator::RightParen) => depth = depth.saturating_sub(1),
                _ if depth == 0 => match token.keyword() {
                    Some(Keyword::SELECT) | Some(Keyword::VALUES) => {
                        return self.parse_select_statement()
                    }
                    Some(Keyword::INSERT) | Some(Keyword::REPLACE) => return self.parse_insert(),
                    Some(Keyword::UPDATE) => return self.parse_update(),
                    Some(Keyword::DELETE) => return self.parse_delete(),
                    _ => {}
                },
                _ => {}
            }
            ahead = ahead.saturating_add(1);
        }
    }

    /// Parses `EXPLAIN [QUERY PLAN] <statement>`.
    fn parse_explain(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::EXPLAIN)?;
        let query_plan = if self.at_keyword(Keyword::QUERY)? {
            self.bump()?;
            self.expect_keyword(Keyword::PLAN)?;
            true
        } else {
            false
        };
        let inner = self.parse_statement()?;
        if inner == Statement::Empty {
            // `EXPLAIN;` is not a statement with nothing in it, it is a missing
            // statement, and SQLite reports it as a syntax error.
            return Err(self.unexpected(&["a statement to explain"])?);
        }
        Ok(Statement::Explain {
            query_plan,
            inner: Box::new(inner),
        })
    }

    /// Parses `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION]`.
    fn parse_begin(&mut self) -> Result<Statement, ParseError> {
        use crate::ast::TransactionBehaviour;
        self.expect_keyword(Keyword::BEGIN)?;
        let behaviour = if self.eat_keyword(Keyword::DEFERRED)? {
            Some(TransactionBehaviour::Deferred)
        } else if self.eat_keyword(Keyword::IMMEDIATE)? {
            Some(TransactionBehaviour::Immediate)
        } else if self.eat_keyword(Keyword::EXCLUSIVE)? {
            Some(TransactionBehaviour::Exclusive)
        } else {
            None
        };
        self.eat_keyword(Keyword::TRANSACTION)?;
        Ok(Statement::Begin { behaviour })
    }

    /// Parses `COMMIT|END [TRANSACTION]`.
    fn parse_commit(&mut self) -> Result<Statement, ParseError> {
        self.bump()?;
        self.eat_keyword(Keyword::TRANSACTION)?;
        Ok(Statement::Commit)
    }

    /// Parses `ROLLBACK [TRANSACTION] [TO [SAVEPOINT] name]`.
    fn parse_rollback(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::ROLLBACK)?;
        self.eat_keyword(Keyword::TRANSACTION)?;
        if self.eat_keyword(Keyword::TO)? {
            self.eat_keyword(Keyword::SAVEPOINT)?;
            let name = self.parse_name()?;
            return Ok(Statement::Rollback {
                savepoint: Some(name),
            });
        }
        Ok(Statement::Rollback { savepoint: None })
    }

    /// Parses `SAVEPOINT name`.
    fn parse_savepoint(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::SAVEPOINT)?;
        Ok(Statement::Savepoint(self.parse_name()?))
    }

    /// Parses `RELEASE [SAVEPOINT] name`.
    fn parse_release(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::RELEASE)?;
        self.eat_keyword(Keyword::SAVEPOINT)?;
        Ok(Statement::Release(self.parse_name()?))
    }

    /// Parses `PRAGMA [schema.]name [= value | (value)]`.
    fn parse_pragma(&mut self) -> Result<Statement, ParseError> {
        use crate::ast::PragmaValue;
        self.expect_keyword(Keyword::PRAGMA)?;
        let (database, name) = self.parse_qualified_name()?;
        let value = if self.eat(Punctuator::Equal)? {
            PragmaValue::Value(self.parse_pragma_value()?)
        } else if self.eat(Punctuator::LeftParen)? {
            let value = if self.at_name()? && self.peek_at(1)?.is(Punctuator::RightParen) {
                PragmaValue::Name(self.parse_name()?)
            } else {
                PragmaValue::Value(self.parse_pragma_value()?)
            };
            self.expect(Punctuator::RightParen)?;
            value
        } else {
            PragmaValue::None
        };
        Ok(Statement::Pragma {
            database,
            name,
            value,
        })
    }

    /// Parses the value half of a PRAGMA, which is a signed literal or a word.
    ///
    /// Any word is a word here, keyword or not. `PRAGMA journal_mode=DELETE`
    /// names a mode, not the statement, and the same is true of `=FULL`,
    /// `=TRUNCATE`, `=ON` and `=OFF` - there is nothing in this position that
    /// could be a column, so there is nothing for a reserved word to shadow.
    fn parse_pragma_value(&mut self) -> Result<crate::ast::ExprId, ParseError> {
        use crate::ast::{Expr, Literal};
        let token = self.peek()?;
        if Parser::token_is_word(token) && !self.peek_at(1)?.is(Punctuator::LeftParen) {
            self.bump()?;
            let text = lexer::identifier_text(self.source, token).into_owned();
            return Ok(self
                .ast
                .add_expr(Expr::Literal(Literal::String(text)), token.span));
        }
        self.parse_expr()
    }

    /// Parses `ATTACH [DATABASE] file AS schema [KEY key]`.
    fn parse_attach(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::ATTACH)?;
        self.eat_keyword(Keyword::DATABASE)?;
        let file = self.parse_expr()?;
        self.expect_keyword(Keyword::AS)?;
        let schema = self.parse_expr()?;
        let key = if self.eat_keyword(Keyword::KEY)? {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Statement::Attach { file, schema, key })
    }

    /// Parses `DETACH [DATABASE] schema`.
    fn parse_detach(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::DETACH)?;
        self.eat_keyword(Keyword::DATABASE)?;
        Ok(Statement::Detach {
            schema: self.parse_expr()?,
        })
    }

    /// Parses `VACUUM [schema] [INTO file]`.
    fn parse_vacuum(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::VACUUM)?;
        let database = if self.at_name()? && !self.at_keyword(Keyword::INTO)? {
            Some(self.parse_name()?)
        } else {
            None
        };
        let into = if self.eat_keyword(Keyword::INTO)? {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Statement::Vacuum { database, into })
    }

    /// Parses `ANALYZE [[schema.]name]`.
    fn parse_analyze(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::ANALYZE)?;
        if !self.at_name()? {
            return Ok(Statement::Analyze {
                database: None,
                name: None,
            });
        }
        let (database, name) = self.parse_qualified_name()?;
        Ok(Statement::Analyze {
            database,
            name: Some(name),
        })
    }

    /// Parses `REINDEX [[schema.]name]`.
    fn parse_reindex(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::REINDEX)?;
        if !self.at_name()? {
            return Ok(Statement::Reindex {
                database: None,
                name: None,
            });
        }
        let (database, name) = self.parse_qualified_name()?;
        Ok(Statement::Reindex {
            database,
            name: Some(name),
        })
    }
}

/// Parses the next statement beginning at `offset`.
///
/// The returned `consumed` count is what a caller advances by to reach the
/// tail, which is SQLite's prepare contract. A source that holds only trivia
/// yields an empty statement and consumes all of it.
pub fn parse_next_statement(
    source: &[u8],
    offset: usize,
    limits: &Limits,
) -> Result<ParsedStatement, ParseError> {
    parse_next_statement_into(source, offset, limits, Ast::new())
}

/// Parses one statement into an arena the caller supplies.
///
/// The same parse as [`parse_next_statement`], with the arena handed in rather
/// than made. A caller that compiles statement after statement keeps one and
/// gets its capacity back on every parse after the first, which on `SELECT 1`
/// is most of what a parse costs.
///
/// @param source - the SQL text
/// @param offset - where in it this statement starts
/// @param limits - the limits to enforce
/// @param arena - the arena to fill, cleared first
pub fn parse_next_statement_into(
    source: &[u8],
    offset: usize,
    limits: &Limits,
    arena: Ast,
) -> Result<ParsedStatement, ParseError> {
    let length = source.len().saturating_sub(offset) as i64;
    if length > limits.get(Limit::SqlLength) {
        return Err(ParseError::new(
            ParseErrorKind::LimitExceeded("SQL statement length"),
            Span::at(offset),
        ));
    }
    let mut parser = Parser::with_arena(source, offset, limits, arena);
    let start = parser.cursor();
    let statement = parser.parse_statement()?;
    let end = parser.cursor();
    // Everything up to and including the terminator belongs to this statement;
    // what follows is the caller's tail.
    let token = parser.peek()?;
    let consumed = match token.kind {
        TokenKind::EndOfInput => source.len(),
        TokenKind::Punctuator(Punctuator::Semicolon) => {
            parser.bump()?;
            token.span.end as usize
        }
        _ => return Err(parser.unexpected(&[";"])?),
    };
    let span = Span::new(start, end);
    // Taken rather than cloned: the parser is about to be consumed, so the map
    // it built is the caller's and copying it is a `Vec` per parse for nothing.
    let parameters = core::mem::take(&mut parser.parameters);
    Ok(ParsedStatement {
        ast: parser.into_ast(),
        statement,
        consumed,
        parameters,
        span,
    })
}

/// Parses a bare expression, which is what a CHECK constraint or a default
/// value is when it is re-read out of `sqlite_schema`.
pub fn parse_expression(
    source: &[u8],
    limits: &Limits,
) -> Result<(Ast, crate::ast::ExprId), ParseError> {
    let mut parser = Parser::new(source, 0, limits);
    let expr = parser.parse_expr()?;
    let token = parser.peek()?;
    if token.kind != TokenKind::EndOfInput {
        return Err(parser.unexpected(&["end of expression"])?);
    }
    Ok((parser.into_ast(), expr))
}

/// Classifies a statement from its leading keywords alone.
///
/// This is what a caller uses to decide whether a statement may run on a
/// read-only connection without paying for a parse.
pub fn classify_statement(source: &[u8]) -> StatementClass {
    let mut lexer = Lexer::new(source);
    let first = match lexer.next_token() {
        Ok(token) => token,
        Err(_) => return StatementClass::Unknown,
    };
    if first.keyword() == Some(Keyword::EXPLAIN) {
        // EXPLAIN never runs the statement, so it is always read-only, but the
        // caller may still want to know what it wraps.
        return StatementClass::ReadOnly;
    }
    if first.kind == TokenKind::EndOfInput
        || first.kind == TokenKind::Punctuator(Punctuator::Semicolon)
    {
        return StatementClass::Empty;
    }
    if first.keyword() == Some(Keyword::WITH) {
        // A WITH prefix may lead to any of SELECT, INSERT, UPDATE or DELETE.
        // The CTE bodies in between are full SELECTs, so the scan has to count
        // parentheses and only believe a keyword at depth zero.
        let mut depth = 0usize;
        loop {
            let token = match lexer.next_token() {
                Ok(token) => token,
                Err(_) => return StatementClass::Unknown,
            };
            match token.kind {
                TokenKind::EndOfInput => return StatementClass::Unknown,
                TokenKind::Punctuator(Punctuator::LeftParen) => {
                    depth = depth.saturating_add(1);
                    continue;
                }
                TokenKind::Punctuator(Punctuator::RightParen) => {
                    depth = depth.saturating_sub(1);
                    continue;
                }
                _ => {}
            }
            if depth != 0 {
                continue;
            }
            match token.keyword() {
                Some(Keyword::SELECT) | Some(Keyword::VALUES) => return StatementClass::ReadOnly,
                Some(Keyword::INSERT) | Some(Keyword::UPDATE) | Some(Keyword::DELETE) => {
                    return StatementClass::Write
                }
                _ => {}
            }
        }
    }
    match first.keyword() {
        Some(Keyword::SELECT) | Some(Keyword::VALUES) => StatementClass::ReadOnly,
        Some(Keyword::INSERT)
        | Some(Keyword::REPLACE)
        | Some(Keyword::UPDATE)
        | Some(Keyword::DELETE) => StatementClass::Write,
        Some(Keyword::CREATE)
        | Some(Keyword::DROP)
        | Some(Keyword::ALTER)
        | Some(Keyword::REINDEX)
        | Some(Keyword::ANALYZE)
        | Some(Keyword::VACUUM) => StatementClass::SchemaChange,
        Some(Keyword::BEGIN)
        | Some(Keyword::COMMIT)
        | Some(Keyword::END)
        | Some(Keyword::ROLLBACK)
        | Some(Keyword::SAVEPOINT)
        | Some(Keyword::RELEASE) => StatementClass::TransactionControl,
        Some(Keyword::PRAGMA) => StatementClass::Pragma,
        Some(Keyword::ATTACH) | Some(Keyword::DETACH) => StatementClass::SchemaChange,
        _ => StatementClass::Unknown,
    }
}
