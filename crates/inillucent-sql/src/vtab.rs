//! What the front end has to know about a virtual table.
//!
//! Invariant: this file holds the *data* of the virtual-table contract and none
//! of its behaviour. The planner has to be able to say "here are the
//! constraints I can offer and the order I would like", and the catalog has to
//! be able to hold the columns a module declared, and neither of those can wait
//! until the layer that owns modules. The traits a module implements live above
//! this, in `inillucent-ext`, where a pager can be named.
//!
//! The split is not bureaucratic: it is what lets a bound statement stay a pure
//! function of its SQL and one catalog generation. A planner that had to call
//! into a module to describe a plan would be a planner whose output depended on
//! run-time state.

use inillucent_base::DbResult;
use inillucent_value::{Affinity, Value};

/// The comparison a constraint applies.
///
/// `Match`, `Like`, `Glob` and `Regexp` are here because an operator a module
/// understands is the reason virtual tables exist: `t MATCH 'x'` means nothing
/// to the engine and everything to FTS5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintOp {
    /// `=`
    Eq,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `<`
    Lt,
    /// `>=`
    Ge,
    /// `MATCH`
    Match,
    /// `LIKE`
    Like,
    /// `GLOB`
    Glob,
    /// `REGEXP`
    Regexp,
    /// `!=`
    Ne,
    /// `IS NOT`
    IsNot,
    /// `IS NOT NULL`
    IsNotNull,
    /// `IS NULL`
    IsNull,
    /// `IS`
    Is,
}

impl ConstraintOp {
    /// Returns the number the C surface gives this operator.
    pub fn code(self) -> i32 {
        match self {
            ConstraintOp::Eq => 2,
            ConstraintOp::Gt => 4,
            ConstraintOp::Le => 8,
            ConstraintOp::Lt => 16,
            ConstraintOp::Ge => 32,
            ConstraintOp::Match => 64,
            ConstraintOp::Like => 65,
            ConstraintOp::Glob => 66,
            ConstraintOp::Regexp => 67,
            ConstraintOp::Ne => 68,
            ConstraintOp::IsNot => 69,
            ConstraintOp::IsNotNull => 70,
            ConstraintOp::IsNull => 71,
            ConstraintOp::Is => 72,
        }
    }

    /// Returns whether the operator has a right-hand value to pass on.
    ///
    /// `IS NULL` and `IS NOT NULL` do not, which is why they are offered to a
    /// module without an argument position ever being filled.
    pub fn has_value(self) -> bool {
        !matches!(self, ConstraintOp::IsNull | ConstraintOp::IsNotNull)
    }
}

/// The column number that names the rowid rather than a declared column.
pub const ROWID_COLUMN: i32 = -1;

/// One constraint the query offers the module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConstraintSpec {
    /// Which column, or [`ROWID_COLUMN`].
    pub column: i32,
    /// The comparison.
    pub op: ConstraintOp,
    /// Whether the value is available at the time this loop runs.
    ///
    /// A constraint against a table the loop has not reached yet is offered but
    /// not usable, which is how one `best_index` answer serves every position
    /// the term could take in the join order.
    pub usable: bool,
}

/// One `ORDER BY` term the query offers the module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderSpec {
    /// Which column, or [`ROWID_COLUMN`].
    pub column: i32,
    /// Whether the term is descending.
    pub descending: bool,
}

/// What the module decided to do with one constraint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConstraintUsage {
    /// The one-based position the value is passed to `filter` in, or zero when
    /// the constraint is not used.
    pub argument: usize,
    /// Whether the engine may stop testing this constraint itself.
    ///
    /// The module promising, not the engine assuming. A module that sets this
    /// and then does not apply the constraint returns wrong rows, which is why
    /// the default is to test it twice.
    pub omit: bool,
}

/// The question put to a module's `best_index`, and the answer written back.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexQuery {
    /// The constraints the query can offer.
    pub constraints: Vec<ConstraintSpec>,
    /// The ordering the query would like.
    pub order_by: Vec<OrderSpec>,
    /// What the module decided about each constraint, in the same order.
    pub usage: Vec<ConstraintUsage>,
    /// The plan number the module chose, passed back to `filter`.
    pub index_number: i32,
    /// The plan string the module chose, passed back to `filter`.
    pub index_string: String,
    /// Whether the rows will already be in the requested order.
    pub ordered: bool,
    /// What the module thinks the scan will cost.
    pub estimated_cost: f64,
    /// How many rows the module thinks it will produce.
    pub estimated_rows: i64,
}

impl IndexQuery {
    /// Returns a question with every answer at its default.
    pub fn new(constraints: Vec<ConstraintSpec>, order_by: Vec<OrderSpec>) -> IndexQuery {
        let usage = vec![ConstraintUsage::default(); constraints.len()];
        IndexQuery {
            constraints,
            order_by,
            usage,
            index_number: 0,
            index_string: String::new(),
            ordered: false,
            // SQLite's own default, which is deliberately enormous: a module
            // that says nothing about cost should not win a join order it has
            // no claim to.
            estimated_cost: 5.0e98,
            estimated_rows: 25,
        }
    }

    /// Marks one constraint as used, taking the next argument position.
    pub fn use_constraint(&mut self, index: usize, omit: bool) -> usize {
        let next = self
            .usage
            .iter()
            .map(|usage| usage.argument)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        if let Some(usage) = self.usage.get_mut(index) {
            usage.argument = next;
            usage.omit = omit;
        }
        next
    }

    /// Returns the constraint positions that feed `filter`, in argument order.
    pub fn argument_order(&self) -> Vec<usize> {
        let mut claimed: Vec<(usize, usize)> = self
            .usage
            .iter()
            .enumerate()
            .filter(|(_, usage)| usage.argument > 0)
            .map(|(index, usage)| (usage.argument, index))
            .collect();
        claimed.sort_unstable();
        claimed.into_iter().map(|(_, index)| index).collect()
    }
}

/// What `filter` is told to do.
#[derive(Clone, Debug)]
pub struct FilterPlan {
    /// The plan number `best_index` chose.
    pub index_number: i32,
    /// The plan string `best_index` chose.
    pub index_string: String,
    /// The constraint values, in the argument order `best_index` assigned.
    pub arguments: Vec<Value<'static>>,
}

/// One column a module declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredColumn {
    /// The column name.
    pub name: Vec<u8>,
    /// The declared type, exactly as the module wrote it.
    pub declared_type: Vec<u8>,
    /// The affinity that type maps to.
    pub affinity: Affinity,
    /// The folded name of the column's collation.
    pub collation: Vec<u8>,
    /// Whether the column is hidden from `SELECT *` and from an `INSERT` with
    /// no column list.
    ///
    /// A hidden column is how a table-valued function takes its arguments:
    /// `json_each('[1]')` is `SELECT * FROM json_each WHERE json = '[1]'`, and
    /// `json` is a hidden column. It is the whole mechanism, not a display
    /// preference.
    pub hidden: bool,
}

impl DeclaredColumn {
    /// Returns an ordinary visible column with no declared type.
    pub fn visible(name: &str) -> DeclaredColumn {
        DeclaredColumn {
            name: name.as_bytes().to_vec(),
            declared_type: Vec::new(),
            affinity: Affinity::Blob,
            collation: b"binary".to_vec(),
            hidden: false,
        }
    }

    /// Returns a hidden column, which is how an argument is declared.
    pub fn hidden(name: &str) -> DeclaredColumn {
        DeclaredColumn {
            hidden: true,
            ..DeclaredColumn::visible(name)
        }
    }

    /// Returns the column with a declared type and the affinity it implies.
    pub fn typed(mut self, declared: &str) -> DeclaredColumn {
        self.declared_type = declared.as_bytes().to_vec();
        self.affinity = inillucent_value::affinity::for_column(declared.as_bytes());
        self
    }
}

/// What a module says its table looks like.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// The columns, in the order `SELECT *` and `column` number them.
    pub columns: Vec<DeclaredColumn>,
    /// Whether the table has no rowid of its own.
    pub without_rowid: bool,
}

/// The root page of one shadow table, by the suffix that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowRoot {
    /// The suffix after the virtual table's own name, such as `data`.
    pub suffix: Vec<u8>,
    /// The root page of the shadow table's b-tree.
    pub root: u32,
}

/// Everything a module is handed when it is connected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleArguments {
    /// The database the table lives in.
    pub database: usize,
    /// The schema name, for messages.
    pub schema: Vec<u8>,
    /// The virtual table's own name.
    pub table: Vec<u8>,
    /// The module's name as written.
    pub module: Vec<u8>,
    /// The arguments inside the parentheses, as written source slices.
    pub arguments: Vec<Vec<u8>>,
    /// The roots of the shadow tables the schema already holds.
    pub shadows: Vec<ShadowRoot>,
}

impl ModuleArguments {
    /// Returns the root page of one shadow table.
    pub fn shadow(&self, suffix: &[u8]) -> Option<u32> {
        self.shadows
            .iter()
            .find(|shadow| shadow.suffix == suffix)
            .map(|shadow| shadow.root)
    }
}

/// What a module needs created before it can be connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowTable {
    /// The suffix after the virtual table's own name.
    pub suffix: Vec<u8>,
    /// The `CREATE` statement, with `%` standing for the table's own name.
    pub create_sql: String,
    /// The table these shadows belong to, when it is not the module's own.
    ///
    /// **How a module reads another table's storage.** `fts5vocab(f, 'row')`
    /// is a view over the index `f` built, and the whole of what it needs is
    /// read access to `f`'s shadow tables - which the module contract
    /// deliberately does not give it, because "a module sees only what it was
    /// handed" is what makes a hostile module a bounded problem.
    ///
    /// So it is handed them, explicitly and by name. A module that names an
    /// owner is asking for shadows that **already exist**: they are looked up
    /// rather than created, and a name the catalog does not have is a refusal
    /// rather than a fresh table. The module still sees only the roots it was
    /// given, and still cannot resolve a name for itself.
    pub owner: Option<Vec<u8>>,
}

/// One row a write asks a module to make.
#[derive(Clone, Debug)]
pub enum Change {
    /// Remove the row with this rowid or primary key.
    Delete(Value<'static>),
    /// Add a row.
    Insert {
        /// The rowid to use, or NULL for one the module chooses.
        rowid: Value<'static>,
        /// One value per declared column, hidden columns included.
        values: Vec<Value<'static>>,
    },
    /// Replace a row.
    Update {
        /// The row being replaced.
        old_rowid: Value<'static>,
        /// The rowid it should have afterwards, which a statement may change.
        new_rowid: Value<'static>,
        /// One value per declared column, hidden columns included.
        values: Vec<Value<'static>>,
    },
}

/// The module one virtual table is implemented by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleRef {
    /// The module name as written.
    pub name: Vec<u8>,
    /// The ASCII-folded lookup key.
    pub folded: Vec<u8>,
    /// The arguments inside the parentheses, as written.
    pub arguments: Vec<Vec<u8>>,
}

/// The rows of a module's shadow tables, whatever engine holds them.
///
/// **This is the seam the TDD's "shadow tables become ordinary trees" needs.**
/// FTS5 and the R-Tree keep their whole state in shadow tables and reach them
/// only through `ShadowTables`, so the modules themselves say nothing about
/// pages, cursors or b-trees - which is what lets the same module code run over
/// the old engine's `sqlite_master` b-trees and the new engine's PAX trees. A
/// module that had reached a pager directly would have to be written twice.
///
/// Every method names a *root*, because a module is handed the roots of its own
/// shadow tables and nothing else. There is no name resolution here and no
/// catalog: a module that wanted to read somebody else's table would have to be
/// given it.
///
/// The rowid methods are for a rowid table, where the first value of a row *is*
/// its rowid; the keyed ones are for a `WITHOUT ROWID` table, whose whole row is
/// its key. FTS5 uses both.
pub trait ShadowStore {
    /// Reads one row by rowid, or nothing when there is not one.
    ///
    /// @param root - the shadow table's root
    /// @param rowid - the row's key
    fn read_row(&mut self, root: u32, rowid: i64) -> DbResult<Option<Vec<Value<'static>>>>;

    /// Writes one row by rowid, replacing whatever was there.
    ///
    /// @param root - the shadow table's root
    /// @param rowid - the row's key
    /// @param values - the row, its rowid first
    fn write_row(&mut self, root: u32, rowid: i64, values: &[Value<'static>]) -> DbResult<()>;

    /// Removes one row by rowid, reporting nothing when there was not one.
    ///
    /// @param root - the shadow table's root
    /// @param rowid - the row's key
    fn delete_row(&mut self, root: u32, rowid: i64) -> DbResult<()>;

    /// Returns the largest rowid one shadow table holds.
    ///
    /// @param root - the shadow table's root
    fn max_rowid(&mut self, root: u32) -> DbResult<i64>;

    /// Runs a body over every row, in rowid order, stopping when it says so.
    ///
    /// @param root - the shadow table's root
    /// @param body - what to do with each row
    fn scan(
        &mut self,
        root: u32,
        body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()>;

    /// Runs a body over every row whose rowid is at least `from`, in rowid
    /// order, stopping when it says so.
    ///
    /// **A seek, not a scan with a filter, when an implementor has one.** A
    /// rowid table's rows are already in key order, so a caller that only
    /// wants what is above a watermark - a delta log's `deltas_above`, chief
    /// among them - does not need every row below it decoded and thrown
    /// away; it needs the store to descend to `from` once and walk right
    /// from there.
    ///
    /// **The default is correct rather than fast, and that is deliberate.**
    /// It is [`Self::scan`] with a callback that skips what is below `from`,
    /// which costs the whole table exactly as a hand-written filter would -
    /// so an implementor with no cheap way to position by key is still right
    /// by doing nothing, and one that can descend directly to a key
    /// overrides this with that descent. Every rowid tree the new engine
    /// keeps can; the retired engine's b-trees, reached only from the
    /// differential suites that still exercise it, are left on the default
    /// because a seek there is not worth building for a store on its way out.
    ///
    /// @param root - the shadow table's root
    /// @param from - the smallest rowid to visit
    /// @param body - what to do with each row
    fn scan_from(
        &mut self,
        root: u32,
        from: i64,
        body: &mut dyn FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        self.scan(root, &mut |rowid, values| {
            if rowid < from {
                return Ok(true);
            }
            body(rowid, values)
        })
    }

    /// Reads one row of a keyed shadow table, or nothing when there is not one.
    ///
    /// @param root - the shadow table's root
    /// @param key - the leading columns that identify it
    /// @param columns - how many columns to return, `usize::MAX` for all
    fn read_keyed(
        &mut self,
        root: u32,
        key: &[Value<'static>],
        columns: usize,
    ) -> DbResult<Option<Vec<Value<'static>>>>;

    /// Writes one row of a keyed shadow table, replacing whatever was there.
    ///
    /// @param root - the shadow table's root
    /// @param key_columns - how many leading columns form the key
    /// @param values - the whole row
    fn write_keyed(
        &mut self,
        root: u32,
        key_columns: usize,
        values: &[Value<'static>],
    ) -> DbResult<()>;

    /// Removes one row of a keyed shadow table.
    ///
    /// @param root - the shadow table's root
    /// @param key - the leading columns that identify it
    fn delete_keyed(&mut self, root: u32, key: &[Value<'static>]) -> DbResult<()>;

    /// Runs a body over every row of a keyed shadow table, in key order.
    ///
    /// @param root - the shadow table's root
    /// @param key_columns - how many leading columns form the key
    /// @param body - what to do with each row
    fn scan_keyed(
        &mut self,
        root: u32,
        key_columns: usize,
        body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()>;

    /// Runs a body over every row of a keyed shadow table whose key sorts at
    /// or after `from`, in key order, stopping when it says so.
    ///
    /// **The keyed twin of [`Self::scan_from`], for a key of more than one
    /// column.** FTS5's `%_idx` is keyed `(segid, term)`, so a caller that
    /// wants one segment's terms starting at a prefix - a term-major seek to
    /// `(segid, prefix)`, once per live segment - needs to position by a key
    /// that is not the whole row, the same shape a rowid seek already had and
    /// a single-column keyed seek would not need a new method for.
    ///
    /// **The default is correct rather than fast, and that is deliberate**,
    /// for the reason [`Self::scan_from`]'s default is: it is
    /// [`Self::scan_keyed`] with a callback that skips whatever sorts below
    /// `from`, so an implementor with no cheap way to position by key is
    /// still right by doing nothing, and one that can descend directly to a
    /// key overrides this with that descent.
    ///
    /// @param root - the shadow table's root
    /// @param key_columns - how many leading columns form the key
    /// @param from - the key to start at, compared column by column
    /// @param body - what to do with each row
    fn scan_keyed_from(
        &mut self,
        root: u32,
        key_columns: usize,
        from: &[Value<'static>],
        body: &mut dyn FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        self.scan_keyed(root, key_columns, &mut |values| {
            if key_sorts_below(values, from) {
                return Ok(true);
            }
            body(values)
        })
    }
}

/// Returns whether a keyed row's leading columns sort before `from`, compared
/// column by column under [`inillucent_value::compare::compare_values`] with
/// `BINARY` collation - what every shadow table's key compares under, since
/// none of them declares a column collation of its own.
///
/// Shared by [`ShadowStore::scan_keyed_from`]'s default implementation and by
/// a real implementor's seek, which still has to discard whatever a leaf
/// below the seek's target key holds - `PagedTree::visit_range` positions at
/// the leaf that *could* hold the key, not necessarily past everything
/// smaller than it.
///
/// @param row - the row read back
/// @param from - the key a caller asked to start at
pub fn key_sorts_below(row: &[Value<'static>], from: &[Value<'static>]) -> bool {
    use std::cmp::Ordering;
    for (left, right) in row.iter().zip(from.iter()) {
        match inillucent_value::compare::compare_values(
            left,
            right,
            inillucent_value::Collation::Binary,
        ) {
            Ordering::Less => return true,
            Ordering::Greater => return false,
            Ordering::Equal => continue,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator numbers are the ones the C surface publishes, because a
    /// module written against the header compares against these constants.
    #[test]
    fn operator_codes_match_the_published_constants() {
        assert_eq!(ConstraintOp::Eq.code(), 2);
        assert_eq!(ConstraintOp::Gt.code(), 4);
        assert_eq!(ConstraintOp::Le.code(), 8);
        assert_eq!(ConstraintOp::Lt.code(), 16);
        assert_eq!(ConstraintOp::Ge.code(), 32);
        assert_eq!(ConstraintOp::Match.code(), 64);
        assert_eq!(ConstraintOp::Is.code(), 72);
    }

    /// Argument positions are handed out in the order they are claimed, and
    /// read back in that same order.
    #[test]
    fn argument_positions_are_claimed_in_order() {
        let mut query = IndexQuery::new(
            vec![
                ConstraintSpec {
                    column: 0,
                    op: ConstraintOp::Eq,
                    usable: true,
                },
                ConstraintSpec {
                    column: 1,
                    op: ConstraintOp::Gt,
                    usable: true,
                },
            ],
            Vec::new(),
        );
        assert_eq!(query.use_constraint(1, true), 1);
        assert_eq!(query.use_constraint(0, false), 2);
        assert_eq!(query.argument_order(), vec![1, 0]);
        assert!(query.usage[1].omit);
        assert!(!query.usage[0].omit);
    }

    /// A module that says nothing about cost must not win a join order.
    #[test]
    fn the_default_cost_is_deliberately_enormous() {
        let query = IndexQuery::new(Vec::new(), Vec::new());
        assert!(query.estimated_cost > 1.0e90);
    }

    /// The two null tests carry no value, so nothing is passed for them.
    #[test]
    fn the_null_tests_carry_no_value() {
        assert!(!ConstraintOp::IsNull.has_value());
        assert!(!ConstraintOp::IsNotNull.has_value());
        assert!(ConstraintOp::Eq.has_value());
    }
}
