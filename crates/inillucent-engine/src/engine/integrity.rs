//! Walking every tree and every index, and reporting the first thing that is wrong.
//!
//! Invariant: **a check reads and never repairs.** `PRAGMA integrity_check` is a
//! question, and a check that quietly fixed what it found would answer `ok` about
//! a file it had just changed.

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_exec::dml::Changes;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::{Datum, OwnedDatum};

use crate::*;

/// How much of a file a check reads.
///
/// **The distinction `quick_check` exists for, drawn where the measurement put
/// it.** Until there was a page walk the two pragmas were the same pass, and
/// the module comment said so because this engine had nothing cheaper to
/// offer. The obvious guess was that the page walk was the expensive half and
/// belonged only in `integrity_check`. It is not: it reads a tree's interior
/// pages and its leaves, and it takes the pages of an out-of-line value from
/// the reference in the leaf it is already holding rather than by reading the
/// value. Counted in page fetches off the pool over a table of sixty
/// out-of-line values, the whole walk cost 4 fetches on top of 133 - three per
/// cent.
///
/// The expensive half is the index pass, which walks each index and the table
/// it is on and merges them. So the line is drawn there, which is also where
/// the pinned SQLite draws it: its `quick_check` omits index content against
/// table content, `UNIQUE`, `CHECK` and `NOT NULL`, and still accounts for
/// every page.
///
/// `the_page_walk_is_not_what_quick_check_is_not_paying_for` counts both halves
/// off the pool rather than off a clock, so the number is the same on every
/// machine and a build where the two halves change places fails there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckDepth {
    /// Every tree's own shape, and who holds every page. `PRAGMA quick_check`.
    Quick,
    /// That, and every index against the table it is on.
    /// `PRAGMA integrity_check`.
    Full,
}

impl crate::ImportedDatabase {
    /// Checks every tree in every attached database, and their agreement.
    ///
    /// The campaign tests run this after every statement. A tree that has
    /// drifted structurally still answers a scan correctly for a long time,
    /// which is precisely why the check has to be a check rather than a query.
    ///
    /// The whole of it - a caller asking for less says so with
    /// [`ImportedDatabase::check_trees_to`].
    pub fn check_trees(&self) -> DbResult<()> {
        self.check_trees_to(CheckDepth::Full)
    }

    /// Checks every tree, reading as much of the file as the depth asks for.
    ///
    /// @param depth - how much of the file to read
    pub fn check_trees_to(&self, depth: CheckDepth) -> DbResult<()> {
        // **First, whether each file's page count can address a file at all.**
        // `integrity-check` answered `ok` about a 163,840-byte file whose meta
        // pages both claimed 2^60 pages, with both checksums resealed
        // (task-2066 section 4.2, item 16). Every check below walks trees by
        // following pointers, so a page count that describes no file is a
        // number none of them reads - and `paged::cursor` bounds the leaf
        // sibling chain by it, so an inflated one disables that cycle guard
        // too. The open path makes the same check; this is the one a caller
        // can run on a file that is already open. See
        // `Database::refuse_a_page_count_that_cannot_be_addressed` for why it
        // is the arithmetic that is checked and not the file's length.
        let mut seen = std::collections::BTreeSet::new();
        for root in self.schema.trees.keys() {
            let at = self.session_state.schema_of(*root);
            if !seen.insert(at) {
                continue;
            }
            self.schema_file(at)
                .ok_or_else(|| refusal("a tree names a database that is not attached"))?
                .refuse_a_page_count_that_cannot_be_addressed()?;
        }
        for (root, tree) in &self.schema.trees {
            let pool = self
                .schema_file(self.session_state.schema_of(*root))
                .ok_or_else(|| refusal("a tree names a database that is not attached"))?
                .pool();
            tree.check(pool)?;
        }
        // **And then whether the trees agree about who owns a page.** Every
        // check above is about one tree in isolation - its key order, its
        // sibling chain, its separators - and every one of them passes over a
        // file where two tables are reachable from the same page, because each
        // tree is a well formed tree. That state loses rows with no symptom at
        // the time, which is what `check_page_ownership` is for; see
        // `crate::engine::pages`.
        self.check_page_ownership()?;
        if depth == CheckDepth::Quick {
            return Ok(());
        }
        // **And then whether each index agrees with its table.** Nothing above
        // can see an index holding two entries under one `UNIQUE` key, or an
        // entry naming a row the table does not have: the index is a well
        // formed tree holding well formed entries on pages nothing else owns.
        // That state was reachable when `UPDATE` skipped a secondary `UNIQUE`
        // index's own check, and `PRAGMA integrity_check` called it healthy -
        // which is what this detector exists for: the write path is where such
        // a state is *created*, and there is more than one way in - an import,
        // a crash recovery, a future write path, a bug like that one.
        //
        // It is the last of the three because it is the one `quick_check`
        // leaves out, and it is left out because it is the expensive one: two
        // walks and a merge per index, where the two above read each page once.
        self.check_indexes_agree()
    }

    /// Writes one entry straight into an index tree, past the write path.
    ///
    /// **A repair and diagnosis hook, and the only way to test the integrity
    /// checker.** Now that `UPDATE` enforces every `UNIQUE` index, no SQL
    /// statement can leave an index holding two entries under one `UNIQUE` key,
    /// or an entry naming a row the table does not have - which is also why a
    /// checker for those states cannot be exercised through SQL. A detector
    /// that has never been shown the damage it looks for is a detector nobody
    /// has tested.
    ///
    /// It maintains nothing and checks nothing: no uniqueness, no table row, no
    /// other index. That is deliberate and is the whole of its use. It goes
    /// through `write`, so the change is logged, committed and recoverable like
    /// any other - the damage is a real state of a real database rather than an
    /// artefact of the test harness.
    ///
    /// @param index - the index's name, as declared
    /// @param entry - the entry: the key columns, then whatever identifies the
    ///   row
    /// @param adding - true to write it, false to remove it
    pub fn write_index_entry_unchecked(
        &mut self,
        index: &str,
        entry: &[OwnedDatum],
        adding: bool,
    ) -> DbResult<()> {
        let folded = index.to_ascii_lowercase().into_bytes();
        let root = self
            .schema
            .tables
            .iter()
            .flat_map(|table| table.indexes.iter())
            .find(|held| held.folded == folded)
            .map(|held| held.root)
            .ok_or_else(|| refusal(format!("no such index: {index}")))?;
        let owned: Vec<OwnedDatum> = entry.to_vec();
        self.write(&Params::new(), Vec::new(), move |target, _| {
            let (database, trees, log) = target.parts_for(root)?;
            let tree = trees
                .get_mut(root)
                .ok_or_else(|| refusal("the index has no tree"))?;
            let borrowed: Vec<Datum<'_>> = owned.iter().map(OwnedDatum::borrow).collect();
            if adding {
                tree.put(database, log, &borrowed)?;
            } else {
                tree.delete(database, log, &borrowed)?;
            }
            Ok(Changes::default())
        })?;
        Ok(())
    }

    /// Points one table's catalog row at another table's tree, past the write
    /// path.
    ///
    /// **The damage `check_page_ownership` looks for, and there is no longer a
    /// statement that produces it.** It is task-2043's own state: a catalog
    /// row naming a page that is not the one holding that table's rows. That
    /// defect is fixed - a dropped tree's pages are freed at commit now - so
    /// the only way to show the detector the damage it looks for is to write
    /// the row.
    ///
    /// Both trees stay well formed, which is the point: `PagedTree::check`
    /// passes over each of them and `check_indexes_agree` has nothing to
    /// compare, because neither is an index of the other. What is wrong is
    /// only visible by asking who holds a page.
    ///
    /// It goes through `rewrite` and `seal`, so the row is logged, committed
    /// and recoverable like any other catalog change - the damage is a real
    /// state of a real file rather than an artefact of the test harness. The
    /// tree identifier is left alone, because what moved is the row's idea of
    /// where its tree is and not which tree the log's records belong to.
    ///
    /// The caller reopens the file afterwards: the handles this connection
    /// already holds were attached before the row changed, and it is the read
    /// back from the catalog that produces two handles over one tree.
    ///
    /// @param table - the table whose catalog row is moved
    /// @param onto - the table whose tree it is made to name
    pub fn point_table_at_unchecked(&mut self, table: &str, onto: &str) -> DbResult<()> {
        let wanted = table.as_bytes().to_ascii_lowercase();
        let target = onto.as_bytes().to_ascii_lowercase();
        let find = |name: &[u8]| {
            self.schema
                .entries
                .iter()
                .find(|held| held.entry.name.to_ascii_lowercase() == name)
                .map(|held| (held.rowid, held.entry.clone()))
        };
        let (_, onto_entry) =
            find(&target).ok_or_else(|| refusal(format!("no such table: {onto}")))?;
        let (rowid, mut entry) =
            find(&wanted).ok_or_else(|| refusal(format!("no such table: {table}")))?;
        entry.root = onto_entry.root;
        entry.stats = onto_entry.stats;
        self.rewrite(rowid, entry)?;
        self.seal()
    }

    /// Takes one page out of the free map and gives it to nothing, past the
    /// write path.
    ///
    /// **The damage `report_leaked_pages` looks for**: a page the free map
    /// calls allocated that no tree reaches, which is what a write path that
    /// let go of a page without saying so leaves behind. Neither pragma reports
    /// it - `crate::engine::pages` says why - so this hook and its test are the
    /// only thing that runs that arm. Returns the page it stranded, so a test
    /// can name it in its assertion rather than guess.
    ///
    /// The engine produces the same state from a rolled-back `CREATE` and from
    /// a `DROP TABLE` of a table holding out-of-line values, and the tests use
    /// those too. This hook is what shows the arm a leak that is neither of
    /// them, so closing those two does not leave it untested.
    ///
    /// The `AllocPage` record is the whole of the change, which is what makes
    /// the state survive a crash: replaying it claims the page in the map, and
    /// there is no second record putting anything on it.
    ///
    /// Always `main`, because a damage hook has no reason to reach an attached
    /// file and every caller is a test over one database.
    pub fn strand_a_page_unchecked(&mut self) -> DbResult<u64> {
        let at = crate::engine::open::SCHEMA_VIEW_ROOT;
        let stranded = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let reported = std::rc::Rc::clone(&stranded);
        self.write(&Params::new(), Vec::new(), move |target, _| {
            let (database, _, log) = target.parts_for(at)?;
            let page = database.allocate(1)?;
            log.log(inillucent_wal::record::Body::AllocPage { page: page.0 })?;
            reported.set(page.0);
            Ok(Changes::default())
        })?;
        Ok(stranded.get())
    }

    /// Tells the free map that one table's root page is free, past the write
    /// path.
    ///
    /// **The second state `check_page_ownership` reports, and the one that
    /// becomes the first.** A live page the map has handed back is not damage
    /// anybody can see yet - every tree still reads correctly - and it stops
    /// being invisible at the next allocation, which gives the page to a second
    /// owner and loses whatever was on it. That is task-2043's own sequence,
    /// and this is the state it passes through.
    ///
    /// The `FreePage` record is the whole of the change, so a replay reaches
    /// the same state rather than a healthy one. Returns the page, so a test
    /// can name it.
    ///
    /// @param table - the table whose root page is given back
    pub fn free_root_page_unchecked(&mut self, table: &str) -> DbResult<u64> {
        let wanted = table.as_bytes().to_ascii_lowercase();
        let page = self
            .schema
            .entries
            .iter()
            .find(|held| held.entry.name.to_ascii_lowercase() == wanted)
            .map(|held| held.entry.root)
            .ok_or_else(|| refusal(format!("no such table: {table}")))?;
        let at = crate::engine::open::SCHEMA_VIEW_ROOT;
        self.write(&Params::new(), Vec::new(), move |target, _| {
            let (database, _, log) = target.parts_for(at)?;
            log.log(inillucent_wal::record::Body::FreePage { page: page.0 })?;
            database.release(page, 1)?;
            Ok(Changes::default())
        })?;
        Ok(page.0)
    }

    /// Reports the first disagreement between an index and the table it is on.
    ///
    /// **Four kinds of disagreement, in SQLite's own wording**, because an
    /// application matching on `integrity_check`'s answer is matching on that
    /// text:
    ///
    /// - `non-unique entry in index <name>` - two entries under one key in a
    ///   `UNIQUE` index. The entries are in key order, so this is a comparison
    ///   against the previous entry and costs one extra comparison per entry
    ///   rather than a second pass. A prefix containing a NULL is skipped,
    ///   because SQL's rule is that every NULL is distinct - the same rule
    ///   `distinct_prefix` applies on the write path.
    /// - `row <rowid> missing from index <name>` - a table row whose entry is
    ///   not there, which is also what an entry whose *key* does not match its
    ///   row looks like from here: the recomputed key is not found.
    /// - `wrong # of entries in index <name>` - which is what an entry naming a
    ///   row the table does not hold shows up as, once every row that is there
    ///   has been found.
    ///
    /// **A partial index and an index on an expression are checked for
    /// uniqueness only.** Deciding which rows *should* have an entry means
    /// evaluating the predicate, and deciding what an entry's key should be
    /// means evaluating the key expression; both need a binder, which the
    /// checker does not have. Counting them as though every row had an entry
    /// would report a healthy partial index as damaged, which is worse than
    /// not looking.
    fn check_indexes_agree(&self) -> DbResult<()> {
        for table in &self.schema.tables {
            let Some(layout) = self.schema.layouts.get(&table.root) else {
                continue;
            };
            let Some(table_tree) = self.schema.trees.get(&table.root) else {
                continue;
            };
            let Some(file) = self.schema_file(self.session_state.schema_of(table.root)) else {
                continue;
            };
            for index in &table.indexes {
                if index.root == 0 || index.root == table.root {
                    continue;
                }
                let Some(index_tree) = self.schema.trees.get(&index.root) else {
                    continue;
                };
                let Some(index_file) = self.schema_file(self.session_state.schema_of(index.root))
                else {
                    continue;
                };
                // **Two sequential walks and a merge, with no probe between
                // them.** The obvious algorithm is SQLite's - walk the table
                // and seek the index for each row - and this engine cannot
                // afford it: a descent swizzles the pointer it followed, so
                // probing a hundred thousand distinct leaves pins the pool, and
                // `an_index_build_bigger_than_the_pool_completes` reported
                // exactly that on its 64 frames. Both trees are walked left to
                // right instead, and the table's implied entries are put into
                // the index's own key order first - by `in_key_order`, which is
                // the tree's own encoding under the tree's own collations, so
                // there is no second opinion about ordering to drift from the
                // first.
                let computed = index.partial_sql.is_some()
                    || index.columns.iter().any(|key| key.expr_sql.is_some());
                let (specs, _) = index_shape(table, index, index.root);
                let width = index.columns.len();
                let mut implied: Vec<Vec<OwnedDatum>> = Vec::new();
                if !computed {
                    let mut page = table_tree.first_leaf();
                    while !page.is_none() {
                        let mut next = inillucent_pool::PageId::NONE;
                        table_tree.visit_from(file.pool(), page, &mut |leaf| {
                            next = leaf.right_sibling();
                            for row in leaf.live()? {
                                let owned: Vec<OwnedDatum> =
                                    row.iter().map(OwnedDatum::from_datum).collect();
                                implied.push(plain_index_entry(index, layout, &owned));
                            }
                            Ok(false)
                        })?;
                        page = next;
                    }
                    implied = in_key_order(implied, &specs, specs.len());
                }
                let rows = implied.len() as u64;
                let mut wanted = implied.into_iter();
                let mut missing: Option<Vec<OwnedDatum>> = None;
                let mut entries = 0u64;
                let mut previous: Option<Vec<OwnedDatum>> = None;
                index_tree.visit_leaves(index_file.pool(), &mut |leaf| {
                    for entry in leaf.live()? {
                        entries = entries.saturating_add(1);
                        let held: Vec<OwnedDatum> =
                            entry.iter().map(OwnedDatum::from_datum).collect();
                        if index.unique {
                            let key = held.get(..width).unwrap_or_default();
                            // Every NULL is distinct, so a key holding one is
                            // not a duplicate of anything - the same rule
                            // `distinct_prefix` applies on the write path.
                            if key.iter().any(|value| matches!(value, OwnedDatum::Null)) {
                                previous = None;
                            } else {
                                if previous.as_deref() == Some(key) {
                                    return Err(corrupt_index(format!(
                                        "non-unique entry in index {}",
                                        String::from_utf8_lossy(&index.name)
                                    )));
                                }
                                previous = Some(key.to_vec());
                            }
                        }
                        // **A partial index and an index on an expression are
                        // checked for uniqueness only.** Deciding which rows
                        // should have an entry means evaluating the predicate,
                        // and deciding what a key should be means evaluating
                        // the key expression; both need a binder the checker
                        // does not have, and counting them as though every row
                        // had an entry would report a healthy partial index as
                        // damaged.
                        if computed || missing.is_some() {
                            continue;
                        }
                        match wanted.next() {
                            Some(want) if want == held => {}
                            Some(want) => missing = Some(want),
                            // More entries than the table implies. The count
                            // below is what names that.
                            None => {}
                        }
                    }
                    Ok(true)
                })?;
                if computed {
                    continue;
                }
                // The row is named before the count, because a table row whose
                // entry was taken away is both - and SQLite names the row.
                if let Some(want) = missing.or_else(|| wanted.next()) {
                    return Err(corrupt_index(format!(
                        "row {} missing from index {}",
                        entry_identity_text(&want, width),
                        String::from_utf8_lossy(&index.name)
                    )));
                }
                if entries != rows {
                    return Err(corrupt_index(format!(
                        "wrong # of entries in index {}",
                        String::from_utf8_lossy(&index.name)
                    )));
                }
            }
        }
        Ok(())
    }
}
