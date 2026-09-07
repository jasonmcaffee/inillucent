//! Foreign keys, as the triggers they are.
//!
//! Invariant: a foreign key is enforced by exactly the machinery a written
//! trigger is enforced by. The clause is turned into `CREATE TRIGGER` text,
//! parsed by the same parser, bound by the same binder and inlined by the same
//! compiler - so `ON DELETE CASCADE` and the `DELETE` somebody wrote by hand
//! cannot disagree about what a conflict clause does, what `OLD` means, or what
//! order things happen in. SQLite makes the same choice for the same reason.
//!
//! Generating text rather than building bound structures is deliberate. The
//! text is printable, so a diagnostic can show what a constraint actually does,
//! and it is the same shape a person would have written - which means every
//! test that covers written triggers covers this too.
//!
//! Four kinds of trigger come out of one clause:
//!
//! - the child's check, on `INSERT` and on `UPDATE OF` its own key columns,
//!   which refuses a row whose parent is not there;
//! - the parent's check, on `DELETE` and on `UPDATE OF` its key, which refuses
//!   to strand a child - this is `NO ACTION` and `RESTRICT`;
//! - the parent's `CASCADE`, which deletes or updates the children with it;
//! - the parent's `SET NULL` and `SET DEFAULT`, which keep the children and
//!   let go of the key.
//!
//! Reference: <https://sqlite.org/foreignkeys.html>.

use inillucent_base::limits::Limits;

use crate::ast::{ReferentialAction, TriggerTime};
use crate::catalog_view::{
    ForeignKeyInfo, ForeignKeyTrigger, TableInfo, TableKind, TriggerEventInfo, TriggerInfo,
};
use crate::parser::parse_next_statement;

/// The message SQLite reports for every foreign-key violation.
pub const VIOLATION_MESSAGE: &str = "FOREIGN KEY constraint failed";

/// Which write a synthesised trigger is generated for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForeignKeyEvent {
    /// A row is being added to the child table.
    ChildInsert,
    /// A row of the child table is being changed.
    ChildUpdate,
    /// A row is being taken out of the parent table.
    ParentDelete,
    /// A row of the parent table is being changed.
    ParentUpdate,
}

impl ForeignKeyEvent {
    /// Reports whether the event happens on the child's side.
    pub fn is_child(self) -> bool {
        matches!(
            self,
            ForeignKeyEvent::ChildInsert | ForeignKeyEvent::ChildUpdate
        )
    }
}

/// The parent columns a key refers to.
///
/// A clause that named none refers to the parent's primary key, and that is
/// resolved here rather than in the catalog because the catalog reads one table
/// at a time and the parent may not have been read yet.
pub fn parent_columns(key: &ForeignKeyInfo, parent: &TableInfo) -> Option<Vec<Vec<u8>>> {
    if !key.parent_columns.is_empty() {
        return Some(key.parent_columns.clone());
    }
    let primary = parent.primary_key();
    if primary.is_empty() {
        return None;
    }
    let mut names = Vec::with_capacity(primary.len());
    for position in primary {
        names.push(parent.columns.get(usize::from(position))?.name.clone());
    }
    Some(names)
}

/// Returns the child column names of a key, in the order they were written.
fn child_columns(key: &ForeignKeyInfo, child: &TableInfo) -> Option<Vec<Vec<u8>>> {
    let mut names = Vec::with_capacity(key.columns.len());
    for position in &key.columns {
        names.push(child.columns.get(usize::from(*position))?.name.clone());
    }
    Some(names)
}

/// Writes an identifier the way it can be read back.
fn quoted(name: &[u8], out: &mut String) {
    out.push('"');
    for byte in name {
        if *byte == b'"' {
            out.push('"');
        }
        out.push(char::from(*byte));
    }
    out.push('"');
}

/// Returns an identifier as a quoted string.
fn quote(name: &[u8]) -> String {
    let mut out = String::new();
    quoted(name, &mut out);
    out
}

/// Returns `db."table"`, so a body cannot be captured by a `temp` table of the
/// same name.
fn qualified(database: &[u8], table: &[u8]) -> String {
    let mut out = quote(database);
    out.push('.');
    quoted(table, &mut out);
    out
}

/// Joins the parts of a key comparison with `AND`.
fn conjunction(parts: &[String]) -> String {
    if parts.is_empty() {
        return "1".to_string();
    }
    parts.join(" AND ")
}

/// Returns `"c1" = OLD."p1" AND ...`, which finds the children of one parent.
fn children_of(child: &[Vec<u8>], parent: &[Vec<u8>], row: &str) -> String {
    let mut parts = Vec::with_capacity(child.len());
    for (near, far) in child.iter().zip(parent.iter()) {
        parts.push(format!("{} = {row}.{}", quote(near), quote(far)));
    }
    conjunction(&parts)
}

/// Returns the name a synthesised trigger is known by.
///
/// It has to be unique and it has to be stable: the binder's recursion guard is
/// a list of names, so two different constraints on the same table must not
/// collide, and the same constraint must be recognisable when the cascade
/// reaches it again.
fn trigger_name(child: &TableInfo, key: &ForeignKeyInfo, event: ForeignKeyEvent) -> Vec<u8> {
    let suffix = match event {
        ForeignKeyEvent::ChildInsert => "ci",
        ForeignKeyEvent::ChildUpdate => "cu",
        ForeignKeyEvent::ParentDelete => "pd",
        ForeignKeyEvent::ParentUpdate => "pu",
    };
    let mut name = b"sqlite_fk_".to_vec();
    name.extend_from_slice(&child.folded);
    name.push(b'_');
    name.extend_from_slice(key.id.to_string().as_bytes());
    name.push(b'_');
    name.extend_from_slice(suffix.as_bytes());
    name
}

/// Builds the trigger that enforces one key for one event, if there is one.
///
/// `None` means the event needs no trigger - a `NO ACTION` parent key whose
/// checks are deferred to the commit, for instance, or a child key whose
/// columns this update does not touch.
pub fn trigger_for(
    child: &TableInfo,
    parent: &TableInfo,
    key: &ForeignKeyInfo,
    event: ForeignKeyEvent,
    database: &[u8],
    deferred: bool,
    limits: &Limits,
) -> Option<TriggerInfo> {
    let near = child_columns(key, child)?;
    let far = parent_columns(key, parent)?;
    if near.len() != far.len() || near.is_empty() {
        return None;
    }
    let sql = match event {
        ForeignKeyEvent::ChildInsert | ForeignKeyEvent::ChildUpdate => {
            if deferred {
                return None;
            }
            child_check(child, parent, key, event, database, &near, &far)
        }
        ForeignKeyEvent::ParentDelete | ForeignKeyEvent::ParentUpdate => {
            parent_action(child, parent, key, event, database, &near, &far, deferred)?
        }
    };
    build(&sql, trigger_name(child, key, event), limits)
}

/// Parses generated trigger text into the form the binder consumes.
///
/// A generator that produced text the parser refuses would be a defect this
/// function cannot repair, so it returns `None` and the caller enforces
/// nothing - which is caught by the tests rather than by a user.
fn build(sql: &str, name: Vec<u8>, limits: &Limits) -> Option<TriggerInfo> {
    let parsed = parse_next_statement(sql.as_bytes(), 0, limits).ok()?;
    let crate::ast::Statement::CreateTrigger {
        time,
        event,
        when,
        body,
        ..
    } = &parsed.statement
    else {
        return None;
    };
    let event = match event {
        crate::ast::TriggerEvent::Insert => TriggerEventInfo::Insert,
        crate::ast::TriggerEvent::Delete => TriggerEventInfo::Delete,
        crate::ast::TriggerEvent::Update(columns) => TriggerEventInfo::Update(
            columns
                .iter()
                .map(|column| parsed.ast.folded(*column).to_vec())
                .collect(),
        ),
    };
    Some(TriggerInfo {
        folded: name.to_ascii_lowercase(),
        name,
        time: time.unwrap_or(TriggerTime::Before),
        event,
        when: *when,
        body: body.clone(),
        ast: parsed.ast,
    })
}

/// Generates the child's check: a row whose key is complete must have a parent.
///
/// A key with a NULL in it is not checked at all. That is `MATCH SIMPLE`, which
/// is the only match mode SQLite implements whatever the clause says, and it is
/// why the guard is a conjunction of `IS NOT NULL` rather than a single test.
fn child_check(
    child: &TableInfo,
    parent: &TableInfo,
    key: &ForeignKeyInfo,
    event: ForeignKeyEvent,
    database: &[u8],
    near: &[Vec<u8>],
    far: &[Vec<u8>],
) -> String {
    let mut guards: Vec<String> = near
        .iter()
        .map(|column| format!("NEW.{} IS NOT NULL", quote(column)))
        .collect();
    let lookup = children_of(far, near, "NEW");
    guards.push(format!(
        "NOT EXISTS (SELECT 1 FROM {} WHERE {lookup})",
        qualified(database, &parent.name)
    ));
    let fires = match event {
        ForeignKeyEvent::ChildUpdate => format!("BEFORE UPDATE OF {} ON", column_list(near)),
        _ => "BEFORE INSERT ON".to_string(),
    };
    format!(
        "CREATE TRIGGER {} {fires} {} BEGIN SELECT RAISE(ABORT, '{VIOLATION_MESSAGE}') WHERE {}; END",
        quote(&trigger_name(child, key, event)),
        quote(&child.name),
        conjunction(&guards)
    )
}

/// Generates what happens to the children when a parent row goes or changes.
fn parent_action(
    child: &TableInfo,
    parent: &TableInfo,
    key: &ForeignKeyInfo,
    event: ForeignKeyEvent,
    database: &[u8],
    near: &[Vec<u8>],
    far: &[Vec<u8>],
    deferred: bool,
) -> Option<String> {
    let action = match event {
        ForeignKeyEvent::ParentDelete => key.on_delete,
        _ => key.on_update,
    };
    let matching = children_of(near, far, "OLD");
    let target = qualified(database, &child.name);
    let body = match action {
        ReferentialAction::NoAction | ReferentialAction::Restrict => {
            // RESTRICT is not deferrable: it refuses the write where it
            // happens, whatever the constraint's timing says. NO ACTION with a
            // deferred constraint is checked when the transaction commits, so
            // there is no trigger for it here.
            if deferred && action == ReferentialAction::NoAction {
                return None;
            }
            format!(
                "SELECT RAISE(ABORT, '{VIOLATION_MESSAGE}') WHERE EXISTS (SELECT 1 FROM {target} WHERE {matching});"
            )
        }
        ReferentialAction::Cascade => match event {
            ForeignKeyEvent::ParentDelete => {
                format!("DELETE FROM {target} WHERE {matching};")
            }
            _ => {
                let sets: Vec<String> = near
                    .iter()
                    .zip(far.iter())
                    .map(|(child_column, parent_column)| {
                        format!("{} = NEW.{}", quote(child_column), quote(parent_column))
                    })
                    .collect();
                format!("UPDATE {target} SET {} WHERE {matching};", sets.join(", "))
            }
        },
        ReferentialAction::SetNull => {
            let sets: Vec<String> = near
                .iter()
                .map(|column| format!("{} = NULL", quote(column)))
                .collect();
            format!("UPDATE {target} SET {} WHERE {matching};", sets.join(", "))
        }
        ReferentialAction::SetDefault => {
            let mut sets = Vec::with_capacity(near.len());
            for (position, column) in key.columns.iter().zip(near.iter()) {
                let default = child
                    .columns
                    .get(usize::from(*position))
                    .and_then(|info| info.default_sql.clone())
                    .unwrap_or_else(|| b"NULL".to_vec());
                sets.push(format!(
                    "{} = ({})",
                    quote(column),
                    String::from_utf8_lossy(&default)
                ));
            }
            format!("UPDATE {target} SET {} WHERE {matching};", sets.join(", "))
        }
    };
    // RESTRICT fires before the parent row is written, the rest afterwards.
    // The difference is visible: a `BEFORE DELETE` trigger that removes the
    // children itself satisfies NO ACTION and does not satisfy RESTRICT.
    let time = if action == ReferentialAction::Restrict {
        "BEFORE"
    } else {
        "AFTER"
    };
    let fires = match event {
        ForeignKeyEvent::ParentDelete => format!("{time} DELETE ON"),
        _ => format!("{time} UPDATE OF {} ON", column_list(far)),
    };
    // An update that leaves the key alone is not a change to the key, and
    // firing for it would cascade a row onto itself.
    let guard = match event {
        ForeignKeyEvent::ParentUpdate => {
            let changed: Vec<String> = far
                .iter()
                .map(|column| {
                    let name = quote(column);
                    format!("OLD.{name} IS NOT NEW.{name}")
                })
                .collect();
            format!(" WHEN {}", changed.join(" OR "))
        }
        _ => String::new(),
    };
    Some(format!(
        "CREATE TRIGGER {} {fires} {}{guard} BEGIN {body} END",
        quote(&trigger_name(child, key, event)),
        quote(&parent.name)
    ))
}

/// Renders a comma-separated list of quoted column names.
fn column_list(columns: &[Vec<u8>]) -> String {
    columns
        .iter()
        .map(|column| quote(column))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the triggers every table's writes fire because of a foreign key.
///
/// It runs once per schema, over every table at once, because that is the only
/// point at which both sides of a key are visible: a child records the key and
/// nothing records the reverse direction, so the parent's side is found by
/// asking every table what it points at.
///
/// A key that cannot be enforced - a parent that is not there, or parent
/// columns that are not a key of the parent - produces an entry with no trigger
/// and the message to report. That is SQLite's timing: the schema loads, and
/// the first write that needs the constraint is what fails.
pub fn plan_schema(tables: &mut [TableInfo], database: &[u8], limits: &Limits) {
    mark_cycles(tables);
    let snapshot: Vec<TableInfo> = tables.to_vec();
    for table in tables.iter_mut() {
        if table.kind != TableKind::Table {
            continue;
        }
        table.foreign_key_triggers = plan_table(table, &snapshot, database, limits);
    }
}

/// Marks every key whose parent can lead back to its own child table.
///
/// The graph is small - one node per table, one edge per key - so the search is
/// a plain walk from each key's parent looking for its child. What it answers
/// is whether applying this key's action can fire the same key again.
fn mark_cycles(tables: &mut [TableInfo]) {
    let edges: Vec<(Vec<u8>, Vec<u8>)> = tables
        .iter()
        .flat_map(|table| {
            table
                .foreign_keys
                .iter()
                .map(|key| (table.folded.clone(), key.parent_folded.clone()))
        })
        .collect();
    for table in tables.iter_mut() {
        for key in &mut table.foreign_keys {
            key.cyclic = reaches(&edges, &key.parent_folded, &table.folded);
        }
    }
}

/// Reports whether `from` can reach `wanted` by following child-to-parent
/// edges backwards, which is the direction an action travels.
fn reaches(edges: &[(Vec<u8>, Vec<u8>)], from: &[u8], wanted: &[u8]) -> bool {
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut pending: Vec<Vec<u8>> = vec![from.to_vec()];
    while let Some(table) = pending.pop() {
        if table == wanted {
            return true;
        }
        if seen.contains(&table) {
            continue;
        }
        seen.push(table.clone());
        for (child, parent) in edges {
            if *child == table {
                pending.push(parent.clone());
            }
        }
    }
    false
}

/// Returns the statement that repairs one cyclic key, or `None` when the key
/// has nothing to repair.
///
/// This is the other half of a cyclic action. The trigger takes the first
/// level - the rows that pointed directly at the row that went - and this
/// takes what that leaves: every row whose key now has no parent. Repeating it
/// until nothing changes reaches the leaves, however deep they are, and it
/// terminates because every pass either changes a row or stops.
///
/// `NO ACTION` and `RESTRICT` are absent on purpose: they refuse rather than
/// repair, and the trigger has already refused.
pub fn sweep_statement(
    child: &TableInfo,
    parent: &TableInfo,
    key: &ForeignKeyInfo,
    database: &[u8],
) -> Option<String> {
    let near = child_columns(key, child)?;
    let far = parent_columns(key, parent)?;
    if near.len() != far.len() || near.is_empty() {
        return None;
    }
    let outer = quote(&child.name);
    let mut guards: Vec<String> = near
        .iter()
        .map(|column| format!("{outer}.{} IS NOT NULL", quote(column)))
        .collect();
    let lookup: Vec<String> = far
        .iter()
        .zip(near.iter())
        .map(|(parent_column, child_column)| {
            format!(
                "p.{} = {outer}.{}",
                quote(parent_column),
                quote(child_column)
            )
        })
        .collect();
    guards.push(format!(
        "NOT EXISTS (SELECT 1 FROM {} AS p WHERE {})",
        qualified(database, &parent.name),
        conjunction(&lookup)
    ));
    let target = qualified(database, &child.name);
    let where_clause = conjunction(&guards);
    match key.on_delete {
        ReferentialAction::Cascade => Some(format!("DELETE FROM {target} WHERE {where_clause}")),
        ReferentialAction::SetNull => {
            let sets: Vec<String> = near
                .iter()
                .map(|column| format!("{} = NULL", quote(column)))
                .collect();
            Some(format!(
                "UPDATE {target} SET {} WHERE {where_clause}",
                sets.join(", ")
            ))
        }
        ReferentialAction::SetDefault => {
            let mut sets = Vec::with_capacity(near.len());
            for (position, column) in key.columns.iter().zip(near.iter()) {
                let default = child
                    .columns
                    .get(usize::from(*position))
                    .and_then(|info| info.default_sql.clone())
                    .unwrap_or_else(|| b"NULL".to_vec());
                sets.push(format!(
                    "{} = ({})",
                    quote(column),
                    String::from_utf8_lossy(&default)
                ));
            }
            Some(format!(
                "UPDATE {target} SET {} WHERE {where_clause}",
                sets.join(", ")
            ))
        }
        ReferentialAction::NoAction | ReferentialAction::Restrict => None,
    }
}

/// Builds the entries for one table, both directions.
fn plan_table(
    table: &TableInfo,
    tables: &[TableInfo],
    database: &[u8],
    limits: &Limits,
) -> Vec<ForeignKeyTrigger> {
    let mut planned = Vec::new();
    for key in &table.foreign_keys {
        let parent = tables
            .iter()
            .find(|candidate| candidate.folded == key.parent_folded);
        let Some(parent) = parent else {
            planned.push(unusable(
                key,
                format!(
                    "no such table: {}.{}",
                    String::from_utf8_lossy(database),
                    String::from_utf8_lossy(&key.parent)
                ),
                true,
            ));
            continue;
        };
        if !parent_key_is_unique(parent, key) {
            planned.push(unusable(key, mismatch(table, parent), true));
            continue;
        }
        for event in [ForeignKeyEvent::ChildInsert, ForeignKeyEvent::ChildUpdate] {
            if let Some(trigger) = trigger_for(table, parent, key, event, database, false, limits) {
                planned.push(ForeignKeyTrigger {
                    is_check: true,
                    deferred: key.is_deferred(),
                    trigger: Some(trigger),
                    fault: Vec::new(),
                });
            }
        }
    }
    for child in tables {
        if child.kind != TableKind::Table {
            continue;
        }
        for key in &child.foreign_keys {
            if key.parent_folded != table.folded {
                continue;
            }
            if !parent_key_is_unique(table, key) {
                planned.push(unusable(key, mismatch(child, table), false));
                continue;
            }
            for event in [ForeignKeyEvent::ParentDelete, ForeignKeyEvent::ParentUpdate] {
                let Some(trigger) = trigger_for(child, table, key, event, database, false, limits)
                else {
                    continue;
                };
                let action = match event {
                    ForeignKeyEvent::ParentDelete => key.on_delete,
                    _ => key.on_update,
                };
                planned.push(ForeignKeyTrigger {
                    // RESTRICT refuses, and is never deferred; NO ACTION
                    // refuses and is deferred with its key; the three that
                    // repair are not checks at all.
                    is_check: action == ReferentialAction::NoAction,
                    deferred: key.is_deferred(),
                    trigger: Some(trigger),
                    fault: Vec::new(),
                });
            }
        }
    }
    planned
}

/// Returns the message SQLite reports for a key whose parent does not match.
fn mismatch(child: &TableInfo, parent: &TableInfo) -> String {
    format!(
        "foreign key mismatch - \"{}\" referencing \"{}\"",
        String::from_utf8_lossy(&child.name),
        String::from_utf8_lossy(&parent.name)
    )
}

/// Returns an entry that reports a fault instead of enforcing anything.
fn unusable(key: &ForeignKeyInfo, message: String, is_check: bool) -> ForeignKeyTrigger {
    ForeignKeyTrigger {
        is_check,
        deferred: key.is_deferred(),
        trigger: None,
        fault: message.into_bytes(),
    }
}

/// Reports whether a key's parent columns are a key of the parent.
///
/// SQLite requires it: the parent columns must be the primary key or carry a
/// UNIQUE index, because a key that could match two parent rows would make
/// `ON DELETE CASCADE` ambiguous. A parent that does not satisfy it is a
/// `foreign key mismatch`, reported when something writes.
pub fn parent_key_is_unique(parent: &TableInfo, key: &ForeignKeyInfo) -> bool {
    let Some(wanted) = parent_columns(key, parent) else {
        return false;
    };
    let folded: Vec<Vec<u8>> = wanted
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    // A single column that is the rowid alias is the table's own key.
    if folded.len() == 1 {
        if let Some(alias) = parent.rowid_alias {
            if let Some(column) = parent.columns.get(usize::from(alias)) {
                if folded.first() == Some(&column.folded) {
                    return true;
                }
            }
        }
    }
    let primary = parent.primary_key();
    if !primary.is_empty() && primary.len() == folded.len() {
        let names: Vec<Vec<u8>> = primary
            .iter()
            .filter_map(|position| parent.columns.get(usize::from(*position)))
            .map(|column| column.folded.clone())
            .collect();
        if same_set(&names, &folded) {
            return true;
        }
    }
    parent.indexes.iter().any(|index| {
        index.unique && index.columns.len() == folded.len() && {
            let names: Vec<Vec<u8>> = index
                .columns
                .iter()
                .filter_map(|key| key.column)
                .filter_map(|position| parent.columns.get(usize::from(position)))
                .map(|column| column.folded.clone())
                .collect();
            same_set(&names, &folded)
        }
    })
}

/// Reports whether two column lists name the same columns, in any order.
///
/// Order does not matter to a key: `REFERENCES p(a, b)` is satisfied by a
/// unique index on `(b, a)`, because either one makes the pair unique.
fn same_set(left: &[Vec<u8>], right: &[Vec<u8>]) -> bool {
    left.len() == right.len() && right.iter().all(|name| left.contains(name))
}

/// Returns the `SELECT` that finds every row of a child table whose key has no
/// parent, which is what `PRAGMA foreign_key_check` reports and what a deferred
/// constraint is tested with at commit.
///
/// It is a query rather than a scan written by hand, so it uses the planner and
/// the indexes an ordinary query would - a check over a million-row child with
/// an index on its key is an index lookup per row, not a second scan.
pub fn violation_query(
    child: &TableInfo,
    parent: &TableInfo,
    key: &ForeignKeyInfo,
    database: &[u8],
) -> Option<String> {
    let near = child_columns(key, child)?;
    let far = parent_columns(key, parent)?;
    if near.len() != far.len() || near.is_empty() {
        return None;
    }
    let mut guards: Vec<String> = near
        .iter()
        .map(|column| format!("c.{} IS NOT NULL", quote(column)))
        .collect();
    let lookup: Vec<String> = far
        .iter()
        .zip(near.iter())
        .map(|(parent_column, child_column)| {
            format!("p.{} = c.{}", quote(parent_column), quote(child_column))
        })
        .collect();
    guards.push(format!(
        "NOT EXISTS (SELECT 1 FROM {} AS p WHERE {})",
        qualified(database, &parent.name),
        conjunction(&lookup)
    ));
    // A WITHOUT ROWID table has no rowid to report, and SQLite prints NULL
    // for it rather than refusing to check the table.
    let identity = if child.without_rowid {
        "NULL"
    } else {
        "c.rowid"
    };
    Some(format!(
        "SELECT {identity} FROM {} AS c WHERE {}",
        qualified(database, &child.name),
        conjunction(&guards)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_view::{ColumnInfo, TableKind};
    use inillucent_value::Affinity;

    /// Builds a table with the named columns, for the generator tests.
    fn table(name: &[u8], columns: &[&[u8]]) -> TableInfo {
        TableInfo {
            name: name.to_vec(),
            folded: name.to_ascii_lowercase(),
            database: 0,
            root: 2,
            columns: columns
                .iter()
                .map(|column| ColumnInfo {
                    name: column.to_vec(),
                    folded: column.to_ascii_lowercase(),
                    declared_type: Vec::new(),
                    affinity: Affinity::Blob,
                    collation: b"binary".to_vec(),
                    not_null: false,
                    not_null_conflict: None,
                    primary_key_conflict: None,
                    default_sql: None,
                    primary_key_position: None,
                    hidden: false,
                    generated: false,
                    stored: false,
                    generated_sql: None,
                })
                .collect(),
            rowid_alias: None,
            without_rowid: false,
            strict: false,
            autoincrement: false,
            kind: TableKind::Table,
            create_sql: Vec::new(),
            indexes: Vec::new(),
            view: None,
            triggers: Vec::new(),
            analysed_rows: None,
            checks: Vec::new(),
            foreign_keys: Vec::new(),
            foreign_key_triggers: Vec::new(),
            module: None,
        }
    }

    /// Builds a key over one child column pointing at one parent column.
    fn key(on_delete: ReferentialAction, on_update: ReferentialAction) -> ForeignKeyInfo {
        ForeignKeyInfo {
            id: 0,
            columns: vec![1],
            parent: b"p".to_vec(),
            parent_folded: b"p".to_vec(),
            parent_columns: vec![b"id".to_vec()],
            on_delete,
            on_update,
            match_clause: Vec::new(),
            deferrable: false,
            initially_deferred: false,
            cyclic: false,
        }
    }

    /// Every generated trigger has to parse. A generator that produced text the
    /// parser refuses would enforce nothing at all, silently.
    #[test]
    fn every_generated_trigger_parses() {
        let child = table(b"c", &[b"id", b"pid"]);
        let parent = table(b"p", &[b"id"]);
        let limits = Limits::default();
        let actions = [
            ReferentialAction::NoAction,
            ReferentialAction::Restrict,
            ReferentialAction::Cascade,
            ReferentialAction::SetNull,
            ReferentialAction::SetDefault,
        ];
        let events = [
            ForeignKeyEvent::ChildInsert,
            ForeignKeyEvent::ChildUpdate,
            ForeignKeyEvent::ParentDelete,
            ForeignKeyEvent::ParentUpdate,
        ];
        for action in actions {
            let key = key(action, action);
            for event in events {
                let built = trigger_for(&child, &parent, &key, event, b"main", false, &limits);
                assert!(
                    built.is_some(),
                    "{action:?} on {event:?} produced no trigger"
                );
            }
        }
    }

    /// The child's check fires before the write, tests every key column for
    /// NULL, and looks the parent up by the columns the clause named.
    #[test]
    fn the_child_check_reads_as_it_should() {
        let child = table(b"c", &[b"id", b"pid"]);
        let parent = table(b"p", &[b"id"]);
        let key = key(ReferentialAction::NoAction, ReferentialAction::NoAction);
        let sql = child_check(
            &child,
            &parent,
            &key,
            ForeignKeyEvent::ChildInsert,
            b"main",
            &[b"pid".to_vec()],
            &[b"id".to_vec()],
        );
        assert!(sql.contains("BEFORE INSERT ON \"c\""), "{sql}");
        assert!(sql.contains("NEW.\"pid\" IS NOT NULL"), "{sql}");
        assert!(sql.contains("NOT EXISTS"), "{sql}");
        assert!(sql.contains("FOREIGN KEY constraint failed"), "{sql}");
    }

    /// RESTRICT fires before the parent write and NO ACTION after it, which is
    /// the one place the two differ.
    #[test]
    fn restrict_fires_before_and_no_action_after() {
        let child = table(b"c", &[b"id", b"pid"]);
        let parent = table(b"p", &[b"id"]);
        let limits = Limits::default();
        for (action, expected) in [
            (ReferentialAction::Restrict, TriggerTime::Before),
            (ReferentialAction::NoAction, TriggerTime::After),
        ] {
            let key = key(action, action);
            let built = trigger_for(
                &child,
                &parent,
                &key,
                ForeignKeyEvent::ParentDelete,
                b"main",
                false,
                &limits,
            )
            .expect("the trigger is generated");
            assert_eq!(built.time, expected, "{action:?}");
        }
    }

    /// A deferred constraint generates no check on the child and no NO ACTION
    /// on the parent - both wait for the commit - but RESTRICT and the cascades
    /// still fire where they are.
    #[test]
    fn a_deferred_key_defers_only_its_checks() {
        let child = table(b"c", &[b"id", b"pid"]);
        let parent = table(b"p", &[b"id"]);
        let limits = Limits::default();
        let deferred = key(ReferentialAction::NoAction, ReferentialAction::NoAction);
        assert!(trigger_for(
            &child,
            &parent,
            &deferred,
            ForeignKeyEvent::ChildInsert,
            b"main",
            true,
            &limits
        )
        .is_none());
        assert!(trigger_for(
            &child,
            &parent,
            &deferred,
            ForeignKeyEvent::ParentDelete,
            b"main",
            true,
            &limits
        )
        .is_none());
        let restrict = key(ReferentialAction::Restrict, ReferentialAction::Restrict);
        assert!(trigger_for(
            &child,
            &parent,
            &restrict,
            ForeignKeyEvent::ParentDelete,
            b"main",
            true,
            &limits
        )
        .is_some());
        let cascade = key(ReferentialAction::Cascade, ReferentialAction::Cascade);
        assert!(trigger_for(
            &child,
            &parent,
            &cascade,
            ForeignKeyEvent::ParentDelete,
            b"main",
            true,
            &limits
        )
        .is_some());
    }

    /// A parent update fires only when the key actually changed, and cascades
    /// the new key onto the rows that carried the old one.
    #[test]
    fn a_parent_update_guards_on_the_key_changing() {
        let child = table(b"c", &[b"id", b"pid"]);
        let parent = table(b"p", &[b"id"]);
        let key = key(ReferentialAction::Cascade, ReferentialAction::Cascade);
        let sql = parent_action(
            &child,
            &parent,
            &key,
            ForeignKeyEvent::ParentUpdate,
            b"main",
            &[b"pid".to_vec()],
            &[b"id".to_vec()],
            false,
        )
        .expect("the trigger is generated");
        assert!(sql.contains("AFTER UPDATE OF \"id\""), "{sql}");
        assert!(sql.contains("WHEN OLD.\"id\" IS NOT NEW.\"id\""), "{sql}");
        assert!(sql.contains("SET \"pid\" = NEW.\"id\""), "{sql}");
        assert!(sql.contains("WHERE \"pid\" = OLD.\"id\""), "{sql}");
    }

    /// An identifier with a quote in it survives the round trip, because the
    /// generated text is parsed again rather than merely printed.
    #[test]
    fn an_awkward_identifier_is_quoted() {
        assert_eq!(quote(b"we\"ird"), "\"we\"\"ird\"");
        let child = table(b"we\"ird", &[b"id", b"pid"]);
        let parent = table(b"p", &[b"id"]);
        let key = key(ReferentialAction::Cascade, ReferentialAction::Cascade);
        let limits = Limits::default();
        assert!(trigger_for(
            &child,
            &parent,
            &key,
            ForeignKeyEvent::ChildInsert,
            b"main",
            false,
            &limits
        )
        .is_some());
    }

    /// A composite key compares every column, in the order the clause wrote.
    #[test]
    fn a_composite_key_compares_every_column() {
        let matching = children_of(
            &[b"a".to_vec(), b"b".to_vec()],
            &[b"x".to_vec(), b"y".to_vec()],
            "OLD",
        );
        assert_eq!(matching, "\"a\" = OLD.\"x\" AND \"b\" = OLD.\"y\"");
    }
}
