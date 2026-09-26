//! `ANALYZE`: measuring the trees, and writing what it measured.
//!
//! Invariant: the table this writes is **SQLite's own `sqlite_stat1`, in
//! SQLite's own format** - `(tbl, idx, stat)`, with `stat` a space-separated
//! list of the table's row count followed by the average number of rows sharing
//! each leading prefix of the index's key. That is not a nicety. The phase's
//! acceptance is that a DDL statement's effect on `sqlite_schema` is
//! digest-equal to SQLite's, and `ANALYZE` *is* a DDL statement: SQLite creates
//! `sqlite_stat1` on its first run and the row for it appears in the catalog. An
//! engine that stored its statistics somewhere else would differ from SQLite in
//! exactly the way the acceptance is looking for.
//!
//! The measurement is `inillucent_catalog::analyze::measure`'s, ported from a
//! SQLite b-tree cursor to a `PagedTree` walk. The arithmetic is the same
//! arithmetic - a run of equal prefixes is counted, and the average is rounded
//! **up**, because rounding down claims a prefix is more selective than it is
//! and that is the direction that picks a bad plan.

use std::collections::HashMap;

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_catalog::analyze::STAT1_SQL;
use inillucent_pool::Pool;
use inillucent_sql::catalog_view::{IndexInfo, TableInfo};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::PagedTree;

use super::{ImportedDatabase, Outcome, WalLog};

/// The name statistics live under, which is SQLite's.
const STAT1: &[u8] = b"sqlite_stat1";

impl ImportedDatabase {
    /// Runs an `ANALYZE` directive.
    ///
    /// Its own function so `run_directive`'s arm stays one line.
    ///
    /// @param directive - the bound `ANALYZE`
    pub(super) fn run_analyze(
        &mut self,
        directive: inillucent_sql::directive::Directive,
    ) -> DbResult<Outcome> {
        let inillucent_sql::directive::Directive::Analyze {
            table,
            every_schema,
            ..
        } = directive
        else {
            return Err(refusal("not an ANALYZE"));
        };
        self.analyze(table.as_deref(), every_schema)
    }

    /// Measures one or every database and writes `sqlite_stat1`.
    ///
    /// **Each database's statistics go in its own `sqlite_stat1`.** A bare
    /// `ANALYZE` measures every database but `temp`, and `ANALYZE aux` measures
    /// `aux`, which is SQLite's `sqlite3Analyze`. This used to measure the
    /// tables of every database and write every row into `main`'s table, and
    /// `ANALYZE aux` was "no such table".
    ///
    /// @param table - the one table to measure, or nothing for all of them
    /// @param every_schema - whether the statement was a bare `ANALYZE`
    pub(super) fn analyze(
        &mut self,
        table: Option<&[u8]>,
        every_schema: bool,
    ) -> DbResult<Outcome> {
        let previous = self.schema.ddl_schema;
        let schemas: Vec<usize> = if every_schema {
            self.schema_numbers()
                .into_iter()
                .filter(|at| *at != crate::TEMP)
                .collect()
        } else {
            vec![previous]
        };
        for at in schemas {
            self.schema.ddl_schema = at;
            let measured = self.analyze_schema(at, table);
            self.schema.ddl_schema = previous;
            measured?;
            self.writing
                .set_touched(self.writing.touched() | crate::schema_bit(at));
        }
        // **The rows are on disk and the planner has not read them yet.**
        // `refresh_catalog` is the only path to `republish_statistics`, so
        // without this the connection that ran `ANALYZE` keeps the snapshot it
        // took before the statistics existed: `IndexInfo::prefix_rows` stays
        // `Some([])` and every table's row count stays the planner's guess of
        // `DEFAULT_ROWS`, 1,048,576, until the database is reopened. Every other
        // schema writing directive - `ddl.rs`, `attach.rs`, `vtab.rs`,
        // `marks.rs` - ends the same way, and the suite missed this one because
        // every case in `analyze_reopen.rs` reopens before it reads anything
        // back (task-1946, H1).
        self.refresh_catalog();
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Measures one database's tables into its `sqlite_stat1`.
    ///
    /// **The rows SQLite's `analyzeOneTable` writes, and no others.** An index
    /// on an empty table gets no row, and a table gets a row of its own only
    /// when it has rows and has no index that is not partial. A `WITHOUT
    /// ROWID` table's primary key is written under the table's own name. This
    /// wrote `t | i | 0 1` for an empty table and `sqlite_autoindex_w_1` for
    /// the key of a `WITHOUT ROWID` table, where SQLite writes nothing and
    /// `w | w`.
    ///
    /// @param at - the database to measure
    /// @param table - the one table to measure, or nothing for all of them
    fn analyze_schema(&mut self, at: usize, table: Option<&[u8]>) -> DbResult<()> {
        let wanted = table.map(|name| name.to_ascii_lowercase());
        let in_schema = |held: &&TableInfo| held.database == at;
        // **A named index is measured alone.** SQLite looks the name up as an
        // index first, and `ANALYZE ix` then writes the row for `ix` and
        // nothing else: not the other indexes, and not the table's own row.
        // Measuring the whole table wrote a `t | NULL` row beside a partial
        // index's that SQLite does not write.
        let only_index: Option<Vec<u8>> = wanted.as_ref().and_then(|name| {
            self.schema
                .tables
                .iter()
                .filter(in_schema)
                .flat_map(|held| held.indexes.iter())
                .find(|index| index.folded == *name)
                .map(|index| index.folded.clone())
        });
        let wanted = match &only_index {
            Some(index) => self
                .schema
                .tables
                .iter()
                .filter(in_schema)
                .find(|held| held.indexes.iter().any(|held| held.folded == *index))
                .map(|held| held.folded.clone()),
            None => wanted,
        };
        self.ensure_stat1(at)?;
        // **Only the things that have rows to count.** A view has none, a
        // virtual table's are the module's, and a reserved name is bookkeeping
        // this engine wrote itself. Measuring them wrote rows the reference
        // never writes, which showed up as `.dump` emitting
        // `INSERT INTO sqlite_stat1 VALUES('vv',NULL,'0')` for a *view* - and
        // then, when that dump was replayed, as statistics claiming the tables
        // were empty.
        let subjects: Vec<TableInfo> = self
            .schema
            .tables
            .iter()
            .filter(in_schema)
            .filter(|held| !held.folded.starts_with(b"sqlite_"))
            .filter(|held| held.kind == inillucent_sql::catalog_view::TableKind::Table)
            .filter(|held| held.module.is_none())
            .filter(|held| wanted.as_ref().is_none_or(|name| held.folded == *name))
            .cloned()
            .collect();
        let mut rows: Vec<(Vec<u8>, Option<Vec<u8>>, Vec<u8>)> = Vec::new();
        for subject in &subjects {
            let count = self.live_rows(subject.root)?;
            let mut needs_table_count = true;
            for index in &subject.indexes {
                if only_index
                    .as_ref()
                    .is_some_and(|only| *only != index.folded)
                {
                    continue;
                }
                let partial = index.partial_sql.is_some();
                if !partial {
                    needs_table_count = false;
                }
                // A partial index may get a row that says it is empty; an
                // empty table gets none for any other index.
                if count == 0 && !partial {
                    continue;
                }
                let stat = self.measure_index(index, count)?;
                let name = if subject.without_rowid
                    && index.origin == inillucent_sql::catalog_view::IndexOrigin::PrimaryKey
                {
                    subject.name.clone()
                } else {
                    index.name.clone()
                };
                rows.push((subject.name.clone(), Some(name), stat));
            }
            if needs_table_count && count > 0 && only_index.is_none() {
                rows.push((subject.name.clone(), None, count.to_string().into_bytes()));
            }
        }
        // Every row for the tables being measured is replaced, and the rows for
        // tables that were not measured are left alone - which is what
        // `ANALYZE one_table` means.
        let names: Vec<Vec<u8>> = subjects.iter().map(|held| held.name.clone()).collect();
        self.clear_stat1(at, &names, only_index.as_deref())?;
        self.write_stat1(at, &rows)?;
        Ok(())
    }

    /// Creates a database's `sqlite_stat1` if it has not got one.
    ///
    /// SQLite creates it on the first `ANALYZE` and leaves it in the schema
    /// afterwards, so the catalog row is part of what `ANALYZE` does rather than
    /// a side effect of it. `define_table` writes into `ddl_schema`, which the
    /// caller has set to `at`.
    ///
    /// @param at - the database
    fn ensure_stat1(&mut self, at: usize) -> DbResult<()> {
        if self.stat1_root(at).is_some() {
            return Ok(());
        }
        self.define_table(STAT1, STAT1_SQL.as_bytes().to_vec())?;
        self.refresh_catalog();
        Ok(())
    }

    /// Returns the tree handle of one database's `sqlite_stat1`.
    ///
    /// @param at - the database
    fn stat1_root(&self, at: usize) -> Option<u32> {
        let folded = STAT1.to_ascii_lowercase();
        self.schema
            .tables
            .iter()
            .find(|held| held.folded == folded && held.database == at)
            .map(|held| held.root)
    }

    /// Returns how many live rows a tree holds.
    ///
    /// The packed row count in the handle is the count as the tree was *built*;
    /// a delta area and a tombstone both move it, and `ANALYZE` is measuring the
    /// table as it is now.
    ///
    /// @param root - the tree's identifier
    fn live_rows(&self, root: u32) -> DbResult<i64> {
        let Some(tree) = self.schema.trees.get(&root) else {
            return Ok(0);
        };
        let pool = self.pool_of(root)?;
        let mut count = 0i64;
        tree.visit_leaves(pool, &mut |leaf| {
            count = count.saturating_add(leaf.live_rows()? as i64);
            Ok(true)
        })?;
        Ok(count)
    }

    /// Renders one index's `stat` column.
    ///
    /// One pass in key order. Entries sharing a prefix are adjacent by
    /// construction, so a run length is a count and nothing has to be
    /// remembered beyond the previous entry.
    ///
    /// @param index - the index to measure
    /// @param rows - the table's row count
    fn measure_index(&self, index: &IndexInfo, rows: i64) -> DbResult<Vec<u8>> {
        let width = index.columns.len();
        let Some(tree) = self.schema.trees.get(&index.root) else {
            return Ok(rows.to_string().into_bytes());
        };
        // `groups[n]` counts how many distinct values the first `n + 1` key
        // columns take, which is what the average is the reciprocal of.
        let pool = self.pool_of(index.root)?;
        let mut groups = vec![0i64; width];
        let mut previous: Option<Vec<OwnedDatum>> = None;
        let mut entries = 0i64;
        // **The key columns only.** `live()` decodes every column of every live row, and refuses one
        // held in a blob extent until the tree has read the extents in. This walks the table's own
        // tree when the index is its primary key, so on a table carrying message bodies it failed
        // outright with "page is not a blob extent" - which meant a real database had no statistics
        // at all, and a planner with no selectivity falls back to scanning. `visit_live` performs the
        // same merge of the sorted region and the delta area while decoding only what is asked for,
        // and a key column is never out of line.
        let measured: Vec<usize> = (0..width).collect();
        tree.visit_leaves(pool, &mut |leaf| {
            leaf.visit_live(&measured, &mut |row| {
                let current: Vec<OwnedDatum> = row.iter().map(OwnedDatum::from_datum).collect();
                entries = entries.saturating_add(1);
                match &previous {
                    None => {
                        for group in groups.iter_mut() {
                            *group = 1;
                        }
                    }
                    Some(before) => {
                        // The first column at which the entries differ opens a
                        // new group for that prefix and every longer one.
                        let mut differ = width;
                        for column in 0..width {
                            if before.get(column) != current.get(column) {
                                differ = column;
                                break;
                            }
                        }
                        for (column, group) in groups.iter_mut().enumerate() {
                            if column >= differ {
                                *group = group.saturating_add(1);
                            }
                        }
                    }
                }
                previous = Some(current);
                Ok(())
            })?;
            Ok(true)
        })?;
        // **A partial index holds the rows its predicate accepted, and no
        // more.** The table's row count is the right total for
        // an ordinary index, because every row has an entry; for a partial one
        // it is the number the index does *not* hold, and writing it says the
        // index returns the whole table. A planner reading that never chooses
        // the index - not even for the predicate the index was declared with,
        // character for character - and falls back to a scan.
        //
        // Measured on a fixture of 6,000 documents where 120 have a NULL
        // `indexed_at` and `document_pending_idx` is declared
        // `WHERE indexed_at IS NULL`: the row said `6000 6000`, the plan said
        // `SCAN document`, and the real corpus's equivalent query was 6,000
        // times slower than PostgreSQL's. The walk already counted the entries;
        // this stops throwing that number away.
        let total = if index.partial_sql.is_some() {
            entries
        } else {
            rows.max(entries)
        };
        let mut out = total.to_string();
        // SQLite's `statGet`: the average is rounded up, except that one
        // between 1.0 and 1.1 is written as 1, and an index with no entries
        // averages 0. Writing at least 1 for an empty partial index said
        // `0 1` where SQLite says `0 0`.
        for group in &groups {
            let distinct = (*group).max(1);
            let mut average = total.saturating_add(distinct.saturating_sub(1)) / distinct;
            if average == 2 && total.saturating_mul(10) <= distinct.saturating_mul(11) {
                average = 1;
            }
            out.push(' ');
            out.push_str(&average.to_string());
        }
        Ok(out.into_bytes())
    }

    /// Removes the `sqlite_stat1` rows for the tables about to be re-measured.
    ///
    /// @param at - the database whose `sqlite_stat1` it is
    /// @param tables - the table names whose rows go
    /// @param only_index - the one index whose row goes, for `ANALYZE ix`
    fn clear_stat1(
        &mut self,
        at: usize,
        tables: &[Vec<u8>],
        only_index: Option<&[u8]>,
    ) -> DbResult<()> {
        let Some(root) = self.stat1_root(at) else {
            return Ok(());
        };
        let doomed: Vec<i64> = {
            let pool = self.pool_of(root)?;
            let tree = self
                .schema
                .trees
                .get(&root)
                .ok_or_else(|| refusal("sqlite_stat1 has no tree"))?;
            let mut keys = Vec::new();
            tree.visit_leaves(pool, &mut |leaf| {
                for row in leaf.live()? {
                    let Some(Datum::Int(rowid)) = row.first().copied() else {
                        continue;
                    };
                    let named = matches!(row.get(1), Some(Datum::Text(name))
                        if tables.iter().any(|wanted| wanted == name));
                    let indexed = only_index.is_none_or(|only| {
                        matches!(row.get(2), Some(Datum::Text(name))
                            if name.eq_ignore_ascii_case(only))
                    });
                    let named = named && indexed;
                    if named {
                        keys.push(rowid);
                    }
                }
                Ok(true)
            })?;
            keys
        };
        let txn = self.current_txn();
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
        let tree = self
            .schema
            .trees
            .get_mut(&root)
            .ok_or_else(|| refusal("sqlite_stat1 has no tree"))?;
        // The file the statistics table is in, which is `aux`'s own for
        // `ANALYZE aux` rather than `main`'s.
        let session = self.session_state.session.get();
        let database = crate::file_of(
            &mut self.storage.database,
            &mut self.session_state.attached,
            &mut self.session_state.temps,
            session,
            at,
        )?;
        for rowid in doomed {
            tree.delete(database, &mut log, &[Datum::Int(rowid)])?;
        }
        Ok(())
    }

    /// Writes the measured rows into `sqlite_stat1`.
    ///
    /// @param at - the database whose `sqlite_stat1` it is
    /// @param rows - table, index and rendered `stat`
    fn write_stat1(
        &mut self,
        at: usize,
        rows: &[(Vec<u8>, Option<Vec<u8>>, Vec<u8>)],
    ) -> DbResult<()> {
        let Some(root) = self.stat1_root(at) else {
            return Ok(());
        };
        let mut next = {
            let pool = self.pool_of(root)?;
            let tree = self
                .schema
                .trees
                .get(&root)
                .ok_or_else(|| refusal("sqlite_stat1 has no tree"))?;
            let mut highest = 0i64;
            tree.visit_leaves(pool, &mut |leaf| {
                for row in leaf.live()? {
                    if let Some(Datum::Int(rowid)) = row.first().copied() {
                        highest = highest.max(rowid);
                    }
                }
                Ok(true)
            })?;
            highest.saturating_add(1)
        };
        let txn = self.current_txn();
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
        let tree = self
            .schema
            .trees
            .get_mut(&root)
            .ok_or_else(|| refusal("sqlite_stat1 has no tree"))?;
        // The file the statistics table is in, which is `aux`'s own for
        // `ANALYZE aux` rather than `main`'s.
        let session = self.session_state.session.get();
        let database = crate::file_of(
            &mut self.storage.database,
            &mut self.session_state.attached,
            &mut self.session_state.temps,
            session,
            at,
        )?;
        for (table, index, stat) in rows {
            let owned = [
                OwnedDatum::Int(next),
                OwnedDatum::Text(table.clone()),
                match index {
                    Some(name) => OwnedDatum::Text(name.clone()),
                    None => OwnedDatum::Null,
                },
                OwnedDatum::Text(stat.clone()),
            ];
            let row: Vec<Datum<'_>> = owned.iter().map(OwnedDatum::borrow).collect();
            tree.insert(database, &mut log, &row)?;
            next = next.saturating_add(1);
        }
        Ok(())
    }

    /// Returns the statistics the planner would read, for a test.
    ///
    /// @param table - the table to report on
    pub fn statistics(&self, table: &[u8]) -> Vec<(Option<Vec<u8>>, Vec<u8>)> {
        let folded = STAT1.to_ascii_lowercase();
        let Some(root) = self
            .schema
            .tables
            .iter()
            .find(|held| held.folded == folded)
            .map(|held| held.root)
        else {
            return Vec::new();
        };
        let Some(tree) = self.schema.trees.get(&root) else {
            return Vec::new();
        };
        let Ok(pool) = self.pool_of(root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let _ = tree.visit_leaves(pool, &mut |leaf| {
            for row in leaf.live()? {
                let matches = matches!(row.get(1), Some(Datum::Text(name)) if *name == table);
                if !matches {
                    continue;
                }
                let index = match row.get(2) {
                    Some(Datum::Text(name)) => Some(name.to_vec()),
                    _ => None,
                };
                let stat = match row.get(3) {
                    Some(Datum::Text(text)) => text.to_vec(),
                    _ => Vec::new(),
                };
                out.push((index, stat));
            }
            Ok(true)
        });
        out
    }

    /// Reads `sqlite_stat1` onto the tables the planner costs with.
    ///
    /// The counterpart of [`ImportedDatabase::analyze`]: that writes the table,
    /// this is what makes the planner see it. Called from `refresh_catalog`, so
    /// every schema change and every `ANALYZE` republishes the measurements.
    ///
    /// **Each database's statistics apply to that database's tables.** A row
    /// in `aux.sqlite_stat1` describes `aux.t`, not a `main.t` of the same
    /// name, and only the first `sqlite_stat1` found used to be read.
    pub(crate) fn republish_statistics(&mut self) {
        let folded = STAT1.to_ascii_lowercase();
        let sources: Vec<(usize, u32)> = self
            .schema
            .tables
            .iter()
            .filter(|held| held.folded == folded)
            .map(|held| (held.database, held.root))
            .collect();
        // Read under an immutable borrow, then patch: the rows come back owned,
        // which is what lets both happen in one method.
        let mut read: Vec<(usize, Vec<(Vec<u8>, Option<Vec<u8>>, Vec<u8>)>)> = Vec::new();
        for (at, root) in sources {
            let rows = match (self.schema.trees.get(&root), self.pool_of(root)) {
                (Some(tree), Ok(pool)) => statistics_rows(pool, tree),
                _ => Vec::new(),
            };
            read.push((at, rows));
        }
        // No statistics table clears whatever a previous one left, so a
        // `DROP TABLE sqlite_stat1` stops changing plans.
        apply_statistics(&mut self.schema.tables, &[]);
        for (at, rows) in read {
            for (table, index, stat) in rows {
                let folded = table.to_ascii_lowercase();
                let Some(held) = self
                    .schema
                    .tables
                    .iter_mut()
                    .find(|held| held.database == at && held.folded == folded)
                else {
                    continue;
                };
                inillucent_catalog::load::apply_statistic(
                    std::slice::from_mut(held),
                    &table,
                    index.as_deref(),
                    &stat,
                );
            }
        }
    }
}

/// Reads `sqlite_stat1` onto the tables it describes.
///
/// **The half of `ANALYZE` that was missing.** This engine wrote the table and
/// never read it: `IndexInfo::prefix_rows` and `TableInfo::analysed_rows` are
/// what the planner costs a join with, and nothing on this path had ever set
/// them - so a file the reference had `ANALYZE`d arrived here with its
/// measurements sitting in a table nobody opened, and the join was planned by
/// the guesses the measurements exist to replace. `inillucent-catalog`'s own
/// loader does this for a SQLite file; this is the same rule over a PAX tree.
///
/// A missing, empty or unreadable statistics table is not an error - statistics
/// are a hint, and a planner that refused to run without them would turn
/// `ANALYZE` into a dependency.
///
/// @param pool - the buffer pool the trees live in
/// @param trees - every tree this schema holds, by handle
/// @param tables - the binder's tables, patched in place
pub(crate) fn attach_statistics(
    pool: &Pool,
    trees: &HashMap<u32, PagedTree>,
    tables: &mut [TableInfo],
) {
    let folded = inillucent_catalog::analyze::STAT1
        .as_bytes()
        .to_ascii_lowercase();
    let Some(root) = tables
        .iter()
        .find(|held| held.folded == folded)
        .map(|held| held.root)
    else {
        return;
    };
    let Some(tree) = trees.get(&root) else {
        return;
    };
    let rows = statistics_rows(pool, tree);
    apply_statistics(tables, &rows);
}

/// Reads the three-column rows out of a `sqlite_stat1` tree.
///
/// Separate from attaching them so a caller holding `&mut self` can read under
/// an immutable borrow, drop it, and then patch its tables.
///
/// @param pool - the buffer pool the tree lives in
/// @param tree - the statistics tree
pub(crate) fn statistics_rows(
    pool: &Pool,
    tree: &PagedTree,
) -> Vec<(Vec<u8>, Option<Vec<u8>>, Vec<u8>)> {
    let mut rows: Vec<(Vec<u8>, Option<Vec<u8>>, Vec<u8>)> = Vec::new();
    let _ = tree.visit_leaves(pool, &mut |leaf| {
        for row in leaf.live()? {
            // The row is the rowid and then the three columns SQLite's own
            // `sqlite_stat1` carries: the table, the index, the measurement.
            let Some(Datum::Text(table)) = row.get(1) else {
                continue;
            };
            let index = match row.get(2) {
                Some(Datum::Text(name)) => Some(name.to_vec()),
                _ => None,
            };
            let Some(Datum::Text(stat)) = row.get(3) else {
                continue;
            };
            rows.push((table.to_vec(), index, stat.to_vec()));
        }
        Ok(true)
    });
    rows
}

/// Clears every table's measurements and applies the ones just read.
///
/// @param tables - the binder's tables, patched in place
/// @param rows - the `sqlite_stat1` rows
pub(crate) fn apply_statistics(
    tables: &mut [TableInfo],
    rows: &[(Vec<u8>, Option<Vec<u8>>, Vec<u8>)],
) {
    // A stale reading is worse than none, so what is there now replaces
    // whatever a previous load left behind rather than adding to it.
    for table in tables.iter_mut() {
        table.analysed_rows = None;
        for index in &mut table.indexes {
            index.prefix_rows = Vec::new();
        }
    }
    for (table, index, stat) in rows {
        inillucent_catalog::load::apply_statistic(tables, table, index.as_deref(), stat);
    }
}
