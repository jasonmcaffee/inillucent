//! Merging the setups of read only cases into one fixture.
//!
//! Invariant: **a case joins a merged fixture only when nothing another member
//! added can change what the case reads.** Every statement a case adds is
//! either already in the fixture word for word, or creates a name nobody else
//! created, or writes to, indexes or triggers on a table this case itself
//! created. A statement that would put an index, a row or statistics on a
//! table another member reads makes the case start a fixture of its own,
//! because an index is an access path and the access path is one of the
//! things a case is about.
//!
//! Measured in phase 3: the `expression` family's 696 change cases built 248
//! fixtures at about 140 ms each, which was three quarters of its time. The
//! templates give every scene's objects names of their own
//! (`scene::scene_tag`), so most of a family's read cases fit in one fixture.

use std::collections::{BTreeMap, BTreeSet};

use crate::statement_matrix::case::{Case, Record};

/// The most cases one merged fixture serves. The cap keeps a fixture's build
/// short and a failure in one member's setup from reaching thousands of cases.
pub const MOST_MEMBERS: usize = 200;

/// A merged fixture and the cases it serves.
#[derive(Clone, Debug, Default)]
pub struct Union {
    /// The setup, in the order the members added it.
    pub setup: Vec<Record>,
    /// The positions of the member cases.
    pub members: Vec<usize>,
    /// Whether the oracle grades these cases.
    pub oracle: bool,
    /// Every member's capability rows, which the setup may need.
    pub capabilities: Vec<String>,
    seen: BTreeSet<String>,
    names: BTreeMap<String, String>,
    closed: bool,
}

/// What one setup statement does to the schema.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Effect {
    /// Creates a name; for an index or a trigger, on a table.
    Create { name: String, on: Option<String> },
    /// Writes to, or gathers statistics for, a table; `None` for all of them.
    Touch(Option<String>),
    /// Anything else, which only a case alone may run.
    Other,
}

/// Reads what a setup statement does.
fn effect(sql: &str) -> Effect {
    let upper = sql.to_ascii_uppercase();
    let words: Vec<&str> = upper
        .split(|c: char| c.is_whitespace() || c == '(')
        .filter(|word| !word.is_empty())
        .collect();
    let original: Vec<&str> = sql
        .split(|c: char| c.is_whitespace() || c == '(')
        .filter(|word| !word.is_empty())
        .collect();
    let at = |index: usize| original.get(index).map(|word| word.to_ascii_lowercase());
    match words.first().copied() {
        Some("CREATE") => {
            let mut index = 1usize;
            while matches!(
                words.get(index).copied(),
                Some("TEMP" | "TEMPORARY" | "UNIQUE" | "VIRTUAL")
            ) {
                index += 1;
            }
            let kind = words.get(index).copied().unwrap_or("");
            index += 1;
            if words.get(index) == Some(&"IF") {
                index += 3;
            }
            let Some(name) = at(index) else {
                return Effect::Other;
            };
            let on = match kind {
                "INDEX" | "TRIGGER" => words
                    .iter()
                    .position(|word| *word == "ON")
                    .and_then(|position| at(position + 1)),
                _ => None,
            };
            Effect::Create { name, on }
        }
        Some("INSERT") => {
            let position = words.iter().position(|word| *word == "INTO").map(|p| p + 1);
            Effect::Touch(position.and_then(at))
        }
        Some("ANALYZE") => Effect::Touch(at(1)),
        Some("ALTER") => Effect::Touch(at(2)),
        Some("BEGIN") | Some("COMMIT") => Effect::Touch(None),
        _ => Effect::Other,
    }
}

impl Union {
    /// Adds a case if it fits, returning whether it did.
    ///
    /// @param case - the case
    /// @param position - its position in the runner's list
    pub fn admit(&mut self, case: &Case, position: usize) -> bool {
        if self.closed
            || self.members.len() >= MOST_MEMBERS
            || (!self.members.is_empty() && self.oracle != case.oracle)
        {
            return false;
        }
        let mut added: Vec<Record> = Vec::new();
        let mut created: BTreeMap<String, String> = BTreeMap::new();
        for record in &case.setup {
            let Some(sql) = record.sql() else {
                return false;
            };
            if self.seen.contains(sql) {
                continue;
            }
            let fits = match effect(sql) {
                Effect::Create { name, on } => {
                    let clashes = self.names.contains_key(&name) || created.contains_key(&name);
                    let own_target = on.as_ref().is_none_or(|table| created.contains_key(table));
                    if !clashes && own_target {
                        created.insert(name, sql.to_string());
                        true
                    } else {
                        false
                    }
                }
                Effect::Touch(Some(table)) => created.contains_key(&table),
                Effect::Touch(None) | Effect::Other => self.members.is_empty() && added.is_empty(),
            };
            if !fits {
                return false;
            }
            added.push(record.clone());
        }
        for record in &added {
            if let Some(sql) = record.sql() {
                self.seen.insert(sql.to_string());
            }
        }
        self.setup.extend(added);
        self.names.extend(created);
        if self.members.is_empty() {
            self.oracle = case.oracle;
        }
        for capability in &case.capabilities {
            if !self.capabilities.contains(capability) {
                self.capabilities.push(capability.clone());
            }
        }
        self.members.push(position);
        true
    }
}

/// Whether a case reads something that describes the whole database rather
/// than its own objects: the schema tables, the table list, statistics, page
/// counts. Its answer would then depend on which other cases share its
/// fixture, so it never shares one.
///
/// @param case - the case
pub fn reads_whole_database(case: &Case) -> bool {
    const WHOLE: &[&str] = &[
        "sqlite_schema",
        "sqlite_master",
        "sqlite_temp",
        "table_list",
        "sqlite_stat",
        "page_count",
        "freelist_count",
        "database_list",
        "schema_version",
        "dbstat",
        "integrity_check",
        "quick_check",
        "function_list",
        "module_list",
        "pragma_list",
    ];
    case.records.iter().filter_map(Record::sql).any(|sql| {
        let lower = sql.to_ascii_lowercase();
        WHOLE.iter().any(|name| lower.contains(name))
    })
}

/// Groups read only cases into merged fixtures, first fit, in order.
///
/// @param cases - every case the runner has
/// @param positions - the positions of the cases to group
pub fn group(cases: &[&Case], positions: &[usize]) -> Vec<Union> {
    let mut unions: Vec<Union> = Vec::new();
    for position in positions {
        let Some(case) = cases.get(*position) else {
            continue;
        };
        if reads_whole_database(case) {
            let mut alone = Union::default();
            if alone.admit(case, *position) {
                alone.closed = true;
                unions.push(alone);
            }
            continue;
        }
        if !unions.iter_mut().any(|union| union.admit(case, *position)) {
            let mut fresh = Union::default();
            if fresh.admit(case, *position) {
                unions.push(fresh);
            }
        }
    }
    unions
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A case whose setup creates new names joins; one that indexes another
    /// member's table does not.
    #[test]
    fn a_case_joins_only_when_it_changes_nothing_another_reads() {
        let mut first = Case::new("f", "t");
        first.setup = vec![
            Record::ok("CREATE TABLE t_1(a)"),
            Record::ok("INSERT INTO t_1 VALUES (1)"),
        ];
        let mut second = Case::new("f", "t");
        second.setup = vec![
            Record::ok("CREATE TABLE t_2(a)"),
            Record::ok("CREATE INDEX i_2 ON t_2(a)"),
        ];
        let mut third = Case::new("f", "t");
        third.setup = vec![
            Record::ok("CREATE TABLE t_1(a)"),
            Record::ok("CREATE INDEX i_1 ON t_1(a)"),
        ];
        let mut fourth = Case::new("f", "t");
        fourth.setup = vec![Record::ok("CREATE TABLE t_1(a, b)")];
        let cases = vec![&first, &second, &third, &fourth];
        let unions = group(&cases, &[0, 1, 2, 3]);
        assert_eq!(unions.len(), 3, "{unions:#?}");
        assert_eq!(
            unions.first().map(|union| union.members.clone()),
            Some(vec![0, 1])
        );
        assert_eq!(unions.first().map(|union| union.setup.len()), Some(4));
    }
}
