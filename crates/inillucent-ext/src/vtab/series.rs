//! `generate_series`, the smallest complete virtual table.
//!
//! Invariant: it is here because it is the module the contract is *tested*
//! with. It has hidden columns that are really arguments, a `best_index` that
//! reports a real cost and a real ordering, an `ORDER BY` it can satisfy
//! without a sorter, and a plan that changes with the constraints - which is
//! every part of the interface a module can exercise without a single page of
//! storage. A contract that only ever ran FTS5 would be a contract nobody could
//! debug.

use inillucent_base::DbResult;
use inillucent_value::{cast, Value};

use super::{
    ConstraintOp, Context, Declaration, DeclaredColumn, FilterPlan, IndexQuery, Module,
    ModuleArguments, VirtualCursor, VirtualTable,
};

/// The visible column.
const VALUE: usize = 0;
/// The hidden column holding the first value.
const START: usize = 1;
/// The hidden column holding the last value.
const STOP: usize = 2;
/// The hidden column holding the step.
const STEP: usize = 3;

/// The `generate_series` module.
pub struct SeriesModule;

impl Module for SeriesModule {
    /// Returns the module's name.
    fn name(&self) -> &str {
        "generate_series"
    }

    /// It is a name rather than a table.
    fn eponymous(&self) -> bool {
        true
    }

    /// Connects, which is only declaring the four columns.
    fn connect(
        &self,
        _arguments: &ModuleArguments,
        _creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        Ok(Box::new(SeriesTable {
            declaration: Declaration {
                columns: vec![
                    DeclaredColumn::visible("value"),
                    DeclaredColumn::hidden("start"),
                    DeclaredColumn::hidden("stop"),
                    DeclaredColumn::hidden("step"),
                ],
                without_rowid: false,
            },
        }))
    }
}

/// The plan bits, one per argument the scan was given.
const HAS_START: i32 = 1;
/// The plan bit for a stop value.
const HAS_STOP: i32 = 2;
/// The plan bit for a step value.
const HAS_STEP: i32 = 4;
/// The plan bit that says the scan runs backwards.
const DESCENDING: i32 = 8;

/// One connected `generate_series`.
struct SeriesTable {
    declaration: Declaration,
}

impl VirtualTable for SeriesTable {
    /// Returns the four columns.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Takes the three hidden columns, and the ordering when it is on `value`.
    ///
    /// Reporting the order as satisfied is the interesting part: a series is
    /// already sorted, so `ORDER BY value DESC` costs a direction rather than a
    /// sorter, and the plan says so.
    fn best_index(&self, info: &mut IndexQuery) -> DbResult<()> {
        let mut plan = 0i32;
        for index in 0..info.constraints.len() {
            let Some(constraint) = info.constraints.get(index).copied() else {
                continue;
            };
            if !constraint.usable || constraint.op != ConstraintOp::Eq {
                continue;
            }
            let bit = match constraint.column as usize {
                START => HAS_START,
                STOP => HAS_STOP,
                STEP => HAS_STEP,
                _ => continue,
            };
            if plan & bit != 0 {
                continue;
            }
            plan |= bit;
            info.use_constraint(index, true);
        }
        if let Some(order) = info.order_by.first() {
            if info.order_by.len() == 1 && order.column as usize == VALUE {
                info.ordered = true;
                if order.descending {
                    plan |= DESCENDING;
                }
            }
        }
        info.index_number = plan;
        // A series with no start is unbounded, so it costs what an unbounded
        // scan costs: enough that the planner puts it last.
        info.estimated_cost = if plan & HAS_START != 0 { 1.0 } else { 2.0e9 };
        info.estimated_rows = 1000;
        Ok(())
    }

    /// Opens a cursor.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(SeriesCursor {
            value: 0,
            stop: 0,
            step: 1,
            descending: false,
            done: true,
            row: 0,
        }))
    }
}

/// A cursor walking one series.
struct SeriesCursor {
    value: i64,
    stop: i64,
    step: i64,
    descending: bool,
    done: bool,
    row: i64,
}

impl VirtualCursor for SeriesCursor {
    /// Reads the three arguments and positions on the first value.
    fn filter(&mut self, _context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        let mut arguments = plan.arguments.iter();
        let mut take = |bit: i32, fallback: i64| -> i64 {
            if plan.index_number & bit == 0 {
                return fallback;
            }
            arguments
                .next()
                .map(|value| cast::integer_value(value))
                .unwrap_or(fallback)
        };
        let start = take(HAS_START, 0);
        let stop = take(HAS_STOP, 0xffff_ffff);
        let step = take(HAS_STEP, 1);
        // A zero step would never terminate, and SQLite treats it as one.
        self.step = if step == 0 { 1 } else { step.abs() };
        self.descending = plan.index_number & DESCENDING != 0;
        self.row = 0;
        if start > stop {
            self.done = true;
            return Ok(());
        }
        self.done = false;
        if self.descending {
            // The last value on the ascending series, which is where a
            // descending walk begins.
            let steps = (stop.saturating_sub(start)) / self.step;
            self.value = start.saturating_add(steps.saturating_mul(self.step));
            self.stop = start;
        } else {
            self.value = start;
            self.stop = stop;
        }
        Ok(())
    }

    /// Moves one step along the series.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.row = self.row.saturating_add(1);
        if self.descending {
            match self.value.checked_sub(self.step) {
                Some(next) if next >= self.stop => self.value = next,
                _ => self.done = true,
            }
            return Ok(());
        }
        match self.value.checked_add(self.step) {
            Some(next) if next <= self.stop => self.value = next,
            _ => self.done = true,
        }
        Ok(())
    }

    /// Returns whether the series is finished.
    fn eof(&self) -> bool {
        self.done
    }

    /// Returns one column: the value, or the argument it was given.
    fn column(&mut self, _context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        Ok(match index {
            VALUE => Value::Integer(self.value),
            START => Value::Integer(if self.descending {
                self.stop
            } else {
                self.value
            }),
            STOP => Value::Integer(if self.descending {
                self.value
            } else {
                self.stop
            }),
            STEP => Value::Integer(self.step),
            _ => Value::Null,
        })
    }

    /// Returns the row's position in the series.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self.row.saturating_add(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_base::limits::Limits;

    /// Runs a series and collects its values.
    fn run(plan: i32, arguments: &[i64]) -> Vec<i64> {
        let module = SeriesModule;
        let table = module
            .connect(
                &ModuleArguments {
                    database: 0,
                    schema: b"main".to_vec(),
                    table: b"generate_series".to_vec(),
                    module: b"generate_series".to_vec(),
                    arguments: Vec::new(),
                    shadows: Vec::new(),
                },
                false,
            )
            .expect("connects");
        let mut cursor = table.open().expect("opens");
        let mut pagers = NoPagers;
        let limits = Limits::default();
        let mut context = Context {
            host: &mut pagers,
            database: 0,
            limits: &limits,
            catalog: None,
        };
        cursor
            .filter(
                &mut context,
                &FilterPlan {
                    index_number: plan,
                    index_string: String::new(),
                    arguments: arguments
                        .iter()
                        .map(|value| Value::Integer(*value))
                        .collect(),
                },
            )
            .expect("filters");
        let mut out = Vec::new();
        while !cursor.eof() {
            let value = cursor.column(&mut context, VALUE).expect("reads");
            out.push(value.as_integer().unwrap_or(0));
            cursor.next(&mut context).expect("advances");
            if out.len() > 100 {
                break;
            }
        }
        out
    }

    /// A pager set with no databases, for a module that reads none.
    struct NoPagers;

    impl crate::vtab::Host for NoPagers {}

    /// The ordinary ascending series.
    #[test]
    fn an_ascending_series_counts_up() {
        assert_eq!(run(HAS_START | HAS_STOP, &[1, 5]), vec![1, 2, 3, 4, 5]);
        assert_eq!(
            run(HAS_START | HAS_STOP | HAS_STEP, &[1, 10, 3]),
            vec![1, 4, 7, 10]
        );
    }

    /// A descending scan walks the same values backwards, which is what lets
    /// `ORDER BY value DESC` skip the sorter.
    #[test]
    fn a_descending_series_counts_down() {
        assert_eq!(
            run(HAS_START | HAS_STOP | DESCENDING, &[1, 5]),
            vec![5, 4, 3, 2, 1]
        );
        assert_eq!(
            run(HAS_START | HAS_STOP | HAS_STEP | DESCENDING, &[1, 10, 3]),
            vec![10, 7, 4, 1]
        );
    }

    /// A stop below the start produces nothing rather than looping.
    #[test]
    fn an_empty_series_produces_nothing() {
        assert!(run(HAS_START | HAS_STOP, &[5, 1]).is_empty());
    }

    /// A zero step would never terminate, so it is read as one.
    #[test]
    fn a_zero_step_is_read_as_one() {
        assert_eq!(
            run(HAS_START | HAS_STOP | HAS_STEP, &[1, 3, 0]),
            vec![1, 2, 3]
        );
    }
}
