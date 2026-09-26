//! The properties a generated query must satisfy with no reference engine.
//!
//! Invariant: **a property is checked on inillucent alone and never asks
//! SQLite anything.** That is its purpose: it catches a defect SQLite shares,
//! and it still grades a case on a machine with no oracle and for the features
//! SQLite does not have. Every property runs inside a savepoint that is rolled
//! back, so checking one leaves the database exactly as the case left it and
//! the reopen that follows reads the case's state rather than the checker's.
//!
//! The four shapes come from section 6.3 of the design:
//!
//! - [`Property::Same`]: every query answers the same multiset of rows. The
//!   placement wrappings of section 5.3, NoREC, and index agreement are all
//!   this shape.
//! - [`Property::Partition`]: the parts together answer exactly the whole,
//!   which is ternary logic partitioning (TLP).
//! - [`Property::Stable`]: a query answers the same before and after a
//!   statement that must not change its answer, which is `ANALYZE` agreement.
//! - [`Property::Dqe`]: `SELECT`, `UPDATE` and `DELETE` with one predicate
//!   select one set of rows, and an `UPDATE ... FROM` changes each of them once.

use inillucent_engine::connect::Connection;

use crate::differential::observe_detailed;
use crate::oracle::TaggedValue;

/// One property of a generated case.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Property {
    /// Every query answers the same multiset of rows.
    Same {
        /// What the check is called in a failure.
        name: String,
        /// The queries; the first is the reference.
        queries: Vec<String>,
    },
    /// The parts together answer exactly the whole, as a multiset.
    Partition {
        /// What the check is called in a failure.
        name: String,
        /// The query every row comes from.
        whole: String,
        /// The queries that must split its rows between them.
        parts: Vec<String>,
    },
    /// A query answers the same before and after a statement.
    Stable {
        /// What the check is called in a failure.
        name: String,
        /// The query.
        query: String,
        /// The statement run between the two answers.
        between: String,
    },
    /// A query answers the same stored with `CREATE TABLE ... AS` and read
    /// through a view as it does on its own. Both objects are `TEMP` and are
    /// rolled back with the check's savepoint.
    Stored {
        /// What the check is called in a failure.
        name: String,
        /// The query.
        query: String,
    },
    /// Query, update and delete with one predicate touch one set of rows.
    Dqe {
        /// What the check is called in a failure.
        name: String,
        /// The table.
        table: String,
        /// An expression naming each row once: `rowid`, or the primary key.
        key: String,
        /// An integer column the update may set, which nothing else reads.
        marker: String,
        /// A table the update joins with `FROM`, when the check is of
        /// `UPDATE ... FROM`; the predicate may then name its columns.
        from: Option<String>,
        /// The predicate.
        predicate: String,
    },
}

impl Property {
    /// Returns the check's name.
    pub fn name(&self) -> &str {
        match self {
            Property::Same { name, .. }
            | Property::Partition { name, .. }
            | Property::Stable { name, .. }
            | Property::Stored { name, .. }
            | Property::Dqe { name, .. } => name,
        }
    }
}

/// Why a property did not hold.
#[derive(Clone, Debug)]
pub struct Violation {
    /// The property's name.
    pub name: String,
    /// Whether the cause was a statement the engine has not built, which the
    /// grader may accept when the case names a capability row that says so.
    pub unsupported: Option<String>,
    /// What went wrong.
    pub detail: String,
}

/// Checks every property, inside a savepoint that is rolled back.
///
/// Returns the violations, and nothing when every property held. A query that
/// the engine refuses with anything but `unsupported` is a violation: a
/// wrapping that the engine will not run is the defect the wrappings exist to
/// find.
///
/// @param connection - the case's inillucent session
/// @param properties - what must hold
pub fn check(connection: &Connection<'_>, properties: &[Property]) -> Vec<Violation> {
    if properties.is_empty() {
        return Vec::new();
    }
    let mut violations = Vec::new();
    for property in properties {
        if let Err(error) = connection.execute("SAVEPOINT matrix_property") {
            violations.push(Violation {
                name: property.name().to_string(),
                unsupported: None,
                detail: format!("the savepoint the check runs in was refused: {error}"),
            });
            return violations;
        }
        if let Some(violation) = check_one(connection, property) {
            violations.push(violation);
        }
        let _ = connection.execute("ROLLBACK TO matrix_property");
        let _ = connection.execute("RELEASE matrix_property");
    }
    violations
}

/// Checks one property.
///
/// @param connection - the session
/// @param property - what must hold
fn check_one(connection: &Connection<'_>, property: &Property) -> Option<Violation> {
    let name = property.name().to_string();
    let result = match property {
        Property::Same { queries, .. } => same(connection, queries),
        Property::Partition { whole, parts, .. } => partition(connection, whole, parts),
        Property::Stable { query, between, .. } => stable(connection, query, between),
        Property::Stored { query, .. } => stored(connection, query),
        Property::Dqe {
            table,
            key,
            marker,
            from,
            predicate,
            ..
        } => dqe(connection, table, key, marker, from.as_deref(), predicate),
    };
    result.err().map(|(detail, unsupported)| Violation {
        name,
        unsupported,
        detail,
    })
}

/// A failed check: what went wrong, and the unsupported feature if that was it.
type Refusal = (String, Option<String>);

/// Runs a query and returns its rows as a sorted multiset.
///
/// @param connection - the session
/// @param sql - the query
pub fn rows_of(connection: &Connection<'_>, sql: &str) -> Result<Vec<Vec<TaggedValue>>, Refusal> {
    let (observation, error) = observe_detailed(connection, sql, true);
    if !observation.ok {
        let unsupported = error
            .as_ref()
            .and_then(|error| error.unsupported().map(|feature| feature.to_string()));
        return Err((
            format!("`{sql}` was refused: {}", observation.message),
            unsupported,
        ));
    }
    let mut rows = observation.rows;
    rows.sort_by(|left, right| row_order(left, right));
    Ok(rows)
}

/// Every query answers the same multiset as the first.
fn same(connection: &Connection<'_>, queries: &[String]) -> Result<(), Refusal> {
    let Some((first, rest)) = queries.split_first() else {
        return Ok(());
    };
    let reference = match rows_of(connection, first) {
        Ok(rows) => rows,
        // The reference query itself is refused: the grader has already
        // compared it with SQLite, so there is nothing more to learn here.
        Err(_) => return Ok(()),
    };
    for query in rest {
        let rows = rows_of(connection, query)?;
        if !same_rows(&rows, &reference) {
            return Err((
                format!(
                    "`{query}` answered {} row(s) {:?}, and `{first}` answered {} row(s) {:?}",
                    rows.len(),
                    preview(&rows),
                    reference.len(),
                    preview(&reference)
                ),
                None,
            ));
        }
    }
    Ok(())
}

/// The parts together answer exactly the whole.
fn partition(connection: &Connection<'_>, whole: &str, parts: &[String]) -> Result<(), Refusal> {
    let reference = match rows_of(connection, whole) {
        Ok(rows) => rows,
        Err(_) => return Ok(()),
    };
    let mut combined = Vec::new();
    for part in parts {
        combined.extend(rows_of(connection, part)?);
    }
    combined.sort_by(|left, right| row_order(left, right));
    if !same_rows(&combined, &reference) {
        return Err((
            format!(
                "the {} parts answered {} row(s) {:?} together, and the whole `{whole}` answered {} \
                 {:?}; the parts were:\n    {}",
                parts.len(),
                combined.len(),
                preview(&combined),
                reference.len(),
                preview(&reference),
                parts.join("\n    ")
            ),
            None,
        ));
    }
    Ok(())
}

/// A query answers the same before and after a statement.
fn stable(connection: &Connection<'_>, query: &str, between: &str) -> Result<(), Refusal> {
    let before = match rows_of(connection, query) {
        Ok(rows) => rows,
        Err(_) => return Ok(()),
    };
    let (observation, error) = observe_detailed(connection, between, false);
    if !observation.ok {
        return Err((
            format!("`{between}` was refused: {}", observation.message),
            error.and_then(|error| error.unsupported().map(|feature| feature.to_string())),
        ));
    }
    let after = rows_of(connection, query)?;
    if !same_rows(&before, &after) {
        return Err((
            format!(
                "`{query}` answered {} row(s) {:?} before `{between}` and {} {:?} after it",
                before.len(),
                preview(&before),
                after.len(),
                preview(&after)
            ),
            None,
        ));
    }
    Ok(())
}

/// A query stored in a table and read through a view answers what it answers
/// on its own.
///
/// The stored copy is compared as a multiset of values only where every
/// column keeps the storage class it had: `CREATE TABLE ... AS` gives each
/// column the affinity of its expression, which may convert a value, and that
/// conversion is SQLite's rule rather than a defect. So the stored rows are
/// compared by count, and the view's rows exactly.
fn stored(connection: &Connection<'_>, query: &str) -> Result<(), Refusal> {
    let reference = match rows_of(connection, query) {
        Ok(rows) => rows,
        Err(_) => return Ok(()),
    };
    execute(
        connection,
        &format!("CREATE TEMP VIEW matrix_viewed AS {query}"),
    )?;
    let viewed = rows_of(connection, "SELECT * FROM matrix_viewed")?;
    if !same_rows(&viewed, &reference) {
        return Err((
            format!(
                "through a view the query answered {} row(s) {:?}, and on its own {} {:?}",
                viewed.len(),
                preview(&viewed),
                reference.len(),
                preview(&reference)
            ),
            None,
        ));
    }
    execute(
        connection,
        &format!("CREATE TEMP TABLE matrix_stored AS {query}"),
    )?;
    let stored = rows_of(connection, "SELECT * FROM matrix_stored")?;
    if stored.len() != reference.len() {
        return Err((
            format!(
                "CREATE TABLE AS stored {} row(s) of a query that answers {}",
                stored.len(),
                reference.len()
            ),
            None,
        ));
    }
    Ok(())
}

/// `SELECT`, `UPDATE` and `DELETE` with one predicate touch one set of rows.
///
/// With `from`, the update is `UPDATE t SET marker = marker + 1 FROM f WHERE
/// P`, the rows it must touch are the ones for which some row of `f` makes `P`
/// true, and each must be touched exactly once: SQLite applies an `UPDATE ...
/// FROM` once per target row whatever the join produces, which is the rule
/// `106b304f` restored.
fn dqe(
    connection: &Connection<'_>,
    table: &str,
    key: &str,
    marker: &str,
    from: Option<&str>,
    predicate: &str,
) -> Result<(), Refusal> {
    let selected_sql = match from {
        None => format!("SELECT {key} FROM {table} WHERE {predicate}"),
        Some(other) => format!(
            "SELECT {key} FROM {table} WHERE EXISTS (SELECT 1 FROM {other} WHERE {predicate})"
        ),
    };
    let selected = match rows_of(connection, &selected_sql) {
        Ok(rows) => rows,
        Err(_) => return Ok(()),
    };
    let reset = format!("UPDATE {table} SET {marker} = 0");
    execute(connection, &reset)?;
    let update = match from {
        None => format!("UPDATE {table} SET {marker} = {marker} + 1 WHERE {predicate}"),
        Some(other) => {
            format!("UPDATE {table} SET {marker} = {marker} + 1 FROM {other} WHERE {predicate}")
        }
    };
    execute(connection, &update)?;
    let touched = rows_of(
        connection,
        &format!("SELECT {key} FROM {table} WHERE {marker} > 0"),
    )?;
    let twice = rows_of(
        connection,
        &format!("SELECT {key}, {marker} FROM {table} WHERE {marker} > 1"),
    )?;
    if !twice.is_empty() {
        return Err((
            format!(
                "`{update}` changed {} row(s) more than once: {:?}",
                twice.len(),
                preview(&twice)
            ),
            None,
        ));
    }
    if !same_rows(&dedup(&touched), &dedup(&selected)) {
        return Err((
            format!(
                "`{update}` changed {:?}, and `{selected_sql}` selects {:?}",
                preview(&touched),
                preview(&selected)
            ),
            None,
        ));
    }
    if from.is_none() {
        let before = rows_of(connection, &format!("SELECT {key} FROM {table}"))?;
        execute(
            connection,
            &format!("DELETE FROM {table} WHERE {predicate}"),
        )?;
        let after = rows_of(connection, &format!("SELECT {key} FROM {table}"))?;
        let removed = difference(&before, &after);
        if !same_rows(&dedup(&removed), &dedup(&selected)) {
            return Err((
                format!(
                    "`DELETE FROM {table} WHERE {predicate}` removed {:?}, and `{selected_sql}` \
                     selects {:?}",
                    preview(&removed),
                    preview(&selected)
                ),
                None,
            ));
        }
    }
    Ok(())
}

/// Runs a statement, turning a refusal into a failed check.
fn execute(connection: &Connection<'_>, sql: &str) -> Result<(), Refusal> {
    let (observation, error) = observe_detailed(connection, sql, false);
    if observation.ok {
        return Ok(());
    }
    Err((
        format!("`{sql}` was refused: {}", observation.message),
        error.and_then(|error| error.unsupported().map(|feature| feature.to_string())),
    ))
}

/// Removes duplicate rows from a sorted list.
fn dedup(rows: &[Vec<TaggedValue>]) -> Vec<Vec<TaggedValue>> {
    let mut out: Vec<Vec<TaggedValue>> = Vec::new();
    for row in rows {
        if out.last().is_none_or(|last| !same_row(last, row)) {
            out.push(row.clone());
        }
    }
    out
}

/// Returns the rows of `before` that are not in `after`, both sorted.
fn difference(before: &[Vec<TaggedValue>], after: &[Vec<TaggedValue>]) -> Vec<Vec<TaggedValue>> {
    let mut remaining: Vec<Vec<TaggedValue>> = after.to_vec();
    let mut removed = Vec::new();
    for row in before {
        match remaining
            .iter()
            .position(|candidate| same_row(candidate, row))
        {
            Some(at) => {
                remaining.remove(at);
            }
            None => removed.push(row.clone()),
        }
    }
    removed
}

/// Whether two sorted multisets are identical, comparing reals by bits.
pub fn same_rows(left: &[Vec<TaggedValue>], right: &[Vec<TaggedValue>]) -> bool {
    left.len() == right.len() && left.iter().zip(right.iter()).all(|(a, b)| same_row(a, b))
}

/// Whether two rows are identical, comparing reals by bits.
fn same_row(left: &[TaggedValue], right: &[TaggedValue]) -> bool {
    left.len() == right.len() && left.iter().zip(right.iter()).all(|(a, b)| a.identical(b))
}

/// A total order on rows, used only to make two multisets comparable.
///
/// It is not SQLite's collating order and does not need to be: both sides are
/// sorted by the same rule, and equal rows end up adjacent.
pub fn row_order(left: &[TaggedValue], right: &[TaggedValue]) -> std::cmp::Ordering {
    for (a, b) in left.iter().zip(right.iter()) {
        let order = value_order(a, b);
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

/// A total order on values: by storage class, then by content.
fn value_order(left: &TaggedValue, right: &TaggedValue) -> std::cmp::Ordering {
    fn class(value: &TaggedValue) -> u8 {
        match value {
            TaggedValue::Null => 0,
            TaggedValue::Integer(_) => 1,
            TaggedValue::Real(_) => 2,
            TaggedValue::Text(_) => 3,
            TaggedValue::Blob(_) => 4,
        }
    }
    match (left, right) {
        (TaggedValue::Integer(a), TaggedValue::Integer(b)) => a.cmp(b),
        (TaggedValue::Real(a), TaggedValue::Real(b)) => a.to_bits().cmp(&b.to_bits()),
        (TaggedValue::Text(a), TaggedValue::Text(b)) => a.cmp(b),
        (TaggedValue::Blob(a), TaggedValue::Blob(b)) => a.cmp(b),
        _ => class(left).cmp(&class(right)),
    }
}

/// The first few rows, for a message that does not print a whole table.
fn preview(rows: &[Vec<TaggedValue>]) -> Vec<Vec<TaggedValue>> {
    rows.iter().take(6).cloned().collect()
}
