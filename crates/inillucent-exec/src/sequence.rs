//! `AUTOINCREMENT`: the high-water mark a table never hands out twice.
//!
//! Invariant: **an `AUTOINCREMENT` table's next key is one past the largest it
//! has ever held**, not one past the largest it holds now. That is the whole of
//! the difference between the two forms, and it is the only thing
//! `AUTOINCREMENT` buys: an ordinary `INTEGER PRIMARY KEY` reuses the numbers
//! its deleted rows had, which makes a foreign key held outside the database -
//! a URL, a log line, a row in another system - point at a different row than
//! the one it was written about.
//!
//! This engine used to parse `AUTOINCREMENT` onto
//! `TableInfo::autoincrement`, resolve `sqlite_sequence`'s root onto
//! `BoundInsert::sequence_root`, and then allocate from the table's own
//! largest key like any other table - so one insert, a delete, and another
//! insert handed out 1 twice where SQLite hands out 1 and then 2.
//!
//! ## Where the mark lives
//!
//! In SQLite's own `sqlite_sequence(name, seq)` table, one row per
//! `AUTOINCREMENT` table, written in the same transaction as the row that moved
//! it. That placement is what makes it roll back with the row: a transaction
//! that inserts and then rolls back leaves the mark where it was, and the next
//! insert reuses the number - which is what SQLite does, and is a *consequence*
//! of the mark being ordinary data rather than a counter beside the file.

use inillucent_base::{DbError, DbResult, PrimaryCode};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_tree::datum::{Datum, OwnedDatum};

use crate::dml::WriteTarget;

/// The table the mark lives in, under SQLite's own name.
pub const SEQUENCE_TABLE: &[u8] = b"sqlite_sequence";

/// The `CREATE` text stored for it, byte for byte SQLite's.
pub const SEQUENCE_SQL: &str = "CREATE TABLE sqlite_sequence(name,seq)";

/// One `sqlite_sequence` row: where it sits, and what it says.
pub struct Mark {
    /// The row's own key, when the row is already there.
    ///
    /// `None` means no row has been written for this table yet, which is the
    /// state between `CREATE TABLE ... AUTOINCREMENT` and its first insert.
    pub rowid: Option<i64>,
    /// The largest key the table has ever handed out.
    pub seq: i64,
}

/// Reads a table's high-water mark.
///
/// The table's own largest key is taken as a floor. It is normally below the
/// stored mark and can only be above it for a table whose rows arrived without
/// the mark being written - a file another engine built, or one this engine
/// wrote before that was fixed - and taking the larger of the two is what stops
/// either of those from handing out a key that is already there.
///
/// @param target - the file and its trees
/// @param sequence_root - the `sqlite_sequence` tree, zero when there is none
/// @param name - the table's name, as `sqlite_sequence` stores it
/// @param floor - the largest key the table itself holds
pub fn read(
    target: &mut dyn WriteTarget,
    sequence_root: u32,
    name: &[u8],
    floor: i64,
) -> DbResult<Mark> {
    let mut mark = Mark {
        rowid: None,
        seq: floor,
    };
    if sequence_root == 0 {
        return Ok(mark);
    }
    let (database, trees, _) = target.parts_for(sequence_root)?;
    let Some(tree) = trees.get(sequence_root) else {
        return Ok(mark);
    };
    tree.visit_leaves(database.pool(), &mut |leaf| {
        for row in leaf.live()? {
            let Some(Datum::Text(held)) = row.get(1) else {
                continue;
            };
            if *held != name {
                continue;
            }
            if let Some(Datum::Int(rowid)) = row.first() {
                mark.rowid = Some(*rowid);
            }
            match row.get(2) {
                Some(Datum::Int(seq)) => mark.seq = mark.seq.max(*seq),
                // A `seq` a person set to something that is not an integer is
                // ignored rather than refused, the way SQLite ignores it.
                _ => continue,
            }
        }
        Ok(true)
    })?;
    Ok(mark)
}

/// Writes a table's high-water mark back, in the caller's transaction.
///
/// @param target - the file and its trees
/// @param sequence_root - the `sqlite_sequence` tree, zero when there is none
/// @param name - the table's name, as `sqlite_sequence` stores it
/// @param mark - the row as it was read, and the value to store
/// @param seq - the new high-water mark
pub fn write(
    target: &mut dyn WriteTarget,
    sequence_root: u32,
    name: &[u8],
    mark: &Mark,
    seq: i64,
) -> DbResult<()> {
    if sequence_root == 0 {
        return Ok(());
    }
    let rowid = match mark.rowid {
        Some(held) => held,
        None => next_rowid(target, sequence_root)?,
    };
    let row = [
        OwnedDatum::Int(rowid),
        OwnedDatum::Text(name.to_vec()),
        OwnedDatum::Int(seq),
    ];
    let (database, trees, log) = target.parts_for(sequence_root)?;
    let Some(tree) = trees.get_mut(sequence_root) else {
        return Ok(());
    };
    let borrowed: Vec<Datum<'_>> = row.iter().map(OwnedDatum::borrow).collect();
    if mark.rowid.is_some() {
        if let Some(key) = borrowed.get(..1) {
            tree.delete(database, log, key)?;
        }
    }
    tree.insert(database, log, &borrowed)?;
    Ok(())
}

/// Removes a table's row, which is what `DROP TABLE` does to it.
///
/// @param target - the file and its trees
/// @param sequence_root - the `sqlite_sequence` tree, zero when there is none
/// @param name - the table's name, as `sqlite_sequence` stores it
pub fn forget(target: &mut dyn WriteTarget, sequence_root: u32, name: &[u8]) -> DbResult<()> {
    let mark = read(target, sequence_root, name, 0)?;
    let Some(rowid) = mark.rowid else {
        return Ok(());
    };
    let (database, trees, log) = target.parts_for(sequence_root)?;
    let Some(tree) = trees.get_mut(sequence_root) else {
        return Ok(());
    };
    tree.delete(database, log, &[Datum::Int(rowid)])?;
    Ok(())
}

/// Returns the key a new `sqlite_sequence` row takes.
///
/// @param target - the file and its trees
/// @param sequence_root - the `sqlite_sequence` tree
fn next_rowid(target: &mut dyn WriteTarget, sequence_root: u32) -> DbResult<i64> {
    let (database, trees, _) = target.parts_for(sequence_root)?;
    let Some(tree) = trees.get(sequence_root) else {
        return Ok(1);
    };
    let mut highest = 0i64;
    tree.visit_leaves(database.pool(), &mut |leaf| {
        for row in leaf.live()? {
            if let Some(Datum::Int(rowid)) = row.first() {
                highest = highest.max(*rowid);
            }
        }
        Ok(true)
    })?;
    Ok(highest.saturating_add(1))
}

/// Returns the key an `AUTOINCREMENT` table hands out next, or the refusal.
///
/// SQLite reports `SQLITE_FULL` - "database or disk is full" - when the mark
/// reaches `i64::MAX`, because there is no next key and the alternative would be
/// to hand out one that is already there.
///
/// @param table - the table being written
/// @param mark - the high-water mark as it stands
pub fn allocate(table: &TableInfo, mark: i64) -> DbResult<i64> {
    if mark == i64::MAX {
        return Err(DbError::primary(PrimaryCode::Full)
            .with_message("database or disk is full")
            .with_detail(format!(
                "{} has handed out every AUTOINCREMENT key",
                String::from_utf8_lossy(&table.name)
            )));
    }
    Ok(mark.saturating_add(1))
}
