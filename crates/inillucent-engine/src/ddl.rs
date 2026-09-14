//! DDL on the catalog tree: creating, dropping and altering the schema.
//!
//! Invariant: **a schema change is one transaction's worth of ordinary writes.**
//! The catalog row goes into the catalog tree through `PagedTree::insert`, the
//! pages a new tree needs come off the same free map a split allocates from, and
//! every one of them is described in the log before it happens. So a
//! `CREATE INDEX` that crashes half-way is undone by the same recovery that
//! undoes an `INSERT`, and there is no second write path to keep in step.
//!
//! ## Why the stored text is sliced rather than printed
//!
//! `sqlite_schema.sql` holds the statement from its object name onward with the
//! keywords prefixed - so `create table IF NOT EXISTS "T" ( a )` is stored as
//! `CREATE TABLE "T" ( a )`, keeping the author's spacing, case and quoting and
//! dropping the `IF NOT EXISTS`. That is what SQLite stores, byte for byte, and
//! `inillucent_catalog::ddl::canonical_sql` is the function that already did it
//! for the old engine. Printing the parse back would produce text that
//! round-trips today and stops round-tripping at the first syntax the renderer
//! forgets - and the acceptance for this phase is *digest-equal to SQLite in its
//! effect on `sqlite_schema`*, which is a byte comparison.
//!
//! ## Two identifiers, and why they are not the same number
//!
//! A catalog row's `rootpage` is the page the tree is rooted at **in this
//! file**. The `trees` and `layouts` maps are keyed by something else: an
//! identifier, which for an imported table is the fixture's SQLite root page and
//! for a created one is a number counted up from [`super::FIRST_CREATED_ROOT`].
//! They are different because the physical root is not known until the tree has
//! been built, and the identifier has to be chosen before it - a tree is stamped
//! with its identifier on every page it packs.
//!
//! ## What invalidates a plan
//!
//! Every statement that changes the catalog ends in [`ImportedDatabase::
//! refresh_catalog`], which rebuilds the binder's view, empties the statement
//! cache and bumps the generation. Emptying the cache is the invalidation; the
//! generation is what a test can read to prove it happened.

use std::collections::HashMap;

use inillucent_base::error::{refusal, statement_refusal};
use inillucent_base::DbResult;
use inillucent_catalog::ddl::canonical_sql;
use inillucent_catalog::load::{index_from_create_sql, table_from_create_sql};
use inillucent_catalog::paged::{
    delete_entry, insert_entry, tables_from_entries, ObjectKind, SchemaEntry,
};
use inillucent_catalog::rename;
use inillucent_exec::dml::Changes;
use inillucent_exec::physical::SourceLayout;
use inillucent_pool::PageId;
use inillucent_sql::bind::BoundStatement;
use inillucent_sql::catalog_view::{IndexInfo, StaticCatalog, TableInfo};
use inillucent_sql::directive::{AlterKind, Directive};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::leaf::MiniColumn;
use inillucent_tree::paged::KeyEncoding;
use inillucent_tree::types::ColumnSpec;
use inillucent_tree::PagedTree;
use inillucent_value::collation::Collation;

mod reindex;

use crate::entries::EntrySet;

use super::{
    in_key_order, index_shape, keyed_table_shape, table_shape, ImportedDatabase, Outcome, Recorded,
    WalLog,
};

/// Returns which schema a directive is about, as the binder numbered them.
///
/// Zero - `main` - for every directive that names no database, which includes
/// the transaction-control statements, `ATTACH` and `DETACH` themselves, and
/// every `PRAGMA` this engine answers from the connection rather than from a
/// file.
///
/// @param directive - the bound statement
fn schema_of(directive: &Directive) -> usize {
    match directive {
        Directive::CreateTable { database, .. }
        | Directive::CreateVirtualTable { database, .. }
        | Directive::CreateView { database, .. }
        | Directive::CreateIndex { database, .. }
        | Directive::CreateTrigger { database, .. }
        | Directive::Drop { database, .. }
        | Directive::Alter { database, .. }
        | Directive::Reindex { database, .. }
        | Directive::Vacuum { database, .. }
        | Directive::Analyze { database, .. } => *database,
        // A pragma may name a database and mostly does not; the ones this
        // engine answers from the connection rather than from a file are about
        // `main` either way.
        Directive::Pragma { database, .. } => database.unwrap_or(super::MAIN),
        _ => super::MAIN,
    }
}

/// Returns whether a directive changes the schema the cookie describes.
///
/// The `CREATE`s, the `DROP`s and `ALTER`. A pragma, a transaction control or
/// an `ANALYZE` does not - `ANALYZE` writes statistics rather than a schema -
/// and neither does anything the engine answers from the connection.
///
/// @param directive - the bound statement
fn schema_change(directive: &Directive) -> bool {
    matches!(
        directive,
        Directive::CreateTable { .. }
            | Directive::CreateTableAsSelect { .. }
            | Directive::CreateVirtualTable { .. }
            | Directive::CreateView { .. }
            | Directive::CreateIndex { .. }
            | Directive::CreateTrigger { .. }
            | Directive::Drop { .. }
            | Directive::Alter { .. }
    )
}

/// `(schema, transaction, whether a rollback has anything to undo, log,
/// no-steal handle)` - what [`ImportedDatabase::catalog_write`] hands back.
type CatalogWrite = (
    usize,
    u64,
    bool,
    std::rc::Rc<inillucent_wal::Wal>,
    std::sync::Arc<std::sync::atomic::AtomicU64>,
);

impl ImportedDatabase {
    /// Returns how many times the catalog has changed.
    ///
    /// A plan compiled at one generation is never run at another, because the
    /// statement cache is emptied in the same breath the generation is bumped.
    /// This is here so a test can say so rather than infer it.
    pub fn catalog_generation(&self) -> u64 {
        self.catalog_generation
    }

    /// Returns the catalog's rows, in the order the tree holds them.
    ///
    /// For the acceptance tests, which compare them against SQLite's
    /// `sqlite_schema`.
    pub fn schema_entries(&self) -> Vec<(i64, SchemaEntry)> {
        self.entries
            .iter()
            .map(|held| (held.rowid, held.entry.clone()))
            .collect()
    }

    /// Runs one statement the session carries out itself.
    ///
    /// @param sql - the statement text, which is also the source the stored
    ///   `CREATE` text is sliced out of
    pub fn execute_ddl(&mut self, sql: &str) -> DbResult<Outcome> {
        let BoundStatement::Directive(directive) = self.bind(sql)? else {
            return Err(refusal(format!("{sql} is not a directive")));
        };
        // **Which file the statement is about, from the statement's own
        // words.** `CREATE TABLE aux.t` and `CREATE TEMP TABLE t` each bind to a
        // schema, and the primitives underneath - `allocate_root`, `record`,
        // `build_tree`, `seal` - have to write into it. Put back afterwards, so
        // nothing outside one statement ever observes it as anything but `main`.
        let previous = self.ddl_schema;
        let at = schema_of(&directive);
        // **The temporary database is made by the first statement that needs
        // one.** The binder has already resolved `temp` to schema one, because
        // the name is always in the catalog; this is where the file behind it
        // comes into being, so a connection that never writes a temporary object
        // never makes one.
        if at == super::TEMP {
            self.ensure_temp()?;
        }
        self.ddl_schema = at;
        // **The schema cookie moves once per schema change, and only here.**
        // Not in `refresh_catalog`, which also runs on open and on `ATTACH`:
        // a cookie that rose every time the catalog was re-derived would climb
        // on a database nobody had changed, which is the one thing an
        // application watching it must be able to rule out. Read back by
        // `PRAGMA schema_version`; a directive that failed does not move it.
        let changes_schema = schema_change(&directive);
        // **Statement atomicity for DDL, which only DML had (task-1932, H3).**
        // A directive is several writes - `alter_table` rewrites every catalog
        // row that names the table, rebuilds the connection's schema, then
        // rebuilds the tree - and nothing put the earlier ones back when a
        // later one failed. There was no rollback wrapper here, unlike
        // `write`'s `abandon`, and `record`/`rewrite`/`forget` recorded no
        // before-image at all outside an explicit transaction, so there was
        // nothing to put back with. `next_txn` had not moved either, because
        // `seal` was never reached, so the half-written catalog rows were
        // committed by whatever the next successful statement committed:
        // `ALTER TABLE t ADD COLUMN b INTEGER DEFAULT (no_such_function())`
        // errored and left `PRAGMA table_info(t)` listing a column the tree had
        // no slot for, on disk, across a reopen.
        //
        // The mark and the floor are exactly `write`'s, at exactly its cost -
        // one integer read off a `Vec`'s length - and the reload is what puts
        // the connection's derived schema back in step with the catalog tree
        // the undo has just restored.
        let autocommit = self.batch.get().is_none();
        let mark = self.undo.borrow().len();
        let txn = self.current_txn();
        let outcome = self.run_directive(*directive, sql);
        self.ddl_schema = previous;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let undone = self.undo_to_floor(mark, true, txn);
                if autocommit {
                    self.undo.borrow_mut().clear();
                }
                // **The undo's own failure is the one worth reporting.** A
                // "no such function" describing a database that is now in a
                // state nobody intended is worse than saying so, which is the
                // argument `abandon` already makes for DML.
                return Err(undone.err().unwrap_or(error));
            }
        };
        if autocommit {
            // **Nothing else can abandon what a committed directive wrote.**
            // `seal` has already gone through `commit_across` by here, so the
            // before-images stop being useful - and leaving them would put a
            // committed `CREATE TABLE` inside the reach of the next explicit
            // `ROLLBACK`, which undoes to floor zero.
            self.undo.borrow_mut().clear();
        }
        if changes_schema {
            self.database.bump_schema_cookie();
        }
        Ok(outcome)
    }

    /// Runs one bound directive against the schema `execute_ddl` selected.
    ///
    /// @param directive - the bound statement
    /// @param sql - the statement text, which the DDL cases slice the stored
    ///   `CREATE` text out of
    fn run_directive(&mut self, directive: Directive, sql: &str) -> DbResult<Outcome> {
        let source = sql.as_bytes();
        match directive {
            Directive::CreateTable {
                if_not_exists,
                name,
                name_offset,
                exists,
                ..
            } => self.create_table(source, name_offset, &name, exists, if_not_exists),
            Directive::CreateTableAsSelect {
                if_not_exists,
                name,
                exists,
                create_sql,
                select_sql,
                ..
            } => self.create_table_as_select(&name, exists, if_not_exists, create_sql, &select_sql),
            // **`USING inillucent_hnsw` is sugar for a store plus a promise.**
            // The store is an ordinary `inillucent_search` virtual table over
            // the same HNSW `inillucent-core` builds for the retrieval engine,
            // so there is one implementation of an approximate vector index
            // rather than two. The promise is `follow_vector_indexes`: the
            // write path reports what it stored and removed for a table an
            // index a module owns is built over, and the engine applies both
            // to the module inside the same transaction.
            Directive::CreateIndex {
                using: Some(module),
                name,
                table,
                columns,
                settings,
                exists,
                if_not_exists,
                ..
            } => self.create_vector_index(
                &module,
                &name,
                &table,
                &columns,
                &settings,
                exists,
                if_not_exists,
            ),
            Directive::CreateIndex {
                unique,
                if_not_exists,
                name,
                name_offset,
                table,
                exists,
                ..
            } => self.create_index(
                source,
                name_offset,
                &name,
                &table,
                unique,
                exists,
                if_not_exists,
            ),
            Directive::CreateView {
                if_not_exists,
                name,
                name_offset,
                exists,
                ..
            } => self.create_bodiless(
                "CREATE VIEW",
                ObjectKind::View,
                source,
                name_offset,
                &name,
                &name,
                exists,
                if_not_exists,
            ),
            // **Stored and fired.** It used to be refused,
            // and the refusal was right at the time: this engine could store a
            // trigger and list it in `sqlite_schema` but could not run one, and
            // a database whose triggers never fire is one whose invariants are
            // not being maintained by anything - which the application finds
            // out from its data rather than from an error.
            //
            // `inillucent-exec`'s firing point is what makes it honest, and it
            // is the same mechanism foreign keys are enforced by: the binder
            // turns a `REFERENCES` clause into `CREATE TRIGGER` text, so a
            // written trigger and a key take exactly one path.
            Directive::CreateTrigger {
                name,
                name_offset,
                table,
                exists,
                ..
            } => self.create_bodiless(
                "CREATE TRIGGER",
                ObjectKind::Trigger,
                source,
                name_offset,
                &name,
                &table,
                exists,
                // The directive carries no `if_not_exists` because it does not
                // need one: `bind_create_trigger` has already refused a
                // duplicate that did not say so, and `exists` reaching here at
                // all therefore means the statement did.
                true,
            ),
            Directive::CreateVirtualTable {
                if_not_exists,
                name,
                name_offset,
                module,
                arguments,
                exists,
                ..
            } => self.create_virtual_table(
                source,
                name_offset,
                &name,
                &module,
                &arguments,
                exists,
                if_not_exists,
            ),
            Directive::Drop {
                kind,
                if_exists,
                name,
                exists,
                ..
            } => self.drop_object(kind, &name, exists, if_exists),
            Directive::Alter { table, action, .. } => self.alter_table(source, &table, &action),
            Directive::Analyze { table, .. } => self.analyze(table.as_deref()),
            Directive::Reindex { indexes, .. } => self.reindex(&indexes),
            // **The three transaction statements refuse what SQLite refuses.**
            // `begin_batch`, `commit_batch` and `rollback` are deliberately
            // tolerant - they are called at boundaries by code that does not
            // know whether a transaction is open - and the *statements* are
            // not: SQLite reports all three of these, and before the write
            // path had a statement boundary, this engine reported none of
            // them. It is the same defect three times,
            // and it is how a caller finds out that an `OR ROLLBACK` ended the
            // transaction underneath it: the `COMMIT` that follows has nothing
            // left to commit and has to say so.
            Directive::Begin(_) => {
                if self.batch.get().is_some() {
                    return Err(refusal("cannot start a transaction within a transaction"));
                }
                self.begin_batch();
                Ok(Outcome::empty())
            }
            Directive::Commit => {
                if self.batch.get().is_none() {
                    return Err(refusal("cannot commit - no transaction is active"));
                }
                self.commit_batch()?;
                Ok(Outcome::empty())
            }
            Directive::Pragma {
                ref name,
                ref argument,
                ..
            } => self.pragma(name, argument.as_ref()),
            Directive::Rollback { savepoint } => match savepoint {
                Some(name) => {
                    self.rollback_to(&name)?;
                    Ok(Outcome::empty())
                }
                None => {
                    if self.batch.get().is_none() {
                        return Err(refusal("cannot rollback - no transaction is active"));
                    }
                    self.rollback()?;
                    Ok(Outcome::empty())
                }
            },
            // A `SAVEPOINT` outside a transaction opens one, which is what
            // SQLite does: it is the only way to name a point inside a
            // statement that would otherwise be its own transaction. Recorded
            // as `implicit_transaction` so `release` knows this transaction is
            // the savepoint stack's own, and not one an explicit `BEGIN`
            // opened around it - see that field's own doc comment.
            Directive::Savepoint(name) => {
                if self.batch.get().is_none() {
                    self.begin_batch();
                    self.implicit_transaction.set(true);
                }
                self.savepoint(&name)?;
                Ok(Outcome::empty())
            }
            Directive::Release(name) => {
                self.release(&name)?;
                // **The last savepoint of a transaction the savepoint stack
                // itself opened releases like a `COMMIT`.** A `SAVEPOINT`
                // inside an explicit `BEGIN` also empties `marks` when
                // released, and that transaction stays open for the `COMMIT`
                // that follows - `implicit_transaction` is what tells the two
                // apart.
                if self.marks.is_empty() && self.implicit_transaction.get() {
                    self.commit_batch()?;
                }
                Ok(Outcome::empty())
            }
            // **A second database, opened beside the one this connection was
            // opened on.** Everything above the file - the binder's schema
            // numbering, the planner's tree handles, the write path's choice of
            // log - was already written for more than one; what was missing was
            // a second file to point them at.
            Directive::Attach { file, schema } => {
                self.attach(&file, &schema)?;
                Ok(Outcome::empty())
            }
            Directive::Detach { schema } => {
                self.detach(&schema)?;
                Ok(Outcome::empty())
            }
            // **`VACUUM` folds the log into the file; `VACUUM INTO` writes a
            // verified copy.** Both were refused by name, and `VACUUM INTO` is
            // how a backup is taken - the one form an application cannot do
            // without, because there is no other statement that produces a
            // second file.
            //
            // What `VACUUM` means here is a checkpoint. SQLite's rebuilds the
            // file to reclaim free pages and to defragment; this engine's pages
            // are reclaimed by the free map as they are released, so the part
            // that is left is making everything durable in the file - which is
            // exactly what a checkpoint does. It is not a lie about the space:
            // `PRAGMA freelist_count` says what is free either way.
            // Neither form may run inside an explicit transaction, which is
            // SQLite's rule and is not a formality here either: a checkpoint
            // folds committed frames into the file, and an open transaction's
            // are not committed. The message is SQLite's, and so is the code:
            // `SQLITE_ERROR` (1), not `refusal`'s `SQLITE_MISUSE` (21) -
            // `dml_differential.rs`'s `vacuum_matches_sqlite` grades it.
            Directive::Vacuum { .. } if self.batch.get().is_some() => {
                Err(statement_refusal("cannot VACUUM from within a transaction"))
            }
            Directive::Vacuum { into: None, .. } => {
                self.vacuum_in_place()?;
                Ok(Outcome::empty())
            }
            Directive::Vacuum {
                into: Some(path), ..
            } => {
                let path = String::from_utf8_lossy(&path).into_owned();
                if path.is_empty() {
                    return Err(refusal("VACUUM INTO needs a file to write"));
                }
                // The second statement that names a file of its own, and so
                // the second one a confined process refuses by name rather
                // than leaving to the VFS's result code. See `attach`.
                let path = match inillucent_vfs::confine::process_root() {
                    None => path,
                    Some(root) => match root.admit(&path) {
                        Ok(inside) => inside.to_string_lossy().into_owned(),
                        Err(refused) => return Err(refusal(refused.message())),
                    },
                };
                if std::path::Path::new(&path).exists() {
                    // SQLite's own rule, and the one that makes the statement
                    // safe to put in a backup script: it never overwrites.
                    return Err(refusal("output file already exists"));
                }
                // **A rebuild, not a copy.** `VACUUM INTO` is documented as
                // writing a *compacted* database, and a byte copy reproduces
                // the free pages and the half-empty leaves it was asked to
                // remove - which is what it used to do here.
                self.rebuild_into(std::path::Path::new(&path))?;
                Ok(Outcome::empty())
            }
        }
    }

    /// Returns the identifier the next created tree is registered under.
    fn allocate_root(&mut self) -> DbResult<u32> {
        // The handle, which is what everything above the file names the tree by.
        // Its file-local identifier goes into the catalog row and into every log
        // record, and `record` reads it back with `local_of`.
        Ok(self.allocate_in(self.ddl_schema)?.1)
    }

    /// Returns the rowid the next catalog row takes.
    ///
    /// One past the largest in use, which is what an `INSERT` into a rowid table
    /// with no explicit key does - and SQLite writes its own `sqlite_schema`
    /// rows with exactly that statement.
    fn next_catalog_rowid(&self) -> i64 {
        self.entries_of(self.ddl_schema)
            .iter()
            .map(|held| held.rowid)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Rebuilds the binder's view of the schema and invalidates every plan.
    ///
    /// **The two happen together, always.** A rebuilt catalog with a live plan
    /// cache is the bug this function exists to make unwriteable: the next
    /// execution would take a plan built against the tree that used to be there.
    pub(super) fn refresh_catalog(&mut self) {
        // **The keys are re-planned every time the schema changes**, and this
        // is the only place that can do it: a foreign key records the child's
        // side alone, so the parent's trigger is found by asking every table
        // what it points at - which cannot be answered one `CREATE TABLE` at a
        // time. Doing it here rather than in `create_table` is also what makes
        // `CREATE TABLE child(... REFERENCES parent)` written *before* the
        // parent exists start being enforced when the parent arrives.
        inillucent_sql::foreign_key::plan_schema(
            &mut self.tables,
            b"main",
            &inillucent_base::limits::Limits::default(),
        );
        // **Before the snapshot, because the snapshot is what the planner
        // reads.** An index a module owns is published onto its table's
        // `indexes` list, and a catalog built before that happened describes a
        // table with no such index - so the one path that can use it is never
        // offered.
        self.refresh_vector_indexes();
        // **And the measurements, for the same reason.** `ANALYZE` writes
        // `sqlite_stat1` and then rebuilds the catalog; a snapshot taken before
        // the new rows were read describes tables whose row counts are still
        // the planner's guesses, so the statement right after an `ANALYZE`
        // would be planned as though it had not run.
        self.republish_statistics();
        // **And the imposters, which a rebuild would otherwise drop.** They are
        // not schema objects, so `rebuild_tables` does not know about them;
        // they are put back after it exactly as the module-owned indexes are.
        self.republish_imposters();
        let mut catalog = StaticCatalog::empty();
        // **In attachment order, `main` first.** The binder numbers schemas by
        // their position here and resolves an unqualified name by walking
        // `temp`, then `main`, then the attachments in the order they arrived -
        // which is SQLite's order and is what `main_wins_an_unqualified_name`
        // grades.
        // **`temp` is listed whether or not this connection has made one.** The
        // binder refuses `CREATE TEMP TABLE` when the name is not in the
        // catalog, and the database it would go into is made by the statement
        // that first needs it - so the name has to be there before the schema
        // is. It also fixes every attachment's number: `main`, `temp`, then the
        // attachments, which is SQLite's own layout.
        catalog.databases.push((b"temp".to_vec(), 0));
        for held in &self.attached {
            catalog.databases.push((held.name.clone(), 0));
        }
        for table in &self.tables {
            catalog = catalog.with_table(table.clone());
        }
        catalog = catalog.with_table(self.schema_info.clone());
        catalog = catalog.with_table(super::schema_alias_of(&self.schema_info));
        // Each attached database's own `sqlite_schema`, reachable only when it
        // is qualified: an unqualified `sqlite_schema` is `main`'s, which is
        // what SQLite answers and what the search order above already gives.
        for held in &self.attached {
            catalog = catalog.with_table(held.schema_info.clone());
            catalog = catalog.with_table(super::schema_alias_of(&held.schema_info));
        }
        // A temporary database's catalog answers to `sqlite_temp_schema` and
        // `sqlite_temp_master`, which is how SQLite names it - and registering
        // it as `sqlite_schema` as well would put it *first* in the search order
        // and make an unqualified `sqlite_schema` mean the temporary one.
        // The temporary database's catalog answers to `sqlite_temp_schema` and
        // `sqlite_temp_master`, which is how SQLite names it - and registering
        // it as `sqlite_schema` as well would put it *first* in the search order
        // and make an unqualified `sqlite_schema` mean the temporary one.
        if let Some(held) = self.schema_at(super::TEMP) {
            let temp_schema = super::schema_named(&held.schema_info, b"sqlite_temp_schema");
            let temp_master = super::schema_named(&held.schema_info, b"sqlite_temp_master");
            catalog = catalog.with_table(temp_schema);
            catalog = catalog.with_table(temp_master);
        }
        // **The eponymous modules, last.** `generate_series`, `json_each`,
        // `json_tree` and the `pragma_*` set are names rather than tables: they
        // belong to no database, have no `sqlite_schema` row, and the binder is
        // already written to turn `FROM generate_series(1,10)` into `Eq`
        // constraints on their hidden columns. The one thing missing was
        // anything putting them in the catalog, so every one of them was
        // `no such table`. They go on last, so a real table of the
        // same name shadows the module.
        if self.eponymous.is_empty() {
            self.eponymous = self.eponymous_tables();
        }
        for table in &self.eponymous {
            catalog = catalog.with_eponymous(table.clone());
        }
        self.catalog = catalog;
        self.forget_compiled_statements();
        self.catalog_generation = self.catalog_generation.saturating_add(1);
        // Last, after the catalog a module would read is the new one; see
        // `vtab::schema_changed_modules` (task-1932, M2).
        self.schema_changed_modules();
    }

    /// Returns one `TableInfo` per eponymous module the registry holds.
    ///
    /// The columns come from the module's own `connect`, for the same reason a
    /// `CREATE VIRTUAL TABLE`'s do: what the columns are is the module's answer,
    /// and deriving them anywhere else would be a second implementation of its
    /// declaration that agreed with the module until the day it did not.
    ///
    /// A module whose `connect` fails with no arguments is skipped rather than
    /// reported: it is a module that cannot be used eponymously, which is not
    /// an error in the schema this is refreshing.
    fn eponymous_tables(&self) -> Vec<inillucent_sql::catalog_view::TableInfo> {
        let mut tables = Vec::new();
        for name in self.registry.module_names() {
            let Some(module) = self.registry.eponymous(name.as_bytes()) else {
                continue;
            };
            let arguments = inillucent_sql::vtab::ModuleArguments {
                database: 0,
                schema: b"main".to_vec(),
                table: name.as_bytes().to_vec(),
                module: name.as_bytes().to_vec(),
                arguments: Vec::new(),
                shadows: Vec::new(),
            };
            let Ok(connected) = module.connect(&arguments, false) else {
                continue;
            };
            let declaration = connected.declaration();
            tables.push(inillucent_sql::catalog_view::TableInfo::eponymous(
                name.as_bytes().to_vec(),
                inillucent_sql::declare::declared_columns(declaration),
                inillucent_sql::vtab::ModuleRef {
                    name: name.as_bytes().to_vec(),
                    folded: name.to_ascii_lowercase().into_bytes(),
                    arguments: Vec::new(),
                },
                declaration.without_rowid,
            ));
        }
        // **The `pragma_*` functions are the same mechanism over the pragma
        // set.** They are not registry modules - a module reaches its rows
        // through a `Context`, and a pragma's rows come from the connection
        // itself - so their columns are read straight off
        // `ImportedDatabase::pragma_rows`, which is the same function the
        // directive form runs. Two hidden columns follow, `arg` and `schema`,
        // which is SQLite's shape and is what makes
        // `SELECT * FROM pragma_table_info(name)` over a list of table names a
        // join rather than a loop.
        for name in Self::pragma_function_names() {
            let Some(table) = self.pragma_function_table(name) else {
                continue;
            };
            tables.push(table);
        }
        // **`dbstat` and `sqlite_dbpage` are the engine's too**, and for the
        // same reason: both are questions about the *pages* under every tree,
        // and a `Module` sees a shadow store rather than a pager. See
        // `crate::inspect`.
        // **The four that describe statements are the engine's too**, for the
        // same reason: `bytecode` and `tables_used` compile the SQL they are
        // handed, `sqlite_stmt` reads the statement cache, and `completion`
        // reads the keyword table and the catalog. See `crate::introspect`.
        for (name, columns, hidden) in [
            (
                "bytecode",
                crate::introspect::BYTECODE_COLUMNS,
                &["stmt"][..],
            ),
            (
                "tables_used",
                crate::introspect::TABLES_USED_COLUMNS,
                &["stmt"][..],
            ),
            ("sqlite_stmt", crate::introspect::STMT_COLUMNS, &[][..]),
            (
                "completion",
                crate::introspect::COMPLETION_COLUMNS,
                crate::introspect::COMPLETION_HIDDEN,
            ),
        ] {
            let mut declared: Vec<inillucent_sql::catalog_view::ColumnInfo> = columns
                .iter()
                .map(|held| pragma_column(held.as_bytes(), false))
                .collect();
            declared.extend(
                hidden
                    .iter()
                    .map(|held| pragma_column(held.as_bytes(), true)),
            );
            tables.push(inillucent_sql::catalog_view::TableInfo::eponymous(
                name.as_bytes().to_vec(),
                declared,
                inillucent_sql::vtab::ModuleRef {
                    name: name.as_bytes().to_vec(),
                    folded: name.as_bytes().to_vec(),
                    arguments: Vec::new(),
                },
                false,
            ));
        }
        for (name, columns) in [
            ("dbstat", crate::inspect::DBSTAT_COLUMNS),
            ("sqlite_dbpage", crate::inspect::DBPAGE_COLUMNS),
        ] {
            let mut declared: Vec<inillucent_sql::catalog_view::ColumnInfo> = columns
                .iter()
                .map(|held| pragma_column(held.as_bytes(), false))
                .collect();
            // `schema` is hidden in SQLite too: it selects which attached
            // database is described, and it is not part of a `SELECT *`.
            declared.push(pragma_column(b"schema", true));
            tables.push(inillucent_sql::catalog_view::TableInfo::eponymous(
                name.as_bytes().to_vec(),
                declared,
                inillucent_sql::vtab::ModuleRef {
                    name: name.as_bytes().to_vec(),
                    folded: name.as_bytes().to_ascii_lowercase(),
                    arguments: Vec::new(),
                },
                false,
            ));
        }
        tables.sort_by(|left, right| left.folded.cmp(&right.folded));
        tables
    }

    /// Returns the eponymous table one `pragma_*` function presents.
    ///
    /// `None` when the pragma answers no columns, which is how a name that has
    /// no read form is left out rather than presented as a function that always
    /// finds nothing.
    ///
    /// @param name - the function's name, `pragma_` and the pragma's own
    fn pragma_function_table(&self, name: &str) -> Option<inillucent_sql::catalog_view::TableInfo> {
        let pragma = name.strip_prefix("pragma_")?;
        let shape = self.pragma_rows(pragma.as_bytes(), None).ok()??;
        if shape.names.is_empty() {
            return None;
        }
        let mut columns: Vec<inillucent_sql::catalog_view::ColumnInfo> = shape
            .names
            .iter()
            .map(|held| pragma_column(held.as_bytes(), false))
            .collect();
        columns.push(pragma_column(b"arg", true));
        columns.push(pragma_column(b"schema", true));
        Some(inillucent_sql::catalog_view::TableInfo::eponymous(
            name.as_bytes().to_vec(),
            columns,
            inillucent_sql::vtab::ModuleRef {
                name: name.as_bytes().to_vec(),
                folded: name.as_bytes().to_ascii_lowercase(),
                arguments: Vec::new(),
            },
            false,
        ))
    }

    /// Throws away every statement compiled against the catalog as it was.
    ///
    /// A compiled statement carries decisions the catalog and the connection's
    /// settings made when it was compiled - which tree it reads, which index it
    /// probes, and whether its foreign keys are checked. Anything that changes
    /// one of those has to come through here, or the next execution answers
    /// with the old decision.
    pub(super) fn forget_compiled_statements(&self) {
        self.statements.borrow_mut().clear();
    }

    /// Returns what a catalog-row write on `self.ddl_schema` needs that is not
    /// the borrow of `self.undo` a method cannot hand back - callers still
    /// write their own `WalLog` literal so that borrow stays disjoint from the
    /// `&mut self.database` they take right after, the reason
    /// [`super::file_of`] is a free function too.
    fn catalog_write(&self) -> DbResult<CatalogWrite> {
        let at = self.ddl_schema;
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        Ok((
            at,
            self.current_txn(),
            self.batch.get().is_some(),
            wal,
            self.uncommitted_handle_of(at),
        ))
    }

    /// Writes one row into the catalog tree and records it.
    ///
    /// @param entry - the object to record
    pub(super) fn record(&mut self, root: u32, mut entry: SchemaEntry) -> DbResult<()> {
        let rowid = self.next_catalog_rowid();
        // The statistics come off the tree that was just built rather than from
        // the caller, so there is one place they can be wrong instead of four.
        entry.stats = self.tree_stats(root);
        // And the identifier, for the same reason and from the same argument.
        // The log refers to a tree by this number, so a caller that filled it in
        // itself would be a fifth place it could disagree with the tree it
        // describes - and a catalog naming the wrong tree would send recovery's
        // row records somewhere else.
        let at = self.ddl_schema;
        entry.tree_id = self.local_of(at, root);
        let catalog_handle = self.catalog_handle_of(at);
        {
            let (_, txn, _open, wal, uncommitted) = self.catalog_write()?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // **A before-image whether or not a transaction is open
                // (task-1932, H3).** This was `open.then_some(&self.undo)`, so
                // outside an explicit transaction a catalog write recorded
                // nothing to put back - and `execute_ddl`, which now takes an
                // undo floor the way `write` does, would have had an empty
                // buffer to undo from. A directive is several catalog writes
                // and a failure in a later one has to unwrite the earlier ones.
                // `build_tree_rows` is deliberately still gated: a bulk build's
                // before-images are one record per row of the table, and a
                // freshly built tree has no earlier state to restore to.
                undo: Some(&self.undo),
                uncommitted,
            };
            let tree = self
                .trees
                .get_mut(&catalog_handle)
                .ok_or_else(|| refusal("the catalog tree is not attached"))?;
            let session = self.session.get();
            let database = super::file_of(
                &mut self.database,
                &mut self.attached,
                &mut self.temps,
                session,
                at,
            )?;
            insert_entry(database, tree, &mut log, rowid, &entry)?;
        }
        self.entries_of_mut(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?
            .push(Recorded { rowid, root, entry });
        // A schema change is a write, and a transaction that made one in two
        // files commits both or neither like any other.
        self.touched |= super::schema_bit(at);
        Ok(())
    }

    /// Returns the shape of a tree, for its catalog row.
    ///
    /// A tree the catalog names but the maps do not hold - a view, a trigger -
    /// has no shape, and zero is what "unknown" reads as.
    ///
    /// @param root - the tree's identifier
    pub(super) fn tree_stats(&self, root: u32) -> inillucent_catalog::paged::TreeStats {
        match self.trees.get(&root) {
            Some(tree) => inillucent_catalog::paged::TreeStats {
                first_leaf: tree.first_leaf(),
                leaf_count: tree.leaf_count(),
                row_count: tree.row_count(),
            },
            None => inillucent_catalog::paged::TreeStats::default(),
        }
    }

    /// Makes a table that reads one index's own b-tree, or removes them all.
    ///
    /// **`.imposter`'s subject, and it is a forensic tool rather than a
    /// feature.** An index's entries are the indexed columns followed by the
    /// row's identity, and that is a perfectly good `WITHOUT ROWID` table - so
    /// declaring one over the index's tree lets a person read what the index
    /// actually holds when a query over it is answering wrongly. Nothing is
    /// written to the file: the declaration lives on this connection and goes
    /// with it.
    ///
    /// @param index - the index to read, or nothing to remove every imposter
    /// @param name - the table name to declare it under
    pub fn imposter(&mut self, index: Option<&[u8]>, name: &[u8]) -> DbResult<Option<String>> {
        let Some(index) = index else {
            // **Removed from the catalog as well as from the list.**
            // `refresh_catalog` rebuilds the *snapshot* rather than the tables,
            // so a declaration this connection added stays until it is taken
            // out - and `.imposter off` that left the table queryable would be
            // the one thing the command exists to undo.
            let held = std::mem::take(&mut self.imposters);
            for (info, layout, _) in held {
                self.tables.retain(|table| table.folded != info.folded);
                self.layouts.remove(&layout.tree_key);
                self.trees.remove(&info.root);
            }
            self.refresh_catalog();
            return Ok(None);
        };
        let folded = index.to_ascii_lowercase();
        let Some((owner, declared)) = self.tables.iter().find_map(|table| {
            table
                .indexes
                .iter()
                .find(|held| held.folded == folded)
                .map(|held| (table.clone(), held.clone()))
        }) else {
            return Err(refusal(format!(
                "no such index: \"{}\"",
                String::from_utf8_lossy(index)
            )));
        };
        let Some(tree) = self.trees.get(&declared.root).cloned() else {
            return Err(refusal(format!(
                "the index {} has no tree",
                String::from_utf8_lossy(index)
            )));
        };
        // The entry's columns, in the order the tree holds them: the index's
        // own keys, then what identifies the table row - which is a rowid on an
        // ordinary table and the primary key on a `WITHOUT ROWID` one. The
        // reference calls the rowid `_ROWID_`, and so does this.
        let mut columns: Vec<Vec<u8>> = Vec::new();
        for key in &declared.columns {
            let name = key
                .column
                .and_then(|at| owner.columns.get(usize::from(at)))
                .map(|held| held.name.clone())
                .unwrap_or_else(|| b"expr".to_vec());
            columns.push(name);
        }
        let trailing = super::identity_columns(&owner);
        if trailing.is_empty() {
            columns.push(b"_ROWID_".to_vec());
        } else {
            for declared in &trailing {
                columns.push(
                    owner
                        .columns
                        .get(*declared)
                        .map(|held| held.name.clone())
                        .unwrap_or_else(|| b"key".to_vec()),
                );
            }
        }
        let quoted: Vec<String> = columns
            .iter()
            .map(|held| format!("\"{}\"", String::from_utf8_lossy(held)))
            .collect();
        let sql = format!(
            "CREATE TABLE \"{}\"({},PRIMARY KEY({}))WITHOUT ROWID",
            String::from_utf8_lossy(name),
            quoted.join(","),
            quoted.join(",")
        );
        // A handle of its own, so the imposter's layout does not stand on the
        // index's - two declarations over one tree, and the planner reads a
        // different one for each.
        let root = self.allocate_root()?;
        let mut info = table_from_create_sql(sql.as_bytes(), 0, root)?;
        info.without_rowid = true;
        let layout = SourceLayout {
            tree_key: root,
            slots: (0..columns.len()).map(Some).collect(),
            // There is no rowid: the entry's own columns are the whole row.
            rowid: None,
            identity: (0..columns.len()).collect(),
            types: (0..columns.len())
                .map(|_| inillucent_exec::StaticType::Unknown)
                .collect(),
            width: columns.len(),
            // Read in the tree's order, which is the order the index is in -
            // that is the whole reason for looking at one this way.
            key_columns: (0..columns.len()).collect(),
        };
        self.imposters.retain(|(held, _, _)| held.name != info.name);
        self.imposters.push((info, layout, tree));
        self.refresh_catalog();
        Ok(Some(format!("{sql};")))
    }

    /// Puts the imposter declarations back after a catalog rebuild.
    pub(crate) fn republish_imposters(&mut self) {
        let held = self.imposters.clone();
        for (info, layout, tree) in held {
            self.layouts
                .insert(layout.tree_key, std::rc::Rc::new(layout));
            self.trees.insert(info.root, tree);
            self.tables.retain(|table| table.folded != info.folded);
            self.tables.push(info);
        }
    }

    /// Rewrites every catalog row whose tree has changed shape.
    ///
    /// Called at a checkpoint, which is the moment the statistics can be made
    /// honest cheaply: the file is being made durable anyway, and a row rewrite
    /// per *changed* tree is a handful of small writes rather than one per
    /// insert. Between checkpoints the persisted numbers are the shape as of the
    /// last one, which is exactly what a reader that has just opened the file
    /// gets - and after recovery replays past that point, the checker is what
    /// notices if they no longer describe the tree.
    pub(super) fn refresh_statistics(&mut self) -> DbResult<()> {
        let stale: Vec<(i64, SchemaEntry)> = self
            .entries
            .iter()
            .filter_map(|held| {
                let stats = self.tree_stats(held.root);
                if stats == held.entry.stats {
                    return None;
                }
                let mut moved = held.entry.clone();
                moved.stats = stats;
                Some((held.rowid, moved))
            })
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        for (rowid, entry) in stale {
            self.rewrite(rowid, entry)?;
        }
        // **Sealed here, because nothing else will.** `rewrite` logs under
        // `current_txn()`, which outside a batch and outside a running
        // statement is `next_txn` read but not advanced - `current_txn`'s own
        // doc comment says a fresh one there "is then committed by
        // `ImportedDatabase::seal` at the end of the statement". A checkpoint
        // is not a statement, so nothing called it: the rewrite's records sat
        // in the log under a transaction number nobody ever committed, and
        // `should_replay` never replays an uncommitted transaction's record.
        // A crash mid-writeback of the page that landed on had no redo behind
        // it at all - the same shape of gap `log_free_map_pages` closes for
        // the free map's own pages, reached here because a stale row is
        // rewritten on every checkpoint whose catalog root is small enough
        // that the rewrite lands on the same page a torn write can still hit.
        self.seal()
    }

    /// Replaces one catalog row in place, by rowid.
    ///
    /// A delete and an insert rather than an update: four of the five columns
    /// are variable-length text, and the in-place path is for fixed-width slots.
    ///
    /// @param rowid - the row's key
    /// @param entry - what it should now say
    fn rewrite(&mut self, rowid: i64, entry: SchemaEntry) -> DbResult<()> {
        {
            let (at, txn, _open, wal, uncommitted) = self.catalog_write()?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // See `record` above: the before-image is kept whether or not
                // an explicit transaction is open (task-1932, H3).
                undo: Some(&self.undo),
                uncommitted,
            };
            let catalog_handle = self.catalog_handle_of(at);
            let tree = self
                .trees
                .get_mut(&catalog_handle)
                .ok_or_else(|| refusal("the catalog tree is not attached"))?;
            let session = self.session.get();
            let database = super::file_of(
                &mut self.database,
                &mut self.attached,
                &mut self.temps,
                session,
                at,
            )?;
            delete_entry(database, tree, &mut log, rowid)?;
            insert_entry(database, tree, &mut log, rowid, &entry)?;
        }
        let at = self.ddl_schema;
        for held in self.entries_of_mut(at).into_iter().flatten() {
            if held.rowid == rowid {
                held.entry = entry.clone();
            }
        }
        Ok(())
    }

    /// Removes one catalog row, by rowid.
    ///
    /// @param rowid - the row's key
    fn forget(&mut self, rowid: i64) -> DbResult<()> {
        {
            let (at, txn, _open, wal, uncommitted) = self.catalog_write()?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // See `record` above: the before-image is kept whether or not
                // an explicit transaction is open (task-1932, H3).
                undo: Some(&self.undo),
                uncommitted,
            };
            let catalog_handle = self.catalog_handle_of(at);
            let tree = self
                .trees
                .get_mut(&catalog_handle)
                .ok_or_else(|| refusal("the catalog tree is not attached"))?;
            let session = self.session.get();
            let database = super::file_of(
                &mut self.database,
                &mut self.attached,
                &mut self.temps,
                session,
                at,
            )?;
            delete_entry(database, tree, &mut log, rowid)?;
        }
        let at = self.ddl_schema;
        if let Some(held) = self.entries_of_mut(at) {
            held.retain(|row| row.rowid != rowid);
        }
        Ok(())
    }

    /// Builds one empty tree and registers it.
    ///
    /// @param root - the identifier to register it under
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    /// @param layout - how a bound expression finds its vector
    fn build_tree(
        &mut self,
        root: u32,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        layout: SourceLayout,
    ) -> DbResult<PageId> {
        self.build_tree_from::<Vec<Datum<'_>>>(root, columns, key_columns, layout, &[])
    }

    /// Builds one tree from rows already in key order, and registers it.
    ///
    /// @param root - the identifier to register it under
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    /// @param layout - how a bound expression finds its vector
    /// **Generic over the row's container, and it no longer copies.** It used
    /// to take owned rows and build a `Vec<Vec<Datum>>` of the whole input to
    /// hand the builder - one allocation per row, 4.2 ms of a 48 ms
    /// `CREATE INDEX` at a hundred thousand rows, and pure waste for a caller
    /// whose rows are already borrowed. A caller that holds `OwnedDatum` rows
    /// now does its own borrow, which is where that cost belongs.
    ///
    /// @param rows - the rows, sorted by the key columns
    fn build_tree_from<'d, R: AsRef<[Datum<'d>]>>(
        &mut self,
        root: u32,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        layout: SourceLayout,
        rows: &[R],
    ) -> DbResult<PageId> {
        self.build_tree_rows(
            root,
            columns,
            key_columns,
            layout,
            &inillucent_tree::leaf::RowSlice(rows),
        )
    }

    /// Builds a tree from a row source rather than from a slice of rows.
    ///
    /// The form `CREATE INDEX` uses, so its entries are packed straight out of
    /// the arena they were scanned into. Everything else about it is
    /// [`Self::build_tree_from`], which is now a one-line wrapper over it.
    ///
    /// @param root - the identifier the tree is registered under
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param layout - how the tree's columns map onto the table's
    /// @param rows - the rows, already in key order
    fn build_tree_rows<'d>(
        &mut self,
        root: u32,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        layout: SourceLayout,
        rows: &dyn inillucent_tree::leaf::Rows<'d>,
    ) -> DbResult<PageId> {
        let at = self.ddl_schema;
        let local = self.local_of(at, root);
        let tree = {
            let (_, txn, open, wal, uncommitted) = self.catalog_write()?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                undo: open.then_some(&self.undo),
                uncommitted,
            };
            let session = self.session.get();
            let database = super::file_of(
                &mut self.database,
                &mut self.attached,
                &mut self.temps,
                session,
                at,
            )?;
            PagedTree::bulk_build_rows(
                database,
                Some(&mut log),
                // **The file's own number, not the connection's.** Every record
                // this tree writes carries it, and the log outlives the process
                // that made the handle.
                local,
                columns,
                key_columns,
                rows,
            )?
        };
        let page = tree.root();
        self.trees.insert(root, tree);
        self.layouts.insert(root, std::rc::Rc::new(layout));
        self.touched |= super::schema_bit(at);
        Ok(page)
    }

    /// Gives one tree's pages back to the free map and forgets it.
    ///
    /// @param root - the identifier it is registered under
    fn release_tree(&mut self, root: u32) -> DbResult<()> {
        let at = self.schema_of(root);
        let owner = self
            .schema_file(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        let pages = match self.trees.get(&root) {
            Some(tree) => tree.pages(owner.pool())?,
            None => Vec::new(),
        };
        let txn = self.current_txn();
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        for page in &pages {
            wal.append(txn, inillucent_wal::record::Body::FreePage { page: page.0 })?;
        }
        {
            let session = self.session.get();
            let database = super::file_of(
                &mut self.database,
                &mut self.attached,
                &mut self.temps,
                session,
                at,
            )?;
            for page in pages {
                database.release(page, 1)?;
            }
        }
        self.owner.remove(&root);
        self.trees.remove(&root);
        self.layouts.remove(&root);
        self.covering.remove(&root);
        for roots in self.covering.values_mut() {
            roots.retain(|held| *held != root);
        }
        Ok(())
    }

    /// Returns the transaction a schema change joins.
    ///
    /// Inside a batch it is the batch's; outside one it is the number the
    /// statement in flight already took, and a fresh one when no statement is
    /// in flight - which is then committed by [`ImportedDatabase::seal`] at the
    /// end of the statement.
    ///
    /// **The middle case is the one that was missing.**
    /// [`ImportedDatabase::write`] reads `next_txn` and moves it on at once, so
    /// asking `next_txn` from inside a running statement names the transaction
    /// *after* the one about to commit. Everything logged under that number is
    /// written and never committed. See `statement_txn` for what that cost.
    pub(super) fn current_txn(&self) -> u64 {
        match self.batch.get() {
            Some(held) => held,
            None => self
                .statement_txn
                .get()
                .unwrap_or_else(|| self.next_txn.get()),
        }
    }

    /// Commits a schema change that was its own transaction.
    ///
    /// Inside a batch this does nothing: the batch's `COMMIT` is what makes the
    /// change durable, which is the whole difference between the two groupings.
    pub(super) fn seal(&mut self) -> DbResult<()> {
        if self.batch.get().is_some() {
            return Ok(());
        }
        let txn = self.next_txn.get();
        self.next_txn.set(txn.saturating_add(1));
        let at = self.ddl_schema;
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        wal.append(
            txn,
            inillucent_wal::record::Body::CatalogChange { delta: &[] },
        )?;
        // **Through the same commit as every other statement's.** A schema
        // change is a write, and it takes the participant set the write left
        // behind - which is one file for every `CREATE TABLE` there has ever
        // been, and therefore the single-file path. Committing the log directly
        // here instead would leave that set uncleared, and the *next* autocommit
        // statement would find a schema in it that it had not written: a
        // one-file insert paying for a two-file protocol, and a `Commit` record
        // in a log for a transaction that never touched it.
        let participants = std::mem::take(&mut self.touched) | super::schema_bit(at);
        self.commit_across(txn, participants)
    }

    /// Creates a table, its tree, and the trees its constraints imply.
    ///
    /// @param source - the statement text
    /// @param name_offset - where the table's name starts in it
    /// @param name - the table's name as written
    /// @param exists - whether a table of that name is already there
    /// @param if_not_exists - whether the statement said so
    fn create_table(
        &mut self,
        source: &[u8],
        name_offset: u32,
        name: &[u8],
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
        let sql = canonical_sql("CREATE TABLE", source, name_offset, source.len() as u32);
        self.define_table(name, sql)?;
        self.refresh_catalog();
        // **`sqlite_sequence` comes into being with the first `AUTOINCREMENT`
        // table**, not with the first row - SQLite writes the schema row at
        // `CREATE TABLE` time and the table's own row at its first insert. The
        // binder resolves `BoundInsert::sequence_root` from the catalog, so the
        // table has to be there before any statement against the new table is
        // compiled.
        if self.table_is_autoincrement(name) {
            self.ensure_sequence_table()?;
        }
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Reports whether a table just defined never reuses a key.
    ///
    /// @param name - the table's name as written
    fn table_is_autoincrement(&self, name: &[u8]) -> bool {
        let folded = name.to_ascii_lowercase();
        self.tables
            .iter()
            .any(|held| held.folded == folded && held.autoincrement)
    }

    /// Creates `sqlite_sequence` if the schema has not got one.
    ///
    /// The same shape `ANALYZE` uses for `sqlite_stat1`: a reserved-prefix table
    /// cannot go through the statement path, because `sqlite_` is a name the
    /// binder refuses, and a schema-writing statement that has to defeat the
    /// binder's own rule to run is a rule with a hole in it.
    fn ensure_sequence_table(&mut self) -> DbResult<()> {
        let folded = inillucent_exec::sequence::SEQUENCE_TABLE.to_ascii_lowercase();
        if self.tables.iter().any(|held| held.folded == folded) {
            return Ok(());
        }
        self.define_table(
            inillucent_exec::sequence::SEQUENCE_TABLE,
            inillucent_exec::sequence::SEQUENCE_SQL.as_bytes().to_vec(),
        )?;
        self.refresh_catalog();
        Ok(())
    }

    /// Removes a table's `sqlite_sequence` row, which is what `DROP` does to it.
    ///
    /// @param name - the dropped table's name, as `sqlite_sequence` stores it
    fn forget_sequence(&mut self, name: &[u8]) -> DbResult<()> {
        let folded = inillucent_exec::sequence::SEQUENCE_TABLE.to_ascii_lowercase();
        let Some(root) = self
            .tables
            .iter()
            .find(|held| held.folded == folded)
            .map(|held| held.root)
        else {
            return Ok(());
        };
        let doomed: Vec<i64> = {
            let pool = self.pool_of(root)?;
            let Some(tree) = self.trees.get(&root) else {
                return Ok(());
            };
            let mut keys = Vec::new();
            tree.visit_leaves(pool, &mut |leaf| {
                for row in leaf.live()? {
                    let Some(Datum::Int(rowid)) = row.first().copied() else {
                        continue;
                    };
                    if matches!(row.get(1), Some(Datum::Text(held)) if *held == name) {
                        keys.push(rowid);
                    }
                }
                Ok(true)
            })?;
            keys
        };
        if doomed.is_empty() {
            return Ok(());
        }
        let txn = self.current_txn();
        let at = self.schema_of(root);
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a statement names a database that is not attached"))?;
        let mut log = WalLog {
            wal,
            txn,
            schema: at,
            wrote: false,
            undo: None,
            uncommitted: self.uncommitted_handle_of(at),
        };
        let Some(tree) = self.trees.get_mut(&root) else {
            return Ok(());
        };
        for rowid in doomed {
            tree.delete(&mut self.database, &mut log, &[Datum::Int(rowid)])?;
        }
        Ok(())
    }

    /// Creates a table from a query's shape and fills it from the query.
    ///
    /// **Two statements, in one transaction.** The `CREATE` half stores the text
    /// the binder synthesised from the query's result columns; the fill half is
    /// an ordinary `INSERT INTO name <select>`, compiled against the schema once
    /// the table is in it. Writing the insert here instead would be a second
    /// implementation of what an insert means - and would miss everything the
    /// write path applies, from column affinity to the `NOT NULL` a declared
    /// type carried over.
    ///
    /// @param name - the table's name as written
    /// @param exists - whether a table of that name is already there
    /// @param if_not_exists - whether the statement said so
    /// @param create_sql - the `CREATE TABLE name(...)` text to store
    /// @param select_sql - the query, as the source text it was written as
    fn create_table_as_select(
        &mut self,
        name: &[u8],
        exists: bool,
        if_not_exists: bool,
        create_sql: Vec<u8>,
        select_sql: &[u8],
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
        self.define_table(name, create_sql)?;
        self.refresh_catalog();
        let fill = format!(
            "INSERT INTO \"{}\" {}",
            String::from_utf8_lossy(name).replace('"', "\"\""),
            String::from_utf8_lossy(select_sql)
        );
        let outcome = self.execute_any(&fill, &inillucent_exec::physical::Params::new())?;
        self.seal()?;
        Ok(Outcome {
            rows: Vec::new(),
            names: Vec::new(),
            changes: outcome.changes,
        })
    }

    /// Builds a table's trees and records it, given its stored text.
    ///
    /// Shared by `CREATE TABLE` and by the `sqlite_stat1` that the first
    /// `ANALYZE` brings into being. `ANALYZE` cannot go through the statement
    /// path for it: `sqlite_` is a reserved prefix, and a schema-writing
    /// statement that has to defeat the binder's own rule to run is a rule with
    /// a hole in it.
    ///
    /// @param name - the table's name as it will be stored
    /// @param sql - the `CREATE` text to store and to derive the shape from
    pub(super) fn define_table(&mut self, name: &[u8], sql: Vec<u8>) -> DbResult<u32> {
        let root = self.allocate_root()?;
        let mut info = table_from_create_sql(&sql, 0, root)?;
        info.name = name.to_vec();
        info.folded = name.to_ascii_lowercase();
        let (columns, key_columns, layout) = if info.without_rowid {
            keyed_table_shape(&info)?
        } else {
            let (columns, layout) = table_shape(&info);
            (columns, 1, layout)
        };
        let page = self.build_tree(root, columns, key_columns, layout)?;
        self.record(
            root,
            SchemaEntry {
                kind: ObjectKind::Table,
                name: name.to_vec(),
                table: name.to_vec(),
                root: page,
                sql,
                stats: Default::default(),
                // Filled by `record` from the identifier it is given.
                tree_id: 0,
            },
        )?;

        // The indexes the table's own constraints imply. SQLite writes a
        // `sqlite_autoindex_<table>_<n>` row for each, with a NULL statement,
        // and the reader reconstructs the declaration from the table's text -
        // which is exactly what `table_from_create_sql` has already done here.
        let automatic: Vec<IndexInfo> = info.indexes.clone();
        // **A `WITHOUT ROWID` table's primary key is the table.** There is one
        // b-tree, keyed by the primary key, so SQLite writes no
        // `sqlite_autoindex_` row for it - and building one here would make a
        // second tree holding the same keys, and put a row in `sqlite_schema`
        // that SQLite's does not have. The import already refuses the same
        // shape, for the same reason.
        let primary: Vec<u16> = info.primary_key();
        for (position, index) in automatic.iter().enumerate() {
            if info.without_rowid {
                let key: Vec<u16> = index.columns.iter().filter_map(|key| key.column).collect();
                if key == primary {
                    continue;
                }
            }
            let index_root = self.allocate_root()?;
            let mut index = index.clone();
            index.root = index_root;
            let (columns, layout) = index_shape(&info, &index, index_root);
            let key_columns = columns.len();
            let page = self.build_tree(index_root, columns, key_columns, layout)?;
            self.record(
                index_root,
                SchemaEntry {
                    kind: ObjectKind::Index,
                    name: index.name.clone(),
                    table: name.to_vec(),
                    root: page,
                    sql: Vec::new(),
                    stats: Default::default(),
                    // Filled by `record` from the identifier it is given.
                    tree_id: 0,
                },
            )?;
            self.covering.entry(root).or_default().push(index_root);
            if let Some(slot) = info.indexes.get_mut(position) {
                slot.root = index_root;
            }
        }
        self.rebuild_tables()?;
        Ok(root)
    }

    /// Creates an index, fills it from the table, and records it.
    ///
    /// The fill is a bottom-up bulk build rather than a per-key insert: the
    /// entries are projected out of the table tree, sorted once, and packed left
    /// to right. That is the TDD's bulk builder and it is what the `schema`
    /// family's bar is a claim about.
    ///
    /// @param source - the statement text
    /// @param name_offset - where the index's name starts in it
    /// @param name - the index's name as written
    /// @param table - the table it indexes
    /// @param unique - whether `UNIQUE` was written
    /// @param exists - whether an index of that name is already there
    /// @param if_not_exists - whether the statement said so
    #[allow(clippy::too_many_arguments)]
    fn create_index(
        &mut self,
        source: &[u8],
        name_offset: u32,
        name: &[u8],
        table: &[u8],
        unique: bool,
        exists: bool,
        if_not_exists: bool,
    ) -> DbResult<Outcome> {
        if exists {
            if if_not_exists {
                return Ok(Outcome::empty());
            }
            return Err(refusal(format!(
                "index {} already exists",
                String::from_utf8_lossy(name)
            )));
        }
        let keywords = if unique {
            "CREATE UNIQUE INDEX"
        } else {
            "CREATE INDEX"
        };
        let sql = canonical_sql(keywords, source, name_offset, source.len() as u32);
        let folded = table.to_ascii_lowercase();
        let position = self
            .tables
            .iter()
            .position(|held| held.folded == folded)
            .ok_or_else(|| refusal(format!("no such table: {}", String::from_utf8_lossy(table))))?;
        let owner = self
            .tables
            .get(position)
            .cloned()
            .ok_or_else(|| refusal("the table that was just found is gone"))?;
        let root = self.allocate_root()?;
        let index = index_from_create_sql(&sql, &owner, root)?;
        let (columns, layout) = index_shape(&owner, &index, root);
        let key_columns = columns.len();
        // The entries are scanned into an arena, sorted by a radix pass over
        // a fixed-width prefix of the tree's own key encoding, and packed
        // straight out of it. The encoding and the collations are the *tree's*,
        // so the order the sort produces is the order the tree will be searched
        // in - which is the invariant `in_key_order` exists to defend, taken
        // here rather than re-derived.
        let encoding = KeyEncoding::choose(&columns, key_columns);
        let collations: Vec<Collation> = columns
            .iter()
            .take(key_columns)
            .map(|spec| spec.collation)
            .collect();
        // And the directions, for the same reason: a `DESC` key column is
        // stored descending, so the sort that packs the tree has to produce
        // that order rather than the ascending one and let the reader cope.
        let directions: Vec<bool> = columns
            .iter()
            .take(key_columns)
            .map(|spec| spec.descending)
            .collect();
        let scanned = std::time::Instant::now();
        // **A partial index and an index on an expression are filled by a
        // query; everything else is filled by a scan.** The scan reads columns
        // out of the leaves and is what the `schema` family measures; it has no
        // way to evaluate `lower(a)` or `WHERE b > 5`, and teaching it to would
        // put an expression evaluator on the path of every ordinary
        // `CREATE INDEX`. The binder, planner and executor already evaluate
        // both, so the two forms that need them are built by asking them - the
        // same shape `create_vector_index` uses to backfill a store.
        let computed =
            index.partial_sql.is_some() || index.columns.iter().any(|key| key.expr_sql.is_some());
        let mut entries = if computed {
            self.index_entries_by_query(
                &owner,
                &index,
                key_columns,
                encoding,
                &collations,
                &directions,
            )?
        } else {
            self.index_entries(
                &owner,
                &index,
                key_columns,
                encoding,
                &collations,
                &directions,
            )?
        };
        let scan = scanned.elapsed().as_nanos();
        let sorted = std::time::Instant::now();
        let order = entries.order();
        let sort = sorted.elapsed().as_nanos();
        let checked = std::time::Instant::now();
        if unique {
            refuse_duplicates(&entries, &order, &owner, &index, key_columns)?;
        }
        let uniqueness = checked.elapsed().as_nanos();
        // The sort is finished and the uniqueness check with it, so everything
        // only they needed goes back before the pack - which is the half of the
        // statement the high-water mark is taken during.
        entries.release_sort_scratch();
        // **No flat run any more, and the stage that made one reads zero.**
        // The packer used to need a slice, so the arena was flattened into a
        // `Vec<Datum>` in key order and sliced into a `Vec<&[Datum]>` - 6.4 MiB
        // of copies at a hundred thousand rows. `EntrySet::in_order` is a view
        // over the arena and the order vector, and the packer indexes it. The
        // timer stays so that four runs of gate output either side of the change
        // are comparable line for line.
        let flattened = std::time::Instant::now();
        let source = entries.in_order(&order);
        let flatten = flattened.elapsed().as_nanos();
        let packed = std::time::Instant::now();
        // **The catalog row names the new tree before the tree is filled
        // (task-1932).** A recovery derives a tree's shape from the catalog
        // rows it has replayed, and skips a record naming a tree no row names -
        // which is right for a tree a rebuild has dropped and wrong for one
        // whose row has not gone past yet. Writing the row first is what keeps
        // those two apart: after this, every record describing a page of this
        // tree follows a row that names it. The row below is superseded by the
        // one at the end of this statement, in the same transaction and before
        // anything can read either, so the only thing it changes is the order
        // two records reach the log in. `rebuild_index` does the same, for the
        // crash `reindex_crash.rs` found.
        let rowid = self.next_catalog_rowid();
        self.record(
            root,
            SchemaEntry {
                kind: ObjectKind::Index,
                name: name.to_vec(),
                table: owner.name.clone(),
                root: inillucent_pool::PageId(0),
                sql: sql.clone(),
                stats: Default::default(),
                tree_id: 0,
            },
        )?;
        let page = self.build_tree_rows(root, columns, key_columns, layout, &source)?;
        let pack = packed.elapsed().as_nanos();
        // The tail is timed too, because it is not free and it is not the
        // build: recording the catalog row, re-deriving every table from the
        // catalog text, and refreshing the planner's view of it. `seal` is
        // timed after it and apart from it - see below.
        let tail = std::time::Instant::now();
        let at = self.ddl_schema;
        self.rewrite(
            rowid,
            SchemaEntry {
                kind: ObjectKind::Index,
                name: name.to_vec(),
                table: owner.name.clone(),
                root: page,
                sql,
                stats: self.tree_stats(root),
                tree_id: self.local_of(at, root),
            },
        )?;
        // **A partial index is not a covering candidate.** The physical pass
        // stands the smallest covering tree in for a plain table scan, and its
        // test is whether the tree carries every *column* the query reads - it
        // has no way to notice that the tree holds fewer *rows* than the table.
        // Offering one here answered `SELECT rowid FROM t` with the rows inside
        // the predicate, silently, under a plan that said `SCAN t`.
        if super::covers_every_row(&index) {
            self.covering.entry(owner.root).or_default().push(root);
            self.sort_covering(owner.root);
        }
        let _ = position;
        let _ = index;
        self.rebuild_tables()?;
        self.refresh_catalog();
        let catalog = tail.elapsed().as_nanos();
        // **`seal` is timed apart from the catalog work, because they are
        // different claims.** `seal` is a log commit and a sync that SQLite
        // pays too under `synchronous = FULL`, so it is not a gap to close;
        // `record`, `rebuild_tables` and `refresh_catalog` are this engine's own
        // and are worth knowing the size of. Reported together they were one
        // number nobody could act on.
        let sealed = std::time::Instant::now();
        self.seal()?;
        self.index_stages.set((
            scan,
            sort,
            uniqueness,
            flatten,
            pack,
            catalog,
            sealed.elapsed().as_nanos(),
        ));
        Ok(Outcome::empty())
    }

    /// Returns the entries a new index holds, unsorted.
    ///
    /// One pass over the table tree, projecting the key columns and the rowid.
    /// The projection is a lookup in the table's own layout - `slots[declared]`
    /// is the tree column a declared column lives in - which is the same map the
    /// scan operators read, so an index built here indexes the column the
    /// planner thinks it does.
    ///
    /// The entries land in an [`EntrySet`] rather than in a `Vec` per row.
    /// Three hundred thousand heap allocations - a vector and a text copy per
    /// row, and then a borrowed vector per row for the packer - were most of
    /// what put the `schema` family under the floor. The arena copies each
    /// payload once, encodes each key once, and hands the packer slices.
    ///
    /// @param owner - the table being indexed
    /// @param index - the index's declaration
    /// @param width - how many columns an entry has
    /// @param encoding - the index tree's key encoding
    /// @param collations - the key columns' collations, in key order
    /// @param directions - the key columns' directions, in key order
    fn index_entries(
        &self,
        owner: &TableInfo,
        index: &IndexInfo,
        width: usize,
        encoding: KeyEncoding,
        collations: &[Collation],
        directions: &[bool],
    ) -> DbResult<EntrySet> {
        let layout = self
            .layouts
            .get(&owner.root)
            .ok_or_else(|| refusal("no layout for the table being indexed"))?;
        let tree = self
            .trees
            .get(&owner.root)
            .ok_or_else(|| refusal("no tree for the table being indexed"))?;
        let mut sources: Vec<usize> = Vec::with_capacity(index.columns.len());
        for key in &index.columns {
            let declared = key
                .column
                .map(usize::from)
                .ok_or_else(|| refusal("an index on an expression"))?;
            let slot = layout
                .slots
                .get(declared)
                .copied()
                .flatten()
                .ok_or_else(|| refusal("an index on a column the tree does not carry"))?;
            sources.push(slot);
        }
        // What identifies the table row: a rowid, or a `WITHOUT ROWID` table's
        // primary key. The layout is asked rather than the table, so the build
        // path and the read path cannot disagree about what an entry carries.
        let trailing: Vec<usize> = if layout.identity.is_empty() {
            vec![layout
                .rowid
                .ok_or_else(|| refusal("an index on a table that identifies no row"))?]
        } else {
            layout.identity.clone()
        };
        let mut entries = EntrySet::with_capacity(
            width,
            tree.row_count() as usize,
            encoding,
            collations,
            directions,
        );
        let pool = self.pool_of(owner.root)?;
        tree.visit_leaves(pool, &mut |leaf| {
            // One reusable buffer per *leaf*, not per row: the values borrow
            // from the leaf, so the buffer cannot outlive it - and a hundred
            // and sixty allocations for a hundred thousand rows is not a cost.
            let mut entry: Vec<Datum<'_>> = Vec::with_capacity(width);
            // **A clean leaf is read column by column, not row by row.** `live`
            // is what merges the delta area and skips the tombstones, and it
            // pays for that by building a `Vec` per row holding *every* column
            // - where an index reads two of them. On a hundred thousand rows
            // that was three allocations and a copy of every column per row, to
            // keep two values.
            //
            // A leaf that has not been written to has no delta area and no
            // tombstones, so there is nothing to merge and the values can be
            // read straight out of the mini-columns. A leaf that has been
            // written to still goes through `live`, because merging is exactly
            // what it is for.
            if leaf.has_writes() {
                // **A leaf that has been written to still goes through the
                // merge, but it no longer materialises a row per row.** `live`
                // hands back every column of every live row in a fresh `Vec`,
                // and an index reads two of them - which is 14.3 ms of the
                // gate's 38.9 ms `CREATE INDEX`, because the gate builds its
                // index after its write workloads and by then almost every leaf
                // has a delta entry. `visit_live` performs the same merge and
                // projects only what was asked for.
                let mut projected: Vec<usize> = Vec::with_capacity(width);
                projected.extend(sources.iter().copied());
                projected.extend(trailing.iter().copied());
                leaf.visit_live(&projected, &mut |values| {
                    entries.push(values);
                    Ok(())
                })?;
                return Ok(true);
            }
            // **The mini-columns are derived once per leaf, not once per
            // value.** `LeafRef::value` re-reads the directory entry and
            // re-derives the class array and slot bounds on every call, which
            // is the same waste `key_view` exists to remove inside a search -
            // and an index build calls it twice for every row in the table.
            // Seventy rows per leaf is seventy times the same answer.
            let mut columns: Vec<MiniColumn<'_>> = Vec::with_capacity(width);
            for slot in sources.iter().chain(trailing.iter()) {
                columns.push(leaf.column(*slot)?);
            }
            for row in 0..leaf.row_count() {
                entry.clear();
                for column in &columns {
                    entry.push(column.value(row)?);
                }
                entries.push(&entry);
            }
            Ok(true)
        })?;
        Ok(entries)
    }

    /// Returns the entries a new index holds, by asking the query engine.
    ///
    /// **For the two forms whose entries are not columns of the row**: a
    /// partial index, whose predicate decides which rows have an entry at all,
    /// and an index on an expression, whose key no column carries. Both are
    /// ordinary SQL, so this writes the SQL and runs it rather than growing a
    /// second evaluator inside the DDL path.
    ///
    /// The projection is the key columns - each either an expression as it was
    /// written or a quoted column name - followed by whatever identifies the
    /// row, which is `rowid` for an ordinary table and the primary key's
    /// columns for a `WITHOUT ROWID` one. That is exactly the entry shape
    /// `index_shape` describes.
    ///
    /// A predicate or an expression naming a column the table has not got is
    /// refused here, by the binder, which is what makes
    /// `CREATE INDEX ix ON t(a) WHERE nosuchcolumn > 5` an error rather than an
    /// index nothing can maintain.
    ///
    /// @param owner - the table being indexed
    /// @param index - the index's declaration
    /// @param width - how many columns an entry has
    /// @param encoding - the index tree's key encoding
    /// @param collations - the key columns' collations, in key order
    /// @param directions - the key columns' directions, in key order
    fn index_entries_by_query(
        &mut self,
        owner: &TableInfo,
        index: &IndexInfo,
        width: usize,
        encoding: KeyEncoding,
        collations: &[Collation],
        directions: &[bool],
    ) -> DbResult<EntrySet> {
        let mut projected: Vec<String> = Vec::with_capacity(width);
        for key in &index.columns {
            match (&key.expr_sql, key.column) {
                (Some(sql), _) => projected.push(String::from_utf8_lossy(sql).into_owned()),
                (None, Some(declared)) => {
                    let column = owner
                        .column(declared)
                        .ok_or_else(|| refusal("an index on a column the table has not got"))?;
                    projected.push(quoted(&column.name));
                }
                (None, None) => return Err(refusal("an index key that is neither")),
            }
        }
        let identity = super::identity_columns(owner);
        if identity.is_empty() {
            projected.push("rowid".to_string());
        } else {
            for declared in &identity {
                let column = owner
                    .columns
                    .get(*declared)
                    .ok_or_else(|| refusal("a primary key column the table has not got"))?;
                projected.push(quoted(&column.name));
            }
        }
        let query = match index.partial_sql.as_ref() {
            Some(predicate) => format!(
                "SELECT {} FROM {} WHERE ({})",
                projected.join(", "),
                quoted(&owner.name),
                String::from_utf8_lossy(predicate)
            ),
            None => format!(
                "SELECT {} FROM {}",
                projected.join(", "),
                quoted(&owner.name)
            ),
        };
        let rows = self
            .execute_any(&query, &inillucent_exec::physical::Params::new())?
            .rows;
        let mut entries =
            EntrySet::with_capacity(width, rows.len(), encoding, collations, directions);
        let mut entry: Vec<Datum<'_>> = Vec::with_capacity(width);
        for row in &rows {
            entry.clear();
            for value in row.iter().take(width) {
                entry.push(value.borrow());
            }
            entries.push(&entry);
        }
        Ok(entries)
    }

    /// Puts a table's covering indexes back in smallest-tree-first order.
    ///
    /// @param table_root - the table whose list changed
    fn sort_covering(&mut self, table_root: u32) {
        let sizes: HashMap<u32, usize> = self
            .trees
            .iter()
            .map(|(root, tree)| (*root, tree.byte_size()))
            .collect();
        if let Some(roots) = self.covering.get_mut(&table_root) {
            roots.sort_by_key(|root| sizes.get(root).copied().unwrap_or(usize::MAX));
        }
    }

    /// Records a view or a trigger, which have text and no tree.
    ///
    /// @param keywords - the prefix the stored text carries
    /// @param kind - which of the two
    /// @param source - the statement text
    /// @param name_offset - where the object's name starts in it
    /// @param name - the object's name
    /// @param table - the table it belongs to, its own name for a view
    /// @param exists - whether one of that name is already there
    /// @param if_not_exists - whether the statement said so
    #[allow(clippy::too_many_arguments)]
    fn create_bodiless(
        &mut self,
        keywords: &str,
        kind: ObjectKind,
        source: &[u8],
        name_offset: u32,
        name: &[u8],
        table: &[u8],
        exists: bool,
        if_not_exists: bool,
    ) -> DbResult<Outcome> {
        if exists {
            if if_not_exists {
                return Ok(Outcome::empty());
            }
            return Err(refusal(format!(
                "{} {} already exists",
                match kind {
                    ObjectKind::View => "view",
                    _ => "trigger",
                },
                String::from_utf8_lossy(name)
            )));
        }
        let sql = canonical_sql(keywords, source, name_offset, source.len() as u32);
        self.record(
            0,
            SchemaEntry {
                kind,
                name: name.to_vec(),
                table: table.to_vec(),
                root: PageId::NONE,
                sql,
                stats: Default::default(),
                // Filled by `record` from the identifier it is given.
                tree_id: 0,
            },
        )?;
        self.rebuild_tables()?;
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Drops a table, index, view or trigger.
    ///
    /// A `DROP TABLE` takes its indexes and its triggers with it, and gives
    /// every page all of their trees held back to the free map in the same
    /// transaction - which is the TDD's rule and the difference between a drop
    /// and a leak.
    ///
    /// @param kind - which kind of object
    /// @param name - its name
    /// @param exists - whether it is there
    /// @param if_exists - whether the statement said `IF EXISTS`
    fn drop_object(
        &mut self,
        kind: inillucent_sql::ast::ObjectKind,
        name: &[u8],
        exists: bool,
        if_exists: bool,
    ) -> DbResult<Outcome> {
        use inillucent_sql::ast::ObjectKind as Ast;
        if !exists {
            if if_exists {
                return Ok(Outcome::empty());
            }
            return Err(refusal(format!(
                "no such {}: {}",
                match kind {
                    Ast::Table => "table",
                    Ast::Index => "index",
                    Ast::View => "view",
                    Ast::Trigger => "trigger",
                },
                String::from_utf8_lossy(name)
            )));
        }
        let folded = name.to_ascii_lowercase();
        match kind {
            Ast::Table => {
                let at = self.ddl_schema;
                let position = self
                    .tables
                    .iter()
                    .position(|held| held.database == at && held.folded == folded)
                    .ok_or_else(|| {
                        refusal(format!("no such table: {}", String::from_utf8_lossy(name)))
                    })?;
                let owner = self
                    .tables
                    .get(position)
                    .cloned()
                    .ok_or_else(|| refusal("the table that was just found is gone"))?;
                // Every row that names the table: the table, its indexes and its
                // triggers. Collected before anything is removed, because the
                // list is what decides what to remove.
                let doomed: Vec<i64> = self
                    .entries_of(at)
                    .iter()
                    .filter(|held| {
                        held.entry.name.to_ascii_lowercase() == folded
                            || held.entry.table.to_ascii_lowercase() == folded
                    })
                    .map(|held| held.rowid)
                    .collect();
                for rowid in doomed {
                    self.forget(rowid)?;
                }
                for index in &owner.indexes {
                    self.release_tree(index.root)?;
                }
                self.release_tree(owner.root)?;
                // The high-water mark goes with the table, so a table dropped
                // and recreated starts from one again - which is SQLite's
                // behaviour and the reason the mark is a row rather than a
                // header field.
                if owner.autoincrement {
                    self.forget_sequence(&owner.name)?;
                }
                let _ = position;
            }
            Ast::Index => {
                let owner = self.ddl_schema;
                let found = self.tables.iter().enumerate().find_map(|(at, table)| {
                    if table.database != owner {
                        return None;
                    }
                    table
                        .indexes
                        .iter()
                        .position(|index| index.folded == folded)
                        .map(|which| (at, which, table.root))
                });
                let Some((table_at, index_at, table_root)) = found else {
                    return Err(refusal(format!(
                        "no such index: {}",
                        String::from_utf8_lossy(name)
                    )));
                };
                let index_root = self
                    .tables
                    .get(table_at)
                    .and_then(|table| table.indexes.get(index_at))
                    .map(|index| index.root)
                    .ok_or_else(|| refusal("the index that was just found is gone"))?;
                let rowids: Vec<i64> = self
                    .entries_of(self.ddl_schema)
                    .iter()
                    .filter(|held| {
                        held.entry.kind == ObjectKind::Index
                            && held.entry.name.to_ascii_lowercase() == folded
                    })
                    .map(|held| held.rowid)
                    .collect();
                for rowid in rowids {
                    self.forget(rowid)?;
                }
                self.release_tree(index_root)?;
                let _ = (table_at, index_at);
                self.sort_covering(table_root);
            }
            Ast::View | Ast::Trigger => {
                let wanted = if kind == Ast::View {
                    ObjectKind::View
                } else {
                    ObjectKind::Trigger
                };
                let rowids: Vec<i64> = self
                    .entries_of(self.ddl_schema)
                    .iter()
                    .filter(|held| {
                        held.entry.kind == wanted && held.entry.name.to_ascii_lowercase() == folded
                    })
                    .map(|held| held.rowid)
                    .collect();
                for rowid in rowids {
                    self.forget(rowid)?;
                }
            }
        }
        self.rebuild_tables()?;
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Runs an `ALTER TABLE`.
    ///
    /// Every rewrite is a rewrite of *stored text*, and the catalog is then
    /// rebuilt from that text, so there is one derivation of what a schema means
    /// and `ALTER` does not get its own.
    ///
    /// @param source - the statement text, for `ADD COLUMN`'s definition
    /// @param table - the table being altered
    /// @param action - what to do to it
    fn alter_table(
        &mut self,
        source: &[u8],
        table: &[u8],
        action: &AlterKind,
    ) -> DbResult<Outcome> {
        let folded = table.to_ascii_lowercase();
        let at = self.ddl_schema;
        if !self
            .tables
            .iter()
            .any(|held| held.database == at && held.folded == folded)
        {
            return Err(refusal(format!(
                "no such table: {}",
                String::from_utf8_lossy(table)
            )));
        }
        if let AlterKind::AddColumn { risk, .. } = action {
            if let Some(message) = risk.refusal() {
                if self.table_has_a_row(&folded)? {
                    return Err(refusal(message));
                }
            }
        }
        let mut updates: Vec<(i64, SchemaEntry)> = Vec::new();
        for held in self.entries_of(at) {
            let (rowid, entry) = (&held.rowid, &held.entry);
            let owns = entry.table.to_ascii_lowercase() == folded;
            let itself =
                entry.name.to_ascii_lowercase() == folded && entry.kind == ObjectKind::Table;
            if entry.sql.is_empty() {
                // An automatic index has no statement of its own, but its
                // `tbl_name` and its generated name still follow a rename.
                if owns {
                    if let AlterKind::RenameTable { to } = action {
                        let mut moved = entry.clone();
                        moved.table = to.clone();
                        moved.name = renamed_automatic(&entry.name, table, to);
                        updates.push((*rowid, moved));
                    }
                }
                continue;
            }
            let rewritten = match action {
                AlterKind::RenameTable { to } => {
                    let next = rename::rewrite(&entry.sql, rename::Rename::Table, table, to)?;
                    if next == entry.sql && !owns {
                        continue;
                    }
                    let mut moved = entry.clone();
                    moved.sql = rename::reparsed(next)?;
                    if itself {
                        moved.name = to.clone();
                        moved.table = to.clone();
                    } else if owns {
                        moved.table = to.clone();
                    }
                    moved
                }
                AlterKind::RenameColumn { from, to } => {
                    if !owns {
                        let reads = rename::referenced_tables(&entry.sql);
                        if !reads.contains(&folded) {
                            continue;
                        }
                        if reads.len() > 1 {
                            return Err(refusal(format!(
                                "error in {}: cannot rename a column it reads alongside another table",
                                String::from_utf8_lossy(&entry.name)
                            )));
                        }
                    }
                    let next = rename::rewrite(&entry.sql, rename::Rename::Column, from, to)?;
                    if next == entry.sql {
                        continue;
                    }
                    let mut moved = entry.clone();
                    moved.sql = rename::reparsed(next)?;
                    moved
                }
                AlterKind::AddColumn { start, end, .. } => {
                    if !itself {
                        continue;
                    }
                    let definition = source
                        .get(*start as usize..*end as usize)
                        .unwrap_or_default()
                        .to_vec();
                    let mut moved = entry.clone();
                    moved.sql = rename::reparsed(rename::add_column(&entry.sql, &definition)?)?;
                    moved
                }
                AlterKind::DropColumn { position, .. } => {
                    if !itself {
                        continue;
                    }
                    let mut moved = entry.clone();
                    moved.sql =
                        rename::reparsed(rename::drop_column(&entry.sql, usize::from(*position))?)?;
                    moved
                }
            };
            updates.push((*rowid, rewritten));
        }
        for (rowid, entry) in updates {
            self.rewrite(rowid, entry)?;
        }
        self.rebuild_tables()?;
        // A `DROP COLUMN` changes the *rows*, not only the text, and the tree is
        // rebuilt rather than edited in place: every leaf's column directory
        // would otherwise still describe a column the catalog no longer has.
        if let AlterKind::DropColumn { .. } = action {
            self.rebuild_table_tree(&folded)?;
        }
        if let AlterKind::AddColumn { .. } = action {
            self.rebuild_table_tree(&folded)?;
        }
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Returns the value a column's `DEFAULT` has for a row that predates it.
    ///
    /// It is evaluated by *running* it - `SELECT <the default text>` through
    /// the ordinary compile-and-execute path - rather than by a second
    /// expression evaluator written for the DDL path. `DEFAULT (1 + 1)` and
    /// `DEFAULT 'x' || 'y'` are expressions, and an evaluator that handled only
    /// literals would fill NULL for those while filling the right value for the
    /// simple ones, which is the shape of bug that hides.
    ///
    /// A default that cannot be evaluated - one calling a function this engine
    /// does not have - reports itself rather than silently becoming NULL.
    ///
    /// @param default_sql - the `DEFAULT` text as the declaration wrote it
    fn constant_default(&mut self, default_sql: &[u8]) -> DbResult<OwnedDatum> {
        let text = String::from_utf8_lossy(default_sql).into_owned();
        let rows = self.query_internally(&format!("SELECT {text}"))?;
        Ok(rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(OwnedDatum::Null))
    }

    /// Rebuilds every `TableInfo` from the catalog rows.
    ///
    /// After an `ALTER`, because the stored text is what changed and the derived
    /// view has to be derived again. The identifiers are carried across by name
    /// so the trees a plan will read stay the trees they were.
    pub(super) fn rebuild_tables(&mut self) -> DbResult<()> {
        // **Every schema, each numbered as the binder numbers it.** A table's
        // `database` is what an unqualified name is resolved through and what a
        // qualified one is matched against, so a table derived under the wrong
        // number is a table the wrong statement finds.
        // **Every schema every session shares, and no session's own.** A
        // temporary table belongs to one connection, so it is derived per
        // session by `session_catalog` rather than kept here where another
        // connection would find it.
        let mut rebuilt: Vec<TableInfo> = Vec::new();
        for at in self.schema_numbers() {
            let held = self.entries_of(at);
            let entries: Vec<inillucent_catalog::paged::SchemaEntry> =
                held.iter().map(|row| row.entry.clone()).collect();
            let roots: Vec<u32> = held.iter().map(|row| row.root).collect();
            rebuilt.extend(tables_from_entries(&entries, &roots, at)?);
        }
        // **A temporary trigger fires on whatever the name finds.** Its row
        // lives in the temporary database and its table usually does not -
        // `CREATE TEMP TRIGGER t_log AFTER INSERT ON t` is a trigger on a
        // permanent table - so `tables_from_entries` leaves it unattached, and
        // this is where it is put where it belongs. Newest first, which is
        // SQLite's own order.
        let orphans: Vec<(Vec<u8>, Vec<u8>)> = self
            .entries_of(super::TEMP)
            .iter()
            .filter(|row| row.entry.kind == ObjectKind::Trigger)
            .map(|row| (row.entry.table.to_ascii_lowercase(), row.entry.sql.clone()))
            .filter(|(folded, _)| {
                !rebuilt
                    .iter()
                    .any(|table| table.database == super::TEMP && table.folded == *folded)
            })
            .collect();
        for (folded, sql) in orphans {
            let Ok(trigger) = inillucent_catalog::load::trigger_from_create_sql(&sql) else {
                continue;
            };
            if let Some(table) = rebuilt.iter_mut().find(|table| table.folded == folded) {
                table.triggers.insert(0, trigger);
            }
        }
        // **A virtual table's columns come from its module, not its text.**
        // `CREATE VIRTUAL TABLE documents USING fts5(title, body)` names a
        // module and its arguments; what the *columns* are is the module's
        // answer, and only a connected module can give it. Deriving them from
        // the statement would be a second implementation of every module's
        // argument grammar, agreeing with the module until the day it did not.
        for table in &mut rebuilt {
            let Some(connected) = self.virtual_tables.get(&table.folded) else {
                continue;
            };
            let declaration = connected.table.declaration();
            table.kind = inillucent_sql::catalog_view::TableKind::Virtual;
            table.without_rowid = declaration.without_rowid;
            table.columns = inillucent_sql::declare::declared_columns(declaration);
            table.module = Some(inillucent_sql::vtab::ModuleRef {
                name: connected.arguments.module.clone(),
                folded: connected.arguments.module.to_ascii_lowercase(),
                arguments: connected.arguments.arguments.clone(),
            });
        }
        self.tables = rebuilt;
        Ok(())
    }

    /// Rebuilds one table's tree so its leaves carry the columns the catalog
    /// now says it has.
    ///
    /// `ADD COLUMN` and `DROP COLUMN` both change the column directory, and a
    /// leaf's directory is written into the page - so the rows are read out
    /// through the old layout, re-shaped, and packed into a fresh tree. The old
    /// tree's pages go back to the free map.
    ///
    /// @param folded - the table's folded name
    /// Returns whether a table holds at least one row.
    ///
    /// `ADD COLUMN` is the only caller: three of the five things it may not add
    /// are only unaddable because an existing row would have no value for them,
    /// so an empty table takes all three and SQLite accepts them. It stops at
    /// the first row rather than counting, because the question is existence.
    ///
    /// @param folded - the table's folded name
    fn table_has_a_row(&mut self, folded: &[u8]) -> DbResult<bool> {
        let Some(root) = self
            .tables
            .iter()
            .find(|table| table.folded == folded)
            .map(|table| table.root)
        else {
            return Ok(false);
        };
        let Some(tree) = self.trees.get(&root) else {
            return Ok(false);
        };
        Ok(!tree.rows(self.pool_of(root)?)?.is_empty())
    }

    fn rebuild_table_tree(&mut self, folded: &[u8]) -> DbResult<()> {
        let Some(info) = self
            .tables
            .iter()
            .find(|table| table.folded == folded)
            .cloned()
        else {
            return Ok(());
        };
        let old_root = info.root;
        let old_layout = self
            .layouts
            .get(&old_root)
            .cloned()
            .ok_or_else(|| refusal("no layout for the table being rebuilt"))?;
        let old_rows = {
            let tree = self
                .trees
                .get(&old_root)
                .ok_or_else(|| refusal("no tree for the table being rebuilt"))?;
            tree.rows(self.pool_of(old_root)?)?
        };
        let (columns, key_columns, layout) = if info.without_rowid {
            keyed_table_shape(&info)?
        } else {
            let (columns, layout) = table_shape(&info);
            (columns, 1, layout)
        };
        // Each new tree column is filled from the old tree column that held the
        // same *declared* column. A column the declaration did not have takes
        // its `DEFAULT`, which is SQLite's rule and is what makes
        // `ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 9` answer 9 for the rows
        // that were already there. Filling NULL instead was a wrong answer
        // rather than a refusal, and only visible to a statement that read the
        // new column on an old row.
        let mut from: Vec<Fill> = vec![Fill::Absent; layout.width];
        for (declared, slot) in layout.slots.iter().enumerate() {
            let Some(slot) = slot else { continue };
            if let Some(Some(source)) = old_layout.slots.get(declared) {
                if let Some(cell) = from.get_mut(*slot) {
                    *cell = Fill::From(*source);
                }
                continue;
            }
            // **Nothing to fill, so nothing to evaluate (task-1932, H3).**
            // This ran whether or not the table had a row, and
            // `constant_default` evaluates the default by running
            // `SELECT <the default text>` through the ordinary execute path -
            // so `ALTER TABLE t ADD COLUMN b INTEGER DEFAULT
            // (no_such_function())` on an empty table failed here, three
            // writes after the catalog already said the column was there, and
            // `PRAGMA table_info(t)` then listed a column the tree had no slot
            // for.
            //
            // An empty table is the only way to reach it:
            // `AddedColumnRisk::refusal` refuses a default that is not a
            // literal, and `alter_table` applies that refusal only when
            // `table_has_a_row`. So on a populated table the statement never
            // gets here, and on an empty one there is no row to give a value
            // to. SQLite behaves the same way - it accepts the `ALTER`,
            // records `DEFAULT (no_such_function())` in the schema text, and
            // reports `unknown function` at the first `INSERT` that needs the
            // value - so skipping the evaluation is what matches the reference
            // rather than merely what avoids the failure.
            if old_rows.is_empty() {
                continue;
            }
            let Some(default) = info
                .columns
                .get(declared)
                .and_then(|column| column.default_sql.clone())
            else {
                continue;
            };
            let value = self.constant_default(&default)?;
            if let Some(cell) = from.get_mut(*slot) {
                *cell = Fill::Constant(value);
            }
        }
        if let (Some(new_rowid), Some(old_rowid)) = (layout.rowid, old_layout.rowid) {
            if let Some(cell) = from.get_mut(new_rowid) {
                *cell = Fill::From(old_rowid);
            }
        }
        let rows: Vec<Vec<OwnedDatum>> = old_rows
            .iter()
            .map(|row| {
                from.iter()
                    .map(|source| match source {
                        Fill::From(at) => row.get(*at).cloned().unwrap_or(OwnedDatum::Null),
                        Fill::Constant(value) => value.clone(),
                        Fill::Absent => OwnedDatum::Null,
                    })
                    .collect()
            })
            .collect();
        let rows = in_key_order(rows, &columns, key_columns);
        // The rebuild holds owned rows, so it does its own borrow. It runs once
        // per `ALTER TABLE` and is not on any measured path, which is exactly
        // why the cost belongs here rather than inside the builder every caller
        // shares.
        let borrowed: Vec<Vec<Datum<'_>>> = rows
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        self.release_tree(old_root)?;
        let covering: Vec<u32> = self.covering.get(&old_root).cloned().unwrap_or_default();
        self.build_tree_from(old_root, columns, key_columns, layout, &borrowed)?;
        if !covering.is_empty() {
            self.covering.insert(old_root, covering);
        }
        // The catalog row's `rootpage` moved with the tree.
        let page = self
            .trees
            .get(&old_root)
            .map(PagedTree::root)
            .unwrap_or(PageId::NONE);
        let update = self
            .entries
            .iter()
            .find(|held| {
                held.entry.kind == ObjectKind::Table
                    && held.entry.name.to_ascii_lowercase() == folded
            })
            .map(|held| {
                let mut moved = held.entry.clone();
                moved.root = page;
                (held.rowid, moved)
            });
        if let Some((rowid, entry)) = update {
            self.rewrite(rowid, entry)?;
        }
        Ok(())
    }
}

/// Refuses a `CREATE UNIQUE INDEX` whose entries are not unique.
///
/// The entries are already in key order, so a duplicate is two adjacent rows
/// agreeing on every key column but the rowid - which is the definition, and is
/// one comparison rather than a set.
///
/// @param rows - the entries, sorted
/// @param owner - the table, for the message
/// @param index - the index, for the message
/// @param key_columns - how wide the entry is, rowid included
fn refuse_duplicates(
    entries: &EntrySet,
    order: &[u32],
    owner: &TableInfo,
    index: &IndexInfo,
    key_columns: usize,
) -> DbResult<()> {
    let compared = key_columns.saturating_sub(1);
    for at in 0..order.len().saturating_sub(1) {
        if entries.shares_key_prefix(order, at, compared) {
            let columns: Vec<String> = index
                .columns
                .iter()
                .map(|key| {
                    let name = key
                        .column
                        .and_then(|at| owner.column(at))
                        .map(|column| String::from_utf8_lossy(&column.name).into_owned())
                        .unwrap_or_else(|| "?".to_string());
                    format!("{}.{}", String::from_utf8_lossy(&owner.name), name)
                })
                .collect();
            return Err(refusal(format!(
                "UNIQUE constraint failed: {}",
                columns.join(", ")
            )));
        }
    }
    Ok(())
}

/// Returns an identifier quoted the way SQL wants it.
///
/// Double quotes, with any inside doubled: a column called `odd"name` is legal
/// and a query built by pasting it in would not parse.
///
/// @param name - the identifier
fn quoted(name: &[u8]) -> String {
    let written = String::from_utf8_lossy(name).replace('"', "\"\"");
    format!("\"{written}\"")
}

/// Returns the new name of an automatic index when its table is renamed.
///
/// SQLite names them `sqlite_autoindex_<table>_<n>`, so the name has to follow
/// the table or the next `CREATE TABLE` of the old name would collide with it.
///
/// @param name - the index's current name
/// @param from - the table's old name
/// @param to - its new one
fn renamed_automatic(name: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let prefix = b"sqlite_autoindex_";
    let Some(rest) = name.strip_prefix(prefix.as_slice()) else {
        return name.to_vec();
    };
    let Some(suffix) = rest.strip_prefix(from) else {
        return name.to_vec();
    };
    let mut out = prefix.to_vec();
    out.extend_from_slice(to);
    out.extend_from_slice(suffix);
    out
}

impl Outcome {
    /// Returns the answer a statement that produced no rows gives.
    pub fn empty() -> Outcome {
        Outcome {
            rows: Vec::new(),
            names: Vec::new(),
            changes: Changes::default(),
        }
    }
}

/// Where one column of a rebuilt tree gets its values.
///
/// Three cases and not two: a column carried across, a column the declaration
/// has just gained with a `DEFAULT`, and one it has gained without.
#[derive(Clone, Debug)]
enum Fill {
    /// The old tree column that held the same declared column.
    From(usize),
    /// The `DEFAULT` a new column declared.
    Constant(OwnedDatum),
    /// Nothing: a new column with no default, which is NULL.
    Absent,
}

/// Returns one column of a `pragma_*` function's declaration.
///
/// Everything about it is the default: a pragma's answer is untyped, so BLOB
/// affinity and BINARY collation are what SQLite gives it too.
///
/// @param name - the column's name
/// @param hidden - whether it is one of the two argument columns
fn pragma_column(name: &[u8], hidden: bool) -> inillucent_sql::catalog_view::ColumnInfo {
    inillucent_sql::catalog_view::ColumnInfo {
        name: name.to_vec(),
        folded: name.to_ascii_lowercase(),
        declared_type: Vec::new(),
        affinity: inillucent_value::Affinity::Blob,
        collation: Vec::new(),
        not_null: false,
        not_null_conflict: None,
        primary_key_conflict: None,
        default_sql: None,
        primary_key_position: None,
        hidden,
        generated: false,
        stored: true,
        generated_sql: None,
    }
}
