//! The virtual-table contract: what a module is asked, in what order, and what
//! it may answer.
//!
//! Invariant: a module sees only what it was handed. It is given the arguments
//! of its own `CREATE VIRTUAL TABLE`, the root pages of its own shadow tables,
//! and a pager to read them through - and nothing else. It never resolves a
//! name, never opens a transaction, and never learns that another table exists.
//! That is what turns "a hostile virtual table" from an open question into a
//! bounded one: the worst a broken module can do is answer wrongly about its
//! own rows, and the engine still checks the rows it hands back.
//!
//! The shape is SQLite's, because the shape is the contract that makes the
//! planner able to push work into a module at all. `best_index` is asked which
//! of the query's constraints it can use and what that would cost; the answer
//! decides both the loop order and which predicates the engine still has to
//! test for itself. A module that lied about consuming a constraint would
//! return wrong rows, so a constraint is only dropped from the residual when
//! the module says `omit` - and `omit` is the module promising, not the engine
//! assuming.

pub mod fts5;
pub mod json_each;
pub mod rtree;
pub mod series;

use inillucent_base::limits::Limits;
use inillucent_base::{DbError, DbResult};
use inillucent_storage::PagerSet;
use inillucent_value::{Collation, Value};

pub use inillucent_sql::vtab::{
    Change, ConstraintOp, ConstraintSpec, ConstraintUsage, Declaration, DeclaredColumn, FilterPlan,
    IndexQuery, ModuleArguments, ModuleRef, OrderSpec, ShadowRoot, ShadowStore, ShadowTable,
    ROWID_COLUMN,
};

/// Everything a module may reach while it is answering.
///
/// It is passed per call rather than held, because the pager is borrowed from
/// the statement that is running and a module that kept it would outlive the
/// borrow. Passing it also makes the reach explicit at every call site: a
/// method with no context cannot touch the database at all.
pub struct Context<'host> {
    /// The connection, as a module is allowed to see it.
    pub host: &'host mut dyn Host,
    /// Where the module's own shadow tables live.
    ///
    /// **A trait rather than a pager, because the engine underneath is not
    /// fixed.** The TDD's Phase 4 puts FTS5 and the R-Tree over the new engine's
    /// PAX trees, and the modules reach their storage only through
    /// [`crate::shadow::ShadowTables`] - so pointing that at a different store
    /// is the whole of the port, and the tokenizers, the ranking, the segment
    /// merges and the R-Tree's node logic are not touched at all.
    ///
    /// `None` means the caller is the old engine and the pager below `host` is
    /// the store, which is what every existing call site is.
    pub store: Option<&'host mut dyn ShadowStore>,
    /// Which one this table lives in.
    pub database: usize,
    /// The run-time limits.
    pub limits: &'host Limits,
    /// The schema the statement was compiled against.
    ///
    /// It is here for the modules that introspect. `fts5vocab` is the one that
    /// needs it: the index it reads stores column *numbers*, and the names it
    /// has to report are in the target's declaration - so a module that had to
    /// be handed a schema through its arguments could not be written.
    ///
    /// **Read-only, and it is the binder's view rather than the file's.** A
    /// module can see what tables exist and what columns they declare; it
    /// cannot reach a row of one through this, which is the line the module
    /// contract draws. Reaching another table's *rows* is `ShadowTable::owner`,
    /// and that is a grant made by name at connect time.
    pub catalog: Option<&'host inillucent_sql::catalog_view::StaticCatalog>,
}

/// What a module may ask the connection for.
///
/// The pragma register, because the `pragma_*` table-valued functions are a
/// module whose rows *are* a pragma's answer - and there must be one
/// implementation of that answer, not two - and, for the old engine only, the
/// pager set its shadow tables live behind.
///
/// **`PagerSet` was a supertrait and is now an optional accessor.** A host that
/// has no pager could not be written while it was a supertrait: the new engine's
/// pages are behind a buffer pool and its rows behind a tree, so its host had to
/// implement `PagerSet` and return an error from every method - a trait
/// implemented in order to refuse it. That also made `inillucent-ext`'s dependency
/// on `inillucent-storage` structural rather than incidental. Now a host that
/// reaches its rows through [`Context::store`] says so by not answering, which
/// is what it means, and the arm below it goes when the old engine does.
pub trait Host {
    /// Answers a pragma that only reads, or `None` when there is no such thing.
    ///
    /// The default refuses everything, which is what a host with no connection
    /// behind it can honestly say.
    fn pragma(
        &mut self,
        _database: Option<usize>,
        _name: &[u8],
        _argument: Option<&Value<'static>>,
    ) -> DbResult<Option<Vec<Vec<Value<'static>>>>> {
        Ok(None)
    }

    /// The pagers this host's shadow tables live behind, when it has any.
    ///
    /// The default is `None`, which is the honest answer for a host whose rows
    /// are in the new engine's trees: it reaches them through
    /// [`Context::store`] instead, and a pager it does not have is not
    /// something it should have to write a refusal for.
    fn pager_set(&mut self) -> Option<&mut dyn PagerSet> {
        None
    }
}

/// A pager on its own is a host with no pragmas.
impl Host for inillucent_storage::Pager {
    fn pager_set(&mut self) -> Option<&mut dyn PagerSet> {
        Some(self)
    }
}

/// A registered virtual-table module.
pub trait Module: Send + Sync {
    /// Returns the module's name, folded.
    fn name(&self) -> &str;

    /// Returns whether the module is usable as a table without being created.
    ///
    /// An eponymous module - `json_each`, `generate_series`, `pragma_table_info`
    /// - is a name that resolves to a table with no `CREATE VIRTUAL TABLE`
    /// anywhere. It is what makes a table-valued function possible.
    fn eponymous(&self) -> bool {
        false
    }

    /// Returns whether the module may be named by `CREATE VIRTUAL TABLE`.
    ///
    /// An eponymous-only module answers false, which is what stops
    /// `CREATE VIRTUAL TABLE t USING json_each` from making a table whose
    /// shadow tables nothing would ever write.
    fn constructible(&self) -> bool {
        true
    }

    /// Returns the shadow tables a `CREATE VIRTUAL TABLE` must build first.
    fn shadow_tables(&self, _arguments: &ModuleArguments) -> DbResult<Vec<ShadowTable>> {
        Ok(Vec::new())
    }

    /// Connects to a table, whether it is being created or reopened.
    ///
    /// `creating` is true only for the `CREATE VIRTUAL TABLE` that first makes
    /// it. A module that has to write an initial row into a shadow table does
    /// it then; every later open is a connect and writes nothing.
    fn connect(
        &self,
        arguments: &ModuleArguments,
        creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>>;
}

/// One connected virtual table.
pub trait VirtualTable: Send {
    /// Returns the schema the module declares.
    fn declaration(&self) -> &Declaration;

    /// Chooses a plan for one set of constraints.
    fn best_index(&self, info: &mut IndexQuery) -> DbResult<()>;

    /// Opens a cursor over the table.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>>;

    /// Applies one change, returning the rowid an insert allocated.
    ///
    /// The default refuses, which is what a read-only module wants: an eponymous
    /// function has no rows of its own to change.
    fn update(&mut self, _context: &mut Context<'_>, _change: &Change) -> DbResult<Option<i64>> {
        Err(read_only())
    }

    /// Starts a transaction on the module.
    fn begin(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        Ok(())
    }

    /// Flushes anything the module is holding, before the engine commits.
    fn sync(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        Ok(())
    }

    /// Finishes the transaction.
    fn commit(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        Ok(())
    }

    /// Abandons the transaction.
    fn rollback(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        Ok(())
    }

    /// Opens a savepoint, numbered from zero.
    fn savepoint(&mut self, _context: &mut Context<'_>, _number: i32) -> DbResult<()> {
        Ok(())
    }

    /// Releases every savepoint above a number.
    fn release(&mut self, _context: &mut Context<'_>, _number: i32) -> DbResult<()> {
        Ok(())
    }

    /// Rolls back to a savepoint.
    fn rollback_to(&mut self, _context: &mut Context<'_>, _number: i32) -> DbResult<()> {
        Ok(())
    }

    /// Checks the module's own structures, returning what is wrong with them.
    ///
    /// This is what `PRAGMA integrity_check` asks a module. `None` means the
    /// module found nothing wrong; a string is reported as one line of the
    /// check's output, in the module's own words.
    fn integrity(&mut self, _context: &mut Context<'_>) -> DbResult<Option<String>> {
        Ok(None)
    }

    /// Returns the collation one column compares with.
    fn collation(&self, _column: usize) -> Collation {
        Collation::Binary
    }
}

/// A cursor over one virtual table.
pub trait VirtualCursor: Send {
    /// Positions the cursor on the first row of a plan.
    fn filter(&mut self, context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()>;

    /// Moves to the next row.
    fn next(&mut self, context: &mut Context<'_>) -> DbResult<()>;

    /// Returns whether the cursor is past the last row.
    fn eof(&self) -> bool;

    /// Returns one column of the current row.
    fn column(&mut self, context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>>;

    /// Returns the current row's rowid.
    fn rowid(&self) -> DbResult<i64>;

    /// Answers one of the module's auxiliary functions on the current row.
    ///
    /// An auxiliary function is written `f(table, ...)` and reads the cursor
    /// rather than a column: `bm25(docs)` is the whole reason the mechanism
    /// exists. A module that has none refuses by name, which is what makes
    /// `sillyname(docs)` an error rather than a null.
    fn auxiliary(
        &mut self,
        context: &mut Context<'_>,
        name: &[u8],
        arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        let _ = (context, arguments);
        Err(failure(format!(
            "no such function: {}",
            String::from_utf8_lossy(name)
        )))
    }
}

/// Returns the error a module that cannot be written reports.
pub fn read_only() -> DbError {
    DbError::primary(inillucent_base::PrimaryCode::Error).with_detail("table may not be modified")
}

/// Returns a statement error in a module's own words.
pub fn failure(detail: impl Into<String>) -> DbError {
    DbError::primary(inillucent_base::PrimaryCode::Error).with_detail(detail)
}

/// Returns a constraint failure, which is what a rejected row reports.
pub fn constraint(detail: impl Into<String>) -> DbError {
    DbError::primary(inillucent_base::PrimaryCode::Constraint).with_detail(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module that declares nothing still declares something readable.
    #[test]
    fn a_declared_column_carries_its_affinity() {
        let column = DeclaredColumn::visible("value").typed("INTEGER");
        assert_eq!(column.affinity, inillucent_value::Affinity::Integer);
        assert!(!column.hidden);
        assert!(DeclaredColumn::hidden("json").hidden);
    }

    /// A read-only module refuses a write rather than ignoring it.
    #[test]
    fn a_read_only_module_refuses_a_write() {
        assert_eq!(read_only().code(), inillucent_base::PrimaryCode::Error);
    }
}
