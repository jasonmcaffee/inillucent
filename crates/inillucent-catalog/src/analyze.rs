//! `ANALYZE`: measuring a schema, and writing what it measured.
//!
//! Invariant: the file this writes is SQLite's own `sqlite_stat1`, in SQLite's
//! own format, so the pinned build reads statistics inillucent gathered and
//! inillucent reads statistics the pinned build gathered. That is not a nicety.
//! Statistics change which plan is chosen, so an engine that could not read the
//! other's would answer the same rows by a different route, and the difference
//! would show up as a performance mystery rather than as a failure.
//!
//! The format is one row per index and one per table, with `stat` a
//! space-separated list: the table's row count, then for each leading prefix of
//! the index's key, the average number of rows sharing that prefix. A table
//! with no index gets a row with a NULL `idx` and just the count.

use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::DbResult;
use inillucent_sql::catalog_view::{IndexInfo, TableInfo};
use inillucent_storage::cursor::BTreeCursor;
use inillucent_storage::pager::Pager;
use inillucent_value::record::{KeyInfo, RecordRef};
use inillucent_value::{compare, Collation, Value};

/// The name of the table statistics live in.
pub const STAT1: &str = "sqlite_stat1";

/// The `CREATE TABLE` text SQLite stores for it.
pub const STAT1_SQL: &str = "CREATE TABLE sqlite_stat1(tbl,idx,stat)";

/// What one object's statistics say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stat {
    /// The table the row is about.
    pub table: Vec<u8>,
    /// The index, when the row is about one.
    pub index: Option<Vec<u8>>,
    /// The `stat` column, already rendered.
    pub stat: Vec<u8>,
}

/// Measures one table and every index on it.
///
/// The table is scanned once and each index once, which is what `ANALYZE` costs
/// and why it is a statement a person runs rather than something maintained on
/// every write.
pub fn measure(pager: &mut Pager, table: &TableInfo) -> DbResult<Vec<Stat>> {
    let rows = count_rows(pager, table.root)?;
    let mut stats = Vec::new();
    if table.indexes.is_empty() {
        stats.push(Stat {
            table: table.name.clone(),
            index: None,
            stat: rows.to_string().into_bytes(),
        });
        return Ok(stats);
    }
    for index in &table.indexes {
        let stat = measure_index(pager, index, rows)?;
        stats.push(Stat {
            table: table.name.clone(),
            index: Some(index.name.clone()),
            stat,
        });
    }
    Ok(stats)
}

/// Counts the rows of a B-tree.
fn count_rows(pager: &mut Pager, root: u32) -> DbResult<i64> {
    if root == 0 {
        return Ok(0);
    }
    let Some(root) = PageId::new(root) else {
        return Ok(0);
    };
    let mut cursor = BTreeCursor::table(root);
    let mut rows = 0i64;
    let mut present = cursor.first(pager)?;
    while present {
        rows = rows.saturating_add(1);
        present = cursor.next(pager)?;
    }
    Ok(rows)
}

/// Renders one index's `stat` column.
///
/// The averages are computed by walking the index in order and counting how
/// many entries share each leading prefix. Walking in order is what makes it
/// one pass: entries with equal prefixes are adjacent by construction, so a
/// run length is a count and nothing has to be remembered.
fn measure_index(pager: &mut Pager, index: &IndexInfo, rows: i64) -> DbResult<Vec<u8>> {
    let width = index.columns.len();
    let key = key_info(index);
    let limits = Limits::default();
    let encoding = pager.text_encoding();
    let Some(root) = PageId::new(index.root) else {
        return Ok(rows.to_string().into_bytes());
    };
    let mut cursor = BTreeCursor::index(root, key.clone());
    // `groups[n]` counts how many distinct values the first `n + 1` key columns
    // take, which is what the average is the reciprocal of.
    let mut groups = vec![0i64; width];
    let mut previous: Option<Vec<Value<'static>>> = None;
    let mut entries = 0i64;
    let mut present = cursor.first(pager)?;
    while present {
        let payload = cursor.payload(pager, &limits)?;
        let record = RecordRef::parse_with_limits(&payload, encoding, &limits)?;
        let mut current = Vec::with_capacity(width);
        for column in 0..width {
            current.push(record.value(column)?.into_owned()?);
        }
        entries = entries.saturating_add(1);
        match &previous {
            None => {
                for group in groups.iter_mut() {
                    *group = 1;
                }
            }
            Some(before) => {
                // The first column at which the entries differ opens a new
                // group for that prefix and every longer one.
                let mut differ = width;
                for column in 0..width {
                    let left = before.get(column).cloned().unwrap_or(Value::Null);
                    let right = current.get(column).cloned().unwrap_or(Value::Null);
                    let collation = key
                        .columns
                        .get(column)
                        .map_or(Collation::Binary, |rules| rules.collation);
                    let same = match (left.is_null(), right.is_null()) {
                        (true, true) => true,
                        (true, false) | (false, true) => false,
                        (false, false) => {
                            compare::compare_values(&left, &right, collation)
                                == std::cmp::Ordering::Equal
                        }
                    };
                    if !same {
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
        present = cursor.next(pager)?;
    }
    let total = rows.max(entries);
    let mut out = total.to_string();
    for group in &groups {
        // The average rows per distinct prefix, rounded up: SQLite writes an
        // integer, and rounding down would claim a prefix is more selective
        // than it is - which is the direction that picks a bad plan.
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

/// Returns the ordering an index's entries are compared with.
fn key_info(index: &IndexInfo) -> KeyInfo {
    KeyInfo {
        columns: index
            .columns
            .iter()
            .map(|key| inillucent_value::record::KeyColumn {
                collation: Collation::from_name(
                    core::str::from_utf8(&key.collation).unwrap_or("BINARY"),
                )
                .unwrap_or(Collation::Binary),
                descending: key.descending,
            })
            .collect(),
    }
}

/// Parses a `stat` column into the numbers the planner reads.
///
/// A malformed or missing `stat` is not an error: statistics are a hint, and a
/// planner that refused to run on a bad one would turn a stale `ANALYZE` into
/// an outage. Anything unparseable simply leaves the defaults in place.
pub fn parse_stat(stat: &[u8]) -> (i64, Vec<i64>) {
    let mut numbers = stat
        .split(|byte| *byte == b' ')
        .filter(|part| !part.is_empty())
        .map(|part| {
            core::str::from_utf8(part)
                .ok()
                .and_then(|text| text.parse::<i64>().ok())
                .unwrap_or(0)
        });
    let rows = numbers.next().unwrap_or(0);
    (rows.max(0), numbers.map(|value| value.max(1)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stat_column_parses_into_a_count_and_its_averages() {
        assert_eq!(parse_stat(b"10000 100 5 1"), (10000, vec![100, 5, 1]));
        assert_eq!(parse_stat(b"42"), (42, Vec::new()));
    }

    /// A malformed stat leaves the planner on its defaults rather than failing.
    #[test]
    fn a_malformed_stat_is_not_an_error() {
        assert_eq!(parse_stat(b""), (0, Vec::new()));
        // Three words: the first is the row count, so two prefixes remain,
        // and each unreadable one is clamped to the harmless value of 1.
        assert_eq!(parse_stat(b"not a number"), (0, vec![1, 1]));
    }
}
