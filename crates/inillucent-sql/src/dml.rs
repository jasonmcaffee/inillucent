//! Binding INSERT, UPDATE and DELETE.
//!
//! Invariant: a bound DML statement names every value it will write, in table
//! column order, before anything is compiled. A column the statement did not
//! mention is not left to be filled in later by whoever runs it - it carries
//! its `DEFAULT`, or a NULL, as an expression like any other. That is what
//! makes `INSERT INTO t(b) VALUES(1)` and `INSERT INTO t VALUES(NULL, 1)`
//! compile to the same shape, and it is why the constraint checks can be
//! written once against a row image rather than twice against two.
//!
//! Constraints are bound here too, out of the `CREATE TABLE` text the file
//! stores. The catalog keeps them as source, because the catalog sits below
//! the binder and cannot bind anything; the binder parses that source against
//! the table it belongs to and gets an ordinary expression back. A CHECK is
//! therefore evaluated by exactly the machinery that evaluates a WHERE clause,
//! which is the only way to be sure the two agree about what `x > 0` means
//! when `x` is text.

use inillucent_base::limits::Limits;
use inillucent_value::Collation;

use crate::ast::{self, ConflictAction};
use crate::bind::{
    no_such_column, refused, unsupported, Binder, BoundExpr, BoundOrderTerm, BoundResultColumn,
    BoundSelect, BoundSource,
};
use crate::catalog_view::{IndexInfo, TableInfo, TableKind, TriggerEventInfo, TriggerInfo};
use crate::diagnostic::ParseError;
use crate::lexer::Span;
use crate::parser::parse_expression;

/// The internal tables an application may write, as SQLite allows.
///
/// **Four, and two of them are the schema.** Every table whose
/// name begins with `sqlite_` used to be refused, which is wrong for all four,
/// because writing them is the documented way to use them:
///
/// - `sqlite_schema`, and `sqlite_master` which is its other name, are what
///   `PRAGMA writable_schema` is for, and `.dump` emits
///   `INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES(...)`
///   for a virtual table - which is the only way a dump can restore one
///   without building empty shadow tables over the ones it is about to fill
///   (task-1979, R2). Whether the pragma is on is the *engine's* question and
///   not the binder's: `ImportedDatabase::refuse_schema_write` refuses the
///   statement when it is off, the way `refuse_shadow_write` refuses a write a
///   defensive connection may not make.
///
/// - `sqlite_sequence` holds one row per `AUTOINCREMENT` table, and
///   `UPDATE sqlite_sequence SET seq = 0 WHERE name = 't'` is how the counter is
///   reset. `DELETE FROM sqlite_sequence` is how it is reset for every table at
///   once. Refusing them left no way at all to do either.
/// - `sqlite_stat1` is what `ANALYZE` writes, and `.dump` emits
///   `INSERT INTO sqlite_stat1 VALUES(...)` for it - so a dump this engine
///   produced could not be replayed into it.
///
/// They are ordinary tables in every other respect: the rows are what they are,
/// and a value written into one is used exactly as `ANALYZE` or the rowid
/// allocator would have used the one it replaced.
const WRITABLE_INTERNAL: [&[u8]; 4] = [
    b"sqlite_sequence",
    b"sqlite_stat1",
    b"sqlite_schema",
    b"sqlite_master",
];

/// Where one column's value comes from in an INSERT.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnSource {
    /// The value at this position of the source row.
    Row(usize),
    /// An expression evaluated once per row, which is what a `DEFAULT` is.
    Expr(BoundExpr),
    /// A generated column, computed from the rest of the row rather than from
    /// anything the statement supplied.
    ///
    /// It is its own variant because it is evaluated at a different *time*: a
    /// `DEFAULT` is a value like any other, while a generated column reads the
    /// row it is part of and so cannot be computed until the rest of it is.
    Generated(BoundExpr),
}

/// What an INSERT inserts.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundInsertSource {
    /// Literal rows, each already bound.
    Values(Vec<Vec<BoundExpr>>),
    /// A query, whose result columns feed the target columns in order.
    Select(Box<BoundSelect>),
}

/// One `CHECK` constraint, bound against its table.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundCheck {
    /// The constraint's name, when it was written with one.
    pub name: Option<Vec<u8>>,
    /// The predicate.
    pub expr: BoundExpr,
}

/// A `NOT NULL` column's `DEFAULT`, bound so a `REPLACE` can stand it in.
///
/// **REPLACE's rule for a `NOT NULL` violation is to substitute the column's
/// default, and to fall back to `ABORT` only when there is no default.** So
/// `UPDATE OR REPLACE t SET c = NULL` on `c TEXT NOT NULL DEFAULT 'd'` stores
/// `'d'`, and this engine used to refuse the statement instead.
///
/// The write path cannot bind one for itself: a default is schema text, and by
/// the time a row is being checked the parser is long out of scope. The binder
/// already binds one for every column a statement *omits*; these are the same
/// expressions bound for the columns it supplies, which is where a NULL that
/// needs replacing can come from.
///
/// Only the columns that can need it are here - `NOT NULL` and with a default -
/// so an ordinary table carries an empty vector and the write path skips the
/// whole apparatus.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundDefault {
    /// The column the default belongs to.
    pub column: u16,
    /// The default expression, bound.
    pub expr: BoundExpr,
}

/// The expressions one index needs evaluated per row to be maintained.
///
/// **An index is usually just columns of the row, and then it needs none of
/// this.** A partial index holds only the rows its predicate accepts, and an
/// index on an expression holds a value no column carries - so for those two,
/// maintaining the index means evaluating something per row rather than
/// copying a slot. They travel on the bound statement for the same reason the
/// table's `CHECK` predicates do: the binder is what can turn schema text into
/// a `BoundExpr`, and the write path is what runs it.
///
/// The list holds only the indexes that need it, so a table with neither kind
/// leaves it empty and the write path's loop runs zero times - which is every
/// table the gate measures.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundIndexExprs {
    /// The index's position in the table's `indexes`.
    pub position: usize,
    /// The partial-index predicate, when it has one.
    pub predicate: Option<BoundExpr>,
    /// One per key column: the expression it indexes, or `None` for a column.
    pub keys: Vec<Option<BoundExpr>>,
}

/// One statement of a trigger body, bound.
///
/// The four the grammar allows and no more. A trigger body is not a general
/// statement list: it cannot create objects, cannot open transactions, and
/// cannot return rows to the caller, so a variant for anything else would be a
/// shape the binder is required to refuse.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundTriggerStatement {
    /// `INSERT`.
    Insert(Box<BoundInsert>),
    /// `UPDATE`.
    Update(Box<BoundUpdate>),
    /// `DELETE`.
    Delete(Box<BoundDelete>),
    /// `SELECT`, which a body runs for its side effects - in practice for the
    /// `RAISE()` inside it.
    Select(Box<BoundSelect>),
}

/// A trigger, bound against the write that fires it.
///
/// It is bound per statement rather than once per schema because the body's
/// FROM terms take statement-wide source numbers, and those only exist relative
/// to the statement they are inlined into.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundTrigger {
    /// The trigger's name, for the diagnostic when its body fails.
    pub name: Vec<u8>,
    /// The folded name of the table it is attached to.
    ///
    /// Read by the executor to decide whether a body statement is writing the
    /// trigger's *own* table, which is what `PRAGMA recursive_triggers` is
    /// about: with it on, such a write fires this trigger again.
    pub table: Vec<u8>,
    /// Whether it fires before or after the row is written.
    pub time: ast::TriggerTime,
    /// The `WHEN` guard, when one was written.
    pub when: Option<BoundExpr>,
    /// The body statements, in written order.
    pub body: Vec<BoundTriggerStatement>,
    /// Whether the binder synthesised this from a `REFERENCES` clause rather
    /// than reading it from a `CREATE TRIGGER`.
    ///
    /// **Read by `DROP TABLE` (task-1979, F6).** Dropping a table with foreign
    /// keys on runs an implicit `DELETE FROM` first, so the keys that reference
    /// it are enforced - and SQLite's rule is that the implicit delete fires no
    /// triggers of its own while still performing every foreign key action. A
    /// delete bound for that purpose keeps the triggers this flag marks and
    /// drops the rest.
    pub foreign_key: bool,
    /// Whether the foreign key this enforces has one table as both its child
    /// and its parent.
    ///
    /// **Also read by `DROP TABLE` (task-1979, F6).** The implicit delete keeps
    /// the foreign key triggers and drops this one, because emptying a table
    /// cannot leave a row of that same table pointing at nothing - see
    /// `ForeignKeyTrigger::self_referencing`, which is where the value comes
    /// from. Always false on a trigger the schema wrote.
    pub self_referencing: bool,
}

/// A bound `INSERT`.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundInsert {
    /// The table being written.
    pub table: TableInfo,
    /// The statement-wide number of the FROM term being written.
    ///
    /// It used to be implicitly zero, because a DML statement had exactly one
    /// source. A trigger body is compiled into the statement that fires it, so
    /// its target takes the next number after the firing statement's - and a
    /// compiler that assumed zero read the wrong cursor for every fire after
    /// the first.
    pub target_source: usize,
    /// Where each table column's value comes from, in column order.
    pub columns: Vec<ColumnSource>,
    /// Where the rowid comes from, when the statement supplies one.
    pub rowid: Option<ColumnSource>,
    /// Which value of the supplied row is the rowid, when the statement named
    /// it outright.
    ///
    /// `INSERT INTO t(rowid, a) VALUES (7, 'x')` is legal on any rowid table,
    /// including one with no `INTEGER PRIMARY KEY` to alias it and including a
    /// virtual table. It is recorded separately from `rowid` because it is not
    /// a column: nothing writes it into the record.
    pub named_rowid: Option<usize>,
    /// The rows.
    pub source: BoundInsertSource,
    /// How many values each source row supplies.
    pub arity: usize,
    /// The statement's conflict algorithm, when it wrote one.
    pub on_conflict: Option<ConflictAction>,
    /// The table's `CHECK` constraints.
    pub checks: Vec<BoundCheck>,
    /// The `DEFAULT`s a `REPLACE` may stand in for a NULL, by column.
    pub not_null_defaults: Vec<BoundDefault>,
    /// The expressions the table's partial and expression indexes need.
    pub index_exprs: Vec<BoundIndexExprs>,
    /// The `ON CONFLICT ... DO UPDATE` clause, when there is one.
    pub upsert: Vec<BoundUpsert>,
    /// `sqlite_sequence`'s root page, when the target is `AUTOINCREMENT`.
    ///
    /// Resolved here rather than in the compiler because it is a fact about the
    /// catalog, and the catalog is what the binder holds. It is zero for every
    /// other table, which is also what it reads as before the first
    /// `AUTOINCREMENT` table in a database is created.
    pub sequence_root: u32,
    /// The `RETURNING` columns.
    pub returning: Vec<BoundResultColumn>,
    /// The triggers this write fires, in schema order.
    pub triggers: Vec<BoundTrigger>,
    /// The foreign-key actions a `REPLACE` fires for the row it removes.
    ///
    /// A `REPLACE` that deletes a row to make room for another is a delete,
    /// and the keys pointing at that row have to be told. Written `DELETE`
    /// triggers are *not* fired - that is SQLite's rule with its default
    /// `recursive_triggers = off` - so these are only the ones a key implies.
    pub replace_triggers: Vec<BoundTrigger>,
}

/// A bound `ON CONFLICT ... DO UPDATE` clause.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundUpsert {
    /// The conflict target columns, when written; empty means any constraint.
    ///
    /// Sorted, because a conflict target names a *set* of columns and
    /// `ON CONFLICT(a,b)` and `ON CONFLICT(b,a)` name the same one. Matching
    /// them against an index's columns is a set comparison, and sorting here
    /// is what makes it one comparison rather than a search per column.
    pub target: Vec<u16>,
    /// The assignments, or empty for `DO NOTHING`.
    pub assignments: Vec<BoundAssignment>,
    /// Whether the action is `DO UPDATE`.
    pub do_update: bool,
    /// The `WHERE` on the `DO UPDATE`.
    pub filter: Option<BoundExpr>,
}

/// One `SET` assignment.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundAssignment {
    /// The column being assigned, as a declared position.
    pub column: u16,
    /// Whether the assignment names the row's own rowid rather than a declared
    /// column, in which case `column` says nothing.
    ///
    /// **`UPDATE t SET rowid = 100` was `no such column: rowid` (task-1979,
    /// F9).** An assignment target was looked up with `column_position`, which
    /// only knows the columns the table declares, and a table with no INTEGER
    /// PRIMARY KEY declares none for its rowid. SQLite accepts all three
    /// spellings of the rowid on either kind of table and moves the row to the
    /// new key.
    pub rowid: bool,
    /// The new value.
    pub value: BoundExpr,
}

/// A bound `UPDATE`.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundUpdate {
    /// The table being written.
    pub table: TableInfo,
    /// The statement-wide number of the FROM term being written.
    ///
    /// It used to be implicitly zero, because a DML statement had exactly one
    /// source. A trigger body is compiled into the statement that fires it, so
    /// its target takes the next number after the firing statement's - and a
    /// compiler that assumed zero read the wrong cursor for every fire after
    /// the first.
    pub source: usize,
    /// The extra FROM terms of an `UPDATE ... FROM`, in written order.
    ///
    /// **The rows being updated come from a join.** `UPDATE t SET v = s.v FROM s
    /// WHERE s.a = t.a` is the shape a migration writes to copy a column across
    /// tables, and the values it assigns are not expressions over the target
    /// row: they read a *different* row, one the join found. So the query that
    /// finds the keys carries these terms too, and projects the assigned values
    /// beside the key; see [`BoundUpdate::from`], which is this field.
    ///
    /// Empty for every ordinary `UPDATE`, which is what keeps the wider row off
    /// the path the gate's `txn.large` measures.
    pub from: Vec<crate::bind::BoundSource>,
    /// The assignments, in table column order with duplicates already refused.
    pub assignments: Vec<BoundAssignment>,
    /// The `STORED` generated columns, recomputed after the assignments.
    ///
    /// **A stored generated column is part of the row, so a row that is
    /// rewritten rewrites it (task-1913).** It is never named in a `SET`, so
    /// an `UPDATE` used to leave whatever was written when the row was
    /// inserted: `c GENERATED ALWAYS AS (a + 1) STORED` still read 2 after
    /// `UPDATE g SET a = 5`, where SQLite reads 6. The wrong value is on the
    /// disk rather than in an answer, so a later read of the same file is
    /// wrong too, and an index on the column indexes the stale value.
    ///
    /// A `VIRTUAL` column is not here: it has no slot in the record and is
    /// computed when it is read, which is why only this half needed fixing.
    ///
    /// These are evaluated against the row *after* the assignments, which is
    /// the one difference from [`BoundUpdate::assignments`] - those read the
    /// before image so `SET a = b, b = a` swaps.
    pub generated: Vec<BoundAssignment>,
    /// The `WHERE` clause.
    pub filter: Option<BoundExpr>,
    /// The statement's conflict algorithm, when it wrote one.
    pub on_conflict: Option<ConflictAction>,
    /// The table's `CHECK` constraints.
    pub checks: Vec<BoundCheck>,
    /// The `DEFAULT`s a `REPLACE` may stand in for a NULL, by column.
    pub not_null_defaults: Vec<BoundDefault>,
    /// The expressions the table's partial and expression indexes need.
    pub index_exprs: Vec<BoundIndexExprs>,
    /// `INDEXED BY` or `NOT INDEXED` on the target, which the query that finds
    /// the rows to change obeys; `inillucent_exec::dml::hint_target` puts it there.
    pub index_hint: crate::bind::IndexChoice,
    /// The `RETURNING` columns.
    pub returning: Vec<BoundResultColumn>,
    /// The `ORDER BY` that decides which rows a `LIMIT` keeps.
    ///
    /// Empty unless the statement wrote one, and then always with a `LIMIT`,
    /// because the binder refuses an order with nothing to limit. It goes onto
    /// the query that finds the rows to change, which is where SQLite puts it
    /// too: a limited write is `WHERE rowid IN (SELECT rowid ... ORDER BY ...
    /// LIMIT ...)` there.
    pub order_by: Vec<BoundOrderTerm>,
    /// The `LIMIT`.
    pub limit: Option<BoundExpr>,
    /// The `OFFSET`.
    pub offset: Option<BoundExpr>,
    /// The triggers this write fires, in schema order.
    pub triggers: Vec<BoundTrigger>,
    /// The rows to fire an `INSTEAD OF` trigger for, when the target is a view.
    ///
    /// A view has no rows of its own, so `OLD` has to come from running the
    /// view. This is that query, with the statement's `WHERE` on it and one
    /// result column per view column.
    pub view_rows: Option<Box<BoundSelect>>,
}

/// A bound `DELETE`.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundDelete {
    /// The table being written.
    pub table: TableInfo,
    /// The expressions the table's partial and expression indexes need.
    ///
    /// A delete needs them too: an entry only comes out of a partial index if
    /// the row was in it, and a key the index computed has to be recomputed to
    /// be found.
    pub index_exprs: Vec<BoundIndexExprs>,
    /// `INDEXED BY` or `NOT INDEXED` on the target, as on [`BoundUpdate`].
    pub index_hint: crate::bind::IndexChoice,
    /// The statement-wide number of the FROM term being written.
    ///
    /// It used to be implicitly zero, because a DML statement had exactly one
    /// source. A trigger body is compiled into the statement that fires it, so
    /// its target takes the next number after the firing statement's - and a
    /// compiler that assumed zero read the wrong cursor for every fire after
    /// the first.
    pub source: usize,
    /// The `WHERE` clause.
    pub filter: Option<BoundExpr>,
    /// The `RETURNING` columns.
    pub returning: Vec<BoundResultColumn>,
    /// The `ORDER BY` that decides which rows a `LIMIT` keeps.
    ///
    /// Empty unless the statement wrote one, and then always with a `LIMIT`,
    /// because the binder refuses an order with nothing to limit. It goes onto
    /// the query that finds the rows to change, which is where SQLite puts it
    /// too: a limited write is `WHERE rowid IN (SELECT rowid ... ORDER BY ...
    /// LIMIT ...)` there.
    pub order_by: Vec<BoundOrderTerm>,
    /// The `LIMIT`.
    pub limit: Option<BoundExpr>,
    /// The `OFFSET`.
    pub offset: Option<BoundExpr>,
    /// The triggers this write fires, in schema order.
    pub triggers: Vec<BoundTrigger>,
    /// The rows to fire an `INSTEAD OF` trigger for, when the target is a view.
    pub view_rows: Option<Box<BoundSelect>>,
}

/// Reports whether an `INSERT` can resolve a conflict by deleting a row.
///
/// Either the statement said so, or one of the table's own constraints did.
/// It is asked before the delete's keys are bound, because binding them costs
/// a parse and a bind each and the answer is no for almost every insert.
fn can_replace(table: &TableInfo, statement: Option<ConflictAction>) -> bool {
    if statement == Some(ConflictAction::Replace) {
        return true;
    }
    table
        .indexes
        .iter()
        .any(|index| index.conflict == Some(ConflictAction::Replace))
        || table.columns.iter().any(|column| {
            column.not_null_conflict == Some(ConflictAction::Replace)
                || column.primary_key_conflict == Some(ConflictAction::Replace)
        })
}

/// Reports whether an unusable key's fault is one this write has to report.
///
/// A child's write reports a missing parent; a parent's write reports a
/// mismatch. A statement that touches neither side of the broken key does not
/// have to care, which is why the fault is carried rather than raised when the
/// schema was read.
fn fault_applies(
    planned: &crate::catalog_view::ForeignKeyTrigger,
    event: &TriggerEventInfo,
) -> bool {
    match event {
        TriggerEventInfo::Insert => planned.is_check,
        TriggerEventInfo::Delete => !planned.is_check,
        TriggerEventInfo::Update(_) => true,
    }
}

/// Marks a synthesised body's aborts as the foreign key's rather than a
/// trigger's.
///
/// The generated text says `RAISE(ABORT, ...)` because that is what a person
/// would have written, and what a person writes reports
/// `SQLITE_CONSTRAINT_TRIGGER`. A foreign key reports its own code, and the
/// only difference between the two is which constraint asked - so it is set
/// here, on the bodies this binder generated, and nowhere else.
fn report_as_foreign_key(trigger: &mut BoundTrigger) {
    trigger.foreign_key = true;
    for statement in &mut trigger.body {
        let BoundTriggerStatement::Select(select) = statement else {
            continue;
        };
        for column in &mut select.columns {
            if let BoundExpr::Raise { foreign_key, .. } = &mut column.expr {
                *foreign_key = true;
            }
        }
    }
}

/// Returns whether a view has an `INSTEAD OF` trigger for one event.
fn has_instead_of(table: &TableInfo, event: &TriggerEventInfo) -> bool {
    table
        .triggers
        .iter()
        .any(|trigger| trigger.time == ast::TriggerTime::InsteadOf && trigger.fires_for(event, &[]))
}

/// The target position that stands for the rowid rather than a column.
///
/// A table cannot have this many columns - SQLite's limit is two thousand - so
/// there is no position it can collide with, and one sentinel is cheaper than
/// a parallel `Option` threaded through every target list.
const ROWID_TARGET: u16 = u16::MAX;

/// Returns whether a name is one of the rowid's three spellings.
fn is_rowid_name(folded: &[u8]) -> bool {
    matches!(folded, b"rowid" | b"oid" | b"_rowid_")
}

/// How deep one write may drive triggers firing other triggers.
///
/// SQLite's own limit is `SQLITE_MAX_TRIGGER_DEPTH`, enforced when the frame is
/// pushed. Trigger bodies are inlined here rather than run as frames, so the
/// same limit is enforced where the inlining happens - and it has to be, or a
/// schema in which two triggers write each other's tables would compile until
/// the compiler ran out of memory.
///
/// **This is one number now, and it is the one `.limit` reports.** There used
/// to be two constants of this name: this one at 32, which was the number
/// actually enforced, and `inillucent-exec`'s at 1000, checked at run time over
/// a tree the binder had already capped at 32 - so that check could never fire.
/// `crates/inillucent-base/manifests/limits.toml` advertised 1000 and
/// `inillucent diagnose` printed 1000, and a chain of forty distinct triggers
/// that the oracle ran was refused here (task-1946, H3). The binder reads
/// `Limit::TriggerDepth` from the connection now, which `.limit trigger_depth`
/// and the driver both set; this constant is what a binder built without limits
/// falls back to, and it is the manifest's default.
pub const MAX_TRIGGER_DEPTH: usize = 1000;

/// How deep one chain of foreign-key actions may go.
///
/// A cascade reaches this only when the keys form a cycle, which in practice
/// means a table whose parent column points at itself. SQLite's own limit is a
/// run-time recursion depth; this one is a compile-time inlining depth, and it
/// is smaller for that reason.
pub const MAX_FOREIGN_KEY_DEPTH: usize = 64;

/// How many foreign-key action bodies one statement may inline in total.
///
/// The depth limit alone is not enough: a table with three keys that all cycle
/// would inline three bodies per level, so the limit that matters is the total.
/// A chain, which is what a self-referencing tree produces, spends one per
/// level and reaches the depth limit first.
pub const MAX_FOREIGN_KEY_STATEMENTS: usize = 256;

impl<'a> Binder<'a> {
    /// Binds an `INSERT` or `REPLACE`.
    pub fn bind_insert(&mut self, insert: &ast::Insert) -> Result<BoundInsert, ParseError> {
        // **A `WITH` on a DML statement is the same `WITH` a `SELECT` has.** The
        // CTEs are in scope for the whole statement - the source query of an
        // `INSERT`, the `WHERE` of an `UPDATE` or `DELETE` - and the binder's
        // CTE stack already handles nesting, so pushing them here is all it
        // takes. They were refused rather than bound, which is what a migration
        // script written for SQLite hits first.
        let pushed = self.push_ctes(&insert.with)?;
        let bound = self.bind_insert_body(insert);
        if pushed {
            self.pop_ctes();
        }
        bound
    }

    /// Binds an `INSERT` with its CTEs already in scope.
    fn bind_insert_body(&mut self, insert: &ast::Insert) -> Result<BoundInsert, ParseError> {
        let table = self.writable_target(
            insert.database,
            insert.table,
            Span::default(),
            &TriggerEventInfo::Insert,
        )?;
        let alias = match insert.alias {
            Some(alias) => self.ast.text(alias).to_vec(),
            None => table.name.clone(),
        };
        let target_source = self.push_write_source(table.clone(), alias);
        // `DEFAULT VALUES` supplies nothing, so every column takes its default
        // - which is what an empty target list means here. The grammar does
        // not allow a column list with it, so there is none to honour.
        let targets = match insert.source {
            ast::InsertSource::DefaultValues => Vec::new(),
            ast::InsertSource::Select(_) => self.insert_targets(&table, &insert.columns)?,
        };
        let (source, arity) = self.bind_insert_source(&insert.source, &table, &targets)?;
        if arity != targets.len() {
            return Err(refused(
                format!("{} values for {} columns", arity, targets.len()),
                Span::default(),
            ));
        }
        let (columns, rowid) = self.column_sources(&table, &targets)?;
        let named_rowid = targets.iter().position(|target| *target == ROWID_TARGET);
        let checks = self.bind_checks(&table)?;
        let not_null_defaults = self.bind_not_null_defaults(&table)?;
        let index_exprs = self.bind_index_exprs(&table)?;
        let upsert = self.bind_upsert(&table, insert)?;
        let returning = self.bind_returning(&insert.returning)?;
        let mut triggers = self.bind_triggers(&table, TriggerEventInfo::Insert, &[])?;
        triggers.extend(self.bind_foreign_keys(&table, TriggerEventInfo::Insert, &[])?);
        let replace_triggers = if can_replace(&table, insert.on_conflict) {
            self.bind_foreign_keys(&table, TriggerEventInfo::Delete, &[])?
        } else {
            Vec::new()
        };
        let sequence_root = if table.autoincrement {
            self.catalog
                .find_table(None, b"sqlite_sequence")
                .map_or(0, |sequence| sequence.root)
        } else {
            0
        };
        Ok(BoundInsert {
            table,
            index_exprs,
            target_source,
            columns,
            rowid,
            named_rowid,
            source,
            arity,
            on_conflict: insert.on_conflict,
            checks,
            not_null_defaults,
            upsert,
            sequence_root,
            returning,
            triggers,
            replace_triggers,
        })
    }

    /// Binds an `UPDATE`.
    pub fn bind_update(&mut self, update: &ast::Update) -> Result<BoundUpdate, ParseError> {
        let pushed = self.push_ctes(&update.with)?;
        let bound = self.bind_update_body(update);
        if pushed {
            self.pop_ctes();
        }
        bound
    }

    /// Binds an `UPDATE ... FROM` clause, after the target.
    ///
    /// Returns the terms the clause adds and the constraints its table-valued
    /// functions' arguments became, which belong in the statement's `WHERE`.
    ///
    /// **A table-valued function's arguments are constraints on its hidden
    /// columns**, which `bind_table_arguments` leaves for the statement's
    /// `WHERE`. A `SELECT` adds them there; the `UPDATE` did not, so `UPDATE
    /// todo SET position = j.key FROM json_each('[3,1,2]') AS j WHERE todo.id =
    /// j.value` ran `json_each` with no document, found no rows and reported
    /// success with nothing changed.
    ///
    /// **The terms this block owns, not every source bound since.** A derived
    /// table binds its own inner terms into the same list, and taking
    /// everything bound after the target made them top level terms of the
    /// `UPDATE` as well: `FROM (SELECT id, pos FROM ord) AS p` joined `ord`
    /// again, beside `p`, so every row was found once per row of `ord` and the
    /// statement reported 9 changes for 3.
    ///
    /// @param from - the clause's terms, in written order
    fn bind_update_from(
        &mut self,
        from: &[ast::FromTermId],
    ) -> Result<(Vec<crate::bind::BoundSource>, Vec<BoundExpr>), ParseError> {
        let before = self.sources.len();
        for term in from {
            self.bind_from_term(*term)?;
        }
        self.desugar_join_constraints(from)?;
        let arguments = core::mem::take(&mut self.pending_constraints);
        let joined: Vec<crate::bind::BoundSource> = self
            .scope()
            .iter()
            .filter(|id| **id >= before)
            .filter_map(|id| self.sources.get(*id).cloned())
            .collect();
        Ok((joined, arguments))
    }

    /// Binds an `UPDATE` with its CTEs already in scope.
    fn bind_update_body(&mut self, update: &ast::Update) -> Result<BoundUpdate, ParseError> {
        if let Some(refusal) = order_without_limit(update.limited_at, update.limit, "UPDATE") {
            return Err(refusal);
        }
        let (table, source) =
            self.write_target_from_term(update.target, &TriggerEventInfo::Update(Vec::new()))?;
        // **The `FROM` terms are bound after the target**, so the target keeps
        // the lowest source number and every reference to an unqualified column
        // resolves to it first - which is SQLite's rule and the reason
        // `UPDATE t SET v = v + 1 FROM s` means the target's `v`.
        let (joined, arguments) = self.bind_update_from(&update.from)?;
        let mut assignments = Vec::new();
        for (names, value) in &update.assignments {
            let bound = self.bind_expr(*value)?;
            for name in names {
                let folded = self.ast.folded(*name).to_vec();
                // `rowid`, `oid` and `_rowid_` name the row's key rather than a
                // declared column, unless the table declares a column by one of
                // those names - which is what `is_rowid_name` decides.
                if table.is_rowid_name(&folded) {
                    if assignments.iter().any(|held: &BoundAssignment| held.rowid) {
                        return Err(refused(
                            format!(
                                "column {} is assigned twice",
                                String::from_utf8_lossy(self.ast.text(*name))
                            ),
                            Span::default(),
                        ));
                    }
                    assignments.push(BoundAssignment {
                        column: 0,
                        rowid: true,
                        value: bound.clone(),
                    });
                    continue;
                }
                let Some(position) = table.column_position(&folded) else {
                    return Err(no_such_column(self.ast.text(*name), Span::default()));
                };
                // **An assignment to a generated column is refused, not
                // ignored (task-1913).** SQLite answers `cannot UPDATE
                // generated column "c"`; this accepted the statement, reported
                // it as a success, and wrote nothing the caller asked for -
                // either the record took the value and the column stopped
                // agreeing with its own expression, or the recompute above put
                // it back and the assignment was silently dropped. `INSERT`
                // already refused the same thing.
                self.refuse_generated(&table, position, "UPDATE", Span::default())?;
                if assignments
                    .iter()
                    .any(|existing: &BoundAssignment| existing.column == position)
                {
                    return Err(refused(
                        format!(
                            "column {} is assigned twice",
                            String::from_utf8_lossy(self.ast.text(*name))
                        ),
                        Span::default(),
                    ));
                }
                assignments.push(BoundAssignment {
                    column: position,
                    rowid: false,
                    value: bound.clone(),
                });
            }
        }
        // The rowid assignment sorts with the declared columns rather than
        // ahead of them, because `column` says nothing for it and the order
        // only has to be stable.
        assignments.sort_by_key(|assignment| (assignment.rowid, assignment.column));
        let mut filter = match update.filter {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        for constraint in arguments {
            filter = Some(match filter.take() {
                Some(existing) => BoundExpr::And(Box::new(existing), Box::new(constraint)),
                None => constraint,
            });
        }
        let generated = self.bind_stored_generated(&table)?;
        let checks = self.bind_checks(&table)?;
        let not_null_defaults = self.bind_not_null_defaults(&table)?;
        let index_exprs = self.bind_index_exprs(&table)?;
        let returning = self.bind_returning(&update.returning)?;
        // Bound as expressions, the way an aggregate's own `ORDER BY` is: a
        // write has no result columns, so a bare integer names no ordinal.
        let order_by = self.bind_aggregate_order(&update.order_by)?;
        let limit = match update.limit {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let offset = match update.offset {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        // The rowid is not a declared column, so no `UPDATE OF` trigger and no
        // foreign key can be keyed on it and it contributes no name here.
        let changed: Vec<Vec<u8>> = assignments
            .iter()
            .filter(|assignment| !assignment.rowid)
            .filter_map(|assignment| table.column(assignment.column))
            .map(|column| column.folded.clone())
            .collect();
        let mut triggers =
            self.bind_triggers(&table, TriggerEventInfo::Update(Vec::new()), &changed)?;
        triggers.extend(self.bind_foreign_keys(
            &table,
            TriggerEventInfo::Update(Vec::new()),
            &changed,
        )?);
        let view_rows = self
            .view_rows(&table, filter.clone())
            .map(|rows| limit_view_rows(rows, &order_by, &limit, &offset));
        let index_hint = self.write_hint(source, &index_exprs, filter.as_ref(), &joined)?;
        Ok(BoundUpdate {
            table,
            index_exprs,
            index_hint,
            source,
            from: joined,
            assignments,
            generated,
            filter,
            on_conflict: update.on_conflict,
            checks,
            not_null_defaults,
            returning,
            order_by,
            limit,
            offset,
            triggers,
            view_rows,
        })
    }

    /// Binds a `DELETE`.
    pub fn bind_delete(&mut self, delete: &ast::Delete) -> Result<BoundDelete, ParseError> {
        let pushed = self.push_ctes(&delete.with)?;
        let bound = self.bind_delete_body(delete);
        if pushed {
            self.pop_ctes();
        }
        bound
    }

    /// Binds a `DELETE` with its CTEs already in scope.
    fn bind_delete_body(&mut self, delete: &ast::Delete) -> Result<BoundDelete, ParseError> {
        if let Some(refusal) = order_without_limit(delete.limited_at, delete.limit, "DELETE") {
            return Err(refusal);
        }
        let (table, source) =
            self.write_target_from_term(delete.target, &TriggerEventInfo::Delete)?;
        let index_exprs = self.bind_index_exprs(&table)?;
        let filter = match delete.filter {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let returning = self.bind_returning(&delete.returning)?;
        let order_by = self.bind_aggregate_order(&delete.order_by)?;
        let limit = match delete.limit {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let offset = match delete.offset {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let mut triggers = self.bind_triggers(&table, TriggerEventInfo::Delete, &[])?;
        triggers.extend(self.bind_foreign_keys(&table, TriggerEventInfo::Delete, &[])?);
        let view_rows = self
            .view_rows(&table, filter.clone())
            .map(|rows| limit_view_rows(rows, &order_by, &limit, &offset));
        let index_hint = self.write_hint(source, &index_exprs, filter.as_ref(), &[])?;
        Ok(BoundDelete {
            table,
            index_exprs,
            index_hint,
            source,
            filter,
            returning,
            order_by,
            limit,
            offset,
            triggers,
            view_rows,
        })
    }

    /// Binds the triggers one write fires, bodies and all.
    ///
    /// The bodies are bound here, into the same binder, so their FROM terms take
    /// statement-wide source numbers alongside the write's own. That is what
    /// lets the compiler inline them: a trigger body is not a separate program
    /// with a separate cursor space, it is more of this statement.
    ///
    /// A trigger already being bound is skipped rather than bound again, which
    /// is SQLite's behaviour with its default `recursive_triggers = off` and is
    /// also the only reason inlining terminates.
    ///
    /// **Walked newest first.** `live.triggers` is in the order
    /// `inillucent_catalog::paged::tables_from_entries` appended them while
    /// reading `sqlite_schema` - the order the triggers were created in - and
    /// SQLite fires two triggers of the same timing and event in the opposite
    /// order: it keeps each table's trigger list with the most recently
    /// created one first, so that one fires first.
    /// `dml_differential.rs`'s `row_triggers_match_sqlite` has two `AFTER
    /// INSERT` triggers on one table - `t_ai`, created first, and `t_high`,
    /// created after it - and the pinned reference fires `t_high` before
    /// `t_ai` on every insert. Reversing the walk here, once, at the one place
    /// that reads `live.triggers` into a statement's own trigger list, is
    /// enough: nothing downstream reorders it again.
    fn bind_triggers(
        &mut self,
        table: &TableInfo,
        event: TriggerEventInfo,
        changed: &[Vec<u8>],
    ) -> Result<Vec<BoundTrigger>, ParseError> {
        // The catalog reference is copied out of `self` first: the trigger's
        // arena has to outlive the binder for the body to be bound in place,
        // and a borrow taken through `&self` would end at the first `&mut self`.
        let catalog = self.catalog;
        let database = catalog.database_name(table.database).to_vec();
        let Some(live) = catalog.find_table(Some(database.as_slice()), &table.folded) else {
            return Ok(Vec::new());
        };
        let (old, new) = match event {
            TriggerEventInfo::Insert => (false, true),
            TriggerEventInfo::Delete => (true, false),
            TriggerEventInfo::Update(_) => (true, true),
        };
        let mut bound = Vec::new();
        for trigger in live.triggers.iter().rev() {
            if !trigger.fires_for(&event, changed) {
                continue;
            }
            if self.firing.contains(&trigger.folded) {
                continue;
            }
            if self.firing.len() >= self.trigger_depth {
                // The number is in the message because a settable limit that
                // refuses without saying what it was leaves a reader guessing
                // between the default and whatever `.limit` last set.
                return Err(refused(
                    format!(
                        "too many levels of trigger recursion: the limit is {}",
                        self.trigger_depth
                    ),
                    Span::default(),
                ));
            }
            self.firing.push(trigger.folded.clone());
            let saved_ast = self.ast;
            let saved_scopes = core::mem::take(&mut self.scopes);
            let saved_aliases = self.row_aliases.take();
            let saved_target = self.view_target.take();
            // A trigger body is schema text: the statements in it were written
            // by whoever wrote the file, and they run because a write happened
            // rather than because anybody submitted them.
            let saved_site = self.call_site;
            self.call_site = crate::function::CallSite::Schema;
            self.ast = &trigger.ast;
            self.row_aliases = Some(crate::bind::RowAliases {
                table: table.clone(),
                old,
                new,
            });
            let result = self.bind_trigger_body(trigger, table);
            self.call_site = saved_site;
            self.ast = saved_ast;
            self.scopes = saved_scopes;
            self.row_aliases = saved_aliases;
            self.view_target = saved_target;
            self.firing.pop();
            bound.push(result?);
        }
        Ok(bound)
    }

    /// Binds the triggers this write's foreign keys imply.
    ///
    /// The triggers themselves were generated when the schema was read - both
    /// directions of every key, since nothing in the file records the reverse
    /// one. What is decided here is which of them apply: whether keys are
    /// enforced at all, whether a check waits for the commit, and whether this
    /// particular write touches the columns a check is about.
    fn bind_foreign_keys(
        &mut self,
        table: &TableInfo,
        event: TriggerEventInfo,
        changed: &[Vec<u8>],
    ) -> Result<Vec<BoundTrigger>, ParseError> {
        if !self.foreign_keys || table.kind != TableKind::Table {
            return Ok(Vec::new());
        }
        let catalog = self.catalog;
        let database = catalog.database_name(table.database).to_vec();
        let Some(live) = catalog.find_table(Some(database.as_slice()), &table.folded) else {
            return Ok(Vec::new());
        };
        let mut bound = Vec::new();
        for planned in &live.foreign_key_triggers {
            if planned.is_check && (planned.deferred || self.defer_foreign_keys) {
                continue;
            }
            let Some(trigger) = planned.trigger.as_ref() else {
                if fault_applies(planned, &event) {
                    return Err(crate::bind::schema_refused(
                        String::from_utf8_lossy(&planned.fault).into_owned(),
                        Span::default(),
                    ));
                }
                continue;
            };
            if !trigger.fires_for(&event, changed) {
                continue;
            }
            if self.firing_foreign_keys.contains(&trigger.folded) {
                continue;
            }
            let mut one = self.bind_foreign_key_trigger(table, trigger, &event)?;
            one.self_referencing = planned.self_referencing;
            bound.push(one);
        }
        Ok(bound)
    }

    /// Binds one synthesised trigger, inside the recursion budget.
    ///
    /// The budget is spent here rather than where the trigger was generated,
    /// because what a cascade costs is the *bound* body: one copy per level it
    /// can reach, and it can reach itself only when the keys form a cycle.
    fn bind_foreign_key_trigger(
        &mut self,
        table: &TableInfo,
        trigger: &'a TriggerInfo,
        event: &TriggerEventInfo,
    ) -> Result<BoundTrigger, ParseError> {
        if self.foreign_key_depth >= MAX_FOREIGN_KEY_DEPTH || self.foreign_key_budget == 0 {
            return Err(refused(
                "too many levels of foreign key recursion",
                Span::default(),
            ));
        }
        self.foreign_key_depth = self.foreign_key_depth.saturating_add(1);
        self.foreign_key_budget = self.foreign_key_budget.saturating_sub(1);
        self.firing_foreign_keys.push(trigger.folded.clone());
        let (old, new) = match event {
            TriggerEventInfo::Insert => (false, true),
            TriggerEventInfo::Delete => (true, false),
            TriggerEventInfo::Update(_) => (true, true),
        };
        let saved_ast = self.ast;
        let saved_scopes = core::mem::take(&mut self.scopes);
        let saved_aliases = self.row_aliases.take();
        let saved_target = self.view_target.take();
        // A synthesised key action is generated from a `REFERENCES` clause the
        // schema wrote, so it is schema too - the same site a written trigger
        // gets, because the binder turns both into the same text.
        let saved_site = self.call_site;
        self.call_site = crate::function::CallSite::Schema;
        self.ast = &trigger.ast;
        self.row_aliases = Some(crate::bind::RowAliases {
            table: table.clone(),
            old,
            new,
        });
        let result = self.bind_trigger_body(trigger, table);
        self.call_site = saved_site;
        self.ast = saved_ast;
        self.scopes = saved_scopes;
        self.row_aliases = saved_aliases;
        self.view_target = saved_target;
        self.foreign_key_depth = self.foreign_key_depth.saturating_sub(1);
        self.firing_foreign_keys.pop();
        let mut bound = result?;
        report_as_foreign_key(&mut bound);
        Ok(bound)
    }

    /// Binds one trigger's guard and body statements.
    fn bind_trigger_body(
        &mut self,
        trigger: &TriggerInfo,
        table: &TableInfo,
    ) -> Result<BoundTrigger, ParseError> {
        let when = match trigger.when {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        let mut body = Vec::new();
        for statement in &trigger.body {
            // Each statement gets a fresh scope stack. A body statement's names
            // resolve against its own tables and against OLD and NEW, never
            // outward into the statement that fired it.
            let saved = core::mem::take(&mut self.scopes);
            let one = self.bind_trigger_statement(statement);
            self.scopes = saved;
            body.push(one?);
        }
        Ok(BoundTrigger {
            name: trigger.name.clone(),
            table: table.folded.clone(),
            time: trigger.time,
            when,
            body,
            foreign_key: false,
            self_referencing: false,
        })
    }

    /// Binds one statement of a trigger body.
    pub(crate) fn bind_trigger_statement(
        &mut self,
        statement: &ast::Statement,
    ) -> Result<BoundTriggerStatement, ParseError> {
        match statement {
            ast::Statement::Insert(insert) => {
                if !insert.returning.is_empty() {
                    return Err(refused(
                        "RETURNING is not allowed on a trigger body statement",
                        Span::default(),
                    ));
                }
                Ok(BoundTriggerStatement::Insert(Box::new(
                    self.bind_insert(insert)?,
                )))
            }
            ast::Statement::Update(update) => {
                if !update.returning.is_empty() {
                    return Err(refused(
                        "RETURNING is not allowed on a trigger body statement",
                        Span::default(),
                    ));
                }
                Ok(BoundTriggerStatement::Update(Box::new(
                    self.bind_update(update)?,
                )))
            }
            ast::Statement::Delete(delete) => {
                if !delete.returning.is_empty() {
                    return Err(refused(
                        "RETURNING is not allowed on a trigger body statement",
                        Span::default(),
                    ));
                }
                Ok(BoundTriggerStatement::Delete(Box::new(
                    self.bind_delete(delete)?,
                )))
            }
            ast::Statement::Select(select) => Ok(BoundTriggerStatement::Select(Box::new(
                self.bind_select(*select)?,
            ))),
            _ => Err(unsupported(
                "that statement in a trigger body",
                Span::default(),
            )),
        }
    }

    /// Resolves a write target and refuses the things that cannot be written.
    fn writable_target(
        &mut self,
        database: Option<ast::NameId>,
        name: ast::NameId,
        span: Span,
        event: &TriggerEventInfo,
    ) -> Result<TableInfo, ParseError> {
        let qualifier = database.map(|id| self.ast.folded(id).to_vec());
        let folded = self.ast.folded(name).to_vec();
        let Some(table) = self
            .catalog
            .find_table(qualifier.as_deref(), &folded)
            .cloned()
        else {
            return Err(crate::bind::no_such_table(self.ast.text(name), span));
        };
        match table.kind {
            TableKind::View => {
                // A view is writable exactly when it has an `INSTEAD OF`
                // trigger for this event: the trigger *is* the write, and the
                // view itself is never touched.
                if !has_instead_of(&table, event) {
                    return Err(unsupported("writing to a view", span));
                }
                let expanded = self.expanded_view(&table, span)?;
                self.record_write_dependency(table.database);
                return Ok(expanded);
            }
            TableKind::Virtual => {
                // A module decides whether it can be written; a module that
                // cannot refuses the call rather than the statement, because
                // "this table is read-only" is the module's fact and not the
                // binder's. What the binder still checks is that the table has
                // a module at all - a virtual table this build has no module
                // for has no columns either, and nothing can be written to it.
                if table.columns.is_empty() {
                    return Err(unsupported("that virtual table's module", span));
                }
                self.record_write_dependency(table.database);
                return Ok(table);
            }
            TableKind::Subquery => return Err(unsupported("writing to a subquery", span)),
            TableKind::Table => {}
        }
        if table.folded.starts_with(b"sqlite_")
            && !WRITABLE_INTERNAL.contains(&table.folded.as_slice())
        {
            return Err(unsupported(
                "writing to a table whose name begins with sqlite_",
                span,
            ));
        }
        self.record_write_dependency(table.database);
        Ok(table)
    }

    /// Resolves the target of an UPDATE or DELETE, which is a FROM term.
    fn write_target_from_term(
        &mut self,
        id: ast::FromTermId,
        event: &TriggerEventInfo,
    ) -> Result<(TableInfo, usize), ParseError> {
        let Some(term) = self.ast.from_term(id) else {
            return Err(unsupported("missing target", Span::default()));
        };
        let ast::FromSource::Table {
            database,
            name,
            indexed_by,
            ..
        } = term.source
        else {
            return Err(unsupported("a target that is not a table", term.span));
        };
        let table = self.writable_target(database, name, term.span, event)?;
        // The same rule as a SELECT's: an `INDEXED BY` that names no index of
        // the table is refused rather than ignored (task-1979, F7). This path
        // has the table in hand rather than a bound source, so it asks the
        // table directly.
        if let ast::IndexHint::IndexedBy(index) = indexed_by {
            let folded = self.ast.folded(index).to_vec();
            if !table.indexes.iter().any(|held| held.folded == folded) {
                return Err(crate::bind::no_such_index(self.ast.text(index), term.span));
            }
        }
        let alias = match term.alias {
            Some(alias) => self.ast.text(alias).to_vec(),
            None => table.name.clone(),
        };
        if table.kind == TableKind::View {
            // The view goes in as an ordinary nested query, so the statement's
            // WHERE and SET bind against the view's own columns and against the
            // term the block producing OLD will iterate. Binding first and
            // re-pointing afterwards would be two chances to disagree.
            let inner = self.view_query(&table, term.span)?;
            let source = BoundSource {
                index_hint: crate::bind::IndexChoice::Any,
                id: self.sources.len(),
                rows: crate::bind::SourceRows::Subquery(Box::new(inner)),
                table: std::rc::Rc::new(table.clone()),
                alias,
                join: ast::JoinKind::Comma,
                constraint: None,
                suppressed: Vec::new(),
                index_exprs: Vec::new(),
            };
            self.view_target = Some(source.id);
            let scope = source.id;
            self.sources.push(source);
            self.scopes.push(vec![scope]);
            return Ok((table, scope));
        }
        let scope = self.push_write_source(table.clone(), alias);
        let choice = self.index_choice(indexed_by);
        if let Some(source) = self.sources.get_mut(scope) {
            source.index_hint = choice;
        }
        Ok((table, scope))
    }

    /// Returns a view's `TableInfo` with the columns its body produces.
    ///
    /// A view's catalog entry carries no column list - its columns are whatever
    /// binding its `SELECT` says they are - so a statement that writes one needs
    /// the body bound before `new.column` can resolve to anything at all.
    pub(crate) fn expanded_view(
        &mut self,
        table: &TableInfo,
        span: Span,
    ) -> Result<TableInfo, ParseError> {
        let bound = self.view_query(table, span)?;
        let mut expanded = table.clone();
        expanded.columns = crate::bind::subquery_columns(&bound, &[]);
        Ok(expanded)
    }

    /// Binds a view's body, out of the arena the catalog snapshot holds.
    fn view_query(&mut self, table: &TableInfo, span: Span) -> Result<BoundSelect, ParseError> {
        let catalog = self.catalog;
        let database = catalog.database_name(table.database).to_vec();
        let Some(live) = catalog.find_table(Some(database.as_slice()), &table.folded) else {
            return Err(crate::bind::no_such_table(&table.name, span));
        };
        let Some(body) = live.view.as_ref() else {
            return Err(unsupported(
                "a view whose definition could not be parsed",
                span,
            ));
        };
        let names = body.columns.clone();
        let saved_ast = self.ast;
        let saved_scopes = core::mem::take(&mut self.scopes);
        self.ast = &body.ast;
        let bound = self.bind_select(body.select);
        self.ast = saved_ast;
        self.scopes = saved_scopes;
        let mut bound = bound?;
        // `CREATE VIEW v (a, b)` renames the body's columns, and those are the
        // names `new.a` resolves against.
        for (position, name) in names.iter().enumerate() {
            if let Some(column) = bound.columns.get_mut(position) {
                column.name = name.clone();
            }
        }
        Ok(bound)
    }

    /// Builds the block whose rows an `INSTEAD OF UPDATE` or `DELETE` fires for.
    ///
    /// It reads the term `write_target_from_term` already pushed, so the filter
    /// handed in here - bound against that same term - needs no adjustment.
    fn view_rows(
        &mut self,
        table: &TableInfo,
        filter: Option<BoundExpr>,
    ) -> Option<Box<BoundSelect>> {
        // The kind is checked before the target is taken. A trigger body's own
        // UPDATE binds through here too, and taking first meant the body's
        // statement - whose target is an ordinary table - consumed the view
        // target belonging to the statement that fired it, which then compiled
        // as a write to a view's root page of zero.
        if table.kind != TableKind::View {
            return None;
        }
        let id = self.view_target.take()?;
        let source = self.sources.get(id)?.clone();
        let columns = table
            .columns
            .iter()
            .enumerate()
            .map(|(position, column)| BoundResultColumn {
                expr: BoundExpr::Column {
                    source: id,
                    column: position as u16,
                    slot: position as u16,
                    affinity: column.affinity,
                    collation: Collation::from_name(
                        core::str::from_utf8(&column.collation).unwrap_or("BINARY"),
                    )
                    .unwrap_or(Collation::Binary),
                },
                name: column.name.clone(),
                origin: None,
                declared_type: column.declared_type.clone(),
            })
            .collect();
        Some(Box::new(crate::bind::block_over(source, filter, columns)))
    }

    /// Returns the target's index hint, or refuses a write whose `INDEXED BY`
    /// index cannot find its rows.
    ///
    /// The same rule and the same test a `SELECT` gets from
    /// `crate::bind::refuse_unanswerable_hints`, asked of the query the write
    /// will run to find its rows: the target, any `UPDATE ... FROM` terms, and
    /// the statement's `WHERE`. The pinned 3.53.4 shell refuses
    /// `DELETE FROM h INDEXED BY h_part WHERE a = 1`, where `h_part` is declared
    /// `WHERE c > 3`, with `no query solution`.
    /// @param source - the target's statement-wide number
    /// @param index_exprs - the target's bound index expressions
    /// @param filter - the statement's `WHERE`
    /// @param joined - the `UPDATE ... FROM` terms, empty for a `DELETE`
    fn write_hint(
        &self,
        source: usize,
        index_exprs: &[BoundIndexExprs],
        filter: Option<&BoundExpr>,
        joined: &[BoundSource],
    ) -> Result<crate::bind::IndexChoice, ParseError> {
        let Some(target) = self.sources.get(source) else {
            return Ok(crate::bind::IndexChoice::Any);
        };
        if target.index_hint == crate::bind::IndexChoice::Any {
            return Ok(crate::bind::IndexChoice::Any);
        }
        let mut probe = target.clone();
        probe.index_exprs = index_exprs.to_vec();
        let mut block = crate::bind::block_over(probe, filter.cloned(), Vec::new());
        block.sources.extend(joined.iter().cloned());
        if crate::plan::unanswerable_index_hint(&block).is_some() {
            return Err(crate::bind::no_query_solution(Span::default()));
        }
        Ok(target.index_hint.clone())
    }

    /// Makes the target table the statement's one visible source.
    ///
    /// It opens a scope holding just the target, so every name in the
    /// statement's `SET`, `WHERE` and `RETURNING` resolves against the table
    /// being written and nothing else.
    fn push_write_source(&mut self, table: TableInfo, alias: Vec<u8>) -> usize {
        let id = self.sources.len();
        self.sources.push(BoundSource {
            index_hint: crate::bind::IndexChoice::Any,
            id,
            rows: crate::bind::SourceRows::Table,
            table: std::rc::Rc::new(table),
            alias,
            join: ast::JoinKind::Comma,
            constraint: None,
            suppressed: Vec::new(),
            index_exprs: Vec::new(),
        });
        self.scopes.push(vec![id]);
        id
    }

    /// Refuses an attempt to write a generated column.
    ///
    /// SQLite's message names the column, because the usual cause is a script
    /// that inserts every column of a table one of whose columns has since been
    /// made generated. It names the statement too - `INSERT` or `UPDATE` - and
    /// so does this.
    ///
    /// @param table - the table being written
    /// @param position - the column the statement named
    /// @param verb - `INSERT into` or `UPDATE`, as SQLite writes it
    /// @param span - where the name was written
    fn refuse_generated(
        &self,
        table: &TableInfo,
        position: u16,
        verb: &str,
        span: Span,
    ) -> Result<(), ParseError> {
        let Some(column) = table.column(position) else {
            return Ok(());
        };
        if !column.generated {
            return Ok(());
        }
        Err(refused(
            format!(
                "cannot {verb} generated column \"{}\"",
                String::from_utf8_lossy(&column.name)
            ),
            span,
        ))
    }

    /// Returns the target column positions an INSERT writes, in source order.
    ///
    /// With no column list the targets are every column in declaration order,
    /// which is why adding a column to a table changes what a positional
    /// INSERT means - SQLite's behaviour, and the reason the column list is
    /// worth writing.
    fn insert_targets(
        &self,
        table: &TableInfo,
        columns: &[ast::NameId],
    ) -> Result<Vec<u16>, ParseError> {
        if columns.is_empty() {
            // A bare `INSERT INTO t VALUES (...)` supplies the columns a person
            // can write, which is every column that is not generated - so a
            // table with a generated column takes fewer values than it has
            // columns, exactly as SQLite counts them.
            // A hidden column is not one of them either: a module's arguments
            // and its `rank` are named by an application that wants them, and
            // an `INSERT INTO fts VALUES ('a', 'b')` supplies the two indexed
            // columns and nothing else.
            return Ok((0..table.columns.len() as u16)
                .filter(|position| {
                    table
                        .column(*position)
                        .is_some_and(|column| !column.generated && !column.hidden)
                })
                .collect());
        }
        let mut targets = Vec::with_capacity(columns.len());
        for name in columns {
            let folded = self.ast.folded(*name).to_vec();
            let position = match table.column_position(&folded) {
                Some(position) => position,
                // A rowid table lets the statement name its rowid, under any
                // of its three spellings, and that is not a column: it is the
                // key. A declared column of the same name wins, which is why
                // this is the fallback rather than the first thing tried.
                None if table.has_rowid() && is_rowid_name(&folded) => ROWID_TARGET,
                None => return Err(no_such_column(self.ast.text(*name), Span::default())),
            };
            if targets.contains(&position) {
                return Err(refused(
                    format!(
                        "column {} is named twice",
                        String::from_utf8_lossy(self.ast.text(*name))
                    ),
                    Span::default(),
                ));
            }
            if position != ROWID_TARGET {
                self.refuse_generated(table, position, "INSERT into", Span::default())?;
            }
            targets.push(position);
        }
        Ok(targets)
    }

    /// Binds the rows an INSERT supplies.
    fn bind_insert_source(
        &mut self,
        source: &ast::InsertSource,
        table: &TableInfo,
        targets: &[u16],
    ) -> Result<(BoundInsertSource, usize), ParseError> {
        match source {
            ast::InsertSource::DefaultValues => {
                let _ = (table, targets);
                Ok((BoundInsertSource::Values(vec![Vec::new()]), 0))
            }
            ast::InsertSource::Select(id) => {
                // The target table is source zero while the rows are bound, so
                // that `INSERT INTO t SELECT ... FROM u` resolves `u`'s columns
                // and not `t`'s. Binding a SELECT replaces the source list, and
                // the target is pushed back afterwards.
                // The scope stack is emptied rather than pushed to, because a
                // pushed scope would still be searched *outward* into the
                // target's, and `INSERT INTO t SELECT a FROM u` would then
                // resolve `a` against `t` when `u` has no such column.
                let saved = core::mem::take(&mut self.scopes);
                let select = self.bind_select(*id);
                let bound = match select {
                    Ok(bound) => bound,
                    Err(error) => {
                        self.scopes = saved;
                        return Err(error);
                    }
                };
                self.scopes = saved;
                if bound.values.is_empty() {
                    let arity = bound.columns.len();
                    return Ok((BoundInsertSource::Select(Box::new(bound)), arity));
                }
                let arity = bound.values.first().map_or(0, Vec::len);
                for row in &bound.values {
                    if row.len() != arity {
                        return Err(unsupported(
                            "all VALUES rows must have the same number of columns",
                            Span::default(),
                        ));
                    }
                }
                Ok((BoundInsertSource::Values(bound.values), arity))
            }
        }
    }

    /// Works out where every table column's value comes from.
    ///
    /// A column the statement named takes its value from the source row; a
    /// column it did not takes its `DEFAULT`, and a column with no default
    /// takes NULL. The rowid is separated out here rather than in the
    /// compiler, because an `INTEGER PRIMARY KEY` column *is* the rowid and
    /// writing it into the record as well would store a duplicate that SQLite
    /// does not.
    fn column_sources(
        &mut self,
        table: &TableInfo,
        targets: &[u16],
    ) -> Result<(Vec<ColumnSource>, Option<ColumnSource>), ParseError> {
        let mut columns = Vec::with_capacity(table.columns.len());
        for position in 0..table.columns.len() as u16 {
            if let Some(expr) = self.generated_expr(table, position)? {
                columns.push(ColumnSource::Generated(expr));
                continue;
            }
            let source = match targets.iter().position(|target| *target == position) {
                Some(index) => ColumnSource::Row(index),
                None => ColumnSource::Expr(self.default_expr(table, position)?),
            };
            columns.push(source);
        }
        let rowid = match table.rowid_alias {
            Some(position) => columns.get(position as usize).cloned(),
            None => None,
        };
        Ok((columns, rowid))
    }

    /// Binds a generated column's expression, when the column is one.
    fn generated_expr(
        &mut self,
        table: &TableInfo,
        position: u16,
    ) -> Result<Option<BoundExpr>, ParseError> {
        let Some(column) = table.column(position) else {
            return Ok(None);
        };
        if !column.generated {
            return Ok(None);
        }
        let Some(sql) = column.generated_sql.clone() else {
            return Ok(Some(BoundExpr::Null));
        };
        Ok(Some(self.bind_schema_expr(&sql)?))
    }

    /// Binds every `STORED` generated column's expression.
    ///
    /// Returns them as assignments, because that is what they are on the write
    /// path: a value the statement did not write and the row has to carry. See
    /// [`BoundUpdate::generated`] for why an `UPDATE` needs them and a
    /// `VIRTUAL` column does not.
    ///
    /// @param table - the table being written
    fn bind_stored_generated(
        &mut self,
        table: &TableInfo,
    ) -> Result<Vec<BoundAssignment>, ParseError> {
        let mut generated = Vec::new();
        for position in 0..table.columns.len() as u16 {
            let Some(column) = table.column(position) else {
                continue;
            };
            if !column.generated || !column.stored {
                continue;
            }
            let Some(expr) = self.generated_expr(table, position)? else {
                continue;
            };
            generated.push(BoundAssignment {
                column: position,
                rowid: false,
                value: expr,
            });
        }
        Ok(generated)
    }

    /// Binds a column's `DEFAULT`, or NULL when it has none.
    fn default_expr(&mut self, table: &TableInfo, position: u16) -> Result<BoundExpr, ParseError> {
        let Some(column) = table.column(position) else {
            return Ok(BoundExpr::Null);
        };
        let Some(sql) = column.default_sql.as_ref() else {
            return Ok(BoundExpr::Null);
        };
        if sql.is_empty() {
            return Ok(BoundExpr::Null);
        }
        self.bind_schema_expr(sql)
    }

    /// Binds the `DEFAULT` of every `NOT NULL` column that declares one.
    ///
    /// What `REPLACE` substitutes for a NULL in such a column - see
    /// [`BoundDefault`]. A column with no default is left out, which is what
    /// makes the write path's fallback to `ABORT` the absence of an entry
    /// rather than a second test.
    ///
    /// The rowid alias is left out too: the row image carries the key the
    /// statement is about to allocate, and the write path does not check it.
    ///
    /// @param table - the table being written
    fn bind_not_null_defaults(
        &mut self,
        table: &TableInfo,
    ) -> Result<Vec<BoundDefault>, ParseError> {
        let mut defaults = Vec::new();
        for (position, column) in table.columns.iter().enumerate() {
            if !column.not_null || Some(position as u16) == table.rowid_alias {
                continue;
            }
            let Some(sql) = column.default_sql.as_ref() else {
                continue;
            };
            if sql.is_empty() {
                continue;
            }
            let expr = self.bind_schema_expr(&sql.clone())?;
            defaults.push(BoundDefault {
                column: position as u16,
                expr,
            });
        }
        Ok(defaults)
    }

    /// Binds every `CHECK` the table declares.
    fn bind_checks(&mut self, table: &TableInfo) -> Result<Vec<BoundCheck>, ParseError> {
        let mut checks = Vec::with_capacity(table.checks.len());
        for check in &table.checks {
            checks.push(BoundCheck {
                name: check.name.clone(),
                expr: self.bind_schema_expr(&check.expr_sql)?,
            });
        }
        Ok(checks)
    }

    /// Binds the expressions the table's indexes need per row.
    ///
    /// Only the indexes that need any: a partial one, and one with an
    /// expression key. Everything else is a slot of the row and needs nothing.
    ///
    /// @param table - the table being written
    fn bind_index_exprs(&mut self, table: &TableInfo) -> Result<Vec<BoundIndexExprs>, ParseError> {
        let mut bound = Vec::new();
        for (position, index) in table.indexes.iter().enumerate() {
            let needs = index.partial_sql.is_some()
                || index.columns.iter().any(|key| key.expr_sql.is_some());
            if !needs {
                continue;
            }
            let predicate = match index.partial_sql.as_ref() {
                Some(sql) => Some(self.bind_schema_expr(sql)?),
                None => None,
            };
            let mut keys = Vec::with_capacity(index.columns.len());
            for key in &index.columns {
                keys.push(match key.expr_sql.as_ref() {
                    Some(sql) => Some(self.bind_schema_expr(sql)?),
                    None => None,
                });
            }
            bound.push(BoundIndexExprs {
                position,
                predicate,
                keys,
            });
        }
        Ok(bound)
    }

    /// Parses and binds an expression that was written in the schema.
    ///
    /// It is parsed into its own arena and bound against the statement's
    /// current sources, so the result is an ordinary `BoundExpr` that refers to
    /// the target table by position and carries no reference to the schema
    /// text it came from.
    pub fn bind_schema_expr(&mut self, sql: &[u8]) -> Result<BoundExpr, ParseError> {
        let limits = Limits::default();
        let (ast, expr) = parse_expression(sql, &limits)?;
        let mut nested = Binder::new(self.catalog, &ast, self.authorizer);
        nested.trigger_depth = self.trigger_depth;
        // **This is where a `DEFAULT`, a `CHECK`, a generated column, an index
        // expression and a partial-index predicate all become a bound tree, so
        // it is where all five are told they are a schema (task-1972).** The
        // nested binder also inherits the connection's registrations and
        // collations, which it did not before: without the registrations
        // `bind_external_call` never sees the call at all, because the name
        // does not resolve to a registered function and the expression fails as
        // "no such function" - an error for the wrong reason, and one that
        // disappears the moment an application registers the same name at a
        // different arity.
        nested.externals = self.externals;
        nested.collations = self.collations;
        nested.trusted_schema = self.trusted_schema;
        nested.call_site = crate::function::CallSite::Schema;
        nested.sources = self.sources.clone();
        nested.scopes = self.scopes.clone();
        let bound = nested.bind_expr(expr)?;
        Ok(bound)
    }

    /// Binds an `ON CONFLICT` clause.
    fn bind_upsert(
        &mut self,
        table: &TableInfo,
        insert: &ast::Insert,
    ) -> Result<Vec<BoundUpsert>, ParseError> {
        if insert.upserts.is_empty() {
            return Ok(Vec::new());
        }
        // **Every clause is bound, in written order.** A statement may carry
        // several - `ON CONFLICT(k) DO UPDATE ... ON CONFLICT(id) DO UPDATE ...`
        // - and which one runs is decided at *run time*, by which constraint
        // the row actually collided with. Binding only the first was the whole
        // of the old refusal.
        for upsert in &insert.upserts {
            if upsert.target_filter.is_some() {
                return Err(unsupported(
                    "a partial-index conflict target",
                    Span::default(),
                ));
            }
        }
        // A clause with no conflict target matches any constraint, so anything
        // written after it could never run. SQLite refuses that rather than
        // accepting a clause it will never reach.
        if let Some(position) = insert
            .upserts
            .iter()
            .position(|upsert| upsert.target.is_empty())
        {
            if position + 1 < insert.upserts.len() {
                return Err(crate::bind::schema_refused(
                    "ON CONFLICT clause with no conflict target must be last",
                    Span::default(),
                ));
            }
        }
        // `excluded` is in scope for the assignments and the WHERE, and only
        // there. Setting it around the binding rather than pushing a second
        // FROM term keeps unqualified names resolving to the target row, which
        // is what SQLite does and what a second source would have made
        // ambiguous - every column of the target is also a column of
        // `excluded`.
        self.excluded = Some(table.clone());
        let mut bound = Vec::with_capacity(insert.upserts.len());
        for upsert in &insert.upserts {
            match self.bind_upsert_body(table, upsert) {
                Ok(Some(one)) => bound.push(one),
                Ok(None) => {}
                Err(error) => {
                    self.excluded = None;
                    return Err(error);
                }
            }
        }
        self.excluded = None;
        Ok(bound)
    }

    /// Binds an upsert's target, assignments and filter.
    fn bind_upsert_body(
        &mut self,
        table: &TableInfo,
        upsert: &ast::Upsert,
    ) -> Result<Option<BoundUpsert>, ParseError> {
        let mut target = Vec::new();
        for column in &upsert.target {
            let Some(name) = bare_indexed_column(self.ast, column) else {
                return Err(unsupported(
                    "an expression in a conflict target",
                    Span::default(),
                ));
            };
            let Some(position) = table.column_position(&name) else {
                return Err(no_such_column(&name, Span::default()));
            };
            target.push(position);
        }
        target.sort_unstable();
        let mut assignments = Vec::new();
        for (names, value) in &upsert.assignments {
            let bound = self.bind_expr(*value)?;
            for name in names {
                let folded = self.ast.folded(*name).to_vec();
                let Some(position) = table.column_position(&folded) else {
                    return Err(no_such_column(self.ast.text(*name), Span::default()));
                };
                assignments.push(BoundAssignment {
                    column: position,
                    rowid: false,
                    value: bound.clone(),
                });
            }
        }
        assignments.sort_by_key(|assignment| assignment.column);
        let filter = match upsert.filter {
            Some(expr) => Some(self.bind_expr(expr)?),
            None => None,
        };
        Ok(Some(BoundUpsert {
            target,
            assignments,
            do_update: upsert.do_update,
            filter,
        }))
    }

    /// Binds a `RETURNING` list, which is a result-column list over the row
    /// that was written.
    fn bind_returning(
        &mut self,
        columns: &[ast::ResultColumn],
    ) -> Result<Vec<BoundResultColumn>, ParseError> {
        if columns.is_empty() {
            return Ok(Vec::new());
        }
        self.bind_result_columns_public(columns)
    }
}

/// Returns an indexed column's bare folded name, when it names a column.
fn bare_indexed_column(ast: &crate::Ast, column: &ast::IndexedColumn) -> Option<Vec<u8>> {
    match ast.expr(column.expr) {
        Some(ast::Expr::Column {
            table: None,
            column: name,
            ..
        }) => Some(ast.folded(*name).to_vec()),
        _ => None,
    }
}

/// The extended result codes a rejected write reports.
///
/// The numbers are SQLite's own extended codes. They are written out rather
/// than derived because an application matches on them, and a code that was
/// computed from an enum's discriminant would change the day the enum did.
///
/// They live here, beside the binder that decides which constraint a statement
/// can violate, because **both** engines report them: the virtual machine
/// compiles them into a `HaltError` and the vectorised executor returns them
/// from its write path. Two copies would agree until one of them was corrected.
pub mod codes {
    /// `SQLITE_CONSTRAINT_CHECK`.
    pub const CHECK: i32 = 275;
    /// `SQLITE_CONSTRAINT_DATATYPE`, which a STRICT table reports.
    pub const DATATYPE: i32 = 3091;
    /// `SQLITE_CONSTRAINT_NOTNULL`.
    pub const NOT_NULL: i32 = 1299;
    /// `SQLITE_CONSTRAINT_PRIMARYKEY`.
    pub const PRIMARY_KEY: i32 = 1555;
    /// `SQLITE_CONSTRAINT_UNIQUE`.
    pub const UNIQUE: i32 = 2067;
    /// `SQLITE_CONSTRAINT_ROWID`.
    pub const ROWID: i32 = 2579;
    /// `SQLITE_MISMATCH`, which an `INTEGER PRIMARY KEY` reports for a value
    /// that is not an integer.
    pub const MISMATCH: i32 = 20;
    /// `SQLITE_CONSTRAINT_TRIGGER`, which `RAISE()` reports.
    pub const TRIGGER: i32 = 1811;
    /// `SQLITE_CONSTRAINT_FOREIGNKEY`.
    pub const FOREIGN_KEY: i32 = 787;
}

/// Returns the message a unique-index violation reports.
///
/// SQLite names every column of the index, comma separated, which is what an
/// application parses to find out which key collided.
///
/// @param table - the table the index belongs to
/// @param index - the index whose key collided
pub fn unique_message(table: &TableInfo, index: &IndexInfo) -> String {
    let names: Vec<String> = index
        .columns
        .iter()
        .filter_map(|key| key.column)
        .filter_map(|column| table.column(column))
        .map(|column| {
            format!(
                "{}.{}",
                String::from_utf8_lossy(&table.name),
                String::from_utf8_lossy(&column.name)
            )
        })
        .collect();
    format!("UNIQUE constraint failed: {}", names.join(", "))
}

/// Returns the message a duplicate rowid reports, and its extended code.
///
/// SQLite names the aliasing column when the table has an `INTEGER PRIMARY
/// KEY` - and reports `SQLITE_CONSTRAINT_PRIMARYKEY` for it - and names the
/// hidden `rowid` under `SQLITE_CONSTRAINT_ROWID` when it does not.
///
/// **A `WITHOUT ROWID` table has no rowid to name.** Its own key *is* its
/// primary key, held in the one index whose root is the table's, so a collision
/// reports every column of that key under `SQLITE_CONSTRAINT_PRIMARYKEY` -
/// `UNIQUE constraint failed: t.a, t.b`. It used to answer `t.rowid`, naming a
/// column the table does not have, on `INSERT` as well as `UPDATE`.
///
/// @param table - the table whose key collided
pub fn rowid_message(table: &TableInfo) -> (i32, String) {
    if table.without_rowid {
        if let Some(index) = table.indexes.iter().find(|index| index.root == table.root) {
            return (codes::PRIMARY_KEY, unique_message(table, index));
        }
    }
    match table.rowid_alias.and_then(|column| table.column(column)) {
        Some(column) => (
            codes::PRIMARY_KEY,
            format!(
                "UNIQUE constraint failed: {}.{}",
                String::from_utf8_lossy(&table.name),
                String::from_utf8_lossy(&column.name)
            ),
        ),
        None => (
            codes::ROWID,
            format!(
                "UNIQUE constraint failed: {}.rowid",
                String::from_utf8_lossy(&table.name)
            ),
        ),
    }
}

/// Puts a limited write's order, limit and offset on the query that finds a
/// view's rows.
///
/// A write through a view's `INSTEAD OF` trigger fires once per row the view
/// produces under the statement's `WHERE`, so a `LIMIT` on the write limits
/// that query. SQLite's `sqlite3MaterializeView` is handed the same three
/// clauses for the same reason.
///
/// @param rows - the query over the view the binder built
/// @param order_by - the statement's bound `ORDER BY`
/// @param limit - the statement's bound `LIMIT`
/// @param offset - the statement's bound `OFFSET`
fn limit_view_rows(
    mut rows: Box<BoundSelect>,
    order_by: &[BoundOrderTerm],
    limit: &Option<BoundExpr>,
    offset: &Option<BoundExpr>,
) -> Box<BoundSelect> {
    rows.order_by = order_by.to_vec();
    rows.limit = limit.clone();
    rows.offset = offset.clone();
    rows
}

/// Returns the refusal a `DELETE` or `UPDATE` with `ORDER BY` and no `LIMIT`
/// earns, or `None` when the clause is allowed.
///
/// **`ORDER BY` and `LIMIT` on a write are run, not refused (task-2120).** They
/// were refused in the pinned reference's words, `near "ORDER": syntax error`,
/// because that build is not compiled with `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`
/// and has no grammar for the clause. But the builds applications actually link
/// often are - Apple's is - and `DELETE FROM t WHERE ... LIMIT 1000` in a loop
/// is the ordinary way to trim a large table without one large transaction. A
/// consumer probing 0.1.8 against the macOS `sqlite3` reported the refusal as a
/// real gap, which it was.
///
/// What remains is the one rule a build compiled with the option enforces:
/// an order with nothing to limit is refused, in SQLite's own words, because
/// sorting the rows a statement changes all of changes nothing.
///
/// @param limited - which of the two words came first and where, from the parser
/// @param limit - the statement's `LIMIT`, when it wrote one
/// @param statement - `DELETE` or `UPDATE`, for the message
fn order_without_limit(
    limited: Option<(ast::Limited, Span)>,
    limit: Option<ast::ExprId>,
    statement: &str,
) -> Option<ParseError> {
    let (word, span) = limited?;
    if word != ast::Limited::OrderBy || limit.is_some() {
        return None;
    }
    Some(refused(
        format!("ORDER BY without LIMIT on {statement}"),
        span,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_view::{
        ColumnInfo, IndexColumnInfo, IndexInfo, IndexOrigin, TableInfo, TableKind,
    };
    use inillucent_value::Affinity;

    /// Returns one plain column.
    ///
    /// @param name - the column's name
    fn a_column(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.as_bytes().to_vec(),
            folded: name.to_ascii_lowercase().into_bytes(),
            declared_type: b"INTEGER".to_vec(),
            affinity: Affinity::Integer,
            collation: b"binary".to_vec(),
            not_null: false,
            not_null_conflict: None,
            primary_key_conflict: None,
            default_sql: None,
            primary_key_position: None,
            hidden: false,
            generated: false,
            stored: false,
            generated_sql: None,
        }
    }

    /// Returns a rowid table with the columns named.
    ///
    /// @param name - the table's name
    /// @param columns - the column names, in declaration order
    fn a_table(name: &str, columns: &[&str]) -> TableInfo {
        TableInfo {
            name: name.as_bytes().to_vec(),
            folded: name.to_ascii_lowercase().into_bytes(),
            database: 0,
            root: 2,
            columns: columns.iter().map(|held| a_column(held)).collect(),
            rowid_alias: None,
            without_rowid: false,
            strict: false,
            autoincrement: false,
            kind: TableKind::Table,
            create_sql: Vec::new(),
            indexes: Vec::new(),
            view: None,
            triggers: Vec::new(),
            analysed_rows: None,
            foreign_key_triggers: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            module: None,
        }
    }

    /// Returns an index over the table columns named.
    ///
    /// @param name - the index's name
    /// @param root - its own tree, or the table's for a `WITHOUT ROWID` key
    /// @param columns - the table columns it keys on
    fn an_index(name: &str, root: u32, columns: &[u16]) -> IndexInfo {
        IndexInfo {
            name: name.as_bytes().to_vec(),
            folded: name.to_ascii_lowercase().into_bytes(),
            root,
            unique: true,
            columns: columns
                .iter()
                .map(|held| IndexColumnInfo {
                    column: Some(*held),
                    expr_sql: None,
                    collation: b"binary".to_vec(),
                    descending: false,
                    declared_descending: false,
                })
                .collect(),
            partial_sql: None,
            origin: IndexOrigin::Unique,
            conflict: None,
            prefix_rows: Vec::new(),
            analysed_rows: None,
            metric: None,
        }
    }

    /// The three spellings of the rowid are the three SQLite accepts.
    ///
    /// **A fourth would be a column name a table could not have (T3,
    /// task-1962).** `rowid`, `oid` and `_rowid_` all name the hidden key, and
    /// a table that declares a column called any of them shadows it - so the
    /// list decides which names a `SELECT rowid` can mean.
    #[test]
    fn the_rowid_has_three_names() {
        assert!(is_rowid_name(b"rowid"));
        assert!(is_rowid_name(b"oid"));
        assert!(is_rowid_name(b"_rowid_"));
        assert!(!is_rowid_name(b"row_id"));
        assert!(!is_rowid_name(b"id"));
        assert!(
            !is_rowid_name(b"ROWID"),
            "the argument is already folded, so an unfolded name is not one this asks about"
        );
    }

    /// A unique violation names every column of the index, table-qualified.
    ///
    /// **The message is what an application matches on.** SQLite's wording is
    /// `UNIQUE constraint failed: t.a, t.b`, and a library that switched on it
    /// would stop recognising a collision if the columns were listed any other
    /// way.
    #[test]
    fn a_unique_violation_names_every_column_of_the_index() {
        let table = a_table("t", &["a", "b", "c"]);
        let one = an_index("by_a", 3, &[0]);
        assert_eq!(
            unique_message(&table, &one),
            "UNIQUE constraint failed: t.a"
        );
        let two = an_index("by_a_b", 4, &[0, 1]);
        assert_eq!(
            unique_message(&table, &two),
            "UNIQUE constraint failed: t.a, t.b",
            "both columns, in key order, separated the way the reference separates them"
        );
    }

    /// A rowid collision names the aliasing column when there is one, and the
    /// hidden `rowid` when there is not.
    ///
    /// The extended code differs with it: `SQLITE_CONSTRAINT_PRIMARYKEY` for an
    /// `INTEGER PRIMARY KEY` and `SQLITE_CONSTRAINT_ROWID` for the hidden one.
    #[test]
    fn a_rowid_collision_names_the_column_that_aliases_it() {
        let hidden = a_table("t", &["a"]);
        assert_eq!(
            rowid_message(&hidden),
            (
                codes::ROWID,
                "UNIQUE constraint failed: t.rowid".to_string()
            )
        );
        let mut aliased = a_table("t", &["id", "a"]);
        aliased.rowid_alias = Some(0);
        assert_eq!(
            rowid_message(&aliased),
            (
                codes::PRIMARY_KEY,
                "UNIQUE constraint failed: t.id".to_string()
            )
        );
    }

    /// A `WITHOUT ROWID` table has no rowid to name, so it names its key.
    ///
    /// **It used to answer `t.rowid`, naming a column the table does not
    /// have.** Its own key *is* its primary key, held in the one index whose
    /// root is the table's.
    #[test]
    fn a_without_rowid_collision_names_the_primary_key() {
        let mut table = a_table("t", &["a", "b"]);
        table.without_rowid = true;
        table.indexes = vec![an_index("sqlite_autoindex_t_1", table.root, &[0, 1])];
        assert_eq!(
            rowid_message(&table),
            (
                codes::PRIMARY_KEY,
                "UNIQUE constraint failed: t.a, t.b".to_string()
            )
        );
    }

    /// A constraint's own `ON CONFLICT REPLACE` makes a statement able to
    /// replace, with no `OR REPLACE` written anywhere.
    #[test]
    fn a_constraint_can_make_a_plain_insert_replace() {
        let plain = a_table("t", &["a"]);
        assert!(!can_replace(&plain, None));
        assert!(can_replace(&plain, Some(ConflictAction::Replace)));

        let mut on_the_index = a_table("t", &["a"]);
        let mut index = an_index("by_a", 3, &[0]);
        index.conflict = Some(ConflictAction::Replace);
        on_the_index.indexes = vec![index];
        assert!(
            can_replace(&on_the_index, None),
            "`a UNIQUE ON CONFLICT REPLACE` replaces without the statement saying so"
        );

        let mut on_the_column = a_table("t", &["a"]);
        if let Some(column) = on_the_column.columns.first_mut() {
            column.not_null_conflict = Some(ConflictAction::Replace);
        }
        assert!(can_replace(&on_the_column, None));
    }
}
