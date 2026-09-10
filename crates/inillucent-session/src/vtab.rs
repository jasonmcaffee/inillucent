//! The connection's side of the virtual-table contract.
//!
//! Invariant: a module is connected once per schema generation and kept, not
//! re-made per statement. That is not only a saving - FTS5 reads its
//! configuration out of a shadow table when it connects - it is what makes a
//! module's transaction methods mean anything: `begin`, `savepoint` and
//! `commit` are calls on a *particular* connected table, and a table that were
//! rebuilt between two of them would have forgotten what the first one said.
//!
//! The declarations go into the catalog snapshot rather than being consulted
//! from the binder, so that the binder stays a pure function of the SQL and one
//! catalog generation. A virtual table therefore looks like any other table to
//! everything above this file: it has columns, some of them hidden, and its
//! rows come from a path the planner chose.

use std::collections::BTreeMap;

use inillucent_base::{error, DbResult};
use inillucent_catalog::snapshot::CatalogSnapshot;
use inillucent_ext::registry::Registry;
use inillucent_ext::vtab::{Context, VirtualCursor, VirtualTable};
use inillucent_sql::catalog_view::{ColumnInfo, TableInfo, TableKind};
use inillucent_sql::vtab::{ModuleArguments, ModuleRef, ShadowRoot};
use inillucent_vm::program::VirtualRef;

/// How a connected virtual table is found again.
pub type VirtualKey = (usize, Vec<u8>);

/// Returns the key one reference resolves to.
pub fn key_of(reference: &VirtualRef) -> VirtualKey {
    (reference.database, reference.table.to_ascii_lowercase())
}

/// The virtual tables one connection has connected, and what they need.
#[derive(Default)]
pub struct VirtualTables {
    tables: BTreeMap<VirtualKey, Box<dyn VirtualTable>>,
    shadows: BTreeMap<VirtualKey, Vec<ShadowRoot>>,
}

impl std::fmt::Debug for VirtualTables {
    /// Reports which tables are connected, since none of them can print itself.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VirtualTables")
            .field("connected", &self.tables.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl VirtualTables {
    /// Forgets every connected table, which a schema reload must do.
    ///
    /// A module's declaration and its shadow roots both came from the schema
    /// that has just been replaced, so keeping one across a reload would be
    /// keeping an answer to a question that has changed.
    pub fn clear(&mut self) {
        self.tables.clear();
        self.shadows.clear();
    }

    /// Records where one table's shadow tables live.
    pub fn set_shadows(&mut self, key: VirtualKey, shadows: Vec<ShadowRoot>) {
        self.shadows.insert(key, shadows);
    }

    /// Returns the shadow roots recorded for one table.
    pub fn shadows(&self, key: &VirtualKey) -> Vec<ShadowRoot> {
        self.shadows.get(key).cloned().unwrap_or_default()
    }

    /// Returns whether a table is already connected.
    pub fn is_connected(&self, key: &VirtualKey) -> bool {
        self.tables.contains_key(key)
    }

    /// Records a connected table.
    pub fn insert(&mut self, key: VirtualKey, table: Box<dyn VirtualTable>) {
        self.tables.insert(key, table);
    }

    /// Returns a connected table.
    pub fn get(&self, key: &VirtualKey) -> Option<&dyn VirtualTable> {
        self.tables.get(key).map(|table| table.as_ref())
    }

    /// Takes a connected table out, to be given back after the call.
    pub fn take(&mut self, key: &VirtualKey) -> Option<Box<dyn VirtualTable>> {
        self.tables.remove(key)
    }

    /// Returns the keys of every connected table, in a stable order.
    pub fn keys(&self) -> Vec<VirtualKey> {
        self.tables.keys().cloned().collect()
    }
}

/// Builds the arguments one module is connected with.
pub fn arguments_of(
    reference: &VirtualRef,
    schema: &[u8],
    shadows: Vec<ShadowRoot>,
) -> ModuleArguments {
    ModuleArguments {
        database: reference.database,
        schema: schema.to_vec(),
        table: reference.table.clone(),
        module: reference.module.name.clone(),
        arguments: reference.module.arguments.clone(),
        shadows,
    }
}

/// Returns the shadow tables of one virtual table, by scanning its database.
///
/// A shadow table is an ordinary table whose name is the virtual table's own
/// plus an underscore and a suffix. Finding them by name is what SQLite does
/// and is why `PRAGMA defensive` exists: the naming convention is the only
/// thing that marks them, so anything that can create a table can create
/// something that looks like one.
pub fn shadow_roots(catalog: &CatalogSnapshot, database: usize, table: &[u8]) -> Vec<ShadowRoot> {
    let prefix = {
        let mut prefix = table.to_ascii_lowercase();
        prefix.push(b'_');
        prefix
    };
    let Some(catalog) = catalog.databases.get(database) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for candidate in &catalog.tables {
        if candidate.kind != TableKind::Table || !candidate.folded.starts_with(&prefix) {
            continue;
        }
        let Some(suffix) = candidate.folded.get(prefix.len()..) else {
            continue;
        };
        found.push(ShadowRoot {
            suffix: suffix.to_vec(),
            root: candidate.root,
        });
    }
    found.sort_by(|left, right| left.suffix.cmp(&right.suffix));
    found
}

/// Turns a module's declaration into the columns the binder will see.
pub use inillucent_sql::declare::declared_columns;

/// Returns the table entry one eponymous module provides.
pub fn eponymous_table(registry: &Registry, name: &str) -> DbResult<Option<TableInfo>> {
    let Some(module) = registry.eponymous(name.as_bytes()) else {
        return Ok(None);
    };
    let reference = ModuleRef {
        name: name.as_bytes().to_vec(),
        folded: name.to_ascii_lowercase().into_bytes(),
        arguments: Vec::new(),
    };
    let arguments = ModuleArguments {
        database: inillucent_storage::MAIN_DATABASE,
        schema: b"main".to_vec(),
        table: name.as_bytes().to_vec(),
        module: reference.name.clone(),
        arguments: Vec::new(),
        shadows: Vec::new(),
    };
    let connected = module.connect(&arguments, false)?;
    let declaration = connected.declaration().clone();
    Ok(Some(TableInfo {
        folded: name.to_ascii_lowercase().into_bytes(),
        name: name.as_bytes().to_vec(),
        database: inillucent_storage::MAIN_DATABASE,
        root: 0,
        columns: declared_columns(&declaration),
        rowid_alias: None,
        without_rowid: declaration.without_rowid,
        strict: false,
        autoincrement: false,
        kind: TableKind::Virtual,
        create_sql: Vec::new(),
        view: None,
        triggers: Vec::new(),
        analysed_rows: None,
        indexes: Vec::new(),
        checks: Vec::new(),
        foreign_keys: Vec::new(),
        foreign_key_triggers: Vec::new(),
        module: Some(reference),
    }))
}

/// Connects one virtual table and returns it with the columns it declares.
pub fn connect(
    registry: &Registry,
    reference: &VirtualRef,
    schema: &[u8],
    shadows: Vec<ShadowRoot>,
    creating: bool,
) -> DbResult<(Box<dyn VirtualTable>, Vec<ColumnInfo>, bool)> {
    let Some(module) = registry.module(&reference.module.name) else {
        return Err(error::misuse(format!(
            "no such module: {}",
            String::from_utf8_lossy(&reference.module.name)
        )));
    };
    if creating && !module.constructible() {
        return Err(error::misuse(format!(
            "{} is an eponymous-only module and cannot be created",
            String::from_utf8_lossy(&reference.module.name)
        )));
    }
    let arguments = arguments_of(reference, schema, shadows);
    let table = module.connect(&arguments, creating)?;
    let declaration = table.declaration().clone();
    let columns = declared_columns(&declaration);
    Ok((table, columns, declaration.without_rowid))
}

/// Runs a body with one connected table and a context over the pagers.
///
/// The table is taken out of the map for the call and put back afterwards,
/// error included: a module reads its shadow tables through the very pagers the
/// caller is holding, so the two cannot be borrowed at once, and a table lost
/// to an early return would be one the next statement could not find.
pub fn with_table<T>(
    tables: &mut VirtualTables,
    key: &VirtualKey,
    host: &mut dyn inillucent_ext::vtab::Host,
    limits: &inillucent_base::limits::Limits,
    database: usize,
    body: impl FnOnce(&mut dyn VirtualTable, &mut Context<'_>) -> DbResult<T>,
) -> DbResult<T> {
    let Some(mut taken) = tables.take(key) else {
        return Err(error::misuse("that virtual table is not connected"));
    };
    let outcome = {
        let mut context = Context {
            host,
            database,
            limits,
            catalog: None,
        };
        body(taken.as_mut(), &mut context)
    };
    tables.insert(key.clone(), taken);
    outcome
}

/// A moment in a transaction that a module is entitled to be told about.
///
/// The contract has always declared these methods; only `CREATE VIRTUAL TABLE`
/// used to call them, which was enough for FTS5 and the
/// R-Tree because both write their whole state through `update` and inherit the
/// pager's atomicity. A module that has to *decide* something at a transaction
/// boundary - which commit sequence its changes were published under, or
/// whether its delta log is now long enough to fold in - cannot be written
/// against a contract nothing invokes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moment {
    /// A write transaction has started.
    Begin,
    /// Everything is about to be committed. The last chance to write.
    Sync,
    /// The commit succeeded.
    Commit,
    /// The transaction was undone.
    Rollback,
    /// A savepoint opened, at this depth.
    Savepoint(i32),
    /// A savepoint was released, keeping its changes.
    Release(i32),
    /// The transaction was rolled back to this depth.
    RollbackTo(i32),
}

impl Moment {
    /// Returns whether a failure at this moment can still stop the transaction.
    ///
    /// `Sync` can: it runs before the commit marker, so a module that cannot
    /// finish has a say. `Commit` and `Rollback` cannot - the decision is made
    /// and the pages are written or discarded - so an error there is recorded
    /// by the module and ignored here, because the alternative is a commit that
    /// reports a failure it has already survived.
    fn is_fallible(self) -> bool {
        matches!(self, Moment::Begin | Moment::Sync | Moment::Savepoint(_))
    }
}

/// Tells every connected virtual table that the transaction reached a moment.
///
/// Tables are visited in key order, which is stable, so a module that writes at
/// `Sync` writes in the same order every time and a crash-recovery test sees a
/// deterministic prefix.
/// @param tables - the connection's connected tables
/// @param host - the connection, as a module may see it
/// @param limits - the run-time limits
/// @param moment - what happened
pub fn notify(
    tables: &std::rc::Rc<core::cell::RefCell<VirtualTables>>,
    host: &mut dyn inillucent_ext::vtab::Host,
    limits: &inillucent_base::limits::Limits,
    moment: Moment,
) -> DbResult<()> {
    let keys = match tables.try_borrow() {
        Ok(borrowed) => borrowed.keys(),
        Err(_) => return Ok(()),
    };
    if keys.is_empty() {
        return Ok(());
    }
    let mut failure: Option<inillucent_base::DbError> = None;
    for key in keys {
        let Ok(mut borrowed) = tables.try_borrow_mut() else {
            continue;
        };
        let outcome = with_table(
            &mut borrowed,
            &key,
            host,
            limits,
            key.0,
            |table, context| match moment {
                Moment::Begin => table.begin(context),
                Moment::Sync => table.sync(context),
                Moment::Commit => table.commit(context),
                Moment::Rollback => table.rollback(context),
                Moment::Savepoint(level) => table.savepoint(context, level),
                Moment::Release(level) => table.release(context, level),
                Moment::RollbackTo(level) => table.rollback_to(context, level),
            },
        );
        if let Err(error) = outcome {
            if moment.is_fallible() && failure.is_none() {
                failure = Some(error);
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Opens a cursor on a connected table.
pub fn open_cursor(tables: &VirtualTables, key: &VirtualKey) -> DbResult<Box<dyn VirtualCursor>> {
    let Some(table) = tables.get(key) else {
        return Err(error::misuse("that virtual table is not connected"));
    };
    table.open()
}

/// Answers `best_index` while a statement is being compiled.
///
/// It holds the registry and a shared handle on the connected tables, and
/// nothing else - in particular no pager. A module that needs to read its
/// shadow tables does so when it connects, which happens while the schema is
/// being loaded and a pager is in hand; by the time a statement is compiled the
/// question is answerable from the table alone.
pub struct SessionPlanner {
    registry: std::sync::Arc<Registry>,
    tables: std::rc::Rc<core::cell::RefCell<VirtualTables>>,
}

impl SessionPlanner {
    /// Returns a planner over one connection's registry and tables.
    pub fn new(
        registry: std::sync::Arc<Registry>,
        tables: std::rc::Rc<core::cell::RefCell<VirtualTables>>,
    ) -> SessionPlanner {
        SessionPlanner { registry, tables }
    }
}

impl inillucent_vm::compile::VirtualPlanner for SessionPlanner {
    /// Puts one offer to a module and reads its answer back.
    ///
    /// An eponymous table is connected here and thrown away, because it has no
    /// state: `json_each` is a name, not a thing, and connecting one is
    /// declaring ten columns. A table with a `CREATE` behind it is already
    /// connected - the schema load did it - and is asked in place.
    fn best_index(
        &mut self,
        reference: &VirtualRef,
        query: &mut inillucent_sql::vtab::IndexQuery,
    ) -> DbResult<()> {
        let key = key_of(reference);
        if let Ok(tables) = self.tables.try_borrow() {
            if let Some(table) = tables.get(&key) {
                return table.best_index(query);
            }
        }
        let Some(module) = self.registry.module(&reference.module.name) else {
            return Err(error::misuse(format!(
                "no such module: {}",
                String::from_utf8_lossy(&reference.module.name)
            )));
        };
        let arguments = arguments_of(reference, b"main", Vec::new());
        let table = module.connect(&arguments, false)?;
        table.best_index(query)
    }
}
