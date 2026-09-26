//! Proving every statement form the engine accepts has a matrix case.
//!
//! Invariant: **the inventory is computed from the engine, never from a list
//! kept beside it.** The AST walker matches every enum in `inillucent-sql`'s
//! `ast.rs` with no wildcard arm, so a new variant stops this file compiling
//! until someone names it here; the function, pragma, module and collation
//! names come from the `PRAGMA ..._list` answers of a live database; the
//! syntax examples come from `compat/syntax.toml`; the capability rows come
//! from the driver's `CAPABILITIES`. Each name must be reached by at least one
//! case the change cadence runs, or [`report`] lists it as missing and
//! `tooling::matrix_inventory` fails.
//!
//! Section 9.1 of `tasks/task-2135-sql-statement-matrix-tdd.md` is the design.

use std::collections::{BTreeMap, BTreeSet};

use inillucent_base::limits::Limits;
use inillucent_sql::ast::{
    AlterAction, Ast, BinaryOp, ColumnConstraint, ColumnDef, CompoundOp, ConflictAction,
    CreateTableBody, Expr, ExprId, ForeignKeyAction, ForeignKeyClause, FrameBound, FrameExclude,
    FrameUnit, FromSource, FromTermId, InRhs, IndexHint, IndexedColumn, InsertSource,
    JoinConstraint, JoinKind, Limited, Literal, NameId, NullOrder, ObjectKind, OrderTerm,
    PatternOp, PragmaValue, RaiseAction, ReferentialAction, ResultColumn, SelectBody, SelectId,
    SortOrder, Statement, TableConstraint, TransactionBehaviour, TriggerEvent, TriggerTime,
    UnaryOp, Upsert, WindowId, With,
};

use crate::statement_matrix::case::{Case, Expect, Record};
use crate::statement_matrix::group::{self, Cadence};

/// Every statement family, in the order section 4.1 of the design lists them,
/// plus `retained`, the shrunk failures.
pub const FAMILIES: &[&str] = &[
    "select",
    "join",
    "compound",
    "cte",
    "window",
    "subquery",
    "expression",
    "function",
    "insert",
    "update",
    "delete",
    "ddl_table",
    "ddl_index",
    "ddl_view",
    "trigger",
    "constraint",
    "transaction",
    "vtab",
    "schema",
    "maintenance",
    "pragma",
    "vector",
    "retained",
];

/// Declares the report names of one AST enum and a function that names a value.
///
/// Every variant is written as `Variant {}` or `Variant { .. }` style by the
/// macro, which Rust accepts for unit, tuple and struct variants alike, so one
/// list both builds the constant and the match. The match has no wildcard arm:
/// a variant added to `ast.rs` and not listed here is a compile error.
macro_rules! variants {
    ($list:ident, $namer:ident, $enum:ident { $($variant:ident),* $(,)? }) => {
        /// Every variant of the enum, by the name the report prints.
        const $list: &[&str] = &[$(concat!(stringify!($enum), "::", stringify!($variant))),*];

        /// The report name of one value's variant.
        ///
        /// @param value - the value to name
        fn $namer(value: &$enum) -> &'static str {
            match value {
                $($enum::$variant { .. } => concat!(stringify!($enum), "::", stringify!($variant)),)*
            }
        }
    };
}

variants!(
    LITERAL,
    literal_name,
    Literal {
        Null,
        Boolean,
        Integer,
        Float,
        String,
        Blob,
        CurrentDate,
        CurrentTime,
        CurrentTimestamp
    }
);
variants!(
    UNARY,
    unary_name,
    UnaryOp {
        Negate,
        Identity,
        BitNot,
        Not
    }
);
variants!(
    BINARY,
    binary_name,
    BinaryOp {
        Or,
        And,
        Equal,
        NotEqual,
        Less,
        LessEqual,
        Greater,
        GreaterEqual,
        Add,
        Subtract,
        Multiply,
        Divide,
        Modulo,
        Concat,
        BitAnd,
        BitOr,
        ShiftLeft,
        ShiftRight,
        Extract,
        ExtractText,
        Match,
        Regexp,
        L2Distance,
        CosineDistance,
        NegativeInnerProduct,
        L1Distance,
        HammingDistance,
        JaccardDistance,
    }
);
variants!(
    PATTERN,
    pattern_name,
    PatternOp {
        Like,
        Glob,
        Regexp,
        Match
    }
);
variants!(
    IN_RHS,
    in_rhs_name,
    InRhs {
        List,
        Select,
        Table
    }
);
variants!(
    RAISE,
    raise_name,
    RaiseAction {
        Ignore,
        Rollback,
        Abort,
        Fail
    }
);
variants!(
    EXPR,
    expr_name,
    Expr {
        Literal,
        Parameter,
        Column,
        Star,
        Unary,
        Binary,
        Collate,
        Cast,
        Pattern,
        Between,
        In,
        IsNull,
        Is,
        Case,
        Function,
        Exists,
        Subquery,
        RowValue,
        Raise,
    }
);
variants!(
    SORT,
    sort_name,
    SortOrder {
        Ascending,
        Descending
    }
);
variants!(NULLS, nulls_name, NullOrder { First, Last });
variants!(
    JOIN,
    join_name,
    JoinKind {
        Comma,
        Inner,
        Cross,
        Left,
        Right,
        Full
    }
);
variants!(
    CONSTRAINT,
    constraint_name,
    JoinConstraint { None, On, Using }
);
variants!(
    FROM,
    from_name,
    FromSource {
        Table,
        Subquery,
        Join
    }
);
variants!(
    HINT,
    hint_name,
    IndexHint {
        None,
        NotIndexed,
        IndexedBy
    }
);
variants!(
    UNIT,
    unit_name,
    FrameUnit {
        Rows,
        Range,
        Groups
    }
);
variants!(
    BOUND,
    bound_name,
    FrameBound {
        UnboundedPreceding,
        Preceding,
        CurrentRow,
        Following,
        UnboundedFollowing
    }
);
variants!(
    EXCLUDE,
    exclude_name,
    FrameExclude {
        NoOthers,
        CurrentRow,
        Group,
        Ties
    }
);
variants!(BODY, body_name, SelectBody { Select, Values });
variants!(
    COMPOUND,
    compound_name,
    CompoundOp {
        Union,
        UnionAll,
        Intersect,
        Except
    }
);
variants!(
    CONFLICT,
    conflict_name,
    ConflictAction {
        Rollback,
        Abort,
        Fail,
        Ignore,
        Replace
    }
);
variants!(
    COLUMN,
    column_name,
    ColumnConstraint {
        PrimaryKey,
        NotNull,
        Null,
        Unique,
        Check,
        Default,
        Collate,
        References,
        Generated
    }
);
variants!(
    FK_ACTION,
    fk_action_name,
    ForeignKeyAction {
        OnDelete,
        OnUpdate,
        Match
    }
);
variants!(
    REFERENTIAL,
    referential_name,
    ReferentialAction {
        SetNull,
        SetDefault,
        Cascade,
        Restrict,
        NoAction
    }
);
variants!(
    TABLE_CONSTRAINT,
    table_constraint_name,
    TableConstraint {
        PrimaryKey,
        Unique,
        Check,
        ForeignKey
    }
);
variants!(
    TABLE_BODY,
    table_body_name,
    CreateTableBody { Columns, AsSelect }
);
variants!(
    INSERT_SOURCE,
    insert_source_name,
    InsertSource {
        Select,
        DefaultValues
    }
);
variants!(LIMITED, limited_name, Limited { OrderBy, Limit });
variants!(
    OBJECT,
    object_name,
    ObjectKind {
        Table,
        Index,
        View,
        Trigger
    }
);
variants!(
    ALTER,
    alter_name,
    AlterAction {
        RenameTo,
        RenameColumn,
        AddColumn,
        DropColumn
    }
);
variants!(
    TIME,
    time_name,
    TriggerTime {
        Before,
        After,
        InsteadOf
    }
);
variants!(
    EVENT,
    event_name,
    TriggerEvent {
        Delete,
        Insert,
        Update
    }
);
variants!(
    PRAGMA_VALUE,
    pragma_value_name,
    PragmaValue { None, Value, Name }
);
variants!(
    STATEMENT,
    statement_name,
    Statement {
        Empty,
        Select,
        Insert,
        Update,
        Delete,
        CreateTable,
        CreateIndex,
        CreateView,
        CreateTrigger,
        CreateVirtualTable,
        Drop,
        AlterTable,
        Begin,
        Commit,
        Rollback,
        Savepoint,
        Release,
        Pragma,
        Attach,
        Detach,
        Vacuum,
        Analyze,
        Reindex,
        Explain,
    }
);
variants!(
    BEHAVIOUR,
    behaviour_name,
    TransactionBehaviour {
        Deferred,
        Immediate,
        Exclusive
    }
);

/// Every enum's variant list, in the order the report prints them.
///
/// `Statement::Empty` is left in: an empty statement is a form the parser
/// accepts, and `;` alone is a case.
const EVERY_VARIANT: &[&[&str]] = &[
    STATEMENT,
    EXPR,
    LITERAL,
    UNARY,
    BINARY,
    PATTERN,
    IN_RHS,
    RAISE,
    SORT,
    NULLS,
    JOIN,
    CONSTRAINT,
    FROM,
    HINT,
    UNIT,
    BOUND,
    EXCLUDE,
    BODY,
    COMPOUND,
    CONFLICT,
    COLUMN,
    FK_ACTION,
    REFERENTIAL,
    TABLE_CONSTRAINT,
    TABLE_BODY,
    INSERT_SOURCE,
    LIMITED,
    OBJECT,
    ALTER,
    TIME,
    EVENT,
    PRAGMA_VALUE,
    BEHAVIOUR,
];

/// The optional clauses a variant can carry, which the enums alone do not
/// name. `CREATE TEMP TABLE` and `CREATE TABLE` are one `Statement` variant,
/// and a suite with only the second would pass the variant check.
const CLAUSES: &[&str] = &[
    "With::recursive",
    "Cte::columns",
    "Cte::materialized",
    "Cte::not_materialized",
    "Select::order_by",
    "Select::limit",
    "Select::offset",
    "Select::distinct",
    "Select::all",
    "Select::group_by",
    "Select::having",
    "Select::named_window",
    "ResultColumn::alias",
    "FromTerm::alias",
    "FromTerm::natural",
    "FromTerm::schema",
    "FromTerm::table_function",
    "Window::base",
    "Window::partition_by",
    "Window::order_by",
    "Window::frame",
    "OrderTerm::nulls",
    "Function::distinct",
    "Function::order_by",
    "Function::filter",
    "Function::over",
    "Function::star",
    "Pattern::escape",
    "Pattern::negated",
    "Case::operand",
    "Case::otherwise",
    "Between::negated",
    "In::negated",
    "Is::distinct_from",
    "Exists::negated",
    "Parameter::named",
    "Column::schema",
    "Insert::columns",
    "Insert::alias",
    "Insert::or",
    "Insert::returning",
    "Insert::with",
    "Upsert::target",
    "Upsert::target_filter",
    "Upsert::do_update",
    "Upsert::do_nothing",
    "Upsert::filter",
    "Upsert::several",
    "Update::or",
    "Update::from",
    "Update::returning",
    "Update::row_value_assignment",
    "Update::with",
    "Update::limited",
    "Delete::returning",
    "Delete::with",
    "Delete::limited",
    "ColumnDef::declared_type",
    "ColumnDef::untyped",
    "ColumnDef::named_constraint",
    "PrimaryKey::autoincrement",
    "PrimaryKey::descending",
    "Generated::stored",
    "Generated::virtual",
    "ForeignKey::deferrable",
    "ForeignKey::initially_deferred",
    "ForeignKey::columns",
    "CreateTable::without_rowid",
    "CreateTable::strict",
    "CreateTable::temporary",
    "CreateTable::if_not_exists",
    "CreateTable::schema",
    "CreateIndex::unique",
    "CreateIndex::if_not_exists",
    "CreateIndex::filter",
    "CreateIndex::using",
    "CreateIndex::settings",
    "CreateIndex::expression",
    "IndexedColumn::collation",
    "IndexedColumn::descending",
    "CreateView::temporary",
    "CreateView::columns",
    "CreateView::if_not_exists",
    "CreateTrigger::temporary",
    "CreateTrigger::when",
    "CreateTrigger::for_each_row",
    "CreateTrigger::update_of",
    "CreateTrigger::no_time",
    "CreateVirtualTable::if_not_exists",
    "CreateVirtualTable::arguments",
    "Drop::if_exists",
    "Rollback::to",
    "Attach::key",
    "Vacuum::schema",
    "Vacuum::into",
    "Analyze::name",
    "Reindex::name",
    "Explain::query_plan",
    "Explain::plain",
    "Pragma::schema",
];

/// The forms the parser never builds, each with the reason, so no case can
/// reach them. The report fails when a case does reach one, because then the
/// reason is no longer true and the form belongs back in the checked list.
const UNREACHABLE: &[(&str, &str)] = &[
    (
        "BinaryOp::Match",
        "`a MATCH b` is parsed as `Expr::Pattern` with `PatternOp::Match`; the binder has an arm for the binary form and nothing builds it",
    ),
    (
        "BinaryOp::Regexp",
        "`a REGEXP b` is parsed as `Expr::Pattern` with `PatternOp::Regexp`",
    ),
    (
        "InRhs::Table",
        "`x IN t` is parsed as `InRhs::Select` over `SELECT * FROM t`; the cases in subquery/constructs.slt run it",
    ),
    (
        "Exists::negated",
        "`NOT EXISTS (...)` is parsed as `UnaryOp::Not` over `Expr::Exists`, whose `negated` is always false",
    ),
    (
        "IndexedColumn::collation",
        "the key expression is parsed first and takes the `COLLATE` as `Expr::Collate`, so the separate field is never filled",
    ),
];

/// Every name the inventory checks, with the first case that reached it.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// AST variants and clauses, by report name.
    pub forms: BTreeMap<&'static str, String>,
    /// Function names, folded to lower case.
    pub functions: BTreeMap<String, String>,
    /// Pragma names, folded to lower case.
    pub pragmas: BTreeMap<String, String>,
    /// Virtual table modules and table valued functions, folded.
    pub modules: BTreeMap<String, String>,
    /// Collation names, folded.
    pub collations: BTreeMap<String, String>,
}

/// One walk over one parsed statement, writing what it meets into [`Seen`].
struct Walk<'a> {
    ast: &'a Ast,
    case: &'a str,
    seen: &'a mut Seen,
}

impl Walk<'_> {
    /// Records a form, keeping the first case that reached it.
    ///
    /// @param form - the report name
    fn form(&mut self, form: &'static str) {
        self.seen
            .forms
            .entry(form)
            .or_insert_with(|| self.case.to_string());
    }

    /// Records a name in one of the register maps.
    ///
    /// @param which - which register the name belongs to
    /// @param name - the name as the case wrote it
    fn register(&mut self, which: Register, name: &str) {
        let map = match which {
            Register::Function => &mut self.seen.functions,
            Register::Pragma => &mut self.seen.pragmas,
            Register::Module => &mut self.seen.modules,
            Register::Collation => &mut self.seen.collations,
        };
        map.entry(name.to_ascii_lowercase())
            .or_insert_with(|| self.case.to_string());
    }

    /// The written spelling of an interned name.
    ///
    /// @param id - the name
    fn text(&self, id: NameId) -> String {
        String::from_utf8_lossy(self.ast.text(id)).into_owned()
    }
}

/// Which live register a name is checked against.
#[derive(Clone, Copy)]
enum Register {
    /// `PRAGMA function_list`.
    Function,
    /// `PRAGMA pragma_list`.
    Pragma,
    /// `PRAGMA module_list`.
    Module,
    /// `PRAGMA collation_list`.
    Collation,
}

impl Walk<'_> {
    /// Walks one statement.
    ///
    /// @param statement - the statement
    fn statement(&mut self, statement: &Statement) {
        self.form(statement_name(statement));
        match statement {
            Statement::Empty | Statement::Commit => {}
            Statement::Select(select) => self.select(*select),
            Statement::Insert(insert) => self.insert(insert),
            Statement::Update(update) => self.update(update),
            Statement::Delete(delete) => self.delete(delete),
            Statement::CreateTable {
                temporary,
                if_not_exists,
                database,
                body,
                ..
            } => {
                self.flag(*temporary, "CreateTable::temporary");
                self.flag(*if_not_exists, "CreateTable::if_not_exists");
                self.flag(database.is_some(), "CreateTable::schema");
                self.table_body(body);
            }
            Statement::CreateIndex { .. } => self.create_index(statement),
            Statement::CreateView {
                temporary,
                if_not_exists,
                columns,
                select,
                ..
            } => {
                self.flag(*temporary, "CreateView::temporary");
                self.flag(*if_not_exists, "CreateView::if_not_exists");
                self.flag(!columns.is_empty(), "CreateView::columns");
                self.select(*select);
            }
            Statement::CreateTrigger { .. } => self.create_trigger(statement),
            Statement::CreateVirtualTable {
                if_not_exists,
                module,
                arguments,
                ..
            } => {
                self.flag(*if_not_exists, "CreateVirtualTable::if_not_exists");
                self.flag(!arguments.is_empty(), "CreateVirtualTable::arguments");
                let module = self.text(*module);
                self.register(Register::Module, &module);
            }
            _ => self.other_statement(statement),
        }
    }

    /// Walks the statements [`Walk::statement`] leaves to keep its length down.
    ///
    /// @param statement - the statement
    fn other_statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Drop {
                kind, if_exists, ..
            } => {
                self.form(object_name(kind));
                self.flag(*if_exists, "Drop::if_exists");
            }
            Statement::AlterTable { action, .. } => {
                self.form(alter_name(action));
                if let AlterAction::AddColumn(column) = action {
                    self.column_def(column);
                }
            }
            Statement::Begin { behaviour } => {
                if let Some(behaviour) = behaviour {
                    self.form(behaviour_name(behaviour));
                }
            }
            Statement::Rollback { savepoint } => self.flag(savepoint.is_some(), "Rollback::to"),
            Statement::Pragma {
                database,
                name,
                value,
            } => {
                self.flag(database.is_some(), "Pragma::schema");
                let name = self.text(*name);
                self.register(Register::Pragma, &name);
                self.form(pragma_value_name(value));
                if let PragmaValue::Value(expr) = value {
                    self.expr(*expr);
                }
            }
            Statement::Attach { file, schema, key } => {
                self.expr(*file);
                self.expr(*schema);
                self.flag(key.is_some(), "Attach::key");
                self.maybe_expr(*key);
            }
            Statement::Detach { schema } => self.expr(*schema),
            Statement::Vacuum { database, into } => {
                self.flag(database.is_some(), "Vacuum::schema");
                self.flag(into.is_some(), "Vacuum::into");
                self.maybe_expr(*into);
            }
            Statement::Analyze { name, .. } => self.flag(name.is_some(), "Analyze::name"),
            Statement::Reindex { name, .. } => self.flag(name.is_some(), "Reindex::name"),
            Statement::Explain { query_plan, inner } => {
                self.form(if *query_plan {
                    "Explain::query_plan"
                } else {
                    "Explain::plain"
                });
                self.statement(inner);
            }
            Statement::Savepoint(_) | Statement::Release(_) => {}
            _ => {}
        }
    }

    /// Records a clause when its condition holds.
    ///
    /// @param present - whether the clause was written
    /// @param form - its report name
    fn flag(&mut self, present: bool, form: &'static str) {
        if present {
            self.form(form);
        }
    }

    /// Walks an optional expression.
    ///
    /// @param expr - the expression, when there is one
    fn maybe_expr(&mut self, expr: Option<ExprId>) {
        if let Some(expr) = expr {
            self.expr(expr);
        }
    }

    /// Walks a `CREATE INDEX`.
    ///
    /// @param statement - the statement, which is a `Statement::CreateIndex`
    fn create_index(&mut self, statement: &Statement) {
        if let Statement::CreateIndex {
            unique,
            if_not_exists,
            using,
            columns,
            settings,
            filter,
            ..
        } = statement
        {
            self.flag(*unique, "CreateIndex::unique");
            self.flag(*if_not_exists, "CreateIndex::if_not_exists");
            self.flag(using.is_some(), "CreateIndex::using");
            if let Some(using) = using {
                let module = self.text(*using);
                self.register(Register::Module, &module);
            }
            self.flag(!settings.is_empty(), "CreateIndex::settings");
            self.flag(filter.is_some(), "CreateIndex::filter");
            self.maybe_expr(*filter);
            for column in columns {
                let bare = matches!(self.ast.expr(column.expr), Some(Expr::Column { .. }));
                self.flag(!bare, "CreateIndex::expression");
                self.indexed_column(column);
            }
        }
    }

    /// Walks a `CREATE TRIGGER`.
    ///
    /// @param statement - the statement, which is a `Statement::CreateTrigger`
    fn create_trigger(&mut self, statement: &Statement) {
        if let Statement::CreateTrigger {
            temporary,
            time,
            event,
            for_each_row,
            when,
            body,
            ..
        } = statement
        {
            self.flag(*temporary, "CreateTrigger::temporary");
            self.flag(*for_each_row, "CreateTrigger::for_each_row");
            self.flag(when.is_some(), "CreateTrigger::when");
            self.maybe_expr(*when);
            match time {
                Some(time) => self.form(time_name(time)),
                None => self.form("CreateTrigger::no_time"),
            }
            self.form(event_name(event));
            if let TriggerEvent::Update(columns) = event {
                self.flag(!columns.is_empty(), "CreateTrigger::update_of");
            }
            for inner in body {
                self.statement(inner);
            }
        }
    }

    /// Walks the `WITH` prefix.
    ///
    /// @param with - the prefix
    fn with(&mut self, with: &With) {
        self.flag(with.recursive, "With::recursive");
        for cte in &with.ctes {
            self.flag(!cte.columns.is_empty(), "Cte::columns");
            match cte.materialized {
                Some(true) => self.form("Cte::materialized"),
                Some(false) => self.form("Cte::not_materialized"),
                None => {}
            }
            self.select(cte.select);
        }
    }

    /// Walks a complete SELECT.
    ///
    /// @param id - the select
    fn select(&mut self, id: SelectId) {
        let Some(select) = self.ast.select(id).cloned() else {
            return;
        };
        self.with(&select.with);
        self.core(select.first);
        for (op, core) in &select.compounds {
            self.form(compound_name(op));
            self.core(*core);
        }
        self.flag(!select.order_by.is_empty(), "Select::order_by");
        self.order_terms(&select.order_by);
        self.flag(select.limit.is_some(), "Select::limit");
        self.flag(select.offset.is_some(), "Select::offset");
        self.maybe_expr(select.limit);
        self.maybe_expr(select.offset);
    }

    /// Walks one arm of a compound SELECT.
    ///
    /// @param id - the arm
    fn core(&mut self, id: inillucent_sql::ast::SelectCoreId) {
        let Some(core) = self.ast.core(id).cloned() else {
            return;
        };
        self.form(body_name(&core.body));
        match &core.body {
            SelectBody::Select {
                distinct,
                all,
                columns,
                from,
                filter,
                group_by,
                having,
                windows,
            } => {
                self.flag(*distinct, "Select::distinct");
                self.flag(*all, "Select::all");
                self.result_columns(columns);
                for term in from {
                    self.from_term(*term);
                }
                self.maybe_expr(*filter);
                self.flag(!group_by.is_empty(), "Select::group_by");
                for expr in group_by {
                    self.expr(*expr);
                }
                self.flag(having.is_some(), "Select::having");
                self.maybe_expr(*having);
                self.flag(!windows.is_empty(), "Select::named_window");
                for (_, window) in windows {
                    self.window(*window);
                }
            }
            SelectBody::Values(rows) => {
                for expr in rows.iter().flatten() {
                    self.expr(*expr);
                }
            }
        }
    }

    /// Walks result columns, including a `RETURNING` list.
    ///
    /// @param columns - the columns
    fn result_columns(&mut self, columns: &[ResultColumn]) {
        for column in columns {
            self.flag(column.alias.is_some(), "ResultColumn::alias");
            self.expr(column.expr);
        }
    }

    /// Walks an `ORDER BY` list.
    ///
    /// @param terms - the terms
    fn order_terms(&mut self, terms: &[OrderTerm]) {
        for term in terms {
            self.form(sort_name(&term.order));
            if let Some(nulls) = &term.nulls {
                self.form("OrderTerm::nulls");
                self.form(nulls_name(nulls));
            }
            self.expr(term.expr);
        }
    }

    /// Walks one FROM term.
    ///
    /// @param id - the term
    fn from_term(&mut self, id: FromTermId) {
        let Some(term) = self.ast.from_term(id).cloned() else {
            return;
        };
        self.form(join_name(&term.join));
        self.form(constraint_name(&term.constraint));
        self.flag(term.natural, "FromTerm::natural");
        self.flag(term.alias.is_some(), "FromTerm::alias");
        if let JoinConstraint::On(expr) = &term.constraint {
            self.expr(*expr);
        }
        self.form(from_name(&term.source));
        match &term.source {
            FromSource::Table {
                database,
                name,
                arguments,
                indexed_by,
            } => {
                self.flag(database.is_some(), "FromTerm::schema");
                self.form(hint_name(indexed_by));
                if let Some(arguments) = arguments {
                    self.form("FromTerm::table_function");
                    self.table_function(*name, arguments);
                } else {
                    self.table_name(*name);
                }
            }
            FromSource::Subquery(select) => self.select(*select),
            FromSource::Join(terms) => {
                for term in terms {
                    self.from_term(*term);
                }
            }
        }
    }

    /// Records a table valued function and walks its arguments.
    ///
    /// `pragma_<name>(...)` is the pragma `<name>`; anything else is the
    /// module of the same name, which is how `json_each` and
    /// `generate_series` are registered.
    ///
    /// @param name - the function name
    /// @param arguments - its arguments
    fn table_function(&mut self, name: NameId, arguments: &[ExprId]) {
        let name = self.text(name);
        self.named_table(&name);
        for argument in arguments {
            self.expr(*argument);
        }
    }

    /// Records a table name that is a table valued function written with no
    /// arguments, such as `pragma_function_list` or `json_each`.
    ///
    /// @param name - the table name
    fn table_name(&mut self, name: NameId) {
        let name = self.text(name);
        self.named_table(&name);
    }

    /// Records a name that may be a pragma or a module used as a table.
    ///
    /// @param name - the name as written
    fn named_table(&mut self, name: &str) {
        let folded = name.to_ascii_lowercase();
        match folded.strip_prefix("pragma_") {
            Some(pragma) => self.register(Register::Pragma, pragma),
            None => self.register(Register::Module, &folded),
        }
    }

    /// Walks a window definition.
    ///
    /// @param id - the window
    fn window(&mut self, id: WindowId) {
        let Some(window) = self.ast.window(id).cloned() else {
            return;
        };
        self.flag(window.base.is_some(), "Window::base");
        self.flag(!window.partition_by.is_empty(), "Window::partition_by");
        for expr in &window.partition_by {
            self.expr(*expr);
        }
        self.flag(!window.order_by.is_empty(), "Window::order_by");
        self.order_terms(&window.order_by);
        if let Some(unit) = &window.unit {
            self.form("Window::frame");
            self.form(unit_name(unit));
        }
        for bound in window.start.iter().chain(window.end.iter()) {
            self.form(bound_name(bound));
            if let FrameBound::Preceding(expr) | FrameBound::Following(expr) = bound {
                self.expr(*expr);
            }
        }
        self.form(exclude_name(&window.exclude));
    }

    /// Walks the body of a `CREATE TABLE`.
    ///
    /// @param body - the body
    fn table_body(&mut self, body: &CreateTableBody) {
        self.form(table_body_name(body));
        match body {
            CreateTableBody::Columns {
                columns,
                constraints,
                without_rowid,
                strict,
            } => {
                self.flag(*without_rowid, "CreateTable::without_rowid");
                self.flag(*strict, "CreateTable::strict");
                for column in columns {
                    self.column_def(column);
                }
                for (name, constraint) in constraints {
                    self.flag(name.is_some(), "ColumnDef::named_constraint");
                    self.table_constraint(constraint);
                }
            }
            CreateTableBody::AsSelect(select) => self.select(*select),
        }
    }

    /// Walks one column definition.
    ///
    /// @param column - the column
    fn column_def(&mut self, column: &ColumnDef) {
        self.flag(column.declared_type.is_some(), "ColumnDef::declared_type");
        self.flag(column.declared_type.is_none(), "ColumnDef::untyped");
        for (name, constraint) in &column.constraints {
            self.flag(name.is_some(), "ColumnDef::named_constraint");
            self.column_constraint(constraint);
        }
    }

    /// Walks one column constraint.
    ///
    /// @param constraint - the constraint
    fn column_constraint(&mut self, constraint: &ColumnConstraint) {
        self.form(column_name(constraint));
        match constraint {
            ColumnConstraint::PrimaryKey {
                order,
                on_conflict,
                autoincrement,
            } => {
                self.flag(*autoincrement, "PrimaryKey::autoincrement");
                self.flag(*order == SortOrder::Descending, "PrimaryKey::descending");
                self.conflict(on_conflict.as_ref());
            }
            ColumnConstraint::NotNull(on_conflict) | ColumnConstraint::Unique(on_conflict) => {
                self.conflict(on_conflict.as_ref())
            }
            ColumnConstraint::Null => {}
            ColumnConstraint::Check(expr) | ColumnConstraint::Default(expr) => self.expr(*expr),
            ColumnConstraint::Collate(name) => {
                let name = self.text(*name);
                self.register(Register::Collation, &name);
            }
            ColumnConstraint::References(clause) => self.foreign_key(clause),
            ColumnConstraint::Generated { expr, stored } => {
                self.form(if *stored {
                    "Generated::stored"
                } else {
                    "Generated::virtual"
                });
                self.expr(*expr);
            }
        }
    }

    /// Walks one table constraint.
    ///
    /// @param constraint - the constraint
    fn table_constraint(&mut self, constraint: &TableConstraint) {
        self.form(table_constraint_name(constraint));
        match constraint {
            TableConstraint::PrimaryKey {
                columns,
                on_conflict,
                autoincrement,
            } => {
                self.flag(*autoincrement, "PrimaryKey::autoincrement");
                self.conflict(on_conflict.as_ref());
                for column in columns {
                    self.indexed_column(column);
                }
            }
            TableConstraint::Unique {
                columns,
                on_conflict,
            } => {
                self.conflict(on_conflict.as_ref());
                for column in columns {
                    self.indexed_column(column);
                }
            }
            TableConstraint::Check { expr, on_conflict } => {
                self.conflict(on_conflict.as_ref());
                self.expr(*expr);
            }
            TableConstraint::ForeignKey { clause, .. } => {
                self.form("ForeignKey::columns");
                self.foreign_key(clause);
            }
        }
    }

    /// Records a conflict clause when one was written.
    ///
    /// @param action - the clause
    fn conflict(&mut self, action: Option<&ConflictAction>) {
        if let Some(action) = action {
            self.form(conflict_name(action));
        }
    }

    /// Walks a foreign key clause.
    ///
    /// @param clause - the clause
    fn foreign_key(&mut self, clause: &ForeignKeyClause) {
        self.flag(clause.deferrable.is_some(), "ForeignKey::deferrable");
        self.flag(clause.initially_deferred, "ForeignKey::initially_deferred");
        for action in &clause.actions {
            self.form(fk_action_name(action));
            match action {
                ForeignKeyAction::OnDelete(what) | ForeignKeyAction::OnUpdate(what) => {
                    self.form(referential_name(what))
                }
                ForeignKeyAction::Match(_) => {}
            }
        }
    }

    /// Walks one indexed column.
    ///
    /// @param column - the column
    fn indexed_column(&mut self, column: &IndexedColumn) {
        self.form(sort_name(&column.order));
        self.flag(
            column.order == SortOrder::Descending,
            "IndexedColumn::descending",
        );
        if let Some(collation) = column.collation {
            self.form("IndexedColumn::collation");
            let name = self.text(collation);
            self.register(Register::Collation, &name);
        }
        self.expr(column.expr);
    }
}

impl Walk<'_> {
    /// Walks an `INSERT`.
    ///
    /// @param insert - the statement
    fn insert(&mut self, insert: &inillucent_sql::ast::Insert) {
        self.flag(!insert.with.ctes.is_empty(), "Insert::with");
        self.with(&insert.with);
        self.flag(insert.on_conflict.is_some(), "Insert::or");
        self.conflict(insert.on_conflict.as_ref());
        self.flag(insert.alias.is_some(), "Insert::alias");
        self.flag(!insert.columns.is_empty(), "Insert::columns");
        self.form(insert_source_name(&insert.source));
        if let InsertSource::Select(select) = &insert.source {
            self.select(*select);
        }
        self.flag(insert.upserts.len() > 1, "Upsert::several");
        for upsert in &insert.upserts {
            self.upsert(upsert);
        }
        self.flag(!insert.returning.is_empty(), "Insert::returning");
        self.result_columns(&insert.returning);
    }

    /// Walks one `ON CONFLICT` clause.
    ///
    /// @param upsert - the clause
    fn upsert(&mut self, upsert: &Upsert) {
        self.flag(!upsert.target.is_empty(), "Upsert::target");
        for column in &upsert.target {
            self.indexed_column(column);
        }
        self.flag(upsert.target_filter.is_some(), "Upsert::target_filter");
        self.maybe_expr(upsert.target_filter);
        self.form(if upsert.do_update {
            "Upsert::do_update"
        } else {
            "Upsert::do_nothing"
        });
        for (_, value) in &upsert.assignments {
            self.expr(*value);
        }
        self.flag(upsert.filter.is_some(), "Upsert::filter");
        self.maybe_expr(upsert.filter);
    }

    /// Walks an `UPDATE`.
    ///
    /// @param update - the statement
    fn update(&mut self, update: &inillucent_sql::ast::Update) {
        self.flag(!update.with.ctes.is_empty(), "Update::with");
        self.with(&update.with);
        self.flag(update.on_conflict.is_some(), "Update::or");
        self.conflict(update.on_conflict.as_ref());
        self.from_term(update.target);
        for (names, value) in &update.assignments {
            self.flag(names.len() > 1, "Update::row_value_assignment");
            self.expr(*value);
        }
        self.flag(!update.from.is_empty(), "Update::from");
        for term in &update.from {
            self.from_term(*term);
        }
        self.maybe_expr(update.filter);
        self.flag(!update.returning.is_empty(), "Update::returning");
        self.result_columns(&update.returning);
        self.limited(&update.limited_at, "Update::limited");
        self.order_terms(&update.order_by);
        self.maybe_expr(update.limit);
        self.maybe_expr(update.offset);
    }

    /// Walks a `DELETE`.
    ///
    /// @param delete - the statement
    fn delete(&mut self, delete: &inillucent_sql::ast::Delete) {
        self.flag(!delete.with.ctes.is_empty(), "Delete::with");
        self.with(&delete.with);
        self.from_term(delete.target);
        self.maybe_expr(delete.filter);
        self.flag(!delete.returning.is_empty(), "Delete::returning");
        self.result_columns(&delete.returning);
        self.limited(&delete.limited_at, "Delete::limited");
        self.order_terms(&delete.order_by);
        self.maybe_expr(delete.limit);
        self.maybe_expr(delete.offset);
    }

    /// Records the `ORDER BY` or `LIMIT` of a limited write.
    ///
    /// @param limited - where the parser found the clause
    /// @param form - the clause's report name
    fn limited(&mut self, limited: &Option<(Limited, inillucent_sql::Span)>, form: &'static str) {
        if let Some((which, _)) = limited {
            self.form(form);
            self.form(limited_name(which));
        }
    }
}

impl Walk<'_> {
    /// Walks one expression and everything under it.
    ///
    /// @param id - the expression
    fn expr(&mut self, id: ExprId) {
        let Some(expr) = self.ast.expr(id).cloned() else {
            return;
        };
        self.form(expr_name(&expr));
        match &expr {
            Expr::Literal(literal) => self.literal(literal),
            Expr::Parameter { name, .. } => self.flag(name.is_some(), "Parameter::named"),
            Expr::Column { database, .. } => self.flag(database.is_some(), "Column::schema"),
            Expr::Star { .. } => {}
            Expr::Unary { op, operand } => {
                self.form(unary_name(op));
                self.expr(*operand);
            }
            Expr::Binary { op, left, right } => {
                self.form(binary_name(op));
                self.binary_function(op);
                self.expr(*left);
                self.expr(*right);
            }
            Expr::Collate { operand, collation } => {
                let name = self.text(*collation);
                self.register(Register::Collation, &name);
                self.expr(*operand);
            }
            Expr::Cast { operand, .. } => self.expr(*operand),
            Expr::Pattern { .. } => self.pattern(&expr),
            Expr::Between {
                negated,
                operand,
                low,
                high,
            } => {
                self.flag(*negated, "Between::negated");
                for part in [operand, low, high] {
                    self.expr(*part);
                }
            }
            Expr::In {
                negated,
                operand,
                rhs,
            } => {
                self.flag(*negated, "In::negated");
                self.expr(*operand);
                self.in_rhs(rhs);
            }
            _ => self.other_expr(&expr),
        }
    }

    /// Walks the expressions [`Walk::expr`] leaves to keep its length down.
    ///
    /// @param expr - the expression
    fn other_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::IsNull { operand, .. } => self.expr(*operand),
            Expr::Is {
                distinct_from,
                left,
                right,
                ..
            } => {
                self.flag(*distinct_from, "Is::distinct_from");
                self.expr(*left);
                self.expr(*right);
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                self.flag(operand.is_some(), "Case::operand");
                self.flag(otherwise.is_some(), "Case::otherwise");
                self.maybe_expr(*operand);
                for (when, then) in branches {
                    self.expr(*when);
                    self.expr(*then);
                }
                self.maybe_expr(*otherwise);
            }
            Expr::Function { .. } => self.function(expr),
            Expr::Exists { negated, select } => {
                self.flag(*negated, "Exists::negated");
                self.select(*select);
            }
            Expr::Subquery(select) => self.select(*select),
            Expr::RowValue(items) => {
                for item in items {
                    self.expr(*item);
                }
            }
            Expr::Raise { action, message } => {
                self.form(raise_name(action));
                self.maybe_expr(*message);
            }
            _ => {}
        }
    }

    /// Records a literal, and the function a date literal stands for.
    ///
    /// @param literal - the literal
    fn literal(&mut self, literal: &Literal) {
        self.form(literal_name(literal));
        let function = match literal {
            Literal::CurrentDate => "current_date",
            Literal::CurrentTime => "current_time",
            Literal::CurrentTimestamp => "current_timestamp",
            _ => return,
        };
        self.register(Register::Function, function);
    }

    /// Records the function an operator is registered as, when it is one.
    ///
    /// @param op - the operator
    fn binary_function(&mut self, op: &BinaryOp) {
        let function = match op {
            BinaryOp::Extract => "->",
            BinaryOp::ExtractText => "->>",
            BinaryOp::Match => "match",
            BinaryOp::Regexp => "regexp",
            _ => return,
        };
        self.register(Register::Function, function);
    }

    /// Walks a `LIKE`, `GLOB`, `REGEXP` or `MATCH`.
    ///
    /// @param expr - the expression, which is an `Expr::Pattern`
    fn pattern(&mut self, expr: &Expr) {
        if let Expr::Pattern {
            negated,
            op,
            operand,
            pattern,
            escape,
        } = expr
        {
            self.form(pattern_name(op));
            let function = match op {
                PatternOp::Like => "like",
                PatternOp::Glob => "glob",
                PatternOp::Regexp => "regexp",
                PatternOp::Match => "match",
            };
            self.register(Register::Function, function);
            self.flag(*negated, "Pattern::negated");
            self.flag(escape.is_some(), "Pattern::escape");
            self.expr(*operand);
            self.expr(*pattern);
            self.maybe_expr(*escape);
        }
    }

    /// Walks the right side of an `IN`.
    ///
    /// @param rhs - the right side
    fn in_rhs(&mut self, rhs: &InRhs) {
        self.form(in_rhs_name(rhs));
        match rhs {
            InRhs::List(items) => {
                for item in items {
                    self.expr(*item);
                }
            }
            InRhs::Select(select) => self.select(*select),
            InRhs::Table {
                table, arguments, ..
            } => match arguments {
                Some(arguments) => self.table_function(*table, arguments),
                None => self.table_name(*table),
            },
        }
    }

    /// Walks a function call.
    ///
    /// @param expr - the expression, which is an `Expr::Function`
    fn function(&mut self, expr: &Expr) {
        if let Expr::Function {
            name,
            distinct,
            arguments,
            order_by,
            filter,
            over,
        } = expr
        {
            let name = self.text(*name);
            self.register(Register::Function, &name);
            self.flag(*distinct, "Function::distinct");
            self.flag(arguments.is_none(), "Function::star");
            for argument in arguments.iter().flatten() {
                self.expr(*argument);
            }
            self.flag(!order_by.is_empty(), "Function::order_by");
            self.order_terms(order_by);
            self.flag(filter.is_some(), "Function::filter");
            self.maybe_expr(*filter);
            self.flag(over.is_some(), "Function::over");
            if let Some(window) = over {
                self.window(*window);
            }
        }
    }
}

/// Parses every statement of one SQL text and walks each.
///
/// A statement that does not parse ends the walk of that text: the rest of the
/// text is not reachable, and a syntax case that expects the refusal is
/// counted by [`syntax_gaps`] rather than here.
///
/// @param sql - the text of one record
/// @param case - the case id, for the report
/// @param seen - what has been reached so far
pub fn walk_sql(sql: &str, case: &str, seen: &mut Seen) {
    let limits = Limits::default();
    let bytes = sql.as_bytes();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let Ok(parsed) = inillucent_sql::parse_next_statement(bytes, offset, &limits) else {
            return;
        };
        if parsed.consumed == 0 {
            return;
        }
        let mut walk = Walk {
            ast: &parsed.ast,
            case,
            seen,
        };
        walk.statement(&parsed.statement);
        offset += parsed.consumed;
    }
}

/// Walks every record of one case, its setup included.
///
/// @param case - the case
/// @param seen - what has been reached so far
pub fn walk_case(case: &Case, seen: &mut Seen) {
    for record in case.setup.iter().chain(case.records.iter()) {
        if let Some(sql) = record.sql() {
            walk_sql(sql, &case.id, seen);
        }
    }
}

/// Reads every case the change cadence runs, across every family.
pub fn change_cases() -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    for family in FAMILIES {
        let work = group::work(family, Cadence::Change)?;
        cases.extend(work.runs.into_iter().map(|(case, _)| case));
    }
    Ok(cases)
}

/// The names a live database lists in one `PRAGMA ..._list`.
///
/// @param connection - a session on a fresh database
/// @param pragma - `function_list`, `pragma_list`, `module_list` or `collation_list`
/// @param column - the index of the name column
fn live_names(
    connection: &inillucent_engine::connect::Connection<'_>,
    pragma: &str,
    column: usize,
) -> Result<BTreeSet<String>, String> {
    let observed = crate::differential::observe(connection, &format!("PRAGMA {pragma}"), true);
    if !observed.ok {
        return Err(format!("PRAGMA {pragma} failed: {}", observed.message));
    }
    Ok(observed
        .rows
        .iter()
        .filter_map(|row| match row.get(column) {
            Some(crate::oracle::TaggedValue::Text(text)) => {
                Some(String::from_utf8_lossy(text).to_ascii_lowercase())
            }
            _ => None,
        })
        .collect())
}

/// The four live registers.
#[derive(Clone, Debug, Default)]
pub struct Registers {
    /// `PRAGMA function_list`.
    pub functions: BTreeSet<String>,
    /// `PRAGMA pragma_list`.
    pub pragmas: BTreeSet<String>,
    /// `PRAGMA module_list`.
    pub modules: BTreeSet<String>,
    /// `PRAGMA collation_list`.
    pub collations: BTreeSet<String>,
}

/// Opens a fresh database and reads its four registers.
///
/// @param directory - where the throwaway database goes
pub fn read_registers(directory: &std::path::Path) -> Result<Registers, String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let path = directory.join("registers.rdb");
    inillucent_base::testing::remove_database(&path);
    let database =
        inillucent_engine::connect::Database::open(&path).map_err(|error| error.to_string())?;
    let connection = database.session();
    Ok(Registers {
        functions: live_names(&connection, "function_list", 0)?,
        pragmas: live_names(&connection, "pragma_list", 0)?,
        modules: live_names(&connection, "module_list", 0)?,
        collations: live_names(&connection, "collation_list", 1)?,
    })
}

/// Every syntax register example that no case runs the way the register says.
///
/// A positive example must be the last statement of the case
/// `convert-syntax` wrote for it; a negative one must be expected to fail.
///
/// A negative example whose case expects success is accepted when the case is
/// in `known.list`: the register records what inillucent refuses, and a line
/// there says SQLite accepts it and the refusal is a defect.
///
/// @param register - the syntax register
/// @param cases - every case, by id
/// @param known - the case ids `known.list` names
pub fn syntax_gaps(
    register: &crate::syntax::SyntaxRegister,
    cases: &BTreeMap<&str, &Case>,
    known: &BTreeSet<String>,
) -> Vec<String> {
    let mut gaps = Vec::new();
    for production in &register.productions {
        let examples = production
            .positive
            .iter()
            .map(|sql| (sql, true))
            .chain(production.negative.iter().map(|sql| (sql, false)));
        for (number, (example, positive)) in examples.enumerate() {
            if example.trim().is_empty() {
                continue;
            }
            let id = format!(
                "syntax-{}-{}{}",
                production.name,
                if positive { "p" } else { "n" },
                number
            );
            let recorded = known.contains(&id);
            if let Some(problem) = syntax_gap(cases.get(id.as_str()), example, positive || recorded)
            {
                gaps.push(format!("{id}: {problem}"));
            }
        }
    }
    gaps
}

/// Why one example's case does not cover it, or `None` when it does.
///
/// @param case - the case `convert-syntax` wrote for it, when it exists
/// @param example - the example's text
/// @param may_succeed - whether the case may expect success: a positive example, or a negative one `known.list` records
fn syntax_gap(case: Option<&&Case>, example: &str, may_succeed: bool) -> Option<String> {
    let Some(case) = case else {
        return Some("no case has this id".to_string());
    };
    let wanted = crate::statement_matrix::convert::scratch_paths(example);
    let record = case
        .records
        .iter()
        .rev()
        .find(|record| record.sql() == Some(wanted.as_str()));
    let Some(record) = record else {
        return Some("the case does not run the example".to_string());
    };
    let expects_error = matches!(
        record,
        Record::Statement {
            expect: Expect::Error(_),
            ..
        }
    );
    if !may_succeed && !expects_error {
        return Some("a negative example whose case does not expect a refusal".to_string());
    }
    None
}

/// The capability rows no case exercises.
///
/// A case exercises a row when it names the row in a `capability` directive,
/// or when `corpora/matrix/capabilities.toml` lists the case under the row.
/// A listed id that names no case is itself a gap, so the file cannot drift.
///
/// @param cases - every case, by id
/// @param extra - further ids that exist outside the corpus, such as surface cases
pub fn capability_gaps(
    cases: &BTreeMap<&str, &Case>,
    extra: &[&str],
) -> Result<Vec<String>, String> {
    let listed = read_capability_map()?;
    let mut gaps = Vec::new();
    for capability in inillucent_driver::capability::CAPABILITIES {
        let row = capability.name;
        let by_directive = cases
            .values()
            .any(|case| case.capabilities.iter().any(|name| name == row));
        let ids = listed.get(row).cloned().unwrap_or_default();
        for id in &ids {
            if !cases.contains_key(id.as_str()) && !extra.contains(&id.as_str()) {
                gaps.push(format!(
                    "{row}: capabilities.toml names `{id}`, which is no case"
                ));
            }
        }
        if !by_directive && ids.is_empty() {
            gaps.push(format!("{row}: no case exercises it"));
        }
    }
    for row in listed.keys() {
        if inillucent_driver::capability::supports(row).is_none() {
            gaps.push(format!(
                "{row}: capabilities.toml names a row CAPABILITIES does not have"
            ));
        }
    }
    Ok(gaps)
}

/// Reads `corpora/matrix/capabilities.toml`: one `row = ["case id", ...]`
/// line per capability row.
fn read_capability_map() -> Result<BTreeMap<String, Vec<String>>, String> {
    let path = crate::statement_matrix::known::corpus_root().join("capabilities.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut map = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((row, ids)) = line.split_once('=') else {
            return Err(format!("capabilities.toml line {}: no `=`", number + 1));
        };
        let ids = ids.trim().trim_start_matches('[').trim_end_matches(']');
        let ids: Vec<String> = ids
            .split(',')
            .map(|id| id.trim().trim_matches('"').to_string())
            .filter(|id| !id.is_empty())
            .collect();
        map.insert(row.trim().to_string(), ids);
    }
    Ok(map)
}

/// The inventory report.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The whole report, as Markdown.
    pub markdown: String,
    /// One line.
    pub summary: String,
    /// Everything that has no case.
    pub missing: Vec<String>,
}

/// Builds the report over every case the change cadence runs.
///
/// @param scratch - a directory for the throwaway database the registers are read from
/// @param surfaces - the ids of the surface cases, which live in code rather than in a corpus file
pub fn report(scratch: &std::path::Path, surfaces: &[&str]) -> Result<Report, String> {
    let cases = change_cases()?;
    let mut seen = Seen::default();
    for case in &cases {
        walk_case(case, &mut seen);
    }
    let by_id: BTreeMap<&str, &Case> = cases.iter().map(|case| (case.id.as_str(), case)).collect();
    let registers = read_registers(scratch)?;
    let register =
        crate::syntax::SyntaxRegister::load(&crate::workspace_root().join("compat/syntax.toml"))?;
    let mut markdown = String::new();
    let mut missing = Vec::new();
    markdown.push_str("# Statement matrix inventory\n\n");
    markdown.push_str(&format!(
        "Written by `tooling::matrix_inventory` over the {} cases the change cadence runs. \
         Each name is followed by the first case that reaches it.\n",
        cases.len()
    ));
    forms_section(&seen, &mut markdown, &mut missing);
    let sections: [(&str, &BTreeSet<String>, &BTreeMap<String, String>); 4] = [
        ("Functions", &registers.functions, &seen.functions),
        ("Pragmas", &registers.pragmas, &seen.pragmas),
        ("Modules", &registers.modules, &seen.modules),
        ("Collations", &registers.collations, &seen.collations),
    ];
    for (title, live, reached) in sections {
        names_section(title, live, reached, &mut markdown, &mut missing);
    }
    let known: BTreeSet<String> = crate::statement_matrix::known::read_known(
        &crate::statement_matrix::known::corpus_root().join("known.list"),
    )?
    .into_keys()
    .collect();
    let syntax = syntax_gaps(&register, &by_id, &known);
    let capabilities = capability_gaps(&by_id, surfaces)?;
    gaps_section("Syntax register", &syntax, &mut markdown, &mut missing);
    gaps_section(
        "Capability rows",
        &capabilities,
        &mut markdown,
        &mut missing,
    );
    let summary = format!(
        "{} cases; {} forms, {} functions, {} pragmas, {} modules, {} collations checked; {} missing",
        cases.len(),
        EVERY_VARIANT.iter().map(|list| list.len()).sum::<usize>() + CLAUSES.len(),
        registers.functions.len(),
        registers.pragmas.len(),
        registers.modules.len(),
        registers.collations.len(),
        missing.len()
    );
    Ok(Report {
        markdown,
        summary,
        missing,
    })
}

/// Writes the AST variants and clauses, and collects the unreached ones.
///
/// A form the walker recorded that is in neither list is also reported, so a
/// clause name typed into the walker and not into [`CLAUSES`] cannot pass
/// unnoticed.
///
/// @param seen - what the cases reached
/// @param markdown - the report
/// @param missing - the gaps
fn forms_section(seen: &Seen, markdown: &mut String, missing: &mut Vec<String>) {
    markdown.push_str(
        "
## AST variants and clauses

| Form | First case |
|---|---|
",
    );
    let every: Vec<&str> = EVERY_VARIANT
        .iter()
        .flat_map(|list| list.iter().copied())
        .chain(CLAUSES.iter().copied())
        .collect();
    for form in &every {
        let unreachable = UNREACHABLE.iter().find(|(name, _)| name == form);
        match (seen.forms.get(form), unreachable) {
            (Some(case), None) => markdown.push_str(&format!(
                "| `{form}` | `{case}` |
"
            )),
            (None, Some((_, reason))) => markdown.push_str(&format!(
                "| `{form}` | never built: {reason} |
"
            )),
            (Some(case), Some(_)) => {
                markdown.push_str(&format!(
                    "| `{form}` | `{case}`, and listed as never built |
"
                ));
                missing.push(format!(
                    "form {form} is listed as never built and `{case}` reaches it"
                ));
            }
            (None, None) => {
                markdown.push_str(&format!(
                    "| `{form}` | **missing** |
"
                ));
                missing.push(format!("form {form}"));
            }
        }
    }
    for form in seen
        .forms
        .keys()
        .chain(UNREACHABLE.iter().map(|(name, _)| name))
    {
        if !every.contains(form) {
            missing.push(format!(
                "form {form} is named by the walker and listed nowhere"
            ));
        }
    }
}

/// Writes one live register and collects the names no case uses.
///
/// @param title - the section heading
/// @param live - what the engine lists
/// @param reached - what the cases use
/// @param markdown - the report
/// @param missing - the gaps
fn names_section(
    title: &str,
    live: &BTreeSet<String>,
    reached: &BTreeMap<String, String>,
    markdown: &mut String,
    missing: &mut Vec<String>,
) {
    markdown.push_str(&format!(
        "\n## {title}\n\n| Name | First case |\n|---|---|\n"
    ));
    for name in live {
        match reached.get(name) {
            Some(case) => markdown.push_str(&format!("| `{name}` | `{case}` |\n")),
            None => {
                markdown.push_str(&format!("| `{name}` | **missing** |\n"));
                missing.push(format!("{} {name}", title.to_ascii_lowercase()));
            }
        }
    }
}

/// Writes a list of gaps under a heading.
///
/// @param title - the section heading
/// @param gaps - the gaps
/// @param markdown - the report
/// @param missing - every gap in the report
fn gaps_section(title: &str, gaps: &[String], markdown: &mut String, missing: &mut Vec<String>) {
    markdown.push_str(&format!("\n## {title}\n\n"));
    if gaps.is_empty() {
        markdown.push_str("Every entry has a case.\n");
    }
    for gap in gaps {
        markdown.push_str(&format!("- {gap}\n"));
        missing.push(format!("{} {gap}", title.to_ascii_lowercase()));
    }
}
