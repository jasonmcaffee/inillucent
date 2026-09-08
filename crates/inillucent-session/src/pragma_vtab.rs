//! The `pragma_*` table-valued functions.
//!
//! Invariant: one implementation, two spellings. `PRAGMA table_info(t)` and
//! `SELECT * FROM pragma_table_info('t')` are the same question, and the second
//! exists so the answer can be joined, filtered and ordered like any other
//! table. If they were two implementations they would eventually be two
//! answers, and the one nobody tested would be the wrong one - so the module
//! asks the host, and the host reads the same register the directive reads.
//!
//! Every pragma that takes an argument becomes a function of that argument,
//! declared as a hidden column called `arg`, plus a second hidden column called
//! `schema` for the qualifier. That is SQLite's shape, and it is the shape that
//! makes `SELECT * FROM pragma_table_info(name) ` over a list of table names a
//! join rather than a loop.

use std::sync::Arc;

use inillucent_base::DbResult;
use inillucent_ext::registry::Registry;
use inillucent_ext::vtab::{
    Context, Declaration, DeclaredColumn, FilterPlan, IndexQuery, Module, ModuleArguments,
    VirtualCursor, VirtualTable,
};
use inillucent_value::Value;

use crate::pragma::{PragmaSpec, REGISTER};

/// The plan bit that says an argument was supplied.
const HAS_ARGUMENT: i32 = 1;
/// The plan bit that says a schema was supplied.
const HAS_SCHEMA: i32 = 2;

/// One pragma, as a table-valued function.
pub struct PragmaModule {
    name: String,
    spec: &'static PragmaSpec,
}

impl PragmaModule {
    /// Returns the module for one pragma, or nothing when it has no rows.
    ///
    /// A pragma with no read form - a verb like `optimize`, or a setting whose
    /// write is the whole of it - is not a function, because a function that
    /// answered nothing would be indistinguishable from one that found nothing.
    pub fn of(spec: &'static PragmaSpec) -> PragmaModule {
        PragmaModule {
            name: format!("pragma_{}", spec.name),
            spec,
        }
    }
}

impl Module for PragmaModule {
    /// Returns the module's name, which is `pragma_` and the pragma's own.
    fn name(&self) -> &str {
        &self.name
    }

    /// A pragma function is a name, not a table.
    fn eponymous(&self) -> bool {
        true
    }

    /// `CREATE VIRTUAL TABLE t USING pragma_table_info` would have no rows of
    /// its own, so it is refused rather than made.
    fn constructible(&self) -> bool {
        false
    }

    /// Connects, which is declaring the pragma's columns and the two arguments.
    fn connect(
        &self,
        _arguments: &ModuleArguments,
        _creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        let mut columns: Vec<DeclaredColumn> = crate::pragma::columns(self.spec.name.as_bytes())
            .iter()
            .map(|column| DeclaredColumn::visible(&String::from_utf8_lossy(column)))
            .collect();
        let argument = columns.len();
        columns.push(DeclaredColumn::hidden("arg"));
        columns.push(DeclaredColumn::hidden("schema"));
        Ok(Box::new(PragmaTable {
            pragma: self.spec.name,
            argument,
            declaration: Declaration {
                columns,
                without_rowid: false,
            },
        }))
    }
}

/// One connected pragma function.
struct PragmaTable {
    pragma: &'static str,
    /// Which column the `arg` argument is; `schema` is the one after it.
    argument: usize,
    declaration: Declaration,
}

impl VirtualTable for PragmaTable {
    /// Returns the two arguments and the pragma's own columns.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Takes the two hidden columns and nothing else.
    fn best_index(&self, query: &mut IndexQuery) -> DbResult<()> {
        let mut plan = 0i32;
        for index in 0..query.constraints.len() {
            let Some(constraint) = query.constraints.get(index).copied() else {
                continue;
            };
            if !constraint.usable || constraint.op != inillucent_ext::vtab::ConstraintOp::Eq {
                continue;
            }
            let column = constraint.column as usize;
            let bit = if column == self.argument {
                HAS_ARGUMENT
            } else if column == self.argument.saturating_add(1) {
                HAS_SCHEMA
            } else {
                continue;
            };
            if plan & bit != 0 {
                continue;
            }
            plan |= bit;
            query.use_constraint(index, true);
        }
        query.index_number = plan;
        query.estimated_cost = 1.0;
        query.estimated_rows = 20;
        Ok(())
    }

    /// Opens a cursor over the rows the pragma will answer with.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(PragmaCursor {
            pragma: self.pragma,
            argument_column: self.argument,
            argument: Value::Null,
            schema: Value::Null,
            rows: Vec::new(),
            at: 0,
        }))
    }
}

/// A cursor over one pragma's answer.
struct PragmaCursor {
    pragma: &'static str,
    /// Which column the `arg` argument is; `schema` is the one after it.
    argument_column: usize,
    argument: Value<'static>,
    schema: Value<'static>,
    rows: Vec<Vec<Value<'static>>>,
    at: usize,
}

impl VirtualCursor for PragmaCursor {
    /// Asks the host for the pragma's rows.
    fn filter(&mut self, context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        self.rows.clear();
        self.at = 0;
        let mut arguments = plan.arguments.iter();
        self.argument = (plan.index_number & HAS_ARGUMENT != 0)
            .then(|| arguments.next())
            .flatten()
            .cloned()
            .unwrap_or(Value::Null);
        self.schema = (plan.index_number & HAS_SCHEMA != 0)
            .then(|| arguments.next())
            .flatten()
            .cloned()
            .unwrap_or(Value::Null);
        let database = match &self.schema {
            Value::Null => None,
            other => {
                let wanted = text_of(other);
                // The binder's view lists a database as a name and a cookie,
                // and the position in that list is the number the host indexes
                // schemas by - which is the same numbering the snapshot used.
                context.catalog.and_then(|catalog| {
                    catalog
                        .databases
                        .iter()
                        .position(|(name, _)| name.eq_ignore_ascii_case(wanted.as_bytes()))
                })
            }
        };
        let argument = match &self.argument {
            Value::Null => None,
            other => Some(other.clone()),
        };
        let rows = context
            .host
            .pragma(database, self.pragma.as_bytes(), argument.as_ref())?;
        self.rows = rows.unwrap_or_default();
        Ok(())
    }

    /// Moves to the next row.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        Ok(())
    }

    /// Returns whether the answer is exhausted.
    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// Returns one column: an argument, or one of the pragma's own.
    fn column(&mut self, _context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        if index == self.argument_column {
            return Ok(self.argument.clone());
        }
        if index == self.argument_column.saturating_add(1) {
            return Ok(self.schema.clone());
        }
        Ok(self
            .rows
            .get(self.at)
            .and_then(|row| row.get(index))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Returns the row's position in the answer.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self.at as i64)
    }
}

/// Returns a value as text, whatever storage class it arrived in.
fn text_of(value: &Value<'static>) -> String {
    match value {
        Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
        Value::Blob(blob) => String::from_utf8_lossy(blob.raw()).into_owned(),
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => {
            String::from_utf8_lossy(&inillucent_value::numeric::real_to_text(*number)).into_owned()
        }
        Value::Null => String::new(),
    }
}

/// Registers one `pragma_*` module per pragma that answers rows.
///
/// The two that are left out are the two whose answer is not a table:
/// `pragma_list` and `compile_options` *are* tables and are in, while a verb
/// like `optimize` has nothing to select from.
pub fn register_all(registry: &mut Registry) {
    for spec in REGISTER {
        if crate::settings::Setting::named(spec.name.as_bytes()).is_some()
            && spec.columns.is_empty()
        {
            // A setting answers one row of one column, and SQLite makes a
            // function of it too - `SELECT * FROM pragma_page_size` - so these
            // are in as well.
        }
        registry.register_module(Arc::new(PragmaModule::of(spec)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pragma in the register has a function, named after it.
    #[test]
    fn every_pragma_has_a_function() {
        let mut registry = Registry::with_builtins();
        register_all(&mut registry);
        assert!(registry.module(b"pragma_table_info").is_some());
        assert!(registry.module(b"PRAGMA_INDEX_LIST").is_some());
        assert!(registry.module(b"pragma_no_such_thing").is_none());
    }

    /// A pragma function declares its answer first and its arguments after.
    #[test]
    fn the_arguments_come_first() {
        let spec = crate::pragma::spec(b"table_info").expect("registered");
        let module = PragmaModule::of(spec);
        let table = module
            .connect(&ModuleArguments::default(), false)
            .expect("connects");
        let columns = &table.declaration().columns;
        assert_eq!(columns[0].name, b"cid");
        assert!(!columns[0].hidden);
        // The two hidden columns are last, because a pragma's own columns may
        // include one called `schema` - `pragma_table_list` does - and a
        // hidden one in front of it would shadow the answer with the argument.
        let hidden = columns.len().saturating_sub(2);
        assert_eq!(columns[hidden].name, b"arg");
        assert!(columns[hidden].hidden);
        assert_eq!(columns[hidden + 1].name, b"schema");
        assert!(columns[hidden + 1].hidden);
    }

    /// A pragma function is a name and cannot be created as a table.
    #[test]
    fn a_pragma_function_cannot_be_created() {
        let spec = crate::pragma::spec(b"table_info").expect("registered");
        let module = PragmaModule::of(spec);
        assert!(module.eponymous());
        assert!(!module.constructible());
    }
}
