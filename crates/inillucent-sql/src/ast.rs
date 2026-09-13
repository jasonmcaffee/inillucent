//! The arena-backed abstract syntax tree.
//!
//! Invariant: a node is an index into an arena, never a box, so an adversarial
//! nesting depth costs one vector push per node and a walker can be iterative.
//! Every node carries the span it was parsed from, and no node has been
//! normalised: `NOT IN`, `IS NOT DISTINCT FROM` and an implicit alias are all
//! distinct nodes rather than reconstructions, because a diagnostic that has to
//! guess what the user wrote points at the wrong place.
//!
//! Identifiers are interned once per parse. The interned form keeps the
//! original spelling *and* an ASCII-folded lookup key, because SQL name
//! resolution is case-insensitive while `sqlite_schema` records the spelling
//! the user chose.

use crate::lexer::{QuoteForm, Span};

/// An identifier, interned per parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameId(pub u32);

/// An expression node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExprId(pub u32);

/// A compound SELECT.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelectId(pub u32);

/// One arm of a compound SELECT.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelectCoreId(pub u32);

/// A FROM term.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FromTermId(pub u32);

/// A window definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u32);

/// An interned identifier: what was written and what it matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    /// The identifier exactly as written, with quoting removed.
    pub text: Vec<u8>,
    /// The ASCII-folded key names are compared by.
    pub folded: Vec<u8>,
    /// How it was quoted, which decides whether it may become a string.
    pub quote: QuoteForm,
    /// Where it came from.
    pub span: Span,
}

impl Name {
    /// Returns the written spelling as text, for diagnostics and schema SQL.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.text).unwrap_or("")
    }
}

/// A literal value, kept as the bytes it was written as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal {
    /// `NULL`.
    Null,
    /// `TRUE` or `FALSE`, which SQLite treats as 1 and 0.
    Boolean(bool),
    /// An integer literal, as written.
    Integer(Vec<u8>),
    /// A floating-point literal, as written.
    Float(Vec<u8>),
    /// A string literal, unescaped.
    String(Vec<u8>),
    /// A blob literal, decoded.
    Blob(Vec<u8>),
    /// `CURRENT_DATE`, `CURRENT_TIME` or `CURRENT_TIMESTAMP`.
    CurrentDate,
    /// `CURRENT_TIME`.
    CurrentTime,
    /// `CURRENT_TIMESTAMP`.
    CurrentTimestamp,
}

/// A unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-x`
    Negate,
    /// `+x`, which SQLite keeps as a no-op that still forces evaluation.
    Identity,
    /// `~x`
    BitNot,
    /// `NOT x`
    Not,
}

/// A binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    /// `OR`
    Or,
    /// `AND`
    And,
    /// `=`
    Equal,
    /// `<>`
    NotEqual,
    /// `<`
    Less,
    /// `<=`
    LessEqual,
    /// `>`
    Greater,
    /// `>=`
    GreaterEqual,
    /// `+`
    Add,
    /// `-`
    Subtract,
    /// `*`
    Multiply,
    /// `/`
    Divide,
    /// `%`
    Modulo,
    /// `||`
    Concat,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `<<`
    ShiftLeft,
    /// `>>`
    ShiftRight,
    /// `->`
    Extract,
    /// `->>`
    ExtractText,
    /// `MATCH`
    Match,
    /// `REGEXP`
    Regexp,
    /// `<->`
    L2Distance,
    /// `<=>`
    CosineDistance,
    /// `<#>`
    NegativeInnerProduct,
    /// `<+>`
    L1Distance,
    /// `<~>`
    HammingDistance,
    /// `<%>`
    JaccardDistance,
}

/// Which pattern operator was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternOp {
    /// `LIKE`
    Like,
    /// `GLOB`
    Glob,
    /// `REGEXP`
    Regexp,
    /// `MATCH`
    Match,
}

/// The right-hand side of `IN`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InRhs {
    /// `IN (1, 2, 3)`, including the empty list.
    List(Vec<ExprId>),
    /// `IN (SELECT ...)`.
    Select(SelectId),
    /// `IN table` or `IN schema.table`.
    Table {
        /// The schema qualifier, when written.
        database: Option<NameId>,
        /// The table or table-valued function name.
        table: NameId,
        /// Arguments, when the name is a table-valued function.
        arguments: Option<Vec<ExprId>>,
    },
}

/// A `RAISE()` action inside a trigger body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseAction {
    /// `RAISE(IGNORE)`
    Ignore,
    /// `RAISE(ROLLBACK, msg)`
    Rollback,
    /// `RAISE(ABORT, msg)`
    Abort,
    /// `RAISE(FAIL, msg)`
    Fail,
}

/// An expression, in the shape it was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// A literal.
    Literal(Literal),
    /// A bound parameter.
    Parameter {
        /// The one-based parameter index assigned at parse time.
        index: u32,
        /// The written name, for `:name` style parameters.
        name: Option<NameId>,
    },
    /// A column reference, with as much qualification as was written.
    Column {
        /// The schema qualifier.
        database: Option<NameId>,
        /// The table qualifier or alias.
        table: Option<NameId>,
        /// The column name.
        column: NameId,
    },
    /// `*` or `table.*`, legal only where the grammar allows it.
    Star {
        /// The table qualifier, when written.
        table: Option<NameId>,
    },
    /// A unary operator applied to one operand.
    Unary {
        /// Which operator.
        op: UnaryOp,
        /// The operand.
        operand: ExprId,
    },
    /// A binary operator applied to two operands.
    Binary {
        /// Which operator.
        op: BinaryOp,
        /// The left operand.
        left: ExprId,
        /// The right operand.
        right: ExprId,
    },
    /// `expr COLLATE name`.
    Collate {
        /// The operand.
        operand: ExprId,
        /// The collation name.
        collation: NameId,
    },
    /// `CAST(expr AS type)`.
    Cast {
        /// The operand.
        operand: ExprId,
        /// The declared type, as written.
        declared: NameId,
    },
    /// `expr [NOT] LIKE|GLOB|REGEXP|MATCH pattern [ESCAPE expr]`.
    Pattern {
        /// Whether `NOT` was written.
        negated: bool,
        /// Which operator.
        op: PatternOp,
        /// The value being matched.
        operand: ExprId,
        /// The pattern.
        pattern: ExprId,
        /// The `ESCAPE` argument, when written.
        escape: Option<ExprId>,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// Whether `NOT` was written.
        negated: bool,
        /// The value being tested.
        operand: ExprId,
        /// The lower bound.
        low: ExprId,
        /// The upper bound.
        high: ExprId,
    },
    /// `expr [NOT] IN rhs`.
    In {
        /// Whether `NOT` was written.
        negated: bool,
        /// The value being tested.
        operand: ExprId,
        /// What it is tested against.
        rhs: InRhs,
    },
    /// `expr ISNULL` / `expr NOTNULL` / `expr IS [NOT] NULL`.
    IsNull {
        /// Whether the test is for not-null.
        negated: bool,
        /// The operand.
        operand: ExprId,
    },
    /// `left IS [NOT] [DISTINCT FROM] right`.
    Is {
        /// Whether `NOT` was written.
        negated: bool,
        /// Whether the `DISTINCT FROM` spelling was used.
        distinct_from: bool,
        /// The left operand.
        left: ExprId,
        /// The right operand.
        right: ExprId,
    },
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
    Case {
        /// The base operand, when the form has one.
        operand: Option<ExprId>,
        /// The `WHEN`/`THEN` pairs, in written order.
        branches: Vec<(ExprId, ExprId)>,
        /// The `ELSE` arm.
        otherwise: Option<ExprId>,
    },
    /// A function call, aggregate or scalar or window.
    Function {
        /// The function name.
        name: NameId,
        /// Whether `DISTINCT` was written.
        distinct: bool,
        /// The arguments, or `None` for `count(*)`.
        arguments: Option<Vec<ExprId>>,
        /// An `ORDER BY` inside the argument list.
        order_by: Vec<OrderTerm>,
        /// A `FILTER (WHERE ...)` clause.
        filter: Option<ExprId>,
        /// An `OVER` clause.
        over: Option<WindowId>,
    },
    /// `[NOT] EXISTS (SELECT ...)`.
    Exists {
        /// Whether `NOT` was written.
        negated: bool,
        /// The subquery.
        select: SelectId,
    },
    /// A scalar subquery.
    Subquery(SelectId),
    /// A parenthesised list of two or more expressions.
    RowValue(Vec<ExprId>),
    /// `RAISE(...)`, legal only inside a trigger body.
    Raise {
        /// Which action.
        action: RaiseAction,
        /// The message, when the action takes one.
        message: Option<Vec<u8>>,
    },
}

/// Ascending or descending.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortOrder {
    /// `ASC`, the default.
    #[default]
    Ascending,
    /// `DESC`.
    Descending,
}

/// Where NULLs sort, when written explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NullOrder {
    /// `NULLS FIRST`.
    First,
    /// `NULLS LAST`.
    Last,
}

/// One term of an `ORDER BY`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderTerm {
    /// The expression, which may be an ordinal or an alias.
    pub expr: ExprId,
    /// The written or defaulted direction.
    pub order: SortOrder,
    /// The written null ordering, when there was one.
    pub nulls: Option<NullOrder>,
}

/// One result column of a SELECT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultColumn {
    /// The expression, which may be `*` or `table.*`.
    pub expr: ExprId,
    /// The alias, when one was written.
    pub alias: Option<NameId>,
    /// Whether the alias was written with `AS`.
    pub alias_was_explicit: bool,
    /// The span of the whole result column.
    pub span: Span,
}

/// Which join was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    /// A comma, which is a cross join that may still be reordered.
    Comma,
    /// `[INNER] JOIN`.
    Inner,
    /// `CROSS JOIN`, which SQLite refuses to reorder.
    Cross,
    /// `LEFT [OUTER] JOIN`.
    Left,
    /// `RIGHT [OUTER] JOIN`.
    Right,
    /// `FULL [OUTER] JOIN`.
    Full,
}

/// The `ON` or `USING` constraint of a join.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinConstraint {
    /// No constraint was written.
    None,
    /// `ON expr`.
    On(ExprId),
    /// `USING (a, b)`.
    Using(Vec<NameId>),
}

/// How a FROM term names its rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromSource {
    /// A table, view or table-valued function.
    Table {
        /// The schema qualifier.
        database: Option<NameId>,
        /// The object name.
        name: NameId,
        /// Arguments, when it is a table-valued function.
        arguments: Option<Vec<ExprId>>,
        /// `INDEXED BY name`, or `NOT INDEXED`.
        indexed_by: IndexHint,
    },
    /// A subquery.
    Subquery(SelectId),
    /// A parenthesised join, which is one term to whatever contains it.
    Join(Vec<FromTermId>),
}

/// An `INDEXED BY` hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexHint {
    /// Nothing was written.
    None,
    /// `NOT INDEXED`.
    NotIndexed,
    /// `INDEXED BY name`.
    IndexedBy(NameId),
}

/// One term of a FROM clause, with the join that attached it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FromTerm {
    /// Where the rows come from.
    pub source: FromSource,
    /// The alias, when one was written.
    pub alias: Option<NameId>,
    /// The join that attaches this term to the one before it.
    pub join: JoinKind,
    /// Whether `NATURAL` was written.
    pub natural: bool,
    /// The `ON` or `USING` constraint.
    pub constraint: JoinConstraint,
    /// The span of the whole term.
    pub span: Span,
}

/// A window frame's unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameUnit {
    /// `ROWS`.
    Rows,
    /// `RANGE`.
    Range,
    /// `GROUPS`.
    Groups,
}

/// One end of a window frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameBound {
    /// `UNBOUNDED PRECEDING`.
    UnboundedPreceding,
    /// `expr PRECEDING`.
    Preceding(ExprId),
    /// `CURRENT ROW`.
    CurrentRow,
    /// `expr FOLLOWING`.
    Following(ExprId),
    /// `UNBOUNDED FOLLOWING`.
    UnboundedFollowing,
}

/// A frame's `EXCLUDE` clause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameExclude {
    /// `EXCLUDE NO OTHERS`, the default.
    NoOthers,
    /// `EXCLUDE CURRENT ROW`.
    CurrentRow,
    /// `EXCLUDE GROUP`.
    Group,
    /// `EXCLUDE TIES`.
    Ties,
}

/// A window definition, named or inline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// The window this one inherits from, when written.
    pub base: Option<NameId>,
    /// `PARTITION BY`.
    pub partition_by: Vec<ExprId>,
    /// `ORDER BY`.
    pub order_by: Vec<OrderTerm>,
    /// The frame unit, when a frame was written.
    pub unit: Option<FrameUnit>,
    /// The frame start.
    pub start: Option<FrameBound>,
    /// The frame end.
    pub end: Option<FrameBound>,
    /// The `EXCLUDE` clause.
    pub exclude: FrameExclude,
    /// The span of the definition.
    pub span: Span,
}

/// The rows of one arm of a compound SELECT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectBody {
    /// `SELECT ...`.
    Select {
        /// Whether `DISTINCT` was written.
        distinct: bool,
        /// Whether `ALL` was written.
        all: bool,
        /// The result columns.
        columns: Vec<ResultColumn>,
        /// The FROM terms, in written order.
        from: Vec<FromTermId>,
        /// The WHERE clause.
        filter: Option<ExprId>,
        /// The GROUP BY terms.
        group_by: Vec<ExprId>,
        /// The HAVING clause.
        having: Option<ExprId>,
        /// Named windows.
        windows: Vec<(NameId, WindowId)>,
    },
    /// `VALUES (...), (...)`.
    Values(Vec<Vec<ExprId>>),
}

/// One arm of a compound SELECT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectCore {
    /// What the arm produces.
    pub body: SelectBody,
    /// The span of the arm.
    pub span: Span,
}

/// A compound operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompoundOp {
    /// `UNION`.
    Union,
    /// `UNION ALL`.
    UnionAll,
    /// `INTERSECT`.
    Intersect,
    /// `EXCEPT`.
    Except,
}

/// A common table expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonTableExpr {
    /// The name it is bound to.
    pub name: NameId,
    /// The explicit column list, when written.
    pub columns: Vec<NameId>,
    /// `MATERIALIZED` or `NOT MATERIALIZED`, when written.
    pub materialized: Option<bool>,
    /// The query.
    pub select: SelectId,
}

/// A `WITH` prefix.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct With {
    /// Whether `RECURSIVE` was written.
    pub recursive: bool,
    /// The CTEs, in written order.
    pub ctes: Vec<CommonTableExpr>,
}

/// A complete SELECT: a `WITH` prefix, compound arms, and the tail clauses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Select {
    /// The `WITH` prefix.
    pub with: With,
    /// The first arm.
    pub first: SelectCoreId,
    /// Later arms, each with the operator that joined it.
    pub compounds: Vec<(CompoundOp, SelectCoreId)>,
    /// The `ORDER BY`, which belongs to the whole compound.
    pub order_by: Vec<OrderTerm>,
    /// The `LIMIT` expression.
    pub limit: Option<ExprId>,
    /// The `OFFSET` expression.
    pub offset: Option<ExprId>,
    /// The span of the whole statement.
    pub span: Span,
}

/// A conflict-resolution algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictAction {
    /// `ROLLBACK`.
    Rollback,
    /// `ABORT`, the default.
    Abort,
    /// `FAIL`.
    Fail,
    /// `IGNORE`.
    Ignore,
    /// `REPLACE`.
    Replace,
}

/// A column constraint, in written order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnConstraint {
    /// `PRIMARY KEY [ASC|DESC] [conflict] [AUTOINCREMENT]`.
    PrimaryKey {
        /// The written direction.
        order: SortOrder,
        /// The conflict clause.
        on_conflict: Option<ConflictAction>,
        /// Whether `AUTOINCREMENT` was written.
        autoincrement: bool,
    },
    /// `NOT NULL [conflict]`.
    NotNull(Option<ConflictAction>),
    /// `NULL`, which SQLite accepts and ignores.
    Null,
    /// `UNIQUE [conflict]`.
    Unique(Option<ConflictAction>),
    /// `CHECK (expr)`.
    ///
    /// **No conflict clause**, which is SQLite's grammar and not an omission:
    /// `ccons ::= CHECK LP expr RP` has no `onconf`, so
    /// `b INTEGER CHECK(b < 9) ON CONFLICT IGNORE` is a syntax error there and
    /// has to be one here. Only a *table*-level `CHECK` takes the clause - see
    /// [`TableConstraint::Check`].
    Check(ExprId),
    /// `DEFAULT expr`.
    Default(ExprId),
    /// `COLLATE name`.
    Collate(NameId),
    /// `REFERENCES ...`.
    References(ForeignKeyClause),
    /// `GENERATED ALWAYS AS (expr) [STORED|VIRTUAL]`.
    Generated {
        /// The generating expression.
        expr: ExprId,
        /// Whether `STORED` was written.
        stored: bool,
    },
}

/// A foreign-key clause, on a column or on a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeyClause {
    /// The parent table.
    pub table: NameId,
    /// The parent columns, when written.
    pub columns: Vec<NameId>,
    /// The `ON DELETE`/`ON UPDATE`/`MATCH` clauses, as written.
    pub actions: Vec<ForeignKeyAction>,
    /// Whether the constraint is deferrable.
    pub deferrable: Option<bool>,
    /// Whether it is initially deferred.
    pub initially_deferred: bool,
}

/// One `ON DELETE`, `ON UPDATE` or `MATCH` clause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignKeyAction {
    /// `ON DELETE <action>`.
    OnDelete(ReferentialAction),
    /// `ON UPDATE <action>`.
    OnUpdate(ReferentialAction),
    /// `MATCH name`.
    Match(NameId),
}

/// What a referential action does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferentialAction {
    /// `SET NULL`.
    SetNull,
    /// `SET DEFAULT`.
    SetDefault,
    /// `CASCADE`.
    Cascade,
    /// `RESTRICT`.
    Restrict,
    /// `NO ACTION`.
    NoAction,
}

/// One column of a `CREATE TABLE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    /// The column name.
    pub name: NameId,
    /// The declared type, exactly as written, when there was one.
    pub declared_type: Option<Vec<u8>>,
    /// The constraints, in written order, each with its optional name.
    pub constraints: Vec<(Option<NameId>, ColumnConstraint)>,
    /// The span of the definition.
    pub span: Span,
}

/// One indexed column of a table constraint or an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexedColumn {
    /// The key expression, which may be a bare column.
    pub expr: ExprId,
    /// An explicit collation.
    pub collation: Option<NameId>,
    /// The direction.
    pub order: SortOrder,
}

/// A table-level constraint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableConstraint {
    /// `PRIMARY KEY (...)`.
    PrimaryKey {
        /// The key columns.
        columns: Vec<IndexedColumn>,
        /// The conflict clause.
        on_conflict: Option<ConflictAction>,
        /// Whether `AUTOINCREMENT` was written.
        autoincrement: bool,
    },
    /// `UNIQUE (...)`.
    Unique {
        /// The key columns.
        columns: Vec<IndexedColumn>,
        /// The conflict clause.
        on_conflict: Option<ConflictAction>,
    },
    /// `CHECK (expr) [conflict]`.
    ///
    /// **Parsed and then ignored, which is what SQLite does with it.**
    /// `tcons ::= CHECK LP expr RP onconf` accepts the clause and
    /// `sqlite3AddCheckConstraint` never reads it, so
    /// `CONSTRAINT small CHECK(b < 9) ON CONFLICT FAIL` behaves exactly as
    /// `ABORT`: measured against the pinned 3.53.4, an `INSERT` of three rows
    /// whose second fails keeps none of them.
    ///
    /// It is in the tree rather than discarded at the token because the table's
    /// `CREATE` text is stored and re-parsed on every open, so the grammar has
    /// to accept everything the text can hold. Not accepting it did not cost
    /// one statement a clause - it made the `CREATE TABLE` a parse error, and
    /// every statement after it said `no such table`.
    Check {
        /// The predicate.
        expr: ExprId,
        /// The conflict clause, accepted and not acted on.
        on_conflict: Option<ConflictAction>,
    },
    /// `FOREIGN KEY (...) REFERENCES ...`.
    ForeignKey {
        /// The child columns.
        columns: Vec<NameId>,
        /// The parent reference.
        clause: ForeignKeyClause,
    },
}

/// The body of a `CREATE TABLE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateTableBody {
    /// A column list.
    Columns {
        /// The columns, in written order.
        columns: Vec<ColumnDef>,
        /// The table constraints, in written order, each with its name.
        constraints: Vec<(Option<NameId>, TableConstraint)>,
        /// Whether `WITHOUT ROWID` was written.
        without_rowid: bool,
        /// Whether `STRICT` was written.
        strict: bool,
    },
    /// `CREATE TABLE ... AS SELECT ...`.
    AsSelect(SelectId),
}

/// An `UPSERT` clause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upsert {
    /// The conflict target columns, when written.
    pub target: Vec<IndexedColumn>,
    /// The conflict target's `WHERE`.
    pub target_filter: Option<ExprId>,
    /// The `DO UPDATE SET` assignments, empty for `DO NOTHING`.
    pub assignments: Vec<(Vec<NameId>, ExprId)>,
    /// Whether the action is `DO UPDATE`.
    pub do_update: bool,
    /// The `DO UPDATE`'s `WHERE`.
    pub filter: Option<ExprId>,
}

/// What an INSERT inserts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertSource {
    /// `VALUES`, or any SELECT.
    Select(SelectId),
    /// `DEFAULT VALUES`.
    DefaultValues,
}

/// An `INSERT` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insert {
    /// The `WITH` prefix.
    pub with: With,
    /// The conflict algorithm from `INSERT OR ...` or `REPLACE`.
    pub on_conflict: Option<ConflictAction>,
    /// The schema qualifier.
    pub database: Option<NameId>,
    /// The target table.
    pub table: NameId,
    /// The table alias.
    pub alias: Option<NameId>,
    /// The column list, when written.
    pub columns: Vec<NameId>,
    /// The rows.
    pub source: InsertSource,
    /// The `ON CONFLICT` clauses, in written order.
    pub upserts: Vec<Upsert>,
    /// The `RETURNING` columns.
    pub returning: Vec<ResultColumn>,
}

/// An `UPDATE` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// The `WITH` prefix.
    pub with: With,
    /// The conflict algorithm from `UPDATE OR ...`.
    pub on_conflict: Option<ConflictAction>,
    /// The target term, which carries its own alias and index hint.
    pub target: FromTermId,
    /// The `SET` assignments; a group of names is the `(a, b) = ...` form.
    pub assignments: Vec<(Vec<NameId>, ExprId)>,
    /// An `UPDATE ... FROM` clause.
    pub from: Vec<FromTermId>,
    /// The `WHERE` clause.
    pub filter: Option<ExprId>,
    /// The `RETURNING` columns.
    pub returning: Vec<ResultColumn>,
    /// The `ORDER BY`, which SQLite allows with `LIMIT`.
    pub order_by: Vec<OrderTerm>,
    /// The `LIMIT`.
    pub limit: Option<ExprId>,
    /// The `OFFSET`.
    pub offset: Option<ExprId>,
    /// Where the clause the reference build has no grammar for was written.
    ///
    /// `ORDER BY` and `LIMIT` on a `DELETE` or an `UPDATE` are a compile-time
    /// option in SQLite, and the pinned build is not compiled with it - so the
    /// reference answers `near "ORDER": syntax error` and points at the word.
    /// The syntax register requires these to *parse* here, so the refusal is
    /// the binder's; it needs the position to be able to point at the same
    /// word, and this is where the parser leaves it.
    pub limited_at: Option<(Limited, crate::lexer::Span)>,
}

/// Which of the two words a limited `DELETE` or `UPDATE` was written with.
///
/// The reference names the first one it cannot parse, so a statement carrying
/// both reports `ORDER` and one carrying only a `LIMIT` reports `LIMIT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limited {
    /// `ORDER BY`.
    OrderBy,
    /// `LIMIT`.
    Limit,
}

impl Limited {
    /// Returns the word the refusal quotes.
    pub fn word(self) -> &'static str {
        match self {
            Limited::OrderBy => "ORDER",
            Limited::Limit => "LIMIT",
        }
    }
}

/// A `DELETE` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delete {
    /// The `WITH` prefix.
    pub with: With,
    /// The target term.
    pub target: FromTermId,
    /// The `WHERE` clause.
    pub filter: Option<ExprId>,
    /// The `RETURNING` columns.
    pub returning: Vec<ResultColumn>,
    /// The `ORDER BY`.
    pub order_by: Vec<OrderTerm>,
    /// The `LIMIT`.
    pub limit: Option<ExprId>,
    /// The `OFFSET`.
    pub offset: Option<ExprId>,
    /// Where the clause the reference build has no grammar for was written.
    ///
    /// `ORDER BY` and `LIMIT` on a `DELETE` or an `UPDATE` are a compile-time
    /// option in SQLite, and the pinned build is not compiled with it - so the
    /// reference answers `near "ORDER": syntax error` and points at the word.
    /// The syntax register requires these to *parse* here, so the refusal is
    /// the binder's; it needs the position to be able to point at the same
    /// word, and this is where the parser leaves it.
    pub limited_at: Option<(Limited, crate::lexer::Span)>,
}

/// Which kind of object a `DROP` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    /// A table.
    Table,
    /// An index.
    Index,
    /// A view.
    View,
    /// A trigger.
    Trigger,
}

/// What an `ALTER TABLE` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlterAction {
    /// `RENAME TO name`.
    RenameTo(NameId),
    /// `RENAME [COLUMN] a TO b`.
    RenameColumn {
        /// The current name.
        from: NameId,
        /// The new name.
        to: NameId,
    },
    /// `ADD [COLUMN] def`.
    AddColumn(ColumnDef),
    /// `DROP [COLUMN] name`.
    DropColumn(NameId),
}

/// When a trigger fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerTime {
    /// `BEFORE`.
    Before,
    /// `AFTER`.
    After,
    /// `INSTEAD OF`.
    InsteadOf,
}

/// What a trigger fires on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerEvent {
    /// `DELETE`.
    Delete,
    /// `INSERT`.
    Insert,
    /// `UPDATE [OF a, b]`.
    Update(Vec<NameId>),
}

/// A `PRAGMA` argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PragmaValue {
    /// Nothing was written.
    None,
    /// `= value` or `(value)`.
    Value(ExprId),
    /// `(name)`, which is a bare word rather than an expression.
    Name(NameId),
}

/// A parsed statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Statement {
    /// An empty statement, which SQLite compiles to nothing.
    Empty,
    /// `SELECT` or `VALUES`.
    Select(SelectId),
    /// `INSERT` or `REPLACE`.
    Insert(Box<Insert>),
    /// `UPDATE`.
    Update(Box<Update>),
    /// `DELETE`.
    Delete(Box<Delete>),
    /// `CREATE TABLE`.
    CreateTable {
        /// Whether `TEMP` was written.
        temporary: bool,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// The schema qualifier.
        database: Option<NameId>,
        /// The table name.
        name: NameId,
        /// The body.
        body: CreateTableBody,
    },
    /// `CREATE INDEX`.
    CreateIndex {
        /// Whether `UNIQUE` was written.
        unique: bool,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// The schema qualifier.
        database: Option<NameId>,
        /// The index name.
        name: NameId,
        /// The table it indexes.
        table: NameId,
        /// The module named by `USING`, when one was.
        ///
        /// SQLite has no `USING` on `CREATE INDEX`; PostgreSQL does, and it is
        /// how pgvector spells `USING hnsw`. This engine borrows the spelling
        /// for the same purpose: an index whose structure is not a b-tree.
        /// A plain `CREATE INDEX` leaves it `None` and nothing
        /// downstream changes.
        using: Option<NameId>,
        /// The key columns.
        columns: Vec<IndexedColumn>,
        /// The storage parameters `WITH ( ... )` named, as written.
        ///
        /// `m = 16`, `ef_construction = 64` and the rest: raw `name = value`
        /// slices, in the order they were written, for the structure named by
        /// `using` to read. Empty for a plain `CREATE INDEX`, which has no
        /// structure to read them.
        settings: Vec<Vec<u8>>,
        /// The partial-index predicate.
        filter: Option<ExprId>,
    },
    /// `CREATE VIEW`.
    CreateView {
        /// Whether `TEMP` was written.
        temporary: bool,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// The schema qualifier.
        database: Option<NameId>,
        /// The view name.
        name: NameId,
        /// The explicit column list.
        columns: Vec<NameId>,
        /// The query.
        select: SelectId,
    },
    /// `CREATE TRIGGER`.
    CreateTrigger {
        /// Whether `TEMP` was written.
        temporary: bool,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// The schema qualifier.
        database: Option<NameId>,
        /// The trigger name.
        name: NameId,
        /// When it fires.
        time: Option<TriggerTime>,
        /// What it fires on.
        event: TriggerEvent,
        /// The table it is attached to.
        table: NameId,
        /// Whether `FOR EACH ROW` was written.
        for_each_row: bool,
        /// The `WHEN` guard.
        when: Option<ExprId>,
        /// The body statements, in written order.
        body: Vec<Statement>,
    },
    /// `CREATE VIRTUAL TABLE`.
    CreateVirtualTable {
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// The schema qualifier.
        database: Option<NameId>,
        /// The table name.
        name: NameId,
        /// The module name.
        module: NameId,
        /// The module arguments, as written source slices.
        arguments: Vec<Vec<u8>>,
    },
    /// `DROP TABLE|INDEX|VIEW|TRIGGER`.
    Drop {
        /// Which kind of object.
        kind: ObjectKind,
        /// Whether `IF EXISTS` was written.
        if_exists: bool,
        /// The schema qualifier.
        database: Option<NameId>,
        /// The object name.
        name: NameId,
    },
    /// `ALTER TABLE`.
    AlterTable {
        /// The schema qualifier.
        database: Option<NameId>,
        /// The table name.
        table: NameId,
        /// What to do to it.
        action: AlterAction,
    },
    /// `BEGIN`.
    Begin {
        /// `DEFERRED`, `IMMEDIATE` or `EXCLUSIVE`, when written.
        behaviour: Option<TransactionBehaviour>,
    },
    /// `COMMIT` or `END`.
    Commit,
    /// `ROLLBACK [TO savepoint]`.
    Rollback {
        /// The savepoint to roll back to.
        savepoint: Option<NameId>,
    },
    /// `SAVEPOINT name`.
    Savepoint(NameId),
    /// `RELEASE [SAVEPOINT] name`.
    Release(NameId),
    /// `PRAGMA`.
    Pragma {
        /// The schema qualifier.
        database: Option<NameId>,
        /// The pragma name.
        name: NameId,
        /// The argument.
        value: PragmaValue,
    },
    /// `ATTACH`.
    Attach {
        /// The file expression.
        file: ExprId,
        /// The schema name expression.
        schema: ExprId,
        /// The `KEY` expression.
        key: Option<ExprId>,
    },
    /// `DETACH`.
    Detach {
        /// The schema name expression.
        schema: ExprId,
    },
    /// `VACUUM`.
    Vacuum {
        /// The schema to vacuum.
        database: Option<NameId>,
        /// The `INTO` target.
        into: Option<ExprId>,
    },
    /// `ANALYZE`.
    Analyze {
        /// The schema qualifier.
        database: Option<NameId>,
        /// The object to analyze.
        name: Option<NameId>,
    },
    /// `REINDEX`.
    Reindex {
        /// The schema qualifier.
        database: Option<NameId>,
        /// The collation, table or index to reindex.
        name: Option<NameId>,
    },
    /// `EXPLAIN` or `EXPLAIN QUERY PLAN`.
    Explain {
        /// Whether `QUERY PLAN` was written.
        query_plan: bool,
        /// The statement being explained.
        inner: Box<Statement>,
    },
}

/// The behaviour of a `BEGIN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionBehaviour {
    /// `DEFERRED`.
    Deferred,
    /// `IMMEDIATE`.
    Immediate,
    /// `EXCLUSIVE`.
    Exclusive,
}

/// The arena every node of one parse lives in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ast {
    names: Vec<Name>,
    /// Where a folded name already is, so `intern` is a lookup rather than a
    /// scan.
    ///
    /// **`intern` was a linear scan of every name interned so far, so N
    /// distinct identifiers cost N-squared comparisons (task-1932, H8).** The
    /// `SqlLength` default is 1 GiB, so a statement naming two hundred thousand
    /// distinct columns is well inside what the parser accepts and was
    /// quadratic to parse. The key is the whole of what the scan compared -
    /// the folded text, the quote form, and the written spelling - so an entry
    /// found here is an entry the scan would have found.
    interned: std::collections::HashMap<(Vec<u8>, QuoteForm, Vec<u8>), u32>,
    exprs: Vec<Expr>,
    expr_spans: Vec<Span>,
    /// How deep each expression's own subtree is, one entry per node.
    ///
    /// **`Limit::ExprDepth` was declared in `compat/limits.toml` and enforced
    /// nowhere (task-1932, H8).** The parser charges `Limit::ParserDepth` in
    /// `enter`/`leave`, which counts recursion, and the two are different
    /// measurements: a flat chain `a1 = 1 AND a2 = 2 AND ...` enters and leaves
    /// `parse_expr_bp` once per term, so the recursion counter never
    /// accumulates, while the tree grows one level per term with nothing
    /// counting it. SQLite refuses at depth 1000. A tree that deep is accepted
    /// here and then walked recursively by the binder, the planner and the
    /// executor, each of which overflows the stack at some depth nobody
    /// measured.
    ///
    /// A node's depth is one more than the deepest of its children, and a child
    /// is always already in the arena when its parent is added, so this is one
    /// pass over the child ids at `add_expr` rather than a walk.
    expr_depths: Vec<u32>,
    /// The deepest expression tree in the arena.
    max_expr_depth: u32,
    selects: Vec<Select>,
    cores: Vec<SelectCore>,
    from_terms: Vec<FromTerm>,
    windows: Vec<Window>,
    bytes: usize,
}

impl Ast {
    /// Returns an empty arena.
    pub fn new() -> Ast {
        Ast::default()
    }

    /// Empties the arena, keeping the memory it has already taken.
    ///
    /// **So that a second statement costs no allocations.** Every one of these
    /// vectors is empty at `Ast::new` and grows on its first push, so parsing
    /// `SELECT 1` takes half a dozen trips to the allocator - about 270 ns of a
    /// 1,337 ns prepare on this platform's CRT heap. A parser handed a cleared
    /// arena pushes into capacity that is already there.
    ///
    /// It is a `clear` rather than a `new` for exactly that reason, and the
    /// names are cleared with everything else: `intern` returns an existing id
    /// for equal text, so a name left behind from the previous statement would
    /// be a live id in the next one's arena.
    pub fn clear(&mut self) {
        self.names.clear();
        self.interned.clear();
        self.exprs.clear();
        self.expr_spans.clear();
        self.expr_depths.clear();
        self.max_expr_depth = 0;
        self.selects.clear();
        self.cores.clear();
        self.from_terms.clear();
        self.windows.clear();
        self.bytes = 0;
    }

    /// Returns the number of arena bytes charged so far.
    ///
    /// This is what the `max_ast_bytes` limit is charged against. It counts the
    /// node structures rather than the source, because the source is borrowed.
    pub fn charged_bytes(&self) -> usize {
        self.bytes
    }

    /// Interns an identifier, returning the id of an equal existing entry when
    /// there is one.
    ///
    /// **A map rather than a scan (task-1932, H8).** This walked every name
    /// interned so far and compared three fields against each, so a statement
    /// naming N distinct identifiers cost N-squared comparisons - and the
    /// `SqlLength` default is 1 GiB, which leaves room for hundreds of
    /// thousands of them. The key is exactly what the scan compared, so the
    /// answer is the same one and only the cost changed.
    ///
    /// The count is charged against `Limit::Column` for the same reason the
    /// depth is charged below: a bound that exists in `compat/limits.toml` and
    /// is enforced nowhere is not a bound. It is generous - a name is a column,
    /// a table, an alias, a function or a collation, so one statement
    /// legitimately interns more names than any one table has columns - and it
    /// is a ceiling on an arena that has to fit in memory rather than a
    /// statement about the schema.
    pub fn intern(&mut self, text: Vec<u8>, quote: QuoteForm, span: Span) -> NameId {
        let folded: Vec<u8> = text.iter().map(|byte| byte.to_ascii_lowercase()).collect();
        let key = (folded, quote, text);
        if let Some(index) = self.interned.get(&key) {
            return NameId(*index);
        }
        let (folded, quote, text) = key.clone();
        self.bytes = self
            .bytes
            .saturating_add(text.len().saturating_add(folded.len()).saturating_add(32));
        let index = self.names.len() as u32;
        self.names.push(Name {
            text,
            folded,
            quote,
            span,
        });
        self.interned.insert(key, index);
        NameId(index)
    }

    /// Returns how many distinct identifiers have been interned.
    pub fn name_count(&self) -> usize {
        self.names.len()
    }

    /// Returns the depth of the deepest expression tree in the arena.
    ///
    /// What `Limit::ExprDepth` is charged against. See `expr_depths`.
    pub fn max_expr_depth(&self) -> u32 {
        self.max_expr_depth
    }

    /// Returns how deep one expression's own subtree is.
    ///
    /// @param id - the node
    pub fn expr_depth(&self, id: ExprId) -> u32 {
        self.expr_depths.get(id.0 as usize).copied().unwrap_or(0)
    }

    /// Returns an interned name.
    pub fn name(&self, id: NameId) -> Option<&Name> {
        self.names.get(id.0 as usize)
    }

    /// Returns the folded key of an interned name, or an empty slice.
    pub fn folded(&self, id: NameId) -> &[u8] {
        self.names.get(id.0 as usize).map_or(&[], |n| &n.folded)
    }

    /// Returns the written spelling of an interned name, or an empty slice.
    pub fn text(&self, id: NameId) -> &[u8] {
        self.names.get(id.0 as usize).map_or(&[], |n| &n.text)
    }

    /// Adds an expression node.
    pub fn add_expr(&mut self, expr: Expr, span: Span) -> ExprId {
        self.bytes = self
            .bytes
            .saturating_add(core::mem::size_of::<Expr>().saturating_add(8));
        let depth = self.depth_of(&expr);
        self.max_expr_depth = self.max_expr_depth.max(depth);
        self.exprs.push(expr);
        self.expr_spans.push(span);
        self.expr_depths.push(depth);
        ExprId(self.exprs.len().saturating_sub(1) as u32)
    }

    /// Returns how deep a node about to be added is.
    ///
    /// One more than the deepest of its children. Every child is already in the
    /// arena - the parser builds bottom up - so this reads their recorded
    /// depths rather than walking them, which is what keeps `add_expr` the
    /// constant-time push it was.
    ///
    /// A subquery's depth is one: the `SELECT` it names has an expression arena
    /// of its own and its own `max_expr_depth`, and charging the outer tree for
    /// the inner one would refuse a shallow expression that happens to contain
    /// a deep query rather than the deep query itself.
    ///
    /// @param expr - the node
    fn depth_of(&self, expr: &Expr) -> u32 {
        let deepest = |ids: &[ExprId]| -> u32 {
            ids.iter().map(|id| self.expr_depth(*id)).max().unwrap_or(0)
        };
        let children = match expr {
            Expr::Literal(_)
            | Expr::Parameter { .. }
            | Expr::Column { .. }
            | Expr::Star { .. }
            | Expr::Exists { .. }
            | Expr::Subquery(_)
            | Expr::Raise { .. } => 0,
            Expr::Unary { operand, .. }
            | Expr::Collate { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::IsNull { operand, .. } => self.expr_depth(*operand),
            Expr::Binary { left, right, .. } | Expr::Is { left, right, .. } => {
                self.expr_depth(*left).max(self.expr_depth(*right))
            }
            Expr::Pattern {
                operand,
                pattern,
                escape,
                ..
            } => self
                .expr_depth(*operand)
                .max(self.expr_depth(*pattern))
                .max(escape.map(|id| self.expr_depth(id)).unwrap_or(0)),
            Expr::Between {
                operand, low, high, ..
            } => self
                .expr_depth(*operand)
                .max(self.expr_depth(*low))
                .max(self.expr_depth(*high)),
            Expr::In { operand, rhs, .. } => {
                let right = match rhs {
                    InRhs::List(ids) => deepest(ids),
                    InRhs::Select(_) => 0,
                    InRhs::Table { arguments, .. } => {
                        arguments.as_deref().map(deepest).unwrap_or(0)
                    }
                };
                self.expr_depth(*operand).max(right)
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                let mut deep = operand.map(|id| self.expr_depth(id)).unwrap_or(0);
                for (when, then) in branches {
                    deep = deep.max(self.expr_depth(*when)).max(self.expr_depth(*then));
                }
                deep.max(otherwise.map(|id| self.expr_depth(id)).unwrap_or(0))
            }
            Expr::Function {
                arguments, filter, ..
            } => arguments
                .as_deref()
                .map(deepest)
                .unwrap_or(0)
                .max(filter.map(|id| self.expr_depth(id)).unwrap_or(0)),
            Expr::RowValue(ids) => deepest(ids),
        };
        children.saturating_add(1)
    }

    /// Returns an expression node.
    pub fn expr(&self, id: ExprId) -> Option<&Expr> {
        self.exprs.get(id.0 as usize)
    }

    /// Returns the span an expression was parsed from.
    pub fn expr_span(&self, id: ExprId) -> Span {
        self.expr_spans
            .get(id.0 as usize)
            .copied()
            .unwrap_or_default()
    }

    /// Returns the number of expression nodes in the arena.
    pub fn expr_count(&self) -> usize {
        self.exprs.len()
    }

    /// Adds a compound SELECT.
    pub fn add_select(&mut self, select: Select) -> SelectId {
        self.bytes = self
            .bytes
            .saturating_add(core::mem::size_of::<Select>().saturating_add(32));
        self.selects.push(select);
        SelectId(self.selects.len().saturating_sub(1) as u32)
    }

    /// Returns a compound SELECT.
    pub fn select(&self, id: SelectId) -> Option<&Select> {
        self.selects.get(id.0 as usize)
    }

    /// Adds one arm of a compound SELECT.
    pub fn add_core(&mut self, core: SelectCore) -> SelectCoreId {
        self.bytes = self
            .bytes
            .saturating_add(core::mem::size_of::<SelectCore>().saturating_add(64));
        self.cores.push(core);
        SelectCoreId(self.cores.len().saturating_sub(1) as u32)
    }

    /// Returns one arm of a compound SELECT.
    pub fn core(&self, id: SelectCoreId) -> Option<&SelectCore> {
        self.cores.get(id.0 as usize)
    }

    /// Adds a FROM term.
    pub fn add_from_term(&mut self, term: FromTerm) -> FromTermId {
        self.bytes = self
            .bytes
            .saturating_add(core::mem::size_of::<FromTerm>().saturating_add(32));
        self.from_terms.push(term);
        FromTermId(self.from_terms.len().saturating_sub(1) as u32)
    }

    /// Returns a FROM term.
    pub fn from_term(&self, id: FromTermId) -> Option<&FromTerm> {
        self.from_terms.get(id.0 as usize)
    }

    /// Returns a FROM term for modification.
    ///
    /// A join's `ON` or `USING` clause follows the table it constrains, so the
    /// term is stored first and its constraint attached once the parser has
    /// read it. Building the term out of order instead would mean holding a
    /// half-built node across a recursive parse.
    pub fn from_term_mut(&mut self, id: FromTermId) -> Option<&mut FromTerm> {
        self.from_terms.get_mut(id.0 as usize)
    }

    /// Adds a window definition.
    pub fn add_window(&mut self, window: Window) -> WindowId {
        self.bytes = self
            .bytes
            .saturating_add(core::mem::size_of::<Window>().saturating_add(32));
        self.windows.push(window);
        WindowId(self.windows.len().saturating_sub(1) as u32)
    }

    /// Returns a window definition.
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(id.0 as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interning is by folded key *and* spelling, so `a` and `A` are two
    /// entries that compare equal by key rather than one entry that has
    /// forgotten which spelling reached it.
    #[test]
    fn interning_keeps_the_spelling_and_folds_the_key() {
        let mut ast = Ast::new();
        let lower = ast.intern(b"abc".to_vec(), QuoteForm::Bare, Span::default());
        let upper = ast.intern(b"ABC".to_vec(), QuoteForm::Bare, Span::default());
        let again = ast.intern(b"abc".to_vec(), QuoteForm::Bare, Span::default());
        assert_eq!(lower, again);
        assert_ne!(lower, upper);
        assert_eq!(ast.folded(lower), ast.folded(upper));
        assert_eq!(ast.text(upper), b"ABC");
    }

    /// Every node id resolves, and an id from another arena does not panic.
    #[test]
    fn an_unknown_id_returns_none_rather_than_panicking() {
        let ast = Ast::new();
        assert!(ast.expr(ExprId(7)).is_none());
        assert!(ast.select(SelectId(7)).is_none());
        assert!(ast.name(NameId(7)).is_none());
        assert_eq!(ast.expr_span(ExprId(7)), Span::default());
    }

    /// The charge grows with the arena, which is what the limit is checked
    /// against before a deep parse allocates.
    #[test]
    fn the_arena_charges_for_what_it_holds() {
        let mut ast = Ast::new();
        let before = ast.charged_bytes();
        ast.add_expr(Expr::Literal(Literal::Null), Span::default());
        assert!(ast.charged_bytes() > before);
    }
}
