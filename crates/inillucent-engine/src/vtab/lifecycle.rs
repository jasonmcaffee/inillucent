//! Telling a module what the transaction did.
//!
//! Invariant: **every module hears about a boundary, in the order the engine
//! crossed it.** A `BEGIN`, a savepoint, a release, a rollback and a commit are
//! each announced; a module that heard about a commit it had not been told to
//! prepare for would be writing against a transaction it thinks is still open.
//!
//! `schema_changed` and `committed_elsewhere` are the two hooks a module needs
//! to cache anything at all: without them a cached manifest is a manifest that
//! goes stale the first time another connection writes.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_catalog::paged::ObjectKind;
use inillucent_ext::vtab::Context;
use inillucent_sql::vtab::{Change, ModuleArguments, ShadowRoot};
use inillucent_value::Value;

use crate::{Outcome, WalLog};

use super::*;

impl crate::ImportedDatabase {
    /// Connects every virtual table the catalog declares.
    ///
    /// **A reopened database has to reach its modules.** `CREATE VIRTUAL TABLE`
    /// connects one and holds it in `virtual_tables`, which lives in memory; a
    /// later open starts with that map empty, so every virtual table answered
    /// "no such table" until this ran. The module trait already anticipated it -
    /// `connect(arguments, creating)` documents `creating` as "true only for
    /// the `CREATE VIRTUAL TABLE` that first makes it. A module that has to
    /// write an initial row into a shadow table does it then; every later open
    /// is a connect and writes nothing." Nobody was calling it with `false`.
    ///
    /// The arguments are read back out of the stored `CREATE VIRTUAL TABLE`
    /// through the ordinary parser rather than remembered separately, because a
    /// second remembered copy of a declaration is a second thing that can
    /// disagree with the file.
    pub(crate) fn reconnect_modules(&mut self) -> DbResult<()> {
        let declarations: Vec<Vec<u8>> = self
            .schema
            .entries
            .iter()
            .filter(|recorded| recorded.entry.kind == ObjectKind::Table)
            .map(|recorded| recorded.entry.sql.clone())
            .collect();
        for sql in declarations {
            let parsed = match inillucent_sql::parser::parse_next_statement(
                &sql,
                0,
                &self.pragmas.limits().borrow(),
            ) {
                Ok(parsed) => parsed,
                Err(_) => continue,
            };
            let inillucent_sql::ast::Statement::CreateVirtualTable {
                name,
                module,
                arguments,
                ..
            } = &parsed.statement
            else {
                continue;
            };
            let name = parsed.ast.text(*name).to_vec();
            let module = parsed.ast.text(*module).to_vec();
            let arguments: Vec<Vec<u8>> = arguments.clone();
            let Some(found) = self.session_state.registry.module(&module) else {
                // A file naming a module this build does not have is a file
                // this build cannot answer for. It is skipped rather than
                // refused so the rest of the database still opens, and the
                // table itself will say "no such table" if anybody asks.
                continue;
            };
            let mut connect = ModuleArguments {
                database: 0,
                schema: b"main".to_vec(),
                table: name.clone(),
                module: module.clone(),
                arguments,
                shadows: Vec::new(),
            };
            for shadow in found.shadow_tables(&connect)? {
                // Looked up in the catalog rows rather than through
                // `table_root`, which answers over the tables the *planner* can
                // see. A shadow table is a real tree either way, and the row is
                // the authority for what it is registered under.
                let owned = shadow.owner.clone().unwrap_or_else(|| name.clone());
                let shadow_name = shadow_table_name(&owned, &shadow.suffix).to_ascii_lowercase();
                let Some(recorded) = self
                    .schema
                    .entries
                    .iter()
                    .find(|recorded| recorded.entry.name.to_ascii_lowercase() == shadow_name)
                else {
                    // **Refused rather than skipped.** A module connected
                    // without one of its shadow tables is a module that will
                    // answer wrongly rather than fail - it may even create a
                    // second copy of the table it could not find - so a shadow
                    // the catalog does not name stops the open and says which
                    // one. This is how a catalog that had lost rows was found:
                    // the connect went ahead without them.
                    return Err(refusal(format!(
                        "the catalog does not name {}, which {} needs",
                        String::from_utf8_lossy(&shadow_name),
                        String::from_utf8_lossy(&name)
                    )));
                };
                connect.shadows.push(ShadowRoot {
                    suffix: shadow.suffix.clone(),
                    root: recorded.root,
                });
            }
            let table = found.connect(&connect, false)?;
            self.session_state.virtual_tables.insert(
                name.to_ascii_lowercase(),
                Connected {
                    table,
                    arguments: connect,
                },
            );
        }
        Ok(())
    }
    /// Applies one change to a virtual table.
    ///
    /// @param name - the table's name
    /// @param change - what to do
    pub(crate) fn change_module(&mut self, name: &[u8], change: &Change) -> DbResult<Option<i64>> {
        let entered = super::stages::clock();
        // **The first write to any module opens the transaction on all of them
        // (task-1932, M2).** Before the flag existed there was no moment at
        // which a module could start buffering, because the engine's only
        // `begin` was at `CREATE VIRTUAL TABLE`. Told before the table is taken
        // out of the map, so the module being written hears it too.
        if !self.session_state.modules_begun.get() {
            self.session_state.modules_begun.set(true);
            self.begin_modules()?;
        }
        let key = name.to_ascii_lowercase();
        let mut connected = self
            .session_state
            .virtual_tables
            .remove(&key)
            .ok_or_else(|| refusal(format!("no such table: {}", String::from_utf8_lossy(name))))?;
        let outcome = {
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
                database: crate::file_of(
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
            let called = super::stages::clock();
            let outcome = connected.table.update(&mut context, change);
            (outcome, super::stages::elapsed(called))
        };
        // The module's own time, and the whole call's, so the difference is the
        // plumbing this function builds per row - the map removal, the `WalLog`,
        // the `WriteStore`, the `Context`. Measured at 0.16 microseconds a row
        // of `extension.fts.build`'s 20.3 (task-2025).
        // Put it back whatever happened: a module that failed a write is still
        // the connected table, and dropping it would make the next statement
        // say the table does not exist.
        self.session_state.virtual_tables.insert(key, connected);
        let (outcome, inside) = outcome;
        let around = super::stages::elapsed(entered);
        super::stages::record(|stages| {
            stages.change = stages.change.saturating_add(around);
            stages.update = stages.update.saturating_add(inside);
        });
        outcome
    }
    /// Applies an `INSERT` into a virtual table by handing the row to the module.
    ///
    /// The engine evaluates the row and the module decides what to do with it,
    /// which is what makes a module a module rather than a table with a funny
    /// name. Only a `VALUES` source is taken: an `INSERT ... SELECT` into a
    /// virtual table is a pipeline feeding a module row by row, and it is
    /// refused by name rather than answered half way.
    ///
    /// @param statement - the bound insert
    /// @param params - the values bound to `?1`, `?2`, ...
    /// Applies an `UPDATE` to a virtual table, one row at a time.
    ///
    /// **A module owns its storage, so an update is a replacement.** The row is
    /// read back through an ordinary query - which is the module's own cursor,
    /// so there is no second reader of its format - the assignments are applied
    /// over it, and the whole row is handed back as one `Change::Update`. That
    /// is what `xUpdate` takes and what an append-only index does with an edit:
    /// tombstone the old version, append the new one.
    ///
    /// The hidden columns are left NULL. They are the module's query interface -
    /// `k`, `vector`, `rank` on a search table - and are not values a row holds.
    ///
    /// @param statement - the bound update
    /// @param keys - the rowid of each row the `WHERE` selected
    /// @param params - the bound parameters
    pub(crate) fn update_module(
        &mut self,
        statement: &inillucent_sql::dml::BoundUpdate,
        keys: &[Vec<inillucent_tree::datum::OwnedDatum>],
        params: &inillucent_exec::physical::Params,
    ) -> DbResult<usize> {
        let table = statement.table.clone();
        let width = table.columns.len();
        // The columns a row actually holds, by declared position, and the query
        // that reads them.
        let visible: Vec<usize> = (0..width)
            .filter(|at| {
                table
                    .column(*at as u16)
                    .is_some_and(|column| !column.hidden)
            })
            .collect();
        let projection = visible
            .iter()
            .filter_map(|at| table.column(*at as u16))
            .map(|column| {
                format!(
                    "\"{}\"",
                    String::from_utf8_lossy(&column.name).replace('"', "\"\"")
                )
            })
            .collect::<Vec<String>>()
            .join(", ");
        let quoted = String::from_utf8_lossy(&table.name).replace('"', "\"\"");
        let mut changed = 0usize;
        for key in keys {
            let Some(&inillucent_tree::datum::OwnedDatum::Int(rowid)) = key.first() else {
                continue;
            };
            let held = self.execute_any(
                &format!("SELECT {projection} FROM \"{quoted}\" WHERE rowid = {rowid}"),
                &inillucent_exec::physical::Params::new(),
            )?;
            let mut values = vec![Value::Null; width];
            if let Some(row) = held.rows.first() {
                for (position, at) in visible.iter().enumerate() {
                    if let (Some(slot), Some(value)) = (values.get_mut(*at), row.get(position)) {
                        *slot = Value::from(&value.borrow()).into_owned()?;
                    }
                }
            }
            for assignment in &statement.assignments {
                // **A constant, because a module's row is not in scope here.**
                // `SET body = body || '!'` reads the row being replaced, which
                // the ordinary write path evaluates against the row image it
                // holds; this path has no such image, and answering with the
                // wrong value would be worse than saying so.
                let value = inillucent_exec::physical::literal_value(&assignment.value, params)
                    .map_err(|_| {
                        refusal(
                            "an UPDATE of a virtual table assigns a constant; \
                             an expression over the row being replaced is not supported",
                        )
                    })?;
                if let Some(slot) = values.get_mut(usize::from(assignment.column)) {
                    *slot = Value::from(&value.borrow()).into_owned()?;
                }
            }
            self.change_module(
                &table.name,
                &Change::Update {
                    old_rowid: Value::Integer(rowid),
                    new_rowid: Value::Integer(rowid),
                    values,
                },
            )?;
            changed = changed.saturating_add(1);
        }
        Ok(changed)
    }
    pub(crate) fn insert_into_module(
        &mut self,
        statement: &inillucent_sql::dml::BoundInsert,
        params: &inillucent_exec::physical::Params,
    ) -> DbResult<Outcome> {
        let inillucent_sql::dml::BoundInsertSource::Values(values) = &statement.source else {
            // **Exit 3, because the statement is written correctly and this
            // engine has not built it (task-1979, section 8.1, gap 5).** It
            // reported the status `syntax` and exit 1, which tells a caller to
            // go and look for a mistake in an `INSERT INTO ft(body) SELECT body
            // FROM src` that has none - and that statement is the FTS5 backfill
            // idiom, so it is the first thing somebody writes after creating
            // the table.
            return Err(refusal("an INSERT ... SELECT into a virtual table")
                .with_unsupported("an INSERT ... SELECT into a virtual table"));
        };
        let width = statement.table.columns.len();
        let mut changed = 0usize;
        for row in values {
            // **Timed per row, because 4.2 ms of `extension.fts.build`'s 8.07
            // was believed to be here and is not (task-2025).** What this arm
            // costs - the owned copy of every text value, the column map, the
            // rowid - was invisible between `execute_statement` and
            // `Fts5Table::add`, so it could be assumed. Measured, the whole arm
            // including `change_module`'s plumbing is 0.45 microseconds a
            // document against the module's own 12.03, and the 4.2 ms was the
            // transaction's commit: FTS5 flushes its doclists at `xSync`, which
            // is outside the `Fts5Table::add` that `BuildStages::whole` times.
            let row_started = super::stages::clock();
            // The statement's own column list decides where each value lands:
            // `INSERT INTO documents(title, body)` supplies two of however many
            // the module declared, and the rest are NULL.
            let mut supplied: Vec<Value<'static>> = Vec::with_capacity(row.len());
            for expr in row {
                supplied.push(
                    Value::from(&inillucent_exec::physical::literal_value(expr, params)?.borrow())
                        .into_owned()?,
                );
            }
            let mut cells = vec![Value::Null; width];
            for (position, column) in statement.columns.iter().enumerate() {
                let inillucent_sql::dml::ColumnSource::Row(at) = column else {
                    continue;
                };
                if let (Some(slot), Some(value)) = (cells.get_mut(position), supplied.get(*at)) {
                    *slot = value.clone();
                }
            }
            // **A supplied rowid is the caller's, not the module's to choose.**
            // This passed `Null` unconditionally, so `INSERT INTO t(rowid, ...)
            // VALUES (?1, ...)` was accepted and the rowid silently discarded -
            // the module allocated its own, and every row came back under a
            // number the caller had not written. It surfaced as a migrated
            // search index whose every ranking was correct and whose every
            // identifier was one too high, because the source numbered its
            // chunks from zero and the module numbered them from one.
            let rowid = match (statement.named_rowid, &statement.rowid) {
                // `INSERT INTO t(rowid, ...)`, which the binder records apart
                // from the columns because a rowid is not one: nothing writes
                // it into the record. It is the only place the value appears -
                // no `ColumnSource` refers to it - so reading `statement.rowid`
                // alone found nothing and the value was dropped on the floor.
                (Some(at), _) => supplied.get(at).cloned().unwrap_or(Value::Null),
                (None, Some(inillucent_sql::dml::ColumnSource::Row(at))) => {
                    supplied.get(*at).cloned().unwrap_or(Value::Null)
                }
                (None, Some(inillucent_sql::dml::ColumnSource::Expr(expr))) => {
                    Value::from(&inillucent_exec::physical::literal_value(expr, params)?.borrow())
                        .into_owned()?
                }
                // Nothing named one, so the module allocates - which is what
                // `Null` asks it for.
                (None, Some(inillucent_sql::dml::ColumnSource::Generated(_)) | None) => Value::Null,
            };
            // **Tried gating this on `change_module`'s `Option<i64>` return -
            // `Some` for an ordinary content row, `None` for a command, on the
            // theory that a command never counts as a change.** That broke
            // three passing cases: SQLite's own `changes()` reports 1 for a
            // *recognised* command (`'pgsz'`, `'rebuild'`,
            // `'integrity-check'`) and only 0 for one SQLite itself refuses -
            // `crates\inillucent-compat\tests\fts5.rs`'s
            // `an_unknown_command_is_refused`. Both engines answer `ok:
            // false` there, so the count is right and the count is what has
            // to answer 0: this loop errors out through the `?` below before
            // `changed` moves, past the `record_changes` call after it, and
            // `changes()` then read whatever the *previous* statement had
            // left - measured at 1, from the schema's last successful
            // single-row insert, where the reference answers 0 for a
            // statement that changed nothing. Recording here, on the way out,
            // is what makes a refused command's `changes()` its own instead
            // of an earlier statement's leftover.
            let built = super::stages::elapsed(row_started);
            let applied = self.change_module(
                &statement.table.name,
                &Change::Insert {
                    rowid,
                    values: cells,
                },
            );
            let whole = super::stages::elapsed(row_started);
            super::stages::record(|stages| {
                stages.rows = stages.rows.saturating_add(1);
                stages.values = stages.values.saturating_add(built);
                stages.whole = stages.whole.saturating_add(whole);
            });
            if let Err(error) = applied {
                self.record_changes(changed as i64, changed as i64);
                return Err(error);
            }
            changed = changed.saturating_add(1);
        }
        // Outside a transaction the statement is its own, so the module flushes
        // and the log commits here; inside one, `commit_batch` does both.
        if self.writing.batch().is_none() {
            self.sync_modules()?;
            self.seal()?;
        }
        // **A module's insert changed rows exactly as much as an ordinary
        // one.** `changes()`/`total_changes()` read `last_changes`/
        // `changed_ever`, and nothing on this path used to touch either -
        // `Outcome::changes` was set correctly and nobody after this call ever
        // read it, since the SQL-level `changes()` and `total_changes()`
        // built-ins read the connection's own counters instead. A module has
        // no triggers of its own, so every row this loop counted is both this
        // statement's own change and the whole of what it changed.
        self.record_changes(changed as i64, changed as i64);
        Ok(Outcome {
            rows: Vec::new(),
            names: std::rc::Rc::new(Vec::new()),
            changes: inillucent_exec::dml::Changes {
                rows: changed,
                ..Default::default()
            },
        })
    }
    /// Flushes every connected module before the engine commits.
    ///
    /// **Once per transaction, not once per row.** FTS5 holds a segment in
    /// memory and writes it out on `sync`, which is the whole reason the method
    /// exists - a sync after every insert turns a bulk load into one segment
    /// flush per document, and `extension.fts.build` measured 436 microseconds
    /// per row against SQLite's 5.5. SQLite syncs its modules at the end of the
    /// statement's transaction and so does this.
    pub(crate) fn sync_modules(&mut self) -> DbResult<()> {
        let names: Vec<Vec<u8>> = self.session_state.virtual_tables.keys().cloned().collect();
        for name in names {
            let Some(mut connected) = self.session_state.virtual_tables.remove(&name) else {
                continue;
            };
            let outcome = {
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
                    // A flush is part of the transaction that asked for it -
                    // at a commit the buffer is cleared immediately after, and
                    // at a savepoint these writes are exactly what a later
                    // `ROLLBACK TO` an earlier point has to be able to undo.
                    undo: Some(self.writing.undo()),
                    uncommitted: self.uncommitted_handle_of(at),
                };
                let store = WriteStore {
                    database: crate::file_of(
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
                connected
                    .table
                    .sync(&mut context)
                    .and_then(|()| connected.table.commit(&mut context))
            };
            self.session_state.virtual_tables.insert(name, connected);
            outcome?;
        }
        Ok(())
    }
    /// Tells every connected module that the transaction was abandoned.
    ///
    /// **The half of the contract the new engine never held up.** The module
    /// trait has had `rollback` since the old engine, and
    /// `inillucent-session/src/vtab.rs` dispatches `Moment::Rollback` to it -
    /// but this engine only ever called `begin`, `sync` and `commit`. A module
    /// that buffers therefore never heard that its buffer was void.
    ///
    /// The file was always right: shadow tables are ordinary trees, so the undo
    /// log put them back. What was wrong was the *connection*, which went on
    /// reading the module's staging area - so one query answered differently
    /// before and after a reopen with nothing written in between, in whichever
    /// direction the abandoned transaction had written. Measured against the
    /// pinned SQLite 3.53.4 before the fix: an abandoned insert left an `fts5`
    /// table reading 2 where the file held 1, and an abandoned delete left it
    /// reading 0 where the file held 2.
    ///
    /// A module that fails to abandon its buffer is not allowed to stop the
    /// rollback - the transaction is going away either way, and a rollback that
    /// could itself fail would leave the connection in a state with no name. The
    /// first failure is remembered and returned once every module has been told.
    ///
    /// @param to_savepoint - the savepoint level, or nothing for the whole
    ///     transaction
    pub(crate) fn rollback_modules(&mut self, to_savepoint: Option<i32>) -> DbResult<()> {
        match to_savepoint {
            Some(level) => self.tell_modules(Moment::RollbackTo(level)),
            None => self.tell_modules(Moment::Rollback),
        }
    }
    /// Tells every connected module that a write transaction has started.
    ///
    /// **Called once per transaction that reaches a module, and before this it
    /// was called once per `CREATE VIRTUAL TABLE` (task-1932, M2).** The only
    /// `begin` in the engine was at creation, so a module that wanted to buffer
    /// a transaction's writes had no moment at which to start one - FTS5's own
    /// `begin` gates on `self.creating` and does nothing afterwards, which is
    /// what a module writes when the hook only ever fires at creation.
    ///
    /// Every connected module is told rather than only the one being written,
    /// which is the same set `sync_modules` flushes at the commit. A module
    /// that begins and is never written syncs nothing.
    pub(crate) fn begin_modules(&mut self) -> DbResult<()> {
        self.tell_modules(Moment::Begin)
    }
    /// Tells every connected module that a savepoint was opened.
    ///
    /// @param level - how many savepoints were already open
    pub(crate) fn savepoint_modules(&mut self, level: i32) -> DbResult<()> {
        self.tell_modules(Moment::Savepoint(level))
    }
    /// Tells every connected module that savepoints above a level were
    /// released.
    ///
    /// @param level - the level being released down to
    pub(crate) fn release_modules(&mut self, level: i32) -> DbResult<()> {
        self.tell_modules(Moment::Release(level))
    }
    /// Tells every connected module that the schema changed under it.
    ///
    /// Infallible, because it is called from `refresh_catalog`, which is called
    /// from paths that have already committed to what they did. A module that
    /// wanted to refuse a schema change would have had to refuse the statement
    /// that made it.
    pub(crate) fn schema_changed_modules(&mut self) {
        let names: Vec<Vec<u8>> = self.session_state.virtual_tables.keys().cloned().collect();
        for name in names {
            if let Some(connected) = self.session_state.virtual_tables.get_mut(&name) {
                connected.table.schema_changed();
            }
        }
    }
    /// Tells every connected module that another process committed.
    ///
    /// Infallible for the same reason: the reload has already happened, and a
    /// module's opinion about it cannot put the pages back.
    pub(crate) fn committed_elsewhere_modules(&mut self) {
        let names: Vec<Vec<u8>> = self.session_state.virtual_tables.keys().cloned().collect();
        for name in names {
            if let Some(connected) = self.session_state.virtual_tables.get_mut(&name) {
                connected.table.committed_elsewhere();
            }
        }
    }
    /// Tells every connected module about one moment.
    ///
    /// **One loop, because the set is always the same set.** A moment told to
    /// some modules and not others is how `begin` came to fire at creation and
    /// nowhere else: there was no loop, only a call beside the thing that
    /// happened.
    ///
    /// A module that fails is not allowed to stop the others being told - a
    /// transaction that is ending is ending either way - so the first failure
    /// is remembered and returned once every module has heard.
    ///
    /// @param moment - what happened
    fn tell_modules(&mut self, moment: Moment) -> DbResult<()> {
        let names: Vec<Vec<u8>> = self.session_state.virtual_tables.keys().cloned().collect();
        let mut first_failure: Option<inillucent_base::DbError> = None;
        for name in names {
            let Some(mut connected) = self.session_state.virtual_tables.remove(&name) else {
                continue;
            };
            let outcome = self.tell_one_module(&mut connected, moment);
            self.session_state.virtual_tables.insert(name, connected);
            if let Err(why) = outcome {
                if first_failure.is_none() {
                    first_failure = Some(why);
                }
            }
        }
        match first_failure {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }
    /// Tells one module the transaction was abandoned.
    ///
    /// Split out so `rollback_modules` can hold the table out of the map across
    /// the call - a module may read its own shadow trees while it discards, and
    /// the map is borrowed for the walk.
    ///
    /// @param connected - the module and the arguments it was connected with
    /// @param to_savepoint - the savepoint level, or nothing for the whole
    ///     transaction
    fn tell_one_module(&mut self, connected: &mut Connected, moment: Moment) -> DbResult<()> {
        let txn = self.current_txn();
        let at = self.schema.ddl_schema;
        let session = self.session_state.session.get();
        let Some(wal) = self.log_of(at) else {
            return Ok(());
        };
        let mut log = WalLog {
            wal,
            txn,
            schema: at,
            wrote: false,
            undo: None,
            uncommitted: self.uncommitted_handle_of(at),
        };
        let store = WriteStore {
            database: crate::file_of(
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
        match moment {
            Moment::Begin => connected.table.begin(&mut context),
            Moment::Rollback => connected.table.rollback(&mut context),
            Moment::RollbackTo(level) => connected.table.rollback_to(&mut context, level),
            Moment::Savepoint(level) => connected.table.savepoint(&mut context, level),
            Moment::Release(level) => connected.table.release(&mut context, level),
        }
    }
}
