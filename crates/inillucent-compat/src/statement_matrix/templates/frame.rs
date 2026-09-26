//! Where a generated query sits (the placement axis), how its constant reaches
//! it (the binding axis), and the Layer 3 wrappings and properties every
//! generated query carries (section 5.3 and 6.3 of the design).
//!
//! Invariant: **a query placed anywhere asks the same question it asks at the
//! top level, and every wrapping added by [`layer_three`] keeps its meaning.**
//! That is what lets the wrappings be graded against each other with no
//! oracle: if the top level and the derived table answer differently, one of
//! them is wrong, whatever SQLite says. The outer filter wrapping is the
//! defect `f593b836` fixed, exactly: a `WHERE` on a derived table's column
//! must select the rows the same predicate selects inside it.
//!
//! A generated query names its result columns `c1`, `c2` and so on, so a
//! placement can refer to them without knowing what they are.

use crate::statement_matrix::case::{Case, Record, Sort};
use crate::statement_matrix::properties::Property;
use crate::statement_matrix::templates::scene::Relation;

/// The placements of section 4.2 that a read takes. `RETURNING` is a
/// placement only the write families take.
pub const PLACEMENTS: &[&str] = &[
    "top",
    "derived",
    "cte",
    "view",
    "in",
    "exists",
    "scalar",
    "trigger",
    "update_from",
    "insert_select",
];

/// How the constant in a predicate reaches the statement.
pub const BINDINGS: &[&str] = &["literal", "positional", "named", "reuse"];

/// Whether a placement goes with a source and a binding.
///
/// @param placement - the placement, if chosen
/// @param source - the source kind, if chosen
/// @param binding - the binding, if chosen
pub fn placement_allowed(
    placement: Option<&str>,
    source: Option<&str>,
    binding: Option<&str>,
) -> bool {
    let bound = binding.is_some_and(|one| one != "literal");
    // A view and a trigger body cannot hold a parameter.
    if bound && matches!(placement, Some("view") | Some("trigger")) {
        return false;
    }
    // SQLite does not allow a WITH clause in a trigger body.
    if placement == Some("trigger") && matches!(source, Some("cte") | Some("materialized")) {
        return false;
    }
    true
}

/// A constant written the way the binding axis says: its SQL text, and the
/// values the record binds.
#[derive(Clone, Debug, Default)]
pub struct Constant {
    /// What goes in the SQL: the literal, `?1`, or `:v`.
    pub text: String,
    /// The runs of values to bind; empty for a literal.
    pub binds: Vec<Vec<String>>,
}

/// Writes a constant for a binding.
///
/// @param binding - the binding axis value
/// @param literal - the constant as a SQL literal
/// @param again - a second value, for the run that reuses the statement
pub fn constant(binding: &str, literal: &str, again: &str) -> Constant {
    match binding {
        "positional" => Constant {
            text: "?1".to_string(),
            binds: vec![vec![literal.to_string()]],
        },
        "named" => Constant {
            text: ":v".to_string(),
            binds: vec![vec![literal.to_string()]],
        },
        "reuse" => Constant {
            text: "?1".to_string(),
            binds: vec![vec![literal.to_string()], vec![again.to_string()]],
        },
        _ => Constant {
            text: literal.to_string(),
            binds: Vec::new(),
        },
    }
}

/// A generated query and what Layer 3 needs to know about it.
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// The statement, its columns named `c1` to `cN`.
    pub sql: String,
    /// How many result columns it has.
    pub columns: usize,
    /// How its rows are compared.
    pub sort: Sort,
    /// Values to bind, from [`constant`].
    pub binds: Vec<Vec<String>>,
    /// The source it reads, for the properties that rewrite its `FROM`.
    pub relation: Relation,
    /// The select list, for TLP and NoREC.
    pub select_list: String,
    /// The expression the first result column is, for the outer filter.
    pub first: String,
    /// The predicate, for TLP, NoREC and the outer filter; empty for none.
    pub predicate: String,
    /// Everything after the `WHERE`: `GROUP BY`, `ORDER BY`, `LIMIT`. TLP
    /// needs a query with none of them.
    pub tail: String,
    /// Whether the property checks may run: false for a query whose rows
    /// depend on something a wrapping changes, such as `LIMIT` without a total
    /// order.
    pub checkable: bool,
}

/// A query record for a generated statement.
///
/// @param sql - the statement
/// @param sort - how its rows are compared
/// @param binds - values to bind
pub fn record(sql: String, sort: Sort, binds: &[Vec<String>]) -> Record {
    // A template's constant is written `?1` or `:v`. A statement that does not
    // use it has no parameter to bind, and binding one is a range error that
    // says nothing about the statement.
    let uses_constant = sql.contains("?1") || sql.contains(":v");
    Record::Query {
        types: "T".to_string(),
        sort,
        expected: None,
        binds: if uses_constant {
            binds.to_vec()
        } else {
            Vec::new()
        },
        sql,
    }
}

/// The table the placements read from and write to.
pub fn u_table() -> Vec<Record> {
    vec![
        Record::ok("CREATE TABLE u(k INTEGER PRIMARY KEY, v)"),
        Record::ok("INSERT INTO u VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)"),
    ]
}

/// The column list `c1, c2, ...` for a query of `count` columns.
fn column_names(count: usize) -> String {
    (1..=count.max(1))
        .map(|at| format!("c{at}"))
        .collect::<Vec<String>>()
        .join(", ")
}

/// Puts a query in a case at a placement.
///
/// @param case - the case to fill; its setup already holds the scene
/// @param query - the query
/// @param placement - where it sits
/// @param temporary - whether the scene is in `temp` or an attached database,
///   which a view or trigger in `main` may not name
pub fn place(case: &mut Case, query: &Query, placement: &str, temporary: bool) {
    let q = &query.sql;
    let sort = query.sort;
    let binds = &query.binds;
    let temp = if temporary { "TEMP " } else { "" };
    let uses_u = matches!(
        placement,
        "in" | "exists" | "scalar" | "trigger" | "update_from"
    );
    if uses_u {
        case.setup.extend(u_table());
    }
    let mut add = |record: Record| case.records.push(record);
    match placement {
        "derived" => add(record(format!("SELECT * FROM ({q}) AS p"), sort, binds)),
        "cte" => add(record(
            format!("WITH p AS ({q}) SELECT * FROM p"),
            sort,
            binds,
        )),
        "view" => {
            // Named for the query, so views of many cases can share one
            // fixture (see `scene::scene_tag`).
            let digest = crate::hash::sha3_256_hex(q.as_bytes());
            let name = format!("pv_{}", digest.get(..8).unwrap_or(&digest));
            let create = Record::ok(format!("CREATE {temp}VIEW {name} AS {q}"));
            if temporary {
                add(create);
            } else {
                case.setup.push(create);
            }
            case.records
                .push(record(format!("SELECT * FROM {name}"), sort, &[]));
        }
        "in" => add(record(
            format!("SELECT u.k FROM u WHERE u.k IN (SELECT p.c1 FROM ({q}) AS p)"),
            Sort::RowSort,
            binds,
        )),
        "exists" => add(record(
            format!("SELECT u.k FROM u WHERE EXISTS (SELECT 1 FROM ({q}) AS p WHERE p.c1 = u.k)"),
            Sort::RowSort,
            binds,
        )),
        "scalar" => add(record(
            format!("SELECT u.k, (SELECT count(*) FROM ({q}) AS p WHERE p.c1 = u.k) FROM u"),
            Sort::RowSort,
            binds,
        )),
        "trigger" => place_in_trigger(case, q, query.columns, temp),
        "update_from" => {
            add(record(
                format!("UPDATE u SET v = p.c1 FROM ({q}) AS p WHERE p.c1 = u.k"),
                Sort::RowSort,
                binds,
            ));
            add(record("SELECT k, v FROM u".to_string(), Sort::RowSort, &[]));
        }
        "insert_select" => {
            let names = column_names(query.columns);
            add(Record::ok(format!("CREATE {temp}TABLE sink({names})")));
            add(record(
                format!("INSERT INTO sink SELECT * FROM ({q})"),
                Sort::RowSort,
                binds,
            ));
            add(record("SELECT * FROM sink".to_string(), Sort::RowSort, &[]));
        }
        _ => add(record(q.clone(), sort, binds)),
    }
}

/// Puts a query in the body of a trigger, fires it, and reads what it wrote.
fn place_in_trigger(case: &mut Case, q: &str, columns: usize, temp: &str) {
    let names = column_names(columns);
    let log = Record::ok(format!("CREATE {temp}TABLE log({names})"));
    let trigger = Record::ok(format!(
        "CREATE {temp}TRIGGER tg AFTER INSERT ON u BEGIN INSERT INTO log SELECT * FROM ({q}); END"
    ));
    if temp.is_empty() {
        case.setup.push(log);
        case.setup.push(trigger);
    } else {
        case.records.push(log);
        case.records.push(trigger);
    }
    // Graded engine against engine, not written as `statement ok`: the body
    // runs the query, and on some data (an overflow, a blob in JSON) SQLite's
    // body fails, which both engines must then do.
    case.records.push(record(
        "INSERT INTO u(k, v) VALUES (7, 70)".to_string(),
        Sort::RowSort,
        &[],
    ));
    case.records
        .push(record("SELECT * FROM log".to_string(), Sort::RowSort, &[]));
}

/// Adds the Layer 3 wrappings, graded against the oracle, and the properties
/// of section 6.3, graded on inillucent alone.
///
/// @param case - the case
/// @param query - the query the template generated
/// @param indexed - whether the scene has an index a `NOT INDEXED` can refuse
pub fn layer_three(case: &mut Case, query: &Query, indexed: bool) {
    let q = &query.sql;
    let binds = &query.binds;
    let wrappings = vec![
        format!("SELECT * FROM ({q}) AS d"),
        format!("WITH c AS ({q}) SELECT * FROM c"),
        format!("WITH c AS MATERIALIZED ({q}) SELECT * FROM c"),
        format!("SELECT * FROM ({q}) AS d UNION ALL SELECT * FROM ({q}) AS z WHERE 0"),
    ];
    for wrapped in &wrappings {
        case.records
            .push(record(wrapped.clone(), Sort::RowSort, binds));
    }
    if !query.first.is_empty() {
        case.records
            .push(record(guarded_filter(q), Sort::RowSort, binds));
    }
    if !query.checkable {
        return;
    }
    let mut same = vec![q.clone()];
    same.extend(wrappings);
    case.properties.push(Property::Same {
        name: "placement equivalence".to_string(),
        queries: same,
    });
    if binds.is_empty() {
        case.properties.push(Property::Stored {
            name: "stored and viewed".to_string(),
            query: q.clone(),
        });
    }
    add_predicate_properties(case, query, indexed);
}

/// Wraps a query so that a column fails for exactly the rows an outer `WHERE`
/// removes: `guard` overflows when `c1` is NULL, and the outer query keeps
/// only the rows where `c1` is not NULL.
///
/// **The shape of the defect `f593b836` fixed, which no other wrapping had.**
/// SQLite copies a `WHERE` on a derived table's column into the derived table,
/// so the rows it removes never compute `guard`; before the fix inillucent
/// computed every column of every row first and failed. The answer is graded
/// against SQLite, so where SQLite cannot copy the filter in (a `LIMIT` inside,
/// for one) both engines fail and agree. Measured: this wrapping fails on a
/// build with the fix reversed and passes on the build with it.
///
/// @param q - the generated query, whose first column is `c1`
fn guarded_filter(q: &str) -> String {
    format!(
        "SELECT d.c1, d.guard FROM (SELECT p.*, CASE WHEN p.c1 IS NULL \
         THEN abs(-9223372036854775807 - 1) ELSE 0 END AS guard FROM ({q}) AS p) AS d \
         WHERE d.c1 IS NOT NULL"
    )
}

/// TLP, NoREC, the outer filter, index agreement and ANALYZE agreement, for a
/// query with a predicate.
fn add_predicate_properties(case: &mut Case, query: &Query, indexed: bool) {
    if query.predicate.is_empty() {
        return;
    }
    let relation = &query.relation;
    let columns = &query.select_list;
    let predicate = &query.predicate;
    case.properties.push(Property::Partition {
        name: "TLP".to_string(),
        whole: relation.select(columns, ""),
        parts: vec![
            relation.select(columns, &format!("WHERE {predicate}")),
            relation.select(columns, &format!("WHERE NOT ({predicate})")),
            relation.select(columns, &format!("WHERE ({predicate}) IS NULL")),
        ],
    });
    case.properties.push(Property::Same {
        name: "NoREC".to_string(),
        queries: vec![
            relation.select("count(*)", &format!("WHERE {predicate}")),
            relation.select(
                &format!("coalesce(sum(CASE WHEN {predicate} THEN 1 ELSE 0 END), 0)"),
                "",
            ),
        ],
    });
    if query.tail.is_empty() && !query.first.is_empty() {
        let first = &query.first;
        let inner = relation.select(
            columns,
            &format!("WHERE ({predicate}) AND ({first}) IS NOT NULL"),
        );
        let outer = format!(
            "SELECT * FROM ({}) AS d WHERE d.c1 IS NOT NULL",
            relation.select(columns, &format!("WHERE {predicate}"))
        );
        case.properties.push(Property::Same {
            name: "outer filter".to_string(),
            queries: vec![inner, outer],
        });
    }
    let table = relation.table.clone().unwrap_or_default();
    let direct = !table.is_empty() && relation.from == format!("{table} AS s");
    if indexed && direct {
        let mut plain = relation.clone();
        plain.from = format!("{table} AS s NOT INDEXED");
        let tail = format!("WHERE {predicate} {}", query.tail);
        case.properties.push(Property::Same {
            name: "index agreement".to_string(),
            queries: vec![
                relation.select(columns, &tail),
                plain.select(columns, &tail),
            ],
        });
        case.properties.push(Property::Stable {
            name: "ANALYZE agreement".to_string(),
            query: relation.select(columns, &tail),
            between: "ANALYZE".to_string(),
        });
    }
}
