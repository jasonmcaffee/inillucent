//! Settling the foreign keys a statement left outstanding.
//!
//! Invariant: **the question is asked, not counted.** SQLite keeps a running
//! count of outstanding violations and moves it as rows appear and disappear; a
//! count that drifts by one reports a violation that is not there, or misses
//! one that is, and neither is visible until a commit fails for a reason nobody
//! can reproduce. Every check here runs the violation query again.

use crate::*;

impl ImportedDatabase {
    /// Runs one query the engine wrote for itself, and returns its rows.
    ///
    /// **The engine asking itself a question.** A foreign-key check *is* a
    /// query, and running it through the ordinary compile-and-execute path is
    /// what makes it use the ordinary indexes - and what stops there being a
    /// second, hand-written scan that has to be kept in step with the first.
    ///
    /// @param sql - the statement the engine generated
    pub(crate) fn query_internally(&mut self, sql: &str) -> DbResult<Vec<Vec<OwnedDatum>>> {
        Ok(self.execute_any(sql, &Params::default())?.rows)
    }

    /// Applies the actions of every key that can lead back to its own table.
    ///
    /// **A cyclic action cannot be inlined**, because the body would have to
    /// appear once per level the data happens to be deep and that is not known
    /// when the statement is compiled. The binder therefore stops a cascade at
    /// the level it can see - the rows that pointed directly at the row that
    /// went - and this takes what that leaves: every row whose key now has no
    /// parent, repeated until nothing changes.
    ///
    /// It terminates because every pass either changes a row or stops, and a
    /// pass only ever removes a row or clears a key.
    ///
    /// It runs after the statement rather than inside it, and only on a schema
    /// that has such a key, so a schema without one pays a flag test.
    pub(crate) fn settle_foreign_keys(&mut self) -> DbResult<()> {
        if !self.pragmas.foreign_keys() || !self.schema.has_cyclic_foreign_keys() {
            return Ok(());
        }
        let mut statements = Vec::new();
        for child in &self.schema.tables {
            if child.kind != inillucent_sql::catalog_view::TableKind::Table {
                continue;
            }
            for key in &child.foreign_keys {
                if !key.cyclic {
                    continue;
                }
                let Some(parent) = self
                    .schema
                    .tables
                    .iter()
                    .find(|candidate| candidate.folded == key.parent_folded)
                else {
                    continue;
                };
                if let Some(sql) =
                    inillucent_sql::foreign_key::sweep_statement(child, parent, key, b"main")
                {
                    statements.push(sql);
                }
            }
        }
        if statements.is_empty() {
            return Ok(());
        }
        for _ in 0..MAX_SWEEP_PASSES {
            // The running total is what says whether a pass did anything: it
            // moves as each statement finishes, so comparing it across a pass
            // asks exactly "did any of these change a row" without the sweep
            // having to count them itself.
            let before = self.counters.changed_ever.get();
            for sql in &statements {
                self.execute_any(sql, &Params::default())?;
            }
            if self.counters.changed_ever.get() == before {
                return Ok(());
            }
        }
        Err(refusal(
            "a foreign key's action did not settle; the schema may have a cycle that cannot resolve",
        ))
    }
}

impl ImportedDatabase {
    /// Checks every deferred foreign key, and reports the first violation.
    ///
    /// **A full check rather than a running count.** SQLite keeps a counter of
    /// outstanding violations and moves it as rows appear and disappear; a
    /// counter that drifts by one reports a violation that is not there, or
    /// misses one that is, and neither is visible until a commit fails for a
    /// reason nobody can reproduce. Asking the question directly costs a query
    /// per deferred key per commit and cannot drift.
    pub(crate) fn check_deferred_foreign_keys(&mut self) -> DbResult<()> {
        if !self.pragmas.foreign_keys() || !self.has_deferred_foreign_keys() {
            return Ok(());
        }
        for query in self.schema.violation_queries(None)? {
            if self.query_internally(&query.sql)?.is_empty() {
                continue;
            }
            return Err(DbError::new(inillucent_base::ExtendedCode(
                inillucent_sql::dml::codes::FOREIGN_KEY,
            ))
            .with_message("FOREIGN KEY constraint failed")
            .with_detail(format!(
                "deferred key {} of {}",
                query.key,
                String::from_utf8_lossy(&query.child)
            )));
        }
        Ok(())
    }

    /// Returns the keys a write will change.
    ///
    /// A rowid-equality `WHERE` is answered from the plan itself - see
    /// `physical::rowid_seek_key` - without asking `query`'s slot at all; that
    /// shape is already a single lookup, so Stage 3's saving is on the range
    /// and index shapes instead. Everything else runs through
    /// [`ImportedDatabase::run_cached_query`], the slot-try/build-once/reuse
    /// mechanism `execute_select_cached` gives a `SELECT`.
    ///
    /// **Runs under `&self` and returns before `self.write` takes `&mut
    /// self`.** `query.slot.try_borrow_mut()` is taken and dropped inside
    /// `run_cached_query`, entirely before this returns, so nothing about it
    /// is still borrowed when the write that follows needs `&mut self`.
    ///
    /// @param query - the keys query and its compiled-chain cache
    /// @param params - the bound parameters
    pub(crate) fn keys_of(
        &self,
        query: &CachedQuery,
        params: &Params,
    ) -> DbResult<Vec<Vec<OwnedDatum>>> {
        if let Some(key) = physical::rowid_seek_key(&query.plan, params)? {
            return Ok(vec![vec![key]]);
        }
        self.run_cached_query(&query.plan, &query.prepared, &query.slot, params)
    }
}

/// How many times the cyclic sweep repeats before it gives up.
///
/// One pass per level of the deepest chain in the data. A tree deeper than this
/// is a tree with a million levels, which is a different problem.
pub(crate) const MAX_SWEEP_PASSES: usize = 1_000_000;

/// One foreign key's violation query, and what it is about.
pub(crate) struct ViolationQuery {
    /// The `SELECT` that finds the rows with no parent.
    pub(crate) sql: String,
    /// The child table's name, which the pragma reports.
    pub(crate) child: Vec<u8>,
    /// The parent table's name, which the pragma reports.
    pub(crate) parent: Vec<u8>,
    /// The key's position in its table, which the pragma reports as `fkid`.
    pub(crate) key: u16,
}
