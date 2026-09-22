//! The transaction manager: opening a batch, undoing to a mark, committing one.
//!
//! Invariant: **a batch is the unit that is kept or discarded, and the undo
//! record is written before the change it undoes.** Everything here is about
//! that: `begin_batch` opens one, `undo_to` walks back to a mark, `rollback`
//! walks back to the floor, and `commit_batch` seals. `commit_across` and `vote`
//! are the same thing over several attached databases, which is why they are
//! beside it rather than in `attach.rs`.

use std::path::PathBuf;

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_catalog::load::table_from_create_sql;
use inillucent_catalog::paged::{attach_catalog, ObjectKind};
use inillucent_exec::physical::SourceLayout;
use inillucent_sql::catalog_view::TableInfo;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::types::ColumnSpec;
use inillucent_tree::PagedTree;

use crate::*;

impl crate::ImportedDatabase {
    /// Opens a transaction that the statements after it all join.
    ///
    /// The difference between this and autocommit is the whole of what a commit
    /// costs, which is the gate's `transaction` family. Calling it twice without
    /// a commit between keeps the first transaction, because that is what
    /// `BEGIN` inside a transaction does.
    pub fn begin_batch(&mut self) {
        if self.writing.batch().is_some() {
            return;
        }
        // **The file is taken here, not at the first write.** A transaction
        // that raised its lock halfway through could be refused halfway
        // through, with statements already applied; taking it at `BEGIN` means
        // a transaction that starts is a transaction that can finish. It is
        // also why this engine's transactions serialise across processes rather
        // than overlapping: there is no shared-memory index that would let a
        // reader follow a writer's log, and pretending otherwise is what would
        // corrupt a file.
        let _ = self.storage.database.begin_write_within(true);
        let txn = self.writing.next_txn();
        self.writing.set_next_txn(txn.saturating_add(1));
        self.writing.set_batch(Some(txn));
        self.writing.undo().borrow_mut().clear();
        self.writing.marks().borrow_mut().clear();
        self.writing.pending_frees().borrow_mut().clear();
        self.writing.built().borrow_mut().clear();
        self.writing.set_touched(0);
    }

    /// Undoes everything the open transaction changed, newest first.
    ///
    /// **Newest first, and that is the whole of the ordering rule.** A key
    /// written twice inside one transaction has two records; restoring the
    /// older one last is what puts the row back the way it was before the
    /// transaction rather than the way it was in the middle of it.
    ///
    /// The restores are ordinary writes and are logged like any other, because
    /// the log is redo-only: a crash between the rollback and the commit record
    /// has to replay to the *rolled back* state, not to the state the aborted
    /// statements left. Undoing by not-logging would leave the log describing
    /// changes the file no longer has.
    ///
    /// @param to - the savepoint to stop at, or `None` for the whole transaction
    pub(crate) fn undo_to(&mut self, to: Option<&[u8]>) -> DbResult<()> {
        let to = match to {
            Some(name) => {
                let folded = name.to_ascii_lowercase();
                // One borrow, held only long enough to find the savepoint:
                // `undo_to_floor` below takes the group again.
                let found = {
                    let marks = self.writing.marks().borrow();
                    marks
                        .iter()
                        .rposition(|(held, _)| *held == folded)
                        .and_then(|index| marks.get(index).map(|(_, mark)| *mark))
                };
                let Some(mark) = found else {
                    return Err(refusal(format!(
                        "no such savepoint: {}",
                        String::from_utf8_lossy(name)
                    )));
                };
                mark
            }
            None => crate::engine::state::StatementMark {
                undo: 0,
                dropped: 0,
                built: 0,
            },
        };
        self.undo_to_floor(to, true, self.current_txn())
    }

    /// Undoes back to a position in the undo buffer, newest first.
    ///
    /// The body of [`ImportedDatabase::undo_to`], separated so a *statement*
    /// can name its own floor. A savepoint is a name this connection was given;
    /// a statement boundary is a length nobody named, taken by `write` before
    /// the statement wrote anything, and there is nothing to look up.
    ///
    /// **It takes the whole mark rather than a length per record**, for the
    /// reason [`crate::engine::state::StatementMark`] gives: the three lengths
    /// are always taken together and always cut together, and a transaction
    /// that learns to record a fourth thing should not have to grow every
    /// caller of this.
    ///
    /// @param to - where the part being abandoned begins, in each of the
    ///   transaction's records: the undo buffer, the pending-free list whose
    ///   `DROP`s are undone by cutting it, and the list of trees built after
    ///   the same point, whose pages are given back because the transaction
    ///   they were allocated by is not going to commit
    /// @param reload - whether to rebuild the schema from the catalog tree
    /// @param txn - the transaction the restores are logged under, which is the
    ///   statement's own rather than `current_txn`: outside a batch `write` has
    ///   already taken a number and moved `next_txn` past it
    pub(crate) fn undo_to_floor(
        &mut self,
        to: crate::engine::state::StatementMark,
        reload: bool,
        txn: u64,
    ) -> DbResult<()> {
        let (floor, dropped_floor, built_floor) = (to.undo, to.dropped, to.built);
        while self.writing.undo().borrow().len() > floor {
            let Some(entry) = self.writing.undo().borrow_mut().pop() else {
                break;
            };
            // **The record says which file it came out of, and that is the
            // whole of the routing.** Two databases each number their own trees
            // from one, so the identifier alone is ambiguous the moment a
            // connection holds a second file - and a rollback that guessed
            // would put a row back into the wrong database, silently.
            let at = entry.schema;
            let wal = self
                .log_of(at)
                .ok_or_else(|| refusal("a rollback names a database that is not attached"))?;
            let mut log = WalLog {
                wal,
                txn,
                schema: at,
                wrote: false,
                // The restore is not itself undoable: it *is* the undo, and
                // recording it would grow the buffer being drained.
                undo: None,
                uncommitted: self.uncommitted_handle_of(at),
            };
            // **The catalog tree answers to two numbers.** Its `tree_id` is
            // `SCHEMA_TREE_ID`, which is what the log records carry, and it
            // lives in `trees` under the handle the planner reads
            // `sqlite_schema` through. An undo record carries the first and
            // this map is keyed by the second, so a catalog row's before-image
            // was looked up under a number nothing held and silently skipped -
            // which is why a rolled-back `CREATE TABLE` stayed in the schema.
            let root = if entry.tree == inillucent_catalog::paged::SCHEMA_TREE_ID {
                self.catalog_handle_of(at)
            } else {
                self.handle_of(at, entry.tree).unwrap_or(0)
            };
            let Some(tree) = self.schema.trees.get_mut(&root) else {
                // The tree is gone, which a rollback of a `CREATE TABLE` makes
                // true. Its rows went with it.
                continue;
            };
            let session = self.session_state.session.get();
            let database = file_of(
                &mut self.storage.database,
                &mut self.session_state.attached,
                &mut self.session_state.temps,
                session,
                at,
            )?;
            match &entry.row {
                Some(row) => {
                    let values: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
                    tree.put(database, &mut log, &values)?;
                }
                None => {
                    let key: Vec<Datum<'_>> = entry.key.iter().map(OwnedDatum::borrow).collect();
                    tree.delete(database, &mut log, &key)?;
                }
            }
        }
        // **Cut before the schema is rebuilt below.** `reload_entries` attaches
        // a restored object's tree at the root page its catalog row names, and
        // a page still on this list is a page the commit would hand back to the
        // free map - so a list that still held a rolled-back `DROP`'s pages
        // would free the pages of a table that is once again in the schema.
        self.writing
            .pending_frees()
            .borrow_mut()
            .truncate(dropped_floor);
        // **And the trees this part of the transaction built go back to the
        // free map, also before the schema is rebuilt (task-2065).** The two
        // lists are cut at the same point and mean opposite things: a page on
        // the list above was allocated before this transaction and is only
        // freed if the transaction commits, and a page below was allocated by
        // the part of the transaction that is being abandoned, so it is freed
        // precisely because the transaction does not commit. Doing it before
        // `reload_entries` runs is what stops the walk it does reaching a tree
        // that is on its way out.
        self.release_built_trees(built_floor)?;
        let held = self.writing.undo().borrow().len();
        self.writing
            .marks()
            .borrow_mut()
            .retain(|(_, mark)| mark.undo <= held);
        // **A DML statement cannot have changed the catalog, so undoing one has
        // nothing to rebuild from it.** `CREATE`, `DROP` and `ALTER` do not go
        // through `write`, and reloading here would cost a catalog read on
        // every failed statement - and could replace a constraint failure with
        // the "no tree attached" refusal below, which would be a different
        // error for a statement that never touched a table's existence.
        if !reload {
            return Ok(());
        }
        // The catalog tree may have been restored along with everything else,
        // so the schema the binder sees is rebuilt from it.
        let missing = self.reload_entries()?;
        if !missing.is_empty() {
            // A `DROP` undone by restoring its catalog row puts the object back
            // in the schema without putting its tree back in this handle, and a
            // table the binder names and nothing can read is a wrong answer
            // waiting to happen. Refused by name until the re-attach is written.
            return Err(refusal(format!(
                "rolling back left {} in the schema with no tree attached;                  undoing a DROP inside a transaction is not supported yet",
                missing.join(", ")
            )));
        }
        Ok(())
    }

    /// Rebuilds the in-memory schema from the catalog tree.
    ///
    /// **The catalog tree is the authority and `entries` is a cache of it.** A
    /// rollback restores the tree - a catalog row is a row, and it is undone
    /// like one - and this is what makes the cache agree again. Without it a
    /// `CREATE TABLE` that was abandoned stayed visible: the row was gone from
    /// the file and still in the list the binder is built from.
    ///
    /// It does not re-attach trees. Every object it names that has no tree is
    /// reported, because a schema naming a table nothing can read is worse than
    /// a refusal - see [`ImportedDatabase::undo_to`], which turns that into
    /// one.
    pub(crate) fn reload_entries(&mut self) -> DbResult<Vec<String>> {
        let mut missing = Vec::new();
        // Every schema, because a transaction may have written more than one of
        // them and a rollback restores every file it touched.
        for at in self.schema_numbers() {
            let Some(database) = self.schema_file(at) else {
                continue;
            };
            let catalog_tree = attach_catalog(database.pool(), database.catalog_root())?;
            let stored =
                inillucent_catalog::paged::read_catalog_rows(database.pool(), &catalog_tree)?;
            let reloaded: Vec<Recorded> = stored
                .into_iter()
                .map(|(rowid, entry)| {
                    let root = self.handle_of(at, entry.tree_id).unwrap_or(0);
                    Recorded { rowid, root, entry }
                })
                .collect();
            // **A `DROP` undone by restoring its catalog row has to get its
            // tree handle back too.** `release_tree` takes the handle out of
            // this connection when the object is dropped, and a rollback
            // restores the *rows* - the catalog row and every page of the tree,
            // which the undo log holds - but not the handle, because a handle
            // is not a row. Before this was fixed, the object came back into the
            // schema with nothing behind it, the rollback was refused, and the
            // connection was then unable to read the table at all: `no layout
            // imported for root page 2147483648`. A refusal that damages the
            // session is worse than one that does not.
            //
            // The shape comes from the entry's own `CREATE` text, which is the
            // same place `define_table` derives it from - so a re-attached tree
            // is described exactly as the original was rather than from
            // whatever this process happens to remember.
            let restored = self.reattach_entries(at, &reloaded)?;
            let reloaded: Vec<Recorded> = reloaded
                .into_iter()
                .map(|mut held| {
                    if let Some(root) = restored.get(&held.rowid) {
                        held.root = *root;
                    }
                    held
                })
                .collect();
            // **A tree that is attached at the wrong page counts as missing
            // (task-2051).** This asked only whether the handle held something,
            // which is the same assumption `reattach_entries` used to skip on,
            // and it has to be relaxed in the same place: a re-attach that
            // could not derive the entry's shape leaves the tree the `ALTER`
            // built sitting in the handle, and a check that accepts any tree
            // would report that rollback as having worked. The refusal below is
            // the right answer to it - a connection that reads a table through
            // a tree the catalog does not name gives wrong answers and strands
            // every row it writes.
            for held in &reloaded {
                let attached = self.schema.trees.get(&held.root).map(PagedTree::root);
                if held.entry.tree_id != 0 && attached != Some(held.entry.root) {
                    missing.push(String::from_utf8_lossy(&held.entry.name).into_owned());
                }
            }
            if let Some(held) = self.entries_of_mut(at) {
                *held = reloaded;
            }
        }
        self.rebuild_tables()?;
        // **A rolled back `CREATE VIRTUAL TABLE` leaves its module connected,
        // and the connection outlives the rows it was made from (task-2043).**
        // `virtual_tables` is this connection's memory; the catalog tree is the
        // file. A module left in the map after its catalog row was undone points
        // at shadow trees that no longer exist, and `is_shadow_table` then
        // reports an ordinary table of that name as a shadow of it.
        //
        // `rebuild_tables` has just derived `tables` from the catalog, so a name
        // that is not a virtual table there is not one in the file either.
        let still_virtual: std::collections::HashSet<Vec<u8>> = self
            .schema
            .tables
            .iter()
            .filter(|table| table.kind == inillucent_sql::catalog_view::TableKind::Virtual)
            .map(|table| table.folded.clone())
            .collect();
        self.session_state
            .virtual_tables
            .retain(|name, _| still_virtual.contains(name));
        self.refresh_catalog();
        Ok(missing)
    }

    /// Re-attaches the tree of every restored entry this connection has lost.
    ///
    /// Called from [`ImportedDatabase::reload_entries`] after a rollback has put
    /// the catalog rows back. Returns the handle each restored entry ended up
    /// with, by catalog rowid, so the caller can correct its own list.
    ///
    /// An entry whose shape cannot be derived is left alone rather than
    /// reported here: the caller's `missing` check is what turns that into a
    /// refusal, and reporting it twice would report a rollback that half worked
    /// as two different failures.
    ///
    /// @param at - which attached database
    /// @param entries - the entries as the catalog tree now holds them
    fn reattach_entries(
        &mut self,
        at: usize,
        entries: &[Recorded],
    ) -> DbResult<std::collections::HashMap<i64, u32>> {
        let mut restored = std::collections::HashMap::new();
        for held in entries {
            if held.entry.tree_id == 0 {
                continue;
            }
            let root = match self.handle_of(at, held.entry.tree_id) {
                Some(root) if root != 0 => root,
                _ => continue,
            };
            // **A handle that is occupied says nothing about whether it holds
            // the tree the catalog describes, and the root page is what says
            // so (task-2051).** This used to skip on `contains_key` alone,
            // which is right for a rolled-back `DROP` - the handle is empty -
            // and wrong for a rolled-back `ALTER TABLE` that rebuilt the tree.
            // `rebuild_table` releases the old tree and registers a new one at
            // a new root page under the *same* handle, so after the rollback
            // restores the catalog row the handle still held the rebuilt tree
            // and the schema and the tree described different tables.
            //
            // For `DROP COLUMN` that was a read error naming a column by
            // position - `the tree read for FROM term 0 does not carry column
            // 1` - for the rest of the connection's life. For `ADD COLUMN`
            // with a `DEFAULT` there was no symptom at all, because the
            // rebuilt tree carries a superset of the catalog's columns and
            // every read still found its slot. Both lost every row written
            // afterwards: the rebuilt tree is an orphan no catalog row names,
            // so the `INSERT` reported success, read back in the same session,
            // and was not in the reopened file.
            //
            // The catalog row is the authority, which is the principle
            // task-2043 applied to the module map for the same class of
            // defect: state a rollback made untrue that nobody put back.
            if self
                .schema
                .trees
                .get(&root)
                .is_some_and(|tree| tree.root() == held.entry.root)
            {
                continue;
            }
            let Some((columns, key_columns, layout)) = self.shape_of_entry(entries, held, root)
            else {
                continue;
            };
            let Some(database) = self.schema_file(at) else {
                continue;
            };
            let tree = PagedTree::attach(
                database.pool(),
                held.entry.tree_id,
                held.entry.root,
                columns,
                key_columns,
                held.entry.stats.leaf_count,
                held.entry.stats.row_count,
            )?;
            self.schema.trees.insert(root, tree);
            self.schema.layouts.insert(root, std::rc::Rc::new(layout));
            self.session_state.owner.insert(root, at);
            restored.insert(held.rowid, root);
        }
        // An index's tree is a covering candidate of its table's, and the link
        // went with the handle when the object was dropped.
        for held in entries {
            if held.entry.kind != ObjectKind::Index {
                continue;
            }
            let (Some(index_root), Some(table_root)) = (
                restored.get(&held.rowid).copied(),
                self.root_of_named(entries, at, &held.entry.table),
            ) else {
                continue;
            };
            // A `DROP` that is rolled back puts the index's tree back, and
            // it may only rejoin the covering set on the same terms it was in
            // it: the catalog text is what says whether it is partial.
            let partial = inillucent_catalog::load::index_from_create_sql(
                &held.entry.sql,
                &TableInfo::subquery(held.entry.table.clone(), 0, Vec::new()),
                index_root,
            )
            .map(|index| index.partial_sql.is_some())
            .unwrap_or(true);
            if partial {
                continue;
            }
            let candidates = self.schema.covering.entry(table_root).or_default();
            if !candidates.contains(&index_root) {
                candidates.push(index_root);
            }
        }
        Ok(restored)
    }

    /// Returns the handle of a table named by one of the restored entries.
    ///
    /// @param entries - the entries as the catalog tree now holds them
    /// @param at - which attached database
    /// @param name - the table's name
    fn root_of_named(&self, entries: &[Recorded], at: usize, name: &[u8]) -> Option<u32> {
        let folded = name.to_ascii_lowercase();
        entries
            .iter()
            .find(|held| {
                held.entry.kind == ObjectKind::Table
                    && held.entry.name.to_ascii_lowercase() == folded
            })
            .and_then(|held| self.handle_of(at, held.entry.tree_id))
    }

    /// Derives one entry's tree shape from its stored `CREATE` text.
    ///
    /// The same derivation `define_table` and `create_index` make, from the same
    /// source: the text in the catalog row. An automatic index carries no text
    /// of its own, so it is reconstructed from its table's - which is what the
    /// loader does for one too.
    ///
    /// @param entries - the entries as the catalog tree now holds them
    /// @param held - the entry whose tree is being rebuilt
    /// @param root - the handle it will be registered under
    fn shape_of_entry(
        &self,
        entries: &[Recorded],
        held: &Recorded,
        root: u32,
    ) -> Option<(Vec<ColumnSpec>, usize, SourceLayout)> {
        match held.entry.kind {
            ObjectKind::Table => {
                let info = table_from_create_sql(&held.entry.sql, 0, root).ok()?;
                if info.without_rowid {
                    keyed_table_shape(&info).ok()
                } else {
                    let (columns, layout) = table_shape(&info);
                    Some((columns, 1, layout))
                }
            }
            ObjectKind::Index => {
                let owner = entries.iter().find(|other| {
                    other.entry.kind == ObjectKind::Table
                        && other.entry.name.eq_ignore_ascii_case(&held.entry.table)
                })?;
                let table = table_from_create_sql(&owner.entry.sql, 0, owner.root).ok()?;
                // An automatic index is declared by the *table's* text, and a
                // created one by its own - the catalog stores an empty `sql`
                // for the first, which is what tells the two apart.
                let mut index = if held.entry.sql.is_empty() {
                    let folded = held.entry.name.to_ascii_lowercase();
                    table
                        .indexes
                        .iter()
                        .find(|index| index.folded == folded)?
                        .clone()
                } else {
                    inillucent_catalog::load::index_from_create_sql(&held.entry.sql, &table, root)
                        .ok()?
                };
                index.root = root;
                let (columns, layout) = index_shape(&table, &index, root);
                let key_columns = columns.len();
                Some((columns, key_columns, layout))
            }
            _ => None,
        }
    }

    /// Abandons the open transaction.
    ///
    /// **Every step runs, and the first failure is reported afterwards.** A
    /// rollback is not a step that can be declined: the caller has said the
    /// transaction is over, and returning early from the middle of it would
    /// leave the rows undone and the connection still believing a transaction
    /// and its savepoints were open - a state with no name, which the next
    /// statement would inherit. So the module notification, the undo and the
    /// bookkeeping all happen, and only then is an error returned.
    pub fn rollback(&mut self) -> DbResult<()> {
        // **The modules are told, or the connection goes on answering out of a
        // transaction that did not happen.** See `rollback_modules`: the file
        // was always put back correctly, and the module's own buffer was not.
        self.session_state.modules_begun.set(false);
        let told = self.rollback_modules(None);
        let undone = self.undo_to(None);
        self.writing.marks().borrow_mut().clear();
        // `undo_to(None)` has already cut it to zero; clearing it again is what
        // makes that true even when the undo above failed part-way. An
        // abandoned transaction frees nothing.
        self.writing.pending_frees().borrow_mut().clear();
        // Cleared rather than released, because `undo_to(None)` has already
        // given these pages back. An entry still here is one the undo failed
        // part-way through, and releasing it a second time would hand a page to
        // the free map twice.
        self.writing.built().borrow_mut().clear();
        self.writing.set_batch(None);
        self.writing.set_implicit_transaction(false);
        // Rolled back, so no-steal has nothing left to hold back on any
        // schema this transaction touched - read before `touched` is cleared
        // below, which is the only record of which schemas those were.
        for at in schemas_in(self.writing.touched()) {
            if let Some(database) = self.schema_file(at) {
                database.pool().set_uncommitted_lsn(u64::MAX);
            }
        }
        // Nothing to decide: an abandoned transaction has no commit for a
        // super-journal to be about, and the records it left are never replayed
        // because no `Commit` follows them.
        self.writing.set_touched(0);
        // The transaction's own setting goes with the transaction, which is
        // SQLite's rule for `PRAGMA defer_foreign_keys`.
        self.pragmas.set_defer_foreign_keys(false);
        self.refresh_catalog();
        undone?;
        told?;
        Ok(())
    }

    /// Names a point the transaction can be rolled back to.
    ///
    /// **Every module is flushed here**, which is what makes rolling back to
    /// this point correct for a module that buffers. The undo log records
    /// writes to the shadow *trees*, so anything a module is still holding in
    /// memory is invisible to it - and a later `ROLLBACK TO` would either keep
    /// staged rows belonging to the abandoned part, or throw away rows written
    /// before the point. Flushing now puts everything before the point under
    /// the undo log, so the buffer that is left belongs entirely to the part
    /// that may be abandoned.
    ///
    /// A savepoint is rare and a flush is not free, which is the right way
    /// round: the alternative is a module buffer the undo log cannot see.
    ///
    /// @param name - the savepoint's name
    /// Commits the open transaction, if there is one.
    ///
    /// A no-op outside a transaction, so a caller can commit at a boundary
    /// without having to know whether it opened one.
    pub fn commit_batch(&mut self) -> DbResult<()> {
        // **A deferred key is checked here, and a failure means the commit does
        // not happen.** That is SQLite's rule and the whole meaning of
        // `DEFERRABLE INITIALLY DEFERRED`: the rows are allowed to be
        // inconsistent inside the transaction and are required to be consistent
        // at its end. The transaction is left open so the caller can repair it
        // or roll it back, which is what SQLite does too.
        self.check_deferred_foreign_keys()?;
        // Every module flushes what it is holding before the log's commit
        // record, because what it flushes is more writes.
        self.sync_modules()?;
        // `PRAGMA defer_foreign_keys` is the transaction's setting, not the
        // connection's, and SQLite clears it at each commit and rollback.
        if self.pragmas.defer_foreign_keys() {
            self.pragmas.set_defer_foreign_keys(false);
            self.forget_compiled_statements();
        }
        self.session_state.modules_begun.set(false);
        // Nothing to abandon once it is committed, and holding the before-images
        // would hold every row a long transaction touched.
        self.writing.undo().borrow_mut().clear();
        self.writing.marks().borrow_mut().clear();
        self.writing.set_implicit_transaction(false);
        let Some(txn) = self.writing.take_batch() else {
            self.writing.set_touched(0);
            return Ok(());
        };
        // Before `touched` is read, because freeing a page is a write to that
        // schema and the participant set has to say so.
        self.flush_pending_frees(txn)?;
        let participants = self.writing.replace_touched(0);
        self.commit_across(txn, participants)
    }

    /// Returns where a statement's writes begin, in each record.
    ///
    /// Taken before the statement writes anything; the success path does
    /// nothing with it and the failure path rolls back to it. That asymmetry is
    /// the whole cost of statement atomicity inside a transaction - three
    /// integers read off a `Vec`'s length - which is why there is no
    /// per-statement savepoint and `txn.large`'s two thousand statements do not
    /// pay for two thousand of them.
    ///
    /// The second length is the pending-free list's, so a statement that dropped
    /// a tree and then failed drops nothing. A DML statement never adds to that
    /// list; the statement this protects is the one that runs a module's `DROP`.
    ///
    /// The third is the list of trees built, so a statement that built one and
    /// then failed gives its pages back rather than leaving them allocated with
    /// nothing naming them - a `CREATE TABLE ... AS SELECT` whose select fails
    /// part way through, and every `ALTER TABLE` that rebuilds a tree and then
    /// refuses.
    pub(crate) fn statement_mark(&self) -> crate::engine::state::StatementMark {
        crate::engine::state::StatementMark {
            undo: self.writing.undo().borrow().len(),
            dropped: self.writing.pending_frees().borrow().len(),
            built: self.writing.built().borrow().len(),
        }
    }

    /// Gives back the pages of every tree built after a point in the
    /// transaction.
    ///
    /// **Called only when that part of the transaction is being abandoned.** A
    /// page here was allocated by a transaction that is not going to commit, so
    /// the on-disk free map never learned about it and an in-memory
    /// [`inillucent_pool::Database::release`] is the whole of the correction.
    /// No `FreePage` record is written and none is wanted: recovery replays a
    /// record only for a transaction whose `Commit` follows it, so the
    /// `AllocPage` this undoes is never replayed either, and a `FreePage` for a
    /// page that was never claimed would be a free of something the recovered
    /// file still uses.
    ///
    /// The tree is walked from the root page rather than through the handle,
    /// because the handle may by now hold nothing - the tree was dropped later
    /// in the same transaction - or a different tree, which is what an
    /// `ALTER TABLE` that rebuilds under the same handle leaves. Nothing has
    /// freed those pages in the meantime, because a drop's frees wait for a
    /// commit this transaction is not going to reach, so the tree is still
    /// there to walk.
    ///
    /// @param floor - how long the list was when the abandoned part began
    fn release_built_trees(&mut self, floor: usize) -> DbResult<()> {
        let abandoned: Vec<crate::engine::state::BuiltTree> = {
            let mut built = self.writing.built().borrow_mut();
            if built.len() <= floor {
                return Ok(());
            }
            built.split_off(floor)
        };
        // Newest first, for the reason the undo buffer is drained newest first:
        // a handle built over twice inside one transaction has two entries, and
        // the schema should end up holding neither.
        for tree in abandoned.into_iter().rev() {
            let at = tree.schema;
            let Some(file) = self.schema_file(at) else {
                continue;
            };
            let released = inillucent_tree::paged::released(file.pool(), tree.page)?;
            // **The handle only goes if it still holds this tree.** A rollback
            // to a savepoint may be abandoning a rebuild whose earlier tree is
            // about to be re-attached under the same handle by
            // `reload_entries`, and taking the handle out for a tree that is
            // not the one being released would strand the one that is staying.
            if self
                .schema
                .trees
                .get(&tree.root)
                .is_some_and(|held| held.root() == tree.page)
            {
                self.schema.trees.remove(&tree.root);
                self.schema.layouts.remove(&tree.root);
                self.schema.covering.remove(&tree.root);
                for roots in self.schema.covering.values_mut() {
                    roots.retain(|held| *held != tree.root);
                }
                self.session_state.owner.remove(&tree.root);
            }
            for reference in released.values {
                self.free_built_value(at, reference)?;
            }
            let session = self.session_state.session.get();
            let database = file_of(
                &mut self.storage.database,
                &mut self.session_state.attached,
                &mut self.session_state.temps,
                session,
                at,
            )?;
            for page in released.pages {
                database.release(page, 1)?;
            }
        }
        Ok(())
    }

    /// Gives back one out-of-line value written by an abandoned transaction.
    ///
    /// **The values go back before the tree's own pages and through
    /// `free_extent`**, for the same reason a `DROP` does: a value small enough
    /// to be packed shares its page with values from other trees, and the page
    /// is only free once the last live slot on it goes. A value the abandoned
    /// transaction packed onto a page that existed before it leaves that page
    /// alone and clears one slot, which is the correct answer - the page is not
    /// this transaction's to give back.
    ///
    /// @param at - which attached database the value lives in
    /// @param reference - the value, as its leaf held it
    fn free_built_value(
        &mut self,
        at: usize,
        reference: inillucent_pool::extent::ExtentRef,
    ) -> DbResult<()> {
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a rollback names a database that is not attached"))?;
        let uncommitted = self.uncommitted_handle_of(at);
        let mut log = WalLog {
            wal,
            // The records this writes belong to the transaction being
            // abandoned, so nothing replays them - see `release_built_trees`.
            txn: self.current_txn(),
            schema: at,
            wrote: false,
            undo: None,
            uncommitted,
        };
        let session = self.session_state.session.get();
        let database = file_of(
            &mut self.storage.database,
            &mut self.session_state.attached,
            &mut self.session_state.temps,
            session,
            at,
        )?;
        inillucent_tree::paged::free_extent(database, &mut log, reference)
    }

    /// Gives every page this transaction dropped back to the free map.
    ///
    /// **The last thing a transaction does before its commit record, and the
    /// only place a dropped page is freed (task-2043).** `release_tree` records
    /// the pages and does not free them; see its doc comment for what freeing
    /// them as the statement ran cost. By the time this runs the transaction is
    /// going to commit, so a page it frees is a page nothing can point at again.
    ///
    /// The `FreePage` records go in here too. The log is redo-only: a record is
    /// replayed only for a transaction whose `Commit` follows it, so writing
    /// them at the drop would have described a free that a rollback then did not
    /// do, and recovery would have replayed a free of pages the recovered
    /// database still uses.
    ///
    /// Each schema it frees in is marked as written, because a free map is part
    /// of that file and a commit is decided over the files it changed.
    ///
    /// @param txn - the transaction the frees are logged under
    /// **A value is freed through `paged::free_extent` rather than released
    /// (task-2065).** A page of the dropped tree belongs to that tree alone and
    /// goes straight back. A page an out-of-line value sits on may hold small
    /// values from other trees, so the reference is what was recorded and
    /// `free_extent` is what decides: it clears the slot, and gives the page
    /// back only when the last live slot on it goes. A value written as a run
    /// of whole pages takes every page of the run with it. `free_extent`
    /// appends its own `FreePage` records, which is why the loop below leaves
    /// a value's records to it.
    ///
    /// @param txn - the transaction the frees are logged under
    pub(crate) fn flush_pending_frees(&mut self, txn: u64) -> DbResult<()> {
        use crate::engine::state::Freed;
        let waiting: Vec<crate::engine::state::PendingFree> =
            std::mem::take(&mut *self.writing.pending_frees().borrow_mut());
        if waiting.is_empty() {
            return Ok(());
        }
        let mut touched = self.writing.touched();
        for dropped in &waiting {
            let at = dropped.schema;
            touched |= crate::schema_bit(at);
            let Freed::Page(page) = dropped.what else {
                continue;
            };
            let wal = self
                .log_of(at)
                .ok_or_else(|| refusal("a commit names a database that is not attached"))?;
            wal.append(txn, inillucent_wal::record::Body::FreePage { page: page.0 })?;
        }
        // **The two kinds cannot collide, which is why the order they were
        // recorded in is the order they are freed in.** A tree's own pages and
        // the pages its values sit on are disjoint sets: a leaf is not an
        // extent page and a shared extent page is not reachable as a child of
        // any tree. Nothing in this loop allocates either - the only allocator
        // is `Database::allocate` - so a page freed here cannot be handed back
        // out before the loop ends and be written over by the free after it.
        for dropped in waiting {
            let at = dropped.schema;
            match dropped.what {
                Freed::Page(page) => {
                    let session = self.session_state.session.get();
                    let database = file_of(
                        &mut self.storage.database,
                        &mut self.session_state.attached,
                        &mut self.session_state.temps,
                        session,
                        at,
                    )?;
                    database.release(page, 1)?;
                }
                Freed::Value(reference) => self.free_dropped_value(at, txn, reference)?,
            }
        }
        self.writing.set_touched(touched);
        Ok(())
    }

    /// Gives one dropped out-of-line value's pages back.
    ///
    /// Split out because `paged::free_extent` needs the file and a log at the
    /// same time, and building the log takes a borrow of the connection that
    /// the loop above cannot hold across `file_of`.
    ///
    /// @param at - which attached database the value lives in
    /// @param txn - the transaction the frees are logged under
    /// @param reference - the value, as its leaf held it
    fn free_dropped_value(
        &mut self,
        at: usize,
        txn: u64,
        reference: inillucent_pool::extent::ExtentRef,
    ) -> DbResult<()> {
        let wal = self
            .log_of(at)
            .ok_or_else(|| refusal("a commit names a database that is not attached"))?;
        let uncommitted = self.uncommitted_handle_of(at);
        let mut log = WalLog {
            wal,
            txn,
            schema: at,
            wrote: false,
            // The free is not undoable: it happens at the commit, which is
            // past the point where anything can be abandoned.
            undo: None,
            uncommitted,
        };
        let session = self.session_state.session.get();
        let database = file_of(
            &mut self.storage.database,
            &mut self.session_state.attached,
            &mut self.session_state.temps,
            session,
            at,
        )?;
        inillucent_tree::paged::free_extent(database, &mut log, reference)
    }

    /// Commits one transaction across every file it wrote.
    ///
    /// **One file is the path this engine has always taken; two is a
    /// super-journal.** A transaction that wrote a single database appends one
    /// `Commit` record and waits for it, exactly as before - no marker, no extra
    /// file, no stat. A transaction that wrote two or more files that will be
    /// recovered writes a super-journal listing them, marks each one as being in
    /// doubt, appends every vote, and then deletes the super-journal. That
    /// deletion is the commit: before it, every participant recovers without the
    /// transaction; after it, every one recovers with it.
    ///
    /// A temporary database is not a participant. It has no file, so it has no
    /// recovery to be in doubt about, and including it would make an ordinary
    /// `CREATE TEMP TABLE ... INSERT` pay for a protocol that decides nothing.
    ///
    /// @param txn - the transaction to commit
    /// @param participants - the schemas it wrote
    pub(crate) fn commit_across(&mut self, txn: u64, participants: u16) -> DbResult<()> {
        // **Every commit passes through here, which is why the record of what
        // this transaction built is cleared here and nowhere else
        // (task-2065).** That list exists so an abandoned transaction can give
        // its allocations back, and a committed allocation is not one anybody
        // takes back - so leaving an entry on it past a commit would let a
        // later rollback to floor zero free the pages of a table that is
        // committed and live. Clearing it at each of the commit paths instead
        // would be the same rule stated three times, and a fourth commit path
        // added later would not know about it.
        //
        // After the undo rather than before it: `abandon`'s `OR FAIL` path
        // undoes the failed statement and then commits what came before, and
        // the undo is what needs the list.
        self.writing.built().borrow_mut().clear();
        let durable: Vec<usize> = schemas_in(participants)
            .filter(|at| self.path_of(*at).is_some())
            .collect();
        self.writing.set_decided_over(durable.len());
        if durable.len() < 2 {
            return self.vote(txn, participants);
        }
        let files: Vec<PathBuf> = durable.iter().filter_map(|at| self.path_of(*at)).collect();
        let near = self.storage.path.clone();
        let mut journal = multi::SuperJournal::create(&near, txn, &files)?;
        // **Every marker is durable before any vote is.** A `Commit` that
        // reached the disk while its marker had not would be replayed by a
        // recovery that never learned to doubt it, which is the one ordering
        // this protocol cannot get wrong.
        for at in &durable {
            let Some(path) = self.path_of(*at) else {
                continue;
            };
            if let Err(error) = journal.mark(&path, txn) {
                journal.abandon();
                return Err(error);
            }
        }
        if let Err(error) = self.vote(txn, participants) {
            // **Not abandoned, and that is the point.** Some participants may
            // already have their `Commit` on disk; removing the super-journal
            // would make those count and the rest not, which is precisely the
            // torn commit this protocol exists to prevent. Leaving it makes
            // every vote a vote that lost, which is the outcome that is
            // consistent. Dropping the handle removes nothing: `SuperJournal`
            // has no destructor precisely so that the safe outcome is the one a
            // path which returns early gets for free.
            drop(journal);
            return Err(error);
        }
        journal.commit()
    }

    /// Appends and awaits one `Commit` record per schema a transaction wrote.
    ///
    /// @param txn - the transaction
    /// @param participants - the schemas it wrote
    fn vote(&mut self, txn: u64, participants: u16) -> DbResult<()> {
        // A transaction that wrote nothing still commits `main`, which is what
        // an empty `BEGIN; COMMIT;` has always done and what keeps the
        // transaction numbers in step with the log.
        let mut wrote_any = false;
        for at in schemas_in(participants) {
            let Some(wal) = self.log_of(at) else {
                continue;
            };
            wal.commit(txn, txn)?;
            if let Some(database) = self.schema_file(at) {
                database.pool().set_durable_lsn(wal.write_ahead_point());
                // Committed, so no-steal has nothing left to hold back on
                // this schema until its next transaction's first record.
                database.pool().set_uncommitted_lsn(u64::MAX);
            }
            wrote_any = true;
        }
        if !wrote_any {
            self.storage.wal.commit(txn, txn)?;
            self.storage
                .database
                .pool()
                .set_durable_lsn(self.storage.wal.write_ahead_point());
        }
        Ok(())
    }

    /// Returns how many databases the last commit was decided over.
    ///
    /// One, or none, for every transaction that wrote a single file - which is
    /// every statement the performance gate measures. Two or more is a
    /// super-journal.
    pub fn decided_over(&self) -> usize {
        self.writing.decided_over()
    }
}
