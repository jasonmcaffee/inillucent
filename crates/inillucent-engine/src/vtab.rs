//! Virtual tables on the new engine: `CREATE VIRTUAL TABLE`, the shadow store,
//! and driving a module's cursor.
//!
//! Invariant: **a module's shadow tables are ordinary trees.** FTS5 declares its
//! storage as five `CREATE TABLE` statements and the R-Tree as three, so
//! creating a virtual table is creating those tables the way any other
//! `CREATE TABLE` is created - through `define_table`, into the catalog, with
//! their rows in `sqlite_schema` exactly as SQLite records them. That is the
//! TDD's "shadow tables become ordinary rowid trees", and it is why the port is
//! a different *store* rather than a different module: the tokenizers, the
//! ranking, the segment merges and the R-Tree's node logic are not touched.
//!
//! ## Why there are two stores rather than one
//!
//! A read runs under `&self` - `TreeCatalog::virtual_rows` is called from a
//! pipeline that is already holding the catalog - and a write runs under
//! `&mut self`. Rust will not let one type be both, and pretending otherwise
//! with interior mutability would put a `RefCell` on the read path of every
//! query to serve the one statement that writes. So there are two, and the
//! reading one refuses a write by name: a module that tried to write while
//! answering a `SELECT` would be doing something the engine above it did not
//! ask for.
//!
//! ## What a module is not given
//!
//! It is handed the roots of its own shadow tables and nothing else. There is no
//! catalog behind `Context` here and no pager: [`Nowhere`] is what a module gets
//! if it reaches past the store, and it says so rather than answering.

use inillucent_base::error::{refusal, statement_refusal};
use inillucent_base::DbResult;
use inillucent_catalog::paged::{ObjectKind, SchemaEntry};
use inillucent_ext::vtab::{Context, VirtualTable};
use inillucent_sql::plan::AccessPath;
use inillucent_sql::vtab::{FilterPlan, IndexQuery, ModuleArguments, ShadowRoot};
use inillucent_tree::datum::{owned_value, OwnedDatum};
use inillucent_value::Value;

use super::{ImportedDatabase, Outcome, WalLog};

// **The two modules this file is made of (task-1962, A1 step 1).** It was
// 2,266 lines holding three things: the shadow store a module keeps its own
// rows in, the host that connects a module and drives its cursor, and the
// lifecycle a transaction announces to it. The first and the last moved; the
// host is what is left here.
mod lifecycle;
mod shadow;
pub mod stages;

// `Nowhere` was `pub` at `vtab::Nowhere` before the split and stays there:
// `inillucent-ext`'s module contract names it in its own documentation.
pub use shadow::Nowhere;
pub(crate) use shadow::{ReadStore, WriteStore};
// `ModuleStages` is named as `inillucent_engine::ModuleStages` by the gate and by
// `crates/inillucent-compat/tests/module_stages.rs`, beside `StageTimings` which
// lives at the crate root, so `lib.rs` re-exports it there rather than moving it
// behind a path a caller would have to learn.
pub use stages::ModuleStages;

/// A connected virtual table and the shadow roots it was given.
pub struct Connected {
    /// The module's own object.
    pub table: Box<dyn VirtualTable>,
    /// What it was connected with, so a write can hand it back.
    pub arguments: ModuleArguments,
}

/// One point in a transaction a module is told about.
///
/// **Five of them, and before task-1932 the engine told a module about two.**
/// `begin` fired at `CREATE VIRTUAL TABLE` and nowhere else, and `savepoint`
/// and `release` were never called at all - so a module could not buffer a
/// transaction, could not mark a point inside one, and could not be told that a
/// point it had marked was no longer needed. The trait has had all five since
/// the old engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Moment {
    /// A write transaction that reaches a module has started.
    Begin,
    /// The transaction was abandoned.
    Rollback,
    /// The transaction was rolled back to a savepoint, and stays open.
    RollbackTo(i32),
    /// A savepoint was opened at this level.
    Savepoint(i32),
    /// The savepoints above this level were released.
    Release(i32),
}
impl ImportedDatabase {
    /// Creates a virtual table, its shadow tables, and its catalog rows.
    ///
    /// The shadow tables are created through `define_table`, which is the same
    /// path an ordinary `CREATE TABLE` takes - so they are rowid or keyed trees
    /// like any other, they appear in `sqlite_schema` the way SQLite records
    /// them, and the integrity checker walks them.
    ///
    /// @param source - the statement text
    /// @param name_offset - where the table's name starts in it
    /// @param name - the table's name as written
    /// @param module - the module's name as written
    /// @param arguments - the arguments inside the parentheses
    /// @param exists - whether a table of that name is already there
    /// @param if_not_exists - whether the statement said so
    pub(super) fn create_virtual_table(
        &mut self,
        source: &[u8],
        name_offset: u32,
        name: &[u8],
        module: &[u8],
        arguments: &[Vec<u8>],
        exists: bool,
        if_not_exists: bool,
    ) -> DbResult<Outcome> {
        if exists {
            if if_not_exists {
                return Ok(Outcome::empty());
            }
            return Err(refusal(format!(
                "table {} already exists",
                String::from_utf8_lossy(name)
            )));
        }
        // **`SQLITE_ERROR` (primary code 1), not `SQLITE_MISUSE` (21).** SQLite
        // answers "no such module: x" - and, for a module it has but will not
        // let `CREATE VIRTUAL TABLE` construct, the very same message and code
        // rather than a distinct one, which is why `an_eponymous_only_module_
        // cannot_be_created` grades this by code and not by wording: a
        // clearer message here is worth keeping, the number behind it is not
        // this engine's to invent. `refusal` answers `Misuse` unconditionally,
        // which is right for an API contract violation and wrong for "the
        // statement named something that is not there" - the same class of
        // mistake `refused()`'s own doc comment already found and fixed for
        // parser and binder refusals.
        let found = self.session_state.registry.module(module).ok_or_else(|| {
            statement_refusal(format!(
                "no such module: {}",
                String::from_utf8_lossy(module)
            ))
        })?;
        if !found.constructible() {
            return Err(statement_refusal(format!(
                "{} may not be used with CREATE VIRTUAL TABLE",
                String::from_utf8_lossy(module)
            )));
        }
        let mut connect = ModuleArguments {
            database: 0,
            schema: b"main".to_vec(),
            table: name.to_vec(),
            module: module.to_vec(),
            arguments: arguments.to_vec(),
            shadows: Vec::new(),
        };
        for shadow in found.shadow_tables(&connect)? {
            // **A shadow with an owner already exists.** It belongs to another
            // table and is being *read*, not made - see `ShadowTable::owner`.
            // Creating it would put a second, empty copy of somebody else's
            // storage beside the real one.
            if let Some(owner) = &shadow.owner {
                let root = self.existing_shadow_root(owner, &shadow.suffix)?;
                connect.shadows.push(ShadowRoot {
                    suffix: shadow.suffix.clone(),
                    root,
                });
                continue;
            }
            // `%` stands for the virtual table's own name, which is what makes
            // one declaration serve every table the module ever creates.
            let text = shadow
                .create_sql
                .replace('%', &String::from_utf8_lossy(name));
            let shadow_name = shadow_table_name(name, &shadow.suffix);
            let root = self.define_table(&shadow_name, text.into_bytes())?;
            connect.shadows.push(ShadowRoot {
                suffix: shadow.suffix.clone(),
                root,
            });
        }
        let sql = inillucent_catalog::ddl::canonical_sql(
            "CREATE VIRTUAL TABLE",
            source,
            name_offset,
            source.len() as u32,
        );
        self.record(
            0,
            SchemaEntry {
                kind: ObjectKind::Table,
                name: name.to_vec(),
                table: name.to_vec(),
                root: inillucent_pool::PageId::NONE,
                sql,
                stats: Default::default(),
                // Filled by `record` from the identifier it is given.
                tree_id: 0,
            },
        )?;
        self.rebuild_tables()?;
        self.refresh_catalog();
        // The module is connected *after* its shadow tables exist, because a
        // module that writes an initial row writes it into one of them.
        let table = {
            let txn = self.current_txn();
            let at = self.schema.ddl_schema;
            let session = self.session_state.session.get();
            let wal = self
                .log_of(at)
                .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // **The before-images a rollback needs.** Every ordinary write
                // passes `Some(self.writing.undo())`; this path passed `None`, so a
                // virtual table's writes went into the pool with nothing
                // recorded that could put them back. `ROLLBACK` then undid
                // every ordinary table and left the module's shadow trees as
                // the abandoned transaction had made them - so the connection
                // read two rows where the file held one, and a reopen was the
                // only thing that corrected it. The file itself was never
                // wrong: no commit record was written, so recovery ignored
                // the pages. Only the live connection was.
                undo: Some(self.writing.undo()),
                uncommitted: self.uncommitted_handle_of(at),
            };
            let store = WriteStore {
                database: super::file_of(
                    &mut self.storage.database,
                    &mut self.session_state.attached,
                    &mut self.session_state.temps,
                    session,
                    at,
                )?,
                trees: &mut self.schema.trees,
                log: &mut log,
            };
            let mut nowhere = inillucent_ext::vtab::WithStore { store };
            let mut context = Context {
                host: &mut nowhere,
                database: 0,
                limits: &self.pragmas.limits().borrow(),
                catalog: Some(&self.schema.catalog),
            };
            let mut table = found.connect(&connect, true)?;
            table.begin(&mut context)?;
            table.sync(&mut context)?;
            table.commit(&mut context)?;
            table
        };
        self.session_state.virtual_tables.insert(
            name.to_ascii_lowercase(),
            Connected {
                table,
                arguments: connect,
            },
        );
        // Rebuilt again now the module is connected: its *columns* are the
        // module's answer, and until it was connected there was nobody to ask.
        self.rebuild_tables()?;
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Runs a module's cursor to completion and returns its rows.
    ///
    /// @param term - which FROM term of the plan
    /// @param path - the access path the planner chose for it
    /// @param params - the values bound to `?1`, `?2`, ...
    /// Removes a virtual table's own catalog row and the shadow tables it owns.
    ///
    /// **A virtual table used to leave its shadow tables behind for ever
    /// (task-1979, R18).** `DROP TABLE e_v` removed one row and left
    /// `e_v_config`, `e_v_content`, `e_v_delta`, `e_v_gen` and `e_v_state` in
    /// the schema, holding every vector the index had been given, with no
    /// statement that could reach them: their names are the module's, nothing
    /// re-derives them once the virtual table's row is gone, and `VACUUM` copied
    /// them forward. A database that had created and dropped one index carried
    /// its rows for the rest of its life.
    ///
    /// **A shadow another table owns is left alone.** An external content FTS5
    /// index is handed the *source* table's rows as its shadow, and that table
    /// belongs to the application - so only a shadow whose catalog row is named
    /// after this table is removed, which is exactly the set
    /// `create_virtual_table` made.
    ///
    /// @param name - the virtual table's name as written
    pub(super) fn drop_module_table(&mut self, name: &[u8]) -> DbResult<()> {
        let folded = name.to_ascii_lowercase();
        let owned: Vec<Vec<u8>> = match self.session_state.virtual_tables.get(&folded) {
            Some(connected) => connected
                .arguments
                .shadows
                .iter()
                .map(|shadow| shadow_table_name(name, &shadow.suffix).to_ascii_lowercase())
                .filter(|held| *held != folded)
                .collect(),
            None => Vec::new(),
        };
        let at = self.schema.ddl_schema;
        let doomed: Vec<(i64, u32)> = self
            .entries_of(at)
            .iter()
            .filter(|held| {
                let held_name = held.entry.name.to_ascii_lowercase();
                held_name == folded || owned.contains(&held_name)
            })
            .map(|held| (held.rowid, held.root))
            .collect();
        for (rowid, root) in doomed {
            self.forget(rowid)?;
            if root != 0 {
                self.release_tree(root)?;
            }
        }
        // The module is disconnected here rather than left for
        // `reconnect_modules`: the connection holds its state in memory, and a
        // module still connected to trees that have been given back to the free
        // map would answer out of pages another table is about to use.
        self.session_state.virtual_tables.remove(&folded);
        Ok(())
    }

    /// Returns the root of a shadow table another object already owns.
    ///
    /// The catalog rows are the authority, as they are at open time, and a name
    /// that is not there is a refusal: a module that was handed a root it could
    /// not find would either answer nothing or make its own copy, and both are
    /// worse than saying so.
    ///
    /// @param owner - the table the shadows belong to
    /// @param suffix - which shadow
    fn existing_shadow_root(&self, owner: &[u8], suffix: &[u8]) -> DbResult<u32> {
        let wanted = shadow_table_name(owner, suffix).to_ascii_lowercase();
        self.schema
            .entries
            .iter()
            .find(|recorded| recorded.entry.name.to_ascii_lowercase() == wanted)
            .map(|recorded| recorded.root)
            .ok_or_else(|| {
                refusal(format!(
                    "no such table: {}",
                    String::from_utf8_lossy(&wanted)
                ))
            })
    }

    /// Returns whether a name is a shadow table of a connected virtual table.
    ///
    /// **What `PRAGMA defensive` needs to refuse a write (task-1972).**
    /// `Registry::authorize_shadow_write` existed, said exactly this, and had
    /// no caller - so a defensive connection refused nothing, and a shadow
    /// table was an ordinary table that any `INSERT` could rewrite into
    /// something no module ever wrote. That is the class of bug the flag exists
    /// to close: a module reads its own storage trusting that it wrote it.
    ///
    /// The names are derived from what each connected table was handed rather
    /// than guessed from the spelling. `reconnect_modules` connects every
    /// virtual table in the catalog when the database is opened, so the set is
    /// complete from the first statement; deriving it instead from "the text
    /// before the last underscore names a virtual table" would refuse
    /// `docs_backup` beside `docs_data`.
    ///
    /// **An empty suffix is not a shadow.** An external-content FTS5 index is
    /// handed the content table itself under the empty suffix (see
    /// [`shadow_table_name`]), and that table is the application's own.
    ///
    /// @param name - the table a statement is about to write
    pub(crate) fn is_shadow_table(&self, name: &[u8]) -> bool {
        let folded = name.to_ascii_lowercase();
        self.session_state
            .virtual_tables
            .values()
            .any(|connected| shadow_names(&connected.arguments).any(|held| held == folded))
    }

    /// Returns whether a name is a virtual table that owns shadow tables.
    ///
    /// **What `VACUUM` asks before it copies a table's rows (task-1979, R2).**
    /// A virtual table's rows are already in its shadow tables, so copying both
    /// wrote every document twice: the rebuild replayed every `CREATE TABLE`
    /// it found, including the shadow ones, and then the `CREATE VIRTUAL TABLE`
    /// made a second set under the same names - six `sqlite_master` rows became
    /// eleven, the file roughly doubled, and the SQL `dump` produced could not
    /// be replayed because every shadow row was in it twice.
    ///
    /// A virtual table that owns none - an eponymous one, or one whose only
    /// shadow is another table's - answers `false`, and its rows are copied
    /// through the module as before.
    ///
    /// @param name - the table's name, as written
    pub(crate) fn owns_shadow_tables(&self, name: &[u8]) -> bool {
        let folded = name.to_ascii_lowercase();
        let Some(connected) = self.session_state.virtual_tables.get(&folded) else {
            return false;
        };
        shadow_names(&connected.arguments).any(|held| held != folded)
    }

    /// Returns what a module says about its own storage.
    ///
    /// `None` when the name is not a connected virtual table, which is what
    /// `rtreecheck` turns into a refusal rather than into a cheerful `ok`.
    ///
    /// **A second connection to the same table, not the one already open.** A
    /// check takes the table by `&mut` because it may flush what a transaction
    /// staged, and this is reached through the read-only catalog the physical
    /// pass holds. Connecting again is cheap - it reads the arguments and the
    /// shadow roots, both of which are already in hand - and it is also more
    /// honest: what is checked is what a *fresh* open would find, which is the
    /// question a caller running an integrity check is asking.
    ///
    /// @param name - the table's name, as written
    pub(super) fn module_integrity(
        &self,
        name: &[u8],
    ) -> DbResult<inillucent_exec::physical::ModuleIntegrity> {
        use inillucent_exec::physical::ModuleIntegrity;
        let folded = name.to_ascii_lowercase();
        let Some(connected) = self.session_state.virtual_tables.get(&folded) else {
            return Ok(ModuleIntegrity::NoSuchModule);
        };
        let arguments = connected.arguments.clone();
        let Some(found) = self.session_state.registry.module(&arguments.module) else {
            return Ok(ModuleIntegrity::NoSuchModule);
        };
        let mut table = found.connect(&arguments, false)?;
        let store = ReadStore {
            pool: self.storage.database.pool(),
            trees: &self.schema.trees,
        };
        let mut nowhere = inillucent_ext::vtab::WithStore { store };
        let mut context = Context {
            host: &mut nowhere,
            database: 0,
            limits: &self.pragmas.limits().borrow(),
            catalog: Some(&self.schema.catalog),
        };
        table.integrity(&mut context).map(ModuleIntegrity::of)
    }

    /// Produces the rows one virtual table's scan answers with.
    ///
    /// **The five decisions, then the scan (task-1962, A8).** It was 313 lines
    /// with no doc comment: which connection answers, what `best_index` chose,
    /// which columns are worth materialising, which predicates the module did
    /// not promise to apply, and the loop over the cursor. `false` means this
    /// path is not a virtual scan, or names a module nothing can connect.
    ///
    /// **The constraints are pushed down, and they have to be.** A residual the
    /// engine can test itself is a choice; `documents MATCH 'lorem'` is not
    /// one, because `MATCH` is the *module's* operator and the engine has no way
    /// to evaluate it. A scan that answered with every row and left the
    /// predicate to the pipeline returned every document rather than the two
    /// that matched - which is what this did until the probe asked it.
    ///
    /// So the offer goes to `best_index`, the module says which constraints it
    /// will use and in what argument order, and `filter` is given their values.
    /// What the module did *not* take stays in the plan's residual and the
    /// pipeline tests it, which is the contract's whole point: a constraint is
    /// dropped from the residual only when the module promises `omit`, and
    /// `omit` is the module promising rather than the engine assuming.
    ///
    /// @param table - the table's catalog entry
    /// @param path - the access path the planner chose for it
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param needed - what the bound statement reads of this term
    /// @param supplied - a lateral join's per-row constraint values, empty for
    ///   an ordinary scan
    /// @param downstream - what to push the produced rows into
    pub(super) fn rows_of_module(
        &self,
        table: &inillucent_sql::catalog_view::TableInfo,
        path: &AccessPath,
        params: &inillucent_exec::physical::Params,
        needed: &inillucent_sql::bind::ColumnUse,
        supplied: &[inillucent_tree::datum::OwnedDatum],
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<bool> {
        let AccessPath::VirtualScan {
            offer, order_by, ..
        } = path
        else {
            return Ok(false);
        };
        if let Some(answered) = self.eponymous_rows(table, offer, params, downstream)? {
            return Ok(answered);
        }
        // **An eponymous module has nothing in `virtual_tables`**, because
        // nothing ever created it: the name is the table. It is connected here,
        // for this scan, with no arguments - which is all `SeriesModule` and
        // `JsonWalkModule` want, since the arguments a caller wrote arrive as
        // `Eq` constraints on the hidden columns rather than as connect-time
        // text. The connection is not cached: these modules hold no state, and
        // caching one would mean a map that has to be invalidated when the
        // registry changes.
        let held;
        let connected = match self.session_state.virtual_tables.get(&table.folded) {
            Some(connected) => connected,
            None => {
                let Some(connected) = self.connect_eponymous(&table.folded)? else {
                    return Ok(false);
                };
                held = connected;
                &held
            }
        };
        let specs: Vec<inillucent_sql::vtab::ConstraintSpec> =
            offer.iter().map(|held| held.spec).collect();
        let mut query = IndexQuery::new(specs, order_by.clone());
        connected.table.best_index(&mut query)?;
        let plan = FilterPlan {
            index_number: query.index_number,
            index_string: query.index_string.clone(),
            arguments: filter_arguments(&query, offer, params, supplied)?,
        };
        let width = connected.table.declaration().columns.len();
        let shape = RowShape {
            width,
            wanted: columns_wanted(width, needed, offer, &query),
            // Asked of the cursor when the query reads it, or when an
            // unpromised rowid constraint needs it to recheck against, so a
            // module whose rowid is expensive is not asked for one nobody
            // wanted otherwise.
            carries_rowid: needed.rowid || rowid_recheck_needed(offer, &query),
            needed,
            rechecks: rechecks_of(connected, offer, &query, supplied, params)?,
        };
        let mut cursor = connected.table.open()?;
        let store = ReadStore {
            pool: self.storage.database.pool(),
            trees: &self.schema.trees,
        };
        let mut nowhere = inillucent_ext::vtab::WithStore { store };
        let mut context = Context {
            host: &mut nowhere,
            database: 0,
            limits: &self.pragmas.limits().borrow(),
            catalog: Some(&self.schema.catalog),
        };
        self.drive_cursor(
            cursor.as_mut(),
            &mut context,
            &plan,
            &shape,
            params,
            downstream,
        )?;
        Ok(true)
    }

    /// Answers the tables whose rows this connection produces itself.
    ///
    /// A `pragma_*` function, the four that describe statements, and the two
    /// that describe the file. `None` means the table belongs to a module.
    ///
    /// @param table - the table's catalog entry
    /// @param offer - the constraints the planner offered
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param downstream - what to push the produced rows into
    fn eponymous_rows(
        &self,
        table: &inillucent_sql::catalog_view::TableInfo,
        offer: &[inillucent_sql::plan::VirtualConstraint],
        params: &inillucent_exec::physical::Params,
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<Option<bool>> {
        // **A `pragma_*` function is answered by the connection, not a module.**
        // Its rows come from `pragma_rows` - the same function `PRAGMA
        // table_info(t)` runs - because a pragma reads the connection, and a
        // `Module` reaches its storage through a `Context` that has no way to
        // ask one. One implementation, two spellings.
        if table.folded.starts_with(b"pragma_") {
            return self
                .pragma_function_rows(table, offer, params, downstream)
                .map(Some);
        }
        // The same arrangement for the four that describe statements; see
        // `crate::introspect`. Each takes its argument as an `Eq` constraint on
        // its hidden column, which is what makes `bytecode('SELECT 1')` a
        // table-valued function rather than a special form.
        if matches!(
            table.folded.as_slice(),
            b"bytecode" | b"tables_used" | b"sqlite_stmt" | b"completion"
        ) {
            let argument = self.eponymous_argument(offer, params, table)?;
            let rows = match table.folded.as_slice() {
                b"bytecode" => self.bytecode_rows(&argument)?,
                b"tables_used" => self.tables_used_rows(&argument)?,
                b"sqlite_stmt" => self.stmt_rows()?,
                _ => self.completion_rows(&argument)?,
            };
            // The argument came out of an `Eq` on the first hidden column and
            // is the only constraint this answer applied; everything else the
            // planner took out of the residual has to be tested here.
            self.emit_filtered(rows, offer, params, &hidden_columns(table), downstream)?;
            return Ok(Some(true));
        }
        // The same arrangement for the two tables that describe the file; see
        // `crate::inspect`.
        if table.folded == b"dbstat" || table.folded == b"sqlite_dbpage" {
            let mut rows = if table.folded == b"dbstat" {
                self.dbstat_rows()?
            } else {
                self.dbpage_rows()?
            };
            // The hidden `schema` column, which every row of an eponymous
            // table carries and no `SELECT *` reads.
            for row in &mut rows {
                row.push(OwnedDatum::Text(b"main".to_vec()));
            }
            // Nothing here consumed a constraint - the schema qualifier is
            // always `main` and the rows are the whole file - so every offered
            // predicate is the engine's to test.
            self.emit_filtered(rows, offer, params, &[], downstream)?;
            return Ok(Some(true));
        }
        Ok(None)
    }

    /// Walks the cursor, collecting rows and emitting them a batch at a time.
    ///
    /// **A batch at a time, and abandoned when the pipeline says stop.** The
    /// buffer is one batch rather than the whole answer, which is what makes
    /// `SELECT value FROM generate_series(1,10) LIMIT 3` return: without it the
    /// scan ran to 4,294,967,295 rows before the `LIMIT` above it saw a single
    /// one.
    ///
    /// @param cursor - the module's cursor, not yet filtered
    /// @param context - what the module reaches its storage through
    /// @param plan - what `best_index` chose, and the argument values
    /// @param shape - what each row has to carry, and what it is tested against
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param downstream - what to push the produced rows into
    fn drive_cursor(
        &self,
        cursor: &mut dyn inillucent_ext::vtab::VirtualCursor,
        context: &mut Context<'_>,
        plan: &FilterPlan,
        shape: &RowShape<'_>,
        params: &inillucent_exec::physical::Params,
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<()> {
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(inillucent_exec::batch::BATCH_ROWS);
        cursor.filter(context, plan)?;
        while !cursor.eof() {
            let row = read_row(cursor, context, shape, params)?;
            if !passes_rechecks(&row, &shape.rechecks, self.pragmas.case_sensitive_like())? {
                cursor.next(context)?;
                continue;
            }
            rows.push(row);
            if rows.len() >= inillucent_exec::batch::BATCH_ROWS {
                if inillucent_exec::ops::emit_rows(&rows, downstream)?
                    == inillucent_exec::ops::Flow::Stop
                {
                    return Ok(());
                }
                rows.clear();
            }
            cursor.next(context)?;
        }
        if !rows.is_empty() {
            inillucent_exec::ops::emit_rows(&rows, downstream)?;
        }
        Ok(())
    }
}

/// What each row of a virtual scan has to carry, decided before it starts.
///
/// **The five values the scan loop reads, named once (task-1962, A8).** Each
/// was computed inline in `rows_of_module` and then read once per row, which is
/// what made the loop unreadable: the loop's own shape was thirty lines of
/// decisions that had already been made.
struct RowShape<'a> {
    /// How many columns the module declared.
    width: usize,
    /// Which of them something downstream reads. The rest go down as NULL.
    wanted: Vec<bool>,
    /// Whether the row carries the module's rowid, appended after the declared
    /// columns - which is where `plan_stages` puts the slot for a materialised
    /// virtual scan.
    carries_rowid: bool,
    /// What the bound statement reads of this term, for the auxiliary
    /// functions it asks for.
    needed: &'a inillucent_sql::bind::ColumnUse,
    /// The predicates the module did not promise to apply.
    rechecks: Vec<Recheck>,
}

/// Collects one row off the cursor, in the shape the pipeline above expects.
///
/// @param cursor - the module's cursor, on the row
/// @param context - what the module reaches its storage through
/// @param shape - what the row has to carry
/// @param params - the values bound to `?1`, `?2`, ...
fn read_row(
    cursor: &mut dyn inillucent_ext::vtab::VirtualCursor,
    context: &mut Context<'_>,
    shape: &RowShape<'_>,
    params: &inillucent_exec::physical::Params,
) -> DbResult<Vec<OwnedDatum>> {
    let mut row = Vec::with_capacity(shape.width);
    for column in 0..shape.width {
        if !shape.wanted.get(column).copied().unwrap_or(true) {
            row.push(OwnedDatum::Null);
            continue;
        }
        row.push(OwnedDatum::from(cursor.column(context, column)?));
    }
    if shape.carries_rowid {
        row.push(OwnedDatum::Int(cursor.rowid()?));
    }
    // The module's auxiliary functions, in the order `plan_stages` allocated
    // their slots. `bm25(docs)` is the whole reason the mechanism exists, and
    // it reads the cursor rather than a column - so it can only be answered
    // here, while the cursor is still on the row. The arguments after the table
    // are constants of the statement; a call whose arguments varied per row
    // would be a different feature and is not one the modules declare.
    //
    // **They are passed.** An empty list used to go down here, so
    // `bm25(t, 10.0)` ignored its weights and `highlight(t, 0, '[', ']')` could
    // not be written at all.
    for (name, arguments) in &shape.needed.functions {
        let mut values: Vec<Value<'static>> = Vec::with_capacity(arguments.len());
        for argument in arguments {
            values.push(owned_value(&inillucent_exec::physical::literal_value(
                argument, params,
            )?)?);
        }
        row.push(OwnedDatum::from(cursor.auxiliary(context, name, &values)?));
    }
    Ok(row)
}

/// The values `filter` is given, in the order the module asked for them.
///
/// **The caller's arguments win when it has any.** A lateral join has already
/// evaluated them against the outer row - which is the only place they *can* be
/// evaluated - and folding them again here would fold an expression reading a
/// column that is not in scope. See `inillucent_exec::lateral`.
///
/// @param query - what `best_index` answered
/// @param offer - the constraints the planner offered
/// @param params - the values bound to `?1`, `?2`, ...
/// @param supplied - a lateral join's per-row values, empty for an ordinary scan
fn filter_arguments(
    query: &IndexQuery,
    offer: &[inillucent_sql::plan::VirtualConstraint],
    params: &inillucent_exec::physical::Params,
    supplied: &[OwnedDatum],
) -> DbResult<Vec<Value<'static>>> {
    let mut arguments: Vec<Value<'static>> = Vec::new();
    if supplied.is_empty() {
        for position in query.argument_order() {
            let Some(constraint) = offer.get(position) else {
                continue;
            };
            arguments.push(owned_value(&inillucent_exec::physical::literal_value(
                &constraint.value,
                params,
            )?)?);
        }
    } else {
        for value in supplied {
            arguments.push(owned_value(value)?);
        }
    }
    Ok(arguments)
}

/// Whether the module promised to apply the constraint at `position`.
///
/// @param query - what `best_index` answered
/// @param position - the constraint's position in the offer
fn promised(query: &IndexQuery, position: usize) -> bool {
    query
        .usage
        .get(position)
        .map(|usage| usage.omit)
        .unwrap_or(false)
}

/// Whether a row has to carry its rowid for the rechecks to have something to
/// test.
///
/// **A rowid constraint the module did not promise still needs the rowid in the
/// row.** A negative `constraint.spec.column` names the rowid, which is never
/// one of the module's declared columns - `WHERE rowid > 2` on an FTS5 table,
/// or `docs.rowid` in a join's `ON`, both offer one. Forcing the rowid into the
/// row is what lets the recheck test it at `width`, the slot it is appended at,
/// instead of refusing outright because "a produced row does not carry it".
///
/// @param offer - the constraints the planner offered
/// @param query - what `best_index` answered
fn rowid_recheck_needed(
    offer: &[inillucent_sql::plan::VirtualConstraint],
    query: &IndexQuery,
) -> bool {
    offer
        .iter()
        .enumerate()
        .any(|(position, constraint)| !promised(query, position) && constraint.spec.column < 0)
}

/// Which of a module's declared columns are worth asking the cursor for.
///
/// **Only the columns something reads.** `needed` is what the bound statement
/// reads of this term - the same answer a covering index is chosen by - plus
/// the columns the rechecks test, which were taken out of the residual on the
/// module's behalf and so may not be read anywhere else. An opaque answer means
/// the reads could not be enumerated, and then every column is materialised.
///
/// @param width - how many columns the module declared
/// @param needed - what the bound statement reads of this term
/// @param offer - the constraints the planner offered
/// @param query - what `best_index` answered
fn columns_wanted(
    width: usize,
    needed: &inillucent_sql::bind::ColumnUse,
    offer: &[inillucent_sql::plan::VirtualConstraint],
    query: &IndexQuery,
) -> Vec<bool> {
    let mut wanted = vec![needed.opaque; width];
    for slot in &needed.columns {
        if let Some(flag) = wanted.get_mut(usize::from(*slot)) {
            *flag = true;
        }
    }
    for (position, constraint) in offer.iter().enumerate() {
        if promised(query, position) {
            continue;
        }
        if let Ok(column) = usize::try_from(constraint.spec.column) {
            if let Some(flag) = wanted.get_mut(column) {
                *flag = true;
            }
        }
    }
    wanted
}

/// The predicates the module did not promise to apply, which the engine tests.
///
/// **Everything the module did not promise is tested here, per row.**
///
/// The planner takes every offered predicate out of the residual on the
/// optimistic assumption that a later pass puts back the ones the module did
/// not promise to apply - which is what `VirtualChoice::recheck` exists for.
/// There is no such pass on this path, so the recheck happens where the rows
/// are: `omit` is the module promising, and anything else is the engine's to
/// test.
///
/// It is not a tidiness point. The R-Tree takes the constraints it can use to
/// prune its own tree and leaves the rest; without this, `WHERE minX > 0 AND
/// maxX < 100000` answered with all three boxes instead of the one that
/// matches.
///
/// It is built before the scan rather than applied after it, because the scan
/// no longer produces a `Vec` there is an "after" for.
///
/// @param connected - the module's connection, for each column's collation
/// @param offer - the constraints the planner offered
/// @param query - what `best_index` answered
/// @param supplied - a lateral join's per-row values, empty for an ordinary scan
/// @param params - the values bound to `?1`, `?2`, ...
fn rechecks_of(
    connected: &Connected,
    offer: &[inillucent_sql::plan::VirtualConstraint],
    query: &IndexQuery,
    supplied: &[OwnedDatum],
    params: &inillucent_exec::physical::Params,
) -> DbResult<Vec<Recheck>> {
    let width = connected.table.declaration().columns.len();
    let mut rechecks: Vec<Recheck> = Vec::new();
    for (position, constraint) in offer.iter().enumerate() {
        if promised(query, position) {
            continue;
        }
        // A negative column is the rowid. It is not one of the module's
        // declared columns, so it is not in the row at its own position -
        // `rowid_recheck_needed` forces it into the row at `width` instead,
        // appended the same way `needed.rowid` does for a `SELECT` that reads
        // it, and `Collation::Binary` is what a rowid - always an integer -
        // compares under.
        let (column, collation) = match usize::try_from(constraint.spec.column) {
            Ok(column) => (column, connected.table.collation(column)),
            Err(_) => (width, inillucent_value::collation::Collation::Binary),
        };
        rechecks.push((
            column,
            constraint.spec.op,
            recheck_value(constraint, position, supplied, params)?,
            collation,
        ));
    }
    Ok(rechecks)
}

/// One constraint the engine has to test for itself: which column, which
/// operator, against what, under which collation.
type Recheck = (
    usize,
    inillucent_sql::vtab::ConstraintOp,
    OwnedDatum,
    inillucent_value::collation::Collation,
);

/// Returns the positions of a table's hidden columns.
///
/// A table-valued function's arguments arrive as `Eq` constraints on these, so
/// they are the constraints an eponymous answer has already applied by the time
/// it has any rows.
///
/// @param table - the function's catalog entry
fn hidden_columns(table: &inillucent_sql::catalog_view::TableInfo) -> Vec<i32> {
    table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.hidden)
        .filter_map(|(at, _)| i32::try_from(at).ok())
        .collect()
}

/// Returns the value one recheck tests a produced row against.
///
/// **A lateral join already evaluated a correlated constraint, and this is
/// where its answer is read back rather than recomputed.** `docs.rowid = t.id`
/// offers `t.id` as a constraint on the module the same way a literal argument
/// does, and when the module does not promise `omit` for it, the recheck loop
/// used to fold it with `literal_value` regardless - which is the fold
/// `literal_value`'s own doc comment refuses: an expression that reads a
/// column has no value outside the row it was read from. `supplied` is that
/// row's values, one per offered constraint and in the same order `offer`
/// itself is in - see `inillucent_exec::lateral::LateralModule`, which is the
/// one thing that ever fills it - so a position it covers already has an
/// answer and does not need one folded. An ordinary scan supplies nothing, and
/// every constraint's value is a genuine statement-wide constant then, which
/// is exactly what `literal_value` answers.
///
/// @param constraint - the offered constraint being rechecked
/// @param position - its position in `offer`, which is also its position in
///   `supplied`
/// @param supplied - the lateral join's per-row values, empty for an ordinary
///   scan
/// @param params - the values bound to `?1`, `?2`, ...
fn recheck_value(
    constraint: &inillucent_sql::plan::VirtualConstraint,
    position: usize,
    supplied: &[OwnedDatum],
    params: &inillucent_exec::physical::Params,
) -> DbResult<OwnedDatum> {
    match supplied.get(position) {
        Some(value) => Ok(value.clone()),
        None => inillucent_exec::physical::literal_value(&constraint.value, params),
    }
}

/// Reports whether a produced row satisfies the constraints the module left.
///
/// @param row - the row the cursor produced
/// @param rechecks - the column, operator, value and collation of each
/// @param case_sensitive - `PRAGMA case_sensitive_like`, for a `LIKE` recheck
fn passes_rechecks(
    row: &[OwnedDatum],
    rechecks: &[Recheck],
    case_sensitive: bool,
) -> DbResult<bool> {
    for (column, op, wanted, collation) in rechecks {
        let Some(held) = row.get(*column) else {
            return Ok(false);
        };
        if !satisfies(held, *op, wanted, *collation, case_sensitive)? {
            return Ok(false);
        }
    }
    Ok(true)
}

impl ImportedDatabase {
    /// Answers a `pragma_*` table-valued function.
    ///
    /// The argument arrives as an `Eq` constraint on the first hidden column and
    /// the schema qualifier as one on the second, which is exactly what
    /// `bind_table_arguments` produces for `pragma_table_info('t')`. Nothing is
    /// promised to the planner, so the two constraints stay in the residual and
    /// the pipeline tests them again - which is why the produced row carries the
    /// argument and the schema in its own hidden columns rather than dropping
    /// them.
    ///
    /// @param table - the function's catalog entry
    /// @param offer - the constraints the planner pushed down
    /// @param params - the values bound to `?1`, `?2`, ...
    /// Returns the value an eponymous table's first hidden column was given.
    ///
    /// A table-valued function's arguments arrive as `Eq` constraints on the
    /// hidden columns rather than as text at connect time, which is what makes
    /// `bytecode(?)` bindable and `bytecode(t.sql)` joinable.
    ///
    /// @param offer - the constraints the planner is offering
    /// @param params - the statement's bound parameters
    /// @param table - the table being scanned
    fn eponymous_argument(
        &self,
        offer: &[inillucent_sql::plan::VirtualConstraint],
        params: &inillucent_exec::physical::Params,
        table: &inillucent_sql::catalog_view::TableInfo,
    ) -> DbResult<Vec<u8>> {
        let Some(first) = table
            .columns
            .iter()
            .position(|column| column.hidden)
            .and_then(|at| i32::try_from(at).ok())
        else {
            return Ok(Vec::new());
        };
        for constraint in offer {
            if constraint.spec.op != inillucent_sql::vtab::ConstraintOp::Eq
                || constraint.spec.column != first
            {
                continue;
            }
            let value = inillucent_exec::physical::literal_value(&constraint.value, params)?;
            return Ok(pragma_argument_text(&value));
        }
        Ok(Vec::new())
    }

    /// @param downstream - where the batches go
    fn pragma_function_rows(
        &self,
        table: &inillucent_sql::catalog_view::TableInfo,
        offer: &[inillucent_sql::plan::VirtualConstraint],
        params: &inillucent_exec::physical::Params,
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<bool> {
        let Some(pragma) = table.folded.strip_prefix(b"pragma_".as_slice()) else {
            return Ok(false);
        };
        let hidden: Vec<usize> = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.hidden)
            .map(|(at, _)| at)
            .collect();
        let mut argument = OwnedDatum::Null;
        let mut schema = OwnedDatum::Null;
        for constraint in offer {
            if constraint.spec.op != inillucent_sql::vtab::ConstraintOp::Eq {
                continue;
            }
            let Ok(column) = usize::try_from(constraint.spec.column) else {
                continue;
            };
            let value = inillucent_exec::physical::literal_value(&constraint.value, params)?;
            if hidden.first() == Some(&column) {
                argument = value;
            } else if hidden.get(1) == Some(&column) {
                schema = value;
            }
        }
        // The pragma reader takes the argument as the parser's own shape, which
        // is a name or an expression; a value bound at run time is neither, so
        // it is spelled back as the text the reader reads.
        let spelled = match &argument {
            OwnedDatum::Null => None,
            other => Some(inillucent_sql::directive::PragmaArgument::Name(
                pragma_argument_text(other),
            )),
        };
        let Some(answer) = self.pragma_rows(pragma, spelled.as_ref())? else {
            return Ok(false);
        };
        let width = answer.names.len();
        let mut rows: Vec<Vec<OwnedDatum>> = Vec::with_capacity(answer.rows.len());
        for row in answer.rows {
            let mut held = row;
            held.truncate(width);
            while held.len() < width {
                held.push(OwnedDatum::Null);
            }
            held.push(argument.clone());
            held.push(schema.clone());
            rows.push(held);
        }
        // The two hidden columns *are* the arguments and were applied above;
        // every other offered predicate was taken out of the residual on the
        // promise that something would test it, and this is the something.
        let applied: Vec<i32> = hidden
            .iter()
            .filter_map(|at| i32::try_from(*at).ok())
            .collect();
        self.emit_filtered(rows, offer, params, &applied, downstream)?;
        Ok(true)
    }

    /// Emits rows this connection produced itself, testing the predicates the
    /// planner took out of the residual and nothing else applied.
    ///
    /// **The eponymous answers are not module cursors, and that is why they
    /// need this.** `virtual_path` consumes every offered predicate on the
    /// optimistic assumption that the scan puts back what it does not apply;
    /// the module path keeps that promise in `passes_rechecks`, and the
    /// branches that answer out of the connection - `dbstat`, `sqlite_dbpage`,
    /// `pragma_*`, `bytecode`, `tables_used`, `sqlite_stmt`, `completion` -
    /// returned before reaching it. `SELECT name FROM dbstat WHERE
    /// name='main_key'` answered with every page in the file, and
    /// `SELECT name FROM pragma_table_info('t') WHERE name='b'` with every
    /// column.
    ///
    /// @param rows - the rows the connection produced, hidden columns included
    /// @param offer - every constraint the planner pushed down
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param applied - the columns this answer already filtered on
    /// @param downstream - where the batches go
    fn emit_filtered(
        &self,
        rows: Vec<Vec<OwnedDatum>>,
        offer: &[inillucent_sql::plan::VirtualConstraint],
        params: &inillucent_exec::physical::Params,
        applied: &[i32],
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<()> {
        let width = rows.first().map(Vec::len).unwrap_or(0);
        let mut rechecks: Vec<Recheck> = Vec::new();
        for constraint in offer {
            let column = constraint.spec.column;
            if applied.contains(&column) {
                continue;
            }
            // A negative column is the rowid, which these rows do not carry -
            // and answering with every row would be the bug this exists to
            // close, so it is refused instead.
            let Some(at) = usize::try_from(column).ok().filter(|at| *at < width) else {
                if rows.is_empty() {
                    continue;
                }
                return Err(refusal(
                    "the engine cannot test that constraint against this table",
                ));
            };
            rechecks.push((
                at,
                constraint.spec.op,
                inillucent_exec::physical::literal_value(&constraint.value, params)?,
                inillucent_value::collation::Collation::Binary,
            ));
        }
        let mut batch: Vec<Vec<OwnedDatum>> =
            Vec::with_capacity(inillucent_exec::batch::BATCH_ROWS);
        for row in rows {
            if !passes_rechecks(&row, &rechecks, self.pragmas.case_sensitive_like())? {
                continue;
            }
            batch.push(row);
            if batch.len() >= inillucent_exec::batch::BATCH_ROWS {
                if inillucent_exec::ops::emit_rows(&batch, downstream)?
                    == inillucent_exec::ops::Flow::Stop
                {
                    return Ok(());
                }
                batch.clear();
            }
        }
        if !batch.is_empty() {
            inillucent_exec::ops::emit_rows(&batch, downstream)?;
        }
        Ok(())
    }

    /// Connects an eponymous module for the length of one scan.
    ///
    /// `Ok(None)` means the name is not an eponymous module, which is how a
    /// caller with no virtual table of that name at all is told so.
    ///
    /// @param folded - the module's folded name
    fn connect_eponymous(&self, folded: &[u8]) -> DbResult<Option<Connected>> {
        let Some(module) = self.session_state.registry.eponymous(folded) else {
            return Ok(None);
        };
        let arguments = inillucent_sql::vtab::ModuleArguments {
            database: 0,
            schema: b"main".to_vec(),
            table: folded.to_vec(),
            module: folded.to_vec(),
            arguments: Vec::new(),
            shadows: Vec::new(),
        };
        let table = module.connect(&arguments, false)?;
        Ok(Some(Connected { table, arguments }))
    }
}

/// Returns the folded names of one connected table's shadow tables.
///
/// @param arguments - what the module was connected with
fn shadow_names(arguments: &ModuleArguments) -> impl Iterator<Item = Vec<u8>> + '_ {
    arguments
        .shadows
        .iter()
        .filter(|shadow| !shadow.suffix.is_empty())
        .map(|shadow| shadow_table_name(&arguments.table, &shadow.suffix).to_ascii_lowercase())
}

/// Returns the name one shadow table is created under.
///
/// SQLite names them `<table>_<suffix>`, and they are ordinary tables in
/// `sqlite_schema` - which is what makes a module's storage visible to the
/// integrity checker and readable by the other engine.
///
/// @param table - the virtual table's own name
/// @param suffix - the shadow table's suffix
fn shadow_table_name(table: &[u8], suffix: &[u8]) -> Vec<u8> {
    // **An empty suffix is the table itself.** A module that asks for a shadow
    // with no suffix is asking for the named table rather than for one derived
    // from it, which is what an external-content FTS5 index needs: its rows are
    // in `c`, not in `c_content`, and there is no other way to say so through a
    // contract whose whole vocabulary is suffixes.
    if suffix.is_empty() {
        return table.to_vec();
    }
    let mut name = table.to_vec();
    name.push(b'_');
    name.extend_from_slice(suffix);
    name
}

impl ImportedDatabase {}

/// Reports whether one value satisfies one of a module's constraints.
///
/// The comparisons are the dialect's, under the column's own collation.
/// `LIKE`, `GLOB` and `REGEXP` are evaluated here with the same implementations
/// the pipeline's own residual uses, because the engine *can* evaluate them and
/// refusing them turned `SELECT value FROM json_each('[\"aa\"]') WHERE value
/// LIKE 'a%'` - a statement SQLite answers - into an error. `MATCH` is the one
/// that stays refused: it is the module's own operator and has no meaning
/// outside it, so answering as though it had been applied would return rows the
/// query excluded.
///
/// @param held - the value the module produced
/// @param op - the operator the constraint carries
/// @param wanted - the value on the other side
/// @param collation - the column's collation
/// @param case_sensitive - `PRAGMA case_sensitive_like`
fn satisfies(
    held: &OwnedDatum,
    op: inillucent_sql::vtab::ConstraintOp,
    wanted: &OwnedDatum,
    collation: inillucent_value::Collation,
    case_sensitive: bool,
) -> DbResult<bool> {
    use inillucent_sql::vtab::ConstraintOp;
    use std::cmp::Ordering;
    let left = Value::from(&held.borrow()).into_owned()?;
    let right = Value::from(&wanted.borrow()).into_owned()?;
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        // A comparison against NULL is unknown, which excludes the row.
        return Ok(false);
    }
    // A pattern reads both sides as text, which is what the pipeline's own
    // `Pattern` expression does; the ordering below would compare a number to a
    // pattern string and answer nonsense.
    let pattern = match op {
        ConstraintOp::Like => Some(inillucent_exec::scalar::PatternOperator::Like),
        ConstraintOp::Glob => Some(inillucent_exec::scalar::PatternOperator::Glob),
        ConstraintOp::Regexp => Some(inillucent_exec::scalar::PatternOperator::Regexp),
        _ => None,
    };
    if let Some(pattern) = pattern {
        return Ok(inillucent_exec::scalar::matches_pattern(
            pattern,
            &left,
            &right,
            case_sensitive,
        ));
    }
    let order = inillucent_value::compare::compare_values(&left, &right, collation);
    Ok(match op {
        ConstraintOp::Eq | ConstraintOp::Is => order == Ordering::Equal,
        ConstraintOp::Ne | ConstraintOp::IsNot => order != Ordering::Equal,
        ConstraintOp::Lt => order == Ordering::Less,
        ConstraintOp::Le => order != Ordering::Greater,
        ConstraintOp::Gt => order == Ordering::Greater,
        ConstraintOp::Ge => order != Ordering::Less,
        other => {
            return Err(refusal(format!(
                "the module did not apply a {other:?} constraint and the engine cannot"
            )))
        }
    })
}

impl ImportedDatabase {}

/// Renders a bound value as the text a pragma reader reads.
///
/// @param value - the value the statement supplied as the argument
fn pragma_argument_text(value: &OwnedDatum) -> Vec<u8> {
    match value {
        OwnedDatum::Null => Vec::new(),
        OwnedDatum::Int(number) => number.to_string().into_bytes(),
        OwnedDatum::Real(number) => inillucent_value::numeric::real_to_text(*number),
        OwnedDatum::Text(bytes) | OwnedDatum::Blob(bytes) => bytes.clone(),
    }
}
