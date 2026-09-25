//! What the binder is allowed to know about a schema.
//!
//! Invariant: this is a read-only view over an immutable snapshot. Nothing here
//! can open a page, and nothing here changes while a statement is being bound,
//! so a bound statement is a pure function of its SQL and one generation of one
//! catalog. That is what makes prepared-statement invalidation a comparison of
//! two numbers rather than a re-derivation.
//!
//! The types are defined here, below the catalog that fills them in, so the
//! binder can be compiled and tested against a hand-built schema with no file
//! anywhere near it.

use crate::ast::{ConflictAction, ReferentialAction};
use inillucent_value::Affinity;

/// Where an index came from, which decides whether it can be dropped and how
/// it is named in `sqlite_schema`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexOrigin {
    /// `CREATE INDEX`.
    Created,
    /// A `UNIQUE` constraint.
    Unique,
    /// A `PRIMARY KEY` constraint on a rowid table.
    PrimaryKey,
    /// An index a module owns, named by `CREATE INDEX ... USING <module>`.
    ///
    /// **Not a b-tree, and the planner has to know that.** Its rows live in a
    /// virtual table, its `root` is that table's own root, and none of the
    /// b-tree paths apply to it - there is nothing to seek and nothing to
    /// range-scan. What it can do is answer "the k nearest to this vector",
    /// which is a whole access path of its own.
    Module,
}

/// The distance a `Module`-origin vector index was declared to minimise.
///
/// **Only a vector index has one of these, and only a real one.** An ordinary
/// b-tree orders by a collation, not a distance, so every `IndexInfo` that is
/// not `IndexOrigin::Module` carries `None`. A `Module` index carries `None`
/// too unless its own module is one the planner has verified actually honours
/// the setting: `inillucent-engine/src/vectors.rs` only ever reports `Some`
/// for `inillucent_search` (which backs `USING inillucent_hnsw`), because that
/// is the one module whose store was changed to read the graph under this
/// metric. An `ivfflat` index that was declared `WITH (metric = 'l2')` still
/// reads back as `None` here, deliberately: `ivfflat`'s own argument parser
/// silently accepts and ignores a key it does not recognise, so trusting the
/// text would let the planner believe an index orders by Euclidean distance
/// when the structure behind it still computes cosine - the exact "wrong
/// answer that looks like a working index" this field exists to prevent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexMetric {
    /// One minus the cosine similarity of two unit vectors.
    Cosine,
    /// Euclidean distance.
    L2,
}

/// One column of a table or view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    /// The name as declared.
    pub name: Vec<u8>,
    /// The ASCII-folded lookup key.
    pub folded: Vec<u8>,
    /// The declared type, exactly as written, empty when none was given.
    pub declared_type: Vec<u8>,
    /// The affinity derived from the declared type.
    pub affinity: Affinity,
    /// The folded name of the column's declared collation.
    pub collation: Vec<u8>,
    /// Whether the column is `NOT NULL`.
    pub not_null: bool,
    /// The `ON CONFLICT` clause written on the `NOT NULL`, when there was one.
    ///
    /// A constraint carries its own algorithm and the statement may override
    /// it: `INSERT OR IGNORE` beats `NOT NULL ON CONFLICT ABORT`. Recording it
    /// per constraint rather than per table is what makes that override a
    /// choice between two known values instead of a guess.
    pub not_null_conflict: Option<ConflictAction>,
    /// The `ON CONFLICT` clause written on the column's `PRIMARY KEY`.
    ///
    /// **A different constraint from the `NOT NULL`, and a different clause.**
    /// For a rowid alias this is the only place a rowid collision's algorithm
    /// is written down - SQLite records `id INTEGER PRIMARY KEY ON CONFLICT
    /// REPLACE` against the column, because the alias *is* the column and there
    /// is no index to hang it on. Every other primary key gets an `IndexInfo`
    /// and carries it there.
    ///
    /// Reading `not_null_conflict` for it, which is what the write path used to
    /// do, answers a question about a constraint the table may not
    /// even declare.
    pub primary_key_conflict: Option<ConflictAction>,
    /// The `DEFAULT` expression, as written.
    pub default_sql: Option<Vec<u8>>,
    /// The one-based position in the primary key, when it is in one.
    pub primary_key_position: Option<u16>,
    /// Whether the column is hidden from `SELECT *`.
    pub hidden: bool,
    /// Whether the column is generated.
    pub generated: bool,
    /// Whether a generated column's value is stored in the record.
    ///
    /// A `VIRTUAL` column occupies no slot and is computed on every read; a
    /// `STORED` one occupies a slot like any other column. The distinction is
    /// not cosmetic: it changes which *record position* every column after it
    /// lives at, so a reader that ignored it would read the wrong column.
    pub stored: bool,
    /// The generating expression, as the source text it was written as.
    pub generated_sql: Option<Vec<u8>>,
}

/// One key column of an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexColumnInfo {
    /// The table column this key indexes, when it indexes a bare column.
    pub column: Option<u16>,
    /// The key expression, as written, when the key is an expression.
    pub expr_sql: Option<Vec<u8>>,
    /// The folded collation name the key is ordered by.
    pub collation: Vec<u8>,
    /// Whether the key is stored descending.
    pub descending: bool,
    /// Whether the *declaration* said descending, whatever the storage does.
    ///
    /// **A different question from `descending`, and the two used to be one.**
    /// This engine's trees are always built ascending, so the catalog flattens
    /// `descending` to false for the planner's sake - a planner told about a
    /// descending tree that does not exist draws three inverted conclusions
    /// (see `inillucent-catalog`'s `stored_ascending`). But
    /// `PRAGMA index_xinfo` reports what was *declared*, and an application
    /// reading it to reconstruct a `CREATE INDEX` needs the `DESC` back.
    pub declared_descending: bool,
}

/// An index over a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    /// The index name.
    pub name: Vec<u8>,
    /// The ASCII-folded lookup key.
    pub folded: Vec<u8>,
    /// The root page of the index B-tree.
    pub root: u32,
    /// Whether the index enforces uniqueness.
    pub unique: bool,
    /// The key columns, in order.
    pub columns: Vec<IndexColumnInfo>,
    /// The partial-index predicate, as written.
    pub partial_sql: Option<Vec<u8>>,
    /// Where the index came from.
    pub origin: IndexOrigin,
    /// The `ON CONFLICT` clause the constraint that created it carried.
    pub conflict: Option<ConflictAction>,
    /// For each leading prefix of the key, the average number of rows sharing
    /// it, as `ANALYZE` measured.
    ///
    /// Empty until the schema has been analysed, which is the *usual* state and
    /// not an error: the planner falls back to SQLite's own guesses, and those
    /// guesses are what make an unanalysed plan match the reference's.
    pub prefix_rows: Vec<i64>,
    /// How many entries the index itself holds, as `ANALYZE` measured.
    ///
    /// **The same number as the table's row count for an ordinary index, and a
    /// different one for a partial index**, which holds only
    /// the rows its predicate accepted. It is what lets the planner price
    /// reading the whole of such an index against scanning the table it is on -
    /// 120 entries against 6,000 rows, in the case this was found on.
    ///
    /// `None` until the schema has been analysed.
    pub analysed_rows: Option<i64>,
    /// The distance a vector index minimises, when it is one the planner may
    /// trust to answer for it. See [`IndexMetric`].
    pub metric: Option<IndexMetric>,
}

/// What kind of schema object a name resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableKind {
    /// An ordinary table.
    Table,
    /// A view.
    View,
    /// A virtual table.
    Virtual,
    /// A nested query standing in for a table: a FROM subquery, a CTE
    /// reference, or an expanded view.
    ///
    /// It is a kind rather than a flag because every question the binder asks
    /// of a table - has it a rowid, can it be written to, may an index be used
    /// on it - has the same answer for all three, and a kind makes the answer
    /// one match arm instead of three conditions that can drift apart.
    Subquery,
}

/// A view's parsed definition.
///
/// The arena lives here, in the catalog snapshot, rather than being re-parsed
/// on every reference. That is not only a saving: the binder holds the snapshot
/// for the whole statement, so a body kept here outlives the bind and can be
/// bound in place, while one parsed inside the binder would be a local whose
/// borrow ends before the bound tree does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewBody {
    /// The arena the view's `SELECT` was parsed into.
    pub ast: crate::ast::Ast,
    /// The `SELECT` inside the arena.
    pub select: crate::ast::SelectId,
    /// The explicit column list, when the `CREATE VIEW` wrote one.
    pub columns: Vec<Vec<u8>>,
}

/// What a trigger fires on, with `UPDATE OF` already folded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerEventInfo {
    /// `INSERT`.
    Insert,
    /// `DELETE`.
    Delete,
    /// `UPDATE`, optionally narrowed to a set of folded column names.
    Update(Vec<Vec<u8>>),
}

/// A trigger's parsed definition.
///
/// Kept parsed here for the same reason a view body is: the arena belongs to
/// the catalog snapshot, which the binder holds for the whole statement, so a
/// body can be bound in place. A body re-parsed inside the binder would be a
/// local whose borrow ends before the bound tree does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerInfo {
    /// The trigger name as declared.
    pub name: Vec<u8>,
    /// The ASCII-folded lookup key.
    pub folded: Vec<u8>,
    /// When it fires. `CREATE TRIGGER` with no time written means `BEFORE`.
    pub time: crate::ast::TriggerTime,
    /// What it fires on.
    pub event: TriggerEventInfo,
    /// The arena the `WHEN` guard and the body were parsed into.
    pub ast: crate::ast::Ast,
    /// The `WHEN` guard, when one was written.
    pub when: Option<crate::ast::ExprId>,
    /// The body statements, in written order.
    pub body: Vec<crate::ast::Statement>,
}

impl ColumnInfo {
    /// Reports whether the column was declared a vector at all.
    ///
    /// `VECTOR(768)` and a bare `VECTOR` both answer true, where
    /// [`ColumnInfo::vector_dimensions`] answers a width only for the first.
    /// The difference matters to the operators: a bare `VECTOR` cannot be
    /// indexed, but adding two of them is just as meaningless.
    pub fn is_vector(&self) -> bool {
        let declared = self.declared_type.to_ascii_lowercase();
        let Some(rest) = declared.strip_prefix(b"vector".as_slice()) else {
            return false;
        };
        rest.is_empty()
            || rest
                .first()
                .is_some_and(|byte| !byte.is_ascii_alphanumeric())
    }

    /// Returns how many dimensions a `VECTOR(N)` column declares.
    ///
    /// **Read out of the declared type rather than stored beside it**, because
    /// every path that builds a `ColumnInfo` - the catalog loader, a module's
    /// declaration, the binder's synthetic ones - would otherwise have to know
    /// about vectors, and a column's declared type is the one place SQLite
    /// itself keeps what a column was called.
    ///
    /// `VECTOR(768)` and `vector( 768 )` both answer 768. A bare `VECTOR`
    /// answers `None`, which means "a vector of whatever arrives" and is what a
    /// table holding two models' embeddings needs; anything that is not a
    /// vector answers `None` too, and its caller then checks nothing.
    ///
    /// **The affinity is deliberately left alone.** SQLite gives `VECTOR(768)`
    /// NUMERIC affinity, and NUMERIC leaves a blob exactly as it arrived - so
    /// the bytes round-trip without this engine having to disagree with the
    /// reference about what an affinity is. What the declaration buys is the
    /// width check on write, and a column an index can be built over.
    pub fn vector_dimensions(&self) -> Option<usize> {
        let declared = self.declared_type.to_ascii_lowercase();
        let rest = declared.strip_prefix(b"vector".as_slice())?;
        let inside: Vec<u8> = rest
            .iter()
            .copied()
            .skip_while(|byte| byte.is_ascii_whitespace())
            .collect();
        let inside = inside.strip_prefix(b"(".as_slice())?;
        let inside = inside.strip_suffix(b")".as_slice())?;
        let text = std::str::from_utf8(inside).ok()?.trim();
        let width: usize = text.parse().ok()?;
        (width > 0).then_some(width)
    }
}

impl TriggerInfo {
    /// Returns whether this trigger fires for one event on one column set.
    ///
    /// `changed` is the folded names an UPDATE assigns, and is empty for the
    /// other two events. `UPDATE OF a, b` fires only when the statement writes
    /// `a` or `b` - which SQLite decides from the *statement*, not from whether
    /// the value actually differs.
    pub fn fires_for(&self, event: &TriggerEventInfo, changed: &[Vec<u8>]) -> bool {
        match (&self.event, event) {
            (TriggerEventInfo::Insert, TriggerEventInfo::Insert) => true,
            (TriggerEventInfo::Delete, TriggerEventInfo::Delete) => true,
            (TriggerEventInfo::Update(of), TriggerEventInfo::Update(_)) => {
                of.is_empty() || of.iter().any(|name| changed.contains(name))
            }
            _ => false,
        }
    }
}

/// A table, view or virtual table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    /// The name as declared.
    pub name: Vec<u8>,
    /// The ASCII-folded lookup key.
    pub folded: Vec<u8>,
    /// Which attached database it belongs to.
    pub database: usize,
    /// The root page of the table B-tree, or zero for a view.
    pub root: u32,
    /// The columns, in declaration order.
    pub columns: Vec<ColumnInfo>,
    /// The column that is an alias for the rowid, when there is one.
    pub rowid_alias: Option<u16>,
    /// Whether the table is `WITHOUT ROWID`.
    pub without_rowid: bool,
    /// Whether the table is `STRICT`.
    pub strict: bool,
    /// Whether the rowid alias was declared `AUTOINCREMENT`.
    ///
    /// It changes where a new rowid comes from: an ordinary table reuses the
    /// numbers its deleted rows had, and an `AUTOINCREMENT` one never does,
    /// because it remembers the largest it has ever handed out in
    /// `sqlite_sequence`.
    pub autoincrement: bool,
    /// What kind of object this is.
    pub kind: TableKind,
    /// The `CREATE` text as stored in `sqlite_schema`.
    pub create_sql: Vec<u8>,
    /// The indexes over this table.
    pub indexes: Vec<IndexInfo>,
    /// The parsed body, when this is a view.
    pub view: Option<Box<ViewBody>>,
    /// The triggers attached to this table or view, in schema order.
    pub triggers: Vec<TriggerInfo>,
    /// How many rows `ANALYZE` counted, when it has run.
    pub analysed_rows: Option<i64>,
    /// The triggers this table's writes fire because of a foreign key.
    ///
    /// Both directions are here, because both are things that happen when
    /// *this* table is written: the checks its own keys need when a row
    /// arrives, and the actions the keys pointing at it need when a row
    /// leaves. They are built once when the schema is read rather than once
    /// per statement, because generating and parsing them is the same work
    /// every time and the schema is what decides them.
    pub foreign_key_triggers: Vec<ForeignKeyTrigger>,
    /// Every foreign key declared on this table, in declaration order.
    ///
    /// The child's side of the relationship, which is the side the table
    /// carries. Finding the keys that point *at* a table means walking the
    /// database's tables and asking each one, which is what
    /// `CatalogView::foreign_keys_referencing` does - and is what SQLite does
    /// too, because nothing in the file records the reverse direction.
    pub foreign_keys: Vec<ForeignKeyInfo>,
    /// Every `CHECK` constraint, as the source text it was written as.
    ///
    /// The text rather than a bound expression, for the same reason
    /// `default_sql` is text: the catalog is below the binder, so it cannot
    /// bind anything, and a constraint that had been half-interpreted on the
    /// way through would be a second source of truth beside the `CREATE`
    /// statement the file actually stores.
    pub checks: Vec<CheckInfo>,
    /// The module a virtual table is implemented by, and its arguments.
    ///
    /// The catalog records the question and the session fills in the answer:
    /// what columns the table has is the module's to say, not the file's, so a
    /// virtual table arrives here with a module and no columns and leaves the
    /// connection's schema load with both.
    pub module: Option<crate::vtab::ModuleRef>,
}

/// One trigger a foreign key implies, or the reason there is not one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeyTrigger {
    /// Whether it refuses a write rather than repairing one.
    ///
    /// Only a check can be deferred. An action is what the constraint *does*,
    /// and doing it at commit time instead would leave the rows in between
    /// visible to the statements that come after.
    pub is_check: bool,
    /// Whether the key it enforces was declared `INITIALLY DEFERRED`.
    pub deferred: bool,
    /// The trigger, or `None` when the key cannot be enforced at all.
    pub trigger: Option<TriggerInfo>,
    /// Why it cannot be, when it cannot.
    ///
    /// A key whose parent table is missing, or whose parent columns are not a
    /// key of the parent, is legal to declare: SQLite reports it when
    /// something writes, not when the schema is read, so that a schema can be
    /// loaded in any order. The message is kept here and reported then.
    pub fault: Vec<u8>,
    /// Whether the key's child table and its parent table are the same table.
    ///
    /// **Read by `DROP TABLE`'s implicit delete (task-1979, F6).** That delete
    /// removes every row of one table, so a key whose child is that same table
    /// cannot be violated once the statement has finished - the rows that would
    /// be left pointing at nothing are themselves gone. SQLite reaches the same
    /// answer a different way: its immediate foreign keys are a counter checked
    /// at the end of the statement, so the violation deleting the first row
    /// creates is cancelled by deleting the row that made it.
    pub self_referencing: bool,
}

/// One foreign key, from the child table that declares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    /// The constraint's position in its table, counting from zero.
    ///
    /// `PRAGMA foreign_key_list` reports it, and it is how a diagnostic names
    /// a constraint that was written without a name - which is most of them.
    pub id: u32,
    /// The child columns, in the order they were written.
    pub columns: Vec<u16>,
    /// The parent table's name as written.
    pub parent: Vec<u8>,
    /// The parent table's folded name.
    pub parent_folded: Vec<u8>,
    /// The parent columns as written, or empty when the clause named none.
    ///
    /// Empty means the parent's primary key, and it stays empty rather than
    /// being resolved here: the catalog builds one table at a time and the
    /// parent may not have been read yet - or may not exist, which is legal
    /// until something writes a row.
    pub parent_columns: Vec<Vec<u8>>,
    /// What happens to the child rows when a parent row is deleted.
    pub on_delete: ReferentialAction,
    /// What happens to the child rows when a parent key changes.
    pub on_update: ReferentialAction,
    /// The `MATCH` clause as written, which SQLite parses and ignores.
    pub match_clause: Vec<u8>,
    /// Whether `DEFERRABLE` was written.
    pub deferrable: bool,
    /// Whether `INITIALLY DEFERRED` was written.
    pub initially_deferred: bool,
    /// Whether following this key can lead back to the table that declares it.
    ///
    /// A tree with `ON DELETE CASCADE` on its parent column is the everyday
    /// case, and it is the one case an action cannot simply be inlined into
    /// the statement that fires it: the body would have to appear once per
    /// level the data happens to be deep, which is not known when the
    /// statement is compiled. A cyclic key's action is applied by repeating it
    /// until nothing changes instead, and this is what says which keys need
    /// that.
    pub cyclic: bool,
}

impl ForeignKeyInfo {
    /// Reports whether the constraint's checks wait until the transaction
    /// commits.
    pub fn is_deferred(&self) -> bool {
        self.deferrable && self.initially_deferred
    }
}

/// One `CHECK` constraint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckInfo {
    /// The constraint's name, when one was written.
    pub name: Option<Vec<u8>>,
    /// The predicate, as the source text between its parentheses.
    pub expr_sql: Vec<u8>,
    /// The `ON CONFLICT` clause a table-level `CHECK` was written with.
    ///
    /// **Recorded and not acted on**, because that is what the reference does:
    /// SQLite's grammar accepts `CHECK (expr) onconf` on a table constraint and
    /// its builder never reads the clause, so such a constraint aborts like any
    /// other. It is kept here so the derivation is a full account of the text
    /// rather than a lossy one, and so the next reader finds the measurement
    /// instead of the question.
    pub conflict: Option<ConflictAction>,
}

impl TableInfo {
    /// Returns the position of a column by its folded name.
    pub fn column_position(&self, folded: &[u8]) -> Option<u16> {
        self.columns
            .iter()
            .position(|column| column.folded == folded)
            .map(|index| index as u16)
    }

    /// Returns a column by position.
    pub fn column(&self, position: u16) -> Option<&ColumnInfo> {
        self.columns.get(position as usize)
    }

    /// Returns whether the table has a rowid a query may refer to.
    pub fn has_rowid(&self) -> bool {
        // A virtual table has one unless its module declared otherwise: FTS5
        // and the R-Tree both key their rows by it, and `SELECT rowid FROM t`
        // is how an application joins to them.
        matches!(self.kind, TableKind::Table | TableKind::Virtual) && !self.without_rowid
    }

    /// Returns a table that stands for an eponymous module.
    ///
    /// A module reached as a name rather than through `CREATE VIRTUAL TABLE` -
    /// `generate_series`, `json_each`, `pragma_table_info` - belongs to no
    /// database and has no `sqlite_schema` row, so everything a stored table
    /// carries is absent and only the module's declaration remains.
    ///
    /// @param name - the module's name, which is also the table's
    /// @param columns - the columns the module declared
    /// @param module - the module reference the executor resolves it by
    /// @param without_rowid - whether the module declared no rowid
    pub fn eponymous(
        name: Vec<u8>,
        columns: Vec<ColumnInfo>,
        module: crate::vtab::ModuleRef,
        without_rowid: bool,
    ) -> TableInfo {
        let folded = name.to_ascii_lowercase();
        TableInfo {
            name,
            folded,
            database: 0,
            root: 0,
            columns,
            rowid_alias: None,
            without_rowid,
            strict: false,
            autoincrement: false,
            kind: TableKind::Virtual,
            create_sql: Vec::new(),
            foreign_keys: Vec::new(),
            foreign_key_triggers: Vec::new(),
            module: Some(module),
            view: None,
            triggers: Vec::new(),
            analysed_rows: None,
            indexes: Vec::new(),
            checks: Vec::new(),
        }
    }

    /// Returns a table that stands for a nested query's result.
    ///
    /// The column list is the block's result columns: their names are what a
    /// reference to the subquery resolves against, and their affinity and
    /// collation are the ones the expressions behind them carry, so a
    /// comparison against a subquery column applies the same rules it would
    /// have applied one level down.
    pub fn subquery(name: Vec<u8>, database: usize, columns: Vec<ColumnInfo>) -> TableInfo {
        let folded = name.to_ascii_lowercase();
        TableInfo {
            name,
            folded,
            database,
            root: 0,
            columns,
            rowid_alias: None,
            without_rowid: true,
            strict: false,
            autoincrement: false,
            kind: TableKind::Subquery,
            create_sql: Vec::new(),
            foreign_keys: Vec::new(),
            foreign_key_triggers: Vec::new(),
            module: None,
            view: None,
            triggers: Vec::new(),
            analysed_rows: None,
            indexes: Vec::new(),
            checks: Vec::new(),
        }
    }

    /// Returns the record slot a column's value lives in, when it has one.
    ///
    /// `VIRTUAL` generated columns take no slot, so the slots of the columns
    /// after them shift down. Every read of a stored column has to go through
    /// this rather than through the column's declared position, and a `VIRTUAL`
    /// column has no slot at all - it is computed.
    pub fn record_slot(&self, column: u16) -> Option<usize> {
        if self.without_rowid {
            return self
                .record_order()
                .iter()
                .position(|stored| *stored == column);
        }
        let mut slot = 0usize;
        for (position, info) in self.columns.iter().enumerate() {
            if info.generated && !info.stored {
                if position == usize::from(column) {
                    return None;
                }
                continue;
            }
            if position == usize::from(column) {
                return Some(slot);
            }
            slot = slot.saturating_add(1);
        }
        None
    }

    /// Returns the primary key's columns, in key order.
    ///
    /// Key order, not declaration order: `PRIMARY KEY(b, a)` is ordered by `b`
    /// and then `a` however the columns were declared, and for a `WITHOUT
    /// ROWID` table that order also decides where in the record they sit.
    pub fn primary_key(&self) -> Vec<u16> {
        let mut keys: Vec<(u16, u16)> = self
            .columns
            .iter()
            .enumerate()
            .filter_map(|(position, column)| {
                column
                    .primary_key_position
                    .map(|key| (key, position as u16))
            })
            .collect();
        keys.sort_by_key(|(key, _)| *key);
        keys.into_iter().map(|(_, position)| position).collect()
    }

    /// Returns the columns a record holds, in the order it holds them.
    ///
    /// A rowid table stores its columns as declared. A `WITHOUT ROWID` table's
    /// B-tree is an index whose key is the primary key, so its record is the
    /// key columns first, in key order, and then everything else as declared -
    /// verified against a file the pinned build wrote: `PRIMARY KEY(b, a)` over
    /// `(a, b, c)` stores `(b, a, c)`.
    pub fn record_order(&self) -> Vec<u16> {
        let stored = |position: usize| {
            self.columns
                .get(position)
                .is_some_and(|column| !column.generated || column.stored)
        };
        if !self.without_rowid {
            return (0..self.columns.len())
                .filter(|position| stored(*position))
                .map(|position| position as u16)
                .collect();
        }
        let keys = self.primary_key();
        let mut order = keys.clone();
        for position in 0..self.columns.len() {
            if keys.contains(&(position as u16)) || !stored(position) {
                continue;
            }
            order.push(position as u16);
        }
        order
    }

    /// Returns whether a name is one of the rowid's three spellings and is not
    /// shadowed by a real column.
    ///
    /// SQLite's rule is exactly this: `rowid`, `_rowid_` and `oid` name the
    /// rowid *unless* the table declares a column with that name, in which case
    /// the column wins. A table without a rowid has none of the three.
    pub fn is_rowid_name(&self, folded: &[u8]) -> bool {
        if !self.has_rowid() {
            return false;
        }
        let spelled = folded == b"rowid" || folded == b"_rowid_" || folded == b"oid";
        spelled && self.column_position(folded).is_none()
    }
}

/// The read-only schema the binder resolves names against.
pub trait CatalogView {
    /// Returns the number of attached databases.
    fn database_count(&self) -> usize;

    /// Returns the name of an attached database by index.
    fn database_name(&self, index: usize) -> &[u8];

    /// Returns the index of an attached database by folded name.
    fn database_index(&self, folded: &[u8]) -> Option<usize>;

    /// Returns a table, view or virtual table by name.
    ///
    /// With no qualifier the search follows SQLite's order: `temp`, then
    /// `main`, then every other attached database in attachment order.
    fn find_table(&self, database: Option<&[u8]>, folded: &[u8]) -> Option<&TableInfo>;

    /// Returns a table as a shared pointer, for a caller that has to keep it.
    ///
    /// A binder keeps what it finds for the life of the bound statement.
    /// [`CatalogView::find_table`] hands back a borrow, so keeping it meant
    /// cloning a `TableInfo` - two name vectors, a `ColumnInfo` per column with
    /// its own heap fields, the `CREATE` text and an `IndexInfo` per index -
    /// for every table reference in every statement. Measured at 2,938 ns of
    /// `prepare.point`'s 6,093 ns compile.
    ///
    /// The default is that clone, so an implementor that has nothing to share
    /// keeps working and is merely no faster. `StaticCatalog` shares.
    ///
    /// @param database - the schema qualifier, if the statement wrote one
    /// @param folded - the table's folded name
    fn shared_table(
        &self,
        database: Option<&[u8]>,
        folded: &[u8],
    ) -> Option<std::rc::Rc<TableInfo>> {
        self.find_table(database, folded)
            .map(|table| std::rc::Rc::new(table.clone()))
    }

    /// Returns the table an index belongs to, together with the index.
    ///
    /// Index names live in the same namespace as table names in SQLite, but
    /// the catalog stores an index inside the table it indexes - which is
    /// where every reader of one wants it. `DROP INDEX` is the caller that
    /// has only the name, so the search lives here rather than being written
    /// out again wherever a name has to be resolved.
    fn find_index(
        &self,
        database: Option<&[u8]>,
        folded: &[u8],
    ) -> Option<(&TableInfo, &IndexInfo)>;

    /// Returns the table a trigger is attached to, together with the trigger.
    ///
    /// Triggers share the name namespace with tables and indexes and are stored
    /// on the object they fire for, so this is `find_index` again for the other
    /// kind of attached object: `DROP TRIGGER` and `CREATE TRIGGER` both have
    /// only the name.
    fn find_trigger(
        &self,
        database: Option<&[u8]>,
        folded: &[u8],
    ) -> Option<(&TableInfo, &TriggerInfo)> {
        let wanted = database.and_then(|name| self.database_index(name));
        for table in self.every_table() {
            if wanted.is_some_and(|index| index != table.database) {
                continue;
            }
            if let Some(trigger) = table.triggers.iter().find(|one| one.folded == folded) {
                return Some((table, trigger));
            }
        }
        None
    }

    /// Returns every table of every attached database.
    ///
    /// It exists so [`CatalogView::find_trigger`] can have one implementation
    /// rather than one per catalog: a trigger search is the same walk whatever
    /// the tables are stored in.
    fn every_table(&self) -> Vec<&TableInfo>;

    /// Returns every table of one attached database, in no particular order.
    fn tables_of(&self, database: usize) -> Vec<&TableInfo>;

    /// Returns the schema cookie of an attached database, which a prepared
    /// statement records so it can tell whether the schema moved under it.
    fn schema_cookie(&self, database: usize) -> u32;

    /// Returns the generation of the whole snapshot.
    fn generation(&self) -> u64;
}

/// A catalog held in memory, which is what a test binds against and what the
/// loader produces once it has read `sqlite_schema`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StaticCatalog {
    /// The attached databases, in attachment order, with their cookies.
    pub databases: Vec<(Vec<u8>, u32)>,
    /// Every table, in no particular order.
    ///
    /// **Shared rather than owned, because binding a statement used to clone
    /// one.** `BoundSource.table` was a `TableInfo` by value, so every table
    /// reference in every statement deep-copied the catalog's entry: two name
    /// vectors, a `ColumnInfo` per column each with its own heap fields, the
    /// full `CREATE` text, and an `IndexInfo` per index with its own column
    /// vector. Forty-odd allocations to bind one `WHERE id = ?1`, measured at
    /// 2,938 ns of `prepare.point`'s 6,093 - 48% of the statement's whole
    /// compile. An `Rc` makes it a refcount bump.
    pub tables: Vec<std::rc::Rc<TableInfo>>,
    /// The eponymous virtual tables the connection's modules provide.
    ///
    /// `generate_series`, `json_each`, `json_tree`, `pragma_table_info`: the
    /// name *is* the table, so they belong to no database and have no
    /// `sqlite_schema` row. They are searched **last**, so a real table called
    /// `generate_series` shadows the module rather than the other way round -
    /// which is SQLite's order and the only safe one, because the file was
    /// there first.
    ///
    /// Filled by the engine from its module registry on every catalog refresh.
    /// Nothing used to fill it, and the eponymous form did not exist:
    /// `FROM generate_series(1,10)` was `no such table`, which also left
    /// `json_each` unreachable from SQL by any route, because `JsonWalkModule`
    /// refuses `CREATE VIRTUAL TABLE` outright.
    pub eponymous: Vec<std::rc::Rc<TableInfo>>,
    /// The generation of this snapshot.
    pub generation: u64,
}

impl StaticCatalog {
    /// Returns a catalog with one `main` database and no objects.
    pub fn empty() -> StaticCatalog {
        StaticCatalog {
            databases: vec![(b"main".to_vec(), 0)],
            tables: Vec::new(),
            eponymous: Vec::new(),
            generation: 0,
        }
    }

    /// Adds an eponymous virtual table, returning the catalog.
    ///
    /// @param table - the module's table, as its declaration describes it
    pub fn with_eponymous(mut self, table: TableInfo) -> StaticCatalog {
        self.eponymous.push(std::rc::Rc::new(table));
        self
    }

    /// Adds a table, returning the catalog, for building fixtures.
    /// Returns one table by folded name, searching every database.
    ///
    /// Attachment order, `main` first, which is the order an unqualified name
    /// resolves in. A module asking about a name it was given as an argument
    /// wants the same table the statement that named it would have found.
    ///
    /// @param folded - the table's ASCII-folded name
    pub fn table_named(&self, folded: &[u8]) -> Option<&TableInfo> {
        self.tables
            .iter()
            .map(std::rc::Rc::as_ref)
            .find(|table| table.folded == folded)
    }

    /// Returns this catalog with one more table in it.
    ///
    /// @param table - the table to add
    pub fn with_table(mut self, table: TableInfo) -> StaticCatalog {
        self.tables.push(std::rc::Rc::new(table));
        self
    }
}

/// Returns the name a schema qualified table name is looked up under.
///
/// **`temp.sqlite_schema` and `temp.sqlite_master` are the temporary
/// catalog**, as SQLite answers them. The temporary catalog is registered only
/// as `sqlite_temp_schema` and `sqlite_temp_master`, because an unqualified
/// `sqlite_schema` searches `temp` first and has to mean `main`'s. A qualified
/// name has no search, so it is mapped here, and after `CREATE TEMP TABLE
/// scratch (x)` all four names answer `scratch`.
///
/// @param database - the qualifier the statement wrote
/// @param folded - the table's folded name
fn qualified_catalog_name<'a>(database: &[u8], folded: &'a [u8]) -> &'a [u8] {
    if !database.eq_ignore_ascii_case(b"temp") {
        return folded;
    }
    match folded {
        b"sqlite_schema" => b"sqlite_temp_schema",
        b"sqlite_master" => b"sqlite_temp_master",
        other => other,
    }
}

impl CatalogView for StaticCatalog {
    /// Returns a table as a shared pointer; the trait method's override.
    ///
    /// @param database - the schema qualifier, if the statement wrote one
    /// @param folded - the table's folded name
    fn shared_table(
        &self,
        database: Option<&[u8]>,
        folded: &[u8],
    ) -> Option<std::rc::Rc<TableInfo>> {
        if let Some(database) = database {
            let index = self.database_index(database)?;
            let folded = qualified_catalog_name(database, folded);
            return self
                .tables
                .iter()
                .find(|table| table.database == index && table.folded == folded)
                .map(std::rc::Rc::clone);
        }
        for index in self.search_order() {
            if let Some(found) = self
                .tables
                .iter()
                .find(|table| table.database == index && table.folded == folded)
            {
                return Some(std::rc::Rc::clone(found));
            }
        }
        self.eponymous
            .iter()
            .find(|table| table.folded == folded)
            .map(std::rc::Rc::clone)
    }

    /// Returns the number of attached databases.
    fn database_count(&self) -> usize {
        self.databases.len()
    }

    /// Returns the name of an attached database by index.
    fn database_name(&self, index: usize) -> &[u8] {
        self.databases.get(index).map_or(&[], |(name, _)| name)
    }

    /// Returns the index of an attached database by folded name.
    fn database_index(&self, folded: &[u8]) -> Option<usize> {
        self.databases
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(folded))
    }

    /// Returns a table by name, searching in SQLite's own order.
    fn find_table(&self, database: Option<&[u8]>, folded: &[u8]) -> Option<&TableInfo> {
        if let Some(database) = database {
            let index = self.database_index(database)?;
            let folded = qualified_catalog_name(database, folded);
            return self
                .tables
                .iter()
                .find(|table| table.database == index && table.folded == folded)
                .map(std::rc::Rc::as_ref);
        }
        for index in self.search_order() {
            if let Some(found) = self
                .tables
                .iter()
                .find(|table| table.database == index && table.folded == folded)
            {
                return Some(found.as_ref());
            }
        }
        // Last, so a real table of the same name shadows the module.
        self.eponymous
            .iter()
            .find(|table| table.folded == folded)
            .map(std::rc::Rc::as_ref)
    }

    /// Returns the table an index belongs to, and the index.
    fn every_table(&self) -> Vec<&TableInfo> {
        self.tables.iter().map(std::rc::Rc::as_ref).collect()
    }

    fn find_index(
        &self,
        database: Option<&[u8]>,
        folded: &[u8],
    ) -> Option<(&TableInfo, &IndexInfo)> {
        let wanted = database.and_then(|name| self.database_index(name));
        for table in &self.tables {
            if wanted.is_some_and(|index| index != table.database) {
                continue;
            }
            if let Some(index) = table.indexes.iter().find(|index| index.folded == folded) {
                return Some((table, index));
            }
        }
        None
    }

    /// Returns every table of one attached database.
    fn tables_of(&self, database: usize) -> Vec<&TableInfo> {
        self.tables
            .iter()
            .filter(|table| table.database == database)
            .map(std::rc::Rc::as_ref)
            .collect()
    }

    /// Returns the schema cookie of an attached database.
    fn schema_cookie(&self, database: usize) -> u32 {
        self.databases
            .get(database)
            .map_or(0, |(_, cookie)| *cookie)
    }

    /// Returns the generation of the snapshot.
    fn generation(&self) -> u64 {
        self.generation
    }
}

impl StaticCatalog {
    /// Returns the database indexes in the order an unqualified name searches.
    fn search_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = Vec::with_capacity(self.databases.len());
        if let Some(temp) = self
            .databases
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(b"temp"))
        {
            order.push(temp);
        }
        for (index, _) in self.databases.iter().enumerate() {
            if !order.contains(&index) {
                order.push(index);
            }
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a one-column table for the tests below.
    fn table(name: &[u8], database: usize) -> TableInfo {
        TableInfo {
            name: name.to_vec(),
            folded: name.to_ascii_lowercase(),
            database,
            root: 2,
            columns: vec![ColumnInfo {
                name: b"a".to_vec(),
                folded: b"a".to_vec(),
                declared_type: Vec::new(),
                affinity: Affinity::Blob,
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
            }],
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
            checks: Vec::new(),
            foreign_keys: Vec::new(),
            foreign_key_triggers: Vec::new(),
            module: None,
        }
    }

    /// An unqualified name finds `temp` before `main`, which is the rule that
    /// lets a temp table shadow a real one.
    #[test]
    fn temp_is_searched_before_main() {
        let catalog = StaticCatalog {
            databases: vec![(b"main".to_vec(), 1), (b"temp".to_vec(), 2)],
            tables: vec![
                std::rc::Rc::new(table(b"t", 0)),
                std::rc::Rc::new(table(b"t", 1)),
            ],
            eponymous: Vec::new(),
            generation: 7,
        };
        let found = catalog.find_table(None, b"t").expect("it resolves");
        assert_eq!(found.database, 1);
        let qualified = catalog
            .find_table(Some(b"main"), b"t")
            .expect("it resolves");
        assert_eq!(qualified.database, 0);
    }

    /// The three rowid spellings resolve, and a real column of that name wins.
    #[test]
    fn the_rowid_spellings_resolve_unless_shadowed() {
        let mut plain = table(b"t", 0);
        assert!(plain.is_rowid_name(b"rowid"));
        assert!(plain.is_rowid_name(b"_rowid_"));
        assert!(plain.is_rowid_name(b"oid"));
        assert!(!plain.is_rowid_name(b"id"));

        if let Some(column) = plain.columns.first_mut() {
            column.name = b"oid".to_vec();
            column.folded = b"oid".to_vec();
        }
        assert!(!plain.is_rowid_name(b"oid"));
        assert!(plain.is_rowid_name(b"rowid"));

        let mut without = table(b"t", 0);
        without.without_rowid = true;
        assert!(!without.is_rowid_name(b"rowid"));
    }

    /// A missing database or table is `None`, never a panic.
    #[test]
    fn a_missing_name_is_none() {
        let catalog = StaticCatalog::empty();
        assert!(catalog.find_table(None, b"nope").is_none());
        assert!(catalog.find_table(Some(b"nodb"), b"t").is_none());
        assert_eq!(catalog.database_name(99), b"");
        assert_eq!(catalog.schema_cookie(99), 0);
    }
}
