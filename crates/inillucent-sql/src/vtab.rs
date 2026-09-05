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
