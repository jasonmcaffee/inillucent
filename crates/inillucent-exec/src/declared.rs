//! What a table's declarations require of a row on the way in.
//!
//! Invariant: **one place decides what a declaration means.** SQLite applies a
//! column's affinity, then refuses a `NOT NULL` that is null, then refuses a
//! value outside a `STRICT` column's type class, then evaluates the `CHECK`
//! predicates - all against the image that is about to be stored, and in that
//! order. Splitting those across the write path is how three of them ended up
//! collected, bound and never consulted: `TableInfo::checks` was filled in by
//! the catalog and read by nobody, `TableInfo::strict` was validated at DDL
//! time and unenforced on write, and `ColumnInfo::affinity` was derived at load
//! and applied only on the *read* side, so `INSERT INTO t(a INTEGER) VALUES
//! ('42')` stored the text where SQLite stores the integer 42.
//!
//! ## Why it is compiled once per statement
//!
//! Every one of these is a fact about the *catalog*, not about the row: which
//! columns convert, which are typed, and what the predicates are. Deriving them
//! per row would put a string scan and an expression bind on the write path,
//! which is the one path the gate's `write.insert.batch` and `txn.large`
//! measure. [`WriteDeclarations::compile`] does the work once and leaves a
//! statement with nothing to decide but the values.
//!
//! ## Why the affinity list is short
//!
//! A column with BLOB affinity converts nothing, so it is not in the list at
//! all, and a value that already has the class its column wants is skipped
//! before anything is converted. On the gate's fixtures - integers into
//! `INTEGER`, text into `TEXT` - that leaves the conversion loop reading one
//! discriminant per typed column and writing nothing, which is what keeps this
//! off the write families' budget.

use inillucent_base::{DbError, DbResult, ExtendedCode};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::dml::{codes, BoundCheck, BoundDefault, BoundIndexExprs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_value::affinity::{self, Affinity};
use inillucent_value::encoding::TextEncoding;

use crate::dml::RowSpace;
use crate::expr::Eval;
use crate::physical::{Params, SourceLayout, TreeCatalog};

/// The storage class a `STRICT` column's declared type admits.
///
/// `STRICT` is the whole of SQLite's rule and no more: the type names it allows
/// each name one storage class, `ANY` names all of them, and NULL is always
/// admitted because nullability is `NOT NULL`'s question rather than this one's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StrictClass {
    /// `INT` and `INTEGER`.
    Integer,
    /// `REAL`.
    Real,
    /// `TEXT`.
    Text,
    /// `BLOB`.
    Blob,
}

impl StrictClass {
    /// Returns the class a `STRICT` type name admits, or `None` for `ANY`.
    ///
    /// @param declared - the declared type, as written
    fn of(declared: &[u8]) -> Option<StrictClass> {
        let folded = declared.to_ascii_uppercase();
        match folded.as_slice() {
            b"INT" | b"INTEGER" => Some(StrictClass::Integer),
            b"REAL" => Some(StrictClass::Real),
            b"TEXT" => Some(StrictClass::Text),
            b"BLOB" => Some(StrictClass::Blob),
            _ => None,
        }
    }

    /// Reports whether a value may be stored in a column of this class.
    ///
    /// @param value - the value about to be written
    fn admits(self, value: &OwnedDatum) -> bool {
        matches!(
            (self, value),
            (_, OwnedDatum::Null)
                | (StrictClass::Integer, OwnedDatum::Int(_))
                | (StrictClass::Real, OwnedDatum::Real(_))
                | (StrictClass::Text, OwnedDatum::Text(_))
                | (StrictClass::Blob, OwnedDatum::Blob(_))
        )
    }
}

/// SQLite's name for a value's storage class, as its diagnostics spell it.
///
/// @param value - the value about to be written
fn class_name(value: &OwnedDatum) -> &'static str {
    match value {
        OwnedDatum::Null => "NULL",
        OwnedDatum::Int(_) => "INTEGER",
        OwnedDatum::Real(_) => "REAL",
        OwnedDatum::Text(_) => "TEXT",
        OwnedDatum::Blob(_) => "BLOB",
    }
}

/// One `STRICT` column: where it sits, what it admits, and how it was declared.
struct TypedColumn {
    /// The record slot the value sits in.
    slot: usize,
    /// The class the declared type admits.
    class: StrictClass,
    /// The declared type as written, which is the name the message uses.
    declared: Vec<u8>,
    /// The column name, for the message.
    name: Vec<u8>,
}

/// One `CHECK` predicate, compiled, with the text SQLite names it by.
struct CompiledCheck {
    /// The name a failure reports: the constraint's name when it was written
    /// with one, otherwise the predicate's source text.
    ///
    /// SQLite reports `CHECK constraint failed: pos` for a named constraint and
    /// `CHECK constraint failed: a > 0` for an unnamed one, so the choice is
    /// made once here rather than at every failure.
    label: Vec<u8>,
    /// The predicate.
    expr: Box<dyn Eval>,
}

/// One index's per-row expressions, compiled.
struct CompiledIndexExprs {
    /// The index's position in the table's `indexes`.
    position: usize,
    /// The partial-index predicate, when it has one.
    predicate: Option<Box<dyn Eval>>,
    /// One per key column: the expression it indexes, or `None` for a column.
    keys: Vec<Option<Box<dyn Eval>>>,
}

/// What the write path needs to maintain a table's indexes, compiled once.
///
/// **Empty for every table with neither a partial index nor an expression
/// key**, which is every table the gate measures - so `holds` answers `true`
/// and `key` answers `None` off an empty slice, and the write path pays a
/// length check per index per row and nothing else.
///
/// It is a borrowed view rather than an owned bundle because the compiled
/// expressions live on the statement's [`WriteDeclarations`] and the row space
/// lives on the statement, and the write path threads one reference rather than
/// two.
#[derive(Clone, Copy)]
pub struct IndexExprs<'a> {
    /// The compiled expressions, one entry per index that needs any.
    compiled: &'a [CompiledIndexExprs],
    /// The space the expressions were compiled against.
    space: &'a RowSpace,
}

impl<'a> IndexExprs<'a> {
    /// Returns a view over a statement's compiled index expressions.
    ///
    /// @param declarations - the statement's compiled declarations
    /// @param space - the space they were compiled against
    pub fn new(declarations: &'a WriteDeclarations, space: &'a RowSpace) -> IndexExprs<'a> {
        IndexExprs {
            compiled: &declarations.index_exprs,
            space,
        }
    }

    /// Returns a view that knows about no index at all.
    ///
    /// For the write paths that have no declarations to hand - a trigger body's
    /// cascade, a view's rows - where the table cannot have an index needing
    /// one either.
    ///
    /// @param space - a space, only ever used if `compiled` were non-empty
    pub fn none(space: &'a RowSpace) -> IndexExprs<'a> {
        IndexExprs {
            compiled: &[],
            space,
        }
    }

    /// Returns the compiled expressions of one index, when it has any.
    ///
    /// @param position - the index's position in the table's `indexes`
    fn at(&self, position: usize) -> Option<&'a CompiledIndexExprs> {
        self.compiled.iter().find(|held| held.position == position)
    }

    /// Reports whether a row belongs in one index.
    ///
    /// True for every ordinary index, and for a partial one exactly when its
    /// predicate is true of the row - which is what "partial" means, and what
    /// makes an `UPDATE` that moves a row across the predicate a removal from
    /// the index on the old image and an insertion on the new one.
    ///
    /// @param position - the index's position in the table's `indexes`
    /// @param row - the row image, in tree-column order
    pub fn holds(&self, position: usize, row: &[OwnedDatum]) -> DbResult<bool> {
        let Some(held) = self.at(position) else {
            return Ok(true);
        };
        let Some(predicate) = held.predicate.as_ref() else {
            return Ok(true);
        };
        let answer = self.space.evaluate(predicate.as_ref(), &[row])?;
        Ok(!is_false(&answer))
    }

    /// Returns one key column's value, when the index computes it.
    ///
    /// `None` for a key that is a column of the table, which the caller reads
    /// out of the row itself.
    ///
    /// @param position - the index's position in the table's `indexes`
    /// @param key - which key column
    /// @param row - the row image, in tree-column order
    pub fn key(
        &self,
        position: usize,
        key: usize,
        row: &[OwnedDatum],
    ) -> DbResult<Option<OwnedDatum>> {
        let Some(held) = self.at(position) else {
            return Ok(None);
        };
        let Some(Some(expr)) = held.keys.get(key) else {
            return Ok(None);
        };
        Ok(Some(self.space.evaluate(expr.as_ref(), &[row])?))
    }
}

/// Everything a table's declarations require of a row, compiled once.
pub struct WriteDeclarations {
    /// The record slot and affinity of every column that converts a value.
    ///
    /// BLOB affinity converts nothing and is left out, so a table of untyped
    /// columns leaves this empty and the conversion loop runs zero times.
    affinities: Vec<(usize, Affinity)>,
    /// The `STRICT` columns, empty for every table that is not `STRICT`.
    typed: Vec<TypedColumn>,
    /// The `CHECK` predicates, in declaration order.
    checks: Vec<CompiledCheck>,
    /// The per-row expressions the table's indexes need, for the indexes that
    /// need any. Empty for every table with neither a partial index nor an
    /// expression key.
    index_exprs: Vec<CompiledIndexExprs>,
    /// The `DEFAULT` a `REPLACE` stands in for a NULL, by record slot.
    ///
    /// Only the `NOT NULL` columns that declare one, so an ordinary table
    /// leaves this empty and nothing on the write path looks at it.
    defaults: Vec<CompiledDefault>,
}

/// One `NOT NULL` column's `DEFAULT`, compiled.
struct CompiledDefault {
    /// Which record slot the value lands in.
    slot: usize,
    /// The default expression.
    expr: Box<dyn Eval>,
}

impl WriteDeclarations {
    /// Compiles a table's declarations against the statement's row space.
    ///
    /// @param table - the table being written
    /// @param layout - the table tree's layout
    /// @param checks - the statement's bound `CHECK` predicates
    /// @param defaults - the bound `DEFAULT`s a `REPLACE` may substitute
    /// @param index_exprs - the statement's bound index expressions
    /// @param space - the statement's row space
    /// @param params - the bound parameters
    /// @param catalog - where a registered function's body is looked up
    pub fn compile(
        table: &TableInfo,
        layout: &SourceLayout,
        checks: &[BoundCheck],
        defaults: &[BoundDefault],
        index_exprs: &[BoundIndexExprs],
        space: &RowSpace,
        params: &Params,
        catalog: &dyn TreeCatalog,
    ) -> DbResult<WriteDeclarations> {
        let mut affinities = Vec::new();
        let mut typed = Vec::new();
        for (position, column) in table.columns.iter().enumerate() {
            let Some(slot) = layout.slots.get(position).copied().flatten() else {
                continue;
            };
            // **The rowid alias is not converted here.** Its value is the key
            // the row is about to be written under, and the key path has its
            // own rule - an integer or a refusal - which applying INTEGER
            // affinity a second time could only disagree with.
            if Some(position as u16) != table.rowid_alias && column.affinity != Affinity::Blob {
                affinities.push((slot, column.affinity));
            }
            if table.strict {
                if let Some(class) = StrictClass::of(&column.declared_type) {
                    typed.push(TypedColumn {
                        slot,
                        class,
                        declared: column.declared_type.clone(),
                        name: column.name.clone(),
                    });
                }
            }
        }
        let mut compiled = Vec::with_capacity(checks.len());
        for check in checks {
            compiled.push(CompiledCheck {
                label: check
                    .name
                    .clone()
                    .unwrap_or_else(|| source_text_of(table, check)),
                expr: space.compile(&check.expr, params, catalog)?,
            });
        }
        let mut indexed = Vec::with_capacity(index_exprs.len());
        for bound in index_exprs {
            let predicate = match bound.predicate.as_ref() {
                Some(expr) => Some(space.compile(expr, params, catalog)?),
                None => None,
            };
            let mut keys = Vec::with_capacity(bound.keys.len());
            for key in &bound.keys {
                keys.push(match key {
                    Some(expr) => Some(space.compile(expr, params, catalog)?),
                    None => None,
                });
            }
            indexed.push(CompiledIndexExprs {
                position: bound.position,
                predicate,
                keys,
            });
        }
        let mut standins = Vec::with_capacity(defaults.len());
        for default in defaults {
            let Some(slot) = layout
                .slots
                .get(usize::from(default.column))
                .copied()
                .flatten()
            else {
                continue;
            };
            standins.push(CompiledDefault {
                slot,
                expr: space.compile(&default.expr, params, catalog)?,
            });
        }
        Ok(WriteDeclarations {
            affinities,
            typed,
            checks: compiled,
            index_exprs: indexed,
            defaults: standins,
        })
    }

    /// Puts a `NOT NULL` column's `DEFAULT` in place of a NULL, when it has one.
    ///
    /// **REPLACE's rule, and only REPLACE's**: a `NOT NULL` violation resolved
    /// as `REPLACE` stores the column's default instead of refusing, and falls
    /// back to `ABORT` when the column declares none. `Ok(true)` means a value
    /// is now there and the constraint is met; `Ok(false)` means there was no
    /// default and the caller reports the violation.
    ///
    /// The expression is evaluated against the row being written, which is what
    /// makes `DEFAULT (3+4)` store 7 rather than the text of it.
    ///
    /// @param space - the statement's row space
    /// @param row - the image about to be written, filled in place
    /// @param slot - the record slot whose value is NULL
    pub fn stand_in_default(
        &self,
        space: &RowSpace,
        row: &mut [OwnedDatum],
        slot: usize,
    ) -> DbResult<bool> {
        let Some(default) = self.defaults.iter().find(|held| held.slot == slot) else {
            return Ok(false);
        };
        let value = space.evaluate(default.expr.as_ref(), &[row])?;
        if matches!(value, OwnedDatum::Null) {
            // `DEFAULT NULL` on a `NOT NULL` column is a default that does not
            // satisfy the constraint, so it is the same as having none.
            return Ok(false);
        }
        let Some(cell) = row.get_mut(slot) else {
            return Ok(false);
        };
        *cell = value;
        Ok(true)
    }

    /// Reports whether this table declares nothing the write path must apply.
    ///
    /// Lets a caller skip the whole apparatus for a table of untyped columns
    /// with no constraints, which is what several of the gate's fixtures are.
    pub fn is_empty(&self) -> bool {
        self.affinities.is_empty() && self.typed.is_empty() && self.checks.is_empty()
    }

    /// Applies each column's affinity to the row about to be stored.
    ///
    /// This is the application point Phase 3's Part B4 is about: SQLite
    /// converts `'42'` into an `INTEGER` column to the integer 42 and `42` into
    /// a `TEXT` column to the text `'42'`, and everything downstream - `typeof`,
    /// the branch a comparison takes, the order an index puts mixed classes in
    /// - follows from it.
    ///
    /// The conversion is skipped whenever the value already has the class its
    /// column wants, which is the ordinary case and costs one discriminant read.
    ///
    /// @param row - the image about to be written, converted in place
    pub fn apply_affinity(&self, row: &mut [OwnedDatum]) {
        for (slot, affinity) in &self.affinities {
            let Some(cell) = row.get_mut(*slot) else {
                continue;
            };
            if already_stored_as(cell, *affinity) {
                continue;
            }
            let taken = std::mem::replace(cell, OwnedDatum::Null);
            *cell = convert(taken, *affinity);
        }
    }

    /// Refuses a value outside a `STRICT` column's type class.
    ///
    /// Runs after `NOT NULL` and before the `CHECK` predicates, which is
    /// SQLite's order: a `STRICT` table whose row is missing a `NOT NULL`
    /// column reports the missing value rather than the wrong class.
    ///
    /// @param table - the table being written
    /// @param row - the image about to be written
    pub fn types_are_met(&self, table: &TableInfo, row: &[OwnedDatum]) -> DbResult<()> {
        for column in &self.typed {
            let Some(value) = row.get(column.slot) else {
                continue;
            };
            if column.class.admits(value) {
                continue;
            }
            return Err(
                DbError::new(ExtendedCode(codes::DATATYPE)).with_message(format!(
                    "cannot store {} value in {} column {}.{}",
                    class_name(value),
                    String::from_utf8_lossy(&column.declared),
                    String::from_utf8_lossy(&table.name),
                    String::from_utf8_lossy(&column.name)
                )),
            );
        }
        Ok(())
    }

    /// Evaluates the `CHECK` predicates against the row about to be stored.
    ///
    /// A predicate fails only when it evaluates to false. NULL is not a
    /// failure - `CHECK (a > 0)` admits a NULL `a`, because SQLite treats an
    /// unknown answer as satisfied, which is the whole reason a `CHECK` and a
    /// `NOT NULL` are two declarations rather than one.
    ///
    /// `Ok(false)` means the statement said `OR IGNORE` and the row is skipped.
    ///
    /// @param space - the statement's row space
    /// @param row - the image about to be written
    /// @param skip_on_failure - whether the statement said `OR IGNORE`
    pub fn checks_are_met(
        &self,
        space: &RowSpace,
        row: &[OwnedDatum],
        skip_on_failure: bool,
    ) -> DbResult<bool> {
        for check in &self.checks {
            let answer = space.evaluate(check.expr.as_ref(), &[row])?;
            if !is_false(&answer) {
                continue;
            }
            if skip_on_failure {
                return Ok(false);
            }
            return Err(
                DbError::new(ExtendedCode(codes::CHECK)).with_message(format!(
                    "CHECK constraint failed: {}",
                    String::from_utf8_lossy(&check.label)
                )),
            );
        }
        Ok(true)
    }
}

/// Reports whether a value evaluates to SQL false.
///
/// NULL is not false, which is what makes a `CHECK` over a nullable column pass
/// rather than fail. Text and blobs are coerced the way a `WHERE` clause
/// coerces them, so a predicate that yields text is false exactly when that
/// text reads as zero.
fn is_false(value: &OwnedDatum) -> bool {
    match value {
        OwnedDatum::Null => false,
        OwnedDatum::Int(number) => *number == 0,
        OwnedDatum::Real(number) => *number == 0.0,
        OwnedDatum::Text(bytes) | OwnedDatum::Blob(bytes) => {
            // SQLite numerifies a blob by reading its bytes as text, which is
            // what makes `x'30'` - the digit zero - read as false.
            match convert(OwnedDatum::Text(bytes.clone()), Affinity::Numeric) {
                OwnedDatum::Int(number) => number == 0,
                OwnedDatum::Real(number) => number == 0.0,
                // Text that is not a number is zero in a boolean context.
                _ => true,
            }
        }
    }
}

/// Reports whether a value is already stored the way its affinity would store
/// it, so the conversion can be skipped.
///
/// This is the fast path the write families pay for: on a column whose value
/// already has the right class it reads one discriminant and writes nothing.
///
/// @param value - the value about to be written
/// @param affinity - the column's affinity
fn already_stored_as(value: &OwnedDatum, affinity: Affinity) -> bool {
    match affinity {
        // Never in the list, but the match is total for the reader's sake.
        Affinity::Blob => true,
        Affinity::Text => !matches!(value, OwnedDatum::Int(_) | OwnedDatum::Real(_)),
        Affinity::Numeric | Affinity::Integer | Affinity::FlexNum => {
            !matches!(value, OwnedDatum::Text(_) | OwnedDatum::Real(_))
        }
        // A REAL column stores a real, so an integer is widened and a real is
        // already where it is going.
        Affinity::Real => matches!(
            value,
            OwnedDatum::Null | OwnedDatum::Blob(_) | OwnedDatum::Real(_)
        ),
    }
}

/// Converts one value the way its column's affinity stores it.
///
/// @param value - the value about to be written
/// @param affinity - the column's affinity
fn convert(value: OwnedDatum, affinity: Affinity) -> OwnedDatum {
    let held = crate::scalar::to_value(value.borrow());
    let converted = match affinity::apply_affinity(held, affinity, TextEncoding::Utf8) {
        Ok(converted) => converted,
        // A conversion that could not allocate leaves the value as it was
        // rather than storing a NULL: affinity is a preference, and a
        // preference that could not be applied has not changed what the row
        // means.
        Err(_) => return value,
    };
    let converted = if affinity == Affinity::Real {
        affinity::realify(converted)
    } else {
        converted
    };
    crate::scalar::from_value(converted)
}

/// Applies INTEGER affinity to a value about to become a rowid.
///
/// The key path is the one place affinity is applied outside
/// [`WriteDeclarations`], because what happens to a value that does *not*
/// convert is different there: an ordinary `INTEGER` column keeps the text,
/// and a rowid reports `datatype mismatch`. Sharing the conversion and not the
/// consequence is what keeps the two consistent.
///
/// @param value - the value the statement supplied as the key
pub fn to_key_affinity(value: OwnedDatum) -> OwnedDatum {
    if already_stored_as(&value, Affinity::Integer) {
        return value;
    }
    convert(value, Affinity::Integer)
}

/// Recovers a `CHECK`'s source text from the table it was declared on.
///
/// The binder hands the write path a bound predicate and the constraint's name
/// when it had one; the text SQLite quotes for an unnamed constraint lives on
/// `TableInfo::checks`, in declaration order beside the bound ones.
///
/// @param table - the table being written
/// @param check - the bound constraint
fn source_text_of(table: &TableInfo, check: &BoundCheck) -> Vec<u8> {
    for declared in &table.checks {
        if declared.name == check.name {
            return declared.expr_sql.clone();
        }
    }
    Vec::new()
}
