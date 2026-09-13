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
use inillucent_catalog::paged::ObjectKind;
use inillucent_pool::Pool;
use inillucent_sql::catalog_view::{IndexInfo, TableInfo};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::PagedTree;

use super::{ImportedDatabase, Outcome, WalLog};

/// The name statistics live under, which is SQLite's.
const STAT1: &[u8] = b"sqlite_stat1";

impl ImportedDatabase {
    /// Measures the schema and writes `sqlite_stat1`.
    ///
    /// @param table - the one table to measure, or nothing for all of them
    pub(super) fn analyze(&mut self, table: Option<&[u8]>) -> DbResult<Outcome> {
        let wanted = table.map(|name| name.to_ascii_lowercase());
        // A named object may be an *index*, in which case SQLite measures the
        // table it is on. Resolving it here rather than refusing keeps
        // `ANALYZE main_label` doing what a person who typed it meant.
        let wanted = match wanted {
            Some(name) if !self.tables.iter().any(|held| held.folded == name) => self
                .tables
                .iter()
                .find(|held| held.indexes.iter().any(|index| index.folded == name))
                .map(|held| held.folded.clone())
                .or(Some(name)),
            other => other,
        };
        self.ensure_stat1()?;
        // **Only the things that have rows to count.** A view has none, a
        // virtual table's are the module's, and a reserved name is bookkeeping
        // this engine wrote itself. Measuring them wrote rows the reference
        // never writes, which showed up as `.dump` emitting
        // `INSERT INTO sqlite_stat1 VALUES('vv',NULL,'0')` for a *view* - and
        // then, when that dump was replayed, as statistics claiming the tables
        // were empty.
        let subjects: Vec<TableInfo> = self
            .tables
            .iter()
            .filter(|held| !held.folded.starts_with(b"sqlite_"))
            .filter(|held| held.kind == inillucent_sql::catalog_view::TableKind::Table)
            .filter(|held| held.module.is_none())
            .filter(|held| wanted.as_ref().is_none_or(|name| held.folded == *name))
            .cloned()
            .collect();
        let mut rows: Vec<(Vec<u8>, Option<Vec<u8>>, Vec<u8>)> = Vec::new();
        for subject in &subjects {
            let count = self.live_rows(subject.root)?;
            if subject.indexes.is_empty() {
                rows.push((subject.name.clone(), None, count.to_string().into_bytes()));
                continue;
            }
            for index in &subject.indexes {
                let stat = self.measure_index(index, count)?;
                rows.push((subject.name.clone(), Some(index.name.clone()), stat));
            }
        }
        // Every row for the tables being measured is replaced, and the rows for
        // tables that were not measured are left alone - which is what
        // `ANALYZE one_table` means.
        let names: Vec<Vec<u8>> = subjects.iter().map(|held| held.name.clone()).collect();
        self.clear_stat1(&names)?;
        self.write_stat1(&rows)?;
        self.seal()?;
        Ok(Outcome::empty())
    }

    /// Creates `sqlite_stat1` if the schema has not got one.
    ///
    /// SQLite creates it on the first `ANALYZE` and leaves it in the schema
    /// afterwards, so the catalog row is part of what `ANALYZE` does rather than
    /// a side effect of it.
    fn ensure_stat1(&mut self) -> DbResult<()> {
        let folded = STAT1.to_ascii_lowercase();
        if self.tables.iter().any(|held| held.folded == folded) {
            return Ok(());
        }
        self.define_table(STAT1, STAT1_SQL.as_bytes().to_vec())?;
        self.refresh_catalog();
        Ok(())
    }

    /// Returns how many live rows a tree holds.
    ///
    /// The packed row count in the handle is the count as the tree was *built*;
    /// a delta area and a tombstone both move it, and `ANALYZE` is measuring the
    /// table as it is now.
    ///
    /// @param root - the tree's identifier
    fn live_rows(&self, root: u32) -> DbResult<i64> {
        let Some(tree) = self.trees.get(&root) else {
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
        let Some(tree) = self.trees.get(&index.root) else {
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
        for group in &groups {
            let average = if *group <= 0 {
                total.max(1)
            } else {
                total.saturating_add(group.saturating_sub(1)) / *group
            };
            out.push(' ');
            out.push_str(&average.max(1).to_string());
        }
        Ok(out.into_bytes())
    }

    /// Removes the `sqlite_stat1` rows for the tables about to be re-measured.
    ///
    /// @param tables - the table names whose rows go
    fn clear_stat1(&mut self, tables: &[Vec<u8>]) -> DbResult<()> {
        let folded = STAT1.to_ascii_lowercase();
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
            let tree = self
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
                    if named {
                        keys.push(rowid);
                    }
                }
                Ok(true)
            })?;
            keys
        };
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
        let tree = self
            .trees
            .get_mut(&root)
            .ok_or_else(|| refusal("sqlite_stat1 has no tree"))?;
        for rowid in doomed {
            tree.delete(&mut self.database, &mut log, &[Datum::Int(rowid)])?;
        }
        Ok(())
    }

    /// Writes the measured rows into `sqlite_stat1`.
    ///
    /// @param rows - table, index and rendered `stat`
    fn write_stat1(&mut self, rows: &[(Vec<u8>, Option<Vec<u8>>, Vec<u8>)]) -> DbResult<()> {
        let folded = STAT1.to_ascii_lowercase();
        let Some(root) = self
            .tables
            .iter()
            .find(|held| held.folded == folded)
            .map(|held| held.root)
        else {
            return Ok(());
        };
        let mut next = {
            let pool = self.pool_of(root)?;
            let tree = self
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
        let tree = self
            .trees
            .get_mut(&root)
            .ok_or_else(|| refusal("sqlite_stat1 has no tree"))?;
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
            tree.insert(&mut self.database, &mut log, &row)?;
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
            .tables
            .iter()
            .find(|held| held.folded == folded)
            .map(|held| held.root)
        else {
            return Vec::new();
        };
        let Some(tree) = self.trees.get(&root) else {
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
    pub(crate) fn republish_statistics(&mut self) {
        let folded = STAT1.to_ascii_lowercase();
        let Some(root) = self
            .tables
            .iter()
            .find(|held| held.folded == folded)
            .map(|held| held.root)
        else {
            // No statistics table: clear whatever a previous one left, so a
            // `DROP TABLE sqlite_stat1` stops changing plans.
            apply_statistics(&mut self.tables, &[]);
            return;
        };
        // Read under an immutable borrow, then patch: the rows come back owned,
        // which is what lets both happen in one method.
        let rows = match (self.trees.get(&root), self.pool_of(root)) {
            (Some(tree), Ok(pool)) => statistics_rows(pool, tree),
            _ => Vec::new(),
        };
        apply_statistics(&mut self.tables, &rows);
    }

    /// Reports whether the schema has a `sqlite_stat1` row.
    pub fn has_statistics(&self) -> bool {
        self.entries
            .iter()
            .any(|held| held.entry.kind == ObjectKind::Table && held.entry.name == STAT1)
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
