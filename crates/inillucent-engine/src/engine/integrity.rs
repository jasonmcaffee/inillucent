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

impl crate::ImportedDatabase {
    /// Checks every tree in every attached database, and their agreement.
    ///
    /// The campaign tests run this after every statement. A tree that has
    /// drifted structurally still answers a scan correctly for a long time,
    /// which is precisely why the check has to be a check rather than a query.
    pub fn check_trees(&self) -> DbResult<()> {
        for (root, tree) in &self.schema.trees {
            let pool = self
                .schema_file(self.session_state.schema_of(*root))
                .ok_or_else(|| refusal("a tree names a database that is not attached"))?
                .pool();
            tree.check(pool)?;
        }
        // **And then whether the trees agree with each other.** Every check
        // above is about one tree in isolation - its key order, its sibling
        // chain, its separators - and every one of them passes over a database
        // where an index holds two entries under one `UNIQUE` key, or an entry
        // naming a row the table does not have. That state was reachable when
        // `UPDATE` skipped a secondary `UNIQUE` index's own check, and
        // `PRAGMA integrity_check` called it healthy - which is what this
        // detector exists for: the write path is where such a state is
        // *created*, and there is more than one way in - an import, a crash
        // recovery, a future write path, a bug like that one.
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
