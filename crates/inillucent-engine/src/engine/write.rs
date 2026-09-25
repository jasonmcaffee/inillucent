//! Where one statement's writes go, and what they can be undone to.
//!
//! Invariant: **a statement writes through one log per schema it touches, and
//! all of them record into the transaction's single undo list.** A write that
//! touches `main` and a `TEMP` table in the same breath appends to two logs,
//! each its own file's; the undo list is what lets one `ROLLBACK` put both
//! files back.
//!
//! [`Logs`] is an enum rather than a `Vec` because almost every connection
//! holds one file, and a heap allocation on every write is measurable there.
//! The measurement is on the type.

use crate::*;

/// The logs one statement writes through, indexed by schema number.
///
/// **Inline for a connection that has one file, which is almost every
/// connection.** A `Vec` here is a heap allocation on every write, and
/// `txn.batched` - two thousand statements inside one transaction - is where
/// that shows: 6.17x to 6.77x across four measured runs, 5.71x to 6.22x with the
/// allocation, on the same fixture, the same rounds and the same machine. It is
/// the only measurable cost the multi-schema write path had, and this is it
/// removed rather than argued away.
pub(crate) enum Logs<'a> {
    /// The only file this connection holds.
    One(WalLog<'a>),
    /// `main`, then `temp`, then the attachments, indexed by schema number.
    Many(Vec<WalLog<'a>>),
}

impl<'a> Logs<'a> {
    /// Returns the log one schema writes through.
    ///
    /// @param at - the schema, as the binder numbers them
    fn get_mut(&mut self, at: usize) -> Option<&mut WalLog<'a>> {
        match self {
            // A connection with one file has one schema, so a handle can only
            // have resolved to `main`; anything else is a plan naming a database
            // that is not there, and the caller refuses it.
            Logs::One(log) => (at == MAIN).then_some(log),
            Logs::Many(held) => held.get_mut(at),
        }
    }

    /// Returns the schemas anything was written through, one bit each.
    pub(crate) fn wrote(&self) -> u16 {
        match self {
            Logs::One(log) => {
                if log.wrote {
                    schema_bit(log.schema)
                } else {
                    0
                }
            }
            Logs::Many(held) => held
                .iter()
                .filter(|log| log.wrote)
                .fold(0, |mask, log| mask | schema_bit(log.schema)),
        }
    }
}

/// The disjoint halves of an [`ImportedDatabase`] a write borrows.
///
/// A write needs `&mut Database` and `&mut PagedTree` at the same instant while
/// the log holds a shared borrow of a third field. Naming the three borrows in
/// one struct is what lets the borrow checker see they are disjoint; a method
/// taking `&mut self` could not, because it would borrow the log too.
pub(crate) struct WriteView<'a> {
    /// The file this connection was opened on.
    pub(crate) database: &'a mut Database,
    /// The files `ATTACH` added beside it.
    pub(crate) attached: &'a mut [Attached],
    /// The temporary databases, one per connection that has one.
    pub(crate) temps: &'a mut [Attached],
    /// The connection this statement belongs to, which is what makes `temp`
    /// mean one of the above rather than another.
    pub(crate) session: u64,
    /// One log per schema, `main`'s first, built once for the statement.
    ///
    /// **One per file, because a statement can write more than one.** A `TEMP`
    /// trigger firing on a write to `main` writes rows into two files, and each
    /// one has to be described in its own log. They are built once per statement
    /// rather than per write, so a statement pays one `Rc` clone per schema
    /// rather than one per row - and none at all beyond the first when the
    /// connection has one file.
    pub(crate) logs: Logs<'a>,
    /// Which schema each tree handle belongs to, for handles that are not
    /// `main`'s.
    pub(crate) owner: &'a HashMap<u32, usize>,
    pub(crate) trees: &'a mut HashMap<u32, PagedTree>,
    pub(crate) layouts: &'a HashMap<u32, std::rc::Rc<SourceLayout>>,
    /// The tables an index a module owns is built over, by root page.
    ///
    /// The write reports what it stored and removed for these and for nothing
    /// else, and the engine applies both to the module afterwards. See
    /// `Changes::written`.
    pub(crate) indexed: &'a HashMap<u32, Vec<VectorIndex>>,
    /// What this statement has written, as
    /// `(rows the statement wrote itself, rows written in all, last rowid)`.
    ///
    /// **The tally that survives a failure.** The `Changes` a write builds is
    /// lost the moment it raises, and `OR FAIL` keeps what it wrote - so
    /// `changes()`, `total_changes()` and `last_insert_rowid()` are read off
    /// the view afterwards on either path. It is a fresh view per statement,
    /// so there is nothing to reset.
    pub(crate) counted: std::cell::Cell<(i64, i64, Option<i64>)>,
    /// Which index trees cover which table, so a query a trigger body runs
    /// inside the write reaches the same covering indexes a typed one does.
    pub(crate) covering: &'a HashMap<u32, Vec<u32>>,
    /// What this connection has registered - `docs/roadmap.md` item 13.
    pub(crate) registry: &'a inillucent_ext::registry::Registry,
    /// The virtual tables this connection has connected, so a statement
    /// running inside the write can read one.
    pub(crate) modules: &'a HashMap<Vec<u8>, crate::vtab::Connected>,
    /// The connection's settings, for the limits and the `LIKE` rule a
    /// module's scan is run under.
    pub(crate) pragmas: &'a Pragmas,
    /// The schema a module that introspects is handed.
    pub(crate) schema_catalog: &'a inillucent_sql::catalog_view::StaticCatalog,
    /// The trigger bodies' writes to virtual tables, in the order they fired.
    ///
    /// Made by the engine after the write hands the trees back; see
    /// `WriteTarget::defer_module_write`.
    pub(crate) deferred: Vec<inillucent_exec::dml::ModuleWrite>,
}

impl WriteView<'_> {
    /// Runs a module's scan for a statement inside the write.
    ///
    /// The same scan the connection runs, reached through what the view
    /// holds; see `crate::vtab::ScanReach`. The tables the connection answers
    /// from its own state - the `pragma_*` functions and the four that describe
    /// statements - are refused by name, because the view does not hold it.
    ///
    /// @param table - the FROM term's table
    /// @param path - the access path the planner chose
    /// @param params - the values bound to `?1`, `?2`, ...
    /// @param needed - which of the term's columns the query reads
    /// @param supplied - a lateral join's values, one per offered constraint
    /// @param downstream - where the rows go
    fn scan_inside(
        &self,
        table: &TableInfo,
        path: &inillucent_sql::plan::AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
        supplied: &[inillucent_tree::datum::OwnedDatum],
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<bool> {
        let answered_by_connection = table.folded.starts_with(b"pragma_")
            || matches!(
                table.folded.as_slice(),
                b"bytecode" | b"tables_used" | b"sqlite_stmt" | b"completion"
            );
        if answered_by_connection {
            let name = String::from_utf8_lossy(&table.name).into_owned();
            return Err(refusal(format!(
                "{name} cannot be read by a statement that runs inside a write, such as a \
                 trigger body or a subquery in an UPDATE's SET"
            ))
            .with_unsupported(format!("reading {name} inside a write")));
        }
        let reach = crate::vtab::ScanReach {
            modules: self.modules,
            registry: self.registry,
            pool: self.database.pool(),
            trees: &*self.trees,
            limits: self.pragmas.limits(),
            catalog: self.schema_catalog,
            folding: self,
            case_sensitive_like: self.pragmas.case_sensitive_like(),
        };
        crate::vtab::scan_module(&reach, table, path, params, needed, supplied, downstream)
    }

    /// Returns which schema a tree handle belongs to; `MAIN` when it is
    /// `main`'s.
    ///
    /// @param root - the handle
    fn schema_of(&self, root: u32) -> usize {
        if self.attached.is_empty() && self.temps.is_empty() {
            return MAIN;
        }
        self.owner.get(&root).copied().unwrap_or(MAIN)
    }
}

impl WriteTarget for WriteView<'_> {
    fn count_row(&self, outer: bool) {
        let (own, all, rowid) = self.counted.get();
        self.counted.set((
            own.saturating_add(i64::from(outer)),
            all.saturating_add(1),
            rowid,
        ));
    }

    fn count_rowid(&self, rowid: i64) {
        let (own, all, _) = self.counted.get();
        self.counted.set((own, all, Some(rowid)));
    }

    fn rows_written(&self) -> (i64, i64, Option<i64>) {
        self.counted.get()
    }

    fn parts_for(
        &mut self,
        root: u32,
    ) -> DbResult<(&mut Database, &mut dyn Trees, &mut dyn TreeLog)> {
        let at = self.schema_of(root);
        // Three separate fields of `self`, which is what lets the borrow checker
        // see that the file, the trees and the log are disjoint - the same
        // arrangement this view has always had, one file wider.
        let log = self
            .logs
            .get_mut(at)
            .ok_or_else(|| refusal("a write names a database that is not attached"))?;
        let database = if at == MAIN {
            &mut *self.database
        } else {
            &mut schema_of_index(self.attached, self.temps, self.session, at)
                .ok_or_else(|| refusal("a write names a database that is not attached"))?
                .database
        };
        Ok((database, self.trees, log))
    }

    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>> {
        self.layouts.get(&root)
    }

    fn catalog(&self) -> &dyn TreeCatalog {
        self
    }

    fn captures(&self, root: u32) -> bool {
        self.indexed.contains_key(&root)
    }

    fn defer_module_write(&mut self, write: inillucent_exec::dml::ModuleWrite) -> DbResult<()> {
        self.deferred.push(write);
        Ok(())
    }
}

/// The write's own view of the trees, read as a planned query reads them.
///
/// **The same trees, seen the other way round.** A trigger body is a statement
/// and has to find its rows, and it fires in the middle of a write that is
/// already holding these trees mutably. Answering as a [`TreeCatalog`] as well
/// is what lets `DELETE FROM child WHERE parent_id = OLD.id` reach the ordinary
/// planner - and so the ordinary index probe - rather than a scan written a
/// second time inside the write path.
///
/// A module's rows are read through the fields the view holds beside the
/// trees, which are all a module's scan reads: see `crate::vtab::ScanReach`.
/// A trigger body can therefore read an FTS5 table, and an `UPDATE` can set a
/// column from `(SELECT ... FROM json_each(...))`.
impl TreeCatalog for WriteView<'_> {
    fn pool_for(&self, root: u32) -> Option<&Pool> {
        let at = self.schema_of(root);
        if at == MAIN {
            return Some(self.database.pool());
        }
        let held = match at {
            TEMP => self
                .temps
                .iter()
                .find(|held| held.session == Some(self.session))?,
            _ => self.attached.get(at.saturating_sub(FIRST_ATTACHED))?,
        };
        Some(held.database.pool())
    }

    fn tree(&self, root: u32) -> Option<&PagedTree> {
        self.trees.get(&root)
    }

    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>> {
        self.layouts.get(&root)
    }

    fn covering_candidates(&self, table_root: u32) -> Vec<u32> {
        self.covering.get(&table_root).cloned().unwrap_or_default()
    }

    fn virtual_cursor(
        &self,
        table: &TableInfo,
        path: &inillucent_sql::plan::AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
        downstream: &mut dyn inillucent_exec::ops::Sink,
    ) -> DbResult<bool> {
        self.scan_inside(table, path, params, needed, &[], downstream)
    }

    fn virtual_rows_supplied(
        &self,
        table: &TableInfo,
        path: &inillucent_sql::plan::AccessPath,
        params: &Params,
        needed: &inillucent_sql::bind::ColumnUse,
        supplied: &[inillucent_tree::datum::OwnedDatum],
    ) -> DbResult<Option<Vec<Vec<inillucent_tree::datum::OwnedDatum>>>> {
        let collected = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut sink = inillucent_exec::ops::CollectInto::new(std::rc::Rc::clone(&collected));
        if !self.scan_inside(table, path, params, needed, supplied, &mut sink)? {
            return Ok(None);
        }
        let rows = collected.borrow().clone();
        Ok(Some(rows))
    }

    // `docs/roadmap.md` item 13.
    fn user_scalar(&self, name: &[u8], argc: usize) -> Option<inillucent_exec::expr::ScalarBody> {
        match self.registry.function(name, argc)?.body.clone() {
            inillucent_ext::registry::UserBody::Scalar(body) => {
                Some(inillucent_exec::expr::ScalarBody(body))
            }
            inillucent_ext::registry::UserBody::Aggregate(_) => None,
        }
    }

    fn user_scalar_is_deterministic(&self, name: &[u8], argc: usize) -> bool {
        self.registry
            .function(name, argc)
            .is_some_and(|function| function.flags.deterministic)
    }
}

/// A [`TreeLog`] that writes to the database's own write-ahead log.
///
/// Every record carries the transaction it belongs to, which is what lets
/// recovery tell a committed change from one whose commit never arrived.
///
/// It holds no handle of its own: when the pool needs to write a page the log
/// has not reached, the pool asks the log directly through the closure
/// `let_the_pool_ask_the_log` registers. See `Pool::on_log_behind`.
pub(crate) struct WalLog<'a> {
    /// The log of the file this one writes into.
    ///
    /// **Owned rather than borrowed.** A write holds its schema's file mutably
    /// while it appends, and a borrow of the log out of the same `Attached`
    /// would be a second borrow of it. An `Rc` clone is a refcount bump, paid
    /// once per schema per statement.
    pub(crate) wal: std::rc::Rc<Wal>,
    pub(crate) txn: u64,
    /// Which schema this log belongs to, as the binder numbers them.
    ///
    /// Stamped onto every before-image, so a rollback puts a row back into the
    /// file it came out of. A tree identifier alone could not say: two files
    /// number their own trees from one.
    pub(crate) schema: usize,
    /// Whether anything has been written through this log.
    ///
    /// **The participant set a cross-file commit needs**, collected where it is
    /// free. A transaction that wrote one file commits the way it always did; a
    /// transaction that wrote two is decided by a super-journal, and this is how
    /// the commit knows which it is.
    pub(crate) wrote: bool,
    /// Where before-images go while a transaction is open, or `None` outside
    /// one.
    ///
    /// An autocommit statement cannot be abandoned, so it collects nothing and
    /// pays nothing for the possibility. The buffer is handed in by the caller
    /// rather than owned here because it has to outlive the log: the log lives
    /// for one statement and the transaction for many.
    pub(crate) undo: Option<&'a std::cell::RefCell<Vec<Before>>>,
    /// This schema's own no-steal watermark - `u64::MAX` until something is
    /// open, or the open transaction's first record.
    ///
    /// Shared with the schema's `Pool`, which is the only other reader:
    /// arming it here is the one place that knows which record was first, and
    /// `Pool::writeback` is the one place that must not write a page stamped
    /// at or above it. See `Pool::holds_uncommitted`.
    pub(crate) uncommitted: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl TreeLog for WalLog<'_> {
    fn log(&mut self, body: Body<'_>) -> DbResult<u64> {
        self.wrote = true;
        let lsn = self.wal.append(self.txn, body)?;
        // Armed once, on the first record since the last commit or rollback -
        // never rearmed while it is already set, so a later record in the
        // same transaction (an ordinary write, or an undo's own restore)
        // cannot move the watermark past the point recovery must not pass.
        if self.uncommitted.load(std::sync::atomic::Ordering::SeqCst) == u64::MAX {
            self.uncommitted
                .store(lsn, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(lsn)
    }

    fn wants_undo(&self) -> bool {
        self.undo.is_some()
    }

    fn undo(
        &mut self,
        tree: u64,
        key: &[Datum<'_>],
        before: Option<Vec<OwnedDatum>>,
    ) -> DbResult<()> {
        if let Some(buffer) = self.undo {
            // **The key is copied only when there is no row to put back.** A
            // restore that has a row calls `put`, which reads the key columns
            // out of the row itself; copying them a second time allocated a
            // vector per write and threw it away on every update and delete.
            let key = match before {
                Some(_) => Vec::new(),
                None => key.iter().map(OwnedDatum::from_datum).collect(),
            };
            buffer.borrow_mut().push(Before {
                schema: self.schema,
                tree,
                key,
                row: before,
            });
        }
        Ok(())
    }
}

/// One row as it was before a statement inside a transaction changed it.
///
/// **Not `inillucent_txn::Undo`, and the difference is worth stating.** That
/// type carries a key and a before-image as `Vec<u8>`, because the transaction
/// engine below works in encoded rows. This engine's rows are `OwnedDatum`
/// vectors all the way down - the write path takes them, the trees store them
/// as PAX mini-columns, and there is no row-bytes encoding to borrow. Encoding
/// a row to bytes to record it and decoding it to restore it would be inventing
/// a third representation to bridge two that already exist.
///
/// So the two undo buffers are not duplicates of each other; they are the same
/// idea at two layers that disagree about what a row is, and that disagreement
/// is why routing this engine's writes through `inillucent_txn::Transaction` is
/// a piece of work rather than a wiring job.
#[derive(Clone, Debug)]
pub(crate) struct Before {
    /// Which schema the tree is in, as the binder numbers them.
    ///
    /// **Because a tree identifier is a file's number, not a connection's.**
    /// Two databases each number their own trees from one, so an undo record
    /// naming tree 7 says nothing until it also says which file - and a rollback
    /// that guessed would restore a row into the wrong database.
    pub(crate) schema: usize,
    /// The tree the row is in, by the identifier its own file knows it by.
    pub(crate) tree: u64,
    /// The row's key columns, and empty when `row` carries them.
    ///
    /// A restore with a row to write back finds the key inside it, so the copy
    /// is made only for the case that needs one: a key that was not there, put
    /// back by deleting it again.
    pub(crate) key: Vec<OwnedDatum>,
    /// The whole row as it was, or `None` when the key was not there.
    pub(crate) row: Option<Vec<OwnedDatum>>,
}

/// Puts imported rows into the order the tree they are about to build compares
/// in.
///
/// **The import cannot rely on SQLite's physical order being ours.** It reads a
/// b-tree by walking it, so the rows arrive in the order *that* file kept them,
/// and there are two ways for that to differ from the order the new tree
/// defines. A `DESC` index column is stored descending by SQLite and ascending
/// here. A collated column is stored under SQLite's implementation of the
/// collation, and agreeing with it byte for byte is an assumption rather than a
/// fact.
///
/// A tree whose leaves are not in its own key order answers a **scan** exactly
/// right and a **seek** wrongly, because the descent binary-searches separators
/// it does not actually obey. That is why this was invisible until the write
/// path became the first thing to seek into an index: `members_score`, over
/// `(score DESC, email)`, imported out of order, and every delete against it
/// silently found nothing and left the entry behind.
///
/// The sort key is the tree's *own* encoding under the tree's *own* collations,
/// so there is no second opinion about ordering to drift from the first.
///
/// @param rows - the rows as the file gave them up
/// @param columns - the tree's column directory
/// @param key_columns - how many leading columns form the key
pub(crate) fn in_key_order(
    rows: Vec<Vec<OwnedDatum>>,
    columns: &[ColumnSpec],
    key_columns: usize,
) -> Vec<Vec<OwnedDatum>> {
    let collations: Vec<Collation> = columns
        .iter()
        .take(key_columns)
        .map(|spec| spec.collation)
        .collect();
    // And the directions, because "key order" is the *tree's* order and a
    // descending key column is part of what that order is. Sorting ascending
    // and then building a tree whose comparisons are descending produces a tree
    // that is sorted by nothing either half agrees with.
    let directions: Vec<bool> = columns
        .iter()
        .take(key_columns)
        .map(|spec| spec.descending)
        .collect();
    // **Sorted by comparing the values, not by encoding a key per row.**
    //
    // The version this replaces built a `Vec<u8>` key for every row, sorted the
    // pairs by memcmp and then rebuilt the vector - two moves of every row and
    // one allocation per row, to reproduce an order the values already have.
    // `compare_rows` is the comparison the tree's own search and its integrity
    // checker use, so sorting by it is what the tree will be read by, and the
    // encoded form is derived from the same order rather than defining it.
    //
    // It is a stable sort because a `sort_unstable` here would reorder rows
    // whose whole key is equal, and a bulk build's input is compared against
    // SQLite's index page for page.
    let mut rows = rows;
    rows.sort_by(|left, right| {
        for column in 0..key_columns {
            let (Some(one), Some(two)) = (left.get(column), right.get(column)) else {
                continue;
            };
            let order = inillucent_tree::types::compare_under(
                &one.borrow(),
                &two.borrow(),
                collations.get(column).copied().unwrap_or(Collation::Binary),
            );
            let order = if directions.get(column).copied().unwrap_or(false) {
                order.reverse()
            } else {
                order
            };
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        std::cmp::Ordering::Equal
    });
    rows
}
