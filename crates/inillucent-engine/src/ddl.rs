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
//! for a created one is a number counted up from `super::FIRST_CREATED_ROOT`.
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

use inillucent_base::error::{refusal, statement_refusal};
use inillucent_base::DbResult;
use inillucent_catalog::paged::{ObjectKind, SchemaEntry};
use inillucent_exec::dml::Changes;
use inillucent_sql::bind::BoundStatement;
use inillucent_sql::catalog_view::{IndexInfo, TableInfo};
use inillucent_sql::directive::Directive;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::paged::KeyEncoding;
use inillucent_value::collation::Collation;

// **The five modules this file is made of (task-1962, A1 step 1).** It was
// 2,753 lines in one `impl` block: the catalog rows a statement writes, the
// trees it builds, and the four statements. Every method moved whole and
// nothing changed shape.
mod alter;
pub(crate) use alter::Already;
mod catalog;
mod index;
mod reindex;
mod table;
mod tree;

use crate::entries::EntrySet;

use super::{index_shape, ImportedDatabase, Outcome};

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
pub(crate) type CatalogWrite = (
    usize,
    u64,
    bool,
    std::rc::Rc<inillucent_wal::Wal>,
    std::sync::Arc<std::sync::atomic::AtomicU64>,
);

impl ImportedDatabase {
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
        let previous = self.schema.ddl_schema;
        let at = schema_of(&directive);
        // **The temporary database is made by the first statement that needs
        // one.** The binder has already resolved `temp` to schema one, because
        // the name is always in the catalog; this is where the file behind it
        // comes into being, so a connection that never writes a temporary object
        // never makes one.
        if at == super::TEMP {
            self.ensure_temp()?;
        }
        self.schema.ddl_schema = at;
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
        let autocommit = self.writing.batch().is_none();
        let mark = self.statement_mark();
        let txn = self.current_txn();
        let outcome = self.run_directive(*directive, sql);
        self.schema.ddl_schema = previous;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let undone = self.undo_to_floor(mark, true, txn);
                if autocommit {
                    self.writing.undo().borrow_mut().clear();
                    self.writing.pending_frees().borrow_mut().clear();
                    self.writing.built().borrow_mut().clear();
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
            self.writing.undo().borrow_mut().clear();
        }
        if changes_schema {
            self.storage.database.bump_schema_cookie();
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
                Already::of(exists, if_not_exists),
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
                // The directive carries no `if_not_exists` because it does not
                // need one: `bind_create_trigger` has already refused a
                // duplicate that did not say so, and `exists` reaching here at
                // all therefore means the statement did.
                Already::of(exists, true),
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
                if self.writing.batch().is_some() {
                    return Err(refusal("cannot start a transaction within a transaction"));
                }
                self.begin_batch();
                Ok(Outcome::empty())
            }
            Directive::Commit => {
                if self.writing.batch().is_none() {
                    return Err(refusal("cannot commit - no transaction is active"));
                }
                self.commit_batch()?;
                Ok(Outcome::empty())
            }
            Directive::Pragma {
                ref name,
                ref argument,
                database,
                ..
            } => self.pragma(name, argument.as_ref(), database),
            Directive::Rollback { savepoint } => match savepoint {
                Some(name) => {
                    self.rollback_to(&name)?;
                    Ok(Outcome::empty())
                }
                None => {
                    if self.writing.batch().is_none() {
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
                if self.writing.batch().is_none() {
                    self.begin_batch();
                    self.writing.set_implicit_transaction(true);
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
                if self.writing.marks().borrow().is_empty() && self.writing.implicit_transaction() {
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
            Directive::Vacuum { .. } if self.writing.batch().is_some() => {
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
pub(crate) fn refuse_duplicates(
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
pub(crate) fn quoted(name: &[u8]) -> String {
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
pub(crate) fn renamed_automatic(name: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
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
            names: std::rc::Rc::new(Vec::new()),
            changes: Changes::default(),
        }
    }
}

/// Where one column of a rebuilt tree gets its values.
///
/// Three cases and not two: a column carried across, a column the declaration
/// has just gained with a `DEFAULT`, and one it has gained without.
#[derive(Clone, Debug)]
pub(crate) enum Fill {
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
pub(crate) fn pragma_column(name: &[u8], hidden: bool) -> inillucent_sql::catalog_view::ColumnInfo {
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
