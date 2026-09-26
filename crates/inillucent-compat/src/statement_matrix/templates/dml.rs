//! The write families: `insert`, `update` and `delete`.
//!
//! Invariant: **every write is followed by a read of the whole target, and a
//! case that writes is always reopened and checked with `PRAGMA
//! integrity_check`, so a write that answered correctly and stored something
//! else still fails.** The target axis puts the write on every kind of table a
//! caller writes to, including a view with `INSTEAD OF` triggers and an FTS5
//! table, because a write lost in a virtual table and `INSERT ... SELECT` into
//! FTS5 were both escaped defects (section 1.1).
//!
//! `UPDATE ... FROM` joins a table whose key repeats, so one target row has
//! several join rows. It sets a constant, since which join row SQLite applies
//! is not defined, and DQE checks each target row changed once, which is the
//! defect `106b304f` fixed.

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::properties::Property;
use crate::statement_matrix::templates::frame::{constant, record, Constant, BINDINGS};
use crate::statement_matrix::templates::scene::{relation, Relation, AFFINITIES, DATA};
use crate::statement_matrix::templates::{Axis, Family, Pick};

/// The kinds of table a write goes to.
const TARGETS: &[&str] = &[
    "rowid",
    "ipk",
    "without_rowid",
    "strict",
    "temp",
    "attached",
    "fts5",
    "view",
];

/// The access paths a write target takes: the indexes a write maintains.
const WRITE_ACCESS: &[&str] = &[
    "none",
    "index",
    "unique",
    "partial",
    "expression",
    "collate",
];

/// The conflict actions.
const CONFLICTS: &[&str] = &["none", "ROLLBACK", "ABORT", "FAIL", "IGNORE", "REPLACE"];

/// The transaction states of section 4.2.
pub const TRANSACTIONS: &[&str] = &["autocommit", "commit", "rollback", "savepoint_rollback"];

/// The write target, as the templates refer to it.
struct Target {
    scene: Relation,
    /// The name a write names.
    table: String,
    /// The key column's name: `k`, or `rowid` for FTS5.
    key: &'static str,
    /// Whether the target is an ordinary table DQE can check.
    plain: bool,
}

/// Builds a write target with its rows and a marker column DQE sets.
fn target(kind: &str, access: &str, data: &str, affinity: &str) -> Target {
    let scene_kind = match kind {
        "view" => "rowid",
        other => other,
    };
    let mut scene = relation(scene_kind, access, data, affinity);
    let mut table = scene.table.clone().unwrap_or_else(|| "t0".to_string());
    let plain = !matches!(kind, "fts5" | "view");
    if plain {
        let marker = Record::ok(format!(
            "ALTER TABLE {table} ADD COLUMN m INTEGER DEFAULT 0"
        ));
        if scene.prelude.is_empty() {
            scene.setup.push(marker);
        } else {
            scene.prelude.push(marker);
        }
    }
    if kind == "view" {
        let base = table.clone();
        scene.setup.extend([
            Record::ok(format!("CREATE VIEW v0 AS SELECT k, a, b FROM {base}")),
            Record::ok(format!("CREATE TRIGGER vi INSTEAD OF INSERT ON v0 BEGIN INSERT INTO {base}(k, a, b) VALUES (NEW.k, NEW.a, NEW.b); END")),
            Record::ok(format!("CREATE TRIGGER vu INSTEAD OF UPDATE ON v0 BEGIN UPDATE {base} SET a = NEW.a, b = NEW.b WHERE k = OLD.k; END")),
            Record::ok(format!("CREATE TRIGGER vd INSTEAD OF DELETE ON v0 BEGIN DELETE FROM {base} WHERE k = OLD.k; END")),
        ]);
        table = "v0".to_string();
    }
    Target {
        scene,
        table,
        key: if kind == "fts5" { "rowid" } else { "k" },
        plain,
    }
}

/// The table a write reads from or joins, whose key repeats.
fn source_table() -> Vec<Record> {
    vec![
        Record::ok("CREATE TABLE r(k INTEGER, a, w TEXT)"),
        Record::ok("INSERT INTO r VALUES (1, 10, 'x'), (1, 11, 'y'), (2, 20, 'z'), (3, NULL, 'w'), (9, 90, 'v')"),
    ]
}

/// The axes every write family takes.
fn write_axes(own: Vec<Axis>) -> Vec<Axis> {
    let mut axes = own;
    axes.extend([
        Axis::new("target", TARGETS),
        Axis::new("access", WRITE_ACCESS),
        Axis::new("data", DATA),
        Axis::new("affinity", AFFINITIES),
        Axis::new("binding", BINDINGS),
        Axis::new("transaction", TRANSACTIONS),
        Axis::new("returning", &["none", "returning"]),
    ]);
    axes
}

/// The constraint every write family shares.
fn write_allowed(pick: &Pick) -> bool {
    if pick.forbids(
        "target",
        "access",
        &[
            ("fts5", "index"),
            ("fts5", "unique"),
            ("fts5", "partial"),
            ("fts5", "expression"),
            ("fts5", "collate"),
        ],
    ) {
        return false;
    }
    if pick.forbids("target", "data", &[("fts5", "mixed"), ("fts5", "limits")]) {
        return false;
    }
    true
}

/// Assembles a write case: the target, the transaction around the write, the
/// write, and a read of the whole target afterwards.
fn write_case(pick: &Pick, write: &dyn Fn(&Target, &Constant) -> Option<String>) -> Option<Case> {
    let target = target(
        pick.value("target"),
        pick.value("access"),
        pick.value("data"),
        pick.value("affinity"),
    );
    let c = constant(pick.value("binding"), "2", "9");
    let mut sql = write(&target, &c)?;
    if pick.value("returning") == "returning" {
        sql.push_str(&format!(" RETURNING {}, a", target.key));
    }
    let mut case = Case::new("", "");
    if pick.value("target") == "view" {
        case.capabilities.push("writing_to_a_view".to_string());
    }
    case.setup = source_table();
    case.setup.extend(target.scene.setup.clone());
    case.records = target.scene.prelude.clone();
    let (open, close): (Vec<&str>, Vec<&str>) = match pick.value("transaction") {
        "commit" => (vec!["BEGIN"], vec!["COMMIT"]),
        "rollback" => (vec!["BEGIN"], vec!["ROLLBACK"]),
        "savepoint_rollback" => (vec!["SAVEPOINT w"], vec!["ROLLBACK TO w", "RELEASE w"]),
        _ => (vec![], vec![]),
    };
    case.records.extend(open.into_iter().map(Record::ok));
    case.records.push(record(sql, Sort::RowSort, &c.binds));
    // Graded rather than expected to succeed: a write refused with
    // `OR ROLLBACK` has already ended the transaction, and the COMMIT or
    // ROLLBACK after it then fails on both engines.
    case.records.extend(
        close
            .into_iter()
            .map(|statement| record(statement.to_string(), Sort::RowSort, &[])),
    );
    case.records.push(record(
        format!("SELECT {}, a, b FROM {}", target.key, target.table),
        Sort::RowSort,
        &[],
    ));
    if target.plain {
        dqe(&mut case, &target, pick);
    }
    Some(case)
}

/// Adds the DQE property for a plain table target.
fn dqe(case: &mut Case, target: &Target, pick: &Pick) {
    let key = target.scene.key.clone();
    case.properties.push(Property::Dqe {
        name: "DQE".to_string(),
        table: target.table.clone(),
        key: key.clone(),
        marker: "m".to_string(),
        from: None,
        predicate: "a > 1 OR b IS NULL".to_string(),
    });
    if pick.get("form") == Some("from_table") {
        case.properties.push(Property::Dqe {
            name: "DQE of UPDATE ... FROM".to_string(),
            table: target.table.clone(),
            key,
            marker: "m".to_string(),
            from: Some("r".to_string()),
            predicate: format!("r.k = {}.k", target.table),
        });
    }
}

/// The conflict clause for `INSERT OR` and `UPDATE OR`.
fn or_clause(conflict: &str) -> String {
    match conflict {
        "none" => String::new(),
        other => format!(" OR {other}"),
    }
}

/// Every write family.
pub fn families() -> Vec<Family> {
    vec![insert(), update(), delete()]
}

/// `insert`: every input form, every conflict action, and upserts.
fn insert() -> Family {
    Family {
        name: "insert",
        axes: write_axes(vec![
            Axis::new(
                "form",
                &[
                    "values_one",
                    "values_many",
                    "select_table",
                    "select_derived",
                    "select_cte",
                    "select_tvf",
                    "default_values",
                    "upsert_update",
                    "upsert_nothing",
                    "upsert_two",
                ],
            ),
            Axis::new("conflict", CONFLICTS),
        ]),
        allowed: |pick| {
            write_allowed(pick)
                && !(pick
                    .get("form")
                    .is_some_and(|form| form.starts_with("upsert"))
                    && pick
                        .get("conflict")
                        .is_some_and(|conflict| conflict != "none"))
        },
        build: |pick| {
            write_case(pick, &|target, c| {
                let key = target.key;
                let table = &target.table;
                let or = or_clause(pick.value("conflict"));
                let head = format!("INSERT{or} INTO {table}({key}, a, b)");
                Some(match pick.value("form") {
                    "values_one" => format!("{head} VALUES ({c}, 'new', 'n')", c = c.text),
                    "values_many" => format!("{head} VALUES ({c}, 'one', 'n'), (8, 'two', NULL), (1, 3, 'x')", c = c.text),
                    "select_table" => format!("{head} SELECT r.k, r.a, r.w FROM r WHERE r.k >= {}", c.text),
                    "select_derived" => format!("{head} SELECT d.k + 10, d.a, d.w FROM (SELECT k, a, w FROM r WHERE k >= {}) AS d", c.text),
                    "select_cte" => format!("WITH d AS (SELECT k, a, w FROM r WHERE k >= {}) {head} SELECT k, a, w FROM d", c.text),
                    "select_tvf" => format!("{head} SELECT j.key + 20, j.value, j.type FROM json_each(json_array(1, 'x', {})) AS j", c.text),
                    "default_values" => format!("INSERT{or} INTO {table} DEFAULT VALUES"),
                    "upsert_update" => format!("{head} VALUES ({}, 'up', 'u') ON CONFLICT DO UPDATE SET a = excluded.a WHERE {table}.a IS NOT excluded.a", c.text),
                    "upsert_nothing" => format!("{head} VALUES ({}, 'up', 'u') ON CONFLICT DO NOTHING", c.text),
                    _ => format!("{head} VALUES ({}, 'up', 'u') ON CONFLICT ({key}) DO UPDATE SET b = 'first' ON CONFLICT DO NOTHING", c.text),
                })
            })
        },
    }
}

/// `update`: one and several columns, row values, `FROM` sources, order and
/// limit, and a subquery value.
fn update() -> Family {
    Family {
        name: "update",
        axes: write_axes(vec![
            Axis::new(
                "form",
                &[
                    "set_one",
                    "set_many",
                    "row_value",
                    "from_table",
                    "from_derived",
                    "from_tvf",
                    "order_limit",
                    "subquery_set",
                ],
            ),
            Axis::new("conflict", CONFLICTS),
        ]),
        allowed: |pick| {
            write_allowed(pick)
                && !pick.forbids(
                    "form",
                    "target",
                    &[
                        ("from_table", "fts5"),
                        ("from_derived", "fts5"),
                        ("from_tvf", "fts5"),
                    ],
                )
        },
        build: |pick| {
            let form = pick.value("form");
            write_case(pick, &|target, c| {
                let table = &target.table;
                let key = target.key;
                let or = or_clause(pick.value("conflict"));
                let head = format!("UPDATE{or} {table}");
                let c = &c.text;
                Some(match form {
                    "set_many" => format!("{head} SET a = {c}, b = 'z' WHERE a > 1 OR b IS NULL"),
                    "row_value" => format!("{head} SET (a, b) = ({c}, 'z') WHERE {key} > 1"),
                    "from_table" => format!("{head} SET b = 'hit' FROM r WHERE r.k = {table}.{key} AND r.a > {c}"),
                    "from_derived" => format!("{head} SET b = 'hit' FROM (SELECT k FROM r WHERE a > {c}) AS d WHERE d.k = {table}.{key}"),
                    "from_tvf" => format!("{head} SET b = 'hit' FROM json_each(json_array(1, {c}, 3)) AS j WHERE j.value = {table}.{key}"),
                    "order_limit" => format!("{head} SET b = 'lim' WHERE {key} > 0 ORDER BY {key} DESC LIMIT {c}"),
                    "subquery_set" => format!("{head} SET a = (SELECT max(r.a) FROM r WHERE r.k = {table}.{key}) WHERE {key} <= {c}"),
                    _ => format!("{head} SET a = {c} WHERE a > 1 OR a IS NULL"),
                })
            })
        },
    }
}

/// `delete`: a filter, everything, order and limit, and subqueries.
fn delete() -> Family {
    Family {
        name: "delete",
        axes: write_axes(vec![Axis::new(
            "form",
            &["where", "all", "order_limit", "in_subquery", "exists"],
        )]),
        allowed: write_allowed,
        build: |pick| {
            write_case(pick, &|target, c| {
                let table = &target.table;
                let key = target.key;
                let c = &c.text;
                Some(match pick.value("form") {
                    "all" => format!("DELETE FROM {table}"),
                    "order_limit" => format!("DELETE FROM {table} WHERE {key} > 0 ORDER BY {key} LIMIT {c}"),
                    "in_subquery" => format!("DELETE FROM {table} WHERE {key} IN (SELECT r.k FROM r WHERE r.a > {c})"),
                    "exists" => format!("DELETE FROM {table} WHERE EXISTS (SELECT 1 FROM r WHERE r.k = {table}.{key} AND r.a > {c})"),
                    _ => format!("DELETE FROM {table} WHERE a > {c} OR a IS NULL"),
                })
            })
        },
    }
}
