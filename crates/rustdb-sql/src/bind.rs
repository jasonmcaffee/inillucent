//! The binder: names to columns, and the bound relational tree.
//!
//! Invariant: the binder is a pure function of one SQL text and one immutable
//! catalog snapshot. It resolves every name, expands every star, decides every
//! affinity and collation, and extracts every aggregate, and it does all of
//! that before a single page is read. A bound statement therefore says exactly
//! what it will touch, which is what lets the authorizer run here rather than
//! part-way through execution.
//!
//! Resolution order is SQLite's: FROM terms left to right, then result aliases
//! where SQLite permits them, with a column always preferred over an alias of
//! the same name. `rowid`, `_rowid_` and `oid` resolve only on a rowid table
//! and only when no real column shadows them.

use rustdb_value::{Affinity, Collation};

use crate::ast::{
    self, Ast, BinaryOp, Expr, ExprId, FromSource, InRhs, JoinConstraint, JoinKind, Literal,
    NullOrder, PatternOp, SelectBody, SelectId, SortOrder, UnaryOp,
};
use crate::catalog_view::{CatalogView, TableInfo, TableKind};
use crate::diagnostic::{ParseError, ParseErrorKind};
use crate::function::{self, AggregateFunc, ScalarFunc};
use crate::lexer::Span;

/// What an authorizer decided about one action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authorization {
    /// The action is allowed.
    Allow,
    /// The action is refused and the statement fails.
    Deny,
    /// The action is allowed but the column reads as NULL.
    Ignore,
}

/// One action an authorizer is asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthAction<'a> {
    /// Reading a column of a table.
    Read {
        /// The database name.
        database: &'a [u8],
        /// The table name.
        table: &'a [u8],
        /// The column name.
        column: &'a [u8],
    },
    /// Running a SELECT at all.
    Select,
    /// Calling a function.
    Function {
        /// The function name.
        name: &'a [u8],
    },
}

/// The callback the binder consults before it binds an action.
pub trait Authorizer {
    /// Returns what to do about one action.
    fn authorize(&self, action: AuthAction<'_>) -> Authorization;
}

/// An authorizer that allows everything, which is the default.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl Authorizer for AllowAll {
    /// Allows every action.
    fn authorize(&self, _action: AuthAction<'_>) -> Authorization {
        Authorization::Allow
    }
}

/// A bound expression, with every name resolved and every rule decided.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundExpr {
    /// A NULL literal.
    Null,
    /// An integer literal.
    Integer(i64),
    /// A real literal.
    Real(f64),
    /// A text literal.
    Text(Vec<u8>),
    /// A blob literal.
    Blob(Vec<u8>),
    /// A bound parameter.
    Parameter(u32),
    /// A column of a FROM term.
    Column {
        /// Which FROM term, by position.
        source: usize,
        /// Which column of it.
        column: u16,
        /// The column's affinity.
        affinity: Affinity,
        /// The column's declared collation.
        collation: Collation,
    },
    /// The rowid of a FROM term.
    Rowid {
        /// Which FROM term.
        source: usize,
    },
    /// A unary operator.
    Unary {
        /// Which operator.
        op: UnaryOp,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// An arithmetic, bitwise or concatenation operator.
    Arithmetic {
        /// Which operator.
        op: BinaryOp,
        /// The left operand.
        left: Box<BoundExpr>,
        /// The right operand.
        right: Box<BoundExpr>,
    },
    /// A comparison, with the affinity and collation it applies.
    Compare {
        /// Which comparison.
        op: BinaryOp,
        /// The left operand.
        left: Box<BoundExpr>,
        /// The right operand.
        right: Box<BoundExpr>,
        /// The affinity applied to both sides before comparing.
        affinity: Option<Affinity>,
        /// The collation the comparison uses.
        collation: Collation,
    },
    /// `AND`, with three-valued semantics.
    And(Box<BoundExpr>, Box<BoundExpr>),
    /// `OR`, with three-valued semantics.
    Or(Box<BoundExpr>, Box<BoundExpr>),
    /// `NOT`.
    Not(Box<BoundExpr>),
    /// `IS NULL` or `NOT NULL`.
    IsNull {
        /// Whether the test is for not-null.
        negated: bool,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// `IS` / `IS NOT`, which never yields NULL.
    Is {
        /// Whether `NOT` was written.
        negated: bool,
        /// The left operand.
        left: Box<BoundExpr>,
        /// The right operand.
        right: Box<BoundExpr>,
        /// The affinity applied before comparing.
        affinity: Option<Affinity>,
        /// The collation the comparison uses.
        collation: Collation,
    },
    /// `BETWEEN`, kept as one node so its operand is evaluated once.
    Between {
        /// Whether `NOT` was written.
        negated: bool,
        /// The value being tested.
        operand: Box<BoundExpr>,
        /// The lower bound.
        low: Box<BoundExpr>,
        /// The upper bound.
        high: Box<BoundExpr>,
        /// The affinity applied to the comparisons.
        affinity: Option<Affinity>,
        /// The collation the comparisons use.
        collation: Collation,
    },
    /// `IN` over a value list.
    InList {
        /// Whether `NOT` was written.
        negated: bool,
        /// The value being tested.
        operand: Box<BoundExpr>,
        /// The list.
        list: Vec<BoundExpr>,
        /// The affinity applied before comparing.
        affinity: Option<Affinity>,
        /// The collation the comparison uses.
        collation: Collation,
    },
    /// `CASE`.
    Case {
        /// The base operand, when the form has one.
        operand: Option<Box<BoundExpr>>,
        /// The `WHEN`/`THEN` pairs.
        branches: Vec<(BoundExpr, BoundExpr)>,
        /// The `ELSE` arm.
        otherwise: Option<Box<BoundExpr>>,
        /// The collation comparisons in the base form use.
        collation: Collation,
    },
    /// `CAST`.
    Cast {
        /// The operand.
        operand: Box<BoundExpr>,
        /// The affinity the declared type maps to.
        affinity: Affinity,
    },
    /// `LIKE`, `GLOB`, `REGEXP` or `MATCH`.
    Pattern {
        /// Whether `NOT` was written.
        negated: bool,
        /// Which operator.
        op: PatternOp,
        /// The value being matched.
        operand: Box<BoundExpr>,
        /// The pattern.
        pattern: Box<BoundExpr>,
        /// The `ESCAPE` argument.
        escape: Option<Box<BoundExpr>>,
    },
    /// A scalar function call.
    Function {
        /// Which function.
        func: ScalarFunc,
        /// The arguments.
        arguments: Vec<BoundExpr>,
        /// The collation the function's comparisons use.
        collation: Collation,
    },
    /// A reference to an aggregate accumulator computed for this row group.
    Aggregate {
        /// Which accumulator, by position.
        slot: usize,
    },
    /// A column of the current sorter row, used after an ORDER BY sort.
    SorterColumn {
        /// Which column of the sorted record.
        column: u16,
    },
    /// An explicit `COLLATE` on an expression that is not a column.
    ///
    /// The node exists so the collation survives to the comparison that uses
    /// it. Attaching it only to columns loses `x = 'BLUE' COLLATE BINARY`,
    /// where the operand carrying the collation is a literal - and losing it
    /// means the column's own collation wins and the comparison quietly
    /// answers a different question.
    Collate {
        /// The operand, which evaluates unchanged.
        operand: Box<BoundExpr>,
        /// The collation the operand forces on a comparison.
        collation: Collation,
    },
}

impl BoundExpr {
    /// Returns the affinity this expression has as an operand.
    ///
    /// SQLite's rule: a column has its own affinity, a cast has the cast's, a
    /// parenthesised expression has its operand's, and everything else has
    /// none. "None" is a real answer here, not a missing one.
    pub fn affinity(&self) -> Option<Affinity> {
        match self {
            BoundExpr::Column { affinity, .. } => Some(*affinity),
            BoundExpr::Cast { affinity, .. } => Some(*affinity),
            BoundExpr::Rowid { .. } => Some(Affinity::Integer),
            BoundExpr::Collate { operand, .. } => operand.affinity(),
            _ => None,
        }
    }

    /// Returns the collation this expression carries, if it forces one.
    pub fn collation(&self) -> Option<Collation> {
        match self {
            BoundExpr::Column { collation, .. } => Some(*collation),
            BoundExpr::Collate { collation, .. } => Some(*collation),
            _ => None,
        }
    }

    /// Returns the collation an explicit `COLLATE` forced on this expression.
    ///
    /// This is *not* the same question as [`BoundExpr::collation`]. A column
    /// declared `COLLATE NOCASE` has an implicit collation; `x COLLATE BINARY`
    /// has an explicit one, and an explicit collation on either side of a
    /// comparison beats an implicit one on the other side.
    pub fn explicit_collation(&self) -> Option<Collation> {
        match self {
            BoundExpr::Collate { collation, .. } => Some(*collation),
            _ => None,
        }
    }

    /// Returns whether the expression reads any column or aggregate.
    pub fn is_constant(&self) -> bool {
        match self {
            BoundExpr::Null
            | BoundExpr::Integer(_)
            | BoundExpr::Real(_)
            | BoundExpr::Text(_)
            | BoundExpr::Blob(_)
            | BoundExpr::Parameter(_) => true,
            BoundExpr::Column { .. }
            | BoundExpr::Rowid { .. }
            | BoundExpr::Aggregate { .. }
            | BoundExpr::SorterColumn { .. } => false,
            BoundExpr::Unary { operand, .. } => operand.is_constant(),
            BoundExpr::Collate { operand, .. } => operand.is_constant(),
            BoundExpr::Not(operand) => operand.is_constant(),
            BoundExpr::IsNull { operand, .. } => operand.is_constant(),
            BoundExpr::Cast { operand, .. } => operand.is_constant(),
            BoundExpr::Arithmetic { left, right, .. }
            | BoundExpr::Compare { left, right, .. }
            | BoundExpr::Is { left, right, .. } => left.is_constant() && right.is_constant(),
            BoundExpr::And(left, right) | BoundExpr::Or(left, right) => {
                left.is_constant() && right.is_constant()
            }
            BoundExpr::Between {
                operand, low, high, ..
            } => operand.is_constant() && low.is_constant() && high.is_constant(),
            BoundExpr::InList { operand, list, .. } => {
                operand.is_constant() && list.iter().all(BoundExpr::is_constant)
            }
            BoundExpr::Case {
                operand,
                branches,
                otherwise,
                ..
            } => {
                operand.as_ref().is_none_or(|e| e.is_constant())
                    && branches
                        .iter()
                        .all(|(when, then)| when.is_constant() && then.is_constant())
                    && otherwise.as_ref().is_none_or(|e| e.is_constant())
            }
            BoundExpr::Pattern {
                operand,
                pattern,
                escape,
                ..
            } => {
                operand.is_constant()
                    && pattern.is_constant()
                    && escape.as_ref().is_none_or(|e| e.is_constant())
            }
            BoundExpr::Function { arguments, .. } => arguments.iter().all(BoundExpr::is_constant),
        }
    }

    /// Returns which FROM terms the expression reads.
    pub fn sources_used(&self, into: &mut Vec<usize>) {
        match self {
            BoundExpr::Column { source, .. } | BoundExpr::Rowid { source } => {
                if !into.contains(source) {
                    into.push(*source);
                }
            }
            BoundExpr::Unary { operand, .. }
            | BoundExpr::Not(operand)
            | BoundExpr::IsNull { operand, .. }
            | BoundExpr::Collate { operand, .. }
            | BoundExpr::Cast { operand, .. } => operand.sources_used(into),
            BoundExpr::Arithmetic { left, right, .. }
            | BoundExpr::Compare { left, right, .. }
            | BoundExpr::Is { left, right, .. }
            | BoundExpr::And(left, right)
            | BoundExpr::Or(left, right) => {
                left.sources_used(into);
                right.sources_used(into);
            }
            BoundExpr::Between {
                operand, low, high, ..
            } => {
                operand.sources_used(into);
                low.sources_used(into);
                high.sources_used(into);
            }
            BoundExpr::InList { operand, list, .. } => {
                operand.sources_used(into);
                for item in list {
                    item.sources_used(into);
                }
            }
            BoundExpr::Case {
                operand,
                branches,
                otherwise,
                ..
            } => {
                if let Some(operand) = operand {
                    operand.sources_used(into);
                }
                for (when, then) in branches {
                    when.sources_used(into);
                    then.sources_used(into);
                }
                if let Some(otherwise) = otherwise {
                    otherwise.sources_used(into);
                }
            }
            BoundExpr::Pattern {
                operand,
                pattern,
                escape,
                ..
            } => {
                operand.sources_used(into);
                pattern.sources_used(into);
                if let Some(escape) = escape {
                    escape.sources_used(into);
                }
            }
            BoundExpr::Function { arguments, .. } => {
                for argument in arguments {
                    argument.sources_used(into);
                }
            }
            _ => {}
        }
    }
}

/// One FROM term, bound to a table.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundSource {
    /// The table, view or virtual table.
    pub table: TableInfo,
    /// The name the query refers to it by.
    pub alias: Vec<u8>,
    /// The join that attaches it to the term before it.
    pub join: JoinKind,
    /// The join constraint, already desugared from NATURAL and USING.
    pub constraint: Option<BoundExpr>,
    /// Columns suppressed from `*` by a NATURAL or USING join.
    pub suppressed: Vec<u16>,
}

/// One aggregate the statement computes.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundAggregate {
    /// Which aggregate.
    pub func: AggregateFunc,
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// The arguments, or empty for `count(*)`.
    pub arguments: Vec<BoundExpr>,
    /// Whether the call was `count(*)`.
    pub star: bool,
    /// The collation the aggregate compares with.
    pub collation: Collation,
}

/// Returns the collation a result column compares with.
///
/// `DISTINCT` and `GROUP BY` compare result values, and a NOCASE column makes
/// `blue` and `Blue` the same value for both. Comparing them with BINARY
/// instead returns more rows than SQLite does, which looks like a duplicate
/// rather than like a bug.
pub fn result_collation(expr: &BoundExpr) -> Collation {
    expr.explicit_collation()
        .or_else(|| expr.collation())
        .unwrap_or(Collation::Binary)
}

/// One result column, after star expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundResultColumn {
    /// The expression.
    pub expr: BoundExpr,
    /// The name the column reports.
    pub name: Vec<u8>,
    /// The table the column came from, when it came from one.
    pub origin: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    /// The declared type the column reports, when it has one.
    pub declared_type: Vec<u8>,
}

/// One `ORDER BY` term, bound.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundOrderTerm {
    /// The expression to sort by.
    pub expr: BoundExpr,
    /// The direction.
    pub order: SortOrder,
    /// Where NULLs sort.
    pub nulls: NullOrder,
    /// The collation the sort compares with.
    pub collation: Collation,
}

/// A bound SELECT.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundSelect {
    /// The FROM terms, in written order.
    pub sources: Vec<BoundSource>,
    /// The `WHERE` clause.
    pub filter: Option<BoundExpr>,
    /// The `GROUP BY` terms.
    pub group_by: Vec<BoundExpr>,
    /// The `HAVING` clause.
    pub having: Option<BoundExpr>,
    /// The result columns, after star expansion.
    pub columns: Vec<BoundResultColumn>,
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// The `ORDER BY` terms.
    pub order_by: Vec<BoundOrderTerm>,
    /// The `LIMIT` expression.
    pub limit: Option<BoundExpr>,
    /// The `OFFSET` expression.
    pub offset: Option<BoundExpr>,
    /// The aggregates the statement computes.
    pub aggregates: Vec<BoundAggregate>,
    /// The rows of a `VALUES` arm, when the statement is one.
    pub values: Vec<Vec<BoundExpr>>,
}

impl BoundSelect {
    /// Returns whether the statement aggregates its input into one group or
    /// into groups.
    pub fn is_aggregate(&self) -> bool {
        !self.aggregates.is_empty() || !self.group_by.is_empty()
    }
}

/// Every database and cookie a bound statement depends on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dependencies {
    /// The `(database index, schema cookie)` pairs the statement was bound
    /// against.
    pub schemas: Vec<(usize, u32)>,
    /// The catalog generation the statement was bound against.
    pub generation: u64,
}

/// A bound statement.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundStatement {
    /// A SELECT or VALUES.
    Select(Box<BoundSelect>),
    /// An INSERT or REPLACE.
    Insert(Box<crate::dml::BoundInsert>),
    /// An UPDATE.
    Update(Box<crate::dml::BoundUpdate>),
    /// A DELETE.
    Delete(Box<crate::dml::BoundDelete>),
    /// A statement the session executes itself rather than compiling.
    Directive(Box<crate::directive::Directive>),
    /// A statement that compiles to no program.
    Empty,
}

/// The binder's working state for one statement.
pub struct Binder<'a> {
    pub(crate) catalog: &'a dyn CatalogView,
    pub(crate) ast: &'a Ast,
    pub(crate) authorizer: &'a dyn Authorizer,
    pub(crate) sources: Vec<BoundSource>,
    aggregates: Vec<BoundAggregate>,
    result_aliases: Vec<(Vec<u8>, BoundExpr)>,
    dependencies: Dependencies,
    inside_aggregate: bool,
    allow_aggregates: bool,
    /// The table `excluded` names while an upsert's `DO UPDATE` is bound.
    pub(crate) excluded: Option<crate::catalog_view::TableInfo>,
}

/// The source number a column of an upsert's `excluded` row carries.
///
/// It is not a FROM term: `excluded` is the row the INSERT was about to write,
/// which lives in registers rather than under a cursor. Giving it a number no
/// real source can have means the compiler must substitute it - and a compiler
/// that forgot to would try to open a cursor two billion and be refused by the
/// verifier, rather than reading the wrong row.
pub const EXCLUDED_SOURCE: usize = usize::MAX;

impl<'a> Binder<'a> {
    /// Returns a binder over one catalog snapshot and one parse.
    pub fn new(
        catalog: &'a dyn CatalogView,
        ast: &'a Ast,
        authorizer: &'a dyn Authorizer,
    ) -> Binder<'a> {
        Binder {
            catalog,
            ast,
            authorizer,
            sources: Vec::new(),
            aggregates: Vec::new(),
            result_aliases: Vec::new(),
            dependencies: Dependencies {
                schemas: Vec::new(),
                generation: catalog.generation(),
            },
            inside_aggregate: false,
            allow_aggregates: false,
            excluded: None,
        }
    }

    /// Returns what the bound statement depends on.
    pub fn dependencies(&self) -> &Dependencies {
        &self.dependencies
    }

    /// Binds a statement, or reports why it cannot be bound.
    pub fn bind_statement(
        &mut self,
        statement: &ast::Statement,
    ) -> Result<BoundStatement, ParseError> {
        match statement {
            ast::Statement::Empty => Ok(BoundStatement::Empty),
            ast::Statement::Select(select) => {
                let bound = self.bind_select(*select)?;
                Ok(BoundStatement::Select(Box::new(bound)))
            }
            ast::Statement::Insert(insert) => {
                let bound = self.bind_insert(insert)?;
                Ok(BoundStatement::Insert(Box::new(bound)))
            }
            ast::Statement::Update(update) => {
                let bound = self.bind_update(update)?;
                Ok(BoundStatement::Update(Box::new(bound)))
            }
            ast::Statement::Delete(delete) => {
                let bound = self.bind_delete(delete)?;
                Ok(BoundStatement::Delete(Box::new(bound)))
            }
            ast::Statement::Explain { .. } => Err(unsupported("EXPLAIN", Span::default())),
            other => {
                let directive = self.bind_directive(other)?;
                Ok(BoundStatement::Directive(Box::new(directive)))
            }
        }
    }

    /// Binds a SELECT.
    pub fn bind_select(&mut self, id: SelectId) -> Result<BoundSelect, ParseError> {
        let Some(select) = self.ast.select(id) else {
            return Err(unsupported("missing select", Span::default()));
        };
        if !select.with.ctes.is_empty() {
            return Err(unsupported("WITH", select.span));
        }
        if !select.compounds.is_empty() {
            return Err(unsupported("compound SELECT", select.span));
        }
        if self.authorizer.authorize(AuthAction::Select) == Authorization::Deny {
            return Err(denied("not authorized", select.span));
        }
        let Some(core) = self.ast.core(select.first) else {
            return Err(unsupported("missing select core", select.span));
        };
        let mut bound = match &core.body {
            SelectBody::Values(rows) => self.bind_values(rows, core.span)?,
            SelectBody::Select { .. } => self.bind_select_core(select.first)?,
        };
        let columns = bound.columns.clone();
        bound.order_by = self.bind_order_by(&select.order_by, &columns)?;
        bound.sources = core::mem::take(&mut self.sources);
        bound.limit = match select.limit {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        bound.offset = match select.offset {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        bound.aggregates = self.aggregates.clone();
        Ok(bound)
    }

    /// Binds a `VALUES` arm, which has no FROM and no names to resolve.
    fn bind_values(&mut self, rows: &[Vec<ExprId>], span: Span) -> Result<BoundSelect, ParseError> {
        let mut bound_rows = Vec::with_capacity(rows.len());
        let mut width = 0usize;
        for row in rows {
            let mut values = Vec::with_capacity(row.len());
            for expr in row {
                values.push(self.bind_expr(*expr)?);
            }
            if bound_rows.is_empty() {
                width = values.len();
            } else if values.len() != width {
                return Err(ParseError::new(
                    ParseErrorKind::Unsupported("all VALUES rows must have the same width"),
                    span,
                ));
            }
            bound_rows.push(values);
        }
        let columns = (0..width)
            .map(|index| BoundResultColumn {
                expr: BoundExpr::SorterColumn {
                    column: index as u16,
                },
                name: format!("column{}", index.saturating_add(1)).into_bytes(),
                origin: None,
                declared_type: Vec::new(),
            })
            .collect();
        Ok(BoundSelect {
            sources: Vec::new(),
            filter: None,
            group_by: Vec::new(),
            having: None,
            columns,
            distinct: false,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            aggregates: Vec::new(),
            values: bound_rows,
        })
    }

    /// Binds a `SELECT` arm: FROM, WHERE, GROUP BY, HAVING, and the results.
    fn bind_select_core(&mut self, id: ast::SelectCoreId) -> Result<BoundSelect, ParseError> {
        let Some(core) = self.ast.core(id) else {
            return Err(unsupported("missing select core", Span::default()));
        };
        let SelectBody::Select {
            distinct,
            columns,
            from,
            filter,
            group_by,
            having,
            windows,
            ..
        } = &core.body
        else {
            return Err(unsupported("expected a select core", core.span));
        };
        if !windows.is_empty() {
            return Err(unsupported("WINDOW", core.span));
        }
        for term in from {
            self.bind_from_term(*term)?;
        }
        self.desugar_join_constraints(from)?;
        let bound_filter = match filter {
            Some(expr) => Some(self.bind_expr(*expr)?),
            None => None,
        };
        self.allow_aggregates = true;
        let bound_columns = self.bind_result_columns(columns)?;
        for column in &bound_columns {
            if !column.name.is_empty() {
                self.result_aliases
                    .push((column.name.to_ascii_lowercase(), column.expr.clone()));
            }
        }
        let mut bound_group = Vec::with_capacity(group_by.len());
        for expr in group_by {
            bound_group.push(self.bind_group_term(*expr, &bound_columns)?);
        }
        let bound_having = match having {
            Some(expr) => Some(self.bind_expr(*expr)?),
            None => None,
        };
        if bound_having.is_some() && bound_group.is_empty() && self.aggregates.is_empty() {
            return Err(unsupported(
                "HAVING requires GROUP BY or an aggregate",
                core.span,
            ));
        }
        // The sources stay in the binder: `ORDER BY` and `LIMIT` belong to the
        // whole statement and are bound after this returns, and `ORDER BY b.id`
        // needs the same scope the result columns had. Moving the scope out
        // here made every qualified name in an ORDER BY report "no such table".
        Ok(BoundSelect {
            sources: Vec::new(),
            filter: bound_filter,
            group_by: bound_group,
            having: bound_having,
            columns: bound_columns,
            distinct: *distinct,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            aggregates: Vec::new(),
            values: Vec::new(),
        })
    }

    /// Binds one FROM term, registering it as a source.
    fn bind_from_term(&mut self, id: ast::FromTermId) -> Result<(), ParseError> {
        let Some(term) = self.ast.from_term(id) else {
            return Err(unsupported("missing FROM term", Span::default()));
        };
        let (database, name) = match &term.source {
            FromSource::Table {
                database,
                name,
                arguments,
                ..
            } => {
                if arguments.is_some() {
                    return Err(unsupported("table-valued functions", term.span));
                }
                (*database, *name)
            }
            FromSource::Subquery(_) => return Err(unsupported("subqueries in FROM", term.span)),
            FromSource::Join(_) => return Err(unsupported("parenthesised joins", term.span)),
        };
        let database_name = database.map(|id| self.ast.folded(id).to_vec());
        let folded = self.ast.folded(name).to_vec();
        let Some(table) = self
            .catalog
            .find_table(database_name.as_deref(), &folded)
            .cloned()
        else {
            return Err(no_such_table(self.ast.text(name), term.span));
        };
        if table.kind == TableKind::View {
            return Err(unsupported("views", term.span));
        }
        if table.kind == TableKind::Virtual {
            return Err(unsupported("virtual tables", term.span));
        }
        self.record_dependency(table.database);
        let alias = match term.alias {
            Some(alias) => self.ast.text(alias).to_vec(),
            None => table.name.clone(),
        };
        if matches!(term.join, JoinKind::Right | JoinKind::Full) {
            return Err(unsupported("RIGHT and FULL joins", term.span));
        }
        if matches!(term.join, JoinKind::Left) {
            return Err(unsupported("LEFT joins", term.span));
        }
        self.sources.push(BoundSource {
            table,
            alias,
            join: term.join,
            constraint: None,
            suppressed: Vec::new(),
        });
        Ok(())
    }

    /// Turns `ON`, `USING` and `NATURAL` into ordinary predicates.
    ///
    /// The output-column rules survive the rewrite: a `USING` or `NATURAL`
    /// column is suppressed from the right-hand term's contribution to `*`,
    /// which is the only visible difference between a `USING` join and the
    /// equality predicate it means.
    fn desugar_join_constraints(&mut self, terms: &[ast::FromTermId]) -> Result<(), ParseError> {
        for (position, id) in terms.iter().enumerate() {
            let Some(term) = self.ast.from_term(*id) else {
                continue;
            };
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

    /// Stores a join constraint on a source.
    fn set_constraint(&mut self, position: usize, constraint: Option<BoundExpr>) {
        if let Some(source) = self.sources.get_mut(position) {
            source.constraint = constraint;
        }
    }

    /// Returns the column names a NATURAL join equates: every name the right
    /// term shares with any term to its left.
    fn natural_columns(&self, position: usize) -> Vec<Vec<u8>> {
        let Some(right) = self.sources.get(position) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for column in &right.table.columns {
            if column.hidden {
                continue;
            }
            let shared = self
                .sources
                .get(..position)
                .unwrap_or(&[])
                .iter()
                .any(|left| left.table.column_position(&column.folded).is_some());
            if shared {
                names.push(column.folded.clone());
            }
        }
        names
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
            let Some((left_source, left_column)) = self.find_column_left_of(position, name) else {
                continue;
            };
            let Some((right_source, right_column)) = self.find_column_in(position, name) else {
                continue;
            };
            if let Some(source) = self.sources.get_mut(position) {
                source.suppressed.push(right_column);
            }
            let left = self.column_expr(left_source, left_column)?;
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

    /// Finds a column by folded name in one source.
    fn find_column_in(&self, position: usize, folded: &[u8]) -> Option<(usize, u16)> {
        let source = self.sources.get(position)?;
        source
            .table
            .column_position(folded)
            .map(|column| (position, column))
    }

    /// Finds a column by folded name in the sources before one.
    fn find_column_left_of(&self, position: usize, folded: &[u8]) -> Option<(usize, u16)> {
        for index in (0..position).rev() {
            if let Some(found) = self.find_column_in(index, folded) {
                return Some(found);
            }
        }
        None
    }

    /// Records that the statement depends on a database's schema cookie.
    fn record_dependency(&mut self, database: usize) {
        if self
            .dependencies
            .schemas
            .iter()
            .any(|(index, _)| *index == database)
        {
            return;
        }
        let cookie = self.catalog.schema_cookie(database);
        self.dependencies.schemas.push((database, cookie));
    }

    /// Binds the result columns, expanding `*` and `table.*`.
    fn bind_result_columns(
        &mut self,
        columns: &[ast::ResultColumn],
    ) -> Result<Vec<BoundResultColumn>, ParseError> {
        let mut bound = Vec::new();
        for column in columns {
            match self.ast.expr(column.expr) {
                Some(Expr::Star { table }) => {
                    let qualifier = table.map(|id| self.ast.folded(id).to_vec());
                    self.expand_star(qualifier.as_deref(), column.span, &mut bound)?;
                }
                _ => {
                    let expr = self.bind_expr(column.expr)?;
                    let name = match column.alias {
                        Some(alias) => self.ast.text(alias).to_vec(),
                        None => self.default_column_name(column.expr, &expr),
                    };
                    let (origin, declared_type) = self.column_origin(&expr);
                    bound.push(BoundResultColumn {
                        expr,
                        name,
                        origin,
                        declared_type,
                    });
                }
            }
        }
        if bound.is_empty() {
            return Err(unsupported(
                "a SELECT must have result columns",
                Span::default(),
            ));
        }
        Ok(bound)
    }

    /// Expands `*` or `table.*` into one bound column per visible column.
    fn expand_star(
        &mut self,
        qualifier: Option<&[u8]>,
        span: Span,
        into: &mut Vec<BoundResultColumn>,
    ) -> Result<(), ParseError> {
        if self.sources.is_empty() {
            return Err(ParseError::new(
                ParseErrorKind::Unexpected {
                    found: "*".to_string(),
                    expected: vec!["a FROM clause"],
                },
                span,
            ));
        }
        let mut matched = false;
        for position in 0..self.sources.len() {
            let Some(source) = self.sources.get(position) else {
                continue;
            };
            if let Some(qualifier) = qualifier {
                if !source.alias.eq_ignore_ascii_case(qualifier) {
                    continue;
                }
            }
            matched = true;
            let columns = source.table.columns.clone();
            let suppressed = source.suppressed.clone();
            let alias = source.alias.clone();
            let database = self.catalog.database_name(source.table.database).to_vec();
            let table_name = source.table.name.clone();
            for (index, column) in columns.iter().enumerate() {
                let position_u16 = index as u16;
                if column.hidden || suppressed.contains(&position_u16) {
                    continue;
                }
                if self.authorizer.authorize(AuthAction::Read {
                    database: &database,
                    table: &table_name,
                    column: &column.name,
                }) == Authorization::Deny
                {
                    return Err(denied("not authorized", span));
                }
                let expr = self.column_expr(position, position_u16)?;
                into.push(BoundResultColumn {
                    expr,
                    name: column.name.clone(),
                    origin: Some((database.clone(), table_name.clone(), column.name.clone())),
                    declared_type: column.declared_type.clone(),
                });
            }
            let _ = alias;
        }
        if !matched {
            return Err(no_such_table(qualifier.unwrap_or(b"*"), span));
        }
        Ok(())
    }

    /// Returns the name an unaliased result column reports.
    ///
    /// SQLite reports a bare column reference by its column name and every
    /// other expression by the source text it was written as. The text is what
    /// makes `SELECT a+1` report `a+1`.
    fn default_column_name(&self, id: ExprId, bound: &BoundExpr) -> Vec<u8> {
        if let BoundExpr::Column { source, column, .. } = bound {
            if let Some(name) = self
                .sources
                .get(*source)
                .and_then(|source| source.table.column(*column))
            {
                return name.name.clone();
            }
        }
        if let Some(Expr::Column { column, .. }) = self.ast.expr(id) {
            return self.ast.text(*column).to_vec();
        }
        Vec::new()
    }

    /// Returns the origin triple and declared type of a bound column.
    fn column_origin(&self, expr: &BoundExpr) -> (Option<(Vec<u8>, Vec<u8>, Vec<u8>)>, Vec<u8>) {
        let BoundExpr::Column { source, column, .. } = expr else {
            return (None, Vec::new());
        };
        let Some(source) = self.sources.get(*source) else {
            return (None, Vec::new());
        };
        let Some(info) = source.table.column(*column) else {
            return (None, Vec::new());
        };
        (
            Some((
                self.catalog.database_name(source.table.database).to_vec(),
                source.table.name.clone(),
                info.name.clone(),
            )),
            info.declared_type.clone(),
        )
    }

    /// Binds one `GROUP BY` term, which may be an ordinal or a result alias.
    fn bind_group_term(
        &mut self,
        id: ExprId,
        columns: &[BoundResultColumn],
    ) -> Result<BoundExpr, ParseError> {
        if let Some(index) = self.as_ordinal(id) {
            let Some(column) = columns.get(index.saturating_sub(1)) else {
                return Err(unsupported(
                    "GROUP BY term is out of range",
                    self.ast.expr_span(id),
                ));
            };
            return Ok(column.expr.clone());
        }
        self.bind_expr(id)
    }

    /// Returns the one-based ordinal an expression is, if it is an integer.
    fn as_ordinal(&self, id: ExprId) -> Option<usize> {
        let Some(Expr::Literal(Literal::Integer(text))) = self.ast.expr(id) else {
            return None;
        };
        let mut value: usize = 0;
        for byte in text {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value
                .saturating_mul(10)
                .saturating_add(usize::from(byte.saturating_sub(b'0')));
        }
        Some(value)
    }

    /// Binds an `ORDER BY` list, resolving ordinals and result aliases.
    fn bind_order_by(
        &mut self,
        terms: &[ast::OrderTerm],
        columns: &[BoundResultColumn],
    ) -> Result<Vec<BoundOrderTerm>, ParseError> {
        let mut bound = Vec::with_capacity(terms.len());
        for term in terms {
            // A bare integer is an ordinal into the result columns; anything
            // else, including `1 + 0`, is an expression. SQLite draws the line
            // at a literal, and so does this.
            let expr = match self.as_ordinal(term.expr) {
                Some(ordinal) => {
                    let Some(column) = ordinal.checked_sub(1).and_then(|index| columns.get(index))
                    else {
                        return Err(order_out_of_range(ordinal, self.ast.expr_span(term.expr)));
                    };
                    column.expr.clone()
                }
                None => self.bind_expr(term.expr)?,
            };
            let collation = expr.collation().unwrap_or(Collation::Binary);
            let nulls = term.nulls.unwrap_or(match term.order {
                // SQLite sorts NULLs first ascending and last descending when
                // no explicit null ordering is written.
                SortOrder::Ascending => NullOrder::First,
                SortOrder::Descending => NullOrder::Last,
            });
            bound.push(BoundOrderTerm {
                expr,
                order: term.order,
                nulls,
                collation,
            });
        }
        Ok(bound)
    }

    /// Returns a bound column reference, checking the authorizer.
    fn column_expr(&mut self, source: usize, column: u16) -> Result<BoundExpr, ParseError> {
        let Some(bound) = self.sources.get(source) else {
            return Err(unsupported("unknown source", Span::default()));
        };
        let Some(info) = bound.table.column(column) else {
            return Err(unsupported("unknown column", Span::default()));
        };
        let affinity = info.affinity;
        let collation =
            Collation::from_name(core::str::from_utf8(&info.collation).unwrap_or("BINARY"))
                .unwrap_or(Collation::Binary);
        if bound.table.rowid_alias == Some(column) {
            // An INTEGER PRIMARY KEY column *is* the rowid, and reading it
            // through the record would read a NULL placeholder.
            return Ok(BoundExpr::Rowid { source });
        }
        Ok(BoundExpr::Column {
            source,
            column,
            affinity,
            collation,
        })
    }

    /// Binds a result-column list against the current sources.
    ///
    /// `RETURNING` is a result-column list over the row a DML statement wrote,
    /// so it is bound by the same code that binds a `SELECT` list rather than
    /// by a second implementation that would have to be kept in step with it.
    pub fn bind_result_columns_public(
        &mut self,
        columns: &[ast::ResultColumn],
    ) -> Result<Vec<BoundResultColumn>, ParseError> {
        self.bind_result_columns(columns)
    }

    /// Records that the statement depends on a database's schema.
    pub(crate) fn record_write_dependency(&mut self, database: usize) {
        self.record_dependency(database);
    }

    /// Binds one expression.
    pub fn bind_expr(&mut self, id: ExprId) -> Result<BoundExpr, ParseError> {
        let span = self.ast.expr_span(id);
        let Some(expr) = self.ast.expr(id) else {
            return Err(unsupported("missing expression", span));
        };
        match expr.clone() {
            Expr::Literal(literal) => self.bind_literal(&literal, span),
            Expr::Parameter { index, .. } => Ok(BoundExpr::Parameter(index)),
            Expr::Column {
                database,
                table,
                column,
            } => self.bind_column_reference(database, table, column, span),
            Expr::Star { .. } => Err(ParseError::new(
                ParseErrorKind::Unexpected {
                    found: "*".to_string(),
                    expected: vec!["an expression"],
                },
                span,
            )),
            Expr::Unary { op, operand } => {
                let operand = Box::new(self.bind_expr(operand)?);
                match op {
                    UnaryOp::Not => Ok(BoundExpr::Not(operand)),
                    _ => Ok(BoundExpr::Unary { op, operand }),
                }
            }
            Expr::Binary { op, left, right } => self.bind_binary(op, left, right),
            Expr::Collate { operand, collation } => {
                let name = self.ast.text(collation);
                let Some(collation) =
                    Collation::from_name(core::str::from_utf8(name).unwrap_or(""))
                else {
                    return Err(no_such_collation(name, span));
                };
                let bound = self.bind_expr(operand)?;
                Ok(apply_collation(bound, collation))
            }
            Expr::Cast { operand, declared } => {
                let operand = Box::new(self.bind_expr(operand)?);
                let affinity =
                    rustdb_value::affinity::affinity_of_declared_type(self.ast.text(declared));
                Ok(BoundExpr::Cast { operand, affinity })
            }
            Expr::Pattern {
                negated,
                op,
                operand,
                pattern,
                escape,
            } => {
                if matches!(op, PatternOp::Regexp | PatternOp::Match) {
                    return Err(no_such_function(
                        if op == PatternOp::Regexp {
                            b"regexp"
                        } else {
                            b"match"
                        },
                        span,
                    ));
                }
                let operand = Box::new(self.bind_expr(operand)?);
                let pattern = Box::new(self.bind_expr(pattern)?);
                let escape = match escape {
                    Some(expr) => Some(Box::new(self.bind_expr(expr)?)),
                    None => None,
                };
                Ok(BoundExpr::Pattern {
                    negated,
                    op,
                    operand,
                    pattern,
                    escape,
                })
            }
            Expr::Between {
                negated,
                operand,
                low,
                high,
            } => {
                let operand = self.bind_expr(operand)?;
                let low = self.bind_expr(low)?;
                let high = self.bind_expr(high)?;
                let (affinity, collation) = comparison_rules(&operand, &low);
                Ok(BoundExpr::Between {
                    negated,
                    operand: Box::new(operand),
                    low: Box::new(low),
                    high: Box::new(high),
                    affinity,
                    collation,
                })
            }
            Expr::In {
                negated,
                operand,
                rhs,
            } => {
                let operand = self.bind_expr(operand)?;
                let InRhs::List(items) = rhs else {
                    return Err(unsupported("IN over a subquery or table", span));
                };
                let mut list = Vec::with_capacity(items.len());
                for item in &items {
                    list.push(self.bind_expr(*item)?);
                }
                let (affinity, collation) = match list.first() {
                    Some(first) => comparison_rules(&operand, first),
                    None => (None, Collation::Binary),
                };
                Ok(BoundExpr::InList {
                    negated,
                    operand: Box::new(operand),
                    list,
                    affinity,
                    collation,
                })
            }
            Expr::IsNull { negated, operand } => Ok(BoundExpr::IsNull {
                negated,
                operand: Box::new(self.bind_expr(operand)?),
            }),
            Expr::Is {
                negated,
                left,
                right,
                ..
            } => {
                let left = self.bind_expr(left)?;
                let right = self.bind_expr(right)?;
                let (affinity, collation) = comparison_rules(&left, &right);
                Ok(BoundExpr::Is {
                    negated,
                    left: Box::new(left),
                    right: Box::new(right),
                    affinity,
                    collation,
                })
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                let bound_operand = match operand {
                    Some(expr) => Some(Box::new(self.bind_expr(expr)?)),
                    None => None,
                };
                let mut bound_branches = Vec::with_capacity(branches.len());
                for (when, then) in &branches {
                    bound_branches.push((self.bind_expr(*when)?, self.bind_expr(*then)?));
                }
                let bound_otherwise = match otherwise {
                    Some(expr) => Some(Box::new(self.bind_expr(expr)?)),
                    None => None,
                };
                let collation = bound_operand
                    .as_ref()
                    .and_then(|operand| operand.collation())
                    .unwrap_or(Collation::Binary);
                Ok(BoundExpr::Case {
                    operand: bound_operand,
                    branches: bound_branches,
                    otherwise: bound_otherwise,
                    collation,
                })
            }
            Expr::Function {
                name,
                distinct,
                arguments,
                order_by,
                filter,
                over,
            } => {
                if over.is_some() {
                    return Err(unsupported("window functions", span));
                }
                if filter.is_some() {
                    return Err(unsupported("FILTER", span));
                }
                if !order_by.is_empty() {
                    return Err(unsupported("ORDER BY inside an aggregate", span));
                }
                self.bind_call(name, distinct, arguments, span)
            }
            Expr::Exists { .. } | Expr::Subquery(_) => Err(unsupported("subqueries", span)),
            Expr::RowValue(_) => Err(unsupported("row values", span)),
            Expr::Raise { .. } => Err(unsupported("RAISE outside a trigger", span)),
        }
    }

    /// Binds a literal, converting its written text into a value.
    fn bind_literal(&self, literal: &Literal, span: Span) -> Result<BoundExpr, ParseError> {
        match literal {
            Literal::Null => Ok(BoundExpr::Null),
            Literal::Boolean(value) => Ok(BoundExpr::Integer(i64::from(*value))),
            Literal::Integer(text) => Ok(integer_literal(text)),
            Literal::Float(text) => {
                let parsed = rustdb_value::numeric::atof(text, rustdb_value::TextEncoding::Utf8);
                Ok(BoundExpr::Real(parsed.value))
            }
            Literal::String(text) => Ok(BoundExpr::Text(text.clone())),
            Literal::Blob(bytes) => Ok(BoundExpr::Blob(bytes.clone())),
            Literal::CurrentDate | Literal::CurrentTime | Literal::CurrentTimestamp => {
                Err(unsupported("CURRENT_ date and time functions", span))
            }
        }
    }

    /// Resolves `excluded.column` inside an upsert's `DO UPDATE`.
    ///
    /// `excluded` is only in scope there, so a query that uses the name
    /// anywhere else gets the ordinary "no such table" answer rather than a
    /// row that came from nowhere.
    fn bind_excluded_column(&mut self, folded: &[u8], span: Span) -> Result<BoundExpr, ParseError> {
        let Some(table) = self.excluded.clone() else {
            return Err(no_such_table(b"excluded", span));
        };
        if let Some(position) = table.column_position(folded) {
            if table.rowid_alias == Some(position) {
                return Ok(BoundExpr::Rowid {
                    source: EXCLUDED_SOURCE,
                });
            }
            let Some(info) = table.column(position) else {
                return Err(no_such_column(folded, span));
            };
            let collation =
                Collation::from_name(core::str::from_utf8(&info.collation).unwrap_or("BINARY"))
                    .unwrap_or(Collation::Binary);
            return Ok(BoundExpr::Column {
                source: EXCLUDED_SOURCE,
                column: position,
                affinity: info.affinity,
                collation,
            });
        }
        if table.is_rowid_name(folded) {
            return Ok(BoundExpr::Rowid {
                source: EXCLUDED_SOURCE,
            });
        }
        Err(no_such_column(folded, span))
    }

    /// Resolves a column reference against the scope.
    fn bind_column_reference(
        &mut self,
        database: Option<ast::NameId>,
        table: Option<ast::NameId>,
        column: ast::NameId,
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        let folded = self.ast.folded(column).to_vec();
        let table_folded = table.map(|id| self.ast.folded(id).to_vec());
        let database_folded = database.map(|id| self.ast.folded(id).to_vec());
        if table_folded.as_deref() == Some(b"excluded".as_slice()) {
            return self.bind_excluded_column(&folded, span);
        }
        let mut found: Option<(usize, u16)> = None;
        let mut rowid_of: Option<usize> = None;
        for (position, source) in self.sources.iter().enumerate() {
            if let Some(qualifier) = table_folded.as_deref() {
                if !source.alias.eq_ignore_ascii_case(qualifier) {
                    continue;
                }
            }
            if let Some(qualifier) = database_folded.as_deref() {
                if !self
                    .catalog
                    .database_name(source.table.database)
                    .eq_ignore_ascii_case(qualifier)
                {
                    continue;
                }
            }
            if let Some(index) = source.table.column_position(&folded) {
                if found.is_some() {
                    return Err(ambiguous_column(self.ast.text(column), span));
                }
                found = Some((position, index));
                continue;
            }
            if source.table.is_rowid_name(&folded) && rowid_of.is_none() {
                rowid_of = Some(position);
            }
        }
        if let Some((source, index)) = found {
            let (database_name, table_name, column_name) = {
                let Some(bound) = self.sources.get(source) else {
                    return Err(unsupported("unknown source", span));
                };
                let Some(info) = bound.table.column(index) else {
                    return Err(unsupported("unknown column", span));
                };
                (
                    self.catalog.database_name(bound.table.database).to_vec(),
                    bound.table.name.clone(),
                    info.name.clone(),
                )
            };
            match self.authorizer.authorize(AuthAction::Read {
                database: &database_name,
                table: &table_name,
                column: &column_name,
            }) {
                Authorization::Allow => {}
                Authorization::Deny => return Err(denied("not authorized", span)),
                Authorization::Ignore => return Ok(BoundExpr::Null),
            }
            return self.column_expr(source, index);
        }
        if let Some(source) = rowid_of {
            return Ok(BoundExpr::Rowid { source });
        }
        // A result alias is visible to GROUP BY, HAVING and ORDER BY, and only
        // after a real column has failed to match, which is SQLite's order.
        if table_folded.is_none() {
            if let Some((_, expr)) = self
                .result_aliases
                .iter()
                .find(|(name, _)| name.as_slice() == folded.as_slice())
            {
                return Ok(expr.clone());
            }
        }
        if self.sources.is_empty() && table_folded.is_none() {
            return Err(no_such_column(self.ast.text(column), span));
        }
        match table_folded {
            Some(_)
                if !self.sources.iter().any(|source| {
                    table_folded
                        .as_deref()
                        .is_some_and(|q| source.alias.eq_ignore_ascii_case(q))
                }) =>
            {
                Err(no_such_table(
                    table.map(|id| self.ast.text(id)).unwrap_or(b""),
                    span,
                ))
            }
            _ => Err(no_such_column(self.ast.text(column), span)),
        }
    }

    /// Binds a binary operator, choosing comparison or arithmetic semantics.
    fn bind_binary(
        &mut self,
        op: BinaryOp,
        left: ExprId,
        right: ExprId,
    ) -> Result<BoundExpr, ParseError> {
        let bound_left = self.bind_expr(left)?;
        let bound_right = self.bind_expr(right)?;
        match op {
            BinaryOp::And => Ok(BoundExpr::And(Box::new(bound_left), Box::new(bound_right))),
            BinaryOp::Or => Ok(BoundExpr::Or(Box::new(bound_left), Box::new(bound_right))),
            BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterEqual => {
                let (affinity, collation) = comparison_rules(&bound_left, &bound_right);
                Ok(BoundExpr::Compare {
                    op,
                    left: Box::new(bound_left),
                    right: Box::new(bound_right),
                    affinity,
                    collation,
                })
            }
            BinaryOp::Match | BinaryOp::Regexp => {
                Err(no_such_function(b"regexp", self.ast.expr_span(right)))
            }
            BinaryOp::Extract | BinaryOp::ExtractText => Err(unsupported(
                "the JSON extract operators",
                self.ast.expr_span(right),
            )),
            _ => Ok(BoundExpr::Arithmetic {
                op,
                left: Box::new(bound_left),
                right: Box::new(bound_right),
            }),
        }
    }

    /// Binds a function call, scalar or aggregate.
    fn bind_call(
        &mut self,
        name: ast::NameId,
        distinct: bool,
        arguments: Option<Vec<ExprId>>,
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        let folded = self.ast.folded(name).to_vec();
        if self
            .authorizer
            .authorize(AuthAction::Function { name: &folded })
            == Authorization::Deny
        {
            return Err(denied("not authorized", span));
        }
        let star = arguments.is_none();
        let list = arguments.unwrap_or_default();
        if function::is_aggregate_call(&folded, list.len(), star) {
            let Some(func) =
                function::lookup_aggregate(&folded).or_else(|| function::minmax_aggregate(&folded))
            else {
                return Err(no_such_function(&folded, span));
            };
            if !self.allow_aggregates || self.inside_aggregate {
                return Err(unsupported("misuse of aggregate function", span));
            }
            if star && func != AggregateFunc::Count {
                return Err(wrong_arguments(&folded, span));
            }
            if !function::aggregate_arity_ok(func, if star { 0 } else { list.len() }, star) {
                return Err(wrong_arguments(&folded, span));
            }
            self.inside_aggregate = true;
            let mut bound = Vec::with_capacity(list.len());
            for argument in &list {
                bound.push(self.bind_expr(*argument)?);
            }
            self.inside_aggregate = false;
            let collation = bound
                .first()
                .and_then(BoundExpr::collation)
                .unwrap_or(Collation::Binary);
            let candidate = BoundAggregate {
                func,
                distinct,
                arguments: bound,
                star,
                collation,
            };
            // The same aggregate written twice is one accumulator. It is not
            // only cheaper: `... ORDER BY count(*)` has to name the *same* slot
            // the result column named, or the two are different values that
            // happen to be spelt alike.
            if let Some(slot) = self
                .aggregates
                .iter()
                .position(|existing| existing == &candidate)
            {
                return Ok(BoundExpr::Aggregate { slot });
            }
            self.aggregates.push(candidate);
            return Ok(BoundExpr::Aggregate {
                slot: self.aggregates.len().saturating_sub(1),
            });
        }
        let Some(func) = function::lookup_scalar(&folded) else {
            return Err(no_such_function(&folded, span));
        };
        if star {
            return Err(wrong_arguments(&folded, span));
        }
        if distinct {
            return Err(unsupported("DISTINCT in a scalar function", span));
        }
        if !function::scalar_arity_ok(func, list.len()) {
            return Err(wrong_arguments(&folded, span));
        }
        let mut bound = Vec::with_capacity(list.len());
        for argument in &list {
            bound.push(self.bind_expr(*argument)?);
        }
        let collation = bound
            .first()
            .and_then(BoundExpr::collation)
            .unwrap_or(Collation::Binary);
        Ok(BoundExpr::Function {
            func,
            arguments: bound,
            collation,
        })
    }
}

/// Returns the affinity and collation a comparison between two operands uses.
///
/// SQLite's rule, in order: if either side has a column affinity the comparison
/// applies it, with the left side winning; the collation is the left operand's
/// if it has one, otherwise the right's, otherwise BINARY.
pub fn comparison_rules(left: &BoundExpr, right: &BoundExpr) -> (Option<Affinity>, Collation) {
    let affinity = match (left.affinity(), right.affinity()) {
        (Some(left), Some(right)) => rustdb_value::compare::comparison_affinity(left, right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    };
    let collation = left
        .explicit_collation()
        .or_else(|| right.explicit_collation())
        .or_else(|| left.collation())
        .or_else(|| right.collation())
        .unwrap_or(Collation::Binary);
    (affinity, collation)
}

/// Rewrites an expression so its comparisons use an explicit collation.
///
/// A column takes the collation directly, because that is the cheapest place
/// for it and the planner reads it there when it decides whether an index is
/// usable. A comparison takes it because `a = b COLLATE X` is about the
/// comparison rather than about `b`. Everything else is wrapped, so that a
/// collation on a literal survives to the comparison that will use it.
fn apply_collation(expr: BoundExpr, collation: Collation) -> BoundExpr {
    match expr {
        BoundExpr::Compare {
            op,
            left,
            right,
            affinity,
            ..
        } => BoundExpr::Compare {
            op,
            left,
            right,
            affinity,
            collation,
        },
        other => BoundExpr::Collate {
            operand: Box::new(other),
            collation,
        },
    }
}

/// Converts an integer literal's text into a bound value.
///
/// A decimal literal too large for `i64` becomes a real, which is what SQLite
/// does rather than failing, and a hexadecimal literal wraps into `i64`, which
/// is also what SQLite does.
fn integer_literal(text: &[u8]) -> BoundExpr {
    if text.len() > 2
        && text.first() == Some(&b'0')
        && text
            .get(1)
            .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'x'))
    {
        let mut value: u64 = 0;
        for byte in text.get(2..).unwrap_or(&[]) {
            let digit = (*byte as char).to_digit(16).unwrap_or(0) as u64;
            value = value.wrapping_mul(16).wrapping_add(digit);
        }
        return BoundExpr::Integer(value as i64);
    }
    let cleaned: Vec<u8> = text.iter().copied().filter(|byte| *byte != b'_').collect();
    let (value, syntax) = rustdb_value::numeric::atoi64(&cleaned, rustdb_value::TextEncoding::Utf8);
    if syntax.is_exact() {
        return BoundExpr::Integer(value);
    }
    let parsed = rustdb_value::numeric::atof(&cleaned, rustdb_value::TextEncoding::Utf8);
    BoundExpr::Real(parsed.value)
}

/// Returns a refusal whose text is computed rather than a fixed phrase.
///
/// `Unsupported` carries a `&'static str` because most refusals are one of a
/// closed set of phrases and interning them keeps the error type cheap. A
/// refusal that has to name a column or count something cannot be one of
/// those, so it is reported as an unexpected-input failure carrying the whole
/// sentence, which is the shape SQLite's own messages take.
pub(crate) fn refused(detail: impl Into<String>, span: Span) -> ParseError {
    ParseError::new(
        ParseErrorKind::Unexpected {
            found: detail.into(),
            expected: Vec::new(),
        },
        span,
    )
}

/// Returns an "unsupported construct" failure.
pub(crate) fn unsupported(what: &'static str, span: Span) -> ParseError {
    ParseError::new(ParseErrorKind::Unsupported(what), span)
}

/// Returns a "no such table" failure in SQLite's wording.
pub(crate) fn no_such_table(name: &[u8], span: Span) -> ParseError {
    ParseError::new(
        ParseErrorKind::Unexpected {
            found: format!("no such table: {}", String::from_utf8_lossy(name)),
            expected: Vec::new(),
        },
        span,
    )
}

/// Returns a "no such column" failure in SQLite's wording.
pub(crate) fn no_such_column(name: &[u8], span: Span) -> ParseError {
    ParseError::new(
        ParseErrorKind::Unexpected {
            found: format!("no such column: {}", String::from_utf8_lossy(name)),
            expected: Vec::new(),
        },
        span,
    )
}

/// Returns an "ambiguous column name" failure.
fn ambiguous_column(name: &[u8], span: Span) -> ParseError {
    ParseError::new(
        ParseErrorKind::Unexpected {
            found: format!("ambiguous column name: {}", String::from_utf8_lossy(name)),
            expected: Vec::new(),
        },
        span,
    )
}

/// Returns a "no such function" failure.
fn no_such_function(name: &[u8], span: Span) -> ParseError {
    ParseError::new(
        ParseErrorKind::Unexpected {
            found: format!("no such function: {}", String::from_utf8_lossy(name)),
            expected: Vec::new(),
        },
        span,
    )
}

/// Returns a "wrong number of arguments" failure.
fn wrong_arguments(name: &[u8], span: Span) -> ParseError {
    ParseError::new(
        ParseErrorKind::Unexpected {
            found: format!(
                "wrong number of arguments to function {}()",
                String::from_utf8_lossy(name)
            ),
            expected: Vec::new(),
        },
        span,
    )
}

/// Returns a "no such collation" failure.
fn no_such_collation(name: &[u8], span: Span) -> ParseError {
    ParseError::new(
        ParseErrorKind::Unexpected {
            found: format!(
                "no such collation sequence: {}",
                String::from_utf8_lossy(name)
            ),
            expected: Vec::new(),
        },
        span,
    )
}

/// Returns an "ORDER BY term out of range" failure.
fn order_out_of_range(ordinal: usize, span: Span) -> ParseError {
    ParseError::new(
        ParseErrorKind::Unexpected {
            found: format!(
                "{ordinal}th ORDER BY term out of range - should be between 1 and the number of result columns"
            ),
            expected: Vec::new(),
        },
        span,
    )
}

/// Returns an authorizer refusal.
fn denied(what: &'static str, span: Span) -> ParseError {
    ParseError::new(ParseErrorKind::Unsupported(what), span)
}
